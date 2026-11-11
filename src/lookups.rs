//! What the coordinator looks up itself (ARCHITECTURE §6 "The coordinator's own looking"). It
//! routes work and answers quick questions; anything that takes searching is a thread's. The
//! prompt says so, and this keeps it so: its Read, Grep and Glob stay inside the project's
//! folders, and a turn gets only so many lookups before the rest is a thread's to do.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::config::ProjectConfig;
use crate::db::SessionRow;

/// How many lookups a coordinator's turn gets: searches, web calls, and reads outside its own
/// folder. Enough to find a file and read it; not enough to research.
pub const PER_TURN: i64 = 6;

/// How a call counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Not a lookup, or a read in the coordinator's own folder (memory, instructions, a
    /// routine's notes): never limited.
    Free,
    /// A search, a web call, or a read in a repository: one of the turn's `PER_TURN`.
    Counted,
    /// A path outside every folder of the project.
    Outside(PathBuf),
}

/// Whether `payload` (a PreToolUse) is a call this module weighs at all.
pub fn is_lookup(payload: &Value) -> bool {
    matches!(
        payload.get("tool_name").and_then(Value::as_str),
        Some("Read" | "Grep" | "Glob" | "WebSearch" | "WebFetch")
    )
}

/// `path` without `.` and `..`, by its text alone.
fn tidy(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A project folder as its settings name it and as the file system resolves it.
fn spellings(path: &Path) -> Vec<PathBuf> {
    let tidy = tidy(path);
    let mut out = vec![tidy.clone()];
    if let Ok(real) = tidy.canonicalize()
        && real != tidy
    {
        out.push(real);
    }
    out
}

/// The absolute path of `text`, which a tool input gave: `~` is the home folder and a relative
/// path starts at `cwd`.
fn absolute(text: &str, cwd: &Path) -> PathBuf {
    let path = match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None if text == "~" => dirs::home_dir().unwrap_or_default(),
        None => PathBuf::from(text),
    };
    if path.is_absolute() {
        tidy(&path)
    } else {
        tidy(&cwd.join(path))
    }
}

/// Where a Read, Grep or Glob call looks. Grep and Glob search `path`, else the session's
/// folder; a pattern that is itself an absolute path searches from its fixed part.
fn target(tool: &str, input: &Value, cwd: &Path) -> PathBuf {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if tool == "Read" {
        return absolute(text("file_path").unwrap_or_default(), cwd);
    }
    let rooted = text("pattern")
        .filter(|_| tool == "Glob")
        .filter(|p| p.starts_with('/') || p.starts_with('~'))
        .map(|p| {
            let fixed = p.split(['*', '?', '[', '{']).next().unwrap_or_default();
            // Up to the last whole folder before the first wildcard.
            match fixed.rfind('/') {
                Some(end) if end > 0 => &fixed[..end],
                _ => "/",
            }
        });
    absolute(rooted.or(text("path")).unwrap_or("."), cwd)
}

/// The folders of a project its coordinator may look in besides its own: the repositories, and
/// where its sessions work (a thread's worktree).
pub fn roots(config: &ProjectConfig, rows: &[SessionRow]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = config.repos.iter().map(|r| r.path.clone()).collect();
    for row in rows {
        roots.extend(row.cwd.as_deref().map(PathBuf::from));
        roots.extend(row.repo_path.as_deref().map(PathBuf::from));
    }
    roots
}

/// Whether `path` is under one of `roots`. Where it really is decides: a link inside a
/// repository that leads out is out. A path that doesn't exist is taken by its text.
fn within(path: &Path, roots: &[PathBuf]) -> bool {
    let tidy = tidy(path);
    let real = tidy.canonicalize().unwrap_or(tidy);
    roots
        .iter()
        .any(|root| spellings(root).iter().any(|root| real.starts_with(root)))
}

/// How the coordinator's call in `payload` (a PreToolUse) counts. `own` is its folder
/// (`<root>/projects/<slug>`), `roots` the project's other folders.
pub fn classify(payload: &Value, own: &Path, roots: &[PathBuf]) -> Kind {
    let field = |key: &str| payload.get(key).and_then(Value::as_str);
    let tool = field("tool_name").unwrap_or_default();
    match tool {
        "WebSearch" | "WebFetch" => return Kind::Counted,
        "Read" | "Grep" | "Glob" => {}
        _ => return Kind::Free,
    }
    let cwd = field("cwd").map(Path::new).unwrap_or(own);
    let empty = Value::Null;
    let path = target(tool, payload.get("tool_input").unwrap_or(&empty), cwd);
    let own_roots = [own.to_path_buf()];
    if within(&path, &own_roots) {
        // Its own files cost nothing to read; searching them is still searching.
        return if tool == "Read" {
            Kind::Free
        } else {
            Kind::Counted
        };
    }
    let session_files = claude_files(field("transcript_path").map(Path::new));
    if within(&path, roots) || within(&path, &session_files) {
        Kind::Counted
    } else {
        Kind::Outside(path)
    }
}

/// What Claude Code keeps for this session and sends it to read, from where its transcript is
/// (`<account>/projects/<folder key>/<session id>.jsonl`): the session's own folder (a long
/// tool result), its folder's auto memory, and the account's skills and plugins (a skill's
/// reference files). Not the transcripts beside its own, which are other conversations.
fn claude_files(transcript: Option<&Path>) -> Vec<PathBuf> {
    let Some(transcript) = transcript else {
        return Vec::new();
    };
    let mut files = vec![transcript.with_extension("")];
    if let Some(folder) = transcript.parent() {
        files.push(folder.join("memory"));
        if let Some(account) = folder.parent().and_then(Path::parent) {
            files.push(account.join("skills"));
            files.push(account.join("plugins"));
        }
    }
    files
}

/// How many lookups `session` made since its turn began (its last prompt, or the end of its
/// last turn), this call included: it is recorded before it is weighed. Read from what the
/// hooks recorded of each call (`hook::summary`): a read in the session's own folder is
/// recorded by its path from there, so only reads elsewhere start with a `/`.
pub fn made(conn: &Connection, session: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM events
         WHERE session_name = ?1 AND hook = 'PreToolUse'
           AND id > coalesce((SELECT max(id) FROM events
                              WHERE session_name = ?1 AND hook IN ('UserPromptSubmit', 'Stop')), 0)
           AND (summary LIKE 'Grep%' OR summary LIKE 'Glob%' OR summary LIKE 'WebSearch%'
                OR summary LIKE 'WebFetch%' OR summary LIKE 'Read: /%' OR summary LIKE 'Read: ~%')",
        params![session],
        |r| r.get(0),
    )?)
}

/// Why chief refuses a look outside the project's folders; the coordinator reads it as the
/// tool's result.
pub fn outside_reason(path: &Path) -> String {
    format!(
        "{} is outside this project's folders, and you look only in the project folder and \
         its repositories. If the answer is somewhere else on this machine (another folder, \
         earlier Claude conversations, another project), start a thread to find it: name the \
         path in its task, say that it changes nothing and reports what it finds, and give it \
         a small model and low effort. Tell the user a thread is looking.",
        path.display()
    )
}

/// Why chief refuses one more lookup this turn.
pub fn budget_reason() -> String {
    format!(
        "That is more looking than a coordinator's turn is for ({PER_TURN} searches and reads \
         since this turn began). You route work and answer quick questions; finding things \
         out is a thread's work. Answer with what you have, or start a thread for it: a task \
         that says what to find, that it changes nothing and reports what it finds, with a \
         small model and low effort. Tell the user a thread is looking. Reading your own \
         memory and instructions stays free."
    )
}

/// The denial for the coordinator's call in `payload`, if it is one too many or looks where
/// it shouldn't.
pub fn refusal(
    conn: &Connection,
    session: &str,
    payload: &Value,
    own: &Path,
    roots: &[PathBuf],
) -> Result<Option<String>> {
    Ok(match classify(payload, own, roots) {
        Kind::Free => None,
        Kind::Outside(path) => Some(outside_reason(&path)),
        Kind::Counted => (made(conn, session)? > PER_TURN).then(budget_reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, input: Value) -> Value {
        json!({
            "tool_name": tool,
            "tool_input": input,
            "cwd": "/chief/projects/home",
            "transcript_path": "/acct/projects/-chief-projects-home/sid-1.jsonl",
        })
    }

    fn kind(tool: &str, input: Value) -> Kind {
        classify(
            &call(tool, input),
            Path::new("/chief/projects/home"),
            &[PathBuf::from("/code/home")],
        )
    }

    #[test]
    fn its_own_files_are_free_and_the_repositories_count() {
        for path in [
            "memory/MEMORY.md",
            "/chief/projects/home/instructions.md",
            "./routines/flights/NOTES.md",
        ] {
            assert_eq!(
                kind("Read", json!({"file_path": path})),
                Kind::Free,
                "{path}"
            );
        }
        assert_eq!(
            kind("Read", json!({"file_path": "/code/home/bin/tg"})),
            Kind::Counted
        );
        // Searching counts wherever it looks, its own folder included (no path is that folder).
        assert_eq!(kind("Grep", json!({"pattern": "fare"})), Kind::Counted);
        assert_eq!(
            kind("Glob", json!({"pattern": "**/*.md", "path": "/code/home"})),
            Kind::Counted
        );
        assert_eq!(kind("WebSearch", json!({"query": "x"})), Kind::Counted);
        assert_eq!(kind("WebFetch", json!({"url": "https://x"})), Kind::Counted);
        // What Claude Code saved of a long tool result for this very session.
        assert_eq!(
            kind(
                "Read",
                json!({"file_path": "/acct/projects/-chief-projects-home/sid-1/tool-results/a.txt"})
            ),
            Kind::Counted
        );
        // And what Claude Code sends it to read: its auto memory, a skill's files.
        for path in [
            "/acct/projects/-chief-projects-home/memory/MEMORY.md",
            "/acct/skills/rex/reference.md",
            "/acct/plugins/x/skills/y/SKILL.md",
        ] {
            assert_eq!(
                kind("Read", json!({"file_path": path})),
                Kind::Counted,
                "{path}"
            );
        }
        // Anything else is no lookup.
        assert_eq!(kind("Edit", json!({"file_path": "/etc/hosts"})), Kind::Free);
        assert_eq!(kind("mcp__chief__list_threads", json!({})), Kind::Free);
    }

    #[test]
    fn nothing_outside_the_project_is_looked_at() {
        let outside = |tool: &str, input: Value, want: &str| {
            assert_eq!(
                kind(tool, input.clone()),
                Kind::Outside(PathBuf::from(want)),
                "{tool} {input}"
            );
        };
        // Another session's transcript, beside this session's own.
        outside(
            "Read",
            json!({"file_path": "/acct/projects/-chief-projects-home/other.jsonl"}),
            "/acct/projects/-chief-projects-home/other.jsonl",
        );
        outside(
            "Grep",
            json!({"pattern": "routine", "path": "/acct/projects"}),
            "/acct/projects",
        );
        // A pattern that is itself a path searches from its fixed part.
        outside(
            "Glob",
            json!({"pattern": "/Users/me/*/projects/*home*/*.jsonl"}),
            "/Users/me",
        );
        outside("Glob", json!({"pattern": "/*.jsonl"}), "/");
        // `..` leads out of the project, and a folder that only starts like one isn't it.
        outside(
            "Read",
            json!({"file_path": "../other/project.toml"}),
            "/chief/projects/other/project.toml",
        );
        outside(
            "Read",
            json!({"file_path": "/code/home-backup/x"}),
            "/code/home-backup/x",
        );
        assert!(outside_reason(Path::new("/x")).contains("start a thread"));
    }

    #[test]
    fn a_link_out_of_a_repository_leads_out() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (own, repo, secret) = (base.join("own"), base.join("repo"), base.join("secret"));
        for d in [&own, &repo, &secret] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(secret.join("key"), "k").unwrap();
        std::os::unix::fs::symlink(&secret, repo.join("link")).unwrap();
        let read = |path: PathBuf| {
            classify(
                &json!({"tool_name": "Read", "tool_input": {"file_path": path}, "cwd": own}),
                &own,
                std::slice::from_ref(&repo),
            )
        };
        assert_eq!(
            read(repo.join("link/key")),
            Kind::Outside(repo.join("link/key"))
        );
        std::fs::write(repo.join("file"), "f").unwrap();
        assert_eq!(read(repo.join("file")), Kind::Counted);
    }

    #[test]
    fn a_turn_gets_only_so_many_lookups() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("chief.db")).unwrap();
        let event = |hook: &str, summary: &str| {
            crate::db::insert_event(
                &conn,
                &crate::db::Event {
                    session_name: Some("chief-home".into()),
                    session_id: None,
                    project_slug: "home".into(),
                    hook: hook.into(),
                    ts: 0,
                    summary: Some(summary.into()),
                    agent_id: None,
                },
            )
            .unwrap();
        };
        let own = Path::new("/chief/projects/home");
        let roots = [PathBuf::from("/code/home")];
        let refused = |tool: &str, input: Value| {
            refusal(&conn, "chief-home", &call(tool, input), own, &roots).unwrap()
        };
        event("UserPromptSubmit", "where did we say that?");
        // Its own reads, its tools and its edits never count.
        for summary in [
            "Read: memory/MEMORY.md",
            "mcp__chief__list_threads",
            "Edit: memory/x.md",
        ] {
            event("PreToolUse", summary);
        }
        for n in 1..=PER_TURN {
            event("PreToolUse", &format!("Grep: pattern {n}"));
            assert_eq!(refused("Grep", json!({"pattern": "x"})), None, "lookup {n}");
        }
        event("PreToolUse", "Read: /code/home/bin/tg");
        let why = refused("Read", json!({"file_path": "/code/home/bin/tg"})).unwrap();
        assert!(why.contains("start a thread"), "{why}");
        // Its memory stays readable, and a look outside is refused for where it looks.
        assert_eq!(
            refused("Read", json!({"file_path": "memory/MEMORY.md"})),
            None
        );
        assert!(
            refused("Read", json!({"file_path": "/etc/hosts"}))
                .unwrap()
                .contains("outside this project's folders")
        );
        // The next turn starts over: after its turn ended, or at the user's next prompt.
        event("Stop", "done");
        event("PreToolUse", "Glob: **/*.rs");
        assert_eq!(refused("Glob", json!({"pattern": "**/*.rs"})), None);
        // Another session's calls are its own.
        assert_eq!(made(&conn, "demo-thread-1a2b").unwrap(), 0);
    }
}
