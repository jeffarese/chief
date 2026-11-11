//! A routine's command (ARCHITECTURE §13 "A command"): a shell line chief runs itself, with no
//! Claude session. One `routine_runs` row per run, written by the chief process that runs it;
//! what the run came to reaches someone only when it is news.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::routines::{self, Kind, RecordValueArgs, Routine, Said, Trigger, Verdict};
use crate::text::{local_hhmm, local_stamp, one_line};
use crate::tools::{self, ToolCtx};

/// How long a command may run when its routine doesn't say.
pub const DEFAULT_TIMEOUT_SECS: i64 = 300;
/// The longest a routine may give its command: a day.
pub const MAX_TIMEOUT_SECS: i64 = 86_400;
/// How much of what a run printed its row keeps: the end of it.
pub const OUTPUT_BYTES: usize = 16 * 1024;
/// In the routine's folder: everything its latest run printed.
pub const LOG_FILE: &str = "last-run.log";
/// How many runs of one routine stay in the database.
const KEPT_RUNS: i64 = 200;
/// How long a run may have no process on record: between its row and its process.
const SPAWN_GRACE_MS: i64 = 30_000;
/// What a command gets between SIGTERM and SIGKILL when its time is up.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How much of a command's output a session started for its news is handed.
const HANDED_BYTES: usize = 4 * 1024;

/// How a run stands or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    /// It exited with status 0.
    Ok,
    /// It exited with another status, died of a signal, or couldn't start.
    Failed,
    /// It ran past its time and was killed.
    Timeout,
    /// The process that ran it is gone without a word: the machine restarted, or it was killed.
    Lost,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Timeout => "timeout",
            Status::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Status {
        match s {
            "running" => Status::Running,
            "ok" => Status::Ok,
            "failed" => Status::Failed,
            "timeout" => Status::Timeout,
            _ => Status::Lost,
        }
    }

    /// Whether the command itself came to an end, well or not: what says something about it.
    pub fn ended(self) -> bool {
        matches!(self, Status::Ok | Status::Failed | Status::Timeout)
    }

    pub fn failed(self) -> bool {
        matches!(self, Status::Failed | Status::Timeout)
    }
}

/// A `routine_runs` row: one run of a routine's command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandRun {
    pub id: i64,
    pub project_slug: String,
    pub routine: String,
    /// `schedule` or `manual`.
    pub fired_by: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// The chief process that runs it.
    pub pid: Option<i64>,
    pub status: Status,
    /// None while it runs, and for a command that couldn't start; 128 + the signal for one a
    /// signal ended, as a shell reports it.
    pub exit_code: Option<i64>,
    /// The end of what it printed, stdout and stderr together.
    pub output: Option<String>,
    /// What the command said of itself with `chief routine outcome`, if it did.
    pub outcome: Option<Verdict>,
    pub headline: Option<String>,
    pub details: Option<String>,
    /// The session started for its news (a routine with a prompt too).
    pub thread: Option<String>,
}

const COLUMNS: &str = "id, project_slug, routine, fired_by, started_at, finished_at, pid, status, \
    exit_code, output, outcome, headline, details, thread";

fn from_row(r: &Row) -> rusqlite::Result<CommandRun> {
    let status: String = r.get(7)?;
    let outcome: Option<String> = r.get(10)?;
    Ok(CommandRun {
        id: r.get(0)?,
        project_slug: r.get(1)?,
        routine: r.get(2)?,
        fired_by: r.get(3)?,
        started_at: r.get(4)?,
        finished_at: r.get(5)?,
        pid: r.get(6)?,
        status: Status::parse(&status),
        exit_code: r.get(8)?,
        output: r.get(9)?,
        outcome: outcome.as_deref().and_then(Verdict::parse),
        headline: r.get(11)?,
        details: r.get(12)?,
        thread: r.get(13)?,
    })
}

/// `0.3s`, `12s`, `3m 05s`, `2h 10m`.
pub fn duration_text(ms: i64) -> String {
    let ms = ms.max(0);
    let secs = ms / 1000;
    match secs {
        0..10 => format!("{}.{}s", secs, (ms % 1000) / 100),
        10..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// The last line of `text` with something on it.
fn last_line(text: &str) -> Option<&str> {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty())
}

/// The end of `text`, at most `bytes` of it, cut at a line when one is near.
fn tail(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut start = text.len() - bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(nl) if nl + 1 < text.len() - start => &text[start + nl + 1..],
        _ => &text[start..],
    }
}

impl CommandRun {
    /// Whether it runs now: its row says so and its process lives. A row with no process yet
    /// is believed for `SPAWN_GRACE_MS`.
    pub fn under_way(&self, now: i64) -> bool {
        self.status == Status::Running
            && match self.pid {
                Some(pid) => alive(pid),
                None => now - self.started_at < SPAWN_GRACE_MS,
            }
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.finished_at.map(|end| (end - self.started_at).max(0))
    }

    /// How it ended, without what it printed: `exit 3`, `timed out after 5m 00s`.
    fn ending(&self) -> String {
        let took = duration_text(self.duration_ms().unwrap_or(0));
        match (self.status, self.exit_code) {
            (Status::Running, _) => format!("running since {}", local_hhmm(self.started_at)),
            (Status::Ok, _) => format!("passed in {took}"),
            (Status::Timeout, _) => format!("timed out after {took}"),
            (Status::Lost, _) => "interrupted: chief stopped while it ran".to_string(),
            (Status::Failed, Some(code)) => format!("exit {code}"),
            (Status::Failed, None) => "didn't start".to_string(),
        }
    }

    /// One line for people: how it ended, and for a failure the last thing it printed.
    pub fn line(&self) -> String {
        let ending = self.ending();
        let last = self.output.as_deref().and_then(last_line);
        match last.filter(|_| self.status.failed()) {
            Some(last) => one_line(&format!("{ending}: {last}"), routines::HEADLINE_MAX),
            None => ending,
        }
    }

    /// For the tools and the dashboard, with the last `output_bytes` of what it printed (none
    /// of it for 0).
    pub fn to_json(&self, output_bytes: usize) -> Value {
        let mut out = json!({
            "id": self.id,
            "status": self.status.as_str(),
            "exit_code": self.exit_code,
            "fired_by": self.fired_by,
            "started_at": self.started_at,
            "started": local_stamp(self.started_at),
            "finished_at": self.finished_at,
            "duration_ms": self.duration_ms(),
            "line": self.line(),
            "outcome": self.outcome.map(Verdict::as_str),
            "headline": self.headline,
            "details": self.details,
            "thread": self.thread,
        });
        if output_bytes > 0 {
            let output = self.output.as_deref().unwrap_or_default();
            out["output"] = json!(tail(output, output_bytes));
            out["output_cut"] = json!(output.len() > output_bytes);
        }
        out
    }
}

/// Whether process `pid` exists.
fn alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks whether the process exists.
    let sent = unsafe { libc::kill(pid, 0) };
    // A process of another user exists too.
    sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<CommandRun>> {
    let sql = format!("SELECT {COLUMNS} FROM routine_runs WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], from_row).optional()?)
}

/// A routine's latest runs, newest first.
pub fn latest(
    conn: &Connection,
    slug: &str,
    routine: &str,
    limit: usize,
) -> Result<Vec<CommandRun>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM routine_runs
         WHERE project_slug = ?1 AND routine = ?2 ORDER BY id DESC LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![slug, routine, limit as i64], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The routine's run that is under way, if any. A run whose process is gone is marked `lost`
/// on the way, so it stops counting.
pub fn under_way(
    conn: &Connection,
    slug: &str,
    routine: &str,
    now: i64,
) -> Result<Option<CommandRun>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM routine_runs
         WHERE project_slug = ?1 AND routine = ?2 AND status = 'running' ORDER BY id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let running = stmt
        .query_map(params![slug, routine], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for run in running {
        if run.under_way(now) {
            return Ok(Some(run));
        }
        conn.execute(
            "UPDATE routine_runs SET status = 'lost', finished_at = ?2
             WHERE id = ?1 AND status = 'running'",
            params![run.id, now],
        )?;
    }
    Ok(None)
}

/// Since when the routine's command fails: the start of the first failed run after its last
/// pass. None while it passes, or hasn't run.
pub fn failing_since(conn: &Connection, slug: &str, routine: &str) -> Result<Option<i64>> {
    Ok(failed_before(conn, slug, routine, i64::MAX)?.1)
}

/// The failed runs in a row right before run `id`: how many, and when the first started.
fn failed_before(
    conn: &Connection,
    slug: &str,
    routine: &str,
    id: i64,
) -> Result<(i64, Option<i64>)> {
    Ok(conn.query_row(
        "SELECT count(*), min(started_at) FROM routine_runs
         WHERE project_slug = ?1 AND routine = ?2 AND id < ?3
           AND status IN ('failed', 'timeout')
           AND id > coalesce((SELECT max(id) FROM routine_runs
                              WHERE project_slug = ?1 AND routine = ?2 AND id < ?3
                                AND status = 'ok'), 0)",
        params![slug, routine, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// The run before `id` that came to an end.
fn previous(conn: &Connection, slug: &str, routine: &str, id: i64) -> Result<Option<CommandRun>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM routine_runs
         WHERE project_slug = ?1 AND routine = ?2 AND id < ?3
           AND status IN ('ok', 'failed', 'timeout')
         ORDER BY id DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![slug, routine, id], from_row)
        .optional()?)
}

/// A new run's row, `running` with no process yet. Returns its id.
pub fn begin(
    conn: &Connection,
    slug: &str,
    routine: &str,
    trigger: Trigger,
    now: i64,
) -> Result<i64> {
    let fired_by = match trigger {
        Trigger::Schedule => "schedule",
        Trigger::Manual => "manual",
    };
    conn.execute(
        "INSERT INTO routine_runs (project_slug, routine, fired_by, started_at, status)
         VALUES (?1, ?2, ?3, ?4, 'running')",
        params![slug, routine, fired_by, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Ends a run that never got to run its command, with why as what it printed.
pub fn abandon(conn: &Connection, id: i64, why: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE routine_runs SET status = 'failed', finished_at = ?2, output = ?3
         WHERE id = ?1 AND status = 'running'",
        params![id, now, why],
    )?;
    Ok(())
}

/// Runs run `id` in a chief process of its own (`chief routine exec`), detached from this one:
/// the scheduler's next look, a tool's answer and a page's request don't wait for a command,
/// and the run records itself even when its starter ends first. That process's own words go
/// to `<root>/logs/routine-runs.log`.
pub fn spawn(ctx: &ToolCtx, id: i64) -> Result<()> {
    let exe = std::env::current_exe().context("finding the chief binary")?;
    let home = crate::serve::home_of(ctx.project_dir);
    let logs = home.join("logs");
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    let log_path = logs.join("routine-runs.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.args(["routine", "exec", &ctx.config.slug, &id.to_string()])
        .env("CHIEF_HOME", &home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it takes the run out of this process's session, so
    // what ends this process (a terminal closing, launchd stopping the server) doesn't end it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting chief routine exec")?;
    ctx.conn.execute(
        "UPDATE routine_runs SET pid = ?2 WHERE id = ?1 AND status = 'running'",
        params![id, child.id()],
    )?;
    // Reaped whenever it ends, so a long-lived starter keeps no zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// What a command's run came to that someone should hear.
#[derive(Debug, Clone, PartialEq)]
pub struct News {
    pub verdict: Verdict,
    pub headline: String,
    pub details: Option<String>,
    /// It passes again after failing: said to people, never handed to a session.
    pub recovery: bool,
}

/// A run that was waited for.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub run: CommandRun,
    /// None when it came to nothing new.
    pub news: Option<News>,
    /// The session started for the news.
    pub thread: Option<String>,
}

/// What a run says that the run before didn't (§13 "News"). A command runs often and repeats
/// itself, so only a change reaches anyone:
/// - what it said itself (`chief routine outcome`), unless it said `nothing` or the run before
///   said the same words;
/// - a failure it didn't explain, unless the run before failed the same way;
/// - its first pass after failing.
fn news_of(run: &CommandRun, previous: Option<&CommandRun>, failed_before: i64) -> Option<News> {
    if let Some(verdict) = run.outcome {
        let repeated =
            previous.is_some_and(|p| p.outcome == run.outcome && p.headline == run.headline);
        if verdict == Verdict::Nothing || repeated {
            return None;
        }
        return Some(News {
            verdict,
            headline: run.headline.clone().unwrap_or_default(),
            details: run.details.clone(),
            recovery: false,
        });
    }
    let failed_too = previous.is_some_and(|p| p.status.failed());
    if run.status.failed() {
        if previous.is_some_and(|p| p.status.failed() && p.outcome.is_none()) {
            return None;
        }
        let printed = run.output.as_deref().unwrap_or_default().trim_end();
        return Some(News {
            verdict: Verdict::Attention,
            headline: one_line(&format!("Failed: {}", run.line()), routines::HEADLINE_MAX),
            details: (!printed.is_empty())
                .then(|| format!("```\n{}\n```", tail(printed, HANDED_BYTES))),
            recovery: false,
        });
    }
    failed_too.then(|| News {
        verdict: Verdict::Update,
        headline: format!(
            "Passes again after {failed_before} failed run{}",
            if failed_before == 1 { "" } else { "s" }
        ),
        details: None,
        recovery: true,
    })
}

/// What ran, as it ended.
struct Ran {
    status: Status,
    exit_code: Option<i64>,
    output: String,
}

/// The last `bytes` of a file, as text.
fn file_tail(path: &Path, bytes: usize) -> String {
    let read = || -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let cut = len > bytes as u64;
        if cut {
            file.seek(SeekFrom::Start(len - bytes as u64))?;
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        // A cut in the middle of a line (or of a character) starts at the next whole line.
        Ok(match text.find('\n').filter(|_| cut) {
            Some(nl) => text[nl + 1..].to_string(),
            None => text,
        })
    };
    read().unwrap_or_default()
}

/// Runs the routine's command to its end or its time limit: `sh -c` in the routine's
/// repository (the project's workspace when it has none), with nothing on stdin and
/// everything it prints in the routine's `last-run.log`. It leads a process group of its own,
/// so what it started dies with it when its time is up.
fn run_command(ctx: &ToolCtx, routine: &Routine, command: &str, id: i64) -> Result<Ran> {
    let (_, cwd) = tools::resolve_place(ctx, routine.repo.as_deref())?;
    let folder = routines::ensure_folder(ctx.project_dir, routine)?;
    let log_path = folder.join(LOG_FILE);
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env("CHIEF_HOME", crate::serve::home_of(ctx.project_dir))
        .env("CHIEF_PROJECT", &ctx.config.slug)
        .env("CHIEF_ROUTINE", &routine.name)
        .env("CHIEF_ROUTINE_DIR", &folder)
        .env("CHIEF_RUN", id.to_string())
        .process_group(0);
    // `chief routine value` and `outcome` are how it speaks, so this chief is on its PATH.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(dir).chain(std::env::split_paths(&rest));
        if let Ok(path) = std::env::join_paths(dirs) {
            cmd.env("PATH", path);
        }
    }
    // The project's account, for a command that calls `claude` itself (§3).
    match &ctx.config.claude_config_dir {
        Some(dir) => cmd.env("CLAUDE_CONFIG_DIR", dir),
        None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
    };
    crate::proc::without_repo_env(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ok(Ran {
                status: Status::Failed,
                exit_code: None,
                output: format!("couldn't start `sh` in {}: {e}", cwd.display()),
            });
        }
    };
    // Its pid is its process group's id (process_group(0)).
    let group = child.id() as libc::pid_t;
    let started = Instant::now();
    let deadline = started + routine.timeout();
    let exit = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // SAFETY: plain syscalls on a group whose leader is unreaped, so the id is still
            // this command's.
            unsafe { libc::kill(-group, libc::SIGTERM) };
            let grace = Instant::now() + KILL_GRACE;
            while child.try_wait()?.is_none() {
                if Instant::now() >= grace {
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            break None;
        }
        // Quick for a command that takes a moment, easy on one that takes an hour.
        std::thread::sleep(if started.elapsed() < Duration::from_secs(2) {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(200)
        });
    };
    let output = file_tail(&log_path, OUTPUT_BYTES);
    Ok(match exit {
        None => Ran {
            status: Status::Timeout,
            exit_code: None,
            output,
        },
        Some(status) => {
            let code = status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .map(i64::from);
            Ran {
                status: if status.success() {
                    Status::Ok
                } else {
                    Status::Failed
                },
                exit_code: code,
                output,
            }
        }
    })
}

/// Why a session starts, for its brief: what the command was, how it ended or what it said,
/// and the end of what it printed.
fn cause(routine: &Routine, run: &CommandRun, news: &News, folder: &Path) -> String {
    let indent = |text: &str| {
        text.lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let what = match run.outcome {
        Some(verdict) => format!(
            "{} and said \"{}: {}\"",
            run.ending(),
            verdict.as_str(),
            news.headline
        ),
        None => run.ending(),
    };
    let printed = run.output.as_deref().unwrap_or_default().trim_end();
    let printed = if printed.is_empty() {
        "It printed nothing.".to_string()
    } else {
        format!(
            "The end of what it printed (all of it is in {}):\n\n{}",
            folder.join(LOG_FILE).display(),
            indent(tail(printed, HANDED_BYTES))
        )
    };
    format!(
        "Why this run started. This routine first runs a command, and a session like this one \
         starts only when the command has news. At {} the command\n\n{}\n\ncame to this: \
         {what}. {printed}",
        local_hhmm(run.started_at),
        indent(routine.command.as_deref().unwrap_or_default()),
    )
}

/// Takes the news where it goes (§13 "News"): to a session, for a routine with a prompt whose
/// last session is done; else to the user and the coordinator as a finding. Returns the
/// session's thread.
fn deliver(ctx: &ToolCtx, routine: &Routine, run: &CommandRun, news: &News) -> Option<String> {
    let name = routine.name.as_str();
    let mut why_not = None;
    if routine.kind() == Kind::Check && !news.recovery {
        match routines::busy_session(ctx, name) {
            Ok(None) => {
                let folder = routines::folder(ctx.project_dir, name);
                let cause = cause(routine, run, news, &folder);
                match routines::start_session(ctx, routine, Some(&cause)) {
                    Ok((thread, _)) => {
                        if let Err(e) = ctx.conn.execute(
                            "UPDATE routine_runs SET thread = ?2 WHERE id = ?1",
                            params![run.id, thread],
                        ) {
                            eprintln!("chief: couldn't note {thread} on run {}: {e:#}", run.id);
                        }
                        return Some(thread);
                    }
                    Err(e) => {
                        why_not = Some(format!("chief couldn't start a session for it: {e:#}"))
                    }
                }
            }
            Ok(Some(busy)) => {
                why_not = Some(format!("its last session ({}) still works", busy.name));
            }
            Err(e) => why_not = Some(format!("chief couldn't look for its last session: {e:#}")),
        }
    }
    // Nobody looks into it, so the user hears of it as it is.
    let details = match (&news.details, why_not) {
        (Some(details), Some(why)) => {
            Some(format!("{details}\n\nNo session looked into it: {why}."))
        }
        (None, Some(why)) => Some(format!("No session looked into it: {why}.")),
        (details, None) => details.clone(),
    };
    let said = Said {
        verdict: news.verdict,
        headline: &news.headline,
        details: details.as_deref(),
        thread: None,
        run: Some(run.id),
    };
    if let Err(e) = routines::announce(ctx, name, &said) {
        eprintln!("chief: couldn't record what routine {name} found: {e:#}");
    }
    None
}

/// Runs run `id` to its end in this process: the command, its row, and its news. A run that
/// can't be run at all ends `failed` with why, and is the error.
pub fn execute(ctx: &ToolCtx, id: i64) -> Result<Finished> {
    let done = execute_run(ctx, id);
    if let Err(e) = &done {
        abandon(
            ctx.conn,
            id,
            &format!("chief couldn't run it: {e:#}"),
            (ctx.now)(),
        )?;
    }
    done
}

fn execute_run(ctx: &ToolCtx, id: i64) -> Result<Finished> {
    let slug = ctx.config.slug.as_str();
    let run = get(ctx.conn, id)?.with_context(|| format!("no run {id}"))?;
    if run.project_slug != slug {
        bail!("run {id} is not of project {slug}");
    }
    if run.status != Status::Running {
        bail!("run {id} has ended already");
    }
    let routine = routines::get(ctx.conn, slug, &run.routine)?
        .with_context(|| format!("routine {} is gone", run.routine))?;
    let command = routine
        .command
        .clone()
        .with_context(|| format!("routine {} has no command", routine.name))?;
    // `routines::run` checks too; this is the door itself.
    if routine.needs_approval() {
        bail!("its command waits for the user's approval");
    }
    ctx.conn.execute(
        "UPDATE routine_runs SET pid = ?2 WHERE id = ?1",
        params![id, std::process::id()],
    )?;
    let ran = run_command(ctx, &routine, &command, id)?;
    ctx.conn.execute(
        "UPDATE routine_runs SET status = ?2, exit_code = ?3, output = ?4, finished_at = ?5
         WHERE id = ?1",
        params![
            id,
            ran.status.as_str(),
            ran.exit_code,
            ran.output,
            (ctx.now)()
        ],
    )?;
    // Read back: what the command said of itself while it ran is on its row.
    let mut run = get(ctx.conn, id)?.with_context(|| format!("run {id} vanished"))?;
    let before = previous(ctx.conn, slug, &routine.name, id)?;
    let (failed_before, _) = failed_before(ctx.conn, slug, &routine.name, id)?;
    let news = news_of(&run, before.as_ref(), failed_before);
    let thread = news
        .as_ref()
        .and_then(|news| deliver(ctx, &routine, &run, news));
    run.thread = thread.clone();
    ctx.conn.execute(
        "DELETE FROM routine_runs WHERE project_slug = ?1 AND routine = ?2 AND id <=
           (SELECT id FROM routine_runs WHERE project_slug = ?1 AND routine = ?2
            ORDER BY id DESC LIMIT 1 OFFSET ?3)",
        params![slug, routine.name, KEPT_RUNS],
    )?;
    Ok(Finished { run, news, thread })
}

/// What a command says of its own run, for `chief routine outcome`: kept on the run's row
/// until the run ends, when it is weighed as news. A later word replaces an earlier one.
pub fn say(
    conn: &Connection,
    id: i64,
    verdict: Verdict,
    headline: &str,
    details: Option<&str>,
) -> Result<()> {
    let headline = one_line(headline, routines::HEADLINE_MAX);
    if headline.is_empty() {
        bail!("the headline is empty: say in one line what the command found");
    }
    let details = details.map(str::trim).filter(|d| !d.is_empty());
    let n = conn.execute(
        "UPDATE routine_runs SET outcome = ?2, headline = ?3, details = ?4
         WHERE id = ?1 AND status = 'running'",
        params![id, verdict.as_str(), headline, details],
    )?;
    if n == 0 {
        bail!("run {id} isn't running: an outcome is said by a routine's command while it runs");
    }
    Ok(())
}

// ---- `chief routine exec|value|outcome` (§2): what a run and its command call ---------------------

/// What a routine's command finds in its environment.
fn from_env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| {
            format!("{name} isn't set: this is for a routine's command, which chief runs with it")
        })
}

/// `chief routine exec <slug> <run>`: the process `spawn` starts.
pub fn cli_exec(slug: &str, id: i64) -> Result<()> {
    let done = routines::for_cli(slug, |ctx| execute(ctx, id));
    if let Err(e) = &done {
        eprintln!(
            "{} {slug}: run {id} couldn't run: {e:#}",
            local_stamp(crate::db::now_ms())
        );
    }
    done.map(|_| ())
}

/// `chief routine value <series> <value>`, from a routine's command: one point of one series,
/// as a session's `record_value`.
pub fn cli_value(
    series: &str,
    value: f64,
    unit: Option<String>,
    note: Option<String>,
) -> Result<()> {
    let slug = from_env("CHIEF_PROJECT")?;
    let routine = from_env("CHIEF_ROUTINE")?;
    let args = RecordValueArgs {
        series: series.to_string(),
        value,
        unit,
        note,
    };
    let recorded = routines::for_cli(&slug, |ctx| {
        routines::record_value_tool(ctx, &routine, &args)
    })?;
    match recorded["previous"].as_f64() {
        Some(previous) => println!("{} = {value} (was {previous})", recorded["series"]),
        None => println!("{} = {value}", recorded["series"]),
    }
    Ok(())
}

/// `chief routine outcome <nothing|update|attention> <headline>`, from a routine's command.
pub fn cli_outcome(outcome: &str, headline: &str, details: Option<&str>) -> Result<()> {
    let verdict = Verdict::parse(outcome)
        .with_context(|| format!("outcome {outcome:?} is not nothing, update or attention"))?;
    let id: i64 = from_env("CHIEF_RUN")?
        .parse()
        .context("CHIEF_RUN is not a run's number")?;
    let conn = crate::db::open(&crate::paths::db())?;
    say(&conn, id, verdict, headline, details)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: Status, output: &str) -> CommandRun {
        CommandRun {
            id: 2,
            project_slug: "demo".into(),
            routine: "check".into(),
            fired_by: "schedule".into(),
            started_at: 1_000,
            finished_at: Some(1_300),
            pid: None,
            status,
            exit_code: match status {
                Status::Ok => Some(0),
                Status::Failed => Some(3),
                _ => None,
            },
            output: Some(output.into()),
            outcome: None,
            headline: None,
            details: None,
            thread: None,
        }
    }

    fn said(mut run: CommandRun, verdict: Verdict, headline: &str) -> CommandRun {
        run.outcome = Some(verdict);
        run.headline = Some(headline.into());
        run
    }

    #[test]
    fn a_run_reads_as_one_line() {
        assert_eq!(run(Status::Ok, "fine\n").line(), "passed in 0.3s");
        assert_eq!(
            run(Status::Failed, "checking\ncurl: (22) 503\n\n").line(),
            "exit 3: curl: (22) 503"
        );
        assert_eq!(run(Status::Failed, "").line(), "exit 3");
        let mut slow = run(Status::Timeout, "still going");
        slow.finished_at = Some(1_000 + 302_000);
        assert_eq!(slow.line(), "timed out after 5m 02s: still going");
        let mut unstarted = run(Status::Failed, "couldn't start `sh`");
        unstarted.exit_code = None;
        assert_eq!(unstarted.line(), "didn't start: couldn't start `sh`");
        assert!(run(Status::Lost, "").line().starts_with("interrupted"));
        for (ms, want) in [
            (0, "0.0s"),
            (9_940, "9.9s"),
            (12_000, "12s"),
            (7_800_000, "2h 10m"),
        ] {
            assert_eq!(duration_text(ms), want);
        }
    }

    #[test]
    fn only_a_change_is_news() {
        let ok = run(Status::Ok, "");
        let failed = run(Status::Failed, "boom");
        // A pass after a pass, and the first pass ever, say nothing.
        assert_eq!(news_of(&ok, Some(&ok), 0), None);
        assert_eq!(news_of(&ok, None, 0), None);
        // The first failure is told; the same failure again isn't.
        let news = news_of(&failed, Some(&ok), 0).unwrap();
        assert_eq!(news.verdict, Verdict::Attention);
        assert_eq!(news.headline, "Failed: exit 3: boom");
        assert_eq!(news.details.as_deref(), Some("```\nboom\n```"));
        assert!(news_of(&failed, None, 0).is_some());
        assert_eq!(news_of(&failed, Some(&failed), 1), None);
        let timeout = run(Status::Timeout, "");
        assert_eq!(news_of(&timeout, Some(&failed), 1), None);
        // The first pass after failing is told once, as news that can wait.
        let back = news_of(&ok, Some(&failed), 3).unwrap();
        assert_eq!(back.verdict, Verdict::Update);
        assert_eq!(back.headline, "Passes again after 3 failed runs");
        assert!(back.recovery);
        assert_eq!(
            news_of(&ok, Some(&timeout), 1).unwrap().headline,
            "Passes again after 1 failed run"
        );
    }

    #[test]
    fn what_a_command_says_is_news_until_it_repeats_itself() {
        let ok = run(Status::Ok, "");
        let low = said(run(Status::Ok, ""), Verdict::Update, "Fare is 412 EUR");
        let news = news_of(&low, Some(&ok), 0).unwrap();
        assert_eq!(
            (news.verdict, news.headline.as_str()),
            (Verdict::Update, "Fare is 412 EUR")
        );
        assert!(!news.recovery);
        // The same words from the run before were heard; other words are news again.
        assert_eq!(news_of(&low, Some(&low), 0), None);
        let lower = said(run(Status::Ok, ""), Verdict::Update, "Fare is 398 EUR");
        assert!(news_of(&lower, Some(&low), 0).is_some());
        // `nothing` is said to nobody, and a pass after it is no recovery.
        let quiet = said(run(Status::Ok, ""), Verdict::Nothing, "No change");
        assert_eq!(news_of(&quiet, Some(&ok), 0), None);
        // A command that explains its own failure is heard in its words, and a failure it
        // doesn't explain after one it did is news of its own.
        let down = said(run(Status::Failed, "x"), Verdict::Attention, "API is down");
        assert_eq!(
            news_of(&down, Some(&ok), 0).unwrap().headline,
            "API is down"
        );
        assert_eq!(news_of(&down, Some(&down), 1), None);
        assert!(news_of(&run(Status::Failed, "x"), Some(&down), 1).is_some());
        // Its pass afterwards is the recovery.
        assert!(news_of(&ok, Some(&down), 2).unwrap().recovery);
    }

    #[test]
    fn the_tail_keeps_whole_lines() {
        assert_eq!(tail("one\ntwo\nthree", 100), "one\ntwo\nthree");
        assert_eq!(tail("one\ntwo\nthree", 8), "three");
        // No line break in reach: the bytes themselves, on a character's edge.
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(last_line("a\nb\n \n"), Some("b"));
        assert_eq!(last_line(" \n"), None);
    }
}
