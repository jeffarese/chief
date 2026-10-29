//! Launcher state and keys, without a terminal (§10a): the project list, the wizard, and the
//! screen that shows a project starting.

use std::cell::RefCell;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::db::SessionRow;
use crate::project::{Note, Where};
use crate::state::{Group, Role};

use super::wizard::{self, Draft, Probe, Wizard};

/// A project in the list, from its project.toml and its stored rows (no `claude agents` call:
/// the list opens at once).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub slug: String,
    pub name: String,
    pub goal: Option<String>,
    /// `2 waiting · 1 working`, `no threads yet`, or why project.toml can't be read.
    pub summary: String,
    pub waiting: bool,
    /// Listed under `Archived`, hidden until A shows it.
    pub archived: bool,
}

impl Entry {
    pub fn new(
        slug: &str,
        config: Result<crate::config::ProjectConfig, String>,
        rows: &[SessionRow],
    ) -> Self {
        let (name, goal) = match &config {
            Ok(c) => (
                c.name.clone(),
                c.goal.clone().filter(|g| !g.trim().is_empty()),
            ),
            Err(_) => (slug.to_string(), None),
        };
        let threads: Vec<_> = rows
            .iter()
            .filter(|r| r.role == Role::Thread.as_str())
            .collect();
        let mut parts = Vec::new();
        for group in Group::ORDER {
            if group == Group::Resolved {
                continue;
            }
            let n = threads
                .iter()
                .filter(|r| Group::parse(&r.group_name) == Some(group))
                .count();
            if n > 0 {
                parts.push(format!("{n} {}", short_label(group)));
            }
        }
        let summary = match config {
            Err(e) => format!("can't read project.toml: {e}"),
            Ok(_) if threads.is_empty() => "no threads yet".to_string(),
            Ok(_) if parts.is_empty() => "all resolved".to_string(),
            Ok(_) => parts.join(" · "),
        };
        let waiting = threads
            .iter()
            .any(|r| Group::parse(&r.group_name) == Some(Group::Waiting));
        Entry {
            slug: slug.to_string(),
            name,
            goal,
            summary,
            waiting,
            archived: false,
        }
    }
}

fn short_label(group: Group) -> &'static str {
    match group {
        Group::Waiting => "waiting",
        Group::Working => "working",
        Group::Review => "in review",
        Group::Landing => "landing",
        Group::Idle => "idle",
        Group::Failed => "failed",
        Group::Resolved => "resolved",
    }
}

/// Something the launcher asks its runner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Quit,
    /// `chief up` the project, reporting into the Starting screen.
    Open(String),
    /// Create the project, trust what the user agreed to, then open it.
    Create(Box<Draft>),
    /// Leave the TUI and show the conversation in this terminal (no Rex, no tmux).
    Pane(String),
    /// `chief archive` the project, reporting into the Archiving screen.
    Archive(String),
    /// `chief unarchive` the project (at once: it starts nothing).
    Unarchive(String),
}

/// What the worker sends back.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Note(Note),
    /// The project exists now: a later failure leaves it in the list, not in the wizard.
    Created(String),
    Done(Result<Where, String>),
    Archived(Result<(), String>),
}

/// A project being created or opened.
#[derive(Debug, Clone, PartialEq)]
pub struct Starting {
    pub slug: String,
    pub name: String,
    pub notes: Vec<Note>,
    /// None while it runs.
    pub done: Option<Result<Where, String>>,
    /// The wizard it came from, kept until the project exists so a failure can go back to it.
    pub wizard: Option<Box<Wizard>>,
}

/// A project being archived: `down --stop-threads`'s reports, then the outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Archiving {
    pub slug: String,
    pub name: String,
    pub notes: Vec<Note>,
    /// None while it runs.
    pub done: Option<Result<(), String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    List,
    Wizard(Box<Wizard>),
    Starting(Box<Starting>),
    Help,
    /// Asks before archiving the project (slug, name).
    ConfirmArchive(String, String),
    Archiving(Box<Archiving>),
}

/// What a click lands on, recorded by the last draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A list row: a shown project, or `visible()` for "New project".
    Row(usize),
    /// The `Archived · N` header: shows or hides the archived projects.
    Archived,
}

pub struct Launcher {
    /// Live projects first, then the archived ones.
    pub projects: Vec<Entry>,
    /// Over the shown projects, then "New project" last.
    pub cursor: usize,
    pub screen: Screen,
    /// Below the list: what the last key did (Ok) or why it couldn't (Err).
    pub status: Option<Result<String, String>>,
    pub show_archived: bool,
    /// Where a new project's repo list starts (the launcher's own folder).
    pub cwd: Option<std::path::PathBuf>,
    pub hits: RefCell<Vec<(u16, Hit)>>,
    /// The first frame's wizard, when there are no projects: Esc from it quits.
    first_run: bool,
}

impl Launcher {
    /// With no projects, straight to the wizard.
    pub fn new(mut projects: Vec<Entry>, cwd: Option<std::path::PathBuf>, new: bool) -> Self {
        let first_run = projects.is_empty();
        let screen = if new || first_run {
            Screen::Wizard(Box::new(Wizard::new(cwd.as_deref())))
        } else {
            Screen::List
        };
        projects.sort_by_key(|p| p.archived);
        Launcher {
            projects,
            cursor: 0,
            screen,
            status: None,
            show_archived: false,
            cwd,
            hits: RefCell::new(Vec::new()),
            first_run,
        }
    }

    /// How many projects the list shows: the archived ones (last) only once A showed them.
    pub fn visible(&self) -> usize {
        let live = self.projects.iter().filter(|p| !p.archived).count();
        if self.show_archived {
            self.projects.len()
        } else {
            live
        }
    }

    pub fn archived_count(&self) -> usize {
        self.projects.iter().filter(|p| p.archived).count()
    }

    pub fn on_new_row(&self) -> bool {
        self.cursor >= self.visible()
    }

    /// The selected project, unless "New project" is.
    pub fn selected(&self) -> Option<&Entry> {
        self.projects[..self.visible()].get(self.cursor)
    }

    pub fn set_projects(&mut self, mut projects: Vec<Entry>, select: Option<&str>) {
        projects.sort_by_key(|p| p.archived);
        self.projects = projects;
        if let Some(slug) = select
            && let Some(i) = self.projects.iter().position(|p| p.slug == slug)
            && i < self.visible()
        {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(self.visible());
    }

    /// A shows or hides the archived projects; "New project" stays selected when it was.
    fn toggle_archived(&mut self) {
        let on_new = self.on_new_row();
        self.show_archived = !self.show_archived;
        self.cursor = if on_new {
            self.visible()
        } else {
            self.cursor.min(self.visible())
        };
    }

    pub fn apply(&mut self, msg: Msg) {
        match (&mut self.screen, msg) {
            (Screen::Starting(s), Msg::Note(note)) => s.notes.push(note),
            (Screen::Starting(s), Msg::Created(slug)) => {
                s.slug = slug;
                s.wizard = None;
            }
            (Screen::Starting(s), Msg::Done(result)) => s.done = Some(result),
            (Screen::Archiving(a), Msg::Note(note)) => a.notes.push(note),
            (Screen::Archiving(a), Msg::Archived(result)) => a.done = Some(result),
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, probe: &dyn Probe) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return vec![Action::Quit];
        }
        match &mut self.screen {
            Screen::List => self.list_key(key),
            Screen::Help => {
                self.screen = Screen::List;
                Vec::new()
            }
            Screen::Wizard(w) => match w.on_key(key, probe) {
                wizard::Outcome::Stay => Vec::new(),
                wizard::Outcome::Cancel if self.first_run && self.projects.is_empty() => {
                    vec![Action::Quit]
                }
                wizard::Outcome::Cancel => {
                    self.screen = Screen::List;
                    Vec::new()
                }
                wizard::Outcome::Create(draft) => {
                    let wizard = w.clone();
                    self.screen = Screen::Starting(Box::new(Starting {
                        slug: wizard.slug(),
                        name: draft.name.clone(),
                        notes: Vec::new(),
                        done: None,
                        wizard: Some(wizard),
                    }));
                    vec![Action::Create(draft)]
                }
            },
            Screen::Starting(s) => {
                let Some(done) = &s.done else {
                    // Nothing to do while it runs but leave.
                    return match key.code {
                        KeyCode::Char('q') => vec![Action::Quit],
                        _ => Vec::new(),
                    };
                };
                match (done, key.code) {
                    (_, KeyCode::Char('q')) => vec![Action::Quit],
                    (Ok(Where::Manual), KeyCode::Enter) => vec![Action::Pane(s.slug.clone())],
                    // A failure before the project existed goes back to the wizard as it was.
                    (Err(_), KeyCode::Esc | KeyCode::Enter) if s.wizard.is_some() => {
                        let wizard = s
                            .wizard
                            .take()
                            .unwrap_or_else(|| Box::new(Wizard::new(None)));
                        self.screen = Screen::Wizard(wizard);
                        Vec::new()
                    }
                    (_, KeyCode::Esc | KeyCode::Enter) => {
                        self.screen = Screen::List;
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            // y archives; any other key cancels.
            Screen::ConfirmArchive(slug, name) => {
                let (slug, name) = (slug.clone(), name.clone());
                if !matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.screen = Screen::List;
                    return Vec::new();
                }
                self.screen = Screen::Archiving(Box::new(Archiving {
                    slug: slug.clone(),
                    name,
                    notes: Vec::new(),
                    done: None,
                }));
                vec![Action::Archive(slug)]
            }
            Screen::Archiving(a) => match (a.done.is_some(), key.code) {
                (_, KeyCode::Char('q')) => vec![Action::Quit],
                (true, KeyCode::Esc | KeyCode::Enter) => {
                    self.screen = Screen::List;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.status = None;
        let last = self.visible();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return vec![Action::Quit],
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('n') => self.new_project(),
            KeyCode::Char('a') => return self.archive(),
            KeyCode::Char('A') if self.archived_count() > 0 => self.toggle_archived(),
            KeyCode::Enter if self.on_new_row() => self.new_project(),
            KeyCode::Enter => return self.open(),
            _ => {}
        }
        Vec::new()
    }

    fn new_project(&mut self) {
        self.screen = Screen::Wizard(Box::new(Wizard::new(self.cwd.as_deref())));
    }

    /// `a`: a live project asks first (its sessions stop); an archived one comes back at once.
    fn archive(&mut self) -> Vec<Action> {
        let Some(p) = self.selected() else {
            return Vec::new();
        };
        if p.archived {
            return vec![Action::Unarchive(p.slug.clone())];
        }
        self.screen = Screen::ConfirmArchive(p.slug.clone(), p.name.clone());
        Vec::new()
    }

    fn open(&mut self) -> Vec<Action> {
        let Some(p) = self.selected() else {
            return Vec::new();
        };
        let slug = p.slug.clone();
        self.screen = Screen::Starting(Box::new(Starting {
            slug: slug.clone(),
            name: p.name.clone(),
            notes: Vec::new(),
            done: None,
            wizard: None,
        }));
        vec![Action::Open(slug)]
    }

    /// In the list a click selects (a click on the selection opens it, one on the Archived header
    /// is A) and the wheel moves; elsewhere a click on help closes it, and one on the archive
    /// question cancels it.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Vec<Action> {
        match (&self.screen, ev.kind) {
            (Screen::Help | Screen::ConfirmArchive(..), MouseEventKind::Down(_)) => {
                self.screen = Screen::List
            }
            (Screen::List, MouseEventKind::ScrollUp) => self.cursor = self.cursor.saturating_sub(1),
            (Screen::List, MouseEventKind::ScrollDown) => {
                self.cursor = (self.cursor + 1).min(self.visible())
            }
            (Screen::List, MouseEventKind::Down(MouseButton::Left)) => {
                let hit = self
                    .hits
                    .borrow()
                    .iter()
                    .find(|(y, _)| *y == ev.row)
                    .map(|(_, h)| *h);
                match hit {
                    Some(Hit::Row(i)) if i == self.cursor => {
                        return self.list_key(KeyEvent::from(KeyCode::Enter));
                    }
                    Some(Hit::Row(i)) => self.cursor = i,
                    Some(Hit::Archived) => self.toggle_archived(),
                    None => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use std::path::Path;

    struct Free;

    impl Probe for Free {
        fn slug_problem(&self, _: &str) -> Option<String> {
            None
        }
        fn trusted(&self, _: &Path) -> bool {
            true
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn entry(slug: &str) -> Entry {
        Entry::new(
            slug,
            Ok(ProjectConfig::new(slug, slug, None, Vec::new())),
            &[],
        )
    }

    #[test]
    fn summaries_count_what_needs_you_first() {
        let thread = |group: Group| {
            let mut r = SessionRow::new("t", "p", Role::Thread, 0);
            r.group_name = group.as_str().into();
            r
        };
        let config = ProjectConfig::new("Pay", "pay", Some("Fast"), Vec::new());
        let rows = [
            thread(Group::Working),
            thread(Group::Waiting),
            thread(Group::Working),
            thread(Group::Resolved),
            SessionRow::new("chief-pay", "pay", Role::Coordinator, 0),
        ];
        let e = Entry::new("pay", Ok(config.clone()), &rows);
        assert_eq!(e.summary, "1 waiting · 2 working");
        assert!(e.waiting);
        assert_eq!(e.goal.as_deref(), Some("Fast"));
        assert_eq!(
            Entry::new("pay", Ok(config.clone()), &rows[3..]).summary,
            "all resolved"
        );
        assert_eq!(Entry::new("pay", Ok(config), &[]).summary, "no threads yet");
        let broken = Entry::new("pay", Err("bad toml".into()), &[]);
        assert_eq!(broken.name, "pay");
        assert!(broken.summary.contains("bad toml"));
    }

    #[test]
    fn no_projects_opens_the_wizard_and_esc_quits() {
        let mut l = Launcher::new(Vec::new(), None, false);
        assert!(matches!(l.screen, Screen::Wizard(_)));
        assert_eq!(l.on_key(key(KeyCode::Esc), &Free), [Action::Quit]);

        let mut l = Launcher::new(vec![entry("a")], None, true);
        assert!(matches!(l.screen, Screen::Wizard(_)));
        assert!(l.on_key(key(KeyCode::Esc), &Free).is_empty());
        assert_eq!(l.screen, Screen::List);
    }

    #[test]
    fn the_list_opens_projects_and_starts_new_ones() {
        let mut l = Launcher::new(vec![entry("a"), entry("b")], None, false);
        l.on_key(key(KeyCode::Down), &Free);
        assert_eq!(
            l.on_key(key(KeyCode::Enter), &Free),
            [Action::Open("b".into())]
        );
        let Screen::Starting(s) = &l.screen else {
            panic!("starting")
        };
        assert_eq!(s.slug, "b");
        // q leaves even while it runs; other keys wait.
        assert!(l.on_key(key(KeyCode::Esc), &Free).is_empty());
        l.apply(Msg::Note(Note::Did("Coordinator chief-b started.".into())));
        l.apply(Msg::Done(Ok(Where::Rex { shown: true })));
        assert!(l.on_key(key(KeyCode::Enter), &Free).is_empty());
        assert_eq!(l.screen, Screen::List);

        // The last row is "New project"; n works from anywhere in the list.
        l.on_key(key(KeyCode::Char('G')), &Free);
        assert!(l.on_new_row());
        l.on_key(key(KeyCode::Enter), &Free);
        assert!(matches!(l.screen, Screen::Wizard(_)));
        l.on_key(key(KeyCode::Esc), &Free);
        l.on_key(key(KeyCode::Char('n')), &Free);
        assert!(matches!(l.screen, Screen::Wizard(_)));
    }

    fn archived(slug: &str) -> Entry {
        Entry {
            archived: true,
            ..entry(slug)
        }
    }

    #[test]
    fn archived_projects_hide_behind_a_toggle() {
        // Listed by slug: the archived one goes last whatever its slug.
        let mut l = Launcher::new(vec![archived("a"), entry("b"), entry("c")], None, false);
        let slugs: Vec<_> = l.projects.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(slugs, ["b", "c", "a"]);
        assert_eq!(l.visible(), 2);
        l.on_key(key(KeyCode::Char('G')), &Free);
        assert!(l.on_new_row());
        assert_eq!(l.cursor, 2);

        // Shown, it sits above "New project", which stays selected.
        l.on_key(key(KeyCode::Char('A')), &Free);
        assert_eq!(l.visible(), 3);
        assert!(l.on_new_row());
        l.on_key(key(KeyCode::Up), &Free);
        assert_eq!(l.selected().map(|p| p.slug.as_str()), Some("a"));
        // a on an archived project brings it back at once; Enter opens it (up unarchives).
        assert_eq!(
            l.on_key(key(KeyCode::Char('a')), &Free),
            [Action::Unarchive("a".into())]
        );
        assert_eq!(l.screen, Screen::List);
        assert_eq!(
            l.on_key(key(KeyCode::Enter), &Free),
            [Action::Open("a".into())]
        );

        // Hidden again with the selection on it: the selection moves to what is shown.
        let mut l = Launcher::new(vec![archived("a"), entry("b")], None, false);
        l.on_key(key(KeyCode::Char('A')), &Free);
        l.on_key(key(KeyCode::Down), &Free);
        l.on_key(key(KeyCode::Char('A')), &Free);
        assert!(l.on_new_row());
        // A refreshed list never selects a hidden project.
        l.cursor = 0;
        l.set_projects(vec![archived("a"), entry("b")], Some("a"));
        assert_eq!(l.cursor, 0);
        // A does nothing without archived projects.
        let mut l = Launcher::new(vec![entry("b")], None, false);
        l.on_key(key(KeyCode::Char('A')), &Free);
        assert!(!l.show_archived);
    }

    #[test]
    fn archiving_asks_first_then_shows_its_reports() {
        let mut l = Launcher::new(vec![entry("a"), entry("b")], None, false);
        assert_eq!(l.on_key(key(KeyCode::Char('a')), &Free), []);
        assert_eq!(l.screen, Screen::ConfirmArchive("a".into(), "a".into()));
        // Any key but y cancels.
        assert_eq!(l.on_key(key(KeyCode::Enter), &Free), []);
        assert_eq!(l.screen, Screen::List);

        l.on_key(key(KeyCode::Char('a')), &Free);
        assert_eq!(
            l.on_key(key(KeyCode::Char('y')), &Free),
            [Action::Archive("a".into())]
        );
        let Screen::Archiving(a) = &l.screen else {
            panic!("archiving")
        };
        assert_eq!(a.slug, "a");
        // Nothing but q while it runs.
        assert_eq!(l.on_key(key(KeyCode::Enter), &Free), []);
        l.apply(Msg::Note(Note::Did("Stopped coordinator chief-a.".into())));
        l.apply(Msg::Archived(Ok(())));
        let Screen::Archiving(a) = &l.screen else {
            panic!("archiving")
        };
        assert_eq!(a.notes.len(), 1);
        assert_eq!(a.done, Some(Ok(())));
        l.on_key(key(KeyCode::Enter), &Free);
        assert_eq!(l.screen, Screen::List);

        // Nothing to archive on "New project".
        l.on_key(key(KeyCode::Char('G')), &Free);
        assert_eq!(l.on_key(key(KeyCode::Char('a')), &Free), []);
        assert_eq!(l.screen, Screen::List);
    }

    #[test]
    fn a_failed_create_goes_back_to_the_wizard_until_the_project_exists() {
        let mut l = Launcher::new(vec![entry("a")], None, true);
        for c in "New".chars() {
            l.on_key(key(KeyCode::Char(c)), &Free);
        }
        let mut actions = Vec::new();
        for _ in 0..5 {
            actions = l.on_key(key(KeyCode::Enter), &Free);
        }
        let [Action::Create(draft)] = actions.as_slice() else {
            panic!("{actions:?}")
        };
        assert_eq!(draft.name, "New");
        l.apply(Msg::Done(Err("claude is too old".into())));
        l.on_key(key(KeyCode::Esc), &Free);
        let Screen::Wizard(w) = &l.screen else {
            panic!("back in the wizard")
        };
        assert_eq!(w.name, "New");

        // Once created, a failure lands in the list; without Rex or tmux, Enter opens the pane.
        let mut l = Launcher::new(vec![entry("a")], None, false);
        l.on_key(key(KeyCode::Enter), &Free);
        l.apply(Msg::Created("a".into()));
        l.apply(Msg::Done(Ok(Where::Manual)));
        assert_eq!(
            l.on_key(key(KeyCode::Enter), &Free),
            [Action::Pane("a".into())]
        );
    }
}
