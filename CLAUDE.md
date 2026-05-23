# Claude Code instructions — grove

This file is symlink-equivalent guidance for Claude Code agents. The full
agent contract is in `AGENTS.md` — read that first.

## Quick start for a bead-implementation teammate

1. Read `AGENTS.md`.
2. Read `docs/00-master-plan.md` for context.
3. `br show <your-bead-id>` for the spec.
4. Reserve files via `mcp-agent-mail`.
5. Implement, test, commit, close, release reservations, exit.

## Critical safeguards

- No `cargo run` against the user's real `~/.config/grove/` until Phase 1
  validation gate (`grove-ktk.6`) passes. Run only against tempdir fixtures.
- The legacy Python `~/bin/grove` is **still in production use**. Do not
  remove it. Cutover happens in `grove-1bt.4`.
- Never push to `main` on `oysteinkrog/grove` without running `cargo test`
  locally first. CI is the safety net, not the first check.
