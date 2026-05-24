use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use is_terminal::IsTerminal;
use tracing_subscriber::EnvFilter;

use grove::cli::{Cli, Command, RepoCmd};
use grove::migrate;
use grove::repo::{Cli as RepoCli, RepoContext, discover};

pub fn build_filter(v_count: u8) -> EnvFilter {
    let default_level = match v_count {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("grove={default_level}")))
}

fn main() -> ExitCode {
    if std::env::var("GROVE_DEBUG").is_ok() {
        unsafe { std::env::set_var("RUST_LOG", "grove=debug") };
    }

    let cli = Cli::parse();

    let use_ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(build_filter(cli.verbose))
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    if let Some(Command::Completions { shell }) = cli.command {
        grove::cli::completions::run(shell);
        return ExitCode::SUCCESS;
    }

    let config_dir = directories::BaseDirs::new()
        .map(|b| b.config_dir().join("grove"))
        .unwrap_or_else(|| PathBuf::from("~/.config/grove"));

    if let Err(e) = migrate::run_if_needed(&config_dir) {
        eprintln!("grove: migration error: {e}");
        return ExitCode::FAILURE;
    }

    match dispatch(cli, &config_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grove: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli, config_dir: &std::path::Path) -> anyhow::Result<()> {
    let Some(command) = cli.command else {
        // No subcommand → print short help.
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    // Repo add bypasses RepoContext discovery (it creates the registry entry).
    if let Command::Repo {
        cmd:
            RepoCmd::Add {
                path,
                id,
                issue_prefix,
                upstream,
                fork,
                default_base,
                make_default,
            },
    } = command
    {
        let args = grove::cli::repo::AddArgs {
            path,
            id,
            issue_prefix,
            upstream,
            fork,
            default_base,
            make_default,
            config_dir: config_dir.to_path_buf(),
        };
        return grove::cli::repo::run_add(&args);
    }

    let cx = discover(config_dir, &RepoCli { repo: cli.repo })?;

    match command {
        Command::Completions { .. } => unreachable!("handled in main()"),

        Command::New {
            tag,
            issue,
            branch,
            base,
            no_fetch,
        } => {
            let args = grove::cli::new::NewArgs {
                tag,
                issue,
                branch,
                base,
                no_fetch,
            };
            grove::cli::new::run(&args, &cx).map_err(Into::into)
        }

        Command::Fork {
            positionals,
            issue,
            branch,
            no_fetch,
        } => {
            let args = grove::cli::fork::ForkArgs {
                positionals,
                issue,
                branch,
                no_fetch,
            };
            grove::cli::fork::run(&args, &cx).map_err(Into::into)
        }

        Command::List {
            repo: _,
            short,
            json,
            no_status,
        } => {
            let args = grove::cli::list::ListArgs {
                repo: None, // global --repo already routed via discover()
                short,
                json,
                no_status,
            };
            grove::cli::list::run(&args, &cx)
        }

        Command::Status { tags, json } => run_status(&cx, tags, json),

        Command::Path { tag } => {
            let args = grove::cli::path::PathArgs { tag };
            grove::cli::path::run(&args, &cx)
        }

        Command::Cd { tag } => {
            let args = grove::cli::cd::CdArgs { tag };
            grove::cli::cd::run(&args, &cx)
        }

        Command::Adopt {
            tag,
            path,
            move_dir,
            issue,
            base,
        } => {
            let args = grove::cli::adopt::AdoptArgs {
                tag,
                path,
                issue,
                base,
                mv: move_dir,
            };
            grove::cli::adopt::run(&args, &cx).map_err(Into::into)
        }

        Command::Rename { old, new, no_move } => {
            let args = grove::cli::rename::RenameArgs {
                old_tag: old,
                new_tag: new,
                no_move,
            };
            grove::cli::rename::run(&args, &cx).map_err(Into::into)
        }

        Command::Freeze { tag } => {
            let args = grove::cli::freeze::FreezeArgs { tag: Some(tag) };
            grove::cli::freeze::run_freeze(&args, &cx)
        }

        Command::Thaw { tag } => {
            let args = grove::cli::freeze::FreezeArgs { tag: Some(tag) };
            grove::cli::freeze::run_thaw(&args, &cx)
        }

        Command::Launch {
            only,
            dry_run,
            no_claude,
        } => {
            let args = grove::cli::launch::LaunchArgs {
                only: only.map(|v| v.join(",")),
                dry_run,
                no_claude,
                terminal: None,
            };
            grove::cli::launch::run(&args, &cx)
        }

        Command::Done {
            tag,
            force,
            keep_local,
            keep_remote,
        } => {
            let args = grove::cli::done::DoneArgs {
                tag,
                force,
                keep_local,
                keep_remote,
            };
            grove::cli::done::run(&args, &cx).map_err(Into::into)
        }

        Command::Repo { cmd } => run_repo(&cx, cmd, config_dir),
    }
}

fn run_status(cx: &RepoContext, tags: Vec<String>, json: bool) -> anyhow::Result<()> {
    if tags.is_empty() {
        anyhow::bail!("grove status requires at least one tag");
    }
    for tag in tags {
        let args = grove::cli::status::StatusArgs { tag, json };
        grove::cli::status::run(&args, cx)?;
    }
    Ok(())
}

fn run_repo(cx: &RepoContext, cmd: RepoCmd, config_dir: &std::path::Path) -> anyhow::Result<()> {
    use grove::cli::repo::{RepoArgs, RepoSubcommand};

    let subcommand = match cmd {
        RepoCmd::Add { .. } => unreachable!("handled in dispatch()"),
        RepoCmd::Path { default } => RepoSubcommand::Path { default },
        RepoCmd::List { json } => RepoSubcommand::List { json },
        RepoCmd::Show { id } => RepoSubcommand::Show { id },
        RepoCmd::Remove { id, force } => RepoSubcommand::Remove { id, force },
        RepoCmd::Default { id } => RepoSubcommand::Default { id },
    };
    let _ = config_dir;
    grove::cli::repo::run(&RepoArgs { subcommand }, cx)
}

#[cfg(test)]
mod tests {
    use super::build_filter;

    fn filter_to_string(filter: tracing_subscriber::EnvFilter) -> String {
        format!("{filter}")
    }

    #[test]
    fn default_verbosity_is_warn() {
        unsafe { std::env::remove_var("RUST_LOG") };
        let f = filter_to_string(build_filter(0));
        assert!(
            f.contains("warn") || f.contains("WARN"),
            "v=0 should produce WARN filter, got: {f}"
        );
    }

    #[test]
    fn verbosity_levels_escalate() {
        unsafe { std::env::remove_var("RUST_LOG") };

        let info_f = filter_to_string(build_filter(1));
        let debug_f = filter_to_string(build_filter(2));
        let trace_f = filter_to_string(build_filter(3));

        assert!(
            info_f.contains("info") || info_f.contains("INFO"),
            "v=1 should produce INFO filter, got: {info_f}"
        );
        assert!(
            debug_f.contains("debug") || debug_f.contains("DEBUG"),
            "v=2 should produce DEBUG filter, got: {debug_f}"
        );
        assert!(
            trace_f.contains("trace") || trace_f.contains("TRACE"),
            "v=3 should produce TRACE filter, got: {trace_f}"
        );
    }
}
