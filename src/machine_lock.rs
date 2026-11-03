//! A machine-wide lock for heavy filesystem work.
//!
//! A reflink copy of a whole tree (`grove new`, `grove fork`) and the delete
//! of a lane (`grove gc`, `grove done`) each create or remove one inode per
//! file. Several at once froze the desktop on 2026-11-03: btrfs transaction
//! commits took up to 20 s and the compositor blocked in metadata
//! reservation. The lock in `<base.lock>` covers one repo only, so this one
//! lives in a per-user directory and covers every repo on the machine:
//!
//! - `$GROVE_LOCK_DIR` when set (tests use it),
//! - else `$XDG_RUNTIME_DIR/grove`,
//! - else `~/.local/state/grove`.
//!
//! The holder writes `copy.lock.json` next to the lock with its pid, tag,
//! command and start time, so a waiting command can say who it waits for.
//!
//! The lock is only taken on Unix. Windows keeps its old behaviour.

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

/// The directory of the machine-wide lock.
pub fn lock_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LOCK_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return Some(dir.join("grove"));
        }
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
    /// Never fails: when the lock file cannot be made, it warns and returns a
    /// guard that holds nothing, so a broken lock directory cannot stop grove.
    pub fn acquire(op: &str, tag: &str) -> Self {
        if !cfg!(unix) {
            return Self::none();
        }
        let Some(dir) = lock_dir() else {
            return Self::none();
        };
        match Self::acquire_in(&dir, op, tag) {
            Ok(lock) => lock,
            Err(e) => {
                eprintln!(
                    "warning: could not take the grove lock in {}: {e}; going on without it",
                    dir.display()
                );
                Self::none()
            }
        }
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
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(serde_json::to_string(holder)?.as_bytes())?;
    }
    std::fs::rename(&tmp, path)
}

/// The name used for a lane in the holder file: the last path component.
pub fn tag_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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
}
