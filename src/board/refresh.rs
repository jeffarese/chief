//! Background refresh (ARCHITECTURE §10). Two threads, each with its own connection, so the UI
//! never waits on `claude agents` or `gh`: one polls `claude agents` every 2 s (or right away when
//! kicked after an action) and sends the rows, the other refreshes PRs every 30 s.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use crate::accounts;
use crate::claude::{AGENTS_TIMEOUT, Claude};
use crate::cleanup::Ctx;
use crate::config::ProjectConfig;
use crate::pr::{self, Gh};
use crate::state::{Group, Role};
use crate::{db, tui};

use super::app::{Msg, PrSummary, Snapshot, group_of};

pub const AGENTS_EVERY: Duration = Duration::from_secs(2);
pub const PR_EVERY: Duration = Duration::from_secs(30);
/// Events older than this are deleted when the board starts.
pub const EVENTS_KEPT_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// How many of a working or waiting thread's latest events the detail gets (§10): a tool call
/// is two, before and after, and the detail tells of four.
const ACTIVITY_EVENTS: usize = 12;

/// Where the refresher looks; tests point it at temp dirs and fakes.
#[derive(Debug, Clone)]
pub struct Sources {
    pub slug: String,
    pub dir: PathBuf,
    pub db: PathBuf,
    pub claude_bin: PathBuf,
    /// The project's Claude Code account (`ProjectConfig::claude_config_dir`).
    pub claude_account: Option<PathBuf>,
    /// Its config folder, for job state (`claude::claude_dir`).
    pub claude_dir: PathBuf,
    pub gh_bin: PathBuf,
    pub agp_bin: PathBuf,
}

impl Sources {
    pub fn claude(&self) -> Claude {
        Claude {
            bin: self.claude_bin.clone(),
            account: self.claude_account.clone(),
            config_dir: self.claude_dir.clone(),
        }
    }
}

/// Handles to the refresh threads. Dropping it stops them at their next wait.
pub struct Refresher {
    kick: Sender<()>,
    _stop_pr: Sender<()>,
}

impl Refresher {
    /// A sender that makes the refresher re-read the rows now (no `claude agents`), for a
    /// worker whose action changed them.
    pub fn kicker(&self) -> Sender<()> {
        self.kick.clone()
    }
}

pub fn spawn(src: Sources, tx: Sender<Msg>) -> Refresher {
    let (kick, kick_rx) = mpsc::channel::<()>();
    let (stop_pr, stop_rx) = mpsc::channel::<()>();

    let agents_src = src.clone();
    let agents_tx = tx.clone();
    std::thread::spawn(move || {
        let Some(conn) = open(&agents_src.db, &agents_tx) else {
            return;
        };
        if let Err(e) = db::prune_events(&conn, db::now_ms() - EVENTS_KEPT_MS) {
            let _ = agents_tx.send(Msg::RefreshFailed(format!("pruning old events: {e:#}")));
        }
        let claude = agents_src.claude();
        keep_going(&agents_tx, AGENTS_EVERY, Msg::RefreshFailed, || {
            agents_loop(&conn, &claude, &agents_src.slug, &agents_tx, &kick_rx)
        });
    });

    let kicker = kick.clone();
    std::thread::spawn(move || {
        let Some(conn) = open(&src.db, &tx) else {
            return;
        };
        let claude = src.claude();
        // One `Gh` for the board's lifetime: its "no gh / not logged in" hint comes once.
        let gh = Gh::new(src.gh_bin.clone());
        keep_going(&tx, PR_EVERY, Msg::PrFailed, || {
            loop {
                let msg = match pr_tick(&conn, &claude, &gh, &src.dir) {
                    Ok(summary) => Msg::Pr(summary),
                    Err(e) => Msg::PrFailed(format!("{e:#}")),
                };
                if tx.send(msg).is_err() {
                    return;
                }
                let _ = kicker.send(());
                match stop_rx.recv_timeout(PR_EVERY) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        });
    });

    Refresher {
        kick,
        _stop_pr: stop_pr,
    }
}

/// Runs `body` until it returns (the board is gone). A panic in it is reported with `failed`
/// and, after `pause`, `body` starts again: a refresher that died would leave the board showing
/// old rows with nothing saying so.
pub fn keep_going(
    tx: &Sender<Msg>,
    pause: Duration,
    failed: impl Fn(String) -> Msg,
    mut body: impl FnMut(),
) {
    while let Err(panic) = tui::catch(&mut body) {
        let text = format!("refresh crashed: {panic}; trying again");
        if tx.send(failed(text)).is_err() {
            return;
        }
        std::thread::sleep(pause);
    }
}

/// `claude agents` every 2 s, and a database read whenever a worker kicks. Returns when the
/// board is gone.
fn agents_loop(
    conn: &Connection,
    claude: &Claude,
    slug: &str,
    tx: &Sender<Msg>,
    kick_rx: &Receiver<()>,
) {
    loop {
        if tx.send(tick(conn, claude, slug)).is_err() {
            return;
        }
        let deadline = Instant::now() + AGENTS_EVERY;
        loop {
            match kick_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => {
                    let msg = read(conn, slug, None)
                        .unwrap_or_else(|e| Msg::RefreshFailed(format!("{e:#}")));
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn open(path: &Path, tx: &Sender<Msg>) -> Option<Connection> {
    match db::open(path) {
        Ok(conn) => Some(conn),
        Err(e) => {
            let _ = tx.send(Msg::RefreshFailed(format!("{e:#}")));
            None
        }
    }
}

/// One 2 s refresh: `claude agents` in every account the project uses, sync, read. A failing
/// `claude agents` still sends the stored rows, with the error.
pub fn tick(conn: &Connection, claude: &Claude, slug: &str) -> Msg {
    let error = match accounts::refresh(conn, slug, claude, AGENTS_TIMEOUT, db::now_ms) {
        Ok(listings) => listings.error(),
        Err(e) => Some(format!("{e:#}")),
    };
    read(conn, slug, error).unwrap_or_else(|e| Msg::RefreshFailed(format!("{e:#}")))
}

fn read(conn: &Connection, slug: &str, error: Option<String>) -> Result<Msg> {
    let sessions = db::list_sessions(conn, slug, None)?;
    let mut activity = HashMap::new();
    let mut subagents = HashMap::new();
    for row in &sessions {
        if row.role == Role::Thread.as_str()
            && matches!(group_of(row), Group::Working | Group::Waiting)
        {
            let events = db::session_events(conn, &row.name, ACTIVITY_EVENTS)?;
            activity.insert(row.name.clone(), events);
            if row.subagents_busy > 0
                && let Some(sid) = &row.session_id
            {
                let busy = db::busy_subagents(conn, &row.name, sid)?;
                subagents.insert(row.name.clone(), busy);
            }
        }
    }
    Ok(Msg::Rows(Snapshot {
        sessions,
        activity,
        subagents,
        denials: db::count_guard_denials(conn, slug)?,
        pane_target: db::get_pane_target(conn, slug)?,
        error: error.map(|e| e.split_whitespace().collect::<Vec<_>>().join(" ")),
    }))
}

/// One 30 s PR refresh. Settings are re-read each time: the cleanup mode may have changed.
pub fn pr_tick(conn: &Connection, claude: &Claude, gh: &Gh, dir: &Path) -> Result<PrSummary> {
    let config = ProjectConfig::load(&dir.join("project.toml"))?;
    let ctx = Ctx {
        conn,
        claude,
        gh,
        now: db::now_ms,
    };
    let r = pr::refresh_project(&ctx, &config)?;
    Ok(PrSummary {
        hint: r.hint,
        cleaned: r.cleaned,
        blocked: r.blocked.len(),
        errors: r.errors.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SessionRow;

    /// A working or waiting thread's latest events come with the rows, newest first; an idle
    /// one's and the coordinator's don't.
    #[test]
    fn the_rows_bring_what_busy_threads_do() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        let rows = [
            ("busy", Role::Thread, Group::Working),
            ("asks", Role::Thread, Group::Waiting),
            ("idle", Role::Thread, Group::Idle),
            ("chief-demo", Role::Coordinator, Group::Working),
        ];
        for (name, role, group) in rows {
            let mut row = SessionRow::new(name, "demo", role, 0);
            row.group_name = group.as_str().to_string();
            db::insert_session(&conn, &row).unwrap();
            for (ts, call) in [(1, "Read: a.rs"), (2, "Bash: ls")] {
                let event = db::Event {
                    session_name: Some(name.to_string()),
                    session_id: None,
                    project_slug: "demo".into(),
                    hook: "PreToolUse".into(),
                    ts,
                    summary: Some(call.into()),
                    agent_id: None,
                };
                db::insert_event(&conn, &event).unwrap();
            }
        }
        let Msg::Rows(snapshot) = read(&conn, "demo", None).unwrap() else {
            panic!("not rows");
        };
        let mut names: Vec<&str> = snapshot.activity.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["asks", "busy"]);
        let calls: Vec<_> = snapshot.activity["busy"]
            .iter()
            .map(|e| e.summary.as_deref().unwrap())
            .collect();
        assert_eq!(calls, ["Bash: ls", "Read: a.rs"]);
    }

    /// A refresher that panics says so and starts again; one that returns is done.
    #[test]
    fn a_crashed_refresher_reports_and_restarts() {
        let (tx, rx) = mpsc::channel();
        let mut runs = 0;
        keep_going(&tx, Duration::ZERO, Msg::RefreshFailed, || {
            runs += 1;
            if runs < 3 {
                panic!("bad row {runs}");
            }
        });
        assert_eq!(runs, 3);
        drop(tx);
        let msgs: Vec<Msg> = rx.iter().collect();
        assert_eq!(
            msgs,
            [
                Msg::RefreshFailed("refresh crashed: bad row 1; trying again".into()),
                Msg::RefreshFailed("refresh crashed: bad row 2; trying again".into()),
            ]
        );
    }
}
