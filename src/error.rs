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
}

pub type Result<T> = std::result::Result<T, GroveError>;
