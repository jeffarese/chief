//! The coordinator's UserPromptSubmit context (ARCHITECTURE §8): a compact board snapshot and
//! the memory index, attached to every turn.

use serde_json::json;

use crate::db::SessionRow;
use crate::registry;
use crate::state::Group;
use crate::text::one_line;

/// Upper bound for the whole context; it rides along on every coordinator turn.
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024;
/// Report preview lengths tried in order until the context fits.
const PREVIEWS: [usize; 4] = [200, 100, 40, 0];

/// `<chief-board as-of="HH:MM">…</chief-board>` and `<chief-memory>…</chief-memory>`, under
/// `MAX_CONTEXT_BYTES`. `threads` are the project's threads in board order; resolved ones are
/// left out. Previews shrink first, then memory, then the board itself is cut.
pub fn coordinator_context(threads: &[SessionRow], memory: &str, as_of: &str) -> String {
    let memory = memory.trim();
    for preview in PREVIEWS {
        let text = assemble(&board(threads, preview, as_of), memory);
        if text.len() <= MAX_CONTEXT_BYTES {
            return text;
        }
    }
    let board = board(threads, 0, as_of);
    let bare = assemble(&board, "");
    if bare.len() < MAX_CONTEXT_BYTES {
        let marker = "\n[memory cut; read memory/MEMORY.md]";
        let room = (MAX_CONTEXT_BYTES - bare.len()).saturating_sub(marker.len());
        // The marker takes the place of `(empty)`, which is shorter: with almost no room left
        // even an empty cut doesn't fit, and the board must be cut instead.
        let text = assemble(&board, &format!("{}{marker}", truncate_bytes(memory, room)));
        if text.len() <= MAX_CONTEXT_BYTES {
            return text;
        }
    }
    let marker = "\n[board cut; call list_threads]\n</chief-board>";
    let tail = "\n<chief-memory>\n[left out; read memory/MEMORY.md]\n</chief-memory>";
    let body = board.strip_suffix("</chief-board>").unwrap_or(&board);
    let room = MAX_CONTEXT_BYTES - marker.len() - tail.len();
    format!("{}{marker}{tail}", truncate_bytes(body, room))
}

/// The UserPromptSubmit hook output that injects `context`.
pub fn hook_output(context: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })
    .to_string()
}

fn assemble(board: &str, memory: &str) -> String {
    let memory = if memory.is_empty() { "(empty)" } else { memory };
    format!("{board}\n<chief-memory>\n{memory}\n</chief-memory>")
}

fn board(threads: &[SessionRow], preview: usize, as_of: &str) -> String {
    let mut out = format!("<chief-board as-of=\"{as_of}\">\n");
    let open: Vec<_> = threads.iter().filter(|t| !t.resolved).collect();
    if open.is_empty() {
        out.push_str("No open threads.\n");
    }
    for group in Group::ORDER {
        let rows: Vec<_> = open
            .iter()
            .filter(|t| Group::parse(&t.group_name).unwrap_or(Group::Idle) == group)
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("{}:\n", group.label()));
        for t in rows {
            out.push_str(&line(t, preview));
        }
    }
    out.push_str("</chief-board>");
    out
}

/// `- name "title" · waiting: permission · 1 subagent running · asleep · branch b · PR url` and
/// an indented preview.
fn line(t: &SessionRow, preview: usize) -> String {
    let mut parts = vec![match t.title.as_deref() {
        Some(title) => format!("- {} \"{}\"", t.name, one_line(title, 80)),
        None => format!("- {}", t.name),
    }];
    if let Some(reason) = &t.waiting_reason {
        parts.push(format!("waiting: {reason}"));
    }
    if let Some(work) = registry::background_work(t) {
        parts.push(format!("{work} running"));
    }
    if registry::limited(t)
        && let Some(limit) = &t.limit_note
    {
        parts.push(format!("limited: {}", one_line(limit, 120)));
    }
    if t.pid.is_none() {
        parts.push("asleep".to_string());
    }
    if let Some(branch) = &t.branch {
        parts.push(format!("branch {branch}"));
    }
    if let Some(pr) = &t.pr_url {
        parts.push(format!("PR {pr}"));
    }
    let mut out = parts.join(" · ");
    out.push('\n');
    if preview > 0
        && let Some(report) = t.last_report.as_deref().filter(|r| !r.trim().is_empty())
    {
        out.push_str(&format!("  report: {}\n", one_line(report, preview)));
    }
    out
}

/// At most `max` bytes of `s`, cut on a char boundary.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Role;

    fn row(name: &str, group: &str) -> SessionRow {
        let mut r = SessionRow::new(name, "p", Role::Thread, 0);
        r.group_name = group.into();
        r.title = Some(format!("Title of {name}"));
        r.pid = Some(1);
        r
    }

    #[test]
    fn compact_board_and_memory() {
        let mut waiting = row("p-a-1111", "waiting");
        waiting.waiting_reason = Some("permission".into());
        waiting.branch = Some("worktree-a".into());
        let mut idle = row("p-b-2222", "idle");
        idle.pid = None;
        idle.pr_url = Some("https://github.com/o/r/pull/7".into());
        idle.last_report = Some(format!("Done.\n\n{}", "x".repeat(400)));
        let mut resolved = row("p-c-3333", "resolved");
        resolved.resolved = true;

        let text = coordinator_context(
            &[waiting, idle, resolved],
            "# Memory\n- [Prefs](prefs.md)\n",
            "14:05",
        );
        let want_start = "<chief-board as-of=\"14:05\">\n\
            Waiting on you:\n\
            - p-a-1111 \"Title of p-a-1111\" · waiting: permission · branch worktree-a\n\
            Idle:\n\
            - p-b-2222 \"Title of p-b-2222\" · asleep · PR https://github.com/o/r/pull/7\n  report: Done. xxx";
        assert!(text.starts_with(want_start), "{text}");
        assert!(!text.contains("p-c-3333"), "resolved threads are left out");
        let report_line = text.lines().find(|l| l.starts_with("  report:")).unwrap();
        assert_eq!(report_line.chars().count(), "  report: ".len() + 200);
        assert!(text.ends_with(
            "</chief-board>\n<chief-memory>\n# Memory\n- [Prefs](prefs.md)\n</chief-memory>"
        ));
    }

    /// §5 "Background work": a thread idle in its own turn still has work running, unless its
    /// process is gone with it.
    #[test]
    fn background_work_is_on_the_line() {
        let mut t = row("p-a-1111", "working");
        t.subagents_busy = 1;
        t.background_running = 2;
        assert_eq!(
            line(&t, 0),
            "- p-a-1111 \"Title of p-a-1111\" · 1 subagent, 2 background commands running\n"
        );
        t.pid = None;
        t.gone_at = Some(5);
        assert_eq!(line(&t, 0), "- p-a-1111 \"Title of p-a-1111\" · asleep\n");
    }

    #[test]
    fn empty_project() {
        let text = coordinator_context(&[], "", "09:00");
        assert_eq!(
            text,
            "<chief-board as-of=\"09:00\">\nNo open threads.\n</chief-board>\n\
             <chief-memory>\n(empty)\n</chief-memory>"
        );
    }

    #[test]
    fn previews_shrink_first() {
        let threads: Vec<_> = (0..40)
            .map(|i| {
                let mut r = row(&format!("p-thread-{i:04}"), "idle");
                r.last_report = Some("y".repeat(500));
                r
            })
            .collect();
        let text = coordinator_context(&threads, "short memory", "10:00");
        assert!(text.len() <= MAX_CONTEXT_BYTES, "{}", text.len());
        assert!(text.contains("short memory"));
        assert_eq!(text.matches("- p-thread-").count(), 40);
        let preview = text.lines().find(|l| l.starts_with("  report:")).unwrap();
        assert!(preview.chars().count() < "  report: ".len() + 200);
    }

    #[test]
    fn memory_then_board_are_cut() {
        let threads = vec![row("p-a-1111", "working")];
        let text = coordinator_context(&threads, &"é".repeat(10_000), "10:00");
        assert!(text.len() <= MAX_CONTEXT_BYTES);
        assert!(text.contains("- p-a-1111"));
        assert!(text.contains("[memory cut; read memory/MEMORY.md]\n</chief-memory>"));

        let many: Vec<_> = (0..400)
            .map(|i| row(&format!("p-a-very-long-thread-name-{i:04}"), "working"))
            .collect();
        let text = coordinator_context(&many, "m", "10:00");
        assert!(text.len() <= MAX_CONTEXT_BYTES, "{}", text.len());
        assert!(text.contains("[board cut; call list_threads]\n</chief-board>"));
        assert!(text.ends_with("</chief-memory>"));
    }

    /// Every gap the bare board can leave below the budget, around the memory marker's size:
    /// the marker takes the place of `(empty)`, so a small gap can't hold even an empty cut.
    #[test]
    fn memory_cut_stays_within_the_budget() {
        let memory = "m".repeat(10_000);
        let sized = |len: usize| {
            let mut r = row(&"n".repeat(len), "working");
            r.title = None;
            vec![r]
        };
        let bare = |threads: &[SessionRow]| assemble(&board(threads, 0, "10:00"), "").len();
        let base = bare(&sized(0));
        for gap in 1..=60 {
            let threads = sized(MAX_CONTEXT_BYTES - gap - base);
            assert_eq!(bare(&threads), MAX_CONTEXT_BYTES - gap);
            let text = coordinator_context(&threads, &memory, "10:00");
            assert!(text.len() <= MAX_CONTEXT_BYTES, "gap {gap}: {}", text.len());
            let cut = if gap >= 29 {
                "[memory cut;"
            } else {
                "[board cut;"
            };
            assert!(text.contains(cut), "gap {gap}: {text}");
        }
    }

    #[test]
    fn hook_output_shape() {
        let v: serde_json::Value = serde_json::from_str(&hook_output("ctx")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "ctx");
    }
}
