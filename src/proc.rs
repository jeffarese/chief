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
    run_bounded(cmd, timeout, &|group| {
        // SAFETY: a plain syscall; `run_bounded` calls it only while the group's leader is
        // unreaped, so the id is still this command's group.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    })
}

/// `output_with_timeout` with the group kill passed in (tests watch when it runs). Pipes are
/// awaited before the child is reaped: an unreaped child, even a zombie, keeps its pid, which is
/// the group's id, so `kill_group` can only reach this command's processes. Once `try_wait` has
/// reaped it, the id may be reused, and the group is never signalled after that.
fn run_bounded(
    cmd: &mut Command,
    timeout: Duration,
    kill_group: &dyn Fn(libc::pid_t),
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

    // Both pipes closed first, while the child is unreaped (exited or not).
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, buf)) => stdout = Some(buf),
            Ok((false, buf)) => stderr = Some(buf),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The child, or something left in its group, still holds a pipe.
                kill_group(group);
                let _ = child.wait();
                return Ok(None);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Then its exit. `try_wait` reaps it, so the group is signalled only before that.
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group(group);
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
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

    /// The group is signalled only while its leader, the child, is still unreaped: once reaped,
    /// its pid (the group's id) may belong to an unrelated process group.
    #[test]
    fn the_group_is_signalled_only_before_the_child_is_reaped() {
        let leader_there = std::sync::Mutex::new(Vec::new());
        let kill = |group: libc::pid_t| {
            // SAFETY: signal 0 only checks that the pid exists (an unreaped zombie still does).
            leader_there
                .lock()
                .unwrap()
                .push(unsafe { libc::kill(group, 0) } == 0);
            // SAFETY: the group of the command this test started.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        };
        // The child exits at once; a grandchild holds the pipes past the deadline.
        let out = run_bounded(
            Command::new("sh").args(["-c", "sleep 30 & exit 0"]),
            Duration::from_millis(500),
            &kill,
        )
        .unwrap();
        assert!(out.is_none());
        // And a child that outlives the deadline itself.
        let out = run_bounded(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            Duration::from_millis(300),
            &kill,
        )
        .unwrap();
        assert!(out.is_none());
        assert_eq!(*leader_there.lock().unwrap(), [true, true]);
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
