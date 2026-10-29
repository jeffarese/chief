//! chief on a real pseudo-terminal it leads, read and typed into like a terminal window.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Home;

/// Held from `openpty` until the pane is spawned: until its master is close-on-exec, a process
/// another test spawns would inherit it and keep that pty from ever hanging up.
static SPAWNING: Mutex<()> = Mutex::new(());

/// `chief pane` running on a pty it leads (its own session, so its process group holds it and
/// every attach it starts). Dropping it kills that whole group and reaps the pane.
pub struct Pane {
    pub child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    /// The pty's master side; None once `hang_up` closed it.
    input: Option<std::fs::File>,
    /// Tells the reader to close its copy of the master.
    closing: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Pane {
    pub fn start(home: &Home) -> Pane {
        Pane::start_with(home, &[])
    }

    pub fn start_with(home: &Home, extra: &[&str]) -> Pane {
        let mut args = vec!["pane", "--project", "demo"];
        args.extend(extra);
        Pane::spawn(home, &args, &[])
    }

    /// `chief <args>` on a terminal of its own, with `env` added.
    pub fn spawn(home: &Home, args: &[&str], env: &[(&str, &str)]) -> Pane {
        Pane::spawn_sized(home, args, env, (80, 24))
    }

    /// `spawn` on a terminal of `cols` × `rows`.
    pub fn spawn_sized(
        home: &Home,
        args: &[&str],
        env: &[(&str, &str)],
        (cols, rows): (u16, u16),
    ) -> Pane {
        let spawning = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
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
    pub fn send(&mut self, keys: &str) {
        use std::io::Write;
        let input = self.input.as_mut().expect("not hung up");
        input.write_all(keys.as_bytes()).unwrap();
    }

    /// Closes the pty's master side, as a terminal window closing or a killed tmux server
    /// does: the pane's terminal hangs up.
    pub fn hang_up(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        self.input = None;
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }

    pub fn says(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.output()).contains(text)
    }

    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub fn output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    /// Waits up to 5 s for the pane to exit.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
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

pub fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn gone(pid: i32) -> bool {
    // SAFETY: probing a pid. A zombie still answers, so this is only true once it is reaped.
    unsafe { libc::kill(pid, 0) == -1 }
}
