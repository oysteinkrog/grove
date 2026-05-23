# grove Rust rewrite — Facet 04: Launch, Testing, CI, Packaging, Install

> Companion plan to facets 01-03. Read those for the core data model and git
> abstractions. This document covers the `launch` subcommand, the terminal
> abstraction layer, testing strategy, CI pipeline, release tooling, and
> observability. No Rust code is written here.

---

## 1. `grove launch` — exact behaviour

### 1.1 What the Python does today

`grove launch` iterates `registry.projects`, skips frozen entries (unless
`--frozen` is passed), sorts by tag alphabetically, and calls `_launch_project`
for each surviving entry. Each call opens **one new tab** in the configured
terminal with a shell command derived from the config key
`launch.shell_command`.

The real config in `~/.config/grove/config.json`:

```json
"launch": {
  "terminal": "wt",
  "wezterm_path": "/c/work/wezterm/target/release/wezterm.exe",
  "shell_command": "fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'"
}
```

The default in the Python source (fallback when key absent):

```
fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'
```

So each tab lands in the worktree directory, runs fish as a login shell, and
inside that fish session starts `claude --dangerously-skip-permissions
--continue` (resume the last Claude Code session). When/if claude exits, `exec
fish` replaces the claude process with an interactive fish so the tab stays
open.

Flags in scope today:

| flag | meaning |
|---|---|
| `tags...` (positional) | restrict to named tags |
| `--only tag1,tag2` | comma-list (deprecated; positional preferred) |
| `--dry-run` | print commands, do not exec |
| `--no-claude` | override `shell_command` with bare `fish -l` |
| `--no-continue` | strip `--continue` from the claude invocation |
| `--wezterm` | force wezterm even when config says `wt` |
| `--frozen` | include frozen projects |

### 1.2 Rust behaviour (exact parity + minor improvements)

The Rust `launch` subcommand must preserve all of the above. Additionally:

**Tab title format** — The Python sets the WezTerm tab title to the project
tag, or `❄ <tag>` for frozen projects. The Windows Terminal invocation passes
`--title '<tag>'`. The Rust version should follow the same convention. For
future multi-repo support, the tag alone is sufficient; if a `--prefix` option
is ever added the title becomes `<prefix>/<tag>`.

**Sort order** — Alphabetical by tag, same as Python. This is deterministic and
matches `list` output.

**`--only` flag** — Keep for backward compatibility but mark deprecated in help
text. Positional arguments are the canonical form.

**`--dry-run` output format** — Print one block per project:

```
[wt] proj-foo
  wt.exe -w 0 nt -p Ubuntu -d 'C:\work\desktop\proj-foo' --title 'proj-foo' wsl.exe -e fish -l -c '...'

[wt] proj-bar
  wt.exe ...
```

**Frozen skip message** — When frozen projects are skipped, print to stderr:
`grove: skipping N frozen project(s): tag1, tag2` — matches Python behaviour.

**Return code** — 0 on success, 1 if no projects matched the filter, 2 on
terminal spawn failure (at least one tab failed to open). This is a slight
improvement over Python which always exits 0.

---

## 2. Terminal abstraction

### 2.1 Trait design

```rust
// src/launch/terminal.rs  (sketch — not final Rust)

pub trait Terminal: Send + Sync {
    /// Human-readable name for display and config keys.
    fn name(&self) -> &'static str;

    /// Return true if this terminal appears to be available on the current host.
    fn is_available(&self) -> bool;

    /// Spawn one new tab/pane in the terminal, changing into `cwd` and running
    /// `shell_argv`. Set the tab title to `title`.
    ///
    /// Returns the pane/tab ID string if the terminal exposes one, else None.
    fn spawn_tab(
        &self,
        cwd: &WindowsPath,        // already converted to Windows format
        title: &str,
        shell_argv: &[&str],
        dry_run: bool,
    ) -> Result<Option<String>, LaunchError>;

    /// Set the title on an already-open tab by its ID. No-op if unsupported.
    fn set_tab_title(&self, tab_id: &str, title: &str) -> Result<(), LaunchError>;

    /// Return a CLI-level dry-run description of what `spawn_tab` would do.
    fn dry_run_description(
        &self,
        cwd: &WindowsPath,
        title: &str,
        shell_argv: &[&str],
    ) -> String;
}
```

`WindowsPath` is a newtype around `String` (see §3). It is the terminal's
concern to receive a Windows-format path; path conversion happens before
calling `spawn_tab`.

**Why a trait and not an enum?** An enum would require touching a `match` arm
everywhere a new terminal is added. A trait with a registry (a
`Vec<Box<dyn Terminal>>`) lets each backend live in its own file.

### 2.2 `WindowsTerminal` impl

```
wt.exe -w 0 nt -p Ubuntu -d '<win_path>' --title '<title>' wsl.exe -e <shell_argv...>
```

- Invoked with `std::process::Command::new("wt.exe")`.
- The `-w 0` targets the first Windows Terminal window; `-w new` opens a fresh
  window. Currently Python uses `-w 0`, so the Rust port does the same.
  Consider making it configurable (`launch.wt_window: 0 | new`).
- `-p Ubuntu` selects the Ubuntu profile. Configurable as
  `launch.wt_profile` (default `"Ubuntu"`).
- Does **not** return a tab ID. `set_tab_title` is a no-op.
- Windows Terminal does not provide a programmable API for tab IDs from the
  command line; the title is baked into the spawn command.

### 2.3 `WeztermTerminal` impl

```
<wezterm_exe> cli spawn --cwd <win_path> -- wsl.exe -e <shell_argv...>
```

- Returns the pane ID from stdout (`r.stdout.strip()`).
- Calls `set_tab_title` using the returned pane ID:
  `<wezterm_exe> cli set-tab-title --pane-id <id> <title>`
- Reads `wezterm_exe` from the `WeztermTerminal` struct field (set from config
  or autodetect).

### 2.4 Future impls (not in scope for v1, just name them)

| Name | Key | Notes |
|---|---|---|
| `TmuxTerminal` | `tmux` | `new-window -c <cwd> -n <title> <shell>` |
| `KittyTerminal` | `kitty` | `kitty @ launch --cwd <cwd> --tab-title <title>` |
| `GnomeTerminalTerminal` | `gnome-terminal` | `--tab --working-directory=<cwd>` |
| `AlacrittyTerminal` | `alacritty` | No native tab support; would open a new window |

### 2.5 Terminal detection and priority

When `config.launch.terminal` is absent or set to `"auto"`, probe in order:

1. Check `WEZTERM_PANE` env var — if set, we are inside WezTerm, use it.
2. Check `WT_SESSION` env var — if set, we are inside Windows Terminal, use it.
3. Try `wezterm.exe` on PATH.
4. Try `wt.exe` on PATH.
5. Fall back to error: "cannot detect terminal; set launch.terminal in config".

When `launch.wezterm_path` is set in config, use that path for WezTerm (the
real binary is at `/c/work/wezterm/target/release/wezterm.exe`, a local build).

Detection is done at runtime (not compile time) so the same binary works on
Linux, macOS, and Windows.

---

## 3. WSL ↔ Windows path bridging

### 3.1 The problem

Both `wt.exe` and `wezterm.exe` run on the Windows side. They expect Windows
paths (`C:\work\desktop\proj-foo`). WSL paths (`/c/work/desktop/proj-foo`)
must be converted before being passed as `--cwd` or `-d` arguments.

### 3.2 Current Python approach

```python
def wsl_to_win(path):
    s = str(path)
    m = re.match(r'^/([a-zA-Z])/(.*)', s)
    if m:
        drive = m.group(1).upper()
        rest = m.group(2).replace("/", "\\")
        return f"{drive}:\\{rest}"
    return s
```

This handles the `/c/work/...` → `C:\work\...` pattern only. There is also a
`_normalize_wsl_path` function that handles the reverse direction (Windows →
WSL) for pane CWD comparison; that belongs in the `close_panes` / lock-finding
logic.

### 3.3 Rust recommendation: pure Rust, no `wslpath` shell-out

Reason: `wslpath` is available but it requires spawning a subprocess, which
adds ~50ms per conversion and is not available when unit tests run on macOS/
Linux CI (where there is no WSL).

Implement two functions in `src/platform/wsl.rs`:

```
/// /c/work/foo  →  C:\work\foo
pub fn wsl_to_win(path: &Path) -> String

/// C:\work\foo  →  /c/work/foo
/// Also handles file:// URLs and UNC paths (from wezterm cwd field)
pub fn win_to_wsl(s: &str) -> PathBuf
```

The logic mirrors the Python exactly. It is cheap (no alloc beyond the output
string) and fully testable.

The `WindowsPath` newtype is a `String` that has been through `wsl_to_win`.
Constructing it is the only place conversion happens.

**Crate check**: The `wslpath` crate on crates.io (last updated 2020) is
abandoned. The `dunce` crate normalises Windows paths but does not do WSL
conversion. Write it ourselves — the logic is 10 lines.

---

## 4. Test strategy

### 4.1 Unit tests (per module, inline)

Every module that contains non-trivial logic gets `#[cfg(test)] mod tests { … }`
at the bottom of the file.

Priority targets:

| Module | What to test |
|---|---|
| `platform::wsl` | Round-trip conversions: `/c/foo` ↔ `C:\foo`, UNC paths, `file://` URLs |
| `registry` | add/remove/get, freeze/thaw, expiry parsing, atomic write |
| `config` | default merge, load, save, migration from old dir |
| `git` | `worktree_list` porcelain parser (feed synthetic output) |
| `launch::terminal` | `dry_run_description` output for each backend |
| `commands::list` | status computation from mock git output |
| `util::duration` | parse `30m`, `2h`, `3d`, `1w`, invalid inputs |

These tests run on all platforms (no filesystem, no subprocess). They are the
largest bucket and should cover the algorithmic core completely.

### 4.2 Integration tests (`tests/` directory)

Use `assert_cmd` + `predicates` to drive the actual `grove` binary compiled by
Cargo. Each test creates a throwaway git repo in a `tempdir`.

```
tests/
  common/mod.rs          — helpers: new_repo(), new_worktree(), grove_cmd()
  test_new.rs
  test_list.rs
  test_done.rs
  test_rename.rs
  test_freeze.rs
  test_launch.rs         — dry-run only (see §4.4)
  test_adopt.rs
```

**Helper pattern** — `grove_cmd()` returns an `assert_cmd::Command` pre-pointed
at the compiled binary with `GROVE_CONFIG_DIR` overridden to a temp path so
tests never touch `~/.config/grove`.

Environment variable `GROVE_CONFIG_DIR` controls config/registry location. This
is the only env override needed; the binary reads it at startup. No other
env vars need to be mocked.

Example sketch:

```
fn grove_cmd(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("grove").unwrap();
    cmd.env("GROVE_CONFIG_DIR", dir.path());
    cmd
}

#[test]
fn list_empty() {
    let td = TempDir::new().unwrap();
    grove_cmd(&td)
        .args(&["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No projects"));
}
```

Integration tests run on Linux (the primary WSL platform) and macOS. They are
excluded from Windows CI because they require a real `git` binary and the
tempdir paths differ; instead a subset of integration tests are tagged
`#[cfg(target_os = "windows")]` if ever needed.

### 4.3 Snapshot tests (`insta`)

Use `insta` for anything whose output format must not accidentally change:

- Help text (`grove --help`, `grove launch --help`, etc.)
- `grove list --json` output shape (keys, not values)
- Table formatting (column alignment when tag/branch names vary in length)
- `grove launch --dry-run` output

Snapshots live in `tests/snapshots/`. They are committed. CI runs `cargo insta
test --check` (no interactive review in CI). Developers run `cargo insta review`
locally when they intentionally change output.

Do **not** snapshot stderr output that contains ANSI codes — strip colors in
tests or use `NO_COLOR=1`.

### 4.4 Launch tests: mock terminals

`grove launch` is the hardest subcommand to test because it shells out to
`wt.exe` / `wezterm.exe`. Strategy:

1. In tests, set env var `GROVE_TERMINAL_OVERRIDE` to the path of a small fake
   binary (written in Rust, lives in `tests/helpers/fake_terminal/`).
2. `fake_terminal` records its argv to `$FAKE_TERMINAL_LOG` (a temp file),
   prints a fake pane ID to stdout, exits 0.
3. Tests assert on the log file contents using `assert_cmd` helpers.

The terminal abstraction trait makes this clean: in the test build, the
`TerminalRegistry::detect()` function checks `GROVE_TERMINAL_OVERRIDE` first
and returns a `ShellTerminal` that execs the provided binary.

`--dry-run` tests require no fake terminal at all; they just assert on stdout
via `assert_cmd`.

### 4.5 Cross-platform test matrix

| Test type | Linux (WSL CI) | macOS | Windows |
|---|---|---|---|
| Unit tests | yes | yes | yes |
| Integration: git ops | yes | yes | skip (path issues) |
| Integration: launch dry-run | yes | yes | yes |
| Integration: launch real | yes (fake terminal) | yes (fake terminal) | yes (fake terminal) |
| Snapshot tests | yes | yes | yes |

The majority of grove's interesting behaviour (git, registry, path conversion)
is tested on Linux. macOS is a sanity check. Windows runs only unit + dry-run
so CI stays fast (Windows runners are 3x slower on GitHub Actions).

---

## 5. CI: GitHub Actions

### 5.1 Workflow overview

Three workflows:

1. **`ci.yml`** — runs on every push + PR. lint, test, coverage.
2. **`release.yml`** — runs on `v*` tags. build cross-platform, upload artifacts.
3. **`bench.yml`** — optional, runs on schedule or manual trigger. Codspeed.

### 5.2 `ci.yml` — full annotated YAML

```yaml
name: CI

on:
  push:
    branches: ["main"]
  pull_request:

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  # ── lint ──────────────────────────────────────────────────────────────────
  lint:
    name: Lint
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install stable toolchain + components
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Format check
        run: cargo fmt --all -- --check

      - name: Clippy (deny warnings)
        run: cargo clippy --all-targets --all-features -- -D warnings

  # ── test ──────────────────────────────────────────────────────────────────
  test:
    name: Test (${{ matrix.os }})
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    steps:
      - uses: actions/checkout@v4

      - name: Install stable toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.os }}

      # Install git (guaranteed on ubuntu/macos; confirm version on windows)
      - name: Confirm git version
        run: git --version

      - name: Build
        run: cargo build --all-targets

      - name: Run tests
        run: cargo test --all-targets
        env:
          # Prevent integration tests from touching ~/.config/grove
          GROVE_CONFIG_DIR: ${{ runner.temp }}/grove-test-config

      - name: Run insta snapshot check
        run: cargo insta test --check --unreferenced=warn
        # Only enforce snapshots on linux to avoid platform-specific line endings
        if: matrix.os == 'ubuntu-latest'

  # ── coverage ──────────────────────────────────────────────────────────────
  coverage:
    name: Coverage
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install stable toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Install cargo-llvm-cov
        uses: taiki-e/install-action@cargo-llvm-cov

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Generate coverage
        run: cargo llvm-cov --all-features --lcov --output-path lcov.info
        env:
          GROVE_CONFIG_DIR: ${{ runner.temp }}/grove-test-config

      - name: Upload to Codecov
        uses: codecov/codecov-action@v4
        with:
          files: lcov.info
          fail_ci_if_error: false    # coverage upload failure should not block merge
        env:
          CODECOV_TOKEN: ${{ secrets.CODECOV_TOKEN }}

  # ── MSRV check (minimum supported Rust version) ──────────────────────────
  msrv:
    name: MSRV
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install MSRV toolchain
        uses: dtolnay/rust-toolchain@master
        with:
          toolchain: "1.80"    # update Cargo.toml rust-version field to match
      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
      - name: Check (no tests — just compilation)
        run: cargo check --all-targets
```

### 5.3 `release.yml` — cross-platform release builds

```yaml
name: Release

on:
  push:
    tags:
      - "v*"

permissions:
  contents: write

jobs:
  build:
    name: Build (${{ matrix.target }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
            artifact: grove
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-latest
            artifact: grove
            cross: true
          - target: x86_64-apple-darwin
            os: macos-latest
            artifact: grove
          - target: aarch64-apple-darwin
            os: macos-latest
            artifact: grove
          - target: x86_64-pc-windows-msvc
            os: windows-latest
            artifact: grove.exe

    steps:
      - uses: actions/checkout@v4

      - name: Install stable toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Install cross (if needed)
        if: matrix.cross
        run: cargo install cross --git https://github.com/cross-rs/cross

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}

      - name: Build release binary
        run: |
          if [ "${{ matrix.cross }}" = "true" ]; then
            cross build --release --target ${{ matrix.target }}
          else
            cargo build --release --target ${{ matrix.target }}
          fi
        shell: bash

      - name: Strip binary (Linux/macOS only)
        if: runner.os != 'Windows'
        run: |
          strip target/${{ matrix.target }}/release/${{ matrix.artifact }} || true
        shell: bash

      - name: Package artifact
        shell: bash
        run: |
          TAG=${GITHUB_REF#refs/tags/}
          STEM="grove-${TAG}-${{ matrix.target }}"
          mkdir -p dist
          if [ "${{ runner.os }}" = "Windows" ]; then
            cp target/${{ matrix.target }}/release/${{ matrix.artifact }} dist/
            cd dist && 7z a "../${STEM}.zip" grove.exe
          else
            cp target/${{ matrix.target }}/release/${{ matrix.artifact }} dist/
            cd dist && tar czf "../${STEM}.tar.gz" grove
          fi

      - name: Upload artifact to release
        uses: softprops/action-gh-release@v2
        with:
          files: "*.tar.gz\n*.zip"
          fail_on_unmatched_files: true
```

### 5.4 `bench.yml` — optional benchmarks

```yaml
name: Benchmarks

on:
  workflow_dispatch:   # manual trigger only; add schedule if desired
  # schedule:
  #   - cron: '0 3 * * 1'   # every Monday 03:00 UTC

jobs:
  bench:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Run criterion benchmarks
        run: cargo bench --bench bench_main -- --output-format bencher | tee bench.txt
      - name: Upload results
        uses: benchmark-action/github-action-benchmark@v1
        with:
          tool: cargo
          output-file-path: bench.txt
          github-token: ${{ secrets.GITHUB_TOKEN }}
          auto-push: true
          alert-threshold: "110%"
          comment-on-alert: true
```

CodSpeed is an alternative to Criterion + benchmark-action. The CodSpeed GitHub
Action (`CodSpeedHQ/action@v2`) wraps cargo bench with Valgrind for
instruction-count-based stable measurements. Use it instead of the manual
criterion workflow if you want sub-noise stability and a web dashboard without
hosting your own. The tradeoff: CodSpeed requires a free account and injects
Valgrind instrumentation (slower per-run but consistent).

**Recommendation**: Start with Criterion + benchmark-action (no external
account). Add CodSpeed if you want CI-gated perf regression detection later.

---

## 6. Release and install

### 6.1 Tool choice: `cargo dist`

**Use `cargo-dist`** (Axo Labs). Reasons:

- Generates a GitHub Actions release workflow, cross-platform matrix, and a
  `curl | sh` installer in one `cargo dist init` invocation.
- Produces `.tar.gz` for Linux/macOS, `.zip` for Windows, and a
  `dist-manifest.json` that `cargo-binstall` can consume.
- Handles code-signing hooks for macOS if needed later.
- Mature (v0.22+), actively maintained, used by ripgrep, fd, and other CLI
  tools in the same niche.

**Not hand-rolling** because maintaining a cross-compilation matrix across 5
targets plus installer scripts is significant toil with no benefit over cargo-
dist for a single-binary CLI.

**Not `goreleaser`** — that is for Go.

Setup:

```toml
# Cargo.toml additions
[package]
name = "grove"
version = "0.1.0"
rust-version = "1.80"

[workspace.metadata.dist]
cargo-dist-version = "0.22"
ci = ["github"]
installers = ["shell", "powershell"]
targets = [
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-pc-windows-msvc",
]
```

`cargo dist init` writes `.github/workflows/release.yml` automatically.
The hand-rolled `release.yml` in §5.3 above can be deleted once cargo-dist is
set up; they cover the same ground.

### 6.2 Artifacts

| Target | Archive | Notes |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `grove-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | Primary for WSL |
| `aarch64-unknown-linux-gnu` | `grove-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | Raspberry Pi / ARM servers |
| `x86_64-apple-darwin` | `grove-vX.Y.Z-x86_64-apple-darwin.tar.gz` | Intel Mac |
| `aarch64-apple-darwin` | `grove-vX.Y.Z-aarch64-apple-darwin.tar.gz` | Apple Silicon |
| `x86_64-pc-windows-msvc` | `grove-vX.Y.Z-x86_64-pc-windows-msvc.zip` | Windows native |

The Windows binary is compiled but its utility is limited (no WSL path
bridging, terminals are WSL-side concepts). It is provided for completeness and
for users who might run grove natively on Windows Git Bash.

### 6.3 Install options

**Primary: `cargo-binstall`**

```
cargo binstall grove
```

This downloads the pre-built binary from the GitHub release. No compilation
required. Works immediately after the first tagged release as long as
`dist-manifest.json` is published (cargo-dist handles this). Oystein already
has cargo-binstall; this is the daily-driver upgrade path.

**Secondary: `curl | sh` (generated by cargo-dist)**

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/oysteinkrog/grove/releases/latest/download/grove-installer.sh | sh
```

Installs to `~/.cargo/bin/` by default. Suitable for CI environments or fresh
machines without cargo-binstall.

**Alternate: `cargo install` (build from source)**

```
cargo install --git https://github.com/oysteinkrog/grove
```

Slow (compiles), but requires no release infrastructure. Useful for unreleased
changes. Not the recommended path.

**Not a Homebrew tap** — Oystein does not use macOS as primary workstation.
Adding a tap has maintenance overhead (formula must track releases). Skip for
now; add if there is a Mac user base.

### 6.4 Oystein's personal upgrade path

Current state: `~/bin/grove` is the Python script, directly on PATH.

Migration plan:

1. When Rust `grove` reaches feature parity, `cargo binstall grove` installs it
   to `~/.cargo/bin/grove`.
2. In fish config, ensure `~/.cargo/bin` is before `~/bin` on PATH (it likely
   already is).
3. Remove or rename `~/bin/grove` to `~/bin/grove.py` so there is no ambiguity.
4. Verify: `which grove` → `~/.cargo/bin/grove`.

For in-between development (before v1.0 tag exists on GitHub):

```bash
cargo install --path /c/work/grove --force
```

This puts the built binary at `~/.cargo/bin/grove` immediately and `grove
--version` shows the cargo.toml version. No need to touch PATH or `~/bin`.

---

## 7. Observability

### 7.1 Logging crate: `tracing`

Use the `tracing` crate with `tracing-subscriber` for structured, levelled
logging. Reasons:

- `log` + `env_logger` is simpler but gives no structured fields. When
  diagnosing why a tab didn't open, structured fields (`project`, `terminal`,
  `path`) are useful.
- `tracing` is the de-facto standard for new Rust CLI tools (tokio, axum, etc.
  all use it; the ecosystem integration is good).
- Overhead is negligible when tracing is disabled (the default).

### 7.2 Log levels and `--verbose` / `-v`

```
grove [GLOBAL OPTIONS] <subcommand> ...

Global options (parsed before subcommand):
  -v, --verbose      Enable INFO-level logging (same as RUST_LOG=grove=info)
  -vv                Enable DEBUG-level logging (RUST_LOG=grove=debug)
  -q, --quiet        Suppress all non-error output
```

Implementation: count `-v` occurrences in clap (using `action = ArgAction::Count`).

```
0   → WARN only    (default; production-ready silence)
1   → INFO         (-v; shows "Launching tab for proj-foo", "Fetching remote")
2   → DEBUG        (-vv; shows path conversion, git command argv)
3+  → TRACE        (-vvv; shows registry load/save, config parsing details)
```

`RUST_LOG` env var is also honoured (tracing-subscriber's `EnvFilter`). If both
`-v` and `RUST_LOG` are set, the more verbose one wins.

### 7.3 Log output destination

- INFO and below → stderr (same as Python's `info()` which prints to stderr).
- User-facing output (table, JSON, path lines for the fish `__POSTCD__`
  protocol) → stdout.
- Errors → stderr.

This mirrors the Python convention and is important because the fish wrapper
captures stdout to intercept `__POSTCD__` lines.

### 7.4 `GROVE_DEBUG` env var (parity with Python)

The Python checks `os.environ.get("GROVE_DEBUG")` in `find_directory_locks`
to print pane CWD normalisation details. In Rust, `GROVE_DEBUG=1` enables
DEBUG-level tracing for the `grove::platform::wsl` and
`grove::launch::terminal` modules specifically. The actual mechanism is just
`RUST_LOG=grove::platform::wsl=debug,grove::launch=debug grove ...`.

For convenience, in the binary startup:

```rust
if std::env::var("GROVE_DEBUG").is_ok() {
    std::env::set_var("RUST_LOG", "grove=debug");
}
```

(Before the subscriber is initialised.)

---

## 8. Performance budget

### 8.1 Targets

| Operation | Target p99 | Notes |
|---|---|---|
| `grove list` (10 projects, no git) | < 30 ms | Table print, no git status |
| `grove list` (10 projects, with git status) | < 500 ms | Parallel git status |
| `grove list --json` (10 projects) | < 30 ms | Just serialise registry |
| `grove launch --dry-run` (10 projects) | < 20 ms | No subprocess spawn |
| `grove cd <tag>` | < 10 ms | Registry lookup + print |
| `grove new <tag>` | < 3 s (excl. git fetch) | Git fetch is the bottleneck |
| Cold start (first byte of output) | < 50 ms | Startup + config parse |

These are soft targets for a usable tool; the Python currently takes 200-400 ms
for `grove list` on 10 projects (all git calls are synchronous). Rust should
be 5-10x faster.

### 8.2 Parallelism strategy

`grove list` with git status (dirty check + ahead/behind) must run git
operations in parallel, just as the Python does with `ThreadPoolExecutor`.

In Rust: use `rayon::par_iter()` over the project list. Each item calls `gix`
APIs (synchronous) in a rayon worker thread. Rayon's thread pool is
appropriately sized for the number of CPUs and reused across calls.

`tokio` is **not** needed. All I/O in grove is either:
- Fast filesystem reads (registry, config)
- `git` subprocess or `gix` synchronous calls
- Terminal subprocess spawn (one-shot, not concurrent)

Do not add tokio unless a future feature explicitly requires async I/O.

### 8.3 Benchmarks

Use `criterion` (not Divan — criterion is more established and has better CI
integration via `benchmark-action`).

Benchmark targets:

```
benches/
  bench_main.rs
  |-- bench_list_10         — list 10 projects (no status, just table)
  |-- bench_list_10_status  — list 10 projects with full git status (rayon)
  |-- bench_registry_load   — parse registry.json (100 entries)
  |-- bench_wsl_path        — 1000x wsl_to_win conversions
```

Benchmarks create throwaway git repos in tempdir with `git2` (or shell) setup.
They are excluded from normal `cargo test` and run only with `cargo bench`.

### 8.4 Measurement methodology

1. Baseline the Python binary: `hyperfine 'grove list' 'grove list --json'` on
   the real registry.
2. Build Rust in release mode (`cargo build --release`).
3. `hyperfine --warmup 5 './target/release/grove list'` to measure cold vs warm.
4. For the git-status path, ensure the test repos have some actual commits and
   dirty state to make the benchmark realistic.

---

## 9. Telemetry / privacy

**No telemetry. None.**

grove is a local CLI tool. It makes no network calls except explicit git
operations the user requests (fetches, pushes). No analytics, no crash
reporting, no update-check pings. This is explicit policy, not an oversight.

No feature flag, config option, or environment variable should enable telemetry
now or in the future. If a dependency (unlikely for a CLI of this scope) adds
telemetry, evaluate replacing it.

Update checking: if ever desired, implement it as an **opt-in** user action
(`grove update`), not as a background check on every invocation. `cargo
binstall` already handles updates; a built-in update checker is unnecessary.

---

## 10. Open questions / decisions deferred

These are noted here so they are not silently dropped:

1. **Config schema versioning** — The Python silently merges defaults for
   missing keys. Rust should define a versioned schema (add `"schema_version": 1`
   to config.json) so future breaking changes can be detected and migrated.

2. **`launch.shell_command` templating** — Currently a raw string with `exec fish`
   hardcoded. Consider a template variable like `{tag}` or `{path}` so users
   can e.g. set the terminal title differently per project. Not in v1.

3. **Windows Terminal profile detection** — `wt.exe -p Ubuntu` hardcodes the WSL
   profile name. Different machines may have `Ubuntu-22.04` or `Debian`. Add
   autodetect via `wt.exe --list-profiles` or make `launch.wt_profile`
   configurable with `"Ubuntu"` as default.

4. **Concurrent launch** — The Python launches tabs sequentially. For large
   registries (10+ projects), spawning all at once would be faster. However,
   `wt.exe` has a known race condition when spawned in rapid succession (it
   sometimes misses tabs). WezTerm handles concurrent spawns fine. Defer for
   now; keep sequential.

5. **`__POSTCD__` protocol** — The fish wrapper intercepts `__POSTCD__<path>`
   on stdout to `cd` the interactive shell. The Rust binary must preserve this
   protocol exactly. Document it as an internal protocol in `PROTOCOL.md`.

---

## Summary

**Biggest decisions: Launch**

The `launch` subcommand wraps platform binaries (`wt.exe` / `wezterm.exe`) that
are Windows-side processes invoked from WSL. The key architectural choice is a
`Terminal` trait with per-terminal impls, enabling mock injection in tests and
clean future extension. Path conversion is done purely in Rust (10-line
`wsl_to_win` function); no `wslpath` shell-out is used. The tab command is
`wsl.exe -e fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'`
with the CWD set to the Windows-format worktree path.

**Biggest decisions: Release and install**

`cargo-dist` is chosen over hand-rolling the release workflow. It generates the
CI matrix, produces `dist-manifest.json` (consumed by `cargo-binstall`), and
ships a `curl | sh` installer — eliminating ~200 lines of workflow YAML. For
Oystein's day-to-day use, the upgrade path is `cargo binstall grove` replacing
the Python script at `~/bin/grove` by putting the Rust binary at
`~/.cargo/bin/grove` (ahead on PATH). The MSRV is pinned to Rust 1.80 and
enforced in CI. No telemetry, ever.
