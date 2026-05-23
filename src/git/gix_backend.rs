use std::path::Path;

use gix::ThreadSafeRepository;

use super::{Worktree, WorktreeInfo, WorktreeManager};
use crate::error::GroveError;

pub struct GixBackend;

impl WorktreeManager for GixBackend {
    fn list(&self, main: &Path) -> Result<Vec<WorktreeInfo>, GroveError> {
        let repo = open_repo(main)?;
        let repo = repo.to_thread_local();

        let main_head = head_oid(&repo);
        let main_branch = head_branch(&repo);
        let mut result = vec![WorktreeInfo {
            path: main.to_path_buf(),
            branch: main_branch,
            head: main_head,
        }];

        // Linked worktrees stored in .git/worktrees/
        let linked = repo
            .worktrees()
            .map_err(|_| GroveError::WorktreeNotFound(main.to_path_buf()))?;

        for proxy in linked {
            let wt_path = proxy.base().unwrap_or_default();
            let wt_repo = proxy.into_repo();
            let (branch, head) = match wt_repo {
                Ok(r) => (head_branch(&r), head_oid(&r)),
                Err(_) => (None, None),
            };
            result.push(WorktreeInfo {
                path: wt_path,
                branch,
                head,
            });
        }

        Ok(result)
    }

    fn open(&self, path: &Path) -> Result<Worktree, GroveError> {
        let repo = open_repo(path)?;
        let repo = repo.to_thread_local();
        Ok(Worktree {
            branch: head_branch(&repo),
            head: head_oid(&repo),
        })
    }
}

fn open_repo(path: &Path) -> Result<ThreadSafeRepository, GroveError> {
    ThreadSafeRepository::open(path).map_err(|_| GroveError::WorktreeNotFound(path.to_path_buf()))
}

fn head_branch(repo: &gix::Repository) -> Option<String> {
    repo.head_name()
        .ok()
        .flatten()
        .map(|r| r.shorten().to_string())
}

fn head_oid(repo: &gix::Repository) -> Option<gix::ObjectId> {
    repo.head_id().ok().map(|id| id.detach())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::git::WorktreeManager;

    /// Parse the output of `git worktree list --porcelain` into a sorted list of paths.
    fn git_worktree_paths(repo_dir: &std::path::Path) -> Vec<PathBuf> {
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

    /// Create a fresh git repo with one initial commit.
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
        // Create an initial commit so HEAD is valid
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

    fn add_worktree(repo_dir: &std::path::Path, wt_dir: &std::path::Path, branch: &str) {
        let status = Command::new("git")
            .args([
                "-C",
                repo_dir.to_str().unwrap(),
                "worktree",
                "add",
                "-b",
                branch,
                wt_dir.to_str().unwrap(),
                "HEAD",
            ])
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git worktree add failed for branch {branch}"
        );
    }

    #[test]
    fn trait_compiles_with_gix_backend() {
        let backend: &dyn WorktreeManager = &GixBackend;
        let _ = backend; // just verify trait object works
    }

    #[test]
    fn list_matches_git_porcelain() {
        let main_dir = init_repo();
        let wt1 = TempDir::new().unwrap();
        let wt2 = TempDir::new().unwrap();
        add_worktree(main_dir.path(), wt1.path(), "feature-a");
        add_worktree(main_dir.path(), wt2.path(), "feature-b");

        let mut expected = git_worktree_paths(main_dir.path());
        expected.sort();

        let backend = GixBackend;
        let mut got: Vec<PathBuf> = backend
            .list(main_dir.path())
            .expect("list should succeed")
            .into_iter()
            .map(|info| info.path)
            .collect();
        got.sort();

        assert_eq!(
            got, expected,
            "GixBackend::list must match git worktree list --porcelain"
        );
    }

    #[test]
    fn open_returns_branch_and_head() {
        let main_dir = init_repo();
        let wt = TempDir::new().unwrap();
        add_worktree(main_dir.path(), wt.path(), "my-branch");

        let backend = GixBackend;
        let handle = backend.open(wt.path()).expect("open should succeed");

        assert_eq!(handle.branch(), Some("my-branch"));
        assert!(handle.head().is_some(), "head oid should be present");
    }

    #[test]
    fn open_missing_path_returns_not_found() {
        let backend = GixBackend;
        let missing = PathBuf::from("/tmp/does-not-exist-grove-test-xyz");
        let err = backend.open(&missing).unwrap_err();
        assert!(
            matches!(err, GroveError::WorktreeNotFound(ref p) if p == &missing),
            "expected WorktreeNotFound, got {:?}",
            err
        );
    }
}
