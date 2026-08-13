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
    let mut registry =
        Registry::load(grove_dir).map_err(|e| format!("could not read the registry: {e}"))?;
    registry
        .remove(tag)
        .map_err(|e| format!("could not drop '{tag}': {e}"))?;
    registry
        .save(grove_dir)
        .map_err(|e| format!("could not write the registry: {e}"))?;
    Ok(format!("dropped registry entry '{tag}'"))
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

    remove_worktree_dir(layout, path)?;

    let mut message = format!("removed {}", path.display());

    if let Some(branch) = branch {
        let sha = head.map(str::to_string).unwrap_or_else(|| {
            safety
                .head
                .clone()
                .unwrap_or_else(|| "unknown-sha".to_string())
        });
        match exec::git(
            &layout.main_repo,
            &["branch", "-D", branch],
            QUICK_GIT_TIMEOUT,
        ) {
            Ok(_) => {
                log_branch_deletion(layout, path, branch, &sha)?;
                message.push_str(&format!(" and deleted branch {branch} ({sha})"));
            }
            Err(e) => {
                message.push_str(&format!("; branch {branch} was left in place ({e})"));
            }
        }
    }

    if let Some(tag) = tag {
        let mut registry =
            Registry::load(grove_dir).map_err(|e| format!("could not read the registry: {e}"))?;
        if registry.remove(tag).is_ok() {
            registry
                .save(grove_dir)
                .map_err(|e| format!("could not write the registry: {e}"))?;
            message.push_str(&format!("; deregistered '{tag}'"));
        }
    }

    Ok(message)
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
    fn dropping_an_unknown_tag_reports_an_error() {
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
        assert!(apply(&finding, &layout, &grove_dir).is_err());
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
