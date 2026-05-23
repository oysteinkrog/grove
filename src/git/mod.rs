pub mod gix_backend;

use std::path::Path;

use crate::error::GroveError;

#[allow(unused_imports)]
pub use gix_backend::GixBackend;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: std::path::PathBuf,
    pub branch: Option<String>,
    pub head: Option<gix::ObjectId>,
}

#[derive(Debug)]
pub struct Worktree {
    branch: Option<String>,
    head: Option<gix::ObjectId>,
}

impl Worktree {
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn head(&self) -> Option<gix::ObjectId> {
        self.head
    }
}

pub trait WorktreeManager {
    fn list(&self, main: &Path) -> Result<Vec<WorktreeInfo>, GroveError>;
    fn open(&self, path: &Path) -> Result<Worktree, GroveError>;
}
