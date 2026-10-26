//! Background refresh (ARCHITECTURE §10). Two threads, each with its own connection, so the UI
//! never waits on `claude agents` or `gh`: one polls `claude agents` every 2 s (or right away when
//! kicked after an action) and sends the rows, the other refreshes PRs every 30 s.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use crate::claude::Claude;
use crate::cleanup::Ctx;
use crate::config::ProjectConfig;
use crate::db;
use crate::pr::{self, Gh};
use crate::registry;

use super::app::{Msg, PrSummary, Snapshot};

pub const AGENTS_EVERY: Duration = Duration::from_secs(2);
pub const PR_EVERY: Duration = Duration::from_secs(30);
/// Events older than this are deleted when the board starts.
pub const EVENTS_KEPT_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Where the refresher looks; tests point it at temp dirs and fakes.
#[derive(Debug, Clone)]
pub struct Sources {
    pub slug: String,
    pub dir: PathBuf,
    pub db: PathBuf,
    pub claude_bin: PathBuf,
    pub gh_bin: PathBuf,
    /// `rex`, for switching the layout's main pane.
    pub rex_bin: PathBuf,
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
        let claude = Claude {
            bin: agents_src.claude_bin.clone(),
        };
        loop {
            if agents_tx
                .send(tick(&conn, &claude, &agents_src.slug))
                .is_err()
            {
                return;
            }
            let deadline = Instant::now() + AGENTS_EVERY;
            loop {
                match kick_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(()) => {
                        let msg = read(&conn, &agents_src.slug, None)
                            .unwrap_or_else(|e| Msg::RefreshFailed(format!("{e:#}")));
                        if agents_tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });

    let kicker = kick.clone();
    std::thread::spawn(move || {
        let Some(conn) = open(&src.db, &tx) else {
            return;
        };
        let claude = Claude {
            bin: src.claude_bin.clone(),
        };
        // One `Gh` for the board's lifetime: its "no gh / not logged in" hint comes once.
        let gh = Gh::new(src.gh_bin.clone());
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

    Refresher {
        kick,
        _stop_pr: stop_pr,
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

/// One 2 s refresh: `claude agents`, sync, read. A failing `claude agents` still sends the stored
/// rows, with the error.
pub fn tick(conn: &Connection, claude: &Claude, slug: &str) -> Msg {
    let taken_at = db::now_ms();
    let error = match claude.agents() {
        Ok(snapshot) => registry::sync(conn, slug, &snapshot, taken_at, db::now_ms())
            .err()
            .map(|e| format!("{e:#}")),
        Err(e) => Some(format!("{e:#}")),
    };
    read(conn, slug, error).unwrap_or_else(|e| Msg::RefreshFailed(format!("{e:#}")))
}

fn read(conn: &Connection, slug: &str, error: Option<String>) -> Result<Msg> {
    Ok(Msg::Rows(Snapshot {
        sessions: db::list_sessions(conn, slug, None)?,
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
