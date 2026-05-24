use std::path::{Path, PathBuf};

use strsim::jaro_winkler;

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

/// Result of resolving a tag across repos.
#[derive(Debug)]
pub struct TagMatch {
    pub repo_id: String,
    pub tag: String,
}

/// Resolve a tag in the context of the global manifest.
///
/// Supports:
/// - `<repo>/<tag>` qualified form → unambiguous selection
/// - bare `<tag>` → search all repos; disambiguate via cwd or error
///
/// Returns the matching `(repo_id, tag)` or an appropriate error.
pub fn resolve_tag(global: &ReposManifest, raw_tag: &str) -> Result<TagMatch> {
    let cwd = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    // Qualified form: "repo/tag" — find the slash but not a path separator
    if let Some((repo_id, tag)) = raw_tag.split_once('/')
        && !repo_id.is_empty()
        && !tag.is_empty()
    {
        if !global.repos.contains_key(repo_id) {
            return Err(GroveError::RepoNotFound {
                id: repo_id.to_string(),
            });
        }
        return Ok(TagMatch {
            repo_id: repo_id.to_string(),
            tag: tag.to_string(),
        });
    }

    // Unqualified: collect every repo that has this tag
    let mut matches: Vec<String> = global
        .repos
        .keys()
        .filter(|repo_id| {
            let entry = &global.repos[*repo_id];
            let grove_dir = entry.work_dir.join(".grove");
            Registry::load(&grove_dir)
                .map(|reg| reg.projects.contains_key(raw_tag))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    match matches.len() {
        0 => {
            // No match — build strsim suggestion from all known tags across repos
            let hint = suggest_tag(global, raw_tag);
            Err(GroveError::UnknownTag {
                tag: raw_tag.to_string(),
                hint,
            })
        }
        1 => Ok(TagMatch {
            repo_id: matches.remove(0),
            tag: raw_tag.to_string(),
        }),
        _ => {
            // Ambiguous — try to disambiguate by cwd
            if let Some(ref cwd_path) = cwd
                && let Some(cwd_repo) = match_work_dir(global, cwd_path)
                && matches.contains(&cwd_repo)
            {
                return Ok(TagMatch {
                    repo_id: cwd_repo,
                    tag: raw_tag.to_string(),
                });
            }

            // Still ambiguous — report qualified candidates
            matches.sort();
            let candidates: Vec<String> =
                matches.iter().map(|r| format!("{r}/{raw_tag}")).collect();
            Err(GroveError::AmbiguousTag {
                tag: raw_tag.to_string(),
                candidates,
            })
        }
    }
}

/// Return the best near-match tag name across all repos using Jaro-Winkler (threshold 0.85).
fn suggest_tag(global: &ReposManifest, tag: &str) -> Option<String> {
    let mut best: Option<(f64, String)> = None;

    for entry in global.repos.values() {
        let grove_dir = entry.work_dir.join(".grove");
        if let Ok(reg) = Registry::load(&grove_dir) {
            for candidate in reg.projects.keys() {
                let score = jaro_winkler(tag, candidate);
                if score >= 0.85 && best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, candidate.clone()));
                }
            }
        }
    }

    best.map(|(_, name)| name)
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

    // ── grove-4pe.4: Tag disambiguation tests ───────────────────────────────

    use crate::registry::{Project, Registry};
    use time::OffsetDateTime;

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

    fn write_registry_with_tags(work_dir: &Path, tags: &[&str]) {
        let grove_dir = work_dir.join(".grove");
        fs::create_dir_all(&grove_dir).unwrap();
        let mut registry = Registry::default();
        for tag in tags {
            registry
                .insert(
                    tag.to_string(),
                    make_project(&format!("{}/{tag}", work_dir.display())),
                )
                .unwrap();
        }
        registry.save(&grove_dir).unwrap();
    }

    // AC1: tag in 2 repos, cwd outside both → AmbiguousTag with qualified candidates
    #[test]
    #[serial_test::serial]
    fn disambiguate_ambiguous_tag_outside_both_repos() {
        let tmp = TempDir::new().unwrap();
        let work_dir_desktop = tmp.path().join("desktop");
        let work_dir_ifkb = tmp.path().join("ifkb");
        let unrelated = tmp.path().join("other");
        fs::create_dir_all(&unrelated).unwrap();

        write_registry_with_tags(&work_dir_desktop, &["foo"]);
        write_registry_with_tags(&work_dir_ifkb, &["foo"]);

        let mut repos = BTreeMap::new();
        repos.insert("desktop".to_string(), make_repo_entry(&work_dir_desktop));
        repos.insert("ifkb".to_string(), make_repo_entry(&work_dir_ifkb));
        let global = make_manifest_with_repos(repos, None);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", unrelated.to_str().unwrap());

        let err = resolve_tag(&global, "foo").unwrap_err();
        match err {
            GroveError::AmbiguousTag { tag, candidates } => {
                assert_eq!(tag, "foo");
                assert!(
                    candidates.contains(&"desktop/foo".to_string()),
                    "candidates should include desktop/foo: {candidates:?}"
                );
                assert!(
                    candidates.contains(&"ifkb/foo".to_string()),
                    "candidates should include ifkb/foo: {candidates:?}"
                );
            }
            other => panic!("expected AmbiguousTag, got: {other:?}"),
        }
    }

    // AC2: tag in 2 repos, cwd inside repo X → X wins
    #[test]
    #[serial_test::serial]
    fn disambiguate_cwd_inside_one_repo_wins() {
        let tmp = TempDir::new().unwrap();
        let work_dir_x = tmp.path().join("repo-x");
        let work_dir_y = tmp.path().join("repo-y");
        let inside_x = work_dir_x.join("subdir");
        fs::create_dir_all(&inside_x).unwrap();

        write_registry_with_tags(&work_dir_x, &["foo"]);
        write_registry_with_tags(&work_dir_y, &["foo"]);

        let mut repos = BTreeMap::new();
        repos.insert("repo-x".to_string(), make_repo_entry(&work_dir_x));
        repos.insert("repo-y".to_string(), make_repo_entry(&work_dir_y));
        let global = make_manifest_with_repos(repos, None);

        let _guard = EnvGuard::set("GROVE_ORIG_CWD", inside_x.to_str().unwrap());

        let result = resolve_tag(&global, "foo").unwrap();
        assert_eq!(result.repo_id, "repo-x");
        assert_eq!(result.tag, "foo");
    }

    // AC3: qualified "repo/tag" → unambiguous selection
    #[test]
    #[serial_test::serial]
    fn disambiguate_qualified_form_selects_correct_repo() {
        let tmp = TempDir::new().unwrap();
        let work_dir_desktop = tmp.path().join("desktop");
        let work_dir_ifkb = tmp.path().join("ifkb");

        write_registry_with_tags(&work_dir_desktop, &["foo"]);
        write_registry_with_tags(&work_dir_ifkb, &["foo"]);

        let mut repos = BTreeMap::new();
        repos.insert("desktop".to_string(), make_repo_entry(&work_dir_desktop));
        repos.insert("ifkb".to_string(), make_repo_entry(&work_dir_ifkb));
        let global = make_manifest_with_repos(repos, None);

        let _guard = EnvGuard::remove("GROVE_ORIG_CWD");

        let result = resolve_tag(&global, "desktop/foo").unwrap();
        assert_eq!(result.repo_id, "desktop");
        assert_eq!(result.tag, "foo");
    }

    // AC4: unknown tag close to existing tag → strsim hint at threshold 0.85
    #[test]
    #[serial_test::serial]
    fn disambiguate_strsim_hint_for_close_unknown_tag() {
        let tmp = TempDir::new().unwrap();
        let work_dir = tmp.path().join("repo-a");

        write_registry_with_tags(&work_dir, &["foo"]);

        let mut repos = BTreeMap::new();
        repos.insert("repo-a".to_string(), make_repo_entry(&work_dir));
        let global = make_manifest_with_repos(repos, None);

        let _guard = EnvGuard::remove("GROVE_ORIG_CWD");

        // "baz" is not close to "foo" (score ~0.0) → no hint
        let err_no_hint = resolve_tag(&global, "baz").unwrap_err();
        match err_no_hint {
            GroveError::UnknownTag { tag, hint } => {
                assert_eq!(tag, "baz");
                assert!(
                    hint.is_none(),
                    "should have no hint for 'baz' vs 'foo': {hint:?}"
                );
            }
            other => panic!("expected UnknownTag, got: {other:?}"),
        }

        // "fo" is close to "foo" (jaro_winkler ~0.94) → hint present
        let err_with_hint = resolve_tag(&global, "fo").unwrap_err();
        match err_with_hint {
            GroveError::UnknownTag { tag, hint } => {
                assert_eq!(tag, "fo");
                assert_eq!(
                    hint.as_deref(),
                    Some("foo"),
                    "should suggest 'foo' for 'fo': {hint:?}"
                );
            }
            other => panic!("expected UnknownTag, got: {other:?}"),
        }
    }
}
