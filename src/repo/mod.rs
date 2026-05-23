use std::path::{Path, PathBuf};

use crate::config::global::ReposManifest;
use crate::config::repo::PerRepoConfig;
use crate::config::{ResolvedConfig, merge};
use crate::error::{GroveError, Result};
use crate::registry::Registry;

pub struct Cli {
    pub repo: Option<String>,
}

#[derive(Debug)]
pub struct RepoContext {
    pub id: String,
    pub global: ReposManifest,
    pub resolved: ResolvedConfig,
    pub registry: Registry,
}

impl RepoContext {
    pub fn grove_dir(&self) -> PathBuf {
        self.resolved.work_dir.join(".grove")
    }

    pub fn registry_path(&self) -> PathBuf {
        self.grove_dir().join("registry.json")
    }

    pub fn config_path(&self) -> PathBuf {
        self.grove_dir().join("config.json")
    }
}

/// Discover the active repo from a `config_dir` (usually `~/.config/grove/`).
///
/// Priority order (high → low):
/// 1. `args.repo` explicit override
/// 2. `GROVE_ORIG_CWD` env var → walk up looking for a `work_dir` match
/// 3. `std::env::current_dir()` walk up looking for a `work_dir` match
/// 4. `default_repo` in `ReposManifest`
/// 5. Error with hint listing known repo IDs
pub fn discover(config_dir: &Path, args: &Cli) -> Result<RepoContext> {
    let global = ReposManifest::load(config_dir).map_err(|e| GroveError::RepoDiscovery {
        hint: e.to_string(),
    })?;

    let repo_id = if let Some(ref id) = args.repo {
        id.clone()
    } else {
        discover_repo_id(&global)?
    };

    build_context(global, &repo_id)
}

fn discover_repo_id(global: &ReposManifest) -> Result<String> {
    let search_path = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    if let Some(cwd) = search_path
        && let Some(id) = match_work_dir(global, &cwd)
    {
        return Ok(id);
    }

    if let Some(ref id) = global.default_repo
        && global.repos.contains_key(id)
    {
        return Ok(id.clone());
    }

    let available: Vec<&str> = global.repos.keys().map(String::as_str).collect();
    let hint = if available.is_empty() {
        "no repos registered; run 'grove repos add' to get started".to_string()
    } else {
        format!(
            "available repos: {}. Pass --repo <id> or set default_repo.",
            available.join(", ")
        )
    };
    Err(GroveError::RepoDiscovery { hint })
}

/// Walk up from `cwd`, returning the repo ID whose `work_dir` is an ancestor.
/// Uses longest-prefix match to handle nested work_dirs.
fn match_work_dir(global: &ReposManifest, cwd: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;

    for (id, entry) in &global.repos {
        let work_dir = &entry.work_dir;
        if cwd.starts_with(work_dir) {
            let depth = work_dir.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, id.clone()));
            }
        }
    }

    best.map(|(_, id)| id)
}

fn build_context(global: ReposManifest, repo_id: &str) -> Result<RepoContext> {
    if !global.repos.contains_key(repo_id) {
        return Err(GroveError::RepoNotFound {
            id: repo_id.to_string(),
        });
    }

    let entry = &global.repos[repo_id];
    let per_repo = PerRepoConfig::load(&entry.work_dir).map_err(|e| GroveError::RepoDiscovery {
        hint: e.to_string(),
    })?;

    let resolved = merge(&global, repo_id, per_repo.as_ref())
        .expect("repo_id already verified present in global.repos");

    let grove_dir = resolved.work_dir.join(".grove");
    let registry = Registry::load(&grove_dir).map_err(|e| GroveError::RepoDiscovery {
        hint: e.to_string(),
    })?;

    Ok(RepoContext {
        id: repo_id.to_string(),
        global,
        resolved,
        registry,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::TempDir;

    use crate::config::global::{RepoEntry, ReposManifest};

    use super::*;

    fn make_manifest_with_repos(
        repos: BTreeMap<String, RepoEntry>,
        default_repo: Option<&str>,
    ) -> ReposManifest {
        ReposManifest {
            schema_version: 1,
            default_repo: default_repo.map(|s| s.to_string()),
            repos,
        }
    }

    fn make_repo_entry(work_dir: &Path) -> RepoEntry {
        RepoEntry {
            main_repo: work_dir.join("master"),
            work_dir: work_dir.to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            launch: None,
        }
    }

    fn write_manifest(config_dir: &Path, manifest: &ReposManifest) {
        fs::create_dir_all(config_dir).unwrap();
        let json = serde_json::to_string_pretty(manifest).unwrap();
        fs::write(config_dir.join("repos.json"), json).unwrap();
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    struct CwdGuard(std::path::PathBuf);

    impl CwdGuard {
        fn save() -> Self {
            Self(std::env::current_dir().unwrap())
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    // AC1: cwd inside a registered repo's work_dir → that repo is selected
    #[test]
    #[serial_test::serial]
    fn discover_from_cwd_inside_work_dir() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir = tmp.path().join("myrepo");
        let subdir = work_dir.join("feature");
        fs::create_dir_all(&subdir).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("myrepo".to_string(), make_repo_entry(&work_dir));
        let manifest = make_manifest_with_repos(repos, None);
        write_manifest(&config_dir, &manifest);

        let _env_guard = EnvGuard::remove("GROVE_ORIG_CWD");
        let _cwd_guard = CwdGuard::save();
        std::env::set_current_dir(&subdir).unwrap();

        let ctx = discover(&config_dir, &Cli { repo: None }).unwrap();
        assert_eq!(ctx.id, "myrepo");
    }

    // AC2: cwd unrelated + default_repo set → default repo selected
    #[test]
    #[serial_test::serial]
    fn discover_uses_default_repo_when_cwd_unrelated() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir = tmp.path().join("myrepo");
        fs::create_dir_all(&work_dir).unwrap();

        let unrelated_dir = tmp.path().join("unrelated");
        fs::create_dir_all(&unrelated_dir).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("myrepo".to_string(), make_repo_entry(&work_dir));
        let manifest = make_manifest_with_repos(repos, Some("myrepo"));
        write_manifest(&config_dir, &manifest);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", unrelated_dir.to_str().unwrap());

        let ctx = discover(&config_dir, &Cli { repo: None }).unwrap();
        assert_eq!(ctx.id, "myrepo");
    }

    // AC3: cwd unrelated + no default_repo → Err(RepoDiscovery { hint })
    #[test]
    #[serial_test::serial]
    fn discover_error_when_no_match_and_no_default() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir = tmp.path().join("myrepo");
        fs::create_dir_all(&work_dir).unwrap();

        let unrelated_dir = tmp.path().join("unrelated");
        fs::create_dir_all(&unrelated_dir).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("myrepo".to_string(), make_repo_entry(&work_dir));
        let manifest = make_manifest_with_repos(repos, None);
        write_manifest(&config_dir, &manifest);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", unrelated_dir.to_str().unwrap());

        let err = discover(&config_dir, &Cli { repo: None }).unwrap_err();
        match err {
            GroveError::RepoDiscovery { hint } => {
                assert!(
                    hint.contains("myrepo"),
                    "hint should list available repos: {hint}"
                );
            }
            other => panic!("expected RepoDiscovery, got: {other:?}"),
        }
    }

    // AC4: $GROVE_ORIG_CWD set → walks up from that path (not current_dir)
    #[test]
    #[serial_test::serial]
    fn discover_prefers_grove_orig_cwd_over_current_dir() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir_a = tmp.path().join("repo-a");
        let work_dir_b = tmp.path().join("repo-b");
        let subdir_a = work_dir_a.join("sub");
        let subdir_b = work_dir_b.join("sub");
        fs::create_dir_all(&subdir_a).unwrap();
        fs::create_dir_all(&subdir_b).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("repo-a".to_string(), make_repo_entry(&work_dir_a));
        repos.insert("repo-b".to_string(), make_repo_entry(&work_dir_b));
        let manifest = make_manifest_with_repos(repos, None);
        write_manifest(&config_dir, &manifest);

        let _orig_guard = EnvGuard::set("GROVE_ORIG_CWD", subdir_a.to_str().unwrap());
        let _cwd_guard = CwdGuard::save();
        std::env::set_current_dir(&subdir_b).unwrap();

        let ctx = discover(&config_dir, &Cli { repo: None }).unwrap();
        assert_eq!(ctx.id, "repo-a");
    }

    // AC5: cwd under exactly one repo's work_dir → that repo wins
    #[test]
    #[serial_test::serial]
    fn discover_cwd_under_exactly_one_repo() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir_a = tmp.path().join("repo-a");
        let work_dir_b = tmp.path().join("repo-b");
        let subdir_a = work_dir_a.join("feature");
        fs::create_dir_all(&subdir_a).unwrap();
        fs::create_dir_all(&work_dir_b).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("repo-a".to_string(), make_repo_entry(&work_dir_a));
        repos.insert("repo-b".to_string(), make_repo_entry(&work_dir_b));
        let manifest = make_manifest_with_repos(repos, None);
        write_manifest(&config_dir, &manifest);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", subdir_a.to_str().unwrap());

        let ctx = discover(&config_dir, &Cli { repo: None }).unwrap();
        assert_eq!(ctx.id, "repo-a");
    }

    // AC6: --repo <id> provided → that repo is selected; cwd/env ignored
    #[test]
    #[serial_test::serial]
    fn discover_explicit_repo_flag_overrides_all() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        let work_dir_a = tmp.path().join("repo-a");
        let work_dir_b = tmp.path().join("repo-b");
        let subdir_a = work_dir_a.join("sub");
        fs::create_dir_all(&subdir_a).unwrap();
        fs::create_dir_all(&work_dir_b).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert("repo-a".to_string(), make_repo_entry(&work_dir_a));
        repos.insert("repo-b".to_string(), make_repo_entry(&work_dir_b));
        let manifest = make_manifest_with_repos(repos, None);
        write_manifest(&config_dir, &manifest);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", subdir_a.to_str().unwrap());

        let ctx = discover(
            &config_dir,
            &Cli {
                repo: Some("repo-b".to_string()),
            },
        )
        .unwrap();
        assert_eq!(ctx.id, "repo-b");
    }
}
