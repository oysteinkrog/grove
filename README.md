# grove

Fast multi-repo git-worktree manager. Rust rewrite of the original Python
[grove](https://github.com/oysteinkrog/dotfiles) script.

> **Status:** scaffolding. Not yet usable. See `docs/00-master-plan.md` for the
> roadmap and `.beads/issues.jsonl` for tracked work.

## Goals

- Manage worktrees across multiple git repos from one CLI.
- Launch terminal tabs (Windows Terminal, WezTerm) with one command per project.
- Native gix backend where possible; shells out to `git` only where gix has gaps.
- Auto-migrate the legacy single-repo config layout.

## Install (planned)

```bash
cargo binstall grove
# or
cargo install grove
```

## Development

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## License

Dual-licensed under either of MIT or Apache-2.0, at your option.
