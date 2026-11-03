use time::OffsetDateTime;

use crate::cli::new::{
    add_from_source, compute_branch_name, compute_target_path, prepare_base_for, resolve_expiry,
    validate_tag,
};
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
    pub ephemeral: bool,
    pub ttl: Option<String>,
    /// Copy the whole source lane, build output included, as grove did
    /// before. Off by default: on a large lane that copy creates about a
    /// million filesystem entries, and several at once froze the desktop.
    pub copy_source: bool,
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
    // Start at the commit the source lane has checked out, which may differ
    // from its registered branch (switched or detached). Fall back to the
    // registered branch when the source tree is gone.
    let start_point = fork_start_point(&source_project.path, &source_branch);
    let source_base = source_project.base.clone();

    let new_branch = compute_branch_name(
        &new_tag,
        args.issue,
        args.branch.as_deref(),
        cx.resolved.issue_prefix.as_deref(),
    );

    let target = compute_target_path(&cx.resolved.work_dir, &new_tag, args.ephemeral);
    let expires_at = resolve_expiry(
        args.ephemeral,
        args.ttl.as_deref(),
        OffsetDateTime::now_utc(),
    )?;

    let backend = ShellBackend::new();

    if !args.no_fetch {
        backend.fetch(&cx.resolved.main_repo, &cx.resolved.upstream_remote)?;
    }

    // By default the fork is a copy of the clean base checkout (tracked files
    // only), moved to the source branch, as `grove new` does. It starts with a
    // cold build. `--copy-source` copies the source lane itself instead,
    // build output included.
    let target_dir = target.parent().unwrap_or(&cx.resolved.work_dir);
    let mut copied_from = None;
    let mut reflinked = false;
    let mut how = "reflink";
    let made = if args.copy_source {
        copied_from = Some(source_project.path.clone());
        // A source lane that is a subvolume is snapshotted, build output and
        // all, at almost no cost; any other is reflink-copied as before.
        add_from_source(
            &backend,
            cx,
            &source_project.path,
            &target,
            &new_branch,
            &start_point,
            None,
        )?
    } else if let Some(prepared) = prepare_base_for(&backend, cx, &source_base, target_dir) {
        let base_dir = prepared.path.clone();
        copied_from = Some(base_dir.clone());
        add_from_source(
            &backend,
            cx,
            &base_dir,
            &target,
            &new_branch,
            &start_point,
            Some(prepared.lock),
        )?
    } else {
        None
    };
    if let Some(kind) = made {
        reflinked = true;
        how = kind;
    }
    if !reflinked {
        // A plain checkout writes every tracked file: serialize it too.
        let heavy = crate::machine_lock::MachineLock::acquire(
            "copy",
            &crate::machine_lock::tag_of(&target),
        )
        .map_err(|e| GroveError::GitCommandFailed {
            cmd: "take the grove copy lock".to_string(),
            stderr: e,
        })?;
        backend.worktree_add(
            &cx.resolved.main_repo,
            &target,
            &new_branch,
            Some(&start_point),
        )?;
        backend.lfs_dedup_or_warn(&target);
        drop(heavy);
    }

    let project = Project {
        path: target.clone(),
        branch: new_branch.clone(),
        base: source_base,
        created: OffsetDateTime::now_utc(),
        issue: args.issue,
        frozen: false,
        expires_at,
        owner: crate::events::Owner::detect_non_empty(),
    };

    let mut event = crate::events::Event::for_project("fork", &cx.id, &new_tag, &project);
    event.ttl = args.ttl.clone();

    crate::registry::Registry::insert_on_disk(&cx.grove_dir(), new_tag.clone(), project)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    crate::events::record(&event);

    println!("Forked '{}' \u{2192} '{}'", source_tag, new_tag);
    println!("  Source branch: {source_branch}");
    if start_point != source_branch {
        println!("  Start:         {start_point}");
    }
    if let Some(issue) = args.issue {
        println!(
            "  Issue:         {}-{issue}",
            cx.resolved.issue_prefix.as_deref().unwrap_or("ISSUE")
        );
    }
    println!("  Branch:        {new_branch}");
    println!("  Path:          {}", target.display());
    if let (true, Some(source)) = (reflinked, &copied_from) {
        println!("  Copy:          {how} of {}", source.display());
    }
    if !args.copy_source {
        println!("  Build output:  none copied (use --copy-source to copy the source lane)");
    }
    if let Some(expires_at) = expires_at {
        println!(
            "  Expires:       {} (ephemeral)",
            expires_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| expires_at.to_string())
        );
    }

    Ok(())
}

/// Where the fork starts: the registered branch when the source lane still
/// has it checked out (so git's branch.autoSetupMerge=inherit still copies
/// its upstream, as before), else the commit the source has checked out, else
/// the registered branch when the source tree is gone.
fn fork_start_point(source: &std::path::Path, branch: &str) -> String {
    let Some(head) = source_head(source) else {
        return branch.to_string();
    };
    let on_branch = ShellBackend::new()
        .run_stdout(source, &["symbolic-ref", "-q", "--short", "HEAD"])
        .is_ok_and(|current| current == branch);
    if on_branch { branch.to_string() } else { head }
}

/// The commit checked out in the worktree at `path`, if it is one.
fn source_head(path: &std::path::Path) -> Option<String> {
    if !path.join(".git").exists() {
        return None;
    }
    ShellBackend::new()
        .run_stdout(path, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .filter(|oid| !oid.is_empty())
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
