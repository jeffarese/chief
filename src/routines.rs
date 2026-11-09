//! Routines (ARCHITECTURE §13): a prompt a project runs on a schedule, each run a fresh thread.
//! The coordinator's tools, `chief routine` and the dashboard all go through these functions;
//! `chief serve` fires the due ones.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{EFFORTS, ProjectConfig};
use crate::cron::Schedule;
use crate::db::{self, SessionRow};
use crate::state::{Group, Role};
use crate::text::{local_hhmm, local_stamp};
use crate::tools::{self, StartThreadArgs, ToolCtx};
use crate::{names, registry};

/// `resolved_reason` of a finished run put away when the routine's next run starts.
pub const RESOLVED_BY_ROUTINE: &str = "routine";
const NAME_MAX: usize = 32;

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
    /// What each run is asked to do: the thread's whole task.
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
}

const COLUMNS: &str = "id, project_slug, name, title, prompt, schedule, repo, model, effort, \
    enabled, notify, created_at, updated_at, next_run_at, last_run_at, last_thread, last_note";

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
           last_run_at = ?12, last_thread = ?13, last_note = ?14
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
    /// What every run does: the thread's whole brief, since it sees neither your conversation
    /// nor earlier runs. Objective, what done means, what to report.
    pub prompt: String,
    /// When it runs, in the user's local time: five cron fields (`0 9 * * 1-5` = weekdays at
    /// 09:00), `@hourly`, `@daily`, `@weekly`, `@monthly`, or `every 30m` / `every 2h`. At most
    /// every 5 minutes.
    pub schedule: String,
    /// A repository name from the project's settings. Default: the project's default
    /// repository.
    #[serde(default)]
    pub repo: Option<String>,
    /// Model override, e.g. "haiku" for a cheap check. Default: the project's thread model.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort override: low, medium, high, xhigh or max. Default: the project's thread effort.
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
    #[serde(default)]
    pub prompt: Option<String>,
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
    if r.prompt.trim().is_empty() {
        bail!("prompt is empty: it is each run's whole brief");
    }
    let schedule = Schedule::parse(&r.schedule)?;
    if let Some(effort) = &r.effort
        && !EFFORTS.contains(&effort.as_str())
    {
        bail!("effort {effort:?} is not one of {}", EFFORTS.join(", "));
    }
    // A run with nowhere to work fails every time, so say so now. An absolute path is refused:
    // the coordinator's deny rules and trust are set up for the project's repositories only.
    let (repo, _) = tools::resolve_repo(config, r.repo.as_deref())?;
    if r.repo.is_some() && config.repo(&repo).is_none() {
        bail!("a routine runs in one of the project's repositories, by name");
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
    now: i64,
) -> Result<Routine> {
    db::immediate(conn, |conn| {
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
        };
        let schedule = check(config, &routine)?;
        if routine.enabled {
            routine.next_run_at = schedule.next_after_ms(now);
        }
        conn.execute(
            "INSERT INTO routines (project_slug, name, title, prompt, schedule, repo, model,
               effort, enabled, notify, created_at, updated_at, next_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
    now: i64,
) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let mut routine = must_get(conn, &config.slug, name)?;
        let was = routine.clone();
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

/// Removes a routine. Its runs' threads stay, with their reports.
pub fn delete(conn: &Connection, slug: &str, name: &str) -> Result<Routine> {
    db::immediate(conn, |conn| {
        let routine = must_get(conn, slug, name)?;
        conn.execute("DELETE FROM routines WHERE id = ?1", [routine.id])?;
        Ok(routine)
    })
}

/// A routine as the tools and the dashboard show it: its row, its schedule in words and its
/// times as local text beside the timestamps.
pub fn view(r: &Routine) -> Value {
    let mut out = serde_json::to_value(r).unwrap_or_default();
    out["schedule_text"] = json!(
        Schedule::parse(&r.schedule)
            .map(|s| s.describe())
            .unwrap_or_else(|_| r.schedule.clone())
    );
    out["next_run"] = json!(r.next_run_at.map(local_stamp));
    out["last_run"] = json!(r.last_run_at.map(local_stamp));
    out
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
    /// Nothing started, on purpose: paused, not due, or its last run still works.
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

/// The brief of a run: the routine's prompt under a line that says nobody is watching.
pub fn brief(routine: &Routine) -> String {
    let when = Schedule::parse(&routine.schedule)
        .map(|s| s.describe())
        .unwrap_or_else(|_| routine.schedule.clone());
    format!(
        "This is a scheduled run of the routine \"{}\" ({when}). Nobody is watching it: don't \
         wait for an answer. If something you need is missing, say exactly what in your report \
         and stop. Earlier runs were separate sessions; you don't have their conversation.\n\n{}",
        routine.title, routine.prompt
    )
}

/// Runs a routine once: a fresh thread with its prompt (§13 "A run"). A scheduled run first
/// claims the occurrence by moving `next_run_at` on, in one transaction, so two processes
/// looking at once start one thread. An occurrence whose last run is still busy is skipped, and
/// a start that fails is recorded in `last_note` and returned as the error.
pub fn run(ctx: &ToolCtx, name: &str, trigger: Trigger) -> Result<Outcome> {
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
    let note = |text: Option<String>, thread: Option<&str>| {
        let written = db::immediate(ctx.conn, |conn| {
            let Some(mut r) = get(conn, slug, name)? else {
                return Ok(());
            };
            r.last_note = text.clone();
            if let Some(thread) = thread {
                r.last_thread = Some(thread.to_string());
                r.last_run_at = Some(now);
            }
            save(conn, &r)
        });
        if let Err(e) = written {
            eprintln!("chief: couldn't record routine {name}'s run: {e:#}");
        }
    };

    let runs: Vec<SessionRow> = db::list_sessions(ctx.conn, slug, Some(Role::Thread))?
        .into_iter()
        .filter(|t| t.routine.as_deref() == Some(name))
        .collect();
    if let Some(last) = runs.iter().find(|t| busy(t)) {
        let why = format!(
            "skipped at {}: its last run ({}) is still {}",
            local_hhmm(now),
            last.name,
            last.group_name
        );
        note(Some(why.clone()), None);
        return Ok(Outcome::Skipped(why));
    }
    // One finished run per routine stays on the board: the ones before it are put away. A run
    // with an open pull request (review, landing) or a failure stays until someone deals with it.
    for old in runs
        .iter()
        .filter(|t| !t.resolved && Group::parse(&t.group_name) == Some(Group::Idle))
    {
        if let Err(e) = registry::set_resolved(ctx.conn, &old.name, true, RESOLVED_BY_ROUTINE, now)
        {
            eprintln!("chief: couldn't resolve {}: {e:#}", old.name);
        }
    }

    let args = StartThreadArgs {
        title: routine.title.clone(),
        task: brief(&routine),
        repo: routine.repo.clone(),
        model: routine.model.clone(),
        effort: routine.effort.clone(),
        account: None,
    };
    let started = match tools::start_thread(ctx, &args) {
        Ok(started) => started,
        Err(e) => {
            note(Some(format!("failed at {}: {e:#}", local_hhmm(now))), None);
            return Err(e.context(format!("starting a run of routine {name}")));
        }
    };
    let thread = started["name"].as_str().unwrap_or_default().to_string();
    let warning = started["warning"].as_str().map(str::to_string);
    // The row exists since before the spawn; its hooks may be writing it already.
    let marked = db::immediate(ctx.conn, |conn| {
        let Some(mut row) = db::get_session(conn, &thread)? else {
            return Ok(());
        };
        row.routine = Some(name.to_string());
        db::update_session(conn, &row)
    });
    if let Err(e) = marked {
        eprintln!("chief: {thread} started, but couldn't be marked as a run of {name}: {e:#}");
    }
    note(warning.clone(), Some(&thread));
    Ok(Outcome::Started { thread, warning })
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

/// The finished runs the coordinator should hear of and hasn't (`waiting::routine_due`), whose
/// report is at least `WAKE_AFTER_MS` old: each thread's name and that report's time.
pub fn unheard(conn: &Connection, slug: &str, now: i64) -> Result<Vec<(String, i64)>> {
    let threads = db::list_sessions(conn, slug, Some(Role::Thread))?;
    let quiet = quiet(conn, slug)?;
    Ok(crate::waiting::routine_due(&threads, &quiet)
        .into_iter()
        .filter_map(|t| Some((t.name.clone(), t.last_report_at?)))
        .filter(|(_, at)| now - at >= WAKE_AFTER_MS)
        .collect())
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
            "chief serve isn't running, so no routine fires on its schedule. The user starts \
             it with `chief serve start` (and `chief serve install` keeps it running after a \
             restart). run_routine still works."
        );
    }
}

pub fn list_tool(ctx: &ToolCtx) -> Result<Value> {
    let routines: Vec<Value> = list(ctx.conn, &ctx.config.slug)?.iter().map(view).collect();
    let mut out = json!({ "routines": routines });
    scheduler_note(ctx, &mut out);
    Ok(out)
}

pub fn create_tool(ctx: &ToolCtx, draft: &Draft) -> Result<Value> {
    let routine = create(ctx.conn, ctx.config, draft, (ctx.now)())?;
    let mut out = view(&routine);
    scheduler_note(ctx, &mut out);
    Ok(out)
}

pub fn update_tool(ctx: &ToolCtx, args: &UpdateRoutineArgs) -> Result<Value> {
    let routine = update(
        ctx.conn,
        ctx.config,
        args.name.trim(),
        &args.changes,
        (ctx.now)(),
    )?;
    let mut out = view(&routine);
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
    if let Outcome::Started { thread, .. } = &outcome {
        out["next"] = json!(format!(
            "{thread} is a thread like any other: list_threads shows it and read_report reads \
             its report. chief tells you when it finishes."
        ));
    }
    Ok(out)
}

// ---- `chief routine …` (§2) -------------------------------------------------------------------

/// The project's context for a command typed by the user: an unknown slug says so plainly.
fn for_cli<T>(slug: &str, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
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
    let mut head = format!("{}  \"{}\" · {schedule}", r.name, r.title);
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
    if let (Some(at), Some(thread)) = (r.last_run_at, &r.last_thread) {
        lines.push(format!("  last run {} · {thread}", local_stamp(at)));
    }
    if let Some(note) = &r.last_note {
        lines.push(format!("  {note}"));
    }
    lines.join("\n")
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

pub fn cli_list(slug: &str) -> Result<()> {
    for_cli(slug, |ctx| {
        let routines = list(ctx.conn, slug)?;
        if routines.is_empty() {
            println!(
                "{slug} has no routines. Ask the coordinator for one, or: chief routine add \
                 {slug} --title … --schedule \"0 9 * * 1-5\" --prompt …"
            );
            return Ok(());
        }
        for r in &routines {
            println!("{}", render(r));
        }
        cli_scheduler_note(ctx);
        Ok(())
    })
}

pub fn cli_add(slug: &str, draft: &Draft) -> Result<()> {
    for_cli(slug, |ctx| {
        let routine = create(ctx.conn, ctx.config, draft, (ctx.now)())?;
        println!("Added {}", render(&routine));
        cli_scheduler_note(ctx);
        Ok(())
    })
}

pub fn cli_update(slug: &str, name: &str, patch: &Patch) -> Result<()> {
    for_cli(slug, |ctx| {
        let routine = update(ctx.conn, ctx.config, name, patch, (ctx.now)())?;
        println!("{}", render(&routine));
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
        match run(ctx, name, Trigger::Manual)? {
            Outcome::Started { thread, warning } => {
                println!("Started {thread}.");
                if let Some(w) = warning {
                    eprintln!("warning: {w}");
                }
            }
            Outcome::Skipped(why) => println!("Nothing started: {why}."),
        }
        Ok(())
    })
}
