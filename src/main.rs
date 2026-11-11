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
    Up {
        slug: String,
        /// builtin: draw the conversation and the board in this terminal, even where Rex or
        /// tmux would open them
        #[arg(long, value_enum, default_value_t = UpLayout::Auto)]
        layout: UpLayout,
    },
    /// Close the layout; threads keep running unless --stop-threads
    Down {
        slug: String,
        #[arg(long)]
        stop_threads: bool,
    },
    /// Stop everything of a project and put it away; nothing is deleted
    Archive { slug: String },
    /// Bring an archived project back to the list; starts nothing
    Unarchive { slug: String },
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
        /// Run beside this Rex block's board, which quitting closes (`chief up` in a Rex block)
        #[arg(long, hide = true, conflicts_with = "layout")]
        board: Option<String>,
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
        /// Serve the tools of this routine's runs instead
        #[arg(long)]
        routine: Option<String>,
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
    /// Routines: work a project runs on a schedule, a Claude session or a plain command
    Routine {
        #[command(subcommand)]
        action: RoutineAction,
    },
    /// The background server: fires routines and serves the web dashboard
    Serve {
        /// What to do; with nothing, runs the server in this terminal
        #[arg(value_enum, default_value_t = ServeAction::Run)]
        action: ServeAction,
        /// The address to listen on (default: serve.toml, else 127.0.0.1)
        #[arg(long)]
        bind: Option<std::net::IpAddr>,
        /// The port to listen on (default: serve.toml, else 4747)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Turn a project's web dashboard on and print the link that opens it
    Dashboard {
        slug: String,
        /// Turn it off instead
        #[arg(long)]
        off: bool,
        /// Don't open the link in the browser
        #[arg(long)]
        no_open: bool,
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

#[derive(Subcommand)]
enum RoutineAction {
    /// List a project's routines
    List { slug: String },
    /// Add a routine
    Add {
        slug: String,
        /// A few words for the board
        #[arg(long)]
        title: String,
        /// When it runs, in local time: five cron fields ("0 9 * * 1-5"), @hourly, @daily,
        /// @weekly, @monthly, or "every 30m" / "every 2h"
        #[arg(long)]
        schedule: String,
        /// What a session does: the thread's whole brief. With --command too, the session
        /// starts only when the command has news
        #[arg(long)]
        prompt: Option<String>,
        /// A shell command chief runs itself, with no session: exit 0 means all is well
        #[arg(long)]
        command: Option<String>,
        /// Seconds the command may run (default 300)
        #[arg(long)]
        timeout: Option<i64>,
        /// A repository of the project (default: its default repository)
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Its handle (default: made from the title)
        #[arg(long)]
        name: Option<String>,
        /// Don't tell the coordinator when a run finishes
        #[arg(long)]
        quiet: bool,
        /// Create it paused
        #[arg(long)]
        paused: bool,
    },
    /// Run a routine now, outside its schedule; a command is waited for and its output shown
    Run { slug: String, name: String },
    /// Show one routine: how it stands, what it found, its command's last runs and output
    Show { slug: String, name: String },
    /// Let a routine's command run: your yes to a command the coordinator wrote or changed
    Approve { slug: String, name: String },
    /// From a routine's command: record a number it tracks over time
    Value {
        /// What the number is; the same name every run
        series: String,
        #[arg(allow_negative_numbers = true)]
        value: f64,
        #[arg(long)]
        unit: Option<String>,
        /// A few words about this point
        #[arg(long)]
        note: Option<String>,
    },
    /// From a routine's command: say what this run came to (nothing, update or attention)
    Outcome {
        outcome: String,
        /// One line the user reads on their phone
        headline: String,
        #[arg(long)]
        details: Option<String>,
    },
    /// Run one run of a routine's command (chief starts this itself)
    #[command(hide = true)]
    Exec { slug: String, run: i64 },
    /// Stop a routine from firing until it is resumed
    Pause { slug: String, name: String },
    /// Let a paused routine fire again, from now
    Resume { slug: String, name: String },
    /// Delete a routine; its past runs stay
    Rm { slug: String, name: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum ServeAction {
    /// Run in this terminal until interrupted
    Run,
    /// Start it in the background
    Start,
    Stop,
    Status,
    /// Keep it running across logins and restarts (macOS launchd)
    Install,
    Uninstall,
}

#[derive(Clone, Copy, ValueEnum)]
enum UpLayout {
    /// Rex, tmux, else the built-in layout
    Auto,
    Builtin,
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
        Command::Up { slug, layout } => chief::project::up(
            &slug,
            match layout {
                UpLayout::Auto => chief::project::LayoutChoice::Auto,
                UpLayout::Builtin => chief::project::LayoutChoice::Builtin,
            },
        ),
        Command::Down { slug, stop_threads } => chief::project::down(&slug, stop_threads),
        Command::Archive { slug } => chief::project::archive(&slug),
        Command::Unarchive { slug } => chief::project::unarchive(&slug),
        Command::Board { project } => chief::board::run(&project),
        Command::Pane {
            project,
            layout,
            board,
        } => {
            let on_quit = match board {
                Some(board) => chief::pane::OnQuit::Board(chief::mux::BlockRef(board)),
                None if layout => chief::pane::OnQuit::Layout,
                None => chief::pane::OnQuit::Nothing,
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
        Command::Mcp { project, routine } => chief::mcp::run(&project, routine.as_deref()),
        Command::Status { slug } => chief::project::status(slug.as_deref()),
        Command::Gc { slug, dry_run } => chief::cleanup::run_gc(slug.as_deref(), dry_run),
        Command::Clean {
            slug,
            thread,
            discard,
            yes,
        } => chief::cleanup::run_clean(&slug, &thread, discard, yes),
        Command::Routine { action } => routine(action),
        Command::Serve { action, bind, port } => match action {
            ServeAction::Run => chief::serve::run(bind, port),
            ServeAction::Start => {
                let info = chief::serve::start()?;
                println!(
                    "chief serve is running at {} (pid {}).",
                    info.url(),
                    info.pid
                );
                Ok(())
            }
            ServeAction::Stop => {
                if chief::serve::stop()? {
                    println!("Stopped chief serve. No routine fires until it runs again.");
                } else {
                    println!("chief serve isn't running.");
                }
                Ok(())
            }
            ServeAction::Status => chief::serve::status(),
            ServeAction::Install => chief::serve::install(),
            ServeAction::Uninstall => chief::serve::uninstall(),
        },
        Command::Dashboard { slug, off, no_open } => chief::serve::dashboard(&slug, off, !no_open),
    }
}

fn routine(action: RoutineAction) -> Result<()> {
    use chief::routines::{self, Draft, Patch};
    match action {
        RoutineAction::List { slug } => routines::cli_list(&slug),
        RoutineAction::Add {
            slug,
            title,
            schedule,
            prompt,
            command,
            timeout,
            repo,
            model,
            effort,
            name,
            quiet,
            paused,
        } => routines::cli_add(
            &slug,
            &Draft {
                title,
                prompt: prompt.unwrap_or_default(),
                command,
                timeout_secs: timeout,
                schedule,
                repo,
                model,
                effort,
                notify: Some(!quiet),
                enabled: Some(!paused),
                name,
            },
        ),
        RoutineAction::Run { slug, name } => routines::cli_run(&slug, &name),
        RoutineAction::Show { slug, name } => routines::cli_show(&slug, &name),
        RoutineAction::Approve { slug, name } => routines::cli_approve(&slug, &name),
        RoutineAction::Value {
            series,
            value,
            unit,
            note,
        } => chief::command::cli_value(&series, value, unit, note),
        RoutineAction::Outcome {
            outcome,
            headline,
            details,
        } => chief::command::cli_outcome(&outcome, &headline, details.as_deref()),
        RoutineAction::Exec { slug, run } => chief::command::cli_exec(&slug, run),
        RoutineAction::Pause { slug, name } => routines::cli_update(
            &slug,
            &name,
            &Patch {
                enabled: Some(false),
                ..Default::default()
            },
        ),
        RoutineAction::Resume { slug, name } => routines::cli_update(
            &slug,
            &name,
            &Patch {
                enabled: Some(true),
                ..Default::default()
            },
        ),
        RoutineAction::Rm { slug, name } => routines::cli_rm(&slug, &name),
    }
}
