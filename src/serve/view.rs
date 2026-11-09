//! What the dashboard reads and does (ARCHITECTURE §14 "API"), as plain functions over a
//! project's tool context: the board as JSON, one session's detail, and the actions the board's
//! keys run. `http.rs` only routes to these.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cleanup::{self, Mode, Outcome};
use crate::db::{self, SessionRow};
use crate::pr::Gh;
use crate::state::{Group, Role};
use crate::text::one_line;
use crate::tools::{self, NameArgs, ToolCtx};
use crate::{names, registry, routines, waiting};

/// Events read for what a thread does or asks.
const ACTIVITY_EVENTS: usize = 12;
const DETAIL_EVENTS: usize = 40;
const DETAIL_REPORTS: usize = 50;
const PREVIEW_CHARS: usize = 240;

fn group_of(row: &SessionRow) -> Group {
    Group::parse(&row.group_name).unwrap_or(Group::Idle)
}

fn pr(row: &SessionRow) -> Value {
    match &row.pr_url {
        None => Value::Null,
        Some(url) => json!({
            "url": url,
            "state": row.pr_state,
            "draft": row.pr_draft.unwrap_or(false),
            "review": row.pr_review,
        }),
    }
}

/// A session as the dashboard lists it. `conn` reads what a working or waiting thread does or
/// asks, as the board's detail does (§10).
fn session(conn: &rusqlite::Connection, row: &SessionRow) -> Result<Value> {
    let group = group_of(row);
    let mut out = json!({
        "name": row.name,
        "role": row.role,
        "title": row.title.as_deref().map(|t| one_line(t, 120)),
        "group": group.as_str(),
        "waiting_reason": row.waiting_reason,
        // No process, though it has (or had) a session: Enter on it wakes it.
        "asleep": row.pid.is_none() && !row.resolved && row.short_id.is_some(),
        "started": row.short_id.is_some(),
        "running": registry::background_work(row).filter(|_| !row.resolved),
        "branch": row.branch,
        "pr": pr(row),
        "account": row.account_shown(),
        "routine": row.routine,
        "repo": row.repo_name,
        "model": row.model,
        "effort": row.effort,
        "last_activity_at": row.last_activity_at,
        "last_report_at": row.last_report_at,
        "report_preview": row.last_report.as_deref().map(|r| one_line(r, PREVIEW_CHARS)),
        "resolved_reason": row.resolved_reason.as_deref().filter(|_| row.resolved),
        "cleaned": row.cleaned_at.is_some(),
        "removed": row.removed_at.is_some(),
        "cleanup_blocked": row.cleanup_blocked,
        "limit": row.limit_note.as_deref().filter(|_| registry::limited(row)),
        "moved": row.move_note,
    });
    if row.role == Role::Thread.as_str() && matches!(group, Group::Working | Group::Waiting) {
        let events = db::session_events(conn, &row.name, ACTIVITY_EVENTS)?;
        if let Some((label, text, _)) = waiting::asks(row, &events) {
            out["asks"] = json!({"label": label, "text": one_line(&text, 300)});
        } else if let Some(call) = events.iter().find(|e| {
            e.agent_id.is_none() && matches!(e.hook.as_str(), "PreToolUse" | "PostToolUse")
        }) {
            out["doing"] = json!({
                "text": call.summary.as_deref().map(|s| one_line(s, 160)),
                "at": call.ts,
                "running": call.hook == "PreToolUse",
            });
        }
    }
    Ok(out)
}

/// The whole board of the context's project: what the sidebar draws. With `refresh`, the rows
/// are first brought up to date from `claude agents` in every account the project uses, as a
/// board poll does (§5); a failed listing is said in `error` and the stored rows are shown.
pub fn board(ctx: &ToolCtx, refresh: bool) -> Result<Value> {
    let slug = ctx.config.slug.as_str();
    let error = if refresh {
        match tools::refresh(ctx) {
            Ok(listings) => listings.error(),
            Err(e) => Some(format!("{e:#}")),
        }
    } else {
        None
    };
    let rows = db::list_sessions(ctx.conn, slug, None)?;
    let coordinator = match rows.iter().find(|r| r.role == Role::Coordinator.as_str()) {
        Some(row) => session(ctx.conn, row)?,
        None => Value::Null,
    };
    let mut threads: Vec<SessionRow> = rows
        .into_iter()
        .filter(|r| r.role == Role::Thread.as_str())
        .collect();
    tools::board_order(&mut threads);
    // What each routine's run said it came to rides on its thread.
    let verdicts = routines::verdicts(ctx.conn, slug)?;
    let threads = threads
        .iter()
        .map(|t| {
            let mut out = session(ctx.conn, t)?;
            if let Some(finding) = verdicts.get(&t.name) {
                out["finding"] = finding.to_json();
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>>>()?;
    let routines = routines::list(ctx.conn, slug)?
        .iter()
        .map(|r| routines::view(ctx.conn, r))
        .collect::<Result<Vec<Value>>>()?;
    Ok(json!({
        "project": {
            "slug": slug,
            "name": ctx.config.name,
            "goal": ctx.config.goal,
            "repos": ctx.config.repos.iter().map(|r| &r.name).collect::<Vec<_>>(),
            "default_repo": ctx.config.threads.default_repo,
            "thread_model": ctx.config.threads.model,
            "thread_effort": ctx.config.threads.effort,
            "max_parallel": ctx.config.threads.max_parallel,
        },
        "groups": Group::ORDER.map(|g| json!({"key": g.as_str(), "label": g.label()})),
        "coordinator": coordinator,
        "coordinator_name": names::coordinator_name(slug),
        "threads": threads,
        "routines": routines,
        "guard_denials": db::count_guard_denials(ctx.conn, slug)?,
        "error": error.map(|e| one_line(&e, 300)),
    }))
}

/// The project's session named `name`, the coordinator included.
fn own_session(ctx: &ToolCtx, name: &str) -> Result<SessionRow> {
    db::get_session(ctx.conn, name)?
        .filter(|r| r.project_slug == ctx.config.slug)
        .with_context(|| format!("no session named {name} in this project"))
}

fn own_thread(ctx: &ToolCtx, name: &str) -> Result<SessionRow> {
    let row = own_session(ctx, name)?;
    if row.role != Role::Thread.as_str() {
        bail!("{name} is the coordinator, not a thread");
    }
    Ok(row)
}

/// One session in full: its task, its reports newest first, its latest events and its
/// subagents mid-turn.
pub fn detail(ctx: &ToolCtx, name: &str) -> Result<Value> {
    let row = own_session(ctx, name)?;
    let mut out = session(ctx.conn, &row)?;
    out["task"] = json!(row.task);
    out["created_at"] = json!(row.created_at);
    out["reports"] = db::recent_reports(ctx.conn, &row.name, DETAIL_REPORTS)?
        .iter()
        .map(|r| json!({"at": r.ts, "text": r.text}))
        .collect();
    out["events"] = db::session_events(ctx.conn, &row.name, DETAIL_EVENTS)?
        .iter()
        .map(|e| json!({"at": e.ts, "hook": e.hook, "summary": e.summary, "agent": e.agent_id}))
        .collect();
    out["subagents"] = match (&row.session_id, row.subagents_busy > 0) {
        (Some(sid), true) => db::busy_subagents(ctx.conn, &row.name, sid)?
            .iter()
            .map(|s| {
                json!({
                    "id": s.agent_id,
                    "name": s.name,
                    "description": s.description,
                    "call": s.call.as_ref().map(|(text, _)| text),
                    "since": s.call.as_ref().map(|(_, at)| at),
                })
            })
            .collect(),
        _ => json!([]),
    };
    Ok(out)
}

/// A thread action from the dashboard, with what to say about it. The same functions the
/// board's keys and the coordinator's tools run.
pub fn act(ctx: &ToolCtx, name: &str, action: &str) -> Result<Value> {
    let row = own_thread(ctx, name)?;
    let args = NameArgs {
        name: row.name.clone(),
    };
    let said = match action {
        "wake" => {
            let out = tools::wake_thread(ctx, &args)?;
            match (out["alive"].as_bool(), out["respawned"].as_bool()) {
                (Some(true), Some(true)) => format!("Woke {name}."),
                (Some(true), _) => format!("{name} is already awake."),
                _ => out["note"]
                    .as_str()
                    .unwrap_or("Respawned, but no process showed up yet.")
                    .to_string(),
            }
        }
        "stop" => {
            let out = tools::stop_thread(ctx, &args)?;
            match out["stopped"].as_bool() {
                Some(true) => format!(
                    "Stopped {name}. Its conversation, worktree and branch stay; wake it to go on."
                ),
                _ => out["note"]
                    .as_str()
                    .unwrap_or("It has no process to stop.")
                    .to_string(),
            }
        }
        "resolve" | "reopen" => {
            let resolved = action == "resolve";
            registry::set_resolved(ctx.conn, &row.name, resolved, "user", (ctx.now)())?;
            if resolved {
                format!("Resolved {name}.")
            } else {
                format!("Reopened {name}.")
            }
        }
        "clean" => {
            let gh = Gh::from_env();
            let cleanup = cleanup::Ctx {
                conn: ctx.conn,
                claude: ctx.claude,
                gh: &gh,
                now: ctx.now,
            };
            // Only what loses nothing: discarding unsaved work needs the loss shown and the
            // thread's name typed, which the board and `chief clean --discard` do (§12).
            match cleanup::clean(&cleanup, &row.name, Mode::Safe)? {
                Outcome::Cleaned => format!("Cleaned up {name}."),
                Outcome::AlreadyCleaned => format!("{name} is already cleaned up."),
                Outcome::InProgress => format!("{name}: {}.", cleanup::IN_PROGRESS),
                Outcome::Blocked(why) => bail!(
                    "{name} wasn't cleaned up: {why}. To throw that work away, use x on the \
                     board or `chief clean {} {name} --discard`.",
                    ctx.config.slug
                ),
            }
        }
        other => bail!("unknown action {other:?}"),
    };
    Ok(json!({"ok": true, "message": said}))
}
