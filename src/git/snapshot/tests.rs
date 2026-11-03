use super::*;
use crate::git::btrfs::tests::btrfs_scratch;
use std::process::Command;

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

struct On;
impl On {
    fn new() -> Self {
        FORCE_ENABLED.with(|f| f.set(Some(true)));
        btrfs::TEST_FLOOR_GIB.with(|f| f.set(Some(0)));
        On
    }
}
impl Drop for On {
    fn drop(&mut self) {
        FORCE_ENABLED.with(|f| f.set(None));
        btrfs::TEST_FLOOR_GIB.with(|f| f.set(None));
        FAIL_AT.with(|f| f.set(None));
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    work_dir: PathBuf,
    main: PathBuf,
    base: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Subvolumes left by a failed assertion; tempdir removes the rest.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.main)
            .args(["worktree", "prune"])
            .output();
    }
}

/// A work_dir on btrfs: `master/` with branches `main` and `feature`, and a
/// base checkout at `.grove/base` that is a subvolume, holding an ignored
/// file and an untracked file.
fn fixture() -> Option<Fixture> {
    let tmp = btrfs_scratch()?;
    let work_dir = tmp.path().to_path_buf();
    let main = work_dir.join("master");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join(".gitignore"), "*.db\n.scratch/\n").unwrap();
    std::fs::write(main.join("a.txt"), "one\n").unwrap();
    std::fs::write(main.join("same.txt"), "same\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "one"]);
    git(&main, &["branch", "feature"]);
    git(&main, &["checkout", "-q", "feature"]);
    std::fs::write(main.join("a.txt"), "feature\n").unwrap();
    std::fs::write(main.join("f.txt"), "new\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "feature"]);
    git(&main, &["checkout", "-q", "main"]);

    let base = work_dir.join(".grove").join("base");
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    btrfs::create_subvolume(&base).unwrap();
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            base.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::write(base.join("cache.db"), "warm\n").unwrap();
    std::fs::write(base.join("stray.txt"), "untracked\n").unwrap();
    Some(Fixture {
        _tmp: tmp,
        work_dir,
        main,
        base,
    })
}

fn index_bytes(worktree: &Path) -> Vec<u8> {
    let p = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    std::fs::read(p).unwrap()
}

fn make(f: &Fixture, target: &Path, branch: &str, start: &str) -> Result<(), GroveError> {
    let backend = ShellBackend::new();
    let probe = backend.snapshot_probe(&f.base, target).unwrap();
    backend.worktree_add_snapshot(
        &f.main,
        &f.base,
        target,
        Checkout::NewBranch { branch, start },
        &staging_dir(&f.work_dir),
        probe,
        None,
    )
}

#[test]
fn snapshot_lane_matches_branch_and_leaves_the_source_alone() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let (head, index) = (git(&f.base, &["rev-parse", "HEAD"]), index_bytes(&f.base));
    let target = f.work_dir.join(".scratch").join("lane");
    make(&f, &target, "lane", "feature").unwrap();

    assert!(btrfs::is_subvolume(&target));
    assert_eq!(
        std::fs::read_to_string(target.join("a.txt")).unwrap(),
        "feature\n"
    );
    assert!(target.join("f.txt").exists());
    assert_eq!(
        std::fs::read_to_string(target.join("cache.db")).unwrap(),
        "warm\n",
        "ignored files are kept"
    );
    assert!(
        !target.join("stray.txt").exists(),
        "untracked files are dropped"
    );
    assert_eq!(git(&target, &["status", "--porcelain"]), "");
    assert_eq!(git(&target, &["symbolic-ref", "--short", "HEAD"]), "lane");
    // Unchanged files kept their inode: no content was rewritten.
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(target.join("same.txt")).unwrap().ino(),
        std::fs::metadata(f.base.join("same.txt")).unwrap().ino()
    );

    assert_eq!(git(&f.base, &["rev-parse", "HEAD"]), head);
    assert_eq!(index_bytes(&f.base), index);
    assert_eq!(git(&f.base, &["status", "--porcelain"]), "?? stray.txt");
    let list = git(&f.main, &["worktree", "list", "--porcelain"]);
    assert!(list.contains(target.to_str().unwrap()));
    assert!(
        !list.contains("locked"),
        "the lane is unlocked once made: {list}"
    );
    assert!(
        btrfs::is_subvolume(&target.join(".scratch")),
        "every lane gets one"
    );
    assert_eq!(
        std::fs::read_dir(target.join(".scratch")).unwrap().count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(staging_dir(&f.work_dir)).unwrap().count(),
        0
    );
}

#[test]
fn a_failure_at_any_step_leaves_only_the_branch() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let (head, index) = (git(&f.base, &["rev-parse", "HEAD"]), index_bytes(&f.base));
    for (i, step) in [
        "after-add",
        "after-snapshot",
        "after-index",
        "after-gitfile",
        "after-rename",
        "after-reset",
    ]
    .into_iter()
    .enumerate()
    {
        FAIL_AT.with(|c| c.set(Some(step)));
        let target = f.work_dir.join(".scratch").join(format!("lane{i}"));
        let err = make(&f, &target, &format!("lane{i}"), "feature").unwrap_err();
        assert!(err.to_string().contains(step), "{step}: {err}");
        assert!(!target.exists(), "{step}: target left behind");
        assert_eq!(
            std::fs::read_dir(staging_dir(&f.work_dir)).unwrap().count(),
            0,
            "{step}: staging left behind"
        );
        let list = git(&f.main, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains(target.to_str().unwrap()), "{step}: {list}");
        git(&f.main, &["rev-parse", "--verify", &format!("lane{i}")]);
        assert_eq!(git(&f.base, &["rev-parse", "HEAD"]), head, "{step}");
        assert_eq!(index_bytes(&f.base), index, "{step}: source index changed");
    }
}

#[test]
fn probe_refuses_what_it_cannot_snapshot() {
    let Some(f) = fixture() else { return };
    let backend = ShellBackend::new();
    let target = f.work_dir.join(".scratch").join("x");
    FORCE_ENABLED.with(|c| c.set(Some(false)));
    assert!(
        backend.snapshot_probe(&f.base, &target).is_err(),
        "off by default"
    );
    let _on = On::new();
    assert!(backend.snapshot_probe(&f.base, &target).is_ok());
    // The main checkout is not a subvolume.
    assert!(backend.snapshot_probe(&f.main, &target).is_err());
    // Flags a copied index would carry without their config.
    git(&f.base, &["update-index", "--skip-worktree", "same.txt"]);
    let err = backend.snapshot_probe(&f.base, &target).unwrap_err();
    assert!(err.contains("skip-worktree"), "{err}");
    git(&f.base, &["update-index", "--no-skip-worktree", "same.txt"]);
    git(&f.base, &["update-index", "--assume-unchanged", "same.txt"]);
    assert!(backend.snapshot_probe(&f.base, &target).is_err());
    git(
        &f.base,
        &["update-index", "--no-assume-unchanged", "same.txt"],
    );
    assert!(backend.snapshot_probe(&f.base, &target).is_ok());
}

#[test]
fn no_space_is_an_error_and_leaves_nothing() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    btrfs::TEST_FLOOR_GIB.with(|c| c.set(Some(1 << 30)));
    let target = f.work_dir.join(".scratch").join("full");
    let err = make(&f, &target, "full", "main").unwrap_err();
    assert!(err.to_string().contains("floor"), "{err}");
    assert!(!target.exists());
    let list = git(&f.main, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(target.to_str().unwrap()));
}

#[test]
fn a_fork_of_a_lane_gets_its_own_empty_scratch_subvolume() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let lane = f.work_dir.join(".scratch").join("src");
    make(&f, &lane, "src", "main").unwrap();
    // The source lane has a nested worktree in its own .scratch subvolume,
    // which every snapshot lane gets.
    assert!(btrfs::is_subvolume(&lane.join(".scratch")));
    let nested = lane.join(".scratch").join("inner");
    git(
        &f.main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inner",
            nested.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::write(lane.join("edit.db"), "build output\n").unwrap();

    let backend = ShellBackend::new();
    let target = f.work_dir.join(".scratch").join("fork");
    let probe = backend.snapshot_probe(&lane, &target).unwrap();
    backend
        .worktree_add_snapshot(
            &f.main,
            &lane,
            &target,
            Checkout::NewBranch {
                branch: "fork",
                start: "src",
            },
            &staging_dir(&f.work_dir),
            probe,
            None,
        )
        .unwrap();
    assert!(btrfs::is_subvolume(&target.join(".scratch")));
    assert_eq!(
        std::fs::read_dir(target.join(".scratch")).unwrap().count(),
        0
    );
    assert!(
        target.join("edit.db").exists(),
        "warm build output comes along"
    );
    assert_eq!(git(&nested, &["status", "--porcelain"]), "");
    assert_eq!(git(&lane, &["status", "--porcelain"]), "");
}

#[test]
fn stale_staging_entries_are_swept() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let staging = staging_dir(&f.work_dir);
    std::fs::create_dir_all(&staging).unwrap();
    // A pid above pid_max: no such process.
    let stale = staging.join("old.999999999");
    btrfs::snapshot(&f.base, &stale, false).unwrap();
    let target = f.work_dir.join(".scratch").join("next");
    make(&f, &target, "next", "main").unwrap();
    assert!(!stale.exists());
}

#[test]
fn subvolume_lane_checks_fail_closed() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let backend = ShellBackend::new();
    let lane = f.work_dir.join(".scratch").join("rm");
    make(&f, &lane, "rm", "main").unwrap();
    assert!(backend.check_subvolume_lane(&f.main, &lane, false).is_ok());

    std::fs::write(lane.join("new.txt"), "x").unwrap();
    let err = backend
        .check_subvolume_lane(&f.main, &lane, false)
        .unwrap_err();
    assert!(err.contains("untracked"), "{err}");
    assert!(backend.check_subvolume_lane(&f.main, &lane, true).is_ok());
    std::fs::remove_file(lane.join("new.txt")).unwrap();

    std::fs::create_dir_all(lane.join(".scratch/inner")).unwrap();
    assert!(backend.check_subvolume_lane(&f.main, &lane, false).is_err());
    std::fs::remove_dir(lane.join(".scratch/inner")).unwrap();

    git(&f.main, &["worktree", "lock", lane.to_str().unwrap()]);
    let err = backend
        .check_subvolume_lane(&f.main, &lane, true)
        .unwrap_err();
    assert!(err.contains("locked"), "{err}");
    git(&f.main, &["worktree", "unlock", lane.to_str().unwrap()]);

    // A .git that names another lane's admin directory is refused.
    let other = f.work_dir.join(".scratch").join("other");
    make(&f, &other, "other", "main").unwrap();
    std::fs::copy(other.join(".git"), lane.join(".git")).unwrap();
    assert!(backend.check_subvolume_lane(&f.main, &lane, true).is_err());
}

/// Without user_subvol_rm_allowed, the current `git worktree remove` path
/// deletes a subvolume lane, nested `.scratch` subvolume included.
#[test]
fn git_worktree_remove_deletes_a_subvolume_lane() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let lane = f.work_dir.join(".scratch").join("gone");
    make(&f, &lane, "gone", "main").unwrap();
    assert!(btrfs::is_subvolume(&lane.join(".scratch")));
    std::fs::write(lane.join("cache.db"), "ignored\n").unwrap();
    btrfs::TEST_RM_ALLOWED.with(|c| c.set(Some(false)));
    let backend = ShellBackend::new();
    assert!(
        !backend
            .remove_subvolume_lane(&f.main, &lane, false)
            .unwrap()
    );
    btrfs::TEST_RM_ALLOWED.with(|c| c.set(None));
    git(
        &f.main,
        &["worktree", "remove", "--force", lane.to_str().unwrap()],
    );
    assert!(!lane.exists());
    let list = git(&f.main, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(lane.to_str().unwrap()));
}

/// With user_subvol_rm_allowed (skipped where it is not set): a clean lane
/// goes with `btrfs subvolume delete`, its .scratch subvolume first, and only
/// its own registration goes with it.
#[test]
fn fast_delete_removes_the_lane_and_only_its_registration() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    if !btrfs::rm_allowed(&f.work_dir) {
        eprintln!("skipping: user_subvol_rm_allowed is not set");
        return;
    }
    let backend = ShellBackend::new();
    let lane = f.work_dir.join(".scratch").join("fast");
    let other = f.work_dir.join(".scratch").join("other");
    make(&f, &lane, "fast", "feature").unwrap();
    make(&f, &other, "other", "main").unwrap();
    std::fs::write(lane.join("cache.db"), "ignored build output\n").unwrap();

    // Refused while dirty, untracked or locked; nothing deleted.
    std::fs::write(lane.join("a.txt"), "edit\n").unwrap();
    assert!(
        backend
            .remove_subvolume_lane(&f.main, &lane, false)
            .is_err()
    );
    git(&lane, &["checkout", "--", "a.txt"]);
    std::fs::write(lane.join("new.txt"), "x").unwrap();
    assert!(
        backend
            .remove_subvolume_lane(&f.main, &lane, false)
            .is_err()
    );
    std::fs::remove_file(lane.join("new.txt")).unwrap();
    git(&f.main, &["worktree", "lock", lane.to_str().unwrap()]);
    assert!(backend.remove_subvolume_lane(&f.main, &lane, true).is_err());
    git(&f.main, &["worktree", "unlock", lane.to_str().unwrap()]);
    assert!(lane.join("a.txt").exists());

    // A nested subvolume inside .scratch: --force deletes it too.
    btrfs::create_subvolume(&lane.join(".scratch").join("inner")).unwrap();
    assert!(
        backend
            .remove_subvolume_lane(&f.main, &lane, false)
            .is_err()
    );
    assert!(backend.remove_subvolume_lane(&f.main, &lane, true).unwrap());
    assert!(!lane.exists());
    let list = git(&f.main, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(lane.to_str().unwrap()), "{list}");
    assert!(list.contains(other.to_str().unwrap()));
    git(&f.main, &["rev-parse", "--verify", "fast"]);
    assert_eq!(git(&other, &["status", "--porcelain"]), "");

    // The plain clean case, no force.
    assert!(
        backend
            .remove_subvolume_lane(&f.main, &other, false)
            .unwrap()
    );
    assert!(!other.exists());
}

/// A run that died after registering the lane, before it had a directory,
/// leaves a locked registration. The next run removes exactly that one.
#[test]
fn a_dead_runs_locked_registration_is_removed() {
    let _on = On::new();
    let Some(f) = fixture() else { return };
    let dead = f.work_dir.join(".scratch").join("dead");
    git(
        &f.main,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            "--lock",
            "-b",
            "dead",
            dead.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::remove_file(dead.join(".git")).unwrap();
    std::fs::remove_dir(&dead).unwrap();
    let staging = staging_dir(&f.work_dir);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("dead.999999999.journal"),
        format!("{}\n", dead.display()),
    )
    .unwrap();
    // A live lane registered meanwhile is not touched.
    let keep = f.work_dir.join(".scratch").join("keep");
    make(&f, &keep, "keep", "main").unwrap();
    let list = git(&f.main, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(dead.to_str().unwrap()), "{list}");
    assert!(list.contains(keep.to_str().unwrap()));
    git(&f.main, &["rev-parse", "--verify", "dead"]);
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
}

#[test]
fn switch_values() {
    for v in ["1", "on", "ON", "true", "always", "on\n", " 1 "] {
        assert!(is_on(v), "{v:?}");
    }
    for v in ["", "0", "off", "false", "no", "yes"] {
        assert!(!is_on(v), "{v:?}");
    }
}
