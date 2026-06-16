use crate::error::{GroveError, Result};
use crate::git::status::compute;
use crate::git::{GixBackend, ShellBackend, WorktreeManager, WorktreeMutator};
use crate::repo::RepoContext;

pub struct DoneArgs {
    /// Worktree tag. When `None`, the tag is inferred from the current directory.
    pub tag: Option<String>,
    pub force: bool,
    pub keep_local: bool,
    pub keep_remote: bool,
}

/// Resolve the target tag: use the explicit argument when present, otherwise
/// infer it from the current working directory (the deepest registered project
/// whose path contains the cwd), mirroring `grove freeze` / `grove fork`.
fn resolve_tag(args: &DoneArgs, cx: &RepoContext) -> Result<String> {
    if let Some(ref tag) = args.tag {
        return Ok(tag.clone());
    }

    let cwd = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| GroveError::RepoDiscovery {
            hint: "cannot determine current directory; pass a tag explicitly: grove done <tag>"
                .to_string(),
        })?;

    let mut best: Option<(usize, String)> = None;
    for (tag, project) in &cx.registry.projects {
        if cwd.starts_with(&project.path) {
            let depth = project.path.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, tag.clone()));
            }
        }
    }

    best.map(|(_, tag)| tag).ok_or_else(|| GroveError::RepoDiscovery {
        hint: "cwd is not inside any known worktree; pass a tag explicitly: grove done <tag>"
            .to_string(),
    })
}

pub fn run(args: &DoneArgs, cx: &RepoContext) -> Result<()> {
    let tag = resolve_tag(args, cx)?;

    let project = cx
        .registry
        .projects
        .get(&tag)
        .ok_or_else(|| GroveError::WorktreeNotFound(std::path::PathBuf::from(&tag)))?;

    let worktree_path = project.path.clone();
    let branch = project.branch.clone();

    let backend = ShellBackend::new();

    if !args.force {
        let wt = GixBackend.open(&worktree_path)?;
        let status = compute(&wt).map_err(|e| GroveError::GitCommandFailed {
            cmd: "git status".to_string(),
            stderr: e.to_string(),
        })?;

        if status.dirty {
            return Err(GroveError::UncommittedChanges { tag: tag.clone() });
        }

        // `is_pushed` (ahead == 0 vs the tracked upstream) is the cheap happy
        // path. It is `false` whenever ahead/behind can't be computed — a
        // detached HEAD or a branch with no upstream — so before blocking, check
        // whether the worktree's HEAD commit is reachable from any remote
        // branch. A merged PR (HEAD contained in the base's remote) or a pushed
        // branch is safe to remove even when `is_pushed` is false.
        let safe = status.is_pushed
            || match wt.head() {
                Some(oid) => backend.commit_on_any_remote(&worktree_path, &oid.to_string())?,
                // No commits at all → nothing to lose.
                None => true,
            };

        if !safe {
            return Err(GroveError::UnpushedCommits { tag: tag.clone() });
        }
    }

    backend.worktree_remove(&cx.resolved.main_repo, &worktree_path, args.force)?;

    if !args.keep_local {
        // Ignore error: branch may already be gone if worktree removal cleaned it up
        let _ = backend.branch_delete(&cx.resolved.main_repo, &branch);
    }

    if !args.keep_remote {
        // Ignore error: remote may not exist or branch may not be pushed
        let _ =
            backend.remote_branch_delete(&cx.resolved.main_repo, &cx.resolved.fork_remote, &branch);
    }

    let mut registry = cx.registry.clone();
    registry
        .remove(&tag)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    println!("Removed project '{tag}'.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;

    use tempfile::TempDir;
    use time::OffsetDateTime;

    use super::*;
    use crate::config::ResolvedConfig;
    use crate::config::global::{RepoEntry, ReposManifest};
    use crate::registry::{Project, Registry};
    use crate::repo::RepoContext;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .status()
            .expect("git must be on PATH");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_bare_and_clone() -> (TempDir, TempDir) {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);

        // Create a temp non-bare repo, commit, push to bare
        let init_tmp = TempDir::new().unwrap();
        git(init_tmp.path(), &["init", "-b", "main"]);
        git(init_tmp.path(), &["config", "user.email", "test@test.com"]);
        git(init_tmp.path(), &["config", "user.name", "Test"]);
        std::fs::write(init_tmp.path().join("README.md"), b"hello").unwrap();
        git(init_tmp.path(), &["add", "."]);
        git(init_tmp.path(), &["commit", "-m", "init"]);
        git(
            init_tmp.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(init_tmp.path(), &["push", "-u", "origin", "main"]);

        // Clone the bare repo as our main repo
        let clone = TempDir::new().unwrap();
        Command::new("git")
            .args([
                "clone",
                bare.path().to_str().unwrap(),
                clone.path().to_str().unwrap(),
            ])
            .status()
            .unwrap();
        git(clone.path(), &["config", "user.email", "test@test.com"]);
        git(clone.path(), &["config", "user.name", "Test"]);

        (bare, clone)
    }

    fn make_worktree_on_branch(main_repo: &std::path::Path, branch: &str) -> TempDir {
        let wt = TempDir::new().unwrap();
        // Create and checkout a new branch in main repo as a worktree
        Command::new("git")
            .args([
                "-C",
                main_repo.to_str().unwrap(),
                "worktree",
                "add",
                "-b",
                branch,
                wt.path().to_str().unwrap(),
                "HEAD",
            ])
            .status()
            .unwrap();
        // Push the branch up so is_pushed reports true by default.
        // Tests that want unpushed state add commits after this.
        git(wt.path(), &["push", "-u", "origin", branch]);
        wt
    }

    fn make_context(
        main_repo: &std::path::Path,
        work_dir: &std::path::Path,
        projects: BTreeMap<String, Project>,
    ) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "test".to_string(),
            RepoEntry {
                main_repo: main_repo.to_path_buf(),
                work_dir: work_dir.to_path_buf(),
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
                fork_remote: "origin".to_string(),
                default_base: "main".to_string(),
                issue_prefix: None,
                launch: None,
            },
        );
        let global = ReposManifest {
            schema_version: 1,
            default_repo: Some("test".to_string()),
            repos,
        };
        let resolved = ResolvedConfig {
            main_repo: main_repo.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let grove_dir = work_dir.join(".grove");
        std::fs::create_dir_all(&grove_dir).unwrap();
        let registry = Registry {
            schema_version: 1,
            projects,
        };
        registry.save(&grove_dir).unwrap();
        RepoContext {
            id: "test".to_string(),
            global,
            resolved,
            registry,
        }
    }

    fn make_project(path: &std::path::Path, branch: &str) -> Project {
        Project {
            path: path.to_path_buf(),
            branch: branch.to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue: None,
            frozen: false,
        }
    }

    // AC1: clean worktree → removes everything, registry entry gone
    #[test]
    fn clean_worktree_removes_and_deregisters() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-clean");

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-clean".to_string(),
            make_project(wt.path(), "feature-clean"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-clean".to_string()),
            force: false,
            keep_local: false,
            keep_remote: false,
        };
        run(&args, &cx).expect("clean worktree should succeed");

        let loaded = Registry::load(&work_dir.path().join(".grove")).unwrap();
        assert!(
            !loaded.projects.contains_key("feature-clean"),
            "registry entry should be removed"
        );
    }

    // AC2: dirty worktree → returns error mentioning --force
    #[test]
    fn dirty_worktree_returns_error() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-dirty");

        // Make the worktree dirty
        std::fs::write(wt.path().join("dirty.txt"), b"uncommitted").unwrap();

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-dirty".to_string(),
            make_project(wt.path(), "feature-dirty"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-dirty".to_string()),
            force: false,
            keep_local: false,
            keep_remote: false,
        };
        let err = run(&args, &cx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--force"),
            "error should mention --force: {msg}"
        );
    }

    // AC3: unpushed commits → returns error mentioning --force
    #[test]
    fn unpushed_commits_returns_error() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-unpushed");

        // Set up tracking so ahead/behind can be computed
        git(
            wt.path(),
            &[
                "branch",
                "--set-upstream-to",
                "origin/main",
                "feature-unpushed",
            ],
        );

        // Make a commit in the worktree (not pushed)
        std::fs::write(wt.path().join("new.txt"), b"new content").unwrap();
        git(wt.path(), &["add", "."]);
        git(wt.path(), &["commit", "-m", "unpushed commit"]);

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-unpushed".to_string(),
            make_project(wt.path(), "feature-unpushed"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-unpushed".to_string()),
            force: false,
            keep_local: false,
            keep_remote: false,
        };
        let err = run(&args, &cx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--force"),
            "error should mention --force: {msg}"
        );
    }

    // AC4: --keep-local skips local branch deletion
    #[test]
    fn keep_local_skips_branch_deletion() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-keeploc");

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-keeploc".to_string(),
            make_project(wt.path(), "feature-keeploc"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-keeploc".to_string()),
            force: false,
            keep_local: true,
            keep_remote: false,
        };
        run(&args, &cx).expect("keep-local should succeed on clean worktree");

        // Branch should still exist locally
        let out = Command::new("git")
            .args([
                "-C",
                main_repo.path().to_str().unwrap(),
                "branch",
                "--list",
                "feature-keeploc",
            ])
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&out.stdout);
        assert!(
            branch_list.contains("feature-keeploc"),
            "local branch should still exist with --keep-local"
        );
    }

    // AC5: --keep-remote skips remote tracking deletion
    #[test]
    fn keep_remote_skips_remote_deletion() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-keeprem");

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-keeprem".to_string(),
            make_project(wt.path(), "feature-keeprem"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        // This just verifies the command doesn't error — we don't have a remote
        // branch to begin with so we just verify successful completion.
        let args = DoneArgs {
            tag: Some("feature-keeprem".to_string()),
            force: false,
            keep_local: false,
            keep_remote: true,
        };
        // Should succeed; remote branch deletion is skipped (and ignored if it fails)
        run(&args, &cx).expect("keep-remote flag should not cause error");
    }

    // AC6: --force bypasses all safety checks
    #[test]
    fn force_bypasses_safety_checks() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-force");

        // Make dirty (uncommitted) and add an unpushed commit
        std::fs::write(wt.path().join("dirty.txt"), b"uncommitted").unwrap();

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-force".to_string(),
            make_project(wt.path(), "feature-force"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-force".to_string()),
            force: true,
            keep_local: false,
            keep_remote: false,
        };
        run(&args, &cx).expect("--force should bypass all safety checks");

        let loaded = Registry::load(&work_dir.path().join(".grove")).unwrap();
        assert!(
            !loaded.projects.contains_key("feature-force"),
            "registry entry should be removed even with dirty state when --force used"
        );
    }

    // Regression: a detached-HEAD worktree whose commit is already on a remote
    // (e.g. PR merged, branch left in detached state) must NOT be reported as
    // having unpushed commits. `is_pushed` is false here (no branch → ahead is
    // None), so removal relies on the remote-containment fallback.
    #[test]
    fn detached_head_on_remote_is_not_unpushed() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-detached");

        // Detach HEAD at the (pushed) commit, mirroring a worktree left detached
        // after its branch was merged.
        git(wt.path(), &["checkout", "--detach", "HEAD"]);

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-detached".to_string(),
            make_project(wt.path(), "feature-detached"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let args = DoneArgs {
            tag: Some("feature-detached".to_string()),
            force: false,
            keep_local: false,
            keep_remote: false,
        };
        run(&args, &cx).expect("detached HEAD on a remote should be safe to remove");

        let loaded = Registry::load(&work_dir.path().join(".grove")).unwrap();
        assert!(
            !loaded.projects.contains_key("feature-detached"),
            "registry entry should be removed"
        );
    }

    // Issue 1: `grove done` with no tag infers the target from the current
    // directory (GROVE_ORIG_CWD), like `grove freeze` / `grove fork`.
    #[test]
    #[serial_test::serial]
    fn no_tag_resolves_from_cwd() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-cwd");

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-cwd".to_string(),
            make_project(wt.path(), "feature-cwd"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let old = std::env::var("GROVE_ORIG_CWD").ok();
        unsafe { std::env::set_var("GROVE_ORIG_CWD", wt.path()) };

        let args = DoneArgs {
            tag: None,
            force: false,
            keep_local: false,
            keep_remote: false,
        };
        let result = run(&args, &cx);

        match old {
            Some(v) => unsafe { std::env::set_var("GROVE_ORIG_CWD", v) },
            None => unsafe { std::env::remove_var("GROVE_ORIG_CWD") },
        }

        result.expect("done with no tag should resolve the worktree from cwd");

        let loaded = Registry::load(&work_dir.path().join(".grove")).unwrap();
        assert!(
            !loaded.projects.contains_key("feature-cwd"),
            "cwd-resolved worktree should be removed"
        );
    }

    // The headline use case: `grove done --force` run from inside a dirty
    // worktree, with no tag — resolves the worktree from cwd and force-removes
    // it, skipping the dirty/unpushed safety checks.
    #[test]
    #[serial_test::serial]
    fn no_tag_force_removes_dirty_worktree_from_cwd() {
        let (_bare, main_repo) = init_bare_and_clone();
        let work_dir = TempDir::new().unwrap();
        let wt = make_worktree_on_branch(main_repo.path(), "feature-cwdforce");

        // Untracked file → would block a non-forced done.
        std::fs::write(wt.path().join("scratch.txt"), b"wip").unwrap();

        let mut projects = BTreeMap::new();
        projects.insert(
            "feature-cwdforce".to_string(),
            make_project(wt.path(), "feature-cwdforce"),
        );
        let cx = make_context(main_repo.path(), work_dir.path(), projects);

        let old = std::env::var("GROVE_ORIG_CWD").ok();
        unsafe { std::env::set_var("GROVE_ORIG_CWD", wt.path()) };

        let args = DoneArgs {
            tag: None,
            force: true,
            keep_local: false,
            keep_remote: false,
        };
        let result = run(&args, &cx);

        match old {
            Some(v) => unsafe { std::env::set_var("GROVE_ORIG_CWD", v) },
            None => unsafe { std::env::remove_var("GROVE_ORIG_CWD") },
        }

        result.expect("done --force with no tag should remove the cwd worktree");

        let loaded = Registry::load(&work_dir.path().join(".grove")).unwrap();
        assert!(
            !loaded.projects.contains_key("feature-cwdforce"),
            "force-removed cwd worktree should be deregistered"
        );
    }
}
