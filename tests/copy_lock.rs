//! Two `grove new` runs in two different repos take the machine-wide copy
//! lock one after the other. Each repo has its own base lock, so only the
//! machine-wide lock can make them wait for each other.

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::GitFixture;
use grove::config::global::{RepoEntry, ReposManifest};
use grove::machine_lock::{HOLD_ENV, LOCK_DIR_ENV, read_holder};

const HOLD_MS: u64 = 1500;

fn entry(fx: &GitFixture) -> RepoEntry {
    RepoEntry {
        main_repo: fx.main_path.clone(),
        work_dir: fx.temp_dir.path().to_path_buf(),
        dir_prefix: String::new(),
        upstream_remote: "origin".to_string(),
        fork_remote: "origin".to_string(),
        default_base: "main".to_string(),
        issue_prefix: None,
        launch: None,
    }
}

fn grove(home: &Path, lock_dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grove"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("GROVE_ORIG_CWD")
        .env(LOCK_DIR_ENV, lock_dir)
        .env(HOLD_ENV, HOLD_MS.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn reflinks_here(dir: &Path) -> bool {
    let a = dir.join(".probe-a");
    std::fs::write(&a, b"x").unwrap();
    let ok = Command::new("cp")
        .arg("--reflink=always")
        .arg(&a)
        .arg(dir.join(".probe-b"))
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(dir.join(".probe-b"));
    ok
}

#[test]
fn two_concurrent_creates_serialize_on_the_copy_lock() {
    let one = GitFixture::new().with_grove_registry();
    let two = GitFixture::new().with_grove_registry();
    if !cfg!(target_os = "linux") || !reflinks_here(one.temp_dir.path()) {
        eprintln!("skipping: no reflinks here, so no copy and no copy lock");
        return;
    }

    let home = tempfile::TempDir::new().unwrap();
    let config_dir = home.path().join(".config").join("grove");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mut repos = BTreeMap::new();
    repos.insert("one".to_string(), entry(&one));
    repos.insert("two".to_string(), entry(&two));
    let manifest = ReposManifest {
        schema_version: 1,
        default_repo: Some("one".to_string()),
        repos,
    };
    std::fs::write(
        config_dir.join("repos.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let lock_dir = home.path().join("locks");

    let started = Instant::now();
    let first = grove(
        home.path(),
        &lock_dir,
        &["--repo", "one", "new", "--no-fetch", "lane-a"],
    )
    .spawn()
    .unwrap();

    // Start the second run only once the first holds the lock.
    let holder_file = lock_dir.join("copy.lock.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    let holder = loop {
        if let Some(h) = read_holder(&holder_file) {
            break h;
        }
        assert!(
            Instant::now() < deadline,
            "first grove new never took the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(holder.pid, first.id());
    assert_eq!(holder.op, "copy");
    assert_eq!(holder.tag, "lane-a");
    assert!(holder.command.contains("new"), "{}", holder.command);

    let second = grove(
        home.path(),
        &lock_dir,
        &["--repo", "two", "new", "--no-fetch", "lane-b"],
    )
    .spawn()
    .unwrap();

    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    let err_a = String::from_utf8_lossy(&first.stderr);
    let err_b = String::from_utf8_lossy(&second.stderr);
    assert!(first.status.success(), "first grove new failed: {err_a}");
    assert!(second.status.success(), "second grove new failed: {err_b}");

    assert!(
        err_b.contains("Waiting for another grove command to finish copying or deleting a lane")
            && err_b.contains(&format!("pid {}", holder.pid))
            && err_b.contains("'lane-a'"),
        "the second run must say it waits for the first, got: {err_b}"
    );
    assert!(
        elapsed >= Duration::from_millis(2 * HOLD_MS),
        "the two copies overlapped: both done in {elapsed:?}"
    );
    assert!(
        !holder_file.exists(),
        "the holder file must go when the lock is released"
    );

    for (fx, tag) in [(&one, "lane-a"), (&two, "lane-b")] {
        let lane = fx.temp_dir.path().join(tag);
        let out = Command::new("git")
            .arg("-C")
            .arg(&lane)
            .args(["status", "--porcelain", "--ignored"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "{tag} is not clean");
    }
}
