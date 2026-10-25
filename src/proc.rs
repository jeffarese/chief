//! Running child processes with a deadline.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Runs `cmd` with stdin closed and stdout/stderr captured. Returns None when it outlives
/// `timeout` (the child is killed). Pipes are drained on threads so a chatty child can't block
/// on a full pipe while we wait.
pub fn output_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // A grandchild that inherited the pipes could keep them open past the deadline; only join
    // the readers when the child itself exited.
    let Some(status) = status else {
        return Ok(None);
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(Some(Output {
        status,
        stdout,
        stderr,
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
}
