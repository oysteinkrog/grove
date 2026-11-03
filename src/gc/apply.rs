//! Applying what `grove gc` found.
//!
//! Every removal re-runs the guards immediately before acting. A plan can be
//! minutes old by the time an operator answers the last prompt, and in that
//! window an agent can have started editing the very tree gc is about to
//! delete. There is no `--force`: a guard that trips at apply time cancels that
//! item and the run continues.

use std::path::Path;

use super::guards::{self, QUICK_GIT_TIMEOUT};
use super::{Finding, Layout, Remedy, exec, log_branch_deletion};
use crate::registry::Registry;

/// Timeout for the mutating git calls; `worktree remove` deletes files, which
/// is slow on drvfs.
const MUTATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Apply one finding, returning the line to print about what happened.
pub fn apply(finding: &Finding, layout: &Layout, grove_dir: &Path) -> Result<String, String> {
    match &finding.remedy {
        Remedy::DropRegistryEntry { tag } => drop_registry_entry(tag, grove_dir),
        Remedy::RemoveEphemeral { tag, branch, head } => remove_tree(
            finding,
            layout,
            grove_dir,
            tag.as_deref(),
            branch.as_deref(),
            head.as_deref(),
        ),
        Remedy::RemoveHarnessWorktree { branch, head } => remove_tree(
            finding,
            layout,
            grove_dir,
            None,
            branch.as_deref(),
            head.as_deref(),
        ),
        Remedy::PruneWorktreeMetadata => prune(layout),
        Remedy::AdoptOrDone { branch, head } => remove_tree(
            finding,
            layout,
            grove_dir,
            None,
            branch.as_deref(),
            head.as_deref(),
        ),
        Remedy::ReportOnly => Err("nothing to apply".to_string()),
    }
}

fn drop_registry_entry(tag: &str, grove_dir: &Path) -> Result<String, String> {
    // Under registry.lock on the file as it is now, so an entry another
    // grove command adds meanwhile is kept.
    // Only while its directory is still missing: a new lane registered under
    // the same tag since the scan has a directory and is kept.
    let mut directory_back = false;
    match Registry::remove_on_disk_if(grove_dir, tag, |p| {
        directory_back = p.path.exists();
        !directory_back
    }) {
        Ok(Some(_)) => Ok(format!("dropped registry entry '{tag}'")),
        Ok(None) if directory_back => Err(format!(
            "'{tag}' has a directory again since the scan; left in place"
        )),
        // Someone else (grove done, another gc) dropped it first.
        Ok(None) => Ok(format!("registry entry '{tag}' was already dropped")),
        Err(e) => Err(format!("could not drop '{tag}': {e}")),
    }
}

/// Remove a worktree directory, then its local branch, then its registry entry.
///
/// Order matters: the branch cannot be deleted while a worktree has it checked
/// out, and a registry entry pointing at a directory that failed to go away
/// would be worse than leaving both.
fn remove_tree(
    finding: &Finding,
    layout: &Layout,
    grove_dir: &Path,
    tag: Option<&str>,
    branch: Option<&str>,
    head: Option<&str>,
) -> Result<String, String> {
    let path = &finding.path;

    // One lane delete at a time machine-wide, and none while I/O pressure is
    // high: a big lane delete is as heavy as a big copy. Taken before the
    // guards run, so they see the tree as it is when the delete starts.
    // Checked first too, so gc does not wait for a lane that is no longer
    // eligible.
    still_eligible(finding, grove_dir, tag)?;
    let delete_lock =
        crate::machine_lock::acquire_paced("delete", &crate::machine_lock::tag_of(path))?;

    // The wait above can be long. Check the finding still holds before the
    // guards, and act on what is there now.
    let expected = still_eligible(finding, grove_dir, tag)?;

    let safety = guards::inspect_tree(path).map_err(|e| {
        format!(
            "could not re-check {} before removing it: {e}",
            path.display()
        )
    })?;
    let blockers = safety.blockers();
    if !blockers.is_empty() {
        return Err(format!(
            "{} changed since the scan and is no longer safe to remove: {}",
            path.display(),
            blockers.join("; ")
        ));
    }

    if let (Some(scanned), Some(now)) = (head, safety.head.as_deref())
        && scanned != now
    {
        return Err(format!(
            "{} moved from {scanned} to {now} since the scan; left in place",
            path.display()
        ));
    }

    if let (Some(scanned), Some(now)) = (branch, safety.branch.as_deref())
        && scanned != now
    {
        return Err(format!(
            "{} switched from branch {scanned} to {now} since the scan; left in place",
            path.display()
        ));
    }

    remove_worktree_dir(layout, path)?;

    let mut event = crate::events::Event::now("gc-remove", path);
    event.tag = tag.map(str::to_string);
    event.branch = branch.map(str::to_string);
    crate::events::record(&event);

    let mut message = format!("removed {}", path.display());

    // Deregister while the lock is still held, and only the entry that was
    // checked: a tag registered again for a new lane is kept.
    if let (Some(tag), Some(expected)) = (tag, &expected) {
        match Registry::remove_on_disk_if(grove_dir, tag, |p| p.same_lane(expected)) {
            Ok(Some(_)) => message.push_str(&format!("; deregistered '{tag}'")),
            Ok(None) => {}
            Err(e) => return Err(format!("could not write the registry: {e}")),
        }
    }

    if let Some(branch) = branch {
        message.push_str(&delete_checked_branch(
            layout,
            path,
            branch,
            safety.head.as_deref(),
        )?);
    }

    drop(delete_lock);
    Ok(message)
}

/// Delete `branch`, but only when its tip is still `checked`, the commit the
/// guards found safe. A branch that moved, or a tree whose HEAD was unknown,
/// keeps its branch. Returns the text to add to the gc message.
fn delete_checked_branch(
    layout: &Layout,
    path: &Path,
    branch: &str,
    checked: Option<&str>,
) -> Result<String, String> {
    let Some(checked) = checked else {
        return Ok(format!(
            "; branch {branch} was left in place (its commit could not be checked)"
        ));
    };
    let refname = format!("refs/heads/{branch}");
    // `update-ref -d` does not refuse a branch another worktree has checked
    // out, as `branch -D` does, so check that first.
    let checked_out_elsewhere = exec::git(
        &layout.main_repo,
        &["worktree", "list", "--porcelain"],
        QUICK_GIT_TIMEOUT,
    )
    .map(|list| {
        list.lines()
            .any(|l| l.strip_prefix("branch ") == Some(refname.as_str()))
    })
    .unwrap_or(true);
    if checked_out_elsewhere {
        return Ok(format!(
            "; branch {branch} was left in place (checked out in another worktree)"
        ));
    }
    // Delete only if the branch is still at the checked commit, in one ref
    // transaction: a branch moved by any git command in between is kept.
    match exec::git(
        &layout.main_repo,
        // --no-deref: a branch turned into a symbolic ref meanwhile must not
        // take the ref it points to with it.
        &["update-ref", "--no-deref", "-d", &refname, checked],
        QUICK_GIT_TIMEOUT,
    ) {
        Ok(_) => {
            // The branch section (upstream and the like) goes with it, as
            // `branch -D` would do. Nothing to do when there is none.
            let _ = exec::git(
                &layout.main_repo,
                &["config", "--remove-section", &format!("branch.{branch}")],
                QUICK_GIT_TIMEOUT,
            );
            log_branch_deletion(layout, path, branch, checked)?;
            Ok(format!(" and deleted branch {branch} ({checked})"))
        }
        Err(_) => Ok(format!(
            "; branch {branch} was left in place (its tip is not the checked commit {checked})"
        )),
    }
}

/// Fail when the registry changed since the scan in a way that makes the
/// finding wrong: a registered lane gone, moved, frozen, or given a later
/// expiry; an unregistered tree registered (adopted) meanwhile.
fn still_eligible(
    finding: &Finding,
    grove_dir: &Path,
    tag: Option<&str>,
) -> Result<Option<crate::registry::Project>, String> {
    let path = &finding.path;
    let registry =
        Registry::load(grove_dir).map_err(|e| format!("could not read the registry: {e}"))?;
    let changed = |why: &str| {
        Err(format!(
            "{} {why} since the scan; left in place",
            path.display()
        ))
    };
    match tag {
        Some(tag) => {
            let Some(project) = registry.projects.get(tag) else {
                return changed(&format!("is no longer registered as '{tag}'"));
            };
            if &project.path != path {
                return changed(&format!("is no longer the path of '{tag}'"));
            }
            if project.frozen {
                return changed("was frozen");
            }
            if finding.category == super::Category::ExpiredEphemeral
                && !project.is_expired(time::OffsetDateTime::now_utc())
            {
                return changed("had its expiry extended");
            }
            Ok(Some(project.clone()))
        }
        None => {
            if registry.projects.values().any(|p| &p.path == path) {
                return changed("was registered");
            }
            Ok(None)
        }
    }
}

/// Ask git to remove the worktree. Directories that are not linked worktrees
/// (a raw clone dropped into `.scratch`) are removed directly, but only after
/// the caller's guards have already found them clean.
fn remove_worktree_dir(layout: &Layout, path: &Path) -> Result<(), String> {
    let is_linked_worktree = path.join(".git").is_file();

    if is_linked_worktree {
        return exec::git(
            &layout.main_repo,
            &["worktree", "remove", path.to_str().unwrap_or_default()],
            MUTATE_TIMEOUT,
        )
        .map(|_| ())
        .map_err(|e| format!("git refused to remove {}: {e}", path.display()));
    }

    std::fs::remove_dir_all(path).map_err(|e| format!("could not remove {}: {e}", path.display()))
}

fn prune(layout: &Layout) -> Result<String, String> {
    exec::git_streams(
        &layout.main_repo,
        &["worktree", "prune", "--verbose"],
        MUTATE_TIMEOUT,
    )
    .map(|out| {
        let count = out.lines().filter(|l| !l.trim().is_empty()).count();
        format!(
            "pruned {count} stale worktree metadata entr{}",
            if count == 1 { "y" } else { "ies" }
        )
    })
    .map_err(|e| format!("git worktree prune failed: {e}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use time::OffsetDateTime;

    use super::super::{Category, Finding};
    use super::*;
    use crate::registry::Project;

    fn project(path: &Path) -> Project {
        Project {
            path: path.to_path_buf(),
            branch: "feature".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::now_utc(),
            issue: None,
            frozen: false,
            expires_at: None,
            owner: None,
        }
    }

    #[test]
    fn dropping_a_registry_entry_leaves_the_others() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");
        let mut projects = BTreeMap::new();
        projects.insert("gone".to_string(), project(Path::new("/nowhere/gone")));
        projects.insert("kept".to_string(), project(Path::new("/nowhere/kept")));
        Registry {
            schema_version: 1,
            projects,
        }
        .save(&grove_dir)
        .unwrap();

        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::StaleRegistryEntry, "gone", "/nowhere/gone").remedy(
            Remedy::DropRegistryEntry {
                tag: "gone".to_string(),
            },
        );

        let message = apply(&finding, &layout, &grove_dir).unwrap();
        assert!(message.contains("gone"), "{message}");

        let loaded = Registry::load(&grove_dir).unwrap();
        assert!(!loaded.projects.contains_key("gone"));
        assert!(loaded.projects.contains_key("kept"));
    }

    #[test]
    fn dropping_an_already_dropped_tag_is_not_a_failure() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");
        Registry::default().save(&grove_dir).unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::StaleRegistryEntry, "ghost", "/nowhere").remedy(
            Remedy::DropRegistryEntry {
                tag: "ghost".to_string(),
            },
        );
        // Someone else dropped it first: nothing left to do, not a failure.
        let msg = apply(&finding, &layout, &grove_dir).unwrap();
        assert!(msg.contains("already dropped"), "{msg}");
    }

    #[test]
    fn report_only_findings_cannot_be_applied() {
        let dir = TempDir::new().unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::ForeignDirectory, "sc-support", dir.path());
        let err = apply(&finding, &layout, &dir.path().join(".grove")).unwrap_err();
        assert_eq!(err, "nothing to apply");
    }

    #[test]
    fn removal_refuses_a_tree_that_turned_dirty_after_the_scan() {
        // The plan says clean; by apply time somebody has started working.
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("wt-a");
        std::fs::create_dir_all(&tree).unwrap();
        for args in [
            vec!["init", "-b", "main", tree.to_str().unwrap()],
            vec![
                "-C",
                tree.to_str().unwrap(),
                "config",
                "user.email",
                "t@t.com",
            ],
            vec!["-C", tree.to_str().unwrap(), "config", "user.name", "T"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(tree.join("wip.txt"), b"in progress").unwrap();

        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::ExpiredEphemeral, "wt-a", &tree).remedy(
            Remedy::RemoveEphemeral {
                tag: None,
                branch: None,
                head: None,
            },
        );

        let err = apply(&finding, &layout, &dir.path().join(".grove")).unwrap_err();
        assert!(err.contains("no longer safe to remove"), "{err}");
        assert!(
            tree.exists(),
            "a tree that failed its re-check must survive"
        );
    }

    fn ephemeral_finding(tag: Option<&str>, path: &Path) -> Finding {
        Finding::new(Category::ExpiredEphemeral, "lane", path).remedy(Remedy::RemoveEphemeral {
            tag: tag.map(str::to_string),
            branch: None,
            head: None,
        })
    }

    #[test]
    fn removal_skips_a_lane_whose_expiry_was_extended_after_the_scan() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("lane");
        std::fs::create_dir_all(&tree).unwrap();
        let grove_dir = dir.path().join(".grove");
        let mut extended = project(&tree);
        extended.expires_at = Some(OffsetDateTime::now_utc() + time::Duration::days(3));
        Registry::insert_on_disk(&grove_dir, "lane".to_string(), extended).unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };

        let err = apply(&ephemeral_finding(Some("lane"), &tree), &layout, &grove_dir).unwrap_err();
        assert!(err.contains("expiry extended"), "{err}");
        assert!(tree.exists());
    }

    #[test]
    fn removal_skips_a_lane_frozen_or_deregistered_after_the_scan() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("lane");
        std::fs::create_dir_all(&tree).unwrap();
        let grove_dir = dir.path().join(".grove");
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };

        // Not registered any more.
        Registry::default().save(&grove_dir).unwrap();
        let err = apply(&ephemeral_finding(Some("lane"), &tree), &layout, &grove_dir).unwrap_err();
        assert!(err.contains("no longer registered"), "{err}");

        // Registered again, but frozen.
        let mut frozen = project(&tree);
        frozen.frozen = true;
        frozen.expires_at = Some(OffsetDateTime::now_utc() - time::Duration::days(1));
        Registry::insert_on_disk(&grove_dir, "lane".to_string(), frozen).unwrap();
        let err = apply(&ephemeral_finding(Some("lane"), &tree), &layout, &grove_dir).unwrap_err();
        assert!(err.contains("frozen"), "{err}");
        assert!(tree.exists());
    }

    #[test]
    fn removal_skips_an_unregistered_tree_adopted_after_the_scan() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join(".scratch").join("raw");
        std::fs::create_dir_all(&tree).unwrap();
        let grove_dir = dir.path().join(".grove");
        Registry::insert_on_disk(&grove_dir, "adopted".to_string(), project(&tree)).unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };

        let err = apply(&ephemeral_finding(None, &tree), &layout, &grove_dir).unwrap_err();
        assert!(err.contains("was registered"), "{err}");
        assert!(tree.exists());
    }

    #[test]
    fn a_stale_entry_whose_directory_came_back_is_kept() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");
        let lane = dir.path().join("reused");
        std::fs::create_dir_all(&lane).unwrap();
        Registry::insert_on_disk(&grove_dir, "reused".to_string(), project(&lane)).unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::StaleRegistryEntry, "reused", &lane).remedy(
            Remedy::DropRegistryEntry {
                tag: "reused".to_string(),
            },
        );
        let err = apply(&finding, &layout, &grove_dir).unwrap_err();
        assert!(err.contains("left in place"), "{err}");
        assert!(
            Registry::load(&grove_dir)
                .unwrap()
                .projects
                .contains_key("reused")
        );
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_branch_is_deleted_only_at_the_checked_commit() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("master");
        std::fs::create_dir_all(&main).unwrap();
        git_in(&main, &["init", "-q", "-b", "main"]);
        git_in(&main, &["config", "user.email", "t@t.com"]);
        git_in(&main, &["config", "user.name", "T"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let checked = git_in(&main, &["rev-parse", "HEAD"]);
        git_in(&main, &["branch", "moved"]);
        git_in(&main, &["branch", "kept"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "two"]);
        // `moved` now points past the checked commit.
        git_in(&main, &["branch", "-f", "moved", "HEAD"]);
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: main.clone(),
        };
        let lane = dir.path().join("lane");

        let msg = delete_checked_branch(&layout, &lane, "moved", Some(&checked)).unwrap();
        assert!(msg.contains("left in place"), "{msg}");
        git_in(&main, &["rev-parse", "--verify", "refs/heads/moved"]);

        let msg = delete_checked_branch(&layout, &lane, "kept", None).unwrap();
        assert!(msg.contains("left in place"), "{msg}");

        git_in(&main, &["config", "branch.kept.remote", "origin"]);
        let msg = delete_checked_branch(&layout, &lane, "kept", Some(&checked)).unwrap();
        assert!(msg.contains("deleted branch kept"), "{msg}");
        assert_eq!(git_in(&main, &["branch", "--list", "kept"]), "");
        assert!(
            !std::process::Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(["config", "--get", "branch.kept.remote"])
                .status()
                .unwrap()
                .success(),
            "the branch's config section goes with it"
        );

        // A branch that became a symbolic ref is deleted alone, never the
        // branch it points to.
        git_in(&main, &["branch", "-f", "target", &checked]);
        git_in(
            &main,
            &["symbolic-ref", "refs/heads/alias", "refs/heads/target"],
        );
        let msg = delete_checked_branch(&layout, &lane, "alias", Some(&checked)).unwrap();
        assert!(msg.contains("deleted branch alias"), "{msg}");
        assert_eq!(
            git_in(&main, &["rev-parse", "--verify", "refs/heads/target"]),
            checked
        );

        // A branch checked out in another worktree is kept even at the
        // checked commit.
        git_in(&main, &["branch", "busy", &checked]);
        let other = dir.path().join("other");
        git_in(
            &main,
            &["worktree", "add", "-q", other.to_str().unwrap(), "busy"],
        );
        let msg = delete_checked_branch(&layout, &lane, "busy", Some(&checked)).unwrap();
        assert!(msg.contains("checked out in another worktree"), "{msg}");
        git_in(&main, &["rev-parse", "--verify", "refs/heads/busy"]);
    }

    #[test]
    fn removal_of_an_unreadable_tree_is_an_error_not_a_deletion() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("not-there");
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        let finding = Finding::new(Category::ExpiredEphemeral, "not-there", &missing).remedy(
            Remedy::RemoveEphemeral {
                tag: None,
                branch: None,
                head: None,
            },
        );
        let err = apply(&finding, &layout, &dir.path().join(".grove")).unwrap_err();
        assert!(err.contains("could not re-check"), "{err}");
    }

    #[test]
    fn a_plain_directory_removal_takes_the_filesystem_path() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("clone");
        std::fs::create_dir_all(tree.join(".git")).unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: dir.path().join("master"),
        };
        remove_worktree_dir(&layout, &tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn path_kinds_route_to_different_removal_mechanics() {
        let dir = TempDir::new().unwrap();
        let linked = dir.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), b"gitdir: /somewhere/else\n").unwrap();
        let layout = Layout {
            work_dir: dir.path().to_path_buf(),
            main_repo: PathBuf::from("/tmp/grove-gc-no-such-repo-xyz"),
        };
        // A linked worktree must go through git, so a bogus main repo fails
        // loudly instead of silently deleting the directory.
        let err = remove_worktree_dir(&layout, &linked).unwrap_err();
        assert!(err.contains("git refused"), "{err}");
        assert!(linked.exists());
    }
}
