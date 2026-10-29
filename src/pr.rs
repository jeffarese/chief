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

    /// The thread's PR on `branch`, run in `repo_path`: the newest one opened from this repository
    /// no earlier than `since` (the thread row's `created_at`, see `parse_pr_list`) whose head
    /// commit is on the local branch (`owns_head`). A missing `gh`, a `gh` that isn't logged in,
    /// or a repository without a GitHub remote is `Ok(None)` plus a hint (`take_hint`); other
    /// failures, a missing `repo_path` among them, are errors, so the stored PR stays.
    pub fn poll(&self, repo_path: &Path, branch: &str, since: i64) -> Result<Option<PrInfo>> {
        // Spawning in a folder that doesn't exist fails with NotFound too, which would read as
        // "gh isn't installed".
        if !repo_path.is_dir() {
            bail!("{} isn't a folder", repo_path.display());
        }
        let mut cmd = Command::new(&self.bin);
        // gh finds the repository with git: not the one a git hook's `GIT_DIR` names.
        crate::proc::without_repo_env(&mut cmd);
        cmd.current_dir(repo_path).args([
            "pr", "list", "--head", branch, "--state", "all", "--json", PR_FIELDS, "--limit", "10",
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
        let listed = parse_pr_list(&String::from_utf8_lossy(&out.stdout), since)?;
        if listed.is_empty() {
            return Ok(None);
        }
        let reference = format!("refs/heads/{branch}");
        if cleanup::branch_sha(repo_path, &reference)?.is_none() {
            bail!("no local branch {branch} to match its pull requests on");
        }
        for pr in listed {
            if let Some(head) = &pr.head
                && owns_head(repo_path, &reference, head)?
            {
                return Ok(Some(pr.info));
            }
        }
        Ok(None)
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

/// The `gh pr list --json` fields chief reads.
const PR_FIELDS: &str = "url,state,isDraft,reviewDecision,createdAt,isCrossRepository,headRefOid";

/// Whether the PR head `oid` is on the thread's branch (`reference`, `refs/heads/<branch>`): its
/// tip or an ancestor of it. A branch name says nothing about whose PR it is: a cleaned thread's
/// remote branch stays, and a later thread can name its worktree the same. Its commits can't be
/// on another thread's branch unless that branch was built on them. A commit this repository
/// doesn't have isn't on the branch; a check that can't run is an error.
fn owns_head(repo: &Path, reference: &str, oid: &str) -> Result<bool> {
    let hex = (7..=64).contains(&oid.len()) && oid.chars().all(|c| c.is_ascii_hexdigit());
    if !hex {
        return Ok(false);
    }
    let commit = format!("{oid}^{{commit}}");
    if !cleanup::git_output(repo, &["cat-file", "-e", &commit])?
        .status
        .success()
    {
        return Ok(false);
    }
    let out = cleanup::git_output(repo, &["merge-base", "--is-ancestor", oid, reference])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "`git merge-base --is-ancestor {oid} {reference}` failed in {} ({}): {}",
            repo.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
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
    #[serde(default)]
    head_ref_oid: Option<String>,
}

/// One PR that may be the thread's, before its head is checked (`owns_head`).
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub info: PrInfo,
    /// `headRefOid`: the commit the PR's branch pointed at on GitHub.
    pub head: Option<String>,
}

/// How far a PR's `createdAt` (GitHub's clock) may precede the thread row (this machine's clock)
/// and still be the thread's.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

/// `gh pr list --json <PR_FIELDS>` output → the PRs that may be the thread's, newest first: those
/// opened from this repository (a fork's branch can share the name) no earlier than `since` minus
/// the clock-skew allowance. Worktree branch names come from thread names, so a new thread can
/// reuse the branch of an old one whose PR merged long ago; that PR must not resolve and clean the
/// new thread. A PR without a readable `createdAt` is left out. `Gh::poll` then takes the first
/// whose head is on the thread's branch.
pub fn parse_pr_list(json: &str, since: i64) -> Result<Vec<Listed>> {
    let prs: Vec<GhPr> = serde_json::from_str(json).context("parsing `gh pr list` output")?;
    let created = |p: &GhPr| {
        let at = chrono::DateTime::parse_from_rfc3339(p.created_at.as_deref()?).ok()?;
        Some(at.timestamp_millis())
    };
    let mut listed: Vec<(i64, GhPr)> = prs
        .into_iter()
        .filter(|p| !p.is_cross_repository)
        .filter_map(|p| Some((created(&p)?, p)))
        .filter(|(at, _)| *at >= since - CLOCK_SKEW_MS)
        .collect();
    listed.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    Ok(listed
        .into_iter()
        .map(|(_, p)| Listed {
            info: PrInfo {
                url: p.url,
                state: p.state,
                draft: p.is_draft,
                review: p.review_decision.filter(|r| !r.is_empty()),
            },
            head: p.head_ref_oid.filter(|h| !h.is_empty()),
        })
        .collect())
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
        // that checkout's branch, often `main`. A job state that can't be read says nothing:
        // skipped, the stored PR kept.
        let branch = match cleanup::branch_of(ctx.claude, &row) {
            Ok(branch) => branch,
            Err(e) => {
                refresh.errors.push(format!("{}: {e:#}", row.name));
                continue;
            }
        };
        let (Some(repo), Some(branch)) = (row.repo_path.as_deref(), branch) else {
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
        let json = r#"[{"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":true,"reviewDecision":"","createdAt":"2027-01-15T09:00:00Z","isCrossRepository":false,"headRefOid":"abc123f"}]"#;
        assert_eq!(
            parse_pr_list(json, CREATED).unwrap(),
            [Listed {
                info: PrInfo {
                    url: "https://github.com/o/r/pull/7".into(),
                    state: "OPEN".into(),
                    draft: true,
                    review: None,
                },
                head: Some("abc123f".into()),
            }]
        );
        let approved = r#"[{"url":"u","state":"MERGED","isDraft":false,"reviewDecision":"APPROVED","createdAt":"2027-01-15T09:00:00+01:00","isCrossRepository":false}]"#;
        let listed = parse_pr_list(approved, CREATED).unwrap();
        assert_eq!(listed[0].info.review.as_deref(), Some("APPROVED"));
        assert_eq!(listed[0].head, None, "no headRefOid: no head to match");
        assert!(parse_pr_list("[]", CREATED).unwrap().is_empty());
        assert!(parse_pr_list("nope", CREATED).is_err());
    }

    /// A branch name reused by a new thread must not pick up the old thread's PR, nor a fork's.
    #[test]
    fn only_this_threads_own_prs_are_listed_newest_first() {
        let list = |prs: &[serde_json::Value]| -> Vec<String> {
            parse_pr_list(&serde_json::Value::from(prs.to_vec()).to_string(), CREATED)
                .unwrap()
                .into_iter()
                .map(|p| p.info.url)
                .collect()
        };
        let old = pr("old", "MERGED", "2026-06-01T10:00:00Z", false);
        assert!(
            list(std::slice::from_ref(&old)).is_empty(),
            "before the thread"
        );
        let fork = pr("fork", "OPEN", "2027-01-15T09:00:00Z", true);
        assert!(
            list(&[fork.clone(), old.clone()]).is_empty(),
            "another repository"
        );
        // Within the clock-skew allowance (5 min) still counts.
        let skewed = pr("skewed", "OPEN", "2027-01-15T07:56:00Z", false);
        assert_eq!(list(&[old.clone(), skewed]), ["skewed"]);
        let early = pr("early", "OPEN", "2027-01-15T07:54:00Z", false);
        assert!(list(&[early]).is_empty());
        // Newest first, whatever order gh lists them in.
        let first = pr("first", "CLOSED", "2027-01-15T09:00:00Z", false);
        let second = pr("second", "OPEN", "2027-01-16T09:00:00Z", false);
        assert_eq!(
            list(&[first.clone(), fork, second.clone(), old]),
            ["second", "first"]
        );
        assert_eq!(list(&[second, first]), ["second", "first"]);
        // No usable createdAt: can't tell whose it is.
        let undated = serde_json::json!([{"url": "u", "state": "MERGED"}]);
        assert!(list(undated.as_array().unwrap()).is_empty());
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
