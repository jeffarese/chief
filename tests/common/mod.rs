//! Shared test helpers: the fake `claude` and a temp chief home. Nothing here starts a real
//! Claude session.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chief::claude::Claude;

/// A temp dir with a `claude` link to the fake script; canned outputs go beside it.
pub struct Fake {
    pub dir: tempfile::TempDir,
}

impl Fake {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh");
        std::os::unix::fs::symlink(script, dir.path().join("claude")).unwrap();
        Fake { dir }
    }

    pub fn bin(&self) -> PathBuf {
        self.dir.path().join("claude")
    }

    pub fn claude(&self) -> Claude {
        Claude { bin: self.bin() }
    }

    pub fn set(&self, file: &str, text: &str) -> &Self {
        std::fs::write(self.dir.path().join(file), text).unwrap();
        self
    }

    pub fn unset(&self, file: &str) -> &Self {
        let _ = std::fs::remove_file(self.dir.path().join(file));
        self
    }

    /// Arguments of every call so far.
    pub fn calls(&self) -> Vec<Vec<String>> {
        (0..)
            .map(|n| self.dir.path().join(format!("calls/{n}")))
            .take_while(|p| p.exists())
            .map(|p| {
                let raw = std::fs::read(p).unwrap();
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8(a.to_vec()).unwrap())
                    .collect()
            })
            .collect()
    }

    /// Calls whose first argument is `first`.
    pub fn calls_of(&self, first: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.first().map(String::as_str) == Some(first))
            .collect()
    }

    pub fn cwd(&self, n: usize) -> PathBuf {
        let text = std::fs::read_to_string(self.dir.path().join(format!("calls/{n}.cwd"))).unwrap();
        PathBuf::from(text.trim())
    }
}

/// `~/.claude.json` stand-in that trusts `folders` (canonical paths).
pub fn trust_file(path: &Path, folders: &[&Path]) {
    let mut projects = serde_json::Map::new();
    for f in folders {
        let key = f.canonicalize().unwrap().to_string_lossy().into_owned();
        projects.insert(key, serde_json::json!({"hasTrustDialogAccepted": true}));
    }
    std::fs::write(path, serde_json::json!({"projects": projects}).to_string()).unwrap();
}

/// One `claude agents --json` row.
pub fn agent(
    short: &str,
    sid: &str,
    name: &str,
    state: &str,
    pid: Option<u32>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": short,
        "sessionId": sid,
        "name": name,
        "cwd": "/x",
        "kind": "background",
        "startedAt": 1,
        "state": state,
    });
    // A live process reports busy while it works (ARCHITECTURE §5 rule 5).
    if let Some(pid) = pid {
        v["pid"] = pid.into();
        v["status"] = if state == "working" { "busy" } else { "idle" }.into();
    }
    v
}

/// Runs the chief binary with a temp home, the fake claude and a trust file.
pub struct Home {
    pub home: tempfile::TempDir,
    pub fake: Fake,
    pub trust: PathBuf,
}

impl Home {
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let trust = home.path().join("claude.json");
        std::fs::write(&trust, "{}").unwrap();
        Home {
            home,
            fake: Fake::new(),
            trust,
        }
    }

    pub fn path(&self) -> &Path {
        self.home.path()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chief"));
        cmd.args(args)
            .env("CHIEF_HOME", self.home.path())
            .env("CHIEF_CLAUDE_BIN", self.fake.bin())
            .env("CHIEF_CLAUDE_CONFIG", &self.trust)
            .env("CHIEF_NO_NOTIFY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    pub fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self.command(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn db(&self) -> rusqlite::Connection {
        chief::db::open(&self.home.path().join("chief.db")).unwrap()
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
