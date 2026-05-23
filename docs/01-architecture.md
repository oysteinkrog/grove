# grove Rust Rewrite — Architecture, Configuration Schema, and Migration

## 1. Crate Layout

### Single binary crate, flat workspace not required yet

grove is a single user-facing binary (~2 kloc Python today). A workspace adds overhead
with no payoff until a second crate appears (e.g. a library consumers could link). Start
with a single crate at the repo root:

```
grove/
├── Cargo.toml          # [package] + [dependencies]
├── src/
│   ├── main.rs         # clap App construction + main()
│   ├── cli/
│   │   ├── mod.rs      # Cli enum, common flag structs
│   │   ├── new.rs
│   │   ├── fork.rs
│   │   ├── done.rs
│   │   ├── list.rs
│   │   ├── launch.rs
│   │   ├── adopt.rs
│   │   ├── rename.rs
│   │   ├── freeze.rs
│   │   └── cd.rs
│   ├── config/
│   │   ├── mod.rs      # GlobalConfig, RepoConfig loading + merging
│   │   ├── global.rs   # ReposManifest (repos.json)
│   │   └── repo.rs     # PerRepoConfig (per-repo config.json)
│   ├── registry/
│   │   ├── mod.rs      # Registry — load/save/query
│   │   └── project.rs  # Project struct
│   ├── repo/
│   │   mod.rs          # RepoContext — resolved view of one repo's settings
│   ├── git/
│   │   ├── mod.rs
│   │   ├── worktree.rs # worktree add/remove/move/list via gix
│   │   └── status.rs   # dirty/merged/ahead-behind checks
│   ├── launch/
│   │   mod.rs          # terminal spawning (wezterm, wt)
│   ├── migrate/
│   │   mod.rs          # one-shot migration from legacy layout
│   ├── paths.rs        # WSL/Windows path normalization
│   ├── display.rs      # color helpers, table formatting
│   └── error.rs        # Error type, Result alias
└── tests/
    ├── config_round_trip.rs
    ├── registry_round_trip.rs
    └── migrate.rs
```

**Justification per module:**

| Module | Reason |
|--------|--------|
| `cli/` | Keeps command-specific arg structs and handlers out of `main.rs`; each file is one command |
| `config/` | Two distinct config layers (global manifest vs per-repo overrides) need separate types but share a load path |
| `registry/` | Registry is a domain object with its own CRUD; split from config to keep separation clean |
| `repo/` | `RepoContext` is the resolved union of global + per-repo config; the rest of the app only touches this |
| `git/` | All git I/O isolated here; the `worktree` sub-module uses gix, `status` may shell out for complex merge detection |
| `launch/` | Terminal-spawning code is platform-specific and evolves independently |
| `migrate/` | One-shot, runs once and is dead code thereafter; isolation avoids polluting stable paths |
| `paths.rs` | WSL ↔ Windows path conversion is used everywhere, must be a shared leaf module |
| `display.rs` | Color/table helpers; keeps rendering logic out of domain code |
| `error.rs` | Central error type; avoids `use crate::error::Error` spread |

---

## 2. Schema Design

### 2a. Format choice: JSON everywhere

Keep JSON rather than switching to TOML. Reasons:
- The existing files are JSON; migration produces JSON again — no format conversion needed.
- TOML's multi-document arrays-of-tables are awkward for keyed maps (projects are
  keyed by tag, not in an array).
- `serde_json` is already a hard dep; TOML would add `toml` for no user-visible benefit.
- The files are written by the tool, not hand-edited by users (power users who want to
  edit them can use any editor).

All files get a top-level `"schema_version"` integer (see section 2e).

---

### 2b. Global repos manifest — `~/.config/grove/repos.json`

This is new in the Rust rewrite. It lists every registered repo and carries the
settings that were previously in the single flat `config.json`.

```json
{
  "schema_version": 1,
  "default_repo": "desktop",
  "repos": {
    "desktop": {
      "main_repo": "/c/work/desktop/master",
      "work_dir": "/c/work/desktop",
      "dir_prefix": "",
      "upstream_remote": "if",
      "fork_remote": "my",
      "default_base": "master",
      "launch": {
        "terminal": "wt",
        "wezterm_path": "/c/work/wezterm/target/release/wezterm.exe",
        "shell_command": "fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'"
      }
    }
  }
}
```

Corresponding Rust types:

```rust
// config/global.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReposManifest {
    pub schema_version: u32,
    pub default_repo: Option<String>,
    pub repos: IndexMap<String, RepoEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub main_repo: PathBuf,
    pub work_dir: PathBuf,
    #[serde(default)]
    pub dir_prefix: String,
    pub upstream_remote: String,
    pub fork_remote: String,
    pub default_base: String,
    #[serde(default)]
    pub launch: LaunchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LaunchConfig {
    #[serde(default = "default_terminal")]
    pub terminal: Terminal,
    pub wezterm_path: Option<PathBuf>,
    pub shell_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Terminal {
    #[default]
    Wt,
    Wezterm,
}
```

`IndexMap` preserves insertion order for deterministic JSON output. Add it via
`indexmap` crate with `serde` feature.

---

### 2c. Per-repo override config — `<work_dir>/.grove/config.json`

This file is optional. Any field present here overrides the corresponding field in the
global `RepoEntry` for this repo. Sparse — only override what differs.

```json
{
  "schema_version": 1,
  "launch": {
    "shell_command": "fish -l -c 'claude --dangerously-skip-permissions; exec fish'"
  }
}
```

Rust type:

```rust
// config/repo.rs

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerRepoConfig {
    #[serde(default)]
    pub schema_version: u32,
    pub upstream_remote: Option<String>,
    pub fork_remote: Option<String>,
    pub default_base: Option<String>,
    pub dir_prefix: Option<String>,
    pub launch: Option<LaunchConfig>,
}
```

Merging is explicit — `RepoContext::resolve(entry: &RepoEntry, override: &PerRepoConfig)`
applies `Option` fields from the override, falling back to the global entry.

```rust
// repo/mod.rs

pub struct RepoContext {
    pub id: String,
    pub main_repo: PathBuf,
    pub work_dir: PathBuf,
    pub dir_prefix: String,
    pub upstream_remote: String,
    pub fork_remote: String,
    pub default_base: String,
    pub launch: LaunchConfig,
    /// Absolute path to <work_dir>/.grove/
    pub grove_dir: PathBuf,
}

impl RepoContext {
    pub fn registry_path(&self) -> PathBuf {
        self.grove_dir.join("registry.json")
    }

    pub fn config_path(&self) -> PathBuf {
        self.grove_dir.join("config.json")
    }
}
```

---

### 2d. Per-repo project registry — `<work_dir>/.grove/registry.json`

Shape is close to today's registry, with a `schema_version` field added.

```json
{
  "schema_version": 1,
  "projects": {
    "rawcodecs": {
      "path": "/c/work/desktop/rawcodecs",
      "branch": "rawcodecs",
      "base": "if/master",
      "created": "2026-02-01T10:51:49Z",
      "issue": null,
      "frozen": true,
      "freeze_expires": null
    },
    "lazy-vm": {
      "path": "/c/work/desktop/lazy-vm",
      "branch": "DESKTOP-11312-lazy-vm",
      "base": "if/master",
      "created": "2026-02-01T10:51:47Z",
      "issue": 11312,
      "frozen": false,
      "freeze_expires": null
    }
  }
}
```

Note: `frozen` and `freeze_expires` are always written explicitly (not omitted when
false/null) to make tooling and migration simpler. Use `#[serde(default)]` for
forward-compat on load.

Rust types:

```rust
// registry/project.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub issue: Option<u32>,
    #[serde(default)]
    pub frozen: bool,
    #[serde(default)]
    pub freeze_expires: Option<DateTime<Utc>>,
}

// registry/mod.rs

#[derive(Debug, Serialize, Deserialize)]
pub struct Registry {
    pub schema_version: u32,
    pub projects: IndexMap<String, Project>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> { ... }
    pub fn save(&self, path: &Path) -> Result<()> { ... }  // atomic write
    pub fn get(&self, tag: &str) -> Option<&Project> { ... }
    pub fn get_mut(&mut self, tag: &str) -> Option<&mut Project> { ... }
    pub fn insert(&mut self, tag: String, project: Project) { ... }
    pub fn remove(&mut self, tag: &str) -> Option<Project> { ... }
    pub fn tags(&self) -> impl Iterator<Item = &str> { ... }
}
```

Use `chrono` with `serde` feature for `DateTime<Utc>`. The Python code stores
naive ISO timestamps; migration will parse them as local time and store as UTC.

---

### 2e. Schema versioning strategy

**Rule:** every file has `"schema_version": N` at the top level. Versions are
monotonic integers, not semver.

**On load:**
- If `schema_version` is absent, assume version 0 (pre-Rust, legacy format).
- If `schema_version` > what the binary understands, emit a hard error:
  ```
  error: repos.json has schema_version 3 but this build only understands up to 2.
  Upgrade grove or downgrade the config.
  ```
- If `schema_version` < current, run the appropriate migration chain
  (version 0→1, then 1→2, etc.) in-memory and rewrite.

**Migration chain** lives entirely in `migrate/mod.rs` as a `fn migrate_vN_to_vN1(v: Value) -> Value`
per transition. Migrations operate on raw `serde_json::Value` before deserialization so that
the typed structs always see the current version.

**Forward compatibility:** unknown JSON fields are silently ignored on load
(`#[serde(deny_unknown_fields)]` is NOT used on any config struct). This means a newer
binary's files can be loaded by an older binary — the older binary just ignores new
fields it doesn't know about. This is a "be liberal on read" policy.

---

## 3. Repo Discovery

### The problem

Multi-repo model: a user may have a `desktop` repo and an `ifkb` repo. `grove cd foo`
needs to find which repo `foo` belongs to without the user specifying it.

### Resolution order (implemented in `repo/mod.rs`)

```
1. Explicit --repo <id> flag → look up repos.json["repos"][id]
2. GROVE_REPO env var        → same lookup
3. Cwd-based detection:
   a. Walk up from $GROVE_ORIG_CWD (or $PWD) until a .grove/registry.json is found.
      The directory containing .grove/ is the work_dir; match against repos.json.
   b. If no .grove/ found in cwd tree, check if cwd is under any known work_dir
      (prefix match against all repos[*].work_dir). Use the longest-prefix match.
4. Default repo from repos.json["default_repo"] if set.
5. Error: "Cannot determine which repo to use. Set GROVE_REPO or pass --repo."
```

Once the repo is identified, load its registry from `<work_dir>/.grove/registry.json`
and check if `foo` exists. In the multi-repo case where the same tag exists in two
repos, step 3 (cwd) breaks the tie naturally; if cwd is ambiguous, step 1/2 (explicit)
is required. The error message should list which repos contain the tag:

```
error: tag 'foo' exists in multiple repos: desktop, ifkb
hint: run from inside a worktree, or pass --repo <id>
```

### Cwd auto-detect for subcommands that default to current project

Commands like `grove done` (no tag given) detect the current project by matching cwd
against all registered paths in all repos. Same prefix-match logic.

```rust
// repo/mod.rs

pub fn detect_from_cwd(
    manifest: &ReposManifest,
    cwd: &Path,
) -> Result<Option<(String, RepoContext, String)>> {
    // Returns (repo_id, context, project_tag)
    ...
}
```

---

## 4. Migration Logic

### Overview

On first run of the new binary, detect the legacy layout (`~/.config/grove/config.json`
exists AND `~/.config/grove/repos.json` does NOT exist). Run migration automatically,
print a summary, and continue normally.

### Step-by-step

**Step 1 — Read legacy files**

```
~/.config/grove/config.json   → LegacyConfig
~/.config/grove/registry.json → LegacyRegistry (schema_version absent = 0)
```

`LegacyConfig` is a permissive type that accepts the Python-era fields:

```rust
#[derive(Debug, Deserialize)]
struct LegacyConfig {
    main_repo: String,
    work_dir: String,
    #[serde(default)]
    dir_prefix: String,
    #[serde(default = "default_upstream")]
    upstream_remote: String,
    #[serde(default = "default_fork")]
    fork_remote: String,
    #[serde(default = "default_base_str")]
    default_base: String,
    #[serde(default)]
    launch: serde_json::Value,  // permissive
}
```

**Step 2 — Normalize paths**

Run `normalize_wsl_path` on `main_repo` and `work_dir`. Derive a repo id from the
last path component of `main_repo` (e.g. `/c/work/desktop/master` → `desktop`).
If a collision exists with a future-registered repo, append `_1`.

**Step 3 — Build new structures**

Construct `ReposManifest` with `schema_version: 1` and a single entry under the
derived repo id. Mark it as `default_repo`.

For the registry: iterate `LegacyRegistry["projects"]`, parse each project. Key
differences to normalize:

- `frozen`: field may be absent (treat as `false`) or present as `true`/`false`.
- `freeze_expires`: may be absent → `None`.
- `created`: Python writes naive ISO (e.g. `"2026-02-01T10:51:47"`). Parse as
  `NaiveDateTime`, treat as local time, convert to UTC. If parse fails, use `Utc::now()`.
- `issue`: may be `null` → `None` or an integer.
- `path`: run through `normalize_wsl_path`.

**Step 4 — Create `<work_dir>/.grove/` directory**

```
mkdir -p <work_dir>/.grove/
```

**Step 5 — Write new files atomically**

Write `<work_dir>/.grove/registry.json` (new per-repo registry).
Write `~/.config/grove/repos.json` (global manifest).

**Step 6 — Backup originals**

```
~/.config/grove/config.json   → ~/.config/grove/config.json.pre-rust-migration
~/.config/grove/registry.json → ~/.config/grove/registry.json.pre-rust-migration
```

Do NOT delete the originals — the user can restore them if the old Python binary
is needed as a fallback.

**Step 7 — Print migration summary**

```
grove: migrating legacy config to multi-repo format...
  repo id:   desktop
  registry:  /c/work/desktop/.grove/registry.json  (27 projects)
  manifest:  ~/.config/grove/repos.json
  backups:   config.json.pre-rust-migration, registry.json.pre-rust-migration
migration complete.
```

### Edge cases

| Situation | Handling |
|-----------|----------|
| `main_repo` path does not exist on disk | Warn but proceed; the repo entry is written; user may mount drives later |
| `work_dir` does not exist | Warn but create `<work_dir>/.grove/` if the parent exists; otherwise error |
| `config.json` absent but `registry.json` present | Write repos.json with sensible defaults (warn user to review); import registry |
| Both legacy and new files present | Skip migration entirely; new binary reads `repos.json` |
| `registry.json` has unrecognized fields | Preserve as-is via `serde_json::Value` round-trip before deserializing into typed struct |
| `created` timestamp unparseable | Use `Utc::now()` and warn with the tag name |
| Custom `dir_prefix` that doesn't match tag → path mismatch | Store the literal path from the registry; do not reconstruct from prefix |

---

## 5. Error Model

### Strategy: `thiserror` for domain errors, `anyhow` at the CLI boundary

```
crate internals (config/, registry/, git/, migrate/) → thiserror
cli/ handlers → anyhow (propagate with ?)
main() → match on Err, format with color and hint
```

**Why:** `thiserror` lets the domain code expose typed errors that subcommand handlers
can match on to provide context-sensitive hints (e.g. "tag not found → here are the
registered tags"). `anyhow` in the CLI handlers gives cheap `?` propagation where the
error message is already good enough.

```rust
// error.rs

#[derive(Debug, thiserror::Error)]
pub enum GroveError {
    #[error("project '{tag}' not found")]
    ProjectNotFound { tag: String },

    #[error("project '{tag}' already exists in repo '{repo}'")]
    ProjectExists { tag: String, repo: String },

    #[error("tag '{tag}' is ambiguous: found in repos {repos:?}")]
    AmbiguousTag { tag: String, repos: Vec<String> },

    #[error("not inside a registered project; specify a tag or cd into one")]
    NoCwdProject,

    #[error("repo '{id}' not found in repos.json")]
    RepoNotFound { id: String },

    #[error("config file is corrupt: {path}: {source}")]
    CorruptConfig {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("schema version {found} in {path} is newer than this build supports (max {max})")]
    SchemaTooNew { found: u32, max: u32, path: PathBuf },

    #[error("worktree path does not exist: {0}")]
    WorktreeMissing(PathBuf),

    #[error("directory is locked: {0}")]
    DirectoryLocked(PathBuf),

    #[error("git error: {0}")]
    Git(#[from] GitError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git command failed: {stderr}")]
    CommandFailed { stderr: String },

    #[error("gix error: {0}")]
    Gix(String),
}

pub type Result<T, E = GroveError> = std::result::Result<T, E>;
```

### User-facing error UX

`main()` catches errors and formats them before printing:

```rust
fn main() {
    if let Err(e) = run() {
        eprintln!("{} {e}", red("error:"));
        if let Some(hint) = hint_for(&e) {
            eprintln!("{} {hint}", cyan("hint:"));
        }
        std::process::exit(1);
    }
}

fn hint_for(e: &GroveError) -> Option<&'static str> {
    match e {
        GroveError::NoCwdProject =>
            Some("use 'grove list' to see registered projects"),
        GroveError::AmbiguousTag { .. } =>
            Some("pass --repo <id> to disambiguate, or cd into a worktree"),
        GroveError::SchemaTooNew { .. } =>
            Some("upgrade grove: https://github.com/oysteinkrog/grove"),
        _ => None,
    }
}
```

Color is gated on `std::io::IsTerminal` for stderr (same as Python's `isatty`).
Use `owo-colors` crate — it respects `NO_COLOR` env var automatically and supports
`OwoColorize` trait on `Display` types. Alternative: `colored` crate which also
auto-detects, but `owo-colors` is zero-allocation and has better `NO_COLOR` support.

---

## 6. Cross-Platform Paths (WSL / Windows)

### Context

grove runs as a WSL binary but operates on Windows-visible filesystem paths:
- Worktree paths are `/c/work/...` (WSL mount points).
- Some subprocesses (wezterm, wt.exe, cmd.exe) require Windows-style `C:\work\...` paths.
- `gix` operates on the filesystem via WSL paths natively.
- Claude Code's project folder encoding uses `/c/...` paths.

### Normalization strategy

All paths stored in config and registry are normalized to lowercase WSL form:
`/c/work/desktop/foo`. This is the canonical internal representation.

The normalization function (porting the Python `_normalize_wsl_path`):

```rust
// paths.rs

/// Normalize any path representation to lowercase WSL form: /c/work/foo
/// Handles: /c/..., C:\..., C:/..., file:///C:/..., \\server\C$\..., /C:/...
pub fn normalize_wsl(path: &str) -> String { ... }

/// Convert WSL path /c/foo to Windows path C:\foo
/// Used only when shelling out to native Windows executables.
pub fn wsl_to_win(path: &Path) -> String { ... }

/// Encode a path to Claude Code's project folder name convention.
/// /c/work/foo -> -c-work-foo
pub fn claude_project_folder(path: &Path) -> String { ... }
```

### gix usage

`gix` handles git operations through the WSL path layer transparently — it operates
on the filesystem the same way `std::fs` does in WSL. No path conversion needed when
calling gix APIs.

When shelling out (for operations not yet in gix — e.g. complex squash-merge detection,
`git worktree move` which has subtle locking behavior on Windows), pass the WSL path
directly to `git -C /c/...`. The git binary in WSL understands `/c/...` paths.

### What requires Windows paths

Only these operations need `wsl_to_win` conversion:
- `wezterm.exe cli spawn --cwd <win_path>` — wezterm.exe is a Windows binary.
- `wt.exe -d '<win_path>'` — Windows Terminal.
- `cmd.exe /c rmdir /s /q <win_path>` — force-removing locked directories.

These are all in `launch/mod.rs` and the locked-dir removal code; they are the only
callers of `wsl_to_win`.

### Path storage in registry

Always store the result of `normalize_wsl(path)` in registry JSON. On load, run every
`path` field through `normalize_wsl` again as a defensive normalization (the Python tool
was not always consistent). This is cheap and idempotent.

### `GROVE_ORIG_CWD`

The fish shell wrapper sets `GROVE_ORIG_CWD` to the pre-cd cwd before invoking grove.
The Rust binary reads this env var the same way the Python binary did, falling back
to `std::env::current_dir()`.

---

## 7. Testing Approach for Config/Registry

### Strategy: tempdir + golden files + insta snapshots

Three test layers:

**Layer 1 — Unit tests for path normalization and schema migration (in-module)**

```rust
// paths.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_path_variants() {
        assert_eq!(normalize_wsl("C:\\work\\foo"), "/c/work/foo");
        assert_eq!(normalize_wsl("/C:/work/foo"), "/c/work/foo");
        assert_eq!(normalize_wsl("file:///C:/work/foo"), "/c/work/foo");
        assert_eq!(normalize_wsl("//hostname/C$/work/foo"), "/c/work/foo");
        assert_eq!(normalize_wsl("/c/work/foo/"), "/c/work/foo");
    }
}
```

**Layer 2 — Config round-trip tests with golden JSON files**

```
tests/
  fixtures/
    legacy_config.json          # copy of real ~/.config/grove/config.json
    legacy_registry.json        # first ~30 entries from real registry
    migrated_repos.json.snap    # insta snapshot of migration output
    migrated_registry.json.snap
  config_round_trip.rs
  registry_round_trip.rs
  migrate.rs
```

```rust
// tests/migrate.rs

#[test]
fn migrate_legacy_to_v1() {
    let legacy_config = include_str!("fixtures/legacy_config.json");
    let legacy_registry = include_str!("fixtures/legacy_registry.json");
    let (manifest, registry) = migrate::from_legacy(legacy_config, legacy_registry).unwrap();
    insta::assert_json_snapshot!(manifest);
    insta::assert_json_snapshot!(registry);
}
```

`insta` snapshots let us review the migration output once and then lock it. When the
migration logic changes, `cargo insta review` shows the diff explicitly.

**Layer 3 — Integration tests with tempdir**

```rust
// tests/registry_round_trip.rs

#[test]
fn registry_add_remove_persist() {
    let dir = tempfile::tempdir().unwrap();
    let reg_path = dir.path().join("registry.json");
    let mut reg = Registry::default();
    reg.insert("foo".to_string(), Project {
        path: "/c/work/desktop/foo".into(),
        branch: "foo".to_string(),
        base: "if/master".to_string(),
        created: Utc::now(),
        issue: Some(1234),
        frozen: false,
        freeze_expires: None,
    });
    reg.save(&reg_path).unwrap();

    let loaded = Registry::load(&reg_path).unwrap();
    assert_eq!(loaded.get("foo").unwrap().branch, "foo");
    assert_eq!(loaded.schema_version, 1);
}
```

**Atomic write test:**

```rust
#[test]
fn atomic_write_leaves_no_partial_file() {
    // Simulate a write mid-flight by injecting a panic hook — verify
    // the original file is unchanged.
}
```

**Key crates:**
- `insta` — snapshot testing
- `tempfile` — tempdir for integration tests
- `serde_json` — JSON comparison in tests

---

## 8. Key Dependencies

```toml
[dependencies]
# CLI
clap = { version = "4", features = ["derive", "color", "wrap_help"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
indexmap = { version = "2", features = ["serde"] }

# Dates
chrono = { version = "0.4", features = ["serde"] }

# Color output
owo-colors = "4"

# Error handling
thiserror = "2"
anyhow = "1"

# Git
gix = { version = "0.67", features = ["worktree-mutation", "status"] }

# Path utils (none needed beyond std)

[dev-dependencies]
tempfile = "3"
insta = { version = "1", features = ["json", "redactions"] }
```

### Note on gix feature selection

gix is a large crate. Enable only the features needed:
- `worktree-mutation` — worktree add/remove/move
- `status` — dirty check
- Disable `blocking-io` / `async-*` since grove is a synchronous CLI

Some operations (squash-merge detection, ahead/behind counts) require running
`git rev-list` with complex args. Use `std::process::Command` for these rather than
trying to reproduce the logic in gix — the correctness of the Python implementation
is already battle-tested and `git` is always available.

---

## 9. Atomic Write Implementation

The Python implementation uses `fcntl.flock` + `os.replace` for atomic writes.
In Rust:

```rust
// registry/mod.rs (and config/global.rs)

pub fn atomic_write(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| GroveError::Io(
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir")
    ))?;
    std::fs::create_dir_all(parent)?;

    // Backup existing
    if path.exists() {
        let bak = path.with_extension(
            format!("{}.bak", path.extension().unwrap_or_default().to_string_lossy())
        );
        let _ = std::fs::copy(path, &bak);
    }

    // Write to tempfile in same directory (same filesystem → rename is atomic)
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    writeln!(tmp)?;  // trailing newline
    tmp.persist(path).map_err(|e| e.error)?;

    Ok(())
}
```

`tempfile::NamedTempFile::persist` does an `fs::rename` which is atomic on
Linux/POSIX. On Windows it would fail if the target exists (NTFS), but grove runs
under WSL which uses the Linux rename semantics through the WSL filesystem driver.

---

## 10. `__POSTCD__` Protocol

The Python binary prints `__POSTCD__<path>` on stdout as a signal to the fish
shell wrapper to cd after grove returns. The Rust binary must preserve this protocol
exactly — it is part of the external interface consumed by the fish function in
`~/.config/fish/functions/grove.fish`.

```rust
// In each command handler that needs post-cd:
fn signal_postcd(path: &Path) {
    println!("__POSTCD__{}", path.display());
}
```

Similarly, the `__WEZTERM_TITLE__<title>` signal (if used) must be preserved.
Check the fish wrapper before changing any stdout protocol.

---

## 11. Stale Directory Queue

The Python binary maintains `~/.config/grove/stale_dirs.json` for Windows-locked
directories that couldn't be removed immediately. This file lives in the global config
dir, not per-repo. In the Rust rewrite, keep it at the same path for continuity:

```
~/.config/grove/stale_dirs.json
```

Rust type:

```rust
// In a new config/stale.rs or inlined in migrate/mod.rs

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct StaleDirs(pub Vec<PathBuf>);
```

On startup (same as Python): load, attempt `cmd.exe /c rmdir` for each, remove
successful ones. This runs before any command dispatch.

---

## Summary of File Locations After Migration

```
~/.config/grove/
├── repos.json                        ← NEW: global multi-repo manifest
├── config.json.pre-rust-migration    ← backup of old config
├── registry.json.pre-rust-migration  ← backup of old registry
└── stale_dirs.json                   ← unchanged: locked-dir queue

/c/work/desktop/.grove/
├── registry.json                     ← NEW: per-repo project registry
└── config.json                       ← OPTIONAL: per-repo overrides
```

---

## Open Questions for the Implementer

1. **`grove config` subcommand:** The Python tool has `grove config --init`. In the
   multi-repo model, should this become `grove repos add` / `grove repos list`?
   Or keep a single `grove config` that shows the resolved config for the current repo?

2. **`grove list` across repos:** Should `grove list` show all repos interleaved (with
   a `REPO` column), or only the current repo? Probably the current repo by default
   with `--all-repos` to show everything.

3. **gix vs shell-out for worktree operations:** gix's `worktree-mutation` API is
   relatively new. Consider keeping `git worktree add/remove/move` as shell-outs
   initially (they are well-tested, handle Windows quirks) and use gix only for
   read operations (status, rev-parse, ahead/behind). Migrate to full gix later.

4. **freeze expiry background processing:** The Python tool runs expiry checks on
   every invocation. This is fine for a CLI but consider whether a separate daemon or
   launchd/systemd timer is better long-term. For now, match Python behavior.

5. **`wt.exe` vs wezterm:** The launch code supports both. For the Rust port, keep
   both but consider making the terminal an enum with strongly-typed launch args
   rather than format strings. This would allow `--dry-run` to print structured output.
