use comfy_table::{Cell, Color};

use crate::display::{self, make_table};
use crate::git::status::Status;
use crate::registry::Project;
use crate::repo::RepoContext;

pub struct ListArgs {
    /// Filter to a specific repo id (currently only cx.id is supported; cross-repo comes in 4pe.3)
    pub repo: Option<String>,
}

pub struct ProjectRow {
    pub tag: String,
    pub project: Project,
    pub status: Option<Status>,
}

/// Render a single repo section (header + table) to a String.
/// This is the unit under insta snapshot test.
pub fn render_repo_section(repo_id: &str, rows: &[ProjectRow]) -> String {
    let mut out = String::new();

    let header = format!("── {repo_id} ──");
    let header = if display::use_color() {
        use owo_colors::OwoColorize;
        header.bold().to_string()
    } else {
        header
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
        let tag_text = if row.project.frozen {
            format!("{} (frozen)", row.tag)
        } else {
            row.tag.clone()
        };

        let tag_cell = if row.project.frozen {
            Cell::new(display::dim(&tag_text))
        } else {
            Cell::new(tag_text)
        };

        let status_text = format_status(row.status.as_ref());
        let status_cell = if let Some(ref s) = row.status {
            if s.dirty {
                Cell::new(&status_text).fg(Color::Yellow)
            } else if s.ahead.unwrap_or(0) > 0 {
                Cell::new(&status_text).fg(Color::Green)
            } else {
                Cell::new(&status_text)
            }
        } else {
            Cell::new(&status_text)
        };

        let issue_text = row
            .project
            .issue
            .map(|n| format!("#{n}"))
            .unwrap_or_default();

        table.add_row(vec![
            tag_cell,
            Cell::new(&row.project.branch),
            Cell::new(&row.project.base),
            status_cell,
            Cell::new(issue_text),
        ]);
    }

    out.push_str(&table.to_string());
    out.push('\n');
    out
}

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

pub fn run(args: &ListArgs, cx: &RepoContext) -> anyhow::Result<()> {
    // Cross-repo dispatch (grove-4pe.3) is a later bead.
    // For now, render only cx.id's repo.
    let _ = &args.repo; // respected by the single-repo constraint

    let rows: Vec<ProjectRow> = cx
        .registry
        .list()
        .map(|(tag, project)| {
            // TODO(grove-rez.2): compute live status via git::status::compute when available.
            // For now we emit None so the table still renders without crashing on tempdir
            // fixtures where no real git repo exists.
            ProjectRow {
                tag: tag.to_string(),
                project: project.clone(),
                status: None,
            }
        })
        .collect();

    let section = render_repo_section(&cx.id, &rows);
    print!("{section}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use insta::assert_snapshot;
    use time::OffsetDateTime;

    use super::*;
    use crate::git::status::Status;
    use crate::registry::Project;

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
        }
    }

    fn dirty_status() -> Status {
        Status {
            dirty: true,
            ahead: Some(0),
            behind: Some(0),
            untracked: 1,
        }
    }

    fn ahead_status(n: u32) -> Status {
        Status {
            dirty: false,
            ahead: Some(n),
            behind: Some(0),
            untracked: 0,
        }
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
            },
            ProjectRow {
                tag: "beta".to_string(),
                project: make_project("PROJ-2-beta", "origin/main", Some(2), false),
                status: Some(dirty_status()),
            },
            ProjectRow {
                tag: "gamma".to_string(),
                project: make_project("PROJ-3-gamma", "origin/main", None, true),
                status: Some(ahead_status(3)),
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
        }];

        let output = render_repo_section("myrepo", &rows);
        unsafe { std::env::remove_var("NO_COLOR") };

        assert!(
            output.contains("(frozen)"),
            "frozen project should show (frozen) suffix"
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
            })),
            "1 behind"
        );
        assert_eq!(
            format_status(Some(&Status {
                dirty: false,
                ahead: Some(2),
                behind: Some(3),
                untracked: 0,
            })),
            "2 ahead, 3 behind"
        );
    }
}
