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
        ephemeral: false,
        ttl: None,
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
        ephemeral: false,
        ttl: None,
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
        ephemeral: false,
        ttl: None,
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
        ephemeral: false,
        ttl: None,
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

/// bd-grove-lifecycle-p0ur.6 AC: `--ephemeral` places the worktree under
/// `<work_dir>/.scratch/<tag>` and registers an expiry (default 14d applied
/// when `--ttl` is omitted).
#[test]
fn new_ephemeral_lands_under_scratch_with_default_ttl() {
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

    let before = time::OffsetDateTime::now_utc();
    let args = NewArgs {
        tag: "probe".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
        ephemeral: true,
        ttl: None,
    };
    run(&args, &cx).expect("grove new --ephemeral should succeed");
    let after = time::OffsetDateTime::now_utc();

    let expected_wt = work_dir.path().join(".scratch").join("probe");
    assert!(
        worktree_exists(clone.path(), &expected_wt),
        "ephemeral worktree should exist under .scratch at {}",
        expected_wt.display()
    );

    let reg = Registry::load(&grove_dir_path).unwrap();
    let proj = reg.projects.get("probe").expect("project in registry");
    assert_eq!(proj.path, expected_wt);
    let expires_at = proj
        .expires_at
        .expect("ephemeral project should have expires_at");
    let expected_min = before + time::Duration::days(14);
    let expected_max = after + time::Duration::days(14);
    assert!(
        expires_at >= expected_min && expires_at <= expected_max,
        "expires_at {expires_at} should be ~14d from creation ({expected_min}..={expected_max})"
    );
}

/// bd-grove-lifecycle-p0ur.6 AC: an explicit `--ttl` overrides the 14d default.
#[test]
fn new_ephemeral_with_explicit_ttl() {
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

    let before = time::OffsetDateTime::now_utc();
    let args = NewArgs {
        tag: "short-lived".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
        ephemeral: true,
        ttl: Some("48h".to_string()),
    };
    run(&args, &cx).expect("grove new --ephemeral --ttl 48h should succeed");

    let reg = Registry::load(&grove_dir_path).unwrap();
    let proj = reg
        .projects
        .get("short-lived")
        .expect("project in registry");
    let expires_at = proj
        .expires_at
        .expect("ephemeral project should have expires_at");
    assert!(
        (expires_at - before).whole_hours() <= 49 && (expires_at - before).whole_hours() >= 47,
        "expires_at should be ~48h from creation, got {}h",
        (expires_at - before).whole_hours()
    );
}

/// A plain (non-ephemeral) `grove new` never sets `expires_at`.
#[test]
fn new_non_ephemeral_has_no_expiry() {
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

    let args = NewArgs {
        tag: "durable".to_string(),
        issue: None,
        branch: None,
        base: None,
        no_fetch: true,
        ephemeral: false,
        ttl: None,
    };
    run(&args, &cx).expect("grove new should succeed");

    let reg = Registry::load(&grove_dir_path).unwrap();
    let proj = reg.projects.get("durable").expect("project in registry");
    assert!(proj.expires_at.is_none(), "durable project must not expire");
    assert_eq!(proj.path, work_dir.path().join("durable"));
}
