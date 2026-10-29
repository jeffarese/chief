//! Shared test helpers: the fake `claude` and `gh`, a temp chief home and real temporary git
//! repositories. Nothing here starts a real Claude session or calls GitHub.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chief::claude::Claude;
use chief::pr::Gh;

/// A temp dir with a `claude` (or `gh`) link to the fake script; canned outputs go beside it.
pub struct Fake {
    pub dir: tempfile::TempDir,
    name: &'static str,
}

impl Fake {
    pub fn new() -> Self {
        Fake::named("claude")
    }

    /// The fake under another binary name; `Fake::named("gh")` with `pr.stdout` for
    /// `gh pr list`.
    pub fn named(name: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh");
        std::os::unix::fs::symlink(script, dir.path().join(name)).unwrap();
        Fake { dir, name }
    }

    pub fn bin(&self) -> PathBuf {
        self.dir.path().join(self.name)
    }

    pub fn gh(&self) -> Gh {
        Gh::new(self.bin())
    }

    /// The default account, whose folder is `config_dir` (as `CHIEF_CLAUDE_DIR` makes it).
    pub fn claude(&self) -> Claude {
        Claude {
            bin: self.bin(),
            account: None,
            config_dir: self.config_dir(),
        }
    }

    /// The Claude Code config folder this fake stands for (`CHIEF_CLAUDE_DIR`).
    pub fn config_dir(&self) -> PathBuf {
        self.dir.path().join("claude-dir")
    }

    /// Writes the job state of background session `short`, with `worktreePath` when given and
    /// the `worktreeBranch` Claude Code gives it (`worktree-<folder name>`).
    pub fn job(&self, short: &str, worktree: Option<&Path>) -> &Self {
        let branch = worktree.map(|wt| {
            format!(
                "worktree-{}",
                wt.file_name().unwrap_or_default().to_string_lossy()
            )
        });
        self.job_on(short, worktree, branch.as_deref())
    }

    /// `job` with the branch given.
    pub fn job_on(&self, short: &str, worktree: Option<&Path>, branch: Option<&str>) -> &Self {
        job_in(&self.config_dir(), short, worktree, branch);
        self
    }

    /// Removes a job's state, as `claude rm` does.
    pub fn drop_job(&self, short: &str) -> &Self {
        let _ = std::fs::remove_dir_all(self.config_dir().join("jobs").join(short));
        self
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

    /// The process id of call `n`, waiting up to 2 s for the call to write it.
    pub fn pid(&self, n: usize) -> i32 {
        let path = self.dir.path().join(format!("calls/{n}.pid"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(pid) = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .trim()
                .parse()
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub fn cwd(&self, n: usize) -> PathBuf {
        let text = std::fs::read_to_string(self.dir.path().join(format!("calls/{n}.cwd"))).unwrap();
        PathBuf::from(text.trim())
    }

    /// The account (`CLAUDE_CONFIG_DIR`) of every call so far, with its first argument;
    /// `unset` for the default account.
    pub fn accounts(&self) -> Vec<(String, String)> {
        self.calls()
            .into_iter()
            .enumerate()
            .map(|(n, args)| {
                let env = std::fs::read_to_string(self.dir.path().join(format!("calls/{n}.env")))
                    .unwrap();
                (args.first().cloned().unwrap_or_default(), env)
            })
            .collect()
    }
}

/// A Claude Code account folder of its own (`CLAUDE_CONFIG_DIR`): its trust file, and canned
/// `claude agents` output the fake serves under it.
pub fn account(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".claude.json"), "{}").unwrap();
    dir.canonicalize().unwrap()
}

/// The job state of background session `short` in the account folder `config_dir`.
pub fn job_in(config_dir: &Path, short: &str, worktree: Option<&Path>, branch: Option<&str>) {
    let dir = config_dir.join("jobs").join(short);
    std::fs::create_dir_all(&dir).unwrap();
    let mut state = serde_json::json!({"daemonShort": short, "state": "done"});
    if let Some(wt) = worktree {
        state["worktreePath"] = wt.to_string_lossy().into_owned().into();
    }
    if let Some(branch) = branch {
        state["worktreeBranch"] = branch.into();
    }
    std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
}

/// `claude agents` in the account folder `account` (the fake serves it there).
pub fn account_agents(account: &Path, rows: &[serde_json::Value]) {
    std::fs::write(
        account.join("fake-agents.stdout"),
        serde_json::Value::from(rows.to_vec()).to_string(),
    )
    .unwrap();
}

/// Puts a thread row in agp account `id` (folder `dir`; None: the default account).
pub fn in_account(row: &mut chief::db::SessionRow, id: &str, label: &str, dir: Option<&Path>) {
    row.account_id = Some(id.into());
    row.account_label = Some(label.into());
    row.account_dir = dir.map(|d| d.to_string_lossy().into_owned());
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

/// One `agp status --json` view: an account in agp's pool unless `in_pool` is false (then it has
/// no score, as agp gives it none).
pub fn agp_view(id: &str, label: &str, in_pool: bool, score: f64) -> serde_json::Value {
    serde_json::json!({
        "profile_id": id,
        "label": label,
        "identity": {"tool": "claude"},
        "usage": {"five_hour": {"utilization": 10.0}, "seven_day": {"utilization": 20.0}},
        "in_pool": in_pool,
        "limited": null,
        "score": if in_pool { serde_json::json!(score) } else { serde_json::Value::Null },
    })
}

/// Gives the fake `agp` these accounts: `status --json` lists `views`, and `exec <id>` prints
/// each account's folder (None: the default account, no `CLAUDE_CONFIG_DIR`).
pub fn agp_accounts(agp: &Fake, views: &[serde_json::Value], dirs: &[(&str, Option<&Path>)]) {
    agp.set(
        "status.stdout",
        &serde_json::json!({ "views": views }).to_string(),
    );
    for (id, dir) in dirs {
        let text = dir
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        agp.set(&format!("exec.{id}.stdout"), &text);
    }
}

/// Runs the chief binary with a temp home, the fake claude and a trust file.
pub struct Home {
    pub home: tempfile::TempDir,
    pub fake: Fake,
    /// `gh`; lists no PRs unless told otherwise.
    pub gh: Fake,
    /// `rex`; fails like a Rex without a running server unless told otherwise, so no test
    /// reaches the real one.
    pub rex: Fake,
    /// `tmux`; only reached when a test sets `TMUX` (`run_env`): chief runs outside tmux
    /// otherwise.
    pub tmux: Fake,
    /// `agp`; not installed (its link is missing) until `install_agp`, so no test reaches the
    /// real one and threads stay in the project's account.
    pub agp: Fake,
    pub trust: PathBuf,
}

impl Home {
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let trust = home.path().join("claude.json");
        std::fs::write(&trust, "{}").unwrap();
        let gh = Fake::named("gh");
        gh.set("pr.stdout", "[]");
        // No Rex server: every command chief starts with fails.
        let rex = Fake::named("rex");
        rex.set("list.exit", "1")
            .set("list.stderr", "Error: no Rex server is running\n")
            .set("new.exit", "1")
            .set("new.stderr", "Error: no Rex server is running\n");
        let agp = Fake::named("agp");
        std::fs::remove_file(agp.bin()).unwrap();
        Home {
            home,
            fake: Fake::new(),
            gh,
            rex,
            tmux: Fake::named("tmux"),
            agp,
            trust,
        }
    }

    /// Puts the fake `agp` in place; give it accounts with `agp_accounts`.
    pub fn install_agp(&self) -> &Fake {
        if !self.agp.bin().exists() {
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh");
            std::os::unix::fs::symlink(script, self.agp.bin()).unwrap();
        }
        &self.agp
    }

    pub fn path(&self) -> &Path {
        self.home.path()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chief"));
        cmd.args(args)
            .env("CHIEF_HOME", self.home.path())
            .env("CHIEF_CLAUDE_BIN", self.fake.bin())
            .env("CHIEF_GH_BIN", self.gh.bin())
            .env("CHIEF_REX_BIN", self.rex.bin())
            .env("CHIEF_TMUX_BIN", self.tmux.bin())
            .env("CHIEF_AGP_BIN", self.agp.bin())
            .env("CHIEF_CLAUDE_CONFIG", &self.trust)
            .env("CHIEF_CLAUDE_DIR", self.fake.config_dir())
            .env("CHIEF_NO_NOTIFY", "1")
            // A developer's own account folder must not leak into the generated files.
            .env_remove("CLAUDE_CONFIG_DIR")
            // Tests run outside tmux unless they say otherwise, even when cargo runs inside it.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            // A run from inside a Rex block would open the board beside it.
            .env_remove("REX_SESSION")
            .env_remove("REX_BLOCK")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    pub fn run(&self, args: &[&str], stdin: &str) -> Output {
        self.run_env(args, &[], stdin)
    }

    /// `run` with extra environment variables (`TMUX`, say).
    pub fn run_env(&self, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
        let mut cmd = self.command(args);
        cmd.envs(env.iter().copied());
        cmd.output_with_stdin(stdin)
    }

    pub fn db(&self) -> rusqlite::Connection {
        chief::db::open(&self.home.path().join("chief.db")).unwrap()
    }

    /// Registers project `slug` with a default project.toml (the default account), as
    /// `chief init` leaves it, without the other files.
    pub fn register(&self, slug: &str) {
        let dir = self.home.path().join("projects").join(slug);
        chief::config::ProjectConfig::new(slug, slug, None, vec![])
            .save(&dir.join("project.toml"))
            .unwrap();
        chief::db::register_project(&self.db(), slug, &dir, 0).unwrap();
    }
}

/// Runs a piped command (`Home::command`) with `stdin` as its input.
pub trait OutputWithStdin {
    fn output_with_stdin(&mut self, stdin: &str) -> Output;
}

impl OutputWithStdin for Command {
    fn output_with_stdin(&mut self, stdin: &str) -> Output {
        let mut child = self.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs git in `dir` without the user's config getting in the way (signing, hooks); panics on
/// failure. Returns stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=chief test",
            "-c",
            "user.email=chief@test.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A bare remote and a clone of it with `main` pushed; threads' worktrees go under
/// `<repo>/.claude/worktrees/`, as Claude Code puts them.
pub struct GitRepo {
    pub root: tempfile::TempDir,
    pub remote: PathBuf,
    pub repo: PathBuf,
}

impl GitRepo {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let remote = base.join("remote.git");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&base, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "Initial"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        GitRepo { root, remote, repo }
    }

    /// `<repo>/.claude/worktrees/<name>` on a new branch `worktree-<name>`.
    pub fn worktree(&self, name: &str) -> PathBuf {
        let path = self.repo.join(".claude/worktrees").join(name);
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("worktree-{name}"),
                path.to_str().unwrap(),
            ],
        );
        path
    }

    /// Writes `file` and commits it in `dir`.
    pub fn commit(&self, dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), format!("{message}\n")).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    pub fn push(&self, dir: &Path) {
        git(dir, &["push", "-q", "-u", "origin", "HEAD"]);
    }
}
