use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::to_windows_path;

use super::{LaunchError, LaunchOptions, Terminal, TerminalKind};

/// Wezterm launcher that issues one `wezterm cli spawn` per tab.
///
/// Unlike wt.exe, the wezterm CLI has no multi-tab single-invocation syntax,
/// so each tab requires a separate subprocess call.
pub struct Wezterm {
    wezterm_exe: PathBuf,
}

impl Wezterm {
    pub fn new() -> Result<Self, LaunchError> {
        let exe = which::which("wezterm")
            .or_else(|_| which::which("wezterm.exe"))
            .map_err(|_| LaunchError::SpawnFailed("wezterm not found on PATH".to_owned()))?;
        Ok(Self { wezterm_exe: exe })
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            wezterm_exe: path.into(),
        }
    }

    /// Build argv for a single `wezterm cli spawn` invocation.
    ///
    /// Each tab gets its own argv slice; the caller is responsible for spawning
    /// one process per entry.
    pub fn build_argv(&self, tabs: &[&LaunchOptions]) -> Vec<Vec<String>> {
        tabs.iter()
            .map(|tab| {
                let win_cwd = to_windows_path(&tab.cwd);
                let mut argv = vec![
                    self.wezterm_exe.to_string_lossy().into_owned(),
                    "cli".to_owned(),
                    "spawn".to_owned(),
                    "--cwd".to_owned(),
                    win_cwd,
                ];
                if let Some(cmd) = &tab.command {
                    argv.push("--".to_owned());
                    argv.push(cmd.clone());
                }
                argv
            })
            .collect()
    }

    /// Spawn one wezterm process per tab.
    pub fn launch_tabs(&self, tabs: &[LaunchOptions]) -> Result<(), LaunchError> {
        let refs: Vec<&LaunchOptions> = tabs.iter().collect();
        for argv in self.build_argv(&refs) {
            let (exe, args) = argv.split_first().expect("argv always has wezterm exe");
            Command::new(exe)
                .args(args)
                .spawn()
                .map_err(|e| LaunchError::SpawnFailed(e.to_string()))?;
        }
        Ok(())
    }

    /// Return the per-tab argvs without spawning any process.
    pub fn dry_run_tabs(&self, tabs: &[LaunchOptions]) -> Vec<Vec<String>> {
        let refs: Vec<&LaunchOptions> = tabs.iter().collect();
        self.build_argv(&refs)
    }
}

impl Default for Wezterm {
    fn default() -> Self {
        Self::with_path("wezterm")
    }
}

impl Terminal for Wezterm {
    fn launch(&self, opts: &LaunchOptions) -> Result<(), LaunchError> {
        self.launch_tabs(std::slice::from_ref(opts))
    }

    fn dry_run(&self, opts: &LaunchOptions) -> Result<String, LaunchError> {
        let argvs = self.dry_run_tabs(std::slice::from_ref(opts));
        Ok(argvs
            .into_iter()
            .map(|a| a.join(" "))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn kind(&self) -> TerminalKind {
        TerminalKind::Wezterm
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_tab(cwd: &str, command: Option<&str>) -> LaunchOptions {
    LaunchOptions {
        cwd: Path::new(cwd).to_path_buf(),
        title: None,
        command: command.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wz() -> Wezterm {
        Wezterm::with_path("wezterm.exe")
    }

    #[test]
    fn three_tabs_produce_three_separate_spawn_invocations() {
        let tabs = vec![
            make_tab("/c/work/foo", None),
            make_tab("/c/work/bar", None),
            make_tab("/c/work/baz", None),
        ];
        let argvs = wz().dry_run_tabs(&tabs);
        assert_eq!(argvs.len(), 3, "expected one argv per tab");
        for argv in &argvs {
            assert_eq!(argv[0], "wezterm.exe");
            assert_eq!(argv[1], "cli");
            assert_eq!(argv[2], "spawn");
        }
    }

    #[test]
    fn cwd_converted_to_windows_path() {
        let tabs = vec![make_tab("/c/work/grove", None)];
        let argvs = wz().dry_run_tabs(&tabs);
        let argv = &argvs[0];
        let cwd_idx = argv
            .iter()
            .position(|a| a == "--cwd")
            .expect("--cwd flag missing");
        assert_eq!(argv[cwd_idx + 1], r"C:\work\grove");
    }

    #[test]
    fn configured_wezterm_path_used() {
        let wz = Wezterm::with_path("/custom/path/wezterm");
        let tabs = vec![make_tab("/c/work/foo", None)];
        let argvs = wz.dry_run_tabs(&tabs);
        assert_eq!(argvs[0][0], "/custom/path/wezterm");
    }

    #[test]
    fn dry_run_returns_one_argv_per_tab_no_process_spawned() {
        // Verifies dry_run_tabs returns Vec<Vec<String>> without needing
        // wezterm.exe to exist on CI.
        let tabs = vec![
            make_tab("/c/work/a", Some("fish -l")),
            make_tab("/c/work/b", Some("fish -l")),
        ];
        let argvs = wz().dry_run_tabs(&tabs);
        assert_eq!(argvs.len(), 2);
        for argv in &argvs {
            assert!(argv.contains(&"--cwd".to_owned()));
        }
    }
}
