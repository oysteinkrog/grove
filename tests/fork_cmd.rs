use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use grove::cli::fork::{ForkArgs, run};
use grove::config::global::{RepoEntry, ReposManifest};
use grove::registry::Registry;
use grove::registry::project::Project;
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

fn make_context_with_project(
    main_repo: &Path,
    work_dir: &Path,
    grove_dir: &Path,
    upstream_remote: &str,
    source_tag: &str,
    source_branch: &str,
) -> RepoContext {
    use time::OffsetDateTime;

    let mut repos = BTreeMap::new();
    repos.insert(
        "test".to_string(),
        RepoEntry {
            main_repo: main_repo.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: upstream_remote.to_string(),
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
        upstream_remote: upstream_remote.to_string(),
        fork_remote: "origin".to_string(),
        default_base: "master".to_string(),
        issue_prefix: None,
        launch: None,
    };

    std::fs::create_dir_all(grove_dir).unwrap();
    let mut registry = Registry::load(grove_dir).unwrap();

    let source_path = work_dir.join(source_tag);
    std::fs::create_dir_all(&source_path).unwrap();
    registry
        .insert(
            source_tag.to_string(),
            Project {
                path: source_path,
                branch: source_branch.to_string(),
                base: format!("{upstream_remote}/master"),
                created: OffsetDateTime::now_utc(),
                issue: None,
                frozen: false,
            },
        )
        .unwrap();
    registry.save(grove_dir).unwrap();

    RepoContext {
        id: "test".to_string(),
        global,
        resolved,
        registry,
    }
}

/// AC1: 2 positionals — source and new tag both explicit.
#[test]
fn fork_two_positionals_explicit_source() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    // Create source branch in git
    Command::new("git")
        .args([
            "-C",
            clone.path().to_str().unwrap(),
            "checkout",
            "-b",
            "source-branch",
        ])
        .status()
        .unwrap();
    Command::new("git")
        .args(["-C", clone.path().to_str().unwrap(), "checkout", "master"])
        .status()
        .unwrap();

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "src",
        "source-branch",
    );

    let args = ForkArgs {
        positionals: vec!["src".to_string(), "new-wt".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
    };

    run(&args, &cx).expect("fork should succeed with 2 positionals");

    let expected_wt = work_dir.path().join("new-wt");
    assert!(
        branch_exists(clone.path(), "new-wt"),
        "branch 'new-wt' should exist"
    );
    assert!(
        worktree_exists(clone.path(), &expected_wt),
        "worktree should exist at {}",
        expected_wt.display()
    );

    let reg = Registry::load(&grove_dir).unwrap();
    let proj = reg.projects.get("new-wt").expect("new project in registry");
    assert_eq!(proj.branch, "new-wt");
    assert_eq!(proj.path, expected_wt);
}

/// AC2: 1 positional + cwd inside source project → source inferred from cwd.
#[test]
#[serial_test::serial]
fn fork_one_positional_infers_source_from_cwd() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    Command::new("git")
        .args([
            "-C",
            clone.path().to_str().unwrap(),
            "checkout",
            "-b",
            "src-branch",
        ])
        .status()
        .unwrap();
    Command::new("git")
        .args(["-C", clone.path().to_str().unwrap(), "checkout", "master"])
        .status()
        .unwrap();

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "mysrc",
        "src-branch",
    );

    let source_path = work_dir.path().join("mysrc");

    let old_cwd = std::env::var("GROVE_ORIG_CWD").ok();
    unsafe { std::env::set_var("GROVE_ORIG_CWD", source_path.to_str().unwrap()) };

    let args = ForkArgs {
        positionals: vec!["forked".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
    };

    let result = run(&args, &cx);

    match old_cwd {
        Some(v) => unsafe { std::env::set_var("GROVE_ORIG_CWD", v) },
        None => unsafe { std::env::remove_var("GROVE_ORIG_CWD") },
    }

    result.expect("fork with inferred source should succeed");

    let reg = Registry::load(&grove_dir).unwrap();
    let proj = reg
        .projects
        .get("forked")
        .expect("forked project in registry");
    assert_eq!(proj.branch, "forked");
}

/// AC3: 1 positional + cwd outside any project → error with hint.
#[test]
#[serial_test::serial]
fn fork_one_positional_cwd_outside_project_errors() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let unrelated = TempDir::new().unwrap();

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "myproj",
        "myproj-branch",
    );

    let old_cwd = std::env::var("GROVE_ORIG_CWD").ok();
    unsafe { std::env::set_var("GROVE_ORIG_CWD", unrelated.path().to_str().unwrap()) };

    let args = ForkArgs {
        positionals: vec!["newproj".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
    };

    let result = run(&args, &cx);

    match old_cwd {
        Some(v) => unsafe { std::env::set_var("GROVE_ORIG_CWD", v) },
        None => unsafe { std::env::remove_var("GROVE_ORIG_CWD") },
    }

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("pass source explicitly"),
        "error should hint to pass source explicitly, got: {msg}"
    );
}

/// AC4: 3+ positionals → handler returns an error (usage error).
#[test]
fn fork_three_positionals_returns_error() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "myproj",
        "myproj-branch",
    );

    let args = ForkArgs {
        positionals: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
    };

    let err = run(&args, &cx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("fork takes 1 or 2 arguments") || msg.contains("3"),
        "error should mention argument count, got: {msg}"
    );
}
