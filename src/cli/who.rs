//! `grove who [tag|path]`: which Claude Code session owns a worktree, and is
//! that session still around?
//!
//! The owner comes from the registry entry when it has one, otherwise from the
//! last event in `events.jsonl` for that path. Status is:
//!
//! - `alive`: a live `~/.claude/sessions/<pid>.json` carries the session id.
//!   A file counts as live only when its `procStart` still matches field 22 of
//!   `/proc/<pid>/stat`, so a stale file left by a crash is ignored.
//! - `dormant`: no live process, but claude-persist kept the session for resume.
//! - `gone`: neither.
//! - `unknown`: no owner was ever recorded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::events::{self, Event, Owner};
use crate::repo::RepoContext;

pub struct WhoArgs {
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Alive,
    Dormant,
    Gone,
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Dormant => "dormant",
            Self::Gone => "gone",
            Self::Unknown => "unknown",
        }
    }
}

/// Which sessions are running and which are parked for resume.
#[derive(Debug, Default)]
pub struct SessionIndex {
    pub live: HashSet<String>,
    pub persisted: HashSet<String>,
}

impl SessionIndex {
    /// Read `~/.claude/sessions` and `~/.local/state/claude-persist/sessions`.
    pub fn load() -> Self {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Self::default();
        };
        Self::load_from(
            &home.join(".claude/sessions"),
            &home.join(".local/state/claude-persist/sessions"),
            Path::new("/proc"),
        )
    }

    pub fn load_from(sessions_dir: &Path, persist_dir: &Path, proc_dir: &Path) -> Self {
        let mut index = Self::default();
        for entry in read_dir_names(sessions_dir) {
            let Some(pid) = entry.strip_suffix(".json") else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(sessions_dir.join(&entry)) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) else {
                continue;
            };
            let recorded = v.get("procStart").map(|p| match p {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            if process_matches(proc_dir, pid, recorded.as_deref()) {
                index.live.insert(sid.to_string());
            }
        }
        for entry in read_dir_names(persist_dir) {
            let stem = entry.strip_suffix(".json").unwrap_or(&entry);
            let sid = stem.split('@').next().unwrap_or(stem);
            if !sid.is_empty() {
                index.persisted.insert(sid.to_string());
            }
        }
        index
    }

    pub fn status(&self, owner: Option<&Owner>) -> Status {
        let Some(sid) = owner.and_then(|o| o.session_id.as_deref()) else {
            return Status::Unknown;
        };
        if self.live.contains(sid) {
            Status::Alive
        } else if self.persisted.contains(sid) {
            Status::Dormant
        } else {
            Status::Gone
        }
    }
}

fn read_dir_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether process `pid` exists and, when a start time was recorded, started
/// at that time. Without `/proc` (not Linux) the session file is trusted.
fn process_matches(proc_dir: &Path, pid: &str, recorded: Option<&str>) -> bool {
    if !proc_dir.join("self").exists() && !proc_dir.join(pid).exists() {
        return !cfg!(target_os = "linux");
    }
    let Ok(stat) = std::fs::read_to_string(proc_dir.join(pid).join("stat")) else {
        return false;
    };
    match recorded {
        Some(want) => proc_start_from_stat(&stat).is_some_and(|got| got == want),
        None => true,
    }
}

/// Field 22 (starttime) of `/proc/<pid>/stat`. The command name in field 2
/// can contain spaces and parentheses, so count from the last `)`.
pub fn proc_start_from_stat(stat: &str) -> Option<String> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After ")" come fields 3.. ; field 22 is the 20th of those.
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// One worktree and what is known about who made it.
#[derive(Debug, Clone)]
pub struct Row {
    pub tag: Option<String>,
    pub path: PathBuf,
    pub owner: Option<Owner>,
    /// Where the owner came from: `registry`, `log` or `hook`.
    pub source: &'static str,
    pub last_event: Option<Event>,
}

/// The newest event for `path`, preferring one that is not a removal.
pub fn last_event_for<'a>(events: &'a [Event], path: &Path) -> Option<&'a Event> {
    let matching = || events.iter().rev().filter(|e| same_path(&e.path, path));
    matching()
        .find(|e| !e.is_removal())
        .or_else(|| matching().next())
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

pub fn make_row(
    tag: Option<&str>,
    path: &Path,
    registry_owner: Option<&Owner>,
    events: &[Event],
) -> Row {
    let last = last_event_for(events, path).cloned();
    let (owner, source) = match registry_owner {
        Some(o) => (Some(o.clone()), "registry"),
        None => match &last {
            Some(e) if !e.owner.is_empty() => {
                let src = if e.source.as_deref() == Some("hook") {
                    "hook"
                } else {
                    "log"
                };
                (Some(e.owner.clone()), src)
            }
            _ => (None, "-"),
        },
    };
    Row {
        tag: tag
            .map(str::to_string)
            .or_else(|| last.as_ref().and_then(|e| e.tag.clone())),
        path: path.to_path_buf(),
        owner,
        source,
        last_event: last,
    }
}

/// Rows for every registered project plus any other linked worktree of the
/// main repo (for example one made by a raw `git worktree add`).
pub fn all_rows(cx: &RepoContext, events: &[Event]) -> Vec<Row> {
    let mut rows: Vec<Row> = cx
        .registry
        .projects
        .iter()
        .map(|(tag, p)| make_row(Some(tag), &p.path, p.owner.as_ref(), events))
        .collect();

    let listed = crate::gc::exec::git(
        &cx.resolved.main_repo,
        &["worktree", "list", "--porcelain"],
        crate::gc::guards::QUICK_GIT_TIMEOUT,
    )
    .map(|t| crate::gc::scan::parse_worktree_list(&t))
    .unwrap_or_default();
    for wt in listed {
        let known = rows.iter().any(|r| same_path(&r.path, &wt.path))
            || same_path(&wt.path, &cx.resolved.main_repo);
        if !known {
            rows.push(make_row(None, &wt.path, None, events));
        }
    }
    rows
}

pub fn run(args: &WhoArgs, cx: &RepoContext) -> anyhow::Result<()> {
    let events = events::log_path()
        .map(|p| events::read_all(&p))
        .unwrap_or_default();
    let sessions = SessionIndex::load();

    let Some(target) = &args.target else {
        print_table(&all_rows(cx, &events), &sessions);
        return Ok(());
    };

    let row = if let Some(p) = cx.registry.projects.get(target) {
        make_row(Some(target), &p.path, p.owner.as_ref(), &events)
    } else {
        let mut path = PathBuf::from(target);
        if path.is_relative() {
            path = std::env::current_dir()?.join(path);
        }
        let path = path.canonicalize().unwrap_or(path);
        match cx
            .registry
            .projects
            .iter()
            .find(|(_, p)| same_path(&p.path, &path))
        {
            Some((tag, p)) => make_row(Some(tag), &p.path, p.owner.as_ref(), &events),
            None => make_row(None, &path, None, &events),
        }
    };
    print_detail(&row, &sessions);
    Ok(())
}

fn owner_label(owner: Option<&Owner>) -> String {
    let Some(o) = owner else {
        return "-".to_string();
    };
    match (&o.session_name, &o.session_id) {
        (Some(name), _) => name.clone(),
        (None, Some(sid)) => sid.chars().take(8).collect(),
        (None, None) => "-".to_string(),
    }
}

fn print_table(rows: &[Row], sessions: &SessionIndex) {
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.tag.clone().unwrap_or_else(|| "-".to_string()),
                sessions.status(r.owner.as_ref()).as_str().to_string(),
                owner_label(r.owner.as_ref()),
                r.owner
                    .as_ref()
                    .and_then(|o| o.agent_name.clone())
                    .unwrap_or_else(|| "-".to_string()),
                r.source.to_string(),
            ]
        })
        .collect();
    let headers = ["TAG", "STATUS", "SESSION", "AGENT", "SOURCE"];
    let mut widths = headers.map(str::len);
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cols: [&str; 5], path: &str| {
        let mut out = String::new();
        for (c, w) in cols.iter().zip(widths) {
            out.push_str(&format!("{c:<w$}  "));
        }
        out.push_str(path);
        println!("{}", out.trim_end());
    };
    line(headers, "PATH");
    for (row, r) in cells.iter().zip(rows) {
        line(
            [&row[0], &row[1], &row[2], &row[3], &row[4]],
            &r.path.display().to_string(),
        );
    }
}

fn print_detail(row: &Row, sessions: &SessionIndex) {
    let o = row.owner.as_ref();
    let field = |v: Option<&String>| v.cloned().unwrap_or_else(|| "-".to_string());
    println!("tag:          {}", row.tag.as_deref().unwrap_or("-"));
    println!("path:         {}", row.path.display());
    println!("status:       {}", sessions.status(o).as_str());
    println!(
        "session:      {}",
        field(o.and_then(|o| o.session_name.as_ref()))
    );
    println!(
        "session id:   {}",
        field(o.and_then(|o| o.session_id.as_ref()))
    );
    println!(
        "agent:        {}",
        field(o.and_then(|o| o.agent_name.as_ref()))
    );
    println!("source:       {}", row.source);
    if let Some(e) = &row.last_event {
        let ts =
            e.ts.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
        println!("last event:   {} at {ts}", e.op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn owner(sid: &str) -> Owner {
        Owner {
            session_id: Some(sid.to_string()),
            ..Owner::default()
        }
    }

    fn index(live: &[&str], persisted: &[&str]) -> SessionIndex {
        SessionIndex {
            live: live.iter().map(|s| s.to_string()).collect(),
            persisted: persisted.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn status_alive_dormant_gone_unknown() {
        let idx = index(&["a"], &["a", "d"]);
        assert_eq!(idx.status(Some(&owner("a"))), Status::Alive);
        assert_eq!(idx.status(Some(&owner("d"))), Status::Dormant);
        assert_eq!(idx.status(Some(&owner("x"))), Status::Gone);
        assert_eq!(idx.status(None), Status::Unknown);
        assert_eq!(idx.status(Some(&Owner::default())), Status::Unknown);
    }

    #[test]
    fn proc_start_parses_field_22_with_odd_comm() {
        // comm contains spaces and a ")" to make sure we count from the last one.
        let stat = "123 (we ird) cmd) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20 21";
        assert_eq!(proc_start_from_stat(stat).as_deref(), Some("424242"));
    }

    fn write_session(dir: &Path, pid: &str, sid: &str, proc_start: &str) {
        std::fs::write(
            dir.join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{sid}","procStart":"{proc_start}"}}"#),
        )
        .unwrap();
    }

    fn write_stat(proc_dir: &Path, pid: &str, start: &str) {
        let d = proc_dir.join(pid);
        std::fs::create_dir_all(&d).unwrap();
        let fields: Vec<String> = (3..=21).map(|n| n.to_string()).collect();
        std::fs::write(
            d.join("stat"),
            format!("{pid} (claude) {} {start} 23", fields.join(" ")),
        )
        .unwrap();
    }

    #[test]
    fn load_ignores_stale_and_recycled_session_files() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let persist = tmp.path().join("persist");
        let proc_dir = tmp.path().join("proc");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&persist).unwrap();
        std::fs::create_dir_all(proc_dir.join("self")).unwrap();

        write_session(&sessions, "10", "live-sid", "500");
        write_stat(&proc_dir, "10", "500");
        // PID recycled: the process exists but started at a different time.
        write_session(&sessions, "11", "recycled-sid", "600");
        write_stat(&proc_dir, "11", "601");
        // Stale file: no such process.
        write_session(&sessions, "12", "stale-sid", "700");
        std::fs::write(persist.join("stale-sid@-home-x.json"), "{}").unwrap();

        let idx = SessionIndex::load_from(&sessions, &persist, &proc_dir);
        assert_eq!(idx.status(Some(&owner("live-sid"))), Status::Alive);
        assert_eq!(idx.status(Some(&owner("recycled-sid"))), Status::Gone);
        assert_eq!(idx.status(Some(&owner("stale-sid"))), Status::Dormant);
    }

    #[test]
    fn row_prefers_registry_owner_then_last_non_removal_event() {
        let path = PathBuf::from("/w/t");
        let mut created = Event::default();
        created.op = "add".to_string();
        created.source = Some("hook".to_string());
        created.path = path.clone();
        created.owner = owner("creator");
        let mut removed = created.clone();
        removed.op = "remove".to_string();
        removed.owner = owner("remover");
        let events = vec![created, removed];

        let from_log = make_row(None, &path, None, &events);
        assert_eq!(from_log.source, "hook");
        assert_eq!(
            from_log.owner.unwrap().session_id.as_deref(),
            Some("creator")
        );

        let reg = owner("registered");
        let from_registry = make_row(Some("t"), &path, Some(&reg), &events);
        assert_eq!(from_registry.source, "registry");

        let nothing = make_row(None, Path::new("/elsewhere"), None, &events);
        assert!(nothing.owner.is_none());
        assert_eq!(
            index(&[], &[]).status(nothing.owner.as_ref()),
            Status::Unknown
        );
    }
}
