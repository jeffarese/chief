//! The settings screen (`s`): a form over `project.toml`. Edits stay in a draft until Save, which
//! validates it and hands it to the runner to write and regenerate `generated/*`.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Cleanup, EFFORTS, ProjectConfig, THREAD_PERMISSION_MODES};
use crate::placement::{PoolTrust, Untrusted};

use super::app::Action;

/// The project's sessions, job state and folder trust live in its account's folder (§3).
const ACCOUNT_FIXED: &str = "The Claude Code account can't be changed: the project's sessions live \
     in it. It is the CLAUDE_CONFIG_DIR chief init ran with.";

pub const SAVED_MESSAGE: &str = "Saved. New threads use these settings; the coordinator's model and effort apply on its next \
     start.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The project's Claude Code account: shown, never edited (§3).
    Account,
    Goal,
    CoordinatorModel,
    CoordinatorEffort,
    ThreadModel,
    ThreadEffort,
    PermissionMode,
    MaxParallel,
    /// `[threads] accounts`: the agp accounts threads may run in (§3).
    ThreadAccounts,
    MaxUsage,
    DefaultRepo,
    Cleanup,
    Notify,
    /// Checks every pool account's trust of the repos.
    PoolTrust,
    /// A (pool account, repo) pair the last check found untrusted.
    Untrusted(usize),
    Repo(usize),
    AddRepo,
    EditInstructions,
    EditMemory,
    Save,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Account => "Claude Code account",
            Field::Goal => "Goal",
            Field::CoordinatorModel => "Coordinator model",
            Field::CoordinatorEffort => "Coordinator effort",
            Field::ThreadModel => "Thread model",
            Field::ThreadEffort => "Thread effort",
            Field::PermissionMode => "Permission mode",
            Field::MaxParallel => "Max parallel",
            Field::ThreadAccounts => "Thread accounts",
            Field::MaxUsage => "Max usage",
            Field::DefaultRepo => "Default repo",
            Field::Cleanup => "Cleanup",
            Field::Notify => "Notifications",
            Field::PoolTrust => "Account trust",
            Field::Untrusted(_) => "Untrusted",
            Field::Repo(_) => "Repo",
            Field::AddRepo => "+ Add repo (path)",
            Field::EditInstructions => "Edit instructions.md",
            Field::EditMemory => "Edit memory/MEMORY.md",
            Field::Save => "Save",
        }
    }

    /// The heading the field sits under on the screen.
    pub fn section(self) -> &'static str {
        match self {
            Field::Goal
            | Field::Repo(_)
            | Field::AddRepo
            | Field::DefaultRepo
            | Field::EditInstructions
            | Field::EditMemory
            | Field::Notify => "Project",
            Field::CoordinatorModel | Field::CoordinatorEffort => "Coordinator",
            Field::ThreadModel
            | Field::ThreadEffort
            | Field::PermissionMode
            | Field::MaxParallel
            | Field::Cleanup => "Threads",
            Field::Account
            | Field::ThreadAccounts
            | Field::MaxUsage
            | Field::PoolTrust
            | Field::Untrusted(_) => "Accounts",
            Field::Save => "",
        }
    }

    /// One line on what the field does, shown under the form for the field under the cursor.
    pub fn help(self) -> &'static str {
        match self {
            Field::Goal => "What the project is for. The coordinator and every thread read it.",
            Field::Repo(_) => "A repository threads work in. d removes it.",
            Field::AddRepo => "Add a repository folder by path (~ works).",
            Field::DefaultRepo => "Where a thread works when the coordinator names no repo.",
            Field::EditInstructions => {
                "The standing brief every session gets: conventions, commands, constraints."
            }
            Field::EditMemory => "What the coordinator remembers across conversations.",
            Field::Notify => "macOS notifications when a thread needs you or finishes.",
            Field::CoordinatorModel => {
                "Model of the session you talk to. Applies on its next start."
            }
            Field::CoordinatorEffort => {
                "How hard the coordinator thinks. Applies on its next start."
            }
            Field::ThreadModel => "Model of each new thread.",
            Field::ThreadEffort => "How hard each new thread thinks.",
            Field::PermissionMode => "What new threads may do without asking you.",
            Field::MaxParallel => "How many threads the coordinator runs at once (a guideline).",
            Field::Cleanup => {
                "merged: remove a thread's worktree and session once its PR merges. manual: x does."
            }
            Field::Account => "The Claude Code account the project runs in. Fixed at creation.",
            Field::ThreadAccounts => {
                "agp accounts threads may run in: auto = the whole pool, empty = this account only."
            }
            Field::MaxUsage => "A pool account at or over this usage takes no new thread.",
            Field::PoolTrust => "Check that every pool account trusts every repo.",
            Field::Untrusted(_) => {
                "Enter trusts this repo in that account, as Claude's prompt would."
            }
            Field::Save => "Write project.toml. Ctrl+S saves from anywhere.",
        }
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            Field::Goal
                | Field::CoordinatorModel
                | Field::ThreadModel
                | Field::MaxParallel
                | Field::ThreadAccounts
                | Field::MaxUsage
                | Field::AddRepo
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Stay,
    Close,
    Run(Action),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    pub draft: ProjectConfig,
    saved: ProjectConfig,
    dir: PathBuf,
    pub cursor: usize,
    /// The field being typed into and its text. The field is kept with the text: the cursor can
    /// move under it (a trust check changes the list), and Enter must apply it where it was typed.
    pub editing: Option<(Field, String)>,
    pub message: Option<String>,
    /// Esc was pressed once with unsaved changes.
    leaving: bool,
    pub trust: TrustCheck,
}

/// Where the pool's trust check stands.
#[derive(Debug, Clone, PartialEq)]
pub enum TrustCheck {
    NotChecked,
    /// Running; the last result's untrusted pairs stay listed, so the cursor doesn't land on
    /// another field (Save) while the list is gone.
    Checking(Option<PoolTrust>),
    Done(PoolTrust),
    Failed(String),
}

impl SettingsForm {
    pub fn new(config: &ProjectConfig, dir: PathBuf) -> Self {
        SettingsForm {
            draft: config.clone(),
            saved: config.clone(),
            dir,
            cursor: 0,
            editing: None,
            message: None,
            leaving: false,
            trust: TrustCheck::NotChecked,
        }
    }

    fn untrusted(&self) -> &[Untrusted] {
        match &self.trust {
            TrustCheck::Done(found) | TrustCheck::Checking(Some(found)) => &found.untrusted,
            _ => &[],
        }
    }

    /// The text being typed, if a field is being edited.
    pub fn edit_text(&self) -> Option<&str> {
        self.editing.as_ref().map(|(_, text)| text.as_str())
    }

    /// Starts a check, keeping the last result's pairs listed while it runs.
    fn start_check(&mut self) {
        let last = match std::mem::replace(&mut self.trust, TrustCheck::NotChecked) {
            TrustCheck::Done(found) | TrustCheck::Checking(Some(found)) => Some(found),
            _ => None,
        };
        self.trust = TrustCheck::Checking(last);
    }

    /// The pool's trust check finished.
    pub fn trust_checked(&mut self, result: Result<PoolTrust, String>) {
        self.trust = match result {
            Ok(found) => {
                self.message = (!found.errors.is_empty())
                    .then(|| format!("Couldn't check {}.", found.errors.join("; ")));
                TrustCheck::Done(found)
            }
            Err(e) => TrustCheck::Failed(e),
        };
        self.cursor = self.cursor.min(self.fields().len() - 1);
    }

    /// Marking a pair trusted went as `text` says, and the pool was checked again.
    pub fn trust_marked(&mut self, text: String, check: Result<PoolTrust, String>) {
        self.trust_checked(check);
        if self.message.is_none() {
            self.message = Some(text);
        }
    }

    /// In screen order: by section, Save last.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Goal];
        fields.extend((0..self.draft.repos.len()).map(Field::Repo));
        fields.extend([
            Field::AddRepo,
            Field::DefaultRepo,
            Field::EditInstructions,
            Field::EditMemory,
            Field::Notify,
            Field::CoordinatorModel,
            Field::CoordinatorEffort,
            Field::ThreadModel,
            Field::ThreadEffort,
            Field::PermissionMode,
            Field::MaxParallel,
            Field::Cleanup,
            Field::Account,
            Field::ThreadAccounts,
            Field::MaxUsage,
            Field::PoolTrust,
        ]);
        fields.extend((0..self.untrusted().len()).map(Field::Untrusted));
        fields.push(Field::Save);
        fields
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.fields().len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        self.show_fix();
    }

    pub fn current(&self) -> Field {
        let fields = self.fields();
        fields[self.cursor.min(fields.len() - 1)]
    }

    pub fn dirty(&self) -> bool {
        self.draft != self.saved
    }

    /// The field's value as shown.
    pub fn value(&self, field: Field) -> String {
        let c = &self.draft;
        match field {
            Field::Account => crate::claude::account_label(c.claude_config_dir.as_deref()),
            Field::Goal => c.goal.clone().unwrap_or_default(),
            Field::CoordinatorModel => c.coordinator.model.clone(),
            Field::CoordinatorEffort => c.coordinator.effort.clone(),
            Field::ThreadModel => c.threads.model.clone(),
            Field::ThreadEffort => c.threads.effort.clone(),
            Field::PermissionMode => c.threads.permission_mode.clone(),
            Field::MaxParallel => c.threads.max_parallel.to_string(),
            Field::ThreadAccounts if c.threads.accounts.is_empty() => {
                "(none: the project's account)".to_string()
            }
            Field::ThreadAccounts => c.threads.accounts.join(", "),
            Field::MaxUsage => match c.threads.max_usage {
                Some(max) => format!("{max}%"),
                None => "(none)".to_string(),
            },
            Field::PoolTrust => match &self.trust {
                TrustCheck::NotChecked => "Enter checks the pool".to_string(),
                TrustCheck::Checking(_) => "checking…".to_string(),
                TrustCheck::Failed(e) => format!("check failed: {e}"),
                TrustCheck::Done(found) if found.checked.is_empty() => {
                    "no agp pool: threads use the project's account".to_string()
                }
                TrustCheck::Done(found) if found.untrusted.is_empty() => {
                    format!("trusted in {}", found.checked.join(", "))
                }
                TrustCheck::Done(found) => format!(
                    "{} untrusted · checked {}",
                    found.untrusted.len(),
                    found.checked.join(", ")
                ),
            },
            Field::Untrusted(i) => self
                .untrusted()
                .get(i)
                .map(|u| {
                    let repo = c.repos.iter().find(|r| r.path == u.folder);
                    match repo {
                        Some(r) => format!("{} · {}", u.label, r.name),
                        None => format!("{} · {}", u.label, u.folder.display()),
                    }
                })
                .unwrap_or_default(),
            Field::DefaultRepo => c
                .threads
                .default_repo
                .clone()
                .unwrap_or_else(|| "(none)".to_string()),
            Field::Cleanup => match c.threads.cleanup {
                Cleanup::Merged => "merged".to_string(),
                Cleanup::Manual => "manual".to_string(),
            },
            Field::Notify => if c.ui.notify { "on" } else { "off" }.to_string(),
            Field::Repo(i) => c
                .repos
                .get(i)
                .map(|r| {
                    format!(
                        "{} {}",
                        r.name,
                        crate::tui::home_relative(&r.path.display().to_string())
                    )
                })
                .unwrap_or_default(),
            Field::AddRepo | Field::EditInstructions | Field::EditMemory | Field::Save => {
                String::new()
            }
        }
    }

    /// project.toml now holds `config`, the draft as it was sent. Edits made while the save ran
    /// stay in the draft, unsaved.
    pub fn saved(&mut self, config: &ProjectConfig, warning: Option<String>) {
        self.saved = config.clone();
        self.leaving = false;
        self.message = Some(match warning {
            Some(w) => format!(
                "Saved, but generated/* wasn't rewritten: {w}. Sessions start with the old \
                 settings until a save rewrites it."
            ),
            None if self.dirty() => {
                "Saved what you had when you saved; later changes aren't saved yet.".to_string()
            }
            None => SAVED_MESSAGE.to_string(),
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        if self.editing.is_some() {
            return self.edit_key(key);
        }
        let field = self.current();
        if !matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.leaving = false;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.activate(Field::Save);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.dirty() && !self.leaving {
                    self.leaving = true;
                    self.message = Some(
                        "Unsaved changes: Esc again throws them away; Save keeps them.".to_string(),
                    );
                } else {
                    return Outcome::Close;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Left | KeyCode::Char('h') => self.cycle(field, -1),
            KeyCode::Right | KeyCode::Char('l') => self.cycle(field, 1),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let Field::Repo(i) = field {
                    self.remove_repo(i);
                }
            }
            KeyCode::Enter => return self.activate(field),
            _ => {}
        }
        Outcome::Stay
    }

    /// On an untrusted pair, the message says what Enter runs.
    fn show_fix(&mut self) {
        if let Field::Untrusted(i) = self.current()
            && let Some(u) = self.untrusted().get(i)
        {
            self.message = Some(format!(
                "Enter trusts {} in {}, as accepting Claude's trust prompt there would.",
                u.folder.display(),
                u.label
            ));
        }
    }

    fn activate(&mut self, field: Field) -> Outcome {
        self.message = None;
        if field.is_text() {
            let start = match field {
                Field::AddRepo => String::new(),
                Field::ThreadAccounts => self.draft.threads.accounts.join(", "),
                Field::MaxUsage => self
                    .draft
                    .threads
                    .max_usage
                    .map(|m| m.to_string())
                    .unwrap_or_default(),
                _ => self.value(field),
            };
            self.editing = Some((field, start));
            return Outcome::Stay;
        }
        let checking = matches!(self.trust, TrustCheck::Checking(_));
        match field {
            Field::PoolTrust | Field::Untrusted(_) if checking => {
                self.message = Some("Wait: the trust check is still running.".to_string());
                Outcome::Stay
            }
            Field::PoolTrust => {
                self.start_check();
                Outcome::Run(Action::CheckTrust(Box::new(self.draft.clone())))
            }
            Field::Untrusted(i) => match self.untrusted().get(i).cloned() {
                Some(u) => {
                    self.message = Some(format!("Trusting {} in {}…", u.folder.display(), u.label));
                    self.start_check();
                    Outcome::Run(Action::Trust(Box::new(u), Box::new(self.draft.clone())))
                }
                None => Outcome::Stay,
            },
            Field::EditInstructions => Outcome::Run(Action::Edit(self.dir.join("instructions.md"))),
            Field::EditMemory => Outcome::Run(Action::Edit(self.dir.join("memory/MEMORY.md"))),
            Field::Save => match self.draft.validate() {
                Ok(()) => Outcome::Run(Action::SaveSettings(Box::new(self.draft.clone()))),
                Err(e) => {
                    self.message = Some(format!("Not saved: {e:#}"));
                    Outcome::Stay
                }
            },
            Field::Repo(_) => {
                self.message = Some("d removes this repo.".to_string());
                Outcome::Stay
            }
            _ => {
                self.cycle(field, 1);
                Outcome::Stay
            }
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> Outcome {
        let Some((_, text)) = self.editing.as_mut() else {
            return Outcome::Stay;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.editing = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if ctrl => text.clear(),
            // Ctrl+S saves from any field (§10): what is typed goes in first.
            KeyCode::Char('s') if ctrl => {
                if self.apply_edit() {
                    return self.activate(Field::Save);
                }
            }
            KeyCode::Char(c) if !ctrl => text.push(c),
            KeyCode::Enter => {
                self.apply_edit();
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Puts the typed text in the field it was typed into. On a bad value the edit stays open
    /// with the reason, and this returns false.
    fn apply_edit(&mut self) -> bool {
        let Some((field, text)) = self.editing.take() else {
            return true;
        };
        match self.set_text(field, &text) {
            Ok(()) => true,
            Err(e) => {
                self.message = Some(e);
                self.editing = Some((field, text));
                false
            }
        }
    }

    fn set_text(&mut self, field: Field, text: &str) -> Result<(), String> {
        let text = text.trim();
        let c = &mut self.draft;
        match field {
            Field::Goal => c.goal = (!text.is_empty()).then(|| text.to_string()),
            Field::CoordinatorModel => c.coordinator.model = text.to_string(),
            Field::ThreadModel => c.threads.model = text.to_string(),
            Field::MaxParallel => {
                c.threads.max_parallel = text.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                    format!("Max parallel must be a whole number ≥ 1, not {text:?}.")
                })?
            }
            Field::ThreadAccounts => {
                c.threads.accounts = text
                    .split(|ch: char| ch == ',' || ch.is_whitespace())
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            Field::MaxUsage if text.is_empty() => c.threads.max_usage = None,
            Field::MaxUsage => {
                let max = text.trim_end_matches('%').trim();
                c.threads.max_usage = Some(
                    max.parse()
                        .ok()
                        .filter(|n| (1..=100).contains(n))
                        .ok_or_else(|| {
                            format!("Max usage must be a percent from 1 to 100, not {text:?}.")
                        })?,
                )
            }
            Field::AddRepo => {
                if text.is_empty() {
                    return Ok(());
                }
                let path = crate::tui::expand_home(text);
                let (name, added) =
                    crate::project::add_repo(&mut c.repos, &path).map_err(|e| format!("{e:#}"))?;
                if c.threads.default_repo.is_none() {
                    c.threads.default_repo = Some(name.clone());
                }
                self.message = Some(if added {
                    format!("Added repo {name}. Save to keep it.")
                } else {
                    format!("{} is already listed as {name}.", path.display())
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn cycle(&mut self, field: Field, step: isize) {
        if field == Field::Account {
            self.message = Some(ACCOUNT_FIXED.to_string());
            return;
        }
        let c = &mut self.draft;
        match field {
            Field::CoordinatorEffort => {
                c.coordinator.effort = next(&EFFORTS, &c.coordinator.effort, step)
            }
            Field::ThreadEffort => c.threads.effort = next(&EFFORTS, &c.threads.effort, step),
            Field::PermissionMode => {
                c.threads.permission_mode =
                    next(&THREAD_PERMISSION_MODES, &c.threads.permission_mode, step)
            }
            Field::MaxParallel => {
                let n = c.threads.max_parallel as isize + step;
                c.threads.max_parallel = n.max(1) as u32;
            }
            Field::DefaultRepo => {
                let names: Vec<&str> = c.repos.iter().map(|r| r.name.as_str()).collect();
                if !names.is_empty() {
                    let current = c.threads.default_repo.clone().unwrap_or_default();
                    c.threads.default_repo = Some(next(&names, &current, step));
                }
            }
            Field::Cleanup => {
                c.threads.cleanup = match c.threads.cleanup {
                    Cleanup::Merged => Cleanup::Manual,
                    Cleanup::Manual => Cleanup::Merged,
                }
            }
            Field::Notify => c.ui.notify = !c.ui.notify,
            _ => {}
        }
    }

    fn remove_repo(&mut self, i: usize) {
        let c = &mut self.draft;
        if i >= c.repos.len() {
            return;
        }
        let removed = c.repos.remove(i);
        if c.threads.default_repo.as_deref() == Some(removed.name.as_str()) {
            c.threads.default_repo = c.repos.first().map(|r| r.name.clone());
        }
        self.cursor = self.cursor.min(self.fields().len() - 1);
        self.message = Some(format!("Removed repo {}. Save to keep it.", removed.name));
    }
}

/// The value `step` places after `current` in `options`, wrapping; an unknown value starts at the
/// first option.
fn next(options: &[&str], current: &str, step: isize) -> String {
    let n = options.len() as isize;
    let i = match options.iter().position(|o| *o == current) {
        Some(i) => (i as isize + step).rem_euclid(n),
        None => 0,
    };
    options[i as usize].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::app::tests::{ch, key};
    use crate::config::Repo;

    fn form() -> SettingsForm {
        let config = ProjectConfig::new(
            "Demo",
            "demo",
            None,
            vec![
                Repo {
                    name: "api".into(),
                    path: "/r/api".into(),
                },
                Repo {
                    name: "web".into(),
                    path: "/r/web".into(),
                },
            ],
        );
        SettingsForm::new(&config, PathBuf::from("/p/demo"))
    }

    fn goto(f: &mut SettingsForm, field: Field) {
        f.cursor = f.fields().iter().position(|x| *x == field).unwrap();
    }

    fn type_text(f: &mut SettingsForm, text: &str) {
        for c in text.chars() {
            f.on_key(ch(c));
        }
    }

    /// The account is shown, never edited: the project's sessions, job state and trust live in
    /// that account's folder.
    #[test]
    fn the_account_is_shown_read_only() {
        let mut f = form();
        goto(&mut f, Field::Account);
        assert_eq!(f.value(Field::Account), "default (~/.claude)");
        for k in [key(KeyCode::Enter), key(KeyCode::Right), key(KeyCode::Left)] {
            assert_eq!(f.on_key(k), Outcome::Stay);
        }
        assert!(f.editing.is_none());
        assert!(!f.dirty());
        assert!(
            f.message.as_deref().unwrap().contains("can't be changed"),
            "{:?}",
            f.message
        );

        let mut config = f.draft.clone();
        config.claude_config_dir = Some("/Users/me/.claude-carlos".into());
        let f = SettingsForm::new(&config, PathBuf::from("/p/demo"));
        assert_eq!(f.value(Field::Account), "/Users/me/.claude-carlos");
    }

    /// The thread accounts and max usage are free text (§3 project.toml); "Account trust"
    /// checks every pool account against the draft's repos, lists each pair not trusted yet,
    /// and Enter on one runs its fix on this terminal.
    #[test]
    fn thread_accounts_and_their_trust() {
        let mut f = form();
        assert_eq!(f.value(Field::ThreadAccounts), "auto");
        assert_eq!(f.value(Field::MaxUsage), "(none)");
        goto(&mut f, Field::ThreadAccounts);
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.edit_text(), Some("auto"));
        f.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut f, "cpb, cpe pb5");
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.threads.accounts, ["cpb", "cpe", "pb5"]);
        f.on_key(key(KeyCode::Enter));
        f.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        f.on_key(key(KeyCode::Enter));
        assert!(f.draft.threads.accounts.is_empty());
        assert_eq!(
            f.value(Field::ThreadAccounts),
            "(none: the project's account)"
        );
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.edit_text(), Some(""));
        type_text(&mut f, "auto");
        f.on_key(key(KeyCode::Enter));

        goto(&mut f, Field::MaxUsage);
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.edit_text(), Some(""));
        type_text(&mut f, "101");
        f.on_key(key(KeyCode::Enter));
        assert!(f.message.as_deref().unwrap().contains("1 to 100"));
        f.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut f, "80");
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.threads.max_usage, Some(80));
        assert_eq!(f.value(Field::MaxUsage), "80%");

        goto(&mut f, Field::PoolTrust);
        assert_eq!(f.value(Field::PoolTrust), "Enter checks the pool");
        let Outcome::Run(Action::CheckTrust(draft)) = f.on_key(key(KeyCode::Enter)) else {
            panic!("checks")
        };
        assert_eq!(*draft, f.draft, "the draft's repos and accounts");
        assert_eq!(f.value(Field::PoolTrust), "checking…");

        let pair = |label: &str, folder: &str| Untrusted {
            id: label.to_lowercase(),
            label: label.into(),
            dir: Some(format!("/acc/{label}").into()),
            folder: folder.into(),
        };
        f.trust_checked(Ok(PoolTrust {
            checked: vec!["PB2".into(), "PB3".into()],
            untrusted: vec![pair("PB2", "/r/web"), pair("PB3", "/elsewhere")],
            errors: vec!["PB5: agp exec failed".into()],
        }));
        assert_eq!(f.value(Field::PoolTrust), "2 untrusted · checked PB2, PB3");
        assert!(
            f.message
                .as_deref()
                .unwrap()
                .contains("PB5: agp exec failed")
        );
        let at = f
            .fields()
            .iter()
            .position(|x| *x == Field::PoolTrust)
            .unwrap();
        assert_eq!(
            f.fields()[at + 1..at + 3],
            [Field::Untrusted(0), Field::Untrusted(1)]
        );
        assert_eq!(f.value(Field::Untrusted(0)), "PB2 · web");
        assert_eq!(f.value(Field::Untrusted(1)), "PB3 · /elsewhere");
        f.on_key(key(KeyCode::Down));
        assert_eq!(f.current(), Field::Untrusted(0));
        assert_eq!(
            f.message.as_deref(),
            Some("Enter trusts /r/web in PB2, as accepting Claude's trust prompt there would.")
        );
        // Enter is the yes: the runner marks it on a worker, then checks again.
        assert_eq!(
            f.on_key(key(KeyCode::Enter)),
            Outcome::Run(Action::Trust(
                Box::new(pair("PB2", "/r/web")),
                Box::new(f.draft.clone())
            ))
        );
        assert_eq!(f.value(Field::PoolTrust), "checking…");
        f.trust_marked(
            "Trusted /r/web (in PB2).".into(),
            Ok(PoolTrust {
                checked: vec!["PB2".into()],
                ..PoolTrust::default()
            }),
        );
        assert_eq!(f.message.as_deref(), Some("Trusted /r/web (in PB2)."));
        assert_eq!(f.value(Field::PoolTrust), "trusted in PB2");
        assert!(!f.fields().contains(&Field::Untrusted(0)));
        assert!(f.cursor < f.fields().len());
        f.trust_checked(Ok(PoolTrust::default()));
        assert_eq!(
            f.value(Field::PoolTrust),
            "no agp pool: threads use the project's account"
        );
        f.trust_checked(Err("agp status failed".into()));
        assert_eq!(f.value(Field::PoolTrust), "check failed: agp status failed");
    }

    /// While a check runs, the untrusted pairs stay listed and the cursor stays on its pair: a
    /// second Enter waits instead of landing on Save.
    #[test]
    fn enter_on_an_untrusted_pair_while_checking() {
        let mut f = form();
        let pair = Untrusted {
            id: "cpb".into(),
            label: "PB2".into(),
            dir: Some("/acc/PB2".into()),
            folder: "/r/web".into(),
        };
        f.trust_checked(Ok(PoolTrust {
            checked: vec!["PB2".into()],
            untrusted: vec![pair.clone()],
            errors: Vec::new(),
        }));
        goto(&mut f, Field::Untrusted(0));
        let Outcome::Run(Action::Trust(..)) = f.on_key(key(KeyCode::Enter)) else {
            panic!("trusts")
        };
        assert_eq!(f.value(Field::PoolTrust), "checking…");
        assert_eq!(f.current(), Field::Untrusted(0));
        assert_eq!(f.value(Field::Untrusted(0)), "PB2 · web");
        assert_eq!(f.on_key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(f.message.as_deref().unwrap().contains("still running"));
        goto(&mut f, Field::PoolTrust);
        assert_eq!(
            f.on_key(key(KeyCode::Enter)),
            Outcome::Stay,
            "one check at a time"
        );
        f.trust_marked("Trusted /r/web (in PB2).".into(), Ok(PoolTrust::default()));
        assert!(!f.fields().contains(&Field::Untrusted(0)));
    }

    /// Ctrl+S in a text field applies what is typed, then saves; a bad value stays to be fixed.
    #[test]
    fn ctrl_s_saves_while_editing() {
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let mut f = form();
        goto(&mut f, Field::MaxParallel);
        f.on_key(key(KeyCode::Enter));
        f.on_key(key(KeyCode::Backspace));
        type_text(&mut f, "0");
        assert_eq!(f.on_key(ctrl_s), Outcome::Stay);
        assert_eq!(f.edit_text(), Some("0"));
        assert!(f.message.as_deref().unwrap().contains("whole number"));
        f.on_key(key(KeyCode::Backspace));
        type_text(&mut f, "7");
        let Outcome::Run(Action::SaveSettings(saved)) = f.on_key(ctrl_s) else {
            panic!("saves")
        };
        assert_eq!(saved.threads.max_parallel, 7);
        assert_eq!(f.edit_text(), None);
    }

    /// A save reports the draft that was sent: edits made while it ran stay, unsaved.
    #[test]
    fn edits_made_during_a_save_are_kept() {
        let mut f = form();
        goto(&mut f, Field::Notify);
        f.on_key(key(KeyCode::Enter));
        let Outcome::Run(Action::SaveSettings(sent)) =
            f.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("saves")
        };
        goto(&mut f, Field::Cleanup);
        f.on_key(key(KeyCode::Enter));
        f.saved(&sent, None);
        assert_eq!(f.draft.threads.cleanup, Cleanup::Manual, "kept");
        assert!(!f.draft.ui.notify);
        assert!(f.dirty());
        assert!(f.message.as_deref().unwrap().contains("later changes"));
        f.saved(&f.draft.clone(), Some("rewriting generated: denied".into()));
        assert!(!f.dirty());
        assert!(f.message.as_deref().unwrap().starts_with("Saved, but"));
    }

    #[test]
    fn cycles_efforts_modes_and_toggles() {
        let mut f = form();
        goto(&mut f, Field::ThreadEffort);
        f.on_key(key(KeyCode::Right));
        assert_eq!(f.draft.threads.effort, "xhigh");
        f.on_key(key(KeyCode::Right));
        f.on_key(key(KeyCode::Right));
        assert_eq!(f.draft.threads.effort, "low", "wraps");
        f.on_key(key(KeyCode::Left));
        assert_eq!(f.draft.threads.effort, "max");

        goto(&mut f, Field::PermissionMode);
        f.on_key(key(KeyCode::Left));
        assert_eq!(f.draft.threads.permission_mode, "plan");
        assert!(THREAD_PERMISSION_MODES.contains(&f.draft.threads.permission_mode.as_str()));

        goto(&mut f, Field::DefaultRepo);
        f.on_key(key(KeyCode::Right));
        assert_eq!(f.draft.threads.default_repo.as_deref(), Some("web"));
        goto(&mut f, Field::Cleanup);
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.threads.cleanup, Cleanup::Manual);
        goto(&mut f, Field::Notify);
        f.on_key(key(KeyCode::Right));
        assert!(!f.draft.ui.notify);
        goto(&mut f, Field::MaxParallel);
        f.on_key(key(KeyCode::Left));
        f.on_key(key(KeyCode::Left));
        f.on_key(key(KeyCode::Left));
        assert_eq!(f.draft.threads.max_parallel, 1, "never below 1");
        assert!(f.dirty());
    }

    #[test]
    fn free_text_fields_edit_in_place() {
        let mut f = form();
        goto(&mut f, Field::Goal);
        f.on_key(key(KeyCode::Enter));
        type_text(&mut f, "Ship q1 jk");
        assert_eq!(f.on_key(key(KeyCode::Enter)), Outcome::Stay);
        assert_eq!(
            f.draft.goal.as_deref(),
            Some("Ship q1 jk"),
            "q and j are text here"
        );
        f.on_key(key(KeyCode::Enter));
        f.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.goal, None, "an empty goal is no goal");

        goto(&mut f, Field::CoordinatorModel);
        f.on_key(key(KeyCode::Enter));
        type_text(&mut f, "-x");
        f.on_key(key(KeyCode::Esc));
        assert_eq!(f.draft.coordinator.model, "opus", "Esc cancels the edit");
        f.on_key(key(KeyCode::Enter));
        f.on_key(key(KeyCode::Backspace));
        f.on_key(key(KeyCode::Backspace));
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.coordinator.model, "op");

        goto(&mut f, Field::MaxParallel);
        f.on_key(key(KeyCode::Enter));
        f.on_key(key(KeyCode::Backspace));
        type_text(&mut f, "0");
        f.on_key(key(KeyCode::Enter));
        assert!(f.message.as_deref().unwrap().contains("whole number ≥ 1"));
        assert_eq!(f.edit_text(), Some("0"), "stays editing to fix it");
        f.on_key(key(KeyCode::Backspace));
        type_text(&mut f, "5");
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.threads.max_parallel, 5);
    }

    #[test]
    fn save_validates_first() {
        let mut f = form();
        goto(&mut f, Field::ThreadModel);
        f.on_key(key(KeyCode::Enter));
        f.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        f.on_key(key(KeyCode::Enter));
        goto(&mut f, Field::Save);
        assert_eq!(f.on_key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(
            f.message
                .as_deref()
                .unwrap()
                .contains("threads.model is empty")
        );

        goto(&mut f, Field::ThreadModel);
        f.on_key(key(KeyCode::Enter));
        type_text(&mut f, "haiku");
        f.on_key(key(KeyCode::Enter));
        goto(&mut f, Field::Save);
        let Outcome::Run(Action::SaveSettings(saved)) = f.on_key(key(KeyCode::Enter)) else {
            panic!("save runs")
        };
        assert_eq!(saved.threads.model, "haiku");
        f.saved(&saved, None);
        assert!(!f.dirty());
        assert_eq!(f.message.as_deref(), Some(SAVED_MESSAGE));
    }

    #[test]
    fn repos_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api");
        std::fs::create_dir(&api).unwrap();
        let mut f = form();
        goto(&mut f, Field::AddRepo);
        f.on_key(key(KeyCode::Enter));
        type_text(&mut f, api.to_str().unwrap());
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.draft.repos.len(), 3);
        assert_eq!(f.draft.repos[2].name, "api-2", "a clash gets a suffix");
        assert_eq!(f.draft.repos[2].path, api.canonicalize().unwrap());

        goto(&mut f, Field::AddRepo);
        f.on_key(key(KeyCode::Enter));
        type_text(&mut f, "/does/not/exist");
        f.on_key(key(KeyCode::Enter));
        assert!(f.message.as_deref().unwrap().contains("doesn't exist"));
        f.on_key(key(KeyCode::Esc));
        assert_eq!(f.draft.repos.len(), 3);

        // Removing the default repo moves the default to the first remaining one.
        goto(&mut f, Field::Repo(0));
        f.on_key(ch('d'));
        assert_eq!(
            f.draft
                .repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["web", "api-2"]
        );
        assert_eq!(f.draft.threads.default_repo.as_deref(), Some("web"));
        f.draft.threads.default_repo = None;
        goto(&mut f, Field::Repo(1));
        f.on_key(ch('d'));
        goto(&mut f, Field::Repo(0));
        f.on_key(ch('d'));
        assert!(f.draft.repos.is_empty());
        assert_eq!(f.draft.threads.default_repo, None);
        assert!(f.draft.validate().is_ok());
    }

    #[test]
    fn editor_actions_and_leaving() {
        let mut f = form();
        goto(&mut f, Field::EditInstructions);
        assert_eq!(
            f.on_key(key(KeyCode::Enter)),
            Outcome::Run(Action::Edit("/p/demo/instructions.md".into()))
        );
        goto(&mut f, Field::EditMemory);
        assert_eq!(
            f.on_key(key(KeyCode::Enter)),
            Outcome::Run(Action::Edit("/p/demo/memory/MEMORY.md".into()))
        );
        assert_eq!(
            f.on_key(key(KeyCode::Esc)),
            Outcome::Close,
            "nothing to lose"
        );

        goto(&mut f, Field::Notify);
        f.on_key(key(KeyCode::Enter));
        assert_eq!(f.on_key(key(KeyCode::Esc)), Outcome::Stay, "warns first");
        assert!(f.message.as_deref().unwrap().contains("Unsaved"));
        assert_eq!(f.on_key(key(KeyCode::Esc)), Outcome::Close);
    }
}
