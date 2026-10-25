//! `chief hook <event>` (ARCHITECTURE §2, §4, §5, §6, §8).

use crate::state::Role;

/// Entry point. Reads the payload from stdin, never panics, never returns an error: failures are
/// appended to the hook error log. Prints only the guard decision or the coordinator's
/// UserPromptSubmit context.
pub fn run(event: &str, project: &str, role: Role) {
    let _ = (event, project, role);
    todo!("M1 (events and state), M2 (guard and context injection)")
}
