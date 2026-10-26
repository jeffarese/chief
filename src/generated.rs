//! `<dir>/generated/*` (ARCHITECTURE §3): hook settings for the coordinator and threads, and the
//! coordinator's MCP config. Rewritten by `init`, `up` and settings changes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::state::Role;
use crate::text::shell_quote;

/// Variables that point chief somewhere other than its defaults. When set at generation time
/// they are written into every hook command and the MCP server's env, so sessions started by
/// Claude Code's supervisor (which doesn't inherit our environment) use the same home. The Rex
/// layout's blocks get them too (they start from the login shell's environment, §9).
pub const PASSTHROUGH: [&str; 6] = [
    "CHIEF_HOME",
    "CHIEF_CLAUDE_BIN",
    "CHIEF_GH_BIN",
    "CHIEF_REX_BIN",
    "CHIEF_CLAUDE_CONFIG",
    "CHIEF_NO_NOTIFY",
];

/// Hook events chief records, with the PreToolUse/PostToolUse matcher where one applies.
const EVENTS: [(&str, Option<&str>); 8] = [
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some("*")),
    ("PostToolUse", Some("*")),
    ("Notification", None),
    ("Stop", None),
    ("SubagentStop", None),
    ("SessionEnd", None),
];
const GUARD_MATCHER: &str = "SendMessage|ListAgents";
/// Permission rule for every tool of the `chief` MCP server.
const MCP_TOOLS_RULE: &str = "mcp__chief";

pub fn coordinator_settings_path(dir: &Path) -> PathBuf {
    dir.join("generated/coordinator-settings.json")
}

pub fn thread_settings_path(dir: &Path) -> PathBuf {
    dir.join("generated/thread-settings.json")
}

pub fn mcp_config_path(dir: &Path) -> PathBuf {
    dir.join("generated/mcp.json")
}

/// The running binary's canonical path, so hooks keep working from any cwd.
pub fn chief_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the chief binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// The `PASSTHROUGH` variables set in this process. Paths are made absolute: hooks run in the
/// session's cwd.
pub fn passthrough_env() -> Vec<(String, String)> {
    PASSTHROUGH
        .iter()
        .filter_map(|key| {
            let value = std::env::var_os(key)?;
            let value = match *key {
                "CHIEF_NO_NOTIFY" => value.to_string_lossy().into_owned(),
                // A bare `claude` or `gh` is looked up on PATH; anything with a slash is a path.
                "CHIEF_CLAUDE_BIN" | "CHIEF_GH_BIN" | "CHIEF_REX_BIN"
                    if !value.to_string_lossy().contains('/') =>
                {
                    value.to_string_lossy().into_owned()
                }
                _ => std::path::absolute(&value)
                    .unwrap_or_else(|_| PathBuf::from(&value))
                    .to_string_lossy()
                    .into_owned(),
            };
            Some((key.to_string(), value))
        })
        .collect()
}

/// Writes all three files.
pub fn write_all(dir: &Path, slug: &str, exe: &Path, env: &[(String, String)]) -> Result<()> {
    write_json(
        &coordinator_settings_path(dir),
        &hook_settings(exe, slug, Role::Coordinator, env),
    )?;
    write_json(
        &thread_settings_path(dir),
        &hook_settings(exe, slug, Role::Thread, env),
    )?;
    write_json(&mcp_config_path(dir), &mcp_config(exe, slug, env))
}

/// Claude Code `--settings` JSON with every chief hook. The coordinator also stays in its
/// folder: it never edits code, so a worktree would only hide memory edits.
pub fn hook_settings(exe: &Path, slug: &str, role: Role, env: &[(String, String)]) -> Value {
    let command = |event: &str| {
        json!({
            "type": "command",
            "command": hook_command(exe, slug, role, event, env),
        })
    };
    let mut hooks = serde_json::Map::new();
    for (event, matcher) in EVENTS {
        let mut entry = json!({"hooks": [command(event)]});
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        let mut entries = vec![entry];
        if event == "PreToolUse" {
            entries.push(json!({"matcher": GUARD_MATCHER, "hooks": [command("guard")]}));
        }
        hooks.insert(event.to_string(), Value::Array(entries));
    }
    let mut settings = json!({"hooks": hooks});
    if role == Role::Coordinator {
        settings["worktree"] = json!({"bgIsolation": "none"});
        // Nobody watches the coordinator's background session for prompts on its own tools.
        settings["permissions"] = json!({"allow": [MCP_TOOLS_RULE]});
    }
    settings
}

/// `[K='v' …] '<exe>' hook <event> --project <slug> --role <role>`, every word shell-quoted.
pub fn hook_command(
    exe: &Path,
    slug: &str,
    role: Role,
    event: &str,
    env: &[(String, String)],
) -> String {
    let mut words: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", shell_quote(v)))
        .collect();
    let exe = exe.to_string_lossy();
    for arg in [
        exe.as_ref(),
        "hook",
        event,
        "--project",
        slug,
        "--role",
        role.as_str(),
    ] {
        words.push(shell_quote(arg));
    }
    words.join(" ")
}

/// `--mcp-config` JSON that starts `chief mcp --project <slug>`.
pub fn mcp_config(exe: &Path, slug: &str, env: &[(String, String)]) -> Value {
    let env: serde_json::Map<String, Value> =
        env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "mcpServers": {
            "chief": {
                "type": "stdio",
                "command": exe.to_string_lossy(),
                "args": ["mcp", "--project", slug],
                "env": env,
            }
        }
    })
}

/// Through a temp file, so a session starting mid-write never reads half a file.
fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)? + "\n";
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/my tools/chief";

    #[test]
    fn hook_settings_register_every_event() {
        let v = hook_settings(Path::new(EXE), "demo", Role::Thread, &[]);
        let hooks = v["hooks"].as_object().unwrap();
        let mut keys: Vec<_> = hooks.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "Notification",
                "PostToolUse",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "SubagentStop",
                "UserPromptSubmit"
            ]
        );
        assert_eq!(
            hooks["Stop"],
            json!([{"hooks": [{"type": "command",
                "command": "'/opt/my tools/chief' hook Stop --project demo --role thread"}]}])
        );
        let pre = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["matcher"], "*");
        assert_eq!(pre[1]["matcher"], "SendMessage|ListAgents");
        assert_eq!(
            pre[1]["hooks"][0]["command"],
            "'/opt/my tools/chief' hook guard --project demo --role thread"
        );
        assert_eq!(hooks["PostToolUse"][0]["matcher"], "*");
        assert!(hooks["Notification"][0].get("matcher").is_none());
        assert!(
            v.get("worktree").is_none(),
            "threads keep worktree isolation"
        );
        assert!(v.get("permissions").is_none());
    }

    #[test]
    fn coordinator_settings_turn_off_isolation_and_carry_env() {
        let env = vec![
            ("CHIEF_HOME".to_string(), "/tmp/chief home".to_string()),
            ("CHIEF_NO_NOTIFY".to_string(), "1".to_string()),
        ];
        let v = hook_settings(Path::new("/bin/chief"), "demo", Role::Coordinator, &env);
        assert_eq!(v["worktree"], json!({"bgIsolation": "none"}));
        assert_eq!(v["permissions"], json!({"allow": ["mcp__chief"]}));
        assert_eq!(
            v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "CHIEF_HOME='/tmp/chief home' CHIEF_NO_NOTIFY=1 /bin/chief hook UserPromptSubmit \
             --project demo --role coordinator"
        );
    }

    #[test]
    fn hook_commands_run_in_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake chief");
        std::fs::write(&script, "#!/bin/sh\necho \"$CHIEF_HOME|$*\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![("CHIEF_HOME".to_string(), "/x/it's here".to_string())];
        let cmd = hook_command(&script, "demo", Role::Thread, "Stop", &env);
        let out = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "/x/it's here|hook Stop --project demo --role thread\n"
        );
    }

    #[test]
    fn mcp_config_shape() {
        let env = vec![("CHIEF_HOME".to_string(), "/h".to_string())];
        assert_eq!(
            mcp_config(Path::new("/bin/chief"), "demo", &env),
            json!({"mcpServers": {"chief": {
                "type": "stdio",
                "command": "/bin/chief",
                "args": ["mcp", "--project", "demo"],
                "env": {"CHIEF_HOME": "/h"},
            }}})
        );
    }

    #[test]
    fn write_all_writes_three_files() {
        let dir = tempfile::tempdir().unwrap();
        write_all(dir.path(), "demo", Path::new("/bin/chief"), &[]).unwrap();
        for p in [
            coordinator_settings_path(dir.path()),
            thread_settings_path(dir.path()),
            mcp_config_path(dir.path()),
        ] {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            assert!(v.is_object(), "{}", p.display());
        }
        let names: Vec<_> = std::fs::read_dir(dir.path().join("generated"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 3, "no temp files left: {names:?}");
    }
}
