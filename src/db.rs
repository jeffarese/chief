//! SQLite registry (ARCHITECTURE §4). Migrations run on open, tracked by `PRAGMA user_version`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::state::Role;

/// One entry per schema version; never edit a shipped entry, append a new one.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE projects (
  slug        TEXT PRIMARY KEY,
  dir         TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);

CREATE TABLE sessions (
  name            TEXT PRIMARY KEY,
  project_slug    TEXT NOT NULL REFERENCES projects(slug),
  role            TEXT NOT NULL CHECK (role IN ('coordinator','thread')),
  short_id        TEXT,
  session_id      TEXT UNIQUE,
  title           TEXT,
  task            TEXT,
  repo_name       TEXT,
  repo_path       TEXT,
  cwd             TEXT,
  branch          TEXT,
  model           TEXT,
  effort          TEXT,
  cc_state        TEXT,
  cc_status       TEXT,
  cc_fresh        INTEGER NOT NULL DEFAULT 0,
  pid             INTEGER,
  last_hook       TEXT,
  pending_permission INTEGER NOT NULL DEFAULT 0,
  group_name      TEXT NOT NULL DEFAULT 'working',
  waiting_reason  TEXT,
  resolved        INTEGER NOT NULL DEFAULT 0,
  resolved_reason TEXT,
  pr_url          TEXT,
  pr_state        TEXT,
  pr_draft        INTEGER,
  pr_review       TEXT,
  last_report     TEXT,
  last_report_at  INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL
);

CREATE TABLE events (
  id           INTEGER PRIMARY KEY,
  session_name TEXT,
  session_id   TEXT,
  project_slug TEXT NOT NULL,
  hook         TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  summary      TEXT
);

CREATE TABLE reports (
  id           INTEGER PRIMARY KEY,
  session_name TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  text         TEXT NOT NULL
);

CREATE TABLE guard_denials (
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL,
  session_name TEXT,
  tool         TEXT NOT NULL,
  target       TEXT,
  reason       TEXT NOT NULL,
  ts           INTEGER NOT NULL
);

CREATE TABLE ui_state (
  project_slug TEXT PRIMARY KEY,
  pane_target  TEXT
);
"#,
    // When the last state-changing hook ran, so an older `claude agents` snapshot can't
    // overwrite it (§5).
    "ALTER TABLE sessions ADD COLUMN last_hook_at INTEGER;",
    // When a snapshot no older than the last hook showed no process, so a thread that died
    // mid-turn stops counting as working (§5).
    "ALTER TABLE sessions ADD COLUMN gone_at INTEGER;",
    // Thread cleanup (§12): when `claude rm` removed the session and its worktree, and why the
    // last attempt didn't.
    "ALTER TABLE sessions ADD COLUMN cleaned_at INTEGER;
     ALTER TABLE sessions ADD COLUMN cleanup_blocked TEXT;",
    // The session `chief pane` is attached to right now, so the board skips a needless switch
    // (§9).
    "ALTER TABLE ui_state ADD COLUMN pane_showing TEXT;",
    // Judging "removed outside chief" (§5): the newest snapshot applied to the row (older ones are
    // ignored), when its ids were last set (a snapshot taken before can't know the session), and
    // when a sync found it removed (so a session that shows up again is restored). Rows that sync
    // already marked removed get `removed_at`, so a wrong old verdict can still be undone.
    "ALTER TABLE sessions ADD COLUMN synced_at INTEGER;
     ALTER TABLE sessions ADD COLUMN ids_at INTEGER;
     ALTER TABLE sessions ADD COLUMN removed_at INTEGER;
     UPDATE sessions SET ids_at = updated_at
       WHERE short_id IS NOT NULL OR session_id IS NOT NULL;
     UPDATE sessions SET removed_at = cleaned_at
       WHERE resolved_reason = 'removed' AND cleaned_at IS NOT NULL;",
    // Which process is cleaning a thread up, as when it claimed it (§12). Not in `SessionRow`:
    // only `claim_cleanup` and `release_cleanup` touch it, so no row write can drop a claim.
    "ALTER TABLE sessions ADD COLUMN cleaning_at INTEGER;",
    // The multiplexer session `chief up` created (§9): the only one chief reuses or kills.
    "ALTER TABLE ui_state ADD COLUMN rex_session TEXT;",
    // The running `chief pane`, which the board signals to switch (§9): its pid, and its start
    // time to prove a pid is still that pane and not a reuse.
    "ALTER TABLE ui_state ADD COLUMN pane_pid INTEGER;
     ALTER TABLE ui_state ADD COLUMN pane_started TEXT;",
];

/// A cleanup claim older than this was left by a process that died mid-cleanup.
pub const CLEANUP_CLAIM_STALE_MS: i64 = 10 * 60 * 1000;

/// Opens (creating if needed) the database with WAL, busy_timeout=5000 and foreign keys on, then
/// migrates it to the latest schema.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version = user_version(&tx)?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        tx.execute_batch(sql)
            .with_context(|| format!("migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
    }
    tx.commit()?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v as usize)
}

pub fn schema_version(conn: &Connection) -> Result<usize> {
    user_version(conn)
}

/// Runs `f` inside `BEGIN IMMEDIATE`, so a read-modify-write of a row can't interleave with a
/// hook or board poll in another process. Rolls back on error.
pub fn immediate<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// A `sessions` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionRow {
    pub name: String,
    pub project_slug: String,
    pub role: String,
    pub short_id: Option<String>,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub task: Option<String>,
    pub repo_name: Option<String>,
    pub repo_path: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cc_state: Option<String>,
    pub cc_status: Option<String>,
    /// True when `cc_state`/`cc_status` come from a `claude agents` poll newer than the last hook.
    pub cc_fresh: bool,
    pub pid: Option<u32>,
    pub last_hook: Option<String>,
    /// When `last_hook` ran (unix ms).
    pub last_hook_at: Option<i64>,
    /// When a `claude agents` snapshot taken at or after `last_hook_at` showed no process.
    pub gone_at: Option<i64>,
    /// When the session, its worktree and its local branch were removed (unix ms).
    pub cleaned_at: Option<i64>,
    /// Why the last cleanup attempt left it in place, e.g. "2 unpushed commits".
    pub cleanup_blocked: Option<String>,
    /// When the newest `claude agents` snapshot applied to the row was taken; older ones are
    /// ignored.
    pub synced_at: Option<i64>,
    /// When `short_id` or `session_id` last changed. `update_session` keeps it: a write that
    /// changes either id stamps it with the row's `updated_at`; other writes keep it (None
    /// leaves the stored value).
    pub ids_at: Option<i64>,
    /// When a sync found the session removed outside chief (§5).
    pub removed_at: Option<i64>,
    pub pending_permission: bool,
    pub group_name: String,
    pub waiting_reason: Option<String>,
    pub resolved: bool,
    pub resolved_reason: Option<String>,
    pub pr_url: Option<String>,
    pub pr_state: Option<String>,
    pub pr_draft: Option<bool>,
    pub pr_review: Option<String>,
    pub last_report: Option<String>,
    pub last_report_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_activity_at: i64,
}

impl SessionRow {
    /// A row for a session that hasn't been spawned yet.
    pub fn new(name: &str, project_slug: &str, role: Role, now: i64) -> Self {
        SessionRow {
            name: name.to_string(),
            project_slug: project_slug.to_string(),
            role: role.as_str().to_string(),
            group_name: "working".to_string(),
            created_at: now,
            updated_at: now,
            last_activity_at: now,
            ..Default::default()
        }
    }
}

const SESSION_COLUMNS: &str = "name, project_slug, role, short_id, session_id, title, task, \
    repo_name, repo_path, cwd, branch, model, effort, cc_state, cc_status, cc_fresh, pid, \
    last_hook, pending_permission, group_name, waiting_reason, resolved, resolved_reason, pr_url, \
    pr_state, pr_draft, pr_review, last_report, last_report_at, created_at, updated_at, \
    last_activity_at, last_hook_at, gone_at, cleaned_at, cleanup_blocked, synced_at, ids_at, \
    removed_at";

fn session_from_row(r: &Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        name: r.get(0)?,
        project_slug: r.get(1)?,
        role: r.get(2)?,
        short_id: r.get(3)?,
        session_id: r.get(4)?,
        title: r.get(5)?,
        task: r.get(6)?,
        repo_name: r.get(7)?,
        repo_path: r.get(8)?,
        cwd: r.get(9)?,
        branch: r.get(10)?,
        model: r.get(11)?,
        effort: r.get(12)?,
        cc_state: r.get(13)?,
        cc_status: r.get(14)?,
        cc_fresh: r.get(15)?,
        pid: r.get(16)?,
        last_hook: r.get(17)?,
        pending_permission: r.get(18)?,
        group_name: r.get(19)?,
        waiting_reason: r.get(20)?,
        resolved: r.get(21)?,
        resolved_reason: r.get(22)?,
        pr_url: r.get(23)?,
        pr_state: r.get(24)?,
        pr_draft: r.get(25)?,
        pr_review: r.get(26)?,
        last_report: r.get(27)?,
        last_report_at: r.get(28)?,
        created_at: r.get(29)?,
        updated_at: r.get(30)?,
        last_activity_at: r.get(31)?,
        last_hook_at: r.get(32)?,
        gone_at: r.get(33)?,
        cleaned_at: r.get(34)?,
        cleanup_blocked: r.get(35)?,
        synced_at: r.get(36)?,
        ids_at: r.get(37)?,
        removed_at: r.get(38)?,
    })
}

pub fn register_project(conn: &Connection, slug: &str, dir: &Path, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (slug, dir, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(slug) DO UPDATE SET dir = excluded.dir",
        params![slug, dir.to_string_lossy(), now],
    )?;
    Ok(())
}

/// Every registered project as (slug, dir), by slug.
pub fn list_projects(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT slug, dir FROM projects ORDER BY slug")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The project's folder, if registered.
pub fn project_dir(conn: &Connection, slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT dir FROM projects WHERE slug = ?1", [slug], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn insert_session(conn: &Connection, s: &SessionRow) -> Result<()> {
    let sql = format!(
        "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
         ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, \
         ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39)"
    );
    let has_ids = s.short_id.is_some() || s.session_id.is_some();
    let ids_at = s.ids_at.or(has_ids.then_some(s.updated_at));
    conn.execute(
        &sql,
        params![
            s.name,
            s.project_slug,
            s.role,
            s.short_id,
            s.session_id,
            s.title,
            s.task,
            s.repo_name,
            s.repo_path,
            s.cwd,
            s.branch,
            s.model,
            s.effort,
            s.cc_state,
            s.cc_status,
            s.cc_fresh,
            s.pid,
            s.last_hook,
            s.pending_permission,
            s.group_name,
            s.waiting_reason,
            s.resolved,
            s.resolved_reason,
            s.pr_url,
            s.pr_state,
            s.pr_draft,
            s.pr_review,
            s.last_report,
            s.last_report_at,
            s.created_at,
            s.updated_at,
            s.last_activity_at,
            s.last_hook_at,
            s.gone_at,
            s.cleaned_at,
            s.cleanup_blocked,
            s.synced_at,
            ids_at,
            s.removed_at,
        ],
    )
    .with_context(|| format!("inserting session {}", s.name))?;
    Ok(())
}

/// Writes every column of `s` (keyed by name). Read the row and write it back inside one
/// `BEGIN IMMEDIATE` transaction so a concurrent hook or board poll can't be overwritten.
/// `ids_at` is the exception: a write that changes `short_id` or `session_id` stamps it with
/// `updated_at` (SET reads the old values), so every writer of ids keeps it without knowing.
pub fn update_session(conn: &Connection, s: &SessionRow) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE sessions SET project_slug = ?2, role = ?3, short_id = ?4, session_id = ?5,
           title = ?6, task = ?7, repo_name = ?8, repo_path = ?9, cwd = ?10, branch = ?11,
           model = ?12, effort = ?13, cc_state = ?14, cc_status = ?15, cc_fresh = ?16, pid = ?17,
           last_hook = ?18, pending_permission = ?19, group_name = ?20, waiting_reason = ?21,
           resolved = ?22, resolved_reason = ?23, pr_url = ?24, pr_state = ?25, pr_draft = ?26,
           pr_review = ?27, last_report = ?28, last_report_at = ?29, created_at = ?30,
           updated_at = ?31, last_activity_at = ?32, last_hook_at = ?33,
           gone_at = ?34, cleaned_at = ?35, cleanup_blocked = ?36, synced_at = ?37,
           ids_at = CASE WHEN short_id IS ?4 AND session_id IS ?5 THEN coalesce(?38, ids_at)
                         ELSE ?31 END,
           removed_at = ?39
         WHERE name = ?1",
            params![
                s.name,
                s.project_slug,
                s.role,
                s.short_id,
                s.session_id,
                s.title,
                s.task,
                s.repo_name,
                s.repo_path,
                s.cwd,
                s.branch,
                s.model,
                s.effort,
                s.cc_state,
                s.cc_status,
                s.cc_fresh,
                s.pid,
                s.last_hook,
                s.pending_permission,
                s.group_name,
                s.waiting_reason,
                s.resolved,
                s.resolved_reason,
                s.pr_url,
                s.pr_state,
                s.pr_draft,
                s.pr_review,
                s.last_report,
                s.last_report_at,
                s.created_at,
                s.updated_at,
                s.last_activity_at,
                s.last_hook_at,
                s.gone_at,
                s.cleaned_at,
                s.cleanup_blocked,
                s.synced_at,
                s.ids_at,
                s.removed_at,
            ],
        )
        .with_context(|| format!("updating session {}", s.name))?;
    anyhow::ensure!(n == 1, "no session named {}", s.name);
    Ok(())
}

/// Moves a row's snapshot watermark forward without rewriting it (a poll that changed nothing).
pub fn set_synced_at(conn: &Connection, name: &str, taken_at: i64) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET synced_at = ?2 WHERE name = ?1 AND (synced_at IS NULL OR synced_at < ?2)",
        params![name, taken_at],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This process may clean the thread up; `release_cleanup` with the same `now` afterwards.
    Claimed,
    Cleaned,
    /// Another process claimed it less than `CLEANUP_CLAIM_STALE_MS` ago.
    Held,
}

/// Claims a thread for cleanup in one `BEGIN IMMEDIATE` transaction, so two processes never
/// clean the same thread at once. `now` identifies the claim.
pub fn claim_cleanup(conn: &Connection, name: &str, now: i64) -> Result<Claim> {
    immediate(conn, |conn| {
        let (cleaned_at, cleaning_at): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT cleaned_at, cleaning_at FROM sessions WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .with_context(|| format!("no thread named {name}"))?;
        if cleaned_at.is_some() {
            return Ok(Claim::Cleaned);
        }
        if cleaning_at.is_some_and(|at| now - at < CLEANUP_CLAIM_STALE_MS) {
            return Ok(Claim::Held);
        }
        conn.execute(
            "UPDATE sessions SET cleaning_at = ?2 WHERE name = ?1",
            params![name, now],
        )?;
        Ok(Claim::Claimed)
    })
}

/// Drops the claim `claim_cleanup` made at `claimed_at`, unless another process has taken it over
/// since (it went stale).
pub fn release_cleanup(conn: &Connection, name: &str, claimed_at: i64) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET cleaning_at = NULL WHERE name = ?1 AND cleaning_at = ?2",
        params![name, claimed_at],
    )?;
    Ok(())
}

/// Removes a session that never started (a failed spawn), with its events and reports.
pub fn delete_session(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM events WHERE session_name = ?1", [name])?;
    conn.execute("DELETE FROM reports WHERE session_name = ?1", [name])?;
    conn.execute("DELETE FROM sessions WHERE name = ?1", [name])?;
    Ok(())
}

pub fn get_session(conn: &Connection, name: &str) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE name = ?1");
    Ok(conn.query_row(&sql, [name], session_from_row).optional()?)
}

pub fn get_session_by_session_id(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE session_id = ?1");
    Ok(conn
        .query_row(&sql, [session_id], session_from_row)
        .optional()?)
}

/// A project's sessions, oldest first; `role` None lists coordinator and threads.
pub fn list_sessions(
    conn: &Connection,
    project_slug: &str,
    role: Option<Role>,
) -> Result<Vec<SessionRow>> {
    let sql = format!(
        "SELECT {SESSION_COLUMNS} FROM sessions
         WHERE project_slug = ?1 AND (?2 IS NULL OR role = ?2)
         ORDER BY created_at, name"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            params![project_slug, role.map(Role::as_str)],
            session_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every session of every project in this chief home, oldest first.
pub fn all_sessions(conn: &Connection) -> Result<Vec<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY created_at, name");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], session_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The project's coordinator row, if any.
pub fn get_coordinator(conn: &Connection, project_slug: &str) -> Result<Option<SessionRow>> {
    Ok(list_sessions(conn, project_slug, Some(Role::Coordinator))?
        .into_iter()
        .next())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub session_name: Option<String>,
    pub session_id: Option<String>,
    pub project_slug: String,
    pub hook: String,
    pub ts: i64,
    pub summary: Option<String>,
}

pub fn insert_event(conn: &Connection, e: &Event) -> Result<()> {
    conn.execute(
        "INSERT INTO events (session_name, session_id, project_slug, hook, ts, summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            e.session_name,
            e.session_id,
            e.project_slug,
            e.hook,
            e.ts,
            e.summary
        ],
    )?;
    Ok(())
}

/// Deletes events older than `before` (unix ms) in every project; returns how many. The board
/// runs it at start so the table doesn't grow without bound.
pub fn prune_events(conn: &Connection, before: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM events WHERE ts < ?1", [before])?)
}

/// The project's latest events, newest first.
pub fn recent_events(conn: &Connection, project_slug: &str, limit: usize) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT session_name, session_id, project_slug, hook, ts, summary FROM events
         WHERE project_slug = ?1 ORDER BY ts DESC, id DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![project_slug, limit as i64], |r| {
            Ok(Event {
                session_name: r.get(0)?,
                session_id: r.get(1)?,
                project_slug: r.get(2)?,
                hook: r.get(3)?,
                ts: r.get(4)?,
                summary: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub session_name: String,
    pub ts: i64,
    pub text: String,
}

pub fn insert_report(conn: &Connection, session_name: &str, ts: i64, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO reports (session_name, ts, text) VALUES (?1, ?2, ?3)",
        params![session_name, ts, text],
    )?;
    Ok(())
}

/// A session's latest `limit` reports, newest first.
pub fn recent_reports(conn: &Connection, session_name: &str, limit: usize) -> Result<Vec<Report>> {
    let mut stmt = conn.prepare(
        "SELECT session_name, ts, text FROM reports
         WHERE session_name = ?1 ORDER BY ts DESC, id DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![session_name, limit as i64], |r| {
            Ok(Report {
                session_name: r.get(0)?,
                ts: r.get(1)?,
                text: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq)]
pub struct GuardDenial {
    pub project_slug: String,
    pub session_name: Option<String>,
    pub tool: String,
    pub target: Option<String>,
    pub reason: String,
    pub ts: i64,
}

pub fn insert_guard_denial(conn: &Connection, d: &GuardDenial) -> Result<()> {
    conn.execute(
        "INSERT INTO guard_denials (project_slug, session_name, tool, target, reason, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            d.project_slug,
            d.session_name,
            d.tool,
            d.target,
            d.reason,
            d.ts
        ],
    )?;
    Ok(())
}

pub fn count_guard_denials(conn: &Connection, project_slug: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM guard_denials WHERE project_slug = ?1",
        [project_slug],
        |r| r.get(0),
    )?)
}

/// The session name the left pane should show; None means the coordinator.
pub fn get_pane_target(conn: &Connection, project_slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT pane_target FROM ui_state WHERE project_slug = ?1",
            [project_slug],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

pub fn set_pane_target(conn: &Connection, project_slug: &str, target: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, pane_target) VALUES (?1, ?2)
         ON CONFLICT(project_slug) DO UPDATE SET pane_target = excluded.pane_target",
        params![project_slug, target],
    )?;
    Ok(())
}

/// Clears `pane_target` only while it still names `name`, so a target the board just wrote
/// isn't lost.
pub fn clear_pane_target_if(conn: &Connection, project_slug: &str, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE ui_state SET pane_target = NULL WHERE project_slug = ?1 AND pane_target = ?2",
        params![project_slug, name],
    )?;
    Ok(())
}

/// The session `chief pane` is attached to; None between attaches or when no pane runs.
pub fn get_pane_showing(conn: &Connection, project_slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT pane_showing FROM ui_state WHERE project_slug = ?1",
            [project_slug],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

pub fn set_pane_showing(conn: &Connection, project_slug: &str, name: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, pane_showing) VALUES (?1, ?2)
         ON CONFLICT(project_slug) DO UPDATE SET pane_showing = excluded.pane_showing",
        params![project_slug, name],
    )?;
    Ok(())
}

/// The id of the layout session `chief up` created for the project, if it stored one.
pub fn get_rex_session(conn: &Connection, project_slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT rex_session FROM ui_state WHERE project_slug = ?1",
            [project_slug],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

pub fn set_rex_session(conn: &Connection, project_slug: &str, id: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, rex_session) VALUES (?1, ?2)
         ON CONFLICT(project_slug) DO UPDATE SET rex_session = excluded.rex_session",
        params![project_slug, id],
    )?;
    Ok(())
}

/// The running `chief pane`'s pid and start time (`proc::start_time`), if one recorded itself.
pub fn get_pane_process(conn: &Connection, project_slug: &str) -> Result<Option<(i32, String)>> {
    Ok(conn
        .query_row(
            "SELECT pane_pid, pane_started FROM ui_state
             WHERE project_slug = ?1 AND pane_pid IS NOT NULL AND pane_started IS NOT NULL",
            [project_slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn set_pane_process(
    conn: &Connection,
    project_slug: &str,
    pid: i32,
    started: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, pane_pid, pane_started) VALUES (?1, ?2, ?3)
         ON CONFLICT(project_slug) DO UPDATE SET pane_pid = excluded.pane_pid,
           pane_started = excluded.pane_started",
        params![project_slug, pid, started],
    )?;
    Ok(())
}

/// Forgets the recorded pane only while it is still `pid`, so a pane that started since keeps
/// its record.
pub fn clear_pane_process_if(conn: &Connection, project_slug: &str, pid: i32) -> Result<()> {
    conn.execute(
        "UPDATE ui_state SET pane_pid = NULL, pane_started = NULL
         WHERE project_slug = ?1 AND pane_pid = ?2",
        params![project_slug, pid],
    )?;
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("nested/chief.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn migrates_a_fresh_db_with_pragmas() {
        let (dir, conn) = fresh();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.sort();
        assert_eq!(
            tables,
            [
                "events",
                "guard_denials",
                "projects",
                "reports",
                "sessions",
                "ui_state"
            ]
        );
        drop(conn);
        // Reopening is a no-op migration.
        let conn = open(&dir.path().join("nested/chief.db")).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn session_round_trip_and_lookups() {
        let (_dir, conn) = fresh();
        register_project(&conn, "p", Path::new("/root/projects/p"), 1).unwrap();
        register_project(&conn, "p", Path::new("/moved/p"), 2).unwrap();
        assert_eq!(
            project_dir(&conn, "p").unwrap().as_deref(),
            Some("/moved/p")
        );
        assert_eq!(project_dir(&conn, "q").unwrap(), None);

        let mut t = SessionRow::new("p-fix-1a2b", "p", Role::Thread, 10);
        t.title = Some("Fix".into());
        t.pr_draft = Some(true);
        t.pid = Some(4242);
        insert_session(&conn, &t).unwrap();
        let c = SessionRow::new("chief-p", "p", Role::Coordinator, 5);
        insert_session(&conn, &c).unwrap();

        assert_eq!(get_session(&conn, "p-fix-1a2b").unwrap().unwrap(), t);
        assert_eq!(get_session(&conn, "nope").unwrap(), None);

        t.session_id = Some("uuid-1".into());
        t.pending_permission = true;
        t.cc_fresh = true;
        t.last_report = Some("done".into());
        t.cleaned_at = Some(20);
        t.cleanup_blocked = Some("2 unpushed commits".into());
        t.synced_at = Some(15);
        t.removed_at = Some(20);
        t.updated_at = 30;
        update_session(&conn, &t).unwrap();
        t.ids_at = Some(30);
        assert_eq!(
            get_session_by_session_id(&conn, "uuid-1").unwrap().unwrap(),
            t
        );

        let all = list_sessions(&conn, "p", None).unwrap();
        assert_eq!(
            all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["chief-p", "p-fix-1a2b"]
        );
        assert_eq!(list_sessions(&conn, "p", Some(Role::Thread)).unwrap(), [t]);
        assert_eq!(
            get_coordinator(&conn, "p").unwrap().unwrap().name,
            "chief-p"
        );
        assert!(list_sessions(&conn, "q", None).unwrap().is_empty());

        let ghost = SessionRow::new("ghost", "p", Role::Thread, 1);
        assert!(update_session(&conn, &ghost).is_err());
    }

    /// Whoever writes an id (a hook's title match, start_thread, a sync), `ids_at` says when.
    #[test]
    fn ids_at_follows_id_changes() {
        let (_dir, conn) = fresh();
        register_project(&conn, "p", Path::new("/p"), 1).unwrap();
        let ids_at = |conn: &Connection| get_session(conn, "t").unwrap().unwrap().ids_at;
        let mut t = SessionRow::new("t", "p", Role::Thread, 10);
        insert_session(&conn, &t).unwrap();
        assert_eq!(ids_at(&conn), None, "not spawned");
        t.short_id = Some("s1".into());
        t.updated_at = 20;
        update_session(&conn, &t).unwrap();
        assert_eq!(ids_at(&conn), Some(20));
        // Other changes keep it, even from a row read before it was stamped.
        t.updated_at = 30;
        t.title = Some("x".into());
        update_session(&conn, &t).unwrap();
        assert_eq!(ids_at(&conn), Some(20));
        t.session_id = Some("sid".into());
        t.updated_at = 50;
        update_session(&conn, &t).unwrap();
        assert_eq!(ids_at(&conn), Some(50));

        // A row inserted with ids has them since it was written.
        let mut u = SessionRow::new("u", "p", Role::Thread, 60);
        u.session_id = Some("sid-u".into());
        insert_session(&conn, &u).unwrap();
        assert_eq!(get_session(&conn, "u").unwrap().unwrap().ids_at, Some(60));
    }

    #[test]
    fn sessions_need_a_registered_project() {
        let (_dir, conn) = fresh();
        let s = SessionRow::new("x", "unregistered", Role::Thread, 1);
        assert!(insert_session(&conn, &s).is_err());
    }

    #[test]
    fn events_reports_denials_and_pane_target() {
        let (_dir, conn) = fresh();
        for (ts, hook) in [(1, "SessionStart"), (2, "Stop")] {
            insert_event(
                &conn,
                &Event {
                    session_name: Some("t".into()),
                    session_id: None,
                    project_slug: "p".into(),
                    hook: hook.into(),
                    ts,
                    summary: Some(hook.to_lowercase()),
                },
            )
            .unwrap();
        }
        let ev = recent_events(&conn, "p", 10).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].hook, "Stop");
        assert_eq!(prune_events(&conn, 2).unwrap(), 1);
        let ev = recent_events(&conn, "p", 10).unwrap();
        assert_eq!(ev.len(), 1, "the event at the cutoff stays");
        assert_eq!(ev[0].hook, "Stop");

        insert_report(&conn, "t", 1, "first").unwrap();
        insert_report(&conn, "t", 2, "second").unwrap();
        let reps = recent_reports(&conn, "t", 1).unwrap();
        assert_eq!(reps.len(), 1);
        assert_eq!(reps[0].text, "second");

        insert_guard_denial(
            &conn,
            &GuardDenial {
                project_slug: "p".into(),
                session_name: Some("chief-p".into()),
                tool: "ListAgents".into(),
                target: None,
                reason: "no".into(),
                ts: 3,
            },
        )
        .unwrap();
        assert_eq!(count_guard_denials(&conn, "p").unwrap(), 1);
        assert_eq!(count_guard_denials(&conn, "q").unwrap(), 0);

        assert_eq!(get_pane_target(&conn, "p").unwrap(), None);
        set_pane_target(&conn, "p", Some("t")).unwrap();
        assert_eq!(get_pane_target(&conn, "p").unwrap().as_deref(), Some("t"));
        set_pane_target(&conn, "p", None).unwrap();
        assert_eq!(get_pane_target(&conn, "p").unwrap(), None);

        set_pane_target(&conn, "p", Some("new")).unwrap();
        clear_pane_target_if(&conn, "p", "old").unwrap();
        assert_eq!(get_pane_target(&conn, "p").unwrap().as_deref(), Some("new"));
        clear_pane_target_if(&conn, "p", "new").unwrap();
        assert_eq!(get_pane_target(&conn, "p").unwrap(), None);

        assert_eq!(get_pane_showing(&conn, "p").unwrap(), None);
        set_pane_showing(&conn, "p", Some("chief-p")).unwrap();
        set_pane_target(&conn, "p", Some("t")).unwrap();
        assert_eq!(
            get_pane_showing(&conn, "p").unwrap().as_deref(),
            Some("chief-p")
        );
        assert_eq!(get_pane_target(&conn, "p").unwrap().as_deref(), Some("t"));
        set_pane_showing(&conn, "q", Some("chief-q")).unwrap();
        assert_eq!(get_pane_target(&conn, "q").unwrap(), None);
        set_pane_showing(&conn, "p", None).unwrap();
        assert_eq!(get_pane_showing(&conn, "p").unwrap(), None);

        assert_eq!(get_rex_session(&conn, "p").unwrap(), None);
        set_rex_session(&conn, "p", Some("session:1")).unwrap();
        assert_eq!(
            get_rex_session(&conn, "p").unwrap().as_deref(),
            Some("session:1")
        );
        assert_eq!(get_pane_showing(&conn, "p").unwrap(), None, "kept apart");
        set_rex_session(&conn, "p", None).unwrap();
        assert_eq!(get_rex_session(&conn, "p").unwrap(), None);

        assert_eq!(get_pane_process(&conn, "p").unwrap(), None);
        set_pane_process(&conn, "p", 41, "100.5").unwrap();
        assert_eq!(
            get_pane_process(&conn, "p").unwrap(),
            Some((41, "100.5".to_string()))
        );
        // A newer pane took over: the old one's exit leaves it alone.
        set_pane_process(&conn, "p", 42, "200.5").unwrap();
        clear_pane_process_if(&conn, "p", 41).unwrap();
        assert_eq!(
            get_pane_process(&conn, "p").unwrap(),
            Some((42, "200.5".to_string()))
        );
        clear_pane_process_if(&conn, "p", 42).unwrap();
        assert_eq!(get_pane_process(&conn, "p").unwrap(), None);
    }
}
