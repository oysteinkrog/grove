use time::OffsetDateTime;
use tracing::warn;

use crate::error::{GroveError, Result};
use crate::git::WorktreeMutator;
use crate::git::shell_backend::ShellBackend;
use crate::registry::Project;
use crate::repo::RepoContext;

pub struct NewArgs {
    pub tag: String,
    pub issue: Option<u32>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub no_fetch: bool,
}

pub fn run(args: &NewArgs, cx: &RepoContext) -> Result<()> {
    validate_tag(&args.tag)?;

    if cx.registry.projects.contains_key(&args.tag) {
        let existing_path = cx.registry.projects[&args.tag].path.clone();
        return Err(GroveError::DuplicateTag {
            tag: args.tag.clone(),
            existing_path,
        });
    }

    let branch = compute_branch_name(
        &args.tag,
        args.issue,
        args.branch.as_deref(),
        cx.resolved.issue_prefix.as_deref(),
    );

    let base_ref = compute_base_ref(
        args.base.as_deref(),
        &cx.resolved.upstream_remote,
        &cx.resolved.default_base,
    );

    let target = cx.resolved.work_dir.join(&args.tag);

    let backend = ShellBackend::new();

    if !args.no_fetch {
        backend.fetch(&cx.resolved.main_repo, &cx.resolved.upstream_remote)?;
    }

    backend.worktree_add(&cx.resolved.main_repo, &target, &branch, Some(&base_ref))?;

    let project = Project {
        path: target.clone(),
        branch: branch.clone(),
        base: base_ref.clone(),
        created: OffsetDateTime::now_utc(),
        issue: args.issue,
        frozen: false,
    };

    let mut registry = cx.registry.clone();
    registry
        .insert(args.tag.clone(), project)
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;
    registry
        .save(&cx.grove_dir())
        .map_err(|e| GroveError::Registry { msg: e.to_string() })?;

    println!("Created project '{}'", args.tag);
    if let Some(issue) = args.issue {
        println!(
            "  Issue:  {}-{issue}",
            cx.resolved.issue_prefix.as_deref().unwrap_or("ISSUE")
        );
    }
    println!("  Branch: {branch}");
    println!("  Base:   {base_ref}");
    println!("  Path:   {}", target.display());

    Ok(())
}

pub fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() || tag.len() > 40 {
        return Err(GroveError::InvalidTag {
            tag: tag.to_string(),
            reason: format!("length must be 1–40, got {}", tag.len()),
        });
    }
    if tag.contains('/') || tag.contains(char::is_whitespace) {
        return Err(GroveError::InvalidTag {
            tag: tag.to_string(),
            reason: "must not contain slashes or whitespace".to_string(),
        });
    }
    Ok(())
}

pub fn compute_branch_name(
    tag: &str,
    issue: Option<u32>,
    branch_override: Option<&str>,
    issue_prefix: Option<&str>,
) -> String {
    if let Some(b) = branch_override {
        if issue.is_some() {
            warn!("Both --branch and --issue supplied; --branch takes precedence");
        }
        return b.to_string();
    }
    if let Some(n) = issue {
        let prefix = issue_prefix.unwrap_or("ISSUE");
        return format!("{prefix}-{n}-{tag}");
    }
    tag.to_string()
}

/// Expand a user-supplied base ref into a fully-qualified remote ref.
///
/// Rules (mirroring the Python `cmd_new`):
/// - No `/` in base → check if it looks like a version (digits.digits prefix) → prepend
///   `<upstream>/stable/`; otherwise prepend `<upstream>/`.
/// - Already contains `/` → use as-is.
/// - No `--base` supplied → `<upstream>/<default_base>`.
pub fn compute_base_ref(base: Option<&str>, upstream: &str, default_base: &str) -> String {
    match base {
        None => format!("{upstream}/{default_base}"),
        Some(b) if b.contains('/') => b.to_string(),
        Some(b) => {
            if looks_like_version(b) {
                format!("{upstream}/stable/{b}")
            } else {
                format!("{upstream}/{b}")
            }
        }
    }
}

fn looks_like_version(s: &str) -> bool {
    // e.g. "25.3", "1.0", "2024.10"
    let mut parts = s.splitn(2, '.');
    let major = parts.next().unwrap_or("");
    let minor = parts.next().unwrap_or("");
    !major.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && !minor.is_empty()
        && minor.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── branch name computation ─────────────────────────────────────────────

    #[test]
    fn branch_no_issue_no_override() {
        let b = compute_branch_name("lazy-vm", None, None, None);
        assert_eq!(b, "lazy-vm");
    }

    #[test]
    fn branch_with_issue_and_prefix() {
        let b = compute_branch_name("lazy-vm", Some(9947), None, Some("DESKTOP"));
        assert_eq!(b, "DESKTOP-9947-lazy-vm");
    }

    #[test]
    fn branch_with_issue_no_prefix_falls_back() {
        let b = compute_branch_name("foo", Some(42), None, None);
        assert_eq!(b, "ISSUE-42-foo");
    }

    #[test]
    fn branch_override_wins_over_issue() {
        let b = compute_branch_name("foo", Some(42), Some("my-custom-branch"), Some("DESK"));
        assert_eq!(b, "my-custom-branch");
    }

    #[test]
    fn branch_override_no_issue() {
        let b = compute_branch_name("foo", None, Some("explicit"), None);
        assert_eq!(b, "explicit");
    }

    // ── base ref computation ────────────────────────────────────────────────

    #[test]
    fn base_none_uses_default() {
        let r = compute_base_ref(None, "if", "master");
        assert_eq!(r, "if/master");
    }

    #[test]
    fn base_version_gets_stable_prefix() {
        let r = compute_base_ref(Some("25.3"), "if", "master");
        assert_eq!(r, "if/stable/25.3");
    }

    #[test]
    fn base_version_single_digit_segments() {
        let r = compute_base_ref(Some("1.0"), "if", "master");
        assert_eq!(r, "if/stable/1.0");
    }

    #[test]
    fn base_plain_name_no_stable_prefix() {
        let r = compute_base_ref(Some("develop"), "if", "master");
        assert_eq!(r, "if/develop");
    }

    #[test]
    fn base_already_fully_qualified() {
        let r = compute_base_ref(Some("if/some/branch"), "if", "master");
        assert_eq!(r, "if/some/branch");
    }

    // ── tag validation ──────────────────────────────────────────────────────

    #[test]
    fn tag_with_slash_invalid() {
        assert!(validate_tag("foo/bar").is_err());
    }

    #[test]
    fn tag_with_space_invalid() {
        assert!(validate_tag("foo bar").is_err());
    }

    #[test]
    fn tag_empty_invalid() {
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn tag_valid() {
        assert!(validate_tag("lazy-vm").is_ok());
        assert!(validate_tag("feature_x").is_ok());
    }

    // ── looks_like_version ──────────────────────────────────────────────────

    #[test]
    fn version_detection() {
        assert!(looks_like_version("25.3"));
        assert!(looks_like_version("1.0"));
        assert!(!looks_like_version("develop"));
        assert!(!looks_like_version("master"));
        assert!(!looks_like_version("25"));
        assert!(!looks_like_version(".3"));
    }
}
