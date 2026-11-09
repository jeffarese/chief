//! `project.toml` (ARCHITECTURE §3).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    pub name: String,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// The Claude Code account the project belongs to (§1, §3): the `CLAUDE_CONFIG_DIR` in
    /// effect at `chief init`, absolute. None (no key) is the default account, `~/.claude`. Every
    /// Claude Code call for the project runs in it; it is never changed afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<PathBuf>,
    pub coordinator: ModelSettings,
    pub threads: ThreadSettings,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub ui: UiSettings,
    /// The web dashboard (§14). Left out of the file while it is off.
    #[serde(default, skip_serializing_if = "DashboardSettings::is_off")]
    pub dashboard: DashboardSettings,
}

/// `[dashboard]`: whether `chief serve` shows this project in the browser.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DashboardSettings {
    #[serde(default)]
    pub enabled: bool,
}

impl DashboardSettings {
    fn is_off(&self) -> bool {
        !self.enabled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSettings {
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadSettings {
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub max_parallel: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_repo: Option<String>,
    /// When chief removes a resolved thread's worktree and branch on its own (§12).
    #[serde(default)]
    pub cleanup: Cleanup,
    /// The agp accounts a new thread may run in (§3, §7): `["auto"]` (the default) is every
    /// account in agp's pool, else these agp profile ids (only those in the pool count). Empty,
    /// or without agp, threads run in the project's account.
    #[serde(default = "auto_accounts")]
    pub accounts: Vec<String>,
    /// A pool account whose 5-hour or weekly usage is at or over this percent takes no new
    /// thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usage: Option<u32>,
    /// What starts a thread (§3 "Threads through agp"): `agp` (the default) starts it through
    /// agp when agp is installed, which then watches it for limits; `claude` starts it with
    /// plain `claude --bg`, as before.
    #[serde(default)]
    pub runner: Runner,
    /// What agp does when a thread's account hits a limit: `switch` (the default) moves it to
    /// the pool account with the most room, `wait` goes on in the same account after the reset,
    /// `stay` only reports it.
    #[serde(default)]
    pub on_limit: OnLimit,
}

/// `threads.runner`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    #[default]
    Agp,
    Claude,
}

/// `threads.on_limit`, passed to agp as `--on-limit`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnLimit {
    #[default]
    Switch,
    Wait,
    Stay,
}

impl OnLimit {
    pub const ALL: [OnLimit; 3] = [OnLimit::Switch, OnLimit::Wait, OnLimit::Stay];

    pub fn as_str(self) -> &'static str {
        match self {
            OnLimit::Switch => "switch",
            OnLimit::Wait => "wait",
            OnLimit::Stay => "stay",
        }
    }
}

impl Runner {
    pub fn as_str(self) -> &'static str {
        match self {
            Runner::Agp => "agp",
            Runner::Claude => "claude",
        }
    }
}

fn auto_accounts() -> Vec<String> {
    vec![crate::agp::AUTO.to_string()]
}

/// `threads.cleanup`: `merged` cleans a thread once its PR merged (never discarding work);
/// `manual` leaves it to `chief clean`, `chief gc` and the board.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Cleanup {
    #[default]
    Merged,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiSettings {
    pub notify: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings { notify: true }
    }
}

pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Characters a permission rule reads as glob or syntax: a repo path holding one would widen or
/// break the coordinator's deny rule for it.
const RULE_GLOB_CHARS: [char; 9] = ['(', ')', '*', '?', '[', ']', '{', '}', '\\'];
/// `bypassPermissions` is excluded: messages to such a session are held for approval.
pub const THREAD_PERMISSION_MODES: [&str; 4] = ["auto", "acceptEdits", "manual", "plan"];

impl ProjectConfig {
    /// Defaults for a new project: Opus at low effort coordinating, Sonnet at high effort working.
    pub fn new(name: &str, slug: &str, goal: Option<&str>, repos: Vec<Repo>) -> Self {
        let default_repo = repos.first().map(|r| r.name.clone());
        ProjectConfig {
            name: name.to_string(),
            slug: slug.to_string(),
            goal: goal.map(str::to_string),
            claude_config_dir: None,
            coordinator: ModelSettings {
                model: "opus".to_string(),
                effort: "low".to_string(),
            },
            threads: ThreadSettings {
                model: "sonnet".to_string(),
                effort: "high".to_string(),
                permission_mode: "auto".to_string(),
                max_parallel: 3,
                default_repo,
                cleanup: Cleanup::Merged,
                accounts: auto_accounts(),
                max_usage: None,
                runner: Runner::Agp,
                on_limit: OnLimit::Switch,
            },
            repos,
            ui: UiSettings::default(),
            dashboard: DashboardSettings::default(),
        }
    }

    /// Reads and validates `project.toml`.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: ProjectConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(config)
    }

    /// Validates, then writes through a temp file (`replace_file`).
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        replace_file(path, &text)
    }

    /// Only `claude_config_dir` from a project.toml: hooks and cross-project commands find the
    /// project's account even when another key doesn't validate.
    pub fn read_account(path: &Path) -> Result<Option<PathBuf>> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        match table.get("claude_config_dir") {
            None => Ok(None),
            Some(toml::Value::String(dir)) if Path::new(dir).is_absolute() => {
                Ok(Some(PathBuf::from(dir)))
            }
            Some(other) => bail!(
                "claude_config_dir in {} must be an absolute path, not {other}",
                path.display()
            ),
        }
    }

    /// Rejects unknown efforts and permission modes, relative repo paths, duplicate repo names,
    /// a default_repo that names no repo, and max_parallel = 0.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("name is empty");
        }
        if self.slug.is_empty() || crate::names::slugify(&self.slug, self.slug.len()) != self.slug {
            bail!(
                "slug {:?} must be lowercase letters, digits and single hyphens",
                self.slug
            );
        }
        if let Some(dir) = &self.claude_config_dir
            && !dir.is_absolute()
        {
            bail!(
                "claude_config_dir {} must be an absolute path",
                dir.display()
            );
        }
        for (what, m) in [
            ("coordinator", &self.coordinator.model),
            ("threads", &self.threads.model),
        ] {
            if m.trim().is_empty() {
                bail!("{what}.model is empty");
            }
        }
        for (what, e) in [
            ("coordinator", &self.coordinator.effort),
            ("threads", &self.threads.effort),
        ] {
            if !EFFORTS.contains(&e.as_str()) {
                bail!("{what}.effort {e:?} is not one of {}", EFFORTS.join(", "));
            }
        }
        let mode = &self.threads.permission_mode;
        if !THREAD_PERMISSION_MODES.contains(&mode.as_str()) {
            bail!(
                "threads.permission_mode {mode:?} is not one of {}",
                THREAD_PERMISSION_MODES.join(", ")
            );
        }
        if self.threads.max_parallel == 0 {
            bail!("threads.max_parallel must be at least 1");
        }
        if let Some(id) = self
            .threads
            .accounts
            .iter()
            .find(|id| id.trim().is_empty() || id.chars().any(char::is_whitespace))
        {
            bail!("threads.accounts entry {id:?} is not an agp account id");
        }
        if let Some(max) = self.threads.max_usage
            && !(1..=100).contains(&max)
        {
            bail!("threads.max_usage {max} must be a percent from 1 to 100");
        }
        for repo in &self.repos {
            // A relative path means one folder in the coordinator's deny rule (resolved where
            // the files are generated) and another in its `--add-dir` (resolved from its cwd).
            if !repo.path.is_absolute() {
                bail!("the repo path {} must be absolute", repo.path.display());
            }
            // Each repo becomes a permission-rule glob for the coordinator (§3).
            if let Some(c) = repo
                .path
                .to_string_lossy()
                .chars()
                .find(|c| RULE_GLOB_CHARS.contains(c))
            {
                bail!(
                    "the repo path {} contains {c:?}, which chief can't put in a permission rule \
                     safely; rename or move the folder",
                    repo.path.display()
                );
            }
        }
        let mut seen = std::collections::HashSet::new();
        for repo in &self.repos {
            if repo.name.trim().is_empty() {
                bail!("a repo at {} has an empty name", repo.path.display());
            }
            if !seen.insert(repo.name.as_str()) {
                bail!("repo name {:?} is used twice", repo.name);
            }
        }
        if let Some(default) = &self.threads.default_repo
            && !seen.contains(default.as_str())
        {
            bail!("threads.default_repo {default:?} names no repo");
        }
        Ok(())
    }

    pub fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name == name)
    }
}

/// Writes `path` through a temp file of this write's own in the same folder, renamed over it:
/// readers never see half a file, and writers at once (`chief up`, the pane's respawn,
/// `start_thread` in the MCP server) never share a temp file, so none renames another's half
/// written one or loses its own. The temp file is removed when the write fails.
pub fn replace_file(path: &Path, contents: &str) -> Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{name}.{}-{n}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProjectConfig {
        let mut c = ProjectConfig::new(
            "Payments latency",
            "payments-latency",
            Some("Hold p95 under 200 ms"),
            vec![
                Repo {
                    name: "payments-api".into(),
                    path: "/work/payments-api".into(),
                },
                Repo {
                    name: "web".into(),
                    path: "/work/web".into(),
                },
            ],
        );
        c.ui.notify = false;
        c
    }

    #[test]
    fn defaults() {
        let c = sample();
        assert_eq!(c.coordinator.model, "opus");
        assert_eq!(c.coordinator.effort, "low");
        assert_eq!(c.threads.model, "sonnet");
        assert_eq!(c.threads.effort, "high");
        assert_eq!(c.threads.permission_mode, "auto");
        assert_eq!(c.threads.max_parallel, 3);
        assert_eq!(c.threads.cleanup, Cleanup::Merged);
        assert_eq!(c.threads.default_repo.as_deref(), Some("payments-api"));
        assert_eq!(c.threads.accounts, ["auto"]);
        assert_eq!(c.threads.max_usage, None);
        assert_eq!(
            (c.threads.runner, c.threads.on_limit),
            (Runner::Agp, OnLimit::Switch)
        );
        assert!(
            ProjectConfig::new("x", "x", None, vec![])
                .threads
                .default_repo
                .is_none()
        );
        assert!(ProjectConfig::new("x", "x", None, vec![]).ui.notify);
        c.validate().unwrap();
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/project.toml");
        let c = sample();
        c.save(&path).unwrap();
        assert_eq!(ProjectConfig::load(&path).unwrap(), c);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[repos]]"), "{text}");
        assert_eq!(
            std::fs::read_dir(dir.path().join("sub")).unwrap().count(),
            1
        );

        let bare = ProjectConfig::new("Bare", "bare", None, vec![]);
        bare.save(&path).unwrap();
        assert_eq!(ProjectConfig::load(&path).unwrap(), bare);
    }

    #[test]
    fn loads_the_documented_example() {
        let text = r#"
name = "Payments latency"
slug = "payments-latency"
goal = "Hold p95 under 200 ms"

[coordinator]
model = "opus"
effort = "low"

[threads]
model = "sonnet"
effort = "high"
permission_mode = "auto"
max_parallel = 3
default_repo = "payments-api"

[[repos]]
name = "payments-api"
path = "/Users/me/workspace/payments-api"

[ui]
notify = true
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.toml");
        std::fs::write(&path, text).unwrap();
        let c = ProjectConfig::load(&path).unwrap();
        assert_eq!(
            c.repos[0].path,
            PathBuf::from("/Users/me/workspace/payments-api")
        );
        assert!(c.ui.notify);
        // Files from before `cleanup`, `accounts`, `runner` and `on_limit` existed get the
        // defaults.
        assert_eq!(c.threads.cleanup, Cleanup::Merged);
        assert_eq!(c.threads.accounts, ["auto"]);
        assert_eq!(c.threads.max_usage, None);
        assert_eq!(
            (c.threads.runner, c.threads.on_limit),
            (Runner::Agp, OnLimit::Switch)
        );

        let plain = text.replace(
            "max_parallel = 3\n",
            "max_parallel = 3\nrunner = \"claude\"\non_limit = \"wait\"\n",
        );
        std::fs::write(&path, plain).unwrap();
        let c = ProjectConfig::load(&path).unwrap();
        assert_eq!(
            (c.threads.runner, c.threads.on_limit),
            (Runner::Claude, OnLimit::Wait)
        );
        let bad = text.replace(
            "max_parallel = 3\n",
            "max_parallel = 3\non_limit = \"ask\"\n",
        );
        std::fs::write(&path, bad).unwrap();
        assert!(ProjectConfig::load(&path).is_err());

        let pooled = text.replace(
            "max_parallel = 3\n",
            "max_parallel = 3\naccounts = [\"cpb\", \"cpe\"]\nmax_usage = 80\n",
        );
        std::fs::write(&path, pooled).unwrap();
        let c = ProjectConfig::load(&path).unwrap();
        assert_eq!(
            (c.threads.accounts, c.threads.max_usage),
            (vec!["cpb".to_string(), "cpe".to_string()], Some(80))
        );

        let manual = text.replace(
            "max_parallel = 3\n",
            "max_parallel = 3\ncleanup = \"manual\"\n",
        );
        std::fs::write(&path, manual).unwrap();
        assert_eq!(
            ProjectConfig::load(&path).unwrap().threads.cleanup,
            Cleanup::Manual
        );
        let bad = text.replace(
            "max_parallel = 3\n",
            "max_parallel = 3\ncleanup = \"always\"\n",
        );
        std::fs::write(&path, bad).unwrap();
        let err = format!("{:#}", ProjectConfig::load(&path).unwrap_err());
        assert!(err.contains("always"), "{err}");
    }

    #[test]
    fn validate_rejects() {
        type Edit = fn(&mut ProjectConfig);
        let cases: [(&str, Edit); 15] = [
            ("empty name", |c| c.name = " ".into()),
            ("bad slug", |c| c.slug = "Payments Latency".into()),
            ("empty slug", |c| c.slug = String::new()),
            ("empty model", |c| c.threads.model = String::new()),
            ("coordinator effort", |c| {
                c.coordinator.effort = "ultra".into()
            }),
            ("thread effort", |c| c.threads.effort = "HIGH".into()),
            ("bypass", |c| {
                c.threads.permission_mode = "bypassPermissions".into()
            }),
            ("max_parallel 0", |c| c.threads.max_parallel = 0),
            ("duplicate repo", |c| {
                c.repos[1].name = "payments-api".into()
            }),
            ("unknown default repo", |c| {
                c.threads.default_repo = Some("nope".into())
            }),
            ("relative repo path", |c| {
                c.repos[0].path = "work/payments-api".into()
            }),
            ("blank account", |c| c.threads.accounts = vec![" ".into()]),
            ("account with a space", |c| {
                c.threads.accounts = vec!["c pb".into()]
            }),
            ("max_usage 0", |c| c.threads.max_usage = Some(0)),
            ("max_usage 101", |c| c.threads.max_usage = Some(101)),
        ];
        for (name, edit) in cases {
            let mut c = sample();
            edit(&mut c);
            assert!(c.validate().is_err(), "{name} should be rejected");
        }
        let mut c = sample();
        c.threads.default_repo = None;
        c.threads.accounts = vec![];
        c.threads.max_usage = Some(100);
        c.validate().unwrap();
    }

    /// A repo path becomes a permission-rule glob (§3): these characters would widen or break the
    /// coordinator's deny rule for it.
    #[test]
    fn validate_rejects_repo_paths_a_rule_cant_hold() {
        for c in ['(', ')', '*', '?', '[', ']', '{', '}', '\\'] {
            let mut config = sample();
            config.repos[0].path = PathBuf::from(format!("/Users/me/work{c}space/api"));
            let err = format!("{:#}", config.validate().unwrap_err());
            assert!(err.contains("rename or move"), "{c}: {err}");
            assert!(err.contains(&format!("{c:?}")), "{c}: {err}");
        }
        let mut config = sample();
        config.repos[0].path = PathBuf::from("/Users/me/work space/api-2.0_x");
        config.validate().unwrap();
    }

    #[test]
    fn claude_config_dir_must_be_absolute_and_reads_alone() {
        let mut config = sample();
        config.claude_config_dir = Some("relative/.claude".into());
        assert!(config.validate().is_err());
        config.claude_config_dir = Some("/Users/me/.claude-carlos".into());
        config.validate().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.toml");
        config.save(&path).unwrap();
        assert_eq!(
            ProjectConfig::read_account(&path).unwrap().as_deref(),
            Some(Path::new("/Users/me/.claude-carlos"))
        );
        // Another key broken: the account still reads.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("effort = \"high\"", "effort = \"ultra\"");
        std::fs::write(&path, text).unwrap();
        assert!(ProjectConfig::load(&path).is_err());
        assert!(ProjectConfig::read_account(&path).unwrap().is_some());
        std::fs::write(&path, "name = \"x\"\n").unwrap();
        assert_eq!(ProjectConfig::read_account(&path).unwrap(), None);
    }

    /// Writers at once (`chief up`, the pane's respawn, `start_thread`) each finish with a whole
    /// file and leave no temp file. With one shared temp name, one would rename another's half
    /// written file or find its own gone.
    #[test]
    fn writers_at_once_never_share_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let texts: Vec<String> = (0..8).map(|i| format!("{i}").repeat(64 * 1024)).collect();
        std::thread::scope(|scope| {
            for text in &texts {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        replace_file(path, text).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(texts.contains(&text), "a whole file of one writer");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        // A rename that fails (a folder in the way) leaves no temp file behind.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        assert!(replace_file(&blocked, "text").is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn load_rejects_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.toml");
        let mut c = sample();
        c.threads.max_parallel = 0;
        std::fs::write(&path, toml::to_string(&c).unwrap()).unwrap();
        assert!(ProjectConfig::load(&path).is_err());
    }
}
