use std::process::Command;

use tempfile::TempDir;

use grove::cli::repo::{AddArgs, run_add};
use grove::config::global::ReposManifest;
use grove::registry::Registry;

/// Initialize a bare git repo with one commit, return temp dir holding clone.
fn init_git_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path();
    for args in [
        vec!["init", path.to_str().unwrap()],
        vec![
            "-C",
            path.to_str().unwrap(),
            "config",
            "user.email",
            "t@t.com",
        ],
        vec!["-C", path.to_str().unwrap(), "config", "user.name", "T"],
    ] {
        Command::new("git").args(&args).status().unwrap();
    }
    std::fs::write(path.join("README.md"), b"hi").unwrap();
    Command::new("git")
        .args(["-C", path.to_str().unwrap(), "add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .args(["-C", path.to_str().unwrap(), "commit", "-m", "init"])
        .status()
        .unwrap();
    dir
}

fn make_add_args(
    repo_dir: &std::path::Path,
    config_dir: &std::path::Path,
    issue_prefix: Option<&str>,
    make_default: bool,
) -> AddArgs {
    AddArgs {
        path: repo_dir.to_path_buf(),
        id: None,
        issue_prefix: issue_prefix.map(|s| s.to_string()),
        upstream: None,
        fork: None,
        default_base: None,
        make_default,
        config_dir: config_dir.to_path_buf(),
    }
}

/// AC1: valid git repo not yet registered → repos.json entry + .grove/registry.json created.
#[test]
fn add_valid_repo_creates_entry_and_grove_dir() {
    let repo = init_git_repo();
    let config = TempDir::new().unwrap();
    std::fs::create_dir_all(config.path()).unwrap();

    let args = make_add_args(repo.path(), config.path(), None, false);
    run_add(&args).expect("run_add should succeed for a valid git repo");

    let manifest = ReposManifest::load(config.path()).unwrap();
    let id = repo.path().file_name().unwrap().to_str().unwrap();
    assert!(
        manifest.repos.contains_key(id),
        "repos.json should contain '{id}'"
    );

    let grove_dir = repo.path().join(".grove");
    assert!(grove_dir.exists(), ".grove directory should be created");
    let registry = Registry::load(&grove_dir).unwrap();
    assert!(
        registry.projects.is_empty(),
        "newly created registry should be empty"
    );
}

/// AC2: --default → repos.json default_repo set to new id.
#[test]
fn add_with_default_flag_sets_default_repo() {
    let repo = init_git_repo();
    let config = TempDir::new().unwrap();
    std::fs::create_dir_all(config.path()).unwrap();

    let args = make_add_args(repo.path(), config.path(), None, true);
    run_add(&args).expect("run_add should succeed");

    let manifest = ReposManifest::load(config.path()).unwrap();
    let id = repo.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(
        manifest.default_repo.as_deref(),
        Some(id),
        "default_repo should be set to '{id}'"
    );
}

/// AC3: --issue-prefix DEV → entry has issue_prefix="DEV"; without it, None.
#[test]
fn add_issue_prefix_stored_correctly() {
    let repo_with = init_git_repo();
    let repo_without = init_git_repo();
    let config = TempDir::new().unwrap();
    std::fs::create_dir_all(config.path()).unwrap();

    // With prefix
    let args = make_add_args(repo_with.path(), config.path(), Some("DEV"), false);
    run_add(&args).expect("run_add with prefix should succeed");

    // Without prefix (use explicit id to avoid basename collision)
    let config2 = TempDir::new().unwrap();
    std::fs::create_dir_all(config2.path()).unwrap();
    let args2 = make_add_args(repo_without.path(), config2.path(), None, false);
    run_add(&args2).expect("run_add without prefix should succeed");

    let manifest = ReposManifest::load(config.path()).unwrap();
    let id = repo_with.path().file_name().unwrap().to_str().unwrap();
    let entry = manifest.repos.get(id).unwrap();
    assert_eq!(
        entry.issue_prefix.as_deref(),
        Some("DEV"),
        "issue_prefix should be 'DEV'"
    );

    let manifest2 = ReposManifest::load(config2.path()).unwrap();
    let id2 = repo_without.path().file_name().unwrap().to_str().unwrap();
    let entry2 = manifest2.repos.get(id2).unwrap();
    assert!(
        entry2.issue_prefix.is_none(),
        "issue_prefix should be None when flag not passed"
    );
}

/// AC4: path is not a git repo → error NotAGitRepo.
#[test]
fn add_non_git_path_errors_not_a_git_repo() {
    let plain = TempDir::new().unwrap();
    let config = TempDir::new().unwrap();
    std::fs::create_dir_all(config.path()).unwrap();

    let args = make_add_args(plain.path(), config.path(), None, false);
    let err = run_add(&args).unwrap_err();
    // The error is wrapped in anyhow; check the source chain.
    let is_not_git = err
        .chain()
        .any(|e| e.to_string().contains("not a git repo"));
    assert!(is_not_git, "expected NotAGitRepo error, got: {err}");
}

/// AC5: id already exists → error DuplicateRepoId.
#[test]
fn add_duplicate_id_errors() {
    let repo = init_git_repo();
    let config = TempDir::new().unwrap();
    std::fs::create_dir_all(config.path()).unwrap();

    // First add succeeds.
    let args = make_add_args(repo.path(), config.path(), None, false);
    run_add(&args).expect("first add should succeed");

    // Second add with same path (same derived id) fails.
    let args2 = make_add_args(repo.path(), config.path(), None, false);
    let err = run_add(&args2).unwrap_err();
    let is_duplicate = err
        .chain()
        .any(|e| e.to_string().contains("already exists"));
    assert!(is_duplicate, "expected DuplicateRepoId error, got: {err}");
}
