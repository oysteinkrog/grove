//! Subprocess helpers for `grove gc`.
//!
//! Every external probe `gc` makes (git, `gh`, `am`) runs through [`run`], which
//! enforces a wall-clock timeout and closes stdin. `gc` walks trees it does not
//! control on a filesystem (WSL1 drvfs) where a single `git status` can take
//! minutes, and probes like `gh` can block on network or auth prompts. A hung
//! probe must degrade into a reported unknown, never into a wedged run.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Stdout split into non-empty trimmed lines.
    pub fn lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum ExecError {
    /// The program could not be started (missing binary, bad cwd).
    Spawn(String),
    /// The program was still running when the timeout elapsed and was killed.
    Timeout(Duration),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(msg) => write!(f, "could not run: {msg}"),
            Self::Timeout(d) => write!(f, "timed out after {}s", d.as_secs()),
        }
    }
}

/// Run `program` with `args`, capturing stdout/stderr, killing it after `timeout`.
///
/// stdout and stderr are drained on separate threads so a chatty child can never
/// deadlock against a full pipe while the main thread polls for exit. stdin is
/// `/dev/null` so a probe that would otherwise prompt fails fast instead of
/// blocking a non-interactive run.
pub fn run(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Output, ExecError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| ExecError::Spawn(e.to_string()))?;

    let mut child_out = child.stdout.take().expect("stdout piped");
    let mut child_err = child.stderr.take().expect("stderr piped");
    let out_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_out.read_to_end(&mut buf);
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_err.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    // Poll with a ramp: short waits keep fast git calls fast, long waits keep a
    // multi-minute `git status` from spinning the CPU.
    let mut nap = Duration::from_millis(1);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => break Err(e).ok(),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(25));
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    match exit {
        Some(status) => Ok(Output {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        None => Err(ExecError::Timeout(timeout)),
    }
}

/// Run `git -C <cwd> <args>`, returning stdout only when git exited 0.
///
/// Failure and timeout collapse into a single `Err(String)` because every gc
/// caller treats "git could not answer" the same way: report it and move on.
pub fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    match run("git", args_with_c(cwd, args).as_slice(), None, timeout) {
        Ok(out) if out.success => Ok(out.stdout),
        Ok(out) => Err(format!(
            "git {} failed: {}",
            args.join(" "),
            out.stderr.trim().lines().next().unwrap_or("(no stderr)")
        )),
        Err(e) => Err(format!("git {}: {e}", args.join(" "))),
    }
}

fn args_with_c<'a>(cwd: &'a Path, args: &[&'a str]) -> Vec<&'a str> {
    let mut all = Vec::with_capacity(args.len() + 2);
    all.push("-C");
    all.push(cwd.to_str().unwrap_or("."));
    all.extend_from_slice(args);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_of_a_fast_command() {
        let out = run("echo", &["hello"], None, Duration::from_secs(10)).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[test]
    fn reports_non_zero_exit_without_erroring() {
        let out = run("false", &[], None, Duration::from_secs(10)).unwrap();
        assert!(!out.success, "`false` should report failure, not Err");
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let err = run("grove-no-such-binary-xyz", &[], None, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ExecError::Spawn(_)), "got {err:?}");
    }

    #[test]
    fn slow_command_is_killed_at_the_timeout() {
        let started = Instant::now();
        let err = run("sleep", &["30"], None, Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, ExecError::Timeout(_)), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout should fire long before the child would exit"
        );
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // A child writing more than a pipe buffer must still be reaped: the
        // reader threads have to drain while the main thread polls.
        let out = run(
            "sh",
            &["-c", "for i in $(seq 1 20000); do echo line-$i; done"],
            None,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(out.success);
        assert_eq!(out.lines().len(), 20_000);
    }

    #[test]
    fn stdin_is_closed_so_readers_do_not_block() {
        let out = run("cat", &[], None, Duration::from_secs(10)).unwrap();
        assert!(out.success, "cat with /dev/null stdin should exit cleanly");
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn git_helper_reports_failure_as_err() {
        let missing = Path::new("/tmp/grove-gc-no-such-dir-xyz");
        let err = git(missing, &["status"], Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("git status"), "{err}");
    }
}
