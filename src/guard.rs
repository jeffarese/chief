//! Message guard (ARCHITECTURE §6). `evaluate` is pure; `hook::run` gathers its inputs.

use crate::state::Role;

/// A live session from a fresh `claude agents` snapshot.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub name: Option<String>,
    pub pid: Option<u32>,
}

/// One of this project's threads, from the registry.
#[derive(Debug, Clone)]
pub struct ProjectThread {
    pub name: String,
    pub resolved: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct GuardInput<'a> {
    pub role: Role,
    pub tool: &'a str,
    /// `SendMessage.to`, verbatim.
    pub to: Option<&'a str>,
    pub coordinator_name: &'a str,
    pub coordinator_pid: Option<u32>,
    pub threads: &'a [ProjectThread],
    pub live: &'a [LiveSession],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

pub fn evaluate(input: &GuardInput) -> Decision {
    let _ = input;
    todo!("M2: implement §6 with table-driven tests")
}

/// The PreToolUse deny JSON Claude Code expects on stdout.
pub fn deny_json(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}
