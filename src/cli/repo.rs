use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::config::global::{RepoEntry, ReposManifest};
use crate::error::GroveError;
use crate::registry::Registry;
use crate::repo::RepoContext;

pub enum RepoSubcommand {
    Path { default: bool },
    Add(AddArgs),
}

pub struct AddArgs {
    pub path: PathBuf,
    pub id: Option<String>,
    pub issue_prefix: Option<String>,
    pub upstream: Option<String>,
    pub fork: Option<String>,
    pub default_base: Option<String>,
    pub make_default: bool,
    pub config_dir: PathBuf,
}

pub struct RepoArgs {
    pub subcommand: RepoSubcommand,
}

/// Return the string to print for `grove repo path --default`.
pub fn render(args: &RepoArgs, cx: &RepoContext) -> Result<String> {
    match &args.subcommand {
        RepoSubcommand::Path { default: true } => {
            let repo_id =
                cx.global.default_repo.as_deref().ok_or_else(|| {
                    anyhow!("no default repo set; set default_repo in repos.json")
                })?;
            let entry = cx
                .global
                .repos
                .get(repo_id)
                .ok_or_else(|| anyhow!("default repo '{}' not found in repos.json", repo_id))?;
            Ok(entry.work_dir.to_string_lossy().into_owned())
        }
        RepoSubcommand::Path { default: false } => {
            Ok(cx.resolved.work_dir.to_string_lossy().into_owned())
        }
        RepoSubcommand::Add(_) => Err(anyhow!("use run() for add subcommand")),
    }
}

pub fn run(args: &RepoArgs, cx: &RepoContext) -> Result<()> {
    match &args.subcommand {
        RepoSubcommand::Add(add_args) => run_add(add_args),
        _ => {
            let output = render(args, cx)?;
            println!("{output}");
            Ok(())
        }
    }
}

fn is_git_repo(path: &Path) -> bool {
    // Fast check: presence of .git entry. Works for both regular repos and worktrees.
    if path.join(".git").exists() {
        return true;
    }
    // Fallback: try gix discovery (handles bare repos and edge cases).
    gix::discover(path).is_ok()
}

pub fn run_add(args: &AddArgs) -> Result<()> {
    let path = args
        .path
        .canonicalize()
        .map_err(|e| anyhow!("cannot resolve path '{}': {e}", args.path.display()))?;

    if !is_git_repo(&path) {
        return Err(GroveError::NotAGitRepo { path }.into());
    }

    let id = match &args.id {
        Some(id) => id.clone(),
        None => path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("cannot derive repo id from path '{}'", path.display()))?,
    };

    let mut manifest = ReposManifest::load(&args.config_dir)
        .map_err(|e| anyhow!("failed to load repos.json: {e}"))?;

    if manifest.repos.contains_key(&id) {
        return Err(GroveError::DuplicateRepoId { id }.into());
    }

    let upstream_remote = args
        .upstream
        .clone()
        .unwrap_or_else(|| "upstream".to_string());
    let fork_remote = args.fork.clone().unwrap_or_else(|| "origin".to_string());
    let default_base = args
        .default_base
        .clone()
        .unwrap_or_else(|| "main".to_string());

    let entry = RepoEntry {
        main_repo: path.clone(),
        work_dir: path.clone(),
        dir_prefix: String::new(),
        upstream_remote,
        fork_remote,
        default_base,
        issue_prefix: args.issue_prefix.clone(),
        launch: None,
    };

    manifest.repos.insert(id.clone(), entry);

    if args.make_default {
        manifest.default_repo = Some(id.clone());
    }

    manifest
        .save(&args.config_dir)
        .map_err(|e| anyhow!("failed to save repos.json: {e}"))?;

    // Create .grove/ directory and empty registry.json under work_dir.
    let grove_dir = path.join(".grove");
    let empty_registry = Registry::default();
    empty_registry
        .save(&grove_dir)
        .map_err(|e| anyhow!("failed to create .grove/registry.json: {e}"))?;

    println!("Added repo '{id}' at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::config::global::{RepoEntry, ReposManifest};
    use crate::registry::{Project, Registry};
    use crate::repo::RepoContext;

    fn make_project(path: &str) -> Project {
        Project {
            path: PathBuf::from(path),
            branch: "main".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue: None,
            frozen: false,
        }
    }

    fn make_context_with_default(default_repo: Option<&str>, work_dir: &str) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "myrepo".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/myrepo/master"),
                work_dir: PathBuf::from(work_dir),
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
                fork_remote: "origin".to_string(),
                default_base: "main".to_string(),
                issue_prefix: None,
                launch: None,
            },
        );
        let global = ReposManifest {
            schema_version: 1,
            default_repo: default_repo.map(|s| s.to_string()),
            repos,
        };
        let resolved = crate::config::ResolvedConfig {
            main_repo: PathBuf::from("/c/work/myrepo/master"),
            work_dir: PathBuf::from(work_dir),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let registry = Registry {
            schema_version: 1,
            projects: {
                let mut m = BTreeMap::new();
                m.insert("feat".to_string(), make_project("/c/work/myrepo/feat"));
                m
            },
        };
        RepoContext {
            id: "myrepo".to_string(),
            global,
            resolved,
            registry,
        }
    }

    // AC4: grove repo path --default prints default repo work_dir
    #[test]
    fn repo_path_default_prints_work_dir() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let result = render(&args, &cx).unwrap();
        assert_eq!(result, "/c/work/myrepo");
    }

    // AC4: no default_repo set → error
    #[test]
    fn repo_path_default_no_default_repo_errors() {
        let cx = make_context_with_default(None, "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let err = render(&args, &cx).unwrap_err();
        assert!(
            err.to_string().contains("no default repo"),
            "expected 'no default repo' error: {}",
            err
        );
    }

    // Path output has no decoration
    #[test]
    fn repo_path_no_decoration() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/my-repo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let result = render(&args, &cx).unwrap();
        assert!(!result.contains('\n'));
        assert_eq!(result, "/c/work/my-repo");
    }

    // grove repo path (no --default) prints current repo work_dir
    #[test]
    fn repo_path_no_flag_prints_current_repo_work_dir() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: false },
        };
        let result = render(&args, &cx).unwrap();
        assert_eq!(result, "/c/work/myrepo");
    }
}
