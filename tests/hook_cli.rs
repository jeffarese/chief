//! `chief hook` as Claude Code runs it: payload on stdin, exit 0, errors logged; stdout only for
//! the guard's decision and the coordinator's context.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, OutputWithStdin, agent, stderr};
use serde_json::Value;

fn hook(home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chief"))
        .arg("hook")
        .args(args)
        .env("CHIEF_HOME", home)
        .env("CHIEF_NO_NOTIFY", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn error_log(home: &Path) -> String {
    std::fs::read_to_string(home.join("logs/hook-errors.log")).unwrap_or_default()
}

fn assert_silent_success(out: &Output) {
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn stop_hook_updates_the_database() {
    let home = tempfile::tempdir().unwrap();
    {
        let conn = db::open(&home.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        db::insert_session(
            &conn,
            &SessionRow::new("demo-a-1a2b", "demo", Role::Thread, 0),
        )
        .unwrap();
    }
    // A project.toml with notifications on: CHIEF_NO_NOTIFY must still keep osascript out.
    let config = chief::config::ProjectConfig::new("Demo", "demo", None, vec![]);
    config
        .save(&home.path().join("projects/demo/project.toml"))
        .unwrap();

    let payload = serde_json::json!({
        "session_id": "aaaaaaaa-0000-0000-0000-000000000000",
        "session_title": "demo-a-1a2b",
        "transcript_path": "/x.jsonl",
        "hook_event_name": "Stop",
        "cwd": home.path().join("nowhere"),
        "last_assistant_message": "All done.",
    });
    let out = hook(
        home.path(),
        &["Stop", "--project", "demo", "--role", "thread"],
        &payload.to_string(),
    );
    assert_silent_success(&out);
    assert_eq!(error_log(home.path()), "");

    let conn = db::open(&home.path().join("chief.db")).unwrap();
    let row = db::get_session(&conn, "demo-a-1a2b").unwrap().unwrap();
    assert_eq!(row.last_report.as_deref(), Some("All done."));
    assert_eq!(
        row.session_id.as_deref(),
        Some("aaaaaaaa-0000-0000-0000-000000000000")
    );
    assert_eq!(row.group_name, "idle");
    assert_eq!(db::recent_events(&conn, "demo", 10).unwrap().len(), 1);
}

#[test]
fn bad_payload_exits_zero_and_logs() {
    let home = tempfile::tempdir().unwrap();
    let out = hook(
        home.path(),
        &["PreToolUse", "--project", "demo", "--role", "thread"],
        "{not json",
    );
    assert_silent_success(&out);
    let log = error_log(home.path());
    assert!(log.contains("hook PreToolUse --project demo"), "{log}");
    assert!(log.contains("parsing the hook payload"), "{log}");
}

/// The fixture's raw payloads (§1: a background session's in-process teammates).
fn teammate_fixture() -> Vec<Value> {
    include_str!("fixtures/hooks/teammate-events.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const LEAD: &str = "7df9ed81-5900-4844-80cf-77bb499e7e0a";

/// The hooks record a session's teammates and forget them when its process starts again, each
/// silently and with exit 0, as every hook.
#[test]
fn hooks_record_and_forget_subagents() {
    let home = tempfile::tempdir().unwrap();
    {
        let conn = db::open(&home.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        let mut lead = SessionRow::new("demo-lead-1111", "demo", Role::Thread, 0);
        lead.session_id = Some(LEAD.into());
        db::insert_session(&conn, &lead).unwrap();
    }
    let lines = teammate_fixture();
    let run = |payload: &Value| {
        let event = payload["hook_event_name"].as_str().unwrap();
        let out = hook(
            home.path(),
            &[event, "--project", "demo", "--role", "thread"],
            &payload.to_string(),
        );
        assert_silent_success(&out);
    };
    let mates = || {
        let conn = db::open(&home.path().join("chief.db")).unwrap();
        db::teammates(&conn, "demo-lead-1111", LEAD)
            .unwrap()
            .into_iter()
            .map(|t| (t.agent_id, t.name))
            .collect::<Vec<_>>()
    };
    run(&lines[2]); // PostToolUse Agent
    run(&lines[3]); // SubagentStart
    assert_eq!(
        mates(),
        [(
            "ahelper-886b99f57d4badec".to_string(),
            Some("helper".to_string())
        )]
    );
    run(&lines[17]); // SessionStart resume
    assert!(mates().is_empty());
    assert_eq!(error_log(home.path()), "");
}

/// §6 "Subagents" end to end: the hooks record a thread's teammates from the fixture's own
/// payloads, the guard lets the thread and its teammates message each other, checks names at
/// spawn, and forgets them when the process starts again.
#[test]
fn the_guard_lets_a_session_reach_its_own_subagents() {
    let home = Home::new();
    {
        let conn = home.db();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("coord-sid".into());
        db::insert_session(&conn, &coord).unwrap();
        let mut lead = SessionRow::new("demo-lead-1111", "demo", Role::Thread, db::now_ms());
        lead.session_id = Some(LEAD.into());
        db::insert_session(&conn, &lead).unwrap();
    }
    let listing = serde_json::json!([
        agent("c1", "coord-sid", "chief-demo", "done", Some(100)),
        agent("l1", LEAD, "demo-lead-1111", "working", Some(201)),
        // An outside session with a teammate's name (§1).
        agent("x1", "x-sid", "spike-other", "working", Some(300)),
    ])
    .to_string();
    home.fake.set("agents.stdout", &listing);
    let lines = teammate_fixture();
    let record = |payload: &Value| {
        let event = payload["hook_event_name"].as_str().unwrap();
        let out = home.run(
            &["hook", event, "--project", "demo", "--role", "thread"],
            &payload.to_string(),
        );
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    };
    let guard = |payload: &Value| {
        deny_reason(&home.run(
            &["hook", "guard", "--project", "demo", "--role", "thread"],
            &payload.to_string(),
        ))
    };
    let with = |i: usize, key: &str, value: &str| {
        let mut p = lines[i].clone();
        p["tool_input"][key] = value.into();
        p
    };

    record(&lines[2]); // PostToolUse Agent: ahelper-…, "helper"
    record(&lines[3]); // SubagentStart
    assert_eq!(guard(&lines[7]), None, "the lead to helper by name");
    assert_eq!(guard(&lines[9]), None, "the lead to helper by agent id");
    assert_eq!(guard(&lines[5]), None, "a teammate to team-lead");
    let reason = guard(&with(7, "to", "helper [807e0b]")).unwrap();
    assert!(reason.contains("without a `[ref]`"), "{reason}");
    assert!(guard(&with(7, "to", "team-lead")).is_some());

    // A teammate named like an outside session: only by its agent id.
    let mut spawned = lines[2].clone();
    spawned["tool_input"]["name"] = "spike-other".into();
    spawned["tool_response"]["name"] = "spike-other".into();
    spawned["tool_response"]["agentId"] = "aspike-other-ba38036fce05d0d2".into();
    record(&spawned);
    let reason = guard(&lines[11]).unwrap();
    assert!(
        reason.contains("by its agent id aspike-other-ba38036fce05d0d2"),
        "{reason}"
    );
    assert_eq!(guard(&with(9, "to", "aspike-other-ba38036fce05d0d2")), None);

    // At spawn (the fixture's Agent call names its teammate helper).
    assert_eq!(lines[1]["tool_name"], "Agent");
    assert_eq!(guard(&lines[1]), None);
    for taken in ["spike-other", "chief-demo", "demo-lead-1111"] {
        let reason = guard(&with(1, "name", taken)).unwrap();
        assert!(reason.contains("Pick another name"), "{taken}: {reason}");
    }
    // No listing: an Agent call is checked against the database only; a message fails closed.
    home.fake
        .set("agents.exit", "1")
        .set("agents.stderr", "supervisor down");
    assert_eq!(guard(&with(1, "name", "spike-other")), None);
    assert!(guard(&with(1, "name", "chief-demo")).is_some());
    let reason = guard(&lines[9]).unwrap();
    assert!(reason.contains("supervisor down"), "{reason}");
    home.fake.unset("agents.exit").unset("agents.stderr");

    // `claude respawn`: a new process, without the teammates.
    record(&lines[17]);
    for i in [7, 9] {
        let reason = guard(&lines[i]).unwrap();
        assert!(reason.contains("only their coordinator"), "{i}: {reason}");
    }

    // Denials are recorded, an Agent call's with the name it asked for.
    let conn = home.db();
    let target: Option<String> = conn
        .query_row(
            "SELECT target FROM guard_denials WHERE tool = 'Agent' ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target.as_deref(), Some("spike-other"));
    // Both Agent calls without a listing say so, and the message's failure is logged too.
    let log = error_log(home.path());
    assert_eq!(log.lines().count(), 3, "{log}");
    assert_eq!(
        log.matches("checked the name against chief's sessions only")
            .count(),
        2,
        "{log}"
    );
}

/// Claude Code reads exit 2 as "block" (every tool for the `*` PreToolUse, the prompt erased,
/// the turn forced on), so a hook whose arguments clap rejects logs it and exits 0, silently.
#[test]
fn hook_argument_errors_never_block_a_session() {
    let home = tempfile::tempdir().unwrap();
    let cases: [&[&str]; 6] = [
        &["Stop", "--project", "demo", "--role", "bogus"],
        &[
            "PreToolUse",
            "--project",
            "demo",
            "--role",
            "thread",
            "--bogus",
        ],
        &["UserPromptSubmit", "--role", "thread"],
        &["Stop", "--project", "guard", "--role", "bogus"],
        &["--project", "demo", "--role", "nope", "Stop"],
        &[],
    ];
    for args in cases {
        let out = hook(home.path(), args, "{}");
        assert_silent_success(&out);
    }
    let log = error_log(home.path());
    assert_eq!(log.lines().count(), cases.len(), "{log}");
    assert!(
        log.contains("hook Stop --project demo: bad arguments"),
        "{log}"
    );
    assert!(log.contains("bogus"), "{log}");
}

/// The guard fails closed instead: exit 2 with the reason on stderr blocks the call.
#[test]
fn guard_argument_errors_block_the_call() {
    let home = tempfile::tempdir().unwrap();
    let cases: [&[&str]; 3] = [
        &["guard", "--project", "demo", "--role", "bogus"],
        &["guard", "--project", "demo"],
        &["--project", "demo", "--role", "nope", "guard"],
    ];
    for args in cases {
        let out = hook(home.path(), args, "{}");
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = stderr(&out);
        assert!(err.contains("message is blocked"), "{args:?}: {err}");
    }
    // Other subcommands keep clap's usual handling.
    let out = Command::new(env!("CARGO_BIN_EXE_chief"))
        .args(["status", "--bogus"])
        .env("CHIEF_HOME", home.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--bogus"));
}

#[test]
fn guard_fails_closed() {
    // No tool_name: the guard can't decide, so it denies, logs, and records the denial.
    let home = Home::new();
    let out = home.run(
        &[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        "{}",
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("guard failed") && reason.contains("tool_name"),
        "{reason}"
    );
    assert!(error_log(home.path()).contains("hook guard --project demo"));
    assert_eq!(db::count_guard_denials(&home.db(), "demo").unwrap(), 1);

    // `claude agents` failing is an error too.
    home.fake
        .set("agents.exit", "1")
        .set("agents.stderr", "supervisor down");
    let out = home.run(
        &[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        &send("demo-a-1111"),
    );
    let reason = deny_reason(&out).unwrap();
    assert!(reason.contains("supervisor down"), "{reason}");

    // Not JSON at all.
    let out = home.run(
        &["hook", "guard", "--project", "demo", "--role", "thread"],
        "{not json",
    );
    assert!(
        deny_reason(&out)
            .unwrap()
            .contains("parsing the hook payload")
    );
}

/// A guard that can't finish (here the database stays locked) still denies before Claude Code's
/// hook timeout would let the call through.
#[test]
fn guard_denies_at_its_deadline() {
    let home = guarded_home();
    let lock = home.db();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let start = std::time::Instant::now();
    let mut cmd = home.command(&[
        "hook",
        "guard",
        "--project",
        "demo",
        "--role",
        "coordinator",
    ]);
    cmd.env("CHIEF_GUARD_DEADLINE_MS", "1000");
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(send("demo-a-1111").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let took = start.elapsed();
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(took < std::time::Duration::from_secs(4), "took {took:?}");
    let reason = deny_reason(&out).unwrap();
    assert!(reason.contains("didn't decide within 1s"), "{reason}");
    assert!(error_log(home.path()).contains("didn't decide within 1s"));
}

fn send(to: &str) -> String {
    serde_json::json!({
        "session_id": "coord-sid",
        "hook_event_name": "PreToolUse",
        "tool_name": "SendMessage",
        "tool_input": {"to": to, "message": "hi"},
        "cwd": "/x",
    })
    .to_string()
}

fn deny_reason(out: &Output) -> Option<String> {
    assert!(out.status.success(), "{out:?}");
    if out.stdout.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    Some(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

/// A project with a live coordinator, a live thread and an asleep one.
fn guarded_home() -> Home {
    let home = Home::new();
    {
        let conn = home.db();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.session_id = Some("coord-sid".into());
        db::insert_session(&conn, &coord).unwrap();
        for (name, sid) in [("demo-a-1111", "a-sid"), ("demo-b-2222", "b-sid")] {
            // Recent, or the inactivity rule would resolve the asleep one.
            let mut t = SessionRow::new(name, "demo", Role::Thread, db::now_ms());
            t.session_id = Some(sid.into());
            db::insert_session(&conn, &t).unwrap();
        }
    }
    home.fake.set(
        "agents.stdout",
        &serde_json::json!([
            agent("c1", "coord-sid", "chief-demo", "done", Some(100)),
            agent("a1", "a-sid", "demo-a-1111", "working", Some(201)),
            agent("b1", "b-sid", "demo-b-2222", "done", None),
            agent("x1", "x-sid", "other-project-thread", "working", Some(300)),
        ])
        .to_string(),
    );
    home
}

/// The guard asks the project's account who is alive, whatever account the hook's environment
/// names: another account's supervisor knows nothing about this project's sessions.
#[test]
fn the_guard_asks_the_projects_account() {
    let home = guarded_home();
    let carlos = common::account(home.path(), "claude-carlos");
    let mut config = chief::config::ProjectConfig::new("Demo", "demo", None, vec![]);
    config.claude_config_dir = Some(carlos.clone());
    config
        .save(&home.path().join("projects/demo/project.toml"))
        .unwrap();
    // The default account's supervisor lists none of them.
    home.fake.set("agents.stdout", "[]");
    std::fs::write(
        carlos.join("fake-agents.stdout"),
        serde_json::json!([
            agent("c1", "coord-sid", "chief-demo", "done", Some(100)),
            agent("a1", "a-sid", "demo-a-1111", "working", Some(201)),
        ])
        .to_string(),
    )
    .unwrap();
    let other = common::account(home.path(), "claude-other");
    let out = home
        .command(&[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ])
        .env("CLAUDE_CONFIG_DIR", &other)
        .output_with_stdin(&send("demo-a-1111"));
    assert_eq!(deny_reason(&out), None);
    let accounts = home.fake.accounts();
    assert_eq!(
        accounts,
        [("agents".to_string(), carlos.to_string_lossy().into_owned())]
    );
}

/// A thread in another account (§3, §6): the coordinator reaches it only at its socket address,
/// checked in that account's own listing; the thread reaches its coordinator only at the
/// coordinator's address, checked in the project's account.
#[test]
fn the_guard_checks_a_thread_in_its_own_account() {
    let home = guarded_home();
    let cpb = common::account(home.path(), "claude-cpb");
    {
        let mut t = SessionRow::new("demo-far-3333", "demo", Role::Thread, db::now_ms());
        t.session_id = Some("far-sid".into());
        common::in_account(&mut t, "cpb", "PB2", Some(&cpb));
        db::insert_session(&home.db(), &t).unwrap();
    }
    common::account_agents(
        &cpb,
        &[agent(
            "f1",
            "far-sid",
            "demo-far-3333",
            "working",
            Some(701),
        )],
    );
    let guard = |role: &str, payload: &str| {
        deny_reason(&home.run(
            &["hook", "guard", "--project", "demo", "--role", role],
            payload,
        ))
    };
    assert_eq!(
        guard("coordinator", &send("uds:/tmp/cc-socks/701.sock")),
        None
    );
    let reason = guard("coordinator", &send("demo-far-3333")).unwrap();
    assert!(
        reason.contains("runs in another Claude account (PB2)")
            && reason.contains("uds:/tmp/cc-socks/701.sock"),
        "{reason}"
    );
    // A thread of the project's account by name: only that account is asked.
    let before = home.fake.accounts().len();
    assert_eq!(guard("coordinator", &send("demo-a-1111")), None);
    let asked: Vec<_> = home.fake.accounts()[before..].to_vec();
    assert_eq!(asked, [("agents".to_string(), "unset".to_string())]);

    // The far thread messaging its coordinator.
    let from_far = |to: &str| {
        serde_json::json!({
            "session_id": "far-sid",
            "tool_name": "SendMessage",
            "tool_input": {"to": to, "message": "done?"},
        })
        .to_string()
    };
    let reason = guard("thread", &from_far("chief-demo")).unwrap();
    assert!(reason.contains("another Claude account (PB2)"), "{reason}");
    assert_eq!(
        guard("thread", &from_far("uds:/tmp/cc-socks/100.sock")),
        None
    );

    // Its account can't be listed: denied, saying why.
    std::fs::write(cpb.join("fake-agents.stdout"), "garbage").unwrap();
    let reason = guard("coordinator", &send("uds:/tmp/cc-socks/701.sock")).unwrap();
    assert!(
        reason.contains("couldn't list the accounts of demo-far-3333"),
        "{reason}"
    );
}

#[test]
fn guard_scopes_the_coordinator_to_its_threads() {
    let home = guarded_home();
    let guard = |to: &str| {
        home.run(
            &[
                "hook",
                "guard",
                "--project",
                "demo",
                "--role",
                "coordinator",
            ],
            &send(to),
        )
    };
    let ok = guard("demo-a-1111");
    assert!(ok.stdout.is_empty() && ok.stderr.is_empty(), "{ok:?}");
    // A ref picks a session, so it could redirect the message: only the bare name passes.
    let reason = deny_reason(&guard("demo-a-1111 [a1]")).unwrap();
    assert!(reason.contains("without a `[ref]`"), "{reason}");

    let reason = deny_reason(&guard("other-project-thread")).unwrap();
    assert!(reason.contains("not a thread of this project"), "{reason}");
    assert_eq!(
        deny_reason(&guard("demo-b-2222")).unwrap(),
        "Thread is asleep: call wake_thread first."
    );
    assert_eq!(
        error_log(home.path()),
        "",
        "denials are decisions, not errors"
    );

    let list =
        serde_json::json!({"session_id": "coord-sid", "tool_name": "ListAgents", "tool_input": {}});
    let reason = deny_reason(&home.run(
        &[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        &list.to_string(),
    ))
    .unwrap();
    assert!(reason.contains("list_threads"));

    let no_to = serde_json::json!({"tool_name": "SendMessage", "tool_input": "demo-a-1111"});
    assert!(
        deny_reason(&home.run(
            &[
                "hook",
                "guard",
                "--project",
                "demo",
                "--role",
                "coordinator"
            ],
            &no_to.to_string(),
        ))
        .is_some()
    );

    let conn = home.db();
    assert_eq!(db::count_guard_denials(&conn, "demo").unwrap(), 5);
    let (who, target, tool): (Option<String>, Option<String>, String) = conn
        .query_row(
            "SELECT session_name, target, tool FROM guard_denials ORDER BY id LIMIT 1 OFFSET 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (who.as_deref(), target.as_deref(), tool.as_str()),
        (
            Some("chief-demo"),
            Some("other-project-thread"),
            "SendMessage"
        )
    );
}

#[test]
fn guard_keeps_threads_to_their_coordinator() {
    let home = guarded_home();
    let guard = |to: &str| {
        home.run(
            &["hook", "guard", "--project", "demo", "--role", "thread"],
            &send(to),
        )
    };
    assert_eq!(deny_reason(&guard("chief-demo")), None);
    assert_eq!(deny_reason(&guard("uds:/tmp/cc-socks/100.sock")), None);
    // Only the coordinator: not a subagent chief has no record of, nor a ref'd coordinator name.
    assert!(deny_reason(&guard("my-subagent")).is_some());
    assert!(deny_reason(&guard("chief-demo [c1]")).is_some());
    assert!(deny_reason(&guard("uds:100.sock")).is_some());
    assert!(deny_reason(&guard("other-project-thread")).is_some());
    assert!(deny_reason(&guard("demo-a-1111")).is_some());
    assert!(deny_reason(&guard("uds:/tmp/cc-socks/300.sock")).is_some());
}

#[test]
fn coordinator_prompts_get_the_board_and_memory() {
    let home = guarded_home();
    std::fs::create_dir_all(home.path().join("projects/demo/memory")).unwrap();
    std::fs::write(
        home.path().join("projects/demo/memory/MEMORY.md"),
        "# Memory\n- [Style](style.md)\n",
    )
    .unwrap();
    let payload = serde_json::json!({
        "session_id": "coord-sid",
        "hook_event_name": "UserPromptSubmit",
        "prompt": "status?",
        "cwd": "/x",
    });
    let out = home.run(
        &[
            "hook",
            "UserPromptSubmit",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        &payload.to_string(),
    );
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.starts_with("<chief-board as-of=\""), "{ctx}");
    assert!(ctx.contains("Working:\n- demo-a-1111"), "{ctx}");
    assert!(ctx.contains("- demo-b-2222 · asleep"), "{ctx}");
    assert!(!ctx.contains("other-project-thread"));
    assert!(
        ctx.ends_with("<chief-memory>\n# Memory\n- [Style](style.md)\n</chief-memory>"),
        "{ctx}"
    );
    assert_eq!(error_log(home.path()), "");

    // The board refresh failing still yields the stored board, and is logged.
    home.fake.set("agents.exit", "1");
    let out = home.run(
        &[
            "hook",
            "UserPromptSubmit",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        &payload.to_string(),
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("demo-a-1111")
    );
    assert!(error_log(home.path()).contains("board refresh failed"));

    // Threads print nothing.
    let thread = home.run(
        &[
            "hook",
            "UserPromptSubmit",
            "--project",
            "demo",
            "--role",
            "thread",
        ],
        &payload.to_string(),
    );
    assert!(thread.stdout.is_empty());
}
