//! `chief hook` as Claude Code runs it: payload on stdin, exit 0, empty stdout, errors logged.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chief::db::{self, SessionRow};
use chief::state::Role;

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

#[test]
fn panics_are_caught_and_logged() {
    // The guard is still a todo!() until M2: a panic must not reach the session.
    let home = tempfile::tempdir().unwrap();
    let out = hook(
        home.path(),
        &["guard", "--project", "demo", "--role", "coordinator"],
        "{}",
    );
    assert_silent_success(&out);
    let log = error_log(home.path());
    assert!(log.contains("hook guard --project demo: panic:"), "{log}");
}
