//! Running child processes with a deadline.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Runs `cmd` with stdin closed and stdout/stderr captured. Returns None when the child hasn't
/// exited *and* closed both pipes within `timeout`; its whole process group is then killed.
///
/// The child leads its own process group, so a grandchild that inherited the pipes (`sh -c
/// 'sleep 60 & exit 0'`) is killed with it instead of holding the call open. Pipes are drained
/// on threads that report over a channel, so a chatty child can't block on a full pipe and a
/// reader stuck on an open pipe is never joined.
pub fn output_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    // The child's pid is its process group id (process_group(0)).
    let group = child.id() as libc::pid_t;
    let kill_group = || {
        // SAFETY: a plain syscall. The id stays this group's while our unreaped child or any
        // process left in the group exists, which is when it is called.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    };
    let (tx, rx) = mpsc::channel();
    let streams: [(bool, Box<dyn Read + Send>); 2] = [
        (true, Box::new(child.stdout.take().expect("piped stdout"))),
        (false, Box::new(child.stderr.take().expect("piped stderr"))),
    ];
    for (is_stdout, mut pipe) in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send((is_stdout, buf));
        });
    }
    drop(tx);

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, buf)) => stdout = Some(buf),
            Ok((false, buf)) => stderr = Some(buf),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Something left in the group still holds a pipe.
                kill_group();
                return Ok(None);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(Some(Output {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output() {
        let out = output_with_timeout(
            Command::new("sh").args(["-c", "echo hi; echo oops >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"oops\n");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn large_output_does_not_block() {
        let out = output_with_timeout(
            Command::new("sh").args(["-c", "head -c 300000 /dev/zero"]),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[test]
    fn times_out() {
        let start = Instant::now();
        let out = output_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200))
            .unwrap();
        assert!(out.is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// Waits up to 2 s for `pid` to be gone (a killed orphan is reaped by init shortly after).
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            // SAFETY: signal 0 only checks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn read_pid(path: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "no pid in {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The child exits at once, but a grandchild keeps its pipes open: the timeout still bounds
    /// the call, and the grandchild is killed with the child's process group.
    #[test]
    fn a_grandchild_holding_the_pipes_does_not_outlive_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; exit 0", pidfile.display());
        let start = Instant::now();
        let out = output_with_timeout(
            Command::new("sh").args(["-c", &script]),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            out.is_none(),
            "output still pending at the deadline is a timeout"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(gone(read_pid(&pidfile)), "the grandchild was killed");
    }

    #[test]
    fn a_timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pidfile.display());
        let out = output_with_timeout(
            Command::new("sh").args(["-c", &script]),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(out.is_none());
        assert!(gone(read_pid(&pidfile)), "the grandchild was killed");
    }

    /// Output that a grandchild finishes writing before the deadline is kept.
    #[test]
    fn waits_for_output_within_the_deadline() {
        let out = output_with_timeout(
            Command::new("sh").args(["-c", "echo hi; (sleep 0.3; echo late) & exit 0"]),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.stdout, b"hi\nlate\n");
        assert!(out.status.success());
    }
}
