//! Board state and key handling, without a terminal. Keys become `Action`s that `mod.rs` runs
//! (on worker threads where they shell out); results come back as `Msg`s through `apply`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::cleanup::{Inspection, Mode};
use crate::config::ProjectConfig;
use crate::db::{BusySubagent, Event, Report, SessionRow};
use crate::placement::{PoolTrust, Untrusted};
use crate::state::{Group, Role};
use crate::tools;

use super::report::{self, ReportView};
use super::settings::{self, SettingsForm};
use super::ui::display_title;

/// Something the board asks its runner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Quit,
    /// Quit chief: ask the pane to quit too, which closes the layout (Ctrl+Q, as in the
    /// conversation).
    QuitAll,
    /// Show this thread (None: the coordinator) in the main pane.
    Show(Option<String>),
    /// Read this thread's reports for the report view.
    Reports(String),
    Wake(String),
    /// Stop this thread's process (`tools::stop_thread`).
    Stop(String),
    SetResolved {
        name: String,
        resolved: bool,
    },
    OpenUrl(String),
    Clean {
        name: String,
        mode: Mode,
    },
    /// A safe cleanup of every resolved thread (`cleanup::gc`).
    CleanAll,
    SaveSettings(Box<ProjectConfig>),
    /// Open a file in `$EDITOR` on this terminal.
    Edit(PathBuf),
    /// Check which repos of this draft the pool's accounts trust (§3).
    CheckTrust(Box<ProjectConfig>),
    /// Mark this pair trusted (the user pressed Enter on it), then check `config` again.
    Trust(Box<Untrusted>, Box<ProjectConfig>),
}

/// What the refresher and the workers send back.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Rows(Snapshot),
    /// Reading the database failed: keep the last rows.
    RefreshFailed(String),
    Pr(PrSummary),
    PrFailed(String),
    /// A worker that set `busy` finished; the text goes in the status line.
    Done(String),
    /// A quick action's result for the status line; leaves `busy` alone.
    Notice(String),
    /// A safe cleanup stopped: ask before discarding what `confirmed` lists, or, when chief
    /// can't list the loss itself (None), only say why.
    CleanBlocked {
        name: String,
        loss: String,
        confirmed: Option<Inspection>,
    },
    /// project.toml now holds `config`; `warning` says why `generated/*` wasn't rewritten.
    Saved {
        config: Box<ProjectConfig>,
        warning: Option<String>,
    },
    SaveFailed(String),
    /// A thread's reports, newest first, for the report view.
    Reports {
        name: String,
        reports: Vec<Report>,
    },
    /// The pool's trust check, or why it failed.
    Trust(Result<PoolTrust, String>),
    /// Marking a pair trusted went as `text` says; `check` is the check that followed.
    Trusted {
        text: String,
        check: Result<PoolTrust, String>,
    },
}

/// One refresh: every session of the project plus what the header needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub sessions: Vec<SessionRow>,
    /// A working or waiting thread's latest events, newest first, by name.
    pub activity: HashMap<String, Vec<Event>>,
    /// A working or waiting thread's subagents mid-turn, by name.
    pub subagents: HashMap<String, Vec<BusySubagent>>,
    pub denials: i64,
    pub pane_target: Option<String>,
    /// `claude agents` failed; the rows are the stored ones.
    pub error: Option<String>,
}

/// One PR refresh, for the status line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrSummary {
    pub hint: Option<String>,
    pub cleaned: Vec<String>,
    pub blocked: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub name: String,
    pub loss: String,
    pub input: String,
    /// What typing the name discards; None: nothing chief can list, so no discard is offered.
    pub confirmed: Option<Inspection>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Board,
    Help,
    Confirm(Confirm),
    /// Stop this thread? `label` is its title, else its name.
    Stop {
        name: String,
        label: String,
    },
    Settings(Box<SettingsForm>),
    Report(Box<ReportView>),
}

/// A line of the thread list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item<'a> {
    /// The coordinator, always first: the conversation in the main pane.
    Coordinator,
    Header {
        group: Group,
        count: usize,
        collapsed: bool,
    },
    Thread(&'a SessionRow),
    /// "No threads yet".
    Empty,
}

/// What a click on a screen line lands on, recorded by the last draw.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    Coordinator,
    Thread(String),
    /// A collapsed or expanded Resolved header: a click toggles it.
    Resolved,
    /// A settings field, by index.
    Field(usize),
}

pub struct App {
    pub slug: String,
    pub dir: PathBuf,
    pub config: ProjectConfig,
    /// Threads in board order.
    pub threads: Vec<SessionRow>,
    pub coordinator: Option<SessionRow>,
    /// What working and waiting threads do or ask: their latest events, newest first.
    pub activity: HashMap<String, Vec<Event>>,
    /// Their subagents mid-turn, each with its latest call.
    pub subagents: HashMap<String, Vec<BusySubagent>>,
    pub denials: i64,
    /// What the main pane shows; None is the coordinator.
    pub pane_target: Option<String>,
    /// The selected thread; ignored while `on_coordinator`.
    pub selected: Option<String>,
    /// The coordinator's row is selected.
    pub on_coordinator: bool,
    /// Selected only because no thread showed: the first thread to show takes the selection.
    coordinator_by_default: bool,
    pub show_resolved: bool,
    /// Short terminals show the detail instead of the list while this is on.
    pub detail_open: bool,
    pub screen: Screen,
    /// A blocked cleanup's modal that came while settings was open: it shows when settings
    /// closes, so the draft isn't thrown away.
    pub pending: Option<Confirm>,
    /// The last action's result; cleared by the next key.
    pub status: Option<String>,
    /// A worker is running (waking, cleaning up).
    pub busy: Option<String>,
    pub loaded: bool,
    pub refresh_error: Option<String>,
    pub pr: PrSummary,
    pub pr_hint: Option<String>,
    pub pr_error: Option<String>,
    pub now: i64,
    /// First visible list line, kept between frames so the list doesn't jump.
    pub scroll: Cell<usize>,
    /// Screen row → what is drawn there, from the last frame, for the mouse.
    pub hits: RefCell<Vec<(u16, Hit)>>,
    /// Drawn beside its conversation in the built-in layout (§9), which takes Ctrl+\.
    pub embedded: bool,
}

impl App {
    pub fn new(slug: &str, dir: PathBuf, config: ProjectConfig, now: i64) -> Self {
        App {
            slug: slug.to_string(),
            dir,
            config,
            threads: Vec::new(),
            coordinator: None,
            activity: HashMap::new(),
            subagents: HashMap::new(),
            denials: 0,
            pane_target: None,
            selected: None,
            on_coordinator: false,
            coordinator_by_default: false,
            show_resolved: false,
            detail_open: false,
            screen: Screen::Board,
            pending: None,
            status: None,
            busy: None,
            loaded: false,
            refresh_error: None,
            pr: PrSummary::default(),
            pr_hint: None,
            pr_error: None,
            now,
            scroll: Cell::new(0),
            hits: RefCell::new(Vec::new()),
            embedded: false,
        }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Rows(s) => {
                let (coordinators, mut threads): (Vec<_>, Vec<_>) = s
                    .sessions
                    .into_iter()
                    .partition(|r| r.role == Role::Coordinator.as_str());
                tools::board_order(&mut threads);
                self.coordinator = coordinators.into_iter().next();
                self.threads = threads;
                self.activity = s.activity;
                self.subagents = s.subagents;
                self.denials = s.denials;
                self.pane_target = s.pane_target;
                self.refresh_error = s.error;
                self.loaded = true;
                self.fix_selection();
            }
            Msg::RefreshFailed(e) => self.refresh_error = Some(e),
            Msg::Pr(summary) => {
                if summary.hint.is_some() {
                    self.pr_hint = summary.hint.clone();
                }
                if !summary.cleaned.is_empty() {
                    self.status = Some(format!(
                        "Cleaned up after merge: {}",
                        summary.cleaned.join(", ")
                    ));
                }
                self.pr = summary;
                self.pr_error = None;
            }
            Msg::PrFailed(e) => self.pr_error = Some(e),
            Msg::Done(text) => {
                self.busy = None;
                self.status = Some(text);
            }
            Msg::Notice(text) => self.status = Some(text),
            Msg::CleanBlocked {
                name,
                loss,
                confirmed,
            } => {
                self.busy = None;
                let confirm = Confirm {
                    name,
                    loss,
                    input: String::new(),
                    confirmed,
                };
                match &mut self.screen {
                    Screen::Settings(form) => {
                        form.message = Some(format!(
                            "Cleanup of {} stopped: the board says why when you leave settings.",
                            confirm.name
                        ));
                        self.pending = Some(confirm);
                    }
                    _ => self.screen = Screen::Confirm(confirm),
                }
            }
            Msg::Saved { config, warning } => {
                if let Screen::Settings(form) = &mut self.screen {
                    form.saved(&config, warning);
                }
                self.config = *config;
            }
            Msg::SaveFailed(e) => match &mut self.screen {
                Screen::Settings(form) => form.message = Some(e),
                _ => self.status = Some(e),
            },
            Msg::Trust(result) => {
                if let Screen::Settings(form) = &mut self.screen {
                    form.trust_checked(result);
                }
            }
            Msg::Trusted { text, check } => match &mut self.screen {
                Screen::Settings(form) => form.trust_marked(text, check),
                _ => self.status = Some(text),
            },
            Msg::Reports { name, reports } => {
                if let Screen::Report(view) = &mut self.screen
                    && view.name == name
                {
                    view.loaded(reports);
                }
            }
        }
    }

    /// Threads the list shows, in order.
    pub fn visible(&self) -> Vec<&SessionRow> {
        self.threads
            .iter()
            .filter(|r| self.show_resolved || group_of(r) != Group::Resolved)
            .collect()
    }

    pub fn items(&self) -> Vec<Item<'_>> {
        let mut items = vec![Item::Coordinator];
        if self.threads.is_empty() {
            items.push(Item::Empty);
            return items;
        }
        for group in Group::ORDER {
            let rows: Vec<_> = self
                .threads
                .iter()
                .filter(|r| group_of(r) == group)
                .collect();
            if rows.is_empty() {
                continue;
            }
            let collapsed = group == Group::Resolved && !self.show_resolved;
            items.push(Item::Header {
                group,
                count: rows.len(),
                collapsed,
            });
            if !collapsed {
                items.extend(rows.into_iter().map(Item::Thread));
            }
        }
        items
    }

    /// The selected thread; None when the coordinator's row is selected.
    pub fn selected_row(&self) -> Option<&SessionRow> {
        if self.on_coordinator {
            return None;
        }
        let name = self.selected.as_deref()?;
        self.visible().into_iter().find(|r| r.name == name)
    }

    /// Keeps the selection on a visible thread: the same one if it still shows, else the first,
    /// else the coordinator. A selected coordinator stays selected.
    fn fix_selection(&mut self) {
        if self.on_coordinator && !self.coordinator_by_default {
            return;
        }
        let visible = self.visible();
        let keep = self
            .selected
            .as_deref()
            .is_some_and(|n| visible.iter().any(|r| r.name == n));
        if !keep || self.coordinator_by_default {
            self.selected = visible.first().map(|r| r.name.clone());
            self.on_coordinator = self.selected.is_none();
            self.coordinator_by_default = self.on_coordinator;
        }
    }

    /// Moves over the coordinator (position 0) and the visible threads, stopping at the ends.
    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        let current = if self.on_coordinator {
            0
        } else {
            self.selected
                .as_deref()
                .and_then(|n| visible.iter().position(|r| r.name == n))
                .map_or(0, |i| i + 1)
        };
        let next = (current as isize)
            .saturating_add(delta)
            .clamp(0, visible.len() as isize);
        let thread = (next > 0).then(|| visible[next as usize - 1].name.clone());
        self.on_coordinator = thread.is_none();
        self.coordinator_by_default = false;
        if thread.is_some() {
            self.selected = thread;
        }
    }

    fn select(&mut self, hit: &Hit) {
        match hit {
            Hit::Coordinator => {
                self.on_coordinator = true;
                self.coordinator_by_default = false;
            }
            Hit::Thread(name) => {
                self.on_coordinator = false;
                self.coordinator_by_default = false;
                self.selected = Some(name.clone());
            }
            Hit::Resolved | Hit::Field(_) => {}
        }
    }

    /// A result for whichever screen is open: the settings form's message, else the status line.
    pub fn say(&mut self, text: String) {
        match &mut self.screen {
            Screen::Settings(form) => form.message = Some(text),
            _ => self.status = Some(text),
        }
    }

    /// Clicks select (a click on the selection shows it, as Enter does; one on the Resolved
    /// header shows or hides it); the wheel moves the selection.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Vec<Action> {
        let hit = || {
            self.hits
                .borrow()
                .iter()
                .find(|(y, _)| *y == ev.row)
                .map(|(_, h)| h.clone())
        };
        match (&mut self.screen, ev.kind) {
            (Screen::Help, MouseEventKind::Down(_)) => self.screen = Screen::Board,
            (Screen::Report(view), kind) => view.on_mouse(kind),
            // The text being typed belongs to its field: the cursor stays until Enter or Esc.
            (Screen::Settings(form), _) if form.editing.is_some() => {}
            (Screen::Settings(form), MouseEventKind::ScrollUp) => form.move_cursor(-1),
            (Screen::Settings(form), MouseEventKind::ScrollDown) => form.move_cursor(1),
            (Screen::Settings(_), MouseEventKind::Down(MouseButton::Left)) => {
                if let (Some(Hit::Field(i)), Screen::Settings(form)) = (hit(), &mut self.screen) {
                    form.cursor = i;
                }
            }
            (Screen::Board, MouseEventKind::ScrollUp) => self.move_selection(-1),
            (Screen::Board, MouseEventKind::ScrollDown) => self.move_selection(1),
            (Screen::Board, MouseEventKind::Down(MouseButton::Left)) => {
                self.status = None;
                match hit() {
                    Some(Hit::Resolved) => {
                        self.show_resolved = !self.show_resolved;
                        self.fix_selection();
                    }
                    Some(h) if self.is_selected(&h) => {
                        return self.board_key(KeyEvent::from(KeyCode::Enter));
                    }
                    Some(h) => self.select(&h),
                    None => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn is_selected(&self, hit: &Hit) -> bool {
        match hit {
            Hit::Coordinator => self.on_coordinator,
            Hit::Thread(name) => !self.on_coordinator && self.selected.as_deref() == Some(name),
            Hit::Resolved | Hit::Field(_) => false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return vec![Action::Quit],
                KeyCode::Char('q' | 'Q') => return vec![Action::QuitAll],
                _ => {}
            }
        }
        match &mut self.screen {
            Screen::Board => self.board_key(key),
            Screen::Help => {
                self.screen = Screen::Board;
                Vec::new()
            }
            Screen::Confirm(_) => self.confirm_key(key),
            Screen::Stop { .. } => self.stop_key(key),
            Screen::Report(view) => {
                if let report::Outcome::Close = view.on_key(key) {
                    self.screen = Screen::Board;
                }
                Vec::new()
            }
            Screen::Settings(form) => match form.on_key(key) {
                settings::Outcome::Stay => Vec::new(),
                settings::Outcome::Close => {
                    self.screen = match self.pending.take() {
                        Some(confirm) => Screen::Confirm(confirm),
                        None => Screen::Board,
                    };
                    Vec::new()
                }
                settings::Outcome::Run(action) => vec![action],
            },
        }
    }

    fn board_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.status = None;
        match key.code {
            KeyCode::Char('q') => return vec![Action::Quit],
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Tab => self.detail_open = !self.detail_open,
            KeyCode::Char('R') => {
                self.show_resolved = !self.show_resolved;
                self.fix_selection();
            }
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Esc | KeyCode::Char('c') => return self.show_coordinator(),
            KeyCode::Enter if self.on_coordinator => return self.show_coordinator(),
            KeyCode::Enter => return self.on_thread(|app, row| app.show(row)),
            KeyCode::Char('n') => return self.next_waiting(),
            KeyCode::Char('w') => return self.on_thread(|app, row| app.wake(row)),
            KeyCode::Char('S') => return self.on_thread(|app, row| app.ask_stop(row)),
            KeyCode::Char('X') => return self.clean_all(),
            KeyCode::Char('r') => return self.on_thread(|app, row| app.resolve(row)),
            KeyCode::Char('o') => return self.on_thread(|app, row| app.open_pr(row)),
            KeyCode::Char('x') => return self.on_thread(|app, row| app.clean(row)),
            KeyCode::Char('v') => return self.on_thread(|app, row| app.read_reports(row)),
            _ => {}
        }
        Vec::new()
    }

    /// The form starts from project.toml as it is now, so a save keeps edits made by hand while
    /// the board ran; the header's name and goal follow them.
    fn open_settings(&mut self) {
        let message = match ProjectConfig::load(&self.dir.join("project.toml")) {
            Ok(config) => {
                self.config = config;
                None
            }
            Err(e) => Some(format!(
                "Couldn't re-read project.toml ({e:#}): this is the board's copy."
            )),
        };
        let mut form = SettingsForm::new(&self.config, self.dir.clone());
        form.message = message;
        self.screen = Screen::Settings(Box::new(form));
    }

    fn show_coordinator(&mut self) -> Vec<Action> {
        self.pane_target = None;
        vec![Action::Show(None)]
    }

    /// Runs a thread key on the selection, or says there is none.
    fn on_thread(&mut self, f: impl FnOnce(&mut App, SessionRow) -> Vec<Action>) -> Vec<Action> {
        match self.selected_row().cloned() {
            Some(row) => f(self, row),
            None if self.on_coordinator => self.refuse(
                "That key acts on a thread: pick one with ↓. The coordinator is the conversation in \
                 the main pane."
                    .to_string(),
            ),
            None => self.refuse("No thread selected.".to_string()),
        }
    }

    fn refuse(&mut self, why: String) -> Vec<Action> {
        self.status = Some(why);
        Vec::new()
    }

    fn show(&mut self, row: SessionRow) -> Vec<Action> {
        if row.removed_at.is_some() {
            return self.refuse(format!(
                "{}'s session was removed outside chief: there is no session to show.",
                row.name
            ));
        }
        if row.cleaned_at.is_some() {
            return self.refuse(format!(
                "{} was cleaned up: there is no session to show.",
                row.name
            ));
        }
        self.pane_target = Some(row.name.clone());
        vec![Action::Show(Some(row.name))]
    }

    /// The report view, with the row's last report until the worker's read comes back.
    fn read_reports(&mut self, row: SessionRow) -> Vec<Action> {
        self.screen = Screen::Report(Box::new(ReportView::new(&row)));
        vec![Action::Reports(row.name)]
    }

    fn wake(&mut self, row: SessionRow) -> Vec<Action> {
        if row.removed_at.is_some() {
            return self.refuse(crate::guard::removed_message(&row.name));
        }
        if row.cleaned_at.is_some() {
            return self.refuse(crate::guard::cleaned_message(&row.name));
        }
        if let Some(busy) = &self.busy {
            return self.refuse(format!("Wait: {busy}"));
        }
        self.busy = Some(format!("waking {}…", row.name));
        vec![Action::Wake(row.name)]
    }

    /// Shows the next thread waiting on the user after the one the main pane shows (wrapping),
    /// and selects it, so answering them in turn needs no looking.
    pub fn next_waiting(&mut self) -> Vec<Action> {
        let waiting: Vec<&SessionRow> = self
            .threads
            .iter()
            .filter(|r| group_of(r) == Group::Waiting)
            .collect();
        let shown = self
            .pane_target
            .as_deref()
            .and_then(|n| waiting.iter().position(|r| r.name == n));
        let next = match (waiting.len(), shown) {
            (0, _) => return self.refuse("Nothing is waiting on you.".to_string()),
            (1, Some(_)) => {
                return self.refuse(format!(
                    "{} is the only thread waiting on you, and the main pane shows it.",
                    display_title(waiting[0])
                ));
            }
            (n, shown) => waiting[shown.map_or(0, |i| (i + 1) % n)].clone(),
        };
        self.select(&Hit::Thread(next.name.clone()));
        self.show(next)
    }

    fn ask_stop(&mut self, row: SessionRow) -> Vec<Action> {
        if row.removed_at.is_some() {
            return self.refuse(crate::guard::removed_message(&row.name));
        }
        if row.cleaned_at.is_some() {
            return self.refuse(crate::guard::cleaned_message(&row.name));
        }
        if row.pid.is_none() {
            return self.refuse(format!("{} is asleep: there is nothing to stop.", row.name));
        }
        if let Some(busy) = &self.busy {
            return self.refuse(format!("Wait: {busy}"));
        }
        self.screen = Screen::Stop {
            label: display_title(&row),
            name: row.name,
        };
        Vec::new()
    }

    /// y stops; any other key leaves it running.
    fn stop_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let Screen::Stop { name, .. } = std::mem::replace(&mut self.screen, Screen::Board) else {
            return Vec::new();
        };
        if key.code != KeyCode::Char('y') {
            self.status = Some(format!("{name} keeps running."));
            return Vec::new();
        }
        // The main pane leaves it first (the worker does that too): it would respawn it.
        if self.pane_target.as_deref() == Some(name.as_str()) {
            self.pane_target = None;
        }
        self.busy = Some(format!("stopping {name}…"));
        vec![Action::Stop(name)]
    }

    fn clean_all(&mut self) -> Vec<Action> {
        let any = self
            .threads
            .iter()
            .any(|r| r.resolved && r.cleaned_at.is_none());
        if !any {
            return self.refuse("No resolved thread is left to clean up.".to_string());
        }
        if let Some(busy) = &self.busy {
            return self.refuse(format!("Wait: {busy}"));
        }
        self.busy = Some("cleaning up resolved threads…".to_string());
        vec![Action::CleanAll]
    }

    fn resolve(&mut self, row: SessionRow) -> Vec<Action> {
        if row.resolved && row.removed_at.is_some() {
            return self.refuse(crate::guard::removed_message(&row.name));
        }
        if row.resolved && row.cleaned_at.is_some() {
            return self.refuse(crate::guard::cleaned_message(&row.name));
        }
        // Flipped here too, so a second r before the refresh undoes the first instead of
        // sending it again.
        if let Some(r) = self.threads.iter_mut().find(|r| r.name == row.name) {
            r.resolved = !row.resolved;
        }
        vec![Action::SetResolved {
            name: row.name,
            resolved: !row.resolved,
        }]
    }

    fn open_pr(&mut self, row: SessionRow) -> Vec<Action> {
        match row.pr_url {
            Some(url) => vec![Action::OpenUrl(url)],
            None => self.refuse(format!("{} has no pull request.", row.name)),
        }
    }

    fn clean(&mut self, row: SessionRow) -> Vec<Action> {
        if row.cleaned_at.is_some() {
            return self.refuse(format!("{} is already cleaned up.", row.name));
        }
        if !row.resolved {
            return self.refuse(format!(
                "x cleans up resolved threads: resolve {} first with r.",
                row.name
            ));
        }
        if let Some(busy) = &self.busy {
            return self.refuse(format!("Wait: {busy}"));
        }
        self.busy = Some(format!("cleaning up {}…", row.name));
        vec![Action::Clean {
            name: row.name,
            mode: Mode::Safe,
        }]
    }

    fn confirm_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let Screen::Confirm(confirm) = &mut self.screen else {
            return Vec::new();
        };
        let Some(confirmed) = confirm.confirmed.clone() else {
            // Nothing to confirm: the modal only explains, and any key closes it.
            self.screen = Screen::Board;
            self.status = Some("Not cleaned up; nothing was removed.".to_string());
            return Vec::new();
        };
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Board;
                self.status = Some("Cleanup cancelled; nothing was removed.".to_string());
            }
            KeyCode::Backspace => {
                confirm.input.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                confirm.input.push(c);
            }
            KeyCode::Enter => {
                if confirm.input.trim() != confirm.name {
                    confirm.input.clear();
                    return Vec::new();
                }
                let name = confirm.name.clone();
                self.screen = Screen::Board;
                self.busy = Some(format!("discarding and cleaning up {name}…"));
                return vec![Action::Clean {
                    name,
                    mode: Mode::Discard(confirmed),
                }];
            }
            _ => {}
        }
        Vec::new()
    }
}

pub fn group_of(row: &SessionRow) -> Group {
    Group::parse(&row.group_name).unwrap_or(Group::Idle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const NOW: i64 = 1_800_000_000_000;

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// A thread last active `activity` minutes after an hour before `NOW` (higher is newer).
    pub fn thread(name: &str, group: Group, activity: i64) -> SessionRow {
        let at = NOW - 3_600_000 + activity * 60_000;
        let mut r = SessionRow::new(name, "demo", Role::Thread, at);
        r.group_name = group.as_str().to_string();
        r.resolved = group == Group::Resolved;
        r.short_id = Some(format!("s-{name}"));
        r.pid = Some(1);
        r
    }

    pub fn app_with(rows: Vec<SessionRow>) -> App {
        let config = ProjectConfig::new("Demo", "demo", Some("Ship it"), Vec::new());
        let mut app = App::new("demo", PathBuf::from("/nowhere/demo"), config, NOW);
        let mut sessions = rows;
        sessions.push(SessionRow::new("chief-demo", "demo", Role::Coordinator, 0));
        app.apply(Msg::Rows(Snapshot {
            sessions,
            ..Default::default()
        }));
        app
    }

    fn sample() -> App {
        app_with(vec![
            thread("idle-1", Group::Idle, 5),
            thread("wait-1", Group::Waiting, 1),
            thread("work-1", Group::Working, 3),
            thread("work-2", Group::Working, 4),
            thread("done-1", Group::Resolved, 9),
        ])
    }

    /// `v` reads a thread's reports: the view opens at once on the row's last report, a worker
    /// reads them all, and only that thread's read reaches it; Esc goes back. The coordinator
    /// has no view.
    #[test]
    fn v_opens_a_threads_reports() {
        let mut app = sample();
        app.on_key(ch('g'));
        assert_eq!(app.on_key(ch('v')), []);
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .starts_with("That key acts on a thread")
        );
        app.on_key(ch('j'));
        let name = app.selected_row().unwrap().name.clone();
        assert_eq!(app.on_key(ch('v')), [Action::Reports(name.clone())]);
        let report = |text: &str| Report {
            session_name: name.clone(),
            ts: NOW,
            text: text.into(),
        };
        app.apply(Msg::Reports {
            name: "other".into(),
            reports: vec![report("not this one")],
        });
        app.apply(Msg::Reports {
            name: name.clone(),
            reports: vec![report("second"), report("first")],
        });
        let Screen::Report(view) = &app.screen else {
            panic!("no report view: {:?}", app.screen);
        };
        assert_eq!(view.reports.len(), 2);
        assert_eq!(view.reports[0].text, "second");
        // Its keys are its own: `q` closes it, not the board.
        assert_eq!(app.on_key(ch('j')), []);
        assert_eq!(app.on_key(ch('q')), []);
        assert_eq!(app.screen, Screen::Board);
        app.on_key(ch('v'));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Board);
    }

    /// n shows the waiting threads in turn, after whichever the main pane shows.
    #[test]
    fn n_shows_the_next_thread_waiting() {
        let mut app = app_with(vec![
            thread("wait-1", Group::Waiting, 2),
            thread("work-1", Group::Working, 3),
            thread("wait-2", Group::Waiting, 1),
        ]);
        let first = app.on_key(ch('n'));
        let Action::Show(Some(first)) = &first[0] else {
            panic!("{first:?}")
        };
        assert_eq!(app.selected.as_deref(), Some(first.as_str()));
        let second = app.on_key(ch('n'));
        let Action::Show(Some(second)) = &second[0] else {
            panic!("{second:?}")
        };
        assert_ne!(first, second);
        assert_eq!(app.on_key(ch('n')), vec![Action::Show(Some(first.clone()))]);

        let mut one = app_with(vec![thread("wait-1", Group::Waiting, 2)]);
        one.on_key(ch('n'));
        assert!(one.on_key(ch('n')).is_empty());
        assert!(one.status.as_deref().unwrap().contains("the only thread"));
        let mut none = app_with(vec![thread("work-1", Group::Working, 3)]);
        assert!(none.on_key(ch('n')).is_empty());
        assert_eq!(none.status.as_deref(), Some("Nothing is waiting on you."));
    }

    /// S asks before stopping a running thread and refuses an asleep one; the main pane leaves
    /// a thread being stopped. X cleans up every resolved thread, one worker at a time.
    #[test]
    fn stop_and_clean_all() {
        let mut asleep = thread("idle-1", Group::Idle, 1);
        asleep.pid = None;
        let mut app = app_with(vec![
            thread("work-1", Group::Working, 3),
            asleep,
            thread("done-1", Group::Resolved, 2),
        ]);
        app.selected = Some("work-1".into());
        app.pane_target = Some("work-1".into());
        assert!(app.on_key(ch('S')).is_empty());
        assert!(matches!(app.screen, Screen::Stop { .. }));
        assert_eq!(app.on_key(ch('y')), vec![Action::Stop("work-1".into())]);
        assert_eq!(app.pane_target, None);
        assert!(app.on_key(ch('X')).is_empty(), "one worker at a time");
        app.apply(Msg::Done("Stopped work-1; w wakes it.".into()));

        app.selected = Some("idle-1".into());
        assert!(app.on_key(ch('S')).is_empty());
        assert_eq!(app.screen, Screen::Board);
        assert!(app.status.as_deref().unwrap().contains("asleep"));

        assert_eq!(app.on_key(ch('X')), vec![Action::CleanAll]);
        let mut none = app_with(vec![thread("work-1", Group::Working, 3)]);
        assert!(none.on_key(ch('X')).is_empty());
    }

    #[test]
    fn rows_are_grouped_in_board_order_with_resolved_collapsed() {
        let app = sample();
        let items = app.items();
        let shown: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::Header {
                    group,
                    count,
                    collapsed,
                } => format!(
                    "[{} {count}{}]",
                    group.as_str(),
                    if *collapsed { "+" } else { "" }
                ),
                Item::Thread(r) => r.name.clone(),
                Item::Empty => "empty".into(),
                Item::Coordinator => "coordinator".into(),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "coordinator",
                "[waiting 1]",
                "wait-1",
                "[working 2]",
                "work-2",
                "work-1",
                "[idle 1]",
                "idle-1",
                "[resolved 1+]"
            ]
        );
        assert_eq!(app.coordinator.as_ref().unwrap().name, "chief-demo");
        assert_eq!(app.selected.as_deref(), Some("wait-1"));
        assert!(!app.on_coordinator);
        let empty = app_with(vec![]);
        assert_eq!(empty.items(), [Item::Coordinator, Item::Empty]);
        // Nothing else to select: the coordinator is.
        assert!(empty.on_coordinator);
        assert_eq!(empty.selected_row(), None);
    }

    #[test]
    fn selection_skips_headers_and_stops_at_the_ends() {
        let mut app = sample();
        // Above the first thread is the coordinator, and above it nothing.
        app.on_key(key(KeyCode::Up));
        assert!(app.on_coordinator);
        app.on_key(key(KeyCode::Up));
        assert!(app.on_coordinator);
        app.on_key(key(KeyCode::Down));
        assert!(!app.on_coordinator);
        assert_eq!(app.selected.as_deref(), Some("wait-1"));
        let mut seen = Vec::new();
        for _ in 0..5 {
            app.on_key(ch('j'));
            seen.push(app.selected.clone().unwrap());
        }
        assert_eq!(seen, ["work-2", "work-1", "idle-1", "idle-1", "idle-1"]);
        app.on_key(ch('k'));
        assert_eq!(app.selected.as_deref(), Some("work-1"));
        app.on_key(ch('g'));
        assert!(app.on_coordinator);
        app.on_key(ch('j'));
        assert_eq!(app.selected.as_deref(), Some("wait-1"));

        // Showing resolved threads makes them selectable; hiding them moves off.
        app.on_key(ch('R'));
        app.on_key(ch('G'));
        assert_eq!(app.selected.as_deref(), Some("done-1"));
        app.on_key(ch('R'));
        assert_eq!(app.selected.as_deref(), Some("wait-1"));
    }

    #[test]
    fn selection_follows_the_thread_across_refreshes() {
        let mut app = sample();
        app.on_key(ch('j'));
        assert_eq!(app.selected.as_deref(), Some("work-2"));
        let mut rows = app.threads.clone();
        rows.iter_mut()
            .find(|r| r.name == "work-2")
            .unwrap()
            .group_name = "idle".into();
        app.apply(Msg::Rows(Snapshot {
            sessions: rows.clone(),
            ..Default::default()
        }));
        assert_eq!(app.selected.as_deref(), Some("work-2"));
        rows.retain(|r| r.name != "work-2");
        app.apply(Msg::Rows(Snapshot {
            sessions: rows,
            ..Default::default()
        }));
        assert_eq!(app.selected.as_deref(), Some("wait-1"));
    }

    #[test]
    fn enter_and_escape_switch_the_main_pane() {
        let mut app = sample();
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            [Action::Show(Some("wait-1".into()))]
        );
        assert_eq!(app.pane_target.as_deref(), Some("wait-1"));
        assert_eq!(app.on_key(key(KeyCode::Esc)), [Action::Show(None)]);
        assert_eq!(app.pane_target, None);
        assert_eq!(app.on_key(ch('c')), [Action::Show(None)]);
        // Enter on the coordinator's row shows it too; thread keys there say so.
        app.on_key(ch('g'));
        assert_eq!(app.on_key(key(KeyCode::Enter)), [Action::Show(None)]);
        assert!(app.on_key(ch('w')).is_empty());
        assert!(app.status.as_deref().unwrap().contains("acts on a thread"));
        // A refresh keeps the coordinator selected.
        app.apply(Msg::Rows(Snapshot {
            sessions: app.threads.clone(),
            ..Default::default()
        }));
        assert!(app.on_coordinator);
        assert_eq!(app.on_key(ch('q')), [Action::Quit]);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            [Action::Quit]
        );
        // Ctrl+Q quits chief, the pane too.
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            [Action::QuitAll]
        );
    }

    #[test]
    fn thread_keys_depend_on_the_row() {
        let mut idle = thread("idle-1", Group::Idle, 5);
        idle.pr_url = Some("https://github.com/o/r/pull/1".into());
        let mut cleaned = thread("gone-1", Group::Resolved, 1);
        cleaned.cleaned_at = Some(2);
        let mut app = app_with(vec![idle, cleaned, thread("done-1", Group::Resolved, 3)]);
        app.on_key(ch('R'));

        // Unresolved: x refused, r resolves, w wakes, o opens.
        assert_eq!(app.selected.as_deref(), Some("idle-1"));
        assert!(app.on_key(ch('x')).is_empty());
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("resolve idle-1 first")
        );
        assert_eq!(
            app.on_key(ch('r')),
            [Action::SetResolved {
                name: "idle-1".into(),
                resolved: true
            }]
        );
        assert_eq!(
            app.on_key(ch('o')),
            [Action::OpenUrl("https://github.com/o/r/pull/1".into())]
        );
        assert_eq!(app.on_key(ch('w')), [Action::Wake("idle-1".into())]);
        assert_eq!(app.busy.as_deref(), Some("waking idle-1…"));
        // One worker at a time for wake and cleanup.
        assert!(app.on_key(ch('w')).is_empty());
        app.apply(Msg::Done("idle-1 is awake.".into()));
        assert_eq!(app.busy, None);
        assert_eq!(app.status.as_deref(), Some("idle-1 is awake."));

        // Resolved: r reopens, x cleans safely, no PR to open.
        app.on_key(ch('j'));
        assert_eq!(app.selected.as_deref(), Some("done-1"));
        assert_eq!(
            app.on_key(ch('r')),
            [Action::SetResolved {
                name: "done-1".into(),
                resolved: false
            }]
        );
        // A second r before the refresh undoes the first rather than sending it again.
        assert_eq!(
            app.on_key(ch('r')),
            [Action::SetResolved {
                name: "done-1".into(),
                resolved: true
            }]
        );
        assert!(app.on_key(ch('o')).is_empty());
        assert_eq!(
            app.on_key(ch('x')),
            [Action::Clean {
                name: "done-1".into(),
                mode: Mode::Safe
            }]
        );
        app.apply(Msg::Done("Cleaned up done-1.".into()));

        // Cleaned: no wake, reopen, cleanup or showing.
        app.on_key(ch('j'));
        assert_eq!(app.selected.as_deref(), Some("gone-1"));
        for c in ['w', 'r', 'x'] {
            assert!(app.on_key(ch(c)).is_empty(), "{c}");
            assert!(app.status.is_some(), "{c} says why");
        }
        assert!(app.on_key(ch('r')).is_empty());
        assert!(app.status.as_deref().unwrap().contains("was cleaned up"));
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    }

    /// Removed outside chief with its worktree left: no session to wake, reopen or show, but x
    /// still runs (the cleanup reports where the worktree remains).
    #[test]
    fn a_thread_removed_outside_chief_cannot_be_woken_reopened_or_shown() {
        let mut removed = thread("gone-1", Group::Resolved, 1);
        removed.removed_at = Some(2);
        let mut app = app_with(vec![removed]);
        app.on_key(ch('R'));
        assert_eq!(app.selected.as_deref(), Some("gone-1"));
        for c in ['w', 'r'] {
            assert!(app.on_key(ch(c)).is_empty(), "{c}");
            assert!(
                app.status
                    .as_deref()
                    .unwrap()
                    .contains("removed outside chief"),
                "{c}: {:?}",
                app.status
            );
        }
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("removed outside chief")
        );
        assert_eq!(
            app.on_key(ch('x')),
            [Action::Clean {
                name: "gone-1".into(),
                mode: Mode::Safe
            }]
        );
    }

    #[test]
    fn without_threads_thread_keys_say_what_they_need() {
        let mut app = app_with(vec![]);
        assert!(app.on_coordinator);
        for c in ['w', 'r', 'o', 'x'] {
            assert!(app.on_key(ch(c)).is_empty());
            assert!(
                app.status.as_deref().unwrap().contains("acts on a thread"),
                "{:?}",
                app.status
            );
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), [Action::Show(None)]);
        // The first thread to show takes the default selection.
        app.apply(Msg::Rows(Snapshot {
            sessions: vec![thread("new-1", Group::Working, 1)],
            ..Default::default()
        }));
        assert!(!app.on_coordinator);
        assert_eq!(app.selected.as_deref(), Some("new-1"));
    }

    fn listed_loss() -> Inspection {
        Inspection {
            worktree: Some("/r/.claude/worktrees/done".into()),
            exists: true,
            unpushed_count: 1,
            unpushed: vec!["abc123 Draft".into()],
            head: Some("abc123".into()),
            ..Default::default()
        }
    }

    #[test]
    fn discard_needs_the_thread_name_typed() {
        let mut app = app_with(vec![thread("done-1", Group::Resolved, 3)]);
        app.on_key(ch('R'));
        app.on_key(ch('x'));
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "This loses:\n  1 commit on no remote".into(),
            confirmed: Some(listed_loss()),
        });
        assert_eq!(app.busy, None);
        assert!(matches!(app.screen, Screen::Confirm(_)));

        // Board keys are text here: q types a q, it doesn't quit.
        for c in "done-q".chars() {
            assert!(app.on_key(ch(c)).is_empty());
        }
        assert!(app.on_key(key(KeyCode::Enter)).is_empty(), "wrong name");
        let Screen::Confirm(c) = &app.screen else {
            panic!("still confirming")
        };
        assert_eq!(c.input, "", "a wrong name clears the input");

        for c in "done-11".chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            [Action::Clean {
                name: "done-1".into(),
                mode: Mode::Discard(listed_loss())
            }]
        );
        assert_eq!(app.screen, Screen::Board);
        assert!(app.busy.is_some());

        // Esc cancels.
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "x".into(),
            confirmed: Some(listed_loss()),
        });
        assert!(app.on_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(app.screen, Screen::Board);
        assert!(app.status.as_deref().unwrap().contains("cancelled"));
    }

    /// When chief can't list what a discard would lose, the modal only explains: typing the name
    /// discards nothing.
    #[test]
    fn a_blocked_modal_without_a_loss_list_never_discards() {
        let mut app = app_with(vec![thread("done-1", Group::Resolved, 3)]);
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "Cleanup blocked: 1 unpushed commit.".into(),
            confirmed: None,
        });
        for c in "done-1".chars() {
            assert!(app.on_key(ch(c)).is_empty());
            if app.screen == Screen::Board {
                break;
            }
        }
        assert_eq!(app.screen, Screen::Board, "any key closes it");
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "x".into(),
            confirmed: None,
        });
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.screen, Screen::Board);
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("nothing was removed")
        );
    }

    /// A blocked cleanup's modal never replaces an open settings form: it waits, and shows once
    /// settings closes.
    #[test]
    fn a_blocked_cleanup_waits_for_settings_to_close() {
        let mut app = app_with(vec![thread("done-1", Group::Resolved, 3)]);
        app.on_key(ch('R'));
        app.on_key(ch('x'));
        app.on_key(ch('s'));
        if let Screen::Settings(form) = &mut app.screen {
            form.draft.ui.notify = !form.draft.ui.notify;
        }
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "This loses:\n  1 commit on no remote".into(),
            confirmed: Some(listed_loss()),
        });
        assert_eq!(app.busy, None);
        let Screen::Settings(form) = &app.screen else {
            panic!("settings stays open")
        };
        assert!(form.dirty(), "the draft is kept");
        assert!(form.message.as_deref().unwrap().contains("done-1 stopped"));
        // Esc twice: the draft is thrown away on purpose, and the modal shows.
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        let Screen::Confirm(c) = &app.screen else {
            panic!("the modal shows: {:?}", app.screen)
        };
        assert_eq!(c.name, "done-1");
        assert_eq!(app.pending, None);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Board);
    }

    fn mouse(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 2,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// While a field is being typed into, the wheel and clicks leave the cursor alone, and Enter
    /// puts the text in the field it was typed into.
    #[test]
    fn the_mouse_waits_for_an_edit() {
        let mut app = sample();
        app.on_key(ch('s'));
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.current(), settings::Field::Goal);
        let model = form
            .fields()
            .iter()
            .position(|f| *f == settings::Field::ThreadModel)
            .unwrap();
        app.hits.borrow_mut().push((9, Hit::Field(model)));
        app.on_key(key(KeyCode::Enter));
        for c in " now".chars() {
            app.on_key(ch(c));
        }
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 0));
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 9));
        app.on_key(key(KeyCode::Enter));
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.cursor, 0);
        assert_eq!(form.draft.goal.as_deref(), Some("Ship it now"));
        assert_eq!(form.draft.threads.model, "sonnet");
        // With no edit open, a click moves the cursor.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 9));
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.cursor, model);
    }

    /// Settings opens on project.toml as it is now, and what the editor reports shows in the
    /// form rather than in the hidden status line.
    #[test]
    fn settings_reads_project_toml_when_it_opens() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sample();
        app.dir = dir.path().into();
        let mut edited = app.config.clone();
        edited.goal = Some("Edited by hand".into());
        edited.save(&dir.path().join("project.toml")).unwrap();
        app.on_key(ch('s'));
        assert_eq!(app.config.goal.as_deref(), Some("Edited by hand"));
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.draft.goal.as_deref(), Some("Edited by hand"));
        assert!(!form.dirty());
        app.say("Edited instructions.md.".into());
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.message.as_deref(), Some("Edited instructions.md."));
        assert_eq!(app.status, None);

        // Unreadable: the board's copy, and the form says so.
        let mut app = sample();
        app.on_key(ch('s'));
        let Screen::Settings(form) = &app.screen else {
            panic!("settings")
        };
        assert_eq!(form.draft.goal.as_deref(), Some("Ship it"));
        assert!(form.message.as_deref().unwrap().contains("re-read"));
    }

    #[test]
    fn help_and_settings_screens_open_and_close() {
        let mut app = sample();
        app.on_key(ch('?'));
        assert_eq!(app.screen, Screen::Help);
        assert!(app.on_key(ch('q')).is_empty(), "q closes help first");
        assert_eq!(app.screen, Screen::Board);
        app.on_key(ch('s'));
        assert!(matches!(app.screen, Screen::Settings(_)));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Board);
    }

    #[test]
    fn refresh_and_pr_messages_feed_the_status_line() {
        let mut app = sample();
        app.apply(Msg::RefreshFailed("db locked".into()));
        assert_eq!(app.threads.len(), 5, "rows kept");
        assert_eq!(app.refresh_error.as_deref(), Some("db locked"));
        app.apply(Msg::Pr(PrSummary {
            hint: Some("install gh".into()),
            cleaned: vec!["done-1".into()],
            blocked: 1,
            errors: 2,
        }));
        assert_eq!(app.pr_hint.as_deref(), Some("install gh"));
        assert!(app.status.as_deref().unwrap().contains("done-1"));
        // The hint comes once; later refreshes keep it.
        app.apply(Msg::Pr(PrSummary::default()));
        assert_eq!(app.pr_hint.as_deref(), Some("install gh"));
        assert_eq!(app.pr.errors, 0);
    }
}
