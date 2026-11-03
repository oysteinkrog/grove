//! `grove snap`: hourly read-only snapshots of lanes that are subvolumes.
//!
//! Snapper's hourly snapshots of `@home` do not include nested subvolumes,
//! so a lane that is a subvolume drops out of the only undo net for
//! uncommitted work. `grove snap`, run from a timer, puts it back:
//!
//! - a read-only snapshot of each registered lane that is a subvolume and
//!   changed since its last snap (the subvolume's `ctransid` moved; a git
//!   status would miss ignored files and nested trees),
//! - kept at `<work_dir>/.grove/snaps/<tag>/<UTC time>`,
//! - pruned to the newest snap per hour for 24 hours and per day for 3 days,
//!   and always the newest one of a lane that is still registered.
//!
//! `grove done` leaves a lane's snaps alone; they age out on this schedule,
//! as snapper would have kept the lane. The index, HEAD and config of a lane
//! live in the main repo's `.git`, which snapper still covers.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use crate::git::btrfs;
use crate::repo::RepoContext;

const NAME_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

const KEEP_HOURLY: i64 = 24;
const KEEP_DAILY: i64 = 3;

/// Where the snaps of `tag` live.
pub fn snaps_dir(work_dir: &Path, tag: &str) -> PathBuf {
    work_dir.join(".grove").join("snaps").join(tag)
}

/// One snap on disk.
#[derive(Debug, Clone)]
pub struct Snap {
    pub path: PathBuf,
    pub time: OffsetDateTime,
}

/// The snaps of `tag`, oldest first. Entries whose name is not a snap time
/// are skipped.
pub fn list_snaps(work_dir: &Path, tag: &str) -> Vec<Snap> {
    let mut snaps: Vec<Snap> = std::fs::read_dir(snaps_dir(work_dir, tag))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let time = time::PrimitiveDateTime::parse(&name, NAME_FORMAT)
                .ok()?
                .assume_utc();
            Some(Snap {
                path: e.path(),
                time,
            })
        })
        .collect();
    snaps.sort_by_key(|s| s.time);
    snaps
}

/// Which of `times` (oldest first) to keep at `now`: the newest per hour for
/// the last 24 hours, the newest per day for the last 3 days, and the newest
/// overall when `keep_newest`.
pub fn retained(times: &[OffsetDateTime], now: OffsetDateTime, keep_newest: bool) -> Vec<bool> {
    let mut keep = vec![false; times.len()];
    let mut seen_hours = std::collections::HashSet::new();
    let mut seen_days = std::collections::HashSet::new();
    for (i, t) in times.iter().enumerate().rev() {
        let age = now - *t;
        let hour = t.unix_timestamp().div_euclid(3600);
        let day = t.unix_timestamp().div_euclid(86_400);
        if age < time::Duration::hours(KEEP_HOURLY) && seen_hours.insert(hour) {
            keep[i] = true;
        }
        if age < time::Duration::days(KEEP_DAILY) && seen_days.insert(day) {
            keep[i] = true;
        }
    }
    if keep_newest && let Some(last) = keep.last_mut() {
        *last = true;
    }
    keep
}

pub struct SnapArgs {
    pub dry_run: bool,
}

/// Snap every changed subvolume lane of the repo, then prune old snaps.
pub fn run(args: &SnapArgs, cx: &RepoContext) -> Result<()> {
    run_at(args, cx, OffsetDateTime::now_utc())
}

pub fn run_at(args: &SnapArgs, cx: &RepoContext, now: OffsetDateTime) -> Result<()> {
    let work_dir = &cx.resolved.work_dir;
    let registry = crate::registry::Registry::load(&cx.grove_dir())?;
    let mut failures = 0;
    for (tag, project) in &registry.projects {
        if !btrfs::is_subvolume(&project.path) {
            continue;
        }
        match snap_lane(work_dir, tag, &project.path, now, args.dry_run) {
            Ok(Some(path)) => println!("snap {tag}: {}", path.display()),
            Ok(None) => {}
            Err(e) => {
                failures += 1;
                eprintln!("snap {tag}: {e}");
            }
        }
    }
    let root = work_dir.join(".grove").join("snaps");
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let tag = entry.file_name().to_string_lossy().into_owned();
        let live = registry.projects.contains_key(&tag);
        if let Err(e) = prune(work_dir, &tag, now, live, args.dry_run) {
            failures += 1;
            eprintln!("prune {tag}: {e}");
        }
    }
    if failures > 0 {
        bail!("{failures} lane(s) failed");
    }
    Ok(())
}

/// Snap one lane when it changed since its newest snap. Returns the new
/// snap's path, or `None` when it was unchanged.
fn snap_lane(
    work_dir: &Path,
    tag: &str,
    lane: &Path,
    now: OffsetDateTime,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    let info = btrfs::subvol_info(lane)?;
    let snaps = list_snaps(work_dir, tag);
    if let Some(last) = snaps.last()
        && let Ok(prev) = btrfs::subvol_info(&last.path)
        && prev.parent_uuid == info.uuid
        && prev.ctransid == info.ctransid
    {
        return Ok(None);
    }
    let name = now.format(NAME_FORMAT)?;
    let dest = snaps_dir(work_dir, tag).join(&name);
    if dest.exists() {
        return Ok(None);
    }
    if dry_run {
        println!("would snap {tag} to {}", dest.display());
        return Ok(None);
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    btrfs::snapshot(lane, &dest, true).map_err(|e| anyhow!("{e}"))?;
    Ok(Some(dest))
}

fn prune(work_dir: &Path, tag: &str, now: OffsetDateTime, live: bool, dry_run: bool) -> Result<()> {
    let snaps = list_snaps(work_dir, tag);
    let times: Vec<_> = snaps.iter().map(|s| s.time).collect();
    let keep = retained(&times, now, live);
    for (snap, keep) in snaps.iter().zip(keep) {
        if keep {
            continue;
        }
        if dry_run {
            println!("would delete {}", snap.path.display());
            continue;
        }
        // Without user_subvol_rm_allowed this unlinks every file: as heavy
        // as a lane delete, so one at a time and not under I/O pressure.
        let lock = (!btrfs::rm_allowed(&snap.path))
            .then(|| crate::machine_lock::acquire_paced("delete", tag))
            .transpose()
            .map_err(|e| anyhow!(e))?;
        btrfs::delete_subvolume(&snap.path, &[]).map_err(|e| anyhow!("{e}"))?;
        drop(lock);
        println!("deleted {}", snap.path.display());
    }
    let dir = snaps_dir(work_dir, tag);
    if std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none()) {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

/// Find the snap of `tag` named `name` (its UTC time, as `grove snap list`
/// prints it).
fn find_snap(work_dir: &Path, tag: &str, name: &str) -> Result<Snap> {
    list_snaps(work_dir, tag)
        .into_iter()
        .find(|s| s.path.file_name().is_some_and(|n| n == name))
        .ok_or_else(|| anyhow!("no snap {name} of '{tag}'; see `grove snap list {tag}`"))
}

/// Print the snaps of `tag`, oldest first.
pub fn run_list(cx: &RepoContext, tag: &str) -> Result<()> {
    for snap in list_snaps(&cx.resolved.work_dir, tag) {
        println!("{}", snap.path.display());
    }
    Ok(())
}

/// Restore from snap `name` of `tag`: one file (`file`, relative to the lane)
/// or, without it, the whole lane. Nothing is overwritten: the current file
/// or lane is moved aside first, and the message says where.
pub fn run_restore(cx: &RepoContext, tag: &str, name: &str, file: Option<&Path>) -> Result<()> {
    let work_dir = &cx.resolved.work_dir;
    let project = cx
        .registry
        .projects
        .get(tag)
        .ok_or_else(|| anyhow!("'{tag}' is not registered"))?;
    let lane = &project.path;
    let snap = find_snap(work_dir, tag, name)?;
    let stamp = OffsetDateTime::now_utc().format(NAME_FORMAT)?;

    if let Some(rel) = file {
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            bail!("give the file as a plain path inside the lane");
        }
        let from = snap.path.join(rel);
        if !from.exists() {
            bail!("{} is not in the snap", rel.display());
        }
        let to = lane.join(rel);
        if to.exists() || to.is_symlink() {
            let aside = to.with_file_name(format!(
                "{}.before-restore-{stamp}",
                to.file_name().unwrap_or_default().to_string_lossy()
            ));
            std::fs::rename(&to, &aside)?;
            println!("moved the current file to {}", aside.display());
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let out = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(&from)
            .arg(&to)
            .output()?;
        if !out.status.success() {
            bail!("cp: {}", String::from_utf8_lossy(&out.stderr));
        }
        println!("restored {} from {}", to.display(), snap.path.display());
        return Ok(());
    }

    // The whole lane. The snap must be of this lane: its .git names the same
    // admin directory.
    let gitfile = |p: &Path| std::fs::read_to_string(p.join(".git")).ok();
    if gitfile(&snap.path).is_none() || gitfile(&snap.path) != gitfile(lane) {
        bail!(
            "{} does not belong to the lane at {} (its .git differs)",
            snap.path.display(),
            lane.display()
        );
    }
    let archive = work_dir.join(".archive");
    std::fs::create_dir_all(&archive)?;
    let aside = archive.join(format!("{tag}.before-restore-{stamp}"));
    std::fs::rename(lane, &aside)?;
    if let Err(e) = btrfs::snapshot(&snap.path, lane, false) {
        std::fs::rename(&aside, lane)?;
        bail!("{e}; the lane was left as it was");
    }
    // The index in the admin directory is the current one: refresh its stat
    // data so `git status` shows what the restore changed.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(lane)
        .args(["update-index", "-q", "--refresh"])
        .output();
    println!(
        "restored {} from {}; the lane as it was is at {}",
        lane.display(),
        snap.path.display(),
        aside.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
