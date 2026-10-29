//! The board TUI (ARCHITECTURE §10). `app` holds the state and turns keys into actions, `ui`
//! draws it, `refresh` polls in the background, and `jobs` runs actions off the UI thread.

pub mod app;
pub mod jobs;
pub mod refresh;
pub mod settings;
pub mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use crate::config::ProjectConfig;
use crate::text::shell_quote;
use crate::{db, pane, paths, tui};

use app::{Action, App, Msg};
use refresh::Sources;

/// How long a frame waits for a key before redrawing and reading worker results.
const POLL: Duration = Duration::from_millis(100);
/// How long Ctrl+Q waits for the pane to quit, which closes the layout, board included.
const PANE_QUIT_WAIT: Duration = Duration::from_secs(3);

/// The board with its workers: what `chief board` runs, and the right side of the built-in
/// layout (§9).
pub struct Board {
    pub app: App,
    src: Sources,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    shows: mpsc::Sender<Option<String>>,
    refresher: refresh::Refresher,
}

impl Board {
    /// Reads the project and starts the refresher and the Show worker.
    pub fn open(project: &str) -> Result<Board> {
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
            // The project's account, whatever the block's shell names (§3).
            claude_account: config.claude_config_dir.clone(),
            claude_dir: crate::claude::claude_dir(config.claude_config_dir.as_deref()),
            gh_bin: crate::pr::Gh::from_env().bin,
            agp_bin: crate::agp::Agp::from_env().bin,
        };
        let (tx, rx) = mpsc::channel();
        let refresher = refresh::spawn(src.clone(), tx.clone());
        let shows = jobs::spawn_shows(src.clone(), tx.clone(), refresher.kicker());
        Ok(Board {
            app: App::new(project, dir, config, db::now_ms()),
            src,
            tx,
            rx,
            shows,
            refresher,
        })
    }

    /// Asks the pane to quit and waits for it (`pane::request_quit`). The board quits either way.
    fn quit_pane(&self) {
        if let Ok(conn) = db::open(&self.src.db) {
            let _ = pane::request_quit(&conn, &self.src.slug, PANE_QUIT_WAIT);
        }
    }

    /// Runs an action off the UI thread. `Quit`, `QuitAll` and `Edit` need the terminal: the
    /// caller's.
    pub fn dispatch(&self, action: Action) {
        match action {
            Action::Show(target) => {
                let _ = self.shows.send(target);
            }
            other => jobs::spawn(
                self.src.clone(),
                other,
                self.tx.clone(),
                self.refresher.kicker(),
            ),
        }
    }

    /// Applies what the workers sent since the last call.
    pub fn take_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.app.apply(msg);
        }
    }
}

pub fn run(project: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("chief board needs a terminal");
    }
    let mut board = Board::open(project)?;
    let mut terminal = tui::init()?;
    let result = event_loop(&mut terminal, &mut board);
    tui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, board: &mut Board) -> Result<()> {
    loop {
        board.app.now = db::now_ms();
        terminal.draw(|f| ui::draw(f, &board.app))?;
        if event::poll(POLL)? {
            // Resize needs nothing: the next draw uses the new size.
            let actions = match event::read()? {
                Event::Key(key) => board.app.on_key(key),
                Event::Mouse(mouse) => board.app.on_mouse(mouse),
                _ => Vec::new(),
            };
            for action in actions {
                match action {
                    Action::Quit => return Ok(()),
                    Action::QuitAll => {
                        board.quit_pane();
                        return Ok(());
                    }
                    Action::Edit(path) => {
                        let msg = edit(terminal, &path);
                        board.app.say(msg);
                    }
                    other => board.dispatch(other),
                }
            }
        }
        board.take_messages();
    }
}

/// Hands the terminal to `$EDITOR` (else `vi`) and takes it back. Returns a status line.
pub fn edit(terminal: &mut DefaultTerminal, path: &Path) -> String {
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Through sh so an EDITOR with arguments (`code -w`) works.
    let status = tui::suspended(terminal, || {
        Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} {}", shell_quote(&path.to_string_lossy())))
            .status()
    });
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
