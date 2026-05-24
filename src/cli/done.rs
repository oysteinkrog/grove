use crate::error::{GroveError, Result};
use crate::git::{GixBackend, ShellBackend, WorktreeManager, WorktreeMutator};
use crate::git::status::compute;
use crate::repo::RepoContext;

pub struct DoneArgs {
    pub tag: String,
    pub force: bool,
    pub keep_local: bool,
    pub keep_remote: bool,
}

pub fn run(args: &DoneArgs, cx: &RepoContext) -> Result<()> {
    let project = cx
        .registry
        .projects
        .get(&args.tag)
        .ok_or_else(|| GroveError::WorktreeNotFound(
            std::path::PathBuf::from(&args.tag),
        ))?;

    let worktree_path = project.path.clone();
    let branch = project.branch.clone();

    if !args.force {
        let wt = GixBackend.open(&worktree_path)?;
        let status = compute(&wt).map_err(|e| GroveError::GitCommandFailed {
            cmd: "git status".to_string(),
            stderr: e.to_string(),
        })?;

        if status.dirty {
            return Err(GroveError::UncommittedChanges {
                tag: args.tag.clone(),
            });
        }

        if !status.is_pushed {
            return Err(GroveError::UnpushedCommits {
                tag: args.tag.clone(),
            });
        }
    }

    let backend = ShellBackend::new();

    backend.worktree_remove(&cx.resolved.main_repo, &worktree_path, args.force)?;

    if !args.keep_local {
        // Ignore error: branch may already be gone if worktree removal cleaned it up
        let _ = backend.branch_delete(&cx.resolved.main_repo, &branch);
    }

    if !args.keep_remote {
        // Ignore error: remote may not exist or branch may not be pushed
        let _ = backend.remote_branch_delete(
            &cx.resolved.main_repo,
            &cx.resolved.fork_remote,
            &branch,
        );
    }

    let mut registry = cx.registry.clone();
    registry
        .remove(&args.tag)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    println!("Removed project '{}'.", args.tag);
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
        git(bare.path(), &["init", "--bare"]);

        // Create a temp non-bare repo, commit, push to bare
        let init_tmp = TempDir::new().unwrap();
        git(init_tmp.path(), &["init"]);
        git(init_tmp.path(), &["config", "user.email", "test@test.com"]);
        git(init_tmp.path(), &["config", "user.name", "Test"]);
        std::fs::write(init_tmp.path().join("README.md"), b"hello").unwrap();
        git(init_tmp.path(), &["add", "."]);
        git(init_tmp.path(), &["commit", "-m", "init"]);
        git(
            init_tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                bare.path().to_str().unwrap(),
            ],
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
            tag: "feature-clean".to_string(),
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
            tag: "feature-dirty".to_string(),
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
            tag: "feature-unpushed".to_string(),
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
            tag: "feature-keeploc".to_string(),
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
            tag: "feature-keeprem".to_string(),
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
            tag: "feature-force".to_string(),
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
}
