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
}
