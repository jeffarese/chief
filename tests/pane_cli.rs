//! `chief pane` on a real pseudo-terminal with the fake `claude`, whose `attach` sleeps the way
//! a real attach stays attached. No real Claude session and no multiplexer.

mod common;

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, agent};
use serde_json::json;

const COORD: &str = "chief-demo";
const THREAD: &str = "demo-a-1111";

/// Held from `openpty` until the pane is spawned: until its master is close-on-exec, a process
/// another test spawns would inherit it and keep that pty from ever hanging up.
static SPAWNING: Mutex<()> = Mutex::new(());

/// `chief pane` running on a pty it leads (its own session, so its process group holds it and
/// every attach it starts). Dropping it kills that whole group and reaps the pane.
struct Pane {
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    /// The pty's master side; None once `hang_up` closed it.
    input: Option<std::fs::File>,
    /// Tells the reader to close its copy of the master.
    closing: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Pane {
    fn start(home: &Home) -> Pane {
        Pane::start_with(home, &[])
    }

    fn start_with(home: &Home, extra: &[&str]) -> Pane {
        let mut args = vec!["pane", "--project", "demo"];
        args.extend(extra);
        Pane::spawn(home, &args, &[])
    }

    /// `chief <args>` on a terminal of its own, with `env` added.
    fn spawn(home: &Home, args: &[&str], env: &[(&str, &str)]) -> Pane {
        let spawning = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty fills two descriptors we then own.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(r, 0, "openpty");
        // The pane and its attach must not hold the master, or closing it here hangs up nothing.
        // SAFETY: sets a flag on the descriptor just opened.
        assert_eq!(
            unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        // SAFETY: fresh descriptors from openpty, owned once each.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut cmd = home.command(args);
        cmd.envs(env.iter().copied());
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(cmd);
        drop(spawning);
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let closing = Arc::new(AtomicBool::new(false));
        let stop = closing.clone();
        let mut master = std::fs::File::from(master);
        let input = master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // Short polls, so `hang_up` can make it drop the master. Reads end with EIO once
            // the pane and its children have closed the pty.
            while !stop.load(Ordering::SeqCst) {
                let mut fd = libc::pollfd {
                    fd: master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one live pollfd.
                if unsafe { libc::poll(&mut fd, 1, 50) } <= 0 {
                    continue;
                }
                match master.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Pane {
            child,
            output,
            input: Some(input),
            closing,
            reader: Some(reader),
        }
    }

    /// Types `keys` into the pane's terminal.
    fn send(&mut self, keys: &str) {
        use std::io::Write;
        let input = self.input.as_mut().expect("not hung up");
        input.write_all(keys.as_bytes()).unwrap();
    }

    /// Closes the pty's master side, as a terminal window closing or a killed tmux server
    /// does: the pane's terminal hangs up.
    fn hang_up(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        self.input = None;
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }

    fn says(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.output()).contains(text)
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    /// Waits up to 5 s for the pane to exit.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: the group this test created; its leader is our unreaped child, so the id
            // can't belong to anyone else.
            unsafe { libc::kill(-self.pid(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

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

fn gone(pid: i32) -> bool {
    // SAFETY: probing a pid. A zombie still answers, so this is only true once it is reaped.
    unsafe { libc::kill(pid, 0) == -1 }
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
        .set("version.stdout", "2.1.310 (Claude Code)\n")
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
