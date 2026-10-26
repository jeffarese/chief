//! `chief hook <event>` (ARCHITECTURE §2, §4, §5, §6, §8).

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;

use crate::claude::{self, AgentRow, Claude};
use crate::config::ProjectConfig;
use crate::db::{self, SessionRow};
use crate::state::{HookKind, Role};
use crate::text::{local_hhmm, one_line};
use crate::{accounts, context, guard, names, paths, registry, tools};

/// The guard holds up a tool call and the context a turn; neither may hang on the supervisor.
pub const GUARD_AGENTS_TIMEOUT: Duration = Duration::from_secs(10);
const CONTEXT_AGENTS_TIMEOUT: Duration = Duration::from_secs(5);
/// The guard answers within this, or a watchdog denies (§6). Claude Code's own hook timeout
/// (`generated::GUARD_HOOK_TIMEOUT_SECS`) would let the call through, and SQLite busy waits (5 s
/// per statement) have no overall bound, so this is what bounds the guard.
pub const GUARD_DEADLINE: Duration = Duration::from_secs(20);
/// Shortens `GUARD_DEADLINE` (tests only; a shorter deadline can only deny sooner).
const GUARD_DEADLINE_ENV: &str = "CHIEF_GUARD_DEADLINE_MS";
const SUMMARY_CHARS: usize = 120;

/// Set once stdout has its answer (or the decision to print nothing), so the guard's watchdog
/// and the guard itself never both answer.
static ANSWERED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Entry point. Reads the payload from stdin, never panics, never returns an error: failures are
/// appended to the hook error log. Prints only the guard decision or the coordinator's
/// UserPromptSubmit context. The guard fails closed: any error or panic in it denies, and so
/// does running past its deadline.
pub fn run(event: &str, project: &str, role: Role) {
    // The default panic hook writes to stderr, which Claude Code may show in the session.
    std::panic::set_hook(Box::new(|_| {}));
    if event == "guard" {
        start_guard_watchdog(project, guard_deadline());
    }
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| dispatch(event, project, role)));
    let (out, error) = match result {
        Ok(Ok(out)) => (out, None),
        Ok(Err(e)) => (None, Some(format!("{e:#}"))),
        Err(panic) => (None, Some(format!("panic: {}", panic_message(&*panic)))),
    };
    let out = match (&error, event) {
        (Some(error), "guard") => Some(guard_failed(project, error)),
        _ => out,
    };
    answer(out.as_deref());
    if let Some(error) = error {
        log_error(&paths::hook_error_log(), event, project, &error);
    }
}

/// Writes `out` (None: nothing) unless something already answered; false if it had.
fn answer(out: Option<&str>) -> bool {
    let mut answered = ANSWERED.lock().unwrap_or_else(|e| e.into_inner());
    if *answered {
        return false;
    }
    *answered = true;
    if let Some(out) = out {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    true
}

fn guard_deadline() -> Duration {
    std::env::var(GUARD_DEADLINE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .filter(|d| *d < GUARD_DEADLINE)
        .unwrap_or(GUARD_DEADLINE)
}

/// Denies and exits at `deadline` unless the guard answered first. It touches no database (that
/// may be what is stuck), so the denial is only logged.
fn start_guard_watchdog(project: &str, deadline: Duration) {
    let project = project.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        let reason = format!(
            "chief's message guard didn't decide within {}s, so the message is blocked. Try \
             again; if it keeps happening, check {}.",
            deadline.as_secs(),
            paths::hook_error_log().display()
        );
        if answer(Some(&guard::deny_json(&reason))) {
            log_error(&paths::hook_error_log(), "guard", &project, &reason);
            std::process::exit(0);
        }
    });
}

/// Routes an event; the returned text goes to stdout.
fn dispatch(event: &str, project: &str, role: Role) -> Result<Option<String>> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the hook payload")?;
    if event == "guard" {
        return guard(project, role, &input);
    }
    let payload: Value = serde_json::from_str(&input).context("parsing the hook payload")?;
    let conn = db::open(&paths::db())?;
    let ctx = Ctx {
        project,
        role,
        now: db::now_ms(),
    };
    let outcome = handle(&conn, &ctx, event, &payload)?;
    if !outcome.notices.is_empty() && notify_enabled(project) {
        for notice in &outcome.notices {
            send_notice(notice);
        }
    }
    if role == Role::Coordinator && event == "UserPromptSubmit" {
        return coordinator_context(&conn, project).map(Some);
    }
    Ok(None)
}

/// §6: gathers the registry rows, the caller's subagents and fresh `claude agents` listings,
/// decides, and records a denial. Returns the deny JSON, or None to allow.
fn guard(project: &str, role: Role, input: &str) -> Result<Option<String>> {
    let payload: Value = serde_json::from_str(input).context("parsing the hook payload")?;
    let call = Call::from_payload(&payload)?;
    let conn = db::open(&paths::db())?;
    let rows = db::list_sessions(&conn, project, None)?;
    let caller_sid = payload.get("session_id").and_then(Value::as_str);
    let teammates = match call.tool {
        "SendMessage" => caller_teammates(&conn, &rows, caller_sid)?,
        _ => Vec::new(),
    };
    let claude = project_claude(project);
    let project_account = claude.account.clone();
    let send_to = call.to.filter(|_| call.tool == "SendMessage");
    let spawn_name = call.name.filter(|n| call.tool == "Agent" && !n.is_empty());
    // Only a SendMessage with an address needs to know who is alive: in the coordinator's
    // account, and in the accounts of the threads the message could be for (§3). A subagent's
    // name, sent or given at spawn, also needs who else answers to it in the caller's own account.
    let (snapshot, others) = if send_to.is_some() || spawn_name.is_some() {
        let mut wanted = vec![project_account.clone()];
        let mut want = |account: Option<std::path::PathBuf>| {
            if !wanted
                .iter()
                .any(|a| claude::same_account(a.as_deref(), account.as_deref()))
            {
                wanted.push(account);
            }
        };
        let mate_named = |to: &str| teammates.iter().any(|t| t.name.as_deref() == Some(to));
        if spawn_name.is_some() || send_to.is_some_and(mate_named) {
            want(caller_account(
                &rows,
                caller_sid,
                project_account.as_deref(),
            ));
        }
        if role == Role::Coordinator
            && let Some(to) = send_to
        {
            for r in rows.iter().filter(|r| reachable_elsewhere(r, to)) {
                want(r.account(project_account.as_deref()));
            }
        }
        let listings = accounts::list(&claude, &wanted, GUARD_AGENTS_TIMEOUT, db::now_ms);
        let snapshot = if send_to.is_some() {
            // Without the coordinator's account chief can check no message: fail closed.
            listings
                .project()
                .context("the project's account was listed")?
                .agents()?
                .to_vec()
        } else {
            // An Agent call reads only who else has the name, and goes on without a listing.
            if let Some(e) = listings.error() {
                log_error(
                    &paths::hook_error_log(),
                    "guard",
                    project,
                    &format!(
                        "couldn't list the Claude sessions for an Agent call ({e}); checked the \
                         name against chief's sessions only"
                    ),
                );
            }
            Vec::new()
        };
        (snapshot, listings.all)
    } else {
        (Vec::new(), Vec::new())
    };
    let decision = guard_decision_in(
        project,
        role,
        &call,
        &rows,
        &Accounts {
            project: project_account.as_deref(),
            snapshot: &snapshot,
            others: &others,
            caller: caller_sid,
        },
        &teammates,
    );
    let guard::Decision::Deny(reason) = decision else {
        return Ok(None);
    };
    let caller = caller_sid
        .and_then(|sid| rows.iter().find(|r| r.session_id.as_deref() == Some(sid)))
        .map(|r| r.name.clone());
    let target = match call.tool {
        "Agent" => call.name,
        _ => call.to,
    };
    db::insert_guard_denial(
        &conn,
        &db::GuardDenial {
            project_slug: project.to_string(),
            session_name: caller,
            tool: call.tool.to_string(),
            target: target.map(str::to_string),
            reason: reason.clone(),
            ts: db::now_ms(),
        },
    )?;
    Ok(Some(guard::deny_json(&reason)))
}

/// The tool call the guard decides on, from a PreToolUse payload (§6).
#[derive(Debug, Clone, Copy, Default)]
pub struct Call<'a> {
    pub tool: &'a str,
    /// `tool_input.to` (SendMessage).
    pub to: Option<&'a str>,
    /// `tool_input.name` (Agent).
    pub name: Option<&'a str>,
    /// The payload has an `agent_id`: one of the caller's subagents made the call (§1).
    pub from_subagent: bool,
}

impl<'a> Call<'a> {
    pub fn from_payload(payload: &'a Value) -> Result<Self> {
        let tool = payload
            .get("tool_name")
            .and_then(Value::as_str)
            .context("the payload has no tool_name")?;
        let input = payload.get("tool_input").and_then(Value::as_object);
        let text = |k: &str| input.and_then(|o| o.get(k)).and_then(Value::as_str);
        Ok(Call {
            tool,
            to: text("to"),
            name: text("name"),
            from_subagent: payload
                .get("agent_id")
                .and_then(Value::as_str)
                .is_some_and(|a| !a.is_empty()),
        })
    }
}

/// The subagents the calling session recorded in its current process, under the session id it
/// calls with (§5); none for a caller chief doesn't know.
pub fn caller_teammates(
    conn: &Connection,
    rows: &[SessionRow],
    caller: Option<&str>,
) -> Result<Vec<guard::Teammate>> {
    let Some(sid) = caller else {
        return Ok(Vec::new());
    };
    match rows.iter().find(|r| r.session_id.as_deref() == Some(sid)) {
        Some(row) => db::teammates(conn, &row.name, sid),
        None => Ok(Vec::new()),
    }
}

/// The account where a name the caller sends reaches sessions (§1): a thread's own, else the
/// project's.
fn caller_account(
    rows: &[SessionRow],
    caller: Option<&str>,
    project: Option<&Path>,
) -> Option<std::path::PathBuf> {
    caller
        .and_then(|sid| rows.iter().find(|r| r.session_id.as_deref() == Some(sid)))
        .filter(|r| r.role == Role::Thread.as_str())
        .map(|r| r.account(project))
        .unwrap_or_else(|| project.map(Path::to_path_buf))
}

/// `claude` in the project's account (project.toml → `claude_config_dir`), whatever account the
/// hook's environment names. When project.toml can't say, the account the generated hook
/// command set (§3), which is the project's unless project.toml was edited by hand since.
fn project_claude(project: &str) -> Claude {
    let account = crate::project::account_of(project).unwrap_or_else(|_| claude::env_account());
    Claude::for_account(account)
}

/// A thread in another account than the project's that a coordinator's message to `to` could
/// be for: by its name, or any live one for a socket address.
fn reachable_elsewhere(row: &SessionRow, to: &str) -> bool {
    row.role == Role::Thread.as_str()
        && row.account_id.is_some()
        && !row.resolved
        && row.cleaned_at.is_none()
        && row.removed_at.is_none()
        && (row.name == to || to.starts_with("uds:"))
}

/// The §6 decision from the project's registry rows and a `claude agents` snapshot, every
/// session in the project's account.
pub fn guard_decision(
    project: &str,
    role: Role,
    tool: &str,
    to: Option<&str>,
    rows: &[SessionRow],
    snapshot: &[AgentRow],
) -> guard::Decision {
    let accounts = Accounts {
        project: None,
        snapshot,
        others: &[],
        caller: None,
    };
    let call = Call {
        tool,
        to,
        ..Call::default()
    };
    guard_decision_in(project, role, &call, rows, &accounts, &[])
}

/// The listings the guard decides on (§3, §6).
pub struct Accounts<'a> {
    /// The project's account, the coordinator's.
    pub project: Option<&'a Path>,
    /// Its fresh `claude agents`.
    pub snapshot: &'a [AgentRow],
    /// Fresh listings of other accounts threads run in (the project's may be among them).
    pub others: &'a [accounts::Listing],
    /// The calling session's id, from the hook payload.
    pub caller: Option<&'a str>,
}

/// `guard_decision` with threads in other accounts and the caller's subagents: each thread is
/// checked in its own account's listing, a thread calling from another account than its
/// coordinator's is told so, and a subagent's name is checked against the listings of the
/// project's account and the caller's own.
pub fn guard_decision_in(
    project: &str,
    role: Role,
    call: &Call,
    rows: &[SessionRow],
    accounts: &Accounts,
    teammates: &[guard::Teammate],
) -> guard::Decision {
    let elsewhere = |r: &SessionRow| -> Option<guard::Elsewhere> {
        let account = r.account(accounts.project);
        if claude::same_account(account.as_deref(), accounts.project) {
            return None;
        }
        let label = r
            .account_shown()
            .map(str::to_string)
            .unwrap_or_else(|| claude::account_label(account.as_deref()));
        let snapshot = match accounts
            .others
            .iter()
            .find(|l| claude::same_account(l.account(), account.as_deref()))
        {
            Some(l) => l.agents.clone(),
            None => Err("not listed".to_string()),
        };
        Some(guard::Elsewhere { label, snapshot })
    };
    let member = |r: &SessionRow| guard::Member {
        name: r.name.clone(),
        session_id: r.session_id.clone(),
        short_id: r.short_id.clone(),
        resolved: r.resolved,
        cleaned: r.cleaned_at.is_some(),
        removed: r.removed_at.is_some(),
        elsewhere: (r.role == Role::Thread.as_str())
            .then(|| elsewhere(r))
            .flatten(),
    };
    let threads: Vec<_> = rows
        .iter()
        .filter(|r| r.role == Role::Thread.as_str())
        .map(member)
        .collect();
    let coordinator = rows
        .iter()
        .find(|r| r.role == Role::Coordinator.as_str())
        .map(member);
    let caller_elsewhere = accounts
        .caller
        .and_then(|sid| rows.iter().find(|r| r.session_id.as_deref() == Some(sid)))
        .filter(|r| r.role == Role::Thread.as_str())
        .and_then(|r| {
            let account = r.account(accounts.project);
            (!claude::same_account(account.as_deref(), accounts.project)).then(|| {
                r.account_shown()
                    .map(str::to_string)
                    .unwrap_or_else(|| claude::account_label(account.as_deref()))
            })
        });
    // Where a name from the caller can reach someone (§1): its own account, and the project's.
    let own = caller_account(rows, accounts.caller, accounts.project);
    let names = |agents: &[AgentRow]| -> Vec<String> {
        agents.iter().filter_map(|a| a.name.clone()).collect()
    };
    let mut listed = guard::Listed {
        names: names(accounts.snapshot),
        missing: None,
    };
    let mut missing = Vec::new();
    for l in accounts.others.iter().filter(|l| {
        claude::same_account(l.account(), accounts.project)
            || claude::same_account(l.account(), own.as_deref())
    }) {
        match &l.agents {
            Ok(agents) => listed.names.extend(names(agents)),
            Err(e) => missing.push(e.clone()),
        }
    }
    listed.missing = (!missing.is_empty()).then(|| missing.join("; "));
    let coordinator_name = names::coordinator_name(project);
    guard::evaluate(&guard::GuardInput {
        role,
        tool: call.tool,
        to: call.to,
        agent_name: call.name,
        coordinator_name: &coordinator_name,
        coordinator: coordinator.as_ref(),
        threads: &threads,
        snapshot: accounts.snapshot,
        caller_elsewhere: caller_elsewhere.as_deref(),
        from_subagent: call.from_subagent,
        teammates,
        listed: &listed,
    })
}

/// The deny JSON for a guard that couldn't decide, recorded if the database still opens.
fn guard_failed(project: &str, error: &str) -> String {
    let reason = format!("chief's message guard failed, so the message is blocked: {error}");
    if let Ok(conn) = db::open(&paths::db()) {
        let _ = db::insert_guard_denial(
            &conn,
            &db::GuardDenial {
                project_slug: project.to_string(),
                session_name: None,
                tool: "unknown".to_string(),
                target: None,
                reason: reason.clone(),
                ts: db::now_ms(),
            },
        );
    }
    guard::deny_json(&reason)
}

/// §8: refreshes the board from `claude agents` (bounded; stored rows if it fails) and returns
/// the UserPromptSubmit output with the board snapshot and MEMORY.md.
fn coordinator_context(conn: &Connection, project: &str) -> Result<String> {
    let claude = project_claude(project);
    let refreshed =
        crate::accounts::refresh(conn, project, &claude, CONTEXT_AGENTS_TIMEOUT, db::now_ms)
            .map(|listings| listings.error())
            .unwrap_or_else(|e| Some(format!("{e:#}")));
    if let Some(e) = refreshed {
        log_error(
            &paths::hook_error_log(),
            "UserPromptSubmit",
            project,
            &format!("board refresh failed, using stored rows: {e}"),
        );
    }
    let mut threads = db::list_sessions(conn, project, Some(Role::Thread))?;
    tools::board_order(&mut threads);
    let memory = std::fs::read_to_string(paths::project_dir(project).join("memory/MEMORY.md"))
        .unwrap_or_default();
    let text = context::coordinator_context(&threads, &memory, &local_hhmm(db::now_ms()));
    Ok(context::hook_output(&text))
}

/// What `handle` needs from the outside world, passed in so tests can replay fixtures.
pub struct Ctx<'a> {
    pub project: &'a str,
    /// The `--role` the hook was installed with.
    pub role: Role,
    pub now: i64,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The matched session, if any.
    pub session: Option<String>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub message: String,
}

/// Records the event and applies the §5 hook updates to the matched session.
pub fn handle(conn: &Connection, ctx: &Ctx, event: &str, payload: &Value) -> Result<Outcome> {
    let str_field = |k: &str| payload.get(k).and_then(Value::as_str);
    let session_id = str_field("session_id");
    let kind = HookKind::parse(event, str_field("notification_type"));
    let row = db::immediate(conn, |conn| {
        let mut row = match_session(conn, ctx.project, session_id, str_field("session_title"))?;
        db::insert_event(
            conn,
            &db::Event {
                session_name: row.as_ref().map(|r| r.name.clone()),
                session_id: session_id.map(str::to_string),
                project_slug: ctx.project.to_string(),
                hook: event.to_string(),
                ts: ctx.now,
                summary: Some(summary(event, payload)),
            },
        )?;
        let Some(r) = row.as_mut() else {
            return Ok(None);
        };
        if let Some(sid) = session_id {
            record_teammates(conn, &r.name, sid, event, payload, ctx.now)?;
        }
        if let Some(kind) = kind
            && let Some(report) = apply(r, kind, payload, ctx.now)
        {
            db::insert_report(conn, &r.name, ctx.now, &report)?;
        }
        db::update_session(conn, r)?;
        Ok(row)
    })?;

    let mut outcome = Outcome::default();
    if let Some(row) = row {
        if let Some(kind) = kind
            && let Some(n) = notice_for(ctx.project, &row, kind)
        {
            outcome.notices.push(n);
        }
        outcome.session = Some(row.name);
    }
    Ok(outcome)
}

/// §4 "Matching a hook to a session". A title match also stores the session id. A row that
/// already has a different session id doesn't match by title: that hook comes from a copy.
fn match_session(
    conn: &Connection,
    project: &str,
    session_id: Option<&str>,
    title: Option<&str>,
) -> Result<Option<SessionRow>> {
    if let Some(sid) = session_id
        && let Some(row) = db::get_session_by_session_id(conn, sid)?
    {
        return Ok((row.project_slug == project).then_some(row));
    }
    let Some(title) = title else {
        return Ok(None);
    };
    let Some(mut row) = db::get_session(conn, title)? else {
        return Ok(None);
    };
    if row.project_slug != project || row.session_id.is_some() {
        return Ok(None);
    }
    row.session_id = session_id.map(str::to_string);
    Ok(Some(row))
}

/// §6 "Subagents": a session's subagents are recorded under it, by the session id its hooks
/// carry (a subagent's own hooks carry its lead's), and forgotten when its process starts again:
/// a SessionStart other than `compact` is a new process, which has none of them (§1).
fn record_teammates(
    conn: &Connection,
    session: &str,
    session_id: &str,
    event: &str,
    payload: &Value,
    now: i64,
) -> Result<()> {
    fn text<'a>(v: Option<&'a Value>, k: &str) -> Option<&'a str> {
        v.and_then(|v| v.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }
    match event {
        "SessionStart" if text(Some(payload), "source") != Some("compact") => {
            db::forget_teammates(conn, session)?;
        }
        // Only the id: `agent_type` is the agent's name only when it was given one.
        "SubagentStart" => {
            if let Some(agent) = text(Some(payload), "agent_id") {
                db::record_teammate(conn, session, session_id, agent, None, now)?;
            }
        }
        "PostToolUse" if text(Some(payload), "tool_name") == Some("Agent") => {
            let response = payload.get("tool_response");
            let Some(agent) = text(response, "agentId").or(text(response, "agent_id")) else {
                return Ok(());
            };
            // The name it was given, unless Claude Code registered another: then that name
            // could reach someone else, and only the id is kept.
            let registered = response.and_then(|r| r.get("name")).and_then(Value::as_str);
            let name = text(payload.get("tool_input"), "name")
                .filter(|n| registered.is_none_or(|r| r == *n));
            db::record_teammate(conn, session, session_id, agent, name, now)?;
        }
        _ => {}
    }
    Ok(())
}

/// §5 "Hook updates". The hook is newer than any `claude agents` poll, so the stored snapshot
/// stops counting until the next poll. Returns the new report, if the hook carried one. The cwd
/// is kept for display only: the thread's worktree and branch come from its job state (sync).
fn apply(row: &mut SessionRow, kind: HookKind, payload: &Value, now: i64) -> Option<String> {
    let str_field = |k: &str| payload.get(k).and_then(Value::as_str);
    row.last_hook = Some(kind.as_str().to_string());
    row.last_hook_at = Some(now);
    row.cc_fresh = false;
    row.updated_at = now;
    if kind != HookKind::IdleNotification {
        row.last_activity_at = now;
    }
    let mut report = None;
    match kind {
        HookKind::SessionStart | HookKind::IdleNotification => {}
        HookKind::UserPromptSubmit | HookKind::PreToolUse => row.pending_permission = false,
        HookKind::PostToolUse => {
            row.pending_permission = false;
            if let Some(cwd) = str_field("cwd") {
                row.cwd = Some(cwd.to_string());
            }
        }
        HookKind::PermissionNotification => row.pending_permission = true,
        HookKind::Stop => {
            row.pending_permission = false;
            if let Some(cwd) = str_field("cwd") {
                row.cwd = Some(cwd.to_string());
            }
            if let Some(text) = str_field("last_assistant_message").filter(|r| !r.trim().is_empty())
            {
                row.last_report = Some(text.to_string());
                row.last_report_at = Some(now);
                report = Some(text.to_string());
            }
        }
    }
    registry::rederive(row);
    report
}

/// Threads only: a permission prompt, or a finished turn.
fn notice_for(project: &str, row: &SessionRow, kind: HookKind) -> Option<Notice> {
    if row.role != Role::Thread.as_str() {
        return None;
    }
    let label = row.title.as_deref().unwrap_or(&row.name);
    let message = match kind {
        HookKind::PermissionNotification => format!("{label} needs your permission"),
        HookKind::Stop => {
            let report = row.last_report.as_deref().unwrap_or("");
            format!("{label} finished: {}", one_line(report, 100))
        }
        _ => return None,
    };
    Some(Notice {
        title: format!("chief · {project}"),
        message,
    })
}

/// A short human line for the events table; never the whole payload.
pub fn summary(event: &str, payload: &Value) -> String {
    let field = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("");
    let text = match event {
        "PreToolUse" | "PostToolUse" => field("tool_name").to_string(),
        "UserPromptSubmit" => field("prompt").to_string(),
        "Notification" => format!("{}: {}", field("notification_type"), field("message")),
        "Stop" => field("last_assistant_message").to_string(),
        "SessionStart" => field("source").to_string(),
        "SessionEnd" => field("reason").to_string(),
        "SubagentStart" | "SubagentStop" => field("agent_id").to_string(),
        _ => String::new(),
    };
    one_line(&text, SUMMARY_CHARS)
}

/// `ui.notify` from project.toml. A missing file means no notifications; a broken one is logged.
fn notify_enabled(project: &str) -> bool {
    let path = paths::project_dir(project).join("project.toml");
    if !path.exists() {
        return false;
    }
    match ProjectConfig::load(&path) {
        Ok(c) => c.ui.notify,
        Err(e) => {
            log_error(
                &paths::hook_error_log(),
                "notify",
                project,
                &format!("{e:#}"),
            );
            false
        }
    }
}

/// Fire and forget: the hook must not wait on Notification Center, and the child must not hold
/// the hook's stdout open. `CHIEF_NO_NOTIFY=1` turns this off (tests).
fn send_notice(n: &Notice) {
    if std::env::var_os("CHIEF_NO_NOTIFY").is_some_and(|v| v == "1") {
        return;
    }
    // Text goes in as argv so quotes in a title can't break the AppleScript.
    let _ = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            &n.title,
            &n.message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Appends one line to the hook error log; errors here are dropped, there is nowhere left to
/// report them.
pub fn log_error(log: &Path, event: &str, project: &str, error: &str) {
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let error = error.replace('\n', " | ");
        let _ = writeln!(
            f,
            "{} hook {event} --project {project}: {error}",
            db::now_ms()
        );
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD_A: &str = "f8627ea8-50af-4cce-b12d-936669f85ed0";
    const COORD: &str = "20dc103a-4f18-4895-8bae-4454c193c7e6";
    const PERM: &str = "bf6db904-3919-4adc-956d-c30266432600";

    struct Line {
        hook: String,
        ts: i64,
        payload: Value,
    }

    fn fixture() -> Vec<Line> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hooks/spike-events.jsonl"
        ))
        .unwrap();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                Line {
                    hook: v["hook"].as_str().unwrap().to_string(),
                    ts: v["ts"].as_i64().unwrap(),
                    payload: v["payload"].clone(),
                }
            })
            .collect()
    }

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        for (name, role, title) in [
            ("spike-thread-a", Role::Thread, "Hello file"),
            ("spike-coord", Role::Coordinator, "Coordinator"),
            ("spike-thread-perm", Role::Thread, "Curl check"),
        ] {
            let mut row = SessionRow::new(name, "demo", role, 0);
            row.title = Some(title.into());
            db::insert_session(&conn, &row).unwrap();
        }
        (dir, conn)
    }

    fn role_of(session_id: &str) -> Role {
        if session_id == COORD {
            Role::Coordinator
        } else {
            Role::Thread
        }
    }

    fn replay_line(conn: &Connection, line: &Line) -> Outcome {
        let sid = line.payload["session_id"].as_str().unwrap();
        let ctx = Ctx {
            project: "demo",
            role: role_of(sid),
            now: line.ts * 1000,
        };
        handle(conn, &ctx, &line.hook, &line.payload).unwrap()
    }

    fn get(conn: &Connection, name: &str) -> SessionRow {
        db::get_session(conn, name).unwrap().unwrap()
    }

    #[test]
    fn replays_the_spike() {
        let (_dir, conn) = setup();
        let lines = fixture();
        assert_eq!(lines.len(), 44);
        let mut notices = Vec::new();
        let mut checked_waiting = false;
        let mut checked_working = 0;

        for line in &lines {
            let out = replay_line(&conn, line);
            assert!(
                out.session.is_some(),
                "every fixture hook matches a session"
            );
            notices.extend(out.notices);
            let sid = line.payload["session_id"].as_str().unwrap();
            let ntype = line.payload["notification_type"].as_str();

            if sid == PERM && ntype == Some("permission_prompt") {
                let r = get(&conn, "spike-thread-perm");
                assert!(r.pending_permission);
                assert_eq!(r.group_name, "waiting");
                assert_eq!(r.waiting_reason.as_deref(), Some("permission"));
                checked_waiting = true;
            }
            if sid == PERM && line.hook == "PostToolUse" {
                let r = get(&conn, "spike-thread-perm");
                assert!(!r.pending_permission, "PostToolUse clears the prompt");
                assert_eq!((r.group_name.as_str(), r.waiting_reason), ("working", None));
            }
            if matches!(
                line.hook.as_str(),
                "UserPromptSubmit" | "PreToolUse" | "PostToolUse"
            ) {
                let name = match sid {
                    THREAD_A => "spike-thread-a",
                    COORD => "spike-coord",
                    _ => "spike-thread-perm",
                };
                assert_eq!(
                    get(&conn, name).group_name,
                    "working",
                    "{} {name}",
                    line.hook
                );
                checked_working += 1;
            }
            if line.hook == "Stop" {
                let name = match sid {
                    THREAD_A => "spike-thread-a",
                    COORD => "spike-coord",
                    _ => "spike-thread-perm",
                };
                let r = get(&conn, name);
                assert_eq!(r.group_name, "idle", "Stop {name}");
                assert_eq!(
                    r.last_report.as_deref(),
                    line.payload["last_assistant_message"].as_str()
                );
                assert_eq!(r.last_report_at, Some(line.ts * 1000));
            }
        }
        assert!(checked_waiting);
        assert!(checked_working > 20);

        // Session ids were learned from session_title.
        assert_eq!(
            get(&conn, "spike-thread-a").session_id.as_deref(),
            Some(THREAD_A)
        );
        assert_eq!(get(&conn, "spike-coord").session_id.as_deref(), Some(COORD));
        assert_eq!(
            get(&conn, "spike-thread-perm").session_id.as_deref(),
            Some(PERM)
        );

        let a = get(&conn, "spike-thread-a");
        assert_eq!(a.group_name, "idle");
        assert_eq!(
            a.last_hook.as_deref(),
            Some("Stop"),
            "SubagentStop/SessionEnd are only recorded"
        );
        assert!(
            a.last_report
                .as_deref()
                .unwrap()
                .starts_with("Commit hash: `7dbc89f")
        );
        assert_eq!(
            a.cwd.as_deref(),
            Some("/tmp/chief-fixture/repo/.claude/worktrees/hello-txt")
        );
        assert_eq!(
            a.branch, None,
            "the branch comes from the job state on sync, never from the hooks' cwd"
        );
        assert!(!a.pending_permission);

        let c = get(&conn, "spike-coord");
        assert_eq!(c.group_name, "idle");
        assert_eq!(c.last_hook.as_deref(), Some("Notification:idle_prompt"));
        assert_eq!(c.branch, None, "no git lookup for the coordinator");
        assert!(
            c.last_report
                .as_deref()
                .unwrap()
                .starts_with("RELAYED spike-thread-a")
        );
        // idle_prompt is not activity: the coordinator's last activity is its last Stop.
        assert_eq!(c.last_activity_at, 1792967179 * 1000);

        let p = get(&conn, "spike-thread-perm");
        assert_eq!(
            (p.group_name.as_str(), p.pending_permission),
            ("idle", false)
        );
        assert_eq!(
            p.last_report.as_deref(),
            Some("The request returned `HTTP/2 200` from https://example.com.")
        );

        let reports = |n: &str| db::recent_reports(&conn, n, 10).unwrap();
        assert_eq!(reports("spike-thread-a").len(), 3);
        assert_eq!(reports("spike-coord").len(), 2);
        assert_eq!(reports("spike-thread-perm").len(), 1);
        assert_eq!(
            reports("spike-thread-a")[0].text,
            a.last_report.clone().unwrap()
        );

        let events = db::recent_events(&conn, "demo", 100).unwrap();
        assert_eq!(events.len(), 44);
        assert!(events.iter().all(|e| e.session_name.is_some()));
        for e in &events {
            let s = e.summary.as_deref().unwrap();
            assert!(
                s.chars().count() <= SUMMARY_CHARS && !s.contains('\n'),
                "{s:?}"
            );
        }
        let perm_event = events
            .iter()
            .find(|e| {
                e.hook == "Notification"
                    && e.session_name.as_deref() == Some("spike-thread-perm")
                    && e.summary
                        .as_deref()
                        .unwrap()
                        .starts_with("permission_prompt")
            })
            .unwrap();
        assert_eq!(
            perm_event.summary.as_deref(),
            Some("permission_prompt: Claude needs your permission")
        );

        // Notices: one permission prompt and one per thread Stop; none for the coordinator.
        let messages: Vec<_> = notices.iter().map(|n| n.message.as_str()).collect();
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.starts_with("Hello file finished"))
                .count(),
            3
        );
        assert!(messages.contains(&"Curl check needs your permission"));
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("Curl check finished: The request returned"))
        );
        assert!(notices.iter().all(|n| n.title == "chief · demo"));
    }

    /// The lead of `tests/fixtures/hooks/teammate-events.jsonl` (§1: a background session whose
    /// Agent calls spawn in-process teammates).
    const LEAD: &str = "7df9ed81-5900-4844-80cf-77bb499e7e0a";
    const HELPER: &str = "ahelper-886b99f57d4badec";
    const SPIKE_OTHER: &str = "aspike-other-ba38036fce05d0d2";

    /// The fixture's raw payloads, in the order the hook logged them.
    pub(crate) fn teammate_fixture() -> Vec<Value> {
        include_str!("../tests/fixtures/hooks/teammate-events.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The fixture's lead as a thread of project demo.
    fn lead_setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        db::insert_session(
            &conn,
            &SessionRow::new("spike-main", "demo", Role::Thread, 0),
        )
        .unwrap();
        (dir, conn)
    }

    fn replay(conn: &Connection, payload: &Value, now: i64) -> Outcome {
        let ctx = Ctx {
            project: "demo",
            role: Role::Thread,
            now,
        };
        let event = payload["hook_event_name"].as_str().unwrap();
        handle(conn, &ctx, event, payload).unwrap()
    }

    fn mates(conn: &Connection) -> Vec<(String, Option<String>)> {
        db::teammates(conn, "spike-main", LEAD)
            .unwrap()
            .into_iter()
            .map(|t| (t.agent_id, t.name))
            .collect()
    }

    /// F1, F2: the Agent call's PostToolUse gives the agent id and the name, SubagentStart the
    /// id alone; `agent_type` is never taken for a name. F6: a SessionStart other than
    /// `compact` is a new process, without them.
    #[test]
    fn teammates_are_recorded_from_the_fixture() {
        let (_dir, conn) = lead_setup();
        let lines = teammate_fixture();
        assert_eq!(lines.len(), 20);
        let event = |i: usize| lines[i]["hook_event_name"].as_str().unwrap();
        assert_eq!(
            (event(0), event(2), event(3), event(17)),
            (
                "SessionStart",
                "PostToolUse",
                "SubagentStart",
                "SessionStart"
            )
        );

        // startup: learns the session id from the title.
        assert_eq!(
            replay(&conn, &lines[0], 1).session.as_deref(),
            Some("spike-main")
        );
        // The Agent call's PreToolUse records nothing: the agent doesn't exist yet.
        replay(&conn, &lines[1], 2);
        assert!(mates(&conn).is_empty());
        // PostToolUse Agent: `tool_response.agentId` and `tool_input.name`.
        replay(&conn, &lines[2], 3);
        assert_eq!(mates(&conn), [(HELPER.into(), Some("helper".into()))]);
        // SubagentStart keeps the name it doesn't carry.
        replay(&conn, &lines[3], 4);
        assert_eq!(mates(&conn), [(HELPER.into(), Some("helper".into()))]);

        // A SubagentStart alone: its `agent_type` is the name here, but for an unnamed agent
        // it is the type, so it is never taken for a name.
        let mut start = lines[3].clone();
        start["agent_id"] = SPIKE_OTHER.into();
        start["agent_type"] = "spike-other".into();
        replay(&conn, &start, 5);
        assert_eq!(
            mates(&conn),
            [
                (HELPER.into(), Some("helper".into())),
                (SPIKE_OTHER.into(), None)
            ]
        );
        // The rest of the first process (the teammates' own calls, SubagentStop, messages)
        // changes nothing.
        for (i, line) in lines.iter().enumerate().take(17).skip(4) {
            replay(&conn, line, 10 + i as i64);
        }
        assert_eq!(mates(&conn).len(), 2);

        // A compaction keeps the process, and its teammates.
        let mut compact = lines[17].clone();
        compact["source"] = "compact".into();
        replay(&conn, &compact, 30);
        assert_eq!(mates(&conn).len(), 2);
        // `claude respawn`: SessionStart `resume`, a new process without them.
        assert_eq!(
            replay(&conn, &lines[17], 31).session.as_deref(),
            Some("spike-main")
        );
        assert!(mates(&conn).is_empty());
        for (i, line) in lines.iter().enumerate().skip(18) {
            replay(&conn, line, 40 + i as i64);
        }
        assert!(mates(&conn).is_empty());

        // `clear` and `startup` are new processes too, and so is a SessionStart without a
        // source.
        for source in [Some("clear"), Some("startup"), None] {
            replay(&conn, &lines[2], 50);
            assert_eq!(mates(&conn).len(), 1);
            let mut start = lines[17].clone();
            start["source"] = source.into();
            replay(&conn, &start, 51);
            assert!(mates(&conn).is_empty(), "{source:?}");
        }

        // Newest first: the fixture's own SubagentStart is the older one.
        let events = db::recent_events(&conn, "demo", 100).unwrap();
        let start = events.iter().rfind(|e| e.hook == "SubagentStart").unwrap();
        assert_eq!(start.session_name.as_deref(), Some("spike-main"));
        assert_eq!(start.summary.as_deref(), Some(HELPER));
    }

    /// Only what Claude Code registered: a response naming the agent differently gives no name
    /// (the id still works), and a response without an agent id records nothing.
    #[test]
    fn teammates_need_the_registered_name_and_an_id() {
        let (_dir, conn) = lead_setup();
        let lines = teammate_fixture();
        replay(&conn, &lines[0], 1);
        let mut renamed = lines[2].clone();
        renamed["tool_response"]["name"] = "helper-2".into();
        replay(&conn, &renamed, 2);
        assert_eq!(mates(&conn), [(HELPER.into(), None)]);

        let mut no_id = lines[2].clone();
        let response = no_id["tool_response"].as_object_mut().unwrap();
        response.remove("agentId");
        response.remove("agent_id");
        no_id["tool_input"]["name"] = "nobody".into();
        replay(&conn, &no_id, 3);
        let mut blank = lines[3].clone();
        blank["agent_id"] = "".into();
        replay(&conn, &blank, 4);
        // Another tool's PostToolUse with the same shape is not a spawn.
        let mut other_tool = lines[2].clone();
        other_tool["tool_name"] = "Task".into();
        other_tool["tool_response"]["agentId"] = "atask-1".into();
        replay(&conn, &other_tool, 5);
        assert_eq!(mates(&conn), [(HELPER.into(), None)]);

        // A hook of a session chief doesn't know records nothing.
        let mut stranger = lines[2].clone();
        stranger["session_id"] = "someone-else".into();
        assert_eq!(replay(&conn, &stranger, 6).session, None);
        assert!(
            db::teammates(&conn, "spike-main", "someone-else")
                .unwrap()
                .is_empty()
        );
    }

    /// §6 "Subagents" from the database: the fixture's lead (a thread) and its teammates, as the
    /// hooks record them, decided as the guard would for the fixture's own PreToolUse payloads.
    #[test]
    fn guard_lets_a_session_and_its_own_subagents_talk() {
        let (_dir, conn) = lead_setup();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("c-sid".into());
        db::insert_session(&conn, &coord).unwrap();
        let mut other = SessionRow::new("demo-other-2222", "demo", Role::Thread, 0);
        other.session_id = Some("o-sid".into());
        db::insert_session(&conn, &other).unwrap();
        db::record_teammate(
            &conn,
            "demo-other-2222",
            "o-sid",
            "aresearcher-1",
            Some("researcher"),
            1,
        )
        .unwrap();
        let snapshot = [
            agent_row("c1", "c-sid", "chief-demo", Some(100)),
            agent_row("l1", LEAD, "spike-main", Some(201)),
            agent_row("o1", "o-sid", "demo-other-2222", Some(202)),
            // An outside session with the name of one of the lead's teammates (§1 F5).
            agent_row("x1", "x-sid", "spike-other", Some(300)),
        ];
        let lines = teammate_fixture();
        let decide = |payload: &Value, role: Role| {
            let rows = db::list_sessions(&conn, "demo", None).unwrap();
            let sid = payload["session_id"].as_str();
            let teammates = caller_teammates(&conn, &rows, sid).unwrap();
            let accounts = Accounts {
                project: None,
                snapshot: &snapshot,
                others: &[],
                caller: sid,
            };
            let call = Call::from_payload(payload).unwrap();
            guard_decision_in("demo", role, &call, &rows, &accounts, &teammates)
        };
        let lead = |payload: &Value| decide(payload, Role::Thread);
        let with = |i: usize, to: &str| {
            let mut p = lines[i].clone();
            p["tool_input"]["to"] = to.into();
            p
        };
        use guard::Decision::Allow;
        // The fixture's lead messages: to helper by name (7) and by agent id (9), to spike-other
        // (11); spike-other's report to team-lead (5); the Agent call (1).
        let (to_helper, to_helper_id, to_spike, report) =
            (&lines[7], &lines[9], &lines[11], &lines[5]);
        assert_eq!(to_helper["tool_input"]["to"], "helper");
        assert_eq!(to_helper_id["tool_input"]["to"], HELPER);
        assert_eq!(report["tool_input"]["to"], "team-lead");
        assert!(report["agent_id"].is_string() && to_helper.get("agent_id").is_none());

        replay(&conn, &lines[0], 1);
        // Before anything is recorded the guard knows no subagent.
        assert!(denied(&lead(to_helper_id), "only their coordinator"));
        // SubagentStart alone: the id works; `agent_type` ("helper") is never a name.
        replay(&conn, &lines[3], 2);
        assert_eq!(lead(to_helper_id), Allow);
        assert!(denied(&lead(to_helper), "only their coordinator"));
        // The Agent call's PostToolUse gives the name.
        replay(&conn, &lines[2], 3);
        assert_eq!(lead(to_helper), Allow);
        assert!(denied(
            &lead(&with(7, "helper [807e0b]")),
            "without a `[ref]`"
        ));
        // A teammate reports to its lead, and reaches its siblings.
        assert_eq!(lead(report), Allow);
        let mut sibling = report.clone();
        sibling["tool_input"]["to"] = "helper".into();
        assert_eq!(lead(&sibling), Allow);
        // The lead's own names are for its subagents only.
        assert!(denied(
            &lead(&with(7, "team-lead")),
            "only their coordinator"
        ));
        // A name an outside session has too: only by agent id.
        let mut spawned = lines[2].clone();
        spawned["tool_input"]["name"] = "spike-other".into();
        spawned["tool_response"]["name"] = "spike-other".into();
        spawned["tool_response"]["agentId"] = SPIKE_OTHER.into();
        replay(&conn, &spawned, 4);
        assert!(denied(&lead(to_spike), SPIKE_OTHER));
        assert_eq!(lead(&with(9, SPIKE_OTHER)), Allow);

        // Never another session's subagents, in either direction.
        assert!(denied(
            &lead(&with(7, "researcher")),
            "only their coordinator"
        ));
        assert!(denied(
            &lead(&with(7, "aresearcher-1")),
            "only their coordinator"
        ));
        let mut from_coord = with(7, "helper");
        from_coord["session_id"] = "c-sid".into();
        assert!(denied(
            &decide(&from_coord, Role::Coordinator),
            "not a thread"
        ));
        let mut from_other = with(7, HELPER);
        from_other["session_id"] = "o-sid".into();
        assert!(denied(&lead(&from_other), "only their coordinator"));
        from_other["tool_input"]["to"] = "researcher".into();
        assert_eq!(lead(&from_other), Allow);

        // A compaction keeps the process and its subagents.
        let mut compact = lines[17].clone();
        compact["source"] = "compact".into();
        replay(&conn, &compact, 5);
        assert_eq!(lead(to_helper), Allow);
        // After a respawn (SessionStart resume) the records are stale: helper is gone, and
        // spike-other now names the outside session (§1 F6).
        replay(&conn, &lines[17], 6);
        assert!(denied(&lead(to_helper), "only their coordinator"));
        assert!(denied(&lead(to_helper_id), "only their coordinator"));
        assert!(denied(&lead(to_spike), "only their coordinator"));
        // The other session's subagents stay its own.
        assert_eq!(lead(&from_other), Allow);

        // At spawn: a free name, or a name another session answers to.
        let spawn = &lines[1];
        assert_eq!(spawn["tool_name"], "Agent");
        assert_eq!(lead(spawn), Allow);
        for taken in ["chief-demo", "demo-other-2222", "spike-other"] {
            let mut p = spawn.clone();
            p["tool_input"]["name"] = taken.into();
            assert!(denied(&lead(&p), "Pick another name"), "{taken}");
        }
        let mut unnamed = spawn.clone();
        unnamed["tool_input"]
            .as_object_mut()
            .unwrap()
            .remove("name");
        assert_eq!(lead(&unnamed), Allow);
    }

    #[test]
    fn unmatched_hooks_are_recorded_without_a_session() {
        let (_dir, conn) = setup();
        let payload = serde_json::json!({
            "session_id": "11111111-2222-3333-4444-555555555555",
            "session_title": "someone-else",
            "hook_event_name": "UserPromptSubmit",
            "cwd": "/x",
            "prompt": "hello\nthere",
        });
        let ctx = Ctx {
            project: "demo",
            role: Role::Thread,
            now: 5,
        };
        let out = handle(&conn, &ctx, "UserPromptSubmit", &payload).unwrap();
        assert_eq!(out.session, None);
        assert!(out.notices.is_empty());
        let ev = db::recent_events(&conn, "demo", 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].session_name, None);
        assert_eq!(
            ev[0].session_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(ev[0].summary.as_deref(), Some("hello there"));
    }

    #[test]
    fn copies_and_other_projects_do_not_match() {
        let (dir, conn) = setup();
        let ctx = Ctx {
            project: "demo",
            role: Role::Thread,
            now: 5,
        };
        let start = |sid: &str| {
            serde_json::json!({
                "session_id": sid,
                "session_title": "spike-thread-a",
                "hook_event_name": "SessionStart",
                "cwd": "/x",
                "source": "startup",
            })
        };
        assert_eq!(
            handle(&conn, &ctx, "SessionStart", &start(THREAD_A))
                .unwrap()
                .session
                .as_deref(),
            Some("spike-thread-a")
        );
        // A copy (e.g. from `--resume`) shares the title but not the id.
        let copy = handle(&conn, &ctx, "SessionStart", &start("copy-id")).unwrap();
        assert_eq!(copy.session, None);
        assert_eq!(
            get(&conn, "spike-thread-a").session_id.as_deref(),
            Some(THREAD_A)
        );

        // The same session reported under another project's hook config.
        db::register_project(&conn, "other", dir.path(), 0).unwrap();
        let other = Ctx {
            project: "other",
            ..ctx
        };
        assert_eq!(
            handle(&conn, &other, "SessionStart", &start(THREAD_A))
                .unwrap()
                .session,
            None
        );
    }

    #[test]
    fn hook_overrides_a_stale_snapshot_until_the_next_poll() {
        let (_dir, conn) = setup();
        let mut row = get(&conn, "spike-thread-a");
        row.session_id = Some(THREAD_A.into());
        row.cc_state = Some("done".into());
        row.cc_status = Some("idle".into());
        row.cc_fresh = true;
        registry::rederive(&mut row);
        db::update_session(&conn, &row).unwrap();
        assert_eq!(get(&conn, "spike-thread-a").group_name, "idle");

        let ctx = Ctx {
            project: "demo",
            role: Role::Thread,
            now: 10,
        };
        let payload = serde_json::json!({"session_id": THREAD_A, "tool_name": "Bash", "cwd": "/x"});
        handle(&conn, &ctx, "PreToolUse", &payload).unwrap();
        let r = get(&conn, "spike-thread-a");
        assert_eq!((r.group_name.as_str(), r.cc_fresh), ("working", false));
        assert_eq!(r.cc_state.as_deref(), Some("done"), "kept for display");
    }

    #[test]
    fn stop_without_a_message_keeps_the_last_report() {
        let (_dir, conn) = setup();
        let ctx = Ctx {
            project: "demo",
            role: Role::Thread,
            now: 10,
        };
        let stop = |msg: Option<&str>| {
            serde_json::json!({
                "session_id": THREAD_A,
                "session_title": "spike-thread-a",
                "cwd": "/x/wt",
                "last_assistant_message": msg,
            })
        };
        handle(&conn, &ctx, "Stop", &stop(Some("first"))).unwrap();
        handle(&conn, &Ctx { now: 20, ..ctx }, "Stop", &stop(None)).unwrap();
        let r = get(&conn, "spike-thread-a");
        assert_eq!(
            (r.last_report.as_deref(), r.last_report_at),
            (Some("first"), Some(10))
        );
        assert_eq!(
            db::recent_reports(&conn, "spike-thread-a", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn guard_decision_uses_the_registry_and_the_snapshot() {
        let snapshot = crate::claude::parse_agents(include_str!(
            "../tests/fixtures/agents/agents-sample.json"
        ))
        .unwrap();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("20dc103a-4f18-4895-8bae-4454c193c7e6".into());
        let mut live = SessionRow::new("demo-hello-txt-1a2b", "demo", Role::Thread, 0);
        live.short_id = Some("f8627ea8".into()); // session id not resolved yet
        let mut asleep = SessionRow::new("demo-asleep-thread-77aa", "demo", Role::Thread, 0);
        asleep.session_id = Some("0a1b2c3d-1111-4222-8333-944455556666".into());
        // Registered here but its pid in the snapshot belongs to another session id: a copy.
        let mut copy = SessionRow::new("demo-curl-check-9f8e", "demo", Role::Thread, 0);
        copy.session_id = Some("not-the-listed-one".into());
        let rows = [coord, live, asleep, copy];
        let decide = |role, to: &str| {
            guard_decision("demo", role, "SendMessage", Some(to), &rows, &snapshot)
        };
        use guard::Decision::{Allow, Deny};
        assert_eq!(decide(Role::Coordinator, "demo-hello-txt-1a2b"), Allow);
        assert_eq!(
            decide(Role::Coordinator, "uds:/tmp/cc-socks/93669.sock"),
            Allow
        );
        assert_eq!(
            decide(Role::Coordinator, "demo-asleep-thread-77aa"),
            Deny(guard::ASLEEP.into())
        );
        assert_eq!(
            decide(Role::Coordinator, "demo-curl-check-9f8e"),
            Deny(guard::ASLEEP.into())
        );
        assert!(
            matches!(decide(Role::Coordinator, "some-other-project"), Deny(r) if r.contains("not a thread of this project"))
        );
        assert_eq!(decide(Role::Thread, "chief-demo"), Allow);
        assert_eq!(decide(Role::Thread, "uds:/tmp/cc-socks/74112.sock"), Allow);
        assert!(matches!(
            decide(Role::Thread, "some-other-project"),
            Deny(_)
        ));
        assert!(matches!(
            decide(Role::Thread, "uds:/tmp/cc-socks/70003.sock"),
            Deny(_)
        ));
    }

    fn agent_row(id: &str, sid: &str, name: &str, pid: Option<u32>) -> AgentRow {
        AgentRow {
            id: Some(id.into()),
            session_id: sid.into(),
            name: Some(name.into()),
            cwd: None,
            kind: Some("background".into()),
            started_at: None,
            state: Some("done".into()),
            status: pid.map(|_| "idle".into()),
            pid,
        }
    }

    /// The coordinator `chief-demo` (c-sid) and one thread `demo-fix-1a2b` (t-sid).
    fn guard_rows() -> Vec<SessionRow> {
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("c-sid".into());
        coord.short_id = Some("c1".into());
        let mut t = SessionRow::new("demo-fix-1a2b", "demo", Role::Thread, 0);
        t.session_id = Some("t-sid".into());
        t.short_id = Some("t1".into());
        vec![coord, t]
    }

    fn denied(d: &guard::Decision, part: &str) -> bool {
        matches!(d, guard::Decision::Deny(r) if r.contains(part))
    }

    #[test]
    fn guard_checks_that_the_name_still_belongs_to_the_thread() {
        // Our thread (t-sid, pid 201) now carries another name, and an outside session carries
        // its old one: a message by name would reach the outsider.
        let snap = [
            agent_row("c1", "c-sid", "chief-demo", Some(100)),
            agent_row("t1", "t-sid", "renamed", Some(201)),
            agent_row("o1", "o-sid", "demo-fix-1a2b", Some(300)),
        ];
        let decide = |to: &str| {
            guard_decision(
                "demo",
                Role::Coordinator,
                "SendMessage",
                Some(to),
                &guard_rows(),
                &snap,
            )
        };
        assert!(
            denied(&decide("demo-fix-1a2b"), "renamed"),
            "{:?}",
            decide("demo-fix-1a2b")
        );
        assert!(denied(
            &decide("uds:/tmp/cc-socks/300.sock"),
            "not the address"
        ));

        // Another session, asleep, carries the thread's name too: ambiguous.
        let snap = [
            agent_row("t1", "t-sid", "demo-fix-1a2b", Some(201)),
            agent_row("o1", "o-sid", "demo-fix-1a2b", None),
        ];
        let d = guard_decision(
            "demo",
            Role::Coordinator,
            "SendMessage",
            Some("demo-fix-1a2b"),
            &guard_rows(),
            &snap,
        );
        assert!(denied(&d, "ambiguous"), "{d:?}");
    }

    #[test]
    fn guard_checks_the_threads_coordinator() {
        let thread = |to: &str, snap: &[AgentRow]| {
            guard_decision(
                "demo",
                Role::Thread,
                "SendMessage",
                Some(to),
                &guard_rows(),
                snap,
            )
        };
        // Coordinator asleep; an unrelated live session is named chief-demo.
        let snap = [
            agent_row("c1", "c-sid", "chief-demo", None),
            agent_row("o1", "o-sid", "chief-demo", Some(300)),
        ];
        let d = thread("chief-demo", &snap);
        assert!(denied(&d, "recorded"), "the report is kept anyway: {d:?}");
        assert!(denied(
            &thread("uds:/tmp/cc-socks/300.sock", &snap),
            "only their coordinator"
        ));
        // Two live sessions with the coordinator's name.
        let snap = [
            agent_row("c1", "c-sid", "chief-demo", Some(100)),
            agent_row("o1", "o-sid", "chief-demo", Some(300)),
        ];
        assert!(denied(&thread("chief-demo", &snap), "ambiguous"));
        // The coordinator's session renamed.
        let snap = [agent_row("c1", "c-sid", "something-else", Some(100))];
        assert!(denied(&thread("chief-demo", &snap), "renamed"));
        // No coordinator row at all.
        let rows: Vec<_> = guard_rows().into_iter().skip(1).collect();
        let snap = [agent_row("o1", "o-sid", "chief-demo", Some(300))];
        let d = guard_decision(
            "demo",
            Role::Thread,
            "SendMessage",
            Some("chief-demo"),
            &rows,
            &snap,
        );
        assert!(denied(&d, "recorded"), "{d:?}");
    }

    #[test]
    fn guard_allows_threads_only_their_coordinators_exact_name() {
        let snap = [
            agent_row("c1", "c-sid", "chief-demo", Some(100)),
            agent_row("o1", "o-sid", "other-thread", Some(300)),
        ];
        let thread = |to: &str| {
            guard_decision(
                "demo",
                Role::Thread,
                "SendMessage",
                Some(to),
                &guard_rows(),
                &snap,
            )
        };
        assert_eq!(thread("chief-demo"), guard::Decision::Allow);
        assert_eq!(thread("uds:/tmp/cc-socks/100.sock"), guard::Decision::Allow);
        for to in [
            "other-thread",
            "other-thread [x]",
            "other-thread\u{feff}",
            "\u{feff}other-thread",
            "other-thread\u{200b}",
            "Other-Thread",
            "other-thread\t[x]",
            "other-thread[x]",
            "other-thread [x] [y]",
            "chief-demo [2c7859]",
            "chief-demo ",
            " chief-demo",
            "chief-demo\u{feff}",
            "Chief-Demo",
            // Not in the local `claude agents`: another machine, the cloud, a teammate, a
            // subagent.
            "researcher",
            "demo-fix-1a2b",
            // Addresses other than the coordinator's socket in exactly Claude Code's form.
            "uds:100.sock",
            "uds:/elsewhere/100.sock",
            "uds:/tmp/cc-socks/0100.sock",
            "uds:/tmp/cc-socks/+100.sock",
            "uds:/tmp/cc-socks//100.sock",
            "uds:/tmp/cc-socks/100.sock [r]",
            "uds:/tmp/cc-socks/100.sock\n",
            "uds:/tmp/cc-socks/300.sock",
        ] {
            assert!(
                matches!(thread(to), guard::Decision::Deny(_)),
                "{to:?} allowed"
            );
        }
        assert!(denied(&thread("chief-demo [2c7859]"), "without a"));
        assert!(denied(&thread("other-thread[x]"), "without a"));
    }

    #[test]
    fn guard_allows_the_coordinator_only_exact_thread_names() {
        let snap = [
            agent_row("c1", "c-sid", "chief-demo", Some(100)),
            agent_row("t1", "t-sid", "demo-fix-1a2b", Some(201)),
            agent_row("o1", "o-sid", "other-thread", Some(300)),
        ];
        let coord = |to: &str| {
            guard_decision(
                "demo",
                Role::Coordinator,
                "SendMessage",
                Some(to),
                &guard_rows(),
                &snap,
            )
        };
        assert_eq!(coord("demo-fix-1a2b"), guard::Decision::Allow);
        assert_eq!(coord("uds:/tmp/cc-socks/201.sock"), guard::Decision::Allow);
        for to in [
            "demo-fix-1a2b [2c7859]",
            "demo-fix-1a2b [ref-of-other-thread]",
            " demo-fix-1a2b",
            "demo-fix-1a2b ",
            "demo-fix-1a2b\u{feff}",
            "Demo-Fix-1a2b",
            "demo-fix-1a2b[x]",
            "uds:201.sock",
            "uds:/elsewhere/201.sock",
            "uds:/tmp/cc-socks/0201.sock",
            "uds:/tmp/cc-socks/201.sock [r]",
            "uds:/tmp/cc-socks/201.sock ",
            "uds:/tmp/cc-socks/100.sock",
        ] {
            assert!(
                matches!(coord(to), guard::Decision::Deny(_)),
                "{to:?} allowed"
            );
        }
        assert!(denied(&coord("demo-fix-1a2b [2c7859]"), "without a"));
    }

    #[test]
    fn error_log_appends_single_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/hook-errors.log");
        log_error(&log, "Stop", "demo", "first\nsecond");
        log_error(&log, "guard", "demo", "boom");
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("hook Stop --project demo: first | second"));
    }
}
