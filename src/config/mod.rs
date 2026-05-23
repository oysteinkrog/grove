pub mod global;
pub mod repo;

use std::path::PathBuf;

use global::{LaunchOverride, RepoEntry, ReposManifest};
use repo::PerRepoConfig;

/// Fully-merged runtime configuration for a single repo.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub main_repo: PathBuf,
    pub work_dir: PathBuf,
    pub dir_prefix: String,
    pub upstream_remote: String,
    pub fork_remote: String,
    pub default_base: String,
    pub issue_prefix: Option<String>,
    pub launch: Option<LaunchOverride>,
}

impl From<&RepoEntry> for ResolvedConfig {
    fn from(entry: &RepoEntry) -> Self {
        Self {
            main_repo: entry.main_repo.clone(),
            work_dir: entry.work_dir.clone(),
            dir_prefix: entry.dir_prefix.clone(),
            upstream_remote: entry.upstream_remote.clone(),
            fork_remote: entry.fork_remote.clone(),
            default_base: entry.default_base.clone(),
            issue_prefix: entry.issue_prefix.clone(),
            launch: entry.launch.clone(),
        }
    }
}

/// Merge global config for `repo_id` with an optional per-repo override.
///
/// Per-repo fields take precedence; `None` per-repo fields fall back to global.
/// Returns `None` if `repo_id` is not found in the global manifest.
pub fn merge(
    global: &ReposManifest,
    repo_id: &str,
    per_repo: Option<&PerRepoConfig>,
) -> Option<ResolvedConfig> {
    let entry = global.repos.get(repo_id)?;
    let mut resolved = ResolvedConfig::from(entry);

    if let Some(pr) = per_repo {
        if let Some(ref base) = pr.default_base {
            resolved.default_base = base.clone();
        }
        if pr.launch.is_some() {
            resolved.launch = pr.launch.clone();
        }
    }

    Some(resolved)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use global::RepoEntry;

    fn make_manifest(default_base: &str, launch: Option<LaunchOverride>) -> ReposManifest {
        let mut repos = BTreeMap::new();
        repos.insert(
            "myrepo".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/myrepo/main"),
                work_dir: PathBuf::from("/c/work/myrepo"),
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
                fork_remote: "origin".to_string(),
                default_base: default_base.to_string(),
                issue_prefix: None,
                launch,
            },
        );
        ReposManifest {
            schema_version: 1,
            default_repo: None,
            repos,
        }
    }

    #[test]
    fn no_per_repo_returns_global_verbatim() {
        let launch = Some(LaunchOverride {
            terminal: Some("wt".to_string()),
            wezterm_path: None,
            shell_command: None,
        });
        let manifest = make_manifest("main", launch.clone());
        let resolved = merge(&manifest, "myrepo", None).unwrap();
        assert_eq!(resolved.default_base, "main");
        assert!(resolved.launch.is_some());
        assert_eq!(
            resolved.launch.as_ref().unwrap().terminal.as_deref(),
            Some("wt")
        );
    }

    #[test]
    fn per_repo_overrides_take_precedence() {
        let global_launch = Some(LaunchOverride {
            terminal: Some("wt".to_string()),
            wezterm_path: None,
            shell_command: None,
        });
        let manifest = make_manifest("main", global_launch);
        let per_repo = PerRepoConfig {
            schema_version: 1,
            launch: Some(LaunchOverride {
                terminal: Some("wezterm".to_string()),
                wezterm_path: Some(PathBuf::from("/usr/bin/wezterm")),
                shell_command: Some("fish".to_string()),
            }),
            default_base: Some("develop".to_string()),
        };
        let resolved = merge(&manifest, "myrepo", Some(&per_repo)).unwrap();
        assert_eq!(resolved.default_base, "develop");
        let launch = resolved.launch.unwrap();
        assert_eq!(launch.terminal.as_deref(), Some("wezterm"));
        assert_eq!(
            launch.wezterm_path.as_deref(),
            Some(std::path::Path::new("/usr/bin/wezterm"))
        );
        assert_eq!(launch.shell_command.as_deref(), Some("fish"));
    }

    #[test]
    fn per_repo_none_fields_fall_back_to_global() {
        let global_launch = Some(LaunchOverride {
            terminal: Some("wt".to_string()),
            wezterm_path: None,
            shell_command: Some("bash".to_string()),
        });
        let manifest = make_manifest("main", global_launch);
        // per-repo sets neither launch nor default_base
        let per_repo = PerRepoConfig {
            schema_version: 1,
            launch: None,
            default_base: None,
        };
        let resolved = merge(&manifest, "myrepo", Some(&per_repo)).unwrap();
        // Should inherit global values
        assert_eq!(resolved.default_base, "main");
        let launch = resolved.launch.unwrap();
        assert_eq!(launch.terminal.as_deref(), Some("wt"));
        assert_eq!(launch.shell_command.as_deref(), Some("bash"));
    }

    #[test]
    fn unknown_repo_id_returns_none() {
        let manifest = make_manifest("main", None);
        let result = merge(&manifest, "nonexistent", None);
        assert!(result.is_none());
    }
}
