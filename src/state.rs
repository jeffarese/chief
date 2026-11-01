//! Thread state derivation (ARCHITECTURE §5). Pure: no IO in this module.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Coordinator,
    Thread,
}

impl Role {
    /// The value stored in `sessions.role`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Coordinator => "coordinator",
            Role::Thread => "thread",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "coordinator" => Some(Role::Coordinator),
            "thread" => Some(Role::Thread),
            _ => None,
        }
    }
}

/// Board groups, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Waiting,
    Working,
    Limited,
    Review,
    Landing,
    Idle,
    Failed,
    Resolved,
}

impl Group {
    pub const ORDER: [Group; 8] = [
        Group::Waiting,
        Group::Working,
        Group::Limited,
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
            Group::Limited => "Limited",
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
            Group::Limited => "limited",
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

impl HookKind {
    /// Maps a hook event (and a Notification's `notification_type`) to a kind; None for events
    /// chief only records.
    pub fn parse(event: &str, notification_type: Option<&str>) -> Option<HookKind> {
        match (event, notification_type) {
            ("SessionStart", _) => Some(HookKind::SessionStart),
            ("UserPromptSubmit", _) => Some(HookKind::UserPromptSubmit),
            ("PreToolUse", _) => Some(HookKind::PreToolUse),
            ("PostToolUse", _) => Some(HookKind::PostToolUse),
            ("Notification", Some("permission_prompt")) => Some(HookKind::PermissionNotification),
            ("Notification", Some("idle_prompt")) => Some(HookKind::IdleNotification),
            ("Stop", _) => Some(HookKind::Stop),
            _ => None,
        }
    }

    /// The value stored in `sessions.last_hook`. Notifications keep their type so the
    /// provisional group survives a round trip through the database.
    pub fn as_str(self) -> &'static str {
        match self {
            HookKind::SessionStart => "SessionStart",
            HookKind::UserPromptSubmit => "UserPromptSubmit",
            HookKind::PreToolUse => "PreToolUse",
            HookKind::PostToolUse => "PostToolUse",
            HookKind::PermissionNotification => "Notification:permission_prompt",
            HookKind::IdleNotification => "Notification:idle_prompt",
            HookKind::Stop => "Stop",
        }
    }

    /// Inverse of `as_str`.
    pub fn from_stored(s: &str) -> Option<HookKind> {
        let (event, kind) = match s.split_once(':') {
            Some((event, kind)) => (event, Some(kind)),
            None => (s, None),
        };
        HookKind::parse(event, kind)
    }

    /// The provisional group says "working" only for hooks fired mid-turn.
    fn means_working(self) -> bool {
        matches!(
            self,
            HookKind::UserPromptSubmit | HookKind::PreToolUse | HookKind::PostToolUse
        )
    }
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
    /// A snapshot no older than the last hook showed no process: nothing can be running, so
    /// rule 5 can't match (a thread that died mid-turn isn't "working" forever).
    pub process_gone: bool,
    /// Its subagents or background commands still run (§5 "Background work"), which its own
    /// turn may have ended without.
    pub background_work: bool,
    /// Its job state says a usage limit stopped its turn and it waits on it (§5 "Limits and
    /// moves").
    pub limited: bool,
    pub pr_state: Option<&'a str>,
    pub pr_draft: bool,
    pub pr_review: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derived {
    pub group: Group,
    pub reason: Option<WaitingReason>,
}

impl Derived {
    fn group(group: Group) -> Self {
        Derived {
            group,
            reason: None,
        }
    }

    fn waiting(reason: WaitingReason) -> Self {
        Derived {
            group: Group::Waiting,
            reason: Some(reason),
        }
    }
}

/// ARCHITECTURE §5, first match wins. Without a snapshot (`cc_state` None) the last hook stands in
/// for rule 5; a Stop or idle notice simply doesn't count as working, so PR rules still apply.
pub fn derive(facts: &Facts) -> Derived {
    if facts.resolved {
        return Derived::group(Group::Resolved);
    }
    if facts.cc_state == Some("failed") {
        return Derived::group(Group::Failed);
    }
    if let Some(reason) = stuck_on(facts) {
        return Derived::waiting(reason);
    }
    // Claude Code blocks a limited turn too; it waits on the clock or another account, not on
    // the user.
    if facts.limited {
        return Derived::group(Group::Limited);
    }
    if facts.cc_state == Some("blocked") {
        return Derived::waiting(WaitingReason::Question);
    }
    // `cc_state` judges the task, not the process: a finished turn can stay "working" while the
    // process idles. Only the status says something runs.
    let working = match (facts.cc_status, facts.cc_state) {
        (Some(status), _) => status == "busy",
        (None, Some(_)) => false,
        (None, None) => facts.last_hook.is_some_and(HookKind::means_working),
    };
    // Subagents and background commands run inside the session's process, so it works on
    // after its own turn ends, until they finish.
    if !facts.process_gone && (working || facts.background_work) {
        return Derived::group(Group::Working);
    }
    if facts.pr_state == Some("OPEN") {
        if facts.pr_review == Some("APPROVED") {
            return Derived::group(Group::Landing);
        }
        if !facts.pr_draft {
            return Derived::group(Group::Review);
        }
    }
    Derived::group(Group::Idle)
}

/// Rule 3: the live process waits on the user in the middle of a turn, and nothing goes on until
/// the user answers. A permission prompt dies with its process and doesn't come back on respawn.
fn stuck_on(facts: &Facts) -> Option<WaitingReason> {
    if facts.process_gone {
        return None;
    }
    if facts.pending_permission {
        return Some(WaitingReason::Permission);
    }
    // A live process waiting on the user that no hook told us about.
    (facts.cc_status == Some("waiting")).then_some(WaitingReason::Question)
}

/// Rule 3 is the one that decides the group: the thread is stuck on the user right now (§5
/// "Telling the coordinator"). A turn that ended with a question (rule 4) isn't: the thread's
/// Stop already sent its coordinator the idle notice.
pub fn blocked(facts: &Facts) -> bool {
    !facts.resolved && facts.cc_state != Some("failed") && stuck_on(facts).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    use Group::*;
    use HookKind::*;
    use WaitingReason::*;

    fn base() -> Facts<'static> {
        Facts::default()
    }

    #[test]
    fn derive_table() {
        // (case, facts, group, reason)
        let cases: Vec<(&str, Facts, Group, Option<WaitingReason>)> = vec![
            ("empty row is idle", base(), Idle, None),
            // 1. resolved beats everything
            (
                "resolved beats failed, permission, working and PR",
                Facts {
                    resolved: true,
                    pending_permission: true,
                    cc_state: Some("failed"),
                    cc_status: Some("busy"),
                    pr_state: Some("OPEN"),
                    pr_review: Some("APPROVED"),
                    ..base()
                },
                Resolved,
                None,
            ),
            // 2. failed
            (
                "failed beats pending permission",
                Facts {
                    cc_state: Some("failed"),
                    pending_permission: true,
                    ..base()
                },
                Failed,
                None,
            ),
            // 3. pending permission
            (
                "permission beats blocked and busy",
                Facts {
                    pending_permission: true,
                    cc_state: Some("blocked"),
                    cc_status: Some("busy"),
                    ..base()
                },
                Waiting,
                Some(Permission),
            ),
            (
                "permission without snapshot",
                Facts {
                    pending_permission: true,
                    last_hook: Some(PermissionNotification),
                    ..base()
                },
                Waiting,
                Some(Permission),
            ),
            (
                // A subagent's tool calls go on while its lead's prompt waits (§5 hook updates).
                "permission beats a subagent's working hook and background work",
                Facts {
                    pending_permission: true,
                    last_hook: Some(PostToolUse),
                    background_work: true,
                    ..base()
                },
                Waiting,
                Some(Permission),
            ),
            // 4. blocked
            (
                "blocked beats busy status",
                Facts {
                    cc_state: Some("blocked"),
                    cc_status: Some("busy"),
                    ..base()
                },
                Waiting,
                Some(Question),
            ),
            // 5. working
            (
                "working state on a busy process",
                Facts {
                    cc_state: Some("working"),
                    cc_status: Some("busy"),
                    ..base()
                },
                Working,
                None,
            ),
            (
                "working state on an idle process is idle (seen live in M2b)",
                Facts {
                    cc_state: Some("working"),
                    cc_status: Some("idle"),
                    last_hook: Some(Stop),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "working state without a status: no process",
                Facts {
                    cc_state: Some("working"),
                    last_hook: Some(PostToolUse),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "busy status with a done state",
                Facts {
                    cc_state: Some("done"),
                    cc_status: Some("busy"),
                    ..base()
                },
                Working,
                None,
            ),
            (
                "busy status without a state",
                Facts {
                    cc_status: Some("busy"),
                    last_hook: Some(Stop),
                    ..base()
                },
                Working,
                None,
            ),
            (
                "working beats approved PR",
                Facts {
                    cc_state: Some("working"),
                    cc_status: Some("busy"),
                    pr_state: Some("OPEN"),
                    pr_review: Some("APPROVED"),
                    ..base()
                },
                Working,
                None,
            ),
            // 6. landing
            (
                "approved open PR",
                Facts {
                    cc_state: Some("done"),
                    pr_state: Some("OPEN"),
                    pr_review: Some("APPROVED"),
                    ..base()
                },
                Landing,
                None,
            ),
            (
                "approved draft PR still lands",
                Facts {
                    pr_state: Some("OPEN"),
                    pr_draft: true,
                    pr_review: Some("APPROVED"),
                    ..base()
                },
                Landing,
                None,
            ),
            // 7. review
            (
                "open PR",
                Facts {
                    cc_state: Some("done"),
                    pr_state: Some("OPEN"),
                    pr_review: Some("REVIEW_REQUIRED"),
                    ..base()
                },
                Review,
                None,
            ),
            (
                "open draft PR is idle",
                Facts {
                    cc_state: Some("done"),
                    pr_state: Some("OPEN"),
                    pr_draft: true,
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "merged PR is idle (auto-resolve is not derive's job)",
                Facts {
                    pr_state: Some("MERGED"),
                    pr_review: Some("APPROVED"),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "closed PR is idle",
                Facts {
                    pr_state: Some("CLOSED"),
                    ..base()
                },
                Idle,
                None,
            ),
            // 8. idle and unknown states
            (
                "done and idle",
                Facts {
                    cc_state: Some("done"),
                    cc_status: Some("idle"),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "unknown state counts as idle",
                Facts {
                    cc_state: Some("hibernating"),
                    cc_status: Some("idle"),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "waiting status before any permission hook",
                Facts {
                    cc_state: Some("blocked"),
                    cc_status: Some("waiting"),
                    ..base()
                },
                Waiting,
                Some(Question),
            ),
            (
                "waiting status with the permission hook",
                Facts {
                    cc_state: Some("blocked"),
                    cc_status: Some("waiting"),
                    pending_permission: true,
                    ..base()
                },
                Waiting,
                Some(Permission),
            ),
            (
                "stopped and idle states",
                Facts {
                    cc_state: Some("stopped"),
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "no process: a working hook doesn't count",
                Facts {
                    last_hook: Some(PreToolUse),
                    process_gone: true,
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "no process: a stale working state doesn't count",
                Facts {
                    cc_state: Some("working"),
                    process_gone: true,
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "no process: a pending permission prompt is gone too",
                Facts {
                    last_hook: Some(PermissionNotification),
                    pending_permission: true,
                    process_gone: true,
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "no process: a blocked question still waits",
                Facts {
                    cc_state: Some("blocked"),
                    process_gone: true,
                    ..base()
                },
                Waiting,
                Some(Question),
            ),
            (
                "no process: an open PR shows in review",
                Facts {
                    last_hook: Some(PostToolUse),
                    pr_state: Some("OPEN"),
                    process_gone: true,
                    ..base()
                },
                Review,
                None,
            ),
            // 5. background work (§5 "Background work")
            (
                "subagents at work on an idle process",
                Facts {
                    cc_state: Some("working"),
                    cc_status: Some("idle"),
                    last_hook: Some(Stop),
                    background_work: true,
                    ..base()
                },
                Working,
                None,
            ),
            (
                "background work without a snapshot",
                Facts {
                    last_hook: Some(Stop),
                    background_work: true,
                    pr_state: Some("OPEN"),
                    ..base()
                },
                Working,
                None,
            ),
            (
                "a question beats background work",
                Facts {
                    cc_status: Some("waiting"),
                    background_work: true,
                    ..base()
                },
                Waiting,
                Some(Question),
            ),
            (
                "background work dies with the process",
                Facts {
                    last_hook: Some(Stop),
                    background_work: true,
                    process_gone: true,
                    ..base()
                },
                Idle,
                None,
            ),
            (
                "a fresh snapshot beats a working hook",
                Facts {
                    cc_state: Some("done"),
                    last_hook: Some(PreToolUse),
                    ..base()
                },
                Idle,
                None,
            ),
        ];
        for (name, facts, group, reason) in cases {
            let got = derive(&facts);
            assert_eq!(got, Derived { group, reason }, "{name}");
        }
    }

    #[test]
    fn provisional_groups_without_snapshot() {
        let cases = [
            (None, Idle),
            (Some(SessionStart), Idle),
            (Some(UserPromptSubmit), Working),
            (Some(PreToolUse), Working),
            (Some(PostToolUse), Working),
            (Some(IdleNotification), Idle),
            (Some(Stop), Idle),
        ];
        for (hook, group) in cases {
            let facts = Facts {
                last_hook: hook,
                ..base()
            };
            assert_eq!(derive(&facts).group, group, "{hook:?}");
        }
    }

    #[test]
    fn provisional_idle_still_shows_pr() {
        let facts = Facts {
            last_hook: Some(Stop),
            pr_state: Some("OPEN"),
            ..base()
        };
        assert_eq!(derive(&facts).group, Review);
    }

    /// Stuck now only when rule 3 decides the group: a live prompt or waiting status, not a
    /// turn that ended asking, a dead process, a failed or a resolved thread.
    #[test]
    fn blocked_is_rule_3() {
        let cases: Vec<(&str, Facts, bool)> = vec![
            ("idle", base(), false),
            (
                "permission prompt",
                Facts {
                    pending_permission: true,
                    ..base()
                },
                true,
            ),
            (
                "waiting status",
                Facts {
                    cc_status: Some("waiting"),
                    cc_state: Some("blocked"),
                    ..base()
                },
                true,
            ),
            (
                "turn ended with a question",
                Facts {
                    cc_status: Some("idle"),
                    cc_state: Some("blocked"),
                    ..base()
                },
                false,
            ),
            (
                "process gone",
                Facts {
                    pending_permission: true,
                    process_gone: true,
                    ..base()
                },
                false,
            ),
            (
                "failed",
                Facts {
                    pending_permission: true,
                    cc_state: Some("failed"),
                    ..base()
                },
                false,
            ),
            (
                "resolved",
                Facts {
                    resolved: true,
                    cc_status: Some("waiting"),
                    ..base()
                },
                false,
            ),
        ];
        for (case, facts, want) in cases {
            assert_eq!(blocked(&facts), want, "{case}");
            if want {
                assert_eq!(derive(&facts).group, Waiting, "{case}");
            }
        }
    }

    #[test]
    fn hook_kind_round_trip() {
        for kind in [
            SessionStart,
            UserPromptSubmit,
            PreToolUse,
            PostToolUse,
            PermissionNotification,
            IdleNotification,
            Stop,
        ] {
            assert_eq!(HookKind::from_stored(kind.as_str()), Some(kind));
        }
        assert_eq!(HookKind::parse("Notification", Some("auth_success")), None);
        assert_eq!(HookKind::parse("SessionEnd", None), None);
        assert_eq!(HookKind::from_stored("SubagentStop"), None);
    }

    #[test]
    fn a_limited_turn_is_limited_not_a_question() {
        let limited = Facts {
            limited: true,
            cc_state: Some("blocked"),
            cc_status: Some("idle"),
            ..base()
        };
        assert_eq!(derive(&limited).group, Group::Limited);
        assert_eq!(derive(&limited).reason, None);
        assert!(!blocked(&limited), "nothing for the user to answer");
        // A permission prompt still comes first, and resolved before everything.
        let prompt = Facts {
            pending_permission: true,
            ..limited.clone()
        };
        assert_eq!(derive(&prompt).group, Group::Waiting);
        let resolved = Facts {
            resolved: true,
            ..limited.clone()
        };
        assert_eq!(derive(&resolved).group, Group::Resolved);
        // A blocked turn without a limit is a question, as before.
        let question = Facts {
            limited: false,
            ..limited
        };
        assert_eq!(derive(&question).reason, Some(WaitingReason::Question));
    }

    #[test]
    fn group_order_matches_board() {
        let labels: Vec<_> = Group::ORDER.iter().map(|g| g.label()).collect();
        assert_eq!(
            labels,
            [
                "Waiting on you",
                "Working",
                "Limited",
                "Ready for review",
                "Landing",
                "Idle",
                "Failed",
                "Resolved"
            ]
        );
        for g in Group::ORDER {
            assert_eq!(Group::parse(g.as_str()), Some(g));
        }
    }
}
