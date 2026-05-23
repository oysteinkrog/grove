# grove-rewrite: Git Internals (gix/gitoxide)

**Facet:** Git/worktree internals using gix (gitoxide)
**gix version:** 0.83.0 (as of 2026-05-23)
**Source analyzed:** `/c/users/oystein/bin/grove` (1936 lines, Python)

---

## 1. Operation Inventory

Every git operation grove performs today, extracted from the `Git` class and command dispatch.

| Operation | Python shell command | gix API path | Fallback needed? |
|-----------|----------------------|--------------|------------------|
| **fetch remote** | `git fetch <remote>` | `repo.find_fetch_remote(Some(name))?.connect(Direction::Fetch)?.prepare_fetch(progress, opts)?.receive(progress, interrupt)` — requires `blocking-network-client` feature | **Yes (see §8)** — auth helper integration is uncertain |
| **worktree add (new branch)** | `git worktree add -b <branch> <path> <base>` | No native API exists in gix 0.83.0. Planned in issue #2596 (closed as "will implement myself"); PR #2601 abandoned. Must manipulate `.git/worktrees/<id>/` manually or shell out. | **Yes — shell fallback** |
| **worktree add (existing branch)** | `git worktree add <path> <branch>` | Same as above — no native API. | **Yes — shell fallback** |
| **worktree remove** | `git worktree remove [--force] <path>` | No API in gix. `Proxy` has `is_locked()` and `lock_reason()` but no remove/prune methods. | **Yes — shell fallback** |
| **worktree move** | `git worktree move <old> <new>` | No API in gix. | **Yes — shell fallback** |
| **worktree list** | `git worktree list --porcelain` | `repo.worktrees()` → `Vec<worktree::Proxy<'_>>`. Returns linked worktrees only (not main). Each proxy: `.id()`, `.base()`, `.git_dir()`, `.is_locked()`, `.lock_reason()`. Branch requires `proxy.into_repo()?.head_name()`. | **No — fully native** |
| **worktree prune** | `git worktree prune` | No API in gix. | **Yes — shell fallback** |
| **is_dirty check** | `git status --porcelain` | `repo.is_dirty()` (feature: `status`) → `Result<bool, status::is_dirty::Error>`. Returns true if index or working tree differ from HEAD. | **No — fully native** |
| **untracked files list** | `git ls-files --others --exclude-standard` | `repo.status(progress)` → configure `UntrackedFiles` → iterate `Item`s. Or use `gix_dir` crate for directory walking with ignore rules. | **No — fully native** (slightly verbose API) |
| **uncommitted changes list** | `git diff --name-only HEAD` + `git diff --cached --name-only` | `repo.index_worktree_status(...)` and `repo.tree_index_status(...)` — available on `Repository`. Returns structured diff. | **No — fully native** |
| **ahead/behind count** | `git rev-list --left-right --count <branch>...<upstream>` | `repo.rev_walk([tip]).with_boundary([base]).all()?.count()` — two passes, one per direction. See `gix::revision::walk::Platform`. | **No — fully native** |
| **rev-parse / verify ref** | `git rev-parse --verify --quiet <ref>` | `repo.find_reference(refname)` or `repo.try_find_reference(refname)` → `Option<Reference<'_>>`. | **No — fully native** |
| **merge-base** | `git merge-base <ref1> <ref2>` | `gix_revision::merge_base` in `gix_revwalk` crate, or `repo.rev_walk` with boundary. No single-call `repo.merge_base()` on `Repository` yet. | **Partial** — use `gix_revision` plumbing directly |
| **rev-list count** | `git rev-list --count <ref>..<branch>` | `repo.rev_walk([branch]).with_boundary([ref]).all()?.count()` | **No — fully native** |
| **branch show-current** | `git branch --show-current` | `repo.head_name()?.shorten()` (yields short branch name) | **No — fully native** |
| **branch delete** | `git branch -D <branch>` | `repo.find_reference("refs/heads/<branch>")?.delete()` | **No — fully native** |
| **push (with -u tracking)** | `git push -u <remote> <branch>` | `repo.find_remote(name)?.connect(Direction::Push)?` — but push in gix 0.83.0 is feature-incomplete for typical branch push with tracking setup. | **Yes — shell fallback** |
| **push delete remote branch** | `git push <remote> --delete <branch>` | Same push limitation. | **Yes — shell fallback** |
| **is-ancestor check** | `git merge-base --is-ancestor <a> <b>` | `repo.rev_walk([a]).with_boundary([b]).all()?.next().is_none()` (empty walk = a is ancestor of b) | **No — fully native** |
| **diff --name-only (squash detection)** | `git diff --name-only <merge_base> <branch>` | `repo.diff_tree_to_tree(tree_a, tree_b, opts)?.into_iter()` — available via `gix::diff`. | **No — fully native** |
| **remote branch exists** | `git rev-parse --verify refs/remotes/<remote>/<branch>` | `repo.try_find_reference("refs/remotes/<remote>/<branch>")?.is_some()` | **No — fully native** |
| **auto-commit WIP** | `git add -A && git commit -m "WIP: ..."` | `repo.index()?.update_from_worktree(...)` then `repo.commit(...)` — gix has commit API but it is low-level. | **Yes — shell fallback** (high complexity, low value to reimplement) |
| **rmdir via cmd.exe** | `cmd.exe /c rmdir /s /q <winpath>` | Not a git operation. Keep as-is. | N/A |

**Summary:** Out of ~22 distinct git operations, approximately **12 are fully implementable with gix**, **8 require shell fallback**, and 2 are non-git OS calls. The biggest gaps are worktree lifecycle (add/remove/move/prune) and push.

---

## 2. gix Worktree API Status

### What gix 0.83.0 actually provides

**`gix::Repository::worktrees()`** (in `gix/src/repository/worktree.rs`):
```rust
pub fn worktrees(&self) -> std::io::Result<Vec<worktree::Proxy<'_>>>
```
Reads `.git/worktrees/` directory, returns one `Proxy` per linked worktree. Main worktree is **not** included (by design). Sorted by git_dir path.

**`gix::worktree::Proxy<'repo>`** methods:
- `.id() -> &BStr` — worktree name (folder name inside `.git/worktrees/`)
- `.base() -> Result<PathBuf>` — path to working tree checkout
- `.git_dir() -> &Path` — path to the admin dir inside `.git/worktrees/<id>/`
- `.is_locked() -> bool` — reads `.git/worktrees/<id>/locked`
- `.lock_reason() -> Option<BString>` — contents of lock file
- `.into_repo() -> Result<Repository, Error>` — open as full Repository (gives access to HEAD, refs, etc.)

**What gix does NOT provide (as of 0.83.0):**
- `worktree add` — issue #2596 was closed May 13, 2026 as "will implement myself" by Byron (Sebastian Thiel). PR #2601 was a community attempt that was rejected as needing architectural refinement. **This feature is actively being worked on but not yet released.**
- `worktree remove` — no API.
- `worktree move` — no API.
- `worktree prune` — no API.
- Getting the branch for a worktree requires `proxy.into_repo()?.head_name()` which opens the full repository object.

### Manual worktree add (lowest-level fallback)

If we do not want to shell out to `git worktree add`, we can implement it manually using gix primitives. The `git worktree add` command does the following at the filesystem level:

1. **Create the directory** at `<path>`.
2. **Create branch** (if `-b`): write `refs/heads/<branch>` pointing to `<base_commit>`.
3. **Create `.git/worktrees/<id>/` directory** in the main repo.
4. **Write `.git/worktrees/<id>/gitdir`**: contains the path to the worktree's `.git` file.
5. **Write `.git/worktrees/<id>/commondir`**: contains `../..` (path back to main `.git`).
6. **Write `.git/worktrees/<id>/HEAD`**: contains `ref: refs/heads/<branch>`.
7. **Write `<path>/.git`** (a file, not directory): contains `gitdir: <absolute-path-to-.git/worktrees/<id>>`.
8. **Check out the branch** into `<path>` using the index.

Using gix:
```rust
use gix_ref::transaction::{PreviousValue, RefEdit};
use gix_ref::FullName;

// Step 2: create branch
let branch_name = FullName::try_from(format!("refs/heads/{branch}"))?;
repo.edit_reference(RefEdit {
    change: gix_ref::transaction::Change::Update {
        log: Default::default(),
        expected: PreviousValue::MustNotExist,
        new: Target::Peeled(base_commit_id),
    },
    name: branch_name,
    deref: false,
})?;

// Steps 3-7: manual filesystem writes (no gix API)
// Step 8: checkout using gix_worktree_state
use gix_worktree_state::checkout;
```

Step 8 checkout is available via the `gix-worktree-state` crate (re-exported as `gix::worktree::state` with `worktree-mutation` feature). This is what PR #2601 attempted but encountered `Send` bound issues with `in_parallel_with_finalize`.

**Recommendation:** Shell out to `git worktree add` for now. When Byron's implementation lands (expected in gix 0.84-0.85 based on issue #2596 closure date), switch to the native API. Design the worktree module with a `trait WorktreeBackend` so the implementation can be swapped without changing call sites.

### Why `gix-worktree-state` (not `gix-worktree`) for checkout

- `gix-worktree` provides `Stack` — a utility for traversing paths while consulting `.gitattributes` and `.gitignore`. It is a low-level building block, **not** a checkout engine.
- `gix-worktree-state` (`gix::worktree::state`) provides the actual checkout: `gix_worktree_state::checkout(...)`.
- `gix-discover` is used by `gix::discover()` to find the git dir from any working directory path.
- `gix-ref` is used for reference manipulation (branch creation, deletion, updates via `RefTransaction`).

---

## 3. Status Detection

### is_dirty (uncommitted changes)

```rust
// Fast path: bool
let dirty = repo.is_dirty()?;  // feature = "status"
// Returns true if index ≠ HEAD tree OR worktree ≠ index
```

`Repository::is_dirty()` is the workhorse for grove's `_get_status()` function. It subsumes both `git diff --name-only HEAD` and `git diff --cached --name-only` in a single call.

### Untracked files

```rust
let platform = repo.status(gix::progress::Discard)?;
// Configure untracked file handling:
// platform.untracked_files(gix::status::UntrackedFiles::Files)
for item in platform.into_iter(None)? {
    match item? {
        gix::status::Item::IndexWorktree(change) => { /* staged or unstaged change */ }
        gix::status::Item::TreeIndex(change) => { /* staged vs HEAD */ }
    }
}
```

For grove's `untracked_files()` and `uncommitted_changes()` functions, the `status` platform replaces two separate shell invocations with one structured iterator.

### ahead/behind counts

The Python uses `git rev-list --left-right --count <branch>...<upstream>`. In gix:

```rust
fn ahead_behind(
    repo: &gix::Repository,
    local: gix::ObjectId,
    upstream: gix::ObjectId,
) -> Result<(usize, usize)> {
    let ahead = repo
        .rev_walk([local])
        .with_boundary([upstream])
        .all()?
        .count();
    let behind = repo
        .rev_walk([upstream])
        .with_boundary([local])
        .all()?
        .count();
    Ok((ahead, behind))
}
```

This requires resolving branch names to OIDs first:
```rust
let local_oid = repo.find_reference("refs/heads/<branch>")?.id();
let upstream_oid = repo.find_reference("refs/remotes/<remote>/<branch>")?.id();
```

**Performance:** Two revwalk passes is equivalent to what git does internally for `--left-right --count`. For grove's `list` command, this is called for every project in parallel (see §6). The revwalk is in-process (no subprocess), so even two passes per worktree will be significantly faster than Python's subprocess-per-worktree approach.

**Alternative:** `gix_revision::merge_base` from `gix-revwalk` can compute the merge base first, then two forward-count walks from the merge base. This is more efficient for repos with deep history because the boundary stops traversal early. Use this for production.

### Squash-merge detection

The Python's `is_merged()` function checks:
1. `rev-list --count ref..branch == 0` (fast path)
2. File-level diff comparison against upstream (squash merge detection)

For the squash detection path, use:
```rust
let merge_base_id = repo.rev_walk([upstream]).with_boundary([branch_tip]).all()?
    .last()  // approximate; use gix_revision::merge_base for correctness
    .ok_or(...)?.id;

let diff = repo.diff_tree_to_tree(
    Some(repo.find_object(merge_base_id)?.peel_to_tree()?),
    Some(repo.find_object(branch_tip)?.peel_to_tree()?),
    Default::default(),
)?;
// Then compare those changed files against upstream content
```

This is doable natively but more complex than a shell call. Since this is only a safety check during `grove done` (not hot path), **shell fallback is acceptable here**.

---

## 4. Branch and Remote Operations

### Create branch

```rust
use gix_ref::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix_ref::{FullName, Target};

let edit = RefEdit {
    name: FullName::try_from("refs/heads/my-branch")?,
    change: Change::Update {
        log: RefLog {
            message: "branch: Created from if/master".into(),
            ..Default::default()
        },
        expected: PreviousValue::MustNotExist,
        new: Target::Peeled(base_commit_id),
    },
    deref: false,
};
repo.edit_reference(edit)?;
```

### Delete branch

```rust
repo.find_reference("refs/heads/my-branch")?.delete()?;
```

### Set upstream tracking

After creating a branch, set `branch.<name>.remote` and `branch.<name>.merge` in git config:
```rust
let mut config = repo.config_snapshot_mut();
config.set_raw_value(
    "branch", Some("my-branch"), "remote", b"my"
)?;
config.set_raw_value(
    "branch", Some("my-branch"), "merge", b"refs/heads/my-branch"
)?;
config.commit()?;
```

This is done via `gix::config` — the `Repository::config_snapshot_mut()` API. This replaces the `-u` flag behavior from `git push -u`.

### Fetch

```rust
let remote = repo.find_fetch_remote(Some("if".into()))?;
let connection = remote.connect(Direction::Fetch)?;
let outcome = connection
    .prepare_fetch(gix::progress::Discard, Default::default())?
    .receive(gix::progress::Discard, &AtomicBool::new(false))?;
```

Requires `blocking-network-client` or `async-network-client` feature. See §8 for auth concerns.

### Push

**gix 0.83.0 push status:** The push module exists (`gix::push`) but is not well-documented and the feature is still maturing. Push requires `blocking-network-client`. There is no high-level `remote.push(branch)` API equivalent to `git push -u origin main`.

**Decision: Shell fallback for all push operations.** This includes:
- `git push -u <remote> <branch>`
- `git push <remote> --delete <branch>`
- `git push <remote> <branch>` (WIP auto-save)

Rationale: These are rare operations (only in `grove done` and `freeze` expiry), and push is one of the least stable parts of the gix API. The risk of credential issues or protocol edge cases is high.

---

## 5. Fork Operation

`grove fork` creates a new worktree from an existing project's branch. The gix call sequence:

```rust
pub fn fork_worktree(
    cfg: &Config,
    repo: &gix::Repository,
    source_branch: &str,     // e.g. "DESKTOP-1234-myfeature"
    new_tag: &str,           // e.g. "myfeature2"
    new_branch: &str,        // e.g. "DESKTOP-5678-myfeature2"
    worktree_path: &Path,
) -> Result<()> {
    // 1. Resolve source branch tip
    let source_ref = format!("refs/heads/{source_branch}");
    let source_oid = repo.find_reference(&source_ref)?.id().detach();

    // 2. Create new branch pointing at source branch tip
    let new_ref = format!("refs/heads/{new_branch}");
    repo.edit_reference(RefEdit {
        name: FullName::try_from(new_ref)?,
        change: Change::Update {
            log: RefLog { message: format!("branch: forked from {source_branch}").into(), ..Default::default() },
            expected: PreviousValue::MustNotExist,
            new: Target::Peeled(source_oid),
        },
        deref: false,
    })?;

    // 3. Create worktree at path
    // FALLBACK: git worktree add (gix has no native worktree-add yet)
    std::process::Command::new("git")
        .args(["-C", repo.path().to_str().unwrap(),
               "worktree", "add", "--no-checkout",
               worktree_path.to_str().unwrap(), new_branch])
        .status()?;

    // 4. Checkout using gix-worktree-state (optional, if worktree-mutation feature enabled)
    //    OR let git do it without --no-checkout above

    // 5. Copy Claude Code sessions from source path to new path (filesystem, no gix)
    copy_claude_sessions(source_path, worktree_path);

    Ok(())
}
```

Once gix gains native worktree add (expected soon after issue #2596), step 3 becomes:
```rust
repo.add_worktree(worktree_path, AddWorktreeOptions {
    branch: Some(new_branch),
    ..Default::default()
})?;
```

---

## 6. Multi-Repo Coordination for `grove list`

The current Python uses `ThreadPoolExecutor(max_workers=8)` to parallelize status checks across all registered projects. Each `_get_status(tag)` call does:
- `git status --porcelain` (is_dirty)
- `git rev-list --left-right --count` (ahead/behind)

Both are subprocess calls today, each paying fork+exec overhead (~30-60ms on WSL). With gix, these become in-process operations but still do I/O (read index file, read pack files).

### Concurrency model recommendation: **rayon**

**Rationale:**
- Status checks are **CPU-bound + I/O-bound** work with no async benefit on WSL1 (which uses synchronous I/O under the hood)
- `gix::Repository` is not `Send` — it is a thread-local handle. However, `gix::ThreadSafeRepository` is `Send + Sync` and can be converted: `repo.into_sync()` → `ThreadSafeRepository`
- Each rayon worker calls `thread_safe_repo.to_thread_local()` to get a `Repository` for its thread
- rayon's work-stealing pool naturally handles N worktrees with configurable parallelism

```rust
use rayon::prelude::*;

let results: Vec<_> = projects
    .par_iter()
    .map(|(tag, project)| {
        let repo = ThreadSafeRepository::open(&project.path)?.to_thread_local();
        let dirty = repo.is_dirty()?;
        let (ahead, behind) = if !dirty {
            compute_ahead_behind(&repo, &project.branch, &cfg.upstream_remote)?
        } else {
            (0, 0)
        };
        Ok((tag, dirty, ahead, behind))
    })
    .collect();
```

**tokio vs rayon decision:**
- tokio is appropriate when you need async network I/O (fetch/push). grove's `list` command does **no network I/O** (it reads local pack files only).
- rayon is simpler, has no runtime setup, and performs better for CPU-bound work.
- Use tokio only for the `grove done --fetch` path where we need to fetch before checking merge status.

**Performance projection:** Python with 8 threads each paying ~80ms (2 subprocesses × ~40ms WSL fork overhead) = ~80ms for N≤8 projects. Rust with rayon and gix in-process operations should be **under 10ms** for N≤8 projects (index read + pack lookup, no fork overhead).

---

## 7. Path and Filesystem Concerns

### WSL paths: `/c/work/foo` style

The existing Python has an extensive `_normalize_wsl_path()` function handling:
- `file://` URLs
- UNC paths (`//hostname/c$/path`)
- Windows drive letters (`C:\path`, `C:/path`, `/C:/path`)
- WSL paths (`/c/path`)

**gix-path (`gix_path`) concerns:**
- `gix_path` operates on byte strings (`BStr`) and normalizes slash direction
- It does **not** know about WSL's `/c/` → `C:\` convention
- When gix opens a repository on a path like `/c/work/desktop_master`, it will work correctly on WSL because the Linux kernel treats `/c/work/` as a normal filesystem path (it's DrvFs)
- The gitdir path stored in `.git/worktrees/<id>/gitdir` may contain the Windows path (`C:\work\...`) or the WSL path depending on which tool created it. gix must handle both.
- `gix_path::to_unix_separators()` converts backslashes but does not do drive letter mapping.

**Recommendation:**
1. Define a `normalize_path(p: &Path) -> PathBuf` function that handles the same cases as Python's `_normalize_wsl_path()`, returning a canonical WSL-style path.
2. Before passing any path to gix (open, discover), normalize it through this function.
3. After receiving paths back from gix (e.g., `Proxy::base()`), normalize them before comparison or display.
4. Keep a `wsl_to_win(p: &Path) -> PathBuf` function for passing paths to `cmd.exe`, `wt.exe`, and `wezterm.exe`.

### Case sensitivity

WSL1 (this machine) runs on Windows NTFS via DrvFs, which is **case-insensitive**. gix operates on UTF-8 byte strings and does not normalize case. Git itself on Windows is case-insensitive for worktree paths.

**Risk:** `repo.worktrees()` sorts by `git_dir`, and path comparisons in grove's registry should use `eq_ignore_ascii_case()` when comparing paths on Windows.

### Long path support

Windows has a 260-character `MAX_PATH` limit by default, but this can be enabled via group policy or `git config core.longpaths true`. gix respects `core.longpaths` via its config system. No special action needed if the user has already enabled long paths for git.

### `.git` file vs directory

Linked worktrees use a `.git` **file** (containing `gitdir: <path>`) rather than a directory. `gix::discover()` handles both cases correctly — it reads the file and follows the reference.

---

## 8. Authentication

### Fetch from `if/master` and `my/` remotes

grove uses two remotes: `if` (upstream, read-only for most contributors) and `my` (fork, read-write). Authentication matters primarily for `push` and for HTTPS fetch.

### How gix handles credentials

`gix-credentials` provides `gix_credentials::builtin(action)` which "reads all context from the git configuration and does everything `git` typically does" — it delegates to whatever credential helper is configured in `.gitconfig`. This is the correct behavior.

For `gix::remote::Connection`, the constructor accepts an `authenticate` callback of type `remote::AuthenticateFn`. When you call `remote.connect(Direction::Fetch)`, you can pass:

```rust
use gix_credentials::builtin;
let connection = remote.connect_with_transport(
    transport,
    |action| builtin(action),
)?;
```

This will invoke the user's configured credential helper (e.g., `git-credential-manager`, `osxkeychain`, etc.) transparently.

**However:** gix's networking features are behind `blocking-network-client` or `async-network-client` features, and the credential integration is less battle-tested than git's. SSH agent forwarding, Kerberos, and Windows Credential Manager edge cases have been reported as imperfect.

**Decision:** Use **shell fallback (`git fetch`)** for all fetch operations. Rationale:
1. grove's fetch is only called in `grove new` and `grove done` — not in the hot path (`grove list` does no network I/O).
2. The `if` remote may use SSH with agent forwarding, which gix handles through `gix_prompt` but has known gaps on WSL.
3. git's credential handling has been battle-tested for years; introducing a new code path for a 2-3× performance gain in an operation that runs ~2-3 times per session is not worth the risk.

**Fetch fallback:**
```rust
fn git_fetch(repo_path: &Path, remote: &str) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["-C", repo_path.to_str().unwrap(), "fetch", remote])
        .status()?;
    if !status.success() {
        return Err(Error::FetchFailed(remote.to_string()));
    }
    Ok(())
}
```

---

## 9. Test Strategy

### Core principle: hermetic git repos in tempdir

Every test that exercises a git operation creates its own repository. No tests touch the real `~/.config/grove/` or any existing worktree.

### Test infrastructure

```rust
// tests/helpers/mod.rs
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct TestRepo {
    pub dir: TempDir,
    pub repo: gix::Repository,
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        // Make an initial commit so HEAD exists
        let sig = gix_actor::Signature::now("Test", "test@example.com").unwrap();
        // ... create tree + commit
        Self { dir, repo }
    }

    pub fn bare_clone(&self) -> Self {
        let dir = TempDir::new().unwrap();
        // git clone --bare <self.dir> <dir>
        std::process::Command::new("git")
            .args(["clone", "--bare", self.dir.path().to_str().unwrap(),
                   dir.path().to_str().unwrap()])
            .status().unwrap();
        let repo = gix::open(dir.path()).unwrap();
        Self { dir, repo }
    }

    pub fn add_remote(&self, name: &str, url: &Path) {
        std::process::Command::new("git")
            .args(["-C", self.dir.path().to_str().unwrap(),
                   "remote", "add", name, url.to_str().unwrap()])
            .status().unwrap();
    }
}
```

### Recommended crates

- **`tempfile`** (v3.x) — `TempDir` for isolated test directories. This is the standard; no alternatives needed.
- **`gix`** itself for repo operations in tests — use `gix::init()` for creating test repos.
- **`assert_cmd`** (v2.x) — for integration tests of the `grove` CLI binary itself. Captures stdout/stderr.
- **`predicates`** (v3.x) — works with `assert_cmd` for ergonomic output matching.
- Do **not** use `git2` (libgit2 bindings) in tests — this would add a second git backend and create confusion about which implementation is under test.

### Test categories

**Unit tests (in-process, pure gix):**
```rust
#[test]
fn test_is_dirty_clean_repo() {
    let r = TestRepo::new();
    assert!(!r.repo.is_dirty().unwrap());
}

#[test]
fn test_is_dirty_with_modifications() {
    let r = TestRepo::new();
    std::fs::write(r.dir.path().join("file.txt"), b"new content").unwrap();
    assert!(r.repo.is_dirty().unwrap());
}
```

**Worktree tests (using shell fallback for add/remove):**
```rust
#[test]
fn test_worktree_list() {
    let r = TestRepo::new();
    // Create a worktree via git (our fallback)
    let wt_dir = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["-C", r.dir.path().to_str().unwrap(),
               "worktree", "add", "-b", "feature",
               wt_dir.path().to_str().unwrap(), "HEAD"])
        .status().unwrap();
    // Now test our gix-based list
    let worktrees = r.repo.worktrees().unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].id().to_str().unwrap(), "feature");
}
```

**Ahead/behind tests:**
```rust
#[test]
fn test_ahead_behind() {
    let origin = TestRepo::new();
    let local = origin.clone_local();
    // Make 2 commits on local
    local.add_commit("commit1");
    local.add_commit("commit2");
    // Make 1 commit on origin
    origin.add_commit("upstream_commit");
    // Fetch
    local.git(&["fetch", "origin"]);

    let (ahead, behind) = compute_ahead_behind(
        &local.repo, "main",
        &local.repo.find_reference("refs/remotes/origin/main")?.id().detach(),
    ).unwrap();
    assert_eq!(ahead, 2);
    assert_eq!(behind, 1);
}
```

**Integration tests (full CLI):**
```rust
#[test]
fn test_grove_new_and_list() {
    let fixture = TestFixture::with_remotes();
    Command::new(grove_bin())
        .args(["new", "myfeature", "--base", "origin/main"])
        .env("GROVE_CONFIG_DIR", fixture.config_dir())
        .assert()
        .success()
        .stdout(predicate::str::contains("Created"));

    Command::new(grove_bin())
        .args(["list"])
        .env("GROVE_CONFIG_DIR", fixture.config_dir())
        .assert()
        .success()
        .stdout(predicate::str::contains("myfeature"));
}
```

**CI environment:** Tests should work without network access (all remotes are local file paths). No external git servers required.

---

## 10. Risk Register

### Risk 1: gix worktree-add not yet released (HIGH)

**Description:** `git worktree add` has no native gix API as of 0.83.0. Issue #2596 was closed May 13, 2026 as "will implement myself" and PR #2601 was rejected. Byron may release this in 0.84-0.85 (crate releases roughly every 4-6 weeks), but there is no timeline guarantee.

**Impact:** `grove new`, `grove fork`, `grove rename` (move fallback), and `grove adopt --move` all require worktree add. If we ship before the API lands, we shell out permanently.

**Mitigation:**
1. Design a `WorktreeManager` trait with a `shell_fallback` feature flag defaulting to `git worktree add`.
2. Watch gix releases and the `add_worktree` tracking issue. Swap to native API once it ships.
3. The manual filesystem approach (§2) is a viable middle ground if gix is too slow to implement this — it avoids the subprocess but requires careful testing for each git version's worktree format.

**Residual risk:** Low if we accept the shell fallback. High if we try to implement it manually and get format details wrong.

---

### Risk 2: gix push is immature (MEDIUM)

**Description:** `gix::push` exists but is not well-documented and lacks a high-level `push_branch_with_tracking()` API. Push is the most complex git network operation (protocol negotiation, ref updates, credential management).

**Impact:** `grove done` (push before delete), `grove freeze --lifetime` (auto-push), and any future push-on-create behavior.

**Mitigation:** Shell fallback for all push. This is already the plan (§4). Re-evaluate when gix 0.90+ is released with documented push examples.

**Residual risk:** Very low if shell fallback is used consistently.

---

### Risk 3: gix credential integration on WSL (MEDIUM)

**Description:** Even if we use gix for fetch, the credential helper chain on WSL (git-credential-manager running as a Windows process, SSH agent forwarding through WSL interop) has edge cases that gix's `gix_credentials::builtin` may not handle identically to git.

**Impact:** `grove new` and `grove done` would fail silently or prompt unexpectedly.

**Mitigation:** Shell out all fetch and push operations (§8). This sidesteps the problem entirely. If gix fetch is desired in future, test specifically with WSL + git-credential-manager + the specific remote protocols in use.

**Residual risk:** Eliminated by shell fallback.

---

### Risk 4: RevWalk API surface churn (LOW-MEDIUM)

**Description:** gix 0.83.0's `repo.rev_walk(tips).with_boundary(bases).all()` pattern is documented and stable in concept, but the exact method names (`with_boundary` vs `push_tip` vs `sorting`) have changed across versions (the doc page itself warns "verify against 0.83.0 struct page before committing to it").

**Impact:** Code that compiles against 0.83.0 may fail on 0.84.0+ if these builder methods are renamed.

**Mitigation:**
1. Pin `gix = "=0.83.0"` initially, then test and update deliberately.
2. Wrap all revwalk calls in `grove::git::revwalk` module so there is one place to fix if APIs change.
3. Write tests for ahead/behind that will catch regressions immediately.

**Residual risk:** Low with pinning + wrapper module.

---

### Risk 5: Path normalization mismatches on WSL1 (LOW-MEDIUM)

**Description:** Paths returned by gix (from `Proxy::base()`, `Repository::path()`, etc.) may be in Windows format (`C:\work\dt-foo`) when the `.git/worktrees/<id>/gitdir` file was written by git-for-Windows. grove's registry stores WSL-style paths (`/c/work/dt-foo`). Comparison failures mean worktrees appear as "untracked" or "missing."

**Impact:** `grove list --all` would show duplicate/wrong entries. `grove done` might not find the worktree to remove.

**Mitigation:**
1. Normalize all paths through `normalize_path()` immediately on read from gix.
2. Test with a repo where the worktree admin dir contains Windows-style paths.
3. The `_normalize_wsl_path()` from Python is a complete reference implementation — port it directly.

**Residual risk:** Medium during initial development, low after the normalize function is battle-tested.

---

## Appendix: Shell Fallback Wrapper Pattern

All shell fallbacks should go through a single `GitCommand` struct to make them easy to audit, mock in tests, and replace when native APIs land:

```rust
/// Operations not yet available in gix, delegated to git subprocess.
pub struct GitCommand {
    repo_path: PathBuf,
}

impl GitCommand {
    pub fn new(repo_path: impl Into<PathBuf>) -> Self {
        Self { repo_path: repo_path.into() }
    }

    fn git(&self, args: &[&str]) -> Result<std::process::Output> {
        let out = std::process::Command::new("git")
            .args(["-C", self.repo_path.to_str().unwrap()])
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(Error::GitSubprocess {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(out)
    }

    /// `git worktree add -b <branch> <path> <base>`
    pub fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<()> { ... }

    /// `git worktree add <path> <branch>` (existing branch)
    pub fn worktree_add_existing(&self, path: &Path, branch: &str) -> Result<()> { ... }

    /// `git worktree remove [--force] <path>`
    pub fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> { ... }

    /// `git worktree move <old> <new>`
    pub fn worktree_move(&self, old: &Path, new: &Path) -> Result<()> { ... }

    /// `git worktree prune`
    pub fn worktree_prune(&self) -> Result<()> { ... }

    /// `git fetch <remote>`
    pub fn fetch(&self, remote: &str) -> Result<()> { ... }

    /// `git push -u <remote> <branch>`
    pub fn push_tracking(&self, remote: &str, branch: &str) -> Result<()> { ... }

    /// `git push <remote> --delete <branch>`
    pub fn push_delete(&self, remote: &str, branch: &str) -> Result<bool> { ... }

    /// `git add -A && git commit -m <msg>`
    pub fn add_all_and_commit(&self, worktree: &Path, message: &str) -> Result<()> { ... }
}
```

This pattern means:
- Tests can swap in a `MockGitCommand` or use a real tempdir repo
- When gix gains the feature, replace one method body, not N call sites
- The struct is the single boundary between gix (pure Rust) and git (subprocess)

---

## Summary Table: gix vs Shell Split

| Category | gix native | Shell fallback |
|----------|-----------|----------------|
| Worktree list | `repo.worktrees()` | — |
| Worktree add | — | `git worktree add` |
| Worktree remove | — | `git worktree remove` |
| Worktree move | — | `git worktree move` |
| Worktree prune | — | `git worktree prune` |
| is_dirty | `repo.is_dirty()` | — |
| Status/untracked | `repo.status()` | — |
| Ahead/behind | `repo.rev_walk()` | — |
| Ref create/delete | `repo.edit_reference()` | — |
| Ref lookup | `repo.find_reference()` | — |
| Fetch | — | `git fetch` |
| Push | — | `git push` |
| Auto-commit WIP | — | `git add -A && git commit` |
| Squash detection | partial (diff API) | `git diff` (safety check only) |
