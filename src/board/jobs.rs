//! Runs the board's actions off the UI thread: each opens its own connection, sends one `Msg`
//! back, then asks the refresher to re-read the rows.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::agp::Agp;
use crate::claude::{Claude, RESOLVE_TIMEOUT};
use crate::cleanup::{self, Ctx, Inspection, Mode, Outcome, Verdict};
use crate::config::ProjectConfig;
use crate::pr::Gh;
use crate::routines::{self, Author, Patch, Trigger};
use crate::tools::{self, NameArgs, ToolCtx};
use crate::{db, generated, pane, placement, registry, trust, tui};

use super::app::{Action, Msg};
use super::refresh::Sources;
use super::report::REPORTS_READ;

const WAKE_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn spawn(src: Sources, action: Action, tx: Sender<Msg>, kick: Sender<()>) {
    std::thread::spawn(move || {
        let _ = tx.send(run(&src, action));
        let _ = kick.send(());
    });
}

/// One worker for every Show, run in order: on separate threads two quick picks race, and the
/// main pane could end on the older one. Picks that queued up while one ran are skipped for the
/// newest. The worker ends when the returned sender is dropped.
pub fn spawn_shows(src: Sources, tx: Sender<Msg>, kick: Sender<()>) -> Sender<Option<String>> {
    let (shows, picks) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        while let Ok(mut target) = picks.recv() {
            while let Ok(newer) = picks.try_recv() {
                target = newer;
            }
            if tx.send(run(&src, Action::Show(target))).is_err() {
                return;
            }
            let _ = kick.send(());
        }
    });
    shows
}

/// Runs one action and says how it went.
pub fn run(src: &Sources, action: Action) -> Msg {
    let failure = Failure::of(&action);
    outcome(failure, || try_run(src, action))
}

/// How an action reports that it failed, so the screen waiting on it hears back.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    kind: FailureKind,
    /// The action, for a crash: `Waking demo-x`.
    what: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FailureKind {
    /// In the settings form's message.
    Save,
    /// In the settings form's trust check.
    Trust,
    /// Ends `busy` (wake, cleanup).
    Busy,
    Notice,
}

impl Failure {
    pub fn of(action: &Action) -> Self {
        let (kind, what) = match action {
            Action::Wake(name) => (FailureKind::Busy, format!("Waking {name}")),
            Action::Stop(name) => (FailureKind::Busy, format!("Stopping {name}")),
            Action::CleanAll => (FailureKind::Busy, "Cleaning up".to_string()),
            Action::Clean { name, .. } => (FailureKind::Busy, format!("Cleaning up {name}")),
            Action::SaveSettings(_) => (FailureKind::Save, "Saving".to_string()),
            Action::CheckTrust(_) => (FailureKind::Trust, "The trust check".to_string()),
            Action::Trust(..) => (FailureKind::Trust, "Trusting the folder".to_string()),
            Action::Show(_) => (FailureKind::Notice, "Switching the main pane".to_string()),
            Action::SetResolved { name, .. } => (FailureKind::Notice, format!("Resolving {name}")),
            Action::OpenUrl(_) => (FailureKind::Notice, "Opening the PR".to_string()),
            Action::Reports(name) => (FailureKind::Notice, format!("Reading {name}'s reports")),
            Action::RunRoutine(name) => (FailureKind::Busy, format!("Running {name}")),
            Action::Approve { name, .. } => (FailureKind::Notice, format!("Approving {name}")),
            Action::SetEnabled { name, enabled } => (
                FailureKind::Notice,
                format!("{} {name}", if *enabled { "Resuming" } else { "Pausing" }),
            ),
            Action::Quit | Action::QuitAll | Action::Edit(_) => {
                (FailureKind::Notice, "The action".to_string())
            }
        };
        Failure { kind, what }
    }

    fn msg(&self, text: String) -> Msg {
        match self.kind {
            FailureKind::Save => Msg::SaveFailed(format!("Not saved: {text}")),
            FailureKind::Trust => Msg::Trust(Err(text)),
            FailureKind::Busy => Msg::Done(text),
            FailureKind::Notice => Msg::Notice(text),
        }
    }
}

/// `job`'s message; its error, or its panic, as `failure` says. A worker that panicked would
/// otherwise send nothing, and the board would show "waking …" for good.
pub fn outcome(failure: Failure, job: impl FnOnce() -> Result<Msg>) -> Msg {
    match tui::catch(job) {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => failure.msg(format!("{e:#}")),
        Err(panic) => failure.msg(format!("{} crashed: {panic}", failure.what)),
    }
}

fn try_run(src: &Sources, action: Action) -> Result<Msg> {
    match action {
        Action::Show(target) => {
            let conn = db::open(&src.db)?;
            db::set_pane_target(&conn, &src.slug, target.as_deref())?;
            pane::request_switch(&conn, &src.slug)?;
            Ok(Msg::Notice(match target {
                Some(name) => format!("Main pane: {name}"),
                None => "Main pane: coordinator".to_string(),
            }))
        }
        Action::Reports(name) => {
            let conn = db::open(&src.db)?;
            let reports = db::recent_reports(&conn, &name, REPORTS_READ)?;
            Ok(Msg::Reports { name, reports })
        }
        Action::Wake(name) => wake(src, &name),
        Action::Stop(name) => stop(src, &name),
        Action::CleanAll => clean_all(src),
        Action::SetResolved { name, resolved } => {
            let conn = db::open(&src.db)?;
            registry::set_resolved(&conn, &name, resolved, "user", db::now_ms())?;
            Ok(Msg::Notice(if resolved {
                format!("Resolved {name}.")
            } else {
                format!("Reopened {name}.")
            }))
        }
        Action::OpenUrl(url) => {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            // Quiet: anything it prints would land on the board's screen.
            let status = Command::new(opener)
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| format!("running {opener}"))?;
            anyhow::ensure!(status.success(), "{opener} {url} failed ({status})");
            Ok(Msg::Notice(format!("Opened {url}")))
        }
        Action::Clean { name, mode } => clean(src, &name, mode),
        Action::SaveSettings(config) => {
            config.save(&src.dir.join("project.toml"))?;
            // project.toml is written by now: a failure here is a warning, not "Not saved".
            let warning = generated::rewrite(&src.dir, &config)
                .err()
                .map(|e| format!("{e:#}"));
            Ok(Msg::Saved { config, warning })
        }
        Action::CheckTrust(config) => Ok(Msg::Trust(Ok(pool_trust(src, &config)?))),
        Action::Trust(pair, config) => {
            let need = trust::Need::from(pair.as_ref());
            let text = match need.mark() {
                Ok(()) => format!("Trusted {}.", need.shown()),
                Err(e) => format!("Couldn't trust {}: {e:#}", need.shown()),
            };
            let check = pool_trust(src, &config).map_err(|e| format!("{e:#}"));
            Ok(Msg::Trusted { text, check })
        }
        Action::RunRoutine(name) => run_routine(src, &name),
        Action::Approve { name, command } => approve(src, &name, &command),
        Action::SetEnabled { name, enabled } => set_enabled(src, &name, enabled),
        // The runner handles these itself: they need the terminal.
        Action::Quit | Action::QuitAll | Action::Edit(_) => Ok(Msg::Notice(String::new())),
    }
}

fn pool_trust(src: &Sources, config: &ProjectConfig) -> Result<placement::PoolTrust> {
    let repos: Vec<_> = config.repos.iter().map(|r| r.path.clone()).collect();
    placement::pool_trust(&config.threads, &Agp::new(src.agp_bin.clone()), &repos)
}

/// Runs a coordinator tool on `name` with the context `chief mcp` would give it.
fn thread_tool(
    src: &Sources,
    name: &str,
    tool: fn(&ToolCtx, &NameArgs) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    with_ctx(src, |ctx| {
        tool(
            ctx,
            &NameArgs {
                name: name.to_string(),
            },
        )
    })
}

/// The context `chief mcp` would give a tool, from the board's sources.
fn with_ctx<T>(src: &Sources, f: impl FnOnce(&ToolCtx) -> Result<T>) -> Result<T> {
    let conn = db::open(&src.db)?;
    let config = ProjectConfig::load(&src.dir.join("project.toml"))?;
    let claude = src.claude();
    let trust_config = claude.trust_config();
    // Waking and stopping never pick an account; only start_thread reads agp.
    let agp = Agp::new(src.agp_bin.clone());
    let ctx = ToolCtx {
        conn: &conn,
        config: &config,
        claude: &claude,
        agp: &agp,
        project_dir: &src.dir,
        trust_config: &trust_config,
        now: db::now_ms,
        resolve_timeout: RESOLVE_TIMEOUT,
        wake_timeout: WAKE_TIMEOUT,
        poll_interval: POLL_INTERVAL,
    };
    f(&ctx)
}

/// Runs a routine now, as `chief routine run` and the dashboard do, without waiting for its
/// command: that runs detached and the refresh shows how it ends.
fn run_routine(src: &Sources, name: &str) -> Result<Msg> {
    with_ctx(src, |ctx| {
        // The rows as they are now decide whether its last run still works.
        let _ = tools::refresh(ctx);
        Ok(Msg::Done(
            match routines::run(ctx, name, Trigger::Manual)? {
                routines::Outcome::Started { thread, warning } => match warning {
                    Some(w) => format!("Started {thread}. Warning: {w}"),
                    None => format!("Started {thread}."),
                },
                routines::Outcome::Running { .. } => {
                    format!("{name}: its command runs; the detail shows how it ends.")
                }
                routines::Outcome::Ran(done) => format!("{name}: {}.", done.run.line()),
                routines::Outcome::Skipped(why) => format!("Nothing started: {why}."),
            },
        ))
    })
}

/// Approves the command only while it reads as the modal showed it: the coordinator may have
/// changed it while the user read.
fn approve(src: &Sources, name: &str, shown: &str) -> Result<Msg> {
    let conn = db::open(&src.db)?;
    let routine = routines::get(&conn, &src.slug, name)?
        .with_context(|| format!("routine {name} is gone"))?;
    if routine.command.as_deref() != Some(shown) {
        return Ok(Msg::Notice(format!(
            "Not approved: {name}'s command changed since it was shown. a shows it again."
        )));
    }
    // The words go along, so a change between this look and the approval is refused too.
    routines::approve(&conn, &src.slug, name, Some(shown), db::now_ms())?;
    Ok(Msg::Notice(format!(
        "Approved {name}'s command: it runs on its schedule now."
    )))
}

fn set_enabled(src: &Sources, name: &str, enabled: bool) -> Result<Msg> {
    let conn = db::open(&src.db)?;
    let config = ProjectConfig::load(&src.dir.join("project.toml"))?;
    let patch = Patch {
        enabled: Some(enabled),
        ..Default::default()
    };
    let routine = routines::update(&conn, &config, name, &patch, Author::User, db::now_ms())?;
    Ok(Msg::Notice(match (enabled, routine.next_run_at) {
        (false, _) => format!("Paused {name}: nothing fires on schedule until p resumes it."),
        (true, Some(at)) => format!("Resumed {name}: next run {}.", crate::text::local_stamp(at)),
        (true, None) => format!("Resumed {name}, but its schedule never comes due."),
    }))
}

fn wake(src: &Sources, name: &str) -> Result<Msg> {
    let out = thread_tool(src, name, tools::wake_thread)?;
    let alive = out["alive"].as_bool().unwrap_or(false);
    let respawned = out["respawned"].as_bool().unwrap_or(false);
    Ok(Msg::Done(match (alive, respawned) {
        (true, false) => format!("{name} is already awake."),
        (true, true) => format!("Woke {name}."),
        _ => out["note"]
            .as_str()
            .unwrap_or("Respawned, but no process showed up yet.")
            .to_string(),
    }))
}

fn stop(src: &Sources, name: &str) -> Result<Msg> {
    let out = thread_tool(src, name, tools::stop_thread)?;
    Ok(Msg::Done(if out["stopped"].as_bool().unwrap_or(false) {
        format!("Stopped {name}; w wakes it.")
    } else {
        format!("{name} is asleep already.")
    }))
}

/// `chief gc` for this project, said in one line.
fn clean_all(src: &Sources) -> Result<Msg> {
    let conn = db::open(&src.db)?;
    let claude = src.claude();
    let gh = Gh::new(src.gh_bin.clone());
    let ctx = Ctx {
        conn: &conn,
        claude: &claude,
        gh: &gh,
        now: db::now_ms,
    };
    let gc = cleanup::gc(&ctx, &src.slug, false)?;
    Ok(Msg::Done(gc_summary(&gc)))
}

fn gc_summary(gc: &cleanup::Gc) -> String {
    let count = |f: fn(&Verdict) -> bool| gc.verdicts.iter().filter(|(_, v)| f(v)).count();
    let cleaned = count(|v| matches!(v, Verdict::Cleaned));
    let kept = count(|v| matches!(v, Verdict::Blocked(_) | Verdict::Skipped(_)));
    let threads = |n: usize| format!("{n} thread{}", if n == 1 { "" } else { "s" });
    let mut text = match (cleaned, kept) {
        (0, 0) => "No resolved thread was left to clean up.".to_string(),
        (n, 0) => format!("Cleaned up {}.", threads(n)),
        (n, k) => format!(
            "Cleaned up {}; {} would lose work or couldn't be checked (x on one says why).",
            threads(n),
            threads(k)
        ),
    };
    if let Some(first) = gc.warnings.first() {
        text.push_str(&format!(" Warning: {first}"));
    }
    text
}

fn clean(src: &Sources, name: &str, mode: Mode) -> Result<Msg> {
    let conn = db::open(&src.db)?;
    let claude = src.claude();
    let gh = Gh::new(src.gh_bin.clone());
    let ctx = Ctx {
        conn: &conn,
        claude: &claude,
        gh: &gh,
        now: db::now_ms,
    };
    let discarding = matches!(mode, Mode::Discard(_));
    Ok(match (cleanup::clean(&ctx, name, mode)?, discarding) {
        (Outcome::Cleaned, _) => Msg::Done(format!("Cleaned up {name}.")),
        (Outcome::AlreadyCleaned, _) => Msg::Done(format!("{name} is already cleaned up.")),
        (Outcome::InProgress, _) => Msg::Done(format!("{name}: {}.", cleanup::IN_PROGRESS)),
        (Outcome::Blocked(why), false) => {
            let (loss, confirmed) = loss(&conn, &claude, name, &why)?;
            Msg::CleanBlocked {
                name: name.to_string(),
                loss,
                confirmed,
            }
        }
        (Outcome::Blocked(why), true) => {
            Msg::Done(format!("{name} is still not cleaned up: {why}"))
        }
    })
}

/// What the modal shows after a safe cleanup stopped, and the inspection a discard would act on.
/// A discard is offered only for a loss chief lists itself; when `claude rm` refused over work
/// chief can't see, or the worktree couldn't be read, the modal says why and offers none.
fn loss(
    conn: &Connection,
    claude: &Claude,
    name: &str,
    why: &str,
) -> Result<(String, Option<Inspection>)> {
    let row = db::get_session(conn, name)?.with_context(|| format!("no thread named {name}"))?;
    if why.contains(cleanup::SHARED_WORKTREE) {
        return Ok((
            format!(
                "Cleanup blocked: {why}.\nAnother thread works in that worktree: finish and \
                 clean up that thread first.\n"
            ),
            None,
        ));
    }
    Ok(match cleanup::inspect(claude, &row) {
        Ok(ins) if ins.blocker().is_some() => (
            format!(
                "Cleanup stopped: {why}.\n{}",
                cleanup::render_loss(name, &ins)
            ),
            Some(ins),
        ),
        Ok(ins) => {
            let seen = match &ins.worktree {
                Some(wt) => format!("chief sees nothing unsaved in {}", wt.display()),
                None => "chief knows no worktree of this thread".to_string(),
            };
            (
                format!(
                    "Cleanup blocked: {why}.\n{seen}, so it can't show what a discard would \
                     lose and offers none. Push or commit the work, or remove the session \
                     yourself with claude rm.\n"
                ),
                None,
            )
        }
        Err(e) => (
            format!(
                "Cleanup blocked: {why}.\nCould not inspect the worktree: {e:#}\nchief offers \
                 no discard for work it can't see.\n"
            ),
            None,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Box<ProjectConfig> {
        Box::new(ProjectConfig::new("Demo", "demo", None, Vec::new()))
    }

    #[test]
    fn a_clean_up_of_all_says_what_stayed() {
        let gc = |verdicts: Vec<Verdict>, warnings: Vec<&str>| cleanup::Gc {
            verdicts: verdicts
                .into_iter()
                .enumerate()
                .map(|(i, v)| (format!("t{i}"), v))
                .collect(),
            warnings: warnings.into_iter().map(String::from).collect(),
        };
        assert_eq!(
            gc_summary(&gc(vec![], vec![])),
            "No resolved thread was left to clean up."
        );
        assert_eq!(
            gc_summary(&gc(vec![Verdict::Cleaned, Verdict::Cleaned], vec![])),
            "Cleaned up 2 threads."
        );
        assert_eq!(
            gc_summary(&gc(
                vec![Verdict::Cleaned, Verdict::Blocked("unpushed".into())],
                vec!["gh isn't signed in"]
            )),
            "Cleaned up 1 thread; 1 thread would lose work or couldn't be checked (x on one says \
             why). Warning: gh isn't signed in"
        );
    }

    /// A worker that panics still answers, in the message its action reports failures with: a
    /// wake's Done ends "waking …", a save's failure shows in the form.
    #[test]
    fn a_panicking_job_still_reports() {
        let wake = Failure::of(&Action::Wake("demo-x".into()));
        let msg = outcome(wake, || panic!("boom"));
        assert_eq!(msg, Msg::Done("Waking demo-x crashed: boom".into()));

        let save = Failure::of(&Action::SaveSettings(config()));
        assert_eq!(
            outcome(save, || panic!("disk")),
            Msg::SaveFailed("Not saved: Saving crashed: disk".into())
        );
        let check = Failure::of(&Action::CheckTrust(config()));
        assert_eq!(
            outcome(check.clone(), || anyhow::bail!("agp failed")),
            Msg::Trust(Err("agp failed".into()))
        );
        assert_eq!(
            outcome(check, || Ok(Msg::Notice("fine".into()))),
            Msg::Notice("fine".into())
        );
    }
}
