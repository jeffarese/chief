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
}

/// What the worker sends back.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Note(Note),
    /// The project exists now: a later failure leaves it in the list, not in the wizard.
    Created(String),
    Done(Result<Where, String>),
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

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    List,
    Wizard(Box<Wizard>),
    Starting(Box<Starting>),
    Help,
}

/// What a click lands on, recorded by the last draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A list row: a project, or `projects.len()` for "New project".
    Row(usize),
}

pub struct Launcher {
    pub projects: Vec<Entry>,
    /// Over the projects, then "New project" last.
    pub cursor: usize,
    pub screen: Screen,
    pub status: Option<String>,
    /// Where a new project's repo list starts (the launcher's own folder).
    pub cwd: Option<std::path::PathBuf>,
    pub hits: RefCell<Vec<(u16, Hit)>>,
    /// The first frame's wizard, when there are no projects: Esc from it quits.
    first_run: bool,
}

impl Launcher {
    /// With no projects, straight to the wizard.
    pub fn new(projects: Vec<Entry>, cwd: Option<std::path::PathBuf>, new: bool) -> Self {
        let first_run = projects.is_empty();
        let screen = if new || first_run {
            Screen::Wizard(Box::new(Wizard::new(cwd.as_deref())))
        } else {
            Screen::List
        };
        Launcher {
            projects,
            cursor: 0,
            screen,
            status: None,
            cwd,
            hits: RefCell::new(Vec::new()),
            first_run,
        }
    }

    pub fn on_new_row(&self) -> bool {
        self.cursor >= self.projects.len()
    }

    pub fn set_projects(&mut self, projects: Vec<Entry>, select: Option<&str>) {
        self.projects = projects;
        if let Some(slug) = select
            && let Some(i) = self.projects.iter().position(|p| p.slug == slug)
        {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(self.projects.len());
    }

    pub fn apply(&mut self, msg: Msg) {
        let Screen::Starting(s) = &mut self.screen else {
            return;
        };
        match msg {
            Msg::Note(note) => s.notes.push(note),
            Msg::Created(slug) => {
                s.slug = slug;
                s.wizard = None;
            }
            Msg::Done(result) => s.done = Some(result),
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
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.status = None;
        let last = self.projects.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return vec![Action::Quit],
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('n') => self.new_project(),
            KeyCode::Enter if self.on_new_row() => self.new_project(),
            KeyCode::Enter => return self.open(),
            _ => {}
        }
        Vec::new()
    }

    fn new_project(&mut self) {
        self.screen = Screen::Wizard(Box::new(Wizard::new(self.cwd.as_deref())));
    }

    fn open(&mut self) -> Vec<Action> {
        let Some(p) = self.projects.get(self.cursor) else {
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

    /// In the list a click selects (a click on the selection opens it) and the wheel moves;
    /// elsewhere a click on help closes it.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Vec<Action> {
        match (&self.screen, ev.kind) {
            (Screen::Help, MouseEventKind::Down(_)) => self.screen = Screen::List,
            (Screen::List, MouseEventKind::ScrollUp) => self.cursor = self.cursor.saturating_sub(1),
            (Screen::List, MouseEventKind::ScrollDown) => {
                self.cursor = (self.cursor + 1).min(self.projects.len())
            }
            (Screen::List, MouseEventKind::Down(MouseButton::Left)) => {
                let hit = self
                    .hits
                    .borrow()
                    .iter()
                    .find(|(y, _)| *y == ev.row)
                    .map(|(_, h)| *h);
                if let Some(Hit::Row(i)) = hit {
                    if i == self.cursor {
                        return self.list_key(KeyEvent::from(KeyCode::Enter));
                    }
                    self.cursor = i;
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
