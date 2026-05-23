# Agent instructions — grove

This repo is being built by a swarm of Claude Code teammates implementing
beads from `.beads/issues.jsonl`. Read this file in full before claiming any
bead.

## Repo layout

- `docs/00-master-plan.md` — phased roadmap, dependency graph, locked decisions.
- `docs/01-architecture.md` — schema, crate layout, migration spec.
- `docs/02-cli-ux.md` — subcommand inventory, output formats, completions.
- `docs/03-git-internals.md` — gix vs shell-fallback split per operation.
- `docs/04-launch-test-ci.md` — terminal abstraction, tests, CI, release.
- `.beads/issues.jsonl` — work units (read via `br show <id>`).

## How to claim and complete a bead

1. `br ready` — find unblocked beads (`status=open`, no open `blockedBy`).
2. `br show <id>` — read full description (Context, Acceptance Criteria, Files, Tests, Dependencies, Notes).
3. **Reserve** every file you intend to edit via the `mcp-agent-mail` MCP
   (`file_reservation_paths`) BEFORE touching it.
4. Implement against the acceptance criteria. Add unit + integration tests as
   specified.
5. Verify locally:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   ```
6. **Commit atomically** — one bead = one commit:
   ```bash
   git add <files>
   git commit -m "feat(<bead-id>): <short description>"
   ```
   Do NOT batch multiple beads into one commit.
7. **Release** file reservations via `release_file_reservations` after the commit.
8. Close the bead: `br close <id> -m "<commit-sha>"`.
9. Exit. Do not pick up another bead — the leader spawns a fresh teammate.

## Hard rules

- **No `isolation: "worktree"`** for implementation work. All teammates commit
  to the same branch (`main`).
- **Atomic commits.** One bead → one commit. Always. If a bead would require
  5+ files, stop and ask the leader to split it.
- **Commit before building large files.** If a bead produces a >500-LOC file,
  commit it first, then run `cargo build` / `cargo test`. Fix follow-ups via
  `git commit --amend --no-edit` only on YOUR commit.
- **No `cargo fix --broken-code` shortcuts.** Fix root causes.
- **Never modify another bead's files outside your reservation.** If you find
  a bug in someone else's code, surface it via agent-mail and file a new bead.

## Tech-stack canon

| Concern | Choice |
|---------|--------|
| CLI parsing | `clap` v4 derive |
| Git ops (inspect) | `gix` 0.83+ |
| Git ops (worktree add/remove/move, fetch, push) | shell out to `git` |
| Parallelism | `rayon` (no tokio) |
| Tables | `comfy-table` |
| Colors | `owo-colors` + `is-terminal` |
| Errors | `thiserror` in domain, `anyhow` at CLI boundary, `color-eyre` reporter |
| Logging | `tracing` + `tracing-subscriber` |
| Tests | `assert_cmd`, `predicates`, `insta`, `tempfile` |
| Time | `time` crate (not chrono) |
| Path normalization | `dunce` for Windows; custom WSL helpers in `src/paths.rs` |
| Release | `cargo-dist` |

Add **only** the dep you need for your bead. Don't introduce a new dep
category without flagging it.

## Code style

- Edition 2024, MSRV 1.89 (edition 2024 requires 1.85+; we pin 1.89).
- `#![warn(clippy::pedantic)]` is NOT enabled; default clippy is the bar.
- No `unwrap()` outside tests. Use `?` and propagate.
- No comments that restate what the code does. Only WHY when non-obvious.
- No `mod.rs` files for modules with a single file — use `<name>.rs` at the
  parent level instead.

## When in doubt

Ask the leader via `SendMessage` rather than guessing. Splitting a bead is
better than implementing the wrong thing.
