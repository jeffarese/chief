use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use chief::state::Role;

#[derive(Parser)]
#[command(
    name = "chief",
    version,
    about = "Local Claude Projects: a coordinator, threads and a board",
    long_about = "Local Claude Projects: a coordinator, threads and a board.\n\nRun `chief` with \
                  no command to pick a project or create one in a short guided setup."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project in a short guided setup, then open it
    New,
    /// Create a project from the command line
    Init {
        name: String,
        #[arg(long)]
        goal: Option<String>,
        /// Repository folders threads work in (repeatable)
        #[arg(long = "repo")]
        repos: Vec<PathBuf>,
        /// Don't offer to open Claude in untrusted folders; print the commands instead
        #[arg(long)]
        no_trust_prompt: bool,
    },
    /// Mark every folder the project's sessions need as trusted by Claude Code
    Trust { slug: String },
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
        /// Run as the `main` block of chief's layout: quitting closes the layout
        #[arg(long, hide = true)]
        layout: bool,
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
    /// Refresh PRs, then clean up every resolved thread that loses nothing (worktree, branch,
    /// session)
    Gc {
        slug: Option<String>,
        /// Show what would be cleaned without removing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Clean up one resolved thread: remove its session, worktree and local branch
    Clean {
        slug: String,
        thread: String,
        /// Also throw away uncommitted changes and unpushed commits (asks first)
        #[arg(long)]
        discard: bool,
        /// Don't ask before discarding
        #[arg(long)]
        yes: bool,
    },
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

/// clap exits 2 on bad arguments, which Claude Code reads from a hook as "block": the `*`
/// PreToolUse would block every tool, UserPromptSubmit would erase the prompt, Stop would force
/// the turn on. So a `chief hook` other than the guard logs the error and exits 0 with nothing on
/// stdout; the guard fails closed (exit 2, the reason on stderr). Other subcommands, and help or
/// version requests, keep clap's usual handling.
fn argument_error(e: clap::Error) -> ! {
    use clap::error::ErrorKind;
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let help = matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if help || args.first().map(String::as_str) != Some("hook") {
        e.exit();
    }
    let (words, project) = hook_words(&args[1..]);
    let error = e.to_string();
    if words.iter().any(|w| w == "guard") {
        eprintln!(
            "chief's message guard got arguments it can't read, so the message is blocked. Run \
             chief up again to rewrite the hooks.\n{error}"
        );
        std::process::exit(2);
    }
    chief::hook::log_error(
        &chief::paths::hook_error_log(),
        words.first().map(String::as_str).unwrap_or("?"),
        project.as_deref().unwrap_or("?"),
        &format!("bad arguments: {}", error.trim()),
    );
    std::process::exit(0);
}

/// The plain words (the event first) and the `--project` of a `chief hook` argument list that
/// clap rejected. Values of `--project` and `--role` aren't words, so a project named `guard`
/// isn't taken for the guard.
fn hook_words(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut words = Vec::new();
    let mut project = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix("--project=") {
            project = Some(v.to_string());
        } else if arg == "--project" {
            project = it.next().cloned();
        } else if arg == "--role" {
            it.next();
        } else if !arg.starts_with('-') {
            words.push(arg.clone());
        }
    }
    (words, project)
}

fn main() -> Result<()> {
    let cli = Cli::try_parse().unwrap_or_else(|e| argument_error(e));
    // Rust ignores SIGPIPE, so `chief status | head` panicked in println!. Commands people pipe get
    // the usual quiet exit; hooks and the MCP server keep the error path they already handle.
    if !matches!(
        cli.command,
        Some(Command::Hook { .. } | Command::Mcp { .. })
    ) {
        // SAFETY: called before any thread starts; restores the default disposition.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let Some(command) = cli.command else {
        return chief::launcher::run(false);
    };
    match command {
        Command::New => chief::launcher::run(true),
        Command::Init {
            name,
            goal,
            repos,
            no_trust_prompt,
        } => chief::project::init(&name, goal.as_deref(), &repos, !no_trust_prompt),
        Command::Trust { slug } => chief::project::trust(&slug),
        Command::Up { slug } => chief::project::up(&slug),
        Command::Down { slug, stop_threads } => chief::project::down(&slug, stop_threads),
        Command::Board { project } => chief::board::run(&project),
        Command::Pane { project, layout } => {
            let on_quit = if layout {
                chief::pane::OnQuit::Layout
            } else {
                chief::pane::OnQuit::Nothing
            };
            chief::pane::run(&project, on_quit)
        }
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
        Command::Gc { slug, dry_run } => chief::cleanup::run_gc(slug.as_deref(), dry_run),
        Command::Clean {
            slug,
            thread,
            discard,
            yes,
        } => chief::cleanup::run_clean(&slug, &thread, discard, yes),
    }
}
