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
         Run `claude` in that folder once (cd {0} && claude), accept the trust prompt, then try again."
    )]
    NotTrusted(PathBuf),
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const AGENTS_TIMEOUT: Duration = Duration::from_secs(15);
const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a freshly spawned session may take to show up in `claude agents` (§7).
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Claude {
    pub bin: PathBuf,
    /// Claude Code's config folder (`claude_dir`), where background sessions' job state lives.
    pub config_dir: PathBuf,
}

impl Claude {
    pub fn from_env() -> Self {
        let bin = std::env::var_os("CHIEF_CLAUDE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("claude"));
        Claude::new(bin)
    }

    /// `bin` with the config folder from the environment.
    pub fn new(bin: PathBuf) -> Self {
        Claude {
            bin,
            config_dir: claude_dir(),
        }
    }

    /// The worktree a background session works in, as its job state records it: the folder
    /// `claude rm` deletes (ARCHITECTURE §12). None without a state file or a `worktreePath`.
    pub fn job_worktree(&self, short_id: &str) -> Option<PathBuf> {
        job_worktree(&self.config_dir, short_id)
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

    /// Runs `claude` in `cwd` on this terminal and waits for it to exit (for the trust prompt).
    pub fn run_interactive(&self, cwd: &Path) -> Result<std::process::ExitStatus> {
        Command::new(&self.bin)
            .current_dir(cwd)
            .status()
            .with_context(|| format!("running {} in {}", self.bin.display(), cwd.display()))
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
        let mut cmd = Command::new(&self.bin);
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

/// Claude Code's config folder: `$CHIEF_CLAUDE_DIR` (tests), else `$CLAUDE_CONFIG_DIR` (Claude
/// Code's own override), else `~/.claude`.
pub fn claude_dir() -> PathBuf {
    for var in ["CHIEF_CLAUDE_DIR", "CLAUDE_CONFIG_DIR"] {
        if let Some(dir) = std::env::var_os(var).filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// `<config_dir>/jobs/<short_id>/state.json` → `worktreePath`. A missing or unreadable file, or
/// a short id that isn't a plain name, is None: no worktree chief may touch.
pub fn job_worktree(config_dir: &Path, short_id: &str) -> Option<PathBuf> {
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
    json["worktreePath"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// `$CHIEF_CLAUDE_CONFIG`, else `$CLAUDE_CONFIG_DIR/.claude.json` (where Claude Code keeps it
/// when that folder is set, one per account), else `~/.claude.json`.
pub fn claude_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CHIEF_CLAUDE_CONFIG") {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join(".claude.json");
    }
    dirs::home_dir().unwrap_or_default().join(".claude.json")
}

/// Reads `~/.claude.json` → `projects[<path>].hasTrustDialogAccepted`. Read-only, never writes.
pub fn is_trusted(path: &Path) -> Result<bool> {
    is_trusted_in(&claude_config_path(), path)
}

/// `is_trusted` against an explicit config file. A missing file trusts nothing. Matches what
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
            r#"{"state":"done","daemonShort":"a1b2c3d4","worktreePath":"/r/.claude/worktrees/fix"}"#,
        );
        write("e5f6", r#"{"state":"done","bgIsolation":"none"}"#);
        write("bad", "{");
        let claude = Claude {
            bin: "claude".into(),
            config_dir: dir.path().to_path_buf(),
        };
        assert_eq!(
            claude.job_worktree("a1b2c3d4"),
            Some(PathBuf::from("/r/.claude/worktrees/fix"))
        );
        for short in ["e5f6", "bad", "missing", "", "../a1b2c3d4", "a1b2c3d4/x"] {
            assert_eq!(claude.job_worktree(short), None, "{short:?}");
        }
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
