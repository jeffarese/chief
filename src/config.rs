//! `project.toml` (ARCHITECTURE §3).

use std::path::{Path, PathBuf};

use anyhow::Result;
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
        let _ = (name, slug, goal, repos);
        todo!("M1")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let _ = path;
        todo!("M1")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let _ = path;
        todo!("M1")
    }

    /// Rejects unknown efforts and permission modes, duplicate repo names, a default_repo
    /// that names no repo, and max_parallel = 0.
    pub fn validate(&self) -> Result<()> {
        todo!("M1")
    }
}
