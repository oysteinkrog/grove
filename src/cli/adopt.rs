use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::error::{GroveError, Result};
use crate::registry::Project;
use crate::repo::RepoContext;

pub struct AdoptArgs {
    pub tag: String,
    pub path: PathBuf,
    pub issue: Option<u32>,
    pub base: Option<String>,
    pub mv: bool,
}

pub fn run(args: &AdoptArgs, cx: &RepoContext) -> Result<()> {
    let path = args
        .path
        .canonicalize()
        .map_err(|_| GroveError::WorktreeInvalid {
            path: args.path.clone(),
        })?;

    if !is_worktree(&path) {
        return Err(GroveError::WorktreeInvalid { path });
    }

    if cx.registry.projects.contains_key(&args.tag) {
        let existing_path = cx.registry.projects[&args.tag].path.clone();
        return Err(GroveError::DuplicateTag {
            tag: args.tag.clone(),
            existing_path,
        });
    }

    let final_path = if args.mv {
        let dest = cx.resolved.work_dir.join(&args.tag);
        move_dir(&path, &dest)?;
        dest
    } else {
        path
    };

    let branch = read_head_branch(&final_path).unwrap_or_else(|| args.tag.clone());
    let base = args.base.clone().unwrap_or_else(|| {
        format!(
            "{}/{}",
            cx.resolved.upstream_remote, cx.resolved.default_base
        )
    });

    let project = Project {
        path: final_path.clone(),
        branch,
        base,
        created: OffsetDateTime::now_utc(),
        issue: args.issue,
        frozen: false,
    };

    let mut registry = cx.registry.clone();
    registry
        .insert(args.tag.clone(), project)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    println!("Adopted project '{}'", args.tag);
    println!("  Path: {}", final_path.display());

    Ok(())
}

fn is_worktree(path: &Path) -> bool {
    // A linked worktree has a .git file (not a directory).
    // A main worktree has a .git directory. Both count as valid worktrees.
    let dot_git = path.join(".git");
    dot_git.exists()
}

fn move_dir(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    }

    // Try rename first (same-filesystem, atomic).
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }

    // Cross-filesystem fallback: copy then delete.
    copy_dir_all(src, dest).map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    std::fs::remove_dir_all(src).map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    Ok(())
}

fn copy_dir_all(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Read the current branch from `.git/HEAD` in the worktree.
fn read_head_branch(path: &Path) -> Option<String> {
    let dot_git = path.join(".git");
    // Linked worktree: .git is a file containing "gitdir: <path>"
    let head_path = if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content
            .strip_prefix("gitdir: ")
            .map(|s| s.trim())
            .map(PathBuf::from)?;
        let gitdir = if gitdir.is_absolute() {
            gitdir
        } else {
            path.join(gitdir)
        };
        gitdir.join("HEAD")
    } else {
        dot_git.join("HEAD")
    };

    let content = std::fs::read_to_string(head_path).ok()?;
    content
        .strip_prefix("ref: refs/heads/")
        .map(|s| s.trim().to_string())
}
