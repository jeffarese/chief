//! Left pane loop (ARCHITECTURE §9): attach to `ui_state.pane_target`, re-read it after each detach.

use anyhow::Result;

pub fn run(project: &str) -> Result<()> {
    let _ = project;
    todo!("M4")
}

/// Asks the project's main pane to show `ui_state.pane_target` now. The board writes the target
/// first, then calls this. A no-op until milestone 4, which sends ctrl+z to the `main` block so
/// `chief pane` detaches and re-attaches to the new target (§9).
pub fn request_switch(project: &str) -> Result<()> {
    let _ = project;
    Ok(())
}
