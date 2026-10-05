//! Append-only log of worktree lifecycle events.
//!
//! Every `grove new`, `fork`, `adopt`, `done` and gc removal appends one JSON
//! line to `$XDG_STATE_HOME/grove/events.jsonl` (default
//! `~/.local/state/grove/events.jsonl`). Each line records who ran the command:
//! the Claude Code session id, its user-set name, the agent-mail name, and the
//! session process start time, so `grove who` can later tell whether that
//! session is still alive.
//!
//! The log is a convenience, never a gate. A failed write prints a warning and
//! the command carries on.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The session that created or touched a worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// PID of the Claude Code process that owns the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_pid: Option<u32>,
    /// Field 22 of `/proc/<claude_pid>/stat`. Pairs with `claude_pid` so a
    /// recycled PID is not mistaken for the same session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_start: Option<String>,
}

impl Owner {
    /// Read the current session identity from the environment and from the
    /// files Claude Code keeps under `~/.claude`. Every field is optional:
    /// outside Claude Code this returns an empty owner.
    pub fn detect() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let session_id = std::env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty());
        let claude_pid = std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|s| s.parse().ok());
        let Some(home) = home else {
            return Self {
                session_id,
                claude_pid,
                ..Self::default()
            };
        };
        Self::detect_in(&home.join(".claude"), session_id, claude_pid)
    }

    /// [`Owner::detect`] with an explicit `~/.claude` directory, for tests.
    pub fn detect_in(
        claude_dir: &Path,
        session_id: Option<String>,
        claude_pid: Option<u32>,
    ) -> Self {
        let mut owner = Self {
            session_id,
            claude_pid,
            ..Self::default()
        };
        if let Some(pid) = claude_pid {
            let file = claude_dir.join("sessions").join(format!("{pid}.json"));
            if let Some(v) = read_json(&file) {
                if owner.session_id.is_none() {
                    owner.session_id = str_field(&v, "sessionId");
                }
                owner.session_name = str_field(&v, "name");
                owner.proc_start = v.get("procStart").and_then(|p| match p {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                });
            }
        }
        if let Some(sid) = &owner.session_id {
            let file = claude_dir
                .join("agent-mail")
                .join(format!("identity-{sid}.json"));
            owner.agent_name = read_json(&file).and_then(|v| str_field(&v, "name"));
        }
        owner
    }

    pub fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.claude_pid.is_none()
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|s| s.as_str()).map(str::to_string)
}

/// One line of `events.jsonl`. Every field except `ts` and `op` is optional so
/// lines written by the Claude Code hook (`"source":"hook"`) parse too.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// new, fork, adopt, done, gc-remove, or add/remove from the hook.
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,
    #[serde(flatten)]
    pub owner: Owner,
}

impl Default for Event {
    fn default() -> Self {
        Self {
            ts: OffsetDateTime::UNIX_EPOCH,
            op: String::new(),
            source: None,
            repo: None,
            tag: None,
            path: PathBuf::new(),
            branch: None,
            base: None,
            ttl: None,
            expires_at: None,
            cwd: None,
            pid: None,
            ppid: None,
            owner: Owner::default(),
        }
    }
}

impl Event {
    /// A new event for `op` on `path`, stamped with now, this process and the
    /// current session.
    pub fn now(op: &str, path: &Path) -> Self {
        Self {
            ts: OffsetDateTime::now_utc(),
            op: op.to_string(),
            source: Some("grove".to_string()),
            path: path.to_path_buf(),
            cwd: std::env::current_dir().ok(),
            pid: Some(std::process::id()),
            ppid: parent_pid(),
            owner: Owner::detect(),
            ..Self::default()
        }
    }

    /// An event describing a registered project.
    pub fn for_project(
        op: &str,
        repo: &str,
        tag: &str,
        project: &crate::registry::Project,
    ) -> Self {
        Self {
            repo: Some(repo.to_string()),
            tag: Some(tag.to_string()),
            branch: Some(project.branch.clone()),
            base: Some(project.base.clone()),
            expires_at: project.expires_at,
            ..Self::now(op, &project.path)
        }
    }

    pub fn is_removal(&self) -> bool {
        matches!(self.op.as_str(), "done" | "gc-remove" | "remove")
    }
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn logging on. Only the `grove` binary calls this, so tests and other
/// library users that drive `new`/`done` in-process never touch the real log.
pub fn enable() {
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// `$GROVE_EVENT_LOG` when set, else `$XDG_STATE_HOME/grove/events.jsonl`,
/// falling back to `~/.local/state/grove/events.jsonl`.
pub fn log_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GROVE_EVENT_LOG").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("grove").join("events.jsonl"))
}

/// Append one event as a single JSON line. The line goes out in one `write`
/// on an `O_APPEND` handle, so concurrent writers never interleave mid-line.
pub fn append_to(path: &Path, event: &Event) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)
}

/// Append to the default log. Never fails: a problem becomes a warning.
pub fn record(event: &Event) {
    if ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        write_or_warn(log_path(), event);
    }
}

fn write_or_warn(path: Option<PathBuf>, event: &Event) {
    let Some(path) = path else {
        eprintln!("grove: warning: no HOME or XDG_STATE_HOME; event not logged");
        return;
    };
    if let Err(e) = append_to(&path, event) {
        eprintln!(
            "grove: warning: could not write event log {}: {e}",
            path.display()
        );
    }
}

/// Every parseable event in the log, oldest first. Unreadable lines are
/// skipped; a missing file is an empty log.
pub fn read_all(path: &Path) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(op: &str) -> Event {
        Event {
            ts: OffsetDateTime::UNIX_EPOCH,
            op: op.to_string(),
            source: Some("grove".to_string()),
            repo: Some("r".to_string()),
            tag: Some("t".to_string()),
            path: PathBuf::from("/w/t"),
            owner: Owner {
                session_id: Some("sid-1".to_string()),
                session_name: Some("my session".to_string()),
                ..Owner::default()
            },
            ..Event::default()
        }
    }

    #[test]
    fn writes_one_json_line_per_event() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("sub/events.jsonl");
        append_to(&log, &sample("new")).unwrap();
        append_to(&log, &sample("done")).unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));

        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["op"], "new");
        assert_eq!(v["ts"], "1970-01-01T00:00:00Z");
        // Owner fields are flattened into the top level.
        assert_eq!(v["session_id"], "sid-1");
        assert_eq!(v["session_name"], "my session");
        assert!(v.get("owner").is_none());
        assert!(v.get("agent_name").is_none(), "empty fields are omitted");

        let events = read_all(&log);
        assert_eq!(events.len(), 2);
        assert!(events[1].is_removal());
        assert_eq!(events[0].owner.session_id.as_deref(), Some("sid-1"));
    }

    #[test]
    fn write_failure_is_an_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        // A regular file where the parent directory should be.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let log = blocker.join("events.jsonl");
        assert!(append_to(&log, &sample("new")).is_err());
    }

    #[test]
    fn unwritable_log_only_warns() {
        let dir = TempDir::new().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        // Returns normally: no panic, no error to propagate.
        write_or_warn(Some(blocker.join("grove/events.jsonl")), &sample("new"));
        write_or_warn(None, &sample("new"));
    }

    #[test]
    fn record_is_off_until_enabled() {
        // Library callers (tests included) never reach the real log.
        assert!(!ENABLED.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn hook_lines_with_few_fields_parse() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("events.jsonl");
        std::fs::write(
            &log,
            "{\"ts\":\"2026-10-05T10:00:00Z\",\"op\":\"add\",\"source\":\"hook\",\"path\":\"/w/x\",\"session_id\":\"s\"}\nnot json\n",
        )
        .unwrap();
        let events = read_all(&log);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source.as_deref(), Some("hook"));
        assert_eq!(events[0].owner.session_id.as_deref(), Some("s"));
    }

    #[test]
    fn detect_reads_session_and_agent_files() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path();
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        std::fs::create_dir_all(claude.join("agent-mail")).unwrap();
        std::fs::write(
            claude.join("sessions/42.json"),
            r#"{"pid":42,"sessionId":"abc","procStart":"777","name":"lane"}"#,
        )
        .unwrap();
        std::fs::write(
            claude.join("agent-mail/identity-abc.json"),
            r#"{"name":"BlueFox"}"#,
        )
        .unwrap();
        let owner = Owner::detect_in(claude, None, Some(42));
        assert_eq!(owner.session_id.as_deref(), Some("abc"));
        assert_eq!(owner.session_name.as_deref(), Some("lane"));
        assert_eq!(owner.proc_start.as_deref(), Some("777"));
        assert_eq!(owner.agent_name.as_deref(), Some("BlueFox"));
    }
}
