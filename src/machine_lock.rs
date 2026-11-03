//! A machine-wide lock for heavy filesystem work.
//!
//! A reflink copy of a whole tree (`grove new`, `grove fork`) and the delete
//! of a lane (`grove gc`, `grove done`) each create or remove one inode per
//! file. Several at once froze the desktop on 2026-11-03: btrfs transaction
//! commits took up to 20 s and the compositor blocked in metadata
//! reservation. The lock in `<base.lock>` covers one repo only, so this one
//! lives in a per-user directory and covers every repo on the machine:
//!
//! `~/.local/state/grove`, or `$GROVE_LOCK_DIR` when set (tests use it).
//! See [`lock_dir`] for why it is not a runtime directory.
//!
//! The holder writes `copy.lock.json` next to the lock with its pid, tag,
//! command and start time, so a waiting command can say who it waits for.
//!
//! Batch deletes (`grove gc`) also wait while the I/O pressure in
//! `/proc/pressure/io` is high, so lane deletes do not follow each other while
//! the filesystem is still busy with the last one. See [`acquire_paced`].
//!
//! The lock is only taken on Unix and the pressure check only runs on Linux.
//! Windows keeps its old behaviour.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Overrides the directory of the lock file.
pub const LOCK_DIR_ENV: &str = "GROVE_LOCK_DIR";

/// Test hook: hold the lock this many milliseconds after taking it, so a
/// test can see a second command wait.
pub const HOLD_ENV: &str = "GROVE_COPY_LOCK_HOLD_MS";

const LOCK_FILE: &str = "copy.lock";
const HOLDER_FILE: &str = "copy.lock.json";

/// What the current holder of the lock wrote about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// `copy` or `delete`.
    pub op: String,
    pub tag: String,
    pub command: String,
    /// RFC 3339, UTC.
    pub started: String,
}

impl Holder {
    fn describe(&self) -> String {
        format!(
            "pid {}, {} of '{}', `{}`, since {}",
            self.pid, self.op, self.tag, self.command, self.started
        )
    }
}

/// The directory of the machine-wide lock: `$GROVE_LOCK_DIR` when set, else
/// `~/.local/state/grove`.
///
/// There is exactly one candidate on purpose. Every grove run of one user
/// must open the same lock file, and a runtime directory is not stable for
/// that: cron runs before login have no `/run/user/<uid>`, and some units
/// set their own `$XDG_RUNTIME_DIR`. The path is built from the home
/// directory, not `$XDG_STATE_HOME`, for the same reason.
pub fn lock_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LOCK_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if cfg!(test) {
        // Unit tests never touch the real machine lock.
        return Some(std::env::temp_dir().join("grove-unit-test-locks"));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(".local").join("state").join("grove"))
}

/// The held lock. Dropping it removes the holder file and releases the lock.
pub struct MachineLock {
    file: Option<File>,
    holder_path: Option<PathBuf>,
}

impl MachineLock {
    /// Take the machine-wide lock for `op` (`copy` or `delete`) on `tag`,
    /// waiting while another grove command holds it.
    ///
    /// Fails when the lock cannot be taken: the heavy work must not run
    /// unserialized just because the lock is broken.
    /// On Windows it holds nothing and never fails.
    pub fn acquire(op: &str, tag: &str) -> Result<Self, String> {
        if !cfg!(unix) {
            return Ok(Self::none());
        }
        let dir = lock_dir().ok_or_else(|| {
            format!("could not take the grove machine lock: no home directory; set {LOCK_DIR_ENV}")
        })?;
        Self::acquire_in(&dir, op, tag).map_err(|e| {
            format!(
                "could not take the grove machine lock in {} ({e}); set {LOCK_DIR_ENV} to a writable directory",
                dir.display()
            )
        })
    }

    /// Take the lock for cleanup that must run anyway (removing a half-made
    /// tree): when the lock cannot be taken, warn and go on without it,
    /// because leaving the half-made tree behind is worse.
    pub fn acquire_for_cleanup(tag: &str) -> Self {
        Self::acquire("delete", tag).unwrap_or_else(|e| {
            eprintln!("warning: {e}; removing the half-made tree without it");
            Self::none()
        })
    }

    /// Same as [`Self::acquire`] with an explicit directory, returning errors.
    pub fn acquire_in(dir: &Path, op: &str, tag: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let lock_path = dir.join(LOCK_FILE);
        let holder_path = dir.join(HOLDER_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        if file.try_lock().is_err() {
            let who = read_holder(&holder_path)
                .map(|h| h.describe())
                .unwrap_or_else(|| "holder unknown".to_string());
            eprintln!(
                "Waiting for another grove command to finish copying or deleting a lane ({who})..."
            );
            file.lock()?;
        }

        let holder = Holder {
            pid: std::process::id(),
            op: op.to_string(),
            tag: tag.to_string(),
            command: std::env::args().collect::<Vec<_>>().join(" "),
            started: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        };
        // The holder file is advisory; a failure to write it only costs the
        // next waiter its message.
        let _ = write_holder(&holder_path, &holder);

        if let Some(ms) = std::env::var(HOLD_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(ms));
        }

        Ok(Self {
            file: Some(file),
            holder_path: Some(holder_path),
        })
    }

    fn none() -> Self {
        Self {
            file: None,
            holder_path: None,
        }
    }

    /// True when this guard holds the lock.
    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // Remove the holder file before the lock goes, and only when it is
        // still ours, so the next holder's file is never removed.
        if let Some(path) = &self.holder_path
            && read_holder(path).is_some_and(|h| h.pid == std::process::id())
        {
            let _ = std::fs::remove_file(path);
        }
        // Closing the file releases the lock.
        self.file.take();
    }
}

/// Read the holder file, if there is a valid one.
pub fn read_holder(path: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_holder(path: &Path, holder: &Holder) -> std::io::Result<()> {
    // Drop whatever an earlier holder left (one killed before its Drop ran),
    // so a failed write below never leaves another process's name behind.
    let _ = std::fs::remove_file(path);
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    let written = File::create(&tmp)
        .and_then(|mut f| f.write_all(serde_json::to_string(holder)?.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The name used for a lane in the holder file: the last path component.
pub fn tag_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Batch deletes wait while "some avg10" in `/proc/pressure/io` is at or
/// above this (percent of time at least one task stalled on I/O).
pub const DELETE_IO_PRESSURE_LIMIT: f64 = 5.0;
const PRESSURE_POLL: Duration = Duration::from_secs(5);
const PRESSURE_MAX_WAIT: Duration = Duration::from_secs(600);
const PRESSURE_REPORT_EVERY: Duration = Duration::from_secs(60);

/// The "some avg10" value of a `/proc/pressure/io` text.
pub fn parse_io_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

/// The current I/O pressure, or `None` where there is none to read (not
/// Linux, or a kernel without PSI).
pub fn io_pressure() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    parse_io_some_avg10(&std::fs::read_to_string("/proc/pressure/io").ok()?)
}

/// How a wait for quiet I/O ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Paced {
    /// Pressure is under the limit.
    Quiet,
    /// No pressure data; nothing to wait for.
    NoData,
    /// Pressure stayed at or above the limit for the whole wait; the last value.
    TimedOut(f64),
}

/// Wait, polling every `poll`, until `read` reports pressure under
/// [`DELETE_IO_PRESSURE_LIMIT`], at most `max_wait`. `on_wait` gets each high
/// value and the time waited so far, before each sleep.
pub fn wait_for_quiet_io(
    mut read: impl FnMut() -> Option<f64>,
    mut sleep: impl FnMut(Duration),
    poll: Duration,
    max_wait: Duration,
    mut on_wait: impl FnMut(f64, Duration),
) -> Paced {
    let mut waited = Duration::ZERO;
    loop {
        match read() {
            None => return Paced::NoData,
            Some(v) if v < DELETE_IO_PRESSURE_LIMIT => return Paced::Quiet,
            Some(v) => {
                if waited >= max_wait {
                    return Paced::TimedOut(v);
                }
                on_wait(v, waited);
                sleep(poll);
                waited += poll;
            }
        }
    }
}

/// Take the lock for one delete in a batch: wait for quiet I/O first, take
/// the lock, and check the pressure again once it is held (another command's
/// delete may just have ended). Gives up with a message after 10 minutes of
/// high pressure; the caller skips that delete.
pub fn acquire_paced(op: &str, tag: &str) -> Result<MachineLock, String> {
    acquire_paced_with(
        tag,
        || MachineLock::acquire(op, tag),
        io_pressure,
        std::thread::sleep,
        PRESSURE_POLL,
        PRESSURE_MAX_WAIT,
    )
}

/// [`acquire_paced`] with the lock, pressure source, sleep and limits passed
/// in.
pub fn acquire_paced_with(
    tag: &str,
    mut take: impl FnMut() -> Result<MachineLock, String>,
    mut read: impl FnMut() -> Option<f64>,
    mut sleep: impl FnMut(Duration),
    poll: Duration,
    max_wait: Duration,
) -> Result<MachineLock, String> {
    // Only time spent waiting for quiet I/O counts against `max_wait`, not
    // time spent waiting for the lock behind another command.
    let mut waited = Duration::ZERO;
    loop {
        let left = max_wait.saturating_sub(waited);
        let mut last_report: Option<Duration> = None;
        let outcome = wait_for_quiet_io(
            &mut read,
            |d| {
                sleep(d);
                waited += d;
            },
            poll,
            left,
            |v, so_far| {
                if last_report.is_none_or(|t| so_far >= t + PRESSURE_REPORT_EVERY) {
                    eprintln!(
                        "I/O pressure is {v:.1} (some avg10, limit {DELETE_IO_PRESSURE_LIMIT}); \
                         waiting before deleting '{tag}'..."
                    );
                    last_report = Some(so_far);
                }
            },
        );
        if let Paced::TimedOut(v) = outcome {
            return Err(format!(
                "skipped: waited {} s for quiet I/O and pressure is still {v:.1} (limit \
                 {DELETE_IO_PRESSURE_LIMIT}); run gc again later",
                waited.as_secs()
            ));
        }
        let lock = take().map_err(|e| format!("skipped: {e}"))?;
        match read() {
            Some(v) if v >= DELETE_IO_PRESSURE_LIMIT => drop(lock),
            _ => return Ok(lock),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn second_taker_waits_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = MachineLock::acquire_in(dir.path(), "copy", "one").unwrap();
        assert!(first.is_held());
        let holder = read_holder(&dir.path().join(HOLDER_FILE)).unwrap();
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.tag, "one");
        assert_eq!(holder.op, "copy");

        let (tx, rx) = mpsc::channel();
        let path = dir.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            let lock = MachineLock::acquire_in(&path, "delete", "two").unwrap();
            tx.send(Instant::now()).unwrap();
            drop(lock);
        });

        // flock locks belong to the open file, so a second open in the same
        // process blocks too.
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the second taker must wait while the first holds the lock"
        );
        let released = Instant::now();
        drop(first);
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(got >= released);
        waiter.join().unwrap();
        assert!(
            !dir.path().join(HOLDER_FILE).exists(),
            "the last holder removes its holder file"
        );
    }

    #[test]
    fn drop_leaves_another_holders_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let lock = MachineLock::acquire_in(dir.path(), "copy", "mine").unwrap();
        let other = Holder {
            pid: std::process::id().wrapping_add(1),
            op: "copy".into(),
            tag: "theirs".into(),
            command: "grove new theirs".into(),
            started: "2026-11-03T00:00:00Z".into(),
        };
        write_holder(&dir.path().join(HOLDER_FILE), &other).unwrap();
        drop(lock);
        assert_eq!(read_holder(&dir.path().join(HOLDER_FILE)), Some(other));
    }

    #[test]
    fn tag_is_the_last_component() {
        assert_eq!(tag_of(Path::new("/w/.scratch/lane-1")), "lane-1");
    }

    #[test]
    fn pressure_line_is_parsed() {
        let text = "some avg10=12.50 avg60=3.00 avg300=1.00 total=123\n\
                    full avg10=2.00 avg60=1.00 avg300=0.50 total=45\n";
        assert_eq!(parse_io_some_avg10(text), Some(12.5));
        assert_eq!(parse_io_some_avg10("full avg10=1.00\n"), None);
        assert_eq!(parse_io_some_avg10(""), None);
    }

    #[test]
    fn delete_waits_while_pressure_is_high() {
        let mut readings = vec![30.0, 12.0, 5.0, 4.9].into_iter();
        let mut slept = Vec::new();
        let outcome = wait_for_quiet_io(
            || readings.next(),
            |d| slept.push(d),
            Duration::from_secs(5),
            Duration::from_secs(600),
            |_, _| {},
        );
        assert_eq!(outcome, Paced::Quiet);
        // 30, 12 and 5 are all at or above the limit: three pauses.
        assert_eq!(slept, vec![Duration::from_secs(5); 3]);
    }

    #[test]
    fn delete_does_not_wait_when_quiet_or_without_data() {
        let mut slept = 0;
        assert_eq!(
            wait_for_quiet_io(
                || Some(0.4),
                |_| slept += 1,
                Duration::from_secs(5),
                Duration::from_secs(600),
                |_, _| {}
            ),
            Paced::Quiet
        );
        assert_eq!(
            wait_for_quiet_io(
                || None,
                |_| slept += 1,
                Duration::from_secs(5),
                Duration::from_secs(600),
                |_, _| {}
            ),
            Paced::NoData
        );
        assert_eq!(slept, 0);
    }

    #[test]
    fn delete_gives_up_after_the_max_wait() {
        let mut slept = Duration::ZERO;
        let outcome = wait_for_quiet_io(
            || Some(40.0),
            |d| slept += d,
            Duration::from_secs(5),
            Duration::from_secs(20),
            |_, _| {},
        );
        assert_eq!(outcome, Paced::TimedOut(40.0));
        assert_eq!(slept, Duration::from_secs(20));
    }

    #[test]
    fn paced_acquire_rechecks_pressure_once_it_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        // Quiet before the lock, high right after it (another delete just
        // ended), then quiet again.
        let mut readings = vec![1.0, 20.0, 1.0, 1.0].into_iter();
        let mut sleeps = 0;
        let mut takes = 0;
        let lock = acquire_paced_with(
            "lane",
            || {
                takes += 1;
                Ok(MachineLock::acquire_in(dir.path(), "delete", "lane").unwrap())
            },
            || readings.next(),
            |_| sleeps += 1,
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(lock.is_held());
        assert_eq!(takes, 2, "the lock is dropped and taken again");
        assert_eq!(readings.next(), None, "all four readings were used");
        assert_eq!(sleeps, 0);
    }

    #[test]
    fn time_waiting_for_the_lock_does_not_count_against_the_pressure_wait() {
        let dir = tempfile::tempdir().unwrap();
        // Quiet, then the lock takes longer than the whole pressure budget,
        // then high once held (another delete just ended), then quiet.
        let mut readings = vec![1.0, 20.0, 1.0, 1.0].into_iter();
        let lock = acquire_paced_with(
            "lane",
            || {
                std::thread::sleep(Duration::from_millis(50));
                Ok(MachineLock::acquire_in(dir.path(), "delete", "lane").unwrap())
            },
            || readings.next(),
            |_| {},
            Duration::from_millis(1),
            Duration::from_millis(10),
        );
        assert!(lock.is_ok(), "{:?}", lock.err());
    }

    #[test]
    fn a_broken_lock_skips_the_delete() {
        let err = acquire_paced_with(
            "lane",
            || Err("no lock directory".to_string()),
            || Some(0.0),
            |_| {},
            Duration::from_secs(5),
            Duration::from_secs(15),
        )
        .err()
        .unwrap();
        assert!(err.starts_with("skipped:"), "{err}");
    }

    #[test]
    fn paced_acquire_skips_when_pressure_stays_high() {
        let err = acquire_paced_with(
            "lane",
            || panic!("the lock must not be taken"),
            || Some(50.0),
            |_| {},
            Duration::from_secs(5),
            Duration::from_secs(15),
        )
        .err()
        .unwrap();
        assert!(err.contains("skipped"), "{err}");
    }
}
