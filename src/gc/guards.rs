//! Safety guards for `grove gc`.
//!
//! Two independent gates stand between a finding and its removal:
//!
//! * [`TreeSafety`] — would removing this tree lose work? Answered from
//!   `git status --porcelain --untracked-files=all` and remote containment of
//!   HEAD. gc computes this itself rather than leaning on `grove done`'s check:
//!   the installed 0.1.0 binary was observed deleting untracked-only dirt
//!   without complaint, so gc treats `done`'s guard as unavailable.
//! * [`Liveness`] — is somebody using this tree right now? Answered from
//!   recent filesystem/commit activity, live process working directories, and
//!   agent-mail file reservations. A live tree is never touched, however clean.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use time::{Duration, OffsetDateTime};

use super::exec;

/// How long a probe of the agent-mail CLI may take before gc gives up on it.
const RESERVATION_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// Timeout for the cheap per-tree git queries (`rev-parse`, `log -1`).
pub const QUICK_GIT_TIMEOUT: StdDuration = StdDuration::from_secs(60);

/// Timeout for `git status`, which walks the whole worktree. Big trees on WSL1
/// drvfs routinely need a minute or more.
pub const STATUS_GIT_TIMEOUT: StdDuration = StdDuration::from_secs(300);

/// Whether a tree can be removed without losing work.
#[derive(Debug, Clone)]
pub struct TreeSafety {
    /// Uncommitted or untracked content, per `--untracked-files=all`.
    pub dirty: bool,
    /// Number of porcelain entries backing `dirty`.
    pub dirty_entries: usize,
    /// HEAD is not reachable from any remote-tracking branch.
    pub unpushed: bool,
    /// HEAD commit id, when the tree has one.
    pub head: Option<String>,
    /// Checked-out branch name, when not detached.
    pub branch: Option<String>,
}

impl TreeSafety {
    /// Blocker strings for whichever gates tripped. Empty means safe to remove.
    pub fn blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.dirty {
            out.push(format!(
                "uncommitted changes ({} entr{})",
                self.dirty_entries,
                if self.dirty_entries == 1 { "y" } else { "ies" }
            ));
        }
        if self.unpushed {
            out.push("commits not present on any remote".to_string());
        }
        out
    }
}

/// Inspect a git tree with gc's own checks.
///
/// `--no-optional-locks` stops `git status` from refreshing the on-disk index.
/// Two reasons, both load-bearing: the refresh takes `index.lock` in a tree
/// another agent may be mid-command in, and it bumps the index mtime — the very
/// signal [`check_liveness`] reads to decide whether a tree is in use, so gc's
/// own inspection would otherwise look exactly like somebody working.
pub fn inspect_tree(path: &Path) -> Result<TreeSafety, String> {
    let porcelain = exec::git(
        path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--no-renames",
        ],
        STATUS_GIT_TIMEOUT,
    )?;
    let dirty_entries = porcelain.lines().filter(|l| !l.trim().is_empty()).count();

    let head = exec::git(path, &["rev-parse", "HEAD"], QUICK_GIT_TIMEOUT)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let branch = exec::git(
        path,
        &["symbolic-ref", "--short", "-q", "HEAD"],
        QUICK_GIT_TIMEOUT,
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());

    // A tree with no commits at all has nothing to lose. Otherwise HEAD must be
    // reachable from some remote-tracking branch — that covers pushed branches,
    // merged branches, and detached HEADs left behind by a landed PR alike,
    // where an `ahead` count against a configured upstream would not.
    let unpushed = match &head {
        None => false,
        Some(oid) => commit_is_unpushed(path, oid),
    };

    Ok(TreeSafety {
        dirty: dirty_entries > 0,
        dirty_entries,
        unpushed,
        head,
        branch,
    })
}

/// Whether `oid` is absent from every remote-tracking branch.
///
/// Asked as `rev-list --no-walk <oid> --not --remotes` rather than the obvious
/// `branch -r --contains <oid>`. The two answer the same question — is this
/// commit reachable from `refs/remotes/**` — but `branch -r` pays for one
/// reachability query *per ref*, so its cost scales with the number of remote
/// refs rather than with the history being asked about. On the repo this tool
/// was built for (9,758 remote refs) that is 31s per call against 0.5s for the
/// rev-list form, and it was most of a 17-minute `grove gc` run.
///
/// A git failure is reported as "not unpushed": the caller uses this to decide
/// whether removal would lose work, and every other gate still applies. Erring
/// the other way would block every removal whenever git hiccuped.
pub fn commit_is_unpushed(path: &Path, oid: &str) -> bool {
    exec::git(
        path,
        &["rev-list", "--no-walk", oid, "--not", "--remotes"],
        QUICK_GIT_TIMEOUT,
    )
    .map(|out| !out.trim().is_empty())
    .unwrap_or(false)
}

/// Why gc considers a tree in use.
#[derive(Debug, Clone, Default)]
pub struct Liveness {
    pub reasons: Vec<String>,
}

impl Liveness {
    pub fn is_live(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// Run-wide facts gathered once and reused for every liveness check, so a
/// 60-worktree scan does not re-read `/proc` or re-probe agent-mail per item.
#[derive(Debug, Default)]
pub struct ProbeContext {
    /// Working directories of every process gc could read, minus its own.
    pub process_cwds: Vec<(u32, PathBuf)>,
    /// Paths covered by an active agent-mail file reservation.
    pub reserved_paths: BTreeSet<PathBuf>,
    /// Probes that could not answer, reported once at the end of the run.
    pub notes: Vec<String>,
}

impl ProbeContext {
    /// Gather process working directories and agent-mail reservations.
    pub fn gather(project_root: &Path) -> Self {
        let mut cx = Self {
            process_cwds: scan_process_cwds(),
            ..Default::default()
        };
        if cx.process_cwds.is_empty() {
            cx.notes
                .push("no process working directories readable (/proc unavailable); the live-session guard is reduced to mtime and commit age".to_string());
        }
        match probe_reservations(project_root) {
            Ok(paths) => cx.reserved_paths = paths,
            Err(note) => cx.notes.push(note),
        }
        cx
    }

    /// A context with no external probe data, for tests and degraded runs.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Decide whether `path` looks like somebody's live working tree.
///
/// `window` is the recency horizon (48h in production). A zero window disables
/// the age-based half of the check, which is what fixture tests want.
///
/// `activity` must be sampled *before* gc runs anything against the tree, and
/// is passed in rather than measured here for that reason: a caller that
/// inspected the tree first would otherwise be reading back the timestamps its
/// own inspection wrote.
pub fn check_liveness(
    path: &Path,
    now: OffsetDateTime,
    window: Duration,
    probes: &ProbeContext,
    activity: Option<OffsetDateTime>,
) -> Liveness {
    let mut liveness = Liveness::default();

    if window > Duration::ZERO {
        if let Some(touched) = activity
            && now - touched < window
        {
            liveness
                .reasons
                .push(format!("modified {}", humanize_age(now - touched)));
        }
        if let Some(committed) = last_commit_time(path)
            && now - committed < window
        {
            liveness
                .reasons
                .push(format!("committed {}", humanize_age(now - committed)));
        }
    }

    for (pid, cwd) in &probes.process_cwds {
        if cwd.starts_with(path) {
            liveness
                .reasons
                .push(format!("process {pid} has its cwd inside the tree"));
            break;
        }
    }

    for reserved in &probes.reserved_paths {
        if reserved.starts_with(path) {
            liveness
                .reasons
                .push("covered by an active agent-mail file reservation".to_string());
            break;
        }
    }

    liveness
}

/// Most recent filesystem activity for a tree: the directory's own mtime plus
/// the git index, which every mutating git command rewrites. The directory
/// mtime alone misses edits to existing files, so both are consulted.
pub fn last_filesystem_activity(path: &Path) -> Option<OffsetDateTime> {
    let mut newest = mtime(path);
    if let Some(git_dir) = resolve_git_dir(path) {
        for candidate in [git_dir.join("index"), git_dir.join("HEAD"), git_dir] {
            if let Some(t) = mtime(&candidate) {
                newest = Some(match newest {
                    Some(prev) if prev >= t => prev,
                    _ => t,
                });
            }
        }
    }
    newest
}

pub fn mtime(path: &Path) -> Option<OffsetDateTime> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(OffsetDateTime::from)
}

fn last_commit_time(path: &Path) -> Option<OffsetDateTime> {
    let out = exec::git(path, &["log", "-1", "--format=%ct"], QUICK_GIT_TIMEOUT).ok()?;
    let secs: i64 = out.trim().parse().ok()?;
    OffsetDateTime::from_unix_timestamp(secs).ok()
}

/// Resolve the git directory backing `path`, following the `gitdir:` pointer a
/// linked worktree keeps in its `.git` file.
pub fn resolve_git_dir(path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let pointer = contents
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
    let pointer = PathBuf::from(pointer.trim());
    if pointer.is_absolute() {
        Some(pointer)
    } else {
        Some(path.join(pointer))
    }
}

/// Read `/proc/<pid>/cwd` for every process we are allowed to inspect.
///
/// Unreadable entries (other users, WSL1 gaps, races with exiting processes)
/// are skipped silently — this guard can only ever add safety, so a partial
/// answer is still worth having. gc's own pid is excluded; a parent shell
/// sitting in the tree is deliberately *not*, since that is a live session.
fn scan_process_cwds() -> Vec<(u32, PathBuf)> {
    let self_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            out.push((pid, cwd));
        }
    }
    out
}

/// Ask the agent-mail CLI which paths are reserved.
///
/// The reservation payload's exact shape is owned by another tool and has
/// changed before, so rather than binding to field names gc harvests every
/// absolute-looking path string in the JSON. Over-collecting only makes the
/// guard more conservative; a reservation gc fails to see is the real risk.
fn probe_reservations(project_root: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let root = project_root.to_str().unwrap_or(".");
    let out = exec::run(
        "am",
        &["reservations", "--all", "--json", "--project", root],
        Some(project_root),
        RESERVATION_PROBE_TIMEOUT,
    )
    .map_err(|e| {
        format!("agent-mail reservations unavailable ({e}); live-session guard reduced")
    })?;

    if !out.success {
        let detail = out
            .stderr
            .trim()
            .lines()
            .next()
            .or_else(|| out.stdout.trim().lines().next())
            .unwrap_or("no detail")
            .to_string();
        return Err(format!(
            "agent-mail reservations unavailable ({detail}); live-session guard reduced"
        ));
    }

    let value: serde_json::Value = serde_json::from_str(&out.stdout).map_err(|e| {
        format!(
            "agent-mail reservations returned unparseable JSON ({e}); live-session guard reduced"
        )
    })?;
    let mut paths = BTreeSet::new();
    collect_paths(&value, &mut paths);
    Ok(paths)
}

fn collect_paths(value: &serde_json::Value, out: &mut BTreeSet<PathBuf>) {
    match value {
        serde_json::Value::String(s) => {
            if s.starts_with('/') && s.len() > 1 {
                out.insert(PathBuf::from(s));
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_paths(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_paths(v, out)),
        _ => {}
    }
}

/// Render a duration as a coarse "3h ago" / "5d ago" phrase.
pub fn humanize_age(age: Duration) -> String {
    let secs = age.whole_seconds().max(0);
    if secs < 90 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 90 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .output()
            .expect("git must be on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git_in_root(dir, &["init", "-b", "main", dir.to_str().unwrap()]);
        git_in(dir, &["config", "user.email", "t@t.com"]);
        git_in(dir, &["config", "user.name", "T"]);
    }

    fn git_in_root(_dir: &Path, args: &[&str]) {
        let out = Command::new("git").args(args).output().unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn clean_committed_tree_has_no_dirt() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "init"]);

        let safety = inspect_tree(dir.path()).unwrap();
        assert!(!safety.dirty, "committed tree should not be dirty");
        assert_eq!(safety.branch.as_deref(), Some("main"));
        assert!(safety.head.is_some());
    }

    #[test]
    fn untracked_only_dirt_still_counts_as_dirty() {
        // The specific failure mode gc must not inherit from `grove done`:
        // untracked-only dirt is real work and blocks removal.
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "init"]);
        std::fs::write(dir.path().join("scratch-notes.md"), b"unsaved thinking").unwrap();

        let safety = inspect_tree(dir.path()).unwrap();
        assert!(safety.dirty, "untracked file must mark the tree dirty");
        assert_eq!(safety.dirty_entries, 1);
        assert!(
            safety.blockers().iter().any(|b| b.contains("uncommitted")),
            "blockers: {:?}",
            safety.blockers()
        );
    }

    #[test]
    fn commit_with_no_remote_is_unpushed() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "init"]);

        let safety = inspect_tree(dir.path()).unwrap();
        assert!(safety.unpushed, "no remote at all ⇒ commits are unpushed");
        assert!(
            safety
                .blockers()
                .iter()
                .any(|b| b.contains("not present on any remote"))
        );
    }

    #[test]
    fn empty_repo_has_nothing_to_lose() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let safety = inspect_tree(dir.path()).unwrap();
        assert!(!safety.unpushed, "a repo with no commits is not unpushed");
        assert!(safety.head.is_none());
    }

    /// The `branch -r --contains` form [`commit_is_unpushed`] replaced, kept as
    /// a test-only oracle. The replacement is not a refactor: it decides whether
    /// `--yes` may delete a tree, so every case below asserts the two agree
    /// rather than asserting the new answer alone.
    fn unpushed_via_branch_contains(path: &Path, oid: &str) -> bool {
        let contains = exec::git(
            path,
            &["branch", "-r", "--contains", oid],
            QUICK_GIT_TIMEOUT,
        )
        .unwrap_or_default();
        contains.trim().is_empty()
    }

    fn assert_both_forms_agree(path: &Path, oid: &str, expected_unpushed: bool, case: &str) {
        let new = commit_is_unpushed(path, oid);
        let old = unpushed_via_branch_contains(path, oid);
        assert_eq!(
            new, old,
            "{case}: rev-list said unpushed={new}, branch -r --contains said {old}"
        );
        assert_eq!(new, expected_unpushed, "{case}: wrong verdict");
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap(), "rev-parse", rev])
            .output()
            .unwrap();
        assert!(out.status.success(), "rev-parse {rev} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, name: &str, body: &str, message: &str) -> String {
        std::fs::write(dir.join(name), body.as_bytes()).unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", message]);
        rev_parse(dir, "HEAD")
    }

    /// A local clone with a real `refs/remotes/**`, plus an upstream to push to.
    struct RemoteFixture {
        _upstream: TempDir,
        local: TempDir,
    }

    fn remote_fixture() -> RemoteFixture {
        let upstream = TempDir::new().unwrap();
        init_repo(upstream.path());
        // The fixture pushes to this repo's checked-out branch, which git
        // refuses by default. Its work tree is never read, so let it drift.
        git_in(
            upstream.path(),
            &["config", "receive.denyCurrentBranch", "ignore"],
        );
        commit_file(upstream.path(), "a.txt", "a", "init");

        let local = TempDir::new().unwrap();
        init_repo(local.path());
        git_in(
            local.path(),
            &["remote", "add", "origin", upstream.path().to_str().unwrap()],
        );
        git_in(local.path(), &["fetch", "origin"]);
        git_in(local.path(), &["reset", "--hard", "origin/main"]);

        RemoteFixture {
            _upstream: upstream,
            local,
        }
    }

    #[test]
    fn pushed_head_is_not_unpushed_under_either_form() {
        let fx = remote_fixture();
        let head = rev_parse(fx.local.path(), "HEAD");
        assert_both_forms_agree(fx.local.path(), &head, false, "tip matching origin/main");
    }

    #[test]
    fn local_commit_on_top_is_unpushed_under_either_form() {
        let fx = remote_fixture();
        let local_only = commit_file(fx.local.path(), "b.txt", "b", "local work");
        assert_both_forms_agree(fx.local.path(), &local_only, true, "unpushed local commit");
    }

    #[test]
    fn detached_head_on_a_remote_commit_is_not_unpushed() {
        // The case an `ahead` count against a configured upstream gets wrong:
        // no branch, no upstream, but the commit is on the remote all the same.
        let fx = remote_fixture();
        let on_remote = rev_parse(fx.local.path(), "origin/main");
        git_in(fx.local.path(), &["checkout", "--detach", &on_remote]);

        let safety = inspect_tree(fx.local.path()).unwrap();
        assert!(
            safety.branch.is_none(),
            "checkout --detach leaves no branch"
        );
        assert!(!safety.unpushed, "a detached HEAD on the remote is safe");
        assert_both_forms_agree(fx.local.path(), &on_remote, false, "detached on remote");
    }

    #[test]
    fn squash_merged_branch_tip_is_still_unpushed_under_either_form() {
        // A squash landed upstream rewrites the diff, so the local tip's own oid
        // is on no remote ref. Both forms must say so: recognising the *content*
        // as merged is `git cherry`'s job in category 7, not this guard's.
        let fx = remote_fixture();
        git_in(fx.local.path(), &["checkout", "-b", "feature"]);
        commit_file(fx.local.path(), "c.txt", "c1", "part one");
        let feature_tip = commit_file(fx.local.path(), "c.txt", "c2", "part two");

        // Upstream lands the same content as one commit with a new oid.
        git_in(fx.local.path(), &["checkout", "main"]);
        git_in(fx.local.path(), &["merge", "--squash", "feature"]);
        git_in(fx.local.path(), &["commit", "-m", "landed as one"]);
        git_in(fx.local.path(), &["push", "origin", "main"]);
        git_in(fx.local.path(), &["fetch", "origin"]);

        assert_both_forms_agree(
            fx.local.path(),
            &feature_tip,
            true,
            "squash-merged feature tip",
        );
        let squashed = rev_parse(fx.local.path(), "origin/main");
        assert_both_forms_agree(
            fx.local.path(),
            &squashed,
            false,
            "the squash commit itself",
        );
    }

    #[test]
    fn commit_reachable_only_from_a_remote_tag_is_unpushed_under_either_form() {
        // Neither form looks outside `refs/remotes/**`, so a fetched tag does not
        // make a commit safe to delete. Asserted because it is the case where a
        // naive "any ref reaches it" rewrite would silently start deleting work.
        let fx = remote_fixture();
        git_in(fx.local.path(), &["checkout", "-b", "tagged-only"]);
        let tagged = commit_file(fx.local.path(), "d.txt", "d", "only ever tagged");
        git_in(fx.local.path(), &["tag", "v1", &tagged]);
        git_in(fx.local.path(), &["push", "origin", "v1"]);
        git_in(fx.local.path(), &["checkout", "main"]);
        git_in(fx.local.path(), &["branch", "-D", "tagged-only"]);
        git_in(fx.local.path(), &["fetch", "origin", "--tags"]);

        assert!(
            !rev_parse(fx.local.path(), "refs/tags/v1").is_empty(),
            "the tag must survive for the case to mean anything"
        );
        assert_both_forms_agree(fx.local.path(), &tagged, true, "reachable only via a tag");
    }

    #[test]
    fn fresh_directory_is_live_within_the_window() {
        let dir = TempDir::new().unwrap();
        let liveness = check_liveness(
            dir.path(),
            OffsetDateTime::now_utc(),
            Duration::hours(48),
            &ProbeContext::empty(),
            last_filesystem_activity(dir.path()),
        );
        assert!(liveness.is_live(), "a just-created dir is recent activity");
        assert!(liveness.reasons[0].contains("modified"));
    }

    #[test]
    fn zero_window_disables_age_based_liveness() {
        let dir = TempDir::new().unwrap();
        let liveness = check_liveness(
            dir.path(),
            OffsetDateTime::now_utc(),
            Duration::ZERO,
            &ProbeContext::empty(),
            last_filesystem_activity(dir.path()),
        );
        assert!(!liveness.is_live(), "reasons: {:?}", liveness.reasons);
    }

    #[test]
    fn process_cwd_inside_the_tree_marks_it_live() {
        let dir = TempDir::new().unwrap();
        let probes = ProbeContext {
            process_cwds: vec![(4242, dir.path().join("src"))],
            ..Default::default()
        };
        let liveness = check_liveness(
            dir.path(),
            OffsetDateTime::now_utc(),
            Duration::ZERO,
            &probes,
            last_filesystem_activity(dir.path()),
        );
        assert!(liveness.is_live());
        assert!(
            liveness.reasons[0].contains("4242"),
            "{:?}",
            liveness.reasons
        );
    }

    #[test]
    fn process_cwd_beside_the_tree_does_not_mark_it_live() {
        let parent = TempDir::new().unwrap();
        let tree = parent.path().join("wt-foo");
        let sibling = parent.path().join("wt-foobar");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let probes = ProbeContext {
            process_cwds: vec![(7, sibling)],
            ..Default::default()
        };
        let liveness = check_liveness(
            &tree,
            OffsetDateTime::now_utc(),
            Duration::ZERO,
            &probes,
            last_filesystem_activity(&tree),
        );
        assert!(
            !liveness.is_live(),
            "a sibling path sharing a name prefix is not inside the tree: {:?}",
            liveness.reasons
        );
    }

    #[test]
    fn agent_mail_reservation_marks_the_tree_live() {
        let dir = TempDir::new().unwrap();
        let probes = ProbeContext {
            reserved_paths: [dir.path().join("src/main.rs")].into_iter().collect(),
            ..Default::default()
        };
        let liveness = check_liveness(
            dir.path(),
            OffsetDateTime::now_utc(),
            Duration::ZERO,
            &probes,
            last_filesystem_activity(dir.path()),
        );
        assert!(liveness.is_live());
        assert!(liveness.reasons[0].contains("reservation"));
    }

    #[test]
    fn reservation_paths_are_harvested_from_any_json_shape() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"reservations":[{"agent":"x","paths":["/c/work/desktop/wt-a/src"]}],
                "meta":{"root":"/c/work/desktop"},"count":1}"#,
        )
        .unwrap();
        let mut out = BTreeSet::new();
        collect_paths(&value, &mut out);
        assert!(out.contains(&PathBuf::from("/c/work/desktop/wt-a/src")));
        assert!(out.contains(&PathBuf::from("/c/work/desktop")));
        assert_eq!(
            out.len(),
            2,
            "non-path strings must not be collected: {out:?}"
        );
    }

    #[test]
    fn resolve_git_dir_follows_a_worktree_pointer() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&target).unwrap();
        let tree = dir.path().join("wt");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", target.display())).unwrap();
        assert_eq!(resolve_git_dir(&tree).as_deref(), Some(target.as_path()));
    }

    #[test]
    fn ages_read_as_coarse_phrases() {
        assert_eq!(humanize_age(Duration::seconds(5)), "just now");
        assert_eq!(humanize_age(Duration::minutes(30)), "30m ago");
        assert_eq!(humanize_age(Duration::hours(5)), "5h ago");
        assert_eq!(humanize_age(Duration::days(9)), "9d ago");
    }
}
