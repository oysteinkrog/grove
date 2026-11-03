//! The base checkout: a grove-owned worktree that `grove new` reflink-copies.
//!
//! The repo's main checkout is a poor copy source. It is often far behind
//! upstream, and it carries ignored files (local databases, settings, virtual
//! environments) that `git clean -fd` keeps, so every copy would inherit them.
//! The base checkout is a clean, detached worktree at the default upstream ref,
//! kept at `<work_dir>/.grove/base`. Nobody works in it.
//!
//! A lock file next to it (`<work_dir>/.grove/base.lock`) is held while the
//! base is moved and copied, so two `grove new` runs at once cannot see it
//! half-updated.
//!
//! Stable branches differ too much from the default branch to share its
//! blocks: a lane on stable/26.1 copied from the default base rewrote about
//! 1.4 GiB. Each stable branch therefore gets its own base,
//! `<work_dir>/.grove/base-stable-<version>`, with its own lock. It is made
//! once as a reflink copy of the default base, so it costs only the files
//! that differ. `GROVE_STABLE_BASES=0` sends stable lanes back to the
//! default base.

use super::shell_backend::{ShellBackend, has_submodules, reflink_enabled};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Path of the base checkout for a work_dir.
pub fn base_path(work_dir: &Path) -> PathBuf {
    work_dir.join(".grove").join("base")
}

/// Path of the base checkout for stable branch `version` (e.g. "26.1").
pub fn stable_base_path(work_dir: &Path, version: &str) -> PathBuf {
    work_dir
        .join(".grove")
        .join(format!("base-stable-{version}"))
}

/// True when `path` is a base checkout of `work_dir` (the default base or a
/// stable base), or inside one.
pub fn is_base_path(work_dir: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(work_dir.join(".grove")) else {
        return false;
    };
    match rest.components().next() {
        Some(std::path::Component::Normal(name)) => {
            let name = name.to_string_lossy();
            name == "base" || name.starts_with("base-stable-")
        }
        _ => false,
    }
}

/// The stable version a base ref points at: "26.1" for `<upstream>/stable/26.1`
/// or `stable/26.1`. `None` for anything else, or when `GROVE_STABLE_BASES` is
/// set to 0, off, never or false.
pub fn stable_version<'a>(upstream: &str, base_ref: &'a str) -> Option<&'a str> {
    if stable_bases_disabled(std::env::var(STABLE_BASES_ENV).ok().as_deref()) {
        return None;
    }
    let rest = base_ref
        .strip_prefix(upstream)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(base_ref);
    let version = rest.strip_prefix("stable/")?;
    (!version.is_empty() && !version.contains('/')).then_some(version)
}

/// True when the value of `GROVE_STABLE_BASES` turns stable bases off.
fn stable_bases_disabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "off" | "never" | "false"
        )
    })
}

/// Environment variable that turns per-stable-branch bases off.
pub const STABLE_BASES_ENV: &str = "GROVE_STABLE_BASES";

/// The base checkout, ready to copy, with its lock held. Drop the lock once
/// the copy is done.
pub struct PreparedBase {
    pub path: PathBuf,
    pub lock: File,
}

impl ShellBackend {
    /// Create the base checkout on first use, or move it to `base_ref`, and
    /// check that it is clean. Returns `None`, after printing why, when the
    /// base cannot be used; the caller then makes a plain worktree.
    ///
    /// `target_dir` is the directory the new worktree goes in. The base must
    /// be on the same filesystem, or reflinks would fail.
    pub fn prepare_base(
        &self,
        repo_path: &Path,
        work_dir: &Path,
        target_dir: &Path,
        base_ref: &str,
    ) -> Option<PreparedBase> {
        if !cfg!(target_os = "linux") || !reflink_enabled() || has_submodules(repo_path) {
            return None;
        }
        let base = base_path(work_dir);
        self.prepare_base_at(repo_path, &base, None, target_dir, base_ref)
    }

    /// Same as [`Self::prepare_base`] for the base of stable branch `version`.
    /// On first use the stable base is made as a reflink copy of the default
    /// base when that exists, so it costs only the files that differ.
    pub fn prepare_stable_base(
        &self,
        repo_path: &Path,
        work_dir: &Path,
        target_dir: &Path,
        upstream: &str,
        version: &str,
    ) -> Option<PreparedBase> {
        if !cfg!(target_os = "linux") || !reflink_enabled() || has_submodules(repo_path) {
            return None;
        }
        let base = stable_base_path(work_dir, version);
        let seed = base_path(work_dir);
        let base_ref = format!("{upstream}/stable/{version}");
        self.prepare_base_at(repo_path, &base, Some(&seed), target_dir, &base_ref)
    }

    fn prepare_base_at(
        &self,
        repo_path: &Path,
        base: &Path,
        seed: Option<&Path>,
        target_dir: &Path,
        base_ref: &str,
    ) -> Option<PreparedBase> {
        let grove_dir = base.parent()?.to_path_buf();
        match self.prepare_base_inner(repo_path, &grove_dir, base, seed, target_dir, base_ref) {
            Ok(prepared) => prepared,
            Err(msg) => {
                eprintln!(
                    "warning: not using the base checkout {}: {msg}",
                    base.display()
                );
                None
            }
        }
    }

    fn prepare_base_inner(
        &self,
        repo_path: &Path,
        grove_dir: &Path,
        base: &Path,
        seed: Option<&Path>,
        target_dir: &Path,
        base_ref: &str,
    ) -> Result<Option<PreparedBase>, String> {
        std::fs::create_dir_all(grove_dir).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(target_dir).map_err(|e| e.to_string())?;
        if !same_device(grove_dir, target_dir) {
            // Not an error: reflinks just cannot cross filesystems.
            return Ok(None);
        }

        let lock = lock_base(base)?;

        if base.exists() {
            if !base.join(".git").is_file() {
                return Err("it exists but is not a git worktree; remove it to recreate".into());
            }
            self.run(base, &["checkout", "-q", "--detach", base_ref])
                .map_err(|e| e.to_string())?;
        } else {
            eprintln!(
                "Creating the base checkout {} at {base_ref} (first use, takes a while)",
                base.display()
            );
            let base_str = base.to_str().ok_or("path is not UTF-8")?;
            let seed = seed.filter(|s| s.join(".git").is_file());
            let created = match seed {
                // Copy the seed base, then let `reset --hard` rewrite only the
                // files that differ. Hold the seed's lock so it does not move
                // while it is copied.
                Some(seed) => lock_base(seed).and_then(|seed_lock| {
                    self.run(
                        repo_path,
                        &[
                            "worktree",
                            "add",
                            "--detach",
                            "--no-checkout",
                            base_str,
                            base_ref,
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                    self.fill_from_source(seed, base, Some(seed_lock))
                        .map_err(|e| e.to_string())
                }),
                None => self
                    .run(
                        repo_path,
                        &["worktree", "add", "--detach", base_str, base_ref],
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            };
            if let Err(e) = created {
                let _ = self.run(repo_path, &["worktree", "remove", "--force", base_str]);
                return Err(e);
            }
            // LFS files are full copies after a checkout; share their blocks
            // with the LFS store.
            self.lfs_dedup_or_warn(base);
        }

        let dirty = self
            .run_stdout(base, &["status", "--porcelain", "--ignored"])
            .map_err(|e| e.to_string())?;
        if !dirty.is_empty() {
            let first: Vec<&str> = dirty.lines().take(5).collect();
            return Err(format!(
                "it has changed, untracked or ignored files ({}); remove it with \
                 `git worktree remove --force {}` and grove recreates it",
                first.join(", "),
                base.display()
            ));
        }
        Ok(Some(PreparedBase {
            path: base.to_path_buf(),
            lock,
        }))
    }
}

/// Take the lock file of a base checkout, `<base>.lock` next to it, waiting
/// when another grove command holds it.
fn lock_base(base: &Path) -> Result<File, String> {
    let name = base.file_name().ok_or("base path has no name")?;
    let lock_path = base.with_file_name(format!("{}.lock", name.to_string_lossy()));
    let lock = File::create(lock_path).map_err(|e| e.to_string())?;
    if lock.try_lock().is_err() {
        eprintln!(
            "Waiting for another grove command to finish with the base checkout {}...",
            base.display()
        );
        lock.lock().map_err(|e| e.to_string())?;
    }
    Ok(lock)
}

fn same_device(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A work_dir with a main checkout at `master/`, two commits on `main`,
    /// placed under target/ so reflinks work where the build dir supports them.
    fn fixture() -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("base-tests");
        std::fs::create_dir_all(&root).ok()?;
        let tmp = tempfile::tempdir_in(&root).ok()?;
        let work_dir = tmp.path().to_path_buf();
        let main = work_dir.join("master");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join(".gitignore"), "*.db\n").unwrap();
        std::fs::write(main.join("a.txt"), "one\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "one"]);
        // Ignored and untracked files in the main checkout must never reach
        // a new worktree.
        std::fs::write(main.join("local.db"), "secret\n").unwrap();
        std::fs::write(main.join("scratch.txt"), "x\n").unwrap();
        Some((tmp, work_dir, main))
    }

    #[test]
    fn base_is_created_moved_and_kept_clean() {
        let Some((_tmp, work_dir, main)) = fixture() else {
            return;
        };
        let backend = ShellBackend::new();
        let scratch = work_dir.join(".scratch");
        let Some(prepared) = backend.prepare_base(&main, &work_dir, &scratch, "main") else {
            eprintln!("skipping: base not usable here (no reflink support?)");
            return;
        };
        assert_eq!(prepared.path, base_path(&work_dir));
        assert!(!prepared.path.join("local.db").exists());
        assert!(!prepared.path.join("scratch.txt").exists());
        drop(prepared);

        // A new upstream commit moves the base on the next use.
        std::fs::write(main.join("a.txt"), "two\n").unwrap();
        git(&main, &["commit", "-q", "-am", "two"]);
        let prepared = backend
            .prepare_base(&main, &work_dir, &scratch, "main")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(prepared.path.join("a.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            git(&prepared.path, &["rev-parse", "HEAD"]),
            git(&main, &["rev-parse", "main"])
        );

        // A copy from the base is clean and has none of master's local files.
        let target = scratch.join("lane");
        let base = prepared.path.clone();
        let used = backend
            .worktree_add_reflinked_locked(
                &main,
                &base,
                &target,
                "lane",
                Some("main"),
                Some(prepared.lock),
            )
            .unwrap();
        assert!(used);
        assert_eq!(git(&target, &["status", "--porcelain", "--ignored"]), "");
        assert!(!target.join("local.db").exists());

        // An ignored file in the base makes it unusable until removed.
        std::fs::write(base.join("junk.db"), "x\n").unwrap();
        assert!(
            backend
                .prepare_base(&main, &work_dir, &scratch, "main")
                .is_none()
        );
    }

    #[test]
    fn concurrent_prepares_serialize_on_the_lock() {
        let Some((_tmp, work_dir, main)) = fixture() else {
            return;
        };
        let scratch = work_dir.join(".scratch");
        if ShellBackend::new()
            .prepare_base(&main, &work_dir, &scratch, "main")
            .is_none()
        {
            return;
        }
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (work_dir, main, scratch) = (work_dir.clone(), main.clone(), scratch.clone());
                std::thread::spawn(move || {
                    let backend = ShellBackend::new();
                    let prepared = backend
                        .prepare_base(&main, &work_dir, &scratch, "main")
                        .expect("base usable");
                    let target = scratch.join(format!("lane-{i}"));
                    backend
                        .worktree_add_reflinked_locked(
                            &main,
                            &prepared.path.clone(),
                            &target,
                            &format!("lane-{i}"),
                            Some("main"),
                            Some(prepared.lock),
                        )
                        .unwrap();
                    git(&target, &["status", "--porcelain"])
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), "");
        }
    }

    #[test]
    fn base_path_check() {
        let wd = Path::new("/w");
        assert!(is_base_path(wd, Path::new("/w/.grove/base")));
        assert!(!is_base_path(wd, Path::new("/w/base")));
        assert!(!is_base_path(wd, Path::new("/w/.grove/basement")));
        assert!(is_base_path(wd, Path::new("/w/.grove/base-stable-26.1")));
        assert!(is_base_path(
            wd,
            Path::new("/w/.grove/base-stable-26.1/src")
        ));
        assert!(!is_base_path(wd, Path::new("/w/.scratch/base-stable-26.1")));
    }

    #[test]
    fn stable_version_parses_refs() {
        assert_eq!(stable_version("if", "if/stable/26.1"), Some("26.1"));
        assert_eq!(stable_version("if", "stable/26.2"), Some("26.2"));
        assert_eq!(stable_version("if", "if/master"), None);
        assert_eq!(stable_version("if", "my/stable/26.1-fix"), None);
        assert_eq!(stable_version("if", "if/stable/"), None);
    }

    #[test]
    fn stable_bases_env_values() {
        assert!(!stable_bases_disabled(None));
        assert!(!stable_bases_disabled(Some("1")));
        assert!(!stable_bases_disabled(Some("")));
        for off in ["0", "off", "OFF", "never", "false", "False"] {
            assert!(stable_bases_disabled(Some(off)), "{off}");
        }
    }

    #[test]
    fn stable_base_path_is_per_version() {
        let wd = Path::new("/w");
        assert_eq!(
            stable_base_path(wd, "26.1"),
            PathBuf::from("/w/.grove/base-stable-26.1")
        );
        assert_ne!(stable_base_path(wd, "26.1"), base_path(wd));
    }
}
