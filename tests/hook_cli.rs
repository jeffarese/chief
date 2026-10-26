//! `chief hook` as Claude Code runs it: payload on stdin, exit 0, errors logged; stdout only for
//! the guard's decision and the coordinator's context.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, agent, stderr};
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
    // Only the coordinator: not even the thread's own subagents or a ref'd coordinator name.
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
