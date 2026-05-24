use anyhow::{Result, anyhow};
use strsim::jaro_winkler;

use crate::repo::RepoContext;

pub struct FreezeArgs {
    pub tag: Option<String>,
}

fn suggest_near_match(tag: &str, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .map(|c| (c, jaro_winkler(tag, c)))
        .filter(|(_, score)| *score > 0.8)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(c, _)| c.to_string())
}

fn resolve_tag(args: &FreezeArgs, cx: &RepoContext) -> Result<String> {
    if let Some(ref tag) = args.tag {
        return Ok(tag.clone());
    }

    let cwd = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow!("cannot determine current directory; pass tag explicitly"))?;

    let mut best: Option<(usize, String)> = None;
    for (tag, project) in &cx.registry.projects {
        if cwd.starts_with(&project.path) {
            let depth = project.path.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, tag.clone()));
            }
        }
    }

    best.map(|(_, tag)| tag)
        .ok_or_else(|| anyhow!("cwd is not inside any known project; pass tag explicitly"))
}

fn set_frozen(args: &FreezeArgs, cx: &RepoContext, frozen: bool) -> Result<()> {
    let tag = resolve_tag(args, cx)?;

    if !cx.registry.projects.contains_key(&tag) {
        let candidates: Vec<&str> = cx.registry.projects.keys().map(String::as_str).collect();
        return if let Some(suggestion) = suggest_near_match(&tag, &candidates) {
            Err(anyhow!(
                "unknown tag '{}' — did you mean '{}'?",
                tag,
                suggestion
            ))
        } else {
            Err(anyhow!("unknown tag '{}'", tag))
        };
    }

    let mut registry = cx.registry.clone();
    registry.projects.get_mut(&tag).unwrap().frozen = frozen;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| anyhow!("registry error: {e}"))?;

    let action = if frozen { "frozen" } else { "thawed" };
    println!("Project '{tag}' {action}.");
    Ok(())
}

pub fn run_freeze(args: &FreezeArgs, cx: &RepoContext) -> Result<()> {
    set_frozen(args, cx, true)
}

pub fn run_thaw(args: &FreezeArgs, cx: &RepoContext) -> Result<()> {
    set_frozen(args, cx, false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::config::ResolvedConfig;
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
        let resolved = ResolvedConfig {
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

    // AC1: grove freeze <tag> sets frozen=true
    #[test]
    fn freeze_sets_frozen_true() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let grove_dir = tmp.path().join(".grove");
        std::fs::create_dir_all(&grove_dir).unwrap();

        let mut projects = BTreeMap::new();
        projects.insert(
            "myfeature".to_string(),
            make_project("/c/work/test/myfeature"),
        );

        let mut cx = make_context(projects);
        cx.resolved.work_dir = tmp.path().to_path_buf();
        cx.registry.save(&grove_dir).unwrap();

        let args = FreezeArgs {
            tag: Some("myfeature".to_string()),
        };
        run_freeze(&args, &cx).unwrap();

        let loaded = crate::registry::Registry::load(&grove_dir).unwrap();
        assert!(loaded.projects["myfeature"].frozen);
    }

    // AC2: grove thaw <tag> sets frozen=false
    #[test]
    fn thaw_sets_frozen_false() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let grove_dir = tmp.path().join(".grove");
        std::fs::create_dir_all(&grove_dir).unwrap();

        let mut projects = BTreeMap::new();
        let mut p = make_project("/c/work/test/myfeature");
        p.frozen = true;
        projects.insert("myfeature".to_string(), p);

        let mut cx = make_context(projects);
        cx.resolved.work_dir = tmp.path().to_path_buf();
        cx.registry.save(&grove_dir).unwrap();

        let args = FreezeArgs {
            tag: Some("myfeature".to_string()),
        };
        run_thaw(&args, &cx).unwrap();

        let loaded = crate::registry::Registry::load(&grove_dir).unwrap();
        assert!(!loaded.projects["myfeature"].frozen);
    }

    // AC3: no tag + cwd inside a project targets that project
    #[test]
    fn no_tag_uses_cwd_project() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let grove_dir = tmp.path().join(".grove");
        std::fs::create_dir_all(&grove_dir).unwrap();

        let project_path = tmp.path().join("myfeature");
        std::fs::create_dir_all(&project_path).unwrap();

        let mut projects = BTreeMap::new();
        let p = Project {
            path: project_path.clone(),
            branch: "main".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue: None,
            frozen: false,
        };
        projects.insert("myfeature".to_string(), p);

        let mut cx = make_context(projects);
        cx.resolved.work_dir = tmp.path().to_path_buf();
        cx.registry.save(&grove_dir).unwrap();

        // Set GROVE_ORIG_CWD to inside the project path
        let old = std::env::var("GROVE_ORIG_CWD").ok();
        unsafe { std::env::set_var("GROVE_ORIG_CWD", &project_path) };

        let args = FreezeArgs { tag: None };
        let result = run_freeze(&args, &cx);

        match old {
            Some(v) => unsafe { std::env::set_var("GROVE_ORIG_CWD", v) },
            None => unsafe { std::env::remove_var("GROVE_ORIG_CWD") },
        }

        result.unwrap();
        let loaded = crate::registry::Registry::load(&grove_dir).unwrap();
        assert!(loaded.projects["myfeature"].frozen);
    }

    // AC4: unknown tag returns error with near-match hint
    #[test]
    fn unknown_tag_suggests_near_match() {
        let mut projects = BTreeMap::new();
        projects.insert("lazy-vm".to_string(), make_project("/c/work/test/lazy-vm"));

        let cx = make_context(projects);
        let args = FreezeArgs {
            tag: Some("lazyvm".to_string()),
        };

        let err = run_freeze(&args, &cx).unwrap_err();
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
}
