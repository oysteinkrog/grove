//! `grove gc` — audit and garbage-collect a work_dir that has drifted.
//!
//! gc sorts everything it finds into eight numbered categories, ordered from
//! "registry bookkeeping" to "informational". The number is part of the user
//! interface: reports name it, and it is how the precedence rules are stated.
//!
//! The categories differ in how much authority gc has over them:
//!
//! | # | Finding                                   | `--yes` may apply |
//! |---|-------------------------------------------|-------------------|
//! | 1 | registry entry whose path is gone         | yes               |
//! | 2 | unregistered worktree under work_dir      | no (asks)         |
//! | 3 | top-level directory that is not a worktree| never             |
//! | 4 | expired ephemeral under `.scratch`        | yes               |
//! | 5 | stale harness worktree under `.claude`    | yes               |
//! | 6 | prunable git worktree metadata            | yes               |
//! | 7 | merged-and-clean registered project       | never             |
//! | 8 | `.archive` contents                       | never             |
//!
//! There is no `--force`. A finding that trips a guard is reported with its
//! blockers and left alone; the operator can still act on it by hand.

pub mod apply;
pub mod archive;
pub mod exec;
pub mod guards;
pub mod merged;
pub mod report;
pub mod scan;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use time::{Duration, OffsetDateTime};

/// The eight things `grove gc` knows how to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    StaleRegistryEntry = 1,
    UnregisteredWorktree = 2,
    ForeignDirectory = 3,
    ExpiredEphemeral = 4,
    HarnessWorktree = 5,
    PrunableMetadata = 6,
    MergedProject = 7,
    ArchiveContents = 8,
}

pub const ALL_CATEGORIES: [Category; 8] = [
    Category::StaleRegistryEntry,
    Category::UnregisteredWorktree,
    Category::ForeignDirectory,
    Category::ExpiredEphemeral,
    Category::HarnessWorktree,
    Category::PrunableMetadata,
    Category::MergedProject,
    Category::ArchiveContents,
];

impl Category {
    pub fn number(self) -> u8 {
        self as u8
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::StaleRegistryEntry => "Registry entries whose worktree is gone",
            Self::UnregisteredWorktree => "Unregistered worktrees under work_dir",
            Self::ForeignDirectory => "Top-level directories that are not worktrees",
            Self::ExpiredEphemeral => "Expired ephemerals under .scratch",
            Self::HarnessWorktree => "Harness worktrees under .claude/worktrees",
            Self::PrunableMetadata => "Prunable git worktree metadata",
            Self::MergedProject => "Merged-and-clean projects (grove done candidates)",
            Self::ArchiveContents => ".archive contents",
        }
    }

    /// One line explaining what gc will and will not do with the category.
    pub fn policy(self) -> &'static str {
        match self {
            Self::StaleRegistryEntry => "dropped from the registry with --yes",
            Self::UnregisteredWorktree => {
                "asks to adopt or remove when interactive; never removed by --yes"
            }
            Self::ForeignDirectory => "report only — gc never deletes these",
            Self::ExpiredEphemeral => "removed with --yes when clean, pushed and idle",
            Self::HarnessWorktree => "removed with --yes when clean, stale and idle",
            Self::PrunableMetadata => "pruned with --yes",
            Self::MergedProject => "report only — run grove done yourself",
            Self::ArchiveContents => "report only — age and size, so it stays visible",
        }
    }

    /// Whether `--yes` is allowed to apply this category's remedy unattended.
    pub fn auto_applicable(self) -> bool {
        matches!(
            self,
            Self::StaleRegistryEntry
                | Self::ExpiredEphemeral
                | Self::HarnessWorktree
                | Self::PrunableMetadata
        )
    }

    /// Whether gc will never mutate anything in this category, prompt or not.
    pub fn report_only(self) -> bool {
        matches!(
            self,
            Self::ForeignDirectory | Self::MergedProject | Self::ArchiveContents
        )
    }
}

/// What gc would do about a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remedy {
    /// Category 1: forget a registry entry whose directory no longer exists.
    DropRegistryEntry { tag: String },
    /// Category 4: remove an expired ephemeral tree, and its registry entry
    /// when it had one.
    RemoveEphemeral {
        tag: Option<String>,
        branch: Option<String>,
        head: Option<String>,
    },
    /// Category 5: remove a stale agent harness worktree.
    RemoveHarnessWorktree {
        branch: Option<String>,
        head: Option<String>,
    },
    /// Category 6: `git worktree prune`.
    PruneWorktreeMetadata,
    /// Category 2: ask the operator to adopt the tree or remove it.
    AdoptOrDone {
        branch: Option<String>,
        head: Option<String>,
    },
    /// Categories 3, 7, 8: nothing to apply.
    ReportOnly,
}

/// One thing gc found, with the evidence behind it.
#[derive(Debug, Clone)]
pub struct Finding {
    pub category: Category,
    /// Short name for the report: a tag, a directory name, or a description.
    pub label: String,
    pub path: PathBuf,
    /// Evidence lines shown under the label.
    pub details: Vec<String>,
    pub remedy: Remedy,
    /// Guard failures. Non-empty means gc will not act, whatever the flags say.
    pub blockers: Vec<String>,
}

impl Finding {
    pub fn new(category: Category, label: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            category,
            label: label.into(),
            path: path.into(),
            details: Vec::new(),
            remedy: Remedy::ReportOnly,
            blockers: Vec::new(),
        }
    }

    pub fn detail(mut self, text: impl Into<String>) -> Self {
        self.details.push(text.into());
        self
    }

    pub fn remedy(mut self, remedy: Remedy) -> Self {
        self.remedy = remedy;
        self
    }

    pub fn block(mut self, reasons: impl IntoIterator<Item = String>) -> Self {
        self.blockers.extend(reasons);
        self
    }

    /// Whether gc could act on this finding if the operator agreed.
    pub fn is_actionable(&self) -> bool {
        self.blockers.is_empty() && !matches!(self.remedy, Remedy::ReportOnly)
    }

    /// Whether `--yes` alone is enough to apply it.
    pub fn is_auto_applicable(&self) -> bool {
        self.is_actionable() && self.category.auto_applicable()
    }
}

/// Everything one gc run found.
#[derive(Debug, Default)]
pub struct GcPlan {
    pub findings: Vec<Finding>,
    /// Problems that cost gc some coverage but did not stop the run: an
    /// unreadable directory, a git call that timed out, a probe that declined.
    pub warnings: Vec<String>,
}

impl GcPlan {
    pub fn in_category(&self, category: Category) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(move |f| f.category == category)
    }

    pub fn auto_applicable(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.is_auto_applicable())
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }
}

/// Knobs the scanners read. Production values come from [`Default`]; tests dial
/// the windows to zero so a fixture created seconds ago can still be treated as
/// stale without touching mtimes.
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub now: OffsetDateTime,
    /// Recency horizon for the live-session guard.
    pub liveness_window: Duration,
    /// Age past which an *unregistered* `.scratch` directory is collectable.
    /// Registered ephemerals use their own recorded `expires_at` instead.
    pub scratch_ttl: Duration,
    /// Age past which a harness worktree counts as abandoned.
    pub harness_stale_after: Duration,
    /// Ask `gh` for PR state while classifying merged projects.
    pub query_pr_state: bool,
    /// Cap on the commit-subject fallback. `git cherry` itself is cheap even
    /// over thousands of commits, but the fallback costs two git calls per
    /// unmatched commit — and a branch with hundreds of unmatched commits is
    /// plainly not merged, so there is nothing to learn by grepping for them.
    pub max_subject_probe_commits: usize,
    /// Emit per-item progress to stderr.
    pub progress: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            now: OffsetDateTime::now_utc(),
            liveness_window: Duration::hours(48),
            scratch_ttl: crate::ttl::default_ttl(),
            harness_stale_after: Duration::hours(48),
            query_pr_state: true,
            max_subject_probe_commits: 40,
            progress: true,
        }
    }
}

impl GcOptions {
    /// Options for fixture tests: no age-based guards, no external lookups,
    /// no progress chatter.
    pub fn for_tests() -> Self {
        Self {
            liveness_window: Duration::ZERO,
            scratch_ttl: Duration::ZERO,
            harness_stale_after: Duration::ZERO,
            query_pr_state: false,
            progress: false,
            ..Self::default()
        }
    }
}

/// Where the work_dir's moving parts live. Kept separate from `RepoContext` so
/// the scanners and their tests can be driven without a full repo discovery.
#[derive(Debug, Clone)]
pub struct Layout {
    pub work_dir: PathBuf,
    pub main_repo: PathBuf,
}

impl Layout {
    pub fn scratch_dir(&self) -> PathBuf {
        self.work_dir.join(".scratch")
    }

    pub fn harness_dir(&self) -> PathBuf {
        self.main_repo.join(".claude/worktrees")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.work_dir.join(".archive")
    }

    /// Append-only log of branches gc deleted, so a mistaken removal is
    /// recoverable from the reflog by name and sha.
    pub fn deleted_branches_log(&self) -> PathBuf {
        self.archive_dir().join("deleted-branches.txt")
    }
}

/// Decide which category owns a git worktree path, applying the precedence
/// rules: the harness directory wins over "unregistered worktree", `.scratch`
/// wins over both, and registered paths belong to their own project.
///
/// Returns `None` for paths gc does not classify at all — the main repo, and
/// anything outside the work_dir.
pub fn classify_worktree_path(
    path: &Path,
    layout: &Layout,
    registered: &BTreeSet<PathBuf>,
) -> Option<Category> {
    if paths_equal(path, &layout.main_repo) {
        return None;
    }
    // The harness directory lives under the main repo, so this test has to come
    // before the work_dir containment test below.
    if path.starts_with(layout.harness_dir()) {
        return Some(Category::HarnessWorktree);
    }
    if path.starts_with(layout.scratch_dir()) {
        return Some(Category::ExpiredEphemeral);
    }
    if !path.starts_with(&layout.work_dir) {
        return None;
    }
    if registered.contains(path) {
        return Some(Category::MergedProject);
    }
    Some(Category::UnregisteredWorktree)
}

/// Compare two paths, tolerating a trailing slash and a missing target.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Record a branch deletion as `DIR BRANCH SHA` so it can be found later.
pub fn log_branch_deletion(
    layout: &Layout,
    dir: &Path,
    branch: &str,
    sha: &str,
) -> Result<(), String> {
    use std::io::Write;

    let log_path = layout.deleted_branches_log();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
    writeln!(file, "{} {branch} {sha}", dir.display())
        .map_err(|e| format!("could not write {}: {e}", log_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            work_dir: PathBuf::from("/c/work/desktop"),
            main_repo: PathBuf::from("/c/work/desktop/master"),
        }
    }

    #[test]
    fn only_bookkeeping_categories_are_auto_applicable() {
        let auto: Vec<u8> = ALL_CATEGORIES
            .iter()
            .filter(|c| c.auto_applicable())
            .map(|c| c.number())
            .collect();
        assert_eq!(auto, vec![1, 4, 5, 6], "--yes must cover exactly 1/4/5/6");
    }

    #[test]
    fn advisory_categories_never_mutate() {
        let report_only: Vec<u8> = ALL_CATEGORIES
            .iter()
            .filter(|c| c.report_only())
            .map(|c| c.number())
            .collect();
        assert_eq!(report_only, vec![3, 7, 8]);
        for category in ALL_CATEGORIES {
            assert!(
                !(category.report_only() && category.auto_applicable()),
                "category {} cannot be both advisory and auto-applied",
                category.number()
            );
        }
    }

    #[test]
    fn unregistered_worktree_is_category_two() {
        let registered = BTreeSet::new();
        assert_eq!(
            classify_worktree_path(Path::new("/c/work/desktop/wt-foo"), &layout(), &registered),
            Some(Category::UnregisteredWorktree)
        );
    }

    #[test]
    fn harness_worktree_beats_unregistered() {
        let registered = BTreeSet::new();
        assert_eq!(
            classify_worktree_path(
                Path::new("/c/work/desktop/master/.claude/worktrees/agent-abc"),
                &layout(),
                &registered
            ),
            Some(Category::HarnessWorktree),
            "paths under MASTER/.claude/worktrees belong to category 5"
        );
    }

    #[test]
    fn scratch_worktree_beats_unregistered() {
        let registered = BTreeSet::new();
        assert_eq!(
            classify_worktree_path(
                Path::new("/c/work/desktop/.scratch/probe"),
                &layout(),
                &registered
            ),
            Some(Category::ExpiredEphemeral)
        );
    }

    #[test]
    fn registered_path_is_not_an_unregistered_worktree() {
        let registered: BTreeSet<PathBuf> = [PathBuf::from("/c/work/desktop/clock-sync")]
            .into_iter()
            .collect();
        assert_eq!(
            classify_worktree_path(
                Path::new("/c/work/desktop/clock-sync"),
                &layout(),
                &registered
            ),
            Some(Category::MergedProject)
        );
    }

    #[test]
    fn main_repo_and_outsiders_are_unclassified() {
        let registered = BTreeSet::new();
        assert_eq!(
            classify_worktree_path(Path::new("/c/work/desktop/master"), &layout(), &registered),
            None
        );
        assert_eq!(
            classify_worktree_path(Path::new("/c/work/grove"), &layout(), &registered),
            None
        );
    }

    #[test]
    fn blocked_findings_are_never_applied() {
        let finding = Finding::new(Category::ExpiredEphemeral, "probe", "/tmp/probe")
            .remedy(Remedy::RemoveEphemeral {
                tag: Some("probe".to_string()),
                branch: None,
                head: None,
            })
            .block(["uncommitted changes (3 entries)".to_string()]);
        assert!(!finding.is_actionable());
        assert!(!finding.is_auto_applicable());
    }

    #[test]
    fn report_only_findings_are_never_applied() {
        let finding = Finding::new(
            Category::ForeignDirectory,
            "sc-support",
            "/c/work/desktop/x",
        );
        assert!(!finding.is_actionable(), "no remedy ⇒ nothing to apply");
    }

    #[test]
    fn branch_deletions_are_logged_as_dir_branch_sha() {
        let dir = tempfile::TempDir::new().unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        log_branch_deletion(
            &layout,
            Path::new("/c/work/desktop/wt-a"),
            "feature/a",
            "abc123",
        )
        .unwrap();
        log_branch_deletion(
            &layout,
            Path::new("/c/work/desktop/wt-b"),
            "feature/b",
            "def456",
        )
        .unwrap();

        let text = std::fs::read_to_string(layout.deleted_branches_log()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "log must append, not overwrite");
        assert_eq!(lines[0], "/c/work/desktop/wt-a feature/a abc123");
        assert_eq!(lines[1], "/c/work/desktop/wt-b feature/b def456");
    }

    #[test]
    fn layout_points_at_the_documented_directories() {
        let layout = layout();
        assert_eq!(layout.scratch_dir(), Path::new("/c/work/desktop/.scratch"));
        assert_eq!(
            layout.harness_dir(),
            Path::new("/c/work/desktop/master/.claude/worktrees")
        );
        assert_eq!(
            layout.deleted_branches_log(),
            Path::new("/c/work/desktop/.archive/deleted-branches.txt")
        );
    }
}
