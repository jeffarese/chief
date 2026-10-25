//! Project lifecycle: init, up, down, status (ARCHITECTURE §2, §3, §7, §9).

use std::path::PathBuf;

use anyhow::Result;

/// Creates `<root>/projects/<slug>/` with project.toml, instructions.md, memory/MEMORY.md and
/// generated/*, registers the project, and reports any repo or project folder that Claude Code
/// doesn't trust yet (with the command to trust it).
pub fn init(name: &str, goal: Option<&str>, repos: &[PathBuf]) -> Result<()> {
    let _ = (name, goal, repos);
    todo!("M2")
}

/// Starts or respawns the coordinator, then opens the layout.
pub fn up(slug: &str) -> Result<()> {
    let _ = slug;
    todo!("M2 (coordinator), M4 (layout)")
}

pub fn down(slug: &str, stop_threads: bool) -> Result<()> {
    let _ = (slug, stop_threads);
    todo!("M4")
}

/// Prints the board as text, grouped as on the board.
pub fn status(slug: Option<&str>) -> Result<()> {
    let _ = slug;
    todo!("M2")
}
