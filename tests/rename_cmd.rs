use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use time::OffsetDateTime;

use grove::cli::rename::{RenameArgs, run};
use grove::config::ResolvedConfig;
use grove::config::global::{RepoEntry, ReposManifest};
use grove::error::GroveError;
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

fn worktree_exists_at(repo: &Path, wt_path: &Path) -> bool {
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

fn add_worktree(repo: &Path, wt_path: &Path, branch: &str) {
    Command::new("git")
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
        .status()
        .unwrap();
}

fn make_context_with_project(
    main_repo: &Path,
    work_dir: &Path,
    grove_dir: &Path,
    tag: &str,
    branch: &str,
) -> RepoContext {
    let mut repos = BTreeMap::new();
    repos.insert(
        "test".to_string(),
        RepoEntry {
            main_repo: main_repo.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "origin".to_string(),
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
    let resolved = ResolvedConfig {
        main_repo: main_repo.to_path_buf(),
        work_dir: work_dir.to_path_buf(),
        dir_prefix: String::new(),
        upstream_remote: "origin".to_string(),
        fork_remote: "origin".to_string(),
        default_base: "master".to_string(),
        issue_prefix: None,
        launch: None,
    };

    std::fs::create_dir_all(grove_dir).unwrap();
    let mut registry = Registry::load(grove_dir).unwrap();

    let project_path = work_dir.join(tag);
    std::fs::create_dir_all(&project_path).unwrap();
    registry
        .insert(
            tag.to_string(),
            Project {
                path: project_path,
                branch: branch.to_string(),
                base: "origin/master".to_string(),
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

/// AC1: rename foo bar — registry key changes, directory moves, git worktree move invoked.
#[test]
fn rename_moves_directory_and_updates_registry() {
    let (_bare, clone) = make_bare_and_clone("origin");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    let old_wt = work_dir.path().join("foo");
    add_worktree(clone.path(), &old_wt, "foo-branch");

    // Rebuild context after worktree was added so registry has correct path
    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "foo",
        "foo-branch",
    );
    // Override registry entry to point to the real worktree path
    let mut registry = cx.registry.clone();
    let mut proj = registry.remove("foo").unwrap();
    proj.path = old_wt.clone();
    registry.insert("foo".to_string(), proj).unwrap();
    registry.save(&grove_dir).unwrap();
    let cx = RepoContext {
        registry: Registry::load(&grove_dir).unwrap(),
        ..cx
    };

    let args = RenameArgs {
        old_tag: "foo".to_string(),
        new_tag: "bar".to_string(),
        no_move: false,
    };
    run(&args, &cx).expect("rename should succeed");

    let new_wt = work_dir.path().join("bar");

    // Registry: foo gone, bar present with new path
    let reg = Registry::load(&grove_dir).unwrap();
    assert!(
        !reg.projects.contains_key("foo"),
        "old tag should be removed from registry"
    );
    let new_proj = reg
        .projects
        .get("bar")
        .expect("new tag should be in registry");
    assert_eq!(
        new_proj.path, new_wt,
        "registry path should point to new location"
    );

    // Git worktree: new path tracked, old path gone
    assert!(
        worktree_exists_at(clone.path(), &new_wt),
        "worktree should exist at new path"
    );
    assert!(
        !worktree_exists_at(clone.path(), &old_wt),
        "worktree should not exist at old path"
    );
}

/// AC2: --no-move — only the registry key changes; filesystem path stays.
#[test]
fn rename_no_move_only_updates_registry_key() {
    let (_bare, clone) = make_bare_and_clone("origin");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    let wt_path = work_dir.path().join("alpha");
    add_worktree(clone.path(), &wt_path, "alpha-branch");

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "alpha",
        "alpha-branch",
    );
    let mut registry = cx.registry.clone();
    let mut proj = registry.remove("alpha").unwrap();
    proj.path = wt_path.clone();
    registry.insert("alpha".to_string(), proj).unwrap();
    registry.save(&grove_dir).unwrap();
    let cx = RepoContext {
        registry: Registry::load(&grove_dir).unwrap(),
        ..cx
    };

    let args = RenameArgs {
        old_tag: "alpha".to_string(),
        new_tag: "beta".to_string(),
        no_move: true,
    };
    run(&args, &cx).expect("rename --no-move should succeed");

    // Registry: alpha gone, beta present with ORIGINAL path
    let reg = Registry::load(&grove_dir).unwrap();
    assert!(
        !reg.projects.contains_key("alpha"),
        "old tag should be removed"
    );
    let new_proj = reg.projects.get("beta").expect("new tag in registry");
    assert_eq!(
        new_proj.path, wt_path,
        "path should be unchanged with --no-move"
    );

    // Filesystem: original directory still at old path
    assert!(
        wt_path.exists(),
        "directory should still exist at original path"
    );
    // Git worktree still at old path (we didn't move it)
    assert!(
        worktree_exists_at(clone.path(), &wt_path),
        "git worktree should still be at original path"
    );
}

/// AC3: new tag already exists → DuplicateTag error.
#[test]
fn rename_duplicate_tag_returns_error() {
    let (_bare, clone) = make_bare_and_clone("origin");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

    // Add two projects to registry
    std::fs::create_dir_all(&grove_dir).unwrap();
    let mut registry = Registry::default();
    let p1 = work_dir.path().join("proj1");
    let p2 = work_dir.path().join("proj2");
    std::fs::create_dir_all(&p1).unwrap();
    std::fs::create_dir_all(&p2).unwrap();
    registry
        .insert(
            "proj1".to_string(),
            Project {
                path: p1,
                branch: "proj1".to_string(),
                base: "origin/master".to_string(),
                created: OffsetDateTime::now_utc(),
                issue: None,
                frozen: false,
            },
        )
        .unwrap();
    registry
        .insert(
            "proj2".to_string(),
            Project {
                path: p2,
                branch: "proj2".to_string(),
                base: "origin/master".to_string(),
                created: OffsetDateTime::now_utc(),
                issue: None,
                frozen: false,
            },
        )
        .unwrap();
    registry.save(&grove_dir).unwrap();

    let mut repos = BTreeMap::new();
    repos.insert(
        "test".to_string(),
        RepoEntry {
            main_repo: clone.path().to_path_buf(),
            work_dir: work_dir.path().to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "origin".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "master".to_string(),
            issue_prefix: None,
            launch: None,
        },
    );
    let cx = RepoContext {
        id: "test".to_string(),
        global: ReposManifest {
            schema_version: 1,
            default_repo: Some("test".to_string()),
            repos,
        },
        resolved: ResolvedConfig {
            main_repo: clone.path().to_path_buf(),
            work_dir: work_dir.path().to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "origin".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "master".to_string(),
            issue_prefix: None,
            launch: None,
        },
        registry: Registry::load(&grove_dir).unwrap(),
    };

    let args = RenameArgs {
        old_tag: "proj1".to_string(),
        new_tag: "proj2".to_string(),
        no_move: false,
    };

    let err = run(&args, &cx).unwrap_err();
    assert!(
        matches!(err, GroveError::DuplicateTag { ref tag, .. } if tag == "proj2"),
        "expected DuplicateTag error, got: {err:?}"
    );
}
