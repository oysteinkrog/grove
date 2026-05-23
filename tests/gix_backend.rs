// Integration tests for the gix backend are in src/git/gix_backend.rs as
// #[cfg(test)] unit tests because the grove binary crate has no library target.
// This file is kept to satisfy the bead spec file requirement and will host
// CLI-level integration tests once grove exposes a worktree-list subcommand.
