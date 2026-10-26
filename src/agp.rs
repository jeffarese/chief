//! The `agp` CLI (agent-prism), which keeps the user's Claude Code accounts (ARCHITECTURE §1,
//! §3, §7). chief reads the accounts and their usage from `agp status --json` and ranks them
//! itself: `agp pick` ignores `--accounts` and `--exclude`. An agp account is a Claude Code
//! config folder, which `agp exec <id>` names in `CLAUDE_CONFIG_DIR`. `CHIEF_AGP_BIN` overrides
//! the binary.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// `[threads] accounts` entry meaning every account in agp's pool.
pub const AUTO: &str = "auto";
const TIMEOUT: Duration = Duration::from_secs(15);

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
                limited: !v.limited.is_null(),
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

/// The pool accounts that can take a thread now (not limited, under `max_usage`), best score
/// first; accounts without a score come last, and ties keep agp's order.
pub fn usable<'a>(pool: &[&'a Account], max_usage: Option<u32>) -> Vec<&'a Account> {
    let mut usable: Vec<&Account> = pool
        .iter()
        .copied()
        .filter(|a| !a.limited && !a.over(max_usage))
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
        let mut cmd = Command::new(&self.bin);
        cmd.args([
            "exec",
            id,
            "--",
            "sh",
            "-c",
            "printf %s \"${CLAUDE_CONFIG_DIR-}\"",
        ]);
        let shown = format!("exec {id}");
        let Some(out) = self.run(&mut cmd, &shown)? else {
            bail!("agp isn't installed");
        };
        Ok(parse_config_dir(&String::from_utf8_lossy(&out.stdout)))
    }

    /// `agp exec <id> -- <claude>` in `cwd` on this terminal, for the trust prompt.
    pub fn run_interactive(&self, id: &str, claude: &Path, cwd: &Path) -> Result<ExitStatus> {
        Command::new(&self.bin)
            .args(["exec", id, "--"])
            .arg(claude)
            .current_dir(cwd)
            .status()
            .with_context(|| format!("running agp exec {id} in {}", cwd.display()))
    }

    /// Runs agp, bounded; None when the binary isn't there, an error on a timeout or a failure.
    fn run(&self, cmd: &mut Command, shown: &str) -> Result<Option<std::process::Output>> {
        let out = match crate::proc::output_with_timeout(cmd, TIMEOUT) {
            Ok(Some(out)) => out,
            Ok(None) => bail!("`agp {shown}` took longer than {}s", TIMEOUT.as_secs()),
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
            {"profile_id": "bare", "in_pool": true}
        ]}"#;
        let all = parse_status(json).unwrap();
        assert_eq!(
            all.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["busy", "bare"]
        );
        assert!(all[0].limited);
        assert_eq!((all[0].five_hour, all[1].limited), (None, false));
        assert_eq!(all[1].shown(), "bare");
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
}
