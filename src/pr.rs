//! Pull request status from the GitHub CLI (ARCHITECTURE §10, §12). `CHIEF_GH_BIN` overrides the
//! binary. The board calls `refresh_project` every 30 s; `chief gc` calls `poll_project`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cleanup::{self, Ctx, Mode, Outcome};
use crate::config::{Cleanup, ProjectConfig};
use crate::db::{self, SessionRow};
use crate::registry::{self, PrInfo};
use crate::state::Role;

const GH_TIMEOUT: Duration = Duration::from_secs(10);
/// A resolved thread's PR is still polled this long after its last activity, so a thread resolved
/// before its PR merged still gets cleaned once it does.
const RESOLVED_POLL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub struct Gh {
    pub bin: PathBuf,
    /// Why PR status is off (no `gh`, not logged in, no GitHub remote), until taken once.
    hint: Mutex<Option<String>>,
    hinted: AtomicBool,
}

impl Gh {
    pub fn new(bin: PathBuf) -> Self {
        Gh {
            bin,
            hint: Mutex::new(None),
            hinted: AtomicBool::new(false),
        }
    }

    pub fn from_env() -> Self {
        let bin = std::env::var_os("CHIEF_GH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("gh"));
        Gh::new(bin)
    }

    /// The PR whose head is `branch`, run in `repo_path`. A missing `gh`, a `gh` that isn't
    /// logged in, or a repository without a GitHub remote is `Ok(None)` plus a hint
    /// (`take_hint`); other failures are errors.
    pub fn poll(&self, repo_path: &Path, branch: &str) -> Result<Option<PrInfo>> {
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(repo_path).args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "url,state,isDraft,reviewDecision",
            "--limit",
            "1",
        ]);
        let out = match crate::proc::output_with_timeout(&mut cmd, GH_TIMEOUT) {
            Ok(Some(out)) => out,
            Ok(None) => bail!("`gh pr list` took longer than {}s", GH_TIMEOUT.as_secs()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.set_hint(
                    "GitHub CLI (gh) isn't installed, so chief can't see pull requests: install \
                     it and run gh auth login."
                        .to_string(),
                );
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("running {}", self.bin.display())),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if let Some(hint) = unavailable_hint(&stderr, repo_path) {
                self.set_hint(hint);
                return Ok(None);
            }
            bail!(
                "`gh pr list --head {branch}` failed ({}): {stderr}",
                out.status
            );
        }
        parse_pr_list(&String::from_utf8_lossy(&out.stdout))
    }

    /// The first hint, once per `Gh`; None afterwards.
    pub fn take_hint(&self) -> Option<String> {
        self.hint.lock().ok()?.take()
    }

    fn set_hint(&self, hint: String) {
        if !self.hinted.swap(true, Ordering::SeqCst)
            && let Ok(mut slot) = self.hint.lock()
        {
            *slot = Some(hint);
        }
    }
}

/// gh errors that mean "no PR status here", not "something broke".
fn unavailable_hint(stderr: &str, repo_path: &Path) -> Option<String> {
    let lower = stderr.to_lowercase();
    if lower.contains("gh auth login") {
        return Some(
            "gh isn't logged in, so chief can't see pull requests: run gh auth login.".into(),
        );
    }
    let no_remote = [
        "no git remotes",
        "none of the git remotes",
        "not a git repository",
        "could not determine base repo",
    ];
    no_remote.iter().any(|m| lower.contains(m)).then(|| {
        format!(
            "{} has no GitHub remote, so chief shows no pull requests for it.",
            repo_path.display()
        )
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
}

/// `gh pr list --json url,state,isDraft,reviewDecision` output → the first PR.
pub fn parse_pr_list(json: &str) -> Result<Option<PrInfo>> {
    let prs: Vec<GhPr> = serde_json::from_str(json).context("parsing `gh pr list` output")?;
    Ok(prs.into_iter().next().map(|p| PrInfo {
        url: p.url,
        state: p.state,
        draft: p.is_draft,
        review: p.review_decision.filter(|r| !r.is_empty()),
    }))
}

/// Threads whose PR is worth asking about: they work in their own worktree (a thread still in
/// the repository checkout reports that checkout's branch, often `main`) and aren't cleaned.
/// Resolved ones only while their PR can still change and they were active lately.
fn wants_poll(row: &SessionRow, now: i64) -> bool {
    let in_worktree = row
        .cwd
        .as_deref()
        .is_some_and(|c| cleanup::worktree_root(Path::new(c)).is_some());
    if row.role != Role::Thread.as_str()
        || row.cleaned_at.is_some()
        || row.branch.is_none()
        || row.repo_path.is_none()
        || !in_worktree
    {
        return false;
    }
    !row.resolved
        || (!matches!(row.pr_state.as_deref(), Some("MERGED" | "CLOSED"))
            && now - row.last_activity_at < RESOLVED_POLL_MS)
}

/// What a refresh did, for the board's status line and `chief gc`.
#[derive(Debug, Default)]
pub struct Refresh {
    pub polled: usize,
    pub cleaned: Vec<String>,
    /// (thread, why) for threads a merged-PR cleanup left in place.
    pub blocked: Vec<(String, String)>,
    /// One line per failed poll or cleanup.
    pub errors: Vec<String>,
    /// Why PR status is off, the first time it is.
    pub hint: Option<String>,
}

/// Polls and stores every relevant thread's PR (§12). Failures are collected, not returned.
pub fn poll_project(ctx: &Ctx, slug: &str) -> Result<Refresh> {
    let mut refresh = Refresh::default();
    let now = (ctx.now)();
    for row in db::list_sessions(ctx.conn, slug, Some(Role::Thread))? {
        if !wants_poll(&row, now) {
            continue;
        }
        let (Some(repo), Some(branch)) = (row.repo_path.as_deref(), row.branch.as_deref()) else {
            continue;
        };
        refresh.polled += 1;
        let stored = ctx
            .gh
            .poll(Path::new(repo), branch)
            .and_then(|pr| registry::set_pr(ctx.conn, &row.name, pr.as_ref(), (ctx.now)()));
        if let Err(e) = stored {
            refresh.errors.push(format!("{}: {e:#}", row.name));
        }
    }
    refresh.hint = ctx.gh.take_hint();
    Ok(refresh)
}

/// The board's 30 s refresh: `poll_project`, then with `cleanup = "merged"` a safe cleanup of
/// every resolved thread whose PR merged, that isn't working or waiting, and that no earlier
/// attempt found blocked (those wait for `chief gc`, `chief clean` or the board's `x`).
pub fn refresh_project(ctx: &Ctx, config: &ProjectConfig) -> Result<Refresh> {
    let mut refresh = poll_project(ctx, &config.slug)?;
    if config.threads.cleanup != Cleanup::Merged {
        return Ok(refresh);
    }
    for row in db::list_sessions(ctx.conn, &config.slug, Some(Role::Thread))? {
        let due = row.resolved
            && row.cleaned_at.is_none()
            && row.cleanup_blocked.is_none()
            && row.pr_state.as_deref() == Some("MERGED")
            && !cleanup::active(&row);
        if !due {
            continue;
        }
        match cleanup::clean(ctx, &row.name, Mode::Safe) {
            Ok(Outcome::Cleaned) => refresh.cleaned.push(row.name),
            Ok(Outcome::Blocked(why)) => refresh.blocked.push((row.name, why)),
            Ok(Outcome::AlreadyCleaned) => {}
            Err(e) => refresh.errors.push(format!("{}: {e:#}", row.name)),
        }
    }
    Ok(refresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_list_parsing() {
        let json = r#"[{"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":true,"reviewDecision":""}]"#;
        assert_eq!(
            parse_pr_list(json).unwrap(),
            Some(PrInfo {
                url: "https://github.com/o/r/pull/7".into(),
                state: "OPEN".into(),
                draft: true,
                review: None,
            })
        );
        let approved =
            r#"[{"url":"u","state":"MERGED","isDraft":false,"reviewDecision":"APPROVED"}]"#;
        assert_eq!(
            parse_pr_list(approved).unwrap().unwrap().review.as_deref(),
            Some("APPROVED")
        );
        assert_eq!(parse_pr_list("[]").unwrap(), None);
        assert!(parse_pr_list("nope").is_err());
    }

    #[test]
    fn unavailable_errors_are_hints() {
        let repo = Path::new("/w/r");
        for stderr in [
            "no git remotes found",
            "none of the git remotes configured for this repository point to a known GitHub host",
            "fatal: not a git repository (or any of the parent directories): .git",
        ] {
            let hint = unavailable_hint(stderr, repo).unwrap();
            assert!(hint.contains("/w/r has no GitHub remote"), "{hint}");
        }
        let auth = "To get started with GitHub CLI, please run:  gh auth login";
        assert!(unavailable_hint(auth, repo).unwrap().contains("logged in"));
        assert_eq!(unavailable_hint("HTTP 502: bad gateway", repo), None);
    }

    #[test]
    fn which_threads_are_polled() {
        let now = 30 * 24 * 60 * 60 * 1000;
        let base = || {
            let mut r = SessionRow::new("t", "p", Role::Thread, now);
            r.repo_path = Some("/w/r".into());
            r.cwd = Some("/w/r/.claude/worktrees/fix/src".into());
            r.branch = Some("worktree-fix".into());
            r
        };
        assert!(wants_poll(&base(), now));
        type Edit = fn(&mut SessionRow);
        let skipped: [(&str, Edit); 6] = [
            ("no branch", |r| r.branch = None),
            ("still in the checkout", |r| r.cwd = Some("/w/r".into())),
            ("cleaned", |r| r.cleaned_at = Some(1)),
            ("coordinator", |r| r.role = "coordinator".into()),
            ("resolved and merged", |r| {
                r.resolved = true;
                r.pr_state = Some("MERGED".into());
            }),
            ("resolved long ago", |r| {
                r.resolved = true;
                r.last_activity_at = 0;
            }),
        ];
        for (name, edit) in skipped {
            let mut r = base();
            edit(&mut r);
            assert!(!wants_poll(&r, now), "{name}");
        }
        let mut open = base();
        open.resolved = true;
        open.pr_state = Some("OPEN".into());
        assert!(wants_poll(&open, now), "resolved, PR still open");
    }
}
