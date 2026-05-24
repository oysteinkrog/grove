use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use grove::cli::adopt::{AdoptArgs, run};
use grove::config::global::{RepoEntry, ReposManifest};
use grove::registry::Registry;
use grove::repo::RepoContext;

fn make_bare_and_clone(remote_name: &str) -> (TempDir, TempDir) {
    let bare = TempDir::new().unwrap();
    let clone = TempDir::new().unwrap();

    let init = Command::new("git")
        .args(["init", "--bare", bare.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(init.status.success(), "git init --bare failed");

    let clone_out = Command::new("git")
        .args([
            "clone",
            bare.path().to_str().unwrap(),
            clone.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(clone_out.status.success(), "git clone failed");

    for (key, val) in [("user.email", "test@test.com"), ("user.name", "Test")] {
        Command::new("git")
            .args(["-C", clone.path().to_str().unwrap(), "config", key, val])
            .status()
            .unwrap();
    }

    let readme = clone.path().join("README.md");
    std::fs::write(&readme, b"test").unwrap();
    Command::new("git")
        .args(["-C", clone.path().to_str().unwrap(), "add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .args(["-C", clone.path().to_str().unwrap(), "commit", "-m", "init"])
        .status()
        .unwrap();
    Command::new("git")
        .args([
            "-C",
            clone.path().to_str().unwrap(),
            "push",
            "origin",
            "master",
        ])
        .output()
        .unwrap();

    if remote_name != "origin" {
        Command::new("git")
            .args([
                "-C",
                clone.path().to_str().unwrap(),
                "remote",
                "add",
                remote_name,
                bare.path().to_str().unwrap(),
            ])
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", clone.path().to_str().unwrap(), "fetch", remote_name])
            .status()
            .unwrap();
    }

    (bare, clone)
}

/// Create a linked worktree rooted at `wt_path` in `repo`.
fn make_linked_worktree(repo: &Path, wt_path: &Path, branch: &str) {
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            branch,
            wt_path.to_str().unwrap(),
            "HEAD",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn make_context(main_repo: &Path, work_dir: &Path, grove_dir: &Path) -> RepoContext {
    let mut repos = BTreeMap::new();
    repos.insert(
        "test".to_string(),
        RepoEntry {
            main_repo: main_repo.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "if".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "master".to_string(),
            issue_prefix: None,
            launch: None,
        },
    );
    let global = ReposManifest {
        schema_version: 1,
        default_repo: Some("test".to_string()),
        repos,
    };
    let resolved = grove::config::ResolvedConfig {
        main_repo: main_repo.to_path_buf(),
        work_dir: work_dir.to_path_buf(),
        dir_prefix: String::new(),
        upstream_remote: "if".to_string(),
        fork_remote: "origin".to_string(),
        default_base: "master".to_string(),
        issue_prefix: None,
        launch: None,
    };
    std::fs::create_dir_all(grove_dir).unwrap();
    let registry = Registry::load(grove_dir).unwrap();

    RepoContext {
        id: "test".to_string(),
        global,
        resolved,
        registry,
    }
}

/// AC1: adopt a valid linked worktree — registers it in the registry pointing at its path.
#[test]
fn adopt_registers_valid_worktree() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let wt_dir = TempDir::new().unwrap();

    make_linked_worktree(clone.path(), wt_dir.path(), "feat-branch");

    let cx = make_context(clone.path(), work_dir.path(), &grove_dir);

    let args = AdoptArgs {
        tag: "foo".to_string(),
        path: wt_dir.path().to_path_buf(),
        issue: None,
        base: None,
        mv: false,
    };

    run(&args, &cx).expect("adopt should succeed for a valid worktree");

    let reg = Registry::load(&grove_dir).unwrap();
    let proj = reg
        .projects
        .get("foo")
        .expect("project should be in registry");
    assert_eq!(proj.path, wt_dir.path().canonicalize().unwrap());
    assert_eq!(proj.branch, "feat-branch");
}

/// AC2: adopt with --move relocates the directory to work_dir/<tag> before registering.
#[test]
fn adopt_move_relocates_directory() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let wt_dir = TempDir::new().unwrap();

    make_linked_worktree(clone.path(), wt_dir.path(), "move-branch");

    let cx = make_context(clone.path(), work_dir.path(), &grove_dir);
    let expected_dest = work_dir.path().join("bar");

    let args = AdoptArgs {
        tag: "bar".to_string(),
        path: wt_dir.path().to_path_buf(),
        issue: None,
        base: None,
        mv: true,
    };

    run(&args, &cx).expect("adopt --move should succeed");

    assert!(
        expected_dest.exists(),
        "directory should be at the new location"
    );

    let reg = Registry::load(&grove_dir).unwrap();
    let proj = reg
        .projects
        .get("bar")
        .expect("project should be in registry");
    assert_eq!(
        proj.path,
        expected_dest
            .canonicalize()
            .unwrap_or(expected_dest.clone())
    );
}

/// AC3: path is not a git worktree → error WorktreeInvalid.
#[test]
fn adopt_non_worktree_path_errors() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let plain_dir = TempDir::new().unwrap();

    let cx = make_context(clone.path(), work_dir.path(), &grove_dir);

    let args = AdoptArgs {
        tag: "notgit".to_string(),
        path: plain_dir.path().to_path_buf(),
        issue: None,
        base: None,
        mv: false,
    };

    let err = run(&args, &cx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not a git worktree"),
        "error should indicate invalid worktree, got: {msg}"
    );
}

/// AC4: tag already exists in registry → error DuplicateTag.
#[test]
fn adopt_duplicate_tag_errors() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let wt_dir1 = TempDir::new().unwrap();
    let wt_dir2 = TempDir::new().unwrap();

    make_linked_worktree(clone.path(), wt_dir1.path(), "first-branch");
    make_linked_worktree(clone.path(), wt_dir2.path(), "second-branch");

    let cx = make_context(clone.path(), work_dir.path(), &grove_dir);

    // First adopt succeeds.
    let args1 = AdoptArgs {
        tag: "dup".to_string(),
        path: wt_dir1.path().to_path_buf(),
        issue: None,
        base: None,
        mv: false,
    };
    run(&args1, &cx).expect("first adopt should succeed");

    // Reload context so registry has the new entry.
    let cx2 = make_context(clone.path(), work_dir.path(), &grove_dir);
    let args2 = AdoptArgs {
        tag: "dup".to_string(),
        path: wt_dir2.path().to_path_buf(),
        issue: None,
        base: None,
        mv: false,
    };

    let err = run(&args2, &cx2).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("dup") && msg.contains("already exists"),
        "error should indicate duplicate tag, got: {msg}"
    );
}
