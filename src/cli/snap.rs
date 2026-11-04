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
//! When a lane is no longer in the registry, its snaps are kept on that
//! schedule (newest included) for a grace period, so a `grove done` by
//! mistake can still be undone with `grove snap restore`. The first run that
//! finds the lane gone writes the time to `<tag>/.gone-since`; once the grace
//! period (`GROVE_SNAP_ORPHAN_GRACE_HOURS`, default 24) has passed since then,
//! all of the lane's snaps are deleted. A run that cannot read the registry
//! deletes nothing.
//!
//! A snap does not hold nested subvolumes (a lane's own `.scratch`): btrfs
//! leaves an empty placeholder for them.
//!
//! `grove done` leaves a lane's snaps alone; they go after the grace period.
//! The index, HEAD and config of a lane live in the main repo's `.git`,
//! which snapper still covers.

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

/// Environment variable with the grace period, in hours, before the snaps
/// of a lane that left the registry are deleted.
pub const ORPHAN_GRACE_ENV: &str = "GROVE_SNAP_ORPHAN_GRACE_HOURS";
const DEFAULT_ORPHAN_GRACE_HOURS: i64 = 24;

/// File in a lane's snaps dir with the time a run first found the lane gone.
use crate::registry::SNAP_GONE_MARKER as GONE_MARKER;

fn orphan_grace() -> time::Duration {
    parse_grace(std::env::var(ORPHAN_GRACE_ENV).ok().as_deref())
}

fn parse_grace(value: Option<&str>) -> time::Duration {
    let hours = value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|h| *h >= 0)
        .unwrap_or(DEFAULT_ORPHAN_GRACE_HOURS);
    time::Duration::hours(hours)
}

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
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
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
    run_with_grace(args, cx, now, orphan_grace())
}

/// The registry, or an error when it is missing or cannot be parsed. A
/// missing file reads as an empty registry elsewhere in grove; here that
/// would make every lane look gone, so it is an error.
fn load_registry(grove_dir: &Path) -> Result<crate::registry::Registry> {
    crate::registry::Registry::load_strict(grove_dir).map_err(|e| {
        anyhow!(
            "registry {}: {e}; nothing snapped or deleted",
            grove_dir.join("registry.json").display()
        )
    })
}

fn run_with_grace(
    args: &SnapArgs,
    cx: &RepoContext,
    now: OffsetDateTime,
    grace: time::Duration,
) -> Result<()> {
    let work_dir = &cx.resolved.work_dir;
    let grove_dir = cx.grove_dir();
    let registry = load_registry(&grove_dir)?;
    let mut failures = 0;
    // Below the floor `grove new` uses, no new snaps: each pins the build
    // output written since the last one. Pruning below still runs, which
    // frees space.
    let room = btrfs::check_headroom(work_dir);
    if let Err(e) = &room {
        eprintln!("warning: no snaps this run: {e}");
    }
    for (tag, project) in &registry.projects {
        if room.is_err() || !btrfs::is_subvolume(&project.path) {
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
        // Only real directories: a symlink could lead out of the snaps dir.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let tag = entry.file_name().to_string_lossy().into_owned();
        let result = if registry.projects.contains_key(&tag) {
            clear_gone_marker(work_dir, &tag, args.dry_run);
            prune(work_dir, &tag, now, true, args.dry_run)
        } else {
            prune_gone(&grove_dir, work_dir, &tag, now, grace, args.dry_run)
        };
        if let Err(e) = result {
            failures += 1;
            eprintln!("prune {tag}: {e}");
        }
    }
    if failures > 0 {
        bail!("{failures} lane(s) failed");
    }
    Ok(())
}

fn gone_marker(work_dir: &Path, tag: &str) -> PathBuf {
    snaps_dir(work_dir, tag).join(GONE_MARKER)
}

/// A lane that is registered again (its tag reused, or the registry put
/// back) starts a fresh grace period the next time it goes.
fn clear_gone_marker(work_dir: &Path, tag: &str, dry_run: bool) {
    let marker = gone_marker(work_dir, tag);
    if !dry_run && marker.is_file() {
        let _ = std::fs::remove_file(&marker);
    }
}

/// When a run first found `tag` gone, or `None` when no run has yet (or the
/// marker is not a regular file or cannot be read, which restarts the grace
/// period).
fn gone_since(work_dir: &Path, tag: &str) -> Option<OffsetDateTime> {
    let marker = gone_marker(work_dir, tag);
    if !std::fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let text = std::fs::read_to_string(marker).ok()?;
    OffsetDateTime::parse(text.trim(), &time::format_description::well_known::Rfc3339).ok()
}

/// Write the marker as a new file and rename it into place, so whatever is
/// at the marker's path (a symlink, say) is replaced, never written through.
fn write_gone_marker(work_dir: &Path, tag: &str, now: OffsetDateTime) -> Result<()> {
    use std::io::Write;
    let marker = gone_marker(work_dir, tag);
    let tmp = snaps_dir(work_dir, tag).join(format!("{GONE_MARKER}.{}", std::process::id()));
    let stamp = now.format(&time::format_description::well_known::Rfc3339)?;
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(format!("{stamp}\n").as_bytes())?;
    drop(file);
    std::fs::rename(&tmp, &marker)?;
    Ok(())
}

/// Prune the snaps of a lane that is not in the registry. Within the grace
/// period they follow the normal schedule with the newest kept; after it,
/// all of them go.
fn prune_gone(
    grove_dir: &Path,
    work_dir: &Path,
    tag: &str,
    now: OffsetDateTime,
    grace: time::Duration,
    dry_run: bool,
) -> Result<()> {
    let since = match gone_since(work_dir, tag) {
        Some(t) if t <= now => t,
        _ => {
            if dry_run {
                println!("would mark {tag} gone since {}", fmt_time(now));
            } else {
                write_gone_marker(work_dir, tag, now)?;
                println!(
                    "lane {tag} is gone; its snaps are kept until {}",
                    fmt_time(now + grace)
                );
            }
            now
        }
    };
    if now - since < grace {
        return prune(work_dir, tag, now, true, dry_run);
    }
    let reason = format!(
        "lane {tag} gone since {}, grace {} h passed",
        fmt_time(since),
        grace.whole_hours()
    );
    for snap in list_snaps(work_dir, tag) {
        if dry_run {
            println!("would delete {} ({reason})", snap.path.display());
            continue;
        }
        // The slow path's pacing lock first, then the registry lock that
        // `grove new` takes to register a tag: the tag is checked again and
        // the snap deleted while no one can register it. A registry that
        // cannot be read now deletes nothing.
        let paced = (!btrfs::rm_allowed(&snap.path))
            .then(|| crate::machine_lock::acquire_paced("delete", tag))
            .transpose()
            .map_err(|e| anyhow!(e))?;
        let lock = std::fs::File::create(grove_dir.join("registry.lock"))?;
        lock.lock()?;
        if load_registry(grove_dir)?.projects.contains_key(tag) {
            clear_gone_marker(work_dir, tag, false);
            return Ok(());
        }
        btrfs::delete_subvolume(&snap.path, &[]).map_err(|e| anyhow!("{e}"))?;
        drop(lock);
        drop(paced);
        println!("deleted {} ({reason})", snap.path.display());
    }
    if !dry_run {
        let _ = std::fs::remove_file(gone_marker(work_dir, tag));
        remove_if_empty(&snaps_dir(work_dir, tag));
    }
    Ok(())
}

fn fmt_time(t: OffsetDateTime) -> String {
    t.format(NAME_FORMAT).unwrap_or_else(|_| t.to_string())
}

fn remove_if_empty(dir: &Path) {
    if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
        let _ = std::fs::remove_dir(dir);
    }
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
        if !keep {
            delete_snap(snap, tag, "past the hourly and daily schedule", dry_run)?;
        }
    }
    if !dry_run && !gone_marker(work_dir, tag).exists() {
        remove_if_empty(&snaps_dir(work_dir, tag));
    }
    Ok(())
}

/// Delete one snap through the fast path when the mount allows it (the
/// read-only flag is cleared first), and log it with `reason`.
fn delete_snap(snap: &Snap, tag: &str, reason: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("would delete {} ({reason})", snap.path.display());
        return Ok(());
    }
    // Without user_subvol_rm_allowed this unlinks every file: as heavy
    // as a lane delete, so one at a time and not under I/O pressure.
    let lock = (!btrfs::rm_allowed(&snap.path))
        .then(|| crate::machine_lock::acquire_paced("delete", tag))
        .transpose()
        .map_err(|e| anyhow!(e))?;
    btrfs::delete_subvolume(&snap.path, &[]).map_err(|e| anyhow!("{e}"))?;
    drop(lock);
    println!("deleted {} ({reason})", snap.path.display());
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
        check_rel(rel)?;
        no_symlinked_parents(&snap.path, rel)?;
        no_symlinked_parents(lane, rel)?;
        let from = snap.path.join(rel);
        if std::fs::symlink_metadata(&from).is_err() {
            bail!("{} is not in the snap", rel.display());
        }
        let to = lane.join(rel);
        if std::fs::symlink_metadata(&to).is_ok() {
            let name = to.file_name().unwrap_or_default().to_string_lossy();
            let aside = move_aside(
                &to,
                &to.with_file_name(format!("{name}.before-restore-{stamp}")),
            )?;
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
    // Nested worktrees in the lane's .scratch are not in the snap; moving
    // the lane aside would cut them off from their registrations.
    let scratch = lane.join(crate::cli::new::SCRATCH_DIR);
    let busy = match std::fs::read_dir(&scratch) {
        Ok(mut d) => d.next().is_some(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => bail!("could not read {}: {e}", scratch.display()),
    };
    if busy {
        bail!(
            "{} is not empty and the snap does not hold it; move what is in it first, \
             or restore single files with --file",
            scratch.display()
        );
    }
    let archive = work_dir.join(".archive");
    std::fs::create_dir_all(&archive)?;
    let aside = move_aside(lane, &archive.join(format!("{tag}.before-restore-{stamp}")))?;
    // The old tree must not share the restored lane's admin directory: git
    // run in it would change the lane's index and HEAD.
    let detached = match move_aside(&aside.join(".git"), &aside.join(".git.detached-by-grove")) {
        Ok(p) => p,
        Err(e) => {
            std::fs::rename(&aside, lane)?;
            bail!("{e}; the lane was left as it was");
        }
    };
    if let Err(e) = btrfs::snapshot(&snap.path, lane, false) {
        std::fs::rename(&detached, aside.join(".git"))?;
        std::fs::rename(&aside, lane)?;
        bail!("{e}; the lane was left as it was");
    }
    // The snap holds only an empty placeholder for the lane's .scratch
    // subvolume: make it a subvolume again, as every lane has.
    if std::fs::read_dir(&scratch).is_ok_and(|mut d| d.next().is_none())
        && !btrfs::is_subvolume(&scratch)
    {
        // The restore itself is done: a failure here is a warning, not a
        // reason to report the restore as failed with the lane replaced.
        let made = std::fs::remove_dir(&scratch)
            .map_err(|e| e.to_string())
            .and_then(|_| btrfs::create_subvolume(&scratch).map_err(|e| e.to_string()));
        if let Err(e) = made {
            eprintln!(
                "warning: could not make {} a subvolume again: {e}",
                scratch.display()
            );
        }
    }
    // The index in the admin directory is the current one: refresh its stat
    // data so `git status` shows what the restore changed.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(lane)
        .args(["update-index", "-q", "--refresh"])
        .output();
    println!(
        "restored {} from {}; the lane as it was is at {} (its .git renamed to {})",
        lane.display(),
        snap.path.display(),
        aside.display(),
        detached.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

/// A path inside a lane: relative, plain names only, not empty, not `.git`.
fn check_rel(rel: &Path) -> Result<()> {
    let mut parts = rel.components();
    let first = parts.next();
    let plain = first.is_some()
        && rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain {
        bail!("give the file as a plain path inside the lane");
    }
    if first.is_some_and(|c| c.as_os_str() == ".git") {
        bail!("the lane's .git is not restored; it names the lane's admin directory");
    }
    Ok(())
}

/// Refuse when a directory on the way to `root/rel` is a symlink, which
/// could lead outside `root`.
fn no_symlinked_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut at = root.to_path_buf();
    let parents: Vec<_> = rel.components().collect();
    for part in &parents[..parents.len().saturating_sub(1)] {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "{} is a symlink; restore the file it leads to instead",
                at.display()
            );
        }
    }
    Ok(())
}

/// Rename `path` to `dest`, never replacing an existing entry: a counter is
/// added to the name until it is free. Returns the new path.
fn move_aside(path: &Path, dest: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    for n in 0..1000 {
        let candidate = if n == 0 {
            dest.to_path_buf()
        } else {
            let mut name = dest.as_os_str().to_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        let from = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(candidate.as_os_str().as_bytes())?;
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(candidate);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(anyhow!(
                "rename {} to {}: {err}",
                path.display(),
                candidate.display()
            ));
        }
    }
    bail!("no free name for {} next to it", path.display())
}

#[cfg(test)]
mod tests;
