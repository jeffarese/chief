use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use chief::state::Role;

#[derive(Parser)]
#[command(
    name = "chief",
    version,
    about = "Local Claude Projects: a coordinator, threads and a board"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project
    Init {
        name: String,
        #[arg(long)]
        goal: Option<String>,
        /// Repository folders threads work in (repeatable)
        #[arg(long = "repo")]
        repos: Vec<PathBuf>,
    },
    /// Start or revive the coordinator and open the layout
    Up { slug: String },
    /// Close the layout; threads keep running unless --stop-threads
    Down {
        slug: String,
        #[arg(long)]
        stop_threads: bool,
    },
    /// The board TUI
    Board {
        #[arg(long)]
        project: String,
    },
    /// Left pane loop: attaches to the current target session
    Pane {
        #[arg(long)]
        project: String,
    },
    /// Hook entry point for Claude Code (reads the payload on stdin)
    Hook {
        /// Hook event name, or `guard` for the message guard
        event: String,
        #[arg(long)]
        project: String,
        #[arg(long, value_enum)]
        role: RoleArg,
    },
    /// Coordinator MCP server over stdio
    Mcp {
        #[arg(long)]
        project: String,
    },
    /// Print a project's board as text
    Status { slug: Option<String> },
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Coordinator,
    Thread,
}

impl From<RoleArg> for Role {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Coordinator => Role::Coordinator,
            RoleArg::Thread => Role::Thread,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { name, goal, repos } => chief::project::init(&name, goal.as_deref(), &repos),
        Command::Up { slug } => chief::project::up(&slug),
        Command::Down { slug, stop_threads } => chief::project::down(&slug, stop_threads),
        Command::Board { project } => chief::board::run(&project),
        Command::Pane { project } => chief::pane::run(&project),
        // A hook must never fail the Claude session that called it.
        Command::Hook {
            event,
            project,
            role,
        } => {
            chief::hook::run(&event, &project, role.into());
            Ok(())
        }
        Command::Mcp { project } => chief::mcp::run(&project),
        Command::Status { slug } => chief::project::status(slug.as_deref()),
    }
}
