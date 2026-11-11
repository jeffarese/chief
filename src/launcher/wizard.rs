//! The new-project wizard (§10a): four short steps, then the project is created and its
//! coordinator opened. Pure state; the launcher runs what it returns.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::EFFORTS;

/// Models offered by ←/→; any other name can be set later in the board's settings.
pub const MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    About,
    Code,
    Models,
    Review,
}

impl Step {
    pub const ALL: [Step; 4] = [Step::About, Step::Code, Step::Models, Step::Review];

    pub fn label(self) -> &'static str {
        match self {
            Step::About => "About",
            Step::Code => "Code",
            Step::Models => "Models",
            Step::Review => "Review",
        }
    }

    fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// The quick settings (step 3), in screen order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    CoordinatorModel,
    CoordinatorEffort,
    ThreadModel,
    ThreadEffort,
    /// Whether threads run through agp. Only offered when agp is installed.
    Runner,
}

impl Setting {
    pub const ALL: [Setting; 5] = [
        Setting::CoordinatorModel,
        Setting::CoordinatorEffort,
        Setting::ThreadModel,
        Setting::ThreadEffort,
        Setting::Runner,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Setting::CoordinatorModel => "Coordinator model",
            Setting::CoordinatorEffort => "Coordinator effort",
            Setting::ThreadModel => "Thread model",
            Setting::ThreadEffort => "Thread effort",
            Setting::Runner => "Threads run through",
        }
    }
}

/// An agp pool account a new project's threads could run in (§7 "Thread account").
#[derive(Debug, Clone, PartialEq)]
pub struct PoolAccount {
    /// What agp shows for it, e.g. `PB2`.
    pub label: String,
    /// The file that records which folders it trusts.
    pub trust: PathBuf,
}

/// A repo an account doesn't trust yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Untrusted {
    pub folder: PathBuf,
    /// The pool account's label; None is the project's own account.
    pub account: Option<String>,
}

/// What the wizard collected, for the launcher to create.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub name: String,
    pub goal: Option<String>,
    pub repos: Vec<PathBuf>,
    pub coordinator_model: String,
    pub coordinator_effort: String,
    pub thread_model: String,
    pub thread_effort: String,
    /// Threads run through agp when it is installed (the `[threads]` defaults); false keeps
    /// them in the project's account, started by plain `claude`.
    pub agp: bool,
    /// The user agreed to mark the untrusted repos trusted.
    pub trust: bool,
}

/// Checks the wizard needs from the outside world, so tests can answer them.
pub trait Probe {
    /// Some(why) when a project can't take this slug (taken, or it can't be checked).
    fn slug_problem(&self, slug: &str) -> Option<String>;
    /// Whether an account trusts `folder` already: the pool account whose trust file is
    /// `account`, or the project's own for None.
    fn trusted(&self, account: Option<&Path>, folder: &Path) -> bool;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Stay,
    /// Esc on the first step.
    Cancel,
    Create(Box<Draft>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wizard {
    pub step: Step,
    pub name: String,
    pub goal: String,
    /// Step 1: the goal field has the cursor (else the name).
    pub on_goal: bool,
    pub repos: Vec<PathBuf>,
    /// Step 2: the folder being typed.
    pub path: String,
    /// Step 2: the selected repo; None is the path input.
    pub repo_cursor: Option<usize>,
    pub setting: usize,
    pub coordinator_model: String,
    pub coordinator_effort: String,
    pub thread_model: String,
    pub thread_effort: String,
    /// agp's pool accounts but the project's own, once the launcher has asked agp. None
    /// without agp (or before it answered): step 3 then has nothing to ask about it.
    pub agp: Option<Vec<PoolAccount>>,
    /// Step 3: threads run through agp. Only shown, and only changed, with `agp`.
    pub use_agp: bool,
    /// Step 4: the repos not trusted yet, in the project's account and, with agp, in each
    /// pool account.
    pub untrusted: Vec<Untrusted>,
    pub trust: bool,
    /// An error or a hint for the current step.
    pub message: Option<String>,
}

impl Wizard {
    /// Starts with `cwd`'s repository listed when it is in one: the usual case is running chief
    /// from the code the project is about.
    pub fn new(cwd: Option<&Path>) -> Self {
        let defaults = crate::config::ProjectConfig::new("", "", None, Vec::new());
        Wizard {
            step: Step::About,
            name: String::new(),
            goal: String::new(),
            on_goal: false,
            repos: cwd.and_then(repo_root).into_iter().collect(),
            path: String::new(),
            repo_cursor: None,
            setting: 0,
            coordinator_model: defaults.coordinator.model,
            coordinator_effort: defaults.coordinator.effort,
            thread_model: defaults.threads.model,
            thread_effort: defaults.threads.effort,
            agp: None,
            use_agp: true,
            untrusted: Vec::new(),
            trust: true,
            message: None,
        }
    }

    pub fn slug(&self) -> String {
        crate::project::slug_of(&self.name)
    }

    pub fn number(&self) -> usize {
        self.step.index() + 1
    }

    /// Step 3's rows: the last asks about agp, so it shows only once agp is found.
    pub fn settings(&self) -> &'static [Setting] {
        let all = &Setting::ALL;
        match self.agp {
            Some(_) => all,
            None => &all[..all.len() - 1],
        }
    }

    /// The selected row of step 3.
    pub fn selected(&self) -> Setting {
        let shown = self.settings();
        shown[self.setting.min(shown.len() - 1)]
    }

    pub fn value(&self, s: Setting) -> &str {
        match s {
            Setting::CoordinatorModel => &self.coordinator_model,
            Setting::CoordinatorEffort => &self.coordinator_effort,
            Setting::ThreadModel => &self.thread_model,
            Setting::ThreadEffort => &self.thread_effort,
            Setting::Runner if self.use_agp => "agp",
            Setting::Runner => "claude",
        }
    }

    /// The line under step 3's rows.
    pub fn help(&self, s: Setting) -> String {
        match s {
            Setting::CoordinatorModel => "The session you talk to: it plans and delegates.".into(),
            Setting::CoordinatorEffort => "Low keeps the conversation quick.".into(),
            Setting::ThreadModel => "Each background thread that does the work.".into(),
            Setting::ThreadEffort => "How hard each thread thinks.".into(),
            Setting::Runner if !self.use_agp => {
                "Plain claude in this account. Nothing watches threads for usage limits.".into()
            }
            Setting::Runner => match self.pool_labels() {
                Some(labels) => format!(
                    "agp starts each thread in the account with the most room and moves it at \
                     a usage limit. Besides this account: {labels}."
                ),
                None => "agp watches each thread and resumes it once a usage limit resets.".into(),
            },
        }
    }

    /// The pool accounts threads would also run in, `PB2, PB3`; None without any.
    pub fn pool_labels(&self) -> Option<String> {
        let labels: Vec<&str> = self
            .agp
            .iter()
            .flatten()
            .map(|a| a.label.as_str())
            .collect();
        (!labels.is_empty()).then(|| labels.join(", "))
    }

    /// What the launcher learnt from agp (§10a): its pool, or None when it isn't installed.
    pub fn set_agp(&mut self, agp: Option<Vec<PoolAccount>>, probe: &dyn Probe) {
        self.agp = agp;
        self.find_untrusted(probe);
    }

    /// Step 4's list: every repo in the project's account, then in each pool account threads
    /// would run in.
    fn find_untrusted(&mut self, probe: &dyn Probe) {
        let mut found: Vec<Untrusted> = self
            .repos
            .iter()
            .filter(|r| !probe.trusted(None, r))
            .map(|r| Untrusted {
                folder: r.clone(),
                account: None,
            })
            .collect();
        if self.use_agp {
            for account in self.agp.iter().flatten() {
                for repo in &self.repos {
                    if !probe.trusted(Some(&account.trust), repo) {
                        found.push(Untrusted {
                            folder: repo.clone(),
                            account: Some(account.label.clone()),
                        });
                    }
                }
            }
        }
        self.untrusted = found;
    }

    pub fn on_key(&mut self, key: KeyEvent, probe: &dyn Probe) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            return self.back();
        }
        if key.code == KeyCode::BackTab {
            return self.back();
        }
        match self.step {
            Step::About => self.about_key(key, ctrl, probe),
            Step::Code => self.code_key(key, ctrl, probe),
            Step::Models => self.models_key(key, probe),
            Step::Review => match key.code {
                KeyCode::Char(' ') | KeyCode::Char('t') if !self.untrusted.is_empty() => {
                    self.trust = !self.trust;
                    Outcome::Stay
                }
                KeyCode::Enter => Outcome::Create(Box::new(self.draft())),
                _ => Outcome::Stay,
            },
        }
    }

    fn back(&mut self) -> Outcome {
        self.message = None;
        match self.step {
            Step::About => Outcome::Cancel,
            Step::Code => {
                self.step = Step::About;
                Outcome::Stay
            }
            Step::Models => {
                self.step = Step::Code;
                Outcome::Stay
            }
            Step::Review => {
                self.step = Step::Models;
                Outcome::Stay
            }
        }
    }

    fn about_key(&mut self, key: KeyEvent, ctrl: bool, probe: &dyn Probe) -> Outcome {
        self.message = None;
        match key.code {
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => self.on_goal = !self.on_goal,
            KeyCode::Enter if !self.on_goal => self.on_goal = true,
            KeyCode::Enter => {
                let slug = self.slug();
                if self.name.trim().is_empty() {
                    self.on_goal = false;
                    self.message = Some("Give the project a name first.".to_string());
                } else if slug.is_empty() {
                    self.on_goal = false;
                    self.message = Some("The name needs at least one letter or digit.".to_string());
                } else if let Some(why) = probe.slug_problem(&slug) {
                    self.on_goal = false;
                    self.message = Some(why);
                } else {
                    self.step = Step::Code;
                }
            }
            _ => {
                let field = if self.on_goal {
                    &mut self.goal
                } else {
                    &mut self.name
                };
                edit(field, key, ctrl);
            }
        }
        Outcome::Stay
    }

    fn code_key(&mut self, key: KeyEvent, ctrl: bool, probe: &dyn Probe) -> Outcome {
        if let Some(i) = self.repo_cursor {
            match key.code {
                KeyCode::Up => self.repo_cursor = Some(i.saturating_sub(1)),
                KeyCode::Down | KeyCode::Tab => {
                    self.repo_cursor = (i + 1 < self.repos.len()).then_some(i + 1)
                }
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                    let gone = self.repos.remove(i);
                    self.message = Some(format!("Removed {}.", crate::tui::path(&gone, 60)));
                    self.repo_cursor = match self.repos.len() {
                        0 => None,
                        n => Some(i.min(n - 1)),
                    };
                }
                KeyCode::Enter => return self.next_from_code(probe),
                _ => {}
            }
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Up if !self.repos.is_empty() => {
                self.repo_cursor = Some(self.repos.len() - 1);
            }
            KeyCode::Tab => match complete(&self.path) {
                Ok(longer) => self.path = longer,
                Err(why) => self.message = Some(why),
            },
            KeyCode::Enter if self.path.trim().is_empty() => return self.next_from_code(probe),
            KeyCode::Enter => self.add_path(),
            _ => {
                self.message = None;
                edit(&mut self.path, key, ctrl);
            }
        }
        Outcome::Stay
    }

    fn add_path(&mut self) {
        let path = crate::tui::expand_home(self.path.trim());
        if !path.is_dir() {
            self.message = Some(format!("{} isn't a folder.", path.display()));
            return;
        }
        let path = path.canonicalize().unwrap_or(path);
        if self.repos.contains(&path) {
            self.message = Some(format!(
                "{} is listed already.",
                crate::tui::path(&path, 60)
            ));
        } else {
            self.message = Some(format!("Added {}.", crate::tui::path(&path, 60)));
            self.repos.push(path);
        }
        self.path.clear();
    }

    fn next_from_code(&mut self, probe: &dyn Probe) -> Outcome {
        self.message = None;
        self.step = Step::Models;
        self.find_untrusted(probe);
        Outcome::Stay
    }

    fn models_key(&mut self, key: KeyEvent, probe: &dyn Probe) -> Outcome {
        let last = self.settings().len() - 1;
        self.setting = self.setting.min(last);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.setting = self.setting.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.setting = (self.setting + 1).min(last)
            }
            KeyCode::Left | KeyCode::Char('h') => self.cycle(-1, probe),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => self.cycle(1, probe),
            KeyCode::Enter => self.step = Step::Review,
            _ => {}
        }
        Outcome::Stay
    }

    fn cycle(&mut self, step: isize, probe: &dyn Probe) {
        let pick = |options: &[&str], current: &str| {
            let n = options.len() as isize;
            let i = options
                .iter()
                .position(|o| *o == current)
                .map_or(0, |i| (i as isize + step).rem_euclid(n) as usize);
            options[i].to_string()
        };
        match self.selected() {
            Setting::CoordinatorModel => {
                self.coordinator_model = pick(&MODELS, &self.coordinator_model)
            }
            Setting::CoordinatorEffort => {
                self.coordinator_effort = pick(&EFFORTS, &self.coordinator_effort)
            }
            Setting::ThreadModel => self.thread_model = pick(&MODELS, &self.thread_model),
            Setting::ThreadEffort => self.thread_effort = pick(&EFFORTS, &self.thread_effort),
            // Which accounts' trust the review lists depends on it.
            Setting::Runner => {
                self.use_agp = !self.use_agp;
                self.find_untrusted(probe);
            }
        }
    }

    pub fn draft(&self) -> Draft {
        let goal = self.goal.trim();
        Draft {
            name: self.name.trim().to_string(),
            goal: (!goal.is_empty()).then(|| goal.to_string()),
            repos: self.repos.clone(),
            coordinator_model: self.coordinator_model.clone(),
            coordinator_effort: self.coordinator_effort.clone(),
            thread_model: self.thread_model.clone(),
            thread_effort: self.thread_effort.clone(),
            agp: self.use_agp,
            trust: self.trust && !self.untrusted.is_empty(),
        }
    }
}

/// Typing into a one-line field: characters, Backspace, ^U clears, ^W drops the last word.
fn edit(field: &mut String, key: KeyEvent, ctrl: bool) {
    match key.code {
        KeyCode::Backspace => {
            field.pop();
        }
        KeyCode::Char('u') if ctrl => field.clear(),
        KeyCode::Char('w') if ctrl => {
            // A completed folder ends in `/`: the word is the folder name before it.
            let text = field.trim_end();
            let text = text.strip_suffix('/').unwrap_or(text);
            let kept = text.rfind([' ', '/']).map_or(0, |i| i + 1);
            field.truncate(kept);
        }
        KeyCode::Char(c) if !ctrl => field.push(c),
        _ => {}
    }
}

/// The git repository `dir` is in (the folder holding `.git`), canonical.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Tab completion of the last folder name in `text`: the one folder that matches, with a `/`,
/// or the longest prefix the matches share. Otherwise, what to say: nothing matches, or how
/// many do when they share nothing longer.
pub fn complete(text: &str) -> Result<String, String> {
    if text == "~" {
        return Ok("~/".to_string());
    }
    let (head, prefix) = match text.rfind('/') {
        Some(i) => (&text[..=i], &text[i + 1..]),
        None => ("", text),
    };
    let dir = if head.is_empty() {
        PathBuf::from(".")
    } else {
        crate::tui::expand_home(head)
    };
    let none = || "No folder matches.".to_string();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|_| none())?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.')))
        .collect();
    names.sort();
    let first = names.first().ok_or_else(none)?;
    if names.len() == 1 {
        return Ok(format!("{head}{first}/"));
    }
    let shared = names.iter().fold(first.clone(), |acc, n| {
        acc.chars()
            .zip(n.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect()
    });
    if shared.len() > prefix.len() {
        Ok(format!("{head}{shared}"))
    } else {
        Err(format!(
            "{} folders match: type more of the name.",
            names.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        taken: Vec<String>,
        trusted: Vec<PathBuf>,
    }

    impl Probe for Fake {
        fn slug_problem(&self, slug: &str) -> Option<String> {
            self.taken
                .iter()
                .any(|t| t == slug)
                .then(|| format!("A project called {slug} exists already."))
        }
        fn trusted(&self, account: Option<&Path>, folder: &Path) -> bool {
            // The pool accounts trust nothing yet.
            account.is_none() && self.trusted.iter().any(|t| t == folder)
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(w: &mut Wizard, text: &str, probe: &dyn Probe) {
        for c in text.chars() {
            w.on_key(key(KeyCode::Char(c)), probe);
        }
    }

    #[test]
    fn four_steps_to_a_draft() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let other = home.path().join("web");
        std::fs::create_dir_all(&other).unwrap();
        let repo = repo.canonicalize().unwrap();
        let other = other.canonicalize().unwrap();
        let probe = Fake {
            taken: vec!["docs".into()],
            trusted: vec![repo.clone()],
        };

        // Started inside the repo: it is listed already.
        let mut w = Wizard::new(Some(&repo.join("src")));
        assert_eq!(w.repos, std::slice::from_ref(&repo));

        // 1. A name is required, and its slug must be free.
        w.on_key(key(KeyCode::Enter), &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::About);
        assert_eq!(w.message.as_deref(), Some("Give the project a name first."));
        typed(&mut w, "Docs", &probe);
        w.on_key(key(KeyCode::Tab), &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        assert!(w.message.as_deref().unwrap().contains("exists already"));
        assert!(!w.on_goal, "back on the name");
        w.on_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &probe,
        );
        typed(&mut w, "Payments Latency", &probe);
        assert_eq!(w.slug(), "payments-latency");
        w.on_key(key(KeyCode::Enter), &probe);
        assert!(w.on_goal);
        typed(&mut w, "Hold p95 under 200 ms", &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::Code);

        // 2. Add a folder by path; a bad one says why; ↑ selects a repo, d removes it.
        typed(&mut w, "/nowhere/at/all", &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        assert!(w.message.as_deref().unwrap().contains("isn't a folder"));
        w.on_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &probe,
        );
        typed(&mut w, &format!("{}/we", home.path().display()), &probe);
        w.on_key(key(KeyCode::Tab), &probe);
        assert!(w.path.ends_with("/web/"), "{:?}", w.path);
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.repos, [repo.clone(), other.clone()]);
        w.on_key(key(KeyCode::Up), &probe);
        assert_eq!(w.repo_cursor, Some(1));
        w.on_key(key(KeyCode::Up), &probe);
        w.on_key(key(KeyCode::Char('d')), &probe);
        assert_eq!(w.repos, std::slice::from_ref(&other));
        // Esc goes back a step and keeps what was typed.
        w.on_key(key(KeyCode::Esc), &probe);
        assert_eq!(w.step, Step::About);
        assert_eq!(w.name, "Payments Latency");
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::Code);
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::Models);
        let own = |folder: &PathBuf| Untrusted {
            folder: folder.clone(),
            account: None,
        };
        assert_eq!(w.untrusted, [own(&other)]);

        // 3. ←→ cycle; ↓ moves.
        assert_eq!(w.coordinator_model, "opus");
        w.on_key(key(KeyCode::Left), &probe);
        assert_eq!(w.coordinator_model, "fable");
        w.on_key(key(KeyCode::Right), &probe);
        w.on_key(key(KeyCode::Right), &probe);
        assert_eq!(w.coordinator_model, "sonnet");
        w.on_key(key(KeyCode::Down), &probe);
        w.on_key(key(KeyCode::Left), &probe);
        assert_eq!(w.coordinator_effort, "max");
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::Review);

        // 4. Space toggles the trust; Enter creates.
        w.on_key(key(KeyCode::Char(' ')), &probe);
        assert!(!w.trust);
        w.on_key(key(KeyCode::Char(' ')), &probe);
        let Outcome::Create(draft) = w.on_key(key(KeyCode::Enter), &probe) else {
            panic!("creates");
        };
        assert_eq!(
            *draft,
            Draft {
                name: "Payments Latency".into(),
                goal: Some("Hold p95 under 200 ms".into()),
                repos: vec![other],
                coordinator_model: "sonnet".into(),
                coordinator_effort: "max".into(),
                thread_model: "sonnet".into(),
                thread_effort: "high".into(),
                agp: true,
                trust: true,
            }
        );
        // Esc from the first step cancels.
        let mut w = Wizard::new(None);
        assert!(w.repos.is_empty());
        assert_eq!(w.on_key(key(KeyCode::Esc), &probe), Outcome::Cancel);
    }

    /// Step 3 asks about agp only once agp is found; the review then lists the pool accounts'
    /// untrusted repos too, and not when threads stay with plain claude.
    #[test]
    fn agp_is_a_choice_once_it_is_found() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let repo = repo.canonicalize().unwrap();
        let probe = Fake {
            taken: Vec::new(),
            trusted: vec![repo.clone()],
        };
        let mut w = Wizard::new(Some(&repo));
        typed(&mut w, "Docs", &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        w.on_key(key(KeyCode::Enter), &probe);
        assert_eq!(w.step, Step::Models);

        // No agp (or no answer yet): four rows, nothing about it, and the defaults stay.
        assert_eq!(w.settings().len(), 4);
        for _ in 0..6 {
            w.on_key(key(KeyCode::Down), &probe);
        }
        assert_eq!(w.selected(), Setting::ThreadEffort);
        assert!(w.untrusted.is_empty());
        assert!(w.draft().agp);

        // agp answers while the step is open: a fifth row, and its accounts' trust.
        let pb2 = PoolAccount {
            label: "PB2".into(),
            trust: PathBuf::from("/pb2/.claude.json"),
        };
        w.set_agp(Some(vec![pb2]), &probe);
        assert_eq!(w.settings().len(), 5);
        w.on_key(key(KeyCode::Down), &probe);
        assert_eq!(w.selected(), Setting::Runner);
        assert_eq!(w.value(Setting::Runner), "agp");
        assert!(
            w.help(Setting::Runner)
                .ends_with("Besides this account: PB2.")
        );
        let in_pb2 = Untrusted {
            folder: repo.clone(),
            account: Some("PB2".into()),
        };
        assert_eq!(w.untrusted, std::slice::from_ref(&in_pb2));
        assert!(w.draft().trust, "the box covers the pool accounts");

        // ←/→ switch to plain claude and back: nothing of the pool's to trust then.
        w.on_key(key(KeyCode::Right), &probe);
        assert_eq!(w.value(Setting::Runner), "claude");
        assert!(w.untrusted.is_empty());
        let draft = w.draft();
        assert!(!draft.agp && !draft.trust);
        w.on_key(key(KeyCode::Left), &probe);
        assert_eq!(w.untrusted, [in_pb2]);

        // An agp with no other account still watches threads.
        w.set_agp(Some(Vec::new()), &probe);
        assert_eq!(w.pool_labels(), None);
        assert!(
            w.help(Setting::Runner)
                .starts_with("agp watches each thread")
        );
        assert!(w.untrusted.is_empty());

        // agp gone (a wizard reopened after a failure): the row goes, the cursor stays valid.
        w.set_agp(None, &probe);
        assert_eq!(w.selected(), Setting::ThreadEffort);
        w.on_key(key(KeyCode::Right), &probe);
        assert_eq!(w.step, Step::Models);
    }

    #[test]
    fn completion_and_editing() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["alpha", "alps", "beta", ".hidden"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        std::fs::write(dir.path().join("alpine.txt"), "").unwrap();
        let base = format!("{}/", dir.path().display());
        assert_eq!(complete(&format!("{base}al")), Ok(format!("{base}alp")));
        assert_eq!(
            complete(&format!("{base}alph")),
            Ok(format!("{base}alpha/"))
        );
        assert_eq!(
            complete(&format!("{base}alp")),
            Err("2 folders match: type more of the name.".into()),
            "nothing to add"
        );
        assert_eq!(
            complete(&format!("{base}z")),
            Err("No folder matches.".into())
        );
        assert_eq!(
            complete(&format!("{base}.h")),
            Ok(format!("{base}.hidden/"))
        );
        assert_eq!(complete("~"), Ok("~/".into()));

        let mut text = "cd /some/path".to_string();
        edit(
            &mut text,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            true,
        );
        assert_eq!(text, "cd /some/");
        edit(
            &mut text,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            false,
        );
        assert_eq!(text, "cd /some");

        // After a completion the text ends in `/`: ^W drops the folder name before it.
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let mut text = "~/code/".to_string();
        edit(&mut text, ctrl_w, true);
        assert_eq!(text, "~/");
        edit(&mut text, ctrl_w, true);
        assert_eq!(text, "");
    }
}
