use anyhow::Result;

use crate::repo::RepoContext;

use super::path::{PathArgs, render};

pub struct CdArgs {
    pub tag: String,
}

/// Alias for `grove path` — the actual `cd` happens in the shell wrapper.
/// Returns the same path string that `path::render` would return.
pub fn render_cd(args: &CdArgs, cx: &RepoContext) -> Result<String> {
    render(
        &PathArgs {
            tag: args.tag.clone(),
        },
        cx,
    )
}

pub fn run(args: &CdArgs, cx: &RepoContext) -> Result<()> {
    let path = render_cd(args, cx)?;
    println!("{path}");
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

    fn make_context(projects: BTreeMap<String, Project>) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "test".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/test/master"),
                work_dir: PathBuf::from("/c/work/test"),
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
            default_repo: Some("test".to_string()),
            repos,
        };
        let resolved = crate::config::ResolvedConfig {
            main_repo: PathBuf::from("/c/work/test/master"),
            work_dir: PathBuf::from("/c/work/test"),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let registry = Registry {
            schema_version: 1,
            projects,
        };
        RepoContext {
            id: "test".to_string(),
            global,
            resolved,
            registry,
        }
    }

    // CD delegates to path::render for known tags
    #[test]
    fn cd_known_tag_returns_same_as_path() {
        let mut projects = BTreeMap::new();
        projects.insert(
            "myfeature".to_string(),
            make_project("/c/work/test/myfeature"),
        );

        let cx = make_context(projects);
        let cd_args = CdArgs {
            tag: "myfeature".to_string(),
        };
        let path_args = crate::cli::path::PathArgs {
            tag: "myfeature".to_string(),
        };

        let cd_result = render_cd(&cd_args, &cx).unwrap();
        let path_result = crate::cli::path::render(&path_args, &cx).unwrap();
        assert_eq!(cd_result, path_result);
    }

    // CD returns error for unknown tags
    #[test]
    fn cd_unknown_tag_returns_error() {
        let cx = make_context(BTreeMap::new());
        let args = CdArgs {
            tag: "nonexistent".to_string(),
        };
        assert!(render_cd(&args, &cx).is_err());
    }

    // CD output has no decoration
    #[test]
    fn cd_output_no_decoration() {
        let mut projects = BTreeMap::new();
        projects.insert("feat".to_string(), make_project("/c/work/test/feat"));

        let cx = make_context(projects);
        let args = CdArgs {
            tag: "feat".to_string(),
        };
        let result = render_cd(&args, &cx).unwrap();
        assert!(!result.contains('\n'));
        assert_eq!(result, "/c/work/test/feat");
    }
}
