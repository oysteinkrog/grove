//! `grove new` and `grove fork` with snapshot lanes on (`GROVE_SNAPSHOT=1`).
//!
//! Runs only when `GROVE_BTRFS_TEST_DIR` names a directory on btrfs; the
//! work dir and every subvolume go there. One test per binary, because it
//! sets `GROVE_SNAPSHOT` for the whole process.

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grove::config::global::{RepoEntry, ReposManifest};
use grove::git::btrfs;
use grove::registry::Registry;
use grove::repo::RepoContext;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn context(main: &Path, work_dir: &Path) -> RepoContext {
    let entry = RepoEntry {
        main_repo: main.to_path_buf(),
        work_dir: work_dir.to_path_buf(),
        dir_prefix: String::new(),
        upstream_remote: "if".to_string(),
        fork_remote: "if".to_string(),
        default_base: "master".to_string(),
        issue_prefix: None,
        launch: None,
    };
    let mut repos = BTreeMap::new();
    repos.insert("test".to_string(), entry);
    let grove_dir = work_dir.join(".grove");
    std::fs::create_dir_all(&grove_dir).unwrap();
    RepoContext {
        id: "test".to_string(),
        global: ReposManifest {
            schema_version: 1,
            default_repo: Some("test".to_string()),
            repos,
        },
        resolved: grove::config::ResolvedConfig {
            main_repo: main.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "if".to_string(),
            fork_remote: "if".to_string(),
            default_base: "master".to_string(),
            issue_prefix: None,
            launch: None,
        },
        registry: Registry::load(&grove_dir).unwrap(),
    }
}

fn new_lane(main: &Path, work_dir: &Path, tag: &str) {
    grove::cli::new::run(
        &grove::cli::new::NewArgs {
            tag: tag.to_string(),
            issue: None,
            branch: None,
            base: None,
            no_fetch: true,
            ephemeral: true,
            ttl: None,
        },
        &context(main, work_dir),
    )
    .unwrap();
}

fn fork(main: &Path, work_dir: &Path, src: &str, tag: &str, copy_source: bool) {
    grove::cli::fork::run(
        &grove::cli::fork::ForkArgs {
            positionals: vec![src.to_string(), tag.to_string()],
            issue: None,
            branch: None,
            no_fetch: true,
            ephemeral: true,
            ttl: None,
            copy_source,
        },
        &context(main, work_dir),
    )
    .unwrap();
}

#[test]
fn new_and_fork_make_snapshot_lanes() {
    common::isolate_machine_lock();
    let Some(root) = std::env::var_os("GROVE_BTRFS_TEST_DIR").map(PathBuf::from) else {
        eprintln!("skipping: GROVE_BTRFS_TEST_DIR is not set");
        return;
    };
    std::fs::create_dir_all(&root).unwrap();
    if !btrfs::is_btrfs(&root) {
        eprintln!("skipping: {} is not on btrfs", root.display());
        return;
    }
    // SAFETY: the only test in this binary, set before any grove code runs.
    unsafe { std::env::set_var(grove::git::snapshot::SNAPSHOT_ENV, "1") };

    let tmp = tempfile::tempdir_in(&root).unwrap();
    let bare = tmp.path().join("bare.git");
    let main = tmp.path().join("master");
    let work_dir = tmp.path().to_path_buf();
    Command::new("git")
        .args(["init", "-q", "--bare", "-b", "master"])
        .arg(&bare)
        .status()
        .unwrap();
    Command::new("git")
        .args(["clone", "-q", "-o", "if"])
        .arg(&bare)
        .arg(&main)
        .status()
        .unwrap();
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join(".gitignore"), "build/\n.scratch/\n").unwrap();
    std::fs::write(main.join("a.txt"), "one\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "one"]);
    git(&main, &["push", "-q", "if", "master"]);

    // new: a snapshot of the base, which became a subvolume.
    new_lane(&main, &work_dir, "lane1");
    let lane1 = work_dir.join(".scratch").join("lane1");
    let base = grove::git::base::base_path(&work_dir);
    assert!(btrfs::is_subvolume(&base));
    assert!(btrfs::is_subvolume(&lane1));
    assert_eq!(
        btrfs::subvol_info(&lane1).unwrap().parent_uuid,
        btrfs::subvol_info(&base).unwrap().uuid
    );
    assert_eq!(git(&lane1, &["status", "--porcelain"]), "");
    assert_eq!(git(&lane1, &["branch", "--show-current"]), "lane1");

    // Work in lane1: a commit and build output.
    std::fs::write(lane1.join("b.txt"), "work\n").unwrap();
    git(&lane1, &["add", "."]);
    git(&lane1, &["commit", "-q", "-m", "work"]);
    std::fs::create_dir_all(lane1.join("build")).unwrap();
    std::fs::write(lane1.join("build/out.bin"), "object\n").unwrap();
    let lane1_head = git(&lane1, &["rev-parse", "HEAD"]);

    // fork --copy-source: a snapshot of lane1, build output and all.
    fork(&main, &work_dir, "lane1", "warm", true);
    let warm = work_dir.join(".scratch").join("warm");
    assert_eq!(
        btrfs::subvol_info(&warm).unwrap().parent_uuid,
        btrfs::subvol_info(&lane1).unwrap().uuid
    );
    assert!(warm.join("build/out.bin").exists());
    assert_eq!(git(&warm, &["rev-parse", "HEAD"]), lane1_head);
    assert_eq!(git(&warm, &["status", "--porcelain"]), "");

    // fork: a snapshot of the base, moved to lane1's commit.
    fork(&main, &work_dir, "lane1", "cold", false);
    let cold = work_dir.join(".scratch").join("cold");
    assert_eq!(
        btrfs::subvol_info(&cold).unwrap().parent_uuid,
        btrfs::subvol_info(&base).unwrap().uuid
    );
    assert!(!cold.join("build").exists());
    assert_eq!(git(&cold, &["rev-parse", "HEAD"]), lane1_head);
    assert_eq!(git(&cold, &["status", "--porcelain"]), "");

    // The source lane is untouched.
    assert_eq!(git(&lane1, &["rev-parse", "HEAD"]), lane1_head);
    assert_eq!(git(&lane1, &["status", "--porcelain"]), "");
    assert_eq!(git(&lane1, &["branch", "--show-current"]), "lane1");

    // done on snapshot lanes. With user_subvol_rm_allowed the lane goes with
    // `btrfs subvolume delete`: a directory the user cannot write proves it,
    // since unlinking file by file would fail there.
    if btrfs::rm_allowed(&work_dir) {
        use std::os::unix::fs::PermissionsExt;
        let ro = cold.join("build");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("out.bin"), "x").unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let done = |tag: &str, force: bool| {
            grove::cli::done::run(
                &grove::cli::done::DoneArgs {
                    tag: Some(tag.to_string()),
                    force,
                    keep_local: true,
                    keep_remote: true,
                },
                &context(&main, &work_dir),
            )
        };
        // cold is at lane1's unpushed commit: refused without --force.
        assert!(done("cold", false).is_err());
        assert!(cold.exists());
        done("cold", true).unwrap();
        assert!(!cold.exists());
        let list = git(&main, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains(cold.to_str().unwrap()), "{list}");
        assert!(list.contains(warm.to_str().unwrap()));
        let registry = Registry::load(&work_dir.join(".grove")).unwrap();
        assert!(!registry.projects.contains_key("cold"));
        assert!(registry.projects.contains_key("warm"));
    } else {
        eprintln!("user_subvol_rm_allowed is not set: fast done not tested");
    }
}
