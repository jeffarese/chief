//! The built-in layout (`chief up --layout builtin`) on a real pseudo-terminal with the fake
//! `claude`: the conversation's attach in an embedded terminal, the board beside it. No real
//! Claude session and no multiplexer.

mod common;

use std::time::{Duration, Instant};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::pty::{Pane, gone, wait_for};
use common::{Home, agent};
use serde_json::json;

const COORD: &str = "chief-demo";
const THREAD: &str = "demo-a-1111";

fn attach_call(home: &Home, short: &str) -> Option<usize> {
    home.fake
        .calls()
        .iter()
        .position(|c| c.len() == 2 && c[0] == "attach" && c[1] == short)
}

/// Waits for `text` on the layout's terminal; on a timeout, says what it showed instead. ratatui
/// skips cells that are already blank, so `text` must have no spaces.
fn wait_until_shown(ui: &Pane, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ui.says(text) {
        if Instant::now() > deadline {
            let out = ui.output();
            let tail = &out[out.len().saturating_sub(3000)..];
            panic!(
                "{text:?} never shown; the end of the output:\n{}",
                String::from_utf8_lossy(tail)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A project with a coordinator and a thread, both running; each attach prints a word and stays.
fn project() -> Home {
    let home = Home::new();
    home.register("demo");
    let conn = home.db();
    for (name, role, short) in [
        (COORD, Role::Coordinator, "c0"),
        (THREAD, Role::Thread, "t1"),
    ] {
        let mut row = SessionRow::new(name, "demo", role, 1);
        row.short_id = Some(short.into());
        row.session_id = Some(format!("{short}-sid"));
        db::insert_session(&conn, &row).unwrap();
    }
    home.fake
        .set("version.stdout", "2.1.316 (Claude Code)\n")
        .set(
            "agents.stdout",
            &json!([
                agent("c0", "c0-sid", COORD, "done", Some(4001)),
                agent("t1", "t1-sid", THREAD, "done", Some(4002)),
            ])
            .to_string(),
        )
        .set("attach.c0.stdout", "coordinator-here\n")
        .set("attach.t1.stdout", "thread-here\n")
        .set("attach.sleep", "30");
    home
}

/// One process shows the coordinator and the board, records itself as the project's pane, so a
/// board's switch reaches it as it reaches `chief pane`, and closes with the board's `q`,
/// leaving every session running.
#[test]
fn the_layout_shows_both_sides_and_switches_like_a_pane() {
    let home = project();
    let conn = home.db();
    let mut ui = Pane::spawn_sized(
        &home,
        &["up", "demo", "--layout", "builtin"],
        &[],
        (140, 40),
    );
    wait_until_shown(&ui, "coordinator-here");
    assert_eq!(
        db::get_pane_showing(&conn, "demo").unwrap().as_deref(),
        Some(COORD)
    );
    let (pid, _) = db::get_pane_process(&conn, "demo")
        .unwrap()
        .expect("recorded as the pane");
    assert_eq!(pid, ui.pid());
    // The board is beside it, and the terminal's title names the project.
    wait_until_shown(&ui, "Coordinator");
    // An escape sequence, not cells: its spaces are there as written.
    wait_until_shown(&ui, "\x1b]0;chief · demo\x07");
    let first = home.fake.pid(attach_call(&home, "c0").unwrap());

    db::set_pane_target(&conn, "demo", Some(THREAD)).unwrap();
    chief::pane::request_switch(&conn, "demo").unwrap();
    wait_until_shown(&ui, "thread-here");
    wait_for("the coordinator's attach reaped", || gone(first));
    assert_eq!(
        db::get_pane_showing(&conn, "demo").unwrap().as_deref(),
        Some(THREAD)
    );
    let second = home.fake.pid(attach_call(&home, "t1").unwrap());

    // Ctrl+\ hands the keys to the board; its q closes the layout.
    ui.send("\x1c");
    std::thread::sleep(Duration::from_millis(300));
    ui.send("q");
    let status = ui.exited().expect("q on the board closes the layout");
    assert!(status.success(), "{status:?}");
    assert!(gone(second), "its attach was reaped");
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
    assert!(
        home.fake.calls_of("stop").is_empty(),
        "sessions keep running"
    );
}

/// Ctrl+Q closes the layout while Claude has the keys, in one key, and leaves every session
/// running.
#[test]
fn ctrl_q_closes_the_layout_while_attached() {
    let home = project();
    let conn = home.db();
    let mut ui = Pane::spawn_sized(
        &home,
        &["up", "demo", "--layout", "builtin"],
        &[],
        (140, 40),
    );
    wait_until_shown(&ui, "coordinator-here");
    wait_until_shown(&ui, "^Q");
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());
    ui.send("\x11");
    let status = ui.exited().expect("Ctrl+Q closes the layout");
    assert!(status.success(), "{status:?}");
    assert!(gone(attach), "its attach was reaped");
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert!(
        home.fake.calls_of("stop").is_empty(),
        "sessions keep running"
    );
}

/// A hangup (the terminal window closing) ends the layout and its attach: nothing spins on.
#[test]
fn a_hangup_ends_the_layout() {
    let home = project();
    let conn = home.db();
    let mut ui = Pane::spawn_sized(
        &home,
        &["up", "demo", "--layout", "builtin"],
        &[],
        (140, 40),
    );
    wait_until_shown(&ui, "coordinator-here");
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());
    ui.hang_up();
    assert!(ui.exited().is_some(), "a hung-up layout ends");
    wait_for("its attach reaped", || gone(attach));
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
}

/// Forcing the built-in layout without a terminal fails before anything starts.
#[test]
fn the_builtin_layout_needs_a_terminal() {
    let home = project();
    let out = home.run(&["up", "demo", "--layout", "builtin"], "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs a terminal"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(home.fake.calls().is_empty(), "nothing ran");
}
