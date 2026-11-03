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
                expires_at: None,
                owner: None,
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
        ephemeral: false,
        ttl: None,
        copy_source: false,
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
        ephemeral: false,
        ttl: None,
        copy_source: false,
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
        ephemeral: false,
        ttl: None,
        copy_source: false,
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
        ephemeral: false,
        ttl: None,
        copy_source: false,
    };

    let err = run(&args, &cx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("fork takes 1 or 2 arguments") || msg.contains("3"),
        "error should mention argument count, got: {msg}"
    );
}

/// bd-grove-lifecycle-p0ur.6 AC: `grove fork --ephemeral` places the new
/// worktree under `.scratch/` and registers an expiry, same as `grove new`.
#[test]
fn fork_ephemeral_lands_under_scratch_with_expiry() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");

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

    let before = time::OffsetDateTime::now_utc();
    let args = ForkArgs {
        positionals: vec!["src".to_string(), "scratch-wt".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
        ephemeral: true,
        ttl: Some("2w".to_string()),
        copy_source: false,
    };

    run(&args, &cx).expect("fork --ephemeral should succeed");

    let expected_wt = work_dir.path().join(".scratch").join("scratch-wt");
    assert!(
        worktree_exists(clone.path(), &expected_wt),
        "worktree should exist under .scratch at {}",
        expected_wt.display()
    );

    let reg = Registry::load(&grove_dir).unwrap();
    let proj = reg
        .projects
        .get("scratch-wt")
        .expect("new project in registry");
    assert_eq!(proj.path, expected_wt);
    let expires_at = proj
        .expires_at
        .expect("ephemeral fork should have expires_at");
    assert!(
        (expires_at - before).whole_days() >= 13 && (expires_at - before).whole_days() <= 14,
        "expires_at should be ~2w from creation, got {} days",
        (expires_at - before).whole_days()
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// True when `dir` is on a filesystem with reflinks, so the base copy path
/// runs instead of the plain `git worktree add` fallback.
fn reflink_works(dir: &Path) -> bool {
    let a = dir.join(".reflink-probe-a");
    let b = dir.join(".reflink-probe-b");
    std::fs::write(&a, b"x").unwrap();
    let ok = Command::new("cp")
        .args(["--reflink=always"])
        .arg(&a)
        .arg(&b)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    ok
}

/// A source lane at `<work_dir>/src` on branch `feature`, with one commit of
/// its own plus build output, an ignored file, an untracked file and an
/// uncommitted edit, none of which a fork from the base may carry.
fn live_source_lane(clone: &Path, work_dir: &Path) -> std::path::PathBuf {
    let src = work_dir.join("src");
    git_out(
        clone,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            src.to_str().unwrap(),
            "if/master",
        ],
    );
    std::fs::write(src.join(".gitignore"), "*.db\nbuild/\n").unwrap();
    std::fs::write(src.join("feature.txt"), "committed\n").unwrap();
    git_out(&src, &["add", "."]);
    git_out(&src, &["commit", "-q", "-m", "feature work"]);
    std::fs::create_dir_all(src.join("build")).unwrap();
    std::fs::write(src.join("build").join("out.bin"), "object code\n").unwrap();
    std::fs::write(src.join("local.db"), "local data\n").unwrap();
    std::fs::write(src.join("notes.txt"), "untracked\n").unwrap();
    std::fs::write(src.join("feature.txt"), "edited, not committed\n").unwrap();
    src
}

/// Step 1 of the grove-snapshot-lanes plan: by default a fork is made from the
/// clean base checkout and moved to the source branch, so it gets the source's
/// HEAD and none of its untracked, ignored or build files.
#[test]
fn fork_from_base_has_source_head_and_no_local_files() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let src = live_source_lane(clone.path(), work_dir.path());

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "src",
        "feature",
    );
    let args = ForkArgs {
        positionals: vec!["src".to_string(), "copy".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
        ephemeral: false,
        ttl: None,
        copy_source: false,
    };
    run(&args, &cx).expect("fork from base should succeed");

    let target = work_dir.path().join("copy");
    assert_eq!(
        git_out(&target, &["rev-parse", "HEAD"]),
        git_out(&src, &["rev-parse", "HEAD"]),
        "the fork must start at the source lane's HEAD"
    );
    assert_eq!(git_out(&target, &["branch", "--show-current"]), "copy");
    assert_eq!(
        std::fs::read_to_string(target.join("feature.txt")).unwrap(),
        "committed\n"
    );
    for local in ["build", "local.db", "notes.txt"] {
        assert!(
            !target.join(local).exists(),
            "{local} from the source lane must not reach the fork"
        );
    }
    assert_eq!(
        git_out(&target, &["status", "--porcelain", "--ignored"]),
        ""
    );
    if reflink_works(work_dir.path()) {
        assert!(
            grove::git::base::base_path(work_dir.path()).exists(),
            "with reflinks the fork must be copied from the base checkout"
        );
    }
}

/// `--copy-source` keeps the old behaviour: the source lane itself is copied,
/// so ignored build output comes along, while edits and untracked files are
/// still dropped.
#[test]
fn fork_copy_source_keeps_build_output() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let src = live_source_lane(clone.path(), work_dir.path());

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "src",
        "feature",
    );
    let args = ForkArgs {
        positionals: vec!["src".to_string(), "warm".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
        ephemeral: false,
        ttl: None,
        copy_source: true,
    };
    run(&args, &cx).expect("fork --copy-source should succeed");

    let target = work_dir.path().join("warm");
    assert_eq!(
        git_out(&target, &["rev-parse", "HEAD"]),
        git_out(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        std::fs::read_to_string(target.join("feature.txt")).unwrap(),
        "committed\n"
    );
    assert!(!target.join("notes.txt").exists());
    if reflink_works(work_dir.path()) {
        assert!(target.join("build").join("out.bin").exists());
        assert!(target.join("local.db").exists());
        assert!(
            !grove::git::base::base_path(work_dir.path()).exists(),
            "--copy-source must not touch the base checkout"
        );
    } else {
        eprintln!("no reflinks here: --copy-source fell back to a plain worktree");
    }
}

/// A source lane that moved off its registered branch (detached with a new
/// commit) is forked at the commit it has checked out.
#[test]
fn fork_starts_at_the_source_checkout_not_the_registered_branch() {
    let (_bare, clone) = make_bare_and_clone("if");
    let work_dir = TempDir::new().unwrap();
    let grove_dir = work_dir.path().join(".grove");
    let src = live_source_lane(clone.path(), work_dir.path());
    git_out(&src, &["checkout", "-q", "--detach"]);
    git_out(&src, &["commit", "-q", "-am", "detached work"]);
    let head = git_out(&src, &["rev-parse", "HEAD"]);
    assert_ne!(head, git_out(&src, &["rev-parse", "feature"]));

    let cx = make_context_with_project(
        clone.path(),
        work_dir.path(),
        &grove_dir,
        "if",
        "src",
        "feature",
    );
    let args = ForkArgs {
        positionals: vec!["src".to_string(), "moved".to_string()],
        issue: None,
        branch: None,
        no_fetch: true,
        ephemeral: false,
        ttl: None,
        copy_source: false,
    };
    run(&args, &cx).expect("fork should succeed");
    let target = work_dir.path().join("moved");
    assert_eq!(git_out(&target, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(target.join("feature.txt")).unwrap(),
        "edited, not committed\n"
    );
}
