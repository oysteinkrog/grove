use std::collections::HashSet;

use anyhow::Result;

use crate::launch::{
    LaunchOptions, Terminal, TerminalKind, autodetect, wezterm::Wezterm,
    windows_terminal::WindowsTerminal,
};
use crate::repo::RepoContext;

const DEFAULT_SHELL_COMMAND: &str =
    "fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'";
const NO_CLAUDE_SHELL_COMMAND: &str = "fish -l";

pub struct LaunchArgs {
    /// Restrict to these tags only (comma-separated values, from --only flag)
    pub only: Option<String>,
    /// Print commands without spawning
    pub dry_run: bool,
    /// Skip claude invocation; use bare fish login shell
    pub no_claude: bool,
    /// Override terminal kind: "wt" or "wezterm"
    pub terminal: Option<String>,
}

pub fn run(args: &LaunchArgs, cx: &RepoContext) -> Result<()> {
    run_with_terminal_factory(args, cx, None)
}

/// Internal entry point that accepts an optional pre-built terminal for testing.
pub fn run_with_terminal_factory(
    args: &LaunchArgs,
    cx: &RepoContext,
    terminal_override: Option<&dyn Terminal>,
) -> Result<()> {
    let only_tags: Option<HashSet<String>> = args.only.as_deref().map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    });

    let shell_cmd = if args.no_claude {
        NO_CLAUDE_SHELL_COMMAND
    } else {
        cx.resolved
            .launch
            .as_ref()
            .and_then(|l| l.shell_command.as_deref())
            .unwrap_or(DEFAULT_SHELL_COMMAND)
    };

    let mut tabs: Vec<LaunchOptions> = Vec::new();
    let mut skipped_frozen: Vec<String> = Vec::new();

    // BTreeMap iteration is already sorted alphabetically by tag.
    for (tag, project) in &cx.registry.projects {
        if let Some(ref only) = only_tags
            && !only.contains(tag.as_str())
        {
            continue;
        }

        if project.frozen {
            skipped_frozen.push(tag.clone());
            continue;
        }

        tabs.push(LaunchOptions {
            cwd: project.path.clone(),
            title: Some(tag.clone()),
            command: Some(shell_cmd.to_string()),
        });
    }

    if !skipped_frozen.is_empty() {
        eprintln!(
            "grove: skipping {} frozen project(s): {}",
            skipped_frozen.len(),
            skipped_frozen.join(", ")
        );
    }

    if tabs.is_empty() {
        anyhow::bail!("no projects matched the filter");
    }

    if args.dry_run {
        // Print dry-run output without spawning.
        if let Some(term) = terminal_override {
            for tab in &tabs {
                let line = term.dry_run(tab)?;
                println!("{line}");
            }
        } else {
            // Use autodetected kind for dry-run output format.
            let kind = resolve_terminal_kind(args, cx);
            match kind {
                Ok(TerminalKind::WindowsTerminal) => {
                    let wt = WindowsTerminal::new();
                    let argv = wt.dry_run_tabs(&tabs);
                    println!("{}", argv.join(" "));
                }
                Ok(TerminalKind::Wezterm) | Err(_) => {
                    let wz = Wezterm::default();
                    for tab in &tabs {
                        let line = wz.dry_run(tab)?;
                        println!("{line}");
                    }
                }
            }
        }
        return Ok(());
    }

    // Real launch path.
    if let Some(term) = terminal_override {
        for tab in &tabs {
            term.launch(tab)
                .map_err(|e| anyhow::anyhow!("terminal launch failed: {e}"))?;
        }
        return Ok(());
    }

    let kind = resolve_terminal_kind(args, cx)
        .map_err(|e| anyhow::anyhow!("terminal detection failed: {e}"))?;

    match kind {
        TerminalKind::WindowsTerminal => {
            let wt = WindowsTerminal::new();
            wt.launch_tabs(&tabs)
                .map_err(|e| anyhow::anyhow!("terminal launch failed: {e}"))?;
        }
        TerminalKind::Wezterm => {
            let wz = Wezterm::new().map_err(|e| anyhow::anyhow!("wezterm not found: {e}"))?;
            wz.launch_tabs(&tabs)
                .map_err(|e| anyhow::anyhow!("terminal launch failed: {e}"))?;
        }
    }

    Ok(())
}

fn resolve_terminal_kind(
    args: &LaunchArgs,
    cx: &RepoContext,
) -> Result<TerminalKind, crate::launch::LaunchError> {
    let override_kind = args
        .terminal
        .as_deref()
        .or_else(|| {
            cx.resolved
                .launch
                .as_ref()
                .and_then(|l| l.terminal.as_deref())
        })
        .and_then(|s| match s {
            "wt" | "windows_terminal" => Some(TerminalKind::WindowsTerminal),
            "wezterm" => Some(TerminalKind::Wezterm),
            _ => None,
        });

    autodetect(override_kind)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::config::ResolvedConfig;
    use crate::config::global::{RepoEntry, ReposManifest};
    use crate::launch::mock_terminal::MockTerminal;
    use crate::registry::{Project, Registry};
    use crate::repo::RepoContext;

    fn make_project(path: &str, frozen: bool) -> Project {
        Project {
            path: PathBuf::from(path),
            branch: "main".to_string(),
            base: "origin/main".to_string(),
            created: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            issue: None,
            frozen,
        }
    }

    fn make_context(projects: BTreeMap<String, Project>) -> RepoContext {
        let mut repos = BTreeMap::new();
        repos.insert(
            "test".to_string(),
            RepoEntry {
                main_repo: PathBuf::from("/c/work/test/master"),
                work_dir: PathBuf::from("/c/work/test"),
                dir_prefix: String::new(),
                upstream_remote: "upstream".to_string(),
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
            main_repo: PathBuf::from("/c/work/test/master"),
            work_dir: PathBuf::from("/c/work/test"),
            upstream_remote: "upstream".to_string(),
            fork_remote: "origin".to_string(),
            default_base: "main".to_string(),
            issue_prefix: None,
            dir_prefix: String::new(),
            launch: None,
        };
        let registry = Registry {
            schema_version: 1,
            projects,
        };
        RepoContext {
            id: "test".to_string(),
            global,
            resolved,
            registry,
        }
    }

    // AC1: all non-frozen projects become tabs, sorted alphabetically by tag
    #[test]
    fn all_unfrozen_projects_become_tabs_alphabetically() {
        let mut projects = BTreeMap::new();
        projects.insert(
            "charlie".to_string(),
            make_project("/c/work/charlie", false),
        );
        projects.insert("alpha".to_string(), make_project("/c/work/alpha", false));
        projects.insert("bravo".to_string(), make_project("/c/work/bravo", false));

        let cx = make_context(projects);
        let mock = MockTerminal::wezterm();
        let args = LaunchArgs {
            only: None,
            dry_run: false,
            no_claude: false,
            terminal: None,
        };

        run_with_terminal_factory(&args, &cx, Some(&mock)).unwrap();

        let tabs = mock.recorded_tabs();
        assert_eq!(tabs.len(), 3);
        // BTreeMap is alphabetically sorted, verify tab order
        assert_eq!(tabs[0].title.as_deref(), Some("alpha"));
        assert_eq!(tabs[1].title.as_deref(), Some("bravo"));
        assert_eq!(tabs[2].title.as_deref(), Some("charlie"));
    }

    // AC2: frozen projects are excluded from default launch
    #[test]
    fn frozen_projects_are_excluded() {
        let mut projects = BTreeMap::new();
        projects.insert("active".to_string(), make_project("/c/work/active", false));
        projects.insert(
            "frozen-one".to_string(),
            make_project("/c/work/frozen-one", true),
        );
        projects.insert(
            "also-active".to_string(),
            make_project("/c/work/also-active", false),
        );

        let cx = make_context(projects);
        let mock = MockTerminal::wezterm();
        let args = LaunchArgs {
            only: None,
            dry_run: false,
            no_claude: false,
            terminal: None,
        };

        run_with_terminal_factory(&args, &cx, Some(&mock)).unwrap();

        let tabs = mock.recorded_tabs();
        assert_eq!(tabs.len(), 2);
        let titles: Vec<_> = tabs.iter().map(|t| t.title.as_deref().unwrap()).collect();
        assert!(titles.contains(&"active"));
        assert!(titles.contains(&"also-active"));
        assert!(!titles.contains(&"frozen-one"));
    }

    // AC3: --only filter restricts to specified tags only
    #[test]
    fn only_flag_restricts_tabs() {
        let mut projects = BTreeMap::new();
        projects.insert("alpha".to_string(), make_project("/c/work/alpha", false));
        projects.insert("bravo".to_string(), make_project("/c/work/bravo", false));
        projects.insert(
            "charlie".to_string(),
            make_project("/c/work/charlie", false),
        );

        let cx = make_context(projects);
        let mock = MockTerminal::wezterm();
        let args = LaunchArgs {
            only: Some("alpha,charlie".to_string()),
            dry_run: false,
            no_claude: false,
            terminal: None,
        };

        run_with_terminal_factory(&args, &cx, Some(&mock)).unwrap();

        let tabs = mock.recorded_tabs();
        assert_eq!(tabs.len(), 2);
        let titles: Vec<_> = tabs.iter().map(|t| t.title.as_deref().unwrap()).collect();
        assert!(titles.contains(&"alpha"));
        assert!(titles.contains(&"charlie"));
        assert!(!titles.contains(&"bravo"));
    }

    // AC4: --no-claude sets bare fish login shell command
    #[test]
    fn no_claude_uses_bare_fish_command() {
        let mut projects = BTreeMap::new();
        projects.insert("myproj".to_string(), make_project("/c/work/myproj", false));

        let cx = make_context(projects);
        let mock = MockTerminal::wezterm();
        let args = LaunchArgs {
            only: None,
            dry_run: false,
            no_claude: true,
            terminal: None,
        };

        run_with_terminal_factory(&args, &cx, Some(&mock)).unwrap();

        let tabs = mock.recorded_tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].command.as_deref(), Some(NO_CLAUDE_SHELL_COMMAND));
    }

    // AC5: --dry-run returns output lines without spawning
    #[test]
    fn dry_run_uses_mock_and_does_not_call_launch() {
        let mut projects = BTreeMap::new();
        projects.insert("proj-a".to_string(), make_project("/c/work/proj-a", false));
        projects.insert("proj-b".to_string(), make_project("/c/work/proj-b", false));

        let cx = make_context(projects);
        let mock = MockTerminal::wezterm();
        let args = LaunchArgs {
            only: None,
            dry_run: true,
            no_claude: false,
            terminal: None,
        };

        run_with_terminal_factory(&args, &cx, Some(&mock)).unwrap();

        // dry_run does not record via launch(); recorded_tabs should be empty
        let tabs = mock.recorded_tabs();
        assert_eq!(
            tabs.len(),
            0,
            "dry_run should not call launch(), so recorded_tabs must be empty"
        );
    }
}
