pub mod gix_backend;
pub mod shell_backend;
pub mod status;

use std::path::{Path, PathBuf};

use crate::error::GroveError;

#[allow(unused_imports)]
pub use gix_backend::GixBackend;
#[allow(unused_imports)]
pub use shell_backend::ShellBackend;
#[allow(unused_imports)]
pub use status::{Status, compute};

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: std::path::PathBuf,
    pub branch: Option<String>,
    pub head: Option<gix::ObjectId>,
}

#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
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

pub trait WorktreeMutator {
    fn worktree_add(
        &self,
        repo_path: &Path,
        target: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<(), GroveError>;
    fn worktree_remove(
        &self,
        repo_path: &Path,
        target: &Path,
        force: bool,
    ) -> Result<(), GroveError>;
    fn worktree_move(&self, repo_path: &Path, old: &Path, new: &Path) -> Result<(), GroveError>;
}
