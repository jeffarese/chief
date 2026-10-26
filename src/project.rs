//! Project lifecycle: init, up, down, status (ARCHITECTURE §2, §3, §7, §9).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::claude::{self, BgSpawn, Claude, ClaudeError, RESOLVE_TIMEOUT};
use crate::config::{ProjectConfig, Repo};
use crate::db::{self, SessionRow};
use crate::layout::{self, Closed, Opened};
use crate::mux::SessionRef;
use crate::mux::rex::Rex;
use crate::state::{Group, Role};
use crate::text::{ago, one_line};
use crate::{generated, names, paths, prompts, registry, tools};

const SLUG_CHARS: usize = 40;
const STATUS_PREVIEW_CHARS: usize = 160;
/// How long `up` waits for a respawned coordinator to get a process.
const RESPAWN_WAIT: Duration = Duration::from_secs(10);

const INSTRUCTIONS_TEMPLATE: &str = "<!--
Standing brief for this project. The coordinator and every new thread get this file's text:
conventions, commands to run, constraints, people to ask. Text inside comment markers like this
one is ignored.
-->
";

/// `<root>/projects/<slug>`, canonical when it exists: trust and session cwds use real paths
/// (macOS `/tmp` is `/private/tmp`).
pub fn project_dir(slug: &str) -> PathBuf {
    let dir = paths::project_dir(slug);
    dir.canonicalize().unwrap_or(dir)
}

/// Creates `<root>/projects/<slug>/` with project.toml, instructions.md, memory/MEMORY.md and
/// generated/*, registers the project, and offers to trust (or explains how to trust) every
/// folder Claude Code doesn't trust yet.
pub fn init(name: &str, goal: Option<&str>, repos: &[PathBuf], trust_prompt: bool) -> Result<()> {
    let claude = Claude::from_env();
    claude.check_version()?;
    let exe = generated::chief_exe()?;
    let (config, dir) = create(name, goal, repos, &exe, &generated::passthrough_env())?;
    println!(
        "Created project {:?} ({}) in {}",
        config.name,
        config.slug,
        dir.display()
    );

    let trust_config = claude::claude_config_path();
    let untrusted = untrusted_folders(&config, &dir, &trust_config)?;
    let interactive =
        trust_prompt && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !untrusted.is_empty() {
        println!(
            "\nClaude Code doesn't trust these folders yet, and background sessions refuse them:"
        );
        for folder in &untrusted {
            println!("  {}", folder.display());
        }
    }
    let mut still = Vec::new();
    for folder in &untrusted {
        if interactive && offer_trust(&claude, folder, &trust_config)? {
            continue;
        }
        still.push(folder);
    }
    if !still.is_empty() {
        println!("\nTo trust a folder, run Claude there once, accept the trust prompt, then quit:");
        for folder in still {
            println!(
                "  cd {} && claude",
                crate::text::shell_quote(&folder.to_string_lossy())
            );
        }
    }
    println!("\nNext: chief up {}", config.slug);
    Ok(())
}

/// Writes the project files and registers the project; nothing is left behind on failure.
pub fn create(
    name: &str,
    goal: Option<&str>,
    repos: &[PathBuf],
    exe: &Path,
    env: &[(String, String)],
) -> Result<(ProjectConfig, PathBuf)> {
    let name = name.trim();
    let slug = names::slugify(name, SLUG_CHARS);
    if slug.is_empty() {
        bail!("the project name {name:?} needs at least one letter or digit");
    }
    let goal = goal.map(str::trim).filter(|g| !g.is_empty());
    let config = ProjectConfig::new(name, &slug, goal, repo_entries(repos)?);
    config.validate()?;

    let conn = db::open(&paths::db())?;
    let dir = paths::project_dir(&slug);
    if dir.exists() || db::project_dir(&conn, &slug)?.is_some() {
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
    generated::write_all(dir, &config.slug, exe, env)
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

/// The project folder (always checked) and every repo Claude Code doesn't trust.
pub fn untrusted_folders(
    config: &ProjectConfig,
    dir: &Path,
    trust_config: &Path,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let folders =
        std::iter::once(dir.to_path_buf()).chain(config.repos.iter().map(|r| r.path.clone()));
    for folder in folders {
        if !claude::is_trusted_in(trust_config, &folder)? {
            out.push(folder);
        }
    }
    Ok(out)
}

/// Asks to open Claude in `folder`; returns whether it is trusted afterwards.
fn offer_trust(claude: &Claude, folder: &Path, trust_config: &Path) -> Result<bool> {
    print!(
        "\nOpen Claude in {} now to trust it? [Y/n] ",
        folder.display()
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes") {
        return Ok(false);
    }
    println!("Accept the trust prompt, then quit with /exit or Ctrl+C twice.");
    claude.run_interactive(folder)?;
    let trusted = claude::is_trusted_in(trust_config, folder)?;
    if trusted {
        println!("Trusted {}.", folder.display());
    } else {
        println!("{} is still not trusted.", folder.display());
    }
    Ok(trusted)
}

/// What `up` did to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorAction {
    Started,
    Respawned,
    Running,
}

/// Starts or respawns the coordinator, then opens the Rex layout (§9). Without Rex it says how
/// to attach and where to run the board instead.
pub fn up(slug: &str) -> Result<()> {
    let claude = Claude::from_env();
    claude.check_version()?;
    let dir = project_dir(slug);
    let config = ProjectConfig::load(&dir.join("project.toml"))
        .with_context(|| format!("no usable chief project {slug:?}; create it with chief init"))?;
    let exe = generated::chief_exe()?;
    if generated::in_cargo_target(&exe) {
        eprintln!(
            "warning: chief runs from {}, inside a cargo target/ folder: `cargo clean` would \
             disable chief's hooks and message guard. Install with `cargo install --path .`.",
            exe.display()
        );
    }
    let env = generated::passthrough_env();
    generated::write_all(&dir, slug, &exe, &env)?;
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        db::register_project(&conn, slug, &dir, db::now_ms())?;
    }
    let trust_config = claude::claude_config_path();
    let (row, action) = ensure_coordinator(&conn, &claude, &config, &dir, &trust_config)?;
    let verb = match action {
        CoordinatorAction::Started => "started",
        CoordinatorAction::Respawned => "respawned",
        CoordinatorAction::Running => "already running",
    };
    println!("Coordinator {} {verb}.", row.name);

    let label = layout::session_label(slug);
    let ours = db::get_rex_session(&conn, slug)?.map(SessionRef);
    match layout::open(&Rex::from_env(), slug, &dir, &exe, &env, ours.as_ref()) {
        Ok(opened) => {
            db::set_rex_session(&conn, slug, Some(&opened.session.0))?;
            let what = match opened.opened {
                Opened::Created => "opened",
                Opened::Repaired => "repaired",
                Opened::Reused => "already open",
            };
            println!("Rex session {label} {what}.");
            if !opened.shown {
                println!(
                    "Rex didn't switch to it (that needs exactly one Rex app connected). Pick \
                     {label} in the app, or run: rex -C <app> do session.select session_id={}",
                    opened.session.0
                );
            }
        }
        Err(e) => {
            println!("No Rex layout: {e:#}");
            if let Some(short_id) = &row.short_id {
                println!("Talk to it: claude attach {short_id}");
            }
            println!("Board, in another terminal: chief board --project {slug}");
        }
    }
    Ok(())
}

/// §7 "Coordinator spawn": start it when it has no row or no short id, `claude respawn` it
/// when it has no live process, otherwise leave it alone.
pub fn ensure_coordinator(
    conn: &Connection,
    claude: &Claude,
    config: &ProjectConfig,
    dir: &Path,
    trust_config: &Path,
) -> Result<(SessionRow, CoordinatorAction)> {
    let slug = config.slug.as_str();
    let name = names::coordinator_name(slug);
    let existing = db::get_coordinator(conn, slug)?;
    if let Some(row) = existing.as_ref()
        && let Some(short_id) = row.short_id.clone()
    {
        let taken_at = db::now_ms();
        let snapshot = claude.agents()?;
        registry::sync(conn, slug, &snapshot, taken_at, db::now_ms())?;
        let row = db::get_session(conn, &row.name)?.context("coordinator row vanished")?;
        if row.pid.is_some() {
            return Ok((row, CoordinatorAction::Running));
        }
        claude.respawn(&short_id)?;
        let row = wait_for_pid(conn, claude, slug, &row.name, RESPAWN_WAIT)?;
        return Ok((row, CoordinatorAction::Respawned));
    }

    if !claude::is_trusted_in(trust_config, dir)? {
        return Err(ClaudeError::NotTrusted(dir.to_path_buf()).into());
    }
    let live = claude.agents()?;
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
    let short_id = match claude.spawn_bg(&spec) {
        Ok(id) => id,
        Err(e) => {
            if inserted {
                let _ = db::delete_session(conn, &name);
            }
            return Err(e);
        }
    };
    let resolved = claude.resolve_session(&short_id, RESOLVE_TIMEOUT);
    if let Err(e) = &resolved {
        eprintln!("warning: {e:#}");
    }
    let row = db::immediate(conn, |conn| {
        let mut row = db::get_session(conn, &name)?.context("coordinator row vanished")?;
        row.short_id = Some(short_id.clone());
        if let Ok(agent) = &resolved
            && row.session_id.is_none()
        {
            row.session_id = Some(agent.session_id.clone());
            row.pid = agent.pid;
        }
        row.updated_at = db::now_ms();
        db::update_session(conn, &row)?;
        Ok(row)
    })?;
    Ok((row, CoordinatorAction::Started))
}

/// Polls `claude agents` until `name` has a pid or `timeout` passes; returns the row either way.
pub fn wait_for_pid(
    conn: &Connection,
    claude: &Claude,
    slug: &str,
    name: &str,
    timeout: Duration,
) -> Result<SessionRow> {
    let deadline = Instant::now() + timeout;
    loop {
        let taken_at = db::now_ms();
        if let Ok(snapshot) = claude.agents() {
            registry::sync(conn, slug, &snapshot, taken_at, db::now_ms())?;
        }
        let row = db::get_session(conn, name)?.with_context(|| format!("{name} vanished"))?;
        if row.pid.is_some() || Instant::now() >= deadline {
            return Ok(row);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Closes the Rex layout and stops the coordinator; with `stop_threads`, every live thread too.
/// Stopping keeps conversations and worktrees (`claude stop`, never `claude rm`).
pub fn down(slug: &str, stop_threads: bool) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        bail!("no chief project {slug:?}");
    }
    let mut failed = 0;
    let label = layout::session_label(slug);
    let ours = db::get_rex_session(&conn, slug)?.map(SessionRef);
    match layout::close(&Rex::from_env(), ours.as_ref()) {
        Ok(Closed::Killed(_)) => {
            db::set_rex_session(&conn, slug, None)?;
            println!("Closed Rex session {label}.");
        }
        Ok(Closed::Blocks(_)) => println!(
            "Closed chief's blocks in Rex session {label}; it holds other blocks too, so it stays \
             open."
        ),
        Ok(Closed::Nothing) => {
            db::set_rex_session(&conn, slug, None)?;
            println!("No Rex session {label} was open.");
        }
        Err(e) => {
            failed += 1;
            println!("Rex session {label} not closed: {e:#}");
        }
    }
    db::set_pane_showing(&conn, slug, None)?;
    // The next `chief up` starts on the coordinator: a pane that went back to the thread it last
    // showed would wake it right after `--stop-threads` stopped it.
    db::set_pane_target(&conn, slug, None)?;

    let claude = Claude::from_env();
    let taken_at = db::now_ms();
    match claude.agents() {
        Ok(snapshot) => registry::sync(&conn, slug, &snapshot, taken_at, db::now_ms())?,
        Err(e) => eprintln!("warning: `claude agents` failed, using the last known state: {e:#}"),
    }
    let mut left_running = 0;
    for row in db::list_sessions(&conn, slug, None)? {
        let coordinator = row.role == Role::Coordinator.as_str();
        let live = row.pid.is_some() && row.cleaned_at.is_none();
        let Some(short_id) = row.short_id.as_deref().filter(|_| live) else {
            if coordinator {
                println!("Coordinator {} isn't running.", row.name);
            }
            continue;
        };
        if !coordinator && !stop_threads {
            left_running += 1;
            continue;
        }
        let what = if coordinator { "coordinator" } else { "thread" };
        match claude.stop(short_id) {
            Ok(()) => println!("Stopped {what} {}.", row.name),
            Err(e) => {
                failed += 1;
                println!("Could not stop {what} {}: {e:#}", row.name);
            }
        }
    }
    if left_running > 0 {
        println!(
            "{left_running} thread{} still running; chief down {slug} --stop-threads stops them.",
            if left_running == 1 { " is" } else { "s are" }
        );
    }
    if failed > 0 {
        bail!("chief down {slug} didn't finish cleanly");
    }
    Ok(())
}

/// Prints the board as text, grouped as on the board: one project, or all of them.
pub fn status(slug: Option<&str>) -> Result<()> {
    let conn = db::open(&paths::db())?;
    let projects = match slug {
        Some(slug) => {
            if db::project_dir(&conn, slug)?.is_none() {
                bail!("no chief project {slug:?}");
            }
            vec![slug.to_string()]
        }
        None => db::list_projects(&conn)?
            .into_iter()
            .map(|(slug, _)| slug)
            .collect(),
    };
    if projects.is_empty() {
        println!("No projects yet. Create one with chief init <name>.");
        return Ok(());
    }
    let taken_at = db::now_ms();
    let snapshot = match Claude::from_env().agents() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: `claude agents` failed, showing the last known state: {e:#}");
            None
        }
    };
    let mut out = String::new();
    for slug in &projects {
        if let Some(snapshot) = &snapshot {
            registry::sync(&conn, slug, snapshot, taken_at, db::now_ms())?;
        }
        let rows = db::list_sessions(&conn, slug, None)?;
        out.push_str(&render_status(slug, &rows, db::now_ms()));
    }
    print!("{out}");
    Ok(())
}

/// One project's board as text.
pub fn render_status(slug: &str, rows: &[SessionRow], now: i64) -> String {
    let mut out = format!("{slug}\n");
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
                .map(|id| format!(", claude attach {id}"))
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
