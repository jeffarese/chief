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
use crate::project::{self, LayoutChoice, Note, Where};
use crate::{db, generated, paths, placement, trust, tui};

use app::{Action, Entry, Launcher, Msg};
use wizard::{Draft, PoolAccount, Probe};

const POLL: Duration = Duration::from_millis(100);

/// The projects of this chief home, from their files and stored rows.
pub fn entries() -> Result<Vec<Entry>> {
    let conn = db::open(&paths::db())?;
    let mut out = Vec::new();
    for (slug, dir) in db::list_projects(&conn)? {
        let config = ProjectConfig::load(&Path::new(&dir).join("project.toml"))
            .map_err(|e| format!("{e:#}"));
        let rows = db::list_sessions(&conn, &slug, None).unwrap_or_default();
        let mut entry = Entry::new(&slug, config, &rows);
        entry.archived = db::archived_at(&conn, &slug)?.is_some();
        out.push(entry);
    }
    Ok(out)
}

/// `a` on an archived project: back in the list, nothing started.
fn unarchive(slug: &str, name: &str) -> Result<String, String> {
    let conn = db::open(&paths::db()).map_err(|e| format!("{e:#}"))?;
    match db::unarchive_project(&conn, slug) {
        Ok(_) => Ok(format!("Unarchived {name}. ⏎ opens it.")),
        Err(e) => Err(format!("{name} isn't unarchived: {e:#}")),
    }
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

    fn trusted(&self, account: Option<&Path>, folder: &Path) -> bool {
        claude::is_trusted_in(account.unwrap_or(&self.trust), folder).unwrap_or(false)
    }
}

/// What the wizard asks about agp (§10a): the pool accounts a new project's threads would run
/// in under the `[threads]` defaults, without the account the project itself gets (this
/// process's). None when agp isn't installed or doesn't answer: the wizard then asks nothing,
/// and creating the project says what failed (`trust::check`).
fn agp_pool() -> Option<Vec<PoolAccount>> {
    let threads = ProjectConfig::new("", "", None, Vec::new()).threads;
    let (accounts, _) = placement::pool_accounts(&threads, &crate::agp::Agp::from_env()).ok()??;
    let project = Claude::from_env().account;
    Some(
        accounts
            .into_iter()
            .filter(|a| !claude::same_account(a.dir.as_deref(), project.as_deref()))
            .map(|a| PoolAccount {
                label: a.label,
                trust: claude::claude_config_path(a.dir.as_deref()),
            })
            .collect(),
    )
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
    // agp may take seconds to answer; the wizard shows its step about it once it has.
    let found = tx.clone();
    std::thread::spawn(move || {
        let _ = found.send(Msg::Agp(agp_pool()));
    });

    let mut terminal = tui::init()?;
    let exit = (|| -> Result<Option<Next>> {
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
                        Action::Pane(slug) => return Ok(Some(Next::Pane(slug, OnQuit::Nothing))),
                        Action::Open(slug) => spawn(tx.clone(), move |send| open(&slug, send)),
                        Action::Create(draft) => {
                            spawn(tx.clone(), move |send| create(&draft, send))
                        }
                        Action::Archive(slug) => spawn_archive(tx.clone(), slug),
                        Action::Unarchive(slug) => {
                            let name = app
                                .selected()
                                .map_or_else(|| slug.clone(), |p| p.name.clone());
                            let status = unarchive(&slug, &name);
                            if let Ok(projects) = entries() {
                                app.set_projects(projects, Some(&slug));
                            }
                            app.status = Some(status);
                        }
                    }
                }
            }
            let mut finished = None;
            while let Ok(msg) = rx.try_recv() {
                let msg = match msg {
                    Msg::Agp(agp) => {
                        app.agp_found(agp, &probe);
                        continue;
                    }
                    other => other,
                };
                if let Msg::Created(slug) = &msg {
                    finished = Some(slug.clone());
                }
                // Archived or not, its sessions may have stopped: the list shows what is now.
                if let (Msg::Archived(_), app::Screen::Archiving(a)) = (&msg, &app.screen) {
                    finished = Some(a.slug.clone());
                }
                let done = matches!(msg, Msg::Done(_));
                // In a Rex block the board is beside it: this terminal becomes the conversation.
                // Without Rex or tmux it becomes the built-in layout, both sides.
                let here = match &msg {
                    Msg::Done(Ok(Where::Here { board })) => Some(Some(board.clone())),
                    Msg::Done(Ok(Where::Builtin)) => Some(None),
                    _ => None,
                };
                app.apply(msg);
                if let (Some(board), app::Screen::Starting(s)) = (here, &app.screen) {
                    let slug = s.slug.clone();
                    return Ok(Some(match board {
                        Some(board) => Next::Pane(slug, OnQuit::Board(board)),
                        None => Next::Builtin(slug),
                    }));
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
        Some(Next::Builtin(slug)) => project::run_builtin(&slug),
        Some(Next::Pane(slug, OnQuit::Nothing)) => {
            println!(
                "Showing the conversation with the coordinator. For its threads, run in another \
                 terminal: chief board --project {slug}"
            );
            crate::pane::run(&slug, OnQuit::Nothing)
        }
        Some(Next::Pane(slug, on_quit)) => crate::pane::run(&slug, on_quit),
        None => Ok(()),
    }
}

/// What this terminal shows once the launcher closes.
enum Next {
    /// `chief pane`, closing `OnQuit` when it ends.
    Pane(String, OnQuit),
    /// The built-in layout (§9): the conversation and the board.
    Builtin(String),
}

/// Runs `job` on a worker; its notes and its result come back as messages.
fn spawn(
    tx: mpsc::Sender<Msg>,
    job: impl FnOnce(&mut dyn FnMut(Msg)) -> Result<Where> + Send + 'static,
) {
    on_worker(tx, job, "Starting", Msg::Done);
}

/// `chief archive` on a worker, reporting into the Archiving card.
fn spawn_archive(tx: mpsc::Sender<Msg>, slug: String) {
    let job = move |send: &mut dyn FnMut(Msg)| {
        project::archive_with(&slug, &mut |note| send(Msg::Note(note)))
    };
    on_worker(tx, job, "Archiving", Msg::Archived);
}

/// Runs `job` on a worker: its notes come back as messages, then `done` with its result.
fn on_worker<T: 'static>(
    tx: mpsc::Sender<Msg>,
    job: impl FnOnce(&mut dyn FnMut(Msg)) -> Result<T> + Send + 'static,
    what: &'static str,
    done: fn(std::result::Result<T, String>) -> Msg,
) {
    std::thread::spawn(move || {
        let mut send = |msg: Msg| {
            let _ = tx.send(msg);
        };
        // A panic must still end the card's spinner.
        let result = match tui::catch(|| job(&mut send)) {
            Ok(result) => result.map_err(|e| format!("{e:#}")),
            Err(panic) => Err(format!("{what} crashed: {panic}")),
        };
        send(done(result));
    });
}

fn open(slug: &str, send: &mut dyn FnMut(Msg)) -> Result<Where> {
    project::up_with(slug, LayoutChoice::Auto, &mut |note| send(Msg::Note(note)))
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
            if !draft.agp {
                c.threads.without_agp();
            }
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
        send(Msg::Note(Note::Warn(untrusted_warning(
            &check.repos,
            &config.slug,
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

/// What repos left untrusted cost: in the project's account threads there don't start; in a
/// pool account only, new threads skip that account.
fn untrusted_warning(needs: &[trust::Need], slug: &str) -> String {
    let count = |n: usize| match n {
        1 => "1 folder isn't".to_string(),
        n => format!("{n} folders aren't"),
    };
    let pool = needs.iter().filter(|n| n.account.is_some()).count();
    match needs.len() - pool {
        0 => format!(
            "{} trusted in agp's other accounts, so threads there skip those accounts: chief \
             trust {slug}",
            count(pool)
        ),
        own => format!(
            "{} trusted, so threads there won't start: chief trust {slug}",
            count(own)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_tells_the_projects_account_from_the_pool() {
        let need = |account: Option<&str>| trust::Need {
            folder: PathBuf::from("/r/web"),
            config: PathBuf::from("/h/.claude.json"),
            account: account.map(str::to_string),
        };
        assert_eq!(
            untrusted_warning(&[need(None), need(None), need(Some("PB2"))], "pay"),
            "2 folders aren't trusted, so threads there won't start: chief trust pay"
        );
        assert_eq!(
            untrusted_warning(&[need(Some("PB2"))], "pay"),
            "1 folder isn't trusted in agp's other accounts, so threads there skip those \
             accounts: chief trust pay"
        );
    }

    /// A worker that panics still sends Done, so the Starting card stops spinning and says why.
    #[test]
    fn a_panicking_worker_still_finishes() {
        let (tx, rx) = mpsc::channel();
        spawn(tx, |send| {
            send(Msg::Note(Note::Did("Created project.".into())));
            panic!("boom")
        });
        let msgs: Vec<Msg> = rx.iter().collect();
        assert_eq!(
            msgs,
            [
                Msg::Note(Note::Did("Created project.".into())),
                Msg::Done(Err("Starting crashed: boom".into())),
            ]
        );
    }
}
