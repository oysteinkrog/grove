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
}

pub type Result<T> = std::result::Result<T, GroveError>;
