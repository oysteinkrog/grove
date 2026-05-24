use std::path::PathBuf;

use thiserror::Error;

pub mod wezterm;
pub mod windows_terminal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalKind {
    Wezterm,
    WindowsTerminal,
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub cwd: PathBuf,
    pub command: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error(
        "no terminal available; set launch.terminal in config or ensure wt.exe/wezterm.exe is on PATH"
    )]
    NoTerminalAvailable,
    #[error("terminal launch failed: {0}")]
    SpawnFailed(String),
}

pub trait Terminal: Send + Sync {
    fn launch(&self, opts: &LaunchOptions) -> Result<(), LaunchError>;
    fn dry_run(&self, opts: &LaunchOptions) -> Result<String, LaunchError>;
    fn kind(&self) -> TerminalKind;
}

/// Autodetect the preferred terminal from env vars and PATH.
///
/// Priority order (mirrors docs/04-launch-test-ci.md §2.5):
///   1. Explicit override wins unconditionally.
///   2. WEZTERM_PANE env var → Wezterm.
///   3. WT_SESSION env var → WindowsTerminal.
///   4. wezterm.exe on PATH → Wezterm.
///   5. wt.exe on PATH → WindowsTerminal.
///   6. Error: NoTerminalAvailable.
pub fn autodetect(override_kind: Option<TerminalKind>) -> Result<TerminalKind, LaunchError> {
    if let Some(kind) = override_kind {
        return Ok(kind);
    }

    if std::env::var("WEZTERM_PANE").is_ok() {
        return Ok(TerminalKind::Wezterm);
    }

    if std::env::var("WT_SESSION").is_ok() {
        return Ok(TerminalKind::WindowsTerminal);
    }

    if which::which("wezterm.exe").is_ok() {
        return Ok(TerminalKind::Wezterm);
    }

    if which::which("wt.exe").is_ok() {
        return Ok(TerminalKind::WindowsTerminal);
    }

    Err(LaunchError::NoTerminalAvailable)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serial_test::serial;

    use super::*;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test-only; serialized by ENV_MUTEX
            unsafe { std::env::set_var(key, value) };
            EnvGuard { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test-only; serialized by ENV_MUTEX
            unsafe { std::env::remove_var(key) };
            EnvGuard { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(val) => unsafe { std::env::set_var(self.key, val) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    #[serial]
    fn wezterm_pane_env_wins() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _wt = EnvGuard::remove("WT_SESSION");
        let _wp = EnvGuard::set("WEZTERM_PANE", "1");
        assert_eq!(autodetect(None).unwrap(), TerminalKind::Wezterm);
    }

    #[test]
    #[serial]
    fn wt_session_env_wins() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _wp = EnvGuard::remove("WEZTERM_PANE");
        let _wt = EnvGuard::set("WT_SESSION", "some-session-id");
        assert_eq!(autodetect(None).unwrap(), TerminalKind::WindowsTerminal);
    }

    #[test]
    #[serial]
    fn override_wins_regardless_of_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _wp = EnvGuard::set("WEZTERM_PANE", "1");
        let _wt = EnvGuard::set("WT_SESSION", "some-session-id");
        assert_eq!(
            autodetect(Some(TerminalKind::Wezterm)).unwrap(),
            TerminalKind::Wezterm
        );
        assert_eq!(
            autodetect(Some(TerminalKind::WindowsTerminal)).unwrap(),
            TerminalKind::WindowsTerminal
        );
    }

    #[test]
    #[serial]
    fn no_terminal_available_returns_error() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _wp = EnvGuard::remove("WEZTERM_PANE");
        let _wt = EnvGuard::remove("WT_SESSION");
        // On Linux CI neither wt.exe nor wezterm.exe will be on PATH, so this
        // test exercises the NoTerminalAvailable path on all platforms.
        // On Windows dev machines where wt.exe exists this test is skipped.
        if which::which("wt.exe").is_err() && which::which("wezterm.exe").is_err() {
            assert!(matches!(
                autodetect(None),
                Err(LaunchError::NoTerminalAvailable)
            ));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[serial]
    fn wt_exe_on_path_returns_windows_terminal() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _wp = EnvGuard::remove("WEZTERM_PANE");
        let _wt = EnvGuard::remove("WT_SESSION");
        // Only meaningful on Windows where wt.exe is available.
        if which::which("wt.exe").is_ok() {
            assert_eq!(autodetect(None).unwrap(), TerminalKind::WindowsTerminal);
        }
    }
}
