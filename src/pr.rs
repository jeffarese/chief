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

    /// The PR whose head is `branch`, run in `repo_path`, opened from this repository no earlier
    /// than `since` (the thread row's `created_at`, see `parse_pr_list`). A missing `gh`, a `gh`
    /// that isn't logged in, or a repository without a GitHub remote is `Ok(None)` plus a hint
    /// (`take_hint`); other failures are errors.
    pub fn poll(&self, repo_path: &Path, branch: &str, since: i64) -> Result<Option<PrInfo>> {
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(repo_path).args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "url,state,isDraft,reviewDecision,createdAt,isCrossRepository",
            "--limit",
            "10",
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
        parse_pr_list(&String::from_utf8_lossy(&out.stdout), since)
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
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    is_cross_repository: bool,
}

/// How far a PR's `createdAt` (GitHub's clock) may precede the thread row (this machine's clock)
/// and still be the thread's.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

/// `gh pr list --json url,state,isDraft,reviewDecision,createdAt,isCrossRepository` output → the
/// thread's PR: the newest one opened from this repository (a fork's branch can share the name)
/// no earlier than `since` minus the clock-skew allowance. Worktree branch names come from thread
/// names, so a new thread can reuse the branch of an old one whose PR merged long ago; that PR
/// must not resolve and clean the new thread. A PR without a readable `createdAt` is ignored.
pub fn parse_pr_list(json: &str, since: i64) -> Result<Option<PrInfo>> {
    let prs: Vec<GhPr> = serde_json::from_str(json).context("parsing `gh pr list` output")?;
    let created = |p: &GhPr| {
        let at = chrono::DateTime::parse_from_rfc3339(p.created_at.as_deref()?).ok()?;
        Some(at.timestamp_millis())
    };
    let newest = prs
        .into_iter()
        .filter(|p| !p.is_cross_repository)
        .filter_map(|p| Some((created(&p)?, p)))
        .filter(|(at, _)| *at >= since - CLOCK_SKEW_MS)
        .max_by_key(|(at, _)| *at);
    Ok(newest.map(|(_, p)| PrInfo {
        url: p.url,
        state: p.state,
        draft: p.is_draft,
        review: p.review_decision.filter(|r| !r.is_empty()),
    }))
}

/// Threads whose PR is worth asking about, once their job state names a worktree and branch
/// (`cleanup::branch_of`): not cleaned; resolved ones only while their PR can still change and
/// they were active lately.
fn wants_poll(row: &SessionRow, now: i64) -> bool {
    if row.role != Role::Thread.as_str() || row.cleaned_at.is_some() || row.repo_path.is_none() {
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
        // The job state's, not the hooks': a thread whose shell sits in the checkout reports
        // that checkout's branch, often `main`.
        let (Some(repo), Some(branch)) = (
            row.repo_path.as_deref(),
            cleanup::branch_of(ctx.claude, &row),
        ) else {
            continue;
        };
        refresh.polled += 1;
        let stored = ctx
            .gh
            .poll(Path::new(repo), &branch, row.created_at)
            .and_then(|pr| registry::set_pr(ctx.conn, &row.name, pr.as_ref(), (ctx.now)()));
        if let Err(e) = stored {
            refresh.errors.push(format!("{}: {e:#}", row.name));
        }
    }
    refresh.hint = ctx.gh.take_hint();
    Ok(refresh)
}

/// The board's 30 s refresh: `poll_project`, then with `cleanup = "merged"` a safe cleanup of
/// every resolved thread whose PR merged, that isn't working, waiting or showing in the pane, and
/// that no earlier attempt found blocked (those wait for `chief gc`, `chief clean` or the board's
/// `x`).
pub fn refresh_project(ctx: &Ctx, config: &ProjectConfig) -> Result<Refresh> {
    let mut refresh = poll_project(ctx, &config.slug)?;
    if config.threads.cleanup != Cleanup::Merged {
        return Ok(refresh);
    }
    let showing = db::get_pane_showing(ctx.conn, &config.slug)?;
    for row in db::list_sessions(ctx.conn, &config.slug, Some(Role::Thread))? {
        let due = row.resolved
            && row.cleaned_at.is_none()
            && row.cleanup_blocked.is_none()
            && row.pr_state.as_deref() == Some("MERGED")
            && !cleanup::active(&row)
            && showing.as_deref() != Some(row.name.as_str());
        if !due {
            continue;
        }
        match cleanup::clean(ctx, &row.name, Mode::Safe) {
            Ok(Outcome::Cleaned) => refresh.cleaned.push(row.name),
            Ok(Outcome::Blocked(why)) => refresh.blocked.push((row.name, why)),
            Ok(Outcome::AlreadyCleaned | Outcome::InProgress) => {}
            Err(e) => refresh.errors.push(format!("{}: {e:#}", row.name)),
        }
    }
    Ok(refresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2027-01-15T08:00:00Z, when the thread row was created.
    const CREATED: i64 = 1_800_000_000_000;

    fn pr(url: &str, state: &str, created: &str, cross: bool) -> serde_json::Value {
        serde_json::json!({"url": url, "state": state, "isDraft": false, "reviewDecision": "",
                           "createdAt": created, "isCrossRepository": cross})
    }

    #[test]
    fn pr_list_parsing() {
        let json = r#"[{"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":true,"reviewDecision":"","createdAt":"2027-01-15T09:00:00Z","isCrossRepository":false}]"#;
        assert_eq!(
            parse_pr_list(json, CREATED).unwrap(),
            Some(PrInfo {
                url: "https://github.com/o/r/pull/7".into(),
                state: "OPEN".into(),
                draft: true,
                review: None,
            })
        );
        let approved = r#"[{"url":"u","state":"MERGED","isDraft":false,"reviewDecision":"APPROVED","createdAt":"2027-01-15T09:00:00+01:00","isCrossRepository":false}]"#;
        assert_eq!(
            parse_pr_list(approved, CREATED)
                .unwrap()
                .unwrap()
                .review
                .as_deref(),
            Some("APPROVED")
        );
        assert_eq!(parse_pr_list("[]", CREATED).unwrap(), None);
        assert!(parse_pr_list("nope", CREATED).is_err());
    }

    /// A branch name reused by a new thread must not pick up the old thread's PR, nor a fork's.
    #[test]
    fn only_this_threads_own_newest_pr_counts() {
        let list = |prs: &[serde_json::Value]| {
            parse_pr_list(&serde_json::Value::from(prs.to_vec()).to_string(), CREATED)
                .unwrap()
                .map(|p| p.url)
        };
        let old = pr("old", "MERGED", "2026-06-01T10:00:00Z", false);
        assert_eq!(list(std::slice::from_ref(&old)), None, "before the thread");
        let fork = pr("fork", "OPEN", "2027-01-15T09:00:00Z", true);
        assert_eq!(
            list(&[fork.clone(), old.clone()]),
            None,
            "another repository"
        );
        // Within the clock-skew allowance (5 min) still counts.
        let skewed = pr("skewed", "OPEN", "2027-01-15T07:56:00Z", false);
        assert_eq!(list(&[old.clone(), skewed]).as_deref(), Some("skewed"));
        let early = pr("early", "OPEN", "2027-01-15T07:54:00Z", false);
        assert_eq!(list(&[early]), None);
        // The newest of the rest, whatever order gh lists them in.
        let first = pr("first", "CLOSED", "2027-01-15T09:00:00Z", false);
        let second = pr("second", "OPEN", "2027-01-16T09:00:00Z", false);
        assert_eq!(
            list(&[first.clone(), fork, second.clone(), old]).as_deref(),
            Some("second")
        );
        assert_eq!(list(&[second, first]).as_deref(), Some("second"));
        // No usable createdAt: can't tell whose it is.
        let undated = serde_json::json!([{"url": "u", "state": "MERGED"}]);
        assert_eq!(list(undated.as_array().unwrap()), None);
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
            r
        };
        assert!(wants_poll(&base(), now));
        type Edit = fn(&mut SessionRow);
        let skipped: [(&str, Edit); 5] = [
            ("no repo", |r| r.repo_path = None),
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
