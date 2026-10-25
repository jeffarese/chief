//! Where chief keeps its files (ARCHITECTURE §3).

use std::path::PathBuf;

/// `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`.
pub fn root() -> PathBuf {
    if let Some(home) = std::env::var_os("CHIEF_HOME") {
        return PathBuf::from(home);
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("chief");
    }
    dirs::home_dir()
        .expect("no home directory")
        .join(".local/share/chief")
}

pub fn db() -> PathBuf {
    root().join("chief.db")
}

pub fn hook_error_log() -> PathBuf {
    root().join("logs/hook-errors.log")
}

pub fn project_dir(slug: &str) -> PathBuf {
    root().join("projects").join(slug)
}
