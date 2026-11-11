//! The coordinator's MCP tools (ARCHITECTURE §7) as plain functions. `mcp.rs` adapts them to
//! rmcp; tests call them directly against the fake `claude`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{self, Listings};
use crate::agp::{Agp, RunOptions};
use crate::claude::{AGENTS_TIMEOUT, AgentRow, BgSpawn, Claude, ClaudeError, Via, same_account};
use crate::config::{EFFORTS, OnLimit, ProjectConfig, Runner};
use crate::db::{self, SessionRow};
use crate::state::{Group, Role};
use crate::text::{ago, one_line};
use crate::{generated, names, placement, prompts, registry};

/// Preview length in `list_threads` (§7).
pub const LIST_PREVIEW_CHARS: usize = 300;
const NAME_ATTEMPTS: usize = 5;
const MAX_HISTORY: usize = 20;

/// Everything a tool touches, so tests can point it at temp dirs and the fake `claude`.
pub struct ToolCtx<'a> {
    pub conn: &'a Connection,
    pub config: &'a ProjectConfig,
    /// `claude` in the project's account.
    pub claude: &'a Claude,
    /// Where `start_thread` finds the accounts a thread may run in (§7).
    pub agp: &'a Agp,
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

/// How long `wake_thread` waits for a pid outside tests, and how often it looks.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(20);
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Runs `f` with the context of a project as it is now: its project.toml read afresh, its own
/// database connection, `claude` in the project's account whatever this process's environment
/// names (§3), and agp from the environment. What `chief mcp`, `chief serve` and `chief routine`
/// give the tools.
pub fn with_project<T>(project: &str, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
    let dir = crate::project::project_dir(project);
    let config = ProjectConfig::load(&dir.join("project.toml"))?;
    let conn = db::open(&crate::paths::db())?;
    let claude = Claude::for_account(config.claude_config_dir.clone());
    let trust_config = claude.trust_config();
    let agp = Agp::from_env();
    let ctx = ToolCtx {
        conn: &conn,
        config: &config,
        claude: &claude,
        agp: &agp,
        project_dir: &dir,
        trust_config: &trust_config,
        now: db::now_ms,
        resolve_timeout: crate::claude::RESOLVE_TIMEOUT,
        wake_timeout: WAKE_TIMEOUT,
        poll_interval: POLL_INTERVAL,
    };
    f(&ctx)
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
    /// The Claude account to run it in, by agp id or label (e.g. "PB2"), only when the user
    /// asks for one; it must be in the project's pool. Default: the pool account with the most
    /// room now.
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MoveThreadArgs {
    /// The thread's name.
    pub name: String,
    /// The account to move it to, by agp id or label (e.g. "PB4"); it must be in the project's
    /// pool. Default: the other pool account with the most room now.
    #[serde(default)]
    pub account: Option<String>,
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
    /// Its subagents mid-turn and its background commands running (§5 "Background work"),
    /// e.g. `1 subagent`; null when none. Its own turn may have ended: it isn't done yet.
    pub running: Option<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub last_report_preview: Option<String>,
    pub last_activity: String,
    /// Its session, worktree and branch were removed; only its reports remain.
    pub cleaned: bool,
    /// The agp account `start_thread` put it in (its label); null: the project's account.
    pub account: Option<String>,
    /// Where to SendMessage it: its name, or for a thread in another account its socket
    /// address (it changes when the thread is woken). Null while that thread has no process,
    /// and once it is cleaned up.
    pub address: Option<String>,
    /// The usage limit it waits on (group `limited`), e.g. "You've hit your 5-hour limit ·
    /// resets 6:10am (Europe/Madrid)".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// How it last moved account (§5 "Limits and moves"), e.g. "moved from PB2 to PB4 after
    /// the 5-hour limit".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<String>,
}

/// `address` is `tools::address` for the row.
pub fn thread_view(
    row: &SessionRow,
    preview_chars: usize,
    now: i64,
    address: Option<String>,
) -> ThreadView {
    ThreadView {
        name: row.name.clone(),
        title: row.title.clone(),
        group: row.group_name.clone(),
        waiting_reason: row.waiting_reason.clone(),
        asleep: row.pid.is_none(),
        running: registry::background_work(row),
        branch: row.branch.clone(),
        pr_url: row.pr_url.clone(),
        last_report_preview: row
            .last_report
            .as_deref()
            .map(|r| one_line(r, preview_chars)),
        last_activity: ago(now, row.last_activity_at),
        cleaned: row.cleaned_at.is_some(),
        account: row.account_shown().map(str::to_string),
        address,
        limit: row.limit_note.clone().filter(|_| registry::limited(row)),
        moved: row.move_note.clone(),
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

/// Polls `claude agents` in every account the project uses and writes each into its rows. A
/// failed account leaves its rows as they were (`Listings::error`).
pub fn refresh(ctx: &ToolCtx) -> Result<Listings> {
    accounts::refresh_with(
        ctx.conn,
        ctx.slug(),
        ctx.claude,
        ctx.agp,
        AGENTS_TIMEOUT,
        ctx.now,
    )
}

pub fn start_thread(ctx: &ToolCtx, args: &StartThreadArgs) -> Result<Value> {
    start_thread_unless(ctx, args, &|| false)
}

/// What makes a thread a routine's run (§13 "A run"): its row names the routine from the
/// start, it can read and write the routine's folder, and it gets the run tools
/// (`record_value`, `report_outcome`) with the settings that allow them.
#[derive(Debug, Clone)]
pub struct RunOf {
    pub routine: String,
    /// The routine's own folder, with its notes.
    pub folder: PathBuf,
}

/// The folder threads work in when the project has no repositories: a project that isn't
/// about code (§3 "Workspace"). Made on first use.
pub fn workspace(project_dir: &Path) -> PathBuf {
    project_dir.join("workspace")
}

/// `resolve_repo`, or the project's workspace when it has no repositories and none is named.
pub fn resolve_place(ctx: &ToolCtx, repo: Option<&str>) -> Result<(String, PathBuf)> {
    let named = repo.map(str::trim).is_some_and(|r| !r.is_empty());
    if named || !ctx.config.repos.is_empty() {
        return resolve_repo(ctx.config, repo);
    }
    let dir = workspace(ctx.project_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.canonicalize().unwrap_or(dir);
    Ok(("workspace".to_string(), dir))
}

/// `start_thread` that starts nothing once `cancelled()` is true: the client gave up the call
/// (§7), so nobody would get the thread's address or its `next`.
pub fn start_thread_unless(
    ctx: &ToolCtx,
    args: &StartThreadArgs,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    start_thread_as(ctx, args, None, cancelled)
}

/// `start_thread_unless`, for a plain thread (`run` None) or a routine's run.
pub fn start_thread_as(
    ctx: &ToolCtx,
    args: &StartThreadArgs,
    run_of: Option<&RunOf>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    let title = args.title.trim();
    if title.is_empty() {
        bail!("title is empty");
    }
    if args.task.trim().is_empty() {
        bail!("task is empty: it is the thread's whole brief");
    }
    let (repo_name, repo_path) = resolve_place(ctx, args.repo.as_deref())?;
    let model = pick(args.model.as_deref(), &ctx.config.threads.model);
    let effort = pick(args.effort.as_deref(), &ctx.config.threads.effort);
    if !EFFORTS.contains(&effort.as_str()) {
        bail!("effort {effort:?} is not one of {}", EFFORTS.join(", "));
    }
    let inputs = placement::Inputs {
        threads: &ctx.config.threads,
        agp: ctx.agp,
        project: ctx.claude,
        project_trust: ctx.trust_config,
    };
    let placed = placement::place(&inputs, &repo_path, args.account.as_deref())?;
    let account = placed.account(ctx.claude.account.as_deref());
    // Every call about the new session runs in its account (§3).
    let claude = ctx.claude.in_account(account.as_deref());

    // Free in the thread's account, where it runs, and in the project's, where the coordinator
    // messages same-account threads by name.
    let mut where_ = vec![ctx.claude.account.clone()];
    if !same_account(account.as_deref(), ctx.claude.account.as_deref()) {
        where_.push(account.clone());
    }
    let listings = accounts::list(ctx.claude, &where_, AGENTS_TIMEOUT, ctx.now);
    let mut taken = Vec::new();
    for l in &listings.all {
        taken.extend_from_slice(
            l.agents()
                .context("checking `claude agents` for a free thread name")?,
        );
    }
    let live = listings
        .of(account.as_deref())
        .context("the thread's account was listed")?
        .agents()?
        .to_vec();
    let name = free_name(ctx, title, &taken)?;
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
    row.routine = run_of.map(|r| r.routine.clone());
    if let Some(c) = &placed.chosen {
        row.account_id = Some(c.id.clone());
        row.account_label = Some(c.label.clone());
        row.account_dir = c.dir.as_ref().map(|d| d.to_string_lossy().into_owned());
    }
    // Through agp, which moves it to another account when its own hits a usage limit (§5
    // "Limits and moves"): launched by agp in the account placement chose, or, in the project's
    // account (no agp profile named), launched by claude and handed to agp to watch.
    let run = RunOptions::from_settings(&ctx.config.threads);
    let agp_runs = ctx.config.threads.runner == Runner::Agp && ctx.agp.supports_bg();
    let via = match placed.chosen.as_ref().filter(|_| agp_runs) {
        Some(c) => Via::Agp {
            agp: ctx.agp,
            id: &c.id,
            run: &run,
        },
        None => Via::Claude,
    };
    if matches!(via, Via::Agp { .. }) {
        row.runner = Some(db::RUNNER_AGP.to_string());
    }
    db::insert_session(ctx.conn, &row)?;

    let coordinator = names::coordinator_name(ctx.slug());
    let instructions = prompts::read_instructions(ctx.project_dir);
    let spec = BgSpawn {
        cwd: repo_path,
        name: name.clone(),
        model: model.clone(),
        effort: effort.clone(),
        permission_mode: ctx.config.threads.permission_mode.clone(),
        settings: match run_of {
            Some(_) => generated::run_settings_path(ctx.project_dir),
            None => generated::thread_settings_path(ctx.project_dir),
        },
        mcp_config: match run_of {
            Some(of) => Some(generated::write_run_mcp(
                ctx.project_dir,
                ctx.config,
                &of.routine,
            )?),
            None => None,
        },
        add_dirs: std::iter::once(ctx.project_dir.join("memory"))
            .chain(run_of.map(|r| r.folder.clone()))
            .collect(),
        disallowed_tools: vec![],
        append_system_prompt: prompts::thread(
            ctx.config,
            &coordinator,
            ctx.project_dir,
            &instructions,
            if agp_runs && run.on_limit == OnLimit::Switch {
                prompts::Reach::MayMove
            } else if same_account(account.as_deref(), ctx.claude.account.as_deref()) {
                prompts::Reach::Same
            } else {
                prompts::Reach::Elsewhere
            },
        ),
        prompt: Some(task_prompt(&args.task)),
    };
    // The last moment the call can still be given up without leaving a thread nobody follows.
    if cancelled() {
        if let Err(e) = db::delete_session(ctx.conn, &name) {
            eprintln!("chief mcp: could not remove {name} after a cancelled start: {e:#}");
        }
        bail!("start_thread was cancelled before {name} started; nothing was started");
    }
    let mut warnings = Vec::new();
    let (short_id, adopted) = match claude.spawn_bg_via(&spec, &via) {
        Ok(id) => (Some(id), None),
        Err(e) => {
            let (kept, warning) = after_failed_spawn(ctx, &claude, &name, &live, &spec.cwd, e)?;
            warnings.push(warning);
            match kept {
                Kept::Listed(agent) => (agent.id.clone(), Some(agent)),
                Kept::Attached => (None, None),
            }
        }
    };
    // From here on the session runs, so nothing may fail the call: a coordinator told it failed
    // would start the thread again. Hooks match it by name until its ids are stored.
    let mut stored = true;
    let mut store_ids = |short_id: &str, agent: Option<&AgentRow>| {
        // Hooks from the new session may already be writing this row: read-modify-write in
        // one transaction.
        let written = db::immediate(ctx.conn, |conn| {
            let mut row = db::get_session(conn, &name)?.context("thread row vanished")?;
            row.short_id = Some(short_id.to_string());
            if let Some(a) = agent
                && row.session_id.is_none()
            {
                row.session_id = Some(a.session_id.clone());
                row.pid = a.pid;
            }
            row.updated_at = (ctx.now)();
            db::update_session(conn, &row)
        });
        if let Err(e) = written {
            eprintln!("chief mcp: {name} started, but storing its ids failed: {e:#}");
            stored = false;
        }
    };
    let started = match (&short_id, adopted) {
        (Some(id), Some(agent)) => {
            store_ids(id, Some(&agent));
            Some(agent)
        }
        (Some(id), None) => {
            store_ids(id, None);
            // Not fatal: hooks match the session by title and sync matches it by short id.
            match claude.resolve_session(id, ctx.resolve_timeout) {
                Ok(agent) => {
                    store_ids(id, Some(&agent));
                    Some(agent)
                }
                Err(e) => {
                    eprintln!("chief mcp: {e:#}");
                    None
                }
            }
        }
        (None, _) => None,
    };
    if agp_runs
        && matches!(via, Via::Claude)
        && let Some(id) = &short_id
    {
        watch_after_start(ctx, &name, id, &run);
    }
    if !stored {
        warnings.push(format!(
            "{name} started, but chief couldn't store its session ids (the database was busy or \
             failing); its hooks store them by its name when they next run. It is in \
             list_threads: don't start it again."
        ));
    }
    // What was stored, else what chief knows.
    let row = match db::get_session(ctx.conn, &name) {
        Ok(Some(stored)) => stored,
        _ => SessionRow {
            short_id: short_id.clone(),
            session_id: started.as_ref().map(|a| a.session_id.clone()),
            pid: started.as_ref().and_then(|a| a.pid),
            ..row
        },
    };
    let address = address(ctx, &row, started.as_slice());
    let mut out = json!({
        "name": name,
        "short_id": short_id,
        "repo": repo_name,
        "model": model,
        "effort": effort,
        "account": row.account_shown(),
        "runner": if row.via_agp() { "agp" } else { "claude" },
        "address": address,
        "next": subscribe("Subscribe now", &row, address.as_deref()),
    });
    if let Some(note) = placed.note {
        out["account_note"] = json!(note);
    }
    if !warnings.is_empty() {
        out["warning"] = json!(warnings.join(" "));
    }
    Ok(out)
}

/// Hands a thread `claude` started in the project's account to agp to watch for limits (§5
/// "Limits and moves"), and marks it agp's once agp took it. Never fails the start: a thread agp
/// doesn't watch is one chief tells the coordinator about when it hits a limit.
fn watch_after_start(ctx: &ToolCtx, name: &str, short_id: &str, run: &RunOptions) {
    if let Err(e) = ctx.agp.watch(short_id, None, run) {
        eprintln!("chief mcp: agp couldn't watch {name} for limits: {e:#}");
        return;
    }
    let marked = db::immediate(ctx.conn, |conn| {
        let Some(mut row) = db::get_session(conn, name)? else {
            return Ok(());
        };
        row.runner = Some(db::RUNNER_AGP.to_string());
        db::update_session(conn, &row)
    });
    if let Err(e) = marked {
        eprintln!("chief mcp: couldn't mark {name} as watched by agp: {e:#}");
    }
}

/// Where the coordinator sends `row`'s messages (§6, §7): its name when it runs in the
/// project's account; else, since names don't reach across accounts (§1), the socket address of
/// its live process in `agents` (its own account's fresh listing), which changes whenever it is
/// respawned and so is never stored. None for a thread with no session left or no live
/// process listed.
pub fn address(ctx: &ToolCtx, row: &SessionRow, agents: &[AgentRow]) -> Option<String> {
    if row.cleaned_at.is_some() || row.removed_at.is_some() {
        return None;
    }
    let project = ctx.claude.account.as_deref();
    if same_account(row.account(project).as_deref(), project) {
        return Some(row.name.clone());
    }
    let own = agents.iter().find(|a| match &row.session_id {
        Some(sid) => &a.session_id == sid,
        None => a.id.is_some() && a.id == row.short_id,
    })?;
    own.pid.map(crate::guard::uds_address)
}

/// The `next` line: subscribe to `row`'s idle notices at its address.
fn subscribe(lead: &str, row: &SessionRow, address: Option<&str>) -> String {
    let Some(address) = address else {
        return format!(
            "{} runs in another Claude account and isn't listed there with a process yet; call \
             list_threads shortly for its address, then subscribe: SendMessage with exactly \
             {{\"to\": \"<address>\", \"notify_when_idle\": true}} and no message key at all.",
            row.name
        );
    };
    let line = format!(
        "{lead}: call SendMessage with exactly {{\"to\": \"{address}\", \"notify_when_idle\": \
         true}} and no message key at all."
    );
    if address == row.name {
        return line;
    }
    let account = row.account_shown().unwrap_or("another account");
    format!(
        "{line} {} runs in another Claude account ({account}), where names don't reach: \
         message it at this address, not by name. The address changes when it is woken. \
         Idle notices do work at such an address, whatever Claude Code says about not \
         knowing. From there it can't message you first, only reply to a message of yours.",
        row.name
    )
}

/// What a failed `claude --bg` started anyway, which chief keeps.
enum Kept {
    /// `claude agents` lists it: adopt its ids.
    Listed(AgentRow),
    /// A hook of the session already stored its session id in the row: it started, though no
    /// listing showed it yet.
    Attached,
}

/// A `claude --bg` in `cwd` that failed (a timeout, output chief doesn't recognise) may have
/// started the session anyway; deleting its row would leave it working unseen, and the guard
/// would refuse it. So: when `claude agents` lists it (by name, in `cwd`), return it with a
/// warning to adopt; when a hook already attached the row, keep it with a warning; otherwise
/// delete the row and return the spawn's error; when chief can't tell, keep the row (its hooks
/// match it by title if it did start) and say so.
fn after_failed_spawn(
    ctx: &ToolCtx,
    claude: &Claude,
    name: &str,
    before: &[AgentRow],
    cwd: &Path,
    err: anyhow::Error,
) -> Result<(Kept, String)> {
    // An untrusted folder starts nothing.
    let found = match err.downcast_ref::<ClaudeError>() {
        Some(ClaudeError::NotTrusted(_)) => Ok(None),
        None => claude.find_started(name, before, cwd, ctx.resolve_timeout),
    };
    match found {
        Ok(Some(agent)) => {
            let warning = format!(
                "`claude --bg` reported a failure, but {name} started anyway, so chief kept it. \
                 The failure: {err:#}"
            );
            eprintln!("chief mcp: {warning}");
            Ok((Kept::Listed(agent), warning))
        }
        Ok(None) => match db::delete_unattached_session(ctx.conn, name) {
            Ok(true) => Err(err),
            Ok(false) => {
                let warning = format!(
                    "`claude --bg` reported a failure, and `claude agents` doesn't list {name} \
                     yet, but its hooks already ran, so it started and chief kept it. The \
                     failure: {err:#}"
                );
                eprintln!("chief mcp: {warning}");
                Ok((Kept::Attached, warning))
            }
            Err(read) => Err(err.context(format!(
                "chief couldn't check {name}'s row to see whether it started anyway ({read:#}), \
                 so the row is kept: if it started, its hooks attach it; if it never shows up \
                 in list_threads, resolve it"
            ))),
        },
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
    let refreshed = refresh(ctx);
    let warning = match &refreshed {
        Ok(listings) => listings.error(),
        Err(e) => Some(format!("{e:#}")),
    }
    .map(|e| format!("`claude agents` failed, showing the last known state: {e}"));
    let listings = refreshed.ok();
    let address_of = |row: &SessionRow| {
        let agents = listings
            .as_ref()
            .and_then(|l| l.of_row(row))
            .and_then(|l| l.agents.as_deref().ok())
            .unwrap_or_default();
        address(ctx, row, agents)
    };
    let include_resolved = args.include_resolved.unwrap_or(false);
    let mut rows: Vec<_> = db::list_sessions(ctx.conn, ctx.slug(), Some(Role::Thread))?
        .into_iter()
        .filter(|r| include_resolved || (!r.resolved && r.cleaned_at.is_none()))
        .collect();
    board_order(&mut rows);
    let now = (ctx.now)();
    let threads: Vec<_> = rows
        .iter()
        .map(|r| thread_view(r, LIST_PREVIEW_CHARS, now, address_of(r)))
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
    // §5 "Background work": its idle notice came from a turn that ended while these run.
    if let Some(work) = registry::background_work(&row) {
        out["note"] = json!(format!(
            "Not finished: {work} still running. The thread may have ended its own turn while \
             they run; it reports to you when they finish."
        ));
        out["running"] = json!(work);
    }
    Ok(out)
}

pub fn wake_thread(ctx: &ToolCtx, args: &NameArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let name = row.name.clone();
    if row.cleaned_at.is_some() {
        bail!(crate::guard::cleaned_message(&name));
    }
    // Its own account's `claude agents` is the one that can see it (the row read again each
    // time: agp may have moved it); Some(row) once it has a process, with its address from
    // that fresh listing (§7).
    let alive =
        |ctx: &ToolCtx, timeout: Duration| -> Result<Option<(SessionRow, Option<String>)>> {
            let row = db::get_session(ctx.conn, &name)?.context("thread row vanished")?;
            let listing =
                accounts::refresh_row_with(ctx.conn, ctx.claude, ctx.agp, &row, timeout, ctx.now)?;
            let now = db::get_session(ctx.conn, &name)?.context("thread row vanished")?;
            if now.pid.is_none() {
                return Ok(None);
            }
            let address = address(ctx, &now, listing.agents()?);
            Ok(Some((now, address)))
        };
    if let Some((_, address)) = alive(ctx, AGENTS_TIMEOUT)? {
        return Ok(json!({"name": name, "alive": true, "respawned": false, "address": address}));
    }
    // After the refresh, which undoes a removal verdict for a session that is listed again.
    let row = project_thread(ctx, &name)?;
    if row.removed_at.is_some() {
        bail!(crate::guard::removed_message(&name));
    }
    // agp may be moving it to another account (§5 "Limits and moves"): a respawn now would start
    // it again where agp just stopped it. Once moved, it may run already.
    let project = ctx.claude.account.as_deref();
    let settled = crate::moves::settle(
        ctx.conn,
        ctx.agp,
        &row,
        project,
        crate::moves::SETTLE_TIMEOUT,
        ctx.now,
    )?;
    let moved = !same_account(
        settled.account(project).as_deref(),
        row.account(project).as_deref(),
    );
    if moved && let Some((now, address)) = alive(ctx, AGENTS_TIMEOUT)? {
        let mut out = json!({"name": name, "alive": true, "respawned": false, "address": address});
        moved_out(&mut out, &now, address.as_deref());
        return Ok(out);
    }
    let row = settled;
    let short_id = row
        .short_id
        .clone()
        .with_context(|| format!("{name} was never started, so there is nothing to wake"))?;
    // A respawned session reads its hooks again.
    generated::rewrite(ctx.project_dir, ctx.config)?;
    // Before the respawn: agp writes again the settings it merged with ours, which the
    // respawned session reads, and brings back a watcher that ended (a reboot).
    if let Err(e) = crate::moves::rearm(ctx.agp, &row, &ctx.config.threads) {
        eprintln!("chief mcp: agp couldn't watch {name} again: {e:#}");
    }
    ctx.claude.for_row(&row).respawn(&short_id)?;
    let deadline = Instant::now() + ctx.wake_timeout;
    let left = || deadline.saturating_duration_since(Instant::now());
    loop {
        std::thread::sleep(ctx.poll_interval.min(left()));
        // A poll gets only the time left, so a slow `claude agents` can't run past the
        // deadline; a failed one is retried until then.
        let poll = left().min(AGENTS_TIMEOUT);
        let found = if poll.is_zero() {
            None
        } else {
            alive(ctx, poll).unwrap_or(None)
        };
        if let Some((now, address)) = found {
            let mut out =
                json!({"name": name, "alive": true, "respawned": true, "address": address});
            if moved {
                moved_out(&mut out, &now, address.as_deref());
            } else if address.as_deref() != Some(name.as_str()) {
                // A socket address names the old process: the subscription must follow.
                out["next"] = json!(subscribe("Subscribe again", &now, address.as_deref()));
            }
            return Ok(out);
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

/// What a reply says about a thread agp moved to another account: where it went, and the
/// subscription at its new address, whatever the old one was.
fn moved_out(out: &mut Value, row: &SessionRow, address: Option<&str>) {
    if let Some(note) = &row.move_note {
        out["moved"] = json!(note);
    }
    out["account"] = json!(row.account_shown());
    out["next"] = json!(subscribe("It moved, so subscribe again", row, address));
}

/// `claude stop` for a thread: its process ends, and the turn it was in with it. Its
/// conversation and worktree stay, and `wake_thread` brings it back. The main pane leaves it
/// first: a pane showing it would respawn it to attach again.
pub fn stop_thread(ctx: &ToolCtx, args: &NameArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let name = row.name.clone();
    if row.cleaned_at.is_some() {
        bail!(crate::guard::cleaned_message(&name));
    }
    // Its own account's `claude agents` is the one that knows whether it has a process.
    accounts::refresh_row_with(ctx.conn, ctx.claude, ctx.agp, &row, AGENTS_TIMEOUT, ctx.now)?;
    let row = project_thread(ctx, &name)?;
    if row.removed_at.is_some() {
        bail!(crate::guard::removed_message(&name));
    }
    let Some(short_id) = row.short_id.as_deref().filter(|_| row.pid.is_some()) else {
        return Ok(json!({"name": name, "stopped": false, "note": "It is asleep already."}));
    };
    if db::get_pane_target(ctx.conn, ctx.slug())?.as_deref() == Some(name.as_str()) {
        db::set_pane_target(ctx.conn, ctx.slug(), None)?;
        crate::pane::request_switch(ctx.conn, ctx.slug())?;
    }
    ctx.claude.for_row(&row).stop(short_id)?;
    Ok(json!({"name": name, "stopped": true}))
}

/// Moves a thread to another Claude account now, through agp (§5 "Limits and moves"): agp stops
/// it and resumes the same conversation, in the same worktree, in `account` or the pool account
/// with the most room. Its turn restarts there from a note saying it moved.
pub fn move_thread(ctx: &ToolCtx, args: &MoveThreadArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let name = row.name.clone();
    if row.cleaned_at.is_some() {
        bail!(crate::guard::cleaned_message(&name));
    }
    if row.removed_at.is_some() {
        bail!(crate::guard::removed_message(&name));
    }
    if !ctx.agp.supports_bg() {
        bail!(
            "agp isn't installed, or has no background sessions (`agp bg`): only agp moves a              thread to another account"
        );
    }
    let session = row
        .session_id
        .clone()
        .or_else(|| row.short_id.clone())
        .with_context(|| format!("{name} was never started, so there is nothing to move"))?;
    let project = ctx.claude.account.as_deref();
    let row = crate::moves::settle(
        ctx.conn,
        ctx.agp,
        &row,
        project,
        crate::moves::SETTLE_TIMEOUT,
        ctx.now,
    )?;
    // A named account must be in the pool and trust the folder it resumes in.
    let to = match args
        .account
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        Some(pinned) => {
            let repo = row
                .repo_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.project_dir.to_path_buf());
            let inputs = placement::Inputs {
                threads: &ctx.config.threads,
                agp: ctx.agp,
                project: ctx.claude,
                project_trust: ctx.trust_config,
            };
            let chosen = placement::place(&inputs, &repo, Some(pinned))?
                .chosen
                .with_context(|| format!("{pinned} is not an agp account"))?;
            if same_account(chosen.dir.as_deref(), row.account(project).as_deref()) {
                bail!("{name} runs in {} already", chosen.label);
            }
            Some(chosen.id)
        }
        None => None,
    };
    let moved = ctx.agp.move_now(&session, to.as_deref())?;
    if !moved.moved {
        bail!("agp didn't move {name}");
    }
    follow_manual_move(ctx, &row, &moved)?;
    let row = project_thread(ctx, &name)?;
    if ctx.config.threads.runner == Runner::Agp
        && let Err(e) = crate::moves::rearm(ctx.agp, &row, &ctx.config.threads)
    {
        eprintln!("chief mcp: agp couldn't watch {name} after the move: {e:#}");
    }
    let listing =
        accounts::refresh_row_with(ctx.conn, ctx.claude, ctx.agp, &row, AGENTS_TIMEOUT, ctx.now);
    let row = project_thread(ctx, &name)?;
    let agents = listing
        .as_ref()
        .ok()
        .and_then(|l| l.agents.as_deref().ok())
        .unwrap_or_default();
    let address = address(ctx, &row, agents);
    let mut out = json!({"name": name, "alive": row.pid.is_some(), "address": address});
    moved_out(&mut out, &row, address.as_deref());
    Ok(out)
}

/// Points the row at the account `agp bg move` put it in: from agp's record when `agp bg` lists
/// the session, else from the move's answer.
fn follow_manual_move(ctx: &ToolCtx, row: &SessionRow, moved: &crate::agp::Moved) -> Result<()> {
    let project = ctx.claude.account.as_deref();
    let now = (ctx.now)();
    let sid = row.session_id.as_deref();
    let listed = match ctx.agp.bg() {
        Ok(Some(sessions)) => sessions
            .into_iter()
            .find(|s| Some(s.session_id.as_str()) == sid || s.short_id == row.short_id),
        _ => None,
    };
    let bg = match listed.filter(|s| !s.moves.is_empty()) {
        Some(bg) => bg,
        None => {
            let to = moved.to.clone().context("agp's move named no account")?;
            crate::agp::BgSession {
                session_id: sid.map(str::to_string).unwrap_or_default(),
                short_id: moved.short_id.clone().or_else(|| row.short_id.clone()),
                profile: to.clone(),
                label: moved.to_label.clone(),
                config_dir: moved.to_config_dir.clone(),
                state: "watching".to_string(),
                limit: None,
                moves: vec![crate::agp::BgMove {
                    at: now / 1000,
                    from: moved
                        .from
                        .clone()
                        .or_else(|| row.account_id.clone())
                        .unwrap_or_default(),
                    from_label: moved
                        .from_label
                        .clone()
                        .or_else(|| row.account_shown().map(str::to_string)),
                    from_config_dir: row.account(project),
                    to,
                    to_label: moved.to_label.clone(),
                    to_config_dir: moved.to_config_dir.clone(),
                    reason: Some("manual move".to_string()),
                }],
            }
        }
    };
    db::immediate(ctx.conn, |conn| {
        let Some(mut stored) = db::get_session(conn, &row.name)? else {
            return Ok(());
        };
        let before = stored.clone();
        crate::moves::follow(&mut stored, &bg, project, now);
        if stored != before {
            stored.updated_at = now;
            db::update_session(conn, &stored)?;
            db::insert_event(
                conn,
                &db::Event {
                    session_name: Some(stored.name.clone()),
                    session_id: stored.session_id.clone(),
                    project_slug: stored.project_slug.clone(),
                    hook: crate::moves::MOVED_EVENT.to_string(),
                    ts: now,
                    summary: stored.move_note.clone(),
                    agent_id: None,
                },
            )?;
        }
        Ok(())
    })
}

pub fn resolve_thread(ctx: &ToolCtx, args: &ResolveThreadArgs) -> Result<Value> {
    let row = project_thread(ctx, &args.name)?;
    let resolve = !args.reopen.unwrap_or(false);
    let row = registry::set_resolved(ctx.conn, &row.name, resolve, "coordinator", (ctx.now)())?;
    Ok(json!({"name": row.name, "resolved": row.resolved, "group": row.group_name}))
}
