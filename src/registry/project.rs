use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    #[serde(default)]
    pub issue: Option<u32>,
    #[serde(default)]
    pub frozen: bool,
    /// Absolute UTC expiry for ephemeral worktrees (`grove new/fork --ephemeral`).
    /// `None` means the project is durable and never expires. Presence of this
    /// field is what marks a project as ephemeral — there is no separate flag.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

impl Project {
    /// A project is ephemeral iff it carries an expiry timestamp.
    pub fn is_ephemeral(&self) -> bool {
        self.expires_at.is_some()
    }

    /// Whether the project's TTL has elapsed as of `now`. Always `false` for
    /// durable (non-ephemeral) projects. Expiry is evaluated lazily by callers
    /// (e.g. `grove list`, `grove gc`) — nothing fires at the expiry instant.
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn base_project(expires_at: Option<OffsetDateTime>) -> Project {
        Project {
            path: PathBuf::from("/c/work/test/proj"),
            branch: "main".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::now_utc(),
            issue: None,
            frozen: false,
            expires_at,
        }
    }

    #[test]
    fn durable_project_is_not_ephemeral() {
        let p = base_project(None);
        assert!(!p.is_ephemeral());
        assert!(!p.is_expired(OffsetDateTime::now_utc()));
    }

    #[test]
    fn ephemeral_project_with_future_expiry_is_not_expired() {
        let now = OffsetDateTime::now_utc();
        let p = base_project(Some(now + Duration::days(14)));
        assert!(p.is_ephemeral());
        assert!(!p.is_expired(now));
    }

    #[test]
    fn ephemeral_project_with_past_expiry_is_expired() {
        let now = OffsetDateTime::now_utc();
        let p = base_project(Some(now - Duration::days(1)));
        assert!(p.is_ephemeral());
        assert!(p.is_expired(now));
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let now = OffsetDateTime::now_utc();
        let p = base_project(Some(now));
        // expires_at == now → already expired (lazy evaluation: "at or past").
        assert!(p.is_expired(now));
    }
}
