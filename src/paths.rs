//! Where chief keeps its files (ARCHITECTURE §3).

use std::path::PathBuf;

/// `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`; made absolute, since
/// the generated files carry it to sessions that run in other folders (§3). An empty variable
/// counts as unset: made absolute, it would be the current folder.
pub fn root() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let root = if let Some(home) = var("CHIEF_HOME") {
        PathBuf::from(home)
    } else if let Some(xdg) = var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("chief")
    } else {
        dirs::home_dir()
            .expect("no home directory")
            .join(".local/share/chief")
    };
    std::path::absolute(&root).unwrap_or(root)
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
