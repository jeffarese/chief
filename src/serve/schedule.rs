//! The routine scheduler inside `chief serve` (ARCHITECTURE §13 "Firing").

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::{TICK, log};
use crate::routines::{self, Outcome};
use crate::{db, paths, tools};

/// Looks for due routines every `TICK`, for as long as the server runs. A look that fails is
/// logged and the next one tries again.
pub async fn run() {
    let scheduler = Arc::new(Mutex::new(Scheduler::default()));
    loop {
        let state = scheduler.clone();
        let looked = tokio::task::spawn_blocking(move || {
            let mut scheduler = state.lock().unwrap_or_else(|e| e.into_inner());
            scheduler.tick(&mut |line| log(&line))
        });
        match looked.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log(&format!("routines: {e:#}")),
            Err(e) => log(&format!("routines: the scheduler crashed: {e}")),
        }
        tokio::time::sleep(tick_every()).await;
    }
}

/// `TICK`, or `CHIEF_SERVE_TICK_MS` for tests (it can only make routines fire sooner after their
/// time, never earlier).
fn tick_every() -> std::time::Duration {
    std::env::var("CHIEF_SERVE_TICK_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(TICK)
}

/// What the scheduler remembers between looks.
#[derive(Default)]
pub struct Scheduler {
    /// The finished runs (thread, or `finding <id>` for a command's, and report time) it
    /// already woke a coordinator for, or found it
    /// awake for: each report gets one wake, so a coordinator that can't be told doesn't come
    /// back every hour forever. Forgotten when the server restarts, which costs one more wake.
    woken: HashSet<(String, i64)>,
}

impl Scheduler {
    /// One look: every live project's due routines, each run started (or skipped), then a
    /// sleeping coordinator woken for the runs that finished; all reported through `say`. A
    /// project whose settings can't be read is reported and the rest go on.
    pub fn tick(&mut self, say: &mut dyn FnMut(String)) -> Result<()> {
        let conn = db::open(&paths::db())?;
        for (slug, _dir) in db::list_projects(&conn)? {
            // An archived project runs nothing (§2).
            if db::archived_at(&conn, &slug)?.is_some() {
                continue;
            }
            if !routines::due(&conn, &slug, db::now_ms())?.is_empty() {
                fire(&slug, say);
            }
            let unheard: Vec<(String, i64)> = routines::unheard(&conn, &slug, db::now_ms())?
                .into_iter()
                .filter(|run| !self.woken.contains(run))
                .collect();
            if unheard.is_empty() {
                continue;
            }
            match tools::with_project(&slug, routines::wake_coordinator) {
                Ok(true) => say(format!(
                    "{slug}: woke the coordinator for {} finished routine run(s)",
                    unheard.len()
                )),
                Ok(false) => {}
                Err(e) => say(format!(
                    "{slug}: couldn't wake the coordinator for finished routine runs: {e:#}"
                )),
            }
            self.woken.extend(unheard);
        }
        Ok(())
    }
}

/// Starts every due routine of one project.
fn fire(slug: &str, say: &mut dyn FnMut(String)) {
    let ran = tools::with_project(slug, |ctx| {
        // Whether a routine's last session still works is read from its row, so bring the rows
        // up to date first; a failed listing leaves them as the hooks wrote them. Not for
        // commands alone: one that runs every minute mustn't cost a `claude agents` a minute.
        if routines::due_needs_rows(ctx)?
            && let Err(e) = tools::refresh(ctx)
        {
            say(format!(
                "{slug}: couldn't refresh before running routines: {e:#}"
            ));
        }
        routines::run_due(ctx)
    });
    match ran {
        Err(e) => say(format!("{slug}: {e:#}")),
        Ok(outcomes) => {
            for (name, outcome) in outcomes {
                say(match outcome {
                    Ok(Outcome::Started { thread, warning }) => format!(
                        "{slug}: routine {name} started {thread}{}",
                        warning.map(|w| format!(" ({w})")).unwrap_or_default()
                    ),
                    Ok(Outcome::Running { run }) => {
                        format!("{slug}: routine {name} runs its command (run {run})")
                    }
                    Ok(Outcome::Ran(done)) => {
                        format!(
                            "{slug}: routine {name} ran its command: {}",
                            done.run.line()
                        )
                    }
                    Ok(Outcome::Skipped(why)) => format!("{slug}: routine {name} {why}"),
                    Err(e) => format!("{slug}: routine {name} failed: {e:#}"),
                });
            }
        }
    }
}
