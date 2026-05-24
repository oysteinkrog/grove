/// Snapshot tests locking down byte-level output of read-only rendering functions.
///
/// Uses 2 repos ("alpha-repo", "beta-repo") and 5 projects in mixed states:
///   clean, dirty, ahead, frozen, behind.
///
/// All tests set NO_COLOR=1 to strip ANSI escapes for determinism.
/// Tests that manipulate env vars are marked #[serial] to prevent interference.
use std::path::PathBuf;

use insta::assert_snapshot;
use serial_test::serial;
use time::OffsetDateTime;

use grove::cli::list::{
    JsonOutput, JsonProject, JsonRepo, JsonStatus, ProjectRow, render_repo_section,
    render_short_section,
};
use grove::cli::status::JsonOutput as StatusJsonOutput;
use grove::git::status::Status;
use grove::registry::Project;

// ── Fixture helpers ───────────────────────────────────────────────────────────

fn fixed_ts() -> OffsetDateTime {
    // 2024-01-15T12:00:00Z — fixed for determinism
    OffsetDateTime::from_unix_timestamp(1_705_320_000).unwrap()
}

fn make_project(path: &str, branch: &str, base: &str, issue: Option<u32>, frozen: bool) -> Project {
    Project {
        path: PathBuf::from(path),
        branch: branch.to_string(),
        base: base.to_string(),
        created: fixed_ts(),
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
        untracked: 2,
        is_pushed: true,
    }
}

fn ahead_status(n: u32) -> Status {
    Status {
        dirty: false,
        ahead: Some(n),
        behind: Some(0),
        untracked: 0,
        is_pushed: false,
    }
}

fn behind_status(n: u32) -> Status {
    Status {
        dirty: false,
        ahead: Some(0),
        behind: Some(n),
        untracked: 0,
        is_pushed: true,
    }
}

/// 5 projects across 2 repos in mixed states:
///   alpha-repo: "clean" (clean+pushed), "dirty" (dirty), "ahead" (3 ahead)
///   beta-repo:  "frozen" (clean, frozen=true), "behind" (2 behind)
fn alpha_rows() -> Vec<ProjectRow> {
    vec![
        ProjectRow {
            tag: "clean".to_string(),
            project: make_project(
                "/c/work/alpha/clean",
                "ALPHA-1-clean",
                "origin/main",
                Some(1),
                false,
            ),
            status: Some(clean_status()),
            missing: false,
        },
        ProjectRow {
            tag: "dirty".to_string(),
            project: make_project(
                "/c/work/alpha/dirty",
                "ALPHA-2-dirty",
                "origin/main",
                Some(2),
                false,
            ),
            status: Some(dirty_status()),
            missing: false,
        },
        ProjectRow {
            tag: "ahead".to_string(),
            project: make_project(
                "/c/work/alpha/ahead",
                "ALPHA-3-ahead",
                "origin/main",
                Some(3),
                false,
            ),
            status: Some(ahead_status(3)),
            missing: false,
        },
    ]
}

fn beta_rows() -> Vec<ProjectRow> {
    vec![
        ProjectRow {
            tag: "frozen".to_string(),
            project: make_project(
                "/c/work/beta/frozen",
                "BETA-4-frozen",
                "origin/main",
                None,
                true,
            ),
            status: Some(clean_status()),
            missing: false,
        },
        ProjectRow {
            tag: "behind".to_string(),
            project: make_project(
                "/c/work/beta/behind",
                "BETA-5-behind",
                "origin/main",
                None,
                false,
            ),
            status: Some(behind_status(2)),
            missing: false,
        },
    ]
}

// ── Snapshot tests ────────────────────────────────────────────────────────────

/// grove list — table renderer, alpha-repo section (3 projects: clean, dirty, ahead)
#[test]
#[serial]
fn snapshot_list_table_alpha() {
    unsafe { std::env::set_var("NO_COLOR", "1") };
    let output = render_repo_section("alpha-repo", &alpha_rows());
    unsafe { std::env::remove_var("NO_COLOR") };

    assert_snapshot!("list__table_alpha", output);
}

/// grove list — table renderer, beta-repo section (2 projects: frozen, behind)
#[test]
#[serial]
fn snapshot_list_table_beta() {
    unsafe { std::env::set_var("NO_COLOR", "1") };
    let output = render_repo_section("beta-repo", &beta_rows());
    unsafe { std::env::remove_var("NO_COLOR") };

    assert_snapshot!("list__table_beta", output);
}

/// grove list --short — compact output, alpha-repo section
#[test]
#[serial]
fn snapshot_list_short() {
    unsafe { std::env::set_var("NO_COLOR", "1") };
    let output = render_short_section("alpha-repo", &alpha_rows());
    unsafe { std::env::remove_var("NO_COLOR") };

    assert_snapshot!("list__short_alpha", output);
}

/// grove list --json — full JSON output with status fields
#[test]
fn snapshot_list_json() {
    let all_rows: Vec<ProjectRow> = alpha_rows().into_iter().chain(beta_rows()).collect();

    let projects: Vec<JsonProject> = all_rows
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
            id: "alpha-repo".to_string(),
            projects,
        }],
    };

    let json = serde_json::to_string_pretty(&output).unwrap();
    assert_snapshot!("list__json_fixture", json);
}

/// grove status <tag> --json — JSON output for a single project detail
#[test]
fn snapshot_status_json() {
    let output = StatusJsonOutput {
        version: 1,
        tag: "clean".to_string(),
        path: "/c/work/alpha/clean".to_string(),
        head_branch: Some("ALPHA-1-clean".to_string()),
        upstream: Some("origin/ALPHA-1-clean".to_string()),
        ahead: Some(0),
        behind: Some(0),
        dirty: false,
        dirty_files: vec![],
        dirty_files_total: 0,
        last_commit_time: Some(fixed_ts()),
    };

    let json = serde_json::to_string_pretty(&output).unwrap();
    assert_snapshot!("status__json_fixture", json);
}

/// NO_COLOR=1 — verify no ANSI escape codes appear in table output
#[test]
#[serial]
fn no_color_produces_no_ansi_escapes() {
    unsafe { std::env::set_var("NO_COLOR", "1") };
    let table_out = render_repo_section("alpha-repo", &alpha_rows());
    let short_out = render_short_section("alpha-repo", &alpha_rows());
    unsafe { std::env::remove_var("NO_COLOR") };

    // ESC char (0x1b) indicates ANSI escape sequences
    assert!(
        !table_out.contains('\x1b'),
        "table output must not contain ANSI escapes with NO_COLOR=1"
    );
    assert!(
        !short_out.contains('\x1b'),
        "short output must not contain ANSI escapes with NO_COLOR=1"
    );

    assert_snapshot!("list__no_color_table", table_out);
}
