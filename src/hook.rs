//! `chief hook <event>` (ARCHITECTURE §2, §4, §5, §6, §8).

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Value, json};

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
/// appended to the hook error log. Prints only the guard decision, the coordinator's
/// UserPromptSubmit context, and for §5 "Background work" the denial of a subagent's background
/// command and the Stop decision that asks a thread for its final report. The guard fails
/// closed: any error or panic in it denies, and so does running past its deadline; everything
/// else fails open.
pub fn run(event: &str, project: &str, role: Role) {
    // The default panic hook writes to stderr, which Claude Code may show in the session.
    std::panic::set_hook(Box::new(|_| {}));
    if event == "guard" {
        start_guard_watchdog(project, guard_deadline());
    }
    // Read here, not in `dispatch`: a guard that fails still records what the payload says.
    let mut input = String::new();
    let read = std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the hook payload");
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        read?;
        dispatch(event, project, role, &input)
    }));
    let (out, error) = match result {
        Ok(Ok(out)) => (out, None),
        Ok(Err(e)) => (None, Some(format!("{e:#}"))),
        Err(panic) => (None, Some(format!("panic: {}", panic_message(&*panic)))),
    };
    let out = match (&error, event) {
        (Some(error), "guard") => Some(guard_failed(project, &input, error)),
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
fn dispatch(event: &str, project: &str, role: Role, input: &str) -> Result<Option<String>> {
    if event == "guard" {
        return guard(project, role, input);
    }
    let payload: Value = serde_json::from_str(input).context("parsing the hook payload")?;
    let conn = db::open(&paths::db())?;
    if event == "Stop" {
        learn_session_id(&conn, project, &payload);
    }
    let ctx = Ctx {
        project,
        role,
        now: db::now_ms(),
    };
    let outcome = handle(&conn, &ctx, event, &payload)?;
    // A thread kept going for its final report finishes on a later Stop, which notifies then.
    let block = match (outcome.final_report, outcome.session.as_deref()) {
        (true, Some(thread)) => final_report_block(&conn, project, thread).unwrap_or_else(|e| {
            log_error(
                &paths::hook_error_log(),
                event,
                project,
                &format!("not asking {thread} for its final report: {e:#}"),
            );
            None
        }),
        _ => None,
    };
    let held = outcome.held.iter().filter(|_| block.is_none());
    let notices: Vec<_> = outcome.notices.iter().chain(held).collect();
    if !notices.is_empty() && notify_enabled(project) {
        for notice in notices {
            send_notice(notice);
        }
    }
    if block.is_some() {
        return Ok(block);
    }
    if event == "PreToolUse"
        && let Some(session) = outcome.session.as_deref()
        && subagent_background_command(&conn, session, &payload)?
    {
        return Ok(Some(guard::deny_json(SUBAGENT_BACKGROUND_REASON)));
    }
    if role == Role::Coordinator && event == "UserPromptSubmit" {
        return coordinator_context(&conn, project).map(Some);
    }
    Ok(None)
}

/// §4 "Matching a hook to a session": after `/clear` a session runs under a new session id,
/// which no row has until a sync finds it under the row's short id (§5), and a Stop carries no
/// title to match by. A Stop in that window would lose its report, so a Stop whose session id no
/// row has syncs the project first, as a board poll would (bounded like the coordinator's
/// context). Only then, and only while the project has a session the sync could match: other
/// Stops never wait on `claude agents`. A failure is logged, and the Stop is recorded unmatched.
fn learn_session_id(conn: &Connection, project: &str, payload: &Value) {
    let Some(sid) = payload.get("session_id").and_then(Value::as_str) else {
        return;
    };
    let unknown = || -> Result<bool> {
        if db::get_session_by_session_id(conn, sid)?.is_some() {
            return Ok(false);
        }
        Ok(db::list_sessions(conn, project, None)?
            .iter()
            .any(|r| r.short_id.is_some() && r.cleaned_at.is_none() && r.removed_at.is_none()))
    };
    let failed = match unknown() {
        Ok(false) => None,
        Ok(true) => {
            let claude = project_claude(project);
            accounts::refresh(conn, project, &claude, CONTEXT_AGENTS_TIMEOUT, db::now_ms)
                .map(|listings| listings.error())
                .unwrap_or_else(|e| Some(format!("{e:#}")))
        }
        Err(e) => Some(format!("{e:#}")),
    };
    if let Some(e) = failed {
        log_error(
            &paths::hook_error_log(),
            "Stop",
            project,
            &format!("couldn't look up session {sid} in claude agents: {e}"),
        );
    }
}

/// A Bash call with `run_in_background` from one of `session`'s recorded subagents (§5
/// "Background work"). Claude Code doesn't wake a subagent when its background command ends
/// (§1), so the subagent would stall with its work unfinished, and its lead would look done.
/// Other agent ids, such as an agent auto mode runs (§1), are left alone.
pub fn subagent_background_command(
    conn: &Connection,
    session: &str,
    payload: &Value,
) -> Result<bool> {
    let field = |k: &str| payload.get(k).and_then(Value::as_str);
    let (Some(sid), Some(agent)) = (field("session_id"), field("agent_id")) else {
        return Ok(false);
    };
    let background = field("tool_name") == Some("Bash")
        && payload
            .pointer("/tool_input/run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if !background {
        return Ok(false);
    }
    Ok(db::teammates(conn, session, sid)?
        .iter()
        .any(|t| t.agent_id == agent))
}

/// Why chief denies a subagent's background command; the subagent reads it as the tool result.
pub const SUBAGENT_BACKGROUND_REASON: &str = "Subagents can't run commands in the background \
    here: Claude Code doesn't wake a subagent when its background command ends, so your work \
    would stop unfinished and nobody would notice. Run the command in the foreground instead, \
    with a timeout long enough for it (at most 600000 ms). A server you need for a check starts \
    and stops inside that one command. If the command needs longer than 10 minutes, or has to \
    keep running after your turn, don't start it: end your turn and say so, and your lead will \
    run it.";

/// §5 "Background work": the Stop decision that keeps a thread going to send its coordinator
/// the final report the coordinator's early idle notice left out, to the address the guard lets
/// it use (§6): the coordinator's name, or its socket address for a thread in another account
/// (§3). None when the coordinator has no live session to send it to; it reads the board when
/// it next runs.
fn final_report_block(conn: &Connection, project: &str, thread: &str) -> Result<Option<String>> {
    let rows = db::list_sessions(conn, project, None)?;
    let (Some(row), Some(coordinator)) = (
        rows.iter().find(|r| r.name == thread),
        rows.iter().find(|r| r.role == Role::Coordinator.as_str()),
    ) else {
        return Ok(None);
    };
    let claude = project_claude(project);
    let project_account = claude.account.clone();
    let listings = accounts::list(
        &claude,
        std::slice::from_ref(&project_account),
        CONTEXT_AGENTS_TIMEOUT,
        db::now_ms,
    );
    let snapshot = listings
        .project()
        .context("the project's account was listed")?
        .agents()?;
    let elsewhere = !claude::same_account(
        row.account(project_account.as_deref()).as_deref(),
        project_account.as_deref(),
    );
    let member = guard::Member {
        name: coordinator.name.clone(),
        session_id: coordinator.session_id.clone(),
        short_id: coordinator.short_id.clone(),
        resolved: coordinator.resolved,
        cleaned: coordinator.cleaned_at.is_some(),
        removed: coordinator.removed_at.is_some(),
        elsewhere: None,
    };
    let name = names::coordinator_name(project);
    Ok(
        guard::coordinator_address(&member, &name, snapshot, elsewhere)
            .map(|a| final_report_json(&a)),
    )
}

/// The Stop hook output that holds the turn open with `reason` as the next instruction (§1).
pub fn final_report_json(address: &str) -> String {
    json!({
        "decision": "block",
        "reason": format!(
            "Everything you waited on has finished. You ended an earlier turn while a subagent \
             still worked, so your coordinator was told you were idle and doesn't know you're \
             done. Send it your final report now with SendMessage (to: \"{address}\"): \
             what you did, the branch and commit, the pull request URL if any, test results, and \
             what you need from the user. Then end your turn."
        ),
    })
    .to_string()
}

/// §6: gathers the registry rows, the caller's subagents and fresh `claude agents` listings,
/// decides, and records a denial. Returns the deny JSON, or None to allow.
fn guard(project: &str, role: Role, input: &str) -> Result<Option<String>> {
    let payload: Value = serde_json::from_str(input).context("parsing the hook payload")?;
    let call = Call::from_payload(&payload)?;
    let conn = db::open(&paths::db())?;
    let rows = db::list_sessions(&conn, project, None)?;
    let caller_sid = payload.get("session_id").and_then(Value::as_str);
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(Path::new);
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
            let place = caller_place(&rows, caller_sid, transcript, project_account.as_deref());
            want(place.account(project_account.as_deref()));
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
            transcript,
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

/// The account the calling session runs in (§3, §6), where the names it sends reach sessions
/// (§1).
#[derive(Debug, Clone, Copy)]
enum Place<'r> {
    /// The project's: the coordinator, or a caller chief doesn't know while every thread that
    /// may still run is there.
    Project,
    /// The account of this thread: the caller itself, or a thread in the account whose folder
    /// holds the caller's transcript.
    Of(&'r SessionRow),
    /// A caller chief doesn't know, while threads of the project run in other accounts.
    Unknown,
}

impl Place<'_> {
    /// Where its names reach sessions, as far as chief can tell: the project's when unknown.
    fn account(&self, project: Option<&Path>) -> Option<std::path::PathBuf> {
        match self {
            Place::Of(row) => row.account(project),
            Place::Project | Place::Unknown => project.map(Path::to_path_buf),
        }
    }
}

/// Where the session calling the guard runs: a registered thread's own account, the project's
/// for the coordinator. A caller that matches no row (a thread whose session id changed with
/// `/clear`, before a hook or a sync stores the new one) is placed by its `transcript_path`,
/// which Claude Code keeps in its account's folder (`<folder>/projects/…`); when that doesn't
/// place it and a thread of the project that isn't cleaned up runs in another account, it is
/// Unknown, so the guard won't let it message its coordinator by name (§6).
fn caller_place<'r>(
    rows: &'r [SessionRow],
    caller: Option<&str>,
    transcript: Option<&Path>,
    project: Option<&Path>,
) -> Place<'r> {
    let matched = caller.and_then(|sid| rows.iter().find(|r| r.session_id.as_deref() == Some(sid)));
    if let Some(row) = matched {
        if row.role == Role::Thread.as_str() {
            return Place::Of(row);
        }
        return Place::Project;
    }
    // A removed session can come back (§5), so only cleaned-up threads are out of the question.
    let elsewhere: Vec<&SessionRow> = rows
        .iter()
        .filter(|r| r.role == Role::Thread.as_str() && r.cleaned_at.is_none())
        .filter(|r| !claude::same_account(r.account(project).as_deref(), project))
        .collect();
    if elsewhere.is_empty() {
        return Place::Project;
    }
    let Some(transcript) = transcript else {
        return Place::Unknown;
    };
    if transcript_in(transcript, project) {
        return Place::Project;
    }
    elsewhere
        .into_iter()
        .find(|r| transcript_in(transcript, r.account(project).as_deref()))
        .map_or(Place::Unknown, Place::Of)
}

/// Whether `transcript` is in `account`'s transcript folder, `<account folder>/projects/`.
fn transcript_in(transcript: &Path, account: Option<&Path>) -> bool {
    use std::path::Component;
    // `starts_with` compares components, and `..` would climb out of the folder.
    if !transcript.is_absolute() || transcript.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let projects = claude::claude_dir(account).join("projects");
    if transcript.starts_with(&projects) {
        return true;
    }
    // An account folder given through a symlink, or `/tmp` for `/private/tmp` on macOS.
    match (
        transcript.parent().map(Path::canonicalize),
        projects.canonicalize(),
    ) {
        (Some(Ok(folder)), Ok(projects)) => folder.starts_with(projects),
        _ => false,
    }
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
        transcript: None,
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
    /// The payload's `transcript_path`, which places a caller chief doesn't know yet.
    pub transcript: Option<&'a Path>,
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
        let label = shown_account(r, account.as_deref());
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
    let place = caller_place(rows, accounts.caller, accounts.transcript, accounts.project);
    // Where a name from the caller can reach someone (§1): its own account, and the project's.
    let own = place.account(accounts.project);
    let caller_label = match place {
        Place::Of(r) if !claude::same_account(own.as_deref(), accounts.project) => {
            Some(shown_account(r, own.as_deref()))
        }
        _ => None,
    };
    let caller_account = match (place, caller_label.as_deref()) {
        (Place::Unknown, _) => guard::CallerAccount::Unknown,
        (_, Some(label)) => guard::CallerAccount::Other(label),
        _ => guard::CallerAccount::Same,
    };
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
        caller_account,
        from_subagent: call.from_subagent,
        teammates,
        listed: &listed,
    })
}

/// A thread's account for messages: its agp label, else the folder.
fn shown_account(row: &SessionRow, account: Option<&Path>) -> String {
    row.account_shown()
        .map(str::to_string)
        .unwrap_or_else(|| claude::account_label(account))
}

/// The deny JSON for a guard that couldn't decide, recorded if the database still opens, with
/// as much of the call as the payload `input` gives: the tool, its target, and the caller when
/// its row can be read.
fn guard_failed(project: &str, input: &str, error: &str) -> String {
    let reason = format!("chief's message guard failed, so the message is blocked: {error}");
    let payload: Option<Value> = serde_json::from_str(input).ok();
    let call = payload.as_ref().and_then(|p| Call::from_payload(p).ok());
    let target = call.and_then(|c| match c.tool {
        "Agent" => c.name,
        _ => c.to,
    });
    let caller_sid = payload
        .as_ref()
        .and_then(|p| p.get("session_id"))
        .and_then(Value::as_str);
    if let Ok(conn) = db::open(&paths::db()) {
        let caller = caller_sid.and_then(|sid| {
            let rows = db::list_sessions(&conn, project, None).ok()?;
            let row = rows
                .into_iter()
                .find(|r| r.session_id.as_deref() == Some(sid))?;
            Some(row.name)
        });
        let _ = db::insert_guard_denial(
            &conn,
            &db::GuardDenial {
                project_slug: project.to_string(),
                session_name: caller,
                tool: call.map_or("unknown", |c| c.tool).to_string(),
                target: target.map(str::to_string),
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
    /// A thread's Stop that found its background work done after an early Stop: ask it for its
    /// final report (§5 "Background work").
    pub final_report: bool,
    /// The Stop's notice, sent only if the thread isn't asked for its report after all.
    pub held: Option<Notice>,
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
    let (row, final_report) = db::immediate(conn, |conn| {
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
            return Ok((None, false));
        };
        let before = (r.subagents_busy, r.background_running);
        if let Some(sid) = session_id {
            record_teammates(conn, &r.name, sid, event, payload, ctx.now)?;
            r.subagents_busy = db::busy_teammates(conn, &r.name, sid)?;
        }
        match kind {
            // A new process runs none of the old one's background work.
            Some(HookKind::SessionStart) if str_field("source") != Some("compact") => {
                r.background_running = 0;
                r.stopped_early_at = None;
            }
            // A session's own command wakes it when it ends, so the Stop after that recounts.
            Some(HookKind::Stop) => r.background_running = listed_tasks(payload, |k| k == "shell"),
            _ => {}
        }
        if let Some(kind) = kind
            && let Some(report) = apply(r, kind, payload, ctx.now)
        {
            db::insert_report(conn, &r.name, ctx.now, &report)?;
        }
        let final_report = kind == Some(HookKind::Stop) && final_report_due(r, payload, ctx.now);
        // SubagentStart and SubagentStop change only the background work.
        if kind.is_none() && (r.subagents_busy, r.background_running) != before {
            r.updated_at = ctx.now;
            registry::rederive(r);
        }
        db::update_session(conn, r)?;
        Ok((row, final_report))
    })?;

    let mut outcome = Outcome::default();
    if let Some(row) = row {
        if let Some(kind) = kind
            && let Some(n) = notice_for(ctx.project, &row, kind)
        {
            // A Stop while background work runs isn't the end of the work; one that asks for
            // the final report isn't the end of the turn.
            if kind != HookKind::Stop {
                outcome.notices.push(n);
            } else if final_report {
                outcome.held = Some(n);
            } else if !background_work(&row) {
                outcome.notices.push(n);
            }
        }
        outcome.final_report = final_report;
        outcome.session = Some(row.name);
    }
    Ok(outcome)
}

/// Subagents mid-turn or background commands running (§5 "Background work").
fn background_work(row: &SessionRow) -> bool {
    row.subagents_busy > 0 || row.background_running > 0
}

/// The `background_tasks` entries a Stop lists as running whose `type` passes `kind` (§1):
/// `shell` for a background command, `teammate` or `subagent` for a subagent.
fn listed_tasks(payload: &Value, kind: impl Fn(&str) -> bool) -> u32 {
    let field = |t: &Value, k: &str| t.get(k).and_then(Value::as_str).map(str::to_string);
    payload
        .get("background_tasks")
        .and_then(Value::as_array)
        .map_or(0, |tasks| {
            tasks
                .iter()
                .filter(|t| {
                    field(t, "type").is_some_and(|k| kind(&k))
                        && field(t, "status").as_deref() == Some("running")
                })
                .count() as u32
        })
}

/// The ids of the `background_tasks` entries of type `kind` a Stop lists as running.
fn listed_ids(payload: &Value, kind: &str) -> Vec<String> {
    fn field<'a>(t: &'a Value, k: &str) -> Option<&'a str> {
        t.get(k).and_then(Value::as_str)
    }
    payload
        .get("background_tasks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| field(t, "type") == Some(kind) && field(t, "status") == Some("running"))
        .filter_map(|t| field(t, "id").map(str::to_string))
        .collect()
}

/// §5 "Background work". A thread's Stop while an in-process teammate works told its
/// coordinator it was idle (§1), so it is marked; async agents and background commands keep it
/// busy, so they don't. The next Stop that finds nothing running clears the mark and is due to
/// ask for the final report. Not while a Stop hook's block runs (`stop_hook_active`): the mark
/// waits for a Stop of the thread's own.
fn final_report_due(row: &mut SessionRow, payload: &Value, now: i64) -> bool {
    if row.role != Role::Thread.as_str() {
        return false;
    }
    if row.subagents_busy > 0 && listed_tasks(payload, |kind| kind == "teammate") > 0 {
        row.stopped_early_at = Some(now);
    }
    if background_work(row) {
        return false;
    }
    let blocked = payload
        .get("stop_hook_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if row.stopped_early_at.is_none() || blocked {
        return false;
    }
    row.stopped_early_at = None;
    true
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
/// a SessionStart other than `compact` is a new process, which has none of them (§1). §5
/// "Background work": each is mid-turn from its SubagentStart to its SubagentStop.
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
        // Only the id: `agent_type` is the agent's name only when it was given one. It fires
        // at every turn of the subagent, which runs until its SubagentStop (§5).
        "SubagentStart" => {
            if let Some(agent) = text(Some(payload), "agent_id") {
                db::record_teammate(conn, session, session_id, agent, None, now)?;
                db::set_teammate_busy(conn, session, agent, true)?;
            }
        }
        "SubagentStop" => {
            if let Some(agent) = text(Some(payload), "agent_id") {
                db::set_teammate_busy(conn, session, agent, false)?;
            }
        }
        // The session's own Stop lists the subagents it still has: a teammate even while idle,
        // an async agent (a `subagent`, by its agent id) until its hand-back. So an agent it
        // doesn't list can't be mid-turn, whatever SubagentStop chief missed (one that hit the
        // busy timeout, say). A teammate is listed under a task id, not its agent id, and busy
        // or not, so while one is listed (or a task of a type chief doesn't know) nothing is
        // cleared.
        "Stop"
            if text(Some(payload), "agent_id").is_none()
                && payload.get("background_tasks").is_some_and(Value::is_array)
                && listed_tasks(payload, |k| k != "shell" && k != "subagent") == 0 =>
        {
            let running = listed_ids(payload, "subagent");
            let running: Vec<&str> = running.iter().map(String::as_str).collect();
            db::idle_teammates(conn, session, &running)?;
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
    use HookKind::*;
    let str_field = |k: &str| payload.get(k).and_then(Value::as_str);
    row.last_hook = Some(kind.as_str().to_string());
    row.last_hook_at = Some(now);
    row.cc_fresh = false;
    row.updated_at = now;
    if kind != IdleNotification {
        row.last_activity_at = now;
    }
    // A subagent's tool calls carry its lead's session id (§1) and so match the lead's row:
    // they show the process works, but they don't answer the lead's permission prompt (rule 3
    // keeps it waiting), and their cwd isn't the lead's.
    let own = str_field("agent_id").is_none_or(str::is_empty);
    if own && matches!(kind, UserPromptSubmit | PreToolUse | PostToolUse | Stop) {
        row.pending_permission = false;
    }
    if own
        && matches!(kind, PostToolUse | Stop)
        && let Some(cwd) = str_field("cwd")
    {
        row.cwd = Some(cwd.to_string());
    }
    if kind == PermissionNotification {
        row.pending_permission = true;
    }
    let mut report = None;
    if kind == Stop
        && let Some(text) = str_field("last_assistant_message").filter(|r| !r.trim().is_empty())
    {
        row.last_report = Some(text.to_string());
        row.last_report_at = Some(now);
        report = Some(text.to_string());
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
        // One write per line: hooks run at once, and `writeln!` writes each piece on its own, so
        // their pieces would interleave even in append mode.
        let line = format!(
            "{} hook {event} --project {project}: {error}\n",
            db::now_ms()
        );
        let _ = f.write_all(line.as_bytes());
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

    /// Three leads recorded live (§5 "Background work"): spike-lead2 messages its subagent
    /// between turns, spike-lead3 stops its subagent with TaskStop, spike-lead4 and its subagent
    /// each start a background command.
    fn subagent_wait_fixture() -> Vec<Value> {
        include_str!("../tests/fixtures/hooks/subagent-wait-events.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn wait_setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        for name in ["spike-lead2", "spike-lead3", "spike-lead4"] {
            db::insert_session(&conn, &SessionRow::new(name, "demo", Role::Thread, 0)).unwrap();
        }
        (dir, conn)
    }

    /// Replays `lines[range]` at times equal to their index; the outcome of each.
    fn replay_range(
        conn: &Connection,
        lines: &[Value],
        range: std::ops::RangeInclusive<usize>,
    ) -> Vec<Outcome> {
        range.map(|i| replay(conn, &lines[i], i as i64)).collect()
    }

    /// A lead that ends its turn while its subagent works was reported idle to its
    /// coordinator (§1): no notice then, and the Stop after its subagent's last turn asks for the
    /// final report, holding that Stop's notice. The Stop the block runs into notifies.
    #[test]
    fn a_thread_waiting_on_its_subagent_reports_once_it_is_done() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        let out = replay_range(&conn, &lines, 0..=24);
        let lead = |i: usize| {
            (
                out[i].notices.len(),
                out[i].final_report,
                out[i].held.is_some(),
            )
        };
        for early in [6, 17, 18] {
            assert_eq!(lead(early), (0, false, false), "Stop {early}");
        }
        assert_eq!(lead(21), (0, true, true));
        let held = &out[21].held.as_ref().unwrap().message;
        assert!(
            held.starts_with("spike-lead2 finished: noted: worker2 idle"),
            "{held}"
        );
        assert_eq!(lead(24), (1, false, false), "the block's own Stop notifies");
        let row = get(&conn, "spike-lead2");
        assert_eq!((row.subagents_busy, row.stopped_early_at), (0, None));
    }

    /// The board's count between the fixture's events, and the group it gives.
    #[test]
    fn subagents_at_work_keep_their_lead_working() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        let at = |i: usize| {
            replay(&conn, &lines[i], i as i64);
            let row = get(&conn, "spike-lead2");
            (row.subagents_busy, row.group_name)
        };
        for i in 0..=3 {
            at(i);
        }
        assert_eq!(at(4), (1, "working".into()));
        assert_eq!(at(6), (1, "working".into()), "its Stop doesn't idle it");
        assert_eq!(at(11), (0, "idle".into()));
        assert_eq!(at(15), (1, "working".into()));
        assert_eq!(at(20), (0, "idle".into()));
    }

    /// A subagent stopped with TaskStop: its SubagentStop ends its turn, and a Stop listing no
    /// subagent ends every turn even without one.
    #[test]
    fn a_stopped_subagent_is_done() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        replay_range(&conn, &lines, 25..=33);
        assert_eq!(get(&conn, "spike-lead3").subagents_busy, 1);
        replay(&conn, &lines[34], 34);
        assert_eq!(get(&conn, "spike-lead3").subagents_busy, 0);
        let out = replay(&conn, &lines[35], 35);
        assert_eq!((out.notices.len(), out.final_report), (1, false));

        let (_dir, conn) = wait_setup();
        replay_range(&conn, &lines, 25..=33);
        replay(&conn, &lines[35], 35);
        assert_eq!(get(&conn, "spike-lead3").subagents_busy, 0);
    }

    /// Background commands its Stops list as running keep a lead working (§5): two, then one
    /// after the subagent's finished, then none once its own woke it. Its subagent was between
    /// turns, and commands keep a lead busy (§1), so its coordinator heard nothing early and the
    /// last Stop is a plain finish. The fixture's subagent started its command in the
    /// background, which chief now denies, because its result never reached it (§1).
    #[test]
    fn background_commands_keep_their_lead_working() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        let out = replay_range(&conn, &lines, 36..=49);
        let at = |i: usize| (out[i - 36].notices.len(), out[i - 36].final_report);
        let row = get(&conn, "spike-lead4");
        assert_eq!((row.background_running, row.stopped_early_at), (0, None));
        assert_eq!(
            (at(46), at(47), at(49)),
            ((0, false), (0, false), (1, false))
        );

        let (_dir, conn) = wait_setup();
        replay_range(&conn, &lines, 36..=46);
        let row = get(&conn, "spike-lead4");
        assert_eq!((row.subagents_busy, row.background_running), (0, 2));
        assert_eq!(row.group_name, "working");
        assert_eq!(
            registry::background_work(&row).as_deref(),
            Some("2 background commands")
        );
        replay(&conn, &lines[47], 47);
        assert_eq!(get(&conn, "spike-lead4").background_running, 1);

        // A new process runs none of it, and its earlier Stop no longer calls for a report.
        let mut restart = lines[36].clone();
        restart["source"] = "resume".into();
        replay(&conn, &restart, 48);
        let row = get(&conn, "spike-lead4");
        assert_eq!(
            (
                row.subagents_busy,
                row.background_running,
                row.stopped_early_at
            ),
            (0, 0, None)
        );
        assert!(!replay(&conn, &lines[49], 49).final_report);
    }

    /// The second machine's flavour (§1): an async agent, listed as a `subagent` until its
    /// hand-back, and a lead's own background command. Both keep the lead busy, so its
    /// coordinator hears nothing early, and the Stop after them is a plain finish, notified.
    #[test]
    fn async_agents_and_commands_keep_their_lead_working() {
        let lines: Vec<Value> = include_str!("../tests/fixtures/hooks/async-wait-events.jsonl")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let setup = || {
            let (dir, conn) = wait_setup();
            for name in ["spike-sub2", "spike-sh2"] {
                db::insert_session(&conn, &SessionRow::new(name, "demo", Role::Thread, 0)).unwrap();
            }
            (dir, conn)
        };
        let row = |conn: &Connection, name: &str| {
            let r = get(conn, name);
            (r.subagents_busy, r.background_running, r.group_name)
        };
        let (_dir, conn) = setup();
        replay_range(&conn, &lines, 0..=11);
        assert_eq!(
            row(&conn, "spike-sub2"),
            (1, 0, "working".into()),
            "listed: running"
        );
        assert_eq!(row(&conn, "spike-sh2"), (0, 1, "working".into()));

        let (_dir, conn) = setup();
        let out = replay_range(&conn, &lines, 0..=22);
        let stop = |i: usize| (out[i].notices.len(), out[i].final_report);
        // Early Stops while the command (9) and the subagent (11) run; then plain finishes.
        assert_eq!((stop(9), stop(11)), ((0, false), (0, false)));
        assert_eq!((stop(14), stop(20)), ((1, false), (1, false)));
        for name in ["spike-sub2", "spike-sh2"] {
            assert_eq!(get(&conn, name).stopped_early_at, None, "{name}");
        }
        assert_eq!(row(&conn, "spike-sub2"), (0, 0, "idle".into()));
    }

    /// A subagent's tool calls carry its lead's session id (§1), so they match the lead's row.
    /// They don't answer the lead's permission prompt (rule 3 comes before rule 5), and their
    /// cwd isn't the lead's; the lead's own next hook clears the prompt.
    #[test]
    fn teammate_tool_calls_keep_the_leads_permission_prompt() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        replay_range(&conn, &lines, 0..=5);
        let lead_cwd = get(&conn, "spike-lead2").cwd;
        assert!(lead_cwd.is_some());
        let mut prompt = lines[1].clone();
        prompt["hook_event_name"] = "Notification".into();
        prompt["notification_type"] = "permission_prompt".into();
        prompt["message"] = "Claude needs your permission".into();
        replay(&conn, &prompt, 6);
        let waiting = |conn: &Connection| {
            let r = get(conn, "spike-lead2");
            (r.pending_permission, r.group_name, r.waiting_reason)
        };
        let permission = (true, "waiting".to_string(), Some("permission".to_string()));
        assert_eq!(waiting(&conn), permission);

        // The teammate works on: its PostToolUse, PreToolUse and PostToolUse (7, 8, 9).
        for (i, line) in lines.iter().enumerate().take(10).skip(7) {
            let mut call = line.clone();
            assert!(call["agent_id"].as_str().is_some_and(|a| !a.is_empty()));
            call["cwd"] = "/elsewhere/teammate-worktree".into();
            replay(&conn, &call, 10 + i as i64);
            assert_eq!(waiting(&conn), permission, "line {i}");
        }
        let r = get(&conn, "spike-lead2");
        assert_eq!(r.cwd, lead_cwd, "a teammate's cwd isn't the lead's");
        assert_eq!(
            (r.last_hook.as_deref(), r.last_hook_at),
            (Some("PostToolUse"), Some(19))
        );
        // A fresh `claude agents` row saying busy changes nothing either.
        let mut polled = r.clone();
        polled.cc_status = Some("busy".into());
        polled.cc_state = Some("working".into());
        polled.cc_fresh = true;
        registry::rederive(&mut polled);
        assert_eq!(polled.waiting_reason.as_deref(), Some("permission"));

        // The lead's own next call (10, no agent_id) ends the prompt.
        replay(&conn, &lines[10], 20);
        assert_eq!(waiting(&conn), (false, "working".to_string(), None));
    }

    /// §5 "Background work": a lost SubagentStop doesn't keep an async agent mid-turn for good.
    /// The lead's own Stop lists each async agent by its agent id until its hand-back, so an
    /// agent it doesn't list is done; a listed one stays busy.
    #[test]
    fn a_lost_subagent_stop_ends_at_the_leads_next_stop() {
        let lines: Vec<Value> = include_str!("../tests/fixtures/hooks/async-wait-events.jsonl")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let (_dir, conn) = wait_setup();
        for name in ["spike-sub2", "spike-sh2"] {
            db::insert_session(&conn, &SessionRow::new(name, "demo", Role::Thread, 0)).unwrap();
        }
        replay_range(&conn, &lines, 0..=11);
        let first = lines[7]["agent_id"].as_str().unwrap();
        assert_eq!(lines[11]["background_tasks"][0]["id"], first);
        // A second async agent starts; the first one's SubagentStop (19) never lands.
        let mut start = lines[7].clone();
        start["agent_id"] = "asecond-1".into();
        replay(&conn, &start, 12);
        assert_eq!(get(&conn, "spike-sub2").subagents_busy, 2);

        let mut stop = lines[20].clone();
        stop["background_tasks"] = serde_json::json!([
            {"id": "asecond-1", "type": "subagent", "status": "running", "description": "x"},
            {"id": "b1", "type": "shell", "status": "running", "description": "y"},
        ]);
        let out = replay(&conn, &stop, 13);
        let row = get(&conn, "spike-sub2");
        assert_eq!((row.subagents_busy, row.background_running), (1, 1));
        assert_eq!((out.notices.len(), row.group_name.as_str()), (0, "working"));
        // A teammate listed (under a task id, idle or not) leaves every turn as it is.
        let mut start = lines[7].clone();
        start["agent_id"] = "athird-2".into();
        replay(&conn, &start, 14);
        stop["background_tasks"] = serde_json::json!([
            {"id": "t5yzdiksp", "type": "teammate", "status": "running", "description": "z"},
        ]);
        replay(&conn, &stop, 15);
        assert_eq!(get(&conn, "spike-sub2").subagents_busy, 2);
        stop["background_tasks"] = serde_json::json!([]);
        replay(&conn, &stop, 16);
        let row = get(&conn, "spike-sub2");
        assert_eq!((row.subagents_busy, row.group_name.as_str()), (0, "idle"));
    }

    /// A Stop without `background_tasks` says nothing about subagents (they keep their turn)
    /// and lists no running command; its report is kept as any Stop's.
    #[test]
    fn a_stop_without_background_tasks() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        replay_range(&conn, &lines, 36..=40);
        replay(&conn, &lines[46], 46);
        let row = get(&conn, "spike-lead4");
        assert_eq!((row.subagents_busy, row.background_running), (1, 2));

        let mut stop = lines[49].clone();
        stop.as_object_mut().unwrap().remove("background_tasks");
        stop["last_assistant_message"] = "still waiting on runner".into();
        let out = replay(&conn, &stop, 50);
        let row = get(&conn, "spike-lead4");
        assert_eq!(
            (
                row.subagents_busy,
                row.background_running,
                row.group_name.as_str()
            ),
            (1, 0, "working")
        );
        assert_eq!(row.last_report.as_deref(), Some("still waiting on runner"));
        // Still waiting: no notice, no report asked for, the early Stop (46) still marked.
        assert_eq!(
            (out.notices.len(), out.final_report, row.stopped_early_at),
            (0, false, Some(46))
        );
    }

    /// Only a recorded subagent's Bash call in the background (§5).
    #[test]
    fn subagent_background_commands_are_denied() {
        let (_dir, conn) = wait_setup();
        let lines = subagent_wait_fixture();
        let denied =
            |payload: &Value| subagent_background_command(&conn, "spike-lead4", payload).unwrap();
        replay_range(&conn, &lines, 36..=40);
        assert!(denied(&lines[43]), "the subagent's");
        assert!(!denied(&lines[41]), "the lead's own");
        let mut other_tool = lines[43].clone();
        other_tool["tool_name"] = "Monitor".into();
        assert!(!denied(&other_tool));
        let mut foreground = lines[43].clone();
        foreground["tool_input"]["run_in_background"] = false.into();
        assert!(!denied(&foreground));
        // An agent chief has no SubagentStart for, like the one auto mode runs (§1).
        let mut unknown = lines[43].clone();
        unknown["agent_id"] = "a0627549f1501fc2e".into();
        assert!(!denied(&unknown));
        let mut blank = lines[43].clone();
        blank["agent_id"] = "".into();
        assert!(!denied(&blank));
    }

    #[test]
    fn final_report_json_blocks_with_the_address() {
        let v: Value =
            serde_json::from_str(&final_report_json("uds:/tmp/cc-socks/7.sock")).unwrap();
        assert_eq!(v["decision"], "block");
        let reason = v["reason"].as_str().unwrap();
        assert!(
            reason.contains("SendMessage (to: \"uds:/tmp/cc-socks/7.sock\")"),
            "{reason}"
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
                transcript: None,
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

    /// §6: a thread caller whose session id matches no row (after `/clear`, before a hook or a
    /// sync stores the new id) is placed by its transcript; while a thread runs in another
    /// account and the transcript doesn't place it, it can't use its coordinator's name.
    #[test]
    fn guard_places_a_caller_it_does_not_know() {
        let accounts_dir = tempfile::tempdir().unwrap();
        let project = accounts_dir.path().join("project");
        let pb2 = accounts_dir.path().join("pb2");
        let mut rows = guard_rows();
        let mut far = SessionRow::new("demo-far-3333", "demo", Role::Thread, 0);
        far.session_id = Some("far-sid".into());
        far.account_id = Some("pb2".into());
        far.account_label = Some("PB2".into());
        far.account_dir = Some(pb2.to_string_lossy().into_owned());
        rows.push(far);
        let snap = [agent_row("c1", "c-sid", "chief-demo", Some(100))];
        let decide = |rows: &[SessionRow], caller: &str, transcript: Option<&Path>, to: &str| {
            let accounts = Accounts {
                project: Some(&project),
                snapshot: &snap,
                others: &[],
                caller: Some(caller),
                transcript,
            };
            let call = Call {
                tool: "SendMessage",
                to: Some(to),
                ..Call::default()
            };
            guard_decision_in("demo", Role::Thread, &call, rows, &accounts, &[])
        };
        let in_project = project.join("projects/-repo/new-sid.jsonl");
        let in_pb2 = pb2.join("projects/-repo/new-sid.jsonl");
        let allowed = guard::Decision::Allow;

        // Known callers keep their account, whatever the transcript says.
        assert_eq!(decide(&rows, "t-sid", Some(&in_pb2), "chief-demo"), allowed);
        let d = decide(&rows, "far-sid", Some(&in_project), "chief-demo");
        assert!(denied(&d, "another Claude account (PB2)"), "{d:?}");

        // Unknown: by its transcript's folder.
        assert_eq!(
            decide(&rows, "new-sid", Some(&in_project), "chief-demo"),
            allowed
        );
        let d = decide(&rows, "new-sid", Some(&in_pb2), "chief-demo");
        assert!(denied(&d, "another Claude account (PB2)"), "{d:?}");
        // Not placed: no transcript, one elsewhere, or one that climbs out of the folder.
        let nowhere = accounts_dir.path().join("other/projects/x.jsonl");
        let climbs = project.join("projects/../../pb2/projects/x.jsonl");
        for transcript in [None, Some(nowhere.as_path()), Some(climbs.as_path())] {
            let d = decide(&rows, "new-sid", transcript, "chief-demo");
            assert!(
                denied(&d, "can't tell whether you run in"),
                "{transcript:?}: {d:?}"
            );
            assert!(denied(&d, "from=\"uds:"), "{d:?}");
            // Its coordinator's exact address still works.
            assert_eq!(
                decide(&rows, "new-sid", transcript, "uds:/tmp/cc-socks/100.sock"),
                allowed
            );
        }
        // A thread elsewhere that is resolved, or was removed outside chief, may still run.
        for change in [
            (|r: &mut SessionRow| r.resolved = true) as fn(&mut SessionRow),
            |r| r.removed_at = Some(1),
        ] {
            let mut rows = rows.clone();
            change(&mut rows[2]);
            let d = decide(&rows, "new-sid", None, "chief-demo");
            assert!(denied(&d, "can't tell"), "{d:?}");
        }
        // Once it is cleaned up, every thread that may run is in the project's account.
        let mut rows = rows.clone();
        rows[2].cleaned_at = Some(1);
        assert_eq!(decide(&rows, "new-sid", None, "chief-demo"), allowed);
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
