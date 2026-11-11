//! The `agp` CLI (agent-prism), which keeps the user's Claude Code accounts (ARCHITECTURE §1,
//! §3, §7). chief reads the accounts and their usage from `agp status --json` and ranks them
//! itself: `agp pick` ignores `--accounts` and `--exclude`. An agp account is a Claude Code
//! config folder, which `agp exec <id>` names in `CLAUDE_CONFIG_DIR`. `CHIEF_AGP_BIN` overrides
//! the binary.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{OnLimit, ThreadSettings};

/// `[threads] accounts` entry meaning every account in agp's pool.
pub const AUTO: &str = "auto";
const TIMEOUT: Duration = Duration::from_secs(15);
/// `agp bg --json` reads a few files; the board runs it with every poll.
const BG_TIMEOUT: Duration = Duration::from_secs(5);
/// `agp bg move` stops the session, waits for its process to end and resumes it elsewhere.
const MOVE_TIMEOUT: Duration = Duration::from_secs(120);

/// One account of `agp status --json` → `views[]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    /// The agp profile id (`agp exec <id>`).
    pub id: String,
    /// What agp shows for it, e.g. `PB2`.
    pub label: String,
    pub in_pool: bool,
    /// agp counts it limited right now.
    pub limited: bool,
    /// agp has a login for it (`token` isn't `Missing`; an expired token still counts, since
    /// the next run refreshes it). A thread started without one would only ask to log in.
    pub logged_in: bool,
    /// agp's ranking score; higher is better.
    pub score: Option<f64>,
    /// 5-hour and weekly utilization in percent; None when agp doesn't know.
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

impl Account {
    /// The label, or the id when agp gives none.
    pub fn shown(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.id
        } else {
            &self.label
        }
    }

    /// At or over `max_usage` percent, in the 5-hour or the weekly window.
    pub fn over(&self, max_usage: Option<u32>) -> bool {
        let Some(max) = max_usage else {
            return false;
        };
        [self.five_hour, self.seven_day]
            .into_iter()
            .flatten()
            .any(|used| used >= f64::from(max))
    }
}

#[derive(Deserialize)]
struct Status {
    #[serde(default)]
    views: Vec<View>,
}

#[derive(Deserialize)]
struct View {
    profile_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    identity: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    in_pool: bool,
    #[serde(default)]
    limited: Value,
    #[serde(default)]
    token: Value,
    #[serde(default)]
    score: Option<f64>,
}

/// `agp status --json` → its Claude accounts, in agp's order. Codex (ChatGPT) accounts are left
/// out: they have no Claude Code config folder.
pub fn parse_status(json: &str) -> Result<Vec<Account>> {
    let status: Status = serde_json::from_str(json).context("parsing `agp status --json`")?;
    Ok(status
        .views
        .into_iter()
        .filter(|v| {
            let tool = v.identity.as_ref().and_then(|i| i["tool"].as_str());
            tool.is_none_or(|t| t == "claude")
        })
        .map(|v| {
            let utilization = |window: &str| {
                v.usage
                    .as_ref()
                    .and_then(|u| u[window]["utilization"].as_f64())
            };
            Account {
                five_hour: utilization("five_hour"),
                seven_day: utilization("seven_day"),
                label: v.label.clone().unwrap_or_default(),
                in_pool: v.in_pool,
                // agp gives the limit as an object; `false` means none.
                limited: matches!(v.limited, Value::Object(_) | Value::Bool(true)),
                // `"Missing"`, next to `{"Valid": …}` and `{"Expired": …}`; an agp that
                // doesn't say counts as logged in.
                logged_in: v.token.as_str() != Some("Missing"),
                score: v.score,
                id: v.profile_id,
            }
        })
        .collect())
}

/// The accounts `[threads] accounts` allows that agp has in its pool, in agp's order: every one
/// for `auto`, else the listed ids. Empty `allowed` is no pool.
pub fn pool<'a>(accounts: &'a [Account], allowed: &[String]) -> Vec<&'a Account> {
    let auto = allowed.iter().any(|a| a == AUTO);
    accounts
        .iter()
        .filter(|a| a.in_pool && (auto || allowed.contains(&a.id)))
        .collect()
}

/// The pool accounts that can take a thread now (logged in, not limited, under `max_usage`),
/// best score first; accounts without a score come last, and ties keep agp's order.
pub fn usable<'a>(pool: &[&'a Account], max_usage: Option<u32>) -> Vec<&'a Account> {
    let mut usable: Vec<&Account> = pool
        .iter()
        .copied()
        .filter(|a| a.logged_in && !a.limited && !a.over(max_usage))
        .collect();
    usable.sort_by(|a, b| {
        let score = |x: &Account| x.score.unwrap_or(f64::NEG_INFINITY);
        score(b).total_cmp(&score(a))
    });
    usable
}

/// The pool account `wanted` names: its id, else its label.
pub fn find<'a>(pool: &[&'a Account], wanted: &str) -> Option<&'a Account> {
    let wanted = wanted.trim();
    pool.iter()
        .find(|a| a.id == wanted)
        .or_else(|| pool.iter().find(|a| a.label == wanted))
        .copied()
}

/// `id (label)` for every pool account, for messages.
pub fn listed(pool: &[&Account]) -> String {
    if pool.is_empty() {
        return "none".to_string();
    }
    pool.iter()
        .map(|a| {
            if a.shown() == a.id {
                a.id.clone()
            } else {
                format!("{} ({})", a.id, a.shown())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a user runs to trust `folder` in agp account `id`: Claude's trust prompt opens there.
pub fn trust_command(id: &str, folder: &Path) -> String {
    format!(
        "cd {} && agp exec {} -- claude",
        crate::text::shell_quote(&folder.to_string_lossy()),
        crate::text::shell_quote(id)
    )
}

/// What agp does about a thread's limits (`--on-limit`, `--accounts`, `--max-usage`), from
/// `[threads]` (§3 "Threads through agp").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub on_limit: OnLimit,
    /// The pool ids `[threads] accounts` lists; empty for `auto`, agp's whole pool.
    pub accounts: Vec<String>,
    pub max_usage: Option<u32>,
}

impl RunOptions {
    pub fn from_settings(threads: &ThreadSettings) -> Self {
        let accounts = if threads.accounts.iter().any(|a| a == AUTO) {
            Vec::new()
        } else {
            threads.accounts.clone()
        };
        // `accounts = []` keeps threads in the project's account (§3): nowhere to switch to,
        // so agp waits for the reset there.
        let on_limit = match threads.on_limit {
            OnLimit::Switch if threads.accounts.is_empty() => OnLimit::Wait,
            other => other,
        };
        RunOptions {
            on_limit,
            accounts,
            max_usage: threads.max_usage,
        }
    }

    /// agp's per-run options for them.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["--on-limit".to_string(), self.on_limit.as_str().to_string()];
        if !self.accounts.is_empty() {
            args.push("--accounts".to_string());
            args.push(self.accounts.join(","));
        }
        if let Some(max) = self.max_usage {
            args.push("--max-usage".to_string());
            args.push(max.to_string());
        }
        args
    }
}

/// `agp --as <id> <run options> --`: what goes before the `claude --bg` arguments for agp to
/// start the session in account `id` and watch it (§7 "Thread spawn"). The `--` ends agp's
/// options, so nothing of claude's is read as one of them.
pub fn launch_args(id: &str, run: &RunOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["--as".into(), id.into()];
    args.extend(run.args().into_iter().map(OsString::from));
    args.push("--".into());
    args
}

/// A background session agp watches (`agp bg --json` → `sessions[]`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct BgSession {
    pub session_id: String,
    #[serde(default)]
    pub short_id: Option<String>,
    /// The agp profile it runs in now.
    pub profile: String,
    #[serde(default)]
    pub label: Option<String>,
    /// That profile's config folder; None is the default account.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// watching, limited, waiting, moving, ended, dormant.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub limit: Option<BgLimit>,
    /// Oldest first.
    #[serde(default)]
    pub moves: Vec<BgMove>,
}

impl BgSession {
    /// agp is stopping it in one account to resume it in another (or after a reset): nothing
    /// may revive it meanwhile.
    pub fn moving(&self) -> bool {
        self.state == "moving"
    }

    pub fn shown(&self) -> &str {
        self.label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&self.profile)
    }

    /// Its account as chief stores one: None for the default account.
    pub fn account(&self) -> Option<PathBuf> {
        normal_dir(self.config_dir.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct BgLimit {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub since: Option<i64>,
    /// Unix seconds; None when agp doesn't know.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// One move of a background session, as `agp bg --json` lists it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct BgMove {
    /// Unix seconds.
    pub at: i64,
    pub from: String,
    #[serde(default)]
    pub from_label: Option<String>,
    #[serde(default)]
    pub from_config_dir: Option<PathBuf>,
    pub to: String,
    #[serde(default)]
    pub to_label: Option<String>,
    #[serde(default)]
    pub to_config_dir: Option<PathBuf>,
    /// "5-hour limit", "weekly limit", "manual move", "limit".
    #[serde(default)]
    pub reason: Option<String>,
}

impl BgMove {
    pub fn from_shown(&self) -> &str {
        self.from_label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&self.from)
    }

    pub fn to_shown(&self) -> &str {
        self.to_label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&self.to)
    }
}

#[derive(Deserialize)]
struct BgList {
    #[serde(default)]
    sessions: Vec<BgSession>,
}

/// `agp bg --json` → its sessions.
pub fn parse_bg(json: &str) -> Result<Vec<BgSession>> {
    let list: BgList = serde_json::from_str(json).context("parsing `agp bg --json`")?;
    Ok(list.sessions)
}

/// `agp bg move --json` → where the session went.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Moved {
    #[serde(default)]
    pub moved: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub short_id: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub from_label: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub to_label: Option<String>,
    #[serde(default)]
    pub to_config_dir: Option<PathBuf>,
}

/// An account folder as chief stores it: the default account's (`~/.claude`) is None, as agp
/// and `CLAUDE_CONFIG_DIR` leave it unset.
pub fn normal_dir(dir: Option<&Path>) -> Option<PathBuf> {
    let dir = dir.filter(|d| !d.as_os_str().is_empty())?;
    let default = dirs::home_dir().map(|h| h.join(".claude"));
    if default.is_some_and(|d| crate::claude::same_folder(dir, &d)) {
        return None;
    }
    Some(dir.to_path_buf())
}

pub struct Agp {
    pub bin: PathBuf,
}

impl Agp {
    pub fn new(bin: PathBuf) -> Self {
        Agp { bin }
    }

    /// `CHIEF_AGP_BIN`, else `agp` on PATH.
    pub fn from_env() -> Self {
        let bin = std::env::var_os("CHIEF_AGP_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("agp"));
        Agp::new(bin)
    }

    /// Whether this agp watches background sessions (`agp bg`): an older one reads `agp bg …`
    /// as a `claude` line and would start a session, so nothing calls `bg` without asking.
    /// Asked once per binary per process; false when agp isn't installed or doesn't answer.
    pub fn supports_bg(&self) -> bool {
        static KNOWN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
        let known = KNOWN.get_or_init(Default::default);
        if let Some(yes) = known.lock().ok().and_then(|k| k.get(&self.bin).copied()) {
            return yes;
        }
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--help").stdin(std::process::Stdio::null());
        let yes = matches!(
            crate::proc::output_with_timeout(&mut cmd, BG_TIMEOUT),
            Ok(Some(out)) if help_has_bg(&String::from_utf8_lossy(&out.stdout))
                || help_has_bg(&String::from_utf8_lossy(&out.stderr))
        );
        if let Ok(mut k) = known.lock() {
            k.insert(self.bin.clone(), yes);
        }
        yes
    }

    /// The command that starts a thread's `claude --bg …` (`claude_args`) through agp in account
    /// `id`. Without `CLAUDE_CONFIG_DIR`: agp sets the account's (and leaves it unset for the
    /// default one), so the project's, which the MCP server inherits, must not reach it. Without
    /// the repository variables too, as for `claude` itself.
    pub fn launch(&self, id: &str, run: &RunOptions, claude_args: &[OsString]) -> Command {
        let mut cmd = Command::new(&self.bin);
        crate::proc::without_repo_env(&mut cmd);
        cmd.env_remove("CLAUDE_CONFIG_DIR");
        cmd.args(launch_args(id, run)).args(claude_args);
        cmd
    }

    /// `agp bg --json`, bounded: the background sessions agp watches. Ok(None) when agp isn't
    /// installed or has no `bg` (`supports_bg`).
    pub fn bg(&self) -> Result<Option<Vec<BgSession>>> {
        if !self.supports_bg() {
            return Ok(None);
        }
        let mut cmd = self.plain();
        cmd.args(["bg", "--json"]);
        let Some(out) = self.run_within(&mut cmd, "bg --json", BG_TIMEOUT)? else {
            return Ok(None);
        };
        Ok(Some(parse_bg(&String::from_utf8_lossy(&out.stdout))?))
    }

    /// `agp bg watch <session> [--as <id>] <run options>`: agp watches an existing background
    /// session (after `claude respawn`, or one started without agp). It detaches its watcher and
    /// answers at once.
    pub fn watch(&self, session: &str, id: Option<&str>, run: &RunOptions) -> Result<()> {
        if !self.supports_bg() {
            bail!("this agp has no `agp bg`; update agp");
        }
        let mut cmd = self.plain();
        cmd.args(["bg", "watch", session]);
        if let Some(id) = id {
            cmd.args(["--as", id]);
        }
        cmd.args(run.args());
        match self.run_within(&mut cmd, "bg watch", TIMEOUT)? {
            Some(_) => Ok(()),
            None => bail!("agp isn't installed"),
        }
    }

    /// `agp bg move <session> [--to <id>] --json`: agp moves the session now, to `to` or the
    /// pool account with the most room.
    pub fn move_now(&self, session: &str, to: Option<&str>) -> Result<Moved> {
        if !self.supports_bg() {
            bail!("this agp has no `agp bg`; update agp");
        }
        let mut cmd = self.plain();
        cmd.args(["bg", "move", session]);
        if let Some(to) = to {
            cmd.args(["--to", to]);
        }
        cmd.arg("--json");
        let Some(out) = self.run_within(&mut cmd, "bg move", MOVE_TIMEOUT)? else {
            bail!("agp isn't installed");
        };
        serde_json::from_slice(&out.stdout).context("parsing `agp bg move --json`")
    }

    /// agp itself, without this process's `CLAUDE_CONFIG_DIR` (agp names accounts itself), the
    /// repository variables, or a terminal to read.
    fn plain(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        crate::proc::without_repo_env(&mut cmd);
        cmd.env_remove("CLAUDE_CONFIG_DIR")
            .stdin(std::process::Stdio::null());
        cmd
    }

    /// `agp status --json`, bounded. Ok(None) when agp isn't installed.
    pub fn status(&self) -> Result<Option<Vec<Account>>> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["status", "--json"]);
        let Some(out) = self.run(&mut cmd, "status --json")? else {
            return Ok(None);
        };
        Ok(Some(parse_status(&String::from_utf8_lossy(&out.stdout))?))
    }

    /// The Claude Code config folder of account `id`, as `agp exec` sets `CLAUDE_CONFIG_DIR`,
    /// made absolute; None is the default account (`~/.claude`).
    pub fn config_dir(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut cmd = self.exec(id);
        cmd.args(["sh", "-c", "printf %s \"${CLAUDE_CONFIG_DIR-}\""]);
        let shown = format!("exec {id}");
        let Some(out) = self.run(&mut cmd, &shown)? else {
            bail!("agp isn't installed");
        };
        Ok(parse_config_dir(&String::from_utf8_lossy(&out.stdout)))
    }

    /// `agp exec <id> --`, without this process's `CLAUDE_CONFIG_DIR`: agp leaves it unset for
    /// the default account (§1), so one inherited from the shell (a project on another account)
    /// would pass for the default account's folder.
    fn exec(&self, id: &str) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["exec", id, "--"]).env_remove("CLAUDE_CONFIG_DIR");
        cmd
    }

    /// Runs agp, bounded; None when the binary isn't there, an error on a timeout or a failure.
    fn run(&self, cmd: &mut Command, shown: &str) -> Result<Option<std::process::Output>> {
        self.run_within(cmd, shown, TIMEOUT)
    }

    fn run_within(
        &self,
        cmd: &mut Command,
        shown: &str,
        timeout: Duration,
    ) -> Result<Option<std::process::Output>> {
        let out = match crate::proc::output_with_timeout(cmd, timeout) {
            Ok(Some(out)) => out,
            Ok(None) => bail!("`agp {shown}` took longer than {}s", timeout.as_secs()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("running {}", self.bin.display())),
        };
        if !out.status.success() {
            bail!(
                "`agp {shown}` failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Some(out))
    }
}

/// `agp --help` lists the `bg` command.
fn help_has_bg(help: &str) -> bool {
    help.lines().any(|l| l.trim_start().starts_with("agp bg"))
}

/// `agp exec`'s `CLAUDE_CONFIG_DIR`: empty is the default account; a path is made absolute and
/// canonical when it exists, like `claude::env_account`.
pub fn parse_config_dir(stdout: &str) -> Option<PathBuf> {
    let dir = stdout.trim();
    if dir.is_empty() {
        return None;
    }
    let dir = PathBuf::from(dir);
    Some(
        dir.canonicalize()
            .or_else(|_| std::path::absolute(&dir))
            .unwrap_or(dir),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Account> {
        parse_status(include_str!("../tests/fixtures/agp/status.json")).unwrap()
    }

    fn ids(accounts: &[&Account]) -> Vec<String> {
        accounts.iter().map(|a| a.id.clone()).collect()
    }

    #[test]
    fn parses_the_status_fixture() {
        let all = fixture();
        assert_eq!(
            all.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["default", "carlos", "cpb", "cpe", "laura", "pb5"]
        );
        let default = &all[0];
        assert_eq!(
            (default.label.as_str(), default.in_pool, default.limited),
            ("Main", false, false)
        );
        assert_eq!(default.score, None);
        assert_eq!(
            (default.five_hour, default.seven_day),
            (Some(1.0), Some(48.0))
        );
        let cpb = &all[2];
        assert_eq!((cpb.label.as_str(), cpb.in_pool), ("PB2", true));
        assert_eq!(cpb.score, Some(0.5865));
        // A weekly window agp doesn't know.
        assert_eq!((all[1].five_hour, all[1].seven_day), (Some(82.0), None));
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn codex_accounts_and_limits() {
        let json = r#"{"views": [
            {"profile_id": "gpt", "label": "GPT", "identity": {"tool": "codex"}, "in_pool": true,
             "limited": null, "score": 0.9},
            {"profile_id": "busy", "label": "B", "in_pool": true, "limited": {"until": 5},
             "score": 0.8, "usage": null},
            {"profile_id": "bare", "in_pool": true},
            {"profile_id": "out", "in_pool": true, "token": "Missing", "limited": null},
            {"profile_id": "idle", "in_pool": true, "token": {"Expired": {"since": 5}}}
        ]}"#;
        let all = parse_status(json).unwrap();
        assert_eq!(
            all.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["busy", "bare", "out", "idle"]
        );
        assert!(all[0].limited);
        assert_eq!((all[0].five_hour, all[1].limited), (None, false));
        assert_eq!(all[1].shown(), "bare");
        // Only a missing token is no login: an agp that says nothing, and an expired token
        // (the next run refreshes it), both count as one.
        let logged_in: Vec<bool> = all.iter().map(|a| a.logged_in).collect();
        assert_eq!(logged_in, [true, true, false, true]);

        // A limit is an object (or true); false and other values aren't one.
        let limited = |value: &str| {
            let json = format!(r#"{{"views": [{{"profile_id": "a", "limited": {value}}}]}}"#);
            parse_status(&json).unwrap()[0].limited
        };
        assert!(limited(r#"{"until": 5}"#) && limited("true"));
        assert!(!limited("false") && !limited("null") && !limited("0") && !limited(r#""""#));
    }

    #[test]
    fn the_pool_is_allowed_and_in_pool() {
        let all = fixture();
        let auto = vec![AUTO.to_string()];
        assert_eq!(
            ids(&pool(&all, &auto)),
            ["carlos", "cpb", "cpe", "laura", "pb5"]
        );
        let listed = vec!["cpe".to_string(), "default".to_string(), "nope".to_string()];
        assert_eq!(
            ids(&pool(&all, &listed)),
            ["cpe"],
            "default isn't in agp's pool"
        );
        assert!(pool(&all, &[]).is_empty());
    }

    #[test]
    fn usable_accounts_rank_by_score_under_the_limits() {
        let mut all = fixture();
        let auto = vec![AUTO.to_string()];
        assert_eq!(
            ids(&usable(&pool(&all, &auto), None)),
            ["pb5", "cpe", "cpb", "laura", "carlos"]
        );
        // carlos is at 82% of its 5-hour window, laura at 22%.
        assert_eq!(
            ids(&usable(&pool(&all, &auto), Some(80))),
            ["pb5", "cpe", "cpb", "laura"]
        );
        assert_eq!(
            ids(&usable(&pool(&all, &auto), Some(22))),
            ["pb5", "cpe", "cpb"],
            "at the limit counts as over"
        );
        all[5].limited = true;
        all[3].score = None;
        assert_eq!(
            ids(&usable(&pool(&all, &auto), None)),
            ["cpb", "laura", "carlos", "cpe"]
        );
        // agp has no login for laura.
        all[4].logged_in = false;
        assert_eq!(
            ids(&usable(&pool(&all, &auto), None)),
            ["cpb", "carlos", "cpe"]
        );
        all[4].logged_in = true;
        assert_eq!(find(&pool(&all, &auto), "PB2").unwrap().id, "cpb");
        assert_eq!(find(&pool(&all, &auto), "cpb").unwrap().id, "cpb");
        assert!(find(&pool(&all, &auto), "default").is_none());
        assert_eq!(
            listed(&pool(&all, &["cpb".into(), "cpe".into()])),
            "cpb (PB2), cpe (PB3)"
        );
    }

    #[test]
    fn config_dirs_and_commands() {
        assert_eq!(parse_config_dir(""), None);
        assert_eq!(parse_config_dir("  \n"), None);
        assert_eq!(
            parse_config_dir("/nonexistent/.claude-cpb"),
            Some(PathBuf::from("/nonexistent/.claude-cpb"))
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            parse_config_dir(&dir.path().to_string_lossy()),
            Some(dir.path().canonicalize().unwrap())
        );
        assert_eq!(
            trust_command("cpb", Path::new("/w/my repo")),
            "cd '/w/my repo' && agp exec cpb -- claude"
        );
    }

    #[test]
    fn a_missing_agp_is_not_installed() {
        let agp = Agp::new("/nonexistent/agp".into());
        assert_eq!(agp.status().unwrap(), None);
        assert!(agp.config_dir("cpb").is_err());
    }

    #[test]
    fn bg_sessions_parse_with_their_moves() {
        let json = r#"{"sessions": [{"session_id": "f1de9de1-1233-4c43-8a68-fc73f11e1f17",
            "short_id": "f1de9de1", "name": "demo-x", "profile": "laura", "label": "PB4",
            "config_dir": "/Users/x/.claude-laura", "cwd": "/r", "on_limit": "switch",
            "state": "watching", "limit": null,
            "moves": [{"at": 1793500100, "from": "cpb", "from_label": "PB2",
                       "from_config_dir": "/Users/x/.claude-cpb", "to": "laura", "to_label": "PB4",
                       "to_config_dir": "/Users/x/.claude-laura", "reason": "5-hour limit"}],
            "previous": [{"profile": "cpb", "label": "PB2", "config_dir": "/Users/x/.claude-cpb"}],
            "watcher_pid": 1234, "watching": true, "updated_at": 1793500100},
            {"session_id": "s2", "profile": "default", "config_dir": null, "state": "moving",
             "limit": {"text": "You've hit your 5-hour limit", "since": 1, "resets_at": null}}]}"#;
        let all = parse_bg(json).unwrap();
        assert_eq!(all.len(), 2);
        let s = &all[0];
        assert_eq!(
            (s.short_id.as_deref(), s.shown(), s.moving()),
            (Some("f1de9de1"), "PB4", false)
        );
        assert_eq!(s.account(), Some(PathBuf::from("/Users/x/.claude-laura")));
        assert_eq!(s.moves.len(), 1);
        assert_eq!(
            (
                s.moves[0].from_shown(),
                s.moves[0].to_shown(),
                s.moves[0].at
            ),
            ("PB2", "PB4", 1793500100)
        );
        let t = &all[1];
        assert!(t.moving());
        assert_eq!((t.account(), t.shown()), (None, "default"));
        assert_eq!(
            t.limit.as_ref().map(|l| (l.text.as_str(), l.resets_at)),
            Some(("You've hit your 5-hour limit", None))
        );
        assert!(parse_bg("{}").unwrap().is_empty());
        assert!(parse_bg("not json").is_err());
    }

    #[test]
    fn only_an_agp_whose_help_lists_bg_has_it() {
        let new = "Usage:\n  agp [options] [--] [claude args]\n  agp bg [--json]  background sessions\n  agp bg watch <session>\n";
        assert!(help_has_bg(new));
        assert!(!help_has_bg("Usage:\n  agp status\nsee also agp bg\n"));
        assert!(!help_has_bg(""));
        assert!(!help_has_bg("Usage:\n  agp status [--json]\n"));
    }

    #[test]
    fn launch_and_run_options() {
        let mut threads = crate::config::ProjectConfig::new("Demo", "demo", None, vec![]).threads;
        threads.accounts = vec![AUTO.to_string()];
        threads.max_usage = Some(85);
        let run = RunOptions::from_settings(&threads);
        assert_eq!(
            launch_args("cpb", &run),
            [
                "--as",
                "cpb",
                "--on-limit",
                "switch",
                "--max-usage",
                "85",
                "--"
            ]
            .map(OsString::from)
        );
        threads.accounts = vec!["cpb".into(), "laura".into()];
        threads.on_limit = OnLimit::Stay;
        threads.max_usage = None;
        assert_eq!(
            RunOptions::from_settings(&threads).args(),
            ["--on-limit", "stay", "--accounts", "cpb,laura"]
        );
        // Threads kept in the project's account have nowhere to switch to.
        threads.accounts = vec![];
        threads.on_limit = OnLimit::Switch;
        assert_eq!(RunOptions::from_settings(&threads).on_limit, OnLimit::Wait);
    }
}
