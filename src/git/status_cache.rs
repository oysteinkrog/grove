//! Short-lived cache for per-project git status.
//!
//! Scanning status is expensive in a way that is easy to underestimate. Each
//! worktree needs an index refresh plus a directory walk for untracked files,
//! and both are pure filesystem work. On a Windows drive under WSL1 that runs
//! about 10 seconds for a worktree with 29k tracked files, and `grove list`
//! does it for every project it knows about.
//!
//! Git's own answer to the untracked walk, `core.untrackedCache`, cannot help
//! here: it keys on directory mtime, and a Windows drive mounted through WSL1
//! does not update directory mtime when a file is added. `git update-index
//! --test-untracked-cache` reports exactly that. So grove caches the finished
//! result instead.
//!
//! Two problems are worth separating:
//!
//! - Repeated runs. Several agent sessions each call `grove list`, so the same
//!   scan is paid many times over. A time-to-live cache fixes this.
//! - Concurrent runs. Those calls overlap, so a plain cache still lets every
//!   one of them miss and scan. The scan lock in this module fixes that: the
//!   first caller scans and the rest wait for its result.
//!
//! Freshness is best-effort by design. Untracked files can appear without
//! touching anything cheap enough to stat, so there is no reliable
//! invalidation signal available at this cost. Callers are expected to show
//! the age of cached data and offer a way to force a rescan, rather than
//! implying the numbers are current.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::status::Status;

/// How long a cached status is offered before it is considered stale.
///
/// This is set against the measured cost of a rescan, not against how quickly
/// a worktree can change. A full scan of 228 projects takes about 9.5 minutes
/// of multi-core work, so a ten minute window would mean scanning almost
/// continuously whenever anything calls `grove list` regularly, which is the
/// problem this cache exists to solve. Thirty minutes keeps the machine
/// usable.
///
/// The honest reading of this number is that the listing is an overview, not a
/// live view. It prints the age of its data, and `--refresh` forces a rescan
/// when the answer has to be current.
pub const DEFAULT_TTL: Duration = Duration::from_secs(1800);

/// Longest a caller waits for another process's scan before giving up.
///
/// This must comfortably exceed a cold full scan, or waiters time out before
/// the scanner finishes and then duplicate all of its work, which is the exact
/// problem the lock exists to prevent. A measured cold scan of 228 projects
/// took 764 seconds, so this leaves room above that.
const LOCK_WAIT: Duration = Duration::from_secs(1200);

/// How often to re-check whether the scanning process is still alive.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A lock older than this is treated as abandoned even if a process with the
/// recorded id exists, which guards against process id reuse.
const LOCK_MAX_AGE: Duration = Duration::from_secs(1800);

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Entry {
    dirty: bool,
    ahead: Option<u32>,
    behind: Option<u32>,
    untracked: u32,
    is_pushed: bool,
    /// Unix seconds when this entry was written.
    ts: u64,
}

impl From<&Status> for Entry {
    fn from(s: &Status) -> Self {
        Self {
            dirty: s.dirty,
            ahead: s.ahead,
            behind: s.behind,
            untracked: s.untracked,
            is_pushed: s.is_pushed,
            ts: now_secs(),
        }
    }
}

impl From<&Entry> for Status {
    fn from(e: &Entry) -> Self {
        Self {
            dirty: e.dirty,
            ahead: e.ahead,
            behind: e.behind,
            untracked: e.untracked,
            is_pushed: e.is_pushed,
        }
    }
}

/// Status for a set of projects, keyed by project path.
#[derive(Serialize, Deserialize, Default)]
pub struct StatusCache {
    #[serde(default)]
    entries: HashMap<String, Entry>,
}

/// What a lookup found, so the caller can tell the user how old the data is.
pub enum Hit {
    /// Cached status, with how long ago it was written.
    Fresh(Status, Duration),
    /// Nothing usable cached.
    Miss,
}

impl StatusCache {
    /// Read the cache for a repo. A missing or unreadable file is an empty
    /// cache, never an error: a broken cache must not break `grove list`.
    pub fn load(grove_dir: &Path) -> Self {
        let path = cache_path(grove_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Look up one project, rejecting entries older than `ttl`.
    pub fn get(&self, project_path: &Path, ttl: Duration) -> Hit {
        let Some(entry) = self.entries.get(&key(project_path)) else {
            return Hit::Miss;
        };
        let age = Duration::from_secs(now_secs().saturating_sub(entry.ts));
        if age > ttl {
            return Hit::Miss;
        }
        Hit::Fresh(Status::from(entry), age)
    }

    /// Look up one project at any age. Used when another process is mid-scan
    /// and showing something stale beats paying for a duplicate scan.
    pub fn get_at_any_age(&self, project_path: &Path) -> Hit {
        let Some(entry) = self.entries.get(&key(project_path)) else {
            return Hit::Miss;
        };
        let age = Duration::from_secs(now_secs().saturating_sub(entry.ts));
        Hit::Fresh(Status::from(entry), age)
    }

    /// Merge fresh results into whatever is on disk and replace it.
    ///
    /// Merging rather than overwriting matters because a caller may have
    /// scanned only some projects, and because a concurrent writer may have
    /// stored results we do not have. Last writer wins per project, which is
    /// acceptable for a cache.
    pub fn store(grove_dir: &Path, fresh: &[(PathBuf, Status)]) -> std::io::Result<()> {
        if fresh.is_empty() {
            return Ok(());
        }
        let mut cache = Self::load(grove_dir);
        for (path, status) in fresh {
            cache.entries.insert(key(path), Entry::from(status));
        }

        std::fs::create_dir_all(grove_dir)?;
        let final_path = cache_path(grove_dir);
        // Write to a process-unique temp file, then rename. A half-written
        // cache file would be read back as empty by every other process.
        let tmp = grove_dir.join(format!(".status-cache.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec(&cache)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, &bytes)?;
        match std::fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// Held while this process is scanning, so concurrent callers wait rather than
/// duplicating the work. Released on drop.
pub struct ScanLock {
    path: PathBuf,
}

/// Outcome of asking for permission to scan.
pub enum LockOutcome {
    /// Nobody else is scanning; this process should scan and holds the lock.
    Acquired(ScanLock),
    /// Another process finished scanning while we waited. Re-read the cache.
    OtherFinished,
    /// Another process is still scanning and waiting timed out. Scan anyway.
    Contended,
}

impl ScanLock {
    /// Try to become the scanning process, waiting for an existing scan if one
    /// is underway.
    pub fn acquire(grove_dir: &Path) -> LockOutcome {
        let path = lock_path(grove_dir);
        let deadline = SystemTime::now() + LOCK_WAIT;

        loop {
            if std::fs::create_dir_all(grove_dir).is_err() {
                // Nowhere to put a lock, so coordination is impossible. Let the
                // caller scan rather than fail.
                return LockOutcome::Contended;
            }

            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return LockOutcome::Acquired(ScanLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_abandoned(&path) {
                        // The holder died, or the lock outlived any plausible
                        // scan. Clear it and try again.
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if SystemTime::now() >= deadline {
                        return LockOutcome::Contended;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                    // Gone means the holder finished and wrote its results.
                    if !path.exists() {
                        return LockOutcome::OtherFinished;
                    }
                }
                Err(_) => return LockOutcome::Contended,
            }
        }
    }
}

impl Drop for ScanLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// True when the lock file's owner is gone or the lock is implausibly old.
fn lock_is_abandoned(path: &Path) -> bool {
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(modified) = meta.modified()
        && let Ok(age) = SystemTime::now().duration_since(modified)
        && age > LOCK_MAX_AGE
    {
        return true;
    }

    let Ok(contents) = std::fs::read_to_string(path) else {
        // Unreadable but present: leave it alone rather than stealing it.
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        // No usable owner recorded, so nobody can be waited for.
        return true;
    };
    !Path::new(&format!("/proc/{pid}")).exists()
}

fn cache_path(grove_dir: &Path) -> PathBuf {
    grove_dir.join("status-cache.json")
}

fn lock_path(grove_dir: &Path) -> PathBuf {
    grove_dir.join("status-scan.lock")
}

/// Cache key. The path as written is enough: grove addresses projects by path,
/// and two different strings for one directory simply miss the cache.
fn key(project_path: &Path) -> String {
    project_path.to_string_lossy().into_owned()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn status(untracked: u32) -> Status {
        Status {
            dirty: true,
            ahead: Some(1),
            behind: Some(2),
            untracked,
            is_pushed: false,
        }
    }

    #[test]
    fn stores_and_reads_back() {
        let dir = TempDir::new().unwrap();
        let p = PathBuf::from("/some/project");
        StatusCache::store(dir.path(), &[(p.clone(), status(7))]).unwrap();

        let cache = StatusCache::load(dir.path());
        match cache.get(&p, DEFAULT_TTL) {
            Hit::Fresh(s, _) => assert_eq!(s, status(7)),
            Hit::Miss => panic!("just-written entry should be a hit"),
        }
    }

    #[test]
    fn unknown_project_misses() {
        let dir = TempDir::new().unwrap();
        StatusCache::store(dir.path(), &[(PathBuf::from("/a"), status(1))]).unwrap();

        let cache = StatusCache::load(dir.path());
        assert!(matches!(
            cache.get(Path::new("/b"), DEFAULT_TTL),
            Hit::Miss
        ));
    }

    /// Write a cache file whose single entry is `age_secs` old, to exercise
    /// expiry without sleeping.
    fn write_aged_entry(grove_dir: &Path, project: &str, age_secs: u64) {
        let entry = Entry {
            dirty: true,
            ahead: Some(1),
            behind: Some(2),
            untracked: 5,
            is_pushed: false,
            ts: now_secs() - age_secs,
        };
        let mut entries = HashMap::new();
        entries.insert(project.to_string(), entry);
        let cache = StatusCache { entries };
        std::fs::create_dir_all(grove_dir).unwrap();
        std::fs::write(
            cache_path(grove_dir),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn entry_older_than_ttl_misses() {
        let dir = TempDir::new().unwrap();
        let p = Path::new("/some/project");
        write_aged_entry(dir.path(), "/some/project", 600);

        let cache = StatusCache::load(dir.path());
        assert!(
            matches!(cache.get(p, Duration::from_secs(300)), Hit::Miss),
            "a 10 minute old entry must not satisfy a 5 minute ttl"
        );
    }

    #[test]
    fn stale_entry_is_still_available_at_any_age() {
        let dir = TempDir::new().unwrap();
        let p = Path::new("/some/project");
        write_aged_entry(dir.path(), "/some/project", 600);

        let cache = StatusCache::load(dir.path());
        match cache.get_at_any_age(p) {
            Hit::Fresh(_, age) => assert!(
                age.as_secs() >= 600,
                "reported age should reflect how old the entry really is"
            ),
            Hit::Miss => panic!("stale entry should still be reachable"),
        }
    }

    #[test]
    fn entry_within_ttl_hits() {
        let dir = TempDir::new().unwrap();
        let p = Path::new("/some/project");
        write_aged_entry(dir.path(), "/some/project", 60);

        let cache = StatusCache::load(dir.path());
        assert!(matches!(
            cache.get(p, Duration::from_secs(300)),
            Hit::Fresh(_, _)
        ));
    }

    #[test]
    fn store_merges_rather_than_replacing() {
        let dir = TempDir::new().unwrap();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");

        StatusCache::store(dir.path(), &[(a.clone(), status(1))]).unwrap();
        StatusCache::store(dir.path(), &[(b.clone(), status(2))]).unwrap();

        let cache = StatusCache::load(dir.path());
        assert!(
            matches!(cache.get(&a, DEFAULT_TTL), Hit::Fresh(s, _) if s.untracked == 1),
            "first entry must survive the second write"
        );
        assert!(matches!(cache.get(&b, DEFAULT_TTL), Hit::Fresh(s, _) if s.untracked == 2));
    }

    #[test]
    fn missing_cache_file_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        let cache = StatusCache::load(&dir.path().join("nonexistent"));
        assert!(matches!(cache.get(Path::new("/a"), DEFAULT_TTL), Hit::Miss));
    }

    #[test]
    fn corrupt_cache_file_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(cache_path(dir.path()), b"{ this is not json").unwrap();

        let cache = StatusCache::load(dir.path());
        assert!(
            matches!(cache.get(Path::new("/a"), DEFAULT_TTL), Hit::Miss),
            "a corrupt cache must degrade to a miss, not panic"
        );
    }

    #[test]
    fn lock_is_exclusive_then_released_on_drop() {
        let dir = TempDir::new().unwrap();

        let first = match ScanLock::acquire(dir.path()) {
            LockOutcome::Acquired(l) => l,
            _ => panic!("uncontended lock should be acquired"),
        };
        assert!(lock_path(dir.path()).exists(), "lock file should exist");

        drop(first);
        assert!(
            !lock_path(dir.path()).exists(),
            "dropping the lock must remove the file"
        );

        assert!(matches!(
            ScanLock::acquire(dir.path()),
            LockOutcome::Acquired(_)
        ));
    }

    #[test]
    fn lock_held_by_dead_process_is_taken_over() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        // Process id 0 never appears in /proc, so this stands in for a holder
        // that died without cleaning up.
        std::fs::write(lock_path(dir.path()), b"0").unwrap();

        assert!(
            matches!(ScanLock::acquire(dir.path()), LockOutcome::Acquired(_)),
            "a lock whose owner is gone must be taken over, not waited on"
        );
    }

    #[test]
    fn lock_with_unparseable_owner_is_taken_over() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(lock_path(dir.path()), b"not-a-pid").unwrap();

        assert!(
            matches!(ScanLock::acquire(dir.path()), LockOutcome::Acquired(_)),
            "a lock with no usable owner cannot be waited for"
        );
    }
}
