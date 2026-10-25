//! `chief hook <event>` (ARCHITECTURE §2, §4, §5, §6, §8).

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;

use crate::config::ProjectConfig;
use crate::db::{self, SessionRow};
use crate::state::{HookKind, Role};
use crate::{paths, registry};

const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const SUMMARY_CHARS: usize = 120;

/// Entry point. Reads the payload from stdin, never panics, never returns an error: failures are
/// appended to the hook error log. Prints only the guard decision or the coordinator's
/// UserPromptSubmit context.
pub fn run(event: &str, project: &str, role: Role) {
    // The default panic hook writes to stderr, which Claude Code may show in the session.
    std::panic::set_hook(Box::new(|_| {}));
    let error = match std::panic::catch_unwind(AssertUnwindSafe(|| dispatch(event, project, role)))
    {
        Ok(Ok(Some(out))) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
            None
        }
        Ok(Ok(None)) => None,
        Ok(Err(e)) => Some(format!("{e:#}")),
        Err(panic) => Some(format!("panic: {}", panic_message(&*panic))),
    };
    if let Some(error) = error {
        log_error(&paths::hook_error_log(), event, project, &error);
    }
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
        branch_of: git_branch,
    };
    let outcome = handle(&conn, &ctx, event, &payload)?;
    if !outcome.notices.is_empty() && notify_enabled(project) {
        for notice in &outcome.notices {
            send_notice(notice);
        }
    }
    Ok(outcome.stdout)
}

fn guard(project: &str, role: Role, input: &str) -> Result<Option<String>> {
    let _ = (project, role, input);
    todo!("M2: §6 message guard")
}

/// What `handle` needs from the outside world, passed in so tests can replay fixtures.
pub struct Ctx<'a> {
    pub project: &'a str,
    /// The `--role` the hook was installed with.
    pub role: Role,
    pub now: i64,
    /// Current branch of a folder; `git_branch` outside tests.
    pub branch_of: fn(&Path) -> Option<String>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The matched session, if any.
    pub session: Option<String>,
    pub notices: Vec<Notice>,
    /// Text for stdout (M2: the coordinator's UserPromptSubmit context).
    pub stdout: Option<String>,
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
    // Outside the write transaction: git may take up to GIT_TIMEOUT.
    let branch = match (kind, ctx.role, str_field("cwd")) {
        (Some(HookKind::Stop), Role::Thread, Some(cwd)) => (ctx.branch_of)(Path::new(cwd)),
        _ => None,
    };

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
        if let Some(kind) = kind
            && let Some(report) = apply(r, kind, payload, branch, ctx.now)
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

/// §5 "Hook updates". The hook is newer than any `claude agents` poll, so the stored snapshot
/// stops counting until the next poll. Returns the new report, if the hook carried one.
fn apply(
    row: &mut SessionRow,
    kind: HookKind,
    payload: &Value,
    branch: Option<String>,
    now: i64,
) -> Option<String> {
    let str_field = |k: &str| payload.get(k).and_then(Value::as_str);
    row.last_hook = Some(kind.as_str().to_string());
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
            if branch.is_some() {
                row.branch = branch;
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
        _ => String::new(),
    };
    one_line(&text, SUMMARY_CHARS)
}

/// Collapses whitespace and cuts to `max` chars with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `git -C <cwd> branch --show-current`; None on error, timeout, or a detached HEAD.
pub fn git_branch(cwd: &Path) -> Option<String> {
    if !cwd.is_dir() {
        return None;
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(["branch", "--show-current"]);
    let out = crate::proc::output_with_timeout(&mut cmd, GIT_TIMEOUT).ok()??;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
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

    fn fake_branch(cwd: &Path) -> Option<String> {
        let name = cwd.file_name()?.to_str()?;
        Some(format!("worktree-{name}"))
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
            branch_of: fake_branch,
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
            assert!(out.stdout.is_none(), "hooks print nothing in M1");
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
        assert_eq!(a.branch.as_deref(), Some("worktree-hello-txt"));
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
            branch_of: fake_branch,
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
            branch_of: fake_branch,
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
            branch_of: fake_branch,
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
            branch_of: fake_branch,
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
    fn one_line_cuts_and_flattens() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        assert_eq!(one_line("ééééé", 5), "ééééé");
    }

    #[test]
    fn git_branch_reads_the_current_branch() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q", "-b", "worktree-demo"])
            .arg(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("git unavailable; skipping");
            return;
        }
        assert_eq!(git_branch(dir.path()).as_deref(), Some("worktree-demo"));
        assert_eq!(git_branch(&dir.path().join("missing")), None);
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(git_branch(plain.path()), None);
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
