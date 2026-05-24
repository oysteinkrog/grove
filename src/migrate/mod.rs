use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use crate::config::global::{LaunchOverride, RepoEntry, ReposManifest};
use crate::registry::{Project, Registry};

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to rename {from} to {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("legacy config missing required field: {0}")]
    MissingField(String),
}

#[derive(Debug)]
pub enum MigrationOutcome {
    NotNeeded,
    Migrated {
        repo_id: String,
        project_count: usize,
    },
}

#[derive(Debug, Deserialize)]
struct LegacyConfig {
    main_repo: String,
    work_dir: String,
    #[serde(default)]
    dir_prefix: String,
    #[serde(default = "default_upstream")]
    upstream_remote: String,
    #[serde(default = "default_fork")]
    fork_remote: String,
    #[serde(default = "default_base")]
    default_base: String,
    #[serde(default)]
    launch: Option<serde_json::Value>,
}

fn default_upstream() -> String {
    "if".to_string()
}
fn default_fork() -> String {
    "my".to_string()
}
fn default_base() -> String {
    "master".to_string()
}

#[derive(Debug, Deserialize)]
struct LegacyProject {
    path: String,
    branch: String,
    base: String,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    issue: Option<u32>,
    #[serde(default)]
    frozen: bool,
    #[serde(default)]
    freeze_expires: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyRegistry {
    #[serde(default)]
    projects: BTreeMap<String, LegacyProject>,
}

/// Parse a naive ISO timestamp (e.g. "2026-02-01T10:51:47") as local time and
/// convert to UTC. Returns `OffsetDateTime::now_utc()` on any parse failure.
fn parse_naive_timestamp(s: &str, tag: &str) -> OffsetDateTime {
    let fmt = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    match PrimitiveDateTime::parse(s, &fmt) {
        Ok(naive) => {
            // Attempt to get the local UTC offset; fall back to UTC on failure.
            let offset =
                UtcOffset::local_offset_at(OffsetDateTime::now_utc()).unwrap_or(UtcOffset::UTC);
            naive.assume_offset(offset).to_offset(UtcOffset::UTC)
        }
        Err(_) => {
            // Try parsing as an already-offset timestamp (RFC3339).
            if let Ok(odt) = OffsetDateTime::parse(s, &Rfc3339) {
                return odt.to_offset(UtcOffset::UTC);
            }
            eprintln!(
                "grove migrate: could not parse timestamp {:?} for project {:?}; using now",
                s, tag
            );
            OffsetDateTime::now_utc()
        }
    }
}

fn build_launch_override(launch_value: Option<&serde_json::Value>) -> Option<LaunchOverride> {
    let obj = launch_value?.as_object()?;
    let terminal = obj
        .get("terminal")
        .and_then(|v| v.as_str())
        .map(String::from);
    let wezterm_path = obj
        .get("wezterm_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let shell_command = obj
        .get("shell_command")
        .and_then(|v| v.as_str())
        .map(String::from);
    if terminal.is_none() && wezterm_path.is_none() && shell_command.is_none() {
        return None;
    }
    Some(LaunchOverride {
        terminal,
        wezterm_path,
        shell_command,
    })
}

fn derive_repo_id(main_repo: &str) -> String {
    // "/c/work/desktop/master" -> "desktop"
    // walk backwards: skip the last component, take the one before it.
    let p = Path::new(main_repo);
    p.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("desktop")
        .to_string()
}

fn atomic_write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), MigrationError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| MigrationError::CreateDir {
        path: parent.to_path_buf(),
        source: e,
    })?;
    let tmp_path = path.with_extension("json.migrating-tmp");
    let data = serde_json::to_string_pretty(value).map_err(|e| MigrationError::Json {
        path: path.to_path_buf(),
        source: e,
    })?;
    fs::write(&tmp_path, &data).map_err(|e| MigrationError::Write {
        path: tmp_path.clone(),
        source: e,
    })?;
    fs::rename(&tmp_path, path).map_err(|e| MigrationError::Rename {
        from: tmp_path,
        to: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

fn read_json_file<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T, MigrationError> {
    let data = fs::read_to_string(path).map_err(|e| MigrationError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    serde_json::from_str(&data).map_err(|e| MigrationError::Json {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Run migration if needed.
///
/// Checks for legacy `config.json` / `registry.json` in `config_dir` and
/// migrates to the new hybrid layout. Returns `NotNeeded` immediately when
/// `repos.json` already exists, or when neither legacy file is present.
pub fn run_if_needed(config_dir: &Path) -> Result<MigrationOutcome, MigrationError> {
    let repos_json = config_dir.join("repos.json");
    if repos_json.exists() {
        return Ok(MigrationOutcome::NotNeeded);
    }

    let legacy_config_path = config_dir.join("config.json");
    let legacy_registry_path = config_dir.join("registry.json");

    if !legacy_config_path.exists() && !legacy_registry_path.exists() {
        return Ok(MigrationOutcome::NotNeeded);
    }

    eprintln!("grove: migrating legacy config to multi-repo format...");

    // Partial state: only registry present — write a minimal manifest with defaults.
    // The default work_dir/main_repo can be overridden via env vars so tests can
    // exercise this branch without writing into the host filesystem.
    let legacy_cfg: LegacyConfig = if legacy_config_path.exists() {
        read_json_file(&legacy_config_path)?
    } else {
        eprintln!(
            "grove migrate: config.json not found; writing manifest with defaults (please review)"
        );
        let default_work_dir = std::env::var("GROVE_MIGRATE_DEFAULT_WORK_DIR")
            .unwrap_or_else(|_| "/c/work/desktop".to_string());
        let default_main_repo = std::env::var("GROVE_MIGRATE_DEFAULT_MAIN_REPO")
            .unwrap_or_else(|_| format!("{default_work_dir}/master"));
        LegacyConfig {
            main_repo: default_main_repo,
            work_dir: default_work_dir,
            dir_prefix: String::new(),
            upstream_remote: "if".to_string(),
            fork_remote: "my".to_string(),
            default_base: "master".to_string(),
            launch: None,
        }
    };

    let repo_id = derive_repo_id(&legacy_cfg.main_repo);
    let work_dir = PathBuf::from(&legacy_cfg.work_dir);

    // Build the per-repo registry from legacy projects.
    let mut new_registry = Registry::default();
    let project_count;

    if legacy_registry_path.exists() {
        let legacy_reg: LegacyRegistry = read_json_file(&legacy_registry_path)?;
        project_count = legacy_reg.projects.len();
        for (tag, lp) in legacy_reg.projects {
            let created = match &lp.created {
                Some(ts) => parse_naive_timestamp(ts, &tag),
                None => OffsetDateTime::now_utc(),
            };
            let project = Project {
                path: PathBuf::from(&lp.path),
                branch: lp.branch,
                base: lp.base,
                created,
                issue: lp.issue,
                frozen: lp.frozen,
            };
            // Ignore duplicate errors — registry shouldn't have duplicates but be safe.
            let _ = new_registry.insert(tag, project);
        }
    } else {
        project_count = 0;
        eprintln!("grove migrate: registry.json not found; starting with empty registry");
    }

    // Build the global manifest.
    let launch_override = build_launch_override(legacy_cfg.launch.as_ref());
    let repo_entry = RepoEntry {
        main_repo: PathBuf::from(&legacy_cfg.main_repo),
        work_dir: work_dir.clone(),
        dir_prefix: legacy_cfg.dir_prefix,
        upstream_remote: legacy_cfg.upstream_remote,
        fork_remote: legacy_cfg.fork_remote,
        default_base: legacy_cfg.default_base,
        issue_prefix: Some("DESKTOP".to_string()),
        launch: launch_override,
    };

    let mut repos = std::collections::BTreeMap::new();
    repos.insert(repo_id.clone(), repo_entry);
    let manifest = ReposManifest {
        default_repo: Some(repo_id.clone()),
        repos,
        ..ReposManifest::default()
    };

    // Write per-repo .grove/ files.
    let grove_dir = work_dir.join(".grove");
    fs::create_dir_all(&grove_dir).map_err(|e| MigrationError::CreateDir {
        path: grove_dir.clone(),
        source: e,
    })?;
    let registry_dest = grove_dir.join("registry.json");
    new_registry
        .save(&grove_dir)
        .map_err(|e| MigrationError::Write {
            path: registry_dest.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;

    // Write global repos.json.
    atomic_write_json(&repos_json, &manifest)?;

    // Back up the originals (rename, not delete).
    if legacy_config_path.exists() {
        let backup = config_dir.join("config.json.pre-rust-migration");
        fs::rename(&legacy_config_path, &backup).map_err(|e| MigrationError::Rename {
            from: legacy_config_path.clone(),
            to: backup,
            source: e,
        })?;
    }
    if legacy_registry_path.exists() {
        let backup = config_dir.join("registry.json.pre-rust-migration");
        fs::rename(&legacy_registry_path, &backup).map_err(|e| MigrationError::Rename {
            from: legacy_registry_path.clone(),
            to: backup,
            source: e,
        })?;
    }

    eprintln!("  repo id:   {}", repo_id);
    eprintln!(
        "  registry:  {}  ({} projects)",
        grove_dir.join("registry.json").display(),
        project_count
    );
    eprintln!("  manifest:  {}", repos_json.display());
    eprintln!("  backups:   config.json.pre-rust-migration, registry.json.pre-rust-migration");
    eprintln!("grove: migration complete.");

    Ok(MigrationOutcome::Migrated {
        repo_id,
        project_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_legacy_config(work_dir: &Path) -> serde_json::Value {
        serde_json::json!({
            "main_repo": work_dir.join("master").to_string_lossy(),
            "work_dir": work_dir.to_string_lossy(),
            "dir_prefix": "",
            "upstream_remote": "if",
            "fork_remote": "my",
            "default_base": "master",
            "launch": {
                "terminal": "wt",
                "wezterm_path": null,
                "shell_command": "fish -l"
            }
        })
    }

    fn make_legacy_registry(n: usize) -> serde_json::Value {
        let mut projects = serde_json::Map::new();
        for i in 0..n {
            let tag = format!("proj-{}", i);
            projects.insert(
                tag.clone(),
                serde_json::json!({
                    "path": format!("/c/work/desktop/{}", tag),
                    "branch": format!("DESKTOP-{}-{}", 1000 + i, tag),
                    "base": "if/master",
                    "created": "2026-02-01T10:51:47",
                    "issue": i as u32,
                    "frozen": false
                }),
            );
        }
        serde_json::json!({ "projects": projects })
    }

    #[test]
    fn migration_runs_when_legacy_present() {
        let config_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();

        let cfg = make_legacy_config(work_dir.path());
        fs::write(
            config_dir.path().join("config.json"),
            serde_json::to_string(&cfg).unwrap(),
        )
        .unwrap();
        fs::write(
            config_dir.path().join("registry.json"),
            serde_json::to_string(&make_legacy_registry(3)).unwrap(),
        )
        .unwrap();

        let outcome = run_if_needed(config_dir.path()).unwrap();
        assert!(
            matches!(
                &outcome,
                MigrationOutcome::Migrated {
                    project_count: 3,
                    ..
                }
            ),
            "expected Migrated with 3 projects, got {:?}",
            outcome
        );

        // repos.json must exist.
        assert!(config_dir.path().join("repos.json").exists());
        // Backups must exist.
        assert!(
            config_dir
                .path()
                .join("config.json.pre-rust-migration")
                .exists()
        );
        assert!(
            config_dir
                .path()
                .join("registry.json.pre-rust-migration")
                .exists()
        );
        // Originals must be gone.
        assert!(!config_dir.path().join("config.json").exists());
        assert!(!config_dir.path().join("registry.json").exists());
        // Per-repo registry must be in work_dir/.grove/
        let grove_dir = work_dir.path().join(".grove");
        assert!(grove_dir.join("registry.json").exists());

        // Verify repo entry has issue_prefix = DESKTOP
        let manifest: ReposManifest = serde_json::from_str(
            &fs::read_to_string(config_dir.path().join("repos.json")).unwrap(),
        )
        .unwrap();
        let (_, entry) = manifest.repos.iter().next().unwrap();
        assert_eq!(entry.issue_prefix, Some("DESKTOP".to_string()));
    }

    #[test]
    fn already_migrated_returns_not_needed() {
        let config_dir = TempDir::new().unwrap();
        // Write repos.json to simulate already-migrated state.
        fs::write(
            config_dir.path().join("repos.json"),
            r#"{"schema_version":1,"repos":{}}"#,
        )
        .unwrap();

        let outcome = run_if_needed(config_dir.path()).unwrap();
        assert!(
            matches!(outcome, MigrationOutcome::NotNeeded),
            "expected NotNeeded, got {:?}",
            outcome
        );
    }

    #[test]
    fn no_legacy_files_returns_not_needed() {
        let config_dir = TempDir::new().unwrap();
        let outcome = run_if_needed(config_dir.path()).unwrap();
        assert!(
            matches!(outcome, MigrationOutcome::NotNeeded),
            "expected NotNeeded, got {:?}",
            outcome
        );
    }

    #[test]
    #[serial_test::serial]
    fn only_registry_present_partial_state() {
        // config.json absent but registry.json present → migrate with defaults.
        // The defaults branch normally hardcodes /c/work/desktop which would
        // clobber the host registry on a developer machine; redirect both
        // default work_dir and main_repo into a tempdir via env vars.
        let config_dir = TempDir::new().unwrap();
        let fake_work_dir = TempDir::new().unwrap();
        fs::create_dir_all(fake_work_dir.path().join("master")).unwrap();

        fs::write(
            config_dir.path().join("registry.json"),
            serde_json::to_string(&make_legacy_registry(2)).unwrap(),
        )
        .unwrap();

        // SAFETY: env-mutation is serialized via serial_test.
        unsafe {
            std::env::set_var(
                "GROVE_MIGRATE_DEFAULT_WORK_DIR",
                fake_work_dir.path().to_string_lossy().as_ref(),
            );
            std::env::set_var(
                "GROVE_MIGRATE_DEFAULT_MAIN_REPO",
                fake_work_dir
                    .path()
                    .join("master")
                    .to_string_lossy()
                    .as_ref(),
            );
        }

        let result = run_if_needed(config_dir.path());

        unsafe {
            std::env::remove_var("GROVE_MIGRATE_DEFAULT_WORK_DIR");
            std::env::remove_var("GROVE_MIGRATE_DEFAULT_MAIN_REPO");
        }

        // Must succeed — defaults now point at a writable tempdir.
        let outcome =
            result.expect("partial-state migration must succeed with redirected defaults");
        assert!(
            matches!(
                outcome,
                MigrationOutcome::Migrated {
                    project_count: 2,
                    ..
                }
            ),
            "expected Migrated with 2 projects, got {:?}",
            outcome
        );

        // Host registry must NOT have been touched.
        let host_registry = std::path::PathBuf::from("/c/work/desktop/.grove/registry.json");
        if host_registry.exists() {
            let contents = fs::read_to_string(&host_registry).unwrap_or_default();
            assert!(
                !contents.contains("proj-0") || !contents.contains("DESKTOP-1000-proj-0"),
                "host registry at {} was clobbered with fixture data",
                host_registry.display()
            );
        }
    }

    #[test]
    fn malformed_legacy_json_returns_error() {
        let config_dir = TempDir::new().unwrap();
        fs::write(config_dir.path().join("config.json"), b"not json at all").unwrap();

        let result = run_if_needed(config_dir.path());
        assert!(result.is_err(), "expected error on malformed JSON");
        let err = result.unwrap_err();
        assert!(
            matches!(err, MigrationError::Json { .. }),
            "expected Json error, got: {:?}",
            err
        );
    }

    #[test]
    fn n_project_round_trip_preservation() {
        let config_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        const N: usize = 7;

        fs::write(
            config_dir.path().join("config.json"),
            serde_json::to_string(&make_legacy_config(work_dir.path())).unwrap(),
        )
        .unwrap();
        fs::write(
            config_dir.path().join("registry.json"),
            serde_json::to_string(&make_legacy_registry(N)).unwrap(),
        )
        .unwrap();

        let outcome = run_if_needed(config_dir.path()).unwrap();
        assert!(matches!(
            &outcome,
            MigrationOutcome::Migrated { project_count, .. } if *project_count == N
        ));

        let grove_dir = work_dir.path().join(".grove");
        let new_registry = Registry::load(&grove_dir).unwrap();
        assert_eq!(
            new_registry.projects.len(),
            N,
            "migrated registry must contain exactly N projects"
        );

        // Verify all tags are present and core fields intact.
        for i in 0..N {
            let tag = format!("proj-{}", i);
            let proj = new_registry.projects.get(&tag).expect("project must exist");
            assert_eq!(proj.branch, format!("DESKTOP-{}-{}", 1000 + i, tag));
            assert_eq!(proj.base, "if/master");
            assert_eq!(proj.issue, Some(i as u32));
            assert!(!proj.frozen);
        }
    }
}
