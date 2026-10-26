//! Message guard (ARCHITECTURE §6). `evaluate` is pure; `hook::guard` gathers its inputs.
//!
//! An allow-list by byte-exact equality: `to` is never trimmed or parsed. A `[ref]` after a name
//! picks one session in Claude Code, so it could redirect a message that the name alone would
//! allow; lookalike names (case, zero-width characters) and sessions the local `claude agents`
//! doesn't list (other machines, the cloud, teammates, subagents) match nothing and are denied.

use crate::claude::AgentRow;
use crate::state::Role;

/// A registry row of this project, as the guard needs it.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub session_id: Option<String>,
    /// Used to find the row in the snapshot while `session_id` is unresolved.
    pub short_id: Option<String>,
    pub resolved: bool,
    /// Its session, worktree and branch were removed (§12).
    pub cleaned: bool,
    /// A sync found its session removed outside chief (§5); its worktree may remain.
    pub removed: bool,
}

#[derive(Debug, Clone)]
pub struct GuardInput<'a> {
    pub role: Role,
    pub tool: &'a str,
    /// `SendMessage.to`, verbatim.
    pub to: Option<&'a str>,
    /// `chief-<slug>`.
    pub coordinator_name: &'a str,
    /// The coordinator's row; None before `chief up` first started it.
    pub coordinator: Option<&'a Member>,
    pub threads: &'a [Member],
    /// A fresh `claude agents --json --all` snapshot (every state).
    pub snapshot: &'a [AgentRow],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

pub const LIST_AGENTS_DENIED: &str =
    "Use the chief list_threads tool; it lists only this project's threads.";
/// Threads have no list_threads tool, so they get a different pointer.
pub const THREAD_LIST_AGENTS_DENIED: &str =
    "ListAgents is blocked in chief threads: you can message only your coordinator.";
pub const ASLEEP: &str = "Thread is asleep: call wake_thread first.";
pub const RESOLVED: &str = "Thread is resolved; reopen it first.";
/// Prefixed to a denial whose `to` contains `[`.
pub const BARE_NAME: &str = "Use the bare name, without a `[ref]`: chief matches names exactly, and a ref can pick \
     another session.";
/// What a thread can do instead of messaging its coordinator.
const REPORT_INSTEAD: &str = "Just finish your turn: your final message is recorded as your \
                              report when you stop anyway, and the coordinator reads it there.";

/// Shared by the guard, `wake_thread` and reopening: a cleaned thread has no session left.
pub fn cleaned_message(name: &str) -> String {
    format!(
        "{name} was cleaned up; start a new thread (its reports are still readable with \
         read_report)"
    )
}

/// Shared by the guard, `wake_thread` and reopening: a sync found the session removed outside
/// chief (§5), so `claude respawn` has nothing to bring back, even while its worktree remains.
pub fn removed_message(name: &str) -> String {
    format!(
        "{name}'s session was removed outside chief, so it can't be woken, reopened or \
         messaged; start a new thread (its reports are still readable with read_report)"
    )
}

/// ARCHITECTURE §6. Tools other than `SendMessage` and `ListAgents` are allowed: the hook's
/// matcher never sends them here.
pub fn evaluate(input: &GuardInput) -> Decision {
    match input.tool {
        "ListAgents" => {
            let reason = match input.role {
                Role::Coordinator => LIST_AGENTS_DENIED,
                Role::Thread => THREAD_LIST_AGENTS_DENIED,
            };
            return Decision::Deny(reason.to_string());
        }
        "SendMessage" => {}
        _ => return Decision::Allow,
    }
    // Blank only picks the message; the comparisons below use `to` as given.
    let Some(to) = input.to.filter(|t| !t.trim().is_empty()) else {
        return Decision::Deny("SendMessage has no `to`; chief can't check where it goes.".into());
    };
    let decision = match input.role {
        Role::Coordinator => coordinator_send(input, to),
        Role::Thread => thread_send(input, to),
    };
    match decision {
        Decision::Deny(reason) if to.contains('[') => {
            Decision::Deny(format!("{BARE_NAME} {reason}"))
        }
        d => d,
    }
}

/// Allowed: the exact name of one of this project's threads, or the exact socket address of
/// one, when that thread can be reached (`reach_thread`).
fn coordinator_send(input: &GuardInput, to: &str) -> Decision {
    if let Some(thread) = input.threads.iter().find(|t| t.name == to) {
        return match reach_thread(input, thread) {
            Ok(_) => Decision::Allow,
            Err(reason) => Decision::Deny(reason),
        };
    }
    if to.starts_with("uds:") {
        let reply = input
            .threads
            .iter()
            .any(|t| reach_thread(input, t).is_ok_and(|pid| uds_address(pid) == to));
        return if reply {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "{to} is not the address of a live thread of this project; message threads by \
                 name."
            ))
        };
    }
    Decision::Deny(format!(
        "{to} is not a thread of this project. Call list_threads for the names; start a thread \
         with start_thread."
    ))
}

/// Allowed: exactly `chief-<slug>`, or exactly the coordinator's socket address, while the
/// coordinator's own session is live under that name.
fn thread_send(input: &GuardInput, to: &str) -> Decision {
    let coordinator = input.coordinator_name;
    let only = || {
        Decision::Deny(format!(
            "Threads may message only their coordinator, by its exact name {coordinator}."
        ))
    };
    let reached = match input.coordinator {
        Some(row) => reach(row, coordinator, input.snapshot),
        None => Reach::Asleep,
    };
    if to == coordinator {
        return match reached {
            Reach::Live(_) => Decision::Allow,
            Reach::Asleep => Decision::Deny(format!(
                "Your coordinator {coordinator} isn't running, so the message can't be \
                 delivered. {REPORT_INSTEAD}"
            )),
            Reach::Renamed(now) => Decision::Deny(format!(
                "Your coordinator's session was renamed ({}), so {coordinator} may name another \
                 session; message not sent. {REPORT_INSTEAD}",
                shown(now.as_deref())
            )),
            Reach::Ambiguous => Decision::Deny(format!(
                "More than one Claude session is named {coordinator}; chief won't send to an \
                 ambiguous name. {REPORT_INSTEAD}"
            )),
        };
    }
    match reached {
        Reach::Live(pid) if to == uds_address(pid) => Decision::Allow,
        _ => only(),
    }
}

/// Whether a message to one of this project's threads would reach its own session; the
/// thread's pid if so, else the denial.
fn reach_thread(input: &GuardInput, thread: &Member) -> Result<u32, String> {
    if thread.removed {
        return Err(removed_message(&thread.name));
    }
    if thread.cleaned {
        return Err(cleaned_message(&thread.name));
    }
    if thread.resolved {
        return Err(RESOLVED.to_string());
    }
    let name = &thread.name;
    match reach(thread, name, input.snapshot) {
        Reach::Live(pid) => Ok(pid),
        Reach::Asleep => Err(ASLEEP.to_string()),
        Reach::Renamed(now) => Err(format!(
            "{name}'s session was renamed ({}), so the name {name} may reach another session; \
             chief won't send it.",
            shown(now.as_deref())
        )),
        Reach::Ambiguous => Err(format!(
            "More than one Claude session is named {name}; chief won't send to an ambiguous \
             name."
        )),
    }
}

/// What a message addressed to `name` would reach, for the registry row `member`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    /// The row's own session is live, carries `name`, and is the only session that does.
    Live(u32),
    /// The row's session isn't listed, or has no process.
    Asleep,
    /// The row's session is live under another name (or none).
    Renamed(Option<String>),
    /// Some other session, in any state, carries `name` too.
    Ambiguous,
}

/// §6 identity check. The row's session is the snapshot entry with its `session_id` (its
/// `short_id` while unresolved), never one found by name.
fn reach(member: &Member, name: &str, snapshot: &[AgentRow]) -> Reach {
    let own = snapshot
        .iter()
        .find(|a| match (&member.session_id, &member.short_id) {
            (Some(sid), _) => &a.session_id == sid,
            (None, Some(short)) => a.id.as_ref() == Some(short),
            (None, None) => false,
        });
    let Some((own, pid)) = own.and_then(|a| a.pid.map(|pid| (a, pid))) else {
        return Reach::Asleep;
    };
    if own.name.as_deref() != Some(name) {
        return Reach::Renamed(own.name.clone());
    }
    let named = snapshot
        .iter()
        .filter(|a| a.name.as_deref() == Some(name))
        .count();
    if named > 1 {
        return Reach::Ambiguous;
    }
    Reach::Live(pid)
}

fn shown(name: Option<&str>) -> String {
    match name {
        Some(n) => format!("now {n}"),
        None => "it has no name now".to_string(),
    }
}

/// The address a live session's messages come from (§1): `uds:/tmp/cc-socks/<pid>.sock`, the
/// pid in decimal. The guard accepts a `uds:` target only in exactly this form.
pub fn uds_address(pid: u32) -> String {
    format!("uds:/tmp/cc-socks/{pid}.sock")
}

/// `name [ref]` → `name`, for the MCP tools' `name` arguments (§7), which look rows up in the
/// registry: there a ref can't redirect anything. The guard never strips refs.
pub fn strip_ref(to: &str) -> &str {
    let to = to.trim();
    match to.rfind(" [") {
        Some(i) if to.ends_with(']') => to[..i].trim_end(),
        _ => to,
    }
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

    fn member(name: &str, sid: &str) -> Member {
        Member {
            name: name.into(),
            session_id: Some(sid.into()),
            short_id: None,
            resolved: false,
            cleaned: false,
            removed: false,
        }
    }

    fn threads() -> Vec<Member> {
        vec![
            member("p-live-1a2b", "live"),
            member("p-asleep-3c4d", "asleep"),
            Member {
                resolved: true,
                ..member("p-done-5e6f", "done")
            },
            Member {
                resolved: true,
                ..member("p-done-asleep-7a8b", "done-asleep")
            },
            Member {
                resolved: true,
                cleaned: true,
                ..member("p-cleaned-9c0d", "cleaned")
            },
            // Removed outside chief, its worktree still there: resolved, not cleaned.
            Member {
                resolved: true,
                removed: true,
                ..member("p-removed-3a3a", "removed")
            },
            // Not resolved yet: found in the snapshot by short id.
            Member {
                session_id: None,
                short_id: Some("u1".into()),
                ..member("p-unresolved-1111", "")
            },
            member("p-dup-9999", "dup"),
            member("p-renamed-2222", "renamed"),
        ]
    }

    fn row(sid: &str, name: Option<&str>, pid: Option<u32>) -> AgentRow {
        AgentRow {
            id: None,
            session_id: sid.into(),
            name: name.map(String::from),
            cwd: None,
            kind: None,
            started_at: None,
            state: None,
            status: None,
            pid,
        }
    }

    fn snapshot(coord_pid: Option<u32>) -> Vec<AgentRow> {
        vec![
            row("coord", Some("chief-p"), coord_pid),
            row("live", Some("p-live-1a2b"), Some(201)),
            row("asleep", Some("p-asleep-3c4d"), None),
            row("done", Some("p-done-5e6f"), Some(203)),
            AgentRow {
                id: Some("u1".into()),
                ..row("u1-sid", Some("p-unresolved-1111"), Some(204))
            },
            row("dup", Some("p-dup-9999"), Some(205)),
            // Another session, asleep, with a project thread's name.
            row("other-dup", Some("p-dup-9999"), None),
            row("renamed", Some("p-renamed-now"), Some(206)),
            row("x", Some("other-project-thread"), Some(300)),
            row("y", Some("p-renamed-2222"), Some(301)),
            row("z", None, Some(500)),
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
        let snapshot = snapshot(coord_pid);
        let coordinator = member("chief-p", "coord");
        let input = GuardInput {
            role,
            tool,
            to,
            coordinator_name: "chief-p",
            coordinator: Some(&coordinator),
            threads: &threads,
            snapshot: &snapshot,
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
            (T, "ListAgents", None, cp, Deny("only your coordinator")),
            // Other tools never reach the guard; allow them.
            (C, "Read", None, cp, Allow),
            // Missing or blank `to`.
            (C, "SendMessage", None, cp, Deny("no `to`")),
            (T, "SendMessage", Some("  "), cp, Deny("no `to`")),
            (C, "SendMessage", Some(""), cp, Deny("no `to`")),
            // Coordinator to its threads, by exact name.
            (C, "SendMessage", Some("p-live-1a2b"), cp, Allow),
            (C, "SendMessage", Some("p-unresolved-1111"), cp, Allow),
            (
                C,
                "SendMessage",
                Some("p-live-1a2b [a1b2c3]"),
                cp,
                Deny(BARE_NAME),
            ),
            (
                C,
                "SendMessage",
                Some(" p-live-1a2b"),
                cp,
                Deny("not a thread"),
            ),
            (
                C,
                "SendMessage",
                Some("p-live-1a2b\u{200b}"),
                cp,
                Deny("not a thread"),
            ),
            (
                C,
                "SendMessage",
                Some("P-Live-1a2b"),
                cp,
                Deny("not a thread"),
            ),
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
            (
                C,
                "SendMessage",
                Some("p-renamed-2222"),
                cp,
                Deny("renamed (now p-renamed-now)"),
            ),
            (
                C,
                "SendMessage",
                Some("p-cleaned-9c0d"),
                cp,
                Deny("p-cleaned-9c0d was cleaned up; start a new thread"),
            ),
            (
                C,
                "SendMessage",
                Some("p-removed-3a3a"),
                cp,
                Deny("p-removed-3a3a's session was removed outside chief"),
            ),
            // Coordinator outside the project, or to itself.
            (
                C,
                "SendMessage",
                Some("other-project-thread"),
                cp,
                Deny("not a thread of this project"),
            ),
            (C, "SendMessage", Some("chief-p"), cp, Deny("not a thread")),
            (
                C,
                "SendMessage",
                Some("my-subagent"),
                cp,
                Deny("not a thread"),
            ),
            // Coordinator replying to a thread's socket address, exactly.
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
                Some("uds:/tmp/cc-socks/204.sock"),
                cp,
                Allow,
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/201.sock [r]"),
                cp,
                Deny(BARE_NAME),
            ),
            (
                C,
                "SendMessage",
                Some("uds:201.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/x/201.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/0201.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/300.sock"),
                cp,
                Deny("not the address"),
            ),
            // Resolved, renamed and ambiguous threads aren't reachable by address either.
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/203.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/205.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:/tmp/cc-socks/206.sock"),
                cp,
                Deny("not the address"),
            ),
            (
                C,
                "SendMessage",
                Some("uds:garbage"),
                cp,
                Deny("not the address"),
            ),
            // Thread to its coordinator, exactly.
            (T, "SendMessage", Some("chief-p"), cp, Allow),
            (
                T,
                "SendMessage",
                Some("uds:/tmp/cc-socks/100.sock"),
                cp,
                Allow,
            ),
            (T, "SendMessage", Some("chief-p [ref]"), cp, Deny(BARE_NAME)),
            (
                T,
                "SendMessage",
                Some("chief-p"),
                None,
                Deny("isn't running"),
            ),
            (
                T,
                "SendMessage",
                Some("chief-p"),
                None,
                Deny("recorded as your report"),
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
                Some("uds:100.sock"),
                cp,
                Deny("only their coordinator"),
            ),
            // Thread to anything else: siblings, other projects, its own subagents, sessions
            // the local `claude agents` doesn't list.
            (
                T,
                "SendMessage",
                Some("p-live-1a2b"),
                cp,
                Deny("only their coordinator"),
            ),
            (
                T,
                "SendMessage",
                Some("other-project-thread"),
                cp,
                Deny("only their coordinator"),
            ),
            (
                T,
                "SendMessage",
                Some("other-project-thread [abc]"),
                cp,
                Deny(BARE_NAME),
            ),
            (
                T,
                "SendMessage",
                Some("researcher"),
                cp,
                Deny("only their coordinator"),
            ),
            (
                T,
                "SendMessage",
                Some("Chief-P"),
                cp,
                Deny("only their coordinator"),
            ),
        ];
        for (role, tool, to, coord_pid, want) in cases {
            check(role, tool, to, coord_pid, want);
        }
    }

    #[test]
    fn identity() {
        let snap = snapshot(Some(COORD_PID));
        let m = member("p-live-1a2b", "live");
        assert_eq!(reach(&m, "p-live-1a2b", &snap), Reach::Live(201));
        assert_eq!(
            reach(&m, "other", &snap),
            Reach::Renamed(Some("p-live-1a2b".into()))
        );
        let nobody = Member {
            session_id: None,
            short_id: None,
            ..m.clone()
        };
        assert_eq!(
            reach(&nobody, "p-live-1a2b", &snap),
            Reach::Asleep,
            "never by name"
        );
        let unnamed = member("x", "z");
        assert_eq!(reach(&unnamed, "x", &snap), Reach::Renamed(None));
    }

    #[test]
    fn helpers() {
        assert_eq!(strip_ref("a [b]"), "a");
        assert_eq!(strip_ref("a"), "a");
        assert_eq!(strip_ref("a [b"), "a [b");
        assert_eq!(strip_ref("[b]"), "[b]");
        assert_eq!(uds_address(123), "uds:/tmp/cc-socks/123.sock");
    }

    #[test]
    fn deny_json_shape() {
        let v: serde_json::Value = serde_json::from_str(&deny_json("no")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "no");
    }
}
