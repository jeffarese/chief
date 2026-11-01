//! `chief pane` on a real pseudo-terminal with the fake `claude`, whose `attach` sleeps the way
//! a real attach stays attached, on the pty chief relays (whose echo shows the keys that reached
//! it). No real Claude session and no multiplexer.

mod common;

use std::time::{Duration, Instant};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::pty::{Pane, gone, wait_for};
use common::{Home, agent};
use serde_json::json;

const COORD: &str = "chief-demo";
const THREAD: &str = "demo-a-1111";

/// Index of the fake's call `attach <short>`, once it has happened.
fn attach_call(home: &Home, short: &str) -> Option<usize> {
    home.fake
        .calls()
        .iter()
        .position(|c| c.len() == 2 && c[0] == "attach" && c[1] == short)
}

fn attach_calls(home: &Home, short: &str) -> usize {
    home.fake
        .calls()
        .iter()
        .filter(|c| c.len() == 2 && c[0] == "attach" && c[1] == short)
        .count()
}

/// The board's switch: the pane SIGTERMs the running attach, reaps it, resets the terminal it
/// left behind and attaches to the new target, in that target's own account (§3: the thread
/// runs in another account than the project's). Then a SIGTERM to the pane ends it cleanly and
/// it forgets its record.
#[test]
fn a_switch_ends_attach_and_attaches_to_the_new_target() {
    let home = Home::new();
    let conn = home.db();
    home.register("demo");
    let cpb = common::account(home.path(), "claude-cpb");
    for (name, role, short) in [
        (COORD, Role::Coordinator, "c0"),
        (THREAD, Role::Thread, "t1"),
    ] {
        let mut row = SessionRow::new(name, "demo", role, 1);
        row.short_id = Some(short.into());
        row.session_id = Some(format!("{short}-sid"));
        if role == Role::Thread {
            common::in_account(&mut row, "cpb", "PB2", Some(&cpb));
        }
        db::insert_session(&conn, &row).unwrap();
    }
    home.fake
        .set(
            "agents.stdout",
            &json!([agent("c0", "c0-sid", COORD, "done", Some(4001))]).to_string(),
        )
        .set("attach.sleep", "30");
    common::account_agents(&cpb, &[agent("t1", "t1-sid", THREAD, "done", Some(4002))]);

    let mut pane = Pane::start(&home);
    wait_for("the coordinator's attach", || {
        attach_call(&home, "c0").is_some()
            && db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(COORD)
    });
    let (pid, _) = db::get_pane_process(&conn, "demo")
        .unwrap()
        .expect("recorded");
    assert_eq!(pid, pane.pid());
    let first = home.fake.pid(attach_call(&home, "c0").unwrap());
    assert!(!gone(first), "attached");
    let before = pane.output().len();

    db::set_pane_target(&conn, "demo", Some(THREAD)).unwrap();
    chief::pane::request_switch(&conn, "demo").unwrap();
    wait_for("the thread's attach", || attach_call(&home, "t1").is_some());
    assert!(
        gone(first),
        "the coordinator's attach was ended and reaped first"
    );
    wait_for("pane_showing", || {
        db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(THREAD)
    });
    let after = pane.output()[before..].to_vec();
    let reset = b"\x1b[?1049l\x1b[?25h";
    assert!(
        after.windows(reset.len()).any(|w| w == reset),
        "terminal reset written: {:?}",
        String::from_utf8_lossy(&after)
    );
    // The coordinator's session was left alone: only attach was ended.
    assert!(home.fake.calls_of("stop").is_empty());
    let accounts = home.fake.accounts();
    assert_eq!(accounts[attach_call(&home, "c0").unwrap()].1, "unset");
    assert_eq!(
        accounts[attach_call(&home, "t1").unwrap()].1,
        cpb.to_string_lossy(),
        "attached in the thread's account"
    );
    assert!(
        home.fake.calls_of("respawn").is_empty(),
        "alive in its own account"
    );

    let second = home.fake.pid(attach_call(&home, "t1").unwrap());
    // SAFETY: our own child.
    unsafe { libc::kill(pane.pid(), libc::SIGTERM) };
    let status = pane.exited().expect("the pane exits on SIGTERM");
    assert!(status.success(), "{status:?}");
    assert!(gone(second), "its attach was reaped");
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
}

/// Leaving Claude by hand (its Ctrl+Z, or quitting its agent view) doesn't put the user straight
/// back in: the pane waits for ⏎ or a board selection, and `q` in chief's layout closes it.
#[test]
fn leaving_waits_for_the_user_and_q_closes_the_layout() {
    let home = Home::new();
    let conn = home.db();
    home.register("demo");
    for (name, role, short) in [
        (COORD, Role::Coordinator, "c0"),
        (THREAD, Role::Thread, "t1"),
    ] {
        let mut row = SessionRow::new(name, "demo", role, 1);
        row.short_id = Some(short.into());
        row.session_id = Some(format!("{short}-sid"));
        db::insert_session(&conn, &row).unwrap();
    }
    db::set_rex_session(&conn, "demo", Some("session:7")).unwrap();
    home.fake
        .set(
            "agents.stdout",
            &json!([
                agent("c0", "c0-sid", COORD, "done", Some(4001)),
                agent("t1", "t1-sid", THREAD, "done", Some(4002)),
            ])
            .to_string(),
        )
        // Each attach ends on its own after a while, as when the user leaves Claude.
        .set("attach.sleep", "1.2");
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:7", "label": "chief-demo"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        );

    let mut pane = Pane::start_with(&home, &["--layout"]);
    wait_for("the coordinator is left", || {
        pane.says("you left the coordinator (chief-demo)")
    });
    assert!(pane.says("pick a thread on the board"));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(attach_calls(&home, "c0"), 1, "not attached again by itself");
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);

    pane.send("\r");
    wait_for("attached again", || attach_calls(&home, "c0") == 2);

    // A board selection while the pane waits attaches it.
    wait_for("left again", || {
        db::get_pane_showing(&conn, "demo").unwrap().is_none()
            && String::from_utf8_lossy(&pane.output())
                .matches("you left the coordinator")
                .count()
                == 2
    });
    db::set_pane_target(&conn, "demo", Some(THREAD)).unwrap();
    chief::pane::request_switch(&conn, "demo").unwrap();
    wait_for("the thread's attach", || attach_calls(&home, "t1") == 1);
    wait_for("the thread is left", || {
        pane.says("you left thread demo-a-1111")
    });

    assert!(home.rex.calls_of("kill").is_empty());
    std::thread::sleep(Duration::from_millis(400));
    pane.send("q");
    let status = pane.exited().expect("q quits");
    assert!(status.success(), "{status:?}");
    assert_eq!(
        home.rex.calls_of("kill"),
        [vec!["kill", "session:7", "--autostart=false"]],
        "the layout closed"
    );
    assert_eq!(db::get_rex_session(&conn, "demo").unwrap(), None);
    assert!(home.fake.calls_of("stop").is_empty(), "no session stopped");
}

/// `chief up` inside the user's own Rex block: the board opens beside that block, the
/// conversation shows in the same terminal, and quitting closes only the board.
#[test]
fn up_in_a_rex_block_shows_the_conversation_there() {
    let home = Home::new();
    let conn = home.db();
    home.register("demo");
    let mut row = SessionRow::new(COORD, "demo", Role::Coordinator, 1);
    row.short_id = Some("c0".into());
    row.session_id = Some("c0-sid".into());
    db::insert_session(&conn, &row).unwrap();
    home.fake
        .set(
            "agents.stdout",
            &json!([agent("c0", "c0-sid", COORD, "done", Some(4001))]).to_string(),
        )
        .set("version.stdout", "2.1.316 (Claude Code)\n")
        .set("attach.sleep", "1.2");
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:mine", "label": "work"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:shell", "label": "shell"}]}"#,
        )
        .set("run.stdout", r#"{"block_ids": ["block:board"]}"#);

    let mut pane = Pane::spawn(
        &home,
        &["up", "demo"],
        &[
            ("REX_SESSION", "session:mine"),
            ("REX_BLOCK", "block:shell"),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while attach_call(&home, "c0").is_none() {
        if Instant::now() > deadline || pane.child.try_wait().unwrap().is_some() {
            panic!(
                "no attach; output: {}\nclaude calls: {:?}\nrex calls: {:?}",
                String::from_utf8_lossy(&pane.output()),
                home.fake.calls(),
                home.rex.calls()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let runs = home.rex.calls_of("run");
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = runs[0].join(" ");
    assert!(
        run.contains(
            "-s session:mine --label chief-demo board --focus=false -b block:shell --split right"
        ),
        "{run}"
    );
    assert!(run.ends_with("board --project demo"), "{run}");
    assert!(home.rex.calls_of("new").is_empty(), "no session of its own");
    // Rex shows the Claude icon for a process named claude: `up` became that pane, still
    // knowing the board it closes.
    let shown = foreground_of(pane.pid());
    assert_eq!(
        args_of(shown),
        "claude pane --project demo --board block:board"
    );

    wait_for("left", || pane.says("close chief and its board"));
    std::thread::sleep(Duration::from_millis(400));
    pane.send("q");
    let status = pane.exited().expect("q quits");
    assert!(status.success(), "{status:?}");
    let closes: Vec<_> = home
        .rex
        .calls_of("block")
        .into_iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("close"))
        .collect();
    assert_eq!(
        closes,
        [vec!["block", "close", "block:board", "--autostart=false"]],
        "only the board closed"
    );
    assert!(home.rex.calls_of("kill").is_empty());
}

/// Ctrl+Q while Claude has the keys quits at once, in the user's own Rex block: attach is ended
/// (its session keeps running), the screen modes it left are undone, and only the board beside
/// the block closes. Before, the way out was Claude's own: Ctrl+C twice, twice more in its agent
/// view, then `q`. Other keys reach attach untouched.
#[test]
fn ctrl_q_quits_while_claude_has_the_keys() {
    let home = coordinator_home(&[
        ("attach.sleep", "30"),
        ("version.stdout", "2.1.316 (Claude Code)\n"),
    ]);
    let conn = home.db();
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:mine", "label": "work"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:shell", "label": "shell"}]}"#,
        )
        .set("run.stdout", r#"{"block_ids": ["block:board"]}"#);
    let mut pane = Pane::spawn(
        &home,
        &["up", "demo"],
        &[
            ("REX_SESSION", "session:mine"),
            ("REX_BLOCK", "block:shell"),
        ],
    );
    wait_for("the attach", || {
        attach_call(&home, "c0").is_some()
            && db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(COORD)
    });
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());
    // attach's terminal echoes what reaches it.
    pane.send("hello");
    wait_for("the echo", || pane.says("hello"));
    let before = pane.output().len();

    pane.send("\x11");
    let status = pane.exited().expect("Ctrl+Q quits");
    assert!(status.success(), "{status:?}");
    assert!(gone(attach), "its attach was reaped");
    let after = pane.output()[before..].to_vec();
    let reset = b"\x1b[?1049l\x1b[?25h";
    assert!(
        after.windows(reset.len()).any(|w| w == reset),
        "terminal reset written: {:?}",
        String::from_utf8_lossy(&after)
    );
    let closes: Vec<_> = home
        .rex
        .calls_of("block")
        .into_iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("close"))
        .collect();
    assert_eq!(
        closes,
        [vec!["block", "close", "block:board", "--autostart=false"]],
        "only the board closed"
    );
    assert!(
        home.fake.calls_of("stop").is_empty(),
        "sessions keep running"
    );
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
}

/// The board's Ctrl+Q (`pane::request_quit`) quits the pane as its own does, closing the board.
#[test]
fn the_boards_ctrl_q_quits_the_pane() {
    let home = coordinator_home(&[
        ("attach.sleep", "30"),
        ("version.stdout", "2.1.316 (Claude Code)\n"),
    ]);
    let conn = home.db();
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:mine", "label": "work"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:shell", "label": "shell"}]}"#,
        )
        .set("run.stdout", r#"{"block_ids": ["block:board"]}"#);
    let mut pane = Pane::spawn(
        &home,
        &["up", "demo"],
        &[
            ("REX_SESSION", "session:mine"),
            ("REX_BLOCK", "block:shell"),
        ],
    );
    wait_for("the attach", || {
        attach_call(&home, "c0").is_some()
            && db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(COORD)
    });
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());

    // The pane is this test's child: it stays a zombie until `exited` reaps it, so the wait
    // can't see it end.
    chief::pane::request_quit(&conn, "demo", Duration::from_millis(100)).unwrap();
    let status = pane.exited().expect("the board's Ctrl+Q quits");
    assert!(status.success(), "{status:?}");
    assert!(gone(attach), "its attach was reaped");
    let closes: Vec<_> = home
        .rex
        .calls_of("block")
        .into_iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("close"))
        .collect();
    assert_eq!(
        closes,
        [vec!["block", "close", "block:board", "--autostart=false"]]
    );
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
}

/// A Rex session whose blocks are chief's `main` and `board`, stored as the project's layout.
fn rex_layout(home: &Home) {
    db::set_rex_session(&home.db(), "demo", Some("session:7")).unwrap();
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:7", "label": "chief-demo"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        );
}

/// Once Claude turns on the kitty keyboard protocol, the terminal sends Ctrl+Q as `CSI 113;5u`:
/// that quits too, and closes chief's layout as `q` does, while other keys in the same encoding
/// (Ctrl+C here) pass to attach.
#[test]
fn ctrl_q_in_the_kitty_protocol_closes_the_layout() {
    let home = coordinator_home(&[("attach.sleep", "30")]);
    rex_layout(&home);
    let mut pane = Pane::start_with(&home, &["--layout"]);
    wait_for("the attach", || attach_call(&home, "c0").is_some());
    pane.send("\x1b[99;5u");
    wait_for("Ctrl+C passed on", || pane.says("[99;5u"));
    assert!(home.rex.calls().is_empty(), "nothing closed yet");

    pane.send("\x1b[113;5u");
    let status = pane.exited().expect("Ctrl+Q quits");
    assert!(status.success(), "{status:?}");
    assert_eq!(
        home.rex.calls_of("kill"),
        [vec!["kill", "session:7", "--autostart=false"]],
        "the layout closed"
    );
    assert!(
        home.fake.calls_of("stop").is_empty(),
        "sessions keep running"
    );
}

/// Ctrl+Q also quits from chief's own screens: here the wait after the user left Claude.
#[test]
fn ctrl_q_quits_after_leaving_claude() {
    let home = coordinator_home(&[("attach.sleep", "1.2")]);
    rex_layout(&home);
    let mut pane = Pane::start_with(&home, &["--layout"]);
    wait_for("left", || pane.says("you left the coordinator"));
    std::thread::sleep(Duration::from_millis(400));
    pane.send("\x11");
    let status = pane.exited().expect("Ctrl+Q quits");
    assert!(status.success(), "{status:?}");
    assert_eq!(home.rex.calls_of("kill").len(), 1, "the layout closed");
}

/// attach's terminal starts at the pane's size and follows it when the window is resized.
#[test]
fn attachs_terminal_follows_the_panes_size() {
    let home = coordinator_home(&[(
        "attach.sh",
        "stty size; trap 'stty size' WINCH; while :; do sleep 0.05; done\n",
    )]);
    let mut pane = Pane::spawn_sized(&home, &["pane", "--project", "demo"], &[], (100, 30));
    wait_for("the starting size", || pane.says("30 100"));
    pane.resize(120, 40);
    wait_for("the new size", || pane.says("40 120"));
    // SAFETY: our own child.
    unsafe { libc::kill(pane.pid(), libc::SIGTERM) };
    assert!(pane.exited().expect("ended").success());
}

/// A home whose coordinator runs (a pid in `claude agents`), with the fake's files `fake` set.
fn coordinator_home(fake: &[(&str, &str)]) -> Home {
    let home = Home::new();
    home.register("demo");
    let mut row = SessionRow::new(COORD, "demo", Role::Coordinator, 1);
    row.short_id = Some("c0".into());
    row.session_id = Some("c0-sid".into());
    db::insert_session(&home.db(), &row).unwrap();
    home.fake.set(
        "agents.stdout",
        &json!([agent("c0", "c0-sid", COORD, "done", Some(4001))]).to_string(),
    );
    for (file, text) in fake {
        home.fake.set(file, text);
    }
    home
}

/// The two waits the pane reads keys in: after the user left a session, and before a retry
/// (here of an attach that keeps failing, as one did live with exit 126).
const WAITS: [(&str, &str, &str); 2] = [
    ("attach.sleep", "1.2", "you left the coordinator"),
    ("attach.exit", "126", "· retrying · q quits"),
];

/// A terminal that hangs up while the pane waits for a key (its window closed, its tmux server
/// killed) ends the pane: before, the key reader retried its read of the hung-up terminal
/// forever, at full CPU, and SIGTERM couldn't stop it. A hangup isn't a `q`: the layout stays
/// for `chief down`.
#[test]
fn a_hangup_ends_a_waiting_pane() {
    for (file, text, waiting) in WAITS {
        let home = coordinator_home(&[(file, text)]);
        let conn = home.db();
        db::set_rex_session(&conn, "demo", Some("session:7")).unwrap();
        let mut pane = Pane::start_with(&home, &["--layout"]);
        wait_for(waiting, || pane.says(waiting));
        assert!(db::get_pane_process(&conn, "demo").unwrap().is_some());
        pane.hang_up();
        assert!(pane.exited().is_some(), "{waiting}: the pane ends");
        assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
        assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
        assert!(home.rex.calls().is_empty(), "{waiting}: the layout stays");
    }
}

/// A hangup while attached (Rex closing the block) ends attach and the pane, but no layout.
#[test]
fn a_hangup_while_attached_reaps_attach() {
    let home = coordinator_home(&[("attach.sleep", "30")]);
    let conn = home.db();
    db::set_rex_session(&conn, "demo", Some("session:7")).unwrap();
    let mut pane = Pane::start_with(&home, &["--layout"]);
    wait_for("the attach", || {
        attach_call(&home, "c0").is_some()
            && db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(COORD)
    });
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());
    pane.hang_up();
    assert!(pane.exited().is_some(), "the pane ends");
    wait_for("its attach reaped", || gone(attach));
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
    assert!(home.rex.calls().is_empty(), "the layout stays");
}

/// SIGTERM (`kill <pid>`, `chief down`) ends a pane waiting for a key, as it ends one attached.
#[test]
fn sigterm_ends_a_waiting_pane() {
    for (file, text, waiting) in WAITS {
        let home = coordinator_home(&[(file, text)]);
        let conn = home.db();
        let mut pane = Pane::start(&home);
        wait_for(waiting, || pane.says(waiting));
        // SAFETY: our own child.
        unsafe { libc::kill(pane.pid(), libc::SIGTERM) };
        let status = pane.exited().expect("the pane ends on SIGTERM");
        assert!(status.success(), "{waiting}: {status:?}");
        assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
        assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
    }
}

/// `q` in chief's layout closes it, and closing it hangs up the pane's own terminal while the
/// pane still reports what it closed: that line is lost, and the pane still ends cleanly
/// (`println!` panicked there, exit 101).
#[test]
fn closing_the_layout_survives_its_own_hangup() {
    let home = coordinator_home(&[("attach.sleep", "1.2")]);
    let conn = home.db();
    db::set_rex_session(&conn, "demo", Some("session:7")).unwrap();
    home.rex
        .set("list.exit", "0")
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:7", "label": "chief-demo"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        )
        // The session's end hangs up the pane's terminal before `rex kill` returns.
        .set("kill.sleep", "1");
    let mut pane = Pane::start_with(&home, &["--layout"]);
    wait_for("the coordinator is left", || {
        pane.says("you left the coordinator")
    });
    std::thread::sleep(Duration::from_millis(400));
    pane.send("q");
    wait_for("rex kill", || !home.rex.calls_of("kill").is_empty());
    pane.hang_up();
    let status = pane.exited().expect("the pane ends");
    assert!(status.success(), "{status:?}");
    assert_eq!(db::get_rex_session(&conn, "demo").unwrap(), None);
}

/// `chief down` ends a pane it didn't open (run by hand here, or in the user's own Rex block)
/// before it stops the coordinator: the pane would otherwise attach again and respawn it.
#[test]
fn down_ends_a_pane_it_did_not_open() {
    let home = coordinator_home(&[("attach.sleep", "30")]);
    let conn = home.db();
    let mut pane = Pane::start(&home);
    wait_for("the attach", || {
        attach_call(&home, "c0").is_some()
            && db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(COORD)
    });
    let attach = home.fake.pid(attach_call(&home, "c0").unwrap());
    let out = home.run(&["down", "demo"], "");
    let text = common::stdout(&out);
    assert!(out.status.success(), "{text}{}", common::stderr(&out));
    assert!(
        text.starts_with(&format!("Ended chief pane (pid {}).\n", pane.pid())),
        "{text}"
    );
    assert!(text.contains("Stopped coordinator chief-demo."), "{text}");
    assert!(pane.exited().expect("ended").success());
    assert!(gone(attach), "its attach was reaped");
    assert_eq!(attach_calls(&home, "c0"), 1, "not attached again");
    assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);
    assert_eq!(db::get_pane_showing(&conn, "demo").unwrap(), None);
}

/// `pane_showing` belongs to the pane recorded last: an older pane still running neither
/// overwrites it nor clears it when it ends, so cleanup never takes the newest pane's thread
/// for one nobody shows.
#[test]
fn an_older_pane_leaves_the_newest_ones_showing_alone() {
    let home = coordinator_home(&[("attach.sleep", "30")]);
    let conn = home.db();
    let mut row = SessionRow::new(THREAD, "demo", Role::Thread, 1);
    row.short_id = Some("t1".into());
    row.session_id = Some("t1-sid".into());
    db::insert_session(&conn, &row).unwrap();
    home.fake.set(
        "agents.stdout",
        &json!([
            agent("c0", "c0-sid", COORD, "done", Some(4001)),
            agent("t1", "t1-sid", THREAD, "done", Some(4002)),
        ])
        .to_string(),
    );
    let recorded = |pane: &Pane| {
        db::get_pane_process(&conn, "demo")
            .unwrap()
            .map(|(pid, _)| pid)
            == Some(pane.pid())
    };
    let mut older = Pane::start(&home);
    wait_for("the older pane's attach", || attach_calls(&home, "c0") == 1);
    let newer = Pane::start(&home);
    wait_for("the newer pane's record", || recorded(&newer));
    wait_for("the newer pane's attach", || attach_calls(&home, "c0") == 2);

    db::set_pane_target(&conn, "demo", Some(THREAD)).unwrap();
    chief::pane::request_switch(&conn, "demo").unwrap();
    wait_for("the newer pane on the thread", || {
        db::get_pane_showing(&conn, "demo").unwrap().as_deref() == Some(THREAD)
    });
    // SAFETY: our own child.
    unsafe { libc::kill(older.pid(), libc::SIGTERM) };
    assert!(older.exited().is_some(), "the older pane ends");
    assert_eq!(
        db::get_pane_showing(&conn, "demo").unwrap().as_deref(),
        Some(THREAD)
    );
    assert!(recorded(&newer));
}

/// In Rex the pane reports the shown session's state as Claude Code would (OSC 7501,
/// `app=claude-code`), again when it changes, never inside a sequence of Claude's, and clears
/// it when it shows the session no more. Elsewhere it reports nothing.
#[test]
fn in_rex_the_pane_reports_the_sessions_state() {
    // Stops halfway through a colour, then finishes it.
    let attach =
        "printf 'before \\033[38;2;1'; sleep 2; printf 'm after'; while :; do sleep 0.05; done\n";
    let home = coordinator_home(&[("attach.sh", attach)]);
    let conn = home.db();
    let mut pane = Pane::spawn(
        &home,
        &["pane", "--project", "demo"],
        &[("REX_BLOCK", "block:test")],
    );
    // The listing says its turn is done.
    let idle = "\x1b]7501;state=idle:app=claude-code:title=Q29vcmRpbmF0b3I=\x1b\\";
    wait_for("the first report", || pane.says(idle));
    wait_for("the cut colour", || pane.says("before \x1b[38;2;1"));
    // A new process, which Rex sees in the block's foreground, not chief renamed in place.
    let shown = foreground_of(pane.pid());
    assert_ne!(shown, pane.pid());
    assert_eq!(args_of(shown), "claude pane --project demo");
    let mut row = db::get_session(&conn, COORD).unwrap().unwrap();
    row.group_name = "waiting".into();
    row.waiting_reason = Some("question".into());
    db::update_session(&conn, &row).unwrap();
    let blocked =
        "\x1b]7501;state=blocked:app=claude-code:kind=question:title=Q29vcmRpbmF0b3I=\x1b\\";
    wait_for("the second report", || pane.says(blocked));
    let out = String::from_utf8_lossy(&pane.output()).into_owned();
    assert!(
        out.contains("before \x1b[38;2;1m after"),
        "the colour stayed whole: {out:?}"
    );
    assert!(out.find(blocked) > out.find("m after"), "{out:?}");
    // Its turn ends with a message while the pane shows it: done, as Claude Code reports it.
    row.group_name = "idle".into();
    row.waiting_reason = None;
    row.last_report = Some("The limit is 100 a minute now.".into());
    row.last_report_at = Some(db::now_ms());
    db::update_session(&conn, &row).unwrap();
    wait_for("done", || {
        pane.says("\x1b]7501;state=done:app=claude-code:title=Q29vcmRpbmF0b3I=\x1b\\")
    });

    // SAFETY: our own child.
    unsafe { libc::kill(pane.pid(), libc::SIGTERM) };
    assert!(pane.exited().expect("ended").success());
    let out = String::from_utf8_lossy(&pane.output()).into_owned();
    assert!(
        out.contains("\x1b]7501;state=clear\x1b\\"),
        "cleared: {out:?}"
    );

    let home = coordinator_home(&[("attach.sh", attach)]);
    let mut pane = Pane::start(&home);
    wait_for("attach", || pane.says("m after"));
    assert!(
        foreground_of(pane.pid()) == pane.pid()
            && args_of(pane.pid()).ends_with("chief pane --project demo"),
        "keeps its name outside Rex: {}",
        args_of(pane.pid())
    );
    // SAFETY: our own child.
    unsafe { libc::kill(pane.pid(), libc::SIGTERM) };
    assert!(pane.exited().expect("ended").success());
    assert!(!pane.says("7501"), "no report outside Rex");
}

/// A process's arguments as `ps` shows them, argv0 first.
fn args_of(pid: i32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The process group in the foreground of `pid`'s terminal, as Rex finds a block's program.
fn foreground_of(pid: i32) -> i32 {
    let out = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}
