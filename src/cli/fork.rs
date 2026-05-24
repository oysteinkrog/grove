use time::OffsetDateTime;

use crate::cli::new::{compute_branch_name, validate_tag};
use crate::error::{GroveError, Result};
use crate::git::WorktreeMutator;
use crate::git::shell_backend::ShellBackend;
use crate::registry::Project;
use crate::repo::RepoContext;

pub struct ForkArgs {
    /// 1 or 2 positionals: [source] <new_tag>. Validated in run().
    pub positionals: Vec<String>,
    pub issue: Option<u32>,
    pub branch: Option<String>,
    pub no_fetch: bool,
}

pub fn run(args: &ForkArgs, cx: &RepoContext) -> Result<()> {
    let (source_tag, new_tag) = parse_positionals(&args.positionals, cx)?;

    validate_tag(&new_tag)?;

    if cx.registry.projects.contains_key(&new_tag) {
        let existing_path = cx.registry.projects[&new_tag].path.clone();
        return Err(GroveError::DuplicateTag {
            tag: new_tag.clone(),
            existing_path,
        });
    }

    let source_project =
        cx.registry
            .projects
            .get(&source_tag)
            .ok_or_else(|| GroveError::RepoNotFound {
                id: source_tag.clone(),
            })?;

    let source_branch = source_project.branch.clone();
    let source_base = source_project.base.clone();

    let new_branch = compute_branch_name(
        &new_tag,
        args.issue,
        args.branch.as_deref(),
        cx.resolved.issue_prefix.as_deref(),
    );

    let target = cx.resolved.work_dir.join(&new_tag);

    let backend = ShellBackend::new();

    if !args.no_fetch {
        backend.fetch(&cx.resolved.main_repo, &cx.resolved.upstream_remote)?;
    }

    backend.worktree_add(
        &cx.resolved.main_repo,
        &target,
        &new_branch,
        Some(&source_branch),
    )?;

    let project = Project {
        path: target.clone(),
        branch: new_branch.clone(),
        base: source_base,
        created: OffsetDateTime::now_utc(),
        issue: args.issue,
        frozen: false,
    };

    let mut registry = cx.registry.clone();
    registry
        .insert(new_tag.clone(), project)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    println!("Forked '{}' \u{2192} '{}'", source_tag, new_tag);
    println!("  Source branch: {source_branch}");
    if let Some(issue) = args.issue {
        println!(
            "  Issue:         {}-{issue}",
            cx.resolved.issue_prefix.as_deref().unwrap_or("ISSUE")
        );
    }
    println!("  Branch:        {new_branch}");
    println!("  Path:          {}", target.display());

    Ok(())
}

/// Resolve `(source_tag, new_tag)` from the 1-or-2-positional Vec.
///
/// With 1 positional: source is inferred from cwd (must be inside a known project).
/// With 2 positionals: first is source, second is new tag.
fn parse_positionals(positionals: &[String], cx: &RepoContext) -> Result<(String, String)> {
    match positionals {
        [new_tag] => {
            let source_tag = infer_source_from_cwd(cx)?;
            Ok((source_tag, new_tag.clone()))
        }
        [source_tag, new_tag] => Ok((source_tag.clone(), new_tag.clone())),
        _ => Err(GroveError::InvalidTag {
            tag: String::new(),
            reason: format!("fork takes 1 or 2 arguments, got {}", positionals.len()),
        }),
    }
}

/// Find which registered project the current working directory is inside.
fn infer_source_from_cwd(cx: &RepoContext) -> Result<String> {
    let cwd = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    let cwd = cwd.ok_or_else(|| GroveError::RepoDiscovery {
        hint: "cannot determine current directory; pass source explicitly".to_string(),
    })?;

    let mut best: Option<(usize, String)> = None;
    for (tag, project) in &cx.registry.projects {
        if cwd.starts_with(&project.path) {
            let depth = project.path.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, tag.clone()));
            }
        }
    }

    best.map(|(_, tag)| tag).ok_or_else(|| GroveError::RepoDiscovery {
        hint: "cwd is not inside any known project; pass source explicitly: grove fork <source> <new-tag>".to_string(),
    })
}
