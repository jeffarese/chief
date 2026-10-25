//! Wrapper around the `claude` CLI (ARCHITECTURE §1, §7). `CHIEF_CLAUDE_BIN` overrides the binary.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

pub const MIN_VERSION: (u32, u32, u32) = (2, 1, 310);

/// One row of `claude agents --json --all`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRow {
    /// Short id that `claude --bg` prints; absent on interactive sessions.
    pub id: Option<String>,
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub kind: Option<String>,
    pub started_at: Option<i64>,
    pub state: Option<String>,
    pub status: Option<String>,
    /// Absent when the process is gone (asleep).
    pub pid: Option<u32>,
}

/// Arguments for a background session.
#[derive(Debug, Clone)]
pub struct BgSpawn {
    pub cwd: PathBuf,
    pub name: String,
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub settings: PathBuf,
    pub mcp_config: Option<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
    pub disallowed_tools: Vec<String>,
    pub append_system_prompt: String,
    /// None starts the session waiting for its first message.
    pub prompt: Option<String>,
}

pub struct Claude {
    pub bin: PathBuf,
}

impl Claude {
    pub fn from_env() -> Self {
        let bin = std::env::var_os("CHIEF_CLAUDE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("claude"));
        Claude { bin }
    }

    /// Fails when `claude --version` is older than MIN_VERSION.
    pub fn check_version(&self) -> Result<()> {
        todo!("M1")
    }

    /// `claude agents --json --all`.
    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        todo!("M1")
    }

    /// Starts a background session and returns its short id.
    pub fn spawn_bg(&self, spec: &BgSpawn) -> Result<String> {
        let _ = spec;
        todo!("M1")
    }

    pub fn respawn(&self, short_id: &str) -> Result<()> {
        let _ = short_id;
        todo!("M1")
    }

    pub fn stop(&self, short_id: &str) -> Result<()> {
        let _ = short_id;
        todo!("M1")
    }
}

/// Parses `claude agents --json` output.
pub fn parse_agents(json: &str) -> Result<Vec<AgentRow>> {
    let _ = json;
    todo!("M1")
}

/// Extracts the short id from `backgrounded · <short-id> · <name>`.
pub fn parse_bg_short_id(stdout: &str) -> Option<String> {
    let _ = stdout;
    todo!("M1")
}

/// Reads `~/.claude.json` → `projects[<path>].hasTrustDialogAccepted`. Read-only, never writes.
pub fn is_trusted(path: &Path) -> Result<bool> {
    let _ = path;
    todo!("M1")
}
