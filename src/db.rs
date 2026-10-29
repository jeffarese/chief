//! SQLite registry (ARCHITECTURE §4). Migrations run on open, tracked by `PRAGMA user_version`.

use std::path::{Path, PathBuf};
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
    // time to prove a pid is still that pane and not a reuse. And the tmux window `chief up`
    // opened, with its server (§9).
    "ALTER TABLE ui_state ADD COLUMN pane_pid INTEGER;
     ALTER TABLE ui_state ADD COLUMN pane_started TEXT;
     ALTER TABLE ui_state ADD COLUMN tmux_window TEXT;",
    // A thread's own Claude Code account, when start_thread put it in another than the project's
    // (§3): the agp account id and label, and its config folder (NULL with an id: the default
    // account). Rows without an id run in the project's account.
    "ALTER TABLE sessions ADD COLUMN account_id TEXT;
     ALTER TABLE sessions ADD COLUMN account_label TEXT;
     ALTER TABLE sessions ADD COLUMN account_dir TEXT;",
    // The subagents (in-process teammates) a session spawned in its current process, which the
    // guard lets it and them message (§6 "Subagents"). A new process has none (§1), so its
    // SessionStart forgets them.
    "CREATE TABLE teammates (
       session_name TEXT NOT NULL,
       session_id   TEXT NOT NULL,
       agent_id     TEXT NOT NULL,
       name         TEXT,
       recorded_at  INTEGER NOT NULL,
       PRIMARY KEY (session_name, agent_id)
     );",
    // Background work, which a session's own turn doesn't show (§5 "Background work"): which
    // subagents are mid-turn and how many, how many background commands its last Stop listed as
    // running, and when a thread last ended a turn while any of that still ran.
    "ALTER TABLE teammates ADD COLUMN busy INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE sessions ADD COLUMN subagents_busy INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE sessions ADD COLUMN background_running INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE sessions ADD COLUMN stopped_early_at INTEGER;",
    // Undoing a removed verdict (§5): the reason a reopened thread kept, which the verdict
    // replaces with `removed`, so a session listed again gets it back and stays reopened. And
    // which cleaned rows may carry a wrong verdict from before migration 6: it set only
    // `cleaned_at` on a resolved row, and migration 6's backfill gave such rows no `removed_at`
    // to undo. Every row cleaned before now is one of them or was removed by `claude rm`, whose
    // session is never listed again.
    "ALTER TABLE sessions ADD COLUMN reason_before_removed TEXT;
     ALTER TABLE sessions ADD COLUMN legacy_cleaned INTEGER NOT NULL DEFAULT 0;
     UPDATE sessions SET legacy_cleaned = 1
       WHERE cleaned_at IS NOT NULL AND removed_at IS NULL;",
    // When the project was archived (§2 `chief archive`): nothing of it runs, and the launcher
    // and `chief status` leave it out. NULL is a live project.
    "ALTER TABLE projects ADD COLUMN archived_at INTEGER;",
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

/// Every hook opens the database, so a current schema is checked with a read alone: a write
/// lock per check would double the write transactions of each tool call. Only a database behind
/// takes `BEGIN IMMEDIATE`, and reads the version again inside it, so processes opening it at
/// once migrate it once.
fn migrate(conn: &mut Connection) -> Result<()> {
    if user_version(conn)? >= MIGRATIONS.len() {
        return Ok(());
    }
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
/// hook or board poll in another process. Rolls back on error, a failed COMMIT included: that
/// leaves the transaction open, and a long-lived connection (the board's) would stay in it.
pub fn immediate<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = f(conn).and_then(|v| {
        conn.execute_batch("COMMIT")?;
        Ok(v)
    });
    if result.is_err() && !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
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
    /// The agp account the session runs in, when it has one of its own (§3); None: the
    /// project's account.
    pub account_id: Option<String>,
    /// That account's agp label (`PB2`), for display.
    pub account_label: Option<String>,
    /// That account's config folder; None with an `account_id` is the default account.
    pub account_dir: Option<String>,
    /// How many of its subagents are mid-turn, from SubagentStart to SubagentStop (§5
    /// "Background work").
    pub subagents_busy: u32,
    /// How many background commands its last Stop listed as running (§5 "Background work").
    pub background_running: u32,
    /// When a thread last ended a turn while background work still ran: its coordinator was told
    /// it was idle, so the Stop that finds nothing running asks it to report (§5).
    pub stopped_early_at: Option<i64>,
    /// The `resolved_reason` a reopened thread kept when a sync marked it removed (§5), put
    /// back if the session is listed again.
    pub reason_before_removed: Option<String>,
    /// Cleaned before migration 13, so possibly by a wrong removed verdict from before
    /// migration 6 that left no `removed_at` (§5): a snapshot taken after `cleaned_at` that lists
    /// the session again clears `cleaned_at`.
    pub legacy_cleaned: bool,
}

impl SessionRow {
    /// The Claude Code account the session runs in (`CLAUDE_CONFIG_DIR`, None = the default
    /// account): its own when it has an `account_id`, else the project's.
    pub fn account(&self, project: Option<&Path>) -> Option<PathBuf> {
        if self.account_id.is_some() {
            self.account_dir.as_deref().map(PathBuf::from)
        } else {
            project.map(Path::to_path_buf)
        }
    }

    /// The account's name for people: its agp label, else its id; None for the project's.
    pub fn account_shown(&self) -> Option<&str> {
        self.account_label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .or(self.account_id.as_deref())
    }

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
    removed_at, account_id, account_label, account_dir, subagents_busy, background_running, \
    stopped_early_at, reason_before_removed, legacy_cleaned";

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
        account_id: r.get(39)?,
        account_label: r.get(40)?,
        account_dir: r.get(41)?,
        subagents_busy: r.get(42)?,
        background_running: r.get(43)?,
        stopped_early_at: r.get(44)?,
        reason_before_removed: r.get(45)?,
        legacy_cleaned: r.get(46)?,
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

/// When the project was archived, if it is.
pub fn archived_at(conn: &Connection, slug: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT archived_at FROM projects WHERE slug = ?1",
            [slug],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

pub fn archive_project(conn: &Connection, slug: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE projects SET archived_at = ?2 WHERE slug = ?1",
        params![slug, now],
    )?;
    Ok(())
}

/// Whether the project was archived (it isn't any more).
pub fn unarchive_project(conn: &Connection, slug: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE projects SET archived_at = NULL WHERE slug = ?1 AND archived_at IS NOT NULL",
        [slug],
    )?;
    Ok(changed > 0)
}

pub fn insert_session(conn: &Connection, s: &SessionRow) -> Result<()> {
    let sql = format!(
        "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
         ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, \
         ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, \
         ?47)"
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
            s.account_id,
            s.account_label,
            s.account_dir,
            s.subagents_busy,
            s.background_running,
            s.stopped_early_at,
            s.reason_before_removed,
            s.legacy_cleaned,
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
           removed_at = ?39, account_id = ?40, account_label = ?41, account_dir = ?42,
           subagents_busy = ?43, background_running = ?44, stopped_early_at = ?45,
           reason_before_removed = ?46, legacy_cleaned = ?47
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
                s.account_id,
                s.account_label,
                s.account_dir,
                s.subagents_busy,
                s.background_running,
                s.stopped_early_at,
                s.reason_before_removed,
                s.legacy_cleaned,
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

/// Removes a session that never started (a failed spawn), with its events, reports and
/// teammates, all or nothing: a hook between the statements could otherwise leave an event or
/// teammate of a session that no longer exists.
pub fn delete_session(conn: &Connection, name: &str) -> Result<()> {
    immediate(conn, |conn| delete_session_rows(conn, name))
}

/// `delete_session` after a failed spawn, unless the session's hooks attached the row: only a
/// running session's hooks store a session id (§4), so it started. The check and the delete are
/// one transaction, so hooks that attach it in between keep it. Whether no attached row is left
/// (it was deleted, or there was none).
pub fn delete_unattached_session(conn: &Connection, name: &str) -> Result<bool> {
    immediate(conn, |conn| {
        let session_id: Option<Option<String>> = conn
            .query_row(
                "SELECT session_id FROM sessions WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        if matches!(session_id, Some(Some(_))) {
            return Ok(false);
        }
        delete_session_rows(conn, name)?;
        Ok(true)
    })
}

fn delete_session_rows(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM events WHERE session_name = ?1", [name])?;
    conn.execute("DELETE FROM reports WHERE session_name = ?1", [name])?;
    forget_teammates(conn, name)?;
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

/// A subagent a session spawned (§6 "Subagents").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teammate {
    /// Claude Code's id for it (`ahelper-886b99f57d4badec`).
    pub agent_id: String,
    /// The `name` its Agent call gave it; None when it had none, or none is known yet.
    pub name: Option<String>,
}

/// Records a subagent of the session `session_name`, under the session id its hook carried.
/// A name goes to the latest agent given it, as Claude Code delivers it (§1). `name` None keeps
/// a name already recorded: SubagentStart, which fires again on every resume, knows only the id.
pub fn record_teammate(
    conn: &Connection,
    session_name: &str,
    session_id: &str,
    agent_id: &str,
    name: Option<&str>,
    now: i64,
) -> Result<()> {
    if let Some(name) = name {
        conn.execute(
            "UPDATE teammates SET name = NULL
             WHERE session_name = ?1 AND name = ?2 AND agent_id <> ?3",
            params![session_name, name, agent_id],
        )?;
    }
    conn.execute(
        "INSERT INTO teammates (session_name, session_id, agent_id, name, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_name, agent_id) DO UPDATE SET session_id = excluded.session_id,
           name = coalesce(excluded.name, name), recorded_at = excluded.recorded_at",
        params![session_name, session_id, agent_id, name, now],
    )?;
    Ok(())
}

/// Marks a recorded subagent mid-turn (SubagentStart) or done with its turn (SubagentStop).
pub fn set_teammate_busy(
    conn: &Connection,
    session_name: &str,
    agent_id: &str,
    busy: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE teammates SET busy = ?3 WHERE session_name = ?1 AND agent_id = ?2",
        params![session_name, agent_id, busy],
    )?;
    Ok(())
}

/// Marks every subagent of `session_name` done with its turn but the agents in `running`: its
/// Stop listed only those as running.
pub fn idle_teammates(conn: &Connection, session_name: &str, running: &[&str]) -> Result<()> {
    let busy: Vec<String> = conn
        .prepare("SELECT agent_id FROM teammates WHERE session_name = ?1 AND busy = 1")?
        .query_map([session_name], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for agent in busy.iter().filter(|a| !running.contains(&a.as_str())) {
        set_teammate_busy(conn, session_name, agent, false)?;
    }
    Ok(())
}

/// How many subagents `session_name` recorded under `session_id` are mid-turn.
pub fn busy_teammates(conn: &Connection, session_name: &str, session_id: &str) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM teammates WHERE session_name = ?1 AND session_id = ?2 AND busy = 1",
        params![session_name, session_id],
        |r| r.get(0),
    )?)
}

/// Forgets every subagent of `session_name`, whose process started again; returns how many.
pub fn forget_teammates(conn: &Connection, session_name: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM teammates WHERE session_name = ?1",
        [session_name],
    )?)
}

/// The subagents `session_name` recorded under `session_id`, in the order first recorded.
pub fn teammates(conn: &Connection, session_name: &str, session_id: &str) -> Result<Vec<Teammate>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, name FROM teammates WHERE session_name = ?1 AND session_id = ?2
         ORDER BY rowid",
    )?;
    let rows = stmt
        .query_map(params![session_name, session_id], |r| {
            Ok(Teammate {
                agent_id: r.get(0)?,
                name: r.get(1)?,
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

/// Sets `pane_showing` whoever runs (`chief down`, tests). A pane writes through
/// `set_pane_showing_as`.
pub fn set_pane_showing(conn: &Connection, project_slug: &str, name: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, pane_showing) VALUES (?1, ?2)
         ON CONFLICT(project_slug) DO UPDATE SET pane_showing = excluded.pane_showing",
        params![project_slug, name],
    )?;
    Ok(())
}

/// Sets `pane_showing` for the pane `pid` (None: a pane that couldn't record itself), only while
/// `pane_pid` is that pane's (NULL for None), so an older pane can't clear or overwrite what the
/// newest one shows (§9). Whether it wrote.
pub fn set_pane_showing_as(
    conn: &Connection,
    project_slug: &str,
    pid: Option<i32>,
    name: Option<&str>,
) -> Result<bool> {
    let written = match pid {
        // Its record made the row.
        Some(pid) => conn.execute(
            "UPDATE ui_state SET pane_showing = ?3 WHERE project_slug = ?1 AND pane_pid = ?2",
            params![project_slug, pid, name],
        )?,
        None => conn.execute(
            "INSERT INTO ui_state (project_slug, pane_showing) VALUES (?1, ?2)
             ON CONFLICT(project_slug) DO UPDATE SET pane_showing = excluded.pane_showing
               WHERE pane_pid IS NULL",
            params![project_slug, name],
        )?,
    };
    Ok(written > 0)
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

/// The tmux window `chief up` opened for the project (`<window id> <server pid> <socket>`), if
/// it stored one.
pub fn get_tmux_window(conn: &Connection, project_slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT tmux_window FROM ui_state WHERE project_slug = ?1",
            [project_slug],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

pub fn set_tmux_window(conn: &Connection, project_slug: &str, id: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO ui_state (project_slug, tmux_window) VALUES (?1, ?2)
         ON CONFLICT(project_slug) DO UPDATE SET tmux_window = excluded.tmux_window",
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

/// Forgets the recorded pane and what it showed, only while it is still `pid`, so a pane that
/// started since keeps its record. Run by the pane as it exits, and for a pane found gone.
pub fn clear_pane_process_if(conn: &Connection, project_slug: &str, pid: i32) -> Result<()> {
    conn.execute(
        "UPDATE ui_state SET pane_pid = NULL, pane_started = NULL, pane_showing = NULL
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
                "teammates",
                "ui_state"
            ]
        );
        assert_eq!(MIGRATIONS.len(), 14);
        drop(conn);
        // Reopening is a no-op migration.
        let conn = open(&dir.path().join("nested/chief.db")).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
    }

    /// A database as an older chief left it: the first `version` migrations, nothing else.
    fn at_version(path: &Path, version: usize) -> Connection {
        let conn = Connection::open(path).unwrap();
        for sql in &MIGRATIONS[..version] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", version as i64)
            .unwrap();
        conn
    }

    /// Migration 6's backfill (ids, sync-made removals) and migration 13's (which cleaned rows
    /// may carry a wrong removed verdict from before migration 6, §5).
    #[test]
    fn upgrades_a_v5_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chief.db");
        let old = at_version(&path, 5);
        old.execute_batch(
            "INSERT INTO projects VALUES ('p', '/p', 1);
             INSERT INTO sessions (name, project_slug, role, short_id, session_id, resolved,
                 resolved_reason, cleaned_at, created_at, updated_at, last_activity_at) VALUES
               ('removed', 'p', 'thread', 'r1', 'r-sid', 1, 'removed', 100, 1, 90, 1),
               ('kept-reason', 'p', 'thread', 'k1', 'k-sid', 1, 'user', 200, 1, 210, 1),
               ('open', 'p', 'thread', 'o1', NULL, 0, NULL, NULL, 1, 70, 1),
               ('unspawned', 'p', 'thread', NULL, NULL, 0, NULL, NULL, 1, 50, 1);",
        )
        .unwrap();
        drop(old);
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        let row = |name: &str| get_session(&conn, name).unwrap().unwrap();
        let backfilled = |name: &str| {
            let r = row(name);
            (r.ids_at, r.removed_at, r.legacy_cleaned)
        };
        assert_eq!(backfilled("removed"), (Some(90), Some(100), false));
        // The old verdict on a resolved row looks like chief's own cleanup: flagged.
        assert_eq!(backfilled("kept-reason"), (Some(210), None, true));
        assert_eq!(backfilled("open"), (Some(70), None, false));
        assert_eq!(backfilled("unspawned"), (None, None, false));
        assert_eq!(row("kept-reason").resolved_reason.as_deref(), Some("user"));
        assert_eq!(row("removed").reason_before_removed, None);
    }

    /// Migration 12 gives existing teammates and sessions no background work, and migration 13
    /// flags rows cleaned until then.
    #[test]
    fn upgrades_a_v11_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chief.db");
        let old = at_version(&path, 11);
        old.execute_batch(
            "INSERT INTO projects VALUES ('p', '/p', 1);
             INSERT INTO sessions (name, project_slug, role, short_id, session_id, resolved,
                 resolved_reason, cleaned_at, created_at, updated_at, last_activity_at) VALUES
               ('lead', 'p', 'thread', 'l1', 'l-sid', 0, NULL, NULL, 1, 10, 1),
               ('cleaned', 'p', 'thread', 'c1', 'c-sid', 1, 'merged', 300, 1, 310, 1);
             INSERT INTO teammates VALUES ('lead', 'l-sid', 'ahelper-1', 'helper', 5);",
        )
        .unwrap();
        drop(old);
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        assert_eq!(busy_teammates(&conn, "lead", "l-sid").unwrap(), 0);
        let busy: i64 = conn
            .query_row("SELECT busy FROM teammates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 0);
        let lead = get_session(&conn, "lead").unwrap().unwrap();
        assert_eq!(
            (
                lead.subagents_busy,
                lead.background_running,
                lead.stopped_early_at,
                lead.legacy_cleaned
            ),
            (0, 0, None, false)
        );
        assert!(
            get_session(&conn, "cleaned")
                .unwrap()
                .unwrap()
                .legacy_cleaned
        );
        assert_eq!(
            teammates(&conn, "lead", "l-sid").unwrap(),
            [Teammate {
                agent_id: "ahelper-1".into(),
                name: Some("helper".into())
            }]
        );
    }

    /// Every hook opens the database: at the current version that takes no write lock, so it
    /// doesn't wait behind a writer.
    #[test]
    fn opening_a_current_database_takes_no_write_lock() {
        let (dir, writer) = fresh();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start = std::time::Instant::now();
        let conn = open(&dir.path().join("nested/chief.db")).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// Processes opening a database behind at once migrate it once: each re-reads the version
    /// inside its write transaction.
    #[test]
    fn concurrent_opens_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chief.db");
        drop(at_version(&path, 5));
        let versions: Vec<usize> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| schema_version(&open(&path).unwrap()).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(versions, [MIGRATIONS.len(); 4]);
    }

    /// A COMMIT that fails leaves its transaction open; `immediate` rolls it back, so the
    /// connection takes the next transaction.
    #[test]
    fn immediate_rolls_back_a_failed_commit() {
        let (_dir, conn) = fresh();
        let err = immediate(&conn, |conn| {
            // Checked at COMMIT: the project isn't registered.
            conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
            insert_session(conn, &SessionRow::new("x", "unregistered", Role::Thread, 1))
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("FOREIGN KEY"), "{err:#}");
        assert!(conn.is_autocommit(), "still inside the failed transaction");
        assert_eq!(get_session(&conn, "x").unwrap(), None);
        immediate(&conn, |conn| {
            register_project(conn, "p", Path::new("/p"), 1)
        })
        .unwrap();
        assert!(project_dir(&conn, "p").unwrap().is_some());
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
        t.account_id = Some("cpb".into());
        t.account_label = Some("PB2".into());
        t.account_dir = Some("/Users/me/.claude-cpb".into());
        t.subagents_busy = 2;
        t.background_running = 1;
        t.stopped_early_at = Some(25);
        t.reason_before_removed = Some("user".into());
        t.legacy_cleaned = true;
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

    /// A row with an agp account id runs in that account (no folder: the default one); without
    /// one, in the project's.
    #[test]
    fn a_rows_account() {
        let project = Path::new("/Users/me/.claude-carlos");
        let mut t = SessionRow::new("t", "p", Role::Thread, 0);
        assert_eq!(t.account(Some(project)), Some(project.to_path_buf()));
        assert_eq!(t.account(None), None);
        assert_eq!(t.account_shown(), None);
        t.account_id = Some("default".into());
        assert_eq!(t.account(Some(project)), None, "agp's default account");
        assert_eq!(t.account_shown(), Some("default"));
        t.account_id = Some("cpb".into());
        t.account_label = Some("PB2".into());
        t.account_dir = Some("/Users/me/.claude-cpb".into());
        assert_eq!(
            t.account(Some(project)),
            Some(PathBuf::from("/Users/me/.claude-cpb"))
        );
        assert_eq!(t.account_shown(), Some("PB2"));
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

    /// A session's subagents (§6 "Subagents"): by agent id, a name moving to the latest agent
    /// given it, kept apart per session and per session id, forgotten all at once.
    #[test]
    fn teammates_per_session() {
        let (_dir, conn) = fresh();
        register_project(&conn, "p", Path::new("/p"), 1).unwrap();
        for name in ["t", "u"] {
            insert_session(&conn, &SessionRow::new(name, "p", Role::Thread, 1)).unwrap();
        }
        let of = |name: &str, sid: &str| teammates(&conn, name, sid).unwrap();
        let mate = |id: &str, name: Option<&str>| Teammate {
            agent_id: id.into(),
            name: name.map(String::from),
        };
        assert!(of("t", "sid-t").is_empty());

        // The Agent call's response names it; SubagentStart (again on every resume) knows only
        // the id and keeps the name.
        record_teammate(&conn, "t", "sid-t", "ahelper-1", Some("helper"), 10).unwrap();
        record_teammate(&conn, "t", "sid-t", "ahelper-1", None, 11).unwrap();
        assert_eq!(of("t", "sid-t"), [mate("ahelper-1", Some("helper"))]);
        // SubagentStart may come first.
        record_teammate(&conn, "t", "sid-t", "aother-2", None, 12).unwrap();
        record_teammate(&conn, "t", "sid-t", "aother-2", Some("other"), 13).unwrap();
        // The latest agent given a name has it.
        record_teammate(&conn, "t", "sid-t", "ahelper-3", Some("helper"), 14).unwrap();
        assert_eq!(
            of("t", "sid-t"),
            [
                mate("ahelper-1", None),
                mate("aother-2", Some("other")),
                mate("ahelper-3", Some("helper")),
            ]
        );
        // Another session's are its own, and a record counts only for the session id it was made
        // under.
        record_teammate(&conn, "u", "sid-u", "ahelper-9", Some("helper"), 15).unwrap();
        assert_eq!(of("u", "sid-u"), [mate("ahelper-9", Some("helper"))]);
        assert!(of("t", "sid-u").is_empty());
        assert_eq!(of("t", "sid-t").len(), 3);

        // Mid-turn from SubagentStart to SubagentStop (§5 "Background work"), counted per
        // session id; recording again keeps it.
        let busy = |name: &str, sid: &str| busy_teammates(&conn, name, sid).unwrap();
        assert_eq!(busy("t", "sid-t"), 0);
        set_teammate_busy(&conn, "t", "ahelper-1", true).unwrap();
        set_teammate_busy(&conn, "t", "aother-2", true).unwrap();
        set_teammate_busy(&conn, "u", "ahelper-9", true).unwrap();
        record_teammate(&conn, "t", "sid-t", "ahelper-1", None, 21).unwrap();
        assert_eq!(busy("t", "sid-t"), 2);
        assert_eq!(busy("t", "sid-u"), 0);
        set_teammate_busy(&conn, "t", "aother-2", false).unwrap();
        set_teammate_busy(&conn, "t", "unknown-agent", false).unwrap();
        assert_eq!(busy("t", "sid-t"), 1);
        // A Stop listing an agent as running keeps it mid-turn, and only it.
        set_teammate_busy(&conn, "t", "ahelper-3", true).unwrap();
        idle_teammates(&conn, "t", &["ahelper-3", "not-recorded"]).unwrap();
        assert_eq!(busy("t", "sid-t"), 1);
        idle_teammates(&conn, "t", &[]).unwrap();
        assert_eq!(busy("t", "sid-t"), 0);
        assert_eq!(busy("u", "sid-u"), 1, "only that session's");

        assert_eq!(forget_teammates(&conn, "t").unwrap(), 3);
        assert!(of("t", "sid-t").is_empty());
        assert_eq!(of("u", "sid-u").len(), 1, "only that session's");
        insert_event(
            &conn,
            &Event {
                session_name: Some("u".into()),
                session_id: None,
                project_slug: "p".into(),
                hook: "Stop".into(),
                ts: 1,
                summary: None,
            },
        )
        .unwrap();
        insert_report(&conn, "u", 1, "done").unwrap();
        delete_session(&conn, "u").unwrap();
        assert!(of("u", "sid-u").is_empty());
        assert_eq!(get_session(&conn, "u").unwrap(), None);
        assert!(recent_events(&conn, "p", 10).unwrap().is_empty());
        assert!(recent_reports(&conn, "u", 10).unwrap().is_empty());
        assert!(conn.is_autocommit());
    }

    #[test]
    fn a_failed_spawns_row_is_deleted_only_while_no_hook_attached_it() {
        let (_dir, conn) = fresh();
        register_project(&conn, "p", Path::new("/p"), 1).unwrap();
        insert_session(&conn, &SessionRow::new("bare", "p", Role::Thread, 1)).unwrap();
        let mut hooked = SessionRow::new("hooked", "p", Role::Thread, 1);
        hooked.session_id = Some("sid-hooked".into());
        insert_session(&conn, &hooked).unwrap();
        insert_report(&conn, "bare", 1, "done").unwrap();

        assert!(delete_unattached_session(&conn, "bare").unwrap());
        assert_eq!(get_session(&conn, "bare").unwrap(), None);
        assert!(recent_reports(&conn, "bare", 10).unwrap().is_empty());
        assert!(!delete_unattached_session(&conn, "hooked").unwrap());
        assert!(get_session(&conn, "hooked").unwrap().is_some());
        // No row: nothing attached is left.
        assert!(delete_unattached_session(&conn, "never").unwrap());
        assert!(conn.is_autocommit());
    }

    #[test]
    fn sessions_need_a_registered_project() {
        let (_dir, conn) = fresh();
        let s = SessionRow::new("x", "unregistered", Role::Thread, 1);
        assert!(insert_session(&conn, &s).is_err());
    }

    #[test]
    fn archiving_a_project_and_back() {
        let (_dir, conn) = fresh();
        register_project(&conn, "p", Path::new("/root/projects/p"), 1).unwrap();
        assert_eq!(archived_at(&conn, "p").unwrap(), None);
        assert!(!unarchive_project(&conn, "p").unwrap(), "wasn't archived");
        archive_project(&conn, "p", 42).unwrap();
        assert_eq!(archived_at(&conn, "p").unwrap(), Some(42));
        // Registering again (chief up does when the row is missing) keeps it archived.
        register_project(&conn, "p", Path::new("/root/projects/p"), 2).unwrap();
        assert_eq!(archived_at(&conn, "p").unwrap(), Some(42));
        assert!(unarchive_project(&conn, "p").unwrap());
        assert_eq!(archived_at(&conn, "p").unwrap(), None);
        assert_eq!(archived_at(&conn, "unknown").unwrap(), None);
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

        assert_eq!(get_tmux_window(&conn, "p").unwrap(), None);
        set_tmux_window(&conn, "p", Some("@3 42 /tmp/s")).unwrap();
        assert_eq!(
            get_tmux_window(&conn, "p").unwrap().as_deref(),
            Some("@3 42 /tmp/s")
        );
        set_tmux_window(&conn, "p", None).unwrap();
        assert_eq!(get_tmux_window(&conn, "p").unwrap(), None);

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

    /// Only the recorded pane writes `pane_showing`: an older one can't clear or overwrite what
    /// the newest shows. A pane that couldn't record itself writes while none is recorded.
    #[test]
    fn pane_showing_belongs_to_the_recorded_pane() {
        let (_dir, conn) = fresh();
        let showing = |conn: &Connection| get_pane_showing(conn, "p").unwrap();
        // Nothing recorded: only a pane without a record writes.
        assert!(!set_pane_showing_as(&conn, "p", Some(41), Some("t")).unwrap());
        assert!(set_pane_showing_as(&conn, "p", None, Some("t")).unwrap());
        assert_eq!(showing(&conn).as_deref(), Some("t"));

        set_pane_process(&conn, "p", 42, "200.5").unwrap();
        assert!(!set_pane_showing_as(&conn, "p", None, None).unwrap());
        assert!(set_pane_showing_as(&conn, "p", Some(42), Some("x")).unwrap());
        // The older pane 41 leaves the newest one's alone.
        assert!(!set_pane_showing_as(&conn, "p", Some(41), None).unwrap());
        assert!(!set_pane_showing_as(&conn, "p", Some(41), Some("y")).unwrap());
        clear_pane_process_if(&conn, "p", 41).unwrap();
        assert_eq!(showing(&conn).as_deref(), Some("x"));
        // Its own exit forgets both.
        clear_pane_process_if(&conn, "p", 42).unwrap();
        assert_eq!(showing(&conn), None);
        assert_eq!(get_pane_process(&conn, "p").unwrap(), None);
    }
}
