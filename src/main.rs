use is_terminal::IsTerminal;
use tracing_subscriber::EnvFilter;

use grove::cli::{Cli, Command};
use grove::migrate;

// ── Tracing filter ────────────────────────────────────────────────────────────

/// Build an `EnvFilter` from the verbosity count (0=WARN, 1=INFO, 2=DEBUG, 3+=TRACE).
/// `RUST_LOG` is always consulted first; if set it takes precedence.
pub fn build_filter(v_count: u8) -> EnvFilter {
    let default_level = match v_count {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    // EnvFilter::try_from_default_env() reads RUST_LOG; fall back to level-from-flag.
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("grove={default_level}")))
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    use clap::Parser;

    // GROVE_DEBUG compat: map it to grove=debug before subscriber init.
    if std::env::var("GROVE_DEBUG").is_ok() {
        unsafe { std::env::set_var("RUST_LOG", "grove=debug") };
    }

    let cli = Cli::parse();

    // Install subscriber: no ANSI colors when stderr is not a TTY.
    let use_ansi = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(build_filter(cli.verbose))
        .with_writer(std::io::stderr)
        .with_ansi(use_ansi)
        .init();

    if let Some(Command::Completions { shell }) = cli.command {
        grove::cli::completions::run(shell);
        return;
    }

    let config_dir = directories::BaseDirs::new()
        .map(|b| b.config_dir().join("grove"))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config/grove"));

    if let Err(e) = migrate::run_if_needed(&config_dir) {
        eprintln!("grove: migration error: {e}");
        std::process::exit(1);
    }

    println!("grove");
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::build_filter;

    fn filter_to_string(filter: tracing_subscriber::EnvFilter) -> String {
        format!("{filter}")
    }

    #[test]
    fn default_verbosity_is_warn() {
        // RUST_LOG must be unset for this test to be meaningful.
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
