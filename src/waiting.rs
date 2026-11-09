//! A thread stuck on the user, moved to another account, or stopped on a usage limit, told to
//! its coordinator (ARCHITECTURE §5 "Telling the coordinator"): what the thread asks, which
//! threads are due, and the watcher the coordinator's hooks start in the background, which wakes
//! the coordinator by exiting 2.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;

use crate::db::{self, Event, SessionRow};
use crate::routines::{Finding, Verdict};
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
/// How often it reads job states and agp's moves itself (`Watch::sync`), so a limit or a move
/// is told with no board open.
pub const SYNC_EVERY_MS: i64 = 10_000;
/// How long a thread waits on a usage limit before its coordinator hears of it: agp, when it
/// watches the thread, has moved it to another account well within that.
pub const LIMIT_TELL_AFTER_MS: i64 = 120_000;
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

/// Threads agp moved to another account since the coordinator last heard (§5 "Limits and
/// moves"): its subscription named the old process.
pub fn moved_due(threads: &[SessionRow]) -> Vec<&SessionRow> {
    threads
        .iter()
        .filter(|t| {
            live(t)
                && t.moved_at
                    .is_some_and(|at| t.moved_told_at.is_none_or(|told| told < at))
        })
        .collect()
}

/// Threads stopped on a usage limit for `LIMIT_TELL_AFTER_MS` that the coordinator wasn't told
/// of since: agp didn't move them (it waits for the reset, or doesn't watch them).
pub fn limited_due(threads: &[SessionRow], now: i64) -> Vec<&SessionRow> {
    threads
        .iter()
        .filter(|t| {
            live(t)
                && registry::limited(t)
                && t.limited_since.is_some_and(|since| {
                    now - since >= LIMIT_TELL_AFTER_MS
                        && t.limit_told_at.is_none_or(|told| told < since)
                })
        })
        .collect()
}

/// Routine runs (§13) that finished and that the coordinator wasn't told of: nobody subscribed
/// to a thread `chief serve` started, so no idle notice carries its report. Finished is a report
/// since the last telling with nothing left running: not working, waiting or limited (those are
/// told, or go on, by themselves). `quiet` names the routines that tell nobody (`notify =
/// false`), and `verdicts` is what each run said of itself (`routines::verdicts`): one that
/// said `nothing` is news to nobody, and one that said nothing at all (it crashed, or forgot)
/// is told, since something may be wrong. A run put away by its routine's next run still
/// counts; one the user or the coordinator resolved was seen.
pub fn routine_due<'a>(
    threads: &'a [SessionRow],
    quiet: &[String],
    verdicts: &HashMap<String, Finding>,
) -> Vec<&'a SessionRow> {
    threads
        .iter()
        .filter(|t| {
            let Some(routine) = &t.routine else {
                return false;
            };
            if verdicts.get(&t.name).map(|f| f.verdict) == Some(Verdict::Nothing) {
                return false;
            }
            let seen = t.resolved
                && t.resolved_reason.as_deref() != Some(crate::routines::RESOLVED_BY_ROUTINE);
            t.role == Role::Thread.as_str()
                && !quiet.contains(routine)
                && !seen
                && t.removed_at.is_none()
                && t.stopped_early_at.is_none()
                && !matches!(
                    state::Group::parse(&t.group_name),
                    Some(state::Group::Working | state::Group::Waiting | state::Group::Limited)
                )
                && t.last_report_at
                    .is_some_and(|at| t.routine_told_at.is_none_or(|told| told < at))
        })
        .collect()
}

/// `- <name> "<title>" · routine <routine> · finished at 09:04 · attention: <headline>`; for a
/// run that gave no verdict, its report's first words instead.
pub fn routine_line(t: &SessionRow, finding: Option<&Finding>) -> String {
    let mut line = format!(
        "- {} \"{}\" · routine {} · finished at {}",
        t.name,
        t.title.as_deref().unwrap_or(&t.name),
        t.routine.as_deref().unwrap_or("?"),
        local_hhmm(t.last_report_at.unwrap_or(t.updated_at)),
    );
    match finding {
        Some(f) => line.push_str(&format!(" · {}: {}", f.verdict.as_str(), f.headline)),
        None => {
            line.push_str(" · gave no outcome");
            if let Some(report) = t.last_report.as_deref().filter(|r| !r.trim().is_empty()) {
                line.push_str(&format!("\n  report: {}", one_line(report, 200)));
            }
        }
    }
    line
}

/// The text for routine runs that finished.
pub fn routine_notice(lines: &[String]) -> String {
    format!(
        "<chief-routine>\n{}\n</chief-routine>\nThese scheduled routine runs finished with \
         nobody watching. Each line carries what the run found. Tell the user in a line each, \
         the ones marked attention first; for those, and for a run that gave no outcome, read \
         more with read_routine (its notes, findings and values) or read_report before you \
         speak. Resolve a run that leaves nothing to do; a follow-up goes to its thread like \
         any other.",
        lines.join("\n")
    )
}

fn live(t: &SessionRow) -> bool {
    t.role == Role::Thread.as_str()
        && !t.resolved
        && t.cleaned_at.is_none()
        && t.removed_at.is_none()
}

/// `- <name> "<title>" · moved from PB2 to PB4 after the 5-hour limit at 03:39`.
pub fn moved_line(t: &SessionRow) -> String {
    let mut line = format!(
        "{} · {}",
        lead(t),
        t.move_note.as_deref().unwrap_or("moved to another account")
    );
    if let Some(at) = t.moved_at {
        line.push_str(&format!(" at {}", local_hhmm(at)));
    }
    line
}

/// `- <name> "<title>" · limited since 03:39 · You've hit your 5-hour limit · resets 6:10am ·
/// agp waits for the reset`.
pub fn limited_line(t: &SessionRow) -> String {
    let mut parts = vec![lead(t)];
    if let Some(since) = t.limited_since {
        parts.push(format!("limited since {}", local_hhmm(since)));
    }
    if let Some(note) = &t.limit_note {
        parts.push(one_line(note, 200));
    }
    parts.push(
        match (t.via_agp(), t.agp_state.as_deref()) {
            (true, Some("waiting")) => "agp found no account with room and resumes it at the reset",
            (true, Some("limited" | "moving")) => "agp is still moving it",
            (true, _) => "agp isn't watching it, so it waits until messaged",
            (false, _) => "it doesn't run through agp, so it waits until messaged",
        }
        .to_string(),
    );
    parts.join(" · ")
}

fn lead(t: &SessionRow) -> String {
    match t.title.as_deref() {
        Some(title) => format!("- {} \"{}\"", t.name, one_line(title, 80)),
        None => format!("- {}", t.name),
    }
}

/// `- <name> "<title>" · permission since 03:39 · wants to run <command>`.
pub fn line(t: &SessionRow, asks: Option<(&str, String)>) -> String {
    let mut parts = vec![lead(t)];
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

/// The text for threads agp moved.
pub fn moved_notice(lines: &[String]) -> String {
    format!(
        "<chief-moved>\n{}\n</chief-moved>\nagp moved these threads to another Claude account \
         (their own hit a usage limit, or one was moved on request); each goes on there with \
         its conversation and worktree. Your subscription named the old process: call \
         list_threads for each one's address now, then subscribe again with SendMessage with \
         exactly {{\"to\": \"<address>\", \"notify_when_idle\": true}} and no message key. \
         Mention it to the user only in passing.",
        lines.join("\n")
    )
}

/// The text for threads stopped on a usage limit.
pub fn limited_notice(lines: &[String]) -> String {
    format!(
        "<chief-limited>\n{}\n</chief-limited>\nThese threads stopped on a Claude usage limit \
         and wait. Tell the user in a line each. move_thread moves one to a pool account with \
         room now; a thread agp doesn't resume goes on only when messaged after the reset.",
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
    let quiet = crate::routines::quiet(conn, project)?;
    let verdicts = crate::routines::verdicts(conn, project)?;
    let any = |threads: &[SessionRow]| {
        !due(threads, now).is_empty()
            || !moved_due(threads).is_empty()
            || !limited_due(threads, now).is_empty()
            || !routine_due(threads, &quiet, &verdicts).is_empty()
    };
    if !any(&threads) {
        return Ok(Look::Nothing);
    }
    db::immediate(conn, |conn| {
        if !current(conn)? {
            return Ok(Look::Superseded);
        }
        let threads = db::list_sessions(conn, project, Some(Role::Thread))?;
        if !any(&threads) {
            return Ok(Look::Nothing);
        }
        let mut told: Vec<SessionRow> = Vec::new();
        let row_of = |t: &SessionRow, told: &mut Vec<SessionRow>| -> usize {
            match told.iter().position(|r| r.name == t.name) {
                Some(i) => i,
                None => {
                    told.push(t.clone());
                    told.len() - 1
                }
            }
        };
        let mut sections = Vec::new();
        let mut lines = Vec::new();
        for t in due(&threads, now) {
            let events = db::session_events(conn, &t.name, EVENTS_READ)?;
            lines.push(line(t, asks(t, &events).map(|(l, text, _)| (l, text))));
            let i = row_of(t, &mut told);
            told[i].blocked_told_at = Some(now);
        }
        if !lines.is_empty() {
            sections.push(notice(&lines));
        }
        let mut lines = Vec::new();
        for t in moved_due(&threads) {
            lines.push(moved_line(t));
            let i = row_of(t, &mut told);
            told[i].moved_told_at = Some(now);
        }
        if !lines.is_empty() {
            sections.push(moved_notice(&lines));
        }
        let mut lines = Vec::new();
        for t in limited_due(&threads, now) {
            lines.push(limited_line(t));
            let i = row_of(t, &mut told);
            told[i].limit_told_at = Some(now);
        }
        if !lines.is_empty() {
            sections.push(limited_notice(&lines));
        }
        let mut lines = Vec::new();
        for t in routine_due(&threads, &quiet, &verdicts) {
            lines.push(routine_line(t, verdicts.get(&t.name)));
            let i = row_of(t, &mut told);
            told[i].routine_told_at = Some(now);
        }
        if !lines.is_empty() {
            sections.push(routine_notice(&lines));
        }
        for row in &told {
            db::update_session(conn, row)?;
        }
        Ok(Look::Tell(sections.join("\n\n")))
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
    /// Reads what no hook reports, every `SYNC_EVERY_MS`: limits from job states, and agp's
    /// moves. Its errors are only logged.
    pub sync: &'a dyn Fn(&Connection, i64) -> Result<()>,
}

/// Makes this the project's watcher, then looks every tick until some thread is due (the
/// notice, with those threads marked told), a newer watcher takes over, the coordinator's process
/// is gone, or `until` passes (None).
pub fn watch(conn: &Connection, w: &Watch) -> Result<Option<String>> {
    db::set_watcher(conn, w.project, w.pid)?;
    let mut synced: Option<i64> = None;
    loop {
        if !(w.parent_alive)() {
            return Ok(None);
        }
        let now = (w.now)();
        if now >= w.until {
            return Ok(None);
        }
        if synced.is_none_or(|at| now - at >= SYNC_EVERY_MS) {
            synced = Some(now);
            if let Err(e) = (w.sync)(conn, now) {
                hook::log_error(
                    &paths::hook_error_log(),
                    "watch",
                    w.project,
                    &format!("reading limits and moves: {e:#}"),
                );
            }
        }
        match look(conn, w.project, w.pid, now)? {
            Look::Superseded => return Ok(None),
            Look::Tell(text) => return Ok(Some(text)),
            Look::Nothing => std::thread::sleep(w.tick),
        }
    }
}

/// The watcher's own sync: agp's moves first (they change the account a job state is read in),
/// then every live thread's limit. Nothing while the project has no live thread.
fn sync(
    conn: &Connection,
    project: &str,
    claude: &crate::claude::Claude,
    agp: &crate::agp::Agp,
    now: i64,
) -> Result<()> {
    let threads = db::list_sessions(conn, project, Some(Role::Thread))?;
    if !threads.iter().any(live) {
        return Ok(());
    }
    let account = claude.account.as_deref();
    let moved = crate::moves::refresh(conn, agp, project, account, now);
    registry::refresh_limits(conn, project, account, now)?;
    moved.map(|_| ())
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
        let claude = hook::project_claude(project);
        let agp = crate::agp::Agp::from_env();
        let sync = |conn: &Connection, now: i64| sync(conn, project, &claude, &agp, now);
        watch(
            &conn,
            &Watch {
                project,
                pid: std::process::id(),
                parent_alive: &alive,
                now: &db::now_ms,
                tick: TICK,
                until,
                sync: &sync,
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

    /// A routine's run that ended its turn with a report at `at`.
    fn run_of(name: &str, routine: &str, at: i64) -> SessionRow {
        let mut r = thread(name);
        r.routine = Some(routine.into());
        r.group_name = "idle".into();
        r.last_report = Some("3 new issues.\n#883 needs you.".into());
        r.last_report_at = Some(at);
        r
    }

    #[test]
    fn a_finished_routine_run_is_told_once_per_report() {
        let fresh = run_of("fresh", "triage", 5_000);
        let mut told = run_of("told", "triage", 5_000);
        told.routine_told_at = Some(6_000);
        // It reported again after the coordinator heard: a follow-up turn.
        let mut again = run_of("again", "triage", 9_000);
        again.routine_told_at = Some(6_000);
        let mut working = run_of("working", "triage", 5_000);
        working.group_name = "working".into();
        let mut waiting = run_of("waiting", "triage", 5_000);
        waiting.group_name = "waiting".into();
        let mut early = run_of("early", "triage", 5_000);
        early.stopped_early_at = Some(5_000);
        let mut silent = run_of("silent", "triage", 5_000);
        silent.last_report_at = None;
        let quiet = run_of("quiet", "nightly", 5_000);
        let mut plain = run_of("plain", "triage", 5_000);
        plain.routine = None;
        // Put away by the routine's next run: still news. Resolved by a person: seen.
        let mut put_away = run_of("put-away", "triage", 5_000);
        put_away.resolved = true;
        put_away.resolved_reason = Some("routine".into());
        let mut seen = run_of("seen", "triage", 5_000);
        seen.resolved = true;
        seen.resolved_reason = Some("user".into());
        let mut removed = run_of("removed", "triage", 5_000);
        removed.removed_at = Some(1);
        let mut review = run_of("review", "triage", 5_000);
        review.group_name = "review".into();

        let threads = [
            fresh, told, again, working, waiting, early, silent, quiet, plain, put_away, seen,
            removed, review,
        ];
        let finding = |verdict| Finding {
            routine: "triage".into(),
            thread: None,
            at: 5_000,
            verdict,
            headline: "h".into(),
            details: None,
        };
        let none = HashMap::new();
        let names = |verdicts: &HashMap<String, Finding>| -> Vec<String> {
            routine_due(&threads, &["nightly".to_string()], verdicts)
                .iter()
                .map(|t| t.name.clone())
                .collect()
        };
        assert_eq!(names(&none), ["fresh", "again", "put-away", "review"]);
        // A run that said nothing changed is news to nobody; any other verdict is.
        let said: HashMap<String, Finding> = [
            ("fresh".to_string(), finding(Verdict::Nothing)),
            ("again".to_string(), finding(Verdict::Update)),
            ("review".to_string(), finding(Verdict::Attention)),
        ]
        .into();
        assert_eq!(names(&said), ["again", "put-away", "review"]);
    }

    #[test]
    fn the_routine_notice_names_the_run_and_previews_its_report() {
        let run = run_of("p-triage-1a2b", "triage", 0);
        // Without a verdict, the report's first words stand in for it.
        let line = routine_line(&run, None);
        assert!(line.starts_with(
            "- p-triage-1a2b \"Title of p-triage-1a2b\" · routine triage · finished at "
        ));
        assert!(line.ends_with(" · gave no outcome\n  report: 3 new issues. #883 needs you."));
        let finding = Finding {
            routine: "triage".into(),
            thread: Some("p-triage-1a2b".into()),
            at: 0,
            verdict: Verdict::Attention,
            headline: "#883 double-charges after a retry".into(),
            details: None,
        };
        let line = routine_line(&run, Some(&finding));
        assert!(line.ends_with(" · attention: #883 double-charges after a retry"));
        assert!(!line.contains("report:"));
        let notice = routine_notice(&[line]);
        assert!(notice.starts_with("<chief-routine>\n- p-triage-1a2b"));
        assert!(notice.contains("</chief-routine>\nThese scheduled routine runs finished"));
        assert!(notice.contains("read_routine"));
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
            sync: &|_, _| Ok(()),
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

    fn moved(name: &str, at: i64) -> SessionRow {
        let mut r = thread(name);
        r.runner = Some(db::RUNNER_AGP.into());
        r.account_id = Some("laura".into());
        r.account_label = Some("PB4".into());
        r.moved_at = Some(at);
        r.move_note = Some("moved from PB2 to PB4 after the 5-hour limit".into());
        r
    }

    fn limited(name: &str, since: i64, agp_state: Option<&str>) -> SessionRow {
        let mut r = thread(name);
        r.limit_note = Some("You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)".into());
        r.limited_since = Some(since);
        r.last_hook_at = Some(since - 1);
        if let Some(state) = agp_state {
            r.runner = Some(db::RUNNER_AGP.into());
            r.agp_state = Some(state.into());
        }
        r
    }

    #[test]
    fn moved_and_limited_threads_are_due_once() {
        let now = 1_000_000;
        let mut told = moved("told", 5_000);
        told.moved_told_at = Some(6_000);
        let mut again = moved("again", 9_000);
        again.moved_told_at = Some(6_000);
        let mut resolved = moved("resolved", 5_000);
        resolved.resolved = true;
        let threads = [moved("new", 5_000), told, again, resolved, thread("never")];
        let names = |due: Vec<&SessionRow>| due.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(moved_due(&threads)), ["new", "again"]);

        let fresh = limited("fresh", now - LIMIT_TELL_AFTER_MS + 1, None);
        let old = limited("old", now - LIMIT_TELL_AFTER_MS, Some("waiting"));
        let mut told = limited("told", 10_000, None);
        told.limit_told_at = Some(20_000);
        // A hook since the limit: it went on (a message after the reset).
        let mut went_on = limited("went-on", 10_000, None);
        went_on.last_hook_at = Some(10_001);
        let threads = [fresh, old, told, went_on];
        assert_eq!(names(limited_due(&threads, now)), ["old"]);
    }

    #[test]
    fn moved_and_limited_lines_say_what_happens_next() {
        let line = moved_line(&moved("p-x-1111", 5_000));
        assert!(
            line.starts_with(
                "- p-x-1111 \"Title of p-x-1111\" · moved from PB2 to PB4 after the 5-hour limit at "
            ),
            "{line}"
        );
        let line = limited_line(&limited("p-x-1111", 5_000, Some("waiting")));
        assert!(line.contains("· limited since "), "{line}");
        assert!(
            line.contains("· You've hit your 5-hour limit · resets 6:10am (Europe/Madrid) · "),
            "{line}"
        );
        assert!(line.ends_with("agp found no account with room and resumes it at the reset"));
        let line = limited_line(&limited("p-x-1111", 5_000, None));
        assert!(line.ends_with("it doesn't run through agp, so it waits until messaged"));
        let line = limited_line(&limited("p-x-1111", 5_000, Some("dormant")));
        assert!(line.ends_with("agp isn't watching it, so it waits until messaged"));
        let text = moved_notice(&["- a".into()]);
        assert!(
            text.starts_with("<chief-moved>\n- a\n</chief-moved>\n"),
            "{text}"
        );
        assert!(text.contains("call list_threads"), "{text}");
        let text = limited_notice(&["- b".into()]);
        assert!(
            text.starts_with("<chief-limited>\n- b\n</chief-limited>\n"),
            "{text}"
        );
        assert!(text.contains("move_thread"), "{text}");
    }

    #[test]
    fn one_notice_tells_every_kind_and_marks_each() {
        let (_dir, conn) = setup();
        let now = 1_000_000;
        db::insert_session(&conn, &stuck("p-stuck-1111", now - TELL_AFTER_MS)).unwrap();
        db::insert_session(&conn, &moved("p-moved-2222", now - 5_000)).unwrap();
        db::insert_session(
            &conn,
            &limited("p-limit-3333", now - LIMIT_TELL_AFTER_MS, None),
        )
        .unwrap();
        db::set_watcher(&conn, "p", 7).unwrap();
        let Look::Tell(text) = look(&conn, "p", 7, now).unwrap() else {
            panic!("all three are due");
        };
        let sections: Vec<_> = text.split("\n\n").collect();
        assert_eq!(sections.len(), 3, "{text}");
        assert!(
            sections[0].starts_with("<chief-waiting>\n- p-stuck-1111"),
            "{text}"
        );
        assert!(
            sections[1].starts_with("<chief-moved>\n- p-moved-2222"),
            "{text}"
        );
        assert!(
            sections[2].starts_with("<chief-limited>\n- p-limit-3333"),
            "{text}"
        );
        let get = |name: &str| db::get_session(&conn, name).unwrap().unwrap();
        assert_eq!(get("p-stuck-1111").blocked_told_at, Some(now));
        assert_eq!(get("p-moved-2222").moved_told_at, Some(now));
        assert_eq!(get("p-limit-3333").limit_told_at, Some(now));
        assert_eq!(look(&conn, "p", 7, now + 1).unwrap(), Look::Nothing);
    }

    #[test]
    fn the_watcher_syncs_every_few_seconds() {
        let (_dir, conn) = setup();
        let clock = Cell::new(0);
        let now = || {
            clock.set(clock.get() + 1_000);
            clock.get()
        };
        let synced = Cell::new(0);
        let sync = |_: &Connection, _: i64| {
            synced.set(synced.get() + 1);
            Ok(())
        };
        let alive = || true;
        let w = Watch {
            sync: &sync,
            ..watcher(7, &alive, &now, 25_000)
        };
        assert_eq!(watch(&conn, &w).unwrap(), None);
        // At 1 s, 11 s and 21 s.
        assert_eq!(synced.get(), 3);
    }
}
