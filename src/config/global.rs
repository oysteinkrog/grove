use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum GlobalConfigError {
    #[error(
        "repos.json has schema_version {found} but this build only understands up to {max}.\nUpgrade grove or downgrade the config."
    )]
    SchemaTooNew { found: u32, max: u32 },
    #[error("failed to read repos.json: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse repos.json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOverride {
    pub terminal: Option<String>,
    pub wezterm_path: Option<PathBuf>,
    pub shell_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub main_repo: PathBuf,
    pub work_dir: PathBuf,
    #[serde(default)]
    pub dir_prefix: String,
    pub upstream_remote: String,
    pub fork_remote: String,
    pub default_base: String,
    #[serde(default)]
    pub issue_prefix: Option<String>,
    #[serde(default)]
    pub launch: Option<LaunchOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReposManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub default_repo: Option<String>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
}

impl Default for ReposManifest {
    fn default() -> Self {
        Self {
            schema_version: MAX_SCHEMA_VERSION,
            default_repo: None,
            repos: BTreeMap::new(),
        }
    }
}

impl ReposManifest {
    pub fn load(config_dir: &Path) -> Result<Self, GlobalConfigError> {
        let path = config_dir.join("repos.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = fs::read_to_string(&path)?;
        let manifest: Self = serde_json::from_str(&data)?;
        if manifest.schema_version > MAX_SCHEMA_VERSION {
            return Err(GlobalConfigError::SchemaTooNew {
                found: manifest.schema_version,
                max: MAX_SCHEMA_VERSION,
            });
        }
        Ok(manifest)
    }

    pub fn save(&self, config_dir: &Path) -> Result<(), GlobalConfigError> {
        fs::create_dir_all(config_dir)?;
        let path = config_dir.join("repos.json");
        let tmp_path = config_dir.join("repos.json.tmp");
        let data = serde_json::to_string_pretty(self)?;
        fs::write(&tmp_path, &data)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let manifest = ReposManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.schema_version, MAX_SCHEMA_VERSION);
        assert!(manifest.repos.is_empty());
        assert!(manifest.default_repo.is_none());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let mut manifest = ReposManifest::default();
        manifest.repos.insert(
            "desktop".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/desktop/master"),
                work_dir: PathBuf::from("/c/work/desktop"),
                dir_prefix: String::new(),
                upstream_remote: "if".to_string(),
                fork_remote: "my".to_string(),
                default_base: "master".to_string(),
                issue_prefix: Some("DESKTOP".to_string()),
                launch: Some(LaunchOverride {
                    terminal: Some("wt".to_string()),
                    wezterm_path: None,
                    shell_command: Some("fish -l".to_string()),
                }),
            },
        );
        manifest.save(dir.path()).unwrap();
        let loaded = ReposManifest::load(dir.path()).unwrap();
        let original_json = serde_json::to_string(&manifest).unwrap();
        let loaded_json = serde_json::to_string(&loaded).unwrap();
        assert_eq!(original_json, loaded_json);
    }

    #[test]
    fn unknown_fields_ignored() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "schema_version": 1,
            "default_repo": null,
            "repos": {},
            "unknown_future_field": "some_value",
            "another_unknown": 42
        }"#;
        std::fs::write(dir.path().join("repos.json"), json).unwrap();
        let manifest = ReposManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert!(manifest.repos.is_empty());
    }

    #[test]
    fn schema_too_new_errors() {
        let dir = TempDir::new().unwrap();
        let json = format!(
            r#"{{"schema_version": {}, "repos": {{}}}}"#,
            MAX_SCHEMA_VERSION + 1
        );
        std::fs::write(dir.path().join("repos.json"), json).unwrap();
        let err = ReposManifest::load(dir.path()).unwrap_err();
        assert!(matches!(err, GlobalConfigError::SchemaTooNew { .. }));
    }
}
