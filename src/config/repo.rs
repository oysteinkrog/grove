use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::config::global::LaunchOverride;

pub const MAX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum RepoConfigError {
    #[error(
        ".grove/config.json has schema_version {found} but this build only understands up to {max}.\nUpgrade grove or downgrade the config."
    )]
    SchemaTooNew { found: u32, max: u32 },
    #[error("failed to read .grove/config.json: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse .grove/config.json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerRepoConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub launch: Option<LaunchOverride>,
    #[serde(default)]
    pub default_base: Option<String>,
}

impl PerRepoConfig {
    /// Load per-repo config from `<work_dir>/.grove/config.json`.
    /// Returns `Ok(None)` if the file does not exist.
    pub fn load(work_dir: &Path) -> Result<Option<Self>, RepoConfigError> {
        let path = work_dir.join(".grove").join("config.json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)?;
        let config: Self = serde_json::from_str(&data)?;
        if config.schema_version > MAX_SCHEMA_VERSION {
            return Err(RepoConfigError::SchemaTooNew {
                found: config.schema_version,
                max: MAX_SCHEMA_VERSION,
            });
        }
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &Path, json: &str) {
        let grove_dir = dir.join(".grove");
        fs::create_dir_all(&grove_dir).unwrap();
        fs::write(grove_dir.join("config.json"), json).unwrap();
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let result = PerRepoConfig::load(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn round_trip_with_launch_overrides() {
        let dir = TempDir::new().unwrap();
        let original = PerRepoConfig {
            schema_version: 1,
            launch: Some(LaunchOverride {
                terminal: Some("wezterm".to_string()),
                wezterm_path: Some(PathBuf::from("/usr/bin/wezterm")),
                shell_command: Some("fish -l".to_string()),
            }),
            default_base: Some("main".to_string()),
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        write_config(dir.path(), &json);
        let loaded = PerRepoConfig::load(dir.path()).unwrap().unwrap();
        let loaded_json = serde_json::to_string_pretty(&loaded).unwrap();
        assert_eq!(json, loaded_json);
    }

    #[test]
    fn schema_too_new_errors() {
        let dir = TempDir::new().unwrap();
        let json = format!(
            r#"{{"schema_version": {}, "launch": null, "default_base": null}}"#,
            MAX_SCHEMA_VERSION + 1
        );
        write_config(dir.path(), &json);
        let err = PerRepoConfig::load(dir.path()).unwrap_err();
        assert!(matches!(err, RepoConfigError::SchemaTooNew { .. }));
    }

    #[test]
    fn minimal_config_no_optional_fields() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), r#"{"schema_version": 1}"#);
        let cfg = PerRepoConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.schema_version, 1);
        assert!(cfg.launch.is_none());
        assert!(cfg.default_base.is_none());
    }
}
