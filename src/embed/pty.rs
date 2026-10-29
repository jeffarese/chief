//! A child program on its own pseudo-terminal: emulated (`spawn`, the built-in layout, whose
//! reader thread only moves bytes to the UI thread's emulator), or relayed to the user's terminal
//! (`open`, the pane: the caller polls and reads it).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// How long the child gets to exit after SIGTERM before it is killed.
const GRACE: Duration = Duration::from_secs(2);

/// What the reader thread reports: output, then `Exited` once the child side closes.
pub enum Output {
    Bytes(Vec<u8>),
    Exited,
}

pub struct Pty {
    master: OwnedFd,
    writer: File,
    child: Child,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    /// Starts `cmd` for an emulator, with the pty as its controlling terminal. `wrap` turns each
    /// read into the caller's event type, so one channel carries the pty, the outer terminal's
    /// input and the workers, and output from an earlier child can be told apart.
    pub fn spawn<E: Send + 'static>(
        mut cmd: Command,
        cols: u16,
        rows: u16,
        tx: Sender<E>,
        wrap: impl Fn(Output) -> E + Send + 'static,
    ) -> io::Result<Pty> {
        // The child talks to this emulator, not to the outer terminal.
        cmd.env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("TERM_PROGRAM")
            .env_remove("TERM_PROGRAM_VERSION");
        let pty = Pty::start(cmd, cols, rows)?;
        let mut reader = File::from(pty.master.try_clone()?);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(wrap(Output::Bytes(buf[..n].to_vec()))).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // EIO once the child side is closed.
                    Err(_) => break,
                }
            }
            let _ = tx.send(wrap(Output::Exited));
        });
        Ok(pty)
    }

    /// Starts `cmd` for a relay: it keeps this process's `TERM`, since its bytes go to the
    /// user's terminal as they are, and the master is non-blocking for the caller's `poll`,
    /// `read` and `write_some`.
    pub fn open(cmd: Command, cols: u16, rows: u16) -> io::Result<Pty> {
        let pty = Pty::start(cmd, cols, rows)?;
        let fd = pty.master.as_raw_fd();
        // SAFETY: plain fcntl on our own fd; the flag is shared by its duplicate, the writer.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Ok(pty)
    }

    fn start(mut cmd: Command, cols: u16, rows: u16) -> io::Result<Pty> {
        let (mut m, mut s) = (0, 0);
        let mut ws = winsize(cols, rows);
        // SAFETY: out-pointers to locals; a null name and termios are allowed.
        let r = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty just returned these fds, owned by nobody else.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        // SAFETY: plain fcntl on our own fd: the child must not inherit the master side.
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                for sig in [libc::SIGTSTP, libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        Ok(Pty {
            writer: File::from(master.try_clone()?),
            master,
            child,
        })
    }

    /// A second handle on the master, for the emulator's replies to the child's queries.
    pub fn replies(&self) -> io::Result<File> {
        self.writer.try_clone()
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    /// The master, for `poll`.
    pub fn fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// The child's output; on an `open` pty, WouldBlock when there is none yet.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.writer).read(buf)
    }

    /// As much of `bytes` as the pty takes now (an `open` pty never blocks).
    pub fn write_some(&self, bytes: &[u8]) -> io::Result<usize> {
        (&self.writer).write(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ on our own master fd; the kernel signals the child's group.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    /// SIGTERM, a grace period, then SIGKILL, reaping it: never waits unbounded. The child leads
    /// its own session, so its group is signalled: whatever it started goes with it.
    pub fn end(mut self) {
        let group = -(self.child.id() as libc::pid_t);
        if let Ok(None) = self.child.try_wait() {
            // SAFETY: our own child's group; the child is not yet reaped, so the id is still ours.
            unsafe { libc::kill(group, libc::SIGTERM) };
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // SAFETY: as above; once the leader is reaped a group of that id can only be what it left.
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = self.child.wait();
    }

    /// The child's exit status once it has ended, reaping it.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn output(rx: &mpsc::Receiver<Output>) -> String {
        let mut out = Vec::new();
        while let Ok(o) = rx.recv_timeout(Duration::from_secs(5)) {
            match o {
                Output::Bytes(b) => out.extend(b),
                Output::Exited => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn the_child_gets_its_own_terminal_of_the_given_size() {
        let (tx, rx) = mpsc::channel();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "stty size; echo \"$TERM ${TERM_PROGRAM-none}\""]);
        let pty = Pty::spawn(cmd, 33, 7, tx, |o| o).unwrap();
        let out = output(&rx);
        assert!(out.contains("7 33"), "{out:?}");
        assert!(out.contains("xterm-256color none"), "{out:?}");
        pty.end();
    }

    /// A relayed child keeps the user's terminal type: its bytes go to that terminal.
    #[test]
    fn an_open_pty_keeps_term_and_is_read_by_the_caller() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "stty size; echo \"$TERM $TERM_PROGRAM\""])
            .env("TERM", "xterm-ghostty")
            .env("TERM_PROGRAM", "rex");
        let pty = Pty::open(cmd, 41, 9).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match pty.read(&mut buf) {
                Ok(0) | Err(_) if out.ends_with(b"rex\r\n") => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("9 41"), "{out:?}");
        assert!(out.contains("xterm-ghostty rex"), "{out:?}");
        pty.end();
    }

    #[test]
    fn a_child_that_ignores_sigterm_is_killed_after_the_grace() {
        let (tx, rx) = mpsc::channel();
        let mut cmd = Command::new("sh");
        // The ignored SIGTERM is inherited: neither the shell nor its sleep goes on SIGTERM.
        cmd.args(["-c", "trap '' TERM; sleep 30 & echo ready; wait"]);
        let pty = Pty::spawn(cmd, 20, 4, tx, |o| o).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Output::Bytes(b)) => assert!(String::from_utf8_lossy(&b).contains("ready")),
            _ => panic!("no output"),
        }
        let group = -(pty.child.id() as libc::pid_t);
        let started = Instant::now();
        pty.end();
        assert!(started.elapsed() < Duration::from_secs(4));
        // SAFETY: signal 0 only checks whether the group still exists.
        assert_eq!(
            unsafe { libc::kill(group, 0) },
            -1,
            "the sleep outlived its shell"
        );
    }
}
