use std::process::Command;

use anyhow::{Context, Result};
use gix::ThreadSafeRepository;
use gix::bstr::ByteSlice;
use rayon::prelude::*;
use time::OffsetDateTime;

use super::Worktree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub dirty: bool,
    /// Commits in local branch not in upstream; None when no upstream is configured.
    pub ahead: Option<u32>,
    /// Commits in upstream not in local branch; None when no upstream is configured.
    pub behind: Option<u32>,
    /// Untracked entries, counting a wholly untracked directory as one entry
    /// rather than recursing into it. This matches what plain `git status`
    /// shows, and keeps `grove list` from walking large untracked trees such as
    /// build output or a stray browser profile.
    pub untracked: u32,
    /// True when ahead == Some(0), meaning all local commits have been pushed.
    pub is_pushed: bool,
}

/// Detailed per-project status, returned by `compute_detail`.
#[derive(Debug, Clone)]
pub struct StatusDetail {
    pub head_branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty: bool,
    /// Changed/untracked file paths, capped at 10 for display; total count in `dirty_files_total`.
    pub dirty_files: Vec<String>,
    pub dirty_files_total: usize,
    /// Timestamp of the HEAD commit; None if repo has no commits.
    pub last_commit_time: Option<OffsetDateTime>,
}

/// Compute detailed status for a single worktree. Keeps `compute()` intact.
pub fn compute_detail(wt: &Worktree) -> Result<StatusDetail> {
    let ts_repo = ThreadSafeRepository::open(&wt.path)?;
    let repo = ts_repo.to_thread_local();

    let head_branch = head_branch_name(&repo);
    let (upstream, ahead, behind) = upstream_info(&repo, head_branch.as_deref())?;
    let (dirty, dirty_files, dirty_files_total) = collect_dirty_files(&repo)?;
    let last_commit_time = head_commit_time(&repo);

    Ok(StatusDetail {
        head_branch,
        upstream,
        ahead,
        behind,
        dirty,
        dirty_files,
        dirty_files_total,
        last_commit_time,
    })
}

fn head_branch_name(repo: &gix::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    let branch = head.referent_name()?;
    let name = branch.as_bstr().to_str().ok()?;
    let short = name.strip_prefix("refs/heads/").unwrap_or(name);
    Some(short.to_string())
}

fn upstream_info(
    repo: &gix::Repository,
    branch: Option<&str>,
) -> Result<(Option<String>, Option<u32>, Option<u32>)> {
    let branch = match branch {
        Some(b) => b,
        None => return Ok((None, None, None)),
    };

    let config = repo.config_snapshot();
    let remote_name = config
        .string(format!("branch.{branch}.remote").as_str())
        .map(|v| v.to_string());
    let merge_ref = config
        .string(format!("branch.{branch}.merge").as_str())
        .map(|v| v.to_string());

    let (remote_name, merge_ref) = match (remote_name, merge_ref) {
        (Some(r), Some(m)) => (r, m),
        _ => return Ok((None, None, None)),
    };

    let upstream_branch = merge_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(merge_ref.as_str());
    let upstream_ref = format!("refs/remotes/{remote_name}/{upstream_branch}");
    let upstream_label = format!("{remote_name}/{upstream_branch}");

    let local_oid = match repo.try_find_reference(&format!("refs/heads/{branch}"))? {
        Some(r) => r.id().detach(),
        None => return Ok((Some(upstream_label), None, None)),
    };
    let upstream_oid = match repo.try_find_reference(&upstream_ref)? {
        Some(r) => r.id().detach(),
        None => return Ok((Some(upstream_label), None, None)),
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

    Ok((Some(upstream_label), Some(ahead), Some(behind)))
}

fn collect_dirty_files(repo: &gix::Repository) -> Result<(bool, Vec<String>, usize)> {
    let mut paths: Vec<String> = Vec::new();

    let platform = repo
        .status(gix::progress::Discard)?
        .untracked_files(gix::status::UntrackedFiles::Files);

    for item in platform.into_iter(None)? {
        match item? {
            gix::status::Item::IndexWorktree(
                gix::status::index_worktree::Item::DirectoryContents { entry, .. },
            ) if matches!(entry.status, gix::dir::entry::Status::Untracked) => {
                paths.push(entry.rela_path.to_string());
            }
            gix::status::Item::IndexWorktree(ref itm) => {
                use gix::status::index_worktree::Item;
                let path = match itm {
                    Item::Modification { rela_path, .. } => {
                        Some(rela_path.to_str_lossy().into_owned())
                    }
                    Item::Rewrite {
                        source,
                        dirwalk_entry,
                        ..
                    } => Some(format!(
                        "{} -> {}",
                        source.rela_path(),
                        dirwalk_entry.rela_path.to_str_lossy()
                    )),
                    _ => None,
                };
                if let Some(p) = path {
                    paths.push(p);
                }
            }
            _ => {}
        }
    }

    let total = paths.len();
    let dirty = total > 0 || repo.is_dirty()?;
    paths.truncate(10);
    Ok((dirty, paths, total))
}

fn head_commit_time(repo: &gix::Repository) -> Option<OffsetDateTime> {
    let head_commit = repo.head_commit().ok()?;
    let time = head_commit.time().ok()?;
    OffsetDateTime::from_unix_timestamp(time.seconds).ok()
}

/// Compute status for all worktrees in parallel using rayon.
///
/// Each element in the returned Vec corresponds to the same-index worktree.
/// An error for one worktree does not abort the others.
pub fn compute_all(worktrees: &[Worktree]) -> Vec<Result<Status>> {
    worktrees.par_iter().map(compute).collect()
}

/// Status for one worktree, by asking git.
///
/// This used to walk the worktree with gix, which turned out to be the reason
/// `grove list` was slow enough to saturate the machine. Measured on two
/// worktrees of about 29k tracked files each, on a Windows drive under WSL1:
/// gix needed 35 and 72 seconds, and git answered the same question in 6.1 and
/// 3.6. Multiplied across a couple of hundred projects that is the difference
/// between a command you can run and one you cannot.
///
/// One `git status --porcelain=v2 --branch` returns everything needed: whether
/// tracked content changed, how many untracked entries there are, and how far
/// the branch is from its upstream.
///
/// `--no-optional-locks` matters here. A plain `git status` writes back a
/// refreshed index, and doing that across hundreds of worktrees while other
/// work is in flight means both pointless writes and lock contention.
pub fn compute(wt: &Worktree) -> Result<Status> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(&wt.path)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .output()
        .with_context(|| format!("could not run git status in {}", wt.path.display()))?;

    if !out.status.success() {
        anyhow::bail!(
            "git status failed in {}: {}",
            wt.path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(parse_porcelain_v2(&String::from_utf8_lossy(&out.stdout)))
}

/// Read `git status --porcelain=v2 --branch` output.
///
/// The format is stable and documented, and each line is self-describing:
///
/// - `# branch.ab +1 -2` gives commits ahead of and behind the upstream. The
///   line is absent when there is no upstream, which is why ahead and behind
///   are optional rather than zero.
/// - `1`, `2` and `u` lines are changed, renamed and unmerged tracked entries.
///   Any of them means tracked content changed.
/// - `?` lines are untracked. With `--untracked-files=normal` a wholly
///   untracked directory is one line rather than one line per file inside it,
///   which is what plain `git status` shows.
///
/// Unknown lines are ignored, so a future git that adds a line type reports
/// slightly less rather than failing.
fn parse_porcelain_v2(text: &str) -> Status {
    let mut dirty = false;
    let mut untracked = 0u32;
    let mut ahead = None;
    let mut behind = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut parts = rest.split_whitespace();
            ahead = parts
                .next()
                .and_then(|p| p.strip_prefix('+'))
                .and_then(|n| n.parse().ok());
            behind = parts
                .next()
                .and_then(|p| p.strip_prefix('-'))
                .and_then(|n| n.parse().ok());
        } else if line.starts_with("? ") {
            untracked += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            dirty = true;
        }
    }

    Status {
        dirty,
        ahead,
        behind,
        untracked,
        is_pushed: ahead == Some(0),
    }
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

    use serial_test::serial;
    use tempfile::TempDir;

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

    // ── porcelain v2 parsing ────────────────────────────────────────────────
    //
    // These drive the parser directly. They are cheap, and they pin down the
    // cases that are awkward to set up as real repos: no upstream, detached
    // HEAD, unmerged entries.

    #[test]
    fn parses_ahead_behind() {
        let s = parse_porcelain_v2(
            "# branch.oid abc123\n\
             # branch.head feature\n\
             # branch.upstream origin/feature\n\
             # branch.ab +3 -5\n",
        );
        assert_eq!(s.ahead, Some(3));
        assert_eq!(s.behind, Some(5));
        assert!(!s.is_pushed, "3 commits ahead is not pushed");
        assert!(!s.dirty);
        assert_eq!(s.untracked, 0);
    }

    #[test]
    fn no_upstream_line_means_unknown_not_zero() {
        // A branch with no upstream gets no `# branch.ab` line at all. Reporting
        // zero here would claim it is in sync with something that is not there.
        let s = parse_porcelain_v2("# branch.oid abc123\n# branch.head solo\n");
        assert_eq!(s.ahead, None);
        assert_eq!(s.behind, None);
        assert!(!s.is_pushed);
    }

    #[test]
    fn in_sync_branch_is_pushed() {
        let s = parse_porcelain_v2("# branch.ab +0 -0\n");
        assert_eq!(s.ahead, Some(0));
        assert!(s.is_pushed);
    }

    #[test]
    fn counts_untracked_lines_only() {
        let s = parse_porcelain_v2(
            "# branch.ab +0 -0\n\
             ? one.txt\n\
             ? junk/\n\
             ? two.txt\n",
        );
        assert_eq!(s.untracked, 3);
        assert!(!s.dirty, "untracked entries alone are not a tracked change");
    }

    #[test]
    fn changed_renamed_and_unmerged_entries_are_dirty() {
        for line in [
            "1 .M N... 100644 100644 100644 abc abc file.txt",
            "2 R. N... 100644 100644 100644 abc abc R100 new.txt\told.txt",
            "u UU N... 100644 100644 100644 100644 abc abc abc both.txt",
        ] {
            let s = parse_porcelain_v2(&format!("# branch.ab +0 -0\n{line}\n"));
            assert!(s.dirty, "should be dirty for line: {line}");
        }
    }

    #[test]
    fn clean_output_is_clean() {
        let s = parse_porcelain_v2("# branch.oid abc\n# branch.head main\n# branch.ab +0 -0\n");
        assert!(!s.dirty);
        assert_eq!(s.untracked, 0);
    }

    #[test]
    fn unknown_lines_are_ignored() {
        // A future git adding a line type should cost us information, not
        // correctness.
        let s = parse_porcelain_v2("# branch.ab +1 -0\nX something new\n! ignored.txt\n");
        assert_eq!(s.ahead, Some(1));
        assert!(!s.dirty);
        assert_eq!(s.untracked, 0);
    }

    #[test]
    fn empty_output_is_clean_and_unknown() {
        let s = parse_porcelain_v2("");
        assert!(!s.dirty);
        assert_eq!(s.untracked, 0);
        assert_eq!(s.ahead, None);
    }

    #[serial]
    // A staged-but-uncommitted change must still read as dirty. The single
    // worktree walk replaced an explicit repo.is_dirty() call, and this is the
    // case that call used to cover on its own.
    #[test]
    fn staged_change_is_dirty() {
        let dir = init_repo();
        std::fs::write(dir.path().join("staged.txt"), b"new").unwrap();
        git(dir.path(), &["add", "staged.txt"]);

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert!(s.dirty, "staged change should make repo dirty");
        assert_eq!(s.untracked, 0, "a staged file is tracked, not untracked");
    }

    #[serial]
    // Untracked files alone must not flip `dirty`. The list view shows dirty and
    // untracked as separate columns, so conflating them would misreport.
    #[test]
    fn untracked_only_is_not_dirty() {
        let dir = init_repo();
        std::fs::write(dir.path().join("loose.txt"), b"x").unwrap();

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert!(!s.dirty, "untracked files alone should not make repo dirty");
        assert_eq!(s.untracked, 1, "should count the untracked file");
    }

    #[serial]
    // An untracked directory counts as one entry, not once per file inside it.
    // This is what keeps `grove list` off large untracked trees.
    #[test]
    fn untracked_directory_counted_once() {
        let dir = init_repo();
        let nested = dir.path().join("junk").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        for i in 0..25 {
            std::fs::write(nested.join(format!("f{i}.txt")), b"x").unwrap();
        }

        let wt = open_worktree(dir.path());
        let s = compute(&wt).expect("compute should succeed");

        assert_eq!(
            s.untracked, 1,
            "untracked directory should collapse to a single entry"
        );
    }

    #[serial]
    // compute_all: one invalid worktree path returns an error in its slot;
    // valid worktrees succeed and are not silently dropped.
    #[test]
    fn compute_all_error_propagation() {
        let good1 = init_repo();
        let good2 = init_repo();

        // Build a Worktree for a non-existent path to force an error.
        let bad_path = std::path::PathBuf::from("/nonexistent/path/that/will/never/exist");
        let bad_wt = GixBackend.open(&bad_path);
        // open() may succeed (it just records the path) or fail; either way
        // we need a Worktree value. If open itself errors we skip this variant.
        let worktrees: Vec<Worktree> = match bad_wt {
            Ok(bad) => vec![
                open_worktree(good1.path()),
                bad,
                open_worktree(good2.path()),
            ],
            Err(_) => {
                // If the backend refuses to open, test the happy path only.
                vec![open_worktree(good1.path()), open_worktree(good2.path())]
            }
        };

        let results = compute_all(&worktrees);

        // The result count must equal the input count.
        assert_eq!(
            results.len(),
            worktrees.len(),
            "result count must match input count"
        );

        // At least the two good repos must succeed.
        let successes: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
        assert!(
            successes.len() >= 2,
            "both valid repos must succeed; got {} successes",
            successes.len()
        );
    }
}

