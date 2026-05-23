pub mod project;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use project::Project;

pub const MAX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("tag already exists: {0}")]
    DuplicateTag(String),
    #[error(
        "registry.json has schema_version {found} but this build only understands up to {max}.\nUpgrade grove or downgrade the config."
    )]
    SchemaTooNew { found: u32, max: u32 },
    #[error("failed to read registry.json: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse registry.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tag not found: {0}")]
    TagNotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub schema_version: u32,
    pub projects: BTreeMap<String, Project>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: MAX_SCHEMA_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

impl Registry {
    pub fn load(grove_dir: &Path) -> Result<Self, RegistryError> {
        let path = grove_dir.join("registry.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = fs::read_to_string(&path)?;
        let registry: Self = serde_json::from_str(&data)?;
        if registry.schema_version > MAX_SCHEMA_VERSION {
            return Err(RegistryError::SchemaTooNew {
                found: registry.schema_version,
                max: MAX_SCHEMA_VERSION,
            });
        }
        Ok(registry)
    }

    pub fn save(&self, grove_dir: &Path) -> Result<(), RegistryError> {
        fs::create_dir_all(grove_dir)?;
        let path = grove_dir.join("registry.json");
        let tmp_path = grove_dir.join("registry.json.tmp");
        let bak_path = grove_dir.join("registry.json.bak");

        // Roll backup of the existing file before overwriting.
        if path.exists() {
            fs::copy(&path, &bak_path)?;
        }

        let data = serde_json::to_string_pretty(self)?;
        fs::write(&tmp_path, &data)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn insert(&mut self, tag: String, project: Project) -> Result<(), RegistryError> {
        if self.projects.contains_key(&tag) {
            return Err(RegistryError::DuplicateTag(tag));
        }
        self.projects.insert(tag, project);
        Ok(())
    }

    pub fn remove(&mut self, tag: &str) -> Result<Project, RegistryError> {
        self.projects
            .remove(tag)
            .ok_or_else(|| RegistryError::TagNotFound(tag.to_string()))
    }

    pub fn rename(&mut self, old_tag: &str, new_tag: String) -> Result<(), RegistryError> {
        if self.projects.contains_key(&new_tag) {
            return Err(RegistryError::DuplicateTag(new_tag));
        }
        let project = self
            .projects
            .remove(old_tag)
            .ok_or_else(|| RegistryError::TagNotFound(old_tag.to_string()))?;
        self.projects.insert(new_tag, project);
        Ok(())
    }

    pub fn list(&self) -> impl Iterator<Item = (&str, &Project)> {
        self.projects.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use time::OffsetDateTime;

    use super::*;

    fn make_project(path: &str) -> Project {
        Project {
            path: PathBuf::from(path),
            branch: "main".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::now_utc(),
            issue: None,
            frozen: false,
        }
    }

    #[test]
    fn load_default_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");
        let registry = Registry::load(&grove_dir).unwrap();
        assert_eq!(registry.schema_version, MAX_SCHEMA_VERSION);
        assert!(registry.projects.is_empty());
    }

    #[test]
    fn round_trip_preserves_fields() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");
        fs::create_dir_all(&grove_dir).unwrap();

        let created = OffsetDateTime::now_utc();
        let project = Project {
            path: PathBuf::from("/c/work/myrepo/feature-x"),
            branch: "PROJ-123-feature-x".to_string(),
            base: "origin/main".to_string(),
            created,
            issue: Some(123),
            frozen: true,
        };

        let mut registry = Registry::default();
        registry.insert("feature-x".to_string(), project).unwrap();
        registry.save(&grove_dir).unwrap();

        let loaded = Registry::load(&grove_dir).unwrap();
        assert_eq!(loaded.projects.len(), 1);
        let loaded_proj = loaded.projects.get("feature-x").unwrap();
        assert_eq!(loaded_proj.branch, "PROJ-123-feature-x");
        assert_eq!(loaded_proj.issue, Some(123));
        assert!(loaded_proj.frozen);
        // timestamps are RFC3339 round-tripped; compare unix seconds
        assert_eq!(
            loaded_proj.created.unix_timestamp(),
            created.unix_timestamp()
        );
    }

    #[test]
    fn duplicate_tag_returns_error() {
        let mut registry = Registry::default();
        registry
            .insert("foo".to_string(), make_project("/c/work/r/foo"))
            .unwrap();
        let err = registry
            .insert("foo".to_string(), make_project("/c/work/r/foo2"))
            .unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateTag(ref t) if t == "foo"));
    }

    #[test]
    fn atomic_save_uses_tmpfile() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");

        let mut registry = Registry::default();
        registry
            .insert("alpha".to_string(), make_project("/c/work/r/alpha"))
            .unwrap();
        registry.save(&grove_dir).unwrap();

        // Verify the final file exists and tmp is gone.
        assert!(grove_dir.join("registry.json").exists());
        assert!(!grove_dir.join("registry.json.tmp").exists());

        // Verify we can load it back.
        let loaded = Registry::load(&grove_dir).unwrap();
        assert_eq!(loaded.projects.len(), 1);
    }

    #[test]
    fn bak_rotation_on_second_save() {
        let dir = TempDir::new().unwrap();
        let grove_dir = dir.path().join(".grove");

        let mut registry = Registry::default();
        registry
            .insert("v1".to_string(), make_project("/c/work/r/v1"))
            .unwrap();
        registry.save(&grove_dir).unwrap();

        // No .bak after the first save (nothing to back up yet).
        // On second save the original becomes .bak.
        registry
            .insert("v2".to_string(), make_project("/c/work/r/v2"))
            .unwrap();
        registry.save(&grove_dir).unwrap();

        let bak_path = grove_dir.join("registry.json.bak");
        assert!(bak_path.exists(), ".bak should exist after second save");

        // The .bak should contain only v1.
        let bak_contents = fs::read_to_string(&bak_path).unwrap();
        let bak_registry: Registry = serde_json::from_str(&bak_contents).unwrap();
        assert_eq!(bak_registry.projects.len(), 1);
        assert!(bak_registry.projects.contains_key("v1"));
    }
}
