use std::path::Path;
use std::process::Command;

use crate::paths::to_windows_path;

use super::{LaunchError, LaunchOptions, Terminal, TerminalKind};

/// Windows Terminal launcher that spawns all tabs in a single wt.exe invocation.
pub struct WindowsTerminal {
    wt_exe: String,
}

impl WindowsTerminal {
    pub fn new() -> Self {
        Self {
            wt_exe: "wt.exe".to_owned(),
        }
    }

    /// Constructor for tests — allows injecting a fake wt.exe path.
    pub fn new_for_test(wt_exe: impl Into<String>) -> Self {
        Self {
            wt_exe: wt_exe.into(),
        }
    }

    /// Build the argv for launching one or more tabs in a single wt.exe invocation.
    ///
    /// Produces: wt.exe new-tab --startingDirectory <win_path> [--title <title>] [cmd]
    ///           [; new-tab --startingDirectory <win_path2> ...]
    pub fn build_argv(&self, tabs: &[&LaunchOptions]) -> Vec<String> {
        let mut argv = vec![self.wt_exe.clone()];

        for (i, tab) in tabs.iter().enumerate() {
            if i > 0 {
                argv.push(";".to_owned());
            }
            argv.push("new-tab".to_owned());
            argv.push("--startingDirectory".to_owned());
            argv.push(to_windows_path(&tab.cwd));
            if let Some(title) = &tab.title {
                argv.push("--title".to_owned());
                argv.push(title.clone());
            }
            if let Some(cmd) = &tab.command {
                argv.push(cmd.clone());
            }
        }

        argv
    }

    /// Launch multiple tabs in a single wt.exe process.
    pub fn launch_tabs(&self, tabs: &[LaunchOptions]) -> Result<(), LaunchError> {
        let refs: Vec<&LaunchOptions> = tabs.iter().collect();
        let argv = self.build_argv(&refs);
        let (exe, args) = argv.split_first().expect("argv always has wt.exe");
        Command::new(exe)
            .args(args)
            .spawn()
            .map_err(|e| LaunchError::SpawnFailed(e.to_string()))?;
        Ok(())
    }

    /// Return the argv that would be passed to wt.exe for multiple tabs, without spawning.
    pub fn dry_run_tabs(&self, tabs: &[LaunchOptions]) -> Vec<String> {
        let refs: Vec<&LaunchOptions> = tabs.iter().collect();
        self.build_argv(&refs)
    }
}

impl Default for WindowsTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal for WindowsTerminal {
    fn launch(&self, opts: &LaunchOptions) -> Result<(), LaunchError> {
        self.launch_tabs(std::slice::from_ref(opts))
    }

    fn dry_run(&self, opts: &LaunchOptions) -> Result<String, LaunchError> {
        let argv = self.dry_run_tabs(std::slice::from_ref(opts));
        Ok(argv.join(" "))
    }

    fn kind(&self) -> TerminalKind {
        TerminalKind::WindowsTerminal
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_tab(cwd: &str, title: Option<&str>, command: Option<&str>) -> LaunchOptions {
    LaunchOptions {
        cwd: Path::new(cwd).to_path_buf(),
        title: title.map(str::to_owned),
        command: command.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> WindowsTerminal {
        WindowsTerminal::new_for_test("wt.exe")
    }

    #[test]
    fn single_wt_invocation_for_three_tabs() {
        let tabs = vec![
            make_tab("/c/work/foo", Some("foo"), None),
            make_tab("/c/work/bar", Some("bar"), None),
            make_tab("/c/work/baz", Some("baz"), None),
        ];
        let argv = wt().dry_run_tabs(&tabs);

        // Must be a single invocation — first element is wt.exe
        assert_eq!(argv[0], "wt.exe");

        // Must contain two `;` separators for three tabs
        let sep_count = argv.iter().filter(|a| a.as_str() == ";").count();
        assert_eq!(sep_count, 2, "expected 2 `;` separators for 3 tabs, got {sep_count}");
    }

    #[test]
    fn cwd_converted_to_windows_path() {
        let tabs = vec![make_tab("/c/work/foo", None, None)];
        let argv = wt().dry_run_tabs(&tabs);

        let dir_idx = argv
            .iter()
            .position(|a| a == "--startingDirectory")
            .expect("--startingDirectory missing");
        let dir_val = &argv[dir_idx + 1];
        assert_eq!(dir_val, r"C:\work\foo");
    }

    #[test]
    fn title_forwarded_to_wt() {
        let tabs = vec![make_tab("/c/work/proj", Some("my-project"), None)];
        let argv = wt().dry_run_tabs(&tabs);

        let title_idx = argv
            .iter()
            .position(|a| a == "--title")
            .expect("--title missing");
        assert_eq!(argv[title_idx + 1], "my-project");
    }

    #[test]
    fn dry_run_does_not_spawn_process() {
        // Verify dry_run_tabs returns Vec<String> without requiring wt.exe to exist.
        // If it tried to spawn, it would fail on non-Windows CI where wt.exe is absent.
        let tabs = vec![
            make_tab("/c/work/a", Some("a"), Some("fish -l")),
            make_tab("/c/work/b", Some("b"), Some("fish -l")),
        ];
        let argv = wt().dry_run_tabs(&tabs);

        // Smoke-check: non-empty, starts with wt.exe
        assert!(!argv.is_empty());
        assert_eq!(argv[0], "wt.exe");

        // Both startingDirectory args present
        let dir_count = argv.iter().filter(|a| a.as_str() == "--startingDirectory").count();
        assert_eq!(dir_count, 2);
    }
}
