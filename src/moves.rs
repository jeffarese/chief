//! Threads agp moves to another account when theirs hits a usage limit (ARCHITECTURE §5 "Limits
//! and moves"). agp stops the session and resumes the same conversation, under the same ids and in
//! the same worktree, in another account; chief follows by reading `agp bg --json` and pointing
//! the row at the account the session runs in now.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rusqlite::Connection;

use crate::agp::{self, Agp, BgSession};
use crate::claude;
use crate::config::ThreadSettings;
use crate::db::{self, Event, PriorAccount, RUNNER_AGP, SessionRow};
use crate::registry;
use crate::state::Role;

/// How long a revive waits for a move agp is in the middle of (stop, wait for the process to
/// end, resume elsewhere).
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);
const SETTLE_POLL: Duration = Duration::from_secs(1);

/// The event `hook` a move is recorded under.
pub const MOVED_EVENT: &str = "Moved";

/// Whether any of `rows` is a live thread agp watches: only then is `agp bg` worth asking.
pub fn wanted(rows: &[SessionRow]) -> bool {
    rows.iter().any(|r| {
        r.via_agp()
            && r.role == Role::Thread.as_str()
            && r.cleaned_at.is_none()
            && r.removed_at.is_none()
    })
}

/// `agp bg --json`, then `apply` to the project's threads; nothing when no thread of the project
/// runs through agp, or agp has no `bg`. Returns the names it moved.
pub fn refresh(
    conn: &Connection,
    agp: &Agp,
    slug: &str,
    project: Option<&Path>,
    now: i64,
) -> Result<Vec<String>> {
    if !wanted(&db::list_sessions(conn, slug, None)?) {
        return Ok(Vec::new());
    }
    let Some(sessions) = agp.bg()? else {
        return Ok(Vec::new());
    };
    apply(conn, slug, project, &sessions, now)
}

/// Writes agp's view into each of the project's threads it lists, in one transaction: its state,
/// and for one agp moved, the account it runs in now (`follow`). Returns the names it moved.
pub fn apply(
    conn: &Connection,
    slug: &str,
    project: Option<&Path>,
    sessions: &[BgSession],
    now: i64,
) -> Result<Vec<String>> {
    db::immediate(conn, |conn| {
        let mut moved = Vec::new();
        for mut row in db::list_sessions(conn, slug, None)? {
            let Some(bg) = row
                .session_id
                .as_deref()
                .and_then(|sid| sessions.iter().find(|s| s.session_id == sid))
            else {
                continue;
            };
            if row.role != Role::Thread.as_str()
                || row.cleaned_at.is_some()
                || row.removed_at.is_some()
            {
                continue;
            }
            let before = row.clone();
            if follow(&mut row, bg, project, now) {
                moved.push(row.name.clone());
                db::insert_event(
                    conn,
                    &Event {
                        session_name: Some(row.name.clone()),
                        session_id: row.session_id.clone(),
                        project_slug: row.project_slug.clone(),
                        hook: MOVED_EVENT.to_string(),
                        ts: now,
                        summary: row.move_note.clone(),
                        agent_id: None,
                    },
                )?;
            }
            if row != before {
                row.updated_at = now;
                db::update_session(conn, &row)?;
            }
        }
        Ok(moved)
    })
}

/// Applies one `agp bg` record to its row: agp watches it, with this state; and once a move is
/// done (never while agp is still `moving` it: the new account may not list it yet), the row
/// runs in the session's account now, remembers the ones before (oldest first: the first holds
/// the job that owns its worktree, §12), and says what happened. True when it moved.
pub fn follow(row: &mut SessionRow, bg: &BgSession, project: Option<&Path>, now: i64) -> bool {
    row.runner = Some(RUNNER_AGP.to_string());
    row.agp_state = Some(bg.state.clone()).filter(|s| !s.is_empty());
    if bg.moving() {
        registry::rederive(row, now);
        return false;
    }
    let here = bg.account();
    let last = bg.moves.last();
    let elsewhere = !claude::same_account(row.account(project).as_deref(), here.as_deref());
    let newer = last.is_some_and(|m| row.moved_at.is_none_or(|at| m.at * 1000 > at));
    if !elsewhere && !newer {
        registry::rederive(row, now);
        return false;
    }
    let mut priors = row.prior_accounts();
    let mut add = |id: &str, label: Option<&str>, dir: Option<PathBuf>| {
        if claude::same_account(dir.as_deref(), here.as_deref())
            || priors
                .iter()
                .any(|p| claude::same_account(p.dir.as_deref().map(Path::new), dir.as_deref()))
        {
            return;
        }
        priors.push(PriorAccount {
            id: id.to_string(),
            label: label
                .filter(|l| !l.trim().is_empty())
                .unwrap_or(id)
                .to_string(),
            dir: dir.map(|d| d.to_string_lossy().into_owned()),
        });
    };
    if elsewhere && bg.moves.is_empty() {
        // agp knows no move, yet runs it elsewhere: the row's account is the one before.
        let label = row
            .account_shown()
            .unwrap_or("the project's account")
            .to_string();
        add(
            row.account_id.as_deref().unwrap_or_default(),
            Some(&label),
            row.account(project),
        );
    }
    for m in &bg.moves {
        add(
            &m.from,
            m.from_label.as_deref(),
            agp::normal_dir(m.from_config_dir.as_deref()),
        );
        add(
            &m.to,
            m.to_label.as_deref(),
            agp::normal_dir(m.to_config_dir.as_deref()),
        );
    }
    row.set_prior_accounts(&priors);
    if claude::same_account(here.as_deref(), project) {
        // The project's account is no account of the row's own (§3).
        row.account_id = None;
        row.account_label = None;
        row.account_dir = None;
    } else {
        row.account_id = Some(bg.profile.clone());
        row.account_label = bg.label.clone().filter(|l| !l.trim().is_empty());
        row.account_dir = here.map(|d| d.to_string_lossy().into_owned());
    }
    if let Some(short) = bg.short_id.as_deref().filter(|s| !s.is_empty()) {
        row.short_id = Some(short.to_string());
    }
    // Its old account's process is gone, and its listing no longer speaks for it; only a
    // snapshot of the new account taken from now on may judge it (§5 "Removed outside chief").
    row.pid = None;
    row.cc_fresh = false;
    row.ids_at = Some(now);
    row.limit_note = None;
    row.limited_since = None;
    row.moved_at = Some(last.map_or(now, |m| m.at * 1000));
    row.move_note = Some(match last {
        Some(m) => move_note(m.from_shown(), m.to_shown(), m.reason.as_deref()),
        None => move_note(
            priors
                .last()
                .map_or("another account", |p| p.label.as_str()),
            bg.shown(),
            None,
        ),
    });
    registry::rederive(row, now);
    true
}

/// "moved from PB2 to PB4 after the 5-hour limit".
pub fn move_note(from: &str, to: &str, reason: Option<&str>) -> String {
    let why = match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) if r.contains("limit") => format!(" after the {r}"),
        Some(r) if r.contains("manual") => " on request".to_string(),
        Some(r) => format!(" ({r})"),
        None => String::new(),
    };
    format!("moved from {from} to {to}{why}")
}

/// Before chief revives a session agp watches (`claude respawn` from `wake_thread`, the pane or
/// the board): waits while agp is moving it, so the revive can't start it again in the account
/// agp just stopped it in, then follows the move. Returns the row as it stands now. A thread agp
/// doesn't watch, or an agp that doesn't answer, leaves the row as it is.
pub fn settle(
    conn: &Connection,
    agp: &Agp,
    row: &SessionRow,
    project: Option<&Path>,
    timeout: Duration,
    now: fn() -> i64,
) -> Result<SessionRow> {
    if !row.via_agp() {
        return Ok(row.clone());
    }
    let Some(sid) = row.session_id.clone() else {
        return Ok(row.clone());
    };
    let deadline = Instant::now() + timeout;
    loop {
        // An agp that doesn't answer leaves nothing to wait on (quietly: the pane calls this
        // with its terminal in use).
        let Ok(Some(sessions)) = agp.bg() else {
            return Ok(row.clone());
        };
        let Some(bg) = sessions.into_iter().find(|s| s.session_id == sid) else {
            return Ok(row.clone());
        };
        if bg.moving() {
            if Instant::now() >= deadline {
                bail!(
                    "agp is still moving {} to another account (a usage limit); try again in a \
                     minute",
                    row.name
                );
            }
            std::thread::sleep(SETTLE_POLL);
            continue;
        }
        apply(conn, &row.project_slug, project, &[bg], now())?;
        return Ok(db::get_session(conn, &row.name)?.unwrap_or_else(|| row.clone()));
    }
}

/// Asks agp to watch `row` again after chief revived it (`agp bg watch`, which keeps a live
/// watcher): one that ended, after a reboot say, comes back. Nothing for a thread agp doesn't
/// watch.
pub fn rearm(agp: &Agp, row: &SessionRow, threads: &ThreadSettings) -> Result<()> {
    if !row.via_agp() {
        return Ok(());
    }
    let Some(session) = row.session_id.as_deref().or(row.short_id.as_deref()) else {
        return Ok(());
    };
    let run = if row.role == Role::Coordinator.as_str() {
        coordinator_run()
    } else {
        agp::RunOptions::from_settings(threads)
    };
    agp.watch(
        session,
        row.account_id.as_deref().filter(|id| !id.is_empty()),
        &run,
    )
}

/// How agp watches a coordinator: it waits for the reset and wakes it there, never moves it
/// (its threads message it by name, which reaches only its own account, §1).
pub fn coordinator_run() -> agp::RunOptions {
    agp::RunOptions {
        on_limit: crate::config::OnLimit::Wait,
        accounts: Vec::new(),
        max_usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agp::{BgLimit, BgMove};

    fn bg(state: &str, profile: &str, dir: &str, moves: Vec<BgMove>) -> BgSession {
        BgSession {
            session_id: "f1de9de1-1233-4c43-8a68-fc73f11e1f17".into(),
            short_id: Some("f1de9de1".into()),
            profile: profile.into(),
            label: Some(profile.to_uppercase()),
            config_dir: Some(dir.into()),
            state: state.into(),
            limit: None,
            moves,
        }
    }

    fn mv(at: i64, from: &str, to: &str) -> BgMove {
        BgMove {
            at,
            from: from.into(),
            from_label: Some(from.to_uppercase()),
            from_config_dir: Some(format!("/accounts/{from}").into()),
            to: to.into(),
            to_label: Some(to.to_uppercase()),
            to_config_dir: Some(format!("/accounts/{to}").into()),
            reason: Some("5-hour limit".into()),
        }
    }

    fn thread() -> SessionRow {
        let mut r = SessionRow::new("demo-bosses-1234", "demo", Role::Thread, 0);
        r.short_id = Some("f1de9de1".into());
        r.session_id = Some("f1de9de1-1233-4c43-8a68-fc73f11e1f17".into());
        r.account_id = Some("cpb".into());
        r.account_label = Some("CPB".into());
        r.account_dir = Some("/accounts/cpb".into());
        r.runner = Some(RUNNER_AGP.into());
        r.pid = Some(42);
        r.cc_fresh = true;
        r.limit_note = Some("You've hit your 5-hour limit · resets 6:10am".into());
        r.limited_since = Some(5);
        r
    }

    #[test]
    fn a_finished_move_points_the_row_at_the_new_account() {
        let mut row = thread();
        let record = bg(
            "watching",
            "laura",
            "/accounts/laura",
            vec![mv(1_000, "cpb", "laura")],
        );
        assert!(follow(&mut row, &record, None, 2_000_000));
        assert_eq!(
            (
                row.account_id.as_deref(),
                row.account_label.as_deref(),
                row.account_dir.as_deref()
            ),
            (Some("laura"), Some("LAURA"), Some("/accounts/laura"))
        );
        assert_eq!(
            row.prior_accounts(),
            [PriorAccount {
                id: "cpb".into(),
                label: "CPB".into(),
                dir: Some("/accounts/cpb".into())
            }]
        );
        assert_eq!(row.moved_at, Some(1_000_000));
        assert_eq!(
            row.move_note.as_deref(),
            Some("moved from CPB to LAURA after the 5-hour limit")
        );
        assert_eq!(
            (row.pid, row.cc_fresh, row.ids_at),
            (None, false, Some(2_000_000))
        );
        assert_eq!((row.limit_note.as_deref(), row.limited_since), (None, None));
        assert_eq!(row.agp_state.as_deref(), Some("watching"));

        // The same record again changes nothing more.
        let again = row.clone();
        assert!(!follow(&mut row, &record, None, 3_000_000));
        assert_eq!(row, again);
    }

    #[test]
    fn nothing_moves_while_agp_is_moving_it() {
        let mut row = thread();
        let record = bg(
            "moving",
            "laura",
            "/accounts/laura",
            vec![mv(1_000, "cpb", "laura")],
        );
        assert!(!follow(&mut row, &record, None, 2_000_000));
        assert_eq!(row.account_id.as_deref(), Some("cpb"));
        assert_eq!(row.agp_state.as_deref(), Some("moving"));
        assert_eq!(row.pid, Some(42));
    }

    #[test]
    fn hops_keep_every_account_before_oldest_first() {
        let mut row = thread();
        let record = bg(
            "watching",
            "cpb",
            "/accounts/cpb",
            vec![mv(1_000, "cpb", "laura"), mv(2_000, "laura", "cpb")],
        );
        // Back in its first account: only the newer move says it moved.
        assert!(follow(&mut row, &record, None, 3_000_000));
        assert_eq!(
            row.prior_accounts()
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["laura"]
        );
        assert_eq!(row.account_id.as_deref(), Some("cpb"));
        assert_eq!(
            row.move_note.as_deref(),
            Some("moved from LAURA to CPB after the 5-hour limit")
        );

        // A thread in the project's account moved away, though agp's record lost the move.
        let mut row = thread();
        row.account_id = None;
        row.account_label = None;
        row.account_dir = None;
        let record = bg("watching", "laura", "/accounts/laura", vec![]);
        assert!(follow(
            &mut row,
            &record,
            Some(Path::new("/accounts/main")),
            9_000
        ));
        assert_eq!(
            row.prior_accounts(),
            [PriorAccount {
                id: String::new(),
                label: "the project's account".into(),
                dir: Some("/accounts/main".into())
            }]
        );
        assert_eq!(row.moved_at, Some(9_000));
        assert_eq!(
            row.move_note.as_deref(),
            Some("moved from the project's account to LAURA")
        );
    }

    #[test]
    fn notes_say_why() {
        assert_eq!(
            move_note("PB2", "PB4", Some("weekly limit")),
            "moved from PB2 to PB4 after the weekly limit"
        );
        assert_eq!(
            move_note("PB2", "PB4", Some("manual move")),
            "moved from PB2 to PB4 on request"
        );
        assert_eq!(move_note("PB2", "PB4", None), "moved from PB2 to PB4");
        assert_eq!(move_note("A", "B", Some(" ")), "moved from A to B");
    }

    #[test]
    fn apply_writes_moves_and_states_and_records_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        let mut row = thread();
        row.project_slug = "demo".into();
        db::insert_session(&conn, &row).unwrap();
        let mut other = SessionRow::new("demo-other-9999", "demo", Role::Thread, 0);
        other.session_id = Some("other".into());
        db::insert_session(&conn, &other).unwrap();
        assert!(wanted(&db::list_sessions(&conn, "demo", None).unwrap()));

        let mut waiting = bg("waiting", "cpb", "/accounts/cpb", vec![]);
        waiting.limit = Some(BgLimit {
            text: "5-hour limit".into(),
            since: Some(1),
            resets_at: Some(2),
        });
        assert!(
            apply(&conn, "demo", None, &[waiting], 10)
                .unwrap()
                .is_empty()
        );
        let stored = db::get_session(&conn, &row.name).unwrap().unwrap();
        assert_eq!(stored.agp_state.as_deref(), Some("waiting"));
        assert_eq!(stored.account_id.as_deref(), Some("cpb"));

        let moved = bg(
            "watching",
            "laura",
            "/accounts/laura",
            vec![mv(1_000, "cpb", "laura")],
        );
        assert_eq!(
            apply(&conn, "demo", None, &[moved], 20).unwrap(),
            [row.name.clone()]
        );
        let stored = db::get_session(&conn, &row.name).unwrap().unwrap();
        assert_eq!(stored.account_id.as_deref(), Some("laura"));
        assert_eq!(
            stored.group_name, "idle",
            "no longer limited, no process yet"
        );
        let events = db::session_events(&conn, &row.name, 5).unwrap();
        assert!(
            events.iter().any(|e| e.hook == MOVED_EVENT
                && e.summary.as_deref() == Some("moved from CPB to LAURA after the 5-hour limit")),
            "{events:?}"
        );
        // The other thread isn't agp's: untouched.
        let other = db::get_session(&conn, "demo-other-9999").unwrap().unwrap();
        assert_eq!(other.runner, None);
    }
}
