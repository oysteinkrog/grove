//! Enumeration for `grove gc`.
//!
//! The work_dir is enumerated from the registry JSON, `git worktree list
//! --porcelain` and plain directory reads. It is deliberately *not* enumerated
//! via `grove list`: that command runs a remote-containment check per project
//! and took over 17 minutes on this machine's 41 projects, which would make a
//! routine gc run unusable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use time::{Duration, OffsetDateTime};

use crate::registry::Registry;

use super::guards::{self, ProbeContext, QUICK_GIT_TIMEOUT};
use super::{
    Category, Finding, GcOptions, GcPlan, Layout, Remedy, classify_worktree_path, exec, paths_equal,
};

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Parse `git worktree list --porcelain` output.
pub fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;

    for line in text.lines() {
        let line = line.trim_end();
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                branch: None,
                head: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ")
            && let Some(entry) = current.as_mut()
        {
            entry.head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(entry) = current.as_mut()
        {
            entry.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    entries
}

/// Parse `git worktree prune --dry-run --verbose` output into human lines.
pub fn parse_prune_dry_run(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub struct Scanner<'a> {
    pub layout: &'a Layout,
    pub registry: &'a Registry,
    pub opts: &'a GcOptions,
    pub probes: &'a ProbeContext,
}

impl Scanner<'_> {
    /// Run every category in order and return the resulting plan.
    pub fn scan(&self) -> GcPlan {
        let mut plan = GcPlan::default();

        let worktrees = match exec::git(
            &self.layout.main_repo,
            &["worktree", "list", "--porcelain"],
            QUICK_GIT_TIMEOUT,
        ) {
            Ok(text) => parse_worktree_list(&text),
            Err(e) => {
                plan.warn(format!(
                    "could not enumerate git worktrees ({e}); categories 2, 5 and 7 are incomplete"
                ));
                Vec::new()
            }
        };

        // One query for every worktree HEAD, before any category asks about a
        // single tree. Each category still falls back per tree for anything the
        // batch did not cover, so this is a shortcut, never the only answer.
        let containment = self.batch_containment(&worktrees);

        self.scan_stale_registry_entries(&mut plan);
        self.scan_unregistered_worktrees(&worktrees, &containment, &mut plan);
        self.scan_foreign_directories(&worktrees, &mut plan);
        self.scan_expired_ephemerals(&containment, &mut plan);
        self.scan_harness_worktrees(&containment, &mut plan);
        self.scan_prunable_metadata(&mut plan);
        super::merged::scan(self, &containment, &mut plan);
        super::archive::scan(self.layout, self.opts, &mut plan);

        for note in &self.probes.notes {
            plan.warn(note.clone());
        }
        plan
    }

    /// Batched remote containment for every worktree git knows about.
    ///
    /// `git worktree list --porcelain` already reports each worktree's HEAD, so
    /// this costs no extra enumeration. Worktrees of *this* repo only, which is
    /// what keeps the query off the network on a partial clone.
    fn batch_containment(&self, worktrees: &[WorktreeEntry]) -> guards::RemoteContainment {
        let oids: Vec<String> = worktrees.iter().filter_map(|wt| wt.head.clone()).collect();
        if oids.is_empty() {
            return guards::RemoteContainment::empty();
        }
        if self.opts.progress {
            eprintln!(
                "[gc] remote containment for {} worktree head(s)",
                oids.len()
            );
        }
        guards::RemoteContainment::batch(&self.layout.main_repo, &oids)
    }

    fn progress(&self, index: usize, total: usize, what: &str) {
        if self.opts.progress {
            eprintln!("[gc] ({index}/{total}) {what}");
        }
    }

    /// Inspect independent trees concurrently, reporting each as it finishes.
    ///
    /// Every tree costs a `git status` walk plus a remote-containment query:
    /// tens of seconds on a small worktree, minutes on one carrying build
    /// output. Run one at a time over a real work_dir that adds up to hours,
    /// nearly all of it spent waiting on the filesystem. The work per tree is
    /// independent, so it overlaps on a small pool — wide enough to hide the
    /// waiting, narrow enough to leave the machine to whoever else is building
    /// on it. Results come back in input order regardless of finish order.
    pub fn inspect_concurrently<T, R>(
        &self,
        items: &[T],
        label: &str,
        inspect: impl Fn(&T) -> R + Sync,
    ) -> Vec<R>
    where
        T: Sync,
        R: Send,
    {
        let total = items.len();
        if total == 0 {
            return Vec::new();
        }
        let done = AtomicUsize::new(0);
        let run = || {
            items
                .par_iter()
                .map(|item| {
                    let result = inspect(item);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if self.opts.progress {
                        eprintln!("[gc] {label} {n}/{total}");
                    }
                    result
                })
                .collect()
        };

        match rayon::ThreadPoolBuilder::new()
            .num_threads(self.opts.scan_threads.max(1))
            .build()
        {
            Ok(pool) => pool.install(run),
            // A pool that will not build is no reason to skip the scan; fall
            // back to whatever rayon's global pool offers.
            Err(_) => run(),
        }
    }

    /// Paths of every registered project, for containment tests.
    pub fn registered_paths(&self) -> BTreeSet<PathBuf> {
        self.registry
            .projects
            .values()
            .map(|p| p.path.clone())
            .collect()
    }

    // ── category 1 ───────────────────────────────────────────────────────────

    /// Registry entries whose directory no longer exists. Nothing on disk is at
    /// stake, so there is no guard to run: the entry is pure stale bookkeeping.
    fn scan_stale_registry_entries(&self, plan: &mut GcPlan) {
        for (tag, project) in &self.registry.projects {
            if project.path.exists() {
                continue;
            }
            plan.findings.push(
                Finding::new(Category::StaleRegistryEntry, tag, &project.path)
                    .detail(format!("branch {}", project.branch))
                    .detail("registered path does not exist")
                    .remedy(Remedy::DropRegistryEntry { tag: tag.clone() }),
            );
        }
    }

    // ── category 2 ───────────────────────────────────────────────────────────

    /// Git worktrees under work_dir that the registry does not know about.
    ///
    /// Paths under `MASTER/.claude/worktrees` and `.scratch` are excluded here
    /// by [`classify_worktree_path`] — categories 5 and 4 own them.
    fn scan_unregistered_worktrees(
        &self,
        worktrees: &[WorktreeEntry],
        containment: &guards::RemoteContainment,
        plan: &mut GcPlan,
    ) {
        let registered = self.registered_paths();
        let candidates: Vec<&WorktreeEntry> = worktrees
            .iter()
            .filter(|wt| {
                classify_worktree_path(&wt.path, self.layout, &registered)
                    == Some(Category::UnregisteredWorktree)
            })
            .collect();

        let outside = worktrees
            .iter()
            .filter(|wt| {
                !wt.path.starts_with(&self.layout.work_dir)
                    && !paths_equal(&wt.path, &self.layout.main_repo)
            })
            .count();
        if outside > 0 {
            plan.warn(format!(
                "{outside} git worktree(s) live outside {} and were not classified",
                self.layout.work_dir.display()
            ));
        }

        // Worktrees whose directory is gone belong to category 6's prune, not
        // to an adopt-or-remove prompt.
        let candidates: Vec<&WorktreeEntry> = candidates
            .into_iter()
            .filter(|wt| wt.path.is_dir())
            .collect();

        let findings = self.inspect_concurrently(&candidates, "worktree", |wt| {
            let activity = guards::last_filesystem_activity(&wt.path);
            let mut finding = Finding::new(
                Category::UnregisteredWorktree,
                dir_label(&wt.path),
                &wt.path,
            );
            match wt.branch.as_deref() {
                Some(branch) => finding = finding.detail(format!("branch {branch}")),
                None => finding = finding.detail("detached HEAD"),
            }

            match guards::inspect_tree_with(&wt.path, containment) {
                Ok(safety) => {
                    finding = finding
                        .detail(describe_safety(&safety))
                        .block(safety.blockers())
                        .remedy(Remedy::AdoptOrDone {
                            branch: wt.branch.clone(),
                            head: safety.head.clone(),
                        });
                }
                Err(e) => {
                    finding = finding
                        .detail(format!("state unknown: {e}"))
                        .block(["worktree state could not be read".to_string()]);
                }
            }

            self.apply_liveness(finding, activity)
        });
        plan.findings.extend(findings);
    }

    // ── category 3 ───────────────────────────────────────────────────────────

    /// Top-level directories that are neither the main repo nor a worktree:
    /// standalone clones, orphaned trees, scratch space someone left behind.
    /// Always report-only — gc has no way to know what they are worth.
    fn scan_foreign_directories(&self, worktrees: &[WorktreeEntry], plan: &mut GcPlan) {
        let mut known: BTreeSet<PathBuf> = worktrees.iter().map(|wt| wt.path.clone()).collect();
        known.extend(self.registered_paths());

        let entries = match std::fs::read_dir(&self.layout.work_dir) {
            Ok(entries) => entries,
            Err(e) => {
                plan.warn(format!(
                    "could not read {} ({e}); category 3 is empty",
                    self.layout.work_dir.display()
                ));
                return;
            }
        };

        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = dir_label(&path);
            // Dot-directories are grove's own bookkeeping (.grove, .archive) or
            // handled elsewhere (.scratch → category 4, .claude → category 5).
            if name.starts_with('.') {
                continue;
            }
            if paths_equal(&path, &self.layout.main_repo) || known.contains(&path) {
                continue;
            }
            dirs.push(path);
        }
        dirs.sort();

        for path in dirs {
            let mut finding = Finding::new(Category::ForeignDirectory, dir_label(&path), &path);
            finding = finding.detail(describe_foreign_dir(&path));
            if let Some(age) = self.age_of(&path) {
                finding = finding.detail(format!("last touched {}", guards::humanize_age(age)));
            }
            plan.findings.push(finding);
        }
    }

    // ── category 4 ───────────────────────────────────────────────────────────

    /// Expired ephemerals: registered `.scratch` projects whose recorded TTL has
    /// elapsed, plus unregistered `.scratch` directories older than the default
    /// TTL (raw clones dropped there by hand or by a hook).
    fn scan_expired_ephemerals(&self, containment: &guards::RemoteContainment, plan: &mut GcPlan) {
        for (tag, project) in &self.registry.projects {
            if !project.is_expired(self.opts.now) || !project.path.exists() {
                continue;
            }
            let expired_for = project
                .expires_at
                .map(|e| guards::humanize_age(self.opts.now - e))
                .unwrap_or_else(|| "unknown".to_string());

            let activity = guards::last_filesystem_activity(&project.path);
            let mut finding = Finding::new(Category::ExpiredEphemeral, tag, &project.path)
                .detail(format!("branch {}", project.branch))
                .detail(format!("TTL elapsed {expired_for}"));

            match guards::inspect_tree_with(&project.path, containment) {
                Ok(safety) => {
                    finding = finding
                        .detail(describe_safety(&safety))
                        .block(safety.blockers())
                        .remedy(Remedy::RemoveEphemeral {
                            tag: Some(tag.clone()),
                            branch: safety
                                .branch
                                .clone()
                                .or_else(|| Some(project.branch.clone())),
                            head: safety.head.clone(),
                        });
                }
                Err(e) => {
                    finding = finding
                        .detail(format!("state unknown: {e}"))
                        .block(["worktree state could not be read".to_string()]);
                }
            }

            plan.findings.push(self.apply_liveness(finding, activity));
        }

        self.scan_unregistered_scratch(containment, plan);
    }

    fn scan_unregistered_scratch(
        &self,
        containment: &guards::RemoteContainment,
        plan: &mut GcPlan,
    ) {
        let scratch = self.layout.scratch_dir();
        if !scratch.is_dir() {
            return;
        }
        let registered = self.registered_paths();
        let entries = match std::fs::read_dir(&scratch) {
            Ok(entries) => entries,
            Err(e) => {
                plan.warn(format!(
                    "could not read {} ({e}); unregistered ephemerals were not scanned",
                    scratch.display()
                ));
                return;
            }
        };

        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && !registered.contains(p))
            .collect();
        dirs.sort();

        for path in dirs {
            let activity = guards::last_filesystem_activity(&path);
            let Some(age) = activity.map(|t| self.opts.now - t) else {
                plan.warn(format!("could not stat {}", path.display()));
                continue;
            };
            if age < self.opts.scratch_ttl {
                continue;
            }

            let mut finding = Finding::new(Category::ExpiredEphemeral, dir_label(&path), &path)
                .detail("unregistered .scratch directory")
                .detail(format!("last touched {}", guards::humanize_age(age)));

            if guards::resolve_git_dir(&path).is_none() {
                // Not a git tree, so gc cannot tell whether anything inside is
                // worth keeping. Report it and let a human look.
                finding = finding
                    .detail("not a git tree")
                    .block(["cannot verify contents of a non-git directory".to_string()]);
                plan.findings.push(self.apply_liveness(finding, activity));
                continue;
            }

            match guards::inspect_tree_with(&path, containment) {
                Ok(safety) => {
                    finding = finding
                        .detail(describe_safety(&safety))
                        .block(safety.blockers())
                        .remedy(Remedy::RemoveEphemeral {
                            tag: None,
                            branch: safety.branch.clone(),
                            head: safety.head.clone(),
                        });
                }
                Err(e) => {
                    finding = finding
                        .detail(format!("state unknown: {e}"))
                        .block(["worktree state could not be read".to_string()]);
                }
            }
            plan.findings.push(self.apply_liveness(finding, activity));
        }
    }

    // ── category 5 ───────────────────────────────────────────────────────────

    /// Harness worktrees under `MASTER/.claude/worktrees/agent-*`. A fresh one
    /// belongs to a running agent session, so only stale ones are collectable;
    /// fresh ones are still listed, with the reason they were left alone.
    fn scan_harness_worktrees(&self, containment: &guards::RemoteContainment, plan: &mut GcPlan) {
        let harness = self.layout.harness_dir();
        if !harness.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(&harness) {
            Ok(entries) => entries,
            Err(e) => {
                plan.warn(format!(
                    "could not read {} ({e}); category 5 is empty",
                    harness.display()
                ));
                return;
            }
        };

        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && dir_label(p).starts_with("agent-"))
            .collect();
        dirs.sort();

        let findings = self.inspect_concurrently(&dirs, "harness", |path| {
            let activity = guards::last_filesystem_activity(path);
            let age = activity.map(|t| self.opts.now - t);
            let mut finding = Finding::new(Category::HarnessWorktree, dir_label(path), path);
            if let Some(age) = age {
                finding = finding.detail(format!("last touched {}", guards::humanize_age(age)));
            }

            let stale = age.is_some_and(|a| a >= self.opts.harness_stale_after);
            if !stale {
                finding =
                    finding.block(["still fresh; a live agent session may own it".to_string()]);
            }

            match guards::inspect_tree_with(path, containment) {
                Ok(safety) => {
                    finding = finding
                        .detail(describe_safety(&safety))
                        .block(safety.blockers())
                        .remedy(Remedy::RemoveHarnessWorktree {
                            branch: safety.branch.clone(),
                            head: safety.head.clone(),
                        });
                }
                Err(e) => {
                    finding = finding
                        .detail(format!("state unknown: {e}"))
                        .block(["worktree state could not be read".to_string()]);
                }
            }

            self.apply_liveness(finding, activity)
        });
        plan.findings.extend(findings);
    }

    // ── category 6 ───────────────────────────────────────────────────────────

    /// Worktree administrative entries git itself considers prunable. Reported
    /// as a single finding because one `git worktree prune` clears all of them.
    fn scan_prunable_metadata(&self, plan: &mut GcPlan) {
        let output = exec::git_streams(
            &self.layout.main_repo,
            &["worktree", "prune", "--dry-run", "--verbose"],
            QUICK_GIT_TIMEOUT,
        );
        let lines = match output {
            Ok(text) => parse_prune_dry_run(&text),
            Err(e) => {
                plan.warn(format!("could not check prunable worktree metadata ({e})"));
                return;
            }
        };
        if lines.is_empty() {
            return;
        }

        let mut finding = Finding::new(
            Category::PrunableMetadata,
            "git worktree prune",
            &self.layout.main_repo,
        )
        .remedy(Remedy::PruneWorktreeMetadata)
        .detail(format!(
            "{} stale administrative entr{}",
            lines.len(),
            if lines.len() == 1 { "y" } else { "ies" }
        ));
        for line in lines {
            finding = finding.detail(line);
        }
        plan.findings.push(finding);
    }

    // ── shared helpers ───────────────────────────────────────────────────────

    /// Add a blocker when somebody appears to be using the tree. Applied to
    /// every destructive category, on top of whatever the tree's git state says.
    ///
    /// `activity` is the tree's last filesystem activity as sampled *before*
    /// this scan touched it, so gc's own inspection cannot masquerade as a live
    /// session.
    pub fn apply_liveness(&self, finding: Finding, activity: Option<OffsetDateTime>) -> Finding {
        if matches!(finding.remedy, Remedy::ReportOnly) && finding.blockers.is_empty() {
            return finding;
        }
        let liveness = guards::check_liveness(
            &finding.path,
            self.opts.now,
            self.opts.liveness_window,
            self.probes,
            activity,
        );
        if liveness.is_live() {
            return finding.block(liveness.reasons.into_iter().map(|r| format!("in use: {r}")));
        }
        finding
    }

    pub fn age_of(&self, path: &Path) -> Option<Duration> {
        guards::last_filesystem_activity(path).map(|t| self.opts.now - t)
    }
}

/// Last path component, or the whole path when it has none.
pub fn dir_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn describe_safety(safety: &guards::TreeSafety) -> String {
    let dirt = if safety.dirty {
        format!("{} uncommitted entries", safety.dirty_entries)
    } else {
        "clean".to_string()
    };
    let push = if safety.unpushed {
        "commits not on any remote"
    } else {
        "HEAD present on a remote"
    };
    format!("{dirt}, {push}")
}

fn describe_foreign_dir(path: &Path) -> String {
    if path.join(".git").is_dir() {
        "standalone git clone".to_string()
    } else if path.join(".git").is_file() {
        "git worktree with no registry entry and no listing".to_string()
    } else {
        "plain directory".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_branch_worktree() {
        let text = "worktree /c/work/desktop/master\nHEAD abc123\nbranch refs/heads/master\n\n";
        let entries = parse_worktree_list(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/c/work/desktop/master"));
        assert_eq!(entries[0].branch.as_deref(), Some("master"));
        assert_eq!(entries[0].head.as_deref(), Some("abc123"));
    }

    #[test]
    fn parses_a_detached_worktree() {
        let text = "worktree /c/work/desktop/wt-a\nHEAD def456\ndetached\n";
        let entries = parse_worktree_list(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch, None, "detached entries carry no branch");
    }

    #[test]
    fn parses_several_entries() {
        let text = "worktree /a\nHEAD 1\nbranch refs/heads/main\n\n\
                    worktree /b\nHEAD 2\ndetached\n\n\
                    worktree /c\nHEAD 3\nbranch refs/heads/feature/x\n";
        let entries = parse_worktree_list(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn empty_worktree_list_parses_to_nothing() {
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn prune_dry_run_lines_are_kept_verbatim() {
        let text = "Removing worktrees/inplace: gitdir file points to non-existent location\n\n";
        let lines = parse_prune_dry_run(text);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("Removing worktrees/inplace"));
    }

    #[test]
    fn dir_label_is_the_last_component() {
        assert_eq!(dir_label(Path::new("/c/work/desktop/wt-a")), "wt-a");
        assert_eq!(dir_label(Path::new("/")), "/");
    }
}
