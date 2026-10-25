//! Message guard (ARCHITECTURE §6). `evaluate` is pure; `hook::guard` gathers its inputs.

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
    /// Its pid in the fresh `claude agents` snapshot; None when it isn't running (asleep).
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

pub const LIST_AGENTS_DENIED: &str =
    "Use the chief list_threads tool; it lists only this project's threads.";
pub const ASLEEP: &str = "Thread is asleep: call wake_thread first.";
pub const RESOLVED: &str = "Thread is resolved; reopen it first.";

/// ARCHITECTURE §6. Tools other than `SendMessage` and `ListAgents` are allowed: the hook's
/// matcher never sends them here.
pub fn evaluate(input: &GuardInput) -> Decision {
    match input.tool {
        "ListAgents" => return Decision::Deny(LIST_AGENTS_DENIED.to_string()),
        "SendMessage" => {}
        _ => return Decision::Allow,
    }
    let Some(raw) = input.to.map(str::trim).filter(|t| !t.is_empty()) else {
        return Decision::Deny("SendMessage has no `to`; chief can't check where it goes.".into());
    };
    let to = strip_ref(raw);
    match input.role {
        Role::Coordinator => coordinator_send(input, to),
        Role::Thread => thread_send(input, to),
    }
}

fn coordinator_send(input: &GuardInput, to: &str) -> Decision {
    if let Some(addr) = to.strip_prefix("uds:") {
        let live = uds_pid(addr).and_then(|pid| {
            input
                .threads
                .iter()
                .find(|t| t.pid == Some(pid) && !t.resolved)
        });
        return match live {
            Some(_) => Decision::Allow,
            None => Decision::Deny(format!(
                "{to} is not a live thread of this project; message threads by name."
            )),
        };
    }
    let Some(thread) = input.threads.iter().find(|t| t.name == to) else {
        return Decision::Deny(format!(
            "{to} is not a thread of this project. Call list_threads for the names; start a \
             thread with start_thread."
        ));
    };
    if thread.resolved {
        return Decision::Deny(RESOLVED.to_string());
    }
    if thread.pid.is_none() {
        return Decision::Deny(ASLEEP.to_string());
    }
    // Names are unique by construction, but a message must never reach two sessions.
    if live_named(input, to) > 1 {
        return Decision::Deny(format!(
            "More than one live session is named {to}; chief won't send to an ambiguous name."
        ));
    }
    Decision::Allow
}

fn thread_send(input: &GuardInput, to: &str) -> Decision {
    if let Some(addr) = to.strip_prefix("uds:") {
        return match (uds_pid(addr), input.coordinator_pid) {
            (Some(pid), Some(coord)) if pid == coord => Decision::Allow,
            _ => Decision::Deny(format!(
                "Threads may message only their coordinator ({}).",
                input.coordinator_name
            )),
        };
    }
    if to != input.coordinator_name && live_named(input, to) > 0 {
        return Decision::Deny(format!(
            "{to} is another Claude session; threads may message only their coordinator ({}).",
            input.coordinator_name
        ));
    }
    Decision::Allow
}

fn live_named(input: &GuardInput, name: &str) -> usize {
    input
        .live
        .iter()
        .filter(|s| s.pid.is_some() && s.name.as_deref() == Some(name))
        .count()
}

/// `name [ref]` → `name`: ListAgents shows a ref after the name, and models copy it.
pub fn strip_ref(to: &str) -> &str {
    let to = to.trim();
    match to.rfind(" [") {
        Some(i) if to.ends_with(']') => to[..i].trim_end(),
        _ => to,
    }
}

/// `/tmp/cc-socks/<pid>.sock` → pid.
pub fn uds_pid(addr: &str) -> Option<u32> {
    let file = addr.rsplit('/').next()?;
    file.strip_suffix(".sock")?.parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    const COORD_PID: u32 = 100;

    fn threads() -> Vec<ProjectThread> {
        vec![
            ProjectThread {
                name: "p-live-1a2b".into(),
                resolved: false,
                pid: Some(201),
            },
            ProjectThread {
                name: "p-asleep-3c4d".into(),
                resolved: false,
                pid: None,
            },
            ProjectThread {
                name: "p-done-5e6f".into(),
                resolved: true,
                pid: Some(203),
            },
            ProjectThread {
                name: "p-done-asleep-7a8b".into(),
                resolved: true,
                pid: None,
            },
        ]
    }

    fn live() -> Vec<LiveSession> {
        let s = |name: &str, pid: Option<u32>| LiveSession {
            name: Some(name.into()),
            pid,
        };
        vec![
            s("chief-p", Some(COORD_PID)),
            s("p-live-1a2b", Some(201)),
            s("p-done-5e6f", Some(203)),
            s("other-project-thread", Some(300)),
            s("someone-asleep", None),
            s("dup", Some(401)),
            LiveSession {
                name: None,
                pid: Some(500),
            },
        ]
    }

    enum Want {
        Allow,
        Deny(&'static str),
    }

    /// role, tool, `to`, coordinator pid, expected decision.
    type Case = (Role, &'static str, Option<&'static str>, Option<u32>, Want);

    fn check(role: Role, tool: &str, to: Option<&str>, coord_pid: Option<u32>, want: Want) {
        let threads = threads();
        let mut live = live();
        // A second live session with a project thread's name, for the ambiguity case.
        if to == Some("p-dup-9999") {
            live.push(LiveSession {
                name: Some("p-dup-9999".into()),
                pid: Some(601),
            });
            live.push(LiveSession {
                name: Some("p-dup-9999".into()),
                pid: Some(602),
            });
        }
        let mut all = threads.clone();
        all.push(ProjectThread {
            name: "p-dup-9999".into(),
            resolved: false,
            pid: Some(601),
        });
        let input = GuardInput {
            role,
            tool,
            to,
            coordinator_name: "chief-p",
            coordinator_pid: coord_pid,
            threads: &all,
            live: &live,
        };
        let got = evaluate(&input);
        let case = format!("{role:?} {tool} {to:?}");
        match want {
            Want::Allow => assert_eq!(got, Decision::Allow, "{case}"),
            Want::Deny(part) => match got {
                Decision::Deny(reason) => {
                    assert!(reason.contains(part), "{case}: {reason:?} lacks {part:?}")
                }
                Decision::Allow => panic!("{case}: allowed, want deny"),
            },
        }
    }

    #[test]
    fn rules() {
        use Role::{Coordinator as C, Thread as T};
        use Want::{Allow, Deny};
        let cp = Some(COORD_PID);
        let cases: Vec<Case> = vec![
            // ListAgents: denied for everyone.
            (C, "ListAgents", None, cp, Deny("list_threads")),
            (T, "ListAgents", None, cp, Deny("list_threads")),
            // Other tools never reach the guard; allow them.
            (C, "Read", None, cp, Allow),
            // Missing `to`.
            (C, "SendMessage", None, cp, Deny("no `to`")),
            (T, "SendMessage", Some("  "), cp, Deny("no `to`")),
            // Coordinator to its threads.
            (C, "SendMessage", Some("p-live-1a2b"), cp, Allow),
            (C, "SendMessage", Some("p-live-1a2b [a1b2c3]"), cp, Allow),
            (C, "SendMessage", Some(" p-live-1a2b  [ref] "), cp, Allow),
            (C, "SendMessage", Some("p-asleep-3c4d"), cp, Deny(ASLEEP)),
            (C, "SendMessage", Some("p-done-5e6f"), cp, Deny(RESOLVED)),
            (
                C,
                "SendMessage",
                Some("p-done-asleep-7a8b"),
                cp,
                Deny(RESOLVED),
            ),
            (C, "SendMessage", Some("p-dup-9999"), cp, Deny("ambiguous")),
            // Coordinator outside the project, or to itself.
            (
                C,
                "SendMessage",
                Some("other-project-thread"),
                cp,
                Deny("not a thread of this project"),
            ),
            (
                C,
                "SendMessage",
                Some("other-project-thread [x1]"),
                cp,
                Deny("not a thread"),
            ),
            (C, "SendMessage", Some("chief-p"), cp, Deny("not a thread")),
            (
                C,
                "SendMessage",
                Some("my-subagent"),
                cp,
                Deny("not a thread"),
            ),
            // Coordinator replying to a uds address.
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/201.sock"),
                cp,
                Allow,
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/201.sock [r]"),
                cp,
                Allow,
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/300.sock"),
                cp,
                Deny("not a live thread"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/203.sock"),
                cp,
                Deny("not a live thread"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/nope.sock"),
                cp,
                Deny("not a live thread"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:garbage"),
                cp,
                Deny("not a live thread"),
            ),
            // Thread to its coordinator.
            (T, "SendMessage", Some("chief-p"), cp, Allow),
            (T, "SendMessage", Some("chief-p [ref]"), cp, Allow),
            (T, "SendMessage", Some("chief-p"), None, Allow),
            (
                T,
                "SendMessage",
                Some("uds:/tmp/cc-socks/100.sock"),
                cp,
                Allow,
            ),
            (
                T,
                "SendMessage",
                Some("uds:/tmp/cc-socks/100.sock"),
                None,
                Deny("only their coordinator"),
            ),
            (
                T,
                "SendMessage",
                Some("uds:/tmp/cc-socks/201.sock"),
                cp,
                Deny("only their coordinator"),
            ),
            (
                T,
                "SendMessage",
                Some("uds:/tmp/cc-socks/300.sock"),
                cp,
                Deny("only their coordinator"),
            ),
            // Thread to other live sessions: siblings and other projects.
            (
                T,
                "SendMessage",
                Some("p-live-1a2b"),
                cp,
                Deny("another Claude session"),
            ),
            (
                T,
                "SendMessage",
                Some("other-project-thread"),
                cp,
                Deny("another Claude session"),
            ),
            (
                T,
                "SendMessage",
                Some("other-project-thread [abc]"),
                cp,
                Deny("another Claude session"),
            ),
            // Thread to its own subagents (not in `claude agents`), or a listed session with
            // no process.
            (T, "SendMessage", Some("researcher"), cp, Allow),
            (T, "SendMessage", Some("someone-asleep"), cp, Allow),
        ];
        for (role, tool, to, coord_pid, want) in cases {
            check(role, tool, to, coord_pid, want);
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(strip_ref("a [b]"), "a");
        assert_eq!(strip_ref("a"), "a");
        assert_eq!(strip_ref("a [b"), "a [b");
        assert_eq!(strip_ref("[b]"), "[b]");
        assert_eq!(uds_pid("/tmp/cc-socks/123.sock"), Some(123));
        assert_eq!(uds_pid("123.sock"), Some(123));
        assert_eq!(uds_pid("/tmp/cc-socks/123"), None);
        assert_eq!(uds_pid(""), None);
    }

    #[test]
    fn deny_json_shape() {
        let v: serde_json::Value = serde_json::from_str(&deny_json("no")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "no");
    }
}
