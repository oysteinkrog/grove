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
    // ...and once it is gone, its snaps age out too.
    Registry::remove_on_disk_if(&grove_dir, "lane", |_| true).unwrap();
    let cx = context(&main, &work_dir);
    run_at(&args, &cx, hours(24 * 10)).unwrap();
    assert!(list_snaps(&work_dir, "lane").is_empty());
    assert!(!snaps_dir(&work_dir, "lane").exists());
    for e in std::fs::read_dir(work_dir.join(".archive"))
        .unwrap()
        .flatten()
    {
        btrfs::delete_subvolume(&e.path(), &[]).unwrap();
    }
}
