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

pub(super) fn scan(scanner: &Scanner<'_>, plan: &mut GcPlan) {
    let projects: Vec<(&String, &crate::registry::Project)> = scanner
        .registry
        .projects
        .iter()
        .filter(|(_, p)| p.path.is_dir())
        .filter(|(_, p)| !p.is_expired(scanner.opts.now))
        .collect();

    // Each project is inspected independently, so the run overlaps them and
    // carries any warnings back out rather than writing to the plan in place.
    let results = scanner.inspect_concurrently(&projects, "merge state", |(tag, project)| {
        let mut warnings = Vec::new();

        let activity = guards::last_filesystem_activity(&project.path);
        let safety = match guards::inspect_tree(&project.path) {
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
            match pull_request_state(&scanner.layout.main_repo, &project.branch) {
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
    Ok(parsed
        .iter()
        .map(|pr| {
            let number = pr.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
            let state = pr
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("UNKNOWN");
            format!("#{number} {state}")
        })
        .collect())
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
