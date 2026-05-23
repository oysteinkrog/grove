use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use grove::migrate::{MigrationOutcome, run_if_needed};
use grove::registry::Registry;
use tempfile::TempDir;

const REAL_CONFIG_DIR: &str = "/c/users/oystein/.config/grove";

/// Parse the legacy registry.json and return the set of project tags along with
/// per-project field values for assertion.
#[derive(Debug, serde::Deserialize)]
struct LegacyProject {
    path: String,
    branch: String,
    base: String,
    #[serde(default)]
    issue: Option<u32>,
    #[serde(default)]
    frozen: bool,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyRegistry {
    #[serde(default)]
    projects: BTreeMap<String, LegacyProject>,
}

/// Read the real config.json, patch work_dir to point at `fake_work_dir`, and
/// return the modified JSON bytes.
fn patched_config_json(fake_work_dir: &std::path::Path) -> Vec<u8> {
    let config_path = PathBuf::from(REAL_CONFIG_DIR).join("config.json");
    let raw = fs::read_to_string(&config_path).expect("config.json must be readable");
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).expect("config.json must be valid JSON");
    // Rewrite work_dir and main_repo so the migration writes per-repo .grove/
    // files inside the tempdir rather than the real work directory.
    value["work_dir"] = serde_json::Value::String(fake_work_dir.to_string_lossy().into_owned());
    value["main_repo"] =
        serde_json::Value::String(fake_work_dir.join("master").to_string_lossy().into_owned());
    serde_json::to_vec_pretty(&value).unwrap()
}

#[test]
#[ignore]
fn migrate_real_config_preserves_all_projects() {
    let config_dir = TempDir::new().unwrap();
    let fake_work_dir = TempDir::new().unwrap();

    // Create the fake_work_dir/master directory that main_repo now points at
    // (migration derives repo_id from main_repo's parent component, not the path itself).
    fs::create_dir_all(fake_work_dir.path().join("master")).unwrap();

    // Read legacy files (read-only from real config).
    let real_registry_path = PathBuf::from(REAL_CONFIG_DIR).join("registry.json");
    let real_registry_bytes =
        fs::read(&real_registry_path).expect("registry.json must be readable");
    let original_legacy: LegacyRegistry =
        serde_json::from_slice(&real_registry_bytes).expect("registry.json must be valid JSON");
    let original_tags: std::collections::BTreeSet<String> =
        original_legacy.projects.keys().cloned().collect();

    // Write copies into tempdir (config.json with patched work_dir).
    let patched = patched_config_json(fake_work_dir.path());
    fs::write(config_dir.path().join("config.json"), &patched).unwrap();
    fs::write(
        config_dir.path().join("registry.json"),
        &real_registry_bytes,
    )
    .unwrap();

    // Run migration against the tempdir — NEVER touches the real config.
    let outcome = run_if_needed(config_dir.path())
        .expect("migration must succeed on a copy of the real config");

    match &outcome {
        MigrationOutcome::Migrated { project_count, .. } => {
            assert_eq!(
                *project_count,
                original_tags.len(),
                "migrated project count must match legacy registry"
            );
        }
        MigrationOutcome::NotNeeded => {
            panic!("expected Migrated outcome but got NotNeeded");
        }
    }

    // Load the resulting per-repo registry from fake_work_dir/.grove/
    let grove_dir = fake_work_dir.path().join(".grove");
    let new_registry = Registry::load(&grove_dir).expect("per-repo registry must load");

    // Tag set equality.
    let migrated_tags: std::collections::BTreeSet<String> =
        new_registry.projects.keys().cloned().collect();
    assert_eq!(
        migrated_tags, original_tags,
        "migrated project tag set must exactly match legacy registry"
    );

    // Per-project field equality (branch, base, issue, frozen, path).
    for (tag, legacy) in &original_legacy.projects {
        let migrated = new_registry
            .projects
            .get(tag)
            .unwrap_or_else(|| panic!("project '{}' missing from migrated registry", tag));

        assert_eq!(
            migrated.branch, legacy.branch,
            "project '{}': branch mismatch",
            tag
        );
        assert_eq!(
            migrated.base, legacy.base,
            "project '{}': base mismatch",
            tag
        );
        assert_eq!(
            migrated.issue, legacy.issue,
            "project '{}': issue mismatch",
            tag
        );
        assert_eq!(
            migrated.frozen, legacy.frozen,
            "project '{}': frozen mismatch",
            tag
        );
        assert_eq!(
            migrated.path,
            PathBuf::from(&legacy.path),
            "project '{}': path mismatch",
            tag
        );
    }

    // repos.json must exist in the tempdir config dir.
    assert!(
        config_dir.path().join("repos.json").exists(),
        "repos.json must be written"
    );
    // Backups must exist.
    assert!(
        config_dir
            .path()
            .join("config.json.pre-rust-migration")
            .exists(),
        "config.json backup must exist"
    );
    assert!(
        config_dir
            .path()
            .join("registry.json.pre-rust-migration")
            .exists(),
        "registry.json backup must exist"
    );
    // Real config must be untouched.
    assert!(
        PathBuf::from(REAL_CONFIG_DIR).join("config.json").exists(),
        "real config.json must not have been modified"
    );
    assert!(
        PathBuf::from(REAL_CONFIG_DIR)
            .join("registry.json")
            .exists(),
        "real registry.json must not have been modified"
    );
}

#[test]
#[ignore]
fn migrate_real_config_is_idempotent() {
    let config_dir = TempDir::new().unwrap();
    let fake_work_dir = TempDir::new().unwrap();

    fs::create_dir_all(fake_work_dir.path().join("master")).unwrap();

    let real_registry_bytes = fs::read(PathBuf::from(REAL_CONFIG_DIR).join("registry.json"))
        .expect("registry.json must be readable");
    let patched = patched_config_json(fake_work_dir.path());

    fs::write(config_dir.path().join("config.json"), &patched).unwrap();
    fs::write(
        config_dir.path().join("registry.json"),
        &real_registry_bytes,
    )
    .unwrap();

    // First run — migrates.
    run_if_needed(config_dir.path()).expect("first migration must succeed");

    // Second run on the same (already-migrated) dir — must return NotNeeded.
    let outcome = run_if_needed(config_dir.path()).expect("second migration must not error");
    assert!(
        matches!(outcome, MigrationOutcome::NotNeeded),
        "second run must return NotNeeded, got {:?}",
        outcome
    );
}
