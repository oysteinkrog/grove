pub mod adopt;
pub mod cd;
pub mod completions;
pub mod done;
pub mod fork;
pub mod freeze;
pub mod launch;
pub mod list;
pub mod new;
pub mod path;
pub mod rename;
pub mod repo;
pub mod status;

use clap::Parser;
use clap_complete::Shell;

#[derive(Parser, Debug)]
#[command(
    name = "grove",
    version,
    about = "Fast multi-repo git-worktree manager"
)]
pub struct Cli {
    /// Increase log verbosity (-v = INFO, -vv = DEBUG, -vvv = TRACE)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Create a new worktree project
    New {
        tag: String,
        #[arg(long)]
        issue: Option<u32>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        no_fetch: bool,
    },
    /// Fork an existing project's branch into a new worktree
    Fork {
        positionals: Vec<String>,
        #[arg(long)]
        issue: Option<u32>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        no_fetch: bool,
    },
    /// List all projects
    List {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        short: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_status: bool,
    },
    /// Show git status for projects
    Status {
        tags: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the path to a project worktree
    Path { tag: String },
    /// Change directory to a project worktree (prints path for shell integration)
    Cd { tag: String },
    /// Import an existing worktree into the registry
    Adopt {
        tag: String,
        path: std::path::PathBuf,
        #[arg(long)]
        move_dir: bool,
        #[arg(long)]
        issue: Option<u32>,
        #[arg(long)]
        base: Option<String>,
    },
    /// Rename a project
    Rename {
        old: String,
        new: String,
        #[arg(long)]
        no_move: bool,
    },
    /// Exclude a project from grove launch
    Freeze { tag: String },
    /// Re-include a frozen project in grove launch
    Thaw { tag: String },
    /// Manage repo configuration
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Launch terminal tabs with Claude Code for projects
    Launch {
        #[arg(long)]
        only: Option<Vec<String>>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_claude: bool,
    },
    /// Remove a worktree project
    Done {
        tag: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        keep_local: bool,
        #[arg(long)]
        keep_remote: bool,
    },
    /// Generate shell completion scripts
    #[command(hide = true, name = "__completions")]
    Completions { shell: Shell },
}

#[derive(clap::Subcommand, Debug)]
pub enum RepoCmd {
    /// List configured repos
    List,
    /// Add a repo
    Add {
        id: String,
        path: std::path::PathBuf,
    },
    /// Remove a repo
    Remove { id: String },
}
