//! `chief pane` on a real pseudo-terminal with the fake `claude`, whose `attach` sleeps the way
//! a real attach stays attached. No real Claude session and no multiplexer.

mod common;

use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, agent};
use serde_json::json;

const COORD: &str = "chief-demo";
const THREAD: &str = "demo-a-1111";

/// `chief pane` running on a pty it leads (its own session, so its process group holds it and
/// every attach it starts). Dropping it kills that whole group and reaps the pane.
struct Pane {
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    input: std::fs::File,
}

impl Pane {
    fn start(home: &Home) -> Pane {
        Pane::start_with(home, &[])
    }

    fn start_with(home: &Home, extra: &[&str]) -> Pane {
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
        // SAFETY: fresh descriptors from openpty, owned once each.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut args = vec!["pane", "--project", "demo"];
        args.extend(extra);
        let mut cmd = home.command(&args);
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
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let mut master = std::fs::File::from(master);
        let input = master.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // Ends with EIO once the pane and its children have closed the pty.
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Pane {
            child,
            output,
            input,
        }
    }

    /// Types `keys` into the pane's terminal.
    fn send(&mut self, keys: &str) {
        use std::io::Write;
        self.input.write_all(keys.as_bytes()).unwrap();
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
