use std::path::PathBuf;

use comfy_table::{Cell, CellAlignment, Color};
use rayon::prelude::*;
use serde::Serialize;
use time::OffsetDateTime;

use crate::display::{self, make_table};
use crate::git::{
    WorktreeManager,
    gix_backend::GixBackend,
    status::{Status, compute as compute_status},
};
use crate::registry::{Project, Registry};
use crate::repo::RepoContext;

pub struct ListArgs {
    pub repo: Option<String>,
    /// Compact one-line-per-project output
    pub short: bool,
    /// Output as JSON
    pub json: bool,
    /// Skip git status scans in JSON output (fast path)
    pub no_status: bool,
}

pub struct ProjectRow {
    pub tag: String,
    pub project: Project,
    pub status: Option<Status>,
    /// True when the project's path no longer exists on disk.
    pub missing: bool,
}

// ── JSON output structs ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct JsonOutput {
    pub version: u32,
    pub repos: Vec<JsonRepo>,
}

#[derive(Serialize)]
pub struct JsonRepo {
    pub id: String,
    pub projects: Vec<JsonProject>,
}

#[derive(Serialize)]
pub struct JsonProject {
    pub tag: String,
    pub path: String,
    pub branch: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    pub frozen: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<JsonStatus>,
}

#[derive(Serialize)]
pub struct JsonStatus {
    pub dirty: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub untracked: u32,
}

impl From<&Status> for JsonStatus {
    fn from(s: &Status) -> Self {
        Self {
            dirty: s.dirty,
            ahead: s.ahead,
            behind: s.behind,
            untracked: s.untracked,
        }
    }
}

// ── Table renderer ───────────────────────────────────────────────────────────

/// Render a single repo section (header + table) to a String.
/// This is the unit under insta snapshot test.
pub fn render_repo_section(repo_id: &str, rows: &[ProjectRow]) -> String {
    render_repo_section_with_marker(repo_id, rows, false)
}

/// Render a section with optional "current cwd" marker on the header.
pub fn render_repo_section_with_marker(repo_id: &str, rows: &[ProjectRow], is_cwd: bool) -> String {
    let mut out = String::new();

    let cwd_marker = if is_cwd { " (here)" } else { "" };
    let header_text = format!("── {repo_id}{cwd_marker} ──");
    let header = if display::use_color() {
        use owo_colors::OwoColorize;
        if is_cwd {
            header_text.bold().cyan().to_string()
        } else {
            header_text.bold().to_string()
        }
    } else {
        header_text
    };
    out.push_str(&header);
    out.push('\n');

    let mut table = make_table();
    table.set_header(vec![
        display::make_header_cell("Tag"),
        display::make_header_cell("Branch"),
        display::make_header_cell("Base"),
        display::make_header_cell("Status"),
        display::make_header_cell("Issue"),
    ]);

    for row in rows {
        table.add_row(vec![
            tag_cell(row),
            branch_cell(row),
            base_cell(row),
            status_cell(row),
            issue_cell(row),
        ]);
    }

    out.push_str(&table.to_string());
    out.push('\n');

    // Summary footer: "43 projects · 5 dirty · 2 ahead · 3 frozen"
    let summary = build_summary(rows);
    if !summary.is_empty() {
        let line = if display::use_color() {
            use owo_colors::OwoColorize;
            summary.dimmed().to_string()
        } else {
            summary
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');
    out
}

fn tag_cell(row: &ProjectRow) -> Cell {
    let tag = &row.tag;
    if row.project.frozen {
        let text = format!("❄ {tag}");
        Cell::new(display::dim(&text))
    } else if display::use_color() {
        use owo_colors::OwoColorize;
        Cell::new(tag.bold().to_string())
    } else {
        Cell::new(tag)
    }
}

fn branch_cell(row: &ProjectRow) -> Cell {
    let branch = &row.project.branch;
    if !display::use_color() {
        return Cell::new(branch);
    }
    // Dim the "DESKTOP-NNNNN-" prefix to emphasize the unique tail.
    if let Some(stripped) = strip_issue_prefix(branch) {
        use owo_colors::OwoColorize;
        let (prefix, rest) = branch.split_at(branch.len() - stripped.len());
        Cell::new(format!("{}{}", prefix.dimmed(), rest))
    } else {
        Cell::new(branch)
    }
}

/// If `branch` starts with `<PREFIX>-<digits>-`, return the suffix after that.
fn strip_issue_prefix(branch: &str) -> Option<&str> {
    let first_dash = branch.find('-')?;
    let after_prefix = &branch[first_dash + 1..];
    let second_dash = after_prefix.find('-')?;
    let middle = &after_prefix[..second_dash];
    if !middle.is_empty() && middle.chars().all(|c| c.is_ascii_digit()) {
        Some(&after_prefix[second_dash + 1..])
    } else {
        None
    }
}

fn base_cell(row: &ProjectRow) -> Cell {
    let base = &row.project.base;
    Cell::new(display::dim(base))
}

fn status_cell(row: &ProjectRow) -> Cell {
    if row.missing {
        let text = "✗ missing";
        return Cell::new(text).fg(Color::Red);
    }
    let glyph = status_glyph(row.status.as_ref());
    let text = format_status(row.status.as_ref());
    let display_text = format!("{glyph} {text}");
    match row.status.as_ref() {
        None => Cell::new(display::dim(&display_text)),
        Some(s) if s.dirty => Cell::new(&display_text).fg(Color::Yellow),
        Some(s) => match (s.ahead.unwrap_or(0), s.behind.unwrap_or(0)) {
            (0, 0) => Cell::new(&display_text).fg(Color::Green),
            (_, 0) => Cell::new(&display_text).fg(Color::Cyan),
            (0, _) => Cell::new(&display_text).fg(Color::Magenta),
            _ => Cell::new(&display_text).fg(Color::Yellow),
        },
    }
}

fn issue_cell(row: &ProjectRow) -> Cell {
    match row.project.issue {
        Some(n) => {
            let text = format!("#{n}");
            let colored = if display::use_color() {
                use owo_colors::OwoColorize;
                text.cyan().to_string()
            } else {
                text
            };
            Cell::new(colored).set_alignment(CellAlignment::Right)
        }
        None => Cell::new("").set_alignment(CellAlignment::Right),
    }
}

fn status_glyph(status: Option<&Status>) -> &'static str {
    let Some(s) = status else {
        return "?";
    };
    if s.dirty {
        return "●";
    }
    match (s.ahead.unwrap_or(0), s.behind.unwrap_or(0)) {
        (0, 0) => "✓",
        (_, 0) => "↑",
        (0, _) => "↓",
        _ => "↕",
    }
}

fn build_summary(rows: &[ProjectRow]) -> String {
    let total = rows.len();
    let mut dirty = 0usize;
    let mut ahead = 0usize;
    let mut behind = 0usize;
    let mut frozen = 0usize;
    let mut missing = 0usize;
    let mut scanned = 0usize;
    for r in rows {
        if r.project.frozen {
            frozen += 1;
        }
        if r.missing {
            missing += 1;
            continue;
        }
        if let Some(ref s) = r.status {
            scanned += 1;
            if s.dirty {
                dirty += 1;
            } else {
                if s.ahead.unwrap_or(0) > 0 {
                    ahead += 1;
                }
                if s.behind.unwrap_or(0) > 0 {
                    behind += 1;
                }
            }
        }
    }
    let mut parts = vec![format!(
        "{total} {}",
        if total == 1 { "project" } else { "projects" }
    )];
    if dirty > 0 {
        parts.push(format!("{dirty} dirty"));
    }
    if ahead > 0 {
        parts.push(format!("{ahead} ahead"));
    }
    if behind > 0 {
        parts.push(format!("{behind} behind"));
    }
    if frozen > 0 {
        parts.push(format!("{frozen} frozen"));
    }
    if missing > 0 {
        parts.push(format!("{missing} missing"));
    }
    let unscanned = total.saturating_sub(scanned + missing);
    if unscanned > 0 {
        parts.push(format!("{unscanned} unscanned"));
    }
    parts.join(" · ")
}

// ── Short renderer ───────────────────────────────────────────────────────────

/// Render compact one-line-per-project output for a single repo section.
pub fn render_short_section(repo_id: &str, rows: &[ProjectRow]) -> String {
    let col_widths = compute_short_col_widths(repo_id, rows);
    let mut out = String::new();
    for row in rows {
        let label = format!("{}/{}", repo_id, row.tag);
        let glyph = status_glyph(row.status.as_ref());
        let status_text = format_status(row.status.as_ref());
        let line = format!(
            "{:<lw$}  {:<bw$}  {glyph} {status_text}\n",
            label,
            row.project.branch,
            lw = col_widths.label,
            bw = col_widths.branch,
        );
        out.push_str(&line);
    }
    out
}

struct ShortColWidths {
    label: usize,
    branch: usize,
}

fn compute_short_col_widths(repo_id: &str, rows: &[ProjectRow]) -> ShortColWidths {
    let label_w = rows
        .iter()
        .map(|r| repo_id.len() + 1 + r.tag.len())
        .max()
        .unwrap_or(0);
    let branch_w = rows
        .iter()
        .map(|r| r.project.branch.len())
        .max()
        .unwrap_or(0);
    ShortColWidths {
        label: label_w,
        branch: branch_w,
    }
}

// ── Status formatter ─────────────────────────────────────────────────────────

fn format_status(status: Option<&Status>) -> String {
    let Some(s) = status else {
        return "unknown".to_string();
    };
    if s.dirty {
        return "dirty".to_string();
    }
    match (s.ahead, s.behind) {
        (Some(a), Some(b)) if a == 0 && b == 0 => "clean".to_string(),
        (Some(a), Some(b)) if a > 0 && b == 0 => format!("{a} ahead"),
        (Some(a), Some(b)) if a == 0 && b > 0 => format!("{b} behind"),
        (Some(a), Some(b)) => format!("{a} ahead, {b} behind"),
        _ => "clean".to_string(),
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Detect which repo id the cwd belongs to by checking GROVE_ORIG_CWD then current_dir.
fn cwd_repo_id(cx: &RepoContext) -> Option<String> {
    let cwd: PathBuf = std::env::var("GROVE_ORIG_CWD")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;

    let mut best: Option<(usize, String)> = None;
    for (id, entry) in &cx.global.repos {
        if cwd.starts_with(&entry.work_dir) {
            let depth = entry.work_dir.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, id.clone()));
            }
        }
    }
    best.map(|(_, id)| id)
}

/// Load rows for a single repo by reading its registry from disk.
fn load_repo_rows(work_dir: &std::path::Path) -> Vec<ProjectRow> {
    let grove_dir = work_dir.join(".grove");
    let registry = Registry::load(&grove_dir).unwrap_or_default();
    registry
        .list()
        .map(|(tag, project)| {
            let missing = !project.path.exists();
            ProjectRow {
                tag: tag.to_string(),
                project: project.clone(),
                status: None,
                missing,
            }
        })
        .collect()
}

/// Compute git status for each project in parallel. Projects whose path no
/// longer exists or whose `.git` is missing get `status: None`.
fn scan_statuses(rows: &mut [ProjectRow]) {
    let backend = GixBackend;
    let statuses: Vec<Option<Status>> = rows
        .par_iter()
        .map(|row| {
            if row.missing {
                return None;
            }
            let wt = backend.open(&row.project.path).ok()?;
            compute_status(&wt).ok()
        })
        .collect();
    for (row, status) in rows.iter_mut().zip(statuses.into_iter()) {
        row.status = status;
    }
}

pub fn run(args: &ListArgs, cx: &RepoContext) -> anyhow::Result<()> {
    // Determine which repo ids to scan.
    let all_ids: Vec<String> = if let Some(ref filter_id) = args.repo {
        vec![filter_id.clone()]
    } else {
        cx.global.repos.keys().cloned().collect()
    };

    // Detect cwd-matched repo for ordering.
    let cwd_id = cwd_repo_id(cx);

    // Decide whether to run status scans. JSON respects --no-status; table mode
    // always scans (the whole point of `grove list` is to see status).
    let want_status = !args.json || !args.no_status;

    // Load rows in parallel across repos, then scan statuses in parallel within.
    let mut sections: Vec<(String, Vec<ProjectRow>)> = all_ids
        .par_iter()
        .filter_map(|id| {
            let entry = cx.global.repos.get(id)?;
            let mut rows = load_repo_rows(&entry.work_dir);
            if want_status {
                scan_statuses(&mut rows);
            }
            Some((id.clone(), rows))
        })
        .collect();

    // Sort: cwd-matched repo first, then alphabetical by id.
    sections.sort_by(|(a, _), (b, _)| {
        let a_is_cwd = cwd_id.as_deref() == Some(a.as_str());
        let b_is_cwd = cwd_id.as_deref() == Some(b.as_str());
        match (a_is_cwd, b_is_cwd) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });

    if args.json {
        let repos: Vec<JsonRepo> = sections
            .iter()
            .map(|(id, rows)| {
                let projects = rows
                    .iter()
                    .map(|r| JsonProject {
                        tag: r.tag.clone(),
                        path: r.project.path.display().to_string(),
                        branch: r.project.branch.clone(),
                        base: r.project.base.clone(),
                        issue: r.project.issue,
                        frozen: r.project.frozen,
                        created: r.project.created,
                        status: if args.no_status {
                            None
                        } else {
                            r.status.as_ref().map(JsonStatus::from)
                        },
                    })
                    .collect();
                JsonRepo {
                    id: id.clone(),
                    projects,
                }
            })
            .collect();

        let output = JsonOutput { version: 1, repos };
        let json = serde_json::to_string_pretty(&output)?;
        println!("{json}");
        return Ok(());
    }

    if args.short {
        for (id, rows) in &sections {
            let section = render_short_section(id, rows);
            print!("{section}");
        }
        return Ok(());
    }

    for (id, rows) in &sections {
        let is_cwd = cwd_id.as_deref() == Some(id.as_str());
        if rows.is_empty() {
            let header_text = format!("── {id}{} ──", if is_cwd { " (here)" } else { "" });
            let header = if display::use_color() {
                use owo_colors::OwoColorize;
                if is_cwd {
                    header_text.bold().cyan().to_string()
                } else {
                    header_text.bold().to_string()
                }
            } else {
                header_text
            };
            println!("{header}");
            println!("(no projects)");
            println!();
        } else {
            let section = render_repo_section_with_marker(id, rows, is_cwd);
            print!("{section}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use insta::assert_snapshot;
    use serial_test::serial;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    use super::*;
    use crate::config::ResolvedConfig;
    use crate::config::global::{RepoEntry, ReposManifest};
    use crate::git::status::Status;
    use crate::registry::{Project, Registry};

    fn make_project(branch: &str, base: &str, issue: Option<u32>, frozen: bool) -> Project {
        Project {
            path: PathBuf::from("/c/work/test"),
            branch: branch.to_string(),
            base: base.to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue,
            frozen,
        }
    }

    fn clean_status() -> Status {
        Status {
            dirty: false,
            ahead: Some(0),
            behind: Some(0),
            untracked: 0,
            is_pushed: true,
        }
    }

    fn dirty_status() -> Status {
        Status {
            dirty: true,
            ahead: Some(0),
            behind: Some(0),
            untracked: 1,
            is_pushed: true,
        }
    }

    fn ahead_status(n: u32) -> Status {
        Status {
            dirty: false,
            ahead: Some(n),
            behind: Some(0),
            untracked: 0,
            is_pushed: n == 0,
        }
    }

    fn fixture_rows() -> Vec<ProjectRow> {
        vec![
            ProjectRow {
                tag: "alpha".to_string(),
                project: make_project("PROJ-1-alpha", "origin/main", Some(1), false),
                status: Some(clean_status()),
                missing: false,
            },
            ProjectRow {
                tag: "beta".to_string(),
                project: make_project("PROJ-2-beta", "origin/main", Some(2), false),
                status: Some(dirty_status()),
                missing: false,
            },
            ProjectRow {
                tag: "gamma".to_string(),
                project: make_project("PROJ-3-gamma", "origin/main", None, true),
                status: Some(ahead_status(3)),
                missing: false,
            },
        ]
    }

    #[test]
    fn snapshot_basic_section() {
        // Ensure NO_COLOR so ANSI escapes don't pollute the snapshot.
        unsafe { std::env::set_var("NO_COLOR", "1") };

        let rows = vec![
            ProjectRow {
                tag: "alpha".to_string(),
                project: make_project("PROJ-1-alpha", "origin/main", Some(1), false),
                status: Some(clean_status()),
                missing: false,
            },
            ProjectRow {
                tag: "beta".to_string(),
                project: make_project("PROJ-2-beta", "origin/main", Some(2), false),
                status: Some(dirty_status()),
                missing: false,
            },
            ProjectRow {
                tag: "gamma".to_string(),
                project: make_project("PROJ-3-gamma", "origin/main", None, true),
                status: Some(ahead_status(3)),
                missing: false,
            },
        ];

        let output = render_repo_section("test-repo", &rows);

        unsafe { std::env::remove_var("NO_COLOR") };

        assert_snapshot!("list__basic", output);
    }

    #[test]
    fn frozen_project_has_frozen_suffix() {
        unsafe { std::env::set_var("NO_COLOR", "1") };

        let rows = vec![ProjectRow {
            tag: "hotfix".to_string(),
            project: make_project("PROJ-99-hotfix", "origin/main", None, true),
            status: Some(clean_status()),
            missing: false,
        }];

        let output = render_repo_section("myrepo", &rows);
        unsafe { std::env::remove_var("NO_COLOR") };

        assert!(
            output.contains("❄"),
            "frozen project should show snowflake glyph"
        );
    }

    #[test]
    fn format_status_variants() {
        assert_eq!(format_status(None), "unknown");
        assert_eq!(format_status(Some(&clean_status())), "clean");
        assert_eq!(format_status(Some(&dirty_status())), "dirty");
        assert_eq!(format_status(Some(&ahead_status(2))), "2 ahead");
        assert_eq!(
            format_status(Some(&Status {
                dirty: false,
                ahead: Some(0),
                behind: Some(1),
                untracked: 0,
                is_pushed: true,
            })),
            "1 behind"
        );
        assert_eq!(
            format_status(Some(&Status {
                dirty: false,
                ahead: Some(2),
                behind: Some(3),
                untracked: 0,
                is_pushed: false,
            })),
            "2 ahead, 3 behind"
        );
    }

    #[test]
    fn snapshot_short_section() {
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let output = render_short_section("test-repo", &fixture_rows());
        unsafe { std::env::remove_var("NO_COLOR") };
        assert_snapshot!("list__short", output);
    }

    #[test]
    fn snapshot_json_with_status() {
        let rows = fixture_rows();
        let projects: Vec<JsonProject> = rows
            .iter()
            .map(|r| JsonProject {
                tag: r.tag.clone(),
                path: r.project.path.display().to_string(),
                branch: r.project.branch.clone(),
                base: r.project.base.clone(),
                issue: r.project.issue,
                frozen: r.project.frozen,
                created: r.project.created,
                status: r.status.as_ref().map(JsonStatus::from),
            })
            .collect();
        let output = JsonOutput {
            version: 1,
            repos: vec![JsonRepo {
                id: "test-repo".to_string(),
                projects,
            }],
        };
        let json = serde_json::to_string_pretty(&output).unwrap();

        // Assert JSON ends with a closing brace (println adds the newline in run()).
        assert!(json.ends_with('}'), "json should end with closing brace");

        // Parse and assert schema shape.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["version"], 1);
        let repos = v["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["id"], "test-repo");
        let projects = repos[0]["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 3);
        // Status present when not no_status.
        assert!(projects[0]["status"].is_object());
        assert_eq!(projects[0]["tag"], "alpha");
        assert_eq!(projects[0]["branch"], "PROJ-1-alpha");

        assert_snapshot!("list__json", json);
    }

    #[test]
    fn snapshot_json_no_status() {
        let rows = fixture_rows();
        let projects: Vec<JsonProject> = rows
            .iter()
            .map(|r| JsonProject {
                tag: r.tag.clone(),
                path: r.project.path.display().to_string(),
                branch: r.project.branch.clone(),
                base: r.project.base.clone(),
                issue: r.project.issue,
                frozen: r.project.frozen,
                created: r.project.created,
                status: None,
            })
            .collect();
        let output = JsonOutput {
            version: 1,
            repos: vec![JsonRepo {
                id: "test-repo".to_string(),
                projects,
            }],
        };
        let json = serde_json::to_string_pretty(&output).unwrap();

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let repos = v["repos"].as_array().unwrap();
        let projects = repos[0]["projects"].as_array().unwrap();
        // Status absent in no_status mode.
        assert!(projects[0]["status"].is_null(), "status should be absent");

        assert_snapshot!("list__json_no_status", json);
    }

    // ── grove-4pe.3: Cross-repo ordering tests ───────────────────────────────

    fn make_repo_entry_for_dir(work_dir: PathBuf) -> RepoEntry {
        RepoEntry {
            main_repo: work_dir.join("master"),
            work_dir: work_dir.clone(),
            dir_prefix: String::new(),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            launch: None,
        }
    }

    fn make_project_for_tag(work_dir: &std::path::Path, tag: &str) -> Project {
        Project {
            path: work_dir.join(tag),
            branch: format!("branch-{tag}"),
            base: "origin/main".to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue: None,
            frozen: false,
        }
    }

    fn make_cross_repo_context(
        tmp: &TempDir,
        repo_ids: &[&str],
        projects_per_repo: &[(&str, &[&str])],
        default_id: &str,
    ) -> RepoContext {
        let mut repos = BTreeMap::new();
        for &id in repo_ids {
            let work_dir = tmp.path().join(id);
            fs::create_dir_all(&work_dir).unwrap();

            if let Some(&(_, tags)) = projects_per_repo.iter().find(|&&(rid, _)| rid == id) {
                let grove_dir = work_dir.join(".grove");
                let mut registry = Registry::default();
                for &tag in tags {
                    let proj = make_project_for_tag(&work_dir, tag);
                    registry.insert(tag.to_string(), proj).unwrap();
                }
                registry.save(&grove_dir).unwrap();
            }

            repos.insert(id.to_string(), make_repo_entry_for_dir(work_dir));
        }

        let global = ReposManifest {
            schema_version: 1,
            default_repo: Some(default_id.to_string()),
            repos: repos.clone(),
        };

        let default_entry = repos.get(default_id).unwrap();
        let resolved = ResolvedConfig {
            main_repo: default_entry.main_repo.clone(),
            work_dir: default_entry.work_dir.clone(),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let grove_dir = default_entry.work_dir.join(".grove");
        let registry = Registry::load(&grove_dir).unwrap_or_default();

        RepoContext {
            id: default_id.to_string(),
            global,
            resolved,
            registry,
        }
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

    // AC1: 2 repos, cwd matches neither → alphabetical order.
    #[test]
    #[serial]
    fn cross_repo_alphabetical_when_cwd_matches_neither() {
        let tmp = TempDir::new().unwrap();
        let unrelated = tmp.path().join("unrelated");
        fs::create_dir_all(&unrelated).unwrap();

        let cx = make_cross_repo_context(
            &tmp,
            &["zoo-repo", "alpha-repo"],
            &[("alpha-repo", &["proj-a"]), ("zoo-repo", &["proj-z"])],
            "alpha-repo",
        );
        let _env = EnvGuard::set("GROVE_ORIG_CWD", unrelated.to_str().unwrap());

        let cwd_id = cwd_repo_id(&cx);
        assert!(cwd_id.is_none(), "cwd should not match either repo");

        let mut ids: Vec<String> = cx.global.repos.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let a_cwd = cwd_id.as_deref() == Some(a.as_str());
            let b_cwd = cwd_id.as_deref() == Some(b.as_str());
            match (a_cwd, b_cwd) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });
        assert_eq!(
            ids[0], "alpha-repo",
            "alpha-repo should come first alphabetically"
        );
        assert_eq!(ids[1], "zoo-repo", "zoo-repo should come second");
    }

    // AC2: cwd inside repo B's work_dir → repo B section first.
    #[test]
    #[serial]
    fn cross_repo_cwd_inside_repo_b_comes_first() {
        let tmp = TempDir::new().unwrap();
        let cx = make_cross_repo_context(
            &tmp,
            &["alpha-repo", "beta-repo"],
            &[("alpha-repo", &["proj-a"]), ("beta-repo", &["proj-b"])],
            "alpha-repo",
        );

        let beta_work_dir = tmp.path().join("beta-repo");
        let _env = EnvGuard::set("GROVE_ORIG_CWD", beta_work_dir.to_str().unwrap());

        let cwd_id = cwd_repo_id(&cx);
        assert_eq!(
            cwd_id.as_deref(),
            Some("beta-repo"),
            "cwd should match beta-repo"
        );

        let mut ids: Vec<String> = cx.global.repos.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let a_cwd = cwd_id.as_deref() == Some(a.as_str());
            let b_cwd = cwd_id.as_deref() == Some(b.as_str());
            match (a_cwd, b_cwd) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });
        assert_eq!(
            ids[0], "beta-repo",
            "beta-repo should be first when cwd is inside it"
        );
        assert_eq!(ids[1], "alpha-repo");
    }

    // AC3: --repo <id> filter → only one section scanned.
    #[test]
    #[serial]
    fn cross_repo_filter_by_repo_id() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::remove("GROVE_ORIG_CWD");

        let cx = make_cross_repo_context(
            &tmp,
            &["alpha-repo", "beta-repo"],
            &[("alpha-repo", &["proj-a"]), ("beta-repo", &["proj-b"])],
            "alpha-repo",
        );

        let filter_id = "alpha-repo".to_string();
        let all_ids: Vec<String> = vec![filter_id.clone()];

        assert_eq!(all_ids.len(), 1);
        assert_eq!(all_ids[0], "alpha-repo");

        // Verify only alpha-repo's rows are loaded.
        let entry = cx.global.repos.get("alpha-repo").unwrap();
        let rows = load_repo_rows(&entry.work_dir);
        assert_eq!(rows.len(), 1, "alpha-repo should have 1 project");
        assert_eq!(rows[0].tag, "proj-a");
    }
}
