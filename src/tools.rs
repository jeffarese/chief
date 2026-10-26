//! The coordinator's MCP tools (ARCHITECTURE §7) as plain functions. `mcp.rs` adapts them to
//! rmcp; tests call them directly against the fake `claude`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::claude::{AgentRow, BgSpawn, Claude, ClaudeError};
use crate::config::{EFFORTS, ProjectConfig};
use crate::db::{self, SessionRow};
use crate::state::{Group, Role};
use crate::text::{ago, one_line};
use crate::{generated, names, prompts, registry};

/// Preview length in `list_threads` (§7).
pub const LIST_PREVIEW_CHARS: usize = 300;
const NAME_ATTEMPTS: usize = 5;
const MAX_HISTORY: usize = 20;

/// Everything a tool touches, so tests can point it at temp dirs and the fake `claude`.
pub struct ToolCtx<'a> {
    pub conn: &'a Connection,
    pub config: &'a ProjectConfig,
    pub claude: &'a Claude,
    pub project_dir: &'a Path,
    /// Where trust is read: `~/.claude.json`, or `$CHIEF_CLAUDE_CONFIG`.
    pub trust_config: &'a Path,
    pub now: fn() -> i64,
    /// How long `start_thread` waits for a new session to show up in `claude agents`.
    pub resolve_timeout: Duration,
    /// How long `wake_thread` waits for a pid after `claude respawn`.
    pub wake_timeout: Duration,
    pub poll_interval: Duration,
}

impl ToolCtx<'_> {
    fn slug(&self) -> &str {
        &self.config.slug
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StartThreadArgs {
    /// A few words for the board, e.g. "Fix flaky checkout test". Also used in the thread's name.
    pub title: String,
    /// The thread's whole brief: it doesn't see your conversation. Objective, repository, what
    /// done means (tests, whether to open a pull request), constraints, what to report back.
    pub task: String,
    /// A repository name from the project's settings, or an absolute path. Default: the
    /// project's default repository.
    #[serde(default)]
    pub repo: Option<String>,
    /// Model override, e.g. "haiku" for small tasks. Default: the project's thread model.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort override: low, medium, high, xhigh or max. Default: the project's thread effort.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ListThreadsArgs {
    /// Also list resolved threads. Default false.
    #[serde(default)]
    pub include_resolved: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadReportArgs {
    /// The thread's name.
    pub name: String,
    /// How many of the latest reports to return, newest first. Default 1.
    #[serde(default)]
    pub history: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NameArgs {
    /// The thread's name.
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResolveThreadArgs {
    /// The thread's name.
    pub name: String,
    /// True reopens a resolved thread instead. Default false.
    #[serde(default)]
    pub reopen: Option<bool>,
}

/// A `list_threads` row.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadView {
    pub name: String,
    pub title: Option<String>,
    pub group: String,
    pub waiting_reason: Option<String>,
    /// No process: `wake_thread` before messaging it.
    pub asleep: bool,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub last_report_preview: Option<String>,
    pub last_activity: String,
    /// Its session, worktree and branch were removed; only its reports remain.
    pub cleaned: bool,
}

pub fn thread_view(row: &SessionRow, preview_chars: usize, now: i64) -> ThreadView {
    ThreadView {
        name: row.name.clone(),
        title: row.title.clone(),
        group: row.group_name.clone(),
        waiting_reason: row.waiting_reason.clone(),
        asleep: row.pid.is_none(),
        branch: row.branch.clone(),
        pr_url: row.pr_url.clone(),
        last_report_preview: row
            .last_report
            .as_deref()
            .map(|r| one_line(r, preview_chars)),
        last_activity: ago(now, row.last_activity_at),
        cleaned: row.cleaned_at.is_some(),
    }
}

/// Board order: group, then most recent activity first.
pub fn board_order(rows: &mut [SessionRow]) {
    rows.sort_by_key(|r| {
        (
            Group::parse(&r.group_name).unwrap_or(Group::Idle),
            -r.last_activity_at,
        )
    });
}

/// Polls `claude agents` and writes it into the project's rows; returns the snapshot.
pub fn refresh(ctx: &ToolCtx) -> Result<Vec<AgentRow>> {
    let taken_at = (ctx.now)();
    let rows = ctx.claude.agents()?;
    registry::sync_in(
        ctx.conn,
        ctx.slug(),
        &rows,
        taken_at,
        (ctx.now)(),
        &ctx.claude.config_dir,
    )?;
    Ok(rows)
}

pub fn start_thread(ctx: &ToolCtx, args: &StartThreadArgs) -> Result<Value> {
    let title = args.title.trim();
    if title.is_empty() {
        bail!("title is empty");
    }
    if args.task.trim().is_empty() {
        bail!("task is empty: it is the thread's whole brief");
    }
    let (repo_name, repo_path) = resolve_repo(ctx.config, args.repo.as_deref())?;
    if !crate::claude::is_trusted_in(ctx.trust_config, &repo_path)? {
        return Err(ClaudeError::NotTrusted(repo_path).into());
    }
    let model = pick(args.model.as_deref(), &ctx.config.threads.model);
    let effort = pick(args.effort.as_deref(), &ctx.config.threads.effort);
    if !EFFORTS.contains(&effort.as_str()) {
        bail!("effort {effort:?} is not one of {}", EFFORTS.join(", "));
    }

    let live = ctx
        .claude
        .agents()
        .context("checking `claude agents` for a free thread name")?;
    let name = free_name(ctx, title, &live)?;
    // From the settings, not whatever is on disk: the session reads these hooks at start.
    generated::rewrite(ctx.project_dir, ctx.config)?;
    let now = (ctx.now)();
    let mut row = SessionRow::new(&name, ctx.slug(), Role::Thread, now);
    row.title = Some(title.to_string());
    row.task = Some(args.task.clone());
    row.repo_name = Some(repo_name.clone());
    row.repo_path = Some(repo_path.to_string_lossy().into_owned());
    row.cwd = row.repo_path.clone();
    row.model = Some(model.clone());
    row.effort = Some(effort.clone());
    db::insert_session(ctx.conn, &row)?;

    let coordinator = names::coordinator_name(ctx.slug());
    let instructions = prompts::read_instructions(ctx.project_dir);
    let spec = BgSpawn {
        cwd: repo_path,
        name: name.clone(),
        model: model.clone(),
        effort: effort.clone(),
        permission_mode: ctx.config.threads.permission_mode.clone(),
        settings: generated::thread_settings_path(ctx.project_dir),
        mcp_config: None,
        add_dirs: vec![ctx.project_dir.join("memory")],
        disallowed_tools: vec![],
        append_system_prompt: prompts::thread(
            ctx.config,
            &coordinator,
            ctx.project_dir,
            &instructions,
        ),
        prompt: Some(task_prompt(&args.task)),
    };
    let (short_id, adopted, warning) = match ctx.claude.spawn_bg(&spec) {
        Ok(id) => (id, None, None),
        Err(e) => {
            let (agent, warning) = after_failed_spawn(ctx, &name, &live, &spec.cwd, e)?;
            let id = agent
                .id
                .clone()
                .context("an adopted session has a short id")?;
            (id, Some(agent), Some(warning))
        }
    };
    // Hooks from the new session may already be writing this row: read-modify-write in one
    // transaction.
    let set_ids = |agent: Option<&AgentRow>| {
        db::immediate(ctx.conn, |conn| {
            let mut row = db::get_session(conn, &name)?.context("thread row vanished")?;
            row.short_id = Some(short_id.clone());
            if let Some(a) = agent
                && row.session_id.is_none()
            {
                row.session_id = Some(a.session_id.clone());
                row.pid = a.pid;
            }
            row.updated_at = (ctx.now)();
            db::update_session(conn, &row)
        })
    };
    if let Some(agent) = &adopted {
        set_ids(Some(agent))?;
    } else {
        set_ids(None)?;
        // Not fatal: hooks match the session by title and sync matches it by short id.
        match ctx.claude.resolve_session(&short_id, ctx.resolve_timeout) {
            Ok(agent) => set_ids(Some(&agent))?,
            Err(e) => eprintln!("chief mcp: {e:#}"),
        }
    }
    let mut out = json!({
        "name": name,
        "short_id": short_id,
        "repo": repo_name,
        "model": model,
        "effort": effort,
        "next": format!(
            "Subscribe now: call SendMessage with exactly {{\"to\": \"{name}\", \"notify_when_idle\": true}} and no message key at all."
        ),
    });
    if let Some(w) = warning {
        out["warning"] = json!(w);
    }
    Ok(out)
}

/// A `claude --bg` in `cwd` that failed (a timeout, output chief doesn't recognise) may have
/// started the session anyway; deleting its row would leave it working unseen, and the guard
/// would refuse it. So: when `claude agents` lists it (by name, in `cwd`), return it with a
/// warning to adopt; when it doesn't, delete the row and return the spawn's error; when chief
/// can't tell, keep the row (its hooks match it by title if it did start) and say so.
fn after_failed_spawn(
    ctx: &ToolCtx,
    name: &str,
    before: &[AgentRow],
    cwd: &Path,
    err: anyhow::Error,
) -> Result<(AgentRow, String)> {
    // An untrusted folder starts nothing.
    let found = match err.downcast_ref::<ClaudeError>() {
        Some(ClaudeError::NotTrusted(_)) => Ok(None),
        None => ctx
            .claude
            .find_started(name, before, cwd, ctx.resolve_timeout),
    };
    match found {
        Ok(Some(agent)) => {
            let warning = format!(
                "`claude --bg` reported a failure, but {name} started anyway, so chief kept it. \
                 The failure: {err:#}"
            );
            eprintln!("chief mcp: {warning}");
            Ok((agent, warning))
        }
        Ok(None) => {
            if let Err(del) = db::delete_session(ctx.conn, name) {
                eprintln!("chief mcp: could not remove {name} after a failed spawn: {del:#}");
            }
            Err(err)
        }
        Err(lookup) => Err(err.context(format!(
            "chief couldn't check whether {name} started anyway ({lookup:#}), so its row is \
             kept: if it started, its hooks attach it; if it never shows up in list_threads, \
             resolve it"
        ))),
    }
}

fn pick(arg: Option<&str>, default: &str) -> String {
    arg.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// A task that starts with `-` would be read as a flag by the `claude` CLI.
fn task_prompt(task: &str) -> String {
    if task.trim_start().starts_with('-') {
        format!("Task: {task}")
    } else {
        task.to_string()
    }
}

/// `repo` as a repos[].name, an absolute path, or (None) the default repo → (name, path).
pub fn resolve_repo(config: &ProjectConfig, repo: Option<&str>) -> Result<(String, PathBuf)> {
    let known = || {
        let names: Vec<_> = config.repos.iter().map(|r| r.name.as_str()).collect();
        if names.is_empty() {
            "the project has no repositories".to_string()
        } else {
            format!("known repositories: {}", names.join(", "))
        }
    };
    let repo = repo.map(str::trim).filter(|r| !r.is_empty());
    let Some(repo) = repo.or(config.threads.default_repo.as_deref()) else {
        bail!(
            "no repo given and the project has no default repository; pass repo ({})",
            known()
        );
    };
    if let Some(r) = config.repo(repo) {
        return Ok((r.name.clone(), r.path.clone()));
    }
    let path = Path::new(repo);
    if !path.is_absolute() {
        bail!("{repo:?} is not a repository of this project ({})", known());
    }
    if !path.is_dir() {
        bail!("{repo} is not a folder");
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let name = config
        .repos
        .iter()
        .find(|r| r.path == path || r.path.canonicalize().ok().as_ref() == Some(&path))
        .map(|r| r.name.clone())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| path.display().to_string());
    Ok((name, path))
}

/// A thread name that neither the registry nor `claude agents` knows.
fn free_name(ctx: &ToolCtx, title: &str, live: &[AgentRow]) -> Result<String> {
    for _ in 0..NAME_ATTEMPTS {
        let name = names::thread_name(ctx.slug(), title, &names::random_suffix());
        let taken = live
            .iter()
            .any(|a| a.name.as_deref() == Some(name.as_str()))
            || db::get_session(ctx.conn, &name)?.is_some();
        if !taken {
            return Ok(name);
        }
    }
    bail!("could not find a free thread name for {title:?} in {NAME_ATTEMPTS} tries")
}

pub fn list_threads(ctx: &ToolCtx, args: &ListThreadsArgs) -> Result<Value> {
    let warning = refresh(ctx)
        .err()
        .map(|e| format!("`claude agents` failed, showing the last known state: {e:#}"));
    let include_resolved = args.include_resolved.unwrap_or(false);
    let mut rows: Vec<_> = db::list_sessions(ctx.conn, ctx.slug(), Some(Role::Thread))?
        .into_iter()
        .filter(|r| include_resolved || (!r.resolved && r.cleaned_at.is_none()))
        .collect();
    board_order(&mut rows);
    let now = (ctx.now)();
    let threads: Vec<_> = rows
        .iter()
        .map(|r| thread_view(r, LIST_PREVIEW_CHARS, now))
        .collect();
    let mut out = json!({"threads": threads});
    if let Some(w) = warning {
        out["warning"] = json!(w);
    }
    Ok(out)
}

/// The project's thread called `name`; the coordinator isn't a thread.
fn project_thread(ctx: &ToolCtx, name: &str) -> Result<SessionRow> {
    let name = crate::guard::strip_ref(name);
    match db::get_session(ctx.conn, name)? {
        Some(row) if row.project_slug == ctx.slug() && row.role == Role::Thread.as_str() => Ok(row),
        _ => bail!("{name} is not a thread of this project; call list_threads for the names"),
    }
}

pub fn read_report(ctx: &ToolCtx, args: &ReadReportArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let history = (args.history.unwrap_or(1) as usize).clamp(1, MAX_HISTORY);
    let now = (ctx.now)();
    let reports: Vec<_> = db::recent_reports(ctx.conn, &row.name, history)?
        .into_iter()
        .map(|r| json!({"at": ago(now, r.ts), "text": r.text}))
        .collect();
    let mut out = json!({
        "name": row.name,
        "title": row.title,
        "group": row.group_name,
        "reports": reports,
    });
    if reports.is_empty() {
        out["note"] = json!("No report yet: the thread hasn't finished a turn.");
    }
    Ok(out)
}

pub fn wake_thread(ctx: &ToolCtx, args: &NameArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let name = row.name.clone();
    if row.cleaned_at.is_some() {
        bail!(crate::guard::cleaned_message(&name));
    }
    let alive = |ctx: &ToolCtx| -> Result<bool> {
        refresh(ctx)?;
        Ok(db::get_session(ctx.conn, &name)?.is_some_and(|r| r.pid.is_some()))
    };
    if alive(ctx)? {
        return Ok(json!({"name": name, "alive": true, "respawned": false}));
    }
    // After the refresh, which undoes a removal verdict for a session that is listed again.
    let row = project_thread(ctx, &name)?;
    if row.removed_at.is_some() {
        bail!(crate::guard::removed_message(&name));
    }
    let short_id = row
        .short_id
        .with_context(|| format!("{name} was never started, so there is nothing to wake"))?;
    // A respawned session reads its hooks again.
    generated::rewrite(ctx.project_dir, ctx.config)?;
    ctx.claude.respawn(&short_id)?;
    let deadline = Instant::now() + ctx.wake_timeout;
    loop {
        std::thread::sleep(ctx.poll_interval);
        // A failed poll is retried until the deadline.
        if alive(ctx).unwrap_or(false) {
            return Ok(json!({"name": name, "alive": true, "respawned": true}));
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "name": name,
                "alive": false,
                "respawned": true,
                "note": format!(
                    "Respawned, but no process showed up within {}s; try list_threads shortly.",
                    ctx.wake_timeout.as_secs()
                ),
            }));
        }
    }
}

pub fn resolve_thread(ctx: &ToolCtx, args: &ResolveThreadArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let resolve = !args.reopen.unwrap_or(false);
    let row = registry::set_resolved(ctx.conn, &row.name, resolve, "coordinator", (ctx.now)())?;
    Ok(json!({"name": row.name, "resolved": row.resolved, "group": row.group_name}))
}
