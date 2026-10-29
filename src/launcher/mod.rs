//! The launcher (ARCHITECTURE §10a): `chief` with no command. It lists the projects, walks a new
//! one through a four-step wizard, and opens a project's layout (§9) while it shows each step.

pub mod app;
pub mod ui;
pub mod wizard;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event};

use crate::claude::{self, Claude};
use crate::config::ProjectConfig;
use crate::pane::OnQuit;
use crate::project::{self, Note, Where};
use crate::{db, generated, paths, trust, tui};

use app::{Action, Entry, Launcher, Msg};
use wizard::{Draft, Probe};

const POLL: Duration = Duration::from_millis(100);

/// The projects of this chief home, from their files and stored rows.
pub fn entries() -> Result<Vec<Entry>> {
    let conn = db::open(&paths::db())?;
    let mut out = Vec::new();
    for (slug, dir) in db::list_projects(&conn)? {
        let config = ProjectConfig::load(&Path::new(&dir).join("project.toml"))
            .map_err(|e| format!("{e:#}"));
        let rows = db::list_sessions(&conn, &slug, None).unwrap_or_default();
        out.push(Entry::new(&slug, config, &rows));
    }
    Ok(out)
}

/// What the wizard checks as it goes: free slugs, and folder trust in the account a new project
/// gets (this process's `CLAUDE_CONFIG_DIR`, as for `chief init`).
struct Live {
    trust: PathBuf,
}

impl Probe for Live {
    fn slug_problem(&self, slug: &str) -> Option<String> {
        let taken = db::open(&paths::db()).and_then(|conn| project::slug_taken(&conn, slug));
        match taken {
            Ok(false) => None,
            Ok(true) => Some(format!(
                "A project called {slug} exists already: pick another name."
            )),
            Err(e) => Some(format!("chief can't check the name: {e:#}")),
        }
    }

    fn trusted(&self, folder: &Path) -> bool {
        claude::is_trusted_in(&self.trust, folder).unwrap_or(false)
    }
}

/// `chief` (`new`: `chief new`, straight to the wizard).
pub fn run(new: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("chief needs a terminal; `chief --help` lists the commands for scripts");
    }
    let probe = Live {
        trust: Claude::from_env().trust_config(),
    };
    let cwd = std::env::current_dir().ok();
    let mut app = Launcher::new(entries()?, cwd, new);
    let (tx, rx) = mpsc::channel();

    let mut terminal = tui::init();
    let exit = (|| -> Result<Option<(String, OnQuit)>> {
        loop {
            terminal.draw(|f| ui::draw(f, &app))?;
            if event::poll(POLL)? {
                let actions = match event::read()? {
                    Event::Key(key) => app.on_key(key, &probe),
                    Event::Mouse(mouse) => app.on_mouse(mouse),
                    _ => Vec::new(),
                };
                for action in actions {
                    match action {
                        Action::Quit => return Ok(None),
                        Action::Pane(slug) => return Ok(Some((slug, OnQuit::Nothing))),
                        Action::Open(slug) => spawn(tx.clone(), move |send| open(&slug, send)),
                        Action::Create(draft) => {
                            spawn(tx.clone(), move |send| create(&draft, send))
                        }
                    }
                }
            }
            let mut finished = None;
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Created(slug) = &msg {
                    finished = Some(slug.clone());
                }
                let done = matches!(msg, Msg::Done(_));
                // In a Rex block the board is beside it: this terminal becomes the conversation.
                let here = match &msg {
                    Msg::Done(Ok(Where::Here { board })) => Some(board.clone()),
                    _ => None,
                };
                app.apply(msg);
                if let (Some(board), app::Screen::Starting(s)) = (here, &app.screen) {
                    return Ok(Some((s.slug.clone(), OnQuit::Board(board))));
                }
                if done && let app::Screen::Starting(s) = &app.screen {
                    finished = Some(s.slug.clone());
                }
            }
            // A project was made or opened: refresh the list behind the screen.
            if let Some(slug) = finished
                && let Ok(projects) = entries()
            {
                app.set_projects(projects, Some(&slug));
            }
        }
    })();
    tui::restore();
    match exit? {
        Some((slug, OnQuit::Nothing)) => {
            println!(
                "Showing the conversation with the coordinator. For its threads, run in another \
                 terminal: chief board --project {slug}"
            );
            crate::pane::run(&slug, OnQuit::Nothing)
        }
        Some((slug, on_quit)) => crate::pane::run(&slug, on_quit),
        None => Ok(()),
    }
}

/// Runs `job` on a worker; its notes and its result come back as messages.
fn spawn(
    tx: mpsc::Sender<Msg>,
    job: impl FnOnce(&mut dyn FnMut(Msg)) -> Result<Where> + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut send = |msg: Msg| {
            let _ = tx.send(msg);
        };
        let result = job(&mut send).map_err(|e| format!("{e:#}"));
        send(Msg::Done(result));
    });
}

fn open(slug: &str, send: &mut dyn FnMut(Msg)) -> Result<Where> {
    project::up_with(slug, &mut |note| send(Msg::Note(note)))
}

/// The wizard's draft: `chief init` with the quick settings, trust as agreed, then `chief up`.
pub fn create(draft: &Draft, send: &mut dyn FnMut(Msg)) -> Result<Where> {
    let claude = Claude::from_env();
    claude.check_version()?;
    let exe = generated::chief_exe()?;
    let account = claude.account.clone();
    let env = generated::passthrough_env(account.as_deref());
    let (config, dir) = project::create_with(
        &draft.name,
        draft.goal.as_deref(),
        &draft.repos,
        account,
        &exe,
        &env,
        |c| {
            c.coordinator.model = draft.coordinator_model.clone();
            c.coordinator.effort = draft.coordinator_effort.clone();
            c.threads.model = draft.thread_model.clone();
            c.threads.effort = draft.thread_effort.clone();
        },
    )?;
    send(Msg::Created(config.slug.clone()));
    send(Msg::Note(Note::Did(format!(
        "Created project {} ({}).",
        config.name, config.slug
    ))));

    let check = trust::check(&config, &dir, &claude)?;
    for e in &check.errors {
        send(Msg::Note(Note::Warn(format!("chief couldn't check {e}"))));
    }
    // chief made the project folder; the repos only when the user agreed (§3 "Trust").
    let mut needs: Vec<&trust::Need> = check.project.iter().collect();
    if draft.trust {
        needs.extend(&check.repos);
    } else if !check.repos.is_empty() {
        send(Msg::Note(Note::Warn(format!(
            "{} folders aren't trusted, so threads there won't start: chief trust {}",
            check.repos.len(),
            config.slug
        ))));
    }
    for line in trust::mark_all(needs) {
        send(Msg::Note(match line {
            Ok(l) => Note::Did(l),
            Err(l) => Note::Warn(l),
        }));
    }
    open(&config.slug, send)
}
