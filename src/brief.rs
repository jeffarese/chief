//! Keeping a running coordinator current (ARCHITECTURE §8 "A brief that went stale"). `claude
//! respawn` starts a session again with the prompt of its first start, and its tools are the
//! chief that was installed when its process started. So after an update, or a change to the
//! project, chief tells the coordinator to read its brief as it reads now, and says when its
//! tools are still the old ones.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::config::ProjectConfig;
use crate::text::local_hhmm;
use crate::{generated, names, prompts};

/// The coordinator's brief as it reads now, for it to read: Claude Code cuts what a hook adds
/// to a session's context at about 10,000 characters, and a brief with the project's
/// instructions is longer, so the hook says to read the file instead of carrying it.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("generated/coordinator-brief.md")
}

/// A copy of the brief as the coordinator last read it, while that reading is still in its
/// context.
fn read_path(dir: &Path) -> PathBuf {
    dir.join("generated/coordinator-brief.read")
}

/// When the coordinator's process last started (unix ms), as its own SessionStart hook noted
/// it. The events table can't always say: after `/clear` a restart's SessionStart may carry a
/// session id no row has yet, and is recorded unmatched (§4).
fn started_path(dir: &Path) -> PathBuf {
    dir.join("generated/coordinator-started")
}

/// Notes that the coordinator's process started now: a SessionStart with source `startup` or
/// `resume`. `/clear` and a compaction keep the process, and its tools.
pub fn note_start(dir: &Path, source: Option<&str>, now_ms: i64) {
    if matches!(source, Some("startup" | "resume")) {
        let _ = crate::config::replace_file(&started_path(dir), &now_ms.to_string());
    }
}

/// The line that tells the coordinator to read its brief, when the one in its context isn't
/// the one chief would give it now: neither the brief its session was created with, nor the
/// one it last read. Writes the brief to `path` first. None when it is current, or the
/// project's settings can't be read.
pub fn notice(dir: &Path, config: Option<&ProjectConfig>) -> Option<String> {
    let now = prompts::coordinator(config?, dir, &prompts::read_instructions(dir));
    let has = |file: PathBuf| std::fs::read_to_string(file).ok().as_deref() == Some(now.as_str());
    if has(generated::coordinator_prompt_path(dir)) || has(read_path(dir)) {
        return None;
    }
    let file = path(dir);
    if !has(file.clone()) {
        crate::config::replace_file(&file, &now).ok()?;
    }
    Some(format!(
        "<chief-brief>\nchief was updated, or this project's settings or instructions changed, \
         since this conversation began: the brief in your system prompt is out of date. Before \
         anything else, and before you answer the user, read {} in full with one Read call. \
         It is your brief as it reads now and replaces the one you started with; don't act on \
         the old one.\n</chief-brief>",
        file.display()
    ))
}

/// The coordinator's context was emptied (a new conversation, `/clear`, a compaction): what
/// it read is gone with it.
pub fn forget_read(dir: &Path) {
    let _ = std::fs::remove_file(read_path(dir));
}

/// Notes that the coordinator read its brief, from the PostToolUse of that Read: the file as
/// it was read is kept, so `notice` is quiet until the brief changes or the context is
/// emptied.
pub fn note_read(dir: &Path, payload: &Value) {
    let field = |pointer: &str| payload.pointer(pointer).and_then(Value::as_str);
    if field("/tool_name") != Some("Read") {
        return;
    }
    let Some(read) = field("/tool_input/file_path") else {
        return;
    };
    let cwd = field("/cwd").map(Path::new).unwrap_or(dir);
    let real = |p: PathBuf| p.canonicalize().unwrap_or(p);
    let file = path(dir);
    if real(crate::lookups::absolute(read, cwd)) != real(file.clone()) {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(&file) {
        let _ = crate::config::replace_file(&read_path(dir), &text);
    }
}

/// The line that says the coordinator's tools are the old ones: its process (and the `chief
/// mcp` inside it) started before the chief binary on disk was installed, at `installed_ms`.
/// Its process's start is its last SessionStart that wasn't a `/clear` or a compaction, which
/// keep the process: the later of what its hook noted (`note_start`) and its events, which
/// are all there is for a process that started under a chief that noted nothing. None when
/// the tools are current, or nothing says when it started.
pub fn stale_tools(
    conn: &Connection,
    slug: &str,
    dir: &Path,
    installed_ms: Option<i64>,
) -> Result<Option<String>> {
    let Some(installed) = installed_ms else {
        return Ok(None);
    };
    let noted: Option<i64> = std::fs::read_to_string(started_path(dir))
        .ok()
        .and_then(|at| at.trim().parse().ok());
    let recorded: Option<i64> = conn
        .query_row(
            "SELECT ts FROM events
             WHERE session_name = ?1 AND hook = 'SessionStart' AND summary IN ('startup', 'resume')
             ORDER BY id DESC LIMIT 1",
            params![names::coordinator_name(slug)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(noted
        .max(recorded)
        .filter(|started| *started < installed)
        .map(|started| {
            format!(
                "<chief-update>\nchief was updated at {}, after your process started at {}: your \
             chief tools are still the old version, and what they can't do yet you can't do \
             either. Tell the user once that `chief down {slug} && chief up {slug}`, run in a \
             terminal, restarts you with the new tools and keeps this conversation.\n\
             </chief-update>",
                local_hhmm(installed),
                local_hhmm(started),
            )
        }))
}

/// When the chief binary that runs this hook was installed (its file's modification time, unix
/// ms).
pub fn installed_ms() -> Option<i64> {
    let modified = std::env::current_exe()
        .ok()?
        .metadata()
        .ok()?
        .modified()
        .ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> (tempfile::TempDir, PathBuf, ProjectConfig) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("projects/demo");
        std::fs::create_dir_all(dir.join("generated")).unwrap();
        (
            root,
            dir,
            ProjectConfig::new("Demo", "demo", Some("Ship it"), vec![]),
        )
    }

    fn read(dir: &Path, file: &Path) -> Value {
        json!({"tool_name": "Read", "tool_input": {"file_path": file}, "cwd": dir})
    }

    #[test]
    fn a_stale_brief_is_read_once_per_context() {
        let (_root, dir, mut config) = project();
        // A session from before chief kept its first brief: it may be anything.
        let said = notice(&dir, Some(&config)).expect("a notice");
        assert!(said.starts_with("<chief-brief>"), "{said}");
        assert!(said.contains(&path(&dir).display().to_string()), "{said}");
        // Short enough to arrive whole: the brief itself is in the file.
        assert!(said.len() < 1_000, "{}", said.len());
        let brief = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(
            brief.starts_with(
                "You are the coordinator of the chief project \"Demo\". Goal: Ship it"
            )
        );
        assert!(brief.contains("start a lookup thread"));

        // Reading another file changes nothing; reading the brief quiets the notice, by any
        // spelling of its path.
        note_read(&dir, &read(&dir, &dir.join("memory/MEMORY.md")));
        assert!(notice(&dir, Some(&config)).is_some());
        note_read(
            &dir,
            &json!({"tool_name": "Grep", "tool_input": {"path": path(&dir)}}),
        );
        assert!(notice(&dir, Some(&config)).is_some());
        note_read(
            &dir,
            &read(&dir, Path::new("generated/coordinator-brief.md")),
        );
        assert_eq!(notice(&dir, Some(&config)), None);

        // An emptied context has lost it; a brief that changed again is new.
        forget_read(&dir);
        assert!(notice(&dir, Some(&config)).is_some());
        note_read(&dir, &read(&dir, &path(&dir)));
        assert_eq!(notice(&dir, Some(&config)), None);
        config.goal = Some("Ship it by Friday".into());
        assert!(notice(&dir, Some(&config)).is_some());
        assert!(
            std::fs::read_to_string(path(&dir))
                .unwrap()
                .contains("by Friday")
        );

        // A session created with this very brief has nothing to read.
        let first = prompts::coordinator(&config, &dir, "");
        generated::save_coordinator_prompt(&dir, &first).unwrap();
        forget_read(&dir);
        assert_eq!(notice(&dir, Some(&config)), None);
        assert_eq!(notice(&dir, None), None);
    }

    #[test]
    fn tools_older_than_the_binary_are_said_to_be_old() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("chief.db")).unwrap();
        let event = |hook: &str, summary: &str, ts: i64, session: &str| {
            crate::db::insert_event(
                &conn,
                &crate::db::Event {
                    session_name: Some(session.into()),
                    session_id: None,
                    project_slug: "demo".into(),
                    hook: hook.into(),
                    ts,
                    summary: Some(summary.into()),
                    agent_id: None,
                },
            )
            .unwrap();
        };
        let stale = |installed| stale_tools(&conn, "demo", dir.path(), installed).unwrap();
        // Nothing says when its process started, or when chief was installed.
        assert_eq!(stale(Some(5_000)), None);
        event("SessionStart", "startup", 1_000, "chief-demo");
        assert_eq!(stale(None), None);
        // Installed before it started: its tools are that chief's.
        assert_eq!(stale(Some(900)), None);
        let said = stale(Some(5_000)).expect("old tools");
        assert!(said.contains("chief down demo && chief up demo"), "{said}");
        // `/clear` keeps the process, and a thread's start is not the coordinator's.
        event("SessionStart", "clear", 6_000, "chief-demo");
        event("SessionStart", "startup", 6_000, "demo-thread-1a2b");
        assert!(stale(Some(5_000)).is_some());
        // A restart brings the new tools.
        event("SessionStart", "resume", 7_000, "chief-demo");
        assert_eq!(stale(Some(5_000)), None);

        // A restart whose SessionStart matched no row (its id changed with a `/clear`) is in
        // no event of the coordinator's: its hook noted it. `/clear` itself notes nothing.
        assert!(stale(Some(9_000)).is_some());
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        note_start(dir.path(), Some("clear"), 9_500);
        note_start(dir.path(), None, 9_500);
        assert!(stale(Some(9_000)).is_some());
        note_start(dir.path(), Some("resume"), 9_500);
        assert_eq!(stale(Some(9_000)), None);
        assert!(stale(Some(9_600)).is_some());
    }
}
