//! Thread state derivation (ARCHITECTURE §5). Pure: no IO in this module.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Coordinator,
    Thread,
}

/// Board groups, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Waiting,
    Working,
    Review,
    Landing,
    Idle,
    Failed,
    Resolved,
}

impl Group {
    pub const ORDER: [Group; 7] = [
        Group::Waiting,
        Group::Working,
        Group::Review,
        Group::Landing,
        Group::Idle,
        Group::Failed,
        Group::Resolved,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::Waiting => "Waiting on you",
            Group::Working => "Working",
            Group::Review => "Ready for review",
            Group::Landing => "Landing",
            Group::Idle => "Idle",
            Group::Failed => "Failed",
            Group::Resolved => "Resolved",
        }
    }

    /// The value stored in `sessions.group_name`.
    pub fn as_str(self) -> &'static str {
        match self {
            Group::Waiting => "waiting",
            Group::Working => "working",
            Group::Review => "review",
            Group::Landing => "landing",
            Group::Idle => "idle",
            Group::Failed => "failed",
            Group::Resolved => "resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Group> {
        Group::ORDER.into_iter().find(|g| g.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitingReason {
    Permission,
    Question,
}

/// Hook events chief reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionNotification,
    IdleNotification,
    Stop,
}

/// Everything the derivation looks at, taken from a `sessions` row plus a fresh `claude agents` row.
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    pub resolved: bool,
    pub pending_permission: bool,
    /// `claude agents` state; None when there is no fresh snapshot for this session.
    pub cc_state: Option<&'a str>,
    pub cc_status: Option<&'a str>,
    pub last_hook: Option<HookKind>,
    pub pr_state: Option<&'a str>,
    pub pr_draft: bool,
    pub pr_review: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derived {
    pub group: Group,
    pub reason: Option<WaitingReason>,
}

/// ARCHITECTURE §5, first match wins.
pub fn derive(facts: &Facts) -> Derived {
    let _ = facts;
    todo!("M1: implement §5 and table-driven tests")
}
