use std::sync::Mutex;

use super::{LaunchError, LaunchOptions, Terminal, TerminalKind};

/// Test-only terminal that records received `LaunchOptions` without spawning processes.
pub struct MockTerminal {
    pub tabs: Mutex<Vec<LaunchOptions>>,
    pub kind: TerminalKind,
}

impl MockTerminal {
    pub fn new(kind: TerminalKind) -> Self {
        Self {
            tabs: Mutex::new(Vec::new()),
            kind,
        }
    }

    pub fn wezterm() -> Self {
        Self::new(TerminalKind::Wezterm)
    }

    pub fn windows_terminal() -> Self {
        Self::new(TerminalKind::WindowsTerminal)
    }

    pub fn recorded_tabs(&self) -> Vec<LaunchOptions> {
        self.tabs.lock().unwrap().clone()
    }
}

impl Terminal for MockTerminal {
    fn launch(&self, opts: &LaunchOptions) -> Result<(), LaunchError> {
        self.tabs.lock().unwrap().push(opts.clone());
        Ok(())
    }

    fn dry_run(&self, opts: &LaunchOptions) -> Result<String, LaunchError> {
        Ok(format!(
            "[mock] cwd={} title={} cmd={}",
            opts.cwd.display(),
            opts.title.as_deref().unwrap_or(""),
            opts.command.as_deref().unwrap_or(""),
        ))
    }

    fn kind(&self) -> TerminalKind {
        self.kind.clone()
    }
}
