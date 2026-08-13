# grove

Fast multi-repo git-worktree manager. Rust rewrite of the original Python
`grove` script.

Manage worktrees across multiple git repos with a single CLI: create, list,
adopt, fork, move, freeze, and clean up worktrees from one place. Launch
terminal tabs (Windows Terminal, WezTerm) with one command per project.

## Install

```bash
# Preferred: prebuilt binary via cargo-binstall
cargo binstall grove

# Fallback: build from source
cargo install grove

# Or grab a release tarball from
# https://github.com/oysteinkrog/grove/releases
```

## Quick start

```bash
# Register a repo (once per repo)
grove repo add /path/to/myrepo --id myrepo --default

# Create a new worktree from main
grove new feature-foo

# Create one tied to an issue number
grove new feature-bar --issue 1234

# List worktrees with git status
grove list

# Jump into a worktree (prints path for shell integration)
grove cd feature-foo

# Adopt an existing worktree into the registry
grove adopt feature-baz /path/to/worktree

# Fork an existing project's branch
grove fork feature-foo feature-foo-2

# Remove a worktree (with safety checks)
grove done feature-foo

# Launch terminal tabs with Claude Code for all projects
grove launch
```

## Configuration

State lives in `~/.config/grove/`:

- `repos.json` — registered repositories
- `<repo>/registry.json` — per-repo worktree registry
- `<repo>/config.json` — per-repo config (issue prefix, base branch, etc.)

The legacy single-repo layout is migrated automatically on first run.

## Subcommands

| Command | Purpose |
|---|---|
| `grove new <tag>` | Create a new worktree |
| `grove fork <src> <dst>` | Fork an existing project |
| `grove list [--short \| --json]` | Show all projects with git status |
| `grove status <tag>...` | Detailed git status for tags |
| `grove path <tag>` | Print worktree path |
| `grove cd <tag>` | Print path for shell integration |
| `grove adopt <tag> <path>` | Import an existing worktree |
| `grove rename <old> <new>` | Rename a project |
| `grove freeze <tag>` / `thaw` | Exclude from `grove launch` |
| `grove done <tag>` | Remove a worktree (safety-checked) |
| `grove gc [--dry-run] [--yes]` | Audit the work_dir and clean up what is safe |
| `grove launch` | Open terminal tabs for projects |
| `grove repo {add,list,show,remove,default,path}` | Manage repos |

Add `--repo <id>` to any command to target a specific repo. Run
`grove <cmd> --help` for full flag reference.

## Garbage collection

`grove gc` sorts a work_dir that has drifted into eight numbered categories and
fixes the four that are pure bookkeeping. `--dry-run` reports and stops; `--yes`
applies without asking; with neither, gc asks item by item when it has a
terminal and reports only when it does not.

| # | Finding | What gc does |
|---|---|---|
| 1 | Registry entry whose worktree is gone | drops the entry with `--yes` |
| 2 | Unregistered worktree under work_dir | asks to adopt or remove; never swept by `--yes` |
| 3 | Top-level directory that is not a worktree | report only |
| 4 | Expired ephemeral under `.scratch` | removes with `--yes` when clean, pushed and idle |
| 5 | Stale harness worktree under `.claude/worktrees` | removes with `--yes` when clean, stale and idle |
| 6 | Prunable git worktree metadata | prunes with `--yes` |
| 7 | Merged-and-clean project | lists it as a `grove done` candidate |
| 8 | `.archive` contents | reports age and size |

There is no `--force`. Before removing anything gc re-checks the tree with
`git status --porcelain --untracked-files=all`, confirms HEAD is reachable from
some remote, and skips any tree that looks in use — recent commits or mtime, a
live process working directory inside it, or an agent-mail file reservation
covering it. Deleted branches are appended to
`<work_dir>/.archive/deleted-branches.txt` as `DIR BRANCH SHA`.

## Shell integration

```bash
# Generate completions
grove __completions bash > ~/.local/share/bash-completion/completions/grove
grove __completions fish > ~/.config/fish/completions/grove.fish
grove __completions zsh  > ~/.zfunc/_grove

# `grove cd` prints a path — wrap it in a shell function:
gr() {
  if [ -z "$1" ]; then grove list --short; else cd "$(grove cd "$1")"; fi
}
```

## Development

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
