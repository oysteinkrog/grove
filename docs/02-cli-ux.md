# grove Rust Rewrite — Facet 02: CLI Surface, UX, Output Formats, Shell Integration

## 1. Complete Subcommand Inventory

| Python subcommand | Aliases | Behavior summary | Rust subcommand | Multi-repo flag changes |
|---|---|---|---|---|
| `config --init` | — | Initialize or display `config.json` | `grove config init` | Gains `--global` / `--repo <id>` scope flags |
| `new <tag>` | — | Create worktree on new branch, launch tab | `grove new <tag>` | Add `--repo <id>` (overrides cwd-detected repo); `--base` unchanged |
| `fork [source] <tag>` | — | Branch off an existing worktree's branch | `grove fork [source] <tag>` | Add `--repo <id>` for cross-repo fork |
| `done [tag]` | — | Remove worktree + branches with safety checks | `grove done [tag]` | Add `--repo <id>` for disambiguation |
| `list` | `ls` | Full table with status, or `--short`, `--json` | `grove list` | Add `--repo <id>` (filter one repo) and `--all-repos` (global flat/grouped) |
| `launch [tags]` | — | Open terminal tabs for projects | `grove launch [tags]` | Add `--repo <id>` to scope launch |
| `adopt <tag> <path>` | — | Import existing worktree into registry | `grove adopt <tag> <path>` | Add `--repo <id>` to specify which repo to register under |
| `cd [tag]` | — | Print path (shell wrapper cds) | `grove cd [tag]` | Add `--repo <id>` for disambiguation when same tag exists in two repos |
| `path [tag]` | — | Print raw path for scripting | `grove path [tag]` | Same as above |
| `rename [old] <new>` | `mv` | Rename tag and optionally move directory | `grove rename [old] <new>` | Add `--repo <id>` |
| `freeze [tag]` | — | Freeze project; optional `--lifetime` auto-expiry | `grove freeze [tag]` | Unchanged |
| `thaw [tag]` | `unfreeze` | Unfreeze a frozen project | `grove thaw [tag]` | Unchanged |

### Flag-by-flag changes for existing subcommands

**`grove new`**
- `--issue / -i <N>` — keep (Jira issue number → branch prefix pattern, now configurable per repo)
- `--branch / -b <name>` — keep
- `--base <ref>` — keep; short numeric (`25.3`) auto-prefixes repo's upstream remote + `stable/`
- `--no-launch` — keep
- **NEW** `--repo <id>` — explicit repo selection (see §2)
- **NEW** `--no-fetch` — skip fetch step (for offline/fast use)

**`grove done`**
- `--force / -f` — keep
- `--keep-local` — keep
- `--keep-remote` — keep
- **REMOVED** no changes; the existing flags are correct

**`grove list`**
- `--all / -a` — keep (include untracked git worktrees)
- `--json` — keep; see §4 for stable schema
- `--short / -s` — keep
- `--tags-only` — keep (completion use)
- **NEW** `--repo <id>` — filter to one repo
- **NEW** `--all-repos` — show all repos grouped (see §2)
- **NEW** `--check-remote` — was present in Python parser but never wired; wire it

**`grove launch`**
- `--only` — deprecate (keep for muscle memory, warn); positional tags are the replacement
- `--dry-run` — keep
- `--no-claude` — keep
- `--wezterm` — keep
- `--frozen` — keep
- `--no-continue` — keep
- **NEW** `--repo <id>`, `--all-repos`

**`grove adopt`**
- `--base` — keep
- `--issue / -i` — keep
- `--move` — keep
- **NEW** `--repo <id>`

**`grove rename` / `mv`**
- `--no-move` — keep
- **NEW** `--repo <id>`

**`grove freeze`**
- `--lifetime / --timeout` — keep both names; `--timeout` is a compat alias

**`grove fork`**
- `--issue / -i` — keep
- `--branch / -b` — keep
- **NEW** `--repo <id>`

---

## 2. New Multi-Repo Commands

### `grove repo` subcommand group

```
grove repo add <path> [--id <id>] [--upstream <remote>] [--fork <remote>]
grove repo list [--json]
grove repo remove <id> [--force]
grove repo set <id> <key>=<value>
grove repo default <id>
```

**`grove repo add <path>`**
Creates an entry in the global index pointing at an existing git repo. The `<id>` defaults to the directory basename (e.g. `/c/work/desktop_master` → `desktop`). Each repo carries its own `upstream_remote`, `fork_remote`, `default_base`, `work_dir`, `dir_prefix`, and `issue_prefix` (e.g. `DESKTOP`).

**`grove repo list`**
```
ID         PATH                        UPSTREAM  FORK  DEFAULT_BASE
desktop    /c/work/desktop_master      if        my    master
infra      /c/work/infra               origin    my    main
```

**`grove repo remove <id>`**
Refuses if the repo still has registered projects unless `--force` is passed.

**`grove repo set <id> upstream=origin`**
Update a single field of a repo's config.

**`grove repo default <id>`**
Set the default repo (used when cwd-detection fails and `--repo` is not passed).

### How `grove new <tag>` picks a repo

Resolution order (first match wins):

1. `--repo <id>` flag — explicit override
2. cwd-based detection — walk up from cwd to find a known `main_repo` path
3. Default repo — as set by `grove repo default`
4. Error with suggestion: `grove repo default <id>`

cwd-based detection uses the global index. When inside any registered worktree path, the parent repo is inferred from the registry entry's `repo_id` field.

When `--repo` is omitted and detection is ambiguous (cwd is not inside any known worktree), the error message lists available repos:

```
error: cannot determine which repo to use
  Repos: desktop (/c/work/desktop_master), infra (/c/work/infra)
  Use --repo <id> or: grove repo default <id>
```

### Cross-repo `grove list` — flat vs grouped output

Default (`grove list`): shows projects for the cwd-detected or default repo, same as today.

`grove list --all-repos`: shows all repos, grouped. Each group is headed by the repo ID:

```
── desktop (/c/work/desktop_master) ──────────────────────────────────────
TAG            BRANCH                   STATUS        PATH
rawcodecs      DESKTOP-9947-rawcodecs   clean         /c/work/dt-rawcodecs
ci-fix         fix-master-ci            2 ahead       /c/work/dt-ci-fix

── infra (/c/work/infra) ─────────────────────────────────────────────────
TAG            BRANCH                   STATUS        PATH
deploy-v2      INFRA-42-deploy-v2       dirty         /c/work/in-deploy-v2
```

`grove list --repo desktop` — shows only one repo, same column layout as today.

`grove list --all-repos --json` — see §4 for schema.

---

## 3. Clap Derive Structure

```rust
/// Git worktree manager for named projects
#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "Git worktree manager for named projects",
    after_help = "Run 'grove <subcommand> --help' for subcommand details.",
    styles = grove::cli::styles(),
)]
pub struct Cli {
    /// Override which repo to operate on (default: detect from cwd or use default repo)
    #[arg(long, global = true, value_name = "REPO_ID")]
    pub repo: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new worktree project
    New(NewArgs),
    /// Fork an existing project into a new worktree
    Fork(ForkArgs),
    /// Remove a project worktree and clean up
    Done(DoneArgs),
    /// List projects [alias: ls]
    #[command(alias = "ls")]
    List(ListArgs),
    /// Open terminal tabs for projects
    Launch(LaunchArgs),
    /// Import existing worktree into registry
    Adopt(AdoptArgs),
    /// Change directory to a project worktree
    Cd(CdArgs),
    /// Print worktree path for a tag
    Path(PathArgs),
    /// Rename a project tag [alias: mv]
    #[command(alias = "mv")]
    Rename(RenameArgs),
    /// Freeze a project (skip in launch, listed separately)
    Freeze(FreezeArgs),
    /// Unfreeze a project [alias: unfreeze]
    #[command(alias = "unfreeze")]
    Thaw(ThawArgs),
    /// Manage repos in the global index
    Repo(RepoArgs),
    /// Manage configuration
    Config(ConfigArgs),
}

// ── Individual arg structs ───────────────────────────────────────────────────

#[derive(Args)]
pub struct NewArgs {
    /// Short project tag (e.g. rawcodecs, ci-fix)
    pub tag: String,
    /// Issue number — creates <PREFIX>-N-<tag> branch
    #[arg(short = 'i', long)]
    pub issue: Option<u32>,
    /// Explicit branch name
    #[arg(short = 'b', long)]
    pub branch: Option<String>,
    /// Base ref (e.g. master, 25.3, origin/main)
    #[arg(long)]
    pub base: Option<String>,
    /// Don't launch terminal tab after creation
    #[arg(long)]
    pub no_launch: bool,
    /// Skip fetch before creating worktree
    #[arg(long)]
    pub no_fetch: bool,
}

#[derive(Args)]
pub struct ForkArgs {
    /// Source tag (default: detect from cwd)
    pub source: Option<String>,
    /// New tag for the fork
    pub tag: String,
    #[arg(short = 'i', long)]
    pub issue: Option<u32>,
    #[arg(short = 'b', long)]
    pub branch: Option<String>,
}

#[derive(Args)]
pub struct DoneArgs {
    /// Project tag (default: detect from cwd)
    pub tag: Option<String>,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(long)]
    pub keep_local: bool,
    #[arg(long)]
    pub keep_remote: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Include untracked git worktrees
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Show all repos grouped
    #[arg(long, conflicts_with = "repo")]
    pub all_repos: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
    /// Compact one-line-per-project output
    #[arg(short = 's', long)]
    pub short: bool,
    /// Print only tag names (for completion scripts)
    #[arg(long, hide = true)]
    pub tags_only: bool,
    /// Verify remote branches exist
    #[arg(long)]
    pub check_remote: bool,
}

#[derive(Args)]
pub struct LaunchArgs {
    /// Project tags to launch (default: all active)
    pub tags: Vec<String>,
    /// Comma-separated list of tags [deprecated: use positional args]
    #[arg(long, hide = true)]
    pub only: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub no_claude: bool,
    #[arg(long)]
    pub wezterm: bool,
    #[arg(long)]
    pub frozen: bool,
    #[arg(long)]
    pub no_continue: bool,
    /// Launch across all repos
    #[arg(long)]
    pub all_repos: bool,
}

#[derive(Args)]
pub struct AdoptArgs {
    pub tag: String,
    pub path: PathBuf,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(short = 'i', long)]
    pub issue: Option<u32>,
    #[arg(long)]
    pub r#move: bool,
}

#[derive(Args)]
pub struct CdArgs {
    /// Project tag (default: detect from cwd)
    pub tag: Option<String>,
}

#[derive(Args)]
pub struct PathArgs {
    pub tag: Option<String>,
}

#[derive(Args)]
pub struct RenameArgs {
    /// Old tag (default: detect from cwd)
    pub old_tag: Option<String>,
    /// New tag name
    pub new_tag: String,
    /// Don't move directory, only update registry
    #[arg(long)]
    pub no_move: bool,
}

#[derive(Args)]
pub struct FreezeArgs {
    pub tag: Option<String>,
    /// Auto-expire after duration (30m, 2h, 3d, 1w)
    #[arg(long, alias = "timeout")]
    pub lifetime: Option<String>,
}

#[derive(Args)]
pub struct ThawArgs {
    pub tag: Option<String>,
}

// ── Repo subgroup ────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommands,
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// Register a git repo in the global index
    Add(RepoAddArgs),
    /// List registered repos
    List(RepoListArgs),
    /// Remove a repo from the global index
    Remove(RepoRemoveArgs),
    /// Update a repo config field
    Set(RepoSetArgs),
    /// Set the default repo for cwd-ambiguous commands
    Default(RepoDefaultArgs),
}

#[derive(Args)]
pub struct RepoAddArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long, default_value = "origin")]
    pub upstream: String,
    #[arg(long, default_value = "my")]
    pub fork: String,
    #[arg(long, default_value = "master")]
    pub default_base: String,
    #[arg(long)]
    pub issue_prefix: Option<String>,
}

// ConfigArgs and other trivials omitted for brevity
```

The `--repo` flag is declared `global = true` on `Cli` so it propagates to all subcommands automatically, rather than being repeated on each `*Args` struct.

---

## 4. Output Formats

### Table rendering — crate choice

Use **`comfy-table`** (v7). Rationale:
- Native TTY width detection via `terminal_size`
- Column constraints (min/max/percentage)
- UTF-8 box-drawing that degrades gracefully to ASCII
- `is-terminal` crate for TTY detection (same crate used for color gating)
- Active maintenance, no heavy transitive deps

Do NOT use `tabled`: heavier, requires more boilerplate for dynamic columns.

### Human table (default TTY)

```
TAG            ISSUE    BRANCH                    STATUS       PATH
─────────────────────────────────────────────────────────────────────────────
rawcodecs      #9947    DESKTOP-9947-rawcodecs    clean        /c/work/dt-rawcodecs
ci-fix                  fix-master-ci             2 ahead      /c/work/dt-ci-fix
dlmodels2      #9901    DESKTOP-9901-dlmodels2    dirty        /c/work/dt-dlmodels2

❄ Frozen (1)
────────────────
hotfix                  DESKTOP-9800-hotfix       clean 1h4m   /c/work/dt-hotfix
```

Rules:
- TAG and BRANCH columns fit to content, minimum 8 chars, no max (table wraps at terminal width)
- PATH column is always last and truncates with `…` when the table would overflow
- ISSUE column is omitted entirely when no project has an issue (matches Python behavior)
- STATUS color codes: green=clean/ahead, yellow=dirty/behind, red=missing, gray=frozen/untracked
- Frozen section is separated by a blank line + header line
- Freeze expiry shown as `clean 1h4m` (appended to status, yellow)

### `--short` (compact, pipe-friendly)

One line per project, no header, fixed column layout. Works well in `fzf`:
```
rawcodecs        #9947    DESKTOP-9947-rawcodecs         /c/work/dt-rawcodecs
ci-fix                    fix-master-ci                  /c/work/dt-ci-fix
❄ hotfix                  DESKTOP-9800-hotfix             /c/work/dt-hotfix  1h4m left
```

### `--json` — stable schema (versioned)

Version field is included from day one so consumers can detect breaking changes.

```json
{
  "version": 1,
  "repos": {
    "desktop": {
      "path": "/c/work/desktop_master",
      "upstream_remote": "if",
      "fork_remote": "my",
      "default_base": "master",
      "issue_prefix": "DESKTOP"
    }
  },
  "projects": {
    "rawcodecs": {
      "repo_id": "desktop",
      "tag": "rawcodecs",
      "path": "/c/work/dt-rawcodecs",
      "branch": "DESKTOP-9947-rawcodecs",
      "base": "if/master",
      "issue": 9947,
      "created": "2025-05-01T10:00:00",
      "frozen": false,
      "freeze_expires": null,
      "status": {
        "exists": true,
        "dirty": false,
        "ahead": 2,
        "behind": 0
      }
    }
  },
  "untracked": []
}
```

Notes:
- `status` sub-object is only populated when `--json` is requested (it requires git calls); a lightweight `--json --no-status` flag skips the git probing for scripts that only need metadata
- `version` must be bumped on any backward-incompatible schema change
- `--all-repos --json` uses the same schema; `projects` contains entries from all repos, `repo_id` disambiguates

### Non-TTY / piped output

When stdout is not a TTY (`!is_terminal(stdout)`):
- All ANSI escape codes suppressed (respect `NO_COLOR` env var too)
- `comfy-table` fallback to plain ASCII separators
- Table still formatted; `--short` is recommended for scripting
- `__POSTCD__` sentinel is printed on stdout regardless of TTY (the fish wrapper always captures it)

---

## 5. `grove cd` Ergonomics and the `gr` Fish Wrapper

### Why `grove cd` cannot cd itself

A process cannot change the cwd of its parent shell. The Python implementation works around this by printing a `__POSTCD__<path>` sentinel on stdout, which the `grove` fish function intercepts and uses to call `cd` in the shell's own context. The Rust binary must preserve this exact protocol.

**Rust side:** `grove cd <tag>` prints to stdout:
```
__POSTCD__/c/work/dt-rawcodecs
```
Then exits 0. The `__POSTCD__` prefix is the stable interface between the binary and the shell wrapper. It is never printed when stdout is a plain pipe without the fish wrapper (scripts calling `grove cd` directly will receive the sentinel and should use `grove path` instead).

**Better practice for scripts:** use `grove path <tag>` which prints only the bare path, no sentinel.

### The `gr` shorthand — unchanged behavior, updated completion

`gr` with no arguments → `grove list --short` (unchanged).
`gr <tag>` → `grove cd <tag>` (unchanged — the grove fish wrapper intercepts `__POSTCD__`).

The `gr.fish` wrapper stays exactly as-is. Tab completion for `gr` already delegates to `grove list --tags-only`, which the Rust binary also supports.

In a multi-repo world, `grove list --tags-only` will print all tags from all repos (unscoped). If the user wants scoped completion they can pass `--repo desktop`, but the default completion bucket should remain flat for muscle-memory reasons. If two repos have a tag with the same name, `grove cd <tag>` will disambiguate using cwd and fall back to prompting when ambiguous.

### The `grove` fish function — required changes for multi-repo

The current `grove.fish` function intercepts `__POSTCD__` only for a hardcoded set of subcommands. This set must be updated to include the new `repo` subcommand's none (no cd needed), and the `done`/`rename` logic that `cd`s to `main_repo` needs updating:

```fish
# Current: cd /c/WORK
# New: cd to the repo's work_dir (read from grove config)
set -l work_dir (grove repo path --default 2>/dev/null; or echo /c/WORK)
cd $work_dir
```

The Rust binary should expose `grove repo path [--default]` to give the fish wrapper a reliable way to find the right fallback directory. This replaces the hardcoded `/c/WORK` in `grove.fish`.

---

## 6. Shell Completions

### Generation with `clap_complete`

Add a hidden `completions` subcommand to generate and install completions:

```
grove completions <shell> [--install]
```

Shells: `fish`, `bash`, `zsh`, `powershell` (clap_complete supports all four).

`--install` without a path auto-detects the correct directory:
- fish → `~/.config/fish/completions/grove.fish`
- bash → `~/.local/share/bash-completion/completions/grove`
- zsh → first writable element of `$fpath` or `~/.zsh/completions/_grove`
- powershell → `$PROFILE` directory + `grove.ps1`

### Dynamic tag completion in fish

The generated fish completion calls `grove list --tags-only` for subcommands that take a tag argument. This is already the pattern in the existing `grove.fish`. The Rust binary must ensure `grove list --tags-only` is fast (<50ms) — it should read only the registry JSON, never run git.

Completion for `--repo` calls `grove repo list --ids-only` (new hidden flag) which prints one repo ID per line.

### Upgrade path

The Rust install should replace the hand-written `grove.fish` completions (lines 46-59) with the `clap_complete`-generated file. The `grove` fish function wrapper (the cd interceptor) is separate from completions and must be preserved as-is.

**Recommended split in dotfiles:**
- `~/.config/fish/functions/grove.fish` — cd-interceptor wrapper (hand-maintained)
- `~/.config/fish/completions/grove.fish` — clap_complete generated (auto-updated on `grove completions fish --install`)

---

## 7. TTY vs Non-TTY Behavior

### Color

```rust
use is_terminal::IsTerminal;

fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}
```

All color rendering paths check `use_color()`. The `colored` or `owo-colors` crate wraps output; use `owo-colors` with `OwoColorize` trait for zero-cost when disabled.

### Table width

```rust
fn terminal_width() -> u16 {
    if std::io::stdout().is_terminal() {
        terminal_size::terminal_size()
            .map(|(w, _)| w.0)
            .unwrap_or(120)
    } else {
        120  // sane default for pipes
    }
}
```

`comfy-table` accepts a `ContentArrangement::DynamicFullWidth` with this width.

### Progress indicators

For operations that run git subprocesses in parallel (`grove list` status gathering, `grove done` fetch), use a spinner only when stdout is a TTY:

```rust
if use_color() {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Fetching...");
    // ... run work ...
    spinner.finish_and_clear();
}
```

Crate: `indicatif`. The spinner is hidden in non-TTY mode. The `grove list` parallel status fetch (Python uses `ThreadPoolExecutor`) should use `rayon` or `tokio::task::spawn_blocking` in Rust.

### `GROVE_DEBUG` env var

Preserve the existing `GROVE_DEBUG=1` behavior (extra diagnostic output to stderr). Rust uses:
```rust
let debug = std::env::var_os("GROVE_DEBUG").is_some();
```

---

## 8. Error UX

### Tag does not exist

```
error: unknown project 'rawcodes'

  Did you mean: rawcodecs?

  Run 'grove list' to see all projects.
```

Implement near-match suggestions using the `strsim` crate (Jaro-Winkler distance). Show up to 3 candidates, threshold 0.7.

### Repo not found (multi-repo)

```
error: repo 'desktop' is not registered

  Known repos: infra (/c/work/infra)
  Add it with: grove repo add /c/work/desktop_master --id desktop
```

### Dirty worktree during `done`

```
error: cannot remove project 'rawcodecs'

  Uncommitted changes:
    - src/codec/raw.rs
    - src/codec/raw_test.rs
    ... and 3 more

  Unpushed commits:
    - 2 commit(s) not pushed to my/DESKTOP-9947-rawcodecs

  Resolve the issues above, or use --force to override.
```

The Rust output matches the Python format but uses `comfy-table` for alignment and `owo-colors` for labels. The list of files is truncated at 10 with a "... and N more" suffix (matches Python).

### Missing remote

```
warning: remote 'my' not found in repo — remote checks skipped
```

Emitted on stderr. Non-fatal; the command continues without push/branch checks.

### Worktree directory locked (Windows/WSL)

Preserve the Python logic verbatim, ported to Rust:
1. Check wezterm pane CWDs via `wezterm.exe cli list --format json`
2. Check `/proc/*/cwd` symlinks for WSL processes
3. Attempt to close non-self panes via `wezterm.exe cli kill-pane`
4. Fall back to `cmd.exe /c rmdir /s /q` on persistent locks
5. Queue in `stale_dirs.json` if still locked

Error message when locked:

```
error: directory is locked by a Windows process

  Open wezterm tabs in /work/:
    tab 3: rawcodecs — claude
    tab 7: rawcodecs — fish

  Close these tabs, then retry.
  Or from admin PowerShell: handle.exe "C:\work\dt-rawcodecs"
  Debug: GROVE_DEBUG=1 grove done rawcodecs
```

### Ambiguous tag in multi-repo context

```
error: tag 'ci-fix' exists in multiple repos

  desktop  →  /c/work/dt-ci-fix
  infra    →  /c/work/in-ci-fix

  Use --repo <id> to disambiguate.
```

---

## 9. Backward-Compat Sugar

### Deprecated flags kept as hidden aliases

| Python flag | Rust treatment |
|---|---|
| `grove list --json` | Kept as-is (not deprecated) |
| `grove launch --only tag1,tag2` | Kept as hidden `--only` alias; prints deprecation warning to stderr: `warning: --only is deprecated; use positional args: grove launch tag1 tag2` |
| `grove freeze --timeout` | Kept as alias for `--lifetime` (no warning, silently aliased) |
| `grove thaw` / `grove unfreeze` | Both aliases kept |
| `grove list ls` alias | Kept |
| `grove rename mv` alias | Kept |
| `grove config --init` | Kept as `grove config init` AND `grove config --init` (hidden flag form calls subcommand internally) |

### Migration from old `~/.config/project/` layout

The Python tool migrated `~/.config/project` → `~/.config/grove`. The Rust binary should repeat this at startup:

```rust
fn migrate_config_dir() {
    let old = dirs::config_dir().unwrap().join("project");
    let new = dirs::config_dir().unwrap().join("grove");
    if old.exists() && !new.exists() {
        fs::rename(&old, &new).ok();
    }
}
```

### Auto-migration from single-repo registry format

Old `registry.json` has `{ "projects": { "tag": { ... } } }` with no `repo_id`. On first Rust load:

1. Detect missing `repo_id` fields
2. Read `config.json`'s `main_repo` field
3. Create a repo entry in the global index with `id = "default"` (or the basename of `main_repo`)
4. Back-fill `repo_id` on every project entry
5. Write updated registry atomically
6. Print: `info: migrated registry to multi-repo format (repo id: 'desktop')`

This migration runs once and is idempotent.

---

## 10. Exact `--help` Output

### Top-level `grove --help`

```
Git worktree manager for named projects

Usage: grove [OPTIONS] <COMMAND>

Commands:
  new        Create a new worktree project
  fork       Fork an existing project into a new worktree
  done       Remove a project worktree and clean up
  list       List projects [alias: ls]
  launch     Open terminal tabs for projects
  adopt      Import existing worktree into registry
  cd         Change directory to a project worktree
  path       Print worktree path for a tag
  rename     Rename a project tag [alias: mv]
  freeze     Freeze a project (skip in launch, listed separately)
  thaw       Unfreeze a project [alias: unfreeze]
  repo       Manage repos in the global index
  config     Manage configuration
  help       Print this message or the help of subcommand(s)

Options:
      --repo <REPO_ID>  Override which repo to operate on
  -h, --help            Print help
  -V, --version         Print version

Run 'grove <COMMAND> --help' for subcommand details.

Examples:
  grove new rawcodecs --issue 11301
  grove new ci-fix --branch fix-master-ci --base 25.3
  grove list
  grove cd rawcodecs
  grove launch --dry-run
  grove done rawcodecs
  grove adopt rawcodecs /c/WORK/desktop_master2
```

### `grove new --help`

```
Create a new worktree project

Usage: grove new [OPTIONS] <TAG>

Arguments:
  <TAG>  Short project tag (e.g. rawcodecs, ci-fix)

Options:
  -i, --issue <N>         Issue number — creates <PREFIX>-N-<tag> branch
  -b, --branch <NAME>     Explicit branch name
      --base <REF>        Base ref (e.g. master, 25.3, origin/main)
      --no-launch         Don't launch terminal tab after creation
      --no-fetch          Skip fetch before creating worktree
      --repo <REPO_ID>    Override which repo to use
  -h, --help              Print help

Examples:
  grove new rawcodecs --issue 9947
  grove new ci-fix --branch fix-master-ci --base 25.3
  grove new hotfix --no-launch --repo desktop
```

### `grove list --help`

```
List projects [alias: ls]

Usage: grove list [OPTIONS]

Options:
  -a, --all             Include untracked git worktrees
      --all-repos       Show projects from all repos grouped
      --json            Output as JSON
  -s, --short           Compact one-line-per-project output
      --check-remote    Verify remote branches exist
      --repo <REPO_ID>  Filter to a specific repo
  -h, --help            Print help

Output:
  Default: rich table with status column (requires git calls)
  --short:  tag | issue | branch | path (no git calls)
  --json:   machine-readable JSON (see docs for schema)
```

### `grove done --help`

```
Remove a project worktree and clean up

Usage: grove done [OPTIONS] [TAG]

Arguments:
  [TAG]  Project tag (default: detect from cwd)

Options:
  -f, --force        Skip confirmation prompt and dirty checks
      --keep-local   Keep local branch after removing worktree
      --keep-remote  Keep remote branch after removing worktree
      --repo <REPO_ID>  Override which repo to use
  -h, --help         Print help

Safety checks (skipped with --force):
  - Uncommitted or untracked files
  - Commits not pushed to fork remote
  - Branch not merged into upstream

Examples:
  grove done                   # auto-detect from cwd
  grove done rawcodecs
  grove done rawcodecs --force --keep-remote
```

---

## 11. Startup Side Effects (Ported from Python)

The Python `main()` runs two side-effect tasks before dispatching:
1. `_cleanup_stale_dirs()` — attempt to remove previously locked directories
2. `_process_expired_freezes()` — auto-commit+push+remove projects whose freeze timer expired

In Rust, run both before dispatch in `main()`:
```rust
fn main() {
    let cli = Cli::parse();
    cleanup_stale_dirs();       // non-fatal, always runs
    process_expired_freezes();  // non-fatal, always runs
    dispatch(cli);
}
```

Both are non-fatal (errors are logged to stderr and ignored). `process_expired_freezes` prints user-visible output only when projects actually expire.

---

## 12. `grove config` Subcommand

Python has `grove config --init [--force]`. Rust models this as a proper subcommand group:

```
grove config init [--force]    Initialize config.json with defaults
grove config show              Print current config as JSON
grove config set <key> <value> Update a config value
grove config edit              Open config in $EDITOR
```

For backward compatibility, `grove config --init` is also accepted (hidden flag, calls `grove config init`).

---

## 13. WSL/Windows-Specific Path Utilities

The Python `_normalize_wsl_path` and `wsl_to_win` functions must be ported to Rust. These handle:
- `file://` URL stripping
- UNC admin share paths (`//hostname/C$`)
- Windows drive letters (`C:\foo` → `/c/foo`)
- Lowercase normalization for path comparison

These belong in a `platform::wsl` module. The normalization is used in both the lock-detection code (comparing `/proc/*/cwd` against worktree paths) and the wezterm integration.

---

## Summary of UX Wins vs. Open Questions

See the final summary section of this document (written as the agent's return message to the orchestrator).
