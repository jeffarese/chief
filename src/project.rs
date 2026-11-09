//! Project lifecycle: init, up, down, archive, status (ARCHITECTURE §2, §3, §7, §9).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::claude::{
    self, AGENTS_TIMEOUT, AgentRow, BgSpawn, Claude, ClaudeError, RESOLVE_TIMEOUT,
};
use crate::config::{ProjectConfig, Repo};
use crate::db::{self, SessionRow};
use crate::layout::{self, Closed, Opened};
use crate::mux::rex::Rex;
use crate::mux::tmux::Tmux;
use crate::mux::{BlockRef, Mux, SessionRef, kill_on_error};
use crate::state::{Group, Role};
use crate::text::{ago, one_line};
use crate::{accounts, generated, names, paths, prompts, tools, trust};

const SLUG_CHARS: usize = 40;
const STATUS_PREVIEW_CHARS: usize = 160;
/// How long `up` waits for a respawned coordinator to get a process.
const RESPAWN_WAIT: Duration = Duration::from_secs(10);
/// How long `down` waits for the pane it ended: its attach gets 2 s to exit (§9).
const PANE_END_WAIT: Duration = Duration::from_secs(3);

const INSTRUCTIONS_TEMPLATE: &str = "<!--
Standing brief for this project. The coordinator and every new thread get this file's text:
conventions, commands to run, constraints, people to ask. Text inside comment markers like this
one is ignored.
-->
";

/// `claude` in the project's account (§3), saying once on stderr when this process's
/// `CLAUDE_CONFIG_DIR` names another one. For commands about one project.
pub fn claude_for(config: &ProjectConfig) -> Claude {
    if let Some(note) = claude::account_note(&config.slug, config.claude_config_dir.as_deref()) {
        eprintln!("{note}");
    }
    Claude::for_account(config.claude_config_dir.clone())
}

/// The project's account from its project.toml, read leniently (`ProjectConfig::read_account`).
pub fn account_of(slug: &str) -> Result<Option<PathBuf>> {
    ProjectConfig::read_account(&paths::project_dir(slug).join("project.toml"))
        .context("chief can't tell which Claude Code account the project belongs to")
}

/// `claude_for` from the project's account alone (`account_of`), for commands that don't need
/// the rest of project.toml.
pub fn claude_for_slug(slug: &str) -> Result<Claude> {
    let account = account_of(slug)?;
    if let Some(note) = claude::account_note(slug, account.as_deref()) {
        eprintln!("{note}");
    }
    Ok(Claude::for_account(account))
}

/// `<root>/projects/<slug>`, canonical when it exists: trust and session cwds use real paths
/// (macOS `/tmp` is `/private/tmp`).
pub fn project_dir(slug: &str) -> PathBuf {
    let dir = paths::project_dir(slug);
    dir.canonicalize().unwrap_or(dir)
}

/// Creates `<root>/projects/<slug>/` with project.toml, instructions.md, memory/MEMORY.md and
/// generated/*, registers the project, and offers to trust (or explains how to trust) every
/// folder Claude Code doesn't trust yet. The project belongs to the account `CLAUDE_CONFIG_DIR`
/// names here (the default one when unset), for good.
pub fn init(name: &str, goal: Option<&str>, repos: &[PathBuf], trust_prompt: bool) -> Result<()> {
    let claude = Claude::from_env();
    claude.check_version()?;
    let exe = generated::chief_exe()?;
    let account = claude.account.clone();
    let env = generated::passthrough_env(account.as_deref());
    let (config, dir) = create(name, goal, repos, account, &exe, &env)?;
    println!(
        "Created project {:?} ({}) in {}",
        config.name,
        config.slug,
        dir.display()
    );
    if let Some(account) = &config.claude_config_dir {
        println!(
            "It belongs to the Claude Code account in {} (CLAUDE_CONFIG_DIR); every chief \
             command uses that account for it.",
            account.display()
        );
    }

    let interactive =
        trust_prompt && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // The project exists by now: a trust check that fails is a line, as for an account agp
    // can't check, not a failed `init` (a second one would find the slug taken).
    match trust::check(&config, &dir, &claude) {
        Ok(check) => report_trust(&config, &check, interactive)?,
        Err(e) => {
            println!("chief couldn't check which folders Claude Code trusts: {e:#}");
            println!("To trust them later: chief trust {}", config.slug);
        }
    }
    println!(
        "\nNext: chief up {} (or run chief and pick it)",
        config.slug
    );
    Ok(())
}

/// Marks chief's own project folder trusted, lists the repos no account trusts yet and, on a
/// terminal, asks once to mark them all (§3 "Trust"); otherwise says how.
fn report_trust(config: &ProjectConfig, check: &trust::Check, interactive: bool) -> Result<()> {
    if let Some(need) = &check.project {
        for line in trust::mark_all([need]) {
            println!("{}", line.unwrap_or_else(|e| e));
        }
    }
    if !check.pool.is_empty() {
        println!(
            "\nThreads can also run in these agp accounts: {}.",
            check.pool.join(", ")
        );
    }
    for e in &check.errors {
        println!("chief couldn't check {e}");
    }
    if check.repos.is_empty() {
        return Ok(());
    }
    println!("\nClaude Code doesn't trust these folders yet, and background sessions refuse them:");
    for need in &check.repos {
        println!("  {}", need.shown());
    }
    if interactive && ask("Trust them now, as accepting Claude's trust prompt would? [Y/n] ")? {
        for line in trust::mark_all(&check.repos) {
            println!("{}", line.unwrap_or_else(|e| e));
        }
    } else {
        println!("To trust them later: chief trust {}", config.slug);
    }
    Ok(())
}

/// `chief trust <slug>`: marks every folder the project's sessions need trusted (§3 "Trust").
/// Running it is the yes.
pub fn trust(slug: &str) -> Result<()> {
    let dir = project_dir(slug);
    let config = ProjectConfig::load(&dir.join("project.toml"))
        .with_context(|| format!("no usable chief project {slug:?}"))?;
    let claude = claude_for(&config);
    let check = trust::check(&config, &dir, &claude)?;
    for e in &check.errors {
        println!("chief couldn't check {e}");
    }
    let needs: Vec<&trust::Need> = check.project.iter().chain(&check.repos).collect();
    if needs.is_empty() {
        println!("Every folder of {slug} is trusted already.");
        return Ok(());
    }
    let mut failed = false;
    for line in trust::mark_all(needs) {
        match line {
            Ok(l) => println!("{l}"),
            Err(l) => {
                failed = true;
                println!("{l}");
            }
        }
    }
    if failed {
        bail!("some folders aren't trusted");
    }
    Ok(())
}

/// Prints `question` and reads a yes (the default) or no.
fn ask(question: &str) -> Result<bool> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

/// Writes the project files and registers the project; nothing is left behind on failure.
pub fn create(
    name: &str,
    goal: Option<&str>,
    repos: &[PathBuf],
    account: Option<PathBuf>,
    exe: &Path,
    env: &[(String, String)],
) -> Result<(ProjectConfig, PathBuf)> {
    create_with(name, goal, repos, account, exe, env, |_| {})
}

/// The slug a project name gets: `[a-z0-9-]`, empty when the name has no letter or digit.
pub fn slug_of(name: &str) -> String {
    names::slugify(name.trim(), SLUG_CHARS)
}

/// Whether a project already has `slug` (its folder or its registration).
pub fn slug_taken(conn: &Connection, slug: &str) -> Result<bool> {
    Ok(paths::project_dir(slug).exists() || db::project_dir(conn, slug)?.is_some())
}

/// `create`, with `tune` changing the defaults (the launcher's quick settings) before anything
/// is validated or written.
pub fn create_with(
    name: &str,
    goal: Option<&str>,
    repos: &[PathBuf],
    account: Option<PathBuf>,
    exe: &Path,
    env: &[(String, String)],
    tune: impl FnOnce(&mut ProjectConfig),
) -> Result<(ProjectConfig, PathBuf)> {
    let name = name.trim();
    let slug = slug_of(name);
    if slug.is_empty() {
        bail!("the project name {name:?} needs at least one letter or digit");
    }
    let goal = goal.map(str::trim).filter(|g| !g.is_empty());
    let mut config = ProjectConfig::new(name, &slug, goal, repo_entries(repos)?);
    config.claude_config_dir = account;
    tune(&mut config);
    config.validate()?;

    let conn = db::open(&paths::db())?;
    let dir = paths::project_dir(&slug);
    if slug_taken(&conn, &slug)? {
        bail!(
            "a project with slug {slug:?} already exists ({})",
            dir.display()
        );
    }
    let written = write_files(&config, &dir, exe, env).and_then(|()| {
        let dir = project_dir(&slug);
        db::register_project(&conn, &slug, &dir, db::now_ms())?;
        Ok(dir)
    });
    match written {
        Ok(dir) => Ok((config, dir)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

fn write_files(
    config: &ProjectConfig,
    dir: &Path,
    exe: &Path,
    env: &[(String, String)],
) -> Result<()> {
    std::fs::create_dir_all(dir.join("memory"))
        .with_context(|| format!("creating {}", dir.display()))?;
    config.save(&dir.join("project.toml"))?;
    std::fs::write(dir.join("instructions.md"), INSTRUCTIONS_TEMPLATE)?;
    std::fs::write(
        dir.join("memory/MEMORY.md"),
        format!(
            "# Memory for {}: one line per memory file in this folder\n",
            config.name
        ),
    )?;
    generated::write_all(dir, config, exe, env)
}

/// Canonical repo folders, named after their folder (`-2`, `-3` … on a clash).
fn repo_entries(paths: &[PathBuf]) -> Result<Vec<Repo>> {
    let mut repos: Vec<Repo> = Vec::new();
    for path in paths {
        add_repo(&mut repos, path)?;
    }
    Ok(repos)
}

/// Appends `path` (canonical, named after its folder with `-2`, `-3` … on a clash) unless it is
/// already listed. Returns the entry's name and whether it was added.
pub fn add_repo(repos: &mut Vec<Repo>, path: &Path) -> Result<(String, bool)> {
    if !path.is_dir() {
        bail!("repository folder {} doesn't exist", path.display());
    }
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if let Some(r) = repos.iter().find(|r| r.path == path) {
        return Ok((r.name.clone(), false));
    }
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    let mut name = base.clone();
    let mut n = 2;
    while repos.iter().any(|r| r.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    repos.push(Repo {
        name: name.clone(),
        path,
    });
    Ok((name, true))
}

/// What `up` did to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorAction {
    Started,
    Respawned,
    Running,
}

/// Starts or respawns the coordinator, then opens the layout (§9): in a Rex block, the board
/// beside it and the conversation right here; else a Rex session when its server runs, else a
/// tmux window when this runs inside tmux, else chief draws both in this terminal, else it
/// prints the two commands to run side by side.
pub fn up(slug: &str, choice: LayoutChoice) -> Result<()> {
    match up_with(slug, choice, &mut print_note)? {
        Where::Here { board } => crate::pane::run(slug, crate::pane::OnQuit::Board(board)),
        Where::Builtin => run_builtin(slug),
        Where::Manual => {
            print_manual_layout(slug);
            Ok(())
        }
        Where::Rex { .. } | Where::Tmux => Ok(()),
    }
}

/// Which layout `up` opens (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutChoice {
    /// Rex, tmux, then the built-in layout, as they apply.
    #[default]
    Auto,
    /// The built-in layout, even where Rex or tmux would do.
    Builtin,
}

/// The built-in layout in this terminal (§9), until the user quits it.
pub fn run_builtin(slug: &str) -> Result<()> {
    crate::builtin::run(slug)
}

/// A line `up`, `down` or `archive` reports: what it did, what it didn't and why (no layout,
/// say), or a warning or failure (stderr on the command line).
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Did(String),
    Info(String),
    Warn(String),
}

/// A note on the command line: lines on stdout, warnings on stderr.
pub fn print_note(note: Note) {
    match note {
        Note::Did(line) | Note::Info(line) => println!("{line}"),
        Note::Warn(line) => eprintln!("{line}"),
    }
}

/// Where `up` put the layout (§9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Where {
    /// The board beside this Rex block: the caller runs the pane in this terminal.
    Here {
        board: BlockRef,
    },
    /// A Rex session; `shown`: Rex switched to it.
    Rex {
        shown: bool,
    },
    Tmux,
    /// Neither, on a terminal: the caller runs the built-in layout in it.
    Builtin,
    /// None of them: the user runs the pane and the board (the reason is in the notes).
    Manual,
}

/// `up`, reporting through `note` instead of printing, for the launcher (§10a).
pub fn up_with(slug: &str, choice: LayoutChoice, note: &mut dyn FnMut(Note)) -> Result<Where> {
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if choice == LayoutChoice::Builtin && !terminal {
        bail!("the built-in layout needs a terminal");
    }
    let dir = project_dir(slug);
    let config = ProjectConfig::load(&dir.join("project.toml"))
        .with_context(|| format!("no usable chief project {slug:?}; create it with chief init"))?;
    if let Some(n) = claude::account_note(&config.slug, config.claude_config_dir.as_deref()) {
        note(Note::Warn(n));
    }
    let claude = Claude::for_account(config.claude_config_dir.clone());
    claude.check_version()?;
    let exe = generated::chief_exe()?;
    if generated::in_cargo_target(&exe) {
        note(Note::Warn(format!(
            "warning: chief runs from {}, inside a cargo target/ folder: `cargo clean` would \
             disable chief's hooks and message guard. Install with `cargo install --path .`.",
            exe.display()
        )));
    }
    let env = generated::passthrough_env(config.claude_config_dir.as_deref());
    generated::write_all(&dir, &config, &exe, &env)?;
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        db::register_project(&conn, slug, &dir, db::now_ms())?;
    }
    // Opening a project is wanting it back; nothing of it runs while archived.
    if db::unarchive_project(&conn, slug)? {
        note(Note::Did(format!("Unarchived {}.", config.name)));
    }
    let trust_config = claude.trust_config();
    let mut warn = |line: String| note(Note::Warn(line));
    let agp = crate::agp::Agp::from_env();
    let (row, action) = ensure_coordinator(
        &conn,
        &claude,
        &agp,
        &config,
        &dir,
        &trust_config,
        &mut warn,
    )?;
    let verb = match action {
        CoordinatorAction::Started => "started",
        CoordinatorAction::Respawned => "respawned",
        CoordinatorAction::Running => "already running",
    };
    note(Note::Did(format!("Coordinator {} {verb}.", row.name)));

    // A project with routines or its dashboard on needs the server (§14); nothing else does.
    match crate::serve::ensure_for(&conn, &config) {
        Ok(Some(line)) => note(Note::Did(line)),
        Ok(None) => {}
        Err(e) => note(Note::Warn(format!(
            "warning: chief serve didn't start, so this project's routines don't fire and its \
             dashboard isn't served: {e:#}"
        ))),
    }

    if choice == LayoutChoice::Builtin {
        return Ok(Where::Builtin);
    }
    let found = open_layout(slug, (&dir, &exe, &env), &conn, terminal, note)?;
    Ok(match found {
        // Neither Rex nor tmux: chief draws both sides itself.
        Where::Manual if terminal => {
            note(Note::Info(
                "Drawing the layout in this terminal: the conversation on the left, its \
                 threads on the right (Ctrl+\\ switches sides, Ctrl+Q quits)."
                    .into(),
            ));
            Where::Builtin
        }
        found => found,
    })
}

/// The first of a Rex block, a Rex session and a tmux window that opens (§9), else `Manual`.
fn open_layout(
    slug: &str,
    (dir, exe, env): (&Path, &Path, &[(String, String)]),
    conn: &Connection,
    terminal: bool,
    note: &mut dyn FnMut(Note),
) -> Result<Where> {
    let label = layout::session_label(slug);
    let rex = Rex::from_env();
    let open = |mux: &dyn Mux, ours: Option<String>| {
        let ours = ours.map(SessionRef);
        layout::open(mux, slug, dir, exe, env, ours.as_ref())
    };
    let done = |opened: &layout::Layout| match opened.opened {
        Opened::Created => "opened",
        Opened::Repaired => "repaired",
        Opened::Reused => "already open",
    };
    // A layout just created whose id isn't stored would be nobody's (`kill_on_error`).
    let store = |mux: &dyn Mux, opened: &layout::Layout, stored: Result<()>| match opened.opened {
        Opened::Created => kill_on_error(mux, &opened.session, stored),
        _ => stored,
    };
    // In a Rex block on a terminal: nothing opens elsewhere (and no app needs to switch).
    if let Some((session, here)) = Rex::here().filter(|_| terminal) {
        let beside = rex
            .check()
            .and_then(|()| layout::open_beside(&rex, slug, (dir, exe, env), &session, &here));
        return Ok(match beside {
            Ok((board, opened)) => {
                let verb = match opened {
                    Opened::Created => "opened",
                    Opened::Repaired => "reopened",
                    Opened::Reused => "already open",
                };
                note(Note::Did(format!("Board {verb} beside this block.")));
                Where::Here { board }
            }
            Err(e) => {
                note(Note::Info(format!("No board beside this Rex block: {e:#}")));
                Where::Manual
            }
        });
    }
    let shown = match rex.check() {
        Ok(()) => match open(&rex, db::get_rex_session(conn, slug)?) {
            Ok(opened) => {
                store(
                    &rex,
                    &opened,
                    db::set_rex_session(conn, slug, Some(&opened.session.0)),
                )?;
                note(Note::Did(format!("Rex session {label} {}.", done(&opened))));
                if !opened.shown {
                    note(Note::Info(format!(
                        "Rex didn't switch to it (that needs exactly one Rex app connected). \
                         Pick {label} in the app, or run: rex -C <app> do session.select \
                         session_id={}",
                        opened.session.0
                    )));
                }
                Where::Rex {
                    shown: opened.shown,
                }
            }
            Err(e) => {
                note(Note::Info(format!("No Rex layout: {e:#}")));
                Where::Manual
            }
        },
        Err(why) => match Tmux::current() {
            Some(tmux) => match open(&tmux, db::get_tmux_window(conn, slug)?) {
                Ok(opened) => {
                    let stored = db::set_tmux_window(conn, slug, Some(&opened.session.0));
                    store(&tmux, &opened, stored)?;
                    note(Note::Did(format!("tmux window {label} {}.", done(&opened))));
                    Where::Tmux
                }
                Err(e) => {
                    note(Note::Info(format!("No tmux layout: {e:#}")));
                    Where::Manual
                }
            },
            None => {
                note(Note::Info(format!(
                    "No layout: Rex isn't available ({why:#}) and this terminal isn't inside tmux."
                )));
                Where::Manual
            }
        },
    };
    Ok(shown)
}

/// Without a multiplexer the user makes the layout: any two terminals or splits work, since the
/// board switches the pane by signalling it (§9).
fn print_manual_layout(slug: &str) {
    println!("Run these side by side, in two terminals or splits of one:");
    println!("  chief pane --project {slug}");
    println!("  chief board --project {slug}");
}

/// §7 "Coordinator spawn": start it when it has no row or no short id, `claude respawn` it
/// when it has no live process, otherwise leave it alone. A spawn that failed but started the
/// session anyway is adopted, now or (when chief couldn't tell) on the next call.
pub fn ensure_coordinator(
    conn: &Connection,
    claude: &Claude,
    agp: &crate::agp::Agp,
    config: &ProjectConfig,
    dir: &Path,
    trust_config: &Path,
    warn: &mut dyn FnMut(String),
) -> Result<(SessionRow, CoordinatorAction)> {
    let slug = config.slug.as_str();
    let name = names::coordinator_name(slug);
    let existing = db::get_coordinator(conn, slug)?;
    if let Some(row) = existing.as_ref().filter(|r| r.short_id.is_some()) {
        accounts::refresh_row(conn, claude, row, AGENTS_TIMEOUT, db::now_ms)?;
        let row = db::get_session(conn, &row.name)?.context("coordinator row vanished")?;
        if row.pid.is_some() {
            // One started before agp watched coordinators.
            let row = match row.via_agp() {
                true => row,
                false => watch_coordinator(conn, agp, config, &row).unwrap_or(row),
            };
            return Ok((row, CoordinatorAction::Running));
        }
        let row = respawn(conn, claude, agp, dir, config, &row, RESPAWN_WAIT)?;
        return Ok((row, CoordinatorAction::Respawned));
    }

    // chief made this folder, so it marks it trusted itself (§3 "Trust"); projects from before
    // that, or a trust Claude Code dropped, get the same.
    if !claude::is_trusted_in(trust_config, dir)? {
        claude::mark_trusted(trust_config, dir).map_err(|e| {
            anyhow::Error::new(ClaudeError::NotTrusted(dir.to_path_buf())).context(format!("{e:#}"))
        })?;
    }
    let live = claude.agents()?;
    // A row without a short id is left by a spawn chief couldn't confirm: adopt its session, but
    // only one in the project folder (the name alone may be another chief home's coordinator).
    let leftover = existing.as_ref().and_then(|row| match &row.session_id {
        Some(sid) => live.iter().find(|a| &a.session_id == sid && a.id.is_some()),
        None => claude::started_session(&live, &name, &[], dir),
    });
    if let Some(agent) = leftover {
        warn(format!(
            "warning: adopting {name} ({}), which an earlier chief up started without confirming",
            agent.id.as_deref().unwrap_or_default()
        ));
        store_ids(conn, &name, agent.id.as_deref(), Some(agent))?;
        return ensure_coordinator(conn, claude, agp, config, dir, trust_config, warn);
    }
    if live
        .iter()
        .any(|a| a.name.as_deref() == Some(name.as_str()) && a.pid.is_some())
    {
        bail!(
            "another live Claude session is already named {name}; stop it (claude stop <id>) and \
             run chief up again"
        );
    }
    let inserted = existing.is_none();
    if inserted {
        let mut row = SessionRow::new(&name, slug, Role::Coordinator, db::now_ms());
        row.title = Some("Coordinator".to_string());
        row.cwd = Some(dir.to_string_lossy().into_owned());
        row.model = Some(config.coordinator.model.clone());
        row.effort = Some(config.coordinator.effort.clone());
        db::insert_session(conn, &row)?;
    }
    let spec = BgSpawn {
        cwd: dir.to_path_buf(),
        name: name.clone(),
        model: config.coordinator.model.clone(),
        effort: config.coordinator.effort.clone(),
        permission_mode: "auto".to_string(),
        settings: generated::coordinator_settings_path(dir),
        mcp_config: Some(generated::mcp_config_path(dir)),
        add_dirs: config.repos.iter().map(|r| r.path.clone()).collect(),
        disallowed_tools: vec!["Bash".to_string()],
        append_system_prompt: prompts::coordinator(config, dir, &prompts::read_instructions(dir)),
        prompt: None,
    };
    let (short_id, agent) = match claude.spawn_bg(&spec) {
        Ok(short_id) => {
            let resolved = claude.resolve_session(&short_id, RESOLVE_TIMEOUT);
            if let Err(e) = &resolved {
                warn(format!("warning: {e:#}"));
            }
            (Some(short_id), resolved.ok())
        }
        // An untrusted folder starts nothing.
        Err(e) if e.downcast_ref::<ClaudeError>().is_some() => {
            if inserted {
                let _ = db::delete_session(conn, &name);
            }
            return Err(e);
        }
        // It may have started anyway (a timeout, output chief doesn't recognise); deleting the
        // row would make the next `up` refuse the name as taken.
        Err(e) => match claude.find_started(&name, &live, dir, RESOLVE_TIMEOUT) {
            Ok(Some(agent)) => {
                warn(format!(
                    "warning: `claude --bg` reported a failure, but {name} started anyway, so \
                     chief kept it. The failure: {e:#}"
                ));
                (agent.id.clone(), Some(agent))
            }
            Ok(None) => {
                let kept = match inserted.then(|| db::delete_unattached_session(conn, &name)) {
                    None | Some(Ok(true)) => return Err(e),
                    Some(Ok(false)) => "its hooks already ran, so it started".to_string(),
                    Some(Err(del)) => format!("chief couldn't remove it ({del:#})"),
                };
                return Err(e.context(format!(
                    "{name}'s row is kept: {kept}; run chief up again, and it adopts the \
                     session once claude agents lists it"
                )));
            }
            Err(lookup) => {
                return Err(e.context(format!(
                    "chief couldn't check whether {name} started anyway ({lookup:#}), so its row \
                     is kept: run chief up again, and it adopts the session if it did start"
                )));
            }
        },
    };
    let mut row = store_ids(conn, &name, short_id.as_deref(), agent.as_ref())?;
    if let Some(watched) = watch_coordinator(conn, agp, config, &row) {
        row = watched;
    }
    Ok((row, CoordinatorAction::Started))
}

/// Hands the coordinator to agp to watch for usage limits (§5 "Limits and moves"), so it goes on
/// by itself after a reset. Always `wait`, never a move: its threads reach it by name in its
/// own account. Best effort, like the agp a project may not have; the row marked agp's on
/// success.
fn watch_coordinator(
    conn: &Connection,
    agp: &crate::agp::Agp,
    config: &ProjectConfig,
    row: &SessionRow,
) -> Option<SessionRow> {
    if config.threads.runner != crate::config::Runner::Agp {
        return None;
    }
    let short = row.short_id.as_deref()?;
    if !agp.supports_bg() {
        return None;
    }
    // Quietly: the pane calls this with its terminal in use.
    agp.watch(short, None, &crate::moves::coordinator_run())
        .ok()?;
    db::immediate(conn, |conn| {
        let Some(mut row) = db::get_session(conn, &row.name)? else {
            return Ok(None);
        };
        row.runner = Some(db::RUNNER_AGP.to_string());
        db::update_session(conn, &row)?;
        Ok(Some(row))
    })
    .ok()
    .flatten()
}

/// Stores a spawned session's short id and, unless the row already has one, its session id and
/// pid, in one transaction (its hooks may be writing the row).
fn store_ids(
    conn: &Connection,
    name: &str,
    short_id: Option<&str>,
    agent: Option<&AgentRow>,
) -> Result<SessionRow> {
    db::immediate(conn, |conn| {
        let mut row = db::get_session(conn, name)?.context("coordinator row vanished")?;
        if let Some(id) = short_id {
            row.short_id = Some(id.to_string());
        }
        if let Some(agent) = agent
            && row.session_id.is_none()
        {
            row.session_id = Some(agent.session_id.clone());
            row.pid = agent.pid;
        }
        row.updated_at = db::now_ms();
        db::update_session(conn, &row)?;
        Ok(row)
    })
}

/// `claude respawn` for `row` in its own account (`claude` is the project's), after rewriting
/// `<dir>/generated/*` from `config` (a respawned session reads its hooks again, §3), then
/// waits up to `wait` for a pid; returns the row.
pub fn respawn(
    conn: &Connection,
    claude: &Claude,
    agp: &crate::agp::Agp,
    dir: &Path,
    config: &ProjectConfig,
    row: &SessionRow,
    wait: Duration,
) -> Result<SessionRow> {
    // A thread agp is moving to another account must not be revived where agp just stopped it
    // (§5 "Limits and moves"); once moved, it may run already.
    let project = claude.account.as_deref();
    let settled = crate::moves::settle(
        conn,
        agp,
        row,
        project,
        crate::moves::SETTLE_TIMEOUT,
        db::now_ms,
    )?;
    if !claude::same_account(
        settled.account(project).as_deref(),
        row.account(project).as_deref(),
    ) {
        let now = wait_for_pid(conn, claude, &settled, Duration::ZERO)?;
        if now.pid.is_some() {
            return Ok(now);
        }
    }
    let row = &settled;
    let short_id = row.short_id.as_deref().with_context(|| {
        format!(
            "{} was never started, so there is nothing to respawn",
            row.name
        )
    })?;
    generated::rewrite(dir, config)?;
    // Before the respawn, best effort: agp writes again the settings it merged with ours (the
    // respawned session reads them) and brings back a watcher that ended. A coordinator from
    // before agp watched coordinators is handed to it after.
    let coordinator = row.role == Role::Coordinator.as_str();
    if row.via_agp() {
        let _ = crate::moves::rearm(agp, row, &config.threads);
    }
    claude.for_row(row).respawn(short_id)?;
    if coordinator && !row.via_agp() {
        watch_coordinator(conn, agp, config, row);
    }
    wait_for_pid(conn, claude, row, wait)
}

/// Polls `claude agents` in `row`'s account until it has a pid or `timeout` passes; returns the
/// row either way. `claude` is the project's account.
pub fn wait_for_pid(
    conn: &Connection,
    claude: &Claude,
    row: &SessionRow,
    timeout: Duration,
) -> Result<SessionRow> {
    let deadline = Instant::now() + timeout;
    let name = &row.name;
    let account = [row.account(claude.account.as_deref())];
    loop {
        // A failed `claude agents` is retried until the deadline.
        accounts::refresh_accounts(
            conn,
            &row.project_slug,
            claude,
            &account,
            AGENTS_TIMEOUT,
            db::now_ms,
        )?;
        let row = db::get_session(conn, name)?.with_context(|| format!("{name} vanished"))?;
        if row.pid.is_some() || Instant::now() >= deadline {
            return Ok(row);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Ends the running pane, closes the layout `up` opened (Rex or tmux; nothing when the user made
/// it) and stops the coordinator; with `stop_threads`, every live thread too. Stopping keeps
/// conversations and worktrees (`claude stop`, never `claude rm`).
pub fn down(slug: &str, stop_threads: bool) -> Result<()> {
    down_with(slug, stop_threads, &mut print_note)
}

/// `down`, reporting through `note` instead of printing, for `archive` and the launcher (§10a).
/// An error once every step ran means some step failed (its note says which).
pub fn down_with(slug: &str, stop_threads: bool, note: &mut dyn FnMut(Note)) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        bail!("no chief project {slug:?}");
    }
    let pane_ended = end_pane(&conn, slug, note)?;
    let mut failed = if pane_ended { 0 } else { 1 };
    failed += close_layout(&conn, slug, note)?;
    // Left by a pane that couldn't record itself; one still running keeps what it shows.
    if pane_ended {
        db::set_pane_showing(&conn, slug, None)?;
    }
    // The next `chief up` starts on the coordinator: a pane that went back to the thread it last
    // showed would wake it right after `--stop-threads` stopped it.
    db::set_pane_target(&conn, slug, None)?;

    let account = match account_of(slug) {
        Ok(account) => account,
        Err(e) => {
            note(Note::Warn(format!("No session stopped: {e:#}")));
            bail!("chief down {slug} didn't finish cleanly");
        }
    };
    if let Some(n) = claude::account_note(slug, account.as_deref()) {
        note(Note::Warn(n));
    }
    let claude = Claude::for_account(account);
    let listings = accounts::refresh(&conn, slug, &claude, AGENTS_TIMEOUT, db::now_ms)?;
    if let Some(e) = listings.error() {
        note(Note::Warn(format!(
            "warning: `claude agents` failed, using the last known state: {e}"
        )));
    }
    let mut left_running = 0;
    for row in db::list_sessions(&conn, slug, None)? {
        let coordinator = row.role == Role::Coordinator.as_str();
        let live = row.pid.is_some() && row.cleaned_at.is_none();
        let Some(short_id) = row.short_id.as_deref().filter(|_| live) else {
            if coordinator {
                note(Note::Info(format!(
                    "Coordinator {} isn't running.",
                    row.name
                )));
            }
            continue;
        };
        if !coordinator && !stop_threads {
            left_running += 1;
            continue;
        }
        let what = if coordinator { "coordinator" } else { "thread" };
        match claude.for_row(&row).stop(short_id) {
            Ok(()) => note(Note::Did(format!("Stopped {what} {}.", row.name))),
            Err(e) => {
                failed += 1;
                note(Note::Warn(format!(
                    "Could not stop {what} {}: {e:#}",
                    row.name
                )));
            }
        }
    }
    if left_running > 0 {
        note(Note::Info(format!(
            "{left_running} thread{} still running; chief down {slug} --stop-threads stops them.",
            if left_running == 1 { " is" } else { "s are" }
        )));
    }
    if failed > 0 {
        bail!("chief down {slug} didn't finish cleanly");
    }
    Ok(())
}

/// Ends the recorded `chief pane` (§9), wherever it runs, and says so through `note`; false when
/// it still runs after `PANE_END_WAIT`. A pane `down` didn't open (in the user's own Rex block,
/// or started by hand) would otherwise attach again and respawn the session `down` is about to
/// stop. SIGTERM ends it as a hangup does: it reaps its attach, clears its record and closes a
/// board it opened beside it, but not chief's layout, which `down` closes next.
fn end_pane(conn: &Connection, slug: &str, note: &mut dyn FnMut(Note)) -> Result<bool> {
    let Some(pid) = crate::pane::live_pane(conn, slug)? else {
        return Ok(true);
    };
    // SAFETY: a plain syscall, to the process `live_pane` just proved to be the pane.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + PANE_END_WAIT;
    while Instant::now() < deadline {
        // Its record cleared, or it is gone without clearing it.
        if crate::pane::live_pane(conn, slug)? != Some(pid) {
            note(Note::Did(format!("Ended chief pane (pid {pid}).")));
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    note(Note::Warn(format!(
        "chief pane (pid {pid}) is still running after {} s.",
        PANE_END_WAIT.as_secs()
    )));
    Ok(false)
}

/// Closes the layout `chief up` opened (Rex session or tmux window, §9), saying what it did
/// through `note`; how many closes failed. Sessions are left alone.
pub fn close_layout(conn: &Connection, slug: &str, note: &mut dyn FnMut(Note)) -> Result<usize> {
    let mut failed = 0;
    let label = layout::session_label(slug);
    let rex_session = db::get_rex_session(conn, slug)?.map(SessionRef);
    let tmux_window = db::get_tmux_window(conn, slug)?.map(SessionRef);
    if rex_session.is_none() && tmux_window.is_none() {
        note(Note::Info("No layout of chief's is open.".into()));
    }
    if let Some(ours) = &rex_session {
        match layout::close(&Rex::from_env(), Some(ours)) {
            Ok(Closed::Killed(_)) => {
                db::set_rex_session(conn, slug, None)?;
                note(Note::Did(format!("Closed Rex session {label}.")));
            }
            Ok(Closed::Blocks(_)) => note(Note::Did(format!(
                "Closed chief's blocks in Rex session {label}; it holds other blocks too, so it \
                 stays open."
            ))),
            Ok(Closed::Nothing) => {
                db::set_rex_session(conn, slug, None)?;
                note(Note::Info(format!("No Rex session {label} was open.")));
            }
            Err(e) => {
                failed += 1;
                note(Note::Warn(format!("Rex session {label} not closed: {e:#}")));
            }
        }
    }
    if let Some(ours) = &tmux_window {
        // The stored id names the server, so this works from any terminal.
        let closed = match Tmux::for_layout(ours) {
            Some(tmux) => layout::close(&tmux, Some(ours)),
            None => Ok(Closed::Nothing),
        };
        match closed {
            Ok(Closed::Killed(_)) => {
                db::set_tmux_window(conn, slug, None)?;
                note(Note::Did(format!("Closed tmux window {label}.")));
            }
            Ok(Closed::Blocks(_)) => note(Note::Did(format!(
                "Closed chief's panes in tmux window {label}; it holds other panes too, so it \
                 stays open."
            ))),
            Ok(Closed::Nothing) => {
                db::set_tmux_window(conn, slug, None)?;
                note(Note::Info(format!("No tmux window {label} was open.")));
            }
            Err(e) => {
                failed += 1;
                note(Note::Warn(format!("tmux window {label} not closed: {e:#}")));
            }
        }
    }
    Ok(failed)
}

/// The project's name from project.toml, or its slug when that can't be read.
fn name_of(slug: &str) -> String {
    ProjectConfig::load(&project_dir(slug).join("project.toml"))
        .map(|c| c.name)
        .unwrap_or_else(|_| slug.to_string())
}

/// `chief archive <slug>`: everything `chief down <slug> --stop-threads` does, then the project
/// is archived (hidden from the launcher's list and `chief status`). Nothing is deleted.
pub fn archive(slug: &str) -> Result<()> {
    archive_with(slug, &mut print_note)
}

/// `archive`, reporting through `note`, for the launcher (§10a). When `down` didn't finish
/// cleanly the project isn't archived: something of it may still run, and archived would hide it.
pub fn archive_with(slug: &str, note: &mut dyn FnMut(Note)) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        bail!("no chief project {slug:?}");
    }
    let name = name_of(slug);
    if db::archived_at(&conn, slug)?.is_some() {
        note(Note::Info(format!("{name} is archived already.")));
        return Ok(());
    }
    if let Err(e) = down_with(slug, true, note) {
        bail!(
            "{name} isn't archived: {e:#}, and an archived project keeps nothing running. Run \
             chief archive {slug} again once that is fixed."
        );
    }
    db::archive_project(&conn, slug, db::now_ms())?;
    note(Note::Did(format!(
        "Archived {name}. Nothing was deleted: chief unarchive {slug} or chief up {slug} brings \
         it back."
    )));
    Ok(())
}

/// `chief unarchive <slug>`: back in the launcher's list and `chief status`; starts nothing.
pub fn unarchive(slug: &str) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        bail!("no chief project {slug:?}");
    }
    let name = name_of(slug);
    if db::unarchive_project(&conn, slug)? {
        println!("Unarchived {name}. chief up {slug} opens it.");
    } else {
        println!("{name} isn't archived.");
    }
    Ok(())
}

/// Prints the board as text, grouped as on the board: one project, or all of them but the
/// archived ones.
pub fn status(slug: Option<&str>) -> Result<()> {
    let conn = db::open(&paths::db())?;
    let (projects, hidden) = match slug {
        Some(slug) => {
            if db::project_dir(&conn, slug)?.is_none() {
                bail!("no chief project {slug:?}");
            }
            (vec![slug.to_string()], 0)
        }
        None => {
            let mut live = Vec::new();
            let mut hidden = 0;
            for (slug, _) in db::list_projects(&conn)? {
                if db::archived_at(&conn, &slug)?.is_some() {
                    hidden += 1;
                } else {
                    live.push(slug);
                }
            }
            (live, hidden)
        }
    };
    if projects.is_empty() && hidden == 0 {
        println!("No projects yet. Create one with chief init <name>.");
        return Ok(());
    }
    // One `claude agents` per account, shared by the projects that use it: each sees only its
    // own supervisor's sessions (§1).
    let mut taken: Vec<accounts::Listing> = Vec::new();
    let mut out = String::new();
    for slug in &projects {
        let account = match account_of(slug) {
            Ok(account) => Some(account),
            Err(e) => {
                eprintln!("warning: {slug}: {e:#}; showing the last known state");
                None
            }
        };
        if let Some(account) = &account {
            let rows = db::list_sessions(&conn, slug, None)?;
            let used = accounts::in_use(account.as_deref(), &rows);
            let had = |a: &Option<PathBuf>, taken: &[accounts::Listing]| {
                taken
                    .iter()
                    .any(|l| claude::same_account(l.account(), a.as_deref()))
            };
            let missing: Vec<_> = used.iter().filter(|a| !had(a, &taken)).cloned().collect();
            let claude = Claude::for_account(account.clone());
            for l in accounts::list(&claude, &missing, AGENTS_TIMEOUT, db::now_ms).all {
                if let Err(e) = &l.agents {
                    eprintln!(
                        "warning: `claude agents` failed for the account {}, showing the last \
                         known state: {e}",
                        claude::account_label(l.account())
                    );
                }
                taken.push(l);
            }
            let ours = taken.iter().filter(|l| {
                used.iter()
                    .any(|a| claude::same_account(l.account(), a.as_deref()))
            });
            accounts::sync(&conn, slug, account.as_deref(), ours, db::now_ms())?;
        }
        let rows = db::list_sessions(&conn, slug, None)?;
        let prefix = claude::shell_prefix(account.flatten().as_deref());
        let archived = db::archived_at(&conn, slug)?.is_some();
        out.push_str(&render_status(slug, archived, &rows, &prefix, db::now_ms()));
    }
    if hidden > 0 {
        out.push_str(&format!(
            "{hidden} archived project{} not shown; chief status <slug> shows one.\n",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    print!("{out}");
    Ok(())
}

/// One project's board as text; `prefix` goes before `claude attach` (`Claude::shell_prefix`).
pub fn render_status(
    slug: &str,
    archived: bool,
    rows: &[SessionRow],
    prefix: &str,
    now: i64,
) -> String {
    let mut out = if archived {
        format!("{slug} (archived; chief unarchive {slug} brings it back)\n")
    } else {
        format!("{slug}\n")
    };
    match rows.iter().find(|r| r.role == Role::Coordinator.as_str()) {
        Some(c) => {
            let state = if c.pid.is_some() {
                c.group_name.as_str()
            } else {
                "asleep"
            };
            let attach = c
                .short_id
                .as_deref()
                .map(|id| format!(", {prefix}claude attach {id}"))
                .unwrap_or_default();
            out.push_str(&format!("  coordinator {} ({state}{attach})\n", c.name));
        }
        None => out.push_str("  coordinator not started (chief up)\n"),
    }
    let mut threads: Vec<_> = rows
        .iter()
        .filter(|r| r.role == Role::Thread.as_str())
        .cloned()
        .collect();
    if threads.is_empty() {
        out.push_str("  no threads\n");
    }
    tools::board_order(&mut threads);
    for group in Group::ORDER {
        let in_group: Vec<_> = threads
            .iter()
            .filter(|t| Group::parse(&t.group_name).unwrap_or(Group::Idle) == group)
            .collect();
        if in_group.is_empty() {
            continue;
        }
        out.push_str(&format!("  {} ({})\n", group.label(), in_group.len()));
        for t in in_group {
            let mut parts = vec![t.name.clone()];
            if let Some(title) = &t.title {
                parts.push(format!("\"{}\"", one_line(title, 60)));
            }
            if let Some(reason) = &t.waiting_reason {
                parts.push(reason.clone());
            }
            if t.pid.is_none() && !t.resolved {
                parts.push("asleep".to_string());
            }
            if let Some(branch) = &t.branch {
                parts.push(format!("branch {branch}"));
            }
            if let Some(pr) = &t.pr_url {
                parts.push(format!("PR {pr}"));
            }
            if let Some(account) = t.account_shown() {
                parts.push(format!("account {account}"));
            }
            if let Some(routine) = &t.routine {
                parts.push(format!("routine {routine}"));
            }
            if let Some(note) = &t.move_note {
                parts.push(note.clone());
            }
            if crate::registry::limited(t)
                && let Some(limit) = &t.limit_note
            {
                parts.push(format!("limited: {}", one_line(limit, 120)));
            }
            if t.cleaned_at.is_some() {
                parts.push("cleaned up".to_string());
            } else if let Some(why) = &t.cleanup_blocked {
                parts.push(format!("cleanup blocked: {why}"));
            }
            parts.push(ago(now, t.last_activity_at));
            out.push_str(&format!("    {}\n", parts.join(" · ")));
            if let Some(report) = t.last_report.as_deref().filter(|r| !r.trim().is_empty()) {
                out.push_str(&format!(
                    "      {}\n",
                    one_line(report, STATUS_PREVIEW_CHARS)
                ));
            }
        }
    }
    out
}
