use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::Serialize;
use strsim::jaro_winkler;

use crate::config::global::{RepoEntry, ReposManifest};
use crate::display::{make_header_cell, make_table};
use crate::error::GroveError;
use crate::registry::Registry;
use crate::repo::RepoContext;

pub enum RepoSubcommand {
    Path { default: bool },
    Add(AddArgs),
    List { json: bool },
    Show { id: String },
    Remove { id: String, force: bool },
    Default { id: String },
}

pub struct AddArgs {
    pub path: PathBuf,
    pub id: Option<String>,
    pub issue_prefix: Option<String>,
    pub upstream: Option<String>,
    pub fork: Option<String>,
    pub default_base: Option<String>,
    pub make_default: bool,
    pub config_dir: PathBuf,
}

pub struct RepoArgs {
    pub subcommand: RepoSubcommand,
}

/// Return the string to print for `grove repo path --default`.
pub fn render(args: &RepoArgs, cx: &RepoContext) -> Result<String> {
    match &args.subcommand {
        RepoSubcommand::Path { default: true } => {
            let repo_id =
                cx.global.default_repo.as_deref().ok_or_else(|| {
                    anyhow!("no default repo set; set default_repo in repos.json")
                })?;
            let entry = cx
                .global
                .repos
                .get(repo_id)
                .ok_or_else(|| anyhow!("default repo '{}' not found in repos.json", repo_id))?;
            Ok(entry.work_dir.to_string_lossy().into_owned())
        }
        RepoSubcommand::Path { default: false } => {
            Ok(cx.resolved.work_dir.to_string_lossy().into_owned())
        }
        RepoSubcommand::Add(_) => Err(anyhow!("use run() for add subcommand")),
        _ => Err(anyhow!("use run() for this subcommand")),
    }
}

pub fn run(args: &RepoArgs, cx: &RepoContext) -> Result<()> {
    match &args.subcommand {
        RepoSubcommand::Add(add_args) => run_add(add_args),
        RepoSubcommand::List { json } => run_list(&cx.global, *json),
        RepoSubcommand::Show { id } => run_show(&cx.global, id),
        RepoSubcommand::Remove { id, force } => run_remove(&cx.global, id, *force),
        RepoSubcommand::Default { id } => run_default(&cx.global, id),
        _ => {
            let output = render(args, cx)?;
            println!("{output}");
            Ok(())
        }
    }
}

fn is_git_repo(path: &Path) -> bool {
    // Fast check: presence of .git entry. Works for both regular repos and worktrees.
    if path.join(".git").exists() {
        return true;
    }
    // Fallback: try gix discovery (handles bare repos and edge cases).
    gix::discover(path).is_ok()
}

pub fn run_add(args: &AddArgs) -> Result<()> {
    let path = args
        .path
        .canonicalize()
        .map_err(|e| anyhow!("cannot resolve path '{}': {e}", args.path.display()))?;

    if !is_git_repo(&path) {
        return Err(GroveError::NotAGitRepo { path }.into());
    }

    let id = match &args.id {
        Some(id) => id.clone(),
        None => path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("cannot derive repo id from path '{}'", path.display()))?,
    };

    let mut manifest = ReposManifest::load(&args.config_dir)
        .map_err(|e| anyhow!("failed to load repos.json: {e}"))?;

    if manifest.repos.contains_key(&id) {
        return Err(GroveError::DuplicateRepoId { id }.into());
    }

    let upstream_remote = args
        .upstream
        .clone()
        .unwrap_or_else(|| "upstream".to_string());
    let fork_remote = args.fork.clone().unwrap_or_else(|| "origin".to_string());
    let default_base = args
        .default_base
        .clone()
        .unwrap_or_else(|| "main".to_string());

    let entry = RepoEntry {
        main_repo: path.clone(),
        work_dir: path.clone(),
        dir_prefix: String::new(),
        upstream_remote,
        fork_remote,
        default_base,
        issue_prefix: args.issue_prefix.clone(),
        launch: None,
    };

    manifest.repos.insert(id.clone(), entry);

    if args.make_default {
        manifest.default_repo = Some(id.clone());
    }

    manifest
        .save(&args.config_dir)
        .map_err(|e| anyhow!("failed to save repos.json: {e}"))?;

    // Create .grove/ directory and empty registry.json under work_dir.
    let grove_dir = path.join(".grove");
    let empty_registry = Registry::default();
    empty_registry
        .save(&grove_dir)
        .map_err(|e| anyhow!("failed to create .grove/registry.json: {e}"))?;

    println!("Added repo '{id}' at {}", path.display());
    Ok(())
}

// ── JSON structs for `grove repo list --json` ────────────────────────────────

#[derive(Serialize)]
struct JsonRepoList {
    version: u32,
    repos: Vec<JsonRepoRow>,
}

#[derive(Serialize)]
struct JsonRepoRow {
    id: String,
    path: String,
    default_base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_prefix: Option<String>,
    is_default: bool,
}

// ── repo list ────────────────────────────────────────────────────────────────

pub fn run_list(manifest: &ReposManifest, json: bool) -> Result<()> {
    if json {
        let repos: Vec<JsonRepoRow> = manifest
            .repos
            .iter()
            .map(|(id, entry)| JsonRepoRow {
                id: id.clone(),
                path: entry.work_dir.display().to_string(),
                default_base: entry.default_base.clone(),
                issue_prefix: entry.issue_prefix.clone(),
                is_default: manifest.default_repo.as_deref() == Some(id.as_str()),
            })
            .collect();
        let out = JsonRepoList { version: 1, repos };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let mut table = make_table();
    table.set_header(vec![
        make_header_cell("ID"),
        make_header_cell("Path"),
        make_header_cell("Default Base"),
        make_header_cell("Issue Prefix"),
        make_header_cell("Default"),
    ]);

    for (id, entry) in &manifest.repos {
        let is_default = manifest.default_repo.as_deref() == Some(id.as_str());
        table.add_row(vec![
            id.clone(),
            entry.work_dir.display().to_string(),
            entry.default_base.clone(),
            entry.issue_prefix.clone().unwrap_or_default(),
            if is_default { "*" } else { "" }.to_string(),
        ]);
    }

    println!("{table}");
    Ok(())
}

// ── repo show ────────────────────────────────────────────────────────────────

pub fn run_show(manifest: &ReposManifest, id: &str) -> Result<()> {
    let entry = manifest
        .repos
        .get(id)
        .ok_or_else(|| GroveError::RepoIdNotFound {
            id: id.to_string(),
            hint: suggest_repo_id(manifest, id),
        })?;

    let project_count = count_projects(entry);
    let is_default = manifest.default_repo.as_deref() == Some(id);

    println!("id:           {id}");
    println!("path:         {}", entry.work_dir.display());
    println!("default_base: {}", entry.default_base);
    println!(
        "issue_prefix: {}",
        entry.issue_prefix.as_deref().unwrap_or("(none)")
    );
    println!("is_default:   {is_default}");
    println!("projects:     {project_count}");
    Ok(())
}

// ── repo remove ──────────────────────────────────────────────────────────────

pub fn run_remove(manifest: &ReposManifest, id: &str, _force: bool) -> Result<()> {
    if !manifest.repos.contains_key(id) {
        return Err(GroveError::RepoIdNotFound {
            id: id.to_string(),
            hint: suggest_repo_id(manifest, id),
        }
        .into());
    }
    // Validation only; actual write requires config_dir from CLI layer.
    Ok(())
}

pub fn run_remove_with_config(
    manifest: &mut ReposManifest,
    config_dir: &Path,
    id: &str,
    force: bool,
) -> Result<()> {
    let entry = manifest
        .repos
        .get(id)
        .ok_or_else(|| GroveError::RepoIdNotFound {
            id: id.to_string(),
            hint: suggest_repo_id(manifest, id),
        })?;

    if !force {
        let project_count = count_projects(entry);
        if project_count > 0 {
            return Err(GroveError::RepoNotEmpty { id: id.to_string() }.into());
        }
    }

    manifest.repos.remove(id);
    if manifest.default_repo.as_deref() == Some(id) {
        manifest.default_repo = None;
    }

    manifest
        .save(config_dir)
        .map_err(|e| anyhow!("failed to save repos.json: {e}"))?;

    println!("Removed repo '{id}'");
    Ok(())
}

// ── repo default ─────────────────────────────────────────────────────────────

pub fn run_default(manifest: &ReposManifest, id: &str) -> Result<()> {
    if !manifest.repos.contains_key(id) {
        return Err(GroveError::RepoIdNotFound {
            id: id.to_string(),
            hint: suggest_repo_id(manifest, id),
        }
        .into());
    }
    // Validation only; actual write requires config_dir from CLI layer.
    Ok(())
}

pub fn run_default_with_config(
    manifest: &mut ReposManifest,
    config_dir: &Path,
    id: &str,
) -> Result<()> {
    if !manifest.repos.contains_key(id) {
        return Err(GroveError::RepoIdNotFound {
            id: id.to_string(),
            hint: suggest_repo_id(manifest, id),
        }
        .into());
    }

    manifest.default_repo = Some(id.to_string());
    manifest
        .save(config_dir)
        .map_err(|e| anyhow!("failed to save repos.json: {e}"))?;

    println!("Default repo set to '{id}'");
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn count_projects(entry: &RepoEntry) -> usize {
    let grove_dir = entry.work_dir.join(".grove");
    Registry::load(&grove_dir)
        .map(|r| r.projects.len())
        .unwrap_or(0)
}

fn suggest_repo_id(manifest: &ReposManifest, id: &str) -> Option<String> {
    let mut best: Option<(f64, String)> = None;
    for candidate in manifest.repos.keys() {
        let score = jaro_winkler(id, candidate);
        if score >= 0.8 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, candidate.clone()));
        }
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use serial_test::serial;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    use super::*;
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

    fn make_context_with_default(default_repo: Option<&str>, work_dir: &str) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "myrepo".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/myrepo/master"),
                work_dir: PathBuf::from(work_dir),
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
            default_repo: default_repo.map(|s| s.to_string()),
            repos,
        };
        let resolved = crate::config::ResolvedConfig {
            main_repo: PathBuf::from("/c/work/myrepo/master"),
            work_dir: PathBuf::from(work_dir),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let registry = Registry {
            schema_version: 1,
            projects: {
                let mut m = BTreeMap::new();
                m.insert("feat".to_string(), make_project("/c/work/myrepo/feat"));
                m
            },
        };
        RepoContext {
            id: "myrepo".to_string(),
            global,
            resolved,
            registry,
        }
    }

    // AC4: grove repo path --default prints default repo work_dir
    #[test]
    fn repo_path_default_prints_work_dir() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let result = render(&args, &cx).unwrap();
        assert_eq!(result, "/c/work/myrepo");
    }

    // AC4: no default_repo set → error
    #[test]
    fn repo_path_default_no_default_repo_errors() {
        let cx = make_context_with_default(None, "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let err = render(&args, &cx).unwrap_err();
        assert!(
            err.to_string().contains("no default repo"),
            "expected 'no default repo' error: {}",
            err
        );
    }

    // Path output has no decoration
    #[test]
    fn repo_path_no_decoration() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/my-repo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: true },
        };
        let result = render(&args, &cx).unwrap();
        assert!(!result.contains('\n'));
        assert_eq!(result, "/c/work/my-repo");
    }

    // grove repo path (no --default) prints current repo work_dir
    #[test]
    fn repo_path_no_flag_prints_current_repo_work_dir() {
        let cx = make_context_with_default(Some("myrepo"), "/c/work/myrepo");
        let args = RepoArgs {
            subcommand: RepoSubcommand::Path { default: false },
        };
        let result = render(&args, &cx).unwrap();
        assert_eq!(result, "/c/work/myrepo");
    }

    // ── grove-4pe.2 tests ────────────────────────────────────────────────────

    fn make_two_repo_manifest(tmp: &TempDir) -> (ReposManifest, PathBuf) {
        let config_dir = tmp.path().join("config");
        let work_dir_a = tmp.path().join("repo-a");
        let work_dir_b = tmp.path().join("repo-b");
        fs::create_dir_all(&work_dir_a).unwrap();
        fs::create_dir_all(&work_dir_b).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert(
            "repo-a".to_string(),
            RepoEntry {
                main_repo: work_dir_a.join("master"),
                work_dir: work_dir_a,
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
                fork_remote: "origin".to_string(),
                default_base: "main".to_string(),
                issue_prefix: Some("DESK".to_string()),
                launch: None,
            },
        );
        repos.insert(
            "repo-b".to_string(),
            RepoEntry {
                main_repo: work_dir_b.join("master"),
                work_dir: work_dir_b,
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
                fork_remote: "origin".to_string(),
                default_base: "develop".to_string(),
                issue_prefix: None,
                launch: None,
            },
        );
        let manifest = ReposManifest {
            schema_version: 1,
            default_repo: Some("repo-a".to_string()),
            repos,
        };
        (manifest, config_dir)
    }

    // Test 1: repo list → outputs all registered repos
    #[test]
    #[serial]
    fn repo_list_outputs_all_repos() {
        let tmp = TempDir::new().unwrap();
        let (manifest, _) = make_two_repo_manifest(&tmp);

        // Table output should not error
        assert!(run_list(&manifest, false).is_ok());

        // JSON output should contain both repos
        let repos: Vec<JsonRepoRow> = manifest
            .repos
            .iter()
            .map(|(id, entry)| JsonRepoRow {
                id: id.clone(),
                path: entry.work_dir.display().to_string(),
                default_base: entry.default_base.clone(),
                issue_prefix: entry.issue_prefix.clone(),
                is_default: manifest.default_repo.as_deref() == Some(id.as_str()),
            })
            .collect();
        let json_out = JsonRepoList { version: 1, repos };
        let json = serde_json::to_string_pretty(&json_out).unwrap();
        assert!(json.contains("repo-a"), "repo-a should appear in JSON list");
        assert!(json.contains("repo-b"), "repo-b should appear in JSON list");
        assert!(
            json.contains("\"is_default\": true"),
            "repo-a should be marked as default"
        );
    }

    // Test 2: repo show <id> → prints entry + project count
    #[test]
    #[serial]
    fn repo_show_prints_entry_and_project_count() {
        let tmp = TempDir::new().unwrap();
        let (manifest, _) = make_two_repo_manifest(&tmp);

        // With no .grove/registry.json present, count_projects returns 0.
        assert!(run_show(&manifest, "repo-a").is_ok());

        // Verify count_projects reflects actual project count
        let entry = manifest.repos.get("repo-a").unwrap();
        assert_eq!(count_projects(entry), 0, "no projects yet");

        // Add a project and re-check
        let grove_dir = entry.work_dir.join(".grove");
        let mut registry = Registry::default();
        registry
            .insert(
                "feat".to_string(),
                make_project(&entry.work_dir.join("feat").display().to_string()),
            )
            .unwrap();
        registry.save(&grove_dir).unwrap();
        assert_eq!(count_projects(entry), 1, "one project added");
    }

    // Test 3: repo remove <id> with no projects → succeeds
    #[test]
    #[serial]
    fn repo_remove_no_projects_succeeds() {
        let tmp = TempDir::new().unwrap();
        let (mut manifest, config_dir) = make_two_repo_manifest(&tmp);
        fs::create_dir_all(&config_dir).unwrap();
        manifest.save(&config_dir).unwrap();

        let result = run_remove_with_config(&mut manifest, &config_dir, "repo-b", false);
        assert!(
            result.is_ok(),
            "remove should succeed with no projects: {result:?}"
        );
        assert!(
            !manifest.repos.contains_key("repo-b"),
            "repo-b should be removed"
        );

        let reloaded = ReposManifest::load(&config_dir).unwrap();
        assert!(
            !reloaded.repos.contains_key("repo-b"),
            "repo-b should be gone from disk"
        );
    }

    // Test 4: repo remove <id> with projects → errors RepoNotEmpty unless --force
    #[test]
    #[serial]
    fn repo_remove_with_projects_errors_without_force() {
        let tmp = TempDir::new().unwrap();
        let (mut manifest, config_dir) = make_two_repo_manifest(&tmp);
        fs::create_dir_all(&config_dir).unwrap();
        manifest.save(&config_dir).unwrap();

        // Add a project to repo-a
        let entry = manifest.repos.get("repo-a").unwrap();
        let grove_dir = entry.work_dir.join(".grove");
        let work_path = entry.work_dir.join("feat").display().to_string();
        let mut registry = Registry::default();
        registry
            .insert("feat".to_string(), make_project(&work_path))
            .unwrap();
        registry.save(&grove_dir).unwrap();

        // Without --force: should error
        let result = run_remove_with_config(&mut manifest, &config_dir, "repo-a", false);
        assert!(result.is_err(), "should error when projects exist");
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("registered projects"),
            "error should mention registered projects: {err_str}"
        );

        // With --force: should succeed
        let result_forced = run_remove_with_config(&mut manifest, &config_dir, "repo-a", true);
        assert!(
            result_forced.is_ok(),
            "force remove should succeed: {result_forced:?}"
        );
        assert!(
            !manifest.repos.contains_key("repo-a"),
            "repo-a should be removed after force"
        );
    }

    // Test 5: repo default <id> → updates default_repo in manifest
    #[test]
    #[serial]
    fn repo_default_updates_manifest() {
        let tmp = TempDir::new().unwrap();
        let (mut manifest, config_dir) = make_two_repo_manifest(&tmp);
        fs::create_dir_all(&config_dir).unwrap();
        manifest.save(&config_dir).unwrap();

        assert_eq!(manifest.default_repo.as_deref(), Some("repo-a"));

        let result = run_default_with_config(&mut manifest, &config_dir, "repo-b");
        assert!(result.is_ok(), "set default should succeed: {result:?}");
        assert_eq!(manifest.default_repo.as_deref(), Some("repo-b"));

        let reloaded = ReposManifest::load(&config_dir).unwrap();
        assert_eq!(reloaded.default_repo.as_deref(), Some("repo-b"));
    }
}
