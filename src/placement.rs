//! Which Claude Code account a new thread runs in (ARCHITECTURE §7 "Thread account"): the best
//! account of agp's pool that `[threads] accounts` allows and that trusts the repository, else
//! the project's own account, as before agp. Chosen once, at `start_thread`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::agp::{self, Agp};
use crate::claude::{self, Claude, ClaudeError};
use crate::config::ThreadSettings;

/// Where a new thread runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// The agp account chosen; None: the project's account.
    pub chosen: Option<Chosen>,
    /// What the coordinator should know about the choice: accounts skipped, a fallback.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub id: String,
    pub label: String,
    /// Its config folder (`CLAUDE_CONFIG_DIR`); None is the default account.
    pub dir: Option<PathBuf>,
}

impl Placement {
    fn project(note: Option<String>) -> Placement {
        Placement { chosen: None, note }
    }

    /// The account the thread runs in; `project` is the project's.
    pub fn account(&self, project: Option<&Path>) -> Option<PathBuf> {
        match &self.chosen {
            Some(c) => c.dir.clone(),
            None => project.map(Path::to_path_buf),
        }
    }
}

/// What `place` reads: the project's settings and account, and agp.
pub struct Inputs<'a> {
    pub threads: &'a ThreadSettings,
    pub agp: &'a Agp,
    /// `claude` in the project's account.
    pub project: &'a Claude,
    /// The project's account's trust file (`Claude::trust_config`, or a test's).
    pub project_trust: &'a Path,
}

impl Inputs<'_> {
    /// The trust file of `account`: the project's own when it is the project's account.
    fn trust_file(&self, account: Option<&Path>) -> PathBuf {
        if claude::same_account(account, self.project.account.as_deref()) {
            self.project_trust.to_path_buf()
        } else {
            claude::claude_config_path(account)
        }
    }

    /// Whether `account` trusts `repo`: None if it does, else why not, with the fix.
    fn untrusted(&self, account: &agp::Account, dir: Option<&Path>, repo: &Path) -> Option<String> {
        match claude::is_trusted_in(&self.trust_file(dir), repo) {
            Ok(true) => None,
            Ok(false) => Some(format!(
                "{} doesn't trust {} yet (run `{}` once and accept the trust prompt)",
                account.shown(),
                repo.display(),
                agp::trust_command(&account.id, repo)
            )),
            Err(e) => Some(format!("{}: {e:#}", account.shown())),
        }
    }

    /// The project's account, which must trust `repo` (as before agp).
    fn fallback(&self, repo: &Path, note: Option<String>) -> Result<Placement> {
        if !claude::is_trusted_in(self.project_trust, repo)? {
            return Err(ClaudeError::NotTrusted(repo.to_path_buf()).into());
        }
        Ok(Placement::project(note))
    }
}

/// The account for a thread in `repo`; `pinned` is `start_thread`'s `account` (an agp id or
/// label from the pool). Without agp, or with no pool account allowed, the project's account,
/// silently. Otherwise the usable pool accounts (`agp::usable`) best first, skipping any that
/// doesn't trust `repo` or whose folder agp can't name; the project's account when none is left,
/// with a note why.
pub fn place(inputs: &Inputs, repo: &Path, pinned: Option<&str>) -> Result<Placement> {
    let pinned = pinned.map(str::trim).filter(|p| !p.is_empty());
    let allowed = &inputs.threads.accounts;
    if allowed.is_empty() {
        if let Some(p) = pinned {
            bail!(
                "can't run it in {p}: `[threads] accounts` in project.toml is empty, so threads \
                 run in the project's account"
            );
        }
        return inputs.fallback(repo, None);
    }
    let all = match inputs.agp.status() {
        Ok(Some(all)) => all,
        Ok(None) => {
            if let Some(p) = pinned {
                bail!(
                    "can't run it in {p}: agp isn't installed, so threads run in the project's account"
                );
            }
            return inputs.fallback(repo, None);
        }
        Err(e) => {
            if let Some(p) = pinned {
                bail!("can't run it in {p}: {e:#}");
            }
            let note = format!("{e:#}; it runs in the project's account");
            return inputs.fallback(repo, Some(note));
        }
    };
    let pool = agp::pool(&all, allowed);
    if let Some(p) = pinned {
        return place_pinned(inputs, &pool, p, repo);
    }
    if pool.is_empty() {
        return inputs.fallback(repo, None);
    }
    let max = inputs.threads.max_usage;
    let usable = agp::usable(&pool, max);
    if usable.is_empty() {
        let why = match max {
            Some(max) => format!("limited or at {max}% usage or more"),
            None => "limited".to_string(),
        };
        let note = format!(
            "Every pool account ({}) is {why} right now; it runs in the project's account.",
            agp::listed(&pool)
        );
        return inputs.fallback(repo, Some(note));
    }
    let mut skipped = Vec::new();
    for account in usable {
        let dir = match inputs.agp.config_dir(&account.id) {
            Ok(dir) => dir,
            Err(e) => {
                skipped.push(format!("{}: {e:#}", account.shown()));
                continue;
            }
        };
        if let Some(why) = inputs.untrusted(account, dir.as_deref(), repo) {
            skipped.push(why);
            continue;
        }
        let note = (!skipped.is_empty()).then(|| {
            format!(
                "It runs in {}; skipped: {}.",
                account.shown(),
                skipped.join("; ")
            )
        });
        return Ok(Placement {
            chosen: Some(chosen(account, dir)),
            note,
        });
    }
    let note = format!(
        "No pool account could take it ({}); it runs in the project's account.",
        skipped.join("; ")
    );
    inputs.fallback(repo, Some(note))
}

/// The account the user asked for, if the pool has it and it trusts `repo`; limited or busy is
/// only a note (the thread waits for the reset).
fn place_pinned(
    inputs: &Inputs,
    pool: &[&agp::Account],
    pinned: &str,
    repo: &Path,
) -> Result<Placement> {
    let Some(account) = agp::find(pool, pinned) else {
        bail!(
            "{pinned} is not an account in this project's pool (pool: {})",
            agp::listed(pool)
        );
    };
    let dir = inputs.agp.config_dir(&account.id)?;
    if let Some(why) = inputs.untrusted(account, dir.as_deref(), repo) {
        bail!("can't run it in {}: {why}", account.shown());
    }
    let note = if account.limited {
        Some(format!(
            "{} is limited right now; the thread waits for its limit to reset.",
            account.shown()
        ))
    } else if account.over(inputs.threads.max_usage) {
        Some(format!(
            "{} is over max_usage right now; it runs there because you asked.",
            account.shown()
        ))
    } else {
        None
    };
    Ok(Placement {
        chosen: Some(chosen(account, dir)),
        note,
    })
}

fn chosen(account: &agp::Account, dir: Option<PathBuf>) -> Chosen {
    Chosen {
        id: account.id.clone(),
        label: account.shown().to_string(),
        dir,
    }
}

/// A pool account that doesn't trust a repository yet, so `start_thread` skips it there (§3).
#[derive(Debug, Clone, PartialEq)]
pub struct Untrusted {
    pub id: String,
    pub label: String,
    /// Its config folder; None is the default account.
    pub dir: Option<PathBuf>,
    pub folder: PathBuf,
}

impl Untrusted {
    /// Whether the account trusts the folder now.
    pub fn trusted(&self) -> Result<bool> {
        claude::is_trusted_in(
            &claude::claude_config_path(self.dir.as_deref()),
            &self.folder,
        )
    }
}

/// What checking the pool's trust found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolTrust {
    /// The pool accounts checked, by label; empty without agp or with no pool account allowed.
    pub checked: Vec<String>,
    pub untrusted: Vec<Untrusted>,
    /// Accounts chief couldn't check, with why.
    pub errors: Vec<String>,
}

/// Every pool account `threads` allows, against every repository: the (account, folder) pairs
/// where `start_thread` would skip the account for want of trust. Limited accounts count: their
/// limits reset. An error only when `agp status` fails.
pub fn pool_trust(threads: &ThreadSettings, agp: &Agp, repos: &[PathBuf]) -> Result<PoolTrust> {
    let mut out = PoolTrust::default();
    if threads.accounts.is_empty() || repos.is_empty() {
        return Ok(out);
    }
    let Some(all) = agp.status()? else {
        return Ok(out);
    };
    for account in agp::pool(&all, &threads.accounts) {
        let label = account.shown().to_string();
        let dir = match agp.config_dir(&account.id) {
            Ok(dir) => dir,
            Err(e) => {
                out.errors.push(format!("{label}: {e:#}"));
                continue;
            }
        };
        let trust = claude::claude_config_path(dir.as_deref());
        for folder in repos {
            match claude::is_trusted_in(&trust, folder) {
                Ok(true) => {}
                Ok(false) => out.untrusted.push(Untrusted {
                    id: account.id.clone(),
                    label: label.clone(),
                    dir: dir.clone(),
                    folder: folder.clone(),
                }),
                Err(e) => out.errors.push(format!("{label}: {e:#}")),
            }
        }
        out.checked.push(label);
    }
    Ok(out)
}
