//! The settings screen (`s`): a form over `project.toml`. Edits stay in a draft until Save, which
//! validates it and hands it to the runner to write and regenerate `generated/*`.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Cleanup, EFFORTS, ProjectConfig, THREAD_PERMISSION_MODES};

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
    DefaultRepo,
    Cleanup,
    Notify,
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
            Field::DefaultRepo => "Default repo",
            Field::Cleanup => "Cleanup",
            Field::Notify => "Notifications",
            Field::Repo(_) => "Repo",
            Field::AddRepo => "+ Add repo (path)",
            Field::EditInstructions => "Edit instructions.md",
            Field::EditMemory => "Edit memory/MEMORY.md",
            Field::Save => "Save",
        }
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            Field::Goal
                | Field::CoordinatorModel
                | Field::ThreadModel
                | Field::MaxParallel
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
    /// The text being typed into the field under the cursor.
    pub editing: Option<String>,
    pub message: Option<String>,
    /// Esc was pressed once with unsaved changes.
    leaving: bool,
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
        }
    }

    pub fn fields(&self) -> Vec<Field> {
        let mut fields = vec![
            Field::Goal,
            Field::CoordinatorModel,
            Field::CoordinatorEffort,
            Field::ThreadModel,
            Field::ThreadEffort,
            Field::PermissionMode,
            Field::MaxParallel,
            Field::DefaultRepo,
            Field::Cleanup,
            Field::Notify,
            Field::Account,
        ];
        fields.extend((0..self.draft.repos.len()).map(Field::Repo));
        fields.extend([
            Field::AddRepo,
            Field::EditInstructions,
            Field::EditMemory,
            Field::Save,
        ]);
        fields
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
                .map(|r| format!("{} {}", r.name, r.path.display()))
                .unwrap_or_default(),
            Field::AddRepo | Field::EditInstructions | Field::EditMemory | Field::Save => {
                String::new()
            }
        }
    }

    /// After a successful save.
    pub fn saved(&mut self, config: &ProjectConfig) {
        self.saved = config.clone();
        self.draft = config.clone();
        self.message = Some(SAVED_MESSAGE.to_string());
        self.leaving = false;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        if self.editing.is_some() {
            self.edit_key(key);
            return Outcome::Stay;
        }
        let field = self.current();
        if !matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.leaving = false;
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
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.fields().len() - 1)
            }
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

    fn activate(&mut self, field: Field) -> Outcome {
        self.message = None;
        if field.is_text() {
            let start = if field == Field::AddRepo {
                String::new()
            } else {
                self.value(field)
            };
            self.editing = Some(start);
            return Outcome::Stay;
        }
        match field {
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

    fn edit_key(&mut self, key: KeyEvent) {
        let Some(text) = self.editing.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.editing = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
            KeyCode::Enter => {
                let text = self.editing.take().unwrap_or_default();
                if let Err(e) = self.set_text(self.current(), &text) {
                    self.message = Some(e);
                    self.editing = Some(text);
                }
            }
            _ => {}
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
            Field::AddRepo => {
                if text.is_empty() {
                    return Ok(());
                }
                let path = expand_home(text);
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

fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(text)),
        None => Path::new(text).to_path_buf(),
    }
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
        assert_eq!(f.editing.as_deref(), Some("0"), "stays editing to fix it");
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
        f.saved(&saved);
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
