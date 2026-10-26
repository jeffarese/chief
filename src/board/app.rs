//! Board state and key handling, without a terminal. Keys become `Action`s that `mod.rs` runs
//! (on worker threads where they shell out); results come back as `Msg`s through `apply`.

use std::cell::Cell;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::cleanup::Mode;
use crate::config::ProjectConfig;
use crate::db::SessionRow;
use crate::state::{Group, Role};
use crate::tools;

use super::settings::{self, SettingsForm};

/// Something the board asks its runner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Quit,
    /// Show this thread (None: the coordinator) in the main pane.
    Show(Option<String>),
    Wake(String),
    SetResolved {
        name: String,
        resolved: bool,
    },
    OpenUrl(String),
    Clean {
        name: String,
        mode: Mode,
    },
    SaveSettings(Box<ProjectConfig>),
    /// Open a file in `$EDITOR` on this terminal.
    Edit(PathBuf),
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
    /// A safe cleanup would lose work: ask before discarding it.
    CleanBlocked {
        name: String,
        loss: String,
    },
    Saved(Box<ProjectConfig>),
    SaveFailed(String),
}

/// One refresh: every session of the project plus what the header needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub sessions: Vec<SessionRow>,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Board,
    Help,
    Confirm(Confirm),
    Settings(Box<SettingsForm>),
}

/// A line of the thread list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item<'a> {
    Header {
        group: Group,
        count: usize,
        collapsed: bool,
    },
    Thread(&'a SessionRow),
    /// "No threads yet".
    Empty,
}

pub struct App {
    pub slug: String,
    pub dir: PathBuf,
    pub config: ProjectConfig,
    /// Threads in board order.
    pub threads: Vec<SessionRow>,
    pub coordinator: Option<SessionRow>,
    pub denials: i64,
    /// What the main pane shows; None is the coordinator.
    pub pane_target: Option<String>,
    pub selected: Option<String>,
    pub show_resolved: bool,
    /// Short terminals show the detail instead of the list while this is on.
    pub detail_open: bool,
    pub screen: Screen,
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
}

impl App {
    pub fn new(slug: &str, dir: PathBuf, config: ProjectConfig, now: i64) -> Self {
        App {
            slug: slug.to_string(),
            dir,
            config,
            threads: Vec::new(),
            coordinator: None,
            denials: 0,
            pane_target: None,
            selected: None,
            show_resolved: false,
            detail_open: false,
            screen: Screen::Board,
            status: None,
            busy: None,
            loaded: false,
            refresh_error: None,
            pr: PrSummary::default(),
            pr_hint: None,
            pr_error: None,
            now,
            scroll: Cell::new(0),
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
            Msg::CleanBlocked { name, loss } => {
                self.busy = None;
                self.screen = Screen::Confirm(Confirm {
                    name,
                    loss,
                    input: String::new(),
                });
            }
            Msg::Saved(config) => {
                if let Screen::Settings(form) = &mut self.screen {
                    form.saved(&config);
                }
                self.config = *config;
            }
            Msg::SaveFailed(e) => match &mut self.screen {
                Screen::Settings(form) => form.message = Some(e),
                _ => self.status = Some(e),
            },
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
        if self.threads.is_empty() {
            return vec![Item::Empty];
        }
        let mut items = Vec::new();
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

    pub fn selected_row(&self) -> Option<&SessionRow> {
        let name = self.selected.as_deref()?;
        self.visible().into_iter().find(|r| r.name == name)
    }

    /// Keeps the selection on a visible thread: the same one if it still shows, else the first.
    fn fix_selection(&mut self) {
        let visible = self.visible();
        let keep = self
            .selected
            .as_deref()
            .is_some_and(|n| visible.iter().any(|r| r.name == n));
        if !keep {
            self.selected = visible.first().map(|r| r.name.clone());
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_deref()
            .and_then(|n| visible.iter().position(|r| r.name == n));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, visible.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(visible[next].name.clone());
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Action::Quit];
        }
        match &mut self.screen {
            Screen::Board => self.board_key(key),
            Screen::Help => {
                self.screen = Screen::Board;
                Vec::new()
            }
            Screen::Confirm(_) => self.confirm_key(key),
            Screen::Settings(form) => match form.on_key(key) {
                settings::Outcome::Stay => Vec::new(),
                settings::Outcome::Close => {
                    self.screen = Screen::Board;
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
            KeyCode::Char('s') => {
                self.screen =
                    Screen::Settings(Box::new(SettingsForm::new(&self.config, self.dir.clone())))
            }
            KeyCode::Esc | KeyCode::Char('c') => {
                self.pane_target = None;
                return vec![Action::Show(None)];
            }
            KeyCode::Enter => return self.on_thread(|app, row| app.show(row)),
            KeyCode::Char('w') => return self.on_thread(|app, row| app.wake(row)),
            KeyCode::Char('r') => return self.on_thread(|app, row| app.resolve(row)),
            KeyCode::Char('o') => return self.on_thread(|app, row| app.open_pr(row)),
            KeyCode::Char('x') => return self.on_thread(|app, row| app.clean(row)),
            _ => {}
        }
        Vec::new()
    }

    /// Runs a thread key on the selection, or says there is none.
    fn on_thread(&mut self, f: impl FnOnce(&mut App, SessionRow) -> Vec<Action>) -> Vec<Action> {
        match self.selected_row().cloned() {
            Some(row) => f(self, row),
            None => {
                self.status = Some("No thread selected.".to_string());
                Vec::new()
            }
        }
    }

    fn refuse(&mut self, why: String) -> Vec<Action> {
        self.status = Some(why);
        Vec::new()
    }

    fn show(&mut self, row: SessionRow) -> Vec<Action> {
        if row.cleaned_at.is_some() {
            return self.refuse(format!(
                "{} was cleaned up: there is no session to show.",
                row.name
            ));
        }
        self.pane_target = Some(row.name.clone());
        vec![Action::Show(Some(row.name))]
    }

    fn wake(&mut self, row: SessionRow) -> Vec<Action> {
        if row.cleaned_at.is_some() {
            return self.refuse(crate::guard::cleaned_message(&row.name));
        }
        if let Some(busy) = &self.busy {
            return self.refuse(format!("Wait: {busy}"));
        }
        self.busy = Some(format!("waking {}…", row.name));
        vec![Action::Wake(row.name)]
    }

    fn resolve(&mut self, row: SessionRow) -> Vec<Action> {
        if row.resolved && row.cleaned_at.is_some() {
            return self.refuse(crate::guard::cleaned_message(&row.name));
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
                    mode: Mode::Discard,
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
            })
            .collect();
        assert_eq!(
            shown,
            [
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
        assert_eq!(app_with(vec![]).items(), [Item::Empty]);
    }

    #[test]
    fn selection_skips_headers_and_stops_at_the_ends() {
        let mut app = sample();
        app.on_key(key(KeyCode::Up));
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
        assert_eq!(app.on_key(ch('q')), [Action::Quit]);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            [Action::Quit]
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

    #[test]
    fn no_selection_means_no_thread_actions() {
        let mut app = app_with(vec![]);
        for c in ['w', 'r', 'o', 'x'] {
            assert!(app.on_key(ch(c)).is_empty());
            assert_eq!(app.status.as_deref(), Some("No thread selected."));
        }
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn discard_needs_the_thread_name_typed() {
        let mut app = app_with(vec![thread("done-1", Group::Resolved, 3)]);
        app.on_key(ch('R'));
        app.on_key(ch('x'));
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "This loses:\n  1 commit on no remote".into(),
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
                mode: Mode::Discard
            }]
        );
        assert_eq!(app.screen, Screen::Board);
        assert!(app.busy.is_some());

        // Esc cancels.
        app.apply(Msg::CleanBlocked {
            name: "done-1".into(),
            loss: "x".into(),
        });
        assert!(app.on_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(app.screen, Screen::Board);
        assert!(app.status.as_deref().unwrap().contains("cancelled"));
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
