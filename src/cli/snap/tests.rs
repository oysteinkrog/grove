use super::*;
use crate::config::global::ReposManifest;
use crate::registry::{Project, Registry};
use std::process::Command;
use time::macros::datetime;

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

#[test]
fn retention_keeps_hourly_daily_and_the_newest() {
    let now = datetime!(2026-11-10 12:30 UTC);
    let times = vec![
        datetime!(2026-11-01 08:00 UTC), // 9 days old: gone
        datetime!(2026-11-08 09:00 UTC), // newest of its day, within 3 days
        datetime!(2026-11-08 10:00 UTC),
        datetime!(2026-11-10 11:05 UTC), // same hour as the next: older one goes
        datetime!(2026-11-10 11:40 UTC),
        datetime!(2026-11-10 12:10 UTC),
    ];
    let keep = retained(&times, now, true);
    assert_eq!(keep, vec![false, false, true, false, true, true]);
    // An orphan's snaps all age out; a live lane keeps its newest.
    let late = datetime!(2026-12-01 00:00 UTC);
    assert_eq!(retained(&times, late, false), vec![false; 6]);
    assert!(*retained(&times, late, true).last().unwrap());
}

fn context(main: &Path, work_dir: &Path) -> RepoContext {
    let resolved = crate::config::ResolvedConfig {
        main_repo: main.to_path_buf(),
        work_dir: work_dir.to_path_buf(),
        dir_prefix: String::new(),
        upstream_remote: "if".into(),
        fork_remote: "if".into(),
        default_base: "main".into(),
        issue_prefix: None,
        launch: None,
    };
    RepoContext {
        id: "t".into(),
        global: ReposManifest {
            schema_version: 1,
            default_repo: None,
            repos: Default::default(),
        },
        resolved,
        registry: Registry::load(&work_dir.join(".grove")).unwrap(),
    }
}

#[test]
fn snap_skips_unchanged_lanes_restores_and_prunes() {
    let Some(tmp) = crate::git::btrfs::tests::btrfs_scratch() else {
        return;
    };
    let work_dir = tmp.path().to_path_buf();
    let main = work_dir.join("master");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("a.txt"), "one\n").unwrap();
    std::fs::write(main.join(".gitignore"), "*.db\n").unwrap();
    git(&main, &["add", "."]);
    git(
        &main,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "one",
        ],
    );
    let lane = work_dir.join("lane");
    btrfs::create_subvolume(&lane).unwrap();
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "lane",
            lane.to_str().unwrap(),
            "main",
        ],
    );
    let grove_dir = work_dir.join(".grove");
    std::fs::create_dir_all(&grove_dir).unwrap();
    let project = Project {
        path: lane.clone(),
        branch: "lane".into(),
        base: "main".into(),
        created: OffsetDateTime::now_utc(),
        issue: None,
        frozen: false,
        expires_at: None,
        owner: None,
    };
    Registry::insert_on_disk(&grove_dir, "lane".into(), project).unwrap();
    let cx = context(&main, &work_dir);
    let args = SnapArgs { dry_run: false };
    let t0 = datetime!(2026-11-03 10:00 UTC);
    let hours = |h: i64| t0 + time::Duration::hours(h);

    // Uncommitted work is what the snaps are for.
    std::fs::write(lane.join("a.txt"), "edited, not committed\n").unwrap();
    // Below the headroom floor: no snap, a warning, no failure.
    btrfs::TEST_FLOOR_GIB.with(|c| c.set(Some(1 << 30)));
    run_at(&args, &cx, t0).unwrap();
    assert!(list_snaps(&work_dir, "lane").is_empty());
    btrfs::TEST_FLOOR_GIB.with(|c| c.set(Some(0)));
    btrfs::create_subvolume(&lane.join(".scratch")).unwrap();
    run_at(&args, &cx, t0).unwrap();
    assert_eq!(list_snaps(&work_dir, "lane").len(), 1);
    run_at(&args, &cx, hours(1)).unwrap();
    assert_eq!(
        list_snaps(&work_dir, "lane").len(),
        1,
        "unchanged lane is not snapped again"
    );
    std::fs::write(lane.join("b.db"), "ignored output\n").unwrap();
    run_at(&args, &cx, hours(2)).unwrap();
    let snaps = list_snaps(&work_dir, "lane");
    assert_eq!(snaps.len(), 2, "an ignored file is a change too");
    assert!(btrfs::subvol_info(&snaps[0].path).unwrap().readonly);

    // Damage the lane, then restore one file and the whole lane.
    std::fs::remove_file(lane.join("a.txt")).unwrap();
    std::fs::write(lane.join("b.db"), "broken\n").unwrap();
    let first = snaps[0]
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    run_restore(&cx, "lane", &first, Some(Path::new("a.txt"))).unwrap();
    assert_eq!(
        std::fs::read_to_string(lane.join("a.txt")).unwrap(),
        "edited, not committed\n"
    );
    for bad in ["../x", "", ".git", "/etc/passwd"] {
        assert!(
            run_restore(&cx, "lane", &first, Some(Path::new(bad))).is_err(),
            "{bad}"
        );
    }
    // Twice within one second: both earlier versions are kept.
    run_restore(&cx, "lane", &first, Some(Path::new("a.txt"))).unwrap();
    run_restore(&cx, "lane", &first, Some(Path::new("a.txt"))).unwrap();
    let kept = std::fs::read_dir(&lane)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("a.txt.before-restore-")
        })
        .count();
    assert_eq!(kept, 2);
    for e in std::fs::read_dir(&lane).unwrap().flatten() {
        if e.file_name()
            .to_string_lossy()
            .starts_with("a.txt.before-restore-")
        {
            std::fs::remove_file(e.path()).unwrap();
        }
    }
    // A symlinked directory on the way is refused.
    std::os::unix::fs::symlink("/etc", lane.join("out")).unwrap();
    assert!(run_restore(&cx, "lane", &first, Some(Path::new("out/passwd"))).is_err());
    std::fs::remove_file(lane.join("out")).unwrap();

    let second = snaps[1]
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    run_restore(&cx, "lane", &second, None).unwrap();
    assert!(btrfs::is_subvolume(&lane));
    assert!(
        btrfs::is_subvolume(&lane.join(".scratch")),
        "restored as a subvolume"
    );
    assert_eq!(
        std::fs::read_to_string(lane.join("b.db")).unwrap(),
        "ignored output\n"
    );
    assert_eq!(git(&lane, &["status", "--porcelain"]), "M a.txt");
    assert_eq!(git(&lane, &["branch", "--show-current"]), "lane");
    let aside: Vec<_> = std::fs::read_dir(work_dir.join(".archive"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(aside.len(), 1, "the damaged lane is kept aside");
    assert!(
        !aside[0].path().join(".git").exists(),
        "the old tree is detached"
    );
    assert!(aside[0].path().join(".git.detached-by-grove").exists());

    // Pruning: days later a live lane keeps only its newest snap...
    std::fs::write(lane.join("c.txt"), "x").unwrap();
    run_at(&args, &cx, hours(3)).unwrap();
    assert_eq!(list_snaps(&work_dir, "lane").len(), 3);
    run_at(&args, &cx, hours(24 * 5)).unwrap();
    assert_eq!(list_snaps(&work_dir, "lane").len(), 1);
    // ...and once it is gone, its snaps go after the grace period.
    Registry::remove_on_disk_if(&grove_dir, "lane", |_| true).unwrap();
    let cx = context(&main, &work_dir);
    run_at(&args, &cx, hours(24 * 10)).unwrap();
    assert_eq!(list_snaps(&work_dir, "lane").len(), 1, "within grace");
    run_at(&args, &cx, hours(24 * 11)).unwrap();
    assert!(list_snaps(&work_dir, "lane").is_empty());
    assert!(!snaps_dir(&work_dir, "lane").exists());
    for e in std::fs::read_dir(work_dir.join(".archive"))
        .unwrap()
        .flatten()
    {
        btrfs::delete_subvolume(&e.path(), &[]).unwrap();
    }
}

fn registered(path: &Path) -> Project {
    Project {
        path: path.to_path_buf(),
        branch: "b".into(),
        base: "main".into(),
        created: OffsetDateTime::now_utc(),
        issue: None,
        frozen: false,
        expires_at: None,
        owner: None,
    }
}

/// A read-only snap of `src` for `tag` at `at`, as `grove snap` makes one.
fn fake_snap(work_dir: &Path, src: &Path, tag: &str, at: OffsetDateTime) -> PathBuf {
    let dir = snaps_dir(work_dir, tag);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join(at.format(NAME_FORMAT).unwrap());
    btrfs::snapshot(src, &dest, true).unwrap();
    dest
}

#[test]
fn gone_lane_snaps_are_deleted_after_the_grace_period_only() {
    let Some(tmp) = crate::git::btrfs::tests::btrfs_scratch() else {
        return;
    };
    let work_dir = tmp.path().to_path_buf();
    let main = work_dir.join("master");
    std::fs::create_dir_all(&main).unwrap();
    let grove_dir = work_dir.join(".grove");
    std::fs::create_dir_all(&grove_dir).unwrap();
    let src = work_dir.join("src");
    btrfs::create_subvolume(&src).unwrap();
    std::fs::write(src.join("work.txt"), "uncommitted\n").unwrap();
    // "live" is registered (a plain dir, so it is not snapped again);
    // "gone" is not. Both have a snap from a week before.
    let live_dir = work_dir.join("live");
    std::fs::create_dir_all(&live_dir).unwrap();
    Registry::insert_on_disk(&grove_dir, "live".into(), registered(&live_dir)).unwrap();
    let t0 = datetime!(2026-11-03 10:00 UTC);
    let week_ago = t0 - time::Duration::days(7);
    let live_snap = fake_snap(&work_dir, &src, "live", week_ago);
    let gone_snap = fake_snap(&work_dir, &src, "gone", week_ago);
    let cx = context(&main, &work_dir);
    let args = SnapArgs { dry_run: false };
    let grace = time::Duration::hours(24);
    let hours = |h: i64| t0 + time::Duration::hours(h);
    btrfs::TEST_FLOOR_GIB.with(|c| c.set(Some(0)));

    // First run that finds the lane gone: kept, even though the snap is
    // older than the daily schedule, and the time is recorded.
    run_with_grace(&args, &cx, t0, grace).unwrap();
    assert!(gone_snap.exists(), "within grace");
    assert!(gone_marker(&work_dir, "gone").is_file());
    assert!(live_snap.exists(), "a live lane keeps its newest snap");

    // A dry run past the grace period deletes nothing.
    run_with_grace(&SnapArgs { dry_run: true }, &cx, hours(25), grace).unwrap();
    assert!(gone_snap.exists());

    run_with_grace(&args, &cx, hours(23), grace).unwrap();
    assert!(gone_snap.exists(), "still within grace");

    // An unreadable registry past the grace period deletes nothing.
    let reg = grove_dir.join("registry.json");
    let good = std::fs::read(&reg).unwrap();
    std::fs::write(&reg, "{ not json").unwrap();
    assert!(run_with_grace(&args, &cx, hours(25), grace).is_err());
    assert!(gone_snap.exists());
    std::fs::write(&reg, &good).unwrap();

    run_with_grace(&args, &cx, hours(25), grace).unwrap();
    assert!(!gone_snap.exists(), "past grace: deleted");
    assert!(!snaps_dir(&work_dir, "gone").exists());
    assert!(live_snap.exists(), "the live lane is untouched");
    assert!(btrfs::subvol_info(&live_snap).unwrap().readonly);
    assert!(src.join("work.txt").exists(), "nothing outside snaps");

    // A tag that comes back and goes again between two runs gets a fresh
    // grace period: saving the registry with the tag clears its mark.
    let back = fake_snap(&work_dir, &src, "back", week_ago);
    run_with_grace(&args, &cx, hours(26), grace).unwrap();
    assert!(gone_marker(&work_dir, "back").is_file());
    Registry::insert_on_disk(&grove_dir, "back".into(), registered(&live_dir)).unwrap();
    assert!(!gone_marker(&work_dir, "back").exists());
    Registry::remove_on_disk_if(&grove_dir, "back", |_| true).unwrap();
    let cx = context(&main, &work_dir);
    run_with_grace(&args, &cx, hours(60), grace).unwrap();
    assert!(back.exists(), "fresh grace period after the tag left again");

    // A marker that is a symlink is replaced, never written through.
    let outside = work_dir.join("outside.txt");
    std::fs::write(&outside, "not a time\n").unwrap();
    std::fs::remove_file(gone_marker(&work_dir, "back")).unwrap();
    std::os::unix::fs::symlink(&outside, gone_marker(&work_dir, "back")).unwrap();
    run_with_grace(&args, &cx, hours(70), grace).unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "not a time\n");
    let marker = std::fs::symlink_metadata(gone_marker(&work_dir, "back")).unwrap();
    assert!(marker.is_file(), "the symlink was replaced by a file");
    assert!(back.exists(), "grace restarted at the replaced marker");
    run_with_grace(&args, &cx, hours(95), grace).unwrap();
    assert!(!back.exists());
    assert!(!snaps_dir(&work_dir, "back").exists());

    btrfs::delete_subvolume(&live_snap, &[]).unwrap();
    btrfs::delete_subvolume(&src, &[]).unwrap();
}

#[test]
fn unreadable_or_missing_registry_deletes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let work_dir = tmp.path().to_path_buf();
    let main = work_dir.join("master");
    std::fs::create_dir_all(&main).unwrap();
    let grove_dir = work_dir.join(".grove");
    std::fs::create_dir_all(&grove_dir).unwrap();
    let snap = snaps_dir(&work_dir, "gone").join("20261001T000000Z");
    std::fs::create_dir_all(&snap).unwrap();
    let cx = context(&main, &work_dir);
    let args = SnapArgs { dry_run: false };
    let late = datetime!(2026-12-01 00:00 UTC);
    let grace = time::Duration::hours(0);

    // Missing: elsewhere grove reads this as empty, which would make every
    // lane look gone.
    let err = run_with_grace(&args, &cx, late, grace).unwrap_err();
    assert!(
        err.to_string().contains("nothing snapped or deleted"),
        "{err}"
    );
    std::fs::write(grove_dir.join("registry.json"), "{ not json").unwrap();
    assert!(run_with_grace(&args, &cx, late, grace).is_err());
    assert!(snap.is_dir());
    assert!(!gone_marker(&work_dir, "gone").exists());
}

#[test]
fn grace_period_comes_from_the_environment() {
    // Read through a helper so the test does not race other tests on the
    // process environment.
    assert_eq!(DEFAULT_ORPHAN_GRACE_HOURS, 24);
    assert_eq!(parse_grace(Some("48")), time::Duration::hours(48));
    assert_eq!(parse_grace(Some(" 0 ")), time::Duration::hours(0));
    assert_eq!(parse_grace(Some("-3")), time::Duration::hours(24));
    assert_eq!(parse_grace(Some("x")), time::Duration::hours(24));
    assert_eq!(parse_grace(None), time::Duration::hours(24));
}
