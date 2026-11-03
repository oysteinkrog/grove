//! Lanes as btrfs snapshots.
//!
//! A reflink copy of a lane makes one new inode and one set of extent
//! references per file: about a million entries for a big lane, and enough
//! metadata load to freeze the desktop. A snapshot of a subvolume shares the
//! whole tree instead. When the source (a base checkout, or a lane that is a
//! subvolume) is a subvolume, a new lane is a snapshot of it plus a small
//! `git reset --hard`.
//!
//! Off by default. `GROVE_SNAPSHOT=1` turns it on.
//!
//! The order of steps guards one hazard above all: a fresh snapshot's `.git`
//! file still points at the source's admin directory, so any git command run
//! in it would change the source's index and HEAD. The snapshot is made in
//! `<work_dir>/.grove/staging/<name>.<pid>`, its `.git` is replaced, and only
//! then is it renamed to the lane path and git run in it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::btrfs::{self, BtrfsError};
use super::shell_backend::{ShellBackend, has_submodules};
use crate::error::GroveError;

/// `GROVE_SNAPSHOT=1` (or `on`, `true`, `always`) makes new lanes btrfs
/// snapshots where the source allows it. Unset or anything else: off.
pub const SNAPSHOT_ENV: &str = "GROVE_SNAPSHOT";

/// True when snapshot lanes are switched on.
pub fn snapshot_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCE_ENABLED.with(|f| f.get()) {
        return on;
    }
    std::env::var(SNAPSHOT_ENV).is_ok_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "always"
        )
    })
}

/// Directory that holds snapshots until their `.git` is fixed.
pub fn staging_dir(work_dir: &Path) -> PathBuf {
    work_dir.join(".grove").join("staging")
}

/// How the new worktree is checked out.
#[derive(Debug, Clone, Copy)]
pub enum Checkout<'a> {
    /// `-b <branch> <start>`
    NewBranch { branch: &'a str, start: &'a str },
    /// An existing branch.
    Branch(&'a str),
    /// `--detach <rev>`, for base checkouts.
    Detach(&'a str),
}

impl Checkout<'_> {
    fn add_args<'b>(&'b self, target: &'b str) -> Vec<&'b str> {
        // Locked until the snapshot is in place, so a `git worktree prune`
        // meanwhile cannot drop the registration of a lane whose directory
        // does not exist yet.
        let mut args = vec![
            "worktree",
            "add",
            "--no-checkout",
            "--lock",
            "--reason",
            "grove: snapshot in progress",
        ];
        match self {
            Checkout::NewBranch { branch, start } => {
                args.extend(["-b", branch, target, start]);
            }
            Checkout::Branch(branch) => args.extend([target, branch]),
            Checkout::Detach(rev) => args.extend(["--detach", target, rev]),
        }
        args
    }
}

/// What the probe found: everything the snapshot needs from the source.
#[derive(Debug)]
pub struct Probe {
    source_index: PathBuf,
}

fn failed(cmd: impl Into<String>, stderr: impl Into<String>) -> GroveError {
    GroveError::GitCommandFailed {
        cmd: cmd.into(),
        stderr: stderr.into(),
    }
}

// Test hook: fail at a named step.
#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AT: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

// Test hook: turn snapshots on or off for this thread only, without
// touching the environment other tests read.
#[cfg(test)]
thread_local! {
    pub(crate) static FORCE_ENABLED: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

fn fail_point(_step: &'static str) -> Result<(), GroveError> {
    #[cfg(test)]
    if FAIL_AT.with(|f| f.get()) == Some(_step) {
        return Err(failed(format!("injected failure at {_step}"), "test"));
    }
    Ok(())
}

impl ShellBackend {
    /// Check, without changing anything, whether a lane at `target` can be a
    /// snapshot of `source`. `Err` holds the reason it cannot; the caller then
    /// uses the reflink copy or a plain checkout.
    pub fn snapshot_probe(&self, source: &Path, target: &Path) -> Result<Probe, String> {
        if !cfg!(target_os = "linux") {
            return Err("not Linux".into());
        }
        if !snapshot_enabled() {
            return Err(format!("{SNAPSHOT_ENV} is not on"));
        }
        if !btrfs::is_own_subvolume(source) {
            return Err(format!(
                "{} is not a btrfs subvolume owned by this user",
                source.display()
            ));
        }
        if !source.join(".git").is_file() {
            return Err(format!("{} is not a linked worktree", source.display()));
        }
        let parent = target.parent().ok_or("target has no parent")?;
        let parent_dir = nearest_existing(parent);
        if !btrfs::same_filesystem(source, &parent_dir) {
            return Err("source and target are on different filesystems".into());
        }
        if has_submodules(source) {
            return Err("the source has submodules".into());
        }
        if !btrfs::btrfs_available() {
            return Err("the btrfs command is not on PATH".into());
        }
        let git_path = |what: &str| {
            self.run_stdout(
                source,
                &["rev-parse", "--path-format=absolute", "--git-path", what],
            )
            .map(PathBuf::from)
            .map_err(|e| e.to_string())
        };
        // Per-worktree config and index state a copied index would carry
        // without the config that explains it.
        if git_path("config.worktree")?.exists() {
            return Err("the source has per-worktree config (config.worktree)".into());
        }
        if git_path("info/sparse-checkout")?.exists() {
            return Err("the source has a sparse checkout".into());
        }
        for key in ["core.splitIndex", "core.fsmonitor"] {
            let value = self
                .run_stdout(source, &["config", "--get", key])
                .unwrap_or_default();
            if !value.is_empty() && value != "false" {
                return Err(format!("{key} is set"));
            }
        }
        let flags = self
            .run_stdout(source, &["ls-files", "-v"])
            .map_err(|e| e.to_string())?;
        if flags
            .lines()
            .any(|l| l.starts_with('S') || l.starts_with(|c: char| c.is_ascii_lowercase()))
        {
            return Err("the source index has skip-worktree or assume-unchanged entries".into());
        }
        Ok(Probe {
            source_index: git_path("index")?,
        })
    }

    /// Make the worktree `target` as a snapshot of `source`, after
    /// [`Self::snapshot_probe`] passed. `source_lock` is held until the
    /// snapshot and the index copy are both done.
    ///
    /// On failure nothing is left behind but the branch, which is kept as
    /// after any failed `grove new`. A snapshot that fails for lack of space
    /// is an error, never a reason to fall back to a full copy.
    #[allow(clippy::too_many_arguments)]
    pub fn worktree_add_snapshot(
        &self,
        repo_path: &Path,
        source: &Path,
        target: &Path,
        checkout: Checkout<'_>,
        staging: &Path,
        probe: Probe,
        source_lock: Option<File>,
    ) -> Result<(), GroveError> {
        let parent = target
            .parent()
            .ok_or_else(|| failed("snapshot", "target has no parent"))?;
        for dir in [parent, staging] {
            std::fs::create_dir_all(dir)
                .map_err(|e| failed(format!("mkdir -p {}", dir.display()), e.to_string()))?;
        }
        if target.exists() {
            return Err(failed(
                "snapshot",
                format!("{} already exists", target.display()),
            ));
        }
        sweep_staging(repo_path, staging);
        btrfs::check_headroom(parent).map_err(|e| failed("snapshot headroom", e))?;

        // The reset after the snapshot can rewrite many files when the source
        // is far from the new branch: serialize it with other copies.
        let copy_lock =
            crate::machine_lock::MachineLock::acquire("copy", &crate::machine_lock::tag_of(target))
                .map_err(|e| failed("take the grove copy lock", e))?;

        let target_str = target
            .to_str()
            .ok_or_else(|| failed("snapshot", "path is not UTF-8"))?;
        self.run(repo_path, &checkout.add_args(target_str))?;

        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staged = staging.join(format!("{name}.{}", std::process::id()));
        // A record of the registration, so a run that dies from here on can
        // be cleaned up by the next one (see `sweep_staging`).
        let journal = journal_path(&staged);
        let result = std::fs::write(&journal, format!("{}\n", target.display()))
            .map_err(|e| failed(format!("write {}", journal.display()), e.to_string()))
            .and_then(|_| self.fill_by_snapshot(source, target, &staged, &probe, source_lock));
        if let Err(e) = result {
            let cleanup = self.roll_back(repo_path, target, &staged);
            // Kept when the rollback was incomplete, so the next run retries.
            if !registered_state(repo_path, target).0 {
                let _ = std::fs::remove_file(&journal);
            }
            drop(copy_lock);
            return Err(failed(
                format!("snapshot {} -> {}", source.display(), target.display()),
                format!(
                    "{e}\n{cleanup} The branch was kept. Unset {SNAPSHOT_ENV} to copy instead."
                ),
            ));
        }
        let _ = std::fs::remove_file(&journal);
        drop(copy_lock);
        Ok(())
    }

    fn fill_by_snapshot(
        &self,
        source: &Path,
        target: &Path,
        staged: &Path,
        probe: &Probe,
        source_lock: Option<File>,
    ) -> Result<(), GroveError> {
        // 1. Keep the gitfile git wrote, then clear the way for the rename.
        let dot_git = target.join(".git");
        let gitfile = std::fs::read_to_string(&dot_git)
            .map_err(|e| failed(format!("read {}", dot_git.display()), e.to_string()))?;
        let admin = admin_dir_of(&gitfile, target)?;
        std::fs::remove_file(&dot_git)
            .and_then(|_| std::fs::remove_dir(target))
            .map_err(|e| failed(format!("clear {}", target.display()), e.to_string()))?;
        fail_point("after-add")?;

        // 2. Snapshot and index copy, both under the source lock, so files
        //    and index come from the same state.
        btrfs::snapshot(source, staged, false).map_err(|e| match e {
            BtrfsError::NoSpace(m) => failed("btrfs subvolume snapshot", m),
            BtrfsError::Failed(m) => failed("btrfs subvolume snapshot", m),
        })?;
        fail_point("after-snapshot")?;
        if probe.source_index.exists() {
            let out = Command::new("cp")
                .arg("--reflink=auto")
                .arg(&probe.source_index)
                .arg(admin.join("index"))
                .output()
                .map_err(|e| failed("cp index", e.to_string()))?;
            if !out.status.success() {
                return Err(failed(
                    "cp index",
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ));
            }
        }
        drop(source_lock);
        fail_point("after-index")?;

        // 3. Point the copy at its own admin directory. Nothing has run in
        //    it yet.
        // create_new: never write through a file or symlink of that name the
        // source happened to have.
        let tmp = staged.join(format!(".git.grove-new-{}", std::process::id()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| std::io::Write::write_all(&mut f, gitfile.as_bytes()))
            .and_then(|_| std::fs::rename(&tmp, staged.join(".git")))
            .map_err(|e| failed(format!("write {}/.git", staged.display()), e.to_string()))?;
        fail_point("after-gitfile")?;

        // 4. Nested worktrees and subvolumes.
        fix_nested_scratch(staged)?;

        // 5. Publish.
        std::fs::rename(staged, target).map_err(|e| {
            failed(
                format!("rename {} {}", staged.display(), target.display()),
                e.to_string(),
            )
        })?;
        fail_point("after-rename")?;
        self.run(
            target,
            &["worktree", "unlock", target.to_str().unwrap_or_default()],
        )?;

        // 6. The snapshot keeps inode numbers and times, so the refresh
        //    should match most entries by stat data alone. It exits non-zero
        //    when the source had edits; the reset puts those right.
        let _ = self.run(target, &["update-index", "-q", "--refresh"]);
        self.run(target, &["reset", "--hard", "-q", "HEAD"])?;
        self.run(target, &["clean", "-fdq"])?;
        fail_point("after-reset")?;
        Ok(())
    }

    /// Undo a failed snapshot lane: the staged or published subvolume, then
    /// this lane's admin directory only. Returns a sentence for the error.
    fn roll_back(&self, repo_path: &Path, target: &Path, staged: &Path) -> String {
        let mut problems = Vec::new();
        for path in [staged, target] {
            if btrfs::is_subvolume(path)
                && let Err(e) = btrfs::delete_subvolume(path, &[path.join(".scratch")])
            {
                problems.push(format!("could not delete {}: {e}", path.display()));
            }
        }
        // `worktree remove --force` on a registered worktree whose directory
        // is gone removes only its admin directory. Never a global prune.
        if !target.exists()
            && let Err(e) = self.run(
                repo_path,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--force",
                    target.to_str().unwrap_or_default(),
                ],
            )
        {
            problems.push(format!("could not deregister {}: {e}", target.display()));
        } else if target.exists() {
            problems.push(format!("{} is still there", target.display()));
        }
        if problems.is_empty() {
            "The half-made lane was removed.".into()
        } else {
            problems.join("; ")
        }
    }
}

/// The admin directory a gitfile names, checked: an absolute path that
/// exists and whose `gitdir` file points back at `<target>/.git`.
fn admin_dir_of(gitfile: &str, target: &Path) -> Result<PathBuf, GroveError> {
    let bad = |why: String| failed(format!("check {}/.git", target.display()), why);
    let admin = gitfile
        .trim()
        .strip_prefix("gitdir: ")
        .map(PathBuf::from)
        .ok_or_else(|| bad(format!("unexpected gitfile: {gitfile:?}")))?;
    if !admin.is_absolute() || !admin.is_dir() {
        return Err(bad(format!(
            "admin directory {} is not usable",
            admin.display()
        )));
    }
    let back = std::fs::read_to_string(admin.join("gitdir"))
        .map_err(|e| bad(format!("read {}/gitdir: {e}", admin.display())))?;
    if Path::new(back.trim()) != target.join(".git") {
        return Err(bad(format!(
            "{}/gitdir points at {}, not at this lane",
            admin.display(),
            back.trim()
        )));
    }
    Ok(admin)
}

/// A snapshot holds an empty placeholder for each nested subvolume, and a
/// copy of each nested worktree, whose `.git` still names the original's
/// admin directory. Remove those gitfiles, and give the copy its own empty
/// `.scratch` subvolume when the source's `.scratch` was empty, so forks of
/// it never carry nested worktrees.
fn fix_nested_scratch(staged: &Path) -> Result<(), GroveError> {
    let scratch = staged.join(crate::cli::new::SCRATCH_DIR);
    match std::fs::symlink_metadata(&scratch) {
        Ok(m) if m.file_type().is_symlink() => {
            // Never follow it: it may point at real worktrees. Drop the link.
            return std::fs::remove_file(&scratch)
                .map_err(|e| failed(format!("rm {}", scratch.display()), e.to_string()));
        }
        Ok(m) if m.is_dir() => {}
        _ => return Ok(()),
    }
    remove_nested_gitfiles(&scratch)
        .map_err(|e| failed(format!("clean {}", scratch.display()), e.to_string()))?;
    let empty = std::fs::read_dir(&scratch)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if empty && !btrfs::is_subvolume(&scratch) {
        std::fs::remove_dir(&scratch)
            .map_err(|e| failed(format!("rmdir {}", scratch.display()), e.to_string()))?;
        btrfs::create_subvolume(&scratch)
            .map_err(|e| failed("btrfs subvolume create", e.to_string()))?;
    }
    Ok(())
}

fn remove_nested_gitfiles(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if entry.file_name() == ".git" && kind.is_file() {
            std::fs::remove_file(entry.path())?;
        } else if kind.is_dir() && entry.file_name() != ".git" {
            remove_nested_gitfiles(&entry.path())?;
        }
    }
    Ok(())
}

fn journal_path(staged: &Path) -> PathBuf {
    let mut name = staged.file_name().unwrap_or_default().to_os_string();
    name.push(".journal");
    staged.with_file_name(name)
}

/// Whether `target` is a registered worktree of `repo_path`, and whether it
/// is locked. When the list cannot be read, both are assumed true.
fn registered_state(repo_path: &Path, target: &Path) -> (bool, bool) {
    let Ok(list) = ShellBackend::new().run_stdout(repo_path, &["worktree", "list", "--porcelain"])
    else {
        return (true, true);
    };
    let block = list.split("\n\n").find(|b| {
        b.lines()
            .next()
            .and_then(|l| l.strip_prefix("worktree "))
            .is_some_and(|p| Path::new(p) == target)
    });
    match block {
        None => (false, false),
        Some(b) => (
            true,
            b.lines().any(|l| l == "locked" || l.starts_with("locked ")),
        ),
    }
}

fn pid_alive(pid: u32) -> bool {
    pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists()
}

/// Clean up after grove runs that died (their pid is gone): delete their
/// staged snapshots, and for each journal, the locked registration of a
/// lane that never got its directory (only that one). A lane that did get
/// its directory is unlocked and left for `grove gc` to report.
fn sweep_staging(repo_path: &Path, staging: &Path) {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".journal") else {
            continue;
        };
        let Some(pid) = stem
            .rsplit_once('.')
            .and_then(|(_, p)| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let target = PathBuf::from(text.trim());
        let backend = ShellBackend::new();
        let t = target.to_str().unwrap_or_default();
        if !target.as_os_str().is_empty() {
            if !target.exists() {
                eprintln!(
                    "Removing the registration of {}, left by an earlier grove run",
                    target.display()
                );
                let _ = backend.run(repo_path, &["worktree", "remove", "--force", "--force", t]);
            } else {
                let _ = backend.run(repo_path, &["worktree", "unlock", t]);
            }
        }
        // Done only when the registration is gone, or there and unlocked;
        // else the journal stays for the next run to retry.
        let (registered, locked) = registered_state(repo_path, &target);
        let done = if target.exists() {
            !locked
        } else {
            !registered
        };
        if target.as_os_str().is_empty() || done {
            let _ = std::fs::remove_file(entry.path());
        } else {
            eprintln!(
                "warning: could not clean up the registration of {}; will retry",
                target.display()
            );
        }
    }
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".journal") {
            continue;
        }
        let Some(pid) = name
            .rsplit_once('.')
            .and_then(|(_, p)| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid_alive(pid) {
            continue;
        }
        let path = entry.path();
        eprintln!("Removing {} left by an earlier grove run", path.display());
        let removed = if btrfs::is_subvolume(&path) {
            btrfs::delete_subvolume(&path, &[path.join(".scratch")]).map(|_| ())
        } else {
            std::fs::remove_dir_all(&path).map_err(|e| BtrfsError::Failed(e.to_string()))
        };
        if let Err(e) = removed {
            eprintln!("warning: could not remove {}: {e}", path.display());
        }
    }
}

fn nearest_existing(path: &Path) -> PathBuf {
    let mut p = path;
    loop {
        if p.exists() {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests;

impl ShellBackend {
    /// Remove the lane at `path` when it is a btrfs subvolume and
    /// `user_subvol_rm_allowed` is set: `btrfs subvolume delete`, which
    /// returns at once, then only this lane's admin directory.
    ///
    /// Returns `Ok(false)`, having changed nothing, when the fast path does
    /// not apply (not a subvolume, the mount option is missing, submodules).
    /// The caller then removes the lane as before, file by file; `rmdir`
    /// removes the empty subvolumes on the way.
    ///
    /// Repeats what `git worktree remove` would check, failing closed: a
    /// locked lane is refused, and without `force` so is a lane with
    /// modified or untracked files, or with anything in its nested
    /// `.scratch` subvolume.
    pub fn remove_subvolume_lane(
        &self,
        repo_path: &Path,
        path: &Path,
        force: bool,
    ) -> Result<bool, String> {
        if !btrfs::is_subvolume(path) {
            return Ok(false);
        }
        if !btrfs::rm_allowed(path) {
            eprintln!(
                "note: {} is a btrfs subvolume, but user_subvol_rm_allowed is not set on its \
                 filesystem, so it is deleted file by file",
                path.display()
            );
            return Ok(false);
        }
        if has_submodules(path) {
            return Ok(false);
        }
        let nested = self.check_subvolume_lane(repo_path, path, force)?;
        btrfs::delete_subvolume(path, &nested).map_err(|e| e.to_string())?;
        // The directory is gone: this removes only its admin directory.
        self.run(
            repo_path,
            &[
                "worktree",
                "remove",
                "--force",
                path.to_str().unwrap_or_default(),
            ],
        )
        .map_err(|e| {
            format!(
                "deleted {} but could not deregister it: {e}",
                path.display()
            )
        })?;
        Ok(true)
    }

    /// The checks behind [`Self::remove_subvolume_lane`]. Returns the nested
    /// subvolumes to delete first.
    pub(crate) fn check_subvolume_lane(
        &self,
        repo_path: &Path,
        path: &Path,
        force: bool,
    ) -> Result<Vec<PathBuf>, String> {
        let gitfile = std::fs::read_to_string(path.join(".git"))
            .map_err(|e| format!("{} has no readable .git file: {e}", path.display()))?;
        let admin = admin_dir_of(&gitfile, path).map_err(|e| e.to_string())?;

        // Registered, and not locked: git needs --force twice for a locked
        // worktree, and grove never passes that.
        let list = self
            .run_stdout(repo_path, &["worktree", "list", "--porcelain"])
            .map_err(|e| e.to_string())?;
        let entry = list
            .split("\n\n")
            .find(|block| {
                block
                    .lines()
                    .next()
                    .and_then(|l| l.strip_prefix("worktree "))
                    .is_some_and(|p| Path::new(p) == path)
            })
            .ok_or_else(|| format!("{} is not a registered worktree", path.display()))?;
        if entry
            .lines()
            .any(|l| l == "locked" || l.starts_with("locked "))
        {
            return Err(format!(
                "{} is locked; unlock it with `git worktree unlock` first",
                path.display()
            ));
        }
        if !admin.join("gitdir").is_file() {
            return Err(format!("{} has no admin gitdir file", admin.display()));
        }

        let scratch = path.join(crate::cli::new::SCRATCH_DIR);
        if !force {
            let status = self
                .run_stdout(path, &["status", "--porcelain", "--untracked-files=all"])
                .map_err(|e| format!("could not check {}: {e}", path.display()))?;
            if !status.is_empty() {
                return Err(format!(
                    "{} has modified or untracked files",
                    path.display()
                ));
            }
            let busy = std::fs::read_dir(&scratch).is_ok_and(|mut d| d.next().is_some());
            if busy {
                return Err(format!(
                    "{} is not empty; it may hold nested worktrees",
                    scratch.display()
                ));
            }
        }
        Ok(vec![scratch])
    }
}
