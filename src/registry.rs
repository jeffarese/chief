//! Keeps `sessions` in step with `claude agents` and PR status (ARCHITECTURE §5): stores the
//! snapshot, re-derives groups and applies auto-resolve. Callers fetch the inputs; this module
//! only touches the database.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::claude::AgentRow;
use crate::db::{self, SessionRow};
use crate::state::{self, Facts, Group, HookKind, Role};

const INACTIVE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The derivation inputs a row carries. The stored `claude agents` state only counts while it is
/// newer than the last hook; otherwise the last hook gives the provisional group.
pub fn facts(row: &SessionRow) -> Facts<'_> {
    let (cc_state, cc_status) = if row.cc_fresh {
        (row.cc_state.as_deref(), row.cc_status.as_deref())
    } else {
        (None, None)
    };
    Facts {
        resolved: row.resolved,
        pending_permission: row.pending_permission,
        cc_state,
        cc_status,
        last_hook: row.last_hook.as_deref().and_then(HookKind::from_stored),
        process_gone: process_gone(row),
        pr_state: row.pr_state.as_deref(),
        pr_draft: row.pr_draft.unwrap_or(false),
        pr_review: row.pr_review.as_deref(),
    }
}

/// No pid, and a snapshot taken at or after the last hook said so. A newer hook (a respawn
/// starting a turn) makes the row count as running again until the next poll.
pub fn process_gone(row: &SessionRow) -> bool {
    row.pid.is_none()
        && row
            .gone_at
            .is_some_and(|gone| row.last_hook_at.is_none_or(|hook| hook <= gone))
}

/// Recomputes `group_name` and `waiting_reason` from the row.
pub fn rederive(row: &mut SessionRow) {
    let d = state::derive(&facts(row));
    row.group_name = d.group.as_str().to_string();
    row.waiting_reason = d.reason.map(|r| {
        match r {
            state::WaitingReason::Permission => "permission",
            state::WaitingReason::Question => "question",
        }
        .to_string()
    });
}

/// §5 auto-resolve for threads: a merged PR, or 7 days without activity while not working or
/// waiting. A thread that was ever resolved keeps its `resolved_reason` when reopened, and is
/// never auto-resolved again: the user reopened it on purpose.
pub fn auto_resolve(row: &mut SessionRow, now: i64) {
    if row.role != Role::Thread.as_str() || row.resolved || row.resolved_reason.is_some() {
        return;
    }
    let reason = if row.pr_state.as_deref() == Some("MERGED") {
        "merged"
    } else if now - row.last_activity_at >= INACTIVE_MS
        && !matches!(
            Group::parse(&row.group_name),
            Some(Group::Working | Group::Waiting)
        )
    {
        "inactive"
    } else {
        return;
    };
    row.resolved = true;
    row.resolved_reason = Some(reason.to_string());
    rederive(row);
}

/// Applies one `claude agents` snapshot, taken at `taken_at`, to a row. Returns whether the row
/// changed. A hook that ran after the snapshot was taken knows better than the snapshot, so the
/// row keeps that hook's provisional state (`cc_fresh = 0`, `cc_state`/`cc_status` untouched);
/// only the ids and a listed pid are taken from it.
pub fn sync_row(row: &mut SessionRow, snapshot: &[AgentRow], taken_at: i64, now: i64) -> bool {
    // Not spawned yet: nothing in the snapshot can be this session.
    if row.short_id.is_none() && row.session_id.is_none() {
        return false;
    }
    let before = row.clone();
    let stale = row.last_hook_at.is_some_and(|at| at > taken_at);
    let found = match (&row.session_id, &row.short_id) {
        (Some(sid), _) => snapshot.iter().find(|a| &a.session_id == sid),
        (None, Some(short)) => snapshot.iter().find(|a| a.id.as_ref() == Some(short)),
        (None, None) => None,
    };
    match found {
        Some(a) => {
            if row.session_id.is_none() {
                row.session_id = Some(a.session_id.clone());
            }
            if row.short_id.is_none() {
                row.short_id = a.id.clone();
            }
            row.pid = a.pid;
            if !stale {
                row.cc_state = a.state.clone();
                row.cc_status = a.status.clone();
                row.cc_fresh = true;
            }
        }
        // A newer hook proves the process ran after the snapshot; keep its pid.
        None if stale => {}
        None => {
            // Gone from the supervisor: asleep, last cc_state kept for display.
            row.pid = None;
            row.cc_fresh = false;
            if removed_outside(row, snapshot) {
                row.cleaned_at = Some(taken_at);
                row.cleanup_blocked = None;
                if !row.resolved {
                    row.resolved = true;
                    row.resolved_reason = Some("removed".to_string());
                }
            }
        }
    }
    // Only when the verdict flips, so repeated polls of an asleep row don't rewrite it. A
    // permission prompt dies with the process and a respawn doesn't bring it back.
    if !stale && row.pid.is_none() && !process_gone(row) {
        row.gone_at = Some(taken_at);
        row.pending_permission = false;
    }
    rederive(row);
    auto_resolve(row, now);
    let changed = *row != before;
    if changed {
        row.updated_at = now;
    }
    changed
}

/// A stopped session stays listed in `claude agents --all`; only `claude rm` (here, or from
/// `claude agents` itself) takes it off. So a thread whose session id was seen once and that a
/// non-stale snapshot no longer lists has been removed. An empty snapshot proves nothing: chief
/// can't tell "every session was removed" from a supervisor that lists nothing for a moment, and
/// a wrong verdict would block the thread for good. Its coordinator is normally listed, so this
/// only delays the verdict until some session shows up again.
fn removed_outside(row: &SessionRow, snapshot: &[AgentRow]) -> bool {
    row.role == Role::Thread.as_str()
        && row.session_id.is_some()
        && row.cleaned_at.is_none()
        && !snapshot.is_empty()
}

/// Writes a `claude agents` snapshot taken at `taken_at` into every session of the project (one
/// transaction). Take `taken_at` just before running `claude agents`.
pub fn sync(
    conn: &Connection,
    project_slug: &str,
    snapshot: &[AgentRow],
    taken_at: i64,
    now: i64,
) -> Result<()> {
    db::immediate(conn, |conn| {
        for mut row in db::list_sessions(conn, project_slug, None)? {
            if sync_row(&mut row, snapshot, taken_at, now) {
                db::update_session(conn, &row)?;
            }
        }
        Ok(())
    })
}

/// Resolves (`reason` = user | coordinator) or reopens a session. Reopening counts as activity so
/// the inactivity rule doesn't fire again right away.
pub fn set_resolved(
    conn: &Connection,
    name: &str,
    resolved: bool,
    reason: &str,
    now: i64,
) -> Result<SessionRow> {
    db::immediate(conn, |conn| {
        let mut row = db::get_session(conn, name)?.with_context(|| format!("no session {name}"))?;
        if !resolved && row.cleaned_at.is_some() {
            anyhow::bail!(crate::guard::cleaned_message(name));
        }
        row.resolved = resolved;
        if resolved {
            row.resolved_reason = Some(reason.to_string());
        } else {
            row.last_activity_at = now;
        }
        row.updated_at = now;
        rederive(&mut row);
        db::update_session(conn, &row)?;
        Ok(row)
    })
}

/// A PR as `gh pr list --json url,state,isDraft,reviewDecision` reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrInfo {
    pub url: String,
    pub state: String,
    pub draft: bool,
    /// Empty `reviewDecision` from gh is None.
    pub review: Option<String>,
}

/// Stores a session's PR status (None: no PR for its branch) and applies auto-resolve.
pub fn set_pr(conn: &Connection, name: &str, pr: Option<&PrInfo>, now: i64) -> Result<SessionRow> {
    db::immediate(conn, |conn| {
        let mut row = db::get_session(conn, name)?.with_context(|| format!("no session {name}"))?;
        let before = row.clone();
        row.pr_url = pr.map(|p| p.url.clone());
        row.pr_state = pr.map(|p| p.state.clone());
        row.pr_draft = pr.map(|p| p.draft);
        row.pr_review = pr.and_then(|p| p.review.clone()).filter(|r| !r.is_empty());
        rederive(&mut row);
        auto_resolve(&mut row, now);
        if row != before {
            row.updated_at = now;
            db::update_session(conn, &row)?;
        }
        Ok(row)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        (dir, conn)
    }

    fn snapshot() -> Vec<AgentRow> {
        let json = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/agents/agents-sample.json"
        ))
        .unwrap();
        crate::claude::parse_agents(&json).unwrap()
    }

    fn thread(name: &str, short: Option<&str>, sid: Option<&str>, now: i64) -> SessionRow {
        let mut r = SessionRow::new(name, "demo", Role::Thread, now);
        r.short_id = short.map(str::to_string);
        r.session_id = sid.map(str::to_string);
        r
    }

    #[test]
    fn sync_matches_by_session_id_and_short_id() {
        let (_dir, conn) = setup();
        let now = 1_792_940_700_000;
        let rows = [
            // matched by full session id
            thread(
                "demo-hello-txt-1a2b",
                Some("f8627ea8"),
                Some("f8627ea8-50af-4cce-b12d-936669f85ed0"),
                now,
            ),
            // session id not resolved yet: matched by short id
            thread("demo-curl-check-9f8e", Some("bf6db904"), None, now),
            // listed without a pid: asleep
            thread(
                "demo-asleep-thread-77aa",
                Some("0a1b2c3d"),
                Some("0a1b2c3d-1111-4222-8333-944455556666"),
                now,
            ),
            // not in the snapshot at all
            thread("demo-vanished-0000", Some("deadbeef"), Some("gone"), now),
            // not spawned yet
            thread("demo-pending-1111", None, None, now),
        ];
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, now);
        coord.short_id = Some("20dc103a".into());
        db::insert_session(&conn, &coord).unwrap();
        for r in &rows {
            db::insert_session(&conn, r).unwrap();
        }
        let mut vanished = db::get_session(&conn, "demo-vanished-0000")
            .unwrap()
            .unwrap();
        vanished.pid = Some(1);
        vanished.cc_state = Some("working".into());
        vanished.last_hook = Some("Stop".into());
        db::update_session(&conn, &vanished).unwrap();

        sync(&conn, "demo", &snapshot(), now + 1, now + 1).unwrap();
        let get = |n: &str| db::get_session(&conn, n).unwrap().unwrap();

        let a = get("demo-hello-txt-1a2b");
        assert_eq!(
            (a.cc_state.as_deref(), a.cc_status.as_deref()),
            (Some("working"), Some("busy"))
        );
        assert_eq!(
            (a.pid, a.cc_fresh, a.group_name.as_str()),
            (Some(93669), true, "working")
        );
        assert_eq!(a.updated_at, now + 1);

        let b = get("demo-curl-check-9f8e");
        assert_eq!(
            b.session_id.as_deref(),
            Some("bf6db904-3919-4adc-956d-c30266432600")
        );
        assert_eq!(
            (b.group_name.as_str(), b.waiting_reason.as_deref()),
            ("waiting", Some("question"))
        );

        let c = get("chief-demo");
        assert_eq!(
            c.session_id.as_deref(),
            Some("20dc103a-4f18-4895-8bae-4454c193c7e6")
        );
        assert_eq!((c.group_name.as_str(), c.pid), ("idle", Some(74112)));

        let d = get("demo-asleep-thread-77aa");
        assert_eq!(
            (d.group_name.as_str(), d.pid, d.cc_state.as_deref()),
            ("idle", None, Some("done"))
        );

        // Seen before (it has a session id) and now unlisted: removed with `claude rm`.
        let v = get("demo-vanished-0000");
        assert_eq!((v.pid, v.cc_fresh), (None, false));
        assert_eq!(v.cc_state.as_deref(), Some("working"), "last cc_state kept");
        assert_eq!(
            (
                v.cleaned_at,
                v.resolved_reason.as_deref(),
                v.group_name.as_str()
            ),
            (Some(now + 1), Some("removed"), "resolved")
        );

        let p = get("demo-pending-1111");
        assert_eq!(p, rows[4], "unspawned rows are untouched");

        // A second identical sync changes nothing.
        let before = db::list_sessions(&conn, "demo", None).unwrap();
        sync(&conn, "demo", &snapshot(), now + 2, now + 2).unwrap();
        assert_eq!(db::list_sessions(&conn, "demo", None).unwrap(), before);
    }

    #[test]
    fn a_snapshot_older_than_the_last_hook_keeps_the_hook_state() {
        let snap = snapshot();
        let mut row = thread(
            "demo-asleep-thread-77aa",
            Some("0a1b2c3d"),
            Some("0a1b2c3d-1111-4222-8333-944455556666"),
            0,
        );
        // A UserPromptSubmit at t=100 woke it; the snapshot (state done, no pid) was taken at 90.
        row.last_hook = Some("UserPromptSubmit".into());
        row.last_hook_at = Some(100);
        row.pid = Some(4242);
        row.cc_state = Some("done".into());
        rederive(&mut row);
        assert_eq!(row.group_name, "working");

        let mut stale = row.clone();
        sync_row(&mut stale, &snap, 90, 110);
        assert_eq!(
            (stale.cc_fresh, stale.group_name.as_str()),
            (false, "working")
        );
        assert_eq!(stale.pid, None, "a listed row still updates the pid");
        assert_eq!(stale.cc_state.as_deref(), Some("done"));

        // Not listed at all: the newer hook proves the process ran, so the pid stays.
        let mut unlisted = row.clone();
        sync_row(&mut unlisted, &[], 90, 110);
        assert_eq!(
            (
                unlisted.pid,
                unlisted.cc_fresh,
                unlisted.group_name.as_str()
            ),
            (Some(4242), false, "working")
        );

        // The same snapshot taken after the hook wins.
        let mut fresh = row.clone();
        let mut busy = snap.clone();
        busy[3].state = Some("working".into());
        busy[3].status = Some("busy".into());
        busy[3].pid = Some(5);
        sync_row(&mut fresh, &busy, 100, 110);
        assert_eq!(
            (fresh.cc_fresh, fresh.cc_state.as_deref(), fresh.pid),
            (true, Some("working"), Some(5))
        );
        let mut done = row.clone();
        sync_row(&mut done, &snap, 101, 110);
        assert_eq!((done.cc_fresh, done.group_name.as_str()), (true, "idle"));
    }

    #[test]
    fn a_thread_that_died_mid_turn_is_not_working() {
        let (_dir, conn) = setup();
        let mut row = thread("t", Some("x"), Some("sid-x"), 0);
        row.pid = Some(7);
        row.last_hook = Some("PreToolUse".into());
        row.last_hook_at = Some(100);
        rederive(&mut row);
        assert_eq!(row.group_name, "working");
        db::insert_session(&conn, &row).unwrap();
        let get = || db::get_session(&conn, "t").unwrap().unwrap();

        // A snapshot from before the hook proves nothing.
        sync(&conn, "demo", &[], 90, 110).unwrap();
        assert_eq!((get().group_name.as_str(), get().pid), ("working", Some(7)));

        // A later snapshot without it: the process is gone.
        sync(&conn, "demo", &[], 150, 160).unwrap();
        let r = get();
        assert_eq!(
            (r.group_name.as_str(), r.pid, r.gone_at),
            ("idle", None, Some(150))
        );
        let before = get();
        sync(&conn, "demo", &[], 170, 180).unwrap();
        assert_eq!(get(), before, "repeated polls change nothing");

        // Listed without a pid while its stale state still says working: also gone.
        let mut listed = snapshot()[3].clone();
        listed.session_id = "sid-x".into();
        listed.state = Some("working".into());
        sync(&conn, "demo", &[listed], 190, 200).unwrap();
        assert_eq!(get().group_name, "idle");

        // A respawn starts a turn: the newer hook counts as working until the next poll.
        let mut r = get();
        r.last_hook_at = Some(300);
        r.cc_fresh = false;
        rederive(&mut r);
        assert_eq!(r.group_name, "working");
        db::update_session(&conn, &r).unwrap();
        sync(&conn, "demo", &[], 250, 310).unwrap();
        assert_eq!(get().group_name, "working", "stale snapshot");
        sync(&conn, "demo", &[], 320, 330).unwrap();
        assert_eq!(get().group_name, "idle");

        // A permission prompt waits on the user until the process is known gone; then it is
        // cleared, since a respawn doesn't bring it back.
        let mut r = get();
        r.last_hook = Some("Notification:permission_prompt".into());
        r.last_hook_at = Some(400);
        r.pending_permission = true;
        r.pid = Some(8);
        rederive(&mut r);
        assert_eq!(r.waiting_reason.as_deref(), Some("permission"));
        db::update_session(&conn, &r).unwrap();
        sync(&conn, "demo", &[], 390, 395).unwrap();
        assert_eq!(get().waiting_reason.as_deref(), Some("permission"), "stale");
        sync(&conn, "demo", &[], 410, 415).unwrap();
        let r = get();
        assert!(!r.pending_permission);
        assert_eq!((r.group_name.as_str(), r.waiting_reason), ("idle", None));

        // A blocked question still waits, and an open PR still shows in review.
        let mut blocked = snapshot()[3].clone();
        blocked.session_id = "sid-x".into();
        blocked.state = Some("blocked".into());
        sync(&conn, "demo", &[blocked], 420, 425).unwrap();
        assert_eq!(get().waiting_reason.as_deref(), Some("question"));
        let mut r = get();
        r.cc_state = Some("done".into());
        r.pr_state = Some("OPEN".into());
        r.pr_draft = Some(false);
        rederive(&mut r);
        assert_eq!(r.group_name, "review");
    }

    #[test]
    fn a_thread_missing_from_a_snapshot_was_removed() {
        let snap = snapshot();
        let mut row = thread("demo-gone-1111", Some("g1"), Some("sid-g1"), 0);
        row.pid = Some(3);
        row.last_hook = Some("Stop".into());
        row.last_hook_at = Some(100);
        rederive(&mut row);

        // An empty list proves nothing, nor does a snapshot older than the last hook.
        let mut empty = row.clone();
        sync_row(&mut empty, &[], 200, 210);
        assert_eq!((empty.cleaned_at, empty.resolved), (None, false));
        let mut stale = row.clone();
        sync_row(&mut stale, &snap, 90, 210);
        assert_eq!(stale.cleaned_at, None);

        let mut gone = row.clone();
        assert!(sync_row(&mut gone, &snap, 200, 210));
        assert_eq!(
            (
                gone.cleaned_at,
                gone.resolved,
                gone.resolved_reason.as_deref(),
                gone.group_name.as_str()
            ),
            (Some(200), true, Some("removed"), "resolved")
        );
        let before = gone.clone();
        sync_row(&mut gone, &snap, 300, 310);
        assert_eq!(gone, before, "the verdict is kept, not rewritten");

        // An already resolved thread keeps its reason; a blocked cleanup is cleared.
        let mut resolved = row.clone();
        resolved.resolved = true;
        resolved.resolved_reason = Some("user".into());
        resolved.cleanup_blocked = Some("2 unpushed commits".into());
        sync_row(&mut resolved, &snap, 200, 210);
        assert_eq!(
            (
                resolved.cleaned_at,
                resolved.resolved_reason.as_deref(),
                resolved.cleanup_blocked
            ),
            (Some(200), Some("user"), None)
        );

        // Never seen in `claude agents` (just spawned), or the coordinator: not removed.
        let mut fresh = thread("demo-new-2222", Some("n2"), None, 0);
        sync_row(&mut fresh, &snap, 200, 210);
        assert_eq!(fresh.cleaned_at, None);
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("coord-gone".into());
        sync_row(&mut coord, &snap, 200, 210);
        assert_eq!((coord.cleaned_at, coord.resolved), (None, false));

        // Listed without a pid (stopped): asleep, not removed.
        let mut stopped = thread(
            "demo-asleep-thread-77aa",
            Some("0a1b2c3d"),
            Some("0a1b2c3d-1111-4222-8333-944455556666"),
            0,
        );
        sync_row(&mut stopped, &snap, 200, 210);
        assert_eq!((stopped.cleaned_at, stopped.pid), (None, None));
    }

    #[test]
    fn auto_resolve_rules() {
        let now = 100 * DAY;
        let mk = |pr: Option<&str>, idle_days: i64, cc: &str, role: Role| {
            let mut r = SessionRow::new("t", "demo", role, now - idle_days * DAY);
            r.short_id = Some("x".into());
            r.pr_state = pr.map(str::to_string);
            r.cc_state = Some(cc.to_string());
            // Only a busy process counts as working (§5 rule 5).
            r.cc_status = Some(if cc == "working" { "busy" } else { "idle" }.to_string());
            r.cc_fresh = true;
            r
        };
        let cases = [
            (
                "merged",
                mk(Some("MERGED"), 0, "done", Role::Thread),
                Some("merged"),
            ),
            (
                "merged while working",
                mk(Some("MERGED"), 0, "working", Role::Thread),
                Some("merged"),
            ),
            (
                "idle 7 days",
                mk(None, 7, "done", Role::Thread),
                Some("inactive"),
            ),
            ("idle 6 days", mk(None, 6, "done", Role::Thread), None),
            ("working 8 days", mk(None, 8, "working", Role::Thread), None),
            ("blocked 8 days", mk(None, 8, "blocked", Role::Thread), None),
            (
                "open PR 8 days",
                mk(Some("OPEN"), 8, "done", Role::Thread),
                Some("inactive"),
            ),
            (
                "coordinator",
                mk(Some("MERGED"), 30, "done", Role::Coordinator),
                None,
            ),
        ];
        for (name, mut row, want) in cases {
            rederive(&mut row);
            auto_resolve(&mut row, now);
            assert_eq!(row.resolved_reason.as_deref(), want, "{name}");
            assert_eq!(row.resolved, want.is_some(), "{name}");
            if want.is_some() {
                assert_eq!(row.group_name, "resolved", "{name}");
            }
        }
    }

    #[test]
    fn reopened_threads_stay_open() {
        let (_dir, conn) = setup();
        let now = 100 * DAY;
        let mut r = thread("t", Some("x"), None, now - 30 * DAY);
        r.pr_state = Some("MERGED".into());
        db::insert_session(&conn, &r).unwrap();

        sync(&conn, "demo", &[], now, now).unwrap();
        let row = db::get_session(&conn, "t").unwrap().unwrap();
        assert_eq!(
            (row.resolved, row.resolved_reason.as_deref()),
            (true, Some("merged"))
        );

        let row = set_resolved(&conn, "t", false, "user", now).unwrap();
        assert!(!row.resolved);
        assert_eq!(row.last_activity_at, now);
        assert_eq!(row.group_name, "idle");

        sync(&conn, "demo", &[], now + 30 * DAY, now + 30 * DAY).unwrap();
        assert!(!db::get_session(&conn, "t").unwrap().unwrap().resolved);

        let row = set_resolved(&conn, "t", true, "coordinator", now).unwrap();
        assert_eq!(
            (row.group_name.as_str(), row.resolved_reason.as_deref()),
            ("resolved", Some("coordinator"))
        );
        assert!(set_resolved(&conn, "nope", true, "user", now).is_err());

        // A cleaned thread has no session left to reopen.
        let mut row = db::get_session(&conn, "t").unwrap().unwrap();
        row.cleaned_at = Some(now);
        db::update_session(&conn, &row).unwrap();
        let err = set_resolved(&conn, "t", false, "user", now).unwrap_err();
        assert!(err.to_string().contains("t was cleaned up"), "{err}");
        assert!(set_resolved(&conn, "t", true, "user", now).is_ok());
    }

    #[test]
    fn pr_status_drives_groups() {
        let (_dir, conn) = setup();
        let now = DAY;
        let mut r = thread("t", Some("x"), None, now);
        r.cc_state = Some("done".into());
        r.cc_fresh = true;
        db::insert_session(&conn, &r).unwrap();

        let mut pr = PrInfo {
            url: "https://github.com/o/r/pull/1".into(),
            state: "OPEN".into(),
            draft: true,
            review: Some(String::new()),
        };
        let row = set_pr(&conn, "t", Some(&pr), now).unwrap();
        assert_eq!(
            (row.group_name.as_str(), row.pr_review.as_deref()),
            ("idle", None)
        );
        pr.draft = false;
        assert_eq!(
            set_pr(&conn, "t", Some(&pr), now).unwrap().group_name,
            "review"
        );
        pr.review = Some("APPROVED".into());
        assert_eq!(
            set_pr(&conn, "t", Some(&pr), now).unwrap().group_name,
            "landing"
        );
        pr.state = "MERGED".into();
        let row = set_pr(&conn, "t", Some(&pr), now).unwrap();
        assert_eq!(
            (row.group_name.as_str(), row.resolved_reason.as_deref()),
            ("resolved", Some("merged"))
        );
        let row = set_pr(&conn, "t", None, now).unwrap();
        assert_eq!((row.pr_url, row.pr_draft), (None, None));
    }
}
