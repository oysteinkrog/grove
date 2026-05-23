use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use grove::cli::new::{NewArgs, run};
use grove::config::global::{RepoEntry, ReposManifest};
use grove::registry::Registry;
use grove::repo::RepoContext;

/// Build a bare repo + working clone with a remote named `remote_name`.
///
/// Returns `(bare_dir, clone_dir)` — both are kept alive by the caller.
fn make_bare_and_clone(remote_name: &str) -> (TempDir, TempDir) {
    let bare = TempDir::new().unwrap();
    let clone = TempDir::new().unwrap();

    // Init bare repo
    let init_status = Command::new("git")
        .args(["init", "--bare", bare.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(init_status.status.success(), "git init --bare failed");

    // Clone it
    let clone_status = Command::new("git")
        .args([
            "clone",
            bare.path().to_str().unwrap(),
            clone.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(clone_status.status.success(), "git clone failed");

    // Configure user in clone
    for (key, val) in [("user.email", "test@test.com"), ("user.name", "Test")] {
        Command::new("git")
            .args(["-C", clone.path().to_str().unwrap(), "config", key, val])
            .status()
            .unwrap();
    }

    // Initial commit so master branch exists
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

    // Push to bare so remote tracking branch exists
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

    // Add a remote alias to the clone repo
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

fn branch_exists(repo: &Path, branch: &str) -> bool {
    let out = Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", "--list", branch])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    stdout.trim().contains(branch)
}

fn worktree_exists(repo: &Path, wt_path: &Path) -> bool {
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    text.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|p| Path::new(p) == wt_path)
    })
}

fn make_context(
    main_repo: &Path,
    work_dir: &Path,
    grove_dir: &Path,
    issue_prefix: Option<&str>,
    upstream_remote: &str,
    default_base: &str,
) -> RepoContext {
    let mut repos = BTreeMap::new();
    repos.insert(
        "test".to_string(),
        RepoEntry {
            main_repo: main_repo.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: upstream_remote.to_string(),
            fork_remote: "origin".to_string(),
            default_base: default_base.to_string(),
            issue_prefix: issue_prefix.map(|s| s.to_string()),
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
        upstream_remote: upstream_remote.to_string(),
        fork_remote: "origin".to_string(),
        default_base: default_base.to_string(),
        issue_prefix: issue_prefix.map(|s| s.to_string()),
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

/// AC1: grove new <tag> --issue N creates branch <PREFIX>-N-<tag> from remote default, worktree at work_dir/<tag>
#[test]
fn new_with_issue_creates_branch_and_worktree() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir_path = work_dir.path().join(".grove");

    let cx = make_context(
        clone.path(),
        work_dir.path(),
        &grove_dir_path,
        Some("DESKTOP"),
        "if",
        "master",
    );

    let args = NewArgs {
        tag: "lazy-vm".to_string(),
        issue: Some(9947),
        branch: None,
        base: None,
        no_fetch: true,
    };

    run(&args, &cx).expect("grove new should succeed");

    let expected_branch = "DESKTOP-9947-lazy-vm";
    let expected_wt = work_dir.path().join("lazy-vm");

    assert!(
        branch_exists(clone.path(), expected_branch),
        "branch {expected_branch} should exist"
    );
    assert!(
        worktree_exists(clone.path(), &expected_wt),
        "worktree should exist at {}",
        expected_wt.display()
    );

    // Registry should be updated
    let reg = Registry::load(&grove_dir_path).unwrap();
    let proj = reg.projects.get("lazy-vm").expect("project in registry");
    assert_eq!(proj.branch, expected_branch);
    assert_eq!(proj.issue, Some(9947));
    assert_eq!(proj.path, expected_wt);
}

/// AC2: no --issue and no --branch → branch name equals tag
#[test]
fn new_no_issue_no_branch_uses_tag_as_branch() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir_path = work_dir.path().join(".grove");

    let cx = make_context(
        clone.path(),
        work_dir.path(),
        &grove_dir_path,
        Some("DESKTOP"),
        "if",
        "master",
    );

    let args = NewArgs {
        tag: "myfeature".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
    };

    run(&args, &cx).expect("grove new should succeed");

    assert!(
        branch_exists(clone.path(), "myfeature"),
        "branch 'myfeature' should exist"
    );

    let reg = Registry::load(&grove_dir_path).unwrap();
    let proj = reg.projects.get("myfeature").expect("project in registry");
    assert_eq!(proj.branch, "myfeature");
    assert_eq!(proj.issue, None);
}

/// AC6: tag already exists in registry → error indicates duplicate with existing path
#[test]
fn new_duplicate_tag_returns_error() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir_path = work_dir.path().join(".grove");

    let cx = make_context(
        clone.path(),
        work_dir.path(),
        &grove_dir_path,
        None,
        "if",
        "master",
    );

    // First creation
    let args = NewArgs {
        tag: "alpha".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
    };
    run(&args, &cx).expect("first grove new should succeed");

    // Reload context so registry has the new entry
    let cx2 = make_context(
        clone.path(),
        work_dir.path(),
        &grove_dir_path,
        None,
        "if",
        "master",
    );
    // Second creation with same tag
    let args2 = NewArgs {
        tag: "alpha".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
    };
    let err = run(&args2, &cx2).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("alpha"),
        "error should mention the duplicate tag, got: {msg}"
    );
    assert!(
        msg.contains("already exists"),
        "error should indicate duplicate, got: {msg}"
    );
}
