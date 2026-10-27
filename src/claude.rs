//! Wrapper around the `claude` CLI (ARCHITECTURE §1, §7). `CHIEF_CLAUDE_BIN` overrides the binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const MIN_VERSION: (u32, u32, u32) = (2, 1, 310);

/// One row of `claude agents --json --all`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRow {
    /// Short id that `claude --bg` prints; absent on interactive sessions.
    pub id: Option<String>,
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub kind: Option<String>,
    pub started_at: Option<i64>,
    pub state: Option<String>,
    pub status: Option<String>,
    /// Absent when the process is gone (asleep).
    pub pid: Option<u32>,
}

/// Arguments for a background session.
#[derive(Debug, Clone)]
pub struct BgSpawn {
    pub cwd: PathBuf,
    pub name: String,
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub settings: PathBuf,
    pub mcp_config: Option<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
    pub disallowed_tools: Vec<String>,
    pub append_system_prompt: String,
    /// None starts the session waiting for its first message.
    pub prompt: Option<String>,
}

/// What `claude rm <id>` did. It stops the session even when it refuses (§1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RmOutcome {
    /// Session, worktree and local branch are gone.
    Removed,
    /// Kept: commits exist on no remote. `commits` are the summary lines Claude Code printed;
    /// `token` is the `<head sha>@<worktree-id>` value `--discard-unpushed` takes.
    Unpushed { commits: Vec<String>, token: String },
    /// Kept: the worktree has uncommitted changes (untracked files count, ignored ones don't).
    Dirty,
    /// Anything else, with the output.
    Other(String),
}

/// Errors callers act on; everything else is a plain `anyhow` error.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error(
        "Claude Code doesn't trust {0} yet, and background sessions refuse untrusted folders. \
         Run `chief trust <project>` to mark it trusted, or run `claude` in that folder once \
         (cd {0} && claude), accept the trust prompt, then try again."
    )]
    NotTrusted(PathBuf),
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
pub const AGENTS_TIMEOUT: Duration = Duration::from_secs(15);
const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a freshly spawned session may take to show up in `claude agents` (§7).
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Claude {
    pub bin: PathBuf,
    /// The Claude Code account every call runs in (§1): Some sets `CLAUDE_CONFIG_DIR` to that
    /// folder, None removes it (the default account), whatever this process's environment says.
    pub account: Option<PathBuf>,
    /// The account's config folder (`claude_dir`), where background sessions' job state lives.
    pub config_dir: PathBuf,
}

impl Claude {
    /// The account this process's `CLAUDE_CONFIG_DIR` names: for `chief init`, which gives a new
    /// project that account, and tests. Everything about an existing project uses
    /// `for_account(config.claude_config_dir)`.
    pub fn from_env() -> Self {
        Claude::for_account(env_account())
    }

    /// `bin` with the account from the environment.
    pub fn new(bin: PathBuf) -> Self {
        Claude::with_bin(bin, env_account())
    }

    /// `CHIEF_CLAUDE_BIN` (else `claude`) in `account`.
    pub fn for_account(account: Option<PathBuf>) -> Self {
        let bin = std::env::var_os("CHIEF_CLAUDE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("claude"));
        Claude::with_bin(bin, account)
    }

    pub fn with_bin(bin: PathBuf, account: Option<PathBuf>) -> Self {
        let config_dir = claude_dir(account.as_deref());
        Claude {
            bin,
            account,
            config_dir,
        }
    }

    /// The account's `.claude.json`, where Claude Code keeps folder trust (`claude_config_path`).
    pub fn trust_config(&self) -> PathBuf {
        claude_config_path(self.account.as_deref())
    }

    /// The same binary in `account`: this one itself when it is that account already (keeping
    /// its folder, which tests point elsewhere for the default account).
    pub fn in_account(&self, account: Option<&Path>) -> Claude {
        if same_account(self.account.as_deref(), account) {
            return self.clone();
        }
        Claude::with_bin(self.bin.clone(), account.map(Path::to_path_buf))
    }

    /// `self` is the project's account; the session's own account when it has one (§3), so
    /// every call about it (attach, respawn, stop, rm, its job state) runs there.
    pub fn for_row(&self, row: &crate::db::SessionRow) -> Claude {
        self.in_account(row.account(self.account.as_deref()).as_deref())
    }

    /// `claude` in this account; every call chief makes goes through here.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        match &self.account {
            Some(dir) => cmd.env("CLAUDE_CONFIG_DIR", dir),
            None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
        };
        cmd
    }

    /// What a user types before `claude` to reach this account's sessions (`shell_prefix`).
    pub fn shell_prefix(&self) -> String {
        shell_prefix(self.account.as_deref())
    }

    /// The worktree a background session works in, as its job state records it: the folder
    /// `claude rm` deletes (ARCHITECTURE §12). None without a state file or a `worktreePath`.
    pub fn job_worktree(&self, short_id: &str) -> Option<PathBuf> {
        job_worktree(&self.config_dir, short_id)
    }

    /// A background session's job state (`job_state`).
    pub fn job_state(&self, short_id: &str) -> Option<JobState> {
        job_state(&self.config_dir, short_id)
    }

    /// Fails when `claude --version` is older than MIN_VERSION.
    pub fn check_version(&self) -> Result<()> {
        let out = self.run_ok(&["--version".into()], None, VERSION_TIMEOUT)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(version) = parse_version(&text) else {
            bail!(
                "could not read the Claude Code version from {:?}",
                text.trim()
            );
        };
        if version < MIN_VERSION {
            let (a, b, c) = version;
            let (x, y, z) = MIN_VERSION;
            bail!("Claude Code {a}.{b}.{c} is older than {x}.{y}.{z}; run `claude update`");
        }
        Ok(())
    }

    /// `claude agents --json --all`.
    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        self.agents_within(AGENTS_TIMEOUT)
    }

    /// `agents` with a shorter deadline, for hooks that hold up a session turn.
    pub fn agents_within(&self, timeout: Duration) -> Result<Vec<AgentRow>> {
        let args: Vec<OsString> = vec!["agents".into(), "--json".into(), "--all".into()];
        let out = self.run_ok(&args, None, timeout)?;
        parse_agents(&String::from_utf8_lossy(&out.stdout))
    }

    /// Starts a background session and returns its short id.
    pub fn spawn_bg(&self, spec: &BgSpawn) -> Result<String> {
        let out = self.run(&bg_args(spec), Some(&spec.cwd), SPAWN_TIMEOUT)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("Workspace not trusted") || stderr.contains("Workspace not trusted") {
            return Err(ClaudeError::NotTrusted(spec.cwd.clone()).into());
        }
        match parse_bg_short_id(&stdout) {
            Some(id) => Ok(id),
            None => bail!(
                "`claude --bg` for {} printed no session id ({}).\nstdout: {}\nstderr: {}",
                spec.name,
                out.status,
                stdout.trim(),
                stderr.trim()
            ),
        }
    }

    pub fn respawn(&self, short_id: &str) -> Result<()> {
        self.run_ok(&["respawn".into(), short_id.into()], None, CONTROL_TIMEOUT)?;
        Ok(())
    }

    pub fn stop(&self, short_id: &str) -> Result<()> {
        self.run_ok(&["stop".into(), short_id.into()], None, CONTROL_TIMEOUT)?;
        Ok(())
    }

    /// `claude rm <id> [--discard-unpushed <token>]`. A refusal is an outcome, not an error;
    /// only a failure to run the command is.
    pub fn rm(&self, short_id: &str, discard: Option<&str>) -> Result<RmOutcome> {
        let mut args: Vec<OsString> = vec!["rm".into(), short_id.into()];
        if let Some(token) = discard {
            args.push("--discard-unpushed".into());
            args.push(token.into());
        }
        let out = self.run(&args, None, CONTROL_TIMEOUT)?;
        Ok(parse_rm(
            short_id,
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ))
    }

    /// Finds a freshly spawned session in `claude agents` by its short id, retrying until
    /// `timeout`: `--session-id` is ignored with `--bg`, so this is the only way to learn the
    /// full id (§1).
    pub fn resolve_session(&self, short_id: &str, timeout: Duration) -> Result<AgentRow> {
        let deadline = Instant::now() + timeout;
        loop {
            let last_err = match self.agents() {
                Ok(rows) => {
                    if let Some(row) = rows.into_iter().find(|r| r.id.as_deref() == Some(short_id))
                    {
                        return Ok(row);
                    }
                    None
                }
                Err(e) => Some(e),
            };
            if Instant::now() >= deadline {
                let why = last_err.map(|e| format!(": {e:#}")).unwrap_or_default();
                bail!(
                    "session {short_id} did not appear in `claude agents` within {}s{why}",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// After a `claude --bg` in `cwd` that failed or printed no id, which may have started the
    /// session anyway (a timeout, output chief doesn't recognise): `claude agents` until
    /// `timeout`, for `started_session`. Ok(None) once a listing succeeded without it; the last
    /// error when every listing failed (chief can't tell).
    pub fn find_started(
        &self,
        name: &str,
        before: &[AgentRow],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<Option<AgentRow>> {
        let deadline = Instant::now() + timeout;
        let mut listed = false;
        loop {
            let error = match self.agents() {
                Ok(rows) => {
                    if let Some(row) = started_session(&rows, name, before, cwd) {
                        return Ok(Some(row.clone()));
                    }
                    listed = true;
                    None
                }
                Err(e) => Some(e),
            };
            if Instant::now() >= deadline {
                return match error {
                    Some(e) if !listed => Err(e),
                    _ => Ok(None),
                };
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Runs the binary; fails only if it can't start or outlives `timeout`.
    fn run(&self, args: &[OsString], cwd: Option<&Path>, timeout: Duration) -> Result<Output> {
        let mut cmd = self.command();
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let shown = format!(
            "{} {}",
            self.bin.display(),
            args.first()
                .map(|a| a.to_string_lossy())
                .unwrap_or_default()
        );
        crate::proc::output_with_timeout(&mut cmd, timeout)
            .with_context(|| format!("running `{shown}`"))?
            .with_context(|| format!("`{shown}` took longer than {}s", timeout.as_secs()))
    }

    /// Like `run`, but a non-zero exit is an error carrying stderr.
    fn run_ok(&self, args: &[OsString], cwd: Option<&Path>, timeout: Duration) -> Result<Output> {
        let out = self.run(args, cwd, timeout)?;
        if !out.status.success() {
            let joined: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
            bail!(
                "`claude {}` failed ({}): {}",
                joined.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }
}

/// The `claude --bg` argument list from §7, skipping empty optionals. Repeated flags rather than
/// variadic lists, and the prompt last after a single-valued flag, so no list can swallow it.
pub fn bg_args(spec: &BgSpawn) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--bg".into(),
        "--name".into(),
        spec.name.clone().into(),
        "--model".into(),
        spec.model.clone().into(),
        "--effort".into(),
        spec.effort.clone().into(),
        "--permission-mode".into(),
        spec.permission_mode.clone().into(),
        "--settings".into(),
        spec.settings.clone().into(),
    ];
    if let Some(mcp) = &spec.mcp_config {
        args.push("--mcp-config".into());
        args.push(mcp.clone().into());
    }
    for tool in &spec.disallowed_tools {
        args.push("--disallowedTools".into());
        args.push(tool.into());
    }
    for dir in &spec.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.clone().into());
    }
    if !spec.append_system_prompt.is_empty() {
        args.push("--append-system-prompt".into());
        args.push(spec.append_system_prompt.clone().into());
    }
    if let Some(prompt) = spec.prompt.as_ref().filter(|p| !p.is_empty()) {
        args.push(prompt.into());
    }
    args
}

/// The background session (it has a short id) named exactly `name`, working in `cwd`, that
/// `before` didn't list: the one a spawn of `name` in `cwd` just started. A session with the name
/// in another folder is someone else's (another chief home's, or the user's own).
pub fn started_session<'a>(
    rows: &'a [AgentRow],
    name: &str,
    before: &[AgentRow],
    cwd: &Path,
) -> Option<&'a AgentRow> {
    rows.iter().find(|r| {
        r.id.is_some()
            && r.name.as_deref() == Some(name)
            && r.cwd
                .as_deref()
                .is_some_and(|c| same_folder(Path::new(c), cwd))
            && !before.iter().any(|b| b.session_id == r.session_id)
    })
}

/// Whether two paths name one folder, compared canonically when both exist (macOS `/tmp` is
/// `/private/tmp`).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

/// Parses `2.1.310 (Claude Code)`.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let token = s.split_whitespace().next()?;
    let mut parts = token.split('.').map(|p| p.parse::<u32>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    Some(v)
}

/// Parses `claude agents --json` output.
pub fn parse_agents(json: &str) -> Result<Vec<AgentRow>> {
    serde_json::from_str(json).context("parsing `claude agents --json` output")
}

/// Extracts the short id from `backgrounded · <short-id> · <name>`.
pub fn parse_bg_short_id(stdout: &str) -> Option<String> {
    let clean = strip_ansi(stdout);
    clean.lines().find_map(|line| {
        let rest = &line[line.find("backgrounded")? + "backgrounded".len()..];
        let id = rest.split('·').nth(1)?.trim();
        let ok = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok.then(|| id.to_string())
    })
}

/// Reads `claude rm` output (texts in `tests/fixtures/rm/`). Only a `removed <id>` line counts
/// as removed: a stale `--discard-unpushed` token printed usage and exited 0 in the M2b probe.
pub fn parse_rm(short_id: &str, success: bool, stdout: &str, stderr: &str) -> RmOutcome {
    let text = strip_ansi(&format!("{stdout}\n{stderr}"));
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let removed = format!("removed {short_id}");
    if lines
        .iter()
        .any(|l| *l == removed || l.starts_with(&format!("{removed} ")))
    {
        return RmOutcome::Removed;
    }
    let kept = lines.iter().any(|l| l.starts_with("kept "));
    if kept && let Some(token) = lines.iter().find_map(|l| discard_token(l)) {
        let commits = lines
            .iter()
            .filter(|l| l.contains("unpushed commit"))
            .map(|l| l.to_string())
            .collect();
        return RmOutcome::Unpushed { commits, token };
    }
    if kept && text.contains("uncommitted changes") {
        return RmOutcome::Dirty;
    }
    let shown = lines.join("\n");
    RmOutcome::Other(if shown.is_empty() {
        format!(
            "`claude rm` exited {} with no output",
            if success { "0" } else { "non-zero" }
        )
    } else {
        shown
    })
}

/// The `<sha>@<worktree-id>` after `--discard-unpushed` on a line, if it looks like one (the
/// usage line shows a `<commit>@<worktree-id>` placeholder).
fn discard_token(line: &str) -> Option<String> {
    let rest = &line[line.find("--discard-unpushed ")? + "--discard-unpushed ".len()..];
    let token = rest.split_whitespace().next()?;
    let (sha, worktree) = token.split_once('@')?;
    let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    (hex(sha) && hex(worktree)).then(|| token.to_string())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The account this process's `CLAUDE_CONFIG_DIR` names, made absolute; None (the default
/// account) when it is unset or empty.
pub fn env_account() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty())?;
    let dir = PathBuf::from(dir);
    Some(
        dir.canonicalize()
            .or_else(|_| std::path::absolute(&dir))
            .unwrap_or(dir),
    )
}

/// Whether two accounts are one: both the default account, or the same folder (compared
/// canonically).
pub fn same_account(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_folder(a, b),
        _ => false,
    }
}

/// An account's config folder: the folder itself, or for the default account
/// `$CHIEF_CLAUDE_DIR` (tests) else `~/.claude`.
pub fn claude_dir(account: Option<&Path>) -> PathBuf {
    if let Some(dir) = account {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("CHIEF_CLAUDE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// `CLAUDE_CONFIG_DIR=<dir> `, or nothing for the default account: what a user types before
/// `claude` to reach that account's sessions.
pub fn shell_prefix(account: Option<&Path>) -> String {
    match account {
        Some(dir) => format!(
            "CLAUDE_CONFIG_DIR={} ",
            crate::text::shell_quote(&dir.to_string_lossy())
        ),
        None => String::new(),
    }
}

/// The account's own folder for display: the path, or `default (~/.claude)`.
pub fn account_label(account: Option<&Path>) -> String {
    match account {
        Some(dir) => dir.display().to_string(),
        None => "default (~/.claude)".to_string(),
    }
}

/// When this process's `CLAUDE_CONFIG_DIR` names another account than the project's: the line
/// that says chief uses the project's (§3). Folders are compared canonically.
pub fn account_note(slug: &str, account: Option<&Path>) -> Option<String> {
    let here = env_account();
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if here.as_deref().map(canonical) == account.map(canonical) {
        return None;
    }
    let theirs = match account {
        Some(dir) => format!("the Claude Code account in {}", dir.display()),
        None => "the default Claude Code account (~/.claude)".to_string(),
    };
    let ours = match &here {
        Some(dir) => format!("CLAUDE_CONFIG_DIR={}", dir.display()),
        None => "the default account (CLAUDE_CONFIG_DIR is unset here)".to_string(),
    };
    Some(format!(
        "note: {slug} belongs to {theirs}; chief uses that account, not {ours}"
    ))
}

/// What the supervisor records about a background session's worktree (§1): the source of truth
/// for a thread's worktree and branch, unlike the hooks' cwd.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobState {
    /// `worktreePath`; None for a session that never left its folder.
    pub worktree: Option<PathBuf>,
    /// `worktreeBranch`.
    pub branch: Option<String>,
}

/// `<config_dir>/jobs/<short_id>/state.json`. A missing or unreadable file, or a short id that
/// isn't a plain name, is None.
pub fn job_state(config_dir: &Path, short_id: &str) -> Option<JobState> {
    let plain = !short_id.is_empty()
        && short_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return None;
    }
    let state = config_dir.join("jobs").join(short_id).join("state.json");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state).ok()?).ok()?;
    let text = |key: &str| {
        json[key]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    };
    Some(JobState {
        worktree: text("worktreePath").map(PathBuf::from),
        branch: text("worktreeBranch"),
    })
}

/// `job_state` → `worktreePath`: None is no worktree chief may touch.
pub fn job_worktree(config_dir: &Path, short_id: &str) -> Option<PathBuf> {
    job_state(config_dir, short_id)?.worktree
}

/// An account's Claude Code config file, where folder trust lives: `<dir>/.claude.json` (one
/// per account folder), or for the default account `$CHIEF_CLAUDE_CONFIG` (tests) else
/// `~/.claude.json`.
pub fn claude_config_path(account: Option<&Path>) -> PathBuf {
    if let Some(dir) = account {
        return dir.join(".claude.json");
    }
    if let Some(p) = std::env::var_os("CHIEF_CLAUDE_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_default().join(".claude.json")
}

/// Reads `config` (`claude_config_path`) → `projects[<path>].hasTrustDialogAccepted`.
/// Read-only, never writes. A missing file trusts nothing. Matches what
/// Claude Code 2.1.310 did live: a trusted folder covers the folders under it (with `/` trusted, a
/// background session started in a folder whose own entry said false), but not past a git
/// repository's root (the same `/` didn't cover a fresh repo; Claude asked to trust it). So the
/// walk up stops at the first folder holding `.git`. Both the path as given and its canonical
/// form (macOS `/tmp` is `/private/tmp`) are checked.
pub fn is_trusted_in(config: &Path, path: &Path) -> Result<bool> {
    let text = match std::fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config.display())),
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;
    let mut keys = vec![path.to_path_buf()];
    if let Ok(canon) = path.canonicalize() {
        keys.push(canon);
    }
    Ok(keys.iter().flat_map(|key| trust_scope(key)).any(|dir| {
        let key = dir.to_string_lossy();
        let key = if key.len() > 1 {
            key.trim_end_matches('/')
        } else {
            &key
        };
        json["projects"][key]["hasTrustDialogAccepted"].as_bool() == Some(true)
    }))
}

/// Marks `path` trusted in `config` the way accepting Claude's trust dialog does:
/// `projects[<canonical path>].hasTrustDialogAccepted = true`, keeping the rest of the file and
/// its key order. Only after the user said yes (§3 "Trust"). The file is replaced by a rename in
/// its own folder, through a symlink to its target, with its permissions kept. Claude Code
/// rewrites this file itself, so the trust is read back; a write that lost a race is tried once
/// more.
pub fn mark_trusted(config: &Path, path: &Path) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    for _ in 0..2 {
        write_trust(config, &path)?;
        if is_trusted_in(config, &path)? {
            return Ok(());
        }
    }
    bail!(
        "{} still isn't trusted in {}: Claude Code may have rewritten it meanwhile",
        path.display(),
        config.display()
    )
}

fn write_trust(config: &Path, path: &Path) -> Result<()> {
    let target = config
        .canonicalize()
        .unwrap_or_else(|_| config.to_path_buf());
    let (mut json, perms) = match std::fs::read_to_string(&target) {
        Ok(text) => (
            serde_json::from_str::<serde_json::Value>(&text)
                .with_context(|| format!("parsing {}", target.display()))?,
            std::fs::metadata(&target).ok().map(|m| m.permissions()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (serde_json::json!({}), None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
    };
    let Some(root) = json.as_object_mut() else {
        bail!("{} isn't a JSON object", target.display());
    };
    let projects = root
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let Some(projects) = projects.as_object_mut() else {
        bail!("{} → projects isn't a JSON object", target.display());
    };
    let entry = projects
        .entry(path.to_string_lossy().into_owned())
        .or_insert_with(|| serde_json::json!({}));
    let Some(entry) = entry.as_object_mut() else {
        bail!(
            "{} → projects → {} isn't a JSON object",
            target.display(),
            path.display()
        );
    };
    entry.insert("hasTrustDialogAccepted".into(), true.into());

    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".claude.json.chief-{}", std::process::id()));
    let text = serde_json::to_string_pretty(&json)? + "\n";
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    let perms = perms.unwrap_or_else(|| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o600)
    });
    let renamed = std::fs::set_permissions(&tmp, perms)
        .and_then(|()| std::fs::rename(&tmp, &target))
        .with_context(|| format!("replacing {}", target.display()));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

/// `path` and its ancestors up to and including the git repository root, or up to `/` outside a
/// repository.
fn trust_scope(path: &Path) -> Vec<&Path> {
    let mut scope = Vec::new();
    for dir in path.ancestors() {
        scope.push(dir);
        if dir.join(".git").exists() {
            break;
        }
    }
    scope
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BgSpawn {
        BgSpawn {
            cwd: "/work/repo".into(),
            name: "p-fix-1a2b".into(),
            model: "sonnet".into(),
            effort: "high".into(),
            permission_mode: "auto".into(),
            settings: "/root/projects/p/generated/thread-settings.json".into(),
            mcp_config: None,
            add_dirs: vec!["/root/projects/p/memory".into()],
            disallowed_tools: vec![],
            append_system_prompt: "brief\nline two".into(),
            prompt: Some("Fix the bug".into()),
        }
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn thread_args_match_the_contract() {
        assert_eq!(
            strs(&bg_args(&spec())),
            [
                "--bg",
                "--name",
                "p-fix-1a2b",
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--permission-mode",
                "auto",
                "--settings",
                "/root/projects/p/generated/thread-settings.json",
                "--add-dir",
                "/root/projects/p/memory",
                "--append-system-prompt",
                "brief\nline two",
                "Fix the bug",
            ]
        );
    }

    #[test]
    fn coordinator_args_match_the_contract() {
        let s = BgSpawn {
            name: "chief-p".into(),
            model: "opus".into(),
            effort: "low".into(),
            settings: "/d/generated/coordinator-settings.json".into(),
            mcp_config: Some("/d/generated/mcp.json".into()),
            add_dirs: vec!["/r1".into(), "/r2".into()],
            disallowed_tools: vec!["Bash".into()],
            append_system_prompt: "coord".into(),
            prompt: None,
            ..spec()
        };
        assert_eq!(
            strs(&bg_args(&s)),
            [
                "--bg",
                "--name",
                "chief-p",
                "--model",
                "opus",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                "--settings",
                "/d/generated/coordinator-settings.json",
                "--mcp-config",
                "/d/generated/mcp.json",
                "--disallowedTools",
                "Bash",
                "--add-dir",
                "/r1",
                "--add-dir",
                "/r2",
                "--append-system-prompt",
                "coord",
            ]
        );
    }

    #[test]
    fn empty_optionals_are_skipped() {
        let s = BgSpawn {
            add_dirs: vec![],
            append_system_prompt: String::new(),
            prompt: Some(String::new()),
            ..spec()
        };
        let args = strs(&bg_args(&s));
        assert_eq!(args.len(), 11);
        assert_eq!(
            args.last().unwrap(),
            "/root/projects/p/generated/thread-settings.json"
        );
    }

    #[test]
    fn short_id_parsing() {
        let cases = [
            (
                "backgrounded · f8627ea8 · spike-thread-a\n",
                Some("f8627ea8"),
            ),
            ("noise\n  backgrounded · a1b2c3d4 · x\n", Some("a1b2c3d4")),
            (
                "\u{1b}[2mbackgrounded\u{1b}[0m · \u{1b}[1mabc123\u{1b}[0m · n",
                Some("abc123"),
            ),
            ("backgrounded · abc123", Some("abc123")),
            ("Workspace not trusted", None),
            ("backgrounded ·  · name", None),
            ("backgrounded", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_bg_short_id(input).as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("2.1.310 (Claude Code)\n"), Some((2, 1, 310)));
        assert_eq!(parse_version("10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_version("2.1 (Claude Code)"), None);
        assert_eq!(parse_version("Claude Code"), None);
        assert!((2, 1, 309) < MIN_VERSION);
        assert!((2, 2, 0) > MIN_VERSION);
    }

    #[test]
    fn parses_the_agents_fixture() {
        let json = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/agents/agents-sample.json"
        ))
        .unwrap();
        let rows = parse_agents(&json).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            AgentRow {
                id: Some("f8627ea8".into()),
                session_id: "f8627ea8-50af-4cce-b12d-936669f85ed0".into(),
                name: Some("demo-hello-txt-1a2b".into()),
                cwd: Some("/tmp/chief-fixture/repo".into()),
                kind: Some("background".into()),
                started_at: Some(1792940000000),
                state: Some("working".into()),
                status: Some("busy".into()),
                pid: Some(93669),
            }
        );
        let asleep = &rows[3];
        assert_eq!(asleep.state.as_deref(), Some("done"));
        assert_eq!((asleep.status.as_deref(), asleep.pid), (None, None));
        let interactive = &rows[4];
        assert_eq!(
            (interactive.id.as_deref(), interactive.state.as_deref()),
            (None, None)
        );
        assert!(parse_agents("not json").is_err());
    }

    fn rm_fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/rm/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn rm_output_parsing() {
        assert_eq!(
            parse_rm("58534bf3", true, &rm_fixture("removed"), ""),
            RmOutcome::Removed
        );
        assert_eq!(
            parse_rm("b65c3d73", false, &rm_fixture("unpushed-one"), ""),
            RmOutcome::Unpushed {
                commits: vec![
                    "1 unpushed commit on “worktree-e2e-add-notes-txt”: a2a6266 “Add notes.txt”. \
                     It exists on no remote, so deleting the worktree would lose it."
                        .into()
                ],
                token: "a2a6266abcfc311c36e618d6f2c556188b528181@03abe83c324a9c3e9a0db4020ffa195d"
                    .into(),
            }
        );
        let RmOutcome::Unpushed { commits, token } =
            parse_rm("58534bf3", false, &rm_fixture("unpushed-many"), "")
        else {
            panic!("want unpushed");
        };
        assert_eq!(
            token,
            "2eb2c0406cd37c70c23cd34e3469a5f4796e3756@bce72e14d86c9546d5f9872220affd57"
        );
        assert!(
            commits[0].starts_with("3 unpushed commits on"),
            "{commits:?}"
        );
        assert_eq!(
            parse_rm("b65c3d73", false, &rm_fixture("dirty"), ""),
            RmOutcome::Dirty
        );
        // Usage on a stale token, even with exit 0, isn't a removal; nor is another id's line.
        let stale = parse_rm("58534bf3", true, "", &rm_fixture("stale-token"));
        assert!(
            matches!(&stale, RmOutcome::Other(t) if t.contains("--discard-unpushed")),
            "{stale:?}"
        );
        assert!(matches!(
            parse_rm("58534bf3", true, "removed 58534bf3x\n", ""),
            RmOutcome::Other(_)
        ));
        assert_eq!(
            parse_rm(
                "abc",
                true,
                "\u{1b}[32mremoved abc\u{1b}[0m\n  worktree: x\n",
                ""
            ),
            RmOutcome::Removed
        );
        assert!(matches!(
            parse_rm("abc", false, "", ""),
            RmOutcome::Other(t) if t.contains("no output")
        ));
        assert!(matches!(
            parse_rm("abc", false, "", "error: no session abc\n"),
            RmOutcome::Other(t) if t == "error: no session abc"
        ));
    }

    #[test]
    fn job_worktree_comes_from_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let write = |short: &str, json: &str| {
            let job = dir.path().join("jobs").join(short);
            std::fs::create_dir_all(&job).unwrap();
            std::fs::write(job.join("state.json"), json).unwrap();
        };
        write(
            "a1b2c3d4",
            r#"{"state":"done","daemonShort":"a1b2c3d4","worktreePath":"/r/.claude/worktrees/fix","worktreeBranch":"worktree-fix"}"#,
        );
        write("e5f6", r#"{"state":"done","bgIsolation":"none"}"#);
        write("bad", "{");
        let claude = Claude::with_bin("claude".into(), Some(dir.path().to_path_buf()));
        assert_eq!(
            claude.job_worktree("a1b2c3d4"),
            Some(PathBuf::from("/r/.claude/worktrees/fix"))
        );
        for short in ["e5f6", "bad", "missing", "", "../a1b2c3d4", "a1b2c3d4/x"] {
            assert_eq!(claude.job_worktree(short), None, "{short:?}");
        }
        assert_eq!(
            claude.job_state("a1b2c3d4").unwrap().branch.as_deref(),
            Some("worktree-fix")
        );
        assert_eq!(claude.job_state("e5f6"), Some(JobState::default()));
        assert_eq!(claude.job_state("bad"), None);
    }

    #[test]
    fn marking_trust_keeps_the_rest_of_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let canon = repo.canonicalize().unwrap();
        let real = dir.path().join("real.json");
        std::fs::write(
            &real,
            r#"{"zeta": 1, "projects": {"/other": {"allowedTools": [], "hasTrustDialogAccepted": false}}, "alpha": {"b": 2, "a": 1}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        // Dotfiles are often symlinks: the target is replaced, the link kept.
        let config = dir.path().join(".claude.json");
        std::os::unix::fs::symlink(&real, &config).unwrap();

        assert!(!is_trusted_in(&config, &repo).unwrap());
        mark_trusted(&config, &repo).unwrap();
        assert!(is_trusted_in(&config, &repo).unwrap());
        assert!(!is_trusted_in(&config, Path::new("/other")).unwrap());
        assert!(config.symlink_metadata().unwrap().file_type().is_symlink());
        let text = std::fs::read_to_string(&real).unwrap();
        let keys: Vec<&str> = ["zeta", "projects", "alpha", "\"b\"", "\"a\""]
            .into_iter()
            .collect();
        let at: Vec<usize> = keys.iter().map(|k| text.find(k).unwrap()).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "order changed:\n{text}");
        assert!(text.contains("allowedTools"));
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        // Marking again changes nothing; an existing entry keeps its other keys.
        mark_trusted(&config, &canon).unwrap();
        mark_trusted(&config, Path::new("/")).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(
            json["projects"]["/other"]["allowedTools"],
            serde_json::json!([])
        );
        assert_eq!(json["projects"]["/"]["hasTrustDialogAccepted"], true);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("chief-"))
            .collect();
        assert!(leftovers.is_empty());

        // A missing file is created private; one chief can't read is left alone.
        let fresh = dir.path().join("fresh/.claude.json");
        mark_trusted(&fresh, &repo).unwrap();
        assert!(is_trusted_in(&fresh, &repo).unwrap());
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert!(mark_trusted(&broken, &repo).is_err());
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ not json");
    }

    #[test]
    fn trust_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let canon = repo.canonicalize().unwrap();
        let config = dir.path().join("claude.json");
        let json = serde_json::json!({
            "projects": {
                canon.to_string_lossy(): {"hasTrustDialogAccepted": true},
                "/untrusted": {"hasTrustDialogAccepted": false},
                "/parent": {"hasTrustDialogAccepted": true},
                "/parent/declined": {"hasTrustDialogAccepted": false},
            }
        });
        std::fs::write(&config, json.to_string()).unwrap();
        let before = std::fs::read(&config).unwrap();

        assert!(is_trusted_in(&config, &repo).unwrap());
        assert!(is_trusted_in(&config, &canon).unwrap());
        assert!(is_trusted_in(&config, &canon.join("sub/dir")).unwrap());
        assert!(!is_trusted_in(&config, Path::new("/untrusted")).unwrap());
        assert!(!is_trusted_in(&config, Path::new("/untrusted/child")).unwrap());
        assert!(is_trusted_in(&config, Path::new("/parent/child")).unwrap());
        // The child's own false entry doesn't override a trusted parent.
        assert!(is_trusted_in(&config, Path::new("/parent/declined")).unwrap());
        assert!(!is_trusted_in(&config, Path::new("/nowhere")).unwrap());

        // A trusted parent stops covering at a git repository's root.
        let outer = dir.path().join("outer");
        let git_repo = outer.join("project");
        std::fs::create_dir_all(git_repo.join(".git")).unwrap();
        std::fs::create_dir_all(git_repo.join("pkg")).unwrap();
        std::fs::create_dir_all(outer.join("plain/sub")).unwrap();
        let outer = outer.canonicalize().unwrap();
        let git_repo = git_repo.canonicalize().unwrap();
        let json = serde_json::json!({
            "projects": { outer.to_string_lossy(): {"hasTrustDialogAccepted": true} }
        });
        let scoped = dir.path().join("scoped.json");
        std::fs::write(&scoped, json.to_string()).unwrap();
        assert!(is_trusted_in(&scoped, &outer.join("plain/sub")).unwrap());
        assert!(!is_trusted_in(&scoped, &git_repo).unwrap());
        assert!(!is_trusted_in(&scoped, &git_repo.join("pkg")).unwrap());
        let json = serde_json::json!({
            "projects": { git_repo.to_string_lossy(): {"hasTrustDialogAccepted": true} }
        });
        std::fs::write(&scoped, json.to_string()).unwrap();
        assert!(is_trusted_in(&scoped, &git_repo.join("pkg")).unwrap());
        assert!(!is_trusted_in(&dir.path().join("missing.json"), &repo).unwrap());
        assert_eq!(std::fs::read(&config).unwrap(), before, "must never write");

        std::fs::write(&config, "{").unwrap();
        assert!(is_trusted_in(&config, &repo).is_err());
    }
}
