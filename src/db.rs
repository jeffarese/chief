//! SQLite registry (ARCHITECTURE §4). Migrations run on open, tracked by `PRAGMA user_version`.

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

/// Opens (creating if needed) the database with WAL, busy_timeout=5000 and foreign keys on, then
/// migrates it to the latest schema.
pub fn open(path: &Path) -> Result<Connection> {
    let _ = path;
    todo!("M1")
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
    pub pid: Option<u32>,
    pub last_hook: Option<String>,
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

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
