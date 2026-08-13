use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::TempDir;

/// Minimal mirror of the registry types — integration test verifies the JSON
/// schema and round-trip behaviour without requiring a library target.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Project {
    path: PathBuf,
    branch: String,
    base: String,
    #[serde(with = "time::serde::rfc3339")]
    created: time::OffsetDateTime,
    #[serde(default)]
    issue: Option<u32>,
    #[serde(default)]
    frozen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registry {
    schema_version: u32,
    projects: BTreeMap<String, Project>,
}

fn save(registry: &Registry, grove_dir: &std::path::Path) {
    std::fs::create_dir_all(grove_dir).unwrap();
    let path = grove_dir.join("registry.json");
    let tmp_path = grove_dir.join("registry.json.tmp");
    let bak_path = grove_dir.join("registry.json.bak");
    if path.exists() {
        std::fs::copy(&path, &bak_path).unwrap();
    }
    let data = serde_json::to_string_pretty(registry).unwrap();
    std::fs::write(&tmp_path, &data).unwrap();
    std::fs::rename(&tmp_path, &path).unwrap();
}

fn load(grove_dir: &std::path::Path) -> Registry {
    let path = grove_dir.join("registry.json");
    if !path.exists() {
        return Registry {
            schema_version: 1,
            projects: BTreeMap::new(),
        };
    }
    let data = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&data).unwrap()
}

#[test]
fn registry_end_to_end_round_trip() {
    let dir = TempDir::new().unwrap();
    let grove_dir = dir.path().join(".grove");

    // Load from non-existent directory — should return empty default.
    let mut registry = load(&grove_dir);
    assert_eq!(registry.schema_version, 1);
    assert!(registry.projects.is_empty());

    let now = time::OffsetDateTime::now_utc();

    registry.projects.insert(
        "alpha".to_string(),
        Project {
            path: PathBuf::from("/c/work/repo/alpha"),
            branch: "PROJ-1-alpha".to_string(),
            base: "origin/main".to_string(),
            created: now,
            issue: Some(1),
            frozen: false,
        },
    );

    registry.projects.insert(
        "beta".to_string(),
        Project {
            path: PathBuf::from("/c/work/repo/beta"),
            branch: "beta".to_string(),
            base: "origin/main".to_string(),
            created: now,
            issue: None,
            frozen: true,
        },
    );

    // Save and reload.
    save(&registry, &grove_dir);
    let loaded = load(&grove_dir);

    assert_eq!(loaded.projects.len(), 2);

    let alpha = loaded.projects.get("alpha").unwrap();
    assert_eq!(alpha.branch, "PROJ-1-alpha");
    assert_eq!(alpha.issue, Some(1));
    assert!(!alpha.frozen);
    assert_eq!(alpha.created.unix_timestamp(), now.unix_timestamp());

    let beta = loaded.projects.get("beta").unwrap();
    assert_eq!(beta.branch, "beta");
    assert!(beta.frozen);
    assert!(beta.issue.is_none());

    // Save again — .bak should appear.
    save(&loaded, &grove_dir);
    assert!(grove_dir.join("registry.json.bak").exists());

    // The .bak should be the previous version (2 projects).
    let bak_data = std::fs::read_to_string(grove_dir.join("registry.json.bak")).unwrap();
    let bak: Value = serde_json::from_str(&bak_data).unwrap();
    assert_eq!(bak["schema_version"], 1);
    assert_eq!(bak["projects"]["alpha"]["branch"], "PROJ-1-alpha");

    // Verify JSON schema shape.
    let json_data = std::fs::read_to_string(grove_dir.join("registry.json")).unwrap();
    let v: Value = serde_json::from_str(&json_data).unwrap();
    assert_eq!(v["schema_version"], 1);
    // RFC3339 timestamp field must be a string.
    assert!(v["projects"]["alpha"]["created"].is_string());
}

/// bd-grove-lifecycle-p0ur.6 AC: a registry.json written by the pre-ephemeral
/// schema (no `expires_at` field at all) must load unchanged under the real
/// `grove::registry::Registry` — this is the schema decision the bead calls
/// for: `expires_at` stays an optional field under `schema_version` 1 rather
/// than bumping to 2, so old data keeps loading. The trade-off (an *older*
/// binary re-saving a registry silently drops `expires_at` on any entry it
/// touches, since `Project` has no `deny_unknown_fields`) is accepted for
/// this single-install machine and is documented on the bead itself.
#[test]
fn pre_ephemeral_schema_registry_loads_unchanged() {
    let dir = TempDir::new().unwrap();
    let grove_dir = dir.path().join(".grove");

    let now = time::OffsetDateTime::now_utc();

    // Write a registry.json using the OLD mirror schema — no `expires_at` key
    // present at all, exactly what a pre-feature grove binary would have
    // written to disk.
    let mut old_registry = Registry {
        schema_version: 1,
        projects: BTreeMap::new(),
    };
    old_registry.projects.insert(
        "legacy".to_string(),
        Project {
            path: PathBuf::from("/c/work/repo/legacy"),
            branch: "PROJ-9-legacy".to_string(),
            base: "origin/main".to_string(),
            created: now,
            issue: Some(9),
            frozen: true,
        },
    );
    save(&old_registry, &grove_dir);

    // Sanity check: the file on disk really has no `expires_at` key.
    let raw = std::fs::read_to_string(grove_dir.join("registry.json")).unwrap();
    assert!(
        !raw.contains("expires_at"),
        "fixture must mirror a pre-feature registry.json with no expires_at key"
    );

    // Load it with the *real* grove registry loader.
    let loaded =
        grove::registry::Registry::load(&grove_dir).expect("old-schema registry should load");
    assert_eq!(loaded.schema_version, 1);
    let legacy = loaded
        .projects
        .get("legacy")
        .expect("legacy project should load");
    assert_eq!(legacy.branch, "PROJ-9-legacy");
    assert_eq!(legacy.base, "origin/main");
    assert_eq!(legacy.issue, Some(9));
    assert!(legacy.frozen);
    assert_eq!(legacy.created.unix_timestamp(), now.unix_timestamp());

    // The missing field defaults to `None` — a project loaded from an old
    // registry is durable (non-ephemeral) by construction.
    assert!(legacy.expires_at.is_none());
    assert!(!legacy.is_ephemeral());
}
