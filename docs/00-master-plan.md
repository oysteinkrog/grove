# grove Rust Rewrite — Master Plan

Synthesis of four parallel Opus planning agents. Each detail document below is
the source of truth for its facet; this master plan resolves cross-cutting
decisions and lays out a phased implementation roadmap.

## Detail documents

| # | Facet | File | Lines |
|---|-------|------|-------|
| 01 | Architecture & schema | `01-architecture.md` | 921 |
| 02 | CLI UX & commands | `02-cli-ux.md` | 905 |
| 03 | Git/gix internals | `03-git-internals.md` | 749 |
| 04 | Launch, test, CI, packaging | `04-launch-test-ci.md` | 945 |

## Locked-in decisions

These were settled by the user before agents were spawned, plus the
post-synthesis follow-up round.

### Pre-agent decisions
- **Multi-repo model:** Hybrid — global `~/.config/grove/repos.json` index +
  per-repo `<work_dir>/.grove/{config.json,registry.json}`.
- **Migration:** Auto-migrate on first run, with `.pre-rust-migration` backups.
- **Source/binary:** New public repo `oysteinkrog/grove`, binary `grove`,
  replaces `~/bin/grove`.
- **Git backend:** gix (gitoxide) where stable, shell out to `git` for gaps.

### Post-agent decisions (resolving agent open questions)

| Question | Decision |
|----------|----------|
| Default scope for `grove list` | All repos grouped by repo, **current repo first**. `--repo <id>` filters; no separate `--all` flag needed. |
| Worktree mutations backend | Pure gix where possible (list/prune/inspect/refs/status). Shell out **only** for `worktree add`, `worktree remove`, `worktree move`, fetch, and push. |
| Issue-prefix model | Per-repo `issue_prefix` field in `repos.json`. Migrated `desktop` repo gets `DESKTOP`. New repos prompt during `grove repo add` (with `--issue-prefix` flag). |
| `grove fork` arg signature | Keep Python behavior: 1 or 2 positionals. One-arg form infers source from cwd. |
| `--no-fetch` on `grove new` | Default off (fetch happens). Flag opts out for slow/LFS repos. |
| Telemetry | None, ever. Explicit policy in README. |
| MSRV | Rust 1.80 (cargo-dist friendly). |
| JSON output schema | `"version": 1` on every JSON-emitting command. Bump on breaking change. |

## Cross-cutting reconciliation

Where the four agents proposed overlapping or near-conflicting designs, the
synthesis below picks one.

### `repo/RepoContext` ownership
Agent 01 introduced `RepoContext` (resolved union of global + per-repo
settings). Agents 02 and 04 reference per-repo data via separate paths.
**Resolution:** all command handlers receive a `RepoContext` parameter,
constructed once in `main.rs` after cwd-based repo discovery. Handlers never
touch `GlobalConfig`/`RepoConfig` raw types.

### Error model split
Agent 01: `thiserror` in domain + `anyhow` at CLI boundary, with typed
`GroveError` and `hint_for()` dispatch.
Agent 02: `miette` was floated for pretty diagnostics.
**Resolution:** stick with `thiserror` + `anyhow`. Add `color-eyre` as
report formatter at `main.rs` if needed. No `miette` — its source-span model
doesn't fit this domain.

### Path normalization location
Both agents 01 and 04 propose path bridging. Agent 04's pure-Rust 10-line
`/c/work/...` → `C:\work\...` function is the canonical impl. It lives in
`src/paths.rs` (single home, used by both `launch/` and `git/`).

### Repo discovery order
Agent 01's algorithm is authoritative:
1. `$GROVE_ORIG_CWD` walk-up for `.grove/registry.json`
2. `work_dir` prefix match across all repos in `repos.json`
3. Fall back to `default_repo` field if set
4. If still ambiguous, error with a hint listing candidates

### `grove list` rendering
Agent 02 spec'd the table format; agent 04 covered async fan-out.
**Resolution:** rayon (not tokio) for parallel status scanning across N×M
worktrees. Grove is CPU/IO-bound on git operations, not network-bound, and
rayon needs no async runtime. Output: `comfy-table` with sections per repo,
current-cwd repo printed first.

### Test infrastructure
All four agents propose tempdir + real git repos for integration tests. **One
shared `tests/common/mod.rs`** exposes a `GitFixture` builder that creates
bare/working repos, plants worktrees, and tears down. `assert_cmd` +
`predicates` for CLI assertions, `insta` for snapshot tests of help/JSON/table
output.

## Module tree (final)

Derived from agent 01, with minor renames for consistency.

```
grove/
├── Cargo.toml
├── README.md
├── LICENSE-APACHE
├── LICENSE-MIT
├── dist-workspace.toml          # cargo-dist config
├── .github/workflows/
│   ├── ci.yml
│   └── release.yml              # cargo-dist-generated
├── src/
│   ├── main.rs                  # CLI entry, color-eyre install, dispatch
│   ├── cli/                     # one file per subcommand
│   ├── config/                  # GlobalConfig (repos.json), RepoConfig (per-repo)
│   ├── registry/                # Registry CRUD, Project struct
│   ├── repo/                    # RepoContext + discovery
│   ├── git/
│   │   ├── mod.rs               # WorktreeManager trait
│   │   ├── gix_backend.rs       # gix impl (list/inspect/status/refs)
│   │   ├── shell_backend.rs     # git CLI impl (add/remove/move/fetch/push)
│   │   └── status.rs            # dirty/ahead/behind composition
│   ├── launch/
│   │   ├── mod.rs               # Terminal trait + autodetect
│   │   ├── windows_terminal.rs
│   │   └── wezterm.rs
│   ├── migrate/                 # one-shot legacy → hybrid migration
│   ├── paths.rs                 # WSL ↔ Windows path bridging
│   ├── display.rs               # color, comfy-table renderers
│   └── error.rs                 # GroveError + hint dispatch
├── tests/
│   ├── common/
│   │   └── mod.rs               # GitFixture, helpers
│   ├── new.rs
│   ├── fork.rs
│   ├── list.rs
│   ├── done.rs
│   ├── migrate.rs
│   └── snapshots/               # insta
└── completions/                 # generated by build.rs into target/, not committed
```

## Dependency budget

| Crate | Purpose | Notes |
|-------|---------|-------|
| `clap` (derive) | CLI parsing | v4 |
| `clap_complete` | shell completions | hidden `__completions` subcommand |
| `serde` + `serde_json` | config/registry I/O | |
| `gix` | git operations | `0.83.x` minimum; default features off, opt in to needed sub-features |
| `rayon` | parallel status scans | not tokio |
| `comfy-table` | `grove list` output | |
| `console` or `owo-colors` | ANSI color | pick one — agent 02 leans `owo-colors` |
| `is-terminal` | TTY detection | |
| `strsim` | near-match suggestions | "did you mean" |
| `thiserror` | domain errors | |
| `anyhow` | boundary errors | |
| `color-eyre` | report formatter at `main()` | |
| `tracing` + `tracing-subscriber` | logging | `-v`/`-vv`, `RUST_LOG` |
| `directories` | XDG config path resolution | |
| `chrono` or `time` | timestamps | agent 01 leans `time` |
| `dunce` | Windows path canonicalization | for paths crossing WSL |
| `assert_cmd`, `predicates`, `insta`, `tempfile` | dev-only | |

Hard cap target: ≤ 30 direct deps. No tokio. No async ecosystem unless a
specific feature forces it.

## Phased implementation roadmap

Each phase ends with a working binary you can use day-to-day. Beads can be
generated from this roadmap when ready.

### Phase 0 — Scaffold (0.5 day)
- New repo `oysteinkrog/grove` initialized with MIT/Apache-2.0 dual license.
- `cargo init --bin`, Rust edition 2024, MSRV 1.80.
- Skeleton modules, empty handlers.
- `cargo-dist init` configured for 5 targets.
- CI workflow (clippy, fmt, test) green on empty crate.

### Phase 1 — Schema, registry, migration (1–2 days)
- Implement `GlobalConfig` (`repos.json`), `RepoConfig`, `Registry` with
  schema_version, serde round-trip tests.
- Implement migration from existing `~/.config/grove/{config,registry}.json`
  with `.pre-rust-migration` backups.
- `RepoContext` discovery (cwd walk-up + global fallback + default repo).
- **Validation gate:** auto-migrate Oystein's real config, verify resulting
  files round-trip and contain identical project data.

### Phase 2 — Read-only commands (1–2 days)
- `grove list` (default + `--short` + `--json` + `--repo`)
- `grove path <tag>`
- `grove cd <tag>` (prints path; fish wrapper unchanged)
- `grove status` (per-project status detail)
- gix-backed status detection with rayon fan-out.
- Snapshot tests for table/JSON output.
- **Validation gate:** `grove list` output matches current Python output for
  the existing desktop registry (modulo cosmetic improvements).

### Phase 3 — Worktree mutations (2–3 days)
- `grove new <tag> [--issue N] [--branch] [--base] [--no-fetch]`
- `grove fork <new_tag>` and `grove fork <source> <new_tag>`
- `grove adopt <tag> <path> [--move] [--issue] [--base]`
- `grove rename <old> <new> [--no-move]`
- `grove freeze` / `grove thaw`
- `grove done <tag> [--force] [--keep-local] [--keep-remote]` with safety
  checks (uncommitted/unpushed work detection via gix).
- `WorktreeManager` trait: gix for inspection, shell for add/remove/move.
- Integration tests with `GitFixture`.
- **Validation gate:** end-to-end on a throwaway repo: new → adopt → rename
  → freeze → done.

### Phase 4 — Multi-repo + repo management (1–2 days)
- `grove repo add <path> [--id <id>] [--issue-prefix <PFX>] [--default]`
- `grove repo list [--json]`
- `grove repo remove <id>`
- `grove repo show [<id>]`
- `grove repo default <id>` (or `--default` on `add`)
- Cross-repo `grove list` with grouping (current cwd's repo first).
- Tag disambiguation logic + "did you mean" hints with strsim.
- **Validation gate:** register a second repo (e.g. `~/.dotfiles`),
  create worktrees in each, `grove list` groups them correctly.

### Phase 5 — Launch (1 day)
- `Terminal` trait + `WindowsTerminal` and `Wezterm` impls.
- WSL ↔ Windows path bridging (`paths.rs`).
- Terminal autodetection (`WEZTERM_PANE`, `WT_SESSION`, PATH probe).
- `grove launch [--only tag,tag] [--dry-run] [--no-claude] [--terminal <name>]`
- Mock terminal trait impl for tests.
- **Validation gate:** `grove launch --dry-run` prints correct command lines;
  real launch opens N tabs with the right cwd in each.

### Phase 6 — Polish + completions + release (1 day)
- `clap_complete` shell completions (fish + bash + zsh + pwsh).
- Color/no-color polish, `NO_COLOR` support, table width detection.
- `tracing-subscriber` wiring for `-v`/`-vv` and `RUST_LOG`.
- README with install instructions (cargo binstall primary).
- `--version` output includes git sha + build date.
- cargo-dist v1 release: `git tag v0.1.0 && git push --tags` triggers the
  release workflow; artifacts on GitHub Releases, installer script generated.
- **Cutover:** `cargo binstall grove` installs to `~/.cargo/bin/grove`
  (ahead of `~/bin/` on PATH). Old Python script renamed to `grove.py`
  inside dotfiles for reference; `~/bin/grove` symlink removed.

### Phase 7 — Future (not in 0.1.0)
- `grove tui` interactive mode (ratatui).
- Swap shell-out `worktree add` for native gix when gix issue #2596 ships.
- gix-native push when gix push stabilizes.
- Optional integration with beads (`br`) for issue-prefixed branch creation.
- macOS/Linux first-class testing (currently developed on WSL).

## Total estimated effort

7–10 focused dev days for the full 0.1.0. Phases 1–3 alone (about 4–6 days)
deliver a functional single-repo replacement that the user can adopt
immediately. Multi-repo (phase 4) and launch (phase 5) can come right
after.

## Risks (top 5)

1. **gix worktree-add gap.** Mitigated by `WorktreeManager` trait + shell
   backend (decided). Re-evaluate at every gix release.
2. **WSL path edge cases on Windows-written repos.** Mitigated by `dunce`
   crate and explicit normalization in `paths.rs`; integration tests cover
   both directions.
3. **Migration data loss.** Mitigated by `.pre-rust-migration` backups +
   dry-run-able migration logic + golden-file test of real config.
4. **clap fork-arg parsing.** Mitigated by `Vec<String>` positional with
   length-1-or-2 validation in handler.
5. **cargo-dist quirks.** Mitigated by following cargo-dist's quickstart
   exactly for 0.1.0; defer customization.

## Non-goals for 0.1.0

- No TUI.
- No web UI.
- No daemon mode.
- No telemetry. Ever.
- No update-check pings.
- No plugin system.
- No cross-machine sync (per-host registry is fine).
- No editing config files via a friendly UI — `grove config edit` opens
  `$EDITOR`, that's it.
