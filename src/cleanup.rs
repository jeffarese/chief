//! Removing a resolved thread's session, worktree and local branch with `claude rm`
//! (ARCHITECTURE §12), plus `chief gc` and `chief clean`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::claude::{Claude, RmOutcome};
use crate::db::{self, SessionRow};
use crate::pr::{self, Gh};
use crate::state::{self, Group, Role};
use crate::text::one_line;
use crate::{guard, paths, registry, tools};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Unpushed commits listed by `inspect`; the count covers all of them.
const UNPUSHED_SHOWN: usize = 20;

/// What cleanup touches, so tests can point it at temp dirs and the fake `claude` and `gh`.
pub struct Ctx<'a> {
    pub conn: &'a Connection,
    pub claude: &'a Claude,
    pub gh: &'a Gh,
    pub now: fn() -> i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Never loses work: a dirty worktree or unpushed commits leave the thread in place.
    Safe,
    /// The user saw this inspection (`render_loss`) and confirmed: exactly what it lists is
    /// reset or discarded, nothing else.
    Discard(Inspection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Cleaned,
    /// Left in place; the reason is stored in `cleanup_blocked`.
    Blocked(String),
    AlreadyCleaned,
    /// Another chief process is cleaning it up right now (`db::claim_cleanup`); nothing done.
    InProgress,
}

/// How `chief gc` and the CLI say `Outcome::InProgress`.
pub const IN_PROGRESS: &str = "another chief process is cleaning it up";

/// A thread's worktree as git sees it now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    /// None when the thread never moved into `.claude/worktrees/` (nothing of its own to lose).
    pub worktree: Option<PathBuf>,
    pub exists: bool,
    pub branch: Option<String>,
    /// `git status --porcelain` lines; git-ignored files aren't listed (Claude Code ignores
    /// them too).
    pub dirty: Vec<String>,
    /// Commits that exist on no remote: all of them counted, the first `UNPUSHED_SHOWN` listed
    /// as `<sha> <subject>`.
    pub unpushed_count: usize,
    pub unpushed: Vec<String>,
    /// The full sha those commits end at: the worktree's HEAD, or the branch's once the worktree
    /// is gone. `claude rm`'s discard token names the head it would throw away.
    pub head: Option<String>,
}

impl Inspection {
    /// Why a safe cleanup would lose work, if it would.
    pub fn blocker(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.unpushed_count > 0 {
            parts.push(plural(self.unpushed_count, "unpushed commit"));
        }
        if !self.dirty.is_empty() {
            parts.push(format!(
                "uncommitted changes: {}",
                plural(self.dirty.len(), "file")
            ));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// `<repo>/.claude/worktrees/<name>` for a cwd at or under it.
pub fn worktree_root(cwd: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = cwd.components().collect();
    let i = parts.windows(3).position(|w| {
        w[0].as_os_str() == ".claude"
            && w[1].as_os_str() == "worktrees"
            && matches!(w[2], Component::Normal(_))
    })?;
    Some(parts[..i + 3].iter().collect())
}

/// Working or waiting as if it weren't resolved: `claude rm` stops the session, so a thread
/// that still runs or waits on the user must not be cleaned.
pub fn active(row: &SessionRow) -> bool {
    let mut facts = registry::facts(row);
    facts.resolved = false;
    matches!(state::derive(&facts).group, Group::Working | Group::Waiting)
}

/// The thread's own worktree: what its job state names (`Claude::job_worktree`), the folder
/// `claude rm` deletes, and only when that is a `<repo>/.claude/worktrees/<name>` folder. Never
/// the hooks' cwd: it follows the thread's shell anywhere, into another thread's worktree too.
pub fn worktree_of(claude: &Claude, row: &SessionRow) -> Option<PathBuf> {
    let path = claude.job_worktree(row.short_id.as_deref()?)?;
    (worktree_root(&path).as_deref() == Some(path.as_path())).then_some(path)
}

/// Reads the thread's worktree (`worktree_of`). A thread without one has nothing of its own to
/// lose.
pub fn inspect(claude: &Claude, row: &SessionRow) -> Result<Inspection> {
    let Some(root) = worktree_of(claude, row) else {
        return Ok(Inspection::default());
    };
    let mut ins = Inspection {
        exists: root.is_dir(),
        worktree: Some(root.clone()),
        branch: row.branch.clone(),
        ..Default::default()
    };
    // Where to ask about commits: the worktree, or the repository if the worktree is gone.
    let dir = if ins.exists {
        // A leftover folder that isn't a worktree would make git answer for the checkout around
        // it, and a discard would reset that.
        let top = git_line(&root, &["rev-parse", "--show-toplevel"])?.map(PathBuf::from);
        if top.and_then(|t| t.canonicalize().ok()) != root.canonicalize().ok() {
            bail!("{} is not a git worktree", root.display());
        }
        if let Some(branch) = git_line(&root, &["branch", "--show-current"])? {
            ins.branch = Some(branch);
        }
        // Explicit, so a `status.showUntrackedFiles = no` config can't hide untracked files.
        ins.dirty = git_lines(
            &root,
            &["status", "--porcelain", "--untracked-files=normal"],
        )?;
        root.clone()
    } else {
        match row.repo_path.as_deref().map(PathBuf::from) {
            Some(repo) if repo.is_dir() => repo,
            _ => return Ok(ins),
        }
    };
    let mut revs: Vec<String> = Vec::new();
    if ins.exists {
        revs.push("HEAD".into());
        ins.head = git_line(&dir, &["rev-parse", "HEAD"])?;
    }
    if let Some(branch) = &ins.branch {
        let reference = format!("refs/heads/{branch}");
        if let Ok(sha) = git_line(&dir, &["rev-parse", "--verify", "--quiet", &reference]) {
            ins.head = ins.head.take().or(sha);
            revs.push(reference);
        }
    }
    if revs.is_empty() {
        return Ok(ins);
    }
    // What Claude Code calls unpushed: `rev-list <branch> --not --remotes`.
    let mut count: Vec<&str> = vec!["rev-list", "--count"];
    count.extend(revs.iter().map(String::as_str));
    count.extend(["--not", "--remotes"]);
    ins.unpushed_count = git_line(&dir, &count)?
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if ins.unpushed_count > 0 {
        let shown = format!("-n{UNPUSHED_SHOWN}");
        let mut log: Vec<&str> = vec!["log", "--format=%h %s", &shown];
        log.extend(revs.iter().map(String::as_str));
        log.extend(["--not", "--remotes"]);
        ins.unpushed = git_lines(&dir, &log)?;
    }
    Ok(ins)
}

fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let shown = format!("git -C {} {}", dir.display(), args.join(" "));
    let out = crate::proc::output_with_timeout(&mut cmd, GIT_TIMEOUT)
        .with_context(|| format!("running `{shown}`"))?
        .with_context(|| format!("`{shown}` took longer than {}s", GIT_TIMEOUT.as_secs()))?;
    if !out.status.success() {
        bail!(
            "`{shown}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

fn git_lines(dir: &Path, args: &[&str]) -> Result<Vec<String>> {
    let out = git(dir, args)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect())
}

/// The first line, None when empty.
fn git_line(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    Ok(git_lines(dir, args)?
        .into_iter()
        .next()
        .map(|l| l.trim().to_string()))
}

/// Refreshes the thread from `claude agents` and checks it may be cleaned. None when it is
/// already cleaned (the refresh may have just found it removed outside chief).
pub fn cleanable(ctx: &Ctx, name: &str) -> Result<Option<SessionRow>> {
    let row = thread_row(ctx.conn, name)?;
    if row.cleaned_at.is_some() {
        return Ok(None);
    }
    let taken_at = (ctx.now)();
    let snapshot = ctx
        .claude
        .agents()
        .context("checking whether the thread is still running")?;
    registry::sync_in(
        ctx.conn,
        &row.project_slug,
        &snapshot,
        taken_at,
        (ctx.now)(),
        &ctx.claude.config_dir,
    )?;
    let row = thread_row(ctx.conn, name)?;
    if row.cleaned_at.is_some() {
        return Ok(None);
    }
    if !row.resolved {
        bail!("{name} is not resolved; resolve it first (cleanup stops its session)");
    }
    ensure_idle(ctx.conn, &row)?;
    Ok(Some(row))
}

/// `claude rm` stops the session even when it refuses: never while the thread works, waits on
/// the user, or shows in the pane (the user may be typing into it; hooks only fire on submit).
fn ensure_idle(conn: &Connection, row: &SessionRow) -> Result<()> {
    let name = &row.name;
    if active(row) {
        let mut facts = registry::facts(row);
        facts.resolved = false;
        let group = state::derive(&facts).group.label().to_lowercase();
        bail!("{name} is {group}; clean it up once it is idle (cleanup stops its session)");
    }
    if db::get_pane_showing(conn, &row.project_slug)?.as_deref() == Some(name.as_str()) {
        bail!(
            "{name} is showing in the pane; show another session there first (cleanup stops \
             its session)"
        );
    }
    Ok(())
}

fn thread_row(conn: &Connection, name: &str) -> Result<SessionRow> {
    match db::get_session(conn, name)? {
        Some(row) if row.role == Role::Thread.as_str() => Ok(row),
        _ => bail!("no thread named {name}"),
    }
}

/// §12: removes a resolved, idle thread with `claude rm`. `Safe` checks the worktree first and
/// doesn't call `claude rm` when it would refuse (it stops the session even then). `Discard`
/// first checks the worktree still is what the user confirmed, resets uncommitted changes
/// (`git clean` without `-x`, so ignored files stay), and retries an unpushed refusal with its
/// token only when that throws away exactly the confirmed commits. The outcome is stored on the
/// row; reports are kept. One process at a time: a thread another process is cleaning up is
/// `InProgress`.
pub fn clean(ctx: &Ctx, name: &str, mode: Mode) -> Result<Outcome> {
    thread_row(ctx.conn, name)?;
    let claimed_at = (ctx.now)();
    match db::claim_cleanup(ctx.conn, name, claimed_at)? {
        db::Claim::Cleaned => return Ok(Outcome::AlreadyCleaned),
        db::Claim::Held => return Ok(Outcome::InProgress),
        db::Claim::Claimed => {}
    }
    let outcome = clean_claimed(ctx, name, mode);
    let released = db::release_cleanup(ctx.conn, name, claimed_at);
    let outcome = outcome?;
    released?;
    Ok(outcome)
}

fn clean_claimed(ctx: &Ctx, name: &str, mode: Mode) -> Result<Outcome> {
    let Some(row) = cleanable(ctx, name)? else {
        return Ok(Outcome::AlreadyCleaned);
    };
    // Never started: Claude Code has nothing to remove.
    let Some(short_id) = row.short_id.clone() else {
        record(ctx, name, None)?;
        return Ok(Outcome::Cleaned);
    };
    if row.removed_at.is_some() {
        return removed_outside(ctx, &row);
    }
    let ins = match (inspect(ctx.claude, &row), &mode) {
        (Ok(ins), _) => ins,
        (Err(e), Mode::Safe) => {
            let why = format!(
                "could not inspect the worktree: {}",
                one_line(&format!("{e:#}"), 200)
            );
            record(ctx, name, Some(&why))?;
            return Ok(Outcome::Blocked(why));
        }
        (Err(e), Mode::Discard(_)) => return Err(e),
    };
    let confirmed = match &mode {
        Mode::Safe => {
            if let Some(why) = ins.blocker() {
                record(ctx, name, Some(&why))?;
                return Ok(Outcome::Blocked(why));
            }
            None
        }
        Mode::Discard(confirmed) => {
            if ins != *confirmed {
                let now = ins
                    .blocker()
                    .unwrap_or_else(|| "nothing unsaved".to_string());
                let why = format!(
                    "the worktree changed after you confirmed (now: {now}); nothing was discarded"
                );
                record(ctx, name, Some(&why))?;
                return Ok(Outcome::Blocked(why));
            }
            if let Some(wt) = &ins.worktree
                && let Some(other) = sharing_worktree(ctx, &row, wt)?
            {
                let why = format!(
                    "{} is also the worktree of {other}; not discarding anything there",
                    wt.display()
                );
                record(ctx, name, Some(&why))?;
                return Ok(Outcome::Blocked(why));
            }
            Some(confirmed)
        }
    };
    // Again with a fresh `claude agents`, right before anything is reset or stopped: the thread
    // may have got a message, or the pane may have attached to it, since `cleanable`.
    let Some(row) = cleanable(ctx, name)? else {
        return Ok(Outcome::AlreadyCleaned);
    };
    if row.removed_at.is_some() {
        return removed_outside(ctx, &row);
    }
    if let Some(wt) = ins
        .worktree
        .as_deref()
        .filter(|_| confirmed.is_some() && ins.exists && !ins.dirty.is_empty())
    {
        git(wt, &["reset", "--hard", "-q"])?;
        git(wt, &["clean", "-fdq"])?;
    }
    let mut outcome = ctx.claude.rm(&short_id, None)?;
    if let (Some(confirmed), RmOutcome::Unpushed { token, .. }) = (confirmed, &outcome)
        && discards_only_confirmed(confirmed, token)
    {
        outcome = ctx.claude.rm(&short_id, Some(token))?;
    }
    let why = match outcome {
        RmOutcome::Removed => {
            record(ctx, name, None)?;
            return Ok(Outcome::Cleaned);
        }
        RmOutcome::Unpushed { commits, .. } => commits
            .first()
            .and_then(|c| c.split_whitespace().next()?.parse::<usize>().ok())
            .map(|n| plural(n, "unpushed commit"))
            .unwrap_or_else(|| "unpushed commits".to_string()),
        RmOutcome::Dirty => "uncommitted changes".to_string(),
        RmOutcome::Other(text) => format!("claude rm: {}", one_line(&text, 200)),
    };
    record(ctx, name, Some(&why))?;
    Ok(Outcome::Blocked(why))
}

/// Whether `--discard-unpushed <token>` throws away only what the user confirmed: commits chief
/// listed itself, ending at the head it saw (the token is `<head sha>@<worktree id>`). Anything
/// else `claude rm` counts is work nobody was shown.
fn discards_only_confirmed(confirmed: &Inspection, token: &str) -> bool {
    let Some((sha, _)) = token.split_once('@') else {
        return false;
    };
    confirmed.unpushed_count > 0
        && confirmed
            .head
            .as_deref()
            .is_some_and(|head| head.eq_ignore_ascii_case(sha))
}

/// Another registered session (any project of this chief home) whose job state names the same
/// worktree, if any.
fn sharing_worktree(ctx: &Ctx, row: &SessionRow, wt: &Path) -> Result<Option<String>> {
    let wt = wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf());
    for other in db::all_sessions(ctx.conn)? {
        if other.name == row.name {
            continue;
        }
        let theirs = other
            .short_id
            .as_deref()
            .and_then(|s| ctx.claude.job_worktree(s));
        if theirs.is_some_and(|p| p.canonicalize().unwrap_or(p) == wt) {
            return Ok(Some(other.name));
        }
    }
    Ok(None)
}

/// Removed outside chief (§5): `claude rm` has no session left to remove, and the worktree it
/// left behind is the user's to delete.
fn removed_outside(ctx: &Ctx, row: &SessionRow) -> Result<Outcome> {
    let Some(wt) = worktree_of(ctx.claude, row).filter(|p| p.is_dir()) else {
        record(ctx, &row.name, None)?;
        return Ok(Outcome::Cleaned);
    };
    let why = format!(
        "{}; worktree remains at {}",
        registry::REMOVED_OUTSIDE,
        wt.display()
    );
    record(ctx, &row.name, Some(&why))?;
    Ok(Outcome::Blocked(why))
}

/// Stores a cleanup result: `blocked` None means cleaned. A row that is cleaned already (a sync
/// found it removed meanwhile) keeps its `cleaned_at` and never gets a `cleanup_blocked`.
fn record(ctx: &Ctx, name: &str, blocked: Option<&str>) -> Result<()> {
    db::immediate(ctx.conn, |conn| {
        let mut row = thread_row(conn, name)?;
        if row.cleaned_at.is_some() {
            return Ok(());
        }
        let now = (ctx.now)();
        match blocked {
            None => {
                row.cleaned_at = Some(now);
                row.cleanup_blocked = None;
                row.pid = None;
            }
            Some(why) => row.cleanup_blocked = Some(why.to_string()),
        }
        row.updated_at = now;
        db::update_session(conn, &row)
    })
}

/// One `chief gc` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Cleaned,
    WouldClean,
    Blocked(String),
    Skipped(String),
}

/// What `gc` did to one project.
#[derive(Debug, Default)]
pub struct Gc {
    /// (thread, verdict), in board order.
    pub verdicts: Vec<(String, Verdict)>,
    /// Failed polls, gh hints, a failed `claude agents`.
    pub warnings: Vec<String>,
}

/// `chief gc` for one project: refresh PRs, then a safe cleanup of every resolved, uncleaned
/// thread (or, dry, what it would do). Returns the verdicts and warnings.
pub fn gc(ctx: &Ctx, slug: &str, dry_run: bool) -> Result<Gc> {
    let refresh = pr::poll_project(ctx, slug)?;
    let mut warnings = refresh.errors;
    warnings.extend(refresh.hint);
    // Also learns which threads were removed outside chief.
    if let Err(e) = sync_agents(ctx, slug) {
        warnings.push(format!("`claude agents` failed: {e:#}"));
    }
    let mut rows: Vec<_> = db::list_sessions(ctx.conn, slug, Some(Role::Thread))?
        .into_iter()
        .filter(|r| r.resolved && r.cleaned_at.is_none())
        .collect();
    tools::board_order(&mut rows);
    let mut verdicts = Vec::new();
    for row in rows {
        let verdict = if dry_run {
            dry_verdict(ctx.claude, &row)
        } else {
            match clean(ctx, &row.name, Mode::Safe) {
                Ok(Outcome::Cleaned | Outcome::AlreadyCleaned) => Verdict::Cleaned,
                Ok(Outcome::Blocked(why)) => Verdict::Blocked(why),
                Ok(Outcome::InProgress) => Verdict::Skipped(IN_PROGRESS.to_string()),
                Err(e) => Verdict::Skipped(format!("{e:#}")),
            }
        };
        verdicts.push((row.name, verdict));
    }
    Ok(Gc { verdicts, warnings })
}

fn sync_agents(ctx: &Ctx, slug: &str) -> Result<()> {
    let taken_at = (ctx.now)();
    let snapshot = ctx.claude.agents()?;
    registry::sync_in(
        ctx.conn,
        slug,
        &snapshot,
        taken_at,
        (ctx.now)(),
        &ctx.claude.config_dir,
    )
}

fn dry_verdict(claude: &Claude, row: &SessionRow) -> Verdict {
    if active(row) {
        return Verdict::Skipped("still working or waiting".to_string());
    }
    if row.short_id.is_none() {
        return Verdict::WouldClean;
    }
    match inspect(claude, row) {
        Ok(ins) => match ins.blocker() {
            Some(why) => Verdict::Blocked(why),
            None => Verdict::WouldClean,
        },
        Err(e) => Verdict::Blocked(format!("could not inspect the worktree: {e:#}")),
    }
}

/// The `chief gc` table for one project.
pub fn render_gc(slug: &str, verdicts: &[(String, Verdict)]) -> String {
    let mut out = format!("{slug}\n");
    if verdicts.is_empty() {
        out.push_str("  nothing to clean\n");
        return out;
    }
    let width = verdicts.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, verdict) in verdicts {
        let (label, why) = match verdict {
            Verdict::Cleaned => ("cleaned", None),
            Verdict::WouldClean => ("would clean", None),
            Verdict::Blocked(why) => ("blocked", Some(why)),
            Verdict::Skipped(why) => ("skipped", Some(why)),
        };
        let line = match why {
            Some(why) => format!("  {label:<11}  {name:<width$}  {}", one_line(why, 160)),
            None => format!("  {label:<11}  {name}"),
        };
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `chief gc [<slug>] [--dry-run]`.
pub fn run_gc(slug: Option<&str>, dry_run: bool) -> Result<()> {
    let conn = db::open(&paths::db())?;
    let slugs: Vec<String> = match slug {
        Some(slug) => {
            if db::project_dir(&conn, slug)?.is_none() {
                bail!("no chief project {slug:?}");
            }
            vec![slug.to_string()]
        }
        None => db::list_projects(&conn)?
            .into_iter()
            .map(|(s, _)| s)
            .collect(),
    };
    let claude = Claude::from_env();
    let gh = Gh::from_env();
    let ctx = Ctx {
        conn: &conn,
        claude: &claude,
        gh: &gh,
        now: db::now_ms,
    };
    for slug in &slugs {
        let done = gc(&ctx, slug, dry_run)?;
        for w in &done.warnings {
            eprintln!("warning: {w}");
        }
        print!("{}", render_gc(slug, &done.verdicts));
    }
    Ok(())
}

/// What a discarding cleanup deletes and loses, for the confirmation prompt.
pub fn render_loss(name: &str, ins: &Inspection) -> String {
    let mut out = String::new();
    match &ins.worktree {
        Some(wt) => {
            let branch = ins
                .branch
                .as_deref()
                .map(|b| format!(" and its local branch {b}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "Cleaning up {name} deletes its worktree {}{branch} (a pushed branch stays on \
                 the remote).\n",
                wt.display()
            ));
        }
        None => out.push_str(&format!(
            "Cleaning up {name} removes its Claude Code session.\n"
        )),
    }
    if ins.dirty.is_empty() && ins.unpushed_count == 0 {
        out.push_str("Nothing unsaved will be lost.\n");
        return out;
    }
    out.push_str("This loses:\n");
    if !ins.dirty.is_empty() {
        out.push_str("  uncommitted changes (git status --short):\n");
        for line in &ins.dirty {
            out.push_str(&format!("    {line}\n"));
        }
    }
    if ins.unpushed_count > 0 {
        out.push_str(&format!(
            "  {} on no remote:\n",
            plural(ins.unpushed_count, "commit")
        ));
        for line in &ins.unpushed {
            out.push_str(&format!("    {line}\n"));
        }
        if ins.unpushed_count > ins.unpushed.len() {
            out.push_str(&format!(
                "    … and {} more\n",
                ins.unpushed_count - ins.unpushed.len()
            ));
        }
    }
    out
}

/// `chief clean <slug> <thread> [--discard] [--yes]`.
pub fn run_clean(slug: &str, thread: &str, discard: bool, yes: bool) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, slug)?.is_none() {
        bail!("no chief project {slug:?}");
    }
    let name = guard::strip_ref(thread);
    match db::get_session(&conn, name)? {
        Some(r) if r.project_slug == slug && r.role == Role::Thread.as_str() => {}
        _ => bail!("{name} is not a thread of {slug}; chief status {slug} lists them"),
    }
    let claude = Claude::from_env();
    let gh = Gh::from_env();
    let ctx = Ctx {
        conn: &conn,
        claude: &claude,
        gh: &gh,
        now: db::now_ms,
    };
    let mode = if discard {
        let Some(row) = cleanable(&ctx, name)? else {
            println!("{name} is already cleaned up.");
            return Ok(());
        };
        let ins = inspect(&claude, &row)?;
        print!("{}", render_loss(name, &ins));
        if ins.blocker().is_some() && !yes {
            confirm(name)?;
        }
        Mode::Discard(ins)
    } else {
        Mode::Safe
    };
    let discarding = matches!(mode, Mode::Discard(_));
    match clean(&ctx, name, mode)? {
        Outcome::Cleaned => println!("Cleaned up {name}."),
        Outcome::AlreadyCleaned => println!("{name} is already cleaned up."),
        Outcome::InProgress => bail!("{name} wasn't cleaned up here: {IN_PROGRESS}"),
        Outcome::Blocked(why) => {
            let listed = db::get_session(&conn, name)?
                .and_then(|row| inspect(&claude, &row).ok())
                .is_some_and(|ins| ins.blocker().is_some());
            let hint = if discarding {
                " chief discards only what it listed and you confirmed; push or remove the rest \
                 yourself."
                    .to_string()
            } else if listed {
                format!(
                    " Push or commit what you want to keep, or throw it away with \
                     chief clean {slug} {name} --discard."
                )
            } else {
                " chief can't list that work itself, so it won't discard it: push it, or remove \
                 the session yourself with claude rm."
                    .to_string()
            };
            bail!("kept {name}: {why}.{hint}");
        }
    }
    Ok(())
}

/// Asks for the thread's name on a terminal; refuses without one.
fn confirm(name: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("not deleting anything: confirming --discard needs a terminal (or pass --yes)");
    }
    print!("Type the thread name to confirm: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() != name {
        bail!("not confirmed; nothing was deleted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sync (another process) found the thread removed while this cleanup ran.
    #[test]
    fn a_cleaned_row_keeps_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "p", dir.path(), 0).unwrap();
        let mut t = SessionRow::new("t", "p", Role::Thread, 0);
        t.cleaned_at = Some(5);
        db::insert_session(&conn, &t).unwrap();
        let claude = Claude::new("/nonexistent/claude".into());
        let gh = Gh::new("/nonexistent/gh".into());
        let ctx = Ctx {
            conn: &conn,
            claude: &claude,
            gh: &gh,
            now: || 9,
        };
        record(&ctx, "t", Some("claude rm: no such session")).unwrap();
        record(&ctx, "t", None).unwrap();
        let row = db::get_session(&conn, "t").unwrap().unwrap();
        assert_eq!((row.cleaned_at, row.cleanup_blocked), (Some(5), None));
    }

    #[test]
    fn worktree_roots() {
        let cases = [
            ("/r/.claude/worktrees/fix", Some("/r/.claude/worktrees/fix")),
            (
                "/r/.claude/worktrees/fix/src/a",
                Some("/r/.claude/worktrees/fix"),
            ),
            ("/r/.claude/worktrees", None),
            ("/r/.claude/other/fix", None),
            ("/r", None),
        ];
        for (cwd, want) in cases {
            assert_eq!(
                worktree_root(Path::new(cwd)),
                want.map(PathBuf::from),
                "{cwd}"
            );
        }
    }

    #[test]
    fn blockers_and_loss_text() {
        let mut ins = Inspection {
            worktree: Some("/r/.claude/worktrees/fix".into()),
            exists: true,
            branch: Some("worktree-fix".into()),
            ..Default::default()
        };
        assert_eq!(ins.blocker(), None);
        assert!(render_loss("t", &ins).contains("Nothing unsaved will be lost"));
        ins.dirty = vec![" M a.txt".into(), "?? b.txt".into(), "?? c".into()];
        assert_eq!(
            ins.blocker().as_deref(),
            Some("uncommitted changes: 3 files")
        );
        ins.unpushed_count = 22;
        ins.unpushed = vec!["abc123 First".into(), "def456 Second".into()];
        ins.head = Some("ABC123ff".into());
        assert!(discards_only_confirmed(&ins, "abc123ff@0e1f"));
        for token in ["abc123fe@0e1f", "abc123", "@0e1f"] {
            assert!(!discards_only_confirmed(&ins, token), "{token}");
        }
        assert_eq!(
            ins.blocker().as_deref(),
            Some("22 unpushed commits, uncommitted changes: 3 files")
        );
        let text = render_loss("t", &ins);
        assert!(
            text.contains(
                "deletes its worktree /r/.claude/worktrees/fix and its local branch worktree-fix"
            ),
            "{text}"
        );
        assert!(text.contains("     M a.txt\n"), "{text}");
        assert!(
            text.contains("22 commits on no remote:\n    abc123 First\n"),
            "{text}"
        );
        assert!(text.contains("… and 20 more"), "{text}");
        ins.dirty.clear();
        ins.unpushed_count = 1;
        assert_eq!(ins.blocker().as_deref(), Some("1 unpushed commit"));
    }

    #[test]
    fn gc_table() {
        let text = render_gc(
            "demo",
            &[
                ("demo-a-1111".into(), Verdict::Cleaned),
                (
                    "demo-long-name-2222".into(),
                    Verdict::Blocked("2 unpushed commits".into()),
                ),
                (
                    "demo-c-3333".into(),
                    Verdict::Skipped("still working".into()),
                ),
            ],
        );
        assert_eq!(
            text,
            "demo\n  cleaned      demo-a-1111\n  blocked      demo-long-name-2222  2 unpushed commits\n  \
             skipped      demo-c-3333          still working\n"
        );
        assert_eq!(render_gc("x", &[]), "x\n  nothing to clean\n");
    }
}
