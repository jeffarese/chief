//! `chief hook` as Claude Code runs it: payload on stdin, exit 0, errors logged; stdout only for
//! the guard's decision, the coordinator's context (and its brief when that went stale), the
//! denial of a coordinator's look too far or too many, the denial of a subagent's background
//! command and the Stop decision that asks a thread for its final report. The coordinator's background
//! watcher is the one exception: it exits 2, with its notice on stderr, to wake the coordinator.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, OutputWithStdin, agent, stderr};
use serde_json::Value;

/// `chief hook` with a temp home and no `claude` at all: a hook that tried to list sessions
/// would fail (and log it) rather than reach the real one.
fn hook(home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chief"))
        .arg("hook")
        .args(args)
        .env("CHIEF_HOME", home)
        .env("CHIEF_NO_NOTIFY", "1")
        .env("CHIEF_CLAUDE_BIN", home.join("no-claude"))
        .env("CHIEF_CLAUDE_DIR", home.join("no-claude-dir"))
        .env_remove("CLAUDE_CONFIG_DIR")
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

/// Another process holds the write lock past the 5 s busy wait: a hook that can't record its
/// event still exits 0 with nothing on stdout and logs why, whatever the event, and gives up after
/// about 5 s of real time (SQLite's own busy_timeout took about 12 s for 5000 ms).
#[test]
fn hooks_against_a_locked_database_log_and_exit_zero() {
    let home = tempfile::tempdir().unwrap();
    let db_path = home.path().join("chief.db");
    {
        let conn = db::open(&db_path).unwrap();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        let mut t = SessionRow::new("demo-a-1a2b", "demo", Role::Thread, 0);
        t.short_id = Some("a1".into());
        t.session_id = Some("a-sid".into());
        db::insert_session(&conn, &t).unwrap();
    }
    let lock = db::open(&db_path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let payload = |event: &str| {
        serde_json::json!({
            "session_id": "a-sid",
            "hook_event_name": event,
            "cwd": "/x",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "last_assistant_message": "done",
            "background_tasks": [],
        })
        .to_string()
    };
    let started = std::time::Instant::now();
    let outs: Vec<Output> = std::thread::scope(|scope| {
        let runs: Vec<_> = ["Stop", "PreToolUse", "PostToolUse"]
            .map(|event| {
                let stdin = payload(event);
                let home = home.path();
                scope.spawn(move || {
                    hook(
                        home,
                        &[event, "--project", "demo", "--role", "thread"],
                        &stdin,
                    )
                })
            })
            .into_iter()
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let waited = started.elapsed();
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(waited < std::time::Duration::from_secs(10), "{waited:?}");
    for out in &outs {
        assert_silent_success(out);
    }
    let log = error_log(home.path());
    assert_eq!(log.lines().count(), 3, "{log}");
    for event in ["Stop", "PreToolUse", "PostToolUse"] {
        assert!(
            log.lines()
                .any(|l| l.contains(&format!("hook {event} --project demo"))
                    && l.contains("database is locked")),
            "{event}: {log}"
        );
    }
    let conn = db::open(&db_path).unwrap();
    assert!(db::recent_events(&conn, "demo", 10).unwrap().is_empty());
}

/// §4: after `/clear` the session runs under a new id, which a sync learns by short id. A Stop
/// in between syncs first, so its report isn't lost; a Stop chief can match asks nothing.
#[test]
fn a_stop_after_clear_keeps_its_report() {
    let home = Home::new();
    {
        let conn = home.db();
        db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
        coord.short_id = Some("c1".into());
        coord.session_id = Some("coord-sid".into());
        db::insert_session(&conn, &coord).unwrap();
        let mut t = SessionRow::new("demo-t-1111", "demo", Role::Thread, db::now_ms());
        t.short_id = Some("t1".into());
        t.session_id = Some("old-sid".into());
        db::insert_session(&conn, &t).unwrap();
    }
    home.fake.set(
        "agents.stdout",
        &serde_json::json!([
            agent("c1", "coord-sid", "chief-demo", "done", Some(100)),
            agent("t1", "new-sid", "demo-t-1111", "working", Some(201)),
        ])
        .to_string(),
    );
    let stop = |sid: &str, message: &str| {
        let payload = serde_json::json!({
            "session_id": sid,
            "hook_event_name": "Stop",
            "cwd": "/x",
            "last_assistant_message": message,
            "background_tasks": [],
        });
        home.run(
            &["hook", "Stop", "--project", "demo", "--role", "thread"],
            &payload.to_string(),
        )
    };
    assert_silent_success(&stop("new-sid", "Done after /clear."));
    assert_eq!(error_log(home.path()), "");
    let conn = home.db();
    let row = db::get_session(&conn, "demo-t-1111").unwrap().unwrap();
    assert_eq!(
        (row.session_id.as_deref(), row.last_report.as_deref()),
        (Some("new-sid"), Some("Done after /clear."))
    );
    assert_eq!(
        db::recent_reports(&conn, "demo-t-1111", 10).unwrap().len(),
        1
    );
    assert_eq!(home.fake.calls_of("agents").len(), 1);

    // Now it matches: no listing.
    assert_silent_success(&stop("new-sid", "Again."));
    assert_eq!(home.fake.calls_of("agents").len(), 1);

    // A session nobody lists stays unmatched, and a failing listing is logged.
    home.fake
        .set("agents.exit", "1")
        .set("agents.stderr", "down");
    assert_silent_success(&stop("stranger-sid", "Who?"));
    let log = error_log(home.path());
    assert!(
        log.contains("hook Stop --project demo: couldn't look up session stranger-sid"),
        "{log}"
    );
    let events = db::recent_events(&conn, "demo", 1).unwrap();
    assert_eq!(
        (
            events[0].session_name.as_deref(),
            events[0].session_id.as_deref()
        ),
        (None, Some("stranger-sid"))
    );
}

/// §5 "Telling the coordinator": the coordinator's background watcher exits 2 with its notice on
/// stderr, which Claude Code wakes the coordinator with, once a thread has been stuck past the
/// grace, and marks it told. A thread's watcher does nothing.
#[test]
fn the_watcher_wakes_the_coordinator_for_a_stuck_thread() {
    let home = tempfile::tempdir().unwrap();
    let conn = db::open(&home.path().join("chief.db")).unwrap();
    db::register_project(&conn, "demo", &home.path().join("projects/demo"), 0).unwrap();
    let mut t = SessionRow::new("demo-a-1a2b", "demo", Role::Thread, 0);
    t.title = Some("Commit the design".into());
    t.group_name = "waiting".into();
    t.waiting_reason = Some("permission".into());
    t.pending_permission = true;
    t.blocked_since = Some(db::now_ms() - 60_000);
    db::insert_session(&conn, &t).unwrap();

    let watch = |role: &str| {
        hook(
            home.path(),
            &["watch", "--project", "demo", "--role", role],
            "{\"hook_event_name\":\"Stop\"}",
        )
    };
    assert_silent_success(&watch("thread"));
    assert_eq!(db::get_watcher(&conn, "demo").unwrap(), None);

    let out = watch("coordinator");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let notice = String::from_utf8_lossy(&out.stderr);
    assert!(
        notice.starts_with(
            "<chief-waiting>\n- demo-a-1a2b \"Commit the design\" · permission since "
        ),
        "{notice}"
    );
    assert!(
        notice.contains("opening the thread on the board"),
        "{notice}"
    );
    let row = db::get_session(&conn, "demo-a-1a2b").unwrap().unwrap();
    assert!(row.blocked_told_at.is_some());
    assert_eq!(error_log(home.path()), "");
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

/// A guard that can't finish (here the database stays locked, so it can't record its denial;
/// opening a current database takes no lock) still denies before Claude Code's hook timeout
/// would let the call through.
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
        .write_all(send("other-project-thread").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let took = start.elapsed();
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(took < std::time::Duration::from_secs(4), "took {took:?}");
    let reason = deny_reason(&out).unwrap();
    assert!(reason.contains("didn't decide within 1s"), "{reason}");
    assert!(error_log(home.path()).contains("didn't decide within 1s"));
}

/// The watchdog also covers a `claude agents` that hangs, whose own bound (10 s) is longer than
/// a shortened deadline.
#[test]
fn guard_denies_at_its_deadline_while_the_listing_hangs() {
    let home = guarded_home();
    home.fake.set("agents.sleep", "30");
    let start = std::time::Instant::now();
    let out = home
        .command(&[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ])
        .env("CHIEF_GUARD_DEADLINE_MS", "1000")
        .output_with_stdin(&send("demo-a-1111"));
    let took = start.elapsed();
    // The guard exited at its deadline and left the hanging listing behind: stop it.
    let listing = home
        .fake
        .calls()
        .iter()
        .position(|c| c.first().map(String::as_str) == Some("agents"))
        .expect("the guard listed the sessions");
    let pid = home.fake.pid(listing);
    assert!(
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success()
    );
    assert!(took < std::time::Duration::from_secs(4), "took {took:?}");
    let reason = deny_reason(&out).unwrap();
    assert!(reason.contains("didn't decide within 1s"), "{reason}");
    assert!(error_log(home.path()).contains("didn't decide within 1s"));
}

/// A guard that fails after reading the payload records the call it blocked: the tool, the
/// target and the caller.
#[test]
fn a_failed_guard_records_what_it_knows() {
    let home = guarded_home();
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
    assert!(deny_reason(&out).unwrap().contains("supervisor down"));
    let out = home.run(
        &[
            "hook",
            "guard",
            "--project",
            "demo",
            "--role",
            "coordinator",
        ],
        "{not json",
    );
    assert!(deny_reason(&out).is_some());
    let rows: Vec<(Option<String>, String, Option<String>)> = home
        .db()
        .prepare("SELECT session_name, tool, target FROM guard_denials ORDER BY ts, rowid")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            (
                Some("chief-demo".to_string()),
                "SendMessage".to_string(),
                Some("demo-a-1111".to_string())
            ),
            (None, "unknown".to_string(), None),
        ]
    );
}

/// A thread caller chief doesn't know (its session id after `/clear`) while a thread runs in
/// another account: placed by its transcript's folder, and kept from its coordinator's name
/// when that doesn't place it (§6).
#[test]
fn the_guard_places_an_unknown_thread_by_its_transcript() {
    let home = guarded_home();
    let cpb = common::account(home.path(), "claude-cpb");
    {
        let mut t = SessionRow::new("demo-far-3333", "demo", Role::Thread, db::now_ms());
        t.session_id = Some("far-sid".into());
        common::in_account(&mut t, "cpb", "PB2", Some(&cpb));
        db::insert_session(&home.db(), &t).unwrap();
    }
    common::account_agents(&cpb, &[]);
    let from_new = |transcript: Option<&Path>| {
        let mut payload = serde_json::json!({
            "session_id": "new-sid",
            "tool_name": "SendMessage",
            "tool_input": {"to": "chief-demo", "message": "done"},
        });
        if let Some(t) = transcript {
            payload["transcript_path"] = t.to_string_lossy().into();
        }
        deny_reason(&home.run(
            &["hook", "guard", "--project", "demo", "--role", "thread"],
            &payload.to_string(),
        ))
    };
    // The project's account is the default one, whose folder is the fake's.
    let here = home.fake.config_dir().join("projects/-repo/new-sid.jsonl");
    assert_eq!(from_new(Some(&here)), None);
    let there = cpb.join("projects/-repo/new-sid.jsonl");
    let reason = from_new(Some(&there)).unwrap();
    assert!(reason.contains("another Claude account (PB2)"), "{reason}");
    let reason = from_new(None).unwrap();
    assert!(reason.contains("can't tell whether you run in"), "{reason}");
    assert!(reason.contains("from=\"uds:"), "{reason}");
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

/// §5 "Background work": a recorded subagent's command in the background is denied, as Claude
/// Code wouldn't wake the subagent when it ends; its lead's own isn't, nor an unknown agent's.
#[test]
fn subagent_background_commands_are_denied() {
    let home = guarded_home();
    let start = serde_json::json!({
        "session_id": "a-sid",
        "hook_event_name": "SubagentStart",
        "agent_id": "aworker-1",
        "agent_type": "general-purpose",
        "cwd": "/x",
    });
    assert_silent_success(&home.run(
        &[
            "hook",
            "SubagentStart",
            "--project",
            "demo",
            "--role",
            "thread",
        ],
        &start.to_string(),
    ));
    let bash = |agent_id: Option<&str>| {
        let mut payload = serde_json::json!({
            "session_id": "a-sid",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test", "run_in_background": true},
            "cwd": "/x",
        });
        if let Some(a) = agent_id {
            payload["agent_id"] = a.into();
        }
        home.run(
            &[
                "hook",
                "PreToolUse",
                "--project",
                "demo",
                "--role",
                "thread",
            ],
            &payload.to_string(),
        )
    };
    let reason = deny_reason(&bash(Some("aworker-1"))).unwrap();
    assert!(
        reason.contains("Run the command in the foreground"),
        "{reason}"
    );
    assert_silent_success(&bash(None));
    assert_silent_success(&bash(Some("aunknown-2")));
    assert_eq!(error_log(home.path()), "");
}

/// §5 "Background work" end to end: a thread that ended a turn while its subagent worked is
/// asked for its final report at its first Stop after, addressed as its coordinator can be
/// reached: by name, or for a thread in another account at the coordinator's socket.
#[test]
fn a_thread_that_stopped_early_is_asked_for_its_final_report() {
    let home = guarded_home();
    let cpb = common::account(home.path(), "claude-cpb");
    {
        let mut t = SessionRow::new("demo-far-3333", "demo", Role::Thread, db::now_ms());
        t.session_id = Some("far-sid".into());
        common::in_account(&mut t, "cpb", "PB2", Some(&cpb));
        db::insert_session(&home.db(), &t).unwrap();
    }
    for (sid, address) in [
        ("a-sid", "chief-demo"),
        ("far-sid", "uds:/tmp/cc-socks/100.sock"),
    ] {
        let event = |name: &str, extra: Value| {
            let mut payload = serde_json::json!({
                "session_id": sid,
                "hook_event_name": name,
                "cwd": "/x",
            });
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            home.run(
                &["hook", name, "--project", "demo", "--role", "thread"],
                &payload.to_string(),
            )
        };
        let teammate = serde_json::json!([{"id": "t1", "type": "teammate", "status": "running"}]);
        let stop = |active: bool| {
            event(
                "Stop",
                serde_json::json!({
                    "stop_hook_active": active,
                    "last_assistant_message": "done",
                    "background_tasks": teammate,
                }),
            )
        };
        let subagent =
            serde_json::json!({"agent_id": "aworker-1", "agent_type": "general-purpose"});
        assert_silent_success(&event("SubagentStart", subagent.clone()));
        assert_silent_success(&stop(false));
        assert_silent_success(&event("SubagentStop", subagent));
        let out = stop(false);
        assert!(out.status.success(), "{out:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["decision"], "block");
        let reason = v["reason"].as_str().unwrap();
        assert!(
            reason.contains(&format!("SendMessage (to: \"{address}\")")),
            "{reason}"
        );
        assert_silent_success(&stop(true));
    }
    assert_eq!(error_log(home.path()), "");
}

fn coordinator_hook(home: &Home, event: &str, payload: &Value) -> Output {
    home.run(
        &["hook", event, "--project", "demo", "--role", "coordinator"],
        &payload.to_string(),
    )
}

/// The coordinator routes work and answers quick questions (§6 "The coordinator's own
/// looking"): it looks only in the project's folders, and a turn gets only so many lookups
/// before the rest is a thread's.
#[test]
fn the_coordinator_looks_only_so_far_and_so_much() {
    use chief::config::{ProjectConfig, Repo};
    let home = guarded_home();
    let dir = home.path().join("projects/demo");
    let repo = home.path().join("repo");
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let config = ProjectConfig::new(
        "Demo",
        "demo",
        None,
        vec![Repo {
            name: "repo".into(),
            path: repo.clone(),
        }],
    );
    config.save(&dir.join("project.toml")).unwrap();
    let call = |role: &str, sid: &str, tool: &str, input: Value| {
        home.run(
            &["hook", "PreToolUse", "--project", "demo", "--role", role],
            &serde_json::json!({
                "session_id": sid,
                "hook_event_name": "PreToolUse",
                "tool_name": tool,
                "tool_input": input,
                "cwd": dir,
            })
            .to_string(),
        )
    };
    let look =
        |tool: &str, input: Value| deny_reason(&call("coordinator", "coord-sid", tool, input));
    let glob = || {
        look(
            "Glob",
            serde_json::json!({"pattern": "**/*.rs", "path": repo}),
        )
    };

    // Its own memory is free, as often as it likes; other tools are none of this.
    for _ in 0..10 {
        assert_eq!(
            look("Read", serde_json::json!({"file_path": "memory/MEMORY.md"})),
            None
        );
    }
    assert_eq!(
        look("Edit", serde_json::json!({"file_path": "memory/x.md"})),
        None
    );
    // A handful of lookups in the repository, then the rest is a thread's.
    for n in 1..=chief::lookups::PER_TURN {
        assert_eq!(glob(), None, "lookup {n}");
    }
    let why = glob().expect("one lookup too many");
    assert!(why.contains("start a thread"), "{why}");
    assert!(
        look("WebSearch", serde_json::json!({"query": "fares"})).is_some(),
        "the web counts too"
    );
    assert_eq!(
        look("Read", serde_json::json!({"file_path": "memory/MEMORY.md"})),
        None
    );

    // The user's next message starts a new turn. Nothing outside the project is looked at,
    // other sessions' transcripts included.
    let prompt = serde_json::json!({
        "session_id": "coord-sid",
        "hook_event_name": "UserPromptSubmit",
        "prompt": "it was in an earlier conversation",
        "cwd": dir,
    });
    assert!(
        coordinator_hook(&home, "UserPromptSubmit", &prompt)
            .status
            .success()
    );
    assert_eq!(glob(), None);
    let transcripts = home.path().join("claude/projects");
    let why = look(
        "Grep",
        serde_json::json!({"pattern": "routine", "path": transcripts}),
    )
    .expect("outside the project");
    assert!(why.contains("outside this project's folders"), "{why}");
    assert!(why.contains("start a thread"), "{why}");

    // A thread looks where and as much as its work needs.
    for _ in 0..10 {
        let out = call(
            "thread",
            "a-sid",
            "Grep",
            serde_json::json!({"pattern": "x", "path": "/"}),
        );
        assert_eq!(deny_reason(&out), None);
    }
    assert!(
        error_log(home.path()).is_empty(),
        "{}",
        error_log(home.path())
    );
}

/// `claude respawn` starts the coordinator again with the brief of its first start, and its
/// tools are the chief its process started with. A brief that changed since is too long for a
/// hook to carry, so chief tells the coordinator to read it, until it has; and says when its
/// tools are the old ones (§8 "A brief that went stale").
#[test]
fn a_coordinator_with_a_stale_brief_is_told_to_read_the_new_one() {
    use chief::config::ProjectConfig;
    let home = guarded_home();
    let dir = home.path().join("projects/demo");
    std::fs::create_dir_all(dir.join("generated")).unwrap();
    let mut config = ProjectConfig::new("Demo", "demo", Some("Ship it"), vec![]);
    config.save(&dir.join("project.toml")).unwrap();
    let event = |name: &str, extra: Value| {
        let mut payload = serde_json::json!({
            "session_id": "coord-sid",
            "hook_event_name": name,
            "cwd": dir,
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        coordinator_hook(&home, name, &payload)
    };
    let context = |out: &Output, hook: &str| -> Option<String> {
        assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
        if out.stdout.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], hook);
        Some(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let start = |source: &str| {
        context(
            &event("SessionStart", serde_json::json!({"source": source})),
            "SessionStart",
        )
    };
    let prompt = || {
        context(
            &event("UserPromptSubmit", serde_json::json!({"prompt": "status?"})),
            "UserPromptSubmit",
        )
        .expect("the board")
    };
    let brief = dir.join("generated/coordinator-brief.md");
    let read_brief = || {
        let read = serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": brief},
        });
        assert_silent_success(&event("PostToolUse", read));
    };

    // A session from before chief kept its first brief: told to read the current one, in a
    // line short enough to arrive whole. The brief itself is in the file.
    let said = start("resume").expect("the notice");
    assert!(said.starts_with("<chief-brief>"), "{said}");
    assert!(said.contains(&brief.display().to_string()), "{said}");
    assert!(said.len() < 1_000);
    let text = std::fs::read_to_string(&brief).unwrap();
    assert!(text.contains("You are the coordinator of the chief project \"Demo\". Goal: Ship it"));
    assert!(text.contains("start a lookup thread"), "{text}");
    // Said again with every prompt until it has read it, then no more.
    assert!(prompt().starts_with("<chief-brief>"));
    assert!(prompt().starts_with("<chief-brief>"));
    read_brief();
    assert!(prompt().starts_with("<chief-board as-of=\""));
    assert_eq!(start("resume"), None);
    // `/clear` and a compaction empty its context: it reads it again.
    assert_silent_success(&event("SessionEnd", serde_json::json!({"reason": "clear"})));
    assert!(prompt().starts_with("<chief-brief>"));
    read_brief();
    assert!(start("compact").is_some());
    read_brief();
    // The project changed since: there is a new brief to read.
    config.goal = Some("Ship it by Friday".into());
    config.save(&dir.join("project.toml")).unwrap();
    assert!(prompt().starts_with("<chief-brief>"));
    assert!(
        std::fs::read_to_string(&brief)
            .unwrap()
            .contains("by Friday")
    );

    // The brief it was created with is the one it has: nothing to say.
    let first = chief::prompts::coordinator(&config, &dir, "");
    chief::generated::save_coordinator_prompt(&dir, &first).unwrap();
    assert_eq!(start("startup"), None);
    assert!(prompt().starts_with("<chief-board as-of=\""));

    // Its process started before this chief was installed: its tools are the old ones, and
    // it is told so with what restarts it. (The startup above is newer than the binary.)
    home.db()
        .execute("UPDATE events SET ts = 1 WHERE hook = 'SessionStart'", [])
        .unwrap();
    let ctx = prompt();
    assert!(ctx.starts_with("<chief-update>"), "{ctx}");
    assert!(ctx.contains("chief down demo && chief up demo"), "{ctx}");
    assert!(ctx.contains("<chief-board as-of=\""), "{ctx}");
    // Everything a prompt carries stays under what Claude Code lets a hook add.
    assert!(ctx.len() < 10_000, "{}", ctx.len());

    // A thread's start says nothing.
    let out = home.run(
        &[
            "hook",
            "SessionStart",
            "--project",
            "demo",
            "--role",
            "thread",
        ],
        &serde_json::json!({
            "session_id": "a-sid",
            "hook_event_name": "SessionStart",
            "source": "resume",
            "cwd": dir,
        })
        .to_string(),
    );
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert!(
        error_log(home.path()).is_empty(),
        "{}",
        error_log(home.path())
    );
}
