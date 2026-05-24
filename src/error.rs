use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GroveError {
    #[error("worktree not found: {0}")]
    WorktreeNotFound(PathBuf),

    #[error("cannot determine which repo to use\nhint: {hint}")]
    RepoDiscovery { hint: String },

    #[error("repo '{id}' not found in repos.json")]
    RepoNotFound { id: String },

    #[error("git command failed: {cmd}\n{stderr}")]
    GitCommandFailed { cmd: String, stderr: String },

    #[error("tag '{tag}' already exists at {existing_path}")]
    DuplicateTag { tag: String, existing_path: PathBuf },

    #[error("invalid tag '{tag}': {reason}")]
    InvalidTag { tag: String, reason: String },

    #[error("registry error: {msg}")]
    Registry { msg: String },

    #[error("path is not a git worktree: {path}")]
    WorktreeInvalid { path: std::path::PathBuf },

    #[error("path is not a git repo: {path}")]
    NotAGitRepo { path: std::path::PathBuf },

    #[error("repo id '{id}' already exists in repos.json")]
    DuplicateRepoId { id: String },

    #[error("tag '{tag}' is ambiguous; try: {}", candidates.join(", "))]
    AmbiguousTag {
        tag: String,
        candidates: Vec<String>,
    },

    #[error("unknown tag '{tag}'{}", hint.as_deref().map(|h| format!(" — did you mean '{h}'?")).unwrap_or_default())]
    UnknownTag { tag: String, hint: Option<String> },
}

pub type Result<T> = std::result::Result<T, GroveError>;
