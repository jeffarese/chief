//! A project's sessions may run in several Claude Code accounts (ARCHITECTURE §3, §5): the
//! project's own, and the ones `start_thread` put threads in. Each account's supervisor lists
//! only its own sessions (§1), so a sync takes one `claude agents --json --all` per account in
//! use and applies each to that account's rows only (`registry::sync_in`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use rusqlite::Connection;

use crate::claude::{self, AgentRow, Claude};
use crate::db::{self, SessionRow};
use crate::registry;

/// One account's `claude agents`.
#[derive(Debug, Clone)]
pub struct Listing {
    /// `claude` in that account.
    pub claude: Claude,
    /// Read just before `claude agents` ran.
    pub taken_at: i64,
    /// The sessions, or why `claude agents` failed.
    pub agents: Result<Vec<AgentRow>, String>,
}

impl Listing {
    pub fn account(&self) -> Option<&Path> {
        self.claude.account.as_deref()
    }

    pub fn agents(&self) -> Result<&[AgentRow]> {
        self.agents.as_deref().map_err(|e| anyhow!("{e}"))
    }
}

/// The listings of the accounts a project uses.
#[derive(Debug, Clone)]
pub struct Listings {
    /// The project's account.
    pub project: Option<PathBuf>,
    pub all: Vec<Listing>,
}

impl Listings {
    /// The project's account's listing, if it was taken.
    pub fn project(&self) -> Option<&Listing> {
        self.of(self.project.as_deref())
    }

    pub fn of(&self, account: Option<&Path>) -> Option<&Listing> {
        self.all
            .iter()
            .find(|l| claude::same_account(l.account(), account))
    }

    /// The listing of the account `row` runs in, if it was taken.
    pub fn of_row(&self, row: &SessionRow) -> Option<&Listing> {
        self.of(row.account(self.project.as_deref()).as_deref())
    }

    /// Every failed listing, one each; the project's account's without naming it, as when it
    /// was the only one.
    pub fn error(&self) -> Option<String> {
        let lines: Vec<String> = self
            .all
            .iter()
            .filter_map(|l| {
                let e = l.agents.as_ref().err()?;
                Some(
                    if claude::same_account(l.account(), self.project.as_deref()) {
                        e.clone()
                    } else {
                        format!("in the account {}: {e}", claude::account_label(l.account()))
                    },
                )
            })
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
    }
}

/// The accounts a project's sessions run in: `project` (the project's), then the account of
/// each thread that isn't cleaned up, once each. A cleaned-up thread has no session left.
pub fn in_use(project: Option<&Path>, rows: &[SessionRow]) -> Vec<Option<PathBuf>> {
    let mut accounts = vec![project.map(Path::to_path_buf)];
    for row in rows.iter().filter(|r| r.cleaned_at.is_none()) {
        let account = row.account(project);
        if !accounts
            .iter()
            .any(|a| claude::same_account(a.as_deref(), account.as_deref()))
        {
            accounts.push(account);
        }
    }
    accounts
}

/// `claude agents` in each account, all at once: a slow supervisor holds up none of the others.
/// `project` is `claude` in the project's account; the others use its binary.
pub fn list(
    project: &Claude,
    accounts: &[Option<PathBuf>],
    timeout: Duration,
    now: fn() -> i64,
) -> Listings {
    let all = std::thread::scope(|scope| {
        let handles: Vec<_> = accounts
            .iter()
            .map(|account| {
                let claude = project.in_account(account.as_deref());
                scope.spawn(move || {
                    let taken_at = now();
                    let agents = claude.agents_within(timeout).map_err(|e| format!("{e:#}"));
                    Listing {
                        claude,
                        taken_at,
                        agents,
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a `claude agents` thread panicked"))
            .collect::<Vec<_>>()
    });
    Listings {
        project: project.account.clone(),
        all,
    }
}

/// Writes each listing that was taken into its account's rows of the project.
pub fn sync<'a>(
    conn: &Connection,
    slug: &str,
    project: Option<&Path>,
    listings: impl IntoIterator<Item = &'a Listing>,
    now: i64,
) -> Result<()> {
    for l in listings {
        let Ok(agents) = &l.agents else {
            continue;
        };
        let listing = registry::Listing {
            account: l.account(),
            agents,
            taken_at: l.taken_at,
            claude_dir: &l.claude.config_dir,
        };
        registry::sync_in(conn, slug, project, &listing, now)?;
    }
    Ok(())
}

/// Lists every account the project uses (`in_use`) and syncs its rows. A failed listing leaves
/// its account's rows as they were; `Listings::error` says which failed.
pub fn refresh(
    conn: &Connection,
    slug: &str,
    project: &Claude,
    timeout: Duration,
    now: fn() -> i64,
) -> Result<Listings> {
    let rows = db::list_sessions(conn, slug, None)?;
    let accounts = in_use(project.account.as_deref(), &rows);
    refresh_accounts(conn, slug, project, &accounts, timeout, now)
}

/// `refresh` for these accounts only.
pub fn refresh_accounts(
    conn: &Connection,
    slug: &str,
    project: &Claude,
    accounts: &[Option<PathBuf>],
    timeout: Duration,
    now: fn() -> i64,
) -> Result<Listings> {
    let listings = list(project, accounts, timeout, now);
    sync(conn, slug, project.account.as_deref(), &listings.all, now())?;
    Ok(listings)
}

/// Lists and syncs only the account `row` runs in; its listing's error is the error.
pub fn refresh_row(
    conn: &Connection,
    project: &Claude,
    row: &SessionRow,
    timeout: Duration,
    now: fn() -> i64,
) -> Result<Listing> {
    let account = row.account(project.account.as_deref());
    let listings = refresh_accounts(conn, &row.project_slug, project, &[account], timeout, now)?;
    let listing = listings.all.into_iter().next().expect("one account listed");
    listing.agents()?;
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Role;

    fn thread(name: &str, account: Option<(&str, Option<&str>)>) -> SessionRow {
        let mut r = SessionRow::new(name, "demo", Role::Thread, 0);
        if let Some((id, dir)) = account {
            r.account_id = Some(id.into());
            r.account_dir = dir.map(str::to_string);
        }
        r
    }

    #[test]
    fn accounts_in_use() {
        let project = Path::new("/accounts/carlos");
        let mut cleaned = thread("c", Some(("pb5", Some("/accounts/pb5"))));
        cleaned.cleaned_at = Some(1);
        let rows = vec![
            SessionRow::new("chief-demo", "demo", Role::Coordinator, 0),
            thread("a", None),
            thread("b", Some(("cpb", Some("/accounts/cpb")))),
            thread("b2", Some(("cpb", Some("/accounts/cpb")))),
            thread("d", Some(("default", None))),
            thread("e", Some(("carlos", Some("/accounts/carlos")))),
            cleaned,
        ];
        assert_eq!(
            in_use(Some(project), &rows),
            [
                Some(project.to_path_buf()),
                Some(PathBuf::from("/accounts/cpb")),
                None,
            ]
        );
        assert_eq!(in_use(None, &rows[..2]), [None]);
    }

    #[test]
    fn errors_name_the_other_accounts() {
        let listing = |account: Option<&str>, agents: Result<Vec<AgentRow>, String>| Listing {
            claude: Claude::with_bin("claude".into(), account.map(PathBuf::from)),
            taken_at: 0,
            agents,
        };
        let ok = Listings {
            project: None,
            all: vec![
                listing(None, Ok(vec![])),
                listing(Some("/a/cpb"), Ok(vec![])),
            ],
        };
        assert_eq!(ok.error(), None);
        let failed = Listings {
            project: None,
            all: vec![
                listing(Some("/a/cpb"), Err("timeout".into())),
                listing(None, Err("boom".into())),
            ],
        };
        assert_eq!(
            failed.error().as_deref(),
            Some("in the account /a/cpb: timeout; boom")
        );
        let row = thread("b", Some(("cpb", Some("/a/cpb"))));
        assert_eq!(
            failed.of_row(&row).unwrap().account(),
            Some(Path::new("/a/cpb"))
        );
        assert_eq!(failed.of_row(&thread("a", None)).unwrap().account(), None);
        assert_eq!(failed.project().unwrap().account(), None);
    }
}
