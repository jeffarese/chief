//! Coordinator and thread prompts (ARCHITECTURE §8): `src/prompts/*.md` with `{{name}}`
//! placeholders, plus the project's `instructions.md`.

use std::path::Path;

use crate::config::ProjectConfig;

const COORDINATOR: &str = include_str!("prompts/coordinator.md");
const THREAD: &str = include_str!("prompts/thread.md");

/// The coordinator's `--append-system-prompt`.
pub fn coordinator(config: &ProjectConfig, project_dir: &Path, instructions: &str) -> String {
    let goal_line = match config.goal.as_deref().map(str::trim) {
        Some(goal) if !goal.is_empty() => format!(" Goal: {goal}"),
        _ => String::new(),
    };
    let prompt = render(
        COORDINATOR,
        &[
            ("name", &config.name),
            ("goal_line", &goal_line),
            ("project_dir", &project_dir.display().to_string()),
            ("max_parallel", &config.threads.max_parallel.to_string()),
        ],
    );
    with_instructions(prompt, instructions)
}

/// A thread's `--append-system-prompt`. `elsewhere`: it runs in another Claude account than its
/// coordinator, where the coordinator's name doesn't reach (§1, §6).
/// Where a thread runs, for how its prompt says it reaches its coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The project's account, for good: the coordinator's name reaches it.
    Same,
    /// Another account, for good: only a reply to the coordinator's address does.
    Elsewhere,
    /// Through agp, which may move it to another account (§5 "Limits and moves"). The prompt
    /// is given once, at the start, and a move keeps it, so it must hold in either account.
    MayMove,
}

pub fn thread(
    config: &ProjectConfig,
    coordinator_name: &str,
    project_dir: &Path,
    instructions: &str,
    reach: Reach,
) -> String {
    let messaging = match reach {
        Reach::Elsewhere => "You run in another Claude account than your coordinator, so its \
             name doesn't reach it, and you don't need to message it to report. To answer a \
             message from it, reply with SendMessage to the exact `from=\"uds:…\"` address in \
             that message's header."
            .to_string(),
        Reach::Same => format!(
            "Of other sessions, you can message only your coordinator, by its exact name \
             `{coordinator_name}`; you \
             don't need to message it to report."
        ),
        Reach::MayMove => format!(
            "Of other sessions, you can message only your coordinator, and you don't need to \
             message it to report. You may be moved to another Claude account when yours hits a \
             usage limit, and then its name `{coordinator_name}` doesn't reach it: to answer a \
             message from it, reply with SendMessage to the exact `from` address in that \
             message's header (`{coordinator_name}` or `uds:…`)."
        ),
    };
    let prompt = render(
        THREAD,
        &[
            ("name", &config.name),
            ("coordinator_name", coordinator_name),
            ("project_dir", &project_dir.display().to_string()),
            ("messaging", &messaging),
        ],
    );
    with_instructions(prompt, instructions)
}

/// `<dir>/instructions.md`; a missing file is no instructions.
pub fn read_instructions(project_dir: &Path) -> String {
    std::fs::read_to_string(project_dir.join("instructions.md")).unwrap_or_default()
}

fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

/// Appends the instructions unless they are empty once HTML comments (the template's hints) are
/// removed.
fn with_instructions(prompt: String, instructions: &str) -> String {
    let text = strip_comments(instructions);
    let text = text.trim();
    if text.is_empty() {
        return prompt;
    }
    format!(
        "{}\n\n## Project instructions\n\n{text}\n",
        prompt.trim_end()
    )
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(goal: Option<&str>) -> ProjectConfig {
        ProjectConfig::new("Payments latency", "payments-latency", goal, vec![])
    }

    #[test]
    fn coordinator_fills_every_placeholder() {
        let p = coordinator(
            &config(Some("Hold p95 under 200 ms")),
            Path::new("/root/projects/payments-latency"),
            "",
        );
        assert!(!p.contains("{{"), "{p}");
        assert!(p.starts_with(
            "You are the coordinator of the chief project \"Payments latency\". Goal: Hold p95 under 200 ms"
        ));
        assert!(p.contains("/root/projects/payments-latency/memory/"));
        assert!(p.contains("at most 3 threads"));
        assert!(!p.contains("## Project instructions"));
        // §6 "Subagents".
        assert!(p.contains("Of other sessions, you can only message this\n  project's threads"));
        assert!(
            p.contains(
                "- You can message your own subagents by the name or agent id the Agent tool \
                 gave them, and they report to you."
            ),
            "{p}"
        );

        let p = coordinator(&config(Some("  ")), Path::new("/d"), "");
        assert!(
            p.starts_with("You are the coordinator of the chief project \"Payments latency\".\n")
        );
    }

    #[test]
    fn thread_fills_every_placeholder() {
        let p = thread(
            &config(None),
            "chief-payments-latency",
            Path::new("/d"),
            "",
            Reach::Same,
        );
        assert!(!p.contains("{{"), "{p}");
        assert!(p.contains("`chief-payments-latency`"));
        assert!(p.contains("/d/memory/MEMORY.md"));
        assert!(p.contains(
            "- Of other sessions, you can message only your coordinator, by its exact name"
        ));
        let subagents = "- You can message your own subagents by the name or agent id the Agent \
                         tool gave them, and they report to you.";
        assert!(p.contains(subagents), "{p}");

        // In another account than the coordinator's: reply at the address, not the name.
        let p = thread(
            &config(None),
            "chief-payments-latency",
            Path::new("/d"),
            "",
            Reach::Elsewhere,
        );
        assert!(!p.contains("{{"), "{p}");
        assert!(
            p.contains("- You run in another Claude account than your coordinator"),
            "{p}"
        );
        assert!(p.contains("`from=\"uds:…\"` address"), "{p}");
        assert!(!p.contains("by its exact name"), "{p}");
        assert!(p.contains(subagents), "{p}");

        // Through agp, which may move it: holds in either account.
        let p = thread(
            &config(None),
            "chief-payments-latency",
            Path::new("/d"),
            "",
            Reach::MayMove,
        );
        assert!(!p.contains("{{"), "{p}");
        assert!(
            p.contains("You may be moved to another Claude account"),
            "{p}"
        );
        assert!(p.contains("(`chief-payments-latency` or `uds:…`)"), "{p}");
        assert!(p.contains(subagents), "{p}");
    }

    #[test]
    fn instructions_are_appended_when_not_empty() {
        let c = config(None);
        let template = "<!-- Standing brief.\nWrite here. -->\n\n";
        assert!(
            !thread(&c, "x", Path::new("/d"), template, Reach::Same)
                .contains("## Project instructions")
        );
        let p = thread(
            &c,
            "x",
            Path::new("/d"),
            "<!-- hint -->\nUse pnpm.\n",
            Reach::Same,
        );
        assert!(
            p.ends_with("\n\n## Project instructions\n\nUse pnpm.\n"),
            "{p}"
        );
        let p = coordinator(&c, Path::new("/d"), "Ship on Fridays.");
        assert!(
            p.ends_with("## Project instructions\n\nShip on Fridays.\n"),
            "{p}"
        );
        assert_eq!(strip_comments("a<!-- b -->c<!-- d"), "ac");
    }
}
