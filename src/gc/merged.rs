//! Category 7: registered projects whose work has already landed.
//!
//! Merge detection cannot lean on `git merge-base --is-ancestor`. The upstream
//! this tool was built for merges through Mergify with rebase and squash
//! strategies, so a landed branch's commits get new SHAs and the original tip
//! is an ancestor of nothing. gc therefore compares *content*: `git cherry`
//! matches by patch-id, and commit subjects are the fallback for squashes that
//! rewrote the diff. A `gh` lookup adds the PR's own verdict when it is
//! reachable, and its absence downgrades the item to "merge state unverified"
//! rather than blocking the run or guessing.
//!
//! Nothing here mutates. The output is a list of `grove done` candidates.

use std::collections::HashMap;
use std::time::Duration as StdDuration;

use super::guards::{self, QUICK_GIT_TIMEOUT};
use super::scan::Scanner;
use super::{Category, Finding, GcPlan, exec};

/// `gh` talks to the network; keep it on a short leash.
const GH_TIMEOUT: StdDuration = StdDuration::from_secs(20);

/// Patch-id comparison over a long branch is slow, so `git cherry` gets more
/// room than the quick queries.
const CHERRY_TIMEOUT: StdDuration = StdDuration::from_secs(180);

/// The verdict of `git cherry <base> HEAD`: commits whose patch-id is absent
/// from the base are `+`, ones already present are `-`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CherryVerdict {
    pub total: usize,
    pub unmerged: Vec<String>,
}

impl CherryVerdict {
    pub fn fully_absorbed(&self) -> bool {
        self.unmerged.is_empty()
    }
}

pub fn parse_cherry(text: &str) -> CherryVerdict {
    let mut verdict = CherryVerdict::default();
    for line in text.lines() {
        let line = line.trim();
        let Some((marker, sha)) = line.split_once(' ') else {
            continue;
        };
        match marker {
            "+" => {
                verdict.total += 1;
                verdict.unmerged.push(sha.trim().to_string());
            }
            "-" => verdict.total += 1,
            _ => {}
        }
    }
    verdict
}

/// How a project's work was found in the base, and how sure gc is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeState {
    /// Every commit's patch-id is already in the base.
    AbsorbedByPatchId { commits: usize },
    /// Patch-ids differ (squash or rebase rewrote them) but every remaining
    /// commit's subject appears in the base's history.
    AbsorbedBySubject { commits: usize },
    /// Work remains that the base does not have.
    NotMerged { remaining: usize },
    /// gc could not answer — the base is unresolvable, git timed out, or the
    /// branch is too long to compare cheaply.
    Unknown { reason: String },
}

impl MergeState {
    pub fn is_absorbed(&self) -> bool {
        matches!(
            self,
            Self::AbsorbedByPatchId { .. } | Self::AbsorbedBySubject { .. }
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Self::AbsorbedByPatchId { commits } => {
                format!("all {commits} commits present in the base by patch-id")
            }
            Self::AbsorbedBySubject { commits } => {
                format!("all {commits} commits matched in the base by subject (rebase or squash)")
            }
            Self::NotMerged { remaining } => {
                format!("{remaining} commit(s) not in the base")
            }
            Self::Unknown { reason } => format!("merge state unknown: {reason}"),
        }
    }
}

/// Decide whether a subject-line fallback rescues the commits `git cherry`
/// flagged. Pure so the decision can be tested without a repo.
pub fn subject_verdict(unmerged: usize, subject_hits: usize) -> MergeState {
    if unmerged == 0 {
        return MergeState::AbsorbedByPatchId { commits: 0 };
    }
    if subject_hits == unmerged {
        MergeState::AbsorbedBySubject { commits: unmerged }
    } else {
        MergeState::NotMerged {
            remaining: unmerged - subject_hits,
        }
    }
}

pub(super) fn scan(
    scanner: &Scanner<'_>,
    containment: &guards::RemoteContainment,
    plan: &mut GcPlan,
) {
    let projects: Vec<(&String, &crate::registry::Project)> = scanner
        .registry
        .projects
        .iter()
        .filter(|(_, p)| p.path.is_dir())
        .filter(|(_, p)| !p.is_expired(scanner.opts.now))
        .collect();

    // One listing for the whole repo instead of a network round trip per
    // absorbed project. Built before the inspection so every worker shares it.
    let pull_requests = if scanner.opts.query_pr_state {
        let (index, note) = PullRequestIndex::fetch(&scanner.layout.main_repo);
        if let Some(note) = note {
            plan.warn(note);
        }
        index
    } else {
        PullRequestIndex::empty()
    };

    // Each project is inspected independently, so the run overlaps them and
    // carries any warnings back out rather than writing to the plan in place.
    let results = scanner.inspect_concurrently(&projects, "merge state", |(tag, project)| {
        let mut warnings = Vec::new();

        let activity = guards::last_filesystem_activity(&project.path);
        let safety = match guards::inspect_tree_with(&project.path, containment) {
            Ok(safety) => safety,
            Err(e) => {
                warnings.push(format!("{tag}: could not read worktree state ({e})"));
                return (None, warnings);
            }
        };
        // "Merged *and clean*" — a project with local edits is not a done
        // candidate no matter what landed upstream.
        if safety.dirty {
            return (None, warnings);
        }

        let state = merge_state(scanner, &project.path, &project.base, &mut warnings, tag);
        if !state.is_absorbed() {
            return (None, warnings);
        }

        let mut finding = Finding::new(Category::MergedProject, tag.as_str(), &project.path)
            .detail(format!(
                "branch {} vs base {}",
                project.branch, project.base
            ))
            .detail(state.describe())
            .detail("clean worktree");

        if scanner.opts.query_pr_state {
            match pull_requests.states_for(&scanner.layout.main_repo, &project.branch) {
                Ok(prs) if prs.is_empty() => {
                    finding = finding.detail("no pull request found for the branch");
                }
                Ok(prs) => finding = finding.detail(format!("pull requests: {}", prs.join(", "))),
                Err(e) => {
                    finding = finding.detail(format!("merge state unverified ({e})"));
                }
            }
        }

        let liveness = guards::check_liveness(
            &project.path,
            scanner.opts.now,
            scanner.opts.liveness_window,
            scanner.probes,
            activity,
        );
        if liveness.is_live() {
            finding = finding.detail(format!("still in use: {}", liveness.reasons.join("; ")));
        }

        finding = finding.detail(format!("run: grove done {tag}"));
        (Some(finding), warnings)
    });

    for (finding, warnings) in results {
        plan.findings.extend(finding);
        plan.warnings.extend(warnings);
    }
}

fn merge_state(
    scanner: &Scanner<'_>,
    path: &std::path::Path,
    base: &str,
    warnings: &mut Vec<String>,
    tag: &str,
) -> MergeState {
    if exec::git(
        path,
        &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")],
        QUICK_GIT_TIMEOUT,
    )
    .is_err()
    {
        return MergeState::Unknown {
            reason: format!("base '{base}' does not resolve in this worktree"),
        };
    }

    // No commit-count precheck before `git cherry`. A rebase-merged branch is
    // exactly the case where `rev-list --count base..HEAD` reads in the
    // hundreds while every patch-id is in fact already in the base, so gating
    // on that count would refuse to answer precisely when the answer matters.
    // `git cherry` is cheap regardless (~3s over 4260 commits on this repo);
    // CHERRY_TIMEOUT is the real bound.
    let cherry = match exec::git(path, &["cherry", base, "HEAD"], CHERRY_TIMEOUT) {
        Ok(text) => parse_cherry(&text),
        Err(e) => {
            warnings.push(format!("{tag}: patch-id comparison failed ({e})"));
            return MergeState::Unknown {
                reason: "patch-id comparison failed".to_string(),
            };
        }
    };

    if cherry.fully_absorbed() {
        return MergeState::AbsorbedByPatchId {
            commits: cherry.total,
        };
    }

    // The subject fallback costs two git calls per unmatched commit. Past the
    // budget the verdict is not in doubt anyway: a branch with that many
    // commits the base has never seen is not merged.
    if cherry.unmerged.len() > scanner.opts.max_subject_probe_commits {
        return MergeState::NotMerged {
            remaining: cherry.unmerged.len(),
        };
    }

    let hits = cherry
        .unmerged
        .iter()
        .filter(|sha| subject_is_in_base(path, base, sha))
        .count();
    subject_verdict(cherry.unmerged.len(), hits)
}

/// Whether the subject line of `sha` also appears somewhere in `base`'s history.
/// The fallback for squash merges, where the landed commit keeps the subject but
/// nothing else.
fn subject_is_in_base(path: &std::path::Path, base: &str, sha: &str) -> bool {
    let Ok(subject) = exec::git(path, &["log", "-1", "--format=%s", sha], QUICK_GIT_TIMEOUT) else {
        return false;
    };
    let subject = subject.trim();
    if subject.is_empty() {
        return false;
    }
    exec::git(
        path,
        &[
            "log",
            base,
            "--max-count=1",
            "--format=%H",
            "--fixed-strings",
            &format!("--grep={subject}"),
        ],
        QUICK_GIT_TIMEOUT,
    )
    .is_ok_and(|out| !out.trim().is_empty())
}

/// Cap on the batched listing. Above this the answer is treated as truncated
/// and every branch falls back to its own query, because a branch missing from
/// a truncated listing is indistinguishable from a branch with no PR.
const PR_LIST_LIMIT: usize = 1000;

/// `gh` fetching up to [`PR_LIST_LIMIT`] pull requests needs more than the
/// per-branch leash.
const GH_BATCH_TIMEOUT: StdDuration = StdDuration::from_secs(90);

/// Pull request state for every branch, from one `gh` call.
///
/// The per-branch query is a network round trip each, on a 20s timeout, run for
/// every project whose work looks absorbed. One listing answers for all of
/// them. Truncation is the only real hazard: if the listing came back at the
/// limit, absence from it means nothing, so the index reports itself unusable
/// and callers go back to asking per branch.
#[derive(Debug, Default)]
pub struct PullRequestIndex {
    by_branch: HashMap<String, Vec<String>>,
    /// The listing was complete, so a branch absent from it genuinely has no PR.
    complete: bool,
}

impl PullRequestIndex {
    /// An index that answers nothing, so every lookup falls back.
    pub fn empty() -> Self {
        Self::default()
    }

    /// One `gh pr list` for the whole repo, indexed by head branch.
    ///
    /// Skipped outright on a repo with more pull requests than the listing can
    /// hold, because the listing would be refused as truncated anyway and the
    /// attempt is not free: 8s at the 1000 limit on the repo this was written
    /// for, 26s at 5000, and still truncated. A single `--head` query is 0.5s,
    /// so falling straight back is cheaper than finding out the hard way.
    pub fn fetch(main_repo: &std::path::Path) -> (Self, Option<String>) {
        if let Some(newest) = newest_pull_request_number(main_repo)
            && newest > PR_LIST_LIMIT as u64
        {
            // Nothing is wrong here, so this is not a warning: asking per
            // branch is the correct plan for a repo this size, not a failure.
            return (Self::empty(), None);
        }

        let limit = PR_LIST_LIMIT.to_string();
        let out = match exec::run(
            "gh",
            &[
                "pr",
                "list",
                "--state",
                "all",
                "--limit",
                &limit,
                "--json",
                "number,state,headRefName",
            ],
            Some(main_repo),
            GH_BATCH_TIMEOUT,
        ) {
            Ok(out) if out.success => out,
            Ok(out) => {
                let detail = out.stderr.trim().lines().next().unwrap_or("no detail");
                return (
                    Self::empty(),
                    Some(format!(
                        "gh pull request listing failed ({detail}); merge state is verified per branch instead"
                    )),
                );
            }
            Err(e) => {
                return (
                    Self::empty(),
                    Some(format!(
                        "gh pull request listing unavailable ({e}); merge state is verified per branch instead"
                    )),
                );
            }
        };

        let parsed: Vec<serde_json::Value> = match serde_json::from_str(out.stdout.trim()) {
            Ok(parsed) => parsed,
            Err(e) => {
                return (
                    Self::empty(),
                    Some(format!(
                        "gh pull request listing unreadable ({e}); merge state is verified per branch instead"
                    )),
                );
            }
        };

        Self::index_rows(&parsed)
    }

    /// Index a listing, or refuse it as truncated.
    ///
    /// Pure, so the truncation rule can be tested without reaching for `gh`.
    fn index_rows(parsed: &[serde_json::Value]) -> (Self, Option<String>) {
        if parsed.len() >= PR_LIST_LIMIT {
            return (
                Self::empty(),
                Some(format!(
                    "gh returned {PR_LIST_LIMIT} pull requests, the listing limit; merge state is verified per branch instead"
                )),
            );
        }

        let mut by_branch: HashMap<String, Vec<String>> = HashMap::new();
        for pr in parsed {
            let Some(branch) = pr.get("headRefName").and_then(|b| b.as_str()) else {
                continue;
            };
            by_branch
                .entry(branch.to_string())
                .or_default()
                .push(describe_pr(pr));
        }

        (
            Self {
                by_branch,
                complete: true,
            },
            None,
        )
    }

    /// Pull requests opened from `branch`, from the listing when it is usable
    /// and from a single `gh` call otherwise.
    pub fn states_for(
        &self,
        main_repo: &std::path::Path,
        branch: &str,
    ) -> Result<Vec<String>, String> {
        if self.complete {
            return Ok(self.by_branch.get(branch).cloned().unwrap_or_default());
        }
        pull_request_state(main_repo, branch)
    }
}

/// The newest pull request number, as an upper bound on how many the repo has.
///
/// One row, so it costs about half a second. GitHub numbers pull requests and
/// issues from one counter, so this over-estimates the pull request count and
/// never under-estimates it. That is the safe direction: it can send a repo to
/// the per-branch path that the batch would in fact have covered, which costs
/// 0.5s per branch, where the reverse would waste the whole listing.
fn newest_pull_request_number(main_repo: &std::path::Path) -> Option<u64> {
    let out = exec::run(
        "gh",
        &[
            "pr", "list", "--state", "all", "--limit", "1", "--json", "number",
        ],
        Some(main_repo),
        GH_TIMEOUT,
    )
    .ok()?;
    if !out.success {
        return None;
    }
    let parsed: Vec<serde_json::Value> = serde_json::from_str(out.stdout.trim()).ok()?;
    parsed
        .first()
        .and_then(|pr| pr.get("number"))
        .and_then(|n| n.as_u64())
}

fn describe_pr(pr: &serde_json::Value) -> String {
    let number = pr.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
    let state = pr
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("UNKNOWN");
    format!("#{number} {state}")
}

/// Ask `gh` for the pull requests opened from `branch`. Best effort: any
/// failure — no `gh`, no auth, no network, slow API — comes back as an error
/// the caller renders as "merge state unverified".
fn pull_request_state(main_repo: &std::path::Path, branch: &str) -> Result<Vec<String>, String> {
    let out = exec::run(
        "gh",
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "5",
            "--json",
            "number,state",
        ],
        Some(main_repo),
        GH_TIMEOUT,
    )
    .map_err(|e| format!("gh {e}"))?;

    if !out.success {
        return Err(format!(
            "gh failed: {}",
            out.stderr.trim().lines().next().unwrap_or("no detail")
        ));
    }

    let parsed: Vec<serde_json::Value> = serde_json::from_str(out.stdout.trim())
        .map_err(|e| format!("gh output unreadable: {e}"))?;
    Ok(parsed.iter().map(describe_pr).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cherry_output_splits_absorbed_from_remaining() {
        let verdict = parse_cherry("- abc111\n+ def222\n- 333aaa\n");
        assert_eq!(verdict.total, 3);
        assert_eq!(verdict.unmerged, vec!["def222"]);
        assert!(!verdict.fully_absorbed());
    }

    #[test]
    fn all_minus_lines_mean_fully_absorbed() {
        let verdict = parse_cherry("- abc111\n- def222\n");
        assert_eq!(verdict.total, 2);
        assert!(verdict.fully_absorbed());
    }

    #[test]
    fn empty_cherry_output_is_absorbed() {
        assert!(parse_cherry("").fully_absorbed());
    }

    #[test]
    fn subject_fallback_rescues_a_full_match() {
        assert_eq!(
            subject_verdict(3, 3),
            MergeState::AbsorbedBySubject { commits: 3 }
        );
    }

    #[test]
    fn subject_fallback_reports_the_shortfall() {
        assert_eq!(
            subject_verdict(5, 2),
            MergeState::NotMerged { remaining: 3 }
        );
    }

    fn pr_row(number: u64, state: &str, branch: &str) -> serde_json::Value {
        serde_json::json!({ "number": number, "state": state, "headRefName": branch })
    }

    #[test]
    fn a_listing_answers_for_every_branch_in_it() {
        let rows = vec![
            pr_row(1, "MERGED", "feature/one"),
            pr_row(2, "CLOSED", "feature/two"),
            pr_row(3, "OPEN", "feature/two"),
        ];
        let (index, note) = PullRequestIndex::index_rows(&rows);
        assert!(note.is_none(), "{note:?}");

        let repo = std::path::Path::new("/nonexistent");
        assert_eq!(
            index.states_for(repo, "feature/one").unwrap(),
            vec!["#1 MERGED"]
        );
        assert_eq!(
            index.states_for(repo, "feature/two").unwrap(),
            vec!["#2 CLOSED", "#3 OPEN"],
            "both pull requests for a branch should be reported"
        );
    }

    #[test]
    fn a_branch_absent_from_a_complete_listing_has_no_pull_request() {
        // The whole point of batching: absence is an answer, so no per-branch
        // call is made. The repo path is deliberately bogus — reaching `gh`
        // here would fail the test rather than pass it quietly.
        let rows = vec![pr_row(1, "MERGED", "feature/one")];
        let (index, _) = PullRequestIndex::index_rows(&rows);
        assert_eq!(
            index
                .states_for(std::path::Path::new("/nonexistent"), "feature/absent")
                .unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_truncated_listing_is_refused_rather_than_trusted() {
        // At the limit, a branch missing from the listing is indistinguishable
        // from a branch with no pull request, so the index must not answer.
        let rows: Vec<serde_json::Value> = (0..PR_LIST_LIMIT)
            .map(|n| pr_row(n as u64, "MERGED", &format!("feature/{n}")))
            .collect();
        let (index, note) = PullRequestIndex::index_rows(&rows);
        assert!(
            note.is_some_and(|n| n.contains("listing limit")),
            "truncation must be reported as a warning"
        );
        assert!(
            !index.complete,
            "a truncated listing must fall back per branch"
        );
    }

    #[test]
    fn rows_without_a_head_branch_are_skipped_not_fatal() {
        let rows = vec![
            serde_json::json!({ "number": 1, "state": "MERGED" }),
            pr_row(2, "OPEN", "feature/real"),
        ];
        let (index, note) = PullRequestIndex::index_rows(&rows);
        assert!(note.is_none());
        assert_eq!(
            index
                .states_for(std::path::Path::new("/nonexistent"), "feature/real")
                .unwrap(),
            vec!["#2 OPEN"]
        );
    }

    #[test]
    fn unknown_state_is_never_absorbed() {
        let state = MergeState::Unknown {
            reason: "base does not resolve".to_string(),
        };
        assert!(!state.is_absorbed());
        assert!(state.describe().contains("merge state unknown"));
    }

    #[test]
    fn descriptions_name_the_detection_method() {
        assert!(
            MergeState::AbsorbedByPatchId { commits: 4 }
                .describe()
                .contains("patch-id")
        );
        assert!(
            MergeState::AbsorbedBySubject { commits: 4 }
                .describe()
                .contains("subject")
        );
    }
}
