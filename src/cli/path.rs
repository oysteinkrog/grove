use anyhow::{Result, anyhow};
use strsim::jaro_winkler;

use crate::repo::RepoContext;

pub struct PathArgs {
    pub tag: String,
}

/// Find the best near-match suggestion for an unknown tag.
fn suggest_near_match(tag: &str, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .map(|c| (c, jaro_winkler(tag, c)))
        .filter(|(_, score)| *score > 0.8)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(c, _)| c.to_string())
}

/// Resolve the worktree path for the given tag and return it as a String.
/// Separates logic from printing so unit tests can assert the value.
pub fn render(args: &PathArgs, cx: &RepoContext) -> Result<String> {
    if let Some(project) = cx.registry.projects.get(&args.tag) {
        return Ok(project.path.to_string_lossy().into_owned());
    }

    let candidates: Vec<&str> = cx.registry.projects.keys().map(String::as_str).collect();

    if let Some(suggestion) = suggest_near_match(&args.tag, &candidates) {
        Err(anyhow!(
            "unknown tag '{}' — did you mean '{}'?",
            args.tag,
            suggestion
        ))
    } else {
        Err(anyhow!("unknown tag '{}'", args.tag))
    }
}

pub fn run(args: &PathArgs, cx: &RepoContext) -> Result<()> {
    let path = render(args, cx)?;
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

    // AC1: known tag prints the exact worktree path
    #[test]
    fn known_tag_returns_path() {
        let mut projects = BTreeMap::new();
        projects.insert(
            "myfeature".to_string(),
            make_project("/c/work/test/myfeature"),
        );

        let cx = make_context(projects);
        let args = PathArgs {
            tag: "myfeature".to_string(),
        };

        let result = render(&args, &cx).unwrap();
        assert_eq!(result, "/c/work/test/myfeature");
    }

    // AC2: unknown tag with near match suggests the match
    #[test]
    fn unknown_tag_suggests_near_match() {
        let mut projects = BTreeMap::new();
        projects.insert("lazy-vm".to_string(), make_project("/c/work/test/lazy-vm"));

        let cx = make_context(projects);
        let args = PathArgs {
            tag: "lazyvm".to_string(),
        };

        let err = render(&args, &cx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lazy-vm"),
            "error should suggest near match 'lazy-vm': {msg}"
        );
        assert!(
            msg.contains("did you mean"),
            "error should say 'did you mean': {msg}"
        );
    }

    // AC2: unknown tag with no near match gives plain error
    #[test]
    fn unknown_tag_no_near_match_plain_error() {
        let mut projects = BTreeMap::new();
        projects.insert("alpha".to_string(), make_project("/c/work/test/alpha"));

        let cx = make_context(projects);
        let args = PathArgs {
            tag: "zzznomatch".to_string(),
        };

        let err = render(&args, &cx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown tag"),
            "error should say 'unknown tag': {msg}"
        );
        assert!(
            !msg.contains("did you mean"),
            "should not suggest near match: {msg}"
        );
    }

    // AC1: path output has no trailing decoration (exactly the path string)
    #[test]
    fn path_output_has_no_decoration() {
        let mut projects = BTreeMap::new();
        projects.insert(
            "clean".to_string(),
            make_project("/c/work/test/clean-branch"),
        );

        let cx = make_context(projects);
        let args = PathArgs {
            tag: "clean".to_string(),
        };

        let result = render(&args, &cx).unwrap();
        assert!(!result.contains('\n'), "path must not contain newlines");
        assert!(!result.contains('\t'), "path must not contain tabs");
        assert_eq!(result, "/c/work/test/clean-branch");
    }
}
