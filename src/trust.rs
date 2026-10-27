//! Folder trust (ARCHITECTURE §3 "Trust"): which folders a project's sessions need trusted, and
//! marking them trusted after the user said yes. Shared by `init`, `chief trust`, the launcher's
//! wizard and the board's settings.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agp::Agp;
use crate::claude::{self, Claude};
use crate::config::ProjectConfig;
use crate::placement;

/// A folder an account doesn't trust yet; background sessions there are refused (§1).
#[derive(Debug, Clone, PartialEq)]
pub struct Need {
    pub folder: PathBuf,
    /// That account's `.claude.json`.
    pub config: PathBuf,
    /// The agp label of a pool account; None is the project's own account.
    pub account: Option<String>,
}

impl Need {
    /// `/path/to/repo`, or `/path/to/repo (in PB2)` for a pool account.
    pub fn shown(&self) -> String {
        match &self.account {
            Some(label) => format!("{} (in {label})", self.folder.display()),
            None => self.folder.display().to_string(),
        }
    }

    pub fn mark(&self) -> Result<()> {
        claude::mark_trusted(&self.config, &self.folder)
    }
}

impl From<&placement::Untrusted> for Need {
    fn from(u: &placement::Untrusted) -> Self {
        Need {
            folder: u.folder.clone(),
            config: claude::claude_config_path(u.dir.as_deref()),
            account: Some(u.label.clone()),
        }
    }
}

/// What a project needs trusted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Check {
    /// chief's own project folder, when its account doesn't trust it: the coordinator's cwd.
    /// chief marks it without asking, since chief made it.
    pub project: Option<Need>,
    /// The user's repos, in the project's account and in every agp pool account (§7 "Thread
    /// account") but the project's own: these are the user's to trust.
    pub repos: Vec<Need>,
    /// The pool accounts checked (labels), for "threads can also run in …".
    pub pool: Vec<String>,
    /// Accounts or folders chief couldn't check.
    pub errors: Vec<String>,
}

pub fn check(config: &ProjectConfig, dir: &Path, claude: &Claude) -> Result<Check> {
    let trust = claude.trust_config();
    let mut out = Check::default();
    if !claude::is_trusted_in(&trust, dir)? {
        out.project = Some(Need {
            folder: dir.to_path_buf(),
            config: trust.clone(),
            account: None,
        });
    }
    for repo in &config.repos {
        if !claude::is_trusted_in(&trust, &repo.path)? {
            out.repos.push(Need {
                folder: repo.path.clone(),
                config: trust.clone(),
                account: None,
            });
        }
    }
    let repos: Vec<PathBuf> = config.repos.iter().map(|r| r.path.clone()).collect();
    match placement::pool_trust(&config.threads, &Agp::from_env(), &repos) {
        Ok(found) => {
            let project = config.claude_config_dir.as_deref();
            out.repos.extend(
                found
                    .untrusted
                    .iter()
                    .filter(|u| !claude::same_account(u.dir.as_deref(), project))
                    .map(Need::from),
            );
            out.pool = found.checked;
            out.errors.extend(found.errors);
        }
        Err(e) => out
            .errors
            .push(format!("which repos agp's accounts trust: {e:#}")),
    }
    Ok(out)
}

/// Marks each need, returning one line per folder: `Trusted …` or why not.
pub fn mark_all<'a>(needs: impl IntoIterator<Item = &'a Need>) -> Vec<Result<String, String>> {
    needs
        .into_iter()
        .map(|n| match n.mark() {
            Ok(()) => Ok(format!("Trusted {}.", n.shown())),
            Err(e) => Err(format!("Couldn't trust {}: {e:#}", n.shown())),
        })
        .collect()
}
