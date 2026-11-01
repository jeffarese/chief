//! A thread stuck on the user, told to its coordinator (ARCHITECTURE §5 "Telling the
//! coordinator"): what the thread asks, which threads are due, and the watcher the coordinator's
//! hooks start in the background, which wakes the coordinator by exiting 2.

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;

use crate::db::{self, Event, SessionRow};
use crate::state::{self, Role};
use crate::text::{local_hhmm, one_line};
use crate::{hook, paths, registry};

/// How long a thread is stuck before its coordinator hears of it: a prompt the user answers at
/// once, watching that thread in the pane, needs no word from the coordinator.
pub const TELL_AFTER_MS: i64 = 10_000;
/// The watcher's `timeout` in the coordinator's hook settings: Claude Code ends it then. A day:
/// a coordinator idle that long hears of the next stuck thread at its next turn's end instead.
pub const WATCH_HOOK_TIMEOUT_SECS: u64 = 24 * 60 * 60;
/// The watcher ends itself this long before Claude Code would.
const WATCH_MARGIN_MS: i64 = 60_000;
/// How often the watcher looks: one read of the threads, no write lock.
const TICK: Duration = Duration::from_secs(1);
/// Events read per stuck thread for what it asks: the prompt and the call before it are among
/// the latest few, whatever its subagents do meanwhile.
const EVENTS_READ: usize = 50;
/// Longest text of what a thread asks, in the coordinator's notice.
const ASKS_CHARS: usize = 300;

/// What a thread waiting on the user asks, from its latest events (newest first): the call a
/// permission prompt is for (PreToolUse comes before the permission check), or the question it
/// asks while that call is still open. The label, the text, and where the events before that
/// call start.
pub fn asks(row: &SessionRow, events: &[Event]) -> Option<(&'static str, String, usize)> {
    let text = |e: &Event| e.summary.clone().unwrap_or_else(|| e.hook.clone());
    if row.waiting_reason.as_deref() == Some("permission") {
        let prompt = events
            .iter()
            .position(|e| e.hook == "Notification" && text(e).starts_with("permission_prompt"))?;
        let at = prompt
            + events[prompt..]
                .iter()
                .position(|e| e.hook == "PreToolUse")?;
        let call = text(&events[at]);
        return Some(match call.strip_prefix("Bash: ") {
            Some(command) => ("wants to run", command.to_string(), at + 1),
            None => ("wants to use", call, at + 1),
        });
    }
    let latest = events
        .iter()
        .position(|e| matches!(e.hook.as_str(), "PreToolUse" | "PostToolUse"))?;
    let call = text(&events[latest]);
    let question = call.strip_prefix("AskUserQuestion: ")?;
    (events[latest].hook == "PreToolUse").then(|| ("asks", question.to_string(), latest + 1))
}

/// The threads the coordinator must hear about now: stuck on the user for `TELL_AFTER_MS` and
/// not told since they got stuck. Resolved, cleaned and removed threads are left out, and so is
/// a row whose `blocked_since` outlived its block (a write that skipped the re-derive, such as an
/// older chief's): a false alarm would send the user to a thread that waits on nobody.
pub fn due(threads: &[SessionRow], now: i64) -> Vec<&SessionRow> {
    threads
        .iter()
        .filter(|t| {
            t.role == Role::Thread.as_str()
                && !t.resolved
                && t.cleaned_at.is_none()
                && t.removed_at.is_none()
                && state::blocked(&registry::facts(t))
                && t.blocked_since.is_some_and(|since| {
                    now - since >= TELL_AFTER_MS
                        && t.blocked_told_at.is_none_or(|told| told < since)
                })
        })
        .collect()
}

/// `- <name> "<title>" · permission since 03:39 · wants to run <command>`.
pub fn line(t: &SessionRow, asks: Option<(&str, String)>) -> String {
    let mut parts = vec![match t.title.as_deref() {
        Some(title) => format!("- {} \"{}\"", t.name, one_line(title, 80)),
        None => format!("- {}", t.name),
    }];
    let reason = t.waiting_reason.as_deref().unwrap_or("question");
    match t.blocked_since {
        Some(since) => parts.push(format!("{reason} since {}", local_hhmm(since))),
        None => parts.push(reason.to_string()),
    }
    if let Some((label, text)) = asks {
        parts.push(format!("{label} {}", one_line(&text, ASKS_CHARS)));
    }
    parts.join(" · ")
}

/// The text that wakes the coordinator.
pub fn notice(lines: &[String]) -> String {
    format!(
        "<chief-waiting>\n{}\n</chief-waiting>\nThese threads are stuck until the user answers \
         them, in the thread itself. Tell the user now, a line each: which thread, what it \
         wants, and that they answer it by opening the thread on the board. You can't answer \
         it for them.",
        lines.join("\n")
    )
}

/// What one look found.
#[derive(Debug, PartialEq)]
enum Look {
    /// A newer watcher runs for the project.
    Superseded,
    Nothing,
    /// The notice for the threads now marked told.
    Tell(String),
}

/// One look. The common case, nothing due, is a read; only telling takes the write lock, and
/// looks again inside it, so two watchers never tell the same thread.
fn look(conn: &Connection, project: &str, pid: u32, now: i64) -> Result<Look> {
    let current =
        |conn: &Connection| Ok::<_, anyhow::Error>(db::get_watcher(conn, project)? == Some(pid));
    if !current(conn)? {
        return Ok(Look::Superseded);
    }
    let threads = db::list_sessions(conn, project, Some(Role::Thread))?;
    if due(&threads, now).is_empty() {
        return Ok(Look::Nothing);
    }
    db::immediate(conn, |conn| {
        if !current(conn)? {
            return Ok(Look::Superseded);
        }
        let threads = db::list_sessions(conn, project, Some(Role::Thread))?;
        let due = due(&threads, now);
        if due.is_empty() {
            return Ok(Look::Nothing);
        }
        let mut lines = Vec::new();
        for t in due {
            let events = db::session_events(conn, &t.name, EVENTS_READ)?;
            lines.push(line(t, asks(t, &events).map(|(l, text, _)| (l, text))));
            let mut told = t.clone();
            told.blocked_told_at = Some(now);
            db::update_session(conn, &told)?;
        }
        Ok(Look::Tell(notice(&lines)))
    })
}

/// What a watcher needs from the outside world, passed in so tests can drive it.
pub struct Watch<'a> {
    pub project: &'a str,
    /// This watcher's id: its pid.
    pub pid: u32,
    /// Whether the session that started it (the coordinator's process) still runs.
    pub parent_alive: &'a dyn Fn() -> bool,
    pub now: &'a dyn Fn() -> i64,
    pub tick: Duration,
    /// When it gives up (unix ms).
    pub until: i64,
}

/// Makes this the project's watcher, then looks every tick until some thread is due (the
/// notice, with those threads marked told), a newer watcher takes over, the coordinator's process
/// is gone, or `until` passes (None).
pub fn watch(conn: &Connection, w: &Watch) -> Result<Option<String>> {
    db::set_watcher(conn, w.project, w.pid)?;
    loop {
        if !(w.parent_alive)() {
            return Ok(None);
        }
        let now = (w.now)();
        if now >= w.until {
            return Ok(None);
        }
        match look(conn, w.project, w.pid, now)? {
            Look::Superseded => return Ok(None),
            Look::Tell(text) => return Ok(Some(text)),
            Look::Nothing => std::thread::sleep(w.tick),
        }
    }
}

/// `chief hook watch` (§2): the coordinator's SessionStart and Stop start it with `asyncRewake`,
/// so it runs in the background. Exit 2 with the notice on stderr wakes the coordinator with it;
/// any other end wakes nothing. Errors are logged, never shown to the session.
pub fn run_hook(project: &str, role: Role) {
    // The payload says nothing it needs, but Claude Code writes it.
    let _ = std::io::stdin().read_to_string(&mut String::new());
    if role != Role::Coordinator {
        return;
    }
    let result = std::panic::catch_unwind(|| -> Result<Option<String>> {
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() };
        let alive = move || parent > 1 && unsafe { libc::getppid() } == parent;
        let conn = db::open(&paths::db())?;
        let start = db::now_ms();
        let until = start + WATCH_HOOK_TIMEOUT_SECS as i64 * 1000 - WATCH_MARGIN_MS;
        watch(
            &conn,
            &Watch {
                project,
                pid: std::process::id(),
                parent_alive: &alive,
                now: &db::now_ms,
                tick: TICK,
                until,
            },
        )
    });
    match result {
        Ok(Ok(Some(text))) => {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(text.as_bytes());
            let _ = stderr.flush();
            std::process::exit(2);
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => hook::log_error(
            &paths::hook_error_log(),
            "watch",
            project,
            &format!("{e:#}"),
        ),
        Err(_) => hook::log_error(&paths::hook_error_log(), "watch", project, "panic"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn thread(name: &str) -> SessionRow {
        let mut r = SessionRow::new(name, "p", Role::Thread, 0);
        r.title = Some(format!("Title of {name}"));
        r.pid = Some(1);
        r
    }

    fn stuck(name: &str, since: i64) -> SessionRow {
        let mut r = thread(name);
        r.group_name = "waiting".into();
        r.waiting_reason = Some("permission".into());
        r.pending_permission = true;
        r.blocked_since = Some(since);
        r
    }

    fn event(hook: &str, summary: &str) -> Event {
        Event {
            session_name: Some("p-core-1111".into()),
            session_id: None,
            project_slug: "p".into(),
            hook: hook.into(),
            ts: 0,
            summary: Some(summary.into()),
            agent_id: None,
        }
    }

    #[test]
    fn due_after_the_grace_and_once_per_block() {
        let now = 100_000;
        let fresh = stuck("fresh", now - TELL_AFTER_MS + 1);
        let old = stuck("old", now - TELL_AFTER_MS);
        let mut told = stuck("told", 50_000);
        told.blocked_told_at = Some(60_000);
        let mut again = stuck("again", 70_000);
        again.blocked_told_at = Some(60_000);
        let mut resolved = stuck("resolved", 0);
        resolved.resolved = true;
        let mut cleaned = stuck("cleaned", 0);
        cleaned.cleaned_at = Some(1);
        let mut removed = stuck("removed", 0);
        removed.removed_at = Some(1);
        let mut coordinator = stuck("coordinator", 0);
        coordinator.role = Role::Coordinator.as_str().into();
        let free = thread("free");
        let mut outlived = stuck("outlived", 0);
        outlived.pending_permission = false;
        let rows = [
            fresh,
            old,
            told,
            again,
            resolved,
            cleaned,
            removed,
            coordinator,
            free,
            outlived,
        ];
        let names: Vec<_> = due(&rows, now).iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["old", "again"]);
    }

    #[test]
    fn asks_names_the_call_or_the_question() {
        let perm = stuck("t", 0);
        let events = [
            event("PostToolUse", "Read: a.rs"),
            event(
                "Notification",
                "permission_prompt: Claude needs your permission",
            ),
            event("PreToolUse", "Bash: git commit -q && rm node_modules"),
            event("PostToolUse", "Bash: cargo test"),
        ];
        assert_eq!(
            asks(&perm, &events),
            Some(("wants to run", "git commit -q && rm node_modules".into(), 3))
        );
        let edit = [
            event(
                "Notification",
                "permission_prompt: Claude needs your permission",
            ),
            event("PreToolUse", "Edit: src/lib.rs"),
        ];
        assert_eq!(
            asks(&perm, &edit),
            Some(("wants to use", "Edit: src/lib.rs".into(), 2))
        );
        assert_eq!(asks(&perm, &[event("PreToolUse", "Bash: ls")]), None);

        let mut question = stuck("t", 0);
        question.waiting_reason = Some("question".into());
        let open = [event("PreToolUse", "AskUserQuestion: Which port?")];
        assert_eq!(
            asks(&question, &open),
            Some(("asks", "Which port?".into(), 1))
        );
        let answered = [event("PostToolUse", "AskUserQuestion: Which port?")];
        assert_eq!(asks(&question, &answered), None);
    }

    #[test]
    fn line_and_notice() {
        let mut t = stuck("p-core-1111", 0);
        t.blocked_since = None;
        assert_eq!(
            line(&t, Some(("wants to run", "git add a\n&& rm b".into()))),
            "- p-core-1111 \"Title of p-core-1111\" · permission · wants to run git add a && rm b"
        );
        t.title = None;
        t.waiting_reason = Some("question".into());
        assert_eq!(line(&t, None), "- p-core-1111 · question");
        let text = notice(&["- a".into(), "- b".into()]);
        assert!(
            text.starts_with("<chief-waiting>\n- a\n- b\n</chief-waiting>\n"),
            "{text}"
        );
        assert!(text.contains("opening the thread on the board"));
    }

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "p", std::path::Path::new("/p"), 0).unwrap();
        (dir, conn)
    }

    fn watcher<'a>(
        pid: u32,
        alive: &'a dyn Fn() -> bool,
        now: &'a dyn Fn() -> i64,
        until: i64,
    ) -> Watch<'a> {
        Watch {
            project: "p",
            pid,
            parent_alive: alive,
            now,
            tick: Duration::ZERO,
            until,
        }
    }

    /// The watcher tells once a thread is due, marks it told and stays quiet about it after;
    /// it gives up at its deadline, when its parent is gone, or when a newer watcher runs.
    #[test]
    fn watch_tells_once_then_gives_up() {
        let (_dir, conn) = setup();
        let mut t = stuck("p-core-1111", 1_000);
        db::insert_session(&conn, &t).unwrap();
        db::insert_event(&conn, &event("PreToolUse", "Bash: git commit")).unwrap();
        db::insert_event(
            &conn,
            &event(
                "Notification",
                "permission_prompt: Claude needs your permission",
            ),
        )
        .unwrap();
        let clock = Cell::new(1_000);
        let now = || {
            clock.set(clock.get() + 1_000);
            clock.get()
        };
        let alive = || true;

        let told = watch(&conn, &watcher(7, &alive, &now, i64::MAX)).unwrap();
        let text = told.expect("due after the grace");
        assert!(text.contains("- p-core-1111 \"Title of p-core-1111\" · permission since"));
        assert!(text.contains("wants to run git commit"), "{text}");
        let row = db::get_session(&conn, "p-core-1111").unwrap().unwrap();
        assert_eq!(row.blocked_told_at, Some(1_000 + TELL_AFTER_MS));
        assert_eq!(db::get_watcher(&conn, "p").unwrap(), Some(7));

        // Told already: the next watcher waits until its deadline.
        assert_eq!(
            watch(&conn, &watcher(8, &alive, &now, clock.get() + 3_000)).unwrap(),
            None
        );
        // A new block is told again.
        t.blocked_since = Some(clock.get());
        t.blocked_told_at = row.blocked_told_at;
        db::update_session(&conn, &t).unwrap();
        assert!(
            watch(&conn, &watcher(9, &alive, &now, i64::MAX))
                .unwrap()
                .is_some()
        );

        // Nothing is told once the coordinator's process is gone, or by a superseded watcher.
        t.blocked_since = Some(clock.get());
        db::update_session(&conn, &t).unwrap();
        let gone = || false;
        assert_eq!(
            watch(&conn, &watcher(10, &gone, &now, i64::MAX)).unwrap(),
            None
        );
        assert_eq!(
            look(&conn, "p", 9, clock.get() + TELL_AFTER_MS).unwrap(),
            Look::Superseded
        );
        let row = db::get_session(&conn, "p-core-1111").unwrap().unwrap();
        assert!(row.blocked_told_at < row.blocked_since, "still untold");
    }
}
