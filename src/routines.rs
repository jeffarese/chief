//! Routines (ARCHITECTURE §13): work a project runs on a schedule. A prompt, each run a fresh
//! thread; a command chief runs itself, with no session (`crate::command`); or both, the
//! command first and a session only when it has news. The coordinator's tools, `chief routine`,
//! the board and the dashboard all go through these functions; `chief serve` fires the due ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command;
use crate::config::{EFFORTS, ProjectConfig};
use crate::cron::{self, Schedule};
use crate::db::{self, SessionRow};
use crate::state::{Group, Role};
use crate::text::{local_hhmm, local_stamp};
use crate::tools::{self, RunOf, StartThreadArgs, ToolCtx};
use crate::{names, push, registry};

/// `resolved_reason` of a finished run put away when the routine's next run starts.
pub const RESOLVED_BY_ROUTINE: &str = "routine";
const NAME_MAX: usize = 32;
/// The longest command a routine keeps: past this it is a script, which belongs in a file.
const COMMAND_MAX: usize = 16 * 1024;

/// A `routines` row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Routine {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub project_slug: String,
    /// Its handle: `[a-z0-9-]`, unique in the project, never changed.
    pub name: String,
    pub title: String,
    /// What each session is asked to do: the thread's whole task. Empty for a routine that
    /// only runs a command.
    pub prompt: String,
    /// As written (`0 9 * * 1-5`); `cron::Schedule` parses it.
    pub schedule: String,
    /// Overrides of the project's thread settings; None follows them.
    pub repo: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub enabled: bool,
    /// Whether the coordinator is told when a run finishes (§13 "Telling the coordinator").
    pub notify: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// When it next fires (unix ms); None while paused, or for a schedule no day matches.
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    /// The thread of its latest run.
    pub last_thread: Option<String>,
    /// Why the last occurrence didn't start a thread, or what the start warned of.
    pub last_note: Option<String>,
    /// The shell command it runs instead of a session, or before one (§13 "A command").
    pub command: Option<String>,
    /// The words of `command` the user approved: it runs only while they are the same.
    #[serde(skip)]
    pub approved_command: Option<String>,
    /// How long the command may run, in seconds; None is `command::DEFAULT_TIMEOUT_SECS`.
    pub timeout_secs: Option<i64>,
}

/// What a routine's run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A prompt: every run is a Claude session.
    Session,
    /// A command: chief runs it and no session starts.
    Command,
    /// Both: the command runs first, and a session starts only when it has news.
    Check,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Command => "command",
            Kind::Check => "check",
        }
    }

    /// For people, in a few words.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Command => "command",
            Kind::Check => "command, then session",
        }
    }
}

impl Routine {
    pub fn kind(&self) -> Kind {
        match (&self.command, self.prompt.trim().is_empty()) {
            (None, _) => Kind::Session,
            (Some(_), true) => Kind::Command,
            (Some(_), false) => Kind::Check,
        }
    }

    /// Whether its command still waits for the user's yes: the coordinator wrote or changed it
    /// since the user last approved its words (§13 "Approval").
    pub fn needs_approval(&self) -> bool {
        self.command.is_some() && self.approved_command != self.command
    }

    /// How long its command may run.
    pub fn timeout(&self) -> std::time::Duration {
        let secs = self.timeout_secs.unwrap_or(command::DEFAULT_TIMEOUT_SECS);
        std::time::Duration::from_secs(secs.max(1) as u64)
    }
}

/// Who writes a routine, which decides whether its command may run (§13 "Approval").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    /// The user: `chief routine`, the board, the dashboard. The command they give is approved.
    User,
    /// The coordinator's tools. A command it writes or changes waits for the user.
    Coordinator,
}

const COLUMNS: &str = "id, project_slug, name, title, prompt, schedule, repo, model, effort, \
    enabled, notify, created_at, updated_at, next_run_at, last_run_at, last_thread, last_note, \
    command, approved_command, timeout_secs";

fn from_row(r: &Row) -> rusqlite::Result<Routine> {
    Ok(Routine {
        id: r.get(0)?,
        project_slug: r.get(1)?,
        name: r.get(2)?,
        title: r.get(3)?,
        prompt: r.get(4)?,
        schedule: r.get(5)?,
        repo: r.get(6)?,
        model: r.get(7)?,
        effort: r.get(8)?,
        enabled: r.get(9)?,
        notify: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        next_run_at: r.get(13)?,
        last_run_at: r.get(14)?,
        last_thread: r.get(15)?,
        last_note: r.get(16)?,
        command: r.get(17)?,
        approved_command: r.get(18)?,
        timeout_secs: r.get(19)?,
    })
}

/// A project's routines, by name.
pub fn list(conn: &Connection, slug: &str) -> Result<Vec<Routine>> {
    let sql = format!("SELECT {COLUMNS} FROM routines WHERE project_slug = ?1 ORDER BY name");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([slug], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, slug: &str, name: &str) -> Result<Option<Routine>> {
    let sql = format!("SELECT {COLUMNS} FROM routines WHERE project_slug = ?1 AND name = ?2");
    Ok(conn
        .query_row(&sql, params![slug, name], from_row)
        .optional()?)
}

fn must_get(conn: &Connection, slug: &str, name: &str) -> Result<Routine> {
    get(conn, slug, name)?.with_context(|| {
        let known: Vec<String> = list(conn, slug)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.name)
            .collect();
        if known.is_empty() {
            format!("no routine {name:?}: this project has none")
        } else {
            format!("no routine {name:?}; this project has {}", known.join(", "))
        }
    })
}

fn save(conn: &Connection, r: &Routine) -> Result<()> {
    let n = conn.execute(
        "UPDATE routines SET title = ?2, prompt = ?3, schedule = ?4, repo = ?5, model = ?6,
           effort = ?7, enabled = ?8, notify = ?9, updated_at = ?10, next_run_at = ?11,
           last_run_at = ?12, last_thread = ?13, last_note = ?14, command = ?15,
           approved_command = ?16, timeout_secs = ?17
         WHERE id = ?1",
        params![
            r.id,
            r.title,
            r.prompt,
            r.schedule,
            r.repo,
            r.model,
            r.effort,
            r.enabled,
            r.notify,
            r.updated_at,
            r.next_run_at,
            r.last_run_at,
            r.last_thread,
            r.last_note,
            r.command,
            r.approved_command,
            r.timeout_secs,
        ],
    )?;
    anyhow::ensure!(n == 1, "routine {} is gone", r.name);
    Ok(())
}

/// The enabled routines of a project whose time has come.
pub fn due(conn: &Connection, slug: &str, now: i64) -> Result<Vec<Routine>> {
    Ok(list(conn, slug)?
        .into_iter()
        .filter(|r| r.enabled && r.next_run_at.is_some_and(|at| at <= now))
        .collect())
}

/// Whether a due routine's run depends on what its sessions do now, so the rows should be
/// brought up to date from `claude agents` first: a session to start, or a check whose last
/// session the stored rows show at work (it may have ended since). A command alone asks
/// nothing of the sessions.
pub fn due_needs_rows(ctx: &ToolCtx) -> Result<bool> {
    for r in due(ctx.conn, &ctx.config.slug, (ctx.now)())? {
        let needs = match r.kind() {
            Kind::Session => true,
            Kind::Check => busy_session(ctx, &r.name)?.is_some(),
            Kind::Command => false,
        };
        if needs {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The names of a project's routines that don't tell the coordinator (`notify = false`).
pub fn quiet(conn: &Connection, slug: &str) -> Result<Vec<String>> {
    Ok(list(conn, slug)?
        .into_iter()
        .filter(|r| !r.notify)
        .map(|r| r.name)
        .collect())
}

/// A new routine, as the coordinator, `chief routine add` or the dashboard gives it.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Draft {
    /// A few words for the board, e.g. "Triage new issues". Each run's thread carries it.
    pub title: String,
    /// What a session does: the thread's whole brief, since it sees neither your conversation
    /// nor earlier runs, and can't ask anyone. What to check and where, what counts as news,
    /// which numbers to record, what to do when something is missing, and when the user wants
    /// to be interrupted ("attention only if a fare is under 700 EUR") rather than merely
    /// told. chief already tells every run how runs work (it reads and rewrites the routine's
    /// notes file, records the values it tracks and ends with an outcome), so don't repeat
    /// that. Leave it out for a routine that only runs a command. With a command too, the
    /// session starts only when the command has news (it failed, or reported an outcome), and
    /// is handed its output.
    #[serde(default)]
    pub prompt: String,
    /// A shell command (`sh -c`) chief runs itself instead of a session: a check, a script, a
    /// cron job. It costs no tokens, so use it for whatever needs no judgement. Exit status 0
    /// means all is well; anything else is a failure. It repeats, so people hear only what
    /// changed: its first failure and its first pass afterwards, not the same result every
    /// run. It can say more than its exit status: `chief routine value <series> <number>
    /// [--unit U]` records a number it tracks, and `chief routine outcome
    /// nothing|update|attention "<headline>"` says what the run came to (the same words twice
    /// in a row are said once). It runs in the routine's repository, with `CHIEF_ROUTINE_DIR`
    /// (the routine's folder, for what it keeps between runs) in its environment. The user
    /// must approve a command you write before it runs.
    #[serde(default)]
    pub command: Option<String>,
    /// How long the command may run, in seconds. Default 300, at most 86400.
    #[serde(default)]
    pub timeout_secs: Option<i64>,
    /// When it runs, in the user's local time: five cron fields (`0 9 * * 1-5` = weekdays at
    /// 09:00), `@hourly`, `@daily`, `@weekly`, `@monthly`, or `every 30m` / `every 2h`. At most
    /// every 5 minutes for a prompt alone; a routine with a command may run every minute.
    /// Not more often than the thing it follows changes.
    pub schedule: String,
    /// A repository name from the project's settings. Default: the project's default
    /// repository.
    #[serde(default)]
    pub repo: Option<String>,
    /// Model override for a session's runs. Runs repeat, so keep them cheap: "haiku" or
    /// "sonnet" for a lookup or a comparison. Default: the project's thread model, for real
    /// analysis or code.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort override: low, medium, high, xhigh or max; low for a lookup or a comparison.
    /// Default: the project's thread effort.
    #[serde(default)]
    pub effort: Option<String>,
    /// Whether you are told when a run finishes. Default true; false for runs nobody needs to
    /// hear about unless they look.
    #[serde(default)]
    pub notify: Option<bool>,
    /// False creates it paused. Default true.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Its handle, lowercase letters, digits and hyphens. Default: made from the title.
    #[serde(default)]
    pub name: Option<String>,
}

/// Changes to a routine: only the fields given change.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Patch {
    #[serde(default)]
    pub title: Option<String>,
    /// An empty string leaves only the command.
    #[serde(default)]
    pub prompt: Option<String>,
    /// See create_routine's command; an empty string removes it. A command you change waits
    /// for the user's approval again.
    #[serde(default)]
    pub command: Option<String>,
    /// Seconds the command may run; 0 goes back to the default.
    #[serde(default)]
    pub timeout_secs: Option<i64>,
    /// See create_routine's schedule.
    #[serde(default)]
    pub schedule: Option<String>,
    /// A repository name; an empty string goes back to the project's default.
    #[serde(default)]
    pub repo: Option<String>,
    /// A model; an empty string goes back to the project's thread model.
    #[serde(default)]
    pub model: Option<String>,
    /// An effort; an empty string goes back to the project's thread effort.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub notify: Option<bool>,
    /// False pauses it; true resumes it from now (runs missed while paused are not made up).
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// What every routine must satisfy, new or changed. Returns its parsed schedule.
fn check(config: &ProjectConfig, r: &Routine) -> Result<Schedule> {
    if r.title.trim().is_empty() {
        bail!("title is empty");
    }
    match r.kind() {
        Kind::Session if r.prompt.trim().is_empty() => bail!(
            "prompt is empty: give a prompt (each run a Claude session), a command (a script \
             chief runs itself), or both"
        ),
        Kind::Session => {}
        Kind::Command | Kind::Check => {
            let command = r.command.as_deref().unwrap_or_default();
            if command.len() > COMMAND_MAX {
                bail!(
                    "command is {} bytes; keep it under {COMMAND_MAX} by putting the script in \
                     a file and running that",
                    command.len()
                );
            }
            if let Some(secs) = r.timeout_secs
                && !(1..=command::MAX_TIMEOUT_SECS).contains(&secs)
            {
                bail!(
                    "timeout_secs must be between 1 and {} (a day)",
                    command::MAX_TIMEOUT_SECS
                );
            }
        }
    }
    let schedule = Schedule::parse(&r.schedule)?;
    // What a run costs decides how often it may fire: a session is minutes of a model, a
    // command a process.
    match r.kind() {
        Kind::Session => schedule.check_gap(cron::MIN_GAP_MINUTES, "each run is a session")?,
        Kind::Command | Kind::Check => schedule.check_gap(
            cron::MIN_GAP_COMMAND_MINUTES,
            "a command runs at most every minute",
        )?,
    }
    if let Some(effort) = &r.effort
        && !EFFORTS.contains(&effort.as_str())
    {
        bail!("effort {effort:?} is not one of {}", EFFORTS.join(", "));
    }
    // A run with nowhere to work fails every time, so say so now. An absolute path is refused:
    // the coordinator's deny rules and trust are set up for the project's repositories only. A
    // project without repositories works in its workspace (`tools::resolve_place`).
    if r.repo.is_some() || !config.repos.is_empty() {
        let (repo, _) = tools::resolve_repo(config, r.repo.as_deref())?;
        if r.repo.is_some() && config.repo(&repo).is_none() {
            bail!("a routine runs in one of the project's repositories, by name");
        }
    }
    Ok(schedule)
}

/// A free handle for a new routine: the one asked for, or one made from the title (`-2`, `-3`
/// … when taken).
fn pick_name(conn: &Connection, slug: &str, draft: &Draft) -> Result<String> {
    if let Some(name) = clean(draft.name.as_deref()) {
        let valid = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && name.chars().any(|c| c.is_ascii_alphanumeric())
            && name.len() <= NAME_MAX;
        if !valid {
            bail!(
                "name {name:?} must be lowercase letters, digits and hyphens, at most \
                 {NAME_MAX} long"
            );
        }
        if get(conn, slug, &name)?.is_some() {
            bail!("a routine named {name} exists already; update it, or pick another name");
        }
        return Ok(name);
    }
    let mut base = names::slugify(&draft.title, NAME_MAX - 4);
    if base.is_empty() {
        base = "routine".to_string();
    }
    for n in 1..100 {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        if get(conn, slug, &name)?.is_none() {
            return Ok(name);
        }
    }
    bail!("could not find a free name for {:?}", draft.title)
}

pub fn create(
    conn: &Connection,
    config: &ProjectConfig,
    draft: &Draft,
    by: Author,
    now: i64,
) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let command = clean(draft.command.as_deref());
        let mut routine = Routine {
            id: 0,
            project_slug: config.slug.clone(),
            name: pick_name(conn, &config.slug, draft)?,
            title: draft.title.trim().to_string(),
            prompt: draft.prompt.trim().to_string(),
            schedule: draft.schedule.trim().to_string(),
            repo: clean(draft.repo.as_deref()),
            model: clean(draft.model.as_deref()),
            effort: clean(draft.effort.as_deref()),
            enabled: draft.enabled.unwrap_or(true),
            notify: draft.notify.unwrap_or(true),
            created_at: now,
            updated_at: now,
            next_run_at: None,
            last_run_at: None,
            last_thread: None,
            last_note: None,
            // The user's own words are approved as they give them; the coordinator's wait.
            approved_command: command.clone().filter(|_| by == Author::User),
            timeout_secs: draft.timeout_secs.filter(|_| command.is_some()),
            command,
        };
        let schedule = check(config, &routine)?;
        if routine.enabled {
            routine.next_run_at = schedule.next_after_ms(now);
        }
        conn.execute(
            "INSERT INTO routines (project_slug, name, title, prompt, schedule, repo, model,
               effort, enabled, notify, created_at, updated_at, next_run_at, command,
               approved_command, timeout_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                routine.project_slug,
                routine.name,
                routine.title,
                routine.prompt,
                routine.schedule,
                routine.repo,
                routine.model,
                routine.effort,
                routine.enabled,
                routine.notify,
                routine.created_at,
                routine.updated_at,
                routine.next_run_at,
                routine.command,
                routine.approved_command,
                routine.timeout_secs,
            ],
        )
        .with_context(|| format!("storing routine {}", routine.name))?;
        routine.id = conn.last_insert_rowid();
        Ok(routine)
    })
}

pub fn update(
    conn: &Connection,
    config: &ProjectConfig,
    name: &str,
    patch: &Patch,
    by: Author,
    now: i64,
) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let mut routine = must_get(conn, &config.slug, name)?;
        let was = routine.clone();
        if let Some(command) = &patch.command {
            routine.command = clean(Some(command));
            // The words the user gives are the words they approve. The coordinator's change
            // leaves the approval on the old words, so the new ones wait.
            if by == Author::User {
                routine.approved_command = routine.command.clone();
            }
        }
        if let Some(secs) = patch.timeout_secs {
            routine.timeout_secs = Some(secs).filter(|s| *s != 0);
        }
        if routine.command.is_none() {
            routine.approved_command = None;
            routine.timeout_secs = None;
        }
        if let Some(title) = &patch.title {
            routine.title = title.trim().to_string();
        }
        if let Some(prompt) = &patch.prompt {
            routine.prompt = prompt.trim().to_string();
        }
        if let Some(schedule) = &patch.schedule {
            routine.schedule = schedule.trim().to_string();
        }
        // An empty string is "back to the project's default".
        if let Some(repo) = &patch.repo {
            routine.repo = clean(Some(repo));
        }
        if let Some(model) = &patch.model {
            routine.model = clean(Some(model));
        }
        if let Some(effort) = &patch.effort {
            routine.effort = clean(Some(effort));
        }
        if let Some(notify) = patch.notify {
            routine.notify = notify;
        }
        if let Some(enabled) = patch.enabled {
            routine.enabled = enabled;
        }
        let schedule = check(config, &routine)?;
        // The next run moves only when what decides it changed: an edit to the prompt mustn't
        // push an interval routine's next run further away.
        if !routine.enabled {
            routine.next_run_at = None;
        } else if !was.enabled || was.schedule != routine.schedule || routine.next_run_at.is_none()
        {
            routine.next_run_at = schedule.next_after_ms(now);
        }
        routine.updated_at = now;
        save(conn, &routine)?;
        Ok(routine)
    })
}

/// The user's yes to a routine's command: it may run (§13 "Approval"). `shown` is the words
/// they were shown, when a screen showed them: a command the coordinator changed while they
/// read isn't the one they said yes to, so it is refused, in the same transaction.
pub fn approve(
    conn: &Connection,
    slug: &str,
    name: &str,
    shown: Option<&str>,
    now: i64,
) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let mut routine = must_get(conn, slug, name)?;
        if routine.command.is_none() {
            bail!("routine {name} runs no command: there is nothing to approve");
        }
        if shown.is_some_and(|shown| routine.command.as_deref() != Some(shown)) {
            bail!("{name}'s command changed since it was shown: read it again, then approve it");
        }
        routine.approved_command = routine.command.clone();
        // Whatever said it was waiting is answered.
        routine.last_note = None;
        routine.updated_at = now;
        save(conn, &routine)?;
        Ok(routine)
    })
}

/// Removes a routine. Its runs' threads stay, with their reports.
pub fn delete(conn: &Connection, slug: &str, name: &str) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let routine = must_get(conn, slug, name)?;
        conn.execute("DELETE FROM routines WHERE id = ?1", [routine.id])?;
        Ok(routine)
    })
}

/// How many of a series' latest points ride along wherever routines are listed.
const SPARK_POINTS: usize = 30;
/// At most this many series per routine there; `read` gives them all.
const LISTED_SERIES: usize = 4;

/// How a routine stands, in one word and one line: what the board, the dashboard and `chief
/// routine` all show for it, so the three agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// `approval` (its command waits for the user's yes), `running` (its command runs now),
    /// `failing` (its command's last run failed), `ok` (it passed), `attention`, `update` or
    /// `nothing` (what its last run said), `new` (it hasn't run).
    pub key: &'static str,
    /// The word for it: "Needs approval", "Failing", "OK", "Needs you" …
    pub label: &'static str,
    /// One line of what stands behind the word; empty when there is nothing to add.
    pub line: String,
    /// Since when (unix ms), when that is known.
    pub at: Option<i64>,
}

impl State {
    /// Whether the user should look: a command to approve, a failing command, or a run that
    /// said `attention`.
    pub fn needs_user(&self) -> bool {
        matches!(self.key, "approval" | "failing" | "attention")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "key": self.key,
            "label": self.label,
            "line": self.line,
            "at": self.at,
            "needs_user": self.needs_user(),
        })
    }
}

fn state_of_finding(finding: Option<&Finding>) -> State {
    match finding {
        Some(f) => State {
            key: f.verdict.as_str(),
            label: match f.verdict {
                Verdict::Attention => "Needs you",
                Verdict::Update => "Update",
                Verdict::Nothing => "Nothing new",
            },
            line: f.headline.clone(),
            at: Some(f.at),
        },
        None => State {
            key: "new",
            label: "No run yet",
            line: String::new(),
            at: None,
        },
    }
}

/// `State` of a routine as the database has it at `now`.
pub fn state(conn: &Connection, r: &Routine, now: i64) -> Result<State> {
    if r.needs_approval() {
        return Ok(State {
            key: "approval",
            label: "Needs approval",
            line: "its command runs once you approve it".to_string(),
            at: None,
        });
    }
    let finding = findings(conn, &r.project_slug, &r.name, 1)?;
    if r.kind() == Kind::Session {
        return Ok(state_of_finding(finding.first()));
    }
    let runs = command::latest(conn, &r.project_slug, &r.name, 2)?;
    if let Some(run) = runs.first().filter(|run| run.under_way(now)) {
        return Ok(State {
            key: "running",
            label: "Running",
            line: format!("since {}", local_hhmm(run.started_at)),
            at: Some(run.started_at),
        });
    }
    // The last run that came to an end: one that was lost says nothing of the command.
    let Some(run) = runs.iter().find(|run| run.status.ended()) else {
        return Ok(state_of_finding(finding.first()));
    };
    if run.status.failed() {
        let since = command::failing_since(conn, &r.project_slug, &r.name)?;
        // What a session made of the failure says more than the exit status.
        let said = finding.first().filter(|f| {
            f.thread.is_some()
                && since.is_some_and(|since| f.at >= since)
                && f.verdict != Verdict::Nothing
        });
        return Ok(State {
            key: "failing",
            label: "Failing",
            line: match said {
                Some(f) => f.headline.clone(),
                None => run.line(),
            },
            at: since.or(Some(run.started_at)),
        });
    }
    Ok(match run.outcome.filter(|v| *v != Verdict::Nothing) {
        Some(verdict) => State {
            key: verdict.as_str(),
            label: if verdict == Verdict::Attention {
                "Needs you"
            } else {
                "Update"
            },
            line: run.headline.clone().unwrap_or_default(),
            at: run.finished_at,
        },
        None => State {
            key: "ok",
            label: "OK",
            line: run.line(),
            at: run.finished_at,
        },
    })
}

/// A routine as the tools and the dashboard show it: its row, what kind of run it is, its
/// schedule in words, its times as local text beside the timestamps, how it stands, what its
/// last run found, its command's last run, and the latest of what it tracks.
pub fn view(conn: &Connection, r: &Routine) -> Result<Value> {
    let mut out = serde_json::to_value(r).unwrap_or_default();
    out["kind"] = json!(r.kind().as_str());
    out["needs_approval"] = json!(r.needs_approval());
    out["state"] = state(conn, r, db::now_ms())?.to_json();
    out["last_exec"] = match command::latest(conn, &r.project_slug, &r.name, 1)?.first() {
        Some(run) => run.to_json(0),
        None => Value::Null,
    };
    out["schedule_text"] = json!(
        Schedule::parse(&r.schedule)
            .map(|s| s.describe())
            .unwrap_or_else(|_| r.schedule.clone())
    );
    out["next_run"] = json!(r.next_run_at.map(local_stamp));
    out["last_run"] = json!(r.last_run_at.map(local_stamp));
    out["last_finding"] = match findings(conn, &r.project_slug, &r.name, 1)?.first() {
        Some(f) => f.to_json(),
        None => Value::Null,
    };
    out["series"] = series(conn, &r.project_slug, &r.name, SPARK_POINTS)?
        .iter()
        .take(LISTED_SERIES)
        .map(Series::to_json)
        .collect();
    Ok(out)
}

/// What asked for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Its time came (`chief serve`).
    Schedule,
    /// Someone asked: the coordinator's `run_routine`, `chief routine run`, the dashboard.
    Manual,
}

/// What a run came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Started {
        thread: String,
        warning: Option<String>,
    },
    /// Its command runs now, in a process of its own (`command::spawn`).
    Running { run: i64 },
    /// Its command ran and was waited for (`run_here`).
    Ran(Box<command::Finished>),
    /// Nothing started, on purpose: paused, not due, its last run still works, or its command
    /// waits for approval.
    Skipped(String),
}

impl Outcome {
    pub fn to_json(&self, routine: &str) -> Value {
        match self {
            Outcome::Started { thread, warning } => {
                let mut out = json!({"routine": routine, "started": true, "thread": thread});
                if let Some(w) = warning {
                    out["warning"] = json!(w);
                }
                out
            }
            Outcome::Running { run } => json!({
                "routine": routine,
                "started": true,
                "run": run,
                "note": "its command runs now; read_routine shows how it ended and what it printed",
            }),
            Outcome::Ran(done) => {
                let mut out = json!({"routine": routine, "started": true});
                out["run"] = done.run.to_json(command::OUTPUT_BYTES);
                if let Some(thread) = &done.thread {
                    out["thread"] = json!(thread);
                }
                if let Some(news) = &done.news {
                    out["news"] = json!({
                        "outcome": news.verdict.as_str(),
                        "headline": news.headline,
                    });
                }
                out
            }
            Outcome::Skipped(why) => json!({"routine": routine, "started": false, "note": why}),
        }
    }
}

/// A run of this routine that still occupies someone: working, waiting on the user, or stopped
/// on a limit.
fn busy(row: &SessionRow) -> bool {
    !row.resolved
        && row.cleaned_at.is_none()
        && row.removed_at.is_none()
        && matches!(
            Group::parse(&row.group_name),
            Some(Group::Working | Group::Waiting | Group::Limited)
        )
}

/// The routine's own folder (`<project>/routines/<name>`): its notes and whatever else its
/// runs keep. It outlives the routine.
pub fn folder(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join("routines").join(name)
}

pub const NOTES_FILE: &str = "NOTES.md";

/// Makes the routine's folder and, the first time, its notes file. A run's memory is that
/// file: runs are separate sessions.
pub fn ensure_folder(project_dir: &Path, routine: &Routine) -> Result<PathBuf> {
    let dir = folder(project_dir, &routine.name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let notes = dir.join(NOTES_FILE);
    if !notes.exists() {
        let text = format!(
            "# {}\n\n<!-- What the runs of this routine remember. Each run reads this first and \
             rewrites it before it ends: the current state, what was checked and when, what to \
             watch next. Keep it short. You can edit it too: the next run reads what is here. \
             -->\n",
            routine.title
        );
        std::fs::write(&notes, text).with_context(|| format!("writing {}", notes.display()))?;
    }
    Ok(dir.canonicalize().unwrap_or(dir))
}

/// The brief of a run: how a run works (its notes, the values it records, the verdict it ends
/// with), why it started when a command's news started it (`cause`), then the routine's prompt.
pub fn brief(routine: &Routine, folder: &Path, cause: Option<&str>) -> String {
    let cause = match cause {
        Some(cause) => format!("{cause}\n\n"),
        None => String::new(),
    };
    let when = Schedule::parse(&routine.schedule)
        .map(|s| s.describe())
        .unwrap_or_else(|_| routine.schedule.clone());
    let notes = folder.join(NOTES_FILE);
    format!(
        "This is a scheduled run of the routine \"{title}\" ({when}). Nobody is watching it: \
         don't wait for an answer. If something you need is missing, say exactly what in your \
         outcome and stop.\n\n\
         Memory. Earlier runs were separate sessions; what they left for you is {notes}. Read it \
         first. Before you finish, rewrite it for the next run: the current state, what you \
         checked and when, what to watch. Keep it under about 150 lines by replacing what is \
         stale, not appending. Anything else worth keeping goes in {folder} too.\n\n\
         Values. When a number matters over time (a price, a rate, a balance), record it with \
         the record_value tool: one series per thing, the same series name every run.\n\n\
         Outcome. End by calling report_outcome once: \"nothing\" when nothing changed that the \
         user would want to hear, \"update\" for news that can wait, \"attention\" when they \
         should look or decide now. Its headline is one line they read on their phone: lead \
         with the fact, with the number in it.\n\n\
         Be economical: do what the task asks and no more.\n\n{cause}{prompt}",
        title = routine.title,
        notes = notes.display(),
        folder = folder.display(),
        prompt = routine.prompt,
    )
}

/// Writes what an occurrence came to on the routine's row: `text` as its `last_note`, and for
/// one that ran, when and (for a session) which thread. A failure to write is only logged: the
/// run itself happened.
pub(crate) fn note(
    ctx: &ToolCtx,
    name: &str,
    text: Option<String>,
    ran: Option<(i64, Option<&str>)>,
) {
    let written = db::immediate(ctx.conn, |conn| {
        let Some(mut r) = get(conn, &ctx.config.slug, name)? else {
            return Ok(());
        };
        r.last_note = text.clone();
        if let Some((at, thread)) = ran {
            r.last_run_at = Some(at);
            if let Some(thread) = thread {
                r.last_thread = Some(thread.to_string());
            }
        }
        save(conn, &r)
    });
    if let Err(e) = written {
        eprintln!("chief: couldn't record routine {name}'s run: {e:#}");
    }
}

/// The routine's sessions, newest first as the database lists them.
fn sessions_of(ctx: &ToolCtx, name: &str) -> Result<Vec<SessionRow>> {
    Ok(
        db::list_sessions(ctx.conn, &ctx.config.slug, Some(Role::Thread))?
            .into_iter()
            .filter(|t| t.routine.as_deref() == Some(name))
            .collect(),
    )
}

/// A session of this routine that still occupies someone, if any.
pub(crate) fn busy_session(ctx: &ToolCtx, name: &str) -> Result<Option<SessionRow>> {
    Ok(sessions_of(ctx, name)?.into_iter().find(busy))
}

/// Starts a session for the routine (§13 "A run", steps 3 and 4): its finished runs are put
/// away, then a fresh thread gets its brief. `cause` is what a command's news adds to the brief.
/// Returns the thread and what the start warned of; the routine's row is stamped either way,
/// and a start that fails is its `last_note` and the error.
pub(crate) fn start_session(
    ctx: &ToolCtx,
    routine: &Routine,
    cause: Option<&str>,
) -> Result<(String, Option<String>)> {
    let name = routine.name.as_str();
    let now = (ctx.now)();
    // One finished run per routine stays on the board: the ones before it are put away. A run
    // with an open pull request (review, landing) or a failure stays until someone deals with it.
    for old in sessions_of(ctx, name)?
        .iter()
        .filter(|t| !t.resolved && Group::parse(&t.group_name) == Some(Group::Idle))
    {
        if let Err(e) = registry::set_resolved(ctx.conn, &old.name, true, RESOLVED_BY_ROUTINE, now)
        {
            eprintln!("chief: couldn't resolve {}: {e:#}", old.name);
        }
    }

    let failed = |e: anyhow::Error| {
        note(
            ctx,
            name,
            Some(format!("failed at {}: {e:#}", local_hhmm(now))),
            None,
        );
        e.context(format!("starting a run of routine {name}"))
    };
    let folder = match ensure_folder(ctx.project_dir, routine) {
        Ok(folder) => folder,
        Err(e) => return Err(failed(e)),
    };
    let args = StartThreadArgs {
        title: routine.title.clone(),
        task: brief(routine, &folder, cause),
        repo: routine.repo.clone(),
        model: routine.model.clone(),
        effort: routine.effort.clone(),
        account: None,
    };
    let of = RunOf {
        routine: name.to_string(),
        folder,
    };
    let started = match tools::start_thread_as(ctx, &args, Some(&of), &|| false) {
        Ok(started) => started,
        Err(e) => return Err(failed(e)),
    };
    let thread = started["name"].as_str().unwrap_or_default().to_string();
    let warning = started["warning"].as_str().map(str::to_string);
    note(ctx, name, warning.clone(), Some((now, Some(&thread))));
    Ok((thread, warning))
}

/// Runs a routine once (§13 "A run"): a fresh thread with its prompt, or its command in a
/// process of its own. A scheduled run first claims the occurrence by moving `next_run_at` on,
/// in one transaction, so two processes looking at once start one run. An occurrence whose last
/// run is still busy, or whose command waits for approval, is skipped, and a start that fails
/// is recorded in `last_note` and returned as the error.
pub fn run(ctx: &ToolCtx, name: &str, trigger: Trigger) -> Result<Outcome> {
    run_as(ctx, name, trigger, false)
}

/// `run` that waits for a command to end, for whoever watches it: `chief routine run`.
pub fn run_here(ctx: &ToolCtx, name: &str, trigger: Trigger) -> Result<Outcome> {
    run_as(ctx, name, trigger, true)
}

fn run_as(ctx: &ToolCtx, name: &str, trigger: Trigger, wait: bool) -> Result<Outcome> {
    let slug = ctx.config.slug.as_str();
    let now = (ctx.now)();
    let claimed = db::immediate(ctx.conn, |conn| {
        let mut routine = must_get(conn, slug, name)?;
        if trigger == Trigger::Schedule {
            if !routine.enabled || routine.next_run_at.is_none_or(|at| at > now) {
                return Ok(Err("not due".to_string()));
            }
            // However many occurrences were missed (chief serve wasn't running), this is the
            // one run that makes up for them.
            routine.next_run_at = Schedule::parse(&routine.schedule)
                .ok()
                .and_then(|s| s.next_after_ms(now));
            save(conn, &routine)?;
        }
        Ok(Ok(routine))
    })?;
    let routine = match claimed {
        Ok(routine) => routine,
        Err(why) => return Ok(Outcome::Skipped(why)),
    };
    let skipped = |why: String| {
        note(ctx, name, Some(why.clone()), None);
        Ok(Outcome::Skipped(why))
    };
    if routine.needs_approval() {
        return skipped(format!(
            "skipped at {}: its command waits for the user's approval",
            local_hhmm(now)
        ));
    }
    if let Some(last) = busy_session(ctx, name)? {
        return skipped(format!(
            "skipped at {}: its last run ({}) is still {}",
            local_hhmm(now),
            last.name,
            last.group_name
        ));
    }
    if routine.kind() == Kind::Session {
        let (thread, warning) = start_session(ctx, &routine, None)?;
        return Ok(Outcome::Started { thread, warning });
    }
    if let Some(last) = command::under_way(ctx.conn, slug, name, now)? {
        return skipped(format!(
            "skipped at {}: its command still runs (since {})",
            local_hhmm(now),
            local_hhmm(last.started_at)
        ));
    }
    let run = command::begin(ctx.conn, slug, name, trigger, now)?;
    note(ctx, name, None, Some((now, None)));
    if wait {
        return Ok(Outcome::Ran(Box::new(command::execute(ctx, run)?)));
    }
    if let Err(e) = command::spawn(ctx, run) {
        let why = format!("failed at {}: {e:#}", local_hhmm(now));
        command::abandon(ctx.conn, run, &why, now)?;
        note(ctx, name, Some(why), None);
        return Err(e.context(format!("starting the command of routine {name}")));
    }
    Ok(Outcome::Running { run })
}

/// Runs every due routine of the context's project, in name order. One that fails doesn't stop
/// the others; each result is returned with its routine's name.
pub fn run_due(ctx: &ToolCtx) -> Result<Vec<(String, Result<Outcome>)>> {
    let now = (ctx.now)();
    Ok(due(ctx.conn, &ctx.config.slug, now)?
        .into_iter()
        .map(|r| {
            let outcome = run(ctx, &r.name, Trigger::Schedule);
            (r.name, outcome)
        })
        .collect())
}

/// How long after a run's report `chief serve` waits before it wakes a sleeping coordinator for
/// it (§13 "Telling the coordinator"): a turn the Stop hook kept going (the final report, §5) has
/// moved on by then, and a coordinator that is awake has heard from its own watcher.
pub const WAKE_AFTER_MS: i64 = 30_000;
const WAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The finished runs worth waking a sleeping coordinator for: due to be told
/// (`waiting::routine_due`, and a command's findings in `untold`), said to need the user
/// (`Verdict::Attention`), and reported at least `WAKE_AFTER_MS` ago. Each thread's name (or
/// `finding <id>` for a command's) and that report's time. News that can wait is
/// told on the coordinator's next turn, at no turn of its own.
pub fn unheard(conn: &Connection, slug: &str, now: i64) -> Result<Vec<(String, i64)>> {
    let threads = db::list_sessions(conn, slug, Some(Role::Thread))?;
    let quiet = quiet(conn, slug)?;
    let verdicts = verdicts(conn, slug)?;
    let mut out: Vec<(String, i64)> = crate::waiting::routine_due(&threads, &quiet, &verdicts)
        .into_iter()
        .filter(|t| verdicts.get(&t.name).map(|f| f.verdict) == Some(Verdict::Attention))
        .filter_map(|t| Some((t.name.clone(), t.last_report_at?)))
        .collect();
    // A command's finding has no thread: its id stands for it.
    out.extend(
        untold(conn, slug)?
            .into_iter()
            .filter(|f| f.verdict == Verdict::Attention)
            .map(|f| (format!("finding {}", f.id), f.at)),
    );
    out.retain(|(_, at)| now - at >= WAKE_AFTER_MS);
    Ok(out)
}

/// Wakes the project's coordinator when it has no process, so it hears of finished runs: its
/// SessionStart hook starts the watcher, which tells it (§5 "Telling the coordinator"). True
/// when it was asleep and respawned; false when it runs already (its watcher tells it) or was
/// never started (it reads the board when the user first opens it).
pub fn wake_coordinator(ctx: &ToolCtx) -> Result<bool> {
    let slug = ctx.config.slug.as_str();
    let Some(row) = db::get_coordinator(ctx.conn, slug)?.filter(|r| r.short_id.is_some()) else {
        return Ok(false);
    };
    crate::accounts::refresh_row(
        ctx.conn,
        ctx.claude,
        &row,
        crate::claude::AGENTS_TIMEOUT,
        ctx.now,
    )?;
    let row = db::get_session(ctx.conn, &row.name)?.context("the coordinator's row vanished")?;
    if row.pid.is_some() {
        return Ok(false);
    }
    crate::project::respawn(
        ctx.conn,
        ctx.claude,
        ctx.agp,
        ctx.project_dir,
        ctx.config,
        &row,
        WAKE_WAIT,
    )?;
    Ok(true)
}

// ---- What a run leaves (§13) --------------------------------------------------------------------

/// What a run says it came to, which decides who hears of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing changed that the user would want to hear: nobody is told.
    Nothing,
    /// News that can wait: the coordinator hears on its next turn.
    Update,
    /// The user should look or decide now: the coordinator is woken and the user pushed.
    Attention,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Nothing => "nothing",
            Verdict::Update => "update",
            Verdict::Attention => "attention",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Verdict> {
        match s {
            "nothing" => Some(Verdict::Nothing),
            "update" => Some(Verdict::Update),
            "attention" => Some(Verdict::Attention),
            _ => None,
        }
    }
}

/// A `routine_outcomes` row: what one run found.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub id: i64,
    pub routine: String,
    /// The run's thread, when a session said it.
    pub thread: Option<String>,
    /// The command's run (`routine_runs.id`), when chief concluded it from one.
    pub run: Option<i64>,
    pub at: i64,
    pub verdict: Verdict,
    /// One line for the user.
    pub headline: String,
    pub details: Option<String>,
}

impl Finding {
    pub fn to_json(&self) -> Value {
        json!({
            "outcome": self.verdict.as_str(),
            "headline": self.headline,
            "details": self.details,
            "at": self.at,
            "when": local_stamp(self.at),
            "thread": self.thread,
            "run": self.run,
        })
    }
}

fn finding_from_row(r: &Row) -> rusqlite::Result<Finding> {
    let verdict: String = r.get(3)?;
    Ok(Finding {
        id: r.get(6)?,
        run: r.get(7)?,
        routine: r.get(0)?,
        thread: r.get(1)?,
        at: r.get(2)?,
        verdict: Verdict::parse(&verdict).unwrap_or(Verdict::Update),
        headline: r.get(4)?,
        details: r.get(5)?,
    })
}

const FINDING_COLUMNS: &str = "routine, thread, ts, outcome, headline, details, id, run";

/// A routine's latest findings, newest first.
pub fn findings(
    conn: &Connection,
    slug: &str,
    routine: &str,
    limit: usize,
) -> Result<Vec<Finding>> {
    let sql = format!(
        "SELECT {FINDING_COLUMNS} FROM routine_outcomes
         WHERE project_slug = ?1 AND routine = ?2 ORDER BY id DESC LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![slug, routine, limit as i64], finding_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// What each run of the project last said of itself, by thread. A run that said nothing isn't
/// in it.
pub fn verdicts(conn: &Connection, slug: &str) -> Result<HashMap<String, Finding>> {
    let sql = format!(
        "SELECT {FINDING_COLUMNS} FROM routine_outcomes
         WHERE project_slug = ?1 AND thread IS NOT NULL ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([slug], finding_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // In id order, so a run's later word replaces its earlier one.
    Ok(rows
        .into_iter()
        .filter_map(|f| Some((f.thread.clone()?, f)))
        .collect())
}

/// The findings of commands' runs the coordinator wasn't told of, oldest first (§13 "Who
/// hears of a run"). A session's finding is told with its thread; one that said `nothing`, or
/// whose routine tells nobody, was stored as told.
pub fn untold(conn: &Connection, slug: &str) -> Result<Vec<Finding>> {
    let sql = format!(
        "SELECT {FINDING_COLUMNS} FROM routine_outcomes
         WHERE project_slug = ?1 AND run IS NOT NULL AND told_at IS NULL ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([slug], finding_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Marks a command's findings told.
pub fn mark_told(conn: &Connection, findings: &[Finding], now: i64) -> Result<()> {
    for f in findings {
        conn.execute(
            "UPDATE routine_outcomes SET told_at = ?2 WHERE id = ?1",
            params![f.id, now],
        )?;
    }
    Ok(())
}

/// Whether `thread` has said what it came to.
pub fn reported(conn: &Connection, thread: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM routine_outcomes WHERE thread = ?1 LIMIT 1",
            [thread],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The routine's run that is under way, or its latest: the newest thread that carries its name.
pub fn current_run(conn: &Connection, slug: &str, routine: &str) -> Result<Option<SessionRow>> {
    Ok(db::list_sessions(conn, slug, Some(Role::Thread))?
        .into_iter()
        .filter(|t| t.routine.as_deref() == Some(routine))
        .max_by_key(|t| t.created_at))
}

/// One thing a routine tracks over time.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    /// The unit of its latest point.
    pub unit: Option<String>,
    /// Oldest first: when, and the value.
    pub points: Vec<(i64, f64)>,
}

impl Series {
    pub fn to_json(&self) -> Value {
        let latest = self.points.last();
        let previous = self.points.iter().rev().nth(1);
        json!({
            "name": self.name,
            "unit": self.unit,
            "latest": latest.map(|p| p.1),
            "at": latest.map(|p| p.0),
            "previous": previous.map(|p| p.1),
            "points": self.points.iter().map(|(at, v)| json!([at, v])).collect::<Vec<_>>(),
        })
    }
}

/// The routine's series, most recently recorded first, each with its latest `points` points.
pub fn series(conn: &Connection, slug: &str, routine: &str, points: usize) -> Result<Vec<Series>> {
    let mut names = conn.prepare(
        "SELECT series, max(id) FROM routine_points
         WHERE project_slug = ?1 AND routine = ?2 GROUP BY series ORDER BY max(id) DESC",
    )?;
    let names = names
        .query_map(params![slug, routine], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut read = conn.prepare(
        "SELECT ts, value, unit FROM routine_points
         WHERE project_slug = ?1 AND routine = ?2 AND series = ?3 ORDER BY id DESC LIMIT ?4",
    )?;
    let mut out = Vec::new();
    for name in names {
        let mut rows = read
            .query_map(params![slug, routine, name, points as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let unit = rows.first().and_then(|r| r.2.clone());
        rows.reverse();
        out.push(Series {
            name,
            unit,
            points: rows.into_iter().map(|(at, v, _)| (at, v)).collect(),
        });
    }
    Ok(out)
}

const SERIES_MAX: usize = 60;
pub(crate) const HEADLINE_MAX: usize = 200;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RecordValueArgs {
    /// What the number is, e.g. "BCN-NYC lowest fare" or "outstanding principal". Use exactly
    /// the same name every run: it is how the points join into one line over time.
    pub series: String,
    /// The number, without its unit.
    pub value: f64,
    /// Its unit, e.g. "EUR", "%", "days".
    #[serde(default)]
    pub unit: Option<String>,
    /// A few words about this point, e.g. the airline or the bank.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReportOutcomeArgs {
    /// "nothing": nothing changed that the user would want to hear. "update": news that can
    /// wait until they next look. "attention": they should look or decide now.
    pub outcome: Verdict,
    /// One line the user reads on their phone. Lead with the fact and put the number in it:
    /// "BCN-NYC dropped to 412 EUR on 14 Mar (was 498)".
    pub headline: String,
    /// What they need to know beyond the headline, in a few lines. Optional.
    #[serde(default)]
    pub details: Option<String>,
}

/// `record_value`, a run's tool: one point of one series of its routine.
pub fn record_value_tool(ctx: &ToolCtx, routine: &str, args: &RecordValueArgs) -> Result<Value> {
    let slug = ctx.config.slug.as_str();
    let name = args.series.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        bail!("series is empty: name what the number is");
    }
    if name.chars().count() > SERIES_MAX {
        bail!("series is a short name (at most {SERIES_MAX} characters), not a sentence");
    }
    if !args.value.is_finite() {
        bail!("value must be a number");
    }
    // The same thing under another case is the same series: take the name already in use.
    let known = series(ctx.conn, slug, routine, 1)?;
    let name = known
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(&name))
        .map(|s| s.name.clone())
        .unwrap_or(name);
    ctx.conn.execute(
        "INSERT INTO routine_points (project_slug, routine, series, ts, value, unit, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            slug,
            routine,
            name,
            (ctx.now)(),
            args.value,
            clean(args.unit.as_deref()),
            clean(args.note.as_deref()),
        ],
    )?;
    let recorded = series(ctx.conn, slug, routine, 2)?
        .into_iter()
        .find(|s| s.name == name);
    let previous = recorded
        .as_ref()
        .and_then(|s| s.points.iter().rev().nth(1).map(|p| p.1));
    Ok(json!({"series": name, "recorded": args.value, "previous": previous}))
}

/// What a run came to, in the words to store: a session's own (`thread`), or what chief made
/// of a command's run (`run`).
pub struct Said<'a> {
    pub verdict: Verdict,
    pub headline: &'a str,
    pub details: Option<&'a str>,
    pub thread: Option<&'a str>,
    pub run: Option<i64>,
}

/// Stores a finding and reaches the user for it: what needs them is pushed at once (the
/// desktop notification, and the project's push command); the coordinator hears through its
/// watcher (§13 "Who hears of a run"). Returns whether the push command ran.
pub fn announce(ctx: &ToolCtx, routine: &str, said: &Said) -> Result<bool> {
    let slug = ctx.config.slug.as_str();
    let now = (ctx.now)();
    let row = get(ctx.conn, slug, routine)?;
    let title = row
        .as_ref()
        .map(|r| r.title.clone())
        .unwrap_or_else(|| routine.to_string());
    // A command's finding is told by itself (`untold`), so one nobody is to hear of is stored
    // as told: nothing to say, a routine that tells nobody, or one that is gone.
    let silent = said.verdict == Verdict::Nothing || !row.is_some_and(|r| r.notify);
    let told_at = (said.run.is_some() && silent).then_some(now);
    ctx.conn.execute(
        "INSERT INTO routine_outcomes
           (project_slug, routine, thread, ts, outcome, headline, details, run, told_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            slug,
            routine,
            said.thread,
            now,
            said.verdict.as_str(),
            said.headline,
            clean(said.details),
            said.run,
            told_at,
        ],
    )?;
    let kind = match said.verdict {
        Verdict::Nothing => return Ok(false),
        Verdict::Update => push::Kind::Update,
        Verdict::Attention => push::Kind::Attention,
    };
    let message = push::Message {
        kind,
        title: format!("{} · {}", ctx.config.name, title),
        body: said.headline.to_string(),
        url: push::dashboard_url(ctx.config, &format!("#/r/{routine}")),
    };
    let pushed = push::send(ctx.config, &message);
    if kind == push::Kind::Attention && ctx.config.ui.notify {
        push::desktop(&message.title, &message.body);
    }
    Ok(pushed)
}

/// `report_outcome`, a run's tool: its verdict and a line for the user (`announce`).
pub fn report_outcome_tool(
    ctx: &ToolCtx,
    routine: &str,
    args: &ReportOutcomeArgs,
) -> Result<Value> {
    let slug = ctx.config.slug.as_str();
    let headline = crate::text::one_line(&args.headline, HEADLINE_MAX);
    if headline.is_empty() {
        bail!("headline is empty: say in one line what you found");
    }
    let run = current_run(ctx.conn, slug, routine)?;
    let pushed = announce(
        ctx,
        routine,
        &Said {
            verdict: args.outcome,
            headline: &headline,
            details: args.details.as_deref(),
            thread: run.as_ref().map(|r| r.name.as_str()),
            run: None,
        },
    )?;
    Ok(json!({
        "recorded": args.outcome.as_str(),
        "pushed": pushed,
        "next": "Make sure your notes file is up to date, then end your turn with a short report.",
    }))
}

/// How much of a routine's notes the coordinator is handed.
const NOTES_READ_BYTES: usize = 8 * 1024;
const READ_FINDINGS: usize = 10;
const READ_POINTS: usize = 60;
const READ_RUNS: usize = 5;

/// `read_routine`, the coordinator's: everything a routine knows without reading a thread.
pub fn read_tool(ctx: &ToolCtx, args: &RoutineNameArgs) -> Result<Value> {
    read(
        ctx,
        args.name.trim(),
        NOTES_READ_BYTES,
        READ_FINDINGS,
        READ_POINTS,
        READ_RUNS,
    )
}

/// How much of an earlier command run's output rides along; the latest run's comes whole.
const EARLIER_OUTPUT_BYTES: usize = 2 * 1024;

/// A routine in full: its row as listed, its notes (at most `notes_bytes` of them), its latest
/// findings, every series with its latest points, and its command's latest `runs` with what
/// they printed.
pub fn read(
    ctx: &ToolCtx,
    name: &str,
    notes_bytes: usize,
    findings_kept: usize,
    points: usize,
    runs: usize,
) -> Result<Value> {
    let slug = ctx.config.slug.as_str();
    let routine = must_get(ctx.conn, slug, name)?;
    let mut out = view(ctx.conn, &routine)?;
    let notes_path = folder(ctx.project_dir, &routine.name).join(NOTES_FILE);
    let notes = std::fs::read_to_string(&notes_path).unwrap_or_default();
    let cut = notes.len() > notes_bytes;
    let mut end = notes.len().min(notes_bytes);
    while !notes.is_char_boundary(end) {
        end -= 1;
    }
    out["notes"] = json!(notes[..end]);
    out["notes_file"] = json!(notes_path.display().to_string());
    if cut {
        out["notes_cut"] = json!("the notes go on; read notes_file for the rest");
    }
    out["findings"] = findings(ctx.conn, slug, &routine.name, findings_kept)?
        .iter()
        .map(Finding::to_json)
        .collect();
    out["series"] = series(ctx.conn, slug, &routine.name, points)?
        .iter()
        .map(Series::to_json)
        .collect();
    out["runs"] = command::latest(ctx.conn, slug, &routine.name, runs)?
        .iter()
        .enumerate()
        .map(|(i, run)| {
            run.to_json(if i == 0 {
                command::OUTPUT_BYTES
            } else {
                EARLIER_OUTPUT_BYTES
            })
        })
        .collect();
    if routine.command.is_some() {
        out["log_file"] = json!(
            folder(ctx.project_dir, &routine.name)
                .join(command::LOG_FILE)
                .display()
                .to_string()
        );
    }
    Ok(out)
}

// ---- The coordinator's tools (§7) -------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RoutineNameArgs {
    /// The routine's name, from list_routines.
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateRoutineArgs {
    /// The routine's name, from list_routines.
    pub name: String,
    #[serde(flatten)]
    pub changes: Patch,
}

/// Said with every answer about routines while nothing would fire them.
fn scheduler_note(ctx: &ToolCtx, out: &mut Value) {
    if !crate::serve::running_in(&crate::serve::home_of(ctx.project_dir)) {
        out["scheduler"] = json!(
            "chief serve isn't running, so no routine fires on its schedule. Tell the user: \
             they start it with `chief serve start` (and `chief serve install` keeps it running after a \
             restart). run_routine still works."
        );
    }
}

pub fn list_tool(ctx: &ToolCtx) -> Result<Value> {
    let routines = list(ctx.conn, &ctx.config.slug)?
        .iter()
        .map(|r| view(ctx.conn, r))
        .collect::<Result<Vec<Value>>>()?;
    let mut out = json!({ "routines": routines });
    scheduler_note(ctx, &mut out);
    Ok(out)
}

/// Said with a routine whose command the user hasn't approved: the coordinator can't, and
/// must say where the user does.
fn approval_note(ctx: &ToolCtx, routine: &Routine, out: &mut Value) {
    if routine.needs_approval() {
        out["approval"] = json!(format!(
            "Its command doesn't run until the user approves it: you can't. Show them the \
             command and tell them to approve it on the board (select the routine, press a), \
             in the dashboard, or with `chief routine approve {} {}`.",
            ctx.config.slug, routine.name
        ));
    }
}

pub fn create_tool(ctx: &ToolCtx, draft: &Draft) -> Result<Value> {
    let routine = create(
        ctx.conn,
        ctx.config,
        draft,
        Author::Coordinator,
        (ctx.now)(),
    )?;
    ensure_folder(ctx.project_dir, &routine)?;
    let mut out = view(ctx.conn, &routine)?;
    approval_note(ctx, &routine, &mut out);
    out["notes"] = json!(
        folder(ctx.project_dir, &routine.name)
            .join(NOTES_FILE)
            .display()
            .to_string()
    );
    scheduler_note(ctx, &mut out);
    Ok(out)
}

pub fn update_tool(ctx: &ToolCtx, args: &UpdateRoutineArgs) -> Result<Value> {
    let routine = update(
        ctx.conn,
        ctx.config,
        args.name.trim(),
        &args.changes,
        Author::Coordinator,
        (ctx.now)(),
    )?;
    let mut out = view(ctx.conn, &routine)?;
    approval_note(ctx, &routine, &mut out);
    scheduler_note(ctx, &mut out);
    Ok(out)
}

pub fn delete_tool(ctx: &ToolCtx, args: &RoutineNameArgs) -> Result<Value> {
    let routine = delete(ctx.conn, &ctx.config.slug, args.name.trim())?;
    Ok(json!({
        "name": routine.name,
        "deleted": true,
        "note": "Its past runs' threads and reports stay.",
    }))
}

pub fn run_tool(ctx: &ToolCtx, args: &RoutineNameArgs) -> Result<Value> {
    let name = args.name.trim();
    let outcome = run(ctx, name, Trigger::Manual)?;
    let mut out = outcome.to_json(name);
    match &outcome {
        Outcome::Started { thread, .. } => {
            out["next"] = json!(format!(
                "{thread} is a thread like any other: list_threads shows it and read_report \
                 reads its report. chief tells you when it finishes."
            ));
        }
        Outcome::Running { .. } => {
            out["next"] = json!(
                "Its command runs in the background. chief tells you if it has news (it \
                 fails, or says something); read_routine shows the run either way."
            );
        }
        Outcome::Ran(_) | Outcome::Skipped(_) => {}
    }
    Ok(out)
}

// ---- `chief routine …` (§2) -------------------------------------------------------------------

/// The project's context for a command typed by the user: an unknown slug says so plainly.
pub(crate) fn for_cli<T>(slug: &str, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
    let dir = crate::project::project_dir(slug);
    if !dir.join("project.toml").exists() {
        bail!("no chief project {slug:?} (looked in {})", dir.display());
    }
    tools::with_project(slug, f)
}

/// One routine in a few lines, for `chief routine list` and after a change.
pub fn render(r: &Routine) -> String {
    let schedule = Schedule::parse(&r.schedule)
        .map(|s| s.describe())
        .unwrap_or_else(|_| r.schedule.clone());
    let mut head = format!(
        "{}  \"{}\" · {} · {schedule}",
        r.name,
        r.title,
        r.kind().label()
    );
    if !r.enabled {
        head.push_str(" · paused");
    } else if let Some(next) = r.next_run_at {
        head.push_str(&format!(" · next {}", local_stamp(next)));
    } else {
        head.push_str(" · never due");
    }
    let mut lines = vec![head];
    let mut how = Vec::new();
    if let Some(repo) = &r.repo {
        how.push(format!("repo {repo}"));
    }
    if let Some(model) = &r.model {
        how.push(format!("model {model}"));
    }
    if let Some(effort) = &r.effort {
        how.push(format!("effort {effort}"));
    }
    if !r.notify {
        how.push("quiet".to_string());
    }
    if !how.is_empty() {
        lines.push(format!("  {}", how.join(" · ")));
    }
    if let Some(command) = &r.command {
        let mut line = format!("  $ {}", crate::text::one_line(command, 200));
        if let Some(secs) = r.timeout_secs {
            line.push_str(&format!("  (at most {secs}s)"));
        }
        lines.push(line);
        if r.needs_approval() {
            lines.push(format!(
                "  waits for your approval: chief routine approve {} {}",
                r.project_slug, r.name
            ));
        }
    }
    match (r.last_run_at, &r.last_thread) {
        (Some(at), Some(thread)) if r.kind() == Kind::Session => {
            lines.push(format!("  last run {} · {thread}", local_stamp(at)));
        }
        (Some(at), _) if r.kind() != Kind::Session => {
            lines.push(format!("  last run {}", local_stamp(at)));
        }
        _ => {}
    }
    if let Some(note) = &r.last_note {
        lines.push(format!("  {note}"));
    }
    lines.join("\n")
}

/// A project's routines for `chief status`, under its threads: a header and a line each, as
/// the board's Routines section says them. Nothing for a project without routines.
pub fn render_status(conn: &Connection, slug: &str, now: i64) -> Result<String> {
    let routines = list(conn, slug)?;
    if routines.is_empty() {
        return Ok(String::new());
    }
    let mut out = format!("  Routines ({})\n", routines.len());
    for r in &routines {
        let schedule = Schedule::parse(&r.schedule)
            .map(|s| s.describe())
            .unwrap_or_else(|_| r.schedule.clone());
        let mut parts = vec![
            r.name.clone(),
            format!("\"{}\"", crate::text::one_line(&r.title, 60)),
            r.kind().label().to_string(),
            schedule,
        ];
        let state = state(conn, r, now)?;
        parts.push(if state.line.is_empty() {
            state.label.to_string()
        } else {
            format!(
                "{}: {}",
                state.label,
                crate::text::one_line(&state.line, 120)
            )
        });
        parts.push(match r.next_run_at {
            _ if !r.enabled => "paused".to_string(),
            Some(next) => format!("next {}", local_stamp(next)),
            None => "never due".to_string(),
        });
        out.push_str(&format!("    {}\n", parts.join(" · ")));
    }
    Ok(out)
}

/// The line under every listing while nothing would fire them.
fn cli_scheduler_note(ctx: &ToolCtx) {
    if !crate::serve::running_in(&crate::serve::home_of(ctx.project_dir)) {
        println!(
            "chief serve isn't running, so nothing fires on schedule: `chief serve start` \
             (and `chief serve install` to keep it running)."
        );
    }
}

/// `render`, with how the routine stands under it.
fn render_with_state(ctx: &ToolCtx, r: &Routine) -> Result<String> {
    let state = state(ctx.conn, r, (ctx.now)())?;
    let mut text = render(r);
    if state.key != "new" && state.key != "approval" {
        text.push_str(&format!("\n  {}", state.label));
        if !state.line.is_empty() {
            text.push_str(&format!(": {}", state.line));
        }
    }
    Ok(text)
}

pub fn cli_list(slug: &str) -> Result<()> {
    for_cli(slug, |ctx| {
        let routines = list(ctx.conn, slug)?;
        if routines.is_empty() {
            println!(
                "{slug} has no routines. Ask the coordinator for one, or: chief routine add \
                 {slug} --title … --schedule \"0 9 * * 1-5\" --prompt … (a session each run) \
                 or --command … (a script, no session)"
            );
            return Ok(());
        }
        for r in &routines {
            println!("{}", render_with_state(ctx, r)?);
        }
        cli_scheduler_note(ctx);
        Ok(())
    })
}

/// Whether this process was started by a Claude session (Claude Code sets `CLAUDECODE` for
/// what its Bash tool runs). `chief routine` typed there isn't the user's hand: a thread has
/// Bash, and the coordinator could ask one to approve for it (§13 "Approval").
fn in_claude_session() -> bool {
    std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty())
}

pub fn cli_add(slug: &str, draft: &Draft) -> Result<()> {
    for_cli(slug, |ctx| {
        let by = if in_claude_session() {
            Author::Coordinator
        } else {
            Author::User
        };
        let routine = create(ctx.conn, ctx.config, draft, by, (ctx.now)())?;
        ensure_folder(ctx.project_dir, &routine)?;
        println!("Added {}", render(&routine));
        cli_scheduler_note(ctx);
        Ok(())
    })
}

pub fn cli_update(slug: &str, name: &str, patch: &Patch) -> Result<()> {
    for_cli(slug, |ctx| {
        let routine = update(ctx.conn, ctx.config, name, patch, Author::User, (ctx.now)())?;
        println!("{}", render(&routine));
        Ok(())
    })
}

/// `chief routine approve`: the user's yes to a command the coordinator wrote.
pub fn cli_approve(slug: &str, name: &str) -> Result<()> {
    if in_claude_session() {
        bail!(
            "approving a routine's command is the user's to do, not a Claude session's: ask \
             them to run `chief routine approve {slug} {name}` in their own terminal, or to \
             approve it on the board or in the dashboard"
        );
    }
    for_cli(slug, |ctx| {
        let routine = approve(ctx.conn, slug, name, None, (ctx.now)())?;
        println!(
            "Approved the command of {}:\n  $ {}",
            routine.name,
            routine.command.as_deref().unwrap_or_default()
        );
        cli_scheduler_note(ctx);
        Ok(())
    })
}

/// `chief routine show`: one routine with how it stands, its command's latest runs and what
/// the last one printed.
pub fn cli_show(slug: &str, name: &str) -> Result<()> {
    for_cli(slug, |ctx| {
        let routine = must_get(ctx.conn, slug, name)?;
        println!("{}", render_with_state(ctx, &routine)?);
        if !routine.prompt.is_empty() {
            println!("\nPrompt:\n{}", routine.prompt);
        }
        let found = findings(ctx.conn, slug, name, 5)?;
        if !found.is_empty() {
            println!("\nFound:");
            for f in &found {
                println!(
                    "  {}  {}: {}",
                    local_stamp(f.at),
                    f.verdict.as_str(),
                    f.headline
                );
            }
        }
        let runs = command::latest(ctx.conn, slug, name, 10)?;
        if !runs.is_empty() {
            println!("\nRuns of its command:");
            for run in &runs {
                println!("  {}  {}", local_stamp(run.started_at), run.line());
            }
            if let Some(output) = runs[0].output.as_deref().filter(|o| !o.trim().is_empty()) {
                println!("\nThe last run printed:\n{}", output.trim_end());
            }
        }
        Ok(())
    })
}

pub fn cli_rm(slug: &str, name: &str) -> Result<()> {
    for_cli(slug, |ctx| {
        let routine = delete(ctx.conn, slug, name)?;
        println!("Deleted {}. Its past runs stay on the board.", routine.name);
        Ok(())
    })
}

pub fn cli_run(slug: &str, name: &str) -> Result<()> {
    for_cli(slug, |ctx| {
        // The rows as they are now decide whether its last run still works.
        if let Err(e) = tools::refresh(ctx) {
            eprintln!("warning: couldn't refresh from claude agents: {e:#}");
        }
        // Whoever typed this watches it: a command is waited for and its output shown.
        match run_here(ctx, name, Trigger::Manual)? {
            Outcome::Started { thread, warning } => {
                println!("Started {thread}.");
                if let Some(w) = warning {
                    eprintln!("warning: {w}");
                }
            }
            Outcome::Running { run } => println!("Its command runs (run {run})."),
            Outcome::Ran(done) => {
                if let Some(output) = done.run.output.as_deref().filter(|o| !o.trim().is_empty()) {
                    println!("{}", output.trim_end());
                }
                println!("{}.", done.run.line());
                if let Some(news) = &done.news {
                    println!("{}: {}", news.verdict.as_str(), news.headline);
                }
                if let Some(thread) = &done.thread {
                    println!("Started {thread} for it.");
                }
            }
            Outcome::Skipped(why) => println!("Nothing started: {why}."),
        }
        Ok(())
    })
}
