use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::GroveError;

use super::WorktreeMutator;

pub struct ShellBackend {
    git_path: PathBuf,
}

impl ShellBackend {
    pub fn new() -> Self {
        Self {
            git_path: PathBuf::from("git"),
        }
    }

    #[cfg(test)]
    fn with_git_path(git_path: PathBuf) -> Self {
        Self { git_path }
    }

    fn run(&self, repo_path: &Path, args: &[&str]) -> Result<(), GroveError> {
        let cmd_str = format!(
            "{} -C {} {}",
            self.git_path.display(),
            repo_path.display(),
            args.join(" ")
        );
        let output = Command::new(&self.git_path)
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .map_err(|e| GroveError::GitCommandFailed {
                cmd: cmd_str.clone(),
                stderr: e.to_string(),
            })?;

        if output.status.success() {
            Ok(())
        } else {
            Err(GroveError::GitCommandFailed {
                cmd: cmd_str,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}

impl Default for ShellBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// `GROVE_REFLINK=0` (or `off`, `never`, `false`) turns reflink worktree
/// creation off and always uses a plain `git worktree add`.
pub const REFLINK_ENV: &str = "GROVE_REFLINK";

impl ShellBackend {
    fn run_stdout(&self, repo_path: &Path, args: &[&str]) -> Result<String, GroveError> {
        let cmd_str = format!(
            "{} -C {} {}",
            self.git_path.display(),
            repo_path.display(),
            args.join(" ")
        );
        let output = Command::new(&self.git_path)
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .map_err(|e| GroveError::GitCommandFailed {
                cmd: cmd_str.clone(),
                stderr: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(GroveError::GitCommandFailed {
                cmd: cmd_str,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Create a worktree as a copy-on-write clone of an existing checkout.
    ///
    /// A plain `git worktree add` writes every tracked file again, so each
    /// worktree of a large repo costs its full size on disk. On a filesystem
    /// with reflinks (Btrfs, XFS) this instead:
    ///
    /// 1. runs `git worktree add --no-checkout`,
    /// 2. reflink-copies everything in `source` except `.git` into `target`,
    ///    including ignored build output, so the new tree starts with a warm
    ///    build cache at almost no disk cost,
    /// 3. copies the index of `source`, so git knows which files it has,
    /// 4. runs `git update-index --refresh` and then `git reset --hard`, which
    ///    rewrites only the files that differ between the commit of `source`
    ///    and the new branch,
    /// 5. runs `git clean -fd`, which drops untracked files carried over from
    ///    `source` but keeps ignored ones.
    ///
    /// Uncommitted edits in `source` do not reach `target`: step 4 resets them.
    ///
    /// Returns `Ok(false)`, having changed nothing, when reflinks are not
    /// available (other OS, other filesystem, `GROVE_REFLINK=0`, or `source`
    /// has submodules). The caller then falls back to [`WorktreeMutator::worktree_add`].
    pub fn worktree_add_reflinked(
        &self,
        repo_path: &Path,
        source: &Path,
        target: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<bool, GroveError> {
        if !reflink_enabled()
            || !source.join(".git").exists()
            || source.join(".gitmodules").exists()
        {
            return Ok(false);
        }
        let Some(parent) = target.parent() else {
            return Ok(false);
        };
        std::fs::create_dir_all(parent).map_err(|e| GroveError::GitCommandFailed {
            cmd: format!("mkdir -p {}", parent.display()),
            stderr: e.to_string(),
        })?;
        if !reflink_probe(source, parent) {
            return Ok(false);
        }

        let target_str = target.to_str().unwrap_or_default();
        match base {
            Some(base_ref) => self.run(
                repo_path,
                &[
                    "worktree",
                    "add",
                    "--no-checkout",
                    "-b",
                    branch,
                    target_str,
                    base_ref,
                ],
            )?,
            None => self.run(
                repo_path,
                &["worktree", "add", "--no-checkout", target_str, branch],
            )?,
        }

        if let Err(e) = self.fill_from_source(source, target) {
            // Leave nothing half-built behind. The branch stays, as it would
            // after any failed `grove new`, and the error says so.
            let _ = self.run(repo_path, &["worktree", "remove", "--force", target_str]);
            return Err(GroveError::GitCommandFailed {
                cmd: format!("reflink copy {} -> {}", source.display(), target.display()),
                stderr: format!(
                    "{e}\nThe worktree was removed. Branch '{branch}' was kept. Set {REFLINK_ENV}=0 to use a plain git worktree add."
                ),
            });
        }
        Ok(true)
    }

    fn fill_from_source(&self, source: &Path, target: &Path) -> Result<(), GroveError> {
        let io_err = |what: String, e: std::io::Error| GroveError::GitCommandFailed {
            cmd: what,
            stderr: e.to_string(),
        };

        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(source)
            .map_err(|e| io_err(format!("read_dir {}", source.display()), e))?
        {
            let entry = entry.map_err(|e| io_err(format!("read_dir {}", source.display()), e))?;
            if entry.file_name() != ".git" {
                entries.push(entry.path());
            }
        }
        if !entries.is_empty() {
            let output = Command::new("cp")
                .args(["-a", "--reflink=always", "-t"])
                .arg(target)
                .args(&entries)
                .output()
                .map_err(|e| io_err("cp -a --reflink=always".to_string(), e))?;
            if !output.status.success() {
                return Err(GroveError::GitCommandFailed {
                    cmd: format!(
                        "cp -a --reflink=always -t {} <entries of {}>",
                        target.display(),
                        source.display()
                    ),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
        }

        let source_index = self.run_stdout(
            source,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let target_index = self.run_stdout(
            target,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        if Path::new(&source_index).exists() {
            let output = Command::new("cp")
                .args(["--reflink=auto", &source_index, &target_index])
                .output()
                .map_err(|e| io_err("cp index".to_string(), e))?;
            if !output.status.success() {
                return Err(GroveError::GitCommandFailed {
                    cmd: format!("cp {source_index} {target_index}"),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
        }

        // The copied index still holds the inode and ctime of the source files,
        // so `reset --hard` would treat every file as changed and write it out
        // again, which breaks the sharing. A refresh first reads each file once,
        // confirms its content, and records the new stat data. It exits non-zero
        // when source files had uncommitted edits; that is expected here, and
        // the reset below puts those files right.
        let _ = self.run(target, &["update-index", "-q", "--refresh"]);
        self.run(target, &["reset", "--hard", "-q", "HEAD"])?;
        self.run(target, &["clean", "-fdq"])?;
        Ok(())
    }
}

fn reflink_enabled() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    match std::env::var(REFLINK_ENV) {
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "off" | "never" | "false"
        ),
        Err(_) => true,
    }
}

/// Try one real reflink from `source` into `dir`. This catches every reason a
/// reflink can fail (filesystem, crossing filesystems, `cp` without
/// `--reflink`) before anything is created.
fn reflink_probe(source: &Path, dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(source) else {
        return false;
    };
    let Some(file) = entries
        .filter_map(|e| e.ok())
        .find(|e| e.file_name() != ".git" && e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
    else {
        return false;
    };
    let probe = dir.join(format!(".grove-reflink-probe-{}", std::process::id()));
    let ok = Command::new("cp")
        .args(["--reflink=always"])
        .arg(&file)
        .arg(&probe)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    ok
}

impl ShellBackend {
    pub fn fetch(&self, repo_path: &Path, remote: &str) -> Result<(), GroveError> {
        self.run(repo_path, &["fetch", remote])
    }

    pub fn branch_delete(&self, repo_path: &Path, branch: &str) -> Result<(), GroveError> {
        self.run(repo_path, &["branch", "-D", branch])
    }

    pub fn remote_branch_delete(
        &self,
        repo_path: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<(), GroveError> {
        self.run(repo_path, &["push", remote, "--delete", branch])
    }

    /// Return true when `commit` is reachable from at least one remote-tracking
    /// branch (`refs/remotes/**`). Used to decide whether removing a worktree
    /// would lose committed work: if the commit lives on a remote (pushed, or
    /// merged into a remote branch), it is safe to remove even when the worktree
    /// is on a detached HEAD or its local branch has no upstream configured.
    ///
    /// Asked as `rev-list --no-walk <commit> --not --remotes`, which prints the
    /// commit when no remote ref reaches it and nothing when one does. The
    /// equivalent `branch -r --contains` runs one reachability query per remote
    /// ref, so it costs 31s against 0.5s on a repo with 9,758 of them. See
    /// [`crate::gc::guards::commit_is_unpushed`].
    pub fn commit_on_any_remote(&self, repo_path: &Path, commit: &str) -> Result<bool, GroveError> {
        let args = ["rev-list", "--no-walk", commit, "--not", "--remotes"];
        let cmd_str = format!(
            "{} -C {} {}",
            self.git_path.display(),
            repo_path.display(),
            args.join(" "),
        );
        let output = Command::new(&self.git_path)
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .map_err(|e| GroveError::GitCommandFailed {
                cmd: cmd_str.clone(),
                stderr: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(GroveError::GitCommandFailed {
                cmd: cmd_str,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }
}

impl WorktreeMutator for ShellBackend {
    fn worktree_add(
        &self,
        repo_path: &Path,
        target: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<(), GroveError> {
        if let Some(base_ref) = base {
            self.run(
                repo_path,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    target.to_str().unwrap_or_default(),
                    base_ref,
                ],
            )
        } else {
            self.run(
                repo_path,
                &[
                    "worktree",
                    "add",
                    target.to_str().unwrap_or_default(),
                    branch,
                ],
            )
        }
    }

    fn worktree_remove(
        &self,
        repo_path: &Path,
        target: &Path,
        force: bool,
    ) -> Result<(), GroveError> {
        if force {
            self.run(
                repo_path,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    target.to_str().unwrap_or_default(),
                ],
            )
        } else {
            self.run(
                repo_path,
                &["worktree", "remove", target.to_str().unwrap_or_default()],
            )
        }
    }

    fn worktree_move(&self, repo_path: &Path, old: &Path, new: &Path) -> Result<(), GroveError> {
        self.run(
            repo_path,
            &[
                "worktree",
                "move",
                old.to_str().unwrap_or_default(),
                new.to_str().unwrap_or_default(),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git_worktree_paths(repo_dir: &Path) -> Vec<PathBuf> {
        let out = Command::new("git")
            .args([
                "-C",
                repo_dir.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain",
            ])
            .output()
            .expect("git must be on PATH");
        assert!(out.status.success(), "git worktree list failed");
        let text = String::from_utf8(out.stdout).unwrap();
        let mut paths: Vec<PathBuf> = text
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect();
        paths.sort();
        paths
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        for args in [
            vec!["init", path.to_str().unwrap()],
            vec![
                "-C",
                path.to_str().unwrap(),
                "config",
                "user.email",
                "test@test.com",
            ],
            vec!["-C", path.to_str().unwrap(), "config", "user.name", "Test"],
        ] {
            let status = Command::new("git").args(&args).status().unwrap();
            assert!(status.success());
        }
        let readme = path.join("README.md");
        std::fs::write(&readme, b"test").unwrap();
        for args in [
            vec!["-C", path.to_str().unwrap(), "add", "."],
            vec!["-C", path.to_str().unwrap(), "commit", "-m", "init"],
        ] {
            let status = Command::new("git").args(&args).status().unwrap();
            assert!(status.success());
        }
        dir
    }

    #[test]
    fn worktree_add_appears_in_list() {
        let repo = init_repo();
        let wt_dir = TempDir::new().unwrap();
        let backend = ShellBackend::new();

        backend
            .worktree_add(repo.path(), wt_dir.path(), "feature-shell", Some("HEAD"))
            .expect("worktree_add should succeed");

        let paths = git_worktree_paths(repo.path());
        assert!(
            paths.contains(&wt_dir.path().to_path_buf()),
            "new worktree should appear in git worktree list"
        );
    }

    #[test]
    fn worktree_add_existing_branch() {
        let repo = init_repo();
        // Create a branch first
        Command::new("git")
            .args([
                "-C",
                repo.path().to_str().unwrap(),
                "branch",
                "existing-branch",
            ])
            .status()
            .unwrap();
        let wt_dir = TempDir::new().unwrap();
        let backend = ShellBackend::new();

        backend
            .worktree_add(repo.path(), wt_dir.path(), "existing-branch", None)
            .expect("worktree_add with existing branch should succeed");

        let paths = git_worktree_paths(repo.path());
        assert!(paths.contains(&wt_dir.path().to_path_buf()));
    }

    #[test]
    fn worktree_remove_disappears_from_list() {
        let repo = init_repo();
        let wt_dir = TempDir::new().unwrap();
        let backend = ShellBackend::new();

        backend
            .worktree_add(repo.path(), wt_dir.path(), "to-remove", Some("HEAD"))
            .expect("add should succeed");

        backend
            .worktree_remove(repo.path(), wt_dir.path(), false)
            .expect("remove should succeed");

        let paths = git_worktree_paths(repo.path());
        assert!(
            !paths.contains(&wt_dir.path().to_path_buf()),
            "removed worktree should not appear in git worktree list"
        );
    }

    #[test]
    fn worktree_remove_force_on_dirty_worktree() {
        let repo = init_repo();
        let wt_dir = TempDir::new().unwrap();
        let backend = ShellBackend::new();

        backend
            .worktree_add(repo.path(), wt_dir.path(), "dirty-branch", Some("HEAD"))
            .expect("add should succeed");

        // Make the worktree dirty
        let dirty_file = wt_dir.path().join("dirty.txt");
        std::fs::write(&dirty_file, b"uncommitted change").unwrap();

        // Without force, remove might fail on locked worktrees; with force it succeeds
        backend
            .worktree_remove(repo.path(), wt_dir.path(), true)
            .expect("force remove should succeed even on dirty worktree");

        let paths = git_worktree_paths(repo.path());
        assert!(
            !paths.contains(&wt_dir.path().to_path_buf()),
            "force-removed worktree should not appear in git worktree list"
        );
    }

    #[test]
    fn worktree_move_appears_at_new_path() {
        let repo = init_repo();
        let wt_dir = TempDir::new().unwrap();
        let backend = ShellBackend::new();

        backend
            .worktree_add(repo.path(), wt_dir.path(), "move-branch", Some("HEAD"))
            .expect("add should succeed");

        // New destination must not exist as a directory (git worktree move creates it)
        let new_parent = TempDir::new().unwrap();
        let new_path = new_parent.path().join("moved-worktree");

        backend
            .worktree_move(repo.path(), wt_dir.path(), &new_path)
            .expect("move should succeed");

        let paths = git_worktree_paths(repo.path());
        assert!(
            paths.contains(&new_path),
            "worktree should appear at new path after move"
        );
        assert!(
            !paths.contains(&wt_dir.path().to_path_buf()),
            "worktree should not appear at old path after move"
        );
    }

    #[test]
    fn error_path_missing_repo_returns_git_command_failed() {
        let backend = ShellBackend::new();
        let missing_repo = PathBuf::from("/tmp/does-not-exist-grove-shell-test-xyz");
        let target = PathBuf::from("/tmp/shell-test-target-xyz");

        let err = backend
            .worktree_add(&missing_repo, &target, "branch", Some("HEAD"))
            .unwrap_err();

        match &err {
            GroveError::GitCommandFailed { cmd, stderr } => {
                assert!(!cmd.is_empty(), "cmd should be non-empty");
                assert!(!stderr.is_empty(), "stderr should be non-empty");
            }
            other => panic!("expected GitCommandFailed, got {:?}", other),
        }
    }

    // ── reflink worktrees ─────────────────────────────────────────────────────

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Reflinks need a real filesystem that supports them; `/tmp` is often
    /// tmpfs. Use a folder next to the build output, and skip when the probe
    /// says reflinks do not work there.
    fn reflink_scratch() -> Option<TempDir> {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("reflink-tests");
        std::fs::create_dir_all(&base).unwrap();
        let dir = TempDir::new_in(&base).unwrap();
        std::fs::write(dir.path().join("probe-src"), b"x").unwrap();
        let ok = Command::new("cp")
            .args(["--reflink=always"])
            .arg(dir.path().join("probe-src"))
            .arg(dir.path().join("probe-dst"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok { Some(dir) } else { None }
    }

    /// True when `filefrag` reports the file's extents as shared (a reflink).
    /// Returns true when `filefrag` is missing, so the test only checks what it can.
    fn extents_shared(file: &Path) -> bool {
        match Command::new("filefrag").arg("-v").arg(file).output() {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).contains("shared")
            }
            _ => true,
        }
    }

    /// A repo with two commits: `main` has a.txt and old.txt, `feature` changes
    /// a.txt, deletes old.txt and adds new.txt. `build/` is ignored.
    fn two_branch_repo(root: &Path) -> PathBuf {
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@t"]);
        git(&main, &["config", "user.name", "T"]);
        std::fs::write(main.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(main.join("a.txt"), "main\n").unwrap();
        std::fs::write(main.join("old.txt"), "old\n").unwrap();
        // Big enough to get a real data extent, not inlined in metadata.
        std::fs::write(main.join("big.dat"), vec![7u8; 1 << 20]).unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "main"]);
        git(&main, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(main.join("a.txt"), "feature\n").unwrap();
        std::fs::remove_file(main.join("old.txt")).unwrap();
        std::fs::write(main.join("new.txt"), "new\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "feature"]);
        git(&main, &["checkout", "-q", "main"]);
        main
    }

    #[test]
    #[serial_test::serial]
    fn reflinked_worktree_matches_branch_and_keeps_ignored_files() {
        let Some(scratch) = reflink_scratch() else {
            eprintln!("skipping: no reflink support under target/");
            return;
        };
        let main = two_branch_repo(scratch.path());
        // State of the source checkout that must not leak into the new tree,
        // except the ignored build output, which should.
        std::fs::create_dir_all(main.join("build")).unwrap();
        std::fs::write(main.join("build").join("out.bin"), "cached\n").unwrap();
        std::fs::write(main.join("a.txt"), "uncommitted edit\n").unwrap();
        std::fs::write(main.join("stray.txt"), "untracked\n").unwrap();

        let target = scratch.path().join("wt");
        let backend = ShellBackend::new();
        let used = backend
            .worktree_add_reflinked(&main, &main, &target, "lane", Some("feature"))
            .unwrap();
        assert!(used, "reflink path should be taken on a reflink filesystem");

        assert_eq!(git(&target, &["rev-parse", "--abbrev-ref", "HEAD"]), "lane");
        assert_eq!(
            std::fs::read_to_string(target.join("a.txt")).unwrap(),
            "feature\n"
        );
        assert!(target.join("new.txt").exists());
        assert!(
            !target.join("old.txt").exists(),
            "file deleted on the branch must go"
        );
        assert!(
            !target.join("stray.txt").exists(),
            "untracked file must not carry over"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("build").join("out.bin")).unwrap(),
            "cached\n",
            "ignored build output should carry over"
        );
        assert_eq!(
            git(&target, &["status", "--porcelain"]),
            "",
            "new worktree must be clean"
        );
        assert!(
            extents_shared(&target.join("big.dat")),
            "a tracked file unchanged between the two commits must share its extents with the source"
        );
        // The source checkout is untouched.
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "uncommitted edit\n"
        );
        assert!(git_worktree_paths(&main).contains(&target));
    }

    #[test]
    #[serial_test::serial]
    fn reflink_env_off_leaves_everything_unchanged() {
        let Some(scratch) = reflink_scratch() else {
            return;
        };
        let main = two_branch_repo(scratch.path());
        let target = scratch.path().join("wt");
        // SAFETY: serial test; no other thread reads the environment meanwhile.
        unsafe { std::env::set_var(REFLINK_ENV, "0") };
        let used = ShellBackend::new().worktree_add_reflinked(
            &main,
            &main,
            &target,
            "lane",
            Some("feature"),
        );
        unsafe { std::env::remove_var(REFLINK_ENV) };
        assert!(!used.unwrap());
        assert!(!target.exists());
        assert!(git(&main, &["branch", "--list", "lane"]).is_empty());
    }
}
