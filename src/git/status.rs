use anyhow::Result;
use gix::ThreadSafeRepository;

use super::Worktree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub dirty: bool,
    /// Commits in local branch not in upstream; None when no upstream is configured.
    pub ahead: Option<u32>,
    /// Commits in upstream not in local branch; None when no upstream is configured.
    pub behind: Option<u32>,
    pub untracked: u32,
}

pub fn compute(wt: &Worktree) -> Result<Status> {
    let ts_repo = ThreadSafeRepository::open(&wt.path)?;
    let repo = ts_repo.to_thread_local();

    let dirty = repo.is_dirty()?;
    let untracked = count_untracked(&repo)?;
    let (ahead, behind) = compute_ahead_behind(&repo, wt.branch())?;

    Ok(Status {
        dirty,
        ahead,
        behind,
        untracked,
    })
}

fn count_untracked(repo: &gix::Repository) -> Result<u32> {
    let platform = repo
        .status(gix::progress::Discard)?
        .untracked_files(gix::status::UntrackedFiles::Files);
    let mut count = 0u32;
    for item in platform.into_iter(None)? {
        if let gix::status::Item::IndexWorktree(
            gix::status::index_worktree::Item::DirectoryContents { entry, .. },
        ) = item?
            && matches!(entry.status, gix::dir::entry::Status::Untracked)
        {
            count += 1;
        }
    }
    Ok(count)
}

fn compute_ahead_behind(
    repo: &gix::Repository,
    branch: Option<&str>,
) -> Result<(Option<u32>, Option<u32>)> {
    let branch = match branch {
        Some(b) => b,
        None => return Ok((None, None)),
    };

    // Look up the upstream tracking ref from config: branch.<name>.remote + branch.<name>.merge
    let config = repo.config_snapshot();
    let remote_name = config
        .string(format!("branch.{branch}.remote").as_str())
        .map(|v| v.to_string());
    let merge_ref = config
        .string(format!("branch.{branch}.merge").as_str())
        .map(|v| v.to_string());

    let (remote_name, merge_ref) = match (remote_name, merge_ref) {
        (Some(r), Some(m)) => (r, m),
        _ => return Ok((None, None)),
    };

    // merge_ref is like refs/heads/<branch>; convert to refs/remotes/<remote>/<branch>
    let upstream_branch = merge_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(merge_ref.as_str());
    let upstream_ref = format!("refs/remotes/{remote_name}/{upstream_branch}");

    let local_oid = match repo.try_find_reference(&format!("refs/heads/{branch}"))? {
        Some(r) => r.id().detach(),
        None => return Ok((None, None)),
    };
    let upstream_oid = match repo.try_find_reference(&upstream_ref)? {
        Some(r) => r.id().detach(),
        None => return Ok((None, None)),
    };

    let ahead = repo
        .rev_walk([local_oid])
        .with_boundary([upstream_oid])
        .all()?
        .count() as u32;
    let behind = repo
        .rev_walk([upstream_oid])
        .with_boundary([local_oid])
        .all()?
        .count() as u32;

    Ok((Some(ahead), Some(behind)))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use serial_test::serial;

    use super::*;
    use crate::git::{GixBackend, WorktreeManager};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .status()
            .expect("git must be on PATH");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        git(p, &["init"]);
        git(p, &["config", "user.email", "test@test.com"]);
        git(p, &["config", "user.name", "Test"]);
        std::fs::write(p.join("README.md"), b"hello").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-m", "init"]);
        dir
    }

    fn open_worktree(path: &std::path::Path) -> Worktree {
        GixBackend.open(path).expect("open should succeed")
    }

    #[serial]
    // AC1: clean worktree → dirty=false, ahead/behind=Some(0), untracked=0
    #[test]
    fn clean_repo_with_upstream_is_all_zero() {
        let origin = init_repo();
        let local = TempDir::new().unwrap();
        // Clone so upstream tracking is set up automatically
        Command::new("git")
            .args([
                "clone",
                origin.path().to_str().unwrap(),
                local.path().to_str().unwrap(),
            ])
            .status()
            .unwrap();
        git(local.path(), &["config", "user.email", "test@test.com"]);
        git(local.path(), &["config", "user.name", "Test"]);

        let wt = open_worktree(local.path());
        let s = compute(&wt).expect("compute should succeed");

        assert!(!s.dirty, "clean repo should not be dirty");
        assert_eq!(s.ahead, Some(0), "no local commits ahead");
        assert_eq!(s.behind, Some(0), "no upstream commits behind");
        assert_eq!(s.untracked, 0, "no untracked files");
    }

    #[serial]
    // AC2: modified tracked file → dirty=true
    #[test]
    fn modified_tracked_file_is_dirty() {
        let dir = init_repo();
        std::fs::write(dir.path().join("README.md"), b"modified").unwrap();

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert!(s.dirty, "modified file should make repo dirty");
    }

    #[serial]
    // AC3: 2 local commits ahead → ahead=Some(2), behind=Some(0)
    #[test]
    fn two_commits_ahead_of_upstream() {
        let origin = init_repo();
        let local = TempDir::new().unwrap();
        Command::new("git")
            .args([
                "clone",
                origin.path().to_str().unwrap(),
                local.path().to_str().unwrap(),
            ])
            .status()
            .unwrap();
        git(local.path(), &["config", "user.email", "test@test.com"]);
        git(local.path(), &["config", "user.name", "Test"]);

        // Make 2 commits in local
        for i in 1..=2u8 {
            std::fs::write(local.path().join(format!("file{i}.txt")), [i]).unwrap();
            git(local.path(), &["add", "."]);
            git(local.path(), &["commit", "-m", &format!("commit {i}")]);
        }

        let wt = open_worktree(local.path());
        let s = compute(&wt).expect("compute should succeed");

        assert_eq!(s.ahead, Some(2), "should be 2 commits ahead");
        assert_eq!(s.behind, Some(0), "should not be behind");
    }

    #[serial]
    // AC4: no upstream → ahead=None, behind=None
    #[test]
    fn no_upstream_gives_none() {
        let dir = init_repo();

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert_eq!(s.ahead, None, "no upstream → ahead should be None");
        assert_eq!(s.behind, None, "no upstream → behind should be None");
    }

    #[serial]
    // AC5: N untracked files → untracked=N
    #[test]
    fn untracked_files_counted() {
        let dir = init_repo();
        std::fs::write(dir.path().join("untracked1.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("untracked2.txt"), b"b").unwrap();

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert_eq!(s.untracked, 2, "should count 2 untracked files");
    }
}
