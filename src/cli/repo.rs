use anyhow::{Result, anyhow};

use crate::repo::RepoContext;

pub enum RepoSubcommand {
    Path { default: bool },
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
            // Print current repo's work_dir.
            Ok(cx.resolved.work_dir.to_string_lossy().into_owned())
        }
    }
}

pub fn run(args: &RepoArgs, cx: &RepoContext) -> Result<()> {
    let output = render(args, cx)?;
    println!("{output}");
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
