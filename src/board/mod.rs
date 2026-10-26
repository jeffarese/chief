//! The board TUI (ARCHITECTURE §10). `app` holds the state and turns keys into actions, `ui`
//! draws it, `refresh` polls in the background, and `jobs` runs actions off the UI thread.

pub mod app;
pub mod jobs;
pub mod refresh;
pub mod settings;
pub mod ui;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::config::ProjectConfig;
use crate::text::shell_quote;
use crate::{db, paths};

use app::{Action, App, Msg};
use refresh::Sources;

/// How long a frame waits for a key before redrawing and reading worker results.
const POLL: Duration = Duration::from_millis(100);

pub fn run(project: &str) -> Result<()> {
    let dir = crate::project::project_dir(project);
    let config = ProjectConfig::load(&dir.join("project.toml"))
        .with_context(|| format!("no usable chief project {project:?}"))?;
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, project)?.is_none() {
        bail!("no chief project {project:?}");
    }
    drop(conn);

    let src = Sources {
        slug: project.to_string(),
        dir: dir.clone(),
        db: paths::db(),
        claude_bin: crate::claude::Claude::from_env().bin,
        claude_dir: crate::claude::claude_dir(),
        gh_bin: crate::pr::Gh::from_env().bin,
    };
    let (tx, rx) = mpsc::channel();
    let refresher = refresh::spawn(src.clone(), tx.clone());
    let mut app = App::new(project, dir, config, db::now_ms());

    // `init` enters the alternate screen and raw mode and installs a panic hook that restores
    // the terminal before the panic message prints.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &rx, |action| {
        jobs::spawn(src.clone(), action, tx.clone(), refresher.kicker())
    });
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<Msg>,
    spawn: impl Fn(Action),
) -> Result<()> {
    loop {
        app.now = db::now_ms();
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(POLL)? {
            // Resize needs nothing: the next draw uses the new size.
            if let Event::Key(key) = event::read()? {
                for action in app.on_key(key) {
                    match action {
                        Action::Quit => return Ok(()),
                        Action::Edit(path) => {
                            let msg = edit(terminal, &path);
                            app.status = Some(msg);
                        }
                        other => spawn(other),
                    }
                }
            }
        }
        while let Ok(msg) = rx.try_recv() {
            app.apply(msg);
        }
    }
}

/// Hands the terminal to `$EDITOR` (else `vi`) and takes it back. Returns a status line.
fn edit(terminal: &mut DefaultTerminal, path: &Path) -> String {
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    // Through sh so an EDITOR with arguments (`code -w`) works.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} {}", shell_quote(&path.to_string_lossy())))
        .status();
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = enable_raw_mode();
    let _ = terminal.clear();
    match status {
        Ok(s) if s.success() => format!("Edited {}.", file_label(path)),
        Ok(s) => format!("{editor} exited with {s}."),
        Err(e) => format!("Could not run {editor}: {e}"),
    }
}

fn file_label(path: &Path) -> String {
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .map(PathBuf::from)
        .unwrap_or_default();
    match path.file_name() {
        Some(name) if parent == Path::new("memory") => format!("memory/{}", name.to_string_lossy()),
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}
