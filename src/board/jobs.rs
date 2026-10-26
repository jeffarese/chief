//! Runs the board's actions off the UI thread: each opens its own connection, sends one `Msg`
//! back, then asks the refresher to re-read the rows.

use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::claude::{self, Claude, RESOLVE_TIMEOUT};
use crate::cleanup::{self, Ctx, Mode, Outcome};
use crate::config::ProjectConfig;
use crate::mux::rex::Rex;
use crate::pr::Gh;
use crate::tools::{self, NameArgs, ToolCtx};
use crate::{db, generated, pane, registry};

use super::app::{Action, Msg};
use super::refresh::Sources;

const WAKE_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn spawn(src: Sources, action: Action, tx: Sender<Msg>, kick: Sender<()>) {
    std::thread::spawn(move || {
        let _ = tx.send(run(&src, action));
        let _ = kick.send(());
    });
}

/// Runs one action and says how it went.
pub fn run(src: &Sources, action: Action) -> Msg {
    let busy = matches!(action, Action::Wake(_) | Action::Clean { .. });
    let save = matches!(action, Action::SaveSettings(_));
    match try_run(src, action) {
        Ok(msg) => msg,
        Err(e) if save => Msg::SaveFailed(format!("Not saved: {e:#}")),
        Err(e) if busy => Msg::Done(format!("{e:#}")),
        Err(e) => Msg::Notice(format!("{e:#}")),
    }
}

fn try_run(src: &Sources, action: Action) -> Result<Msg> {
    match action {
        Action::Show(target) => {
            let conn = db::open(&src.db)?;
            db::set_pane_target(&conn, &src.slug, target.as_deref())?;
            let rex = Rex {
                bin: src.rex_bin.clone(),
            };
            pane::request_switch(&rex, &conn, &src.slug)?;
            Ok(Msg::Notice(match target {
                Some(name) => format!("Main pane: {name}"),
                None => "Main pane: coordinator".to_string(),
            }))
        }
        Action::Wake(name) => wake(src, &name),
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
            generated::write_all(
                &src.dir,
                &config.slug,
                &generated::chief_exe()?,
                &generated::passthrough_env(),
            )?;
            Ok(Msg::Saved(config))
        }
        // The runner handles these itself.
        Action::Quit | Action::Edit(_) => Ok(Msg::Notice(String::new())),
    }
}

fn wake(src: &Sources, name: &str) -> Result<Msg> {
    let conn = db::open(&src.db)?;
    let config = ProjectConfig::load(&src.dir.join("project.toml"))?;
    let claude = src.claude();
    let trust_config = claude::claude_config_path();
    let ctx = ToolCtx {
        conn: &conn,
        config: &config,
        claude: &claude,
        project_dir: &src.dir,
        trust_config: &trust_config,
        now: db::now_ms,
        resolve_timeout: RESOLVE_TIMEOUT,
        wake_timeout: WAKE_TIMEOUT,
        poll_interval: POLL_INTERVAL,
    };
    let out = tools::wake_thread(
        &ctx,
        &NameArgs {
            name: name.to_string(),
        },
    )?;
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
    Ok(match (cleanup::clean(&ctx, name, mode)?, mode) {
        (Outcome::Cleaned, _) => Msg::Done(format!("Cleaned up {name}.")),
        (Outcome::AlreadyCleaned, _) => Msg::Done(format!("{name} is already cleaned up.")),
        (Outcome::Blocked(why), Mode::Safe) => Msg::CleanBlocked {
            name: name.to_string(),
            loss: loss(&conn, &claude, name, &why)?,
        },
        (Outcome::Blocked(why), Mode::Discard) => {
            Msg::Done(format!("{name} is still not cleaned up: {why}"))
        }
    })
}

/// What the confirmation shows: why the safe cleanup stopped and what discarding would lose.
fn loss(conn: &Connection, claude: &Claude, name: &str, why: &str) -> Result<String> {
    let row = db::get_session(conn, name)?.with_context(|| format!("no thread named {name}"))?;
    let detail = match cleanup::inspect(claude, &row) {
        Ok(ins) => cleanup::render_loss(name, &ins),
        Err(e) => format!("Could not inspect the worktree: {e:#}\n"),
    };
    Ok(format!("Cleanup stopped: {why}.\n{detail}"))
}
