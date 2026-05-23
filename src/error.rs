use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GroveError {
    #[error("worktree not found: {0}")]
    WorktreeNotFound(PathBuf),
}
