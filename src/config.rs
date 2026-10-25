//! `project.toml` (ARCHITECTURE §3).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    pub name: String,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub coordinator: ModelSettings,
    pub threads: ThreadSettings,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub ui: UiSettings,
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
            },
            repos,
            ui: UiSettings::default(),
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

    /// Validates, then writes through a temp file so readers never see a half-written file.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Rejects unknown efforts and permission modes, duplicate repo names, a default_repo
    /// that names no repo, and max_parallel = 0.
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
        assert_eq!(c.threads.default_repo.as_deref(), Some("payments-api"));
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
        assert!(!dir.path().join("sub/project.toml.tmp").exists());

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
    }

    #[test]
    fn validate_rejects() {
        type Edit = fn(&mut ProjectConfig);
        let cases: [(&str, Edit); 10] = [
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
        ];
        for (name, edit) in cases {
            let mut c = sample();
            edit(&mut c);
            assert!(c.validate().is_err(), "{name} should be rejected");
        }
        let mut c = sample();
        c.threads.default_repo = None;
        c.validate().unwrap();
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
