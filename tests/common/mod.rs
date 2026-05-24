use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use time::OffsetDateTime;

use grove::config::ResolvedConfig;
use grove::config::global::{RepoEntry, ReposManifest};
use grove::registry::Registry;
use grove::registry::project::Project;
use grove::repo::RepoContext;

pub struct GitFixture {
    pub temp_dir: TempDir,
    pub bare_path: PathBuf,
    pub main_path: PathBuf,
    pub grove_dir: PathBuf,
}

impl GitFixture {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().unwrap();
        let bare_path = temp_dir.path().join("bare.git");
        let main_path = temp_dir.path().join("main");

        // Init bare repo on main branch
        let out = Command::new("git")
            .args(["init", "--bare", "-b", "main", bare_path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Init working clone on main branch
        let out = Command::new("git")
            .args(["init", "-b", "main", main_path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        git(&main_path, &["config", "user.email", "test@test.com"]);
        git(&main_path, &["config", "user.name", "Test"]);

        // Initial commit
        std::fs::write(main_path.join("README.md"), b"grove test fixture").unwrap();
        git(&main_path, &["add", "."]);
        git(&main_path, &["commit", "-m", "init"]);

        // Add bare as remote and push
        git(
            &main_path,
            &["remote", "add", "origin", bare_path.to_str().unwrap()],
        );
        git(&main_path, &["push", "-u", "origin", "main"]);

        let grove_dir = temp_dir.path().join(".grove");
        std::fs::create_dir_all(&grove_dir).unwrap();

        Self {
            temp_dir,
            bare_path,
            main_path,
            grove_dir,
        }
    }

    pub fn add_worktree(&self, tag: &str, branch: &str) -> PathBuf {
        let wt_path = self.temp_dir.path().join(tag);
        let out = Command::new("git")
            .args([
                "-C",
                self.main_path.to_str().unwrap(),
                "worktree",
                "add",
                "-b",
                branch,
                wt_path.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        wt_path
    }

    pub fn with_grove_registry(self) -> Self {
        let registry = Registry::default();
        registry.save(&self.grove_dir).unwrap();
        self
    }

    pub fn make_context(&self) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "test".to_string(),
            RepoEntry {
                main_repo: self.main_path.clone(),
                work_dir: self.temp_dir.path().to_path_buf(),
                dir_prefix: String::new(),
                upstream_remote: "origin".to_string(),
                fork_remote: "origin".to_string(),
                default_base: "main".to_string(),
                issue_prefix: None,
                launch: None,
            },
        );
        let global = ReposManifest {
            schema_version: 1,
            default_repo: Some("test".to_string()),
            repos,
        };
        let resolved = ResolvedConfig {
            main_repo: self.main_path.clone(),
            work_dir: self.temp_dir.path().to_path_buf(),
            dir_prefix: String::new(),
            upstream_remote: "origin".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            launch: None,
        };
        let registry = Registry::load(&self.grove_dir).unwrap();
        RepoContext {
            id: "test".to_string(),
            global,
            resolved,
            registry,
        }
    }

    #[allow(dead_code)]
    pub fn make_project(&self, tag: &str, branch: &str) -> Project {
        let path = self.temp_dir.path().join(tag);
        Project {
            path,
            branch: branch.to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::now_utc(),
            issue: None,
            frozen: false,
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Unit test: GitFixture::new() creates the expected layout
#[test]
fn fixture_new_creates_bare_and_main() {
    let fx = GitFixture::new();
    assert!(fx.bare_path.exists(), "bare repo directory should exist");
    assert!(fx.main_path.exists(), "main clone directory should exist");
    assert!(fx.grove_dir.exists(), ".grove directory should exist");
    // Bare repo has HEAD file
    assert!(
        fx.bare_path.join("HEAD").exists(),
        "bare repo should have HEAD"
    );
    // Main repo has .git
    assert!(fx.main_path.join(".git").exists(), "main should have .git");
}

// Unit test: TempDir cleanup — fixture temp_dir is removed on drop
#[test]
fn fixture_drops_cleanly() {
    let path = {
        let fx = GitFixture::new();
        fx.temp_dir.path().to_path_buf()
    };
    assert!(!path.exists(), "temp_dir should be cleaned up after drop");
}
