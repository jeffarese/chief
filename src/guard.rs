//! Message guard (ARCHITECTURE §6). `evaluate` is pure; `hook::guard` gathers its inputs.
//!
//! An allow-list by byte-exact equality: `to` is never trimmed or parsed. A `[ref]` after a name
//! picks one session in Claude Code, so it could redirect a message that the name alone would
//! allow; lookalike names (case, zero-width characters) and sessions the local `claude agents`
//! doesn't list (other machines, the cloud, other sessions' subagents) match nothing and are
//! denied. A session's own subagents are the exception (§6 "Subagents").

use crate::claude::AgentRow;
pub use crate::db::Teammate;
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
    /// Set when it runs in another Claude account than the coordinator (§3).
    pub elsewhere: Option<Elsewhere>,
}

/// A thread's own account, which only its own account's `claude agents` lists.
#[derive(Debug, Clone)]
pub struct Elsewhere {
    /// The account's name for messages (its agp label).
    pub label: String,
    /// A fresh `claude agents` of that account, or why chief couldn't take one.
    pub snapshot: Result<Vec<AgentRow>, String>,
}

#[derive(Debug, Clone)]
pub struct GuardInput<'a> {
    pub role: Role,
    pub tool: &'a str,
    /// `SendMessage.to`, verbatim.
    pub to: Option<&'a str>,
    /// `Agent.name`, verbatim.
    pub agent_name: Option<&'a str>,
    /// `chief-<slug>`.
    pub coordinator_name: &'a str,
    /// The coordinator's row; None before `chief up` first started it.
    pub coordinator: Option<&'a Member>,
    /// Every thread registered to the project, in any state.
    pub threads: &'a [Member],
    /// A fresh `claude agents --json --all` snapshot (every state) of the coordinator's account,
    /// the project's (§3). Empty for the Agent tool, which reads `listed`.
    pub snapshot: &'a [AgentRow],
    /// For a thread's call: the label of its own account when that isn't its coordinator's.
    pub caller_elsewhere: Option<&'a str>,
    /// One of the caller's subagents made the call: the payload has an `agent_id`, which Claude
    /// Code sets, not the model (§1).
    pub from_subagent: bool,
    /// The caller's own subagents, recorded since its process started (§5).
    pub teammates: &'a [Teammate],
    /// Who else a name could reach.
    pub listed: &'a Listed,
}

/// The sessions in the guard's fresh listings of the project's account and the caller's own
/// (§6 "Subagents"): a name the caller sends reaches one of them when none of its subagents has
/// it any more.
#[derive(Debug, Clone, Default)]
pub struct Listed {
    pub names: Vec<String>,
    /// Why a listing couldn't be taken, when one couldn't.
    pub missing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

pub const LIST_AGENTS_DENIED: &str =
    "Use the chief list_threads tool; it lists only this project's threads.";
/// Threads have no list_threads tool, so they get a different pointer.
pub const THREAD_LIST_AGENTS_DENIED: &str = "ListAgents is blocked in chief threads: you can message only your coordinator and your own \
     subagents.";
pub const ASLEEP: &str = "Thread is asleep: call wake_thread first.";
pub const RESOLVED: &str = "Thread is resolved; reopen it first.";
/// Prefixed to a denial whose `to` contains `[`.
pub const BARE_NAME: &str = "Use the bare name, without a `[ref]`: chief matches names exactly, and a ref can pick \
     another session.";
/// What a thread can do instead of messaging its coordinator.
const REPORT_INSTEAD: &str = "Just finish your turn: your final message is recorded as your \
                              report when you stop anyway, and the coordinator reads it there.";
/// What a thread in another account than its coordinator's can do instead.
const REPLY_INSTEAD: &str = "To answer a message from it, send your reply to the exact \
                             `from=\"uds:…\"` address in that message's header. Otherwise just \
                             finish your turn: your final message is recorded as your report, \
                             and the coordinator reads it there.";

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

/// What a subagent calls its lead (§1); from the lead itself they name nobody chief knows.
const LEAD_NAMES: [&str; 2] = ["team-lead", "main"];

/// ARCHITECTURE §6. Tools other than `SendMessage`, `ListAgents` and `Agent` are allowed: the
/// hook's matcher never sends them here.
pub fn evaluate(input: &GuardInput) -> Decision {
    match input.tool {
        "ListAgents" => {
            let reason = match input.role {
                Role::Coordinator => LIST_AGENTS_DENIED,
                Role::Thread => THREAD_LIST_AGENTS_DENIED,
            };
            return Decision::Deny(reason.to_string());
        }
        "Agent" => return agent_spawn(input),
        "SendMessage" => {}
        _ => return Decision::Allow,
    }
    // Blank only picks the message; the comparisons below use `to` as given.
    let Some(to) = input.to.filter(|t| !t.trim().is_empty()) else {
        return Decision::Deny("SendMessage has no `to`; chief can't check where it goes.".into());
    };
    let decision = subagent_send(input, to).unwrap_or_else(|| match input.role {
        Role::Coordinator => coordinator_send(input, to),
        Role::Thread => thread_send(input, to),
    });
    match decision {
        Decision::Deny(reason) if to.contains('[') => {
            Decision::Deny(format!("{BARE_NAME} {reason}"))
        }
        d => d,
    }
}

/// §6 "Subagents": a message between the caller and its own subagents, which never leaves its
/// process. None when `to` is neither one of them nor, from one of them, its lead: the role's
/// rules decide (a subagent's message goes out under its session's address).
fn subagent_send(input: &GuardInput, to: &str) -> Option<Decision> {
    if input.from_subagent && LEAD_NAMES.contains(&to) {
        return Some(Decision::Allow);
    }
    let by_id = input.teammates.iter().find(|t| t.agent_id == to);
    let by_name = || {
        input
            .teammates
            .iter()
            .find(|t| t.name.as_deref() == Some(to))
    };
    let agent = by_id.or_else(by_name)?;
    let id = &agent.agent_id;
    let by_its_id = format!("Message the subagent by its agent id {id}.");
    // Claude Code may read these as an address, a ref or the lead, not as the subagent's name.
    if !plain_address(to) || LEAD_NAMES.contains(&to) {
        return Some(Decision::Deny(format!(
            "chief takes a subagent's name only when it is plain (letters, digits, `-`, `_`, \
             `.`) and not team-lead or main: {to} could be read as another address. {by_its_id}"
        )));
    }
    // Claude Code makes agent ids; no session answers to one.
    if by_id.is_some() {
        return Some(Decision::Allow);
    }
    // In-process names win only while the subagent lives in this process (§1).
    if name_taken(input, to) {
        return Some(Decision::Deny(format!(
            "{to} is also the name of another Claude session, so a message to {to} could reach \
             it instead of your subagent. {by_its_id}"
        )));
    }
    if let Some(why) = &input.listed.missing {
        return Some(Decision::Deny(format!(
            "chief couldn't list the Claude sessions to check that the name {to} reaches only \
             your subagent ({why}). {by_its_id}"
        )));
    }
    Some(Decision::Allow)
}

/// §6 "Subagents" at spawn: a subagent named like another session would take, in its process,
/// the messages meant for that session (§1). Without a name, or with a free one, allowed. The
/// coordinator and the project's threads come from the database, so they are checked even when
/// no listing could be taken; a broken listing doesn't stop subagents.
fn agent_spawn(input: &GuardInput) -> Decision {
    let Some(name) = input.agent_name.filter(|n| !n.is_empty()) else {
        return Decision::Allow;
    };
    if name_taken(input, name) {
        return Decision::Deny(format!(
            "A Claude session is already named {name}, and a subagent named {name} would get \
             the messages meant for it. Pick another name for the subagent."
        ));
    }
    Decision::Allow
}

/// Whether a session other than the caller's subagents answers to `name`: the coordinator, a
/// thread of the project (any state), or a session in the listings.
fn name_taken(input: &GuardInput, name: &str) -> bool {
    name == input.coordinator_name
        || input.threads.iter().any(|t| t.name == name)
        || input.listed.names.iter().any(|n| n == name)
}

/// Letters, digits, `-`, `_` and `.` only: no `uds:` or other scheme, no ref, no whitespace.
fn plain_address(to: &str) -> bool {
    !to.is_empty()
        && to
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Allowed: the exact name of one of this project's threads in the coordinator's account, or
/// the exact socket address of any of them, when that thread can be reached (`reach_thread`).
/// A thread in another account is reached only by address: there its name reaches nothing, or
/// another session (§1).
fn coordinator_send(input: &GuardInput, to: &str) -> Decision {
    if let Some(thread) = input.threads.iter().find(|t| t.name == to) {
        return match (reach_thread(input, thread), &thread.elsewhere) {
            (Ok(_), None) => Decision::Allow,
            (Ok(pid), Some(e)) => Decision::Deny(format!(
                "{to} runs in another Claude account ({}), where names don't reach: message it \
                 at its address {} (list_threads shows it).",
                e.label,
                uds_address(pid)
            )),
            (Err(reason), _) => Decision::Deny(reason),
        };
    }
    if to.starts_with("uds:") {
        let reply = input
            .threads
            .iter()
            .any(|t| reach_thread(input, t).is_ok_and(|pid| uds_address(pid) == to));
        if reply {
            return Decision::Allow;
        }
        let unchecked: Vec<String> = input
            .threads
            .iter()
            .filter(|t| !t.resolved && !t.cleaned && !t.removed)
            .filter_map(|t| {
                let e = t.elsewhere.as_ref()?;
                let why = e.snapshot.as_ref().err()?;
                Some(format!("{} ({}: {why})", t.name, e.label))
            })
            .collect();
        let unchecked = if unchecked.is_empty() {
            String::new()
        } else {
            format!(
                " chief couldn't list the accounts of {}.",
                unchecked.join(", ")
            )
        };
        return Decision::Deny(format!(
            "{to} is not the address of a live thread of this project; message threads at the \
             address list_threads gives.{unchecked}"
        ));
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
        let how = match input.caller_elsewhere {
            Some(_) => {
                "by replying to the exact `from=\"uds:…\"` address of its message".to_string()
            }
            None => format!("by its exact name {coordinator}"),
        };
        Decision::Deny(format!(
            "Threads may message only their coordinator, {how}, and their own subagents, by the \
             name or agent id the Agent tool gave."
        ))
    };
    let reached = match input.coordinator {
        Some(row) => reach(row, coordinator, input.snapshot),
        None => Reach::Asleep,
    };
    if to == coordinator {
        // From another account the name reaches nothing, or a session of that account's.
        if let Some(label) = input.caller_elsewhere {
            return Decision::Deny(format!(
                "You run in another Claude account ({label}) than your coordinator, and \
                 {coordinator}'s name doesn't reach across accounts. {REPLY_INSTEAD}"
            ));
        }
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
    // Only its own account lists it (§1).
    let snapshot = match &thread.elsewhere {
        None => input.snapshot,
        Some(e) => e.snapshot.as_deref().map_err(|why| {
            format!(
                "chief couldn't list {name}'s Claude account ({}) to check it ({why}); message \
                 not sent.",
                e.label
            )
        })?,
    };
    match reach(thread, name, snapshot) {
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
            elsewhere: None,
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
        let listed = listed(&snapshot, &[]);
        let input = GuardInput {
            role,
            tool,
            to,
            agent_name: None,
            coordinator_name: "chief-p",
            coordinator: Some(&coordinator),
            threads: &threads,
            snapshot: &snapshot,
            caller_elsewhere: None,
            from_subagent: false,
            teammates: &[],
            listed: &listed,
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

    /// Threads in another account than the coordinator's (§3): only their address reaches them,
    /// checked in their own account's listing with the same identity rules; a thread there
    /// can't reach its coordinator by name either.
    #[test]
    fn across_accounts() {
        let coordinator = member("chief-p", "coord");
        // The coordinator's account lists only the coordinator and a session that took a
        // far thread's name; PB2 lists the far threads.
        let here = vec![
            row("coord", Some("chief-p"), Some(COORD_PID)),
            row("someone", Some("p-far-1a2b"), Some(400)),
        ];
        let pb2 = vec![
            row("far", Some("p-far-1a2b"), Some(701)),
            row("far-renamed", Some("p-other-name"), Some(702)),
            row("far-dup", Some("p-far-dup-3333"), Some(703)),
            row("x-dup", Some("p-far-dup-3333"), None),
            row("far-asleep", Some("p-far-asleep-4444"), None),
        ];
        let elsewhere = |snapshot: Result<Vec<AgentRow>, String>| {
            Some(Elsewhere {
                label: "PB2".into(),
                snapshot,
            })
        };
        let far = |name: &str, sid: &str| Member {
            elsewhere: elsewhere(Ok(pb2.clone())),
            ..member(name, sid)
        };
        let threads = vec![
            far("p-far-1a2b", "far"),
            far("p-far-renamed-2222", "far-renamed"),
            far("p-far-dup-3333", "far-dup"),
            far("p-far-asleep-4444", "far-asleep"),
            Member {
                elsewhere: elsewhere(Err("timed out".into())),
                ..member("p-unlisted-5555", "unlisted")
            },
        ];
        let listed = Listed::default();
        let decide = |role: Role, to: &str, caller: Option<&str>| {
            evaluate(&GuardInput {
                role,
                tool: "SendMessage",
                to: Some(to),
                agent_name: None,
                coordinator_name: "chief-p",
                coordinator: Some(&coordinator),
                threads: &threads,
                snapshot: &here,
                caller_elsewhere: caller,
                from_subagent: false,
                teammates: &[],
                listed: &listed,
            })
        };
        let denied = |d: Decision, part: &str| match d {
            Decision::Deny(r) => assert!(r.contains(part), "{r:?} lacks {part:?}"),
            Decision::Allow => panic!("allowed, want {part:?}"),
        };
        use Role::{Coordinator as C, Thread as T};

        // By address: checked in PB2's listing.
        assert_eq!(
            decide(C, "uds:/tmp/cc-socks/701.sock", None),
            Decision::Allow
        );
        // By name: the name may reach someone else (pid 400 here); the denial gives the address.
        denied(
            decide(C, "p-far-1a2b", None),
            "runs in another Claude account (PB2), where names don't reach: message it at its \
             address uds:/tmp/cc-socks/701.sock",
        );
        // The same identity checks, in its own account.
        denied(
            decide(C, "uds:/tmp/cc-socks/702.sock", None),
            "not the address",
        );
        denied(decide(C, "p-far-renamed-2222", None), "renamed");
        denied(
            decide(C, "uds:/tmp/cc-socks/703.sock", None),
            "not the address",
        );
        denied(decide(C, "p-far-dup-3333", None), "ambiguous");
        denied(decide(C, "p-far-asleep-4444", None), ASLEEP);
        // Never another account's session at that address: 400 is in the coordinator's.
        denied(
            decide(C, "uds:/tmp/cc-socks/400.sock", None),
            "not the address",
        );
        // An account chief couldn't list: denied, saying so.
        denied(
            decide(C, "p-unlisted-5555", None),
            "couldn't list p-unlisted-5555's Claude account (PB2) to check it (timed out)",
        );
        denied(
            decide(C, "uds:/tmp/cc-socks/999.sock", None),
            "couldn't list the accounts of p-unlisted-5555 (PB2: timed out)",
        );

        // A thread in PB2: its coordinator only at its socket address, checked in the
        // coordinator's own account's listing.
        denied(
            decide(T, "chief-p", Some("PB2")),
            "You run in another Claude account (PB2) than your coordinator",
        );
        denied(decide(T, "chief-p", Some("PB2")), "from=\"uds:");
        assert_eq!(
            decide(T, &uds_address(COORD_PID), Some("PB2")),
            Decision::Allow
        );
        denied(
            decide(T, "uds:/tmp/cc-socks/701.sock", Some("PB2")),
            "only their coordinator",
        );
        assert_eq!(decide(T, "chief-p", None), Decision::Allow);
    }

    fn mate(agent_id: &str, name: Option<&str>) -> Teammate {
        Teammate {
            agent_id: agent_id.into(),
            name: name.map(String::from),
        }
    }

    /// The caller's own subagents, as `teammates` recorded them.
    fn mates() -> Vec<Teammate> {
        vec![
            mate("ahelper-1111", Some("helper")),
            // An outside session is named spike-other too (`listed`).
            mate("aspike-other-2222", Some("spike-other")),
            mate("anoname-3333", None),
            // Named like a cleaned thread of the project, which no listing shows.
            mate("acleaned-4444", Some("p-cleaned-9c0d")),
            // Names that read as other kinds of address.
            mate("auds-5555", Some("uds:/tmp/cc-socks/201.sock")),
            mate("aref-6666", Some("helper [abc]")),
            mate("alead-7777", Some("team-lead")),
        ]
    }

    fn listed(snapshot: &[AgentRow], extra: &[&str]) -> Listed {
        Listed {
            names: snapshot
                .iter()
                .filter_map(|a| a.name.clone())
                .chain(extra.iter().map(|n| n.to_string()))
                .collect(),
            missing: None,
        }
    }

    /// role, from one of its subagents, `to`, expected decision.
    type SubCase = (Role, bool, &'static str, Want);

    /// §6 "Subagents": a session and its own subagents message each other by the agent id or the
    /// exact recorded name, unless another session answers to that name too; a subagent's other
    /// messages follow its session's rules; nobody else's subagents, no refs, no lookalikes.
    #[test]
    fn subagents() {
        use Role::{Coordinator as C, Thread as T};
        use Want::{Allow, Deny};
        let threads = threads();
        let snapshot = snapshot(Some(COORD_PID));
        let coordinator = member("chief-p", "coord");
        let mates = mates();
        let decide = |role: Role, from_subagent: bool, to: &str, listed: &Listed| {
            evaluate(&GuardInput {
                role,
                tool: "SendMessage",
                to: Some(to),
                agent_name: None,
                coordinator_name: "chief-p",
                coordinator: Some(&coordinator),
                threads: &threads,
                snapshot: &snapshot,
                caller_elsewhere: None,
                from_subagent,
                teammates: &mates,
                listed,
            })
        };
        let all = listed(&snapshot, &["spike-other"]);
        let cases: Vec<SubCase> = vec![
            // The lead to its own subagents: by agent id, or by the exact name.
            (C, false, "helper", Allow),
            (T, false, "helper", Allow),
            (C, false, "ahelper-1111", Allow),
            (T, false, "anoname-3333", Allow),
            (T, false, "aspike-other-2222", Allow),
            // Another session answers to the name: in-process names win only while the
            // subagent lives, so the name could reach it (§1).
            (
                C,
                false,
                "spike-other",
                Deny("by its agent id aspike-other-2222"),
            ),
            (
                T,
                false,
                "spike-other",
                Deny("also the name of another Claude session"),
            ),
            // So does a thread of the project, listed or not.
            (
                C,
                false,
                "p-cleaned-9c0d",
                Deny("by its agent id acleaned-4444"),
            ),
            // Names that read as an address or a ref: never as a name.
            (
                C,
                false,
                "uds:/tmp/cc-socks/201.sock",
                Deny("agent id auds-5555"),
            ),
            (T, false, "helper [abc]", Deny(BARE_NAME)),
            (T, false, "helper [abc]", Deny("agent id aref-6666")),
            // The lead's own names are for its subagents only.
            (C, false, "team-lead", Deny("agent id alead-7777")),
            (C, false, "main", Deny("not a thread")),
            (T, false, "main", Deny("only their coordinator")),
            // Exact matches only.
            (C, false, "helper [a1b2c3]", Deny(BARE_NAME)),
            (C, false, "ahelper-1111 [a1b2c3]", Deny(BARE_NAME)),
            (C, false, "Helper", Deny("not a thread")),
            (T, false, "helper ", Deny("only their coordinator")),
            (T, false, "\u{200b}helper", Deny("only their coordinator")),
            (
                T,
                false,
                "ahelper-1111\u{feff}",
                Deny("only their coordinator"),
            ),
            // Anything else follows the role's rules, as before.
            (C, false, "p-live-1a2b", Allow),
            (T, false, "chief-p", Allow),
            (C, false, "someone-elses-helper", Deny("not a thread")),
            (T, false, "researcher", Deny("only their coordinator")),
            // A subagent to its lead, by the names Claude Code gives it (§1).
            (C, true, "team-lead", Allow),
            (T, true, "team-lead", Allow),
            (C, true, "main", Allow),
            (T, true, "main", Allow),
            (T, true, "team-lead [ab12]", Deny(BARE_NAME)),
            (T, true, "Team-Lead", Deny("only their coordinator")),
            (T, true, "team-lead\u{200b}", Deny("only their coordinator")),
            // To its siblings, the same as the lead.
            (T, true, "helper", Allow),
            (C, true, "anoname-3333", Allow),
            (
                T,
                true,
                "spike-other",
                Deny("by its agent id aspike-other-2222"),
            ),
            // Its other messages go out under its session's address, with its session's rules.
            (T, true, "chief-p", Allow),
            (T, true, "p-live-1a2b", Deny("only their coordinator")),
            (C, true, "p-live-1a2b", Allow),
            (C, true, "p-asleep-3c4d", Deny(ASLEEP)),
            (C, true, "other-project-thread", Deny("not a thread")),
        ];
        for (role, from_subagent, to, want) in cases {
            let got = decide(role, from_subagent, to, &all);
            let case = format!("{role:?} from_subagent={from_subagent} {to:?}");
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

        // Without a listing chief can't tell who else answers to a name; an agent id needs none.
        let unlisted = Listed {
            names: vec![],
            missing: Some("timed out".into()),
        };
        let d = decide(T, false, "helper", &unlisted);
        assert!(
            matches!(&d, Decision::Deny(r) if r.contains("couldn't list") && r.contains("timed out") && r.contains("ahelper-1111")),
            "{d:?}"
        );
        assert_eq!(decide(T, false, "ahelper-1111", &unlisted), Decision::Allow);
        assert_eq!(decide(T, true, "team-lead", &unlisted), Decision::Allow);

        // Nobody else's subagents: with none recorded for the caller, every name is the role's.
        let none = GuardInput {
            role: T,
            tool: "SendMessage",
            to: Some("helper"),
            agent_name: None,
            coordinator_name: "chief-p",
            coordinator: Some(&coordinator),
            threads: &threads,
            snapshot: &snapshot,
            caller_elsewhere: None,
            from_subagent: false,
            teammates: &[],
            listed: &all,
        };
        assert!(matches!(evaluate(&none), Decision::Deny(r)
            if r.contains("only their coordinator, by its exact name chief-p")
                && r.contains("their own subagents")));
        // From another account the coordinator's name doesn't reach: point at the reply address.
        let elsewhere = evaluate(&GuardInput {
            caller_elsewhere: Some("PB3"),
            ..none.clone()
        });
        assert!(
            matches!(&elsewhere, Decision::Deny(r)
                if r.contains("from=\"uds:") && !r.contains("chief-p") && r.contains("own subagents")),
            "{elsewhere:?}"
        );
        for to in ["ahelper-1111", "team-lead", "main"] {
            let d = evaluate(&GuardInput {
                to: Some(to),
                ..none.clone()
            });
            assert!(matches!(d, Decision::Deny(_)), "{to}: {d:?}");
        }
    }

    /// §6 "Subagents" at spawn: a subagent may not take a name another session answers to, or
    /// messages meant for that session would reach it instead (§1). The database's names are
    /// checked even when no listing could be taken; the listing's only when it was.
    #[test]
    fn agent_names_at_spawn() {
        use Role::{Coordinator as C, Thread as T};
        let threads = threads();
        let snapshot = snapshot(Some(COORD_PID));
        let coordinator = member("chief-p", "coord");
        let all = listed(&snapshot, &["spike-other"]);
        let unlisted = Listed {
            names: vec![],
            missing: Some("supervisor down".into()),
        };
        let decide = |role: Role, from_subagent: bool, name: Option<&str>, listed: &Listed| {
            evaluate(&GuardInput {
                role,
                tool: "Agent",
                to: None,
                agent_name: name,
                coordinator_name: "chief-p",
                coordinator: Some(&coordinator),
                threads: &threads,
                snapshot: &snapshot,
                caller_elsewhere: None,
                from_subagent,
                teammates: &[],
                listed,
            })
        };
        let taken = |d: Decision| matches!(&d, Decision::Deny(r) if r.contains("Pick another name for the subagent"));
        for listed in [&all, &unlisted] {
            // No name, or a free one.
            assert_eq!(decide(T, false, None, listed), Decision::Allow);
            assert_eq!(decide(C, false, Some(""), listed), Decision::Allow);
            assert_eq!(decide(T, false, Some("helper"), listed), Decision::Allow);
            assert_eq!(decide(C, false, Some("Chief-P"), listed), Decision::Allow);
            // The coordinator and every thread of the project, from the database.
            assert!(taken(decide(T, false, Some("chief-p"), listed)));
            assert!(taken(decide(T, true, Some("chief-p"), listed)));
            assert!(taken(decide(C, false, Some("p-live-1a2b"), listed)));
            assert!(taken(decide(C, false, Some("p-cleaned-9c0d"), listed)));
        }
        // Every listed session.
        assert!(taken(decide(T, false, Some("other-project-thread"), &all)));
        assert!(taken(decide(C, false, Some("spike-other"), &all)));
        // Without a listing those are allowed: a broken listing doesn't stop subagents.
        assert_eq!(
            decide(T, false, Some("other-project-thread"), &unlisted),
            Decision::Allow
        );
        let d = decide(C, false, Some("p-renamed-now"), &all);
        assert!(
            matches!(&d, Decision::Deny(r) if r.contains("p-renamed-now")),
            "{d:?}"
        );
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
