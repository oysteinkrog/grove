use crate::cli::new::validate_tag;
use crate::error::{GroveError, Result};
use crate::git::WorktreeMutator;
use crate::git::shell_backend::ShellBackend;
use crate::repo::RepoContext;

pub struct RenameArgs {
    pub old_tag: String,
    pub new_tag: String,
    pub no_move: bool,
}

pub fn run(args: &RenameArgs, cx: &RepoContext) -> Result<()> {
    validate_tag(&args.new_tag)?;

    let old_project =
        cx.registry
            .projects
            .get(&args.old_tag)
            .ok_or_else(|| GroveError::RepoNotFound {
                id: args.old_tag.clone(),
            })?;

    if cx.registry.projects.contains_key(&args.new_tag) {
        let existing_path = cx.registry.projects[&args.new_tag].path.clone();
        return Err(GroveError::DuplicateTag {
            tag: args.new_tag.clone(),
            existing_path,
        });
    }

    let old_path = old_project.path.clone();

    let mut registry = cx.registry.clone();

    if args.no_move {
        registry
            .rename(&args.old_tag, args.new_tag.clone())
            .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    } else {
        let new_path = old_path
            .parent()
            .map(|p| p.join(&args.new_tag))
            .unwrap_or_else(|| cx.resolved.work_dir.join(&args.new_tag));

        let backend = ShellBackend::new();
        // git worktree move first so git records the new location before fs rename
        backend.worktree_move(&cx.resolved.main_repo, &old_path, &new_path)?;

        let project = registry
            .remove(&args.old_tag)
            .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
        let mut updated = project;
        updated.path = new_path.clone();
        registry
            .insert(args.new_tag.clone(), updated)
            .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

        println!("Renamed '{}' \u{2192} '{}'", args.old_tag, args.new_tag);
        println!("  Old path: {}", old_path.display());
        println!("  New path: {}", new_path.display());

        registry
            .save(&cx.grove_dir())
            .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

        return Ok(());
    }

    println!(
        "Renamed '{}' \u{2192} '{}' (registry only)",
        args.old_tag, args.new_tag
    );
    println!("  Path unchanged: {}", old_path.display());

    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    Ok(())
}
