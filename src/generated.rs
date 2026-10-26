//! `<dir>/generated/*` (ARCHITECTURE §3): hook settings for the coordinator and threads, and the
//! coordinator's MCP config. Rewritten by `init`, `up` and settings changes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::ProjectConfig;
use crate::state::Role;
use crate::text::shell_quote;

/// Variables that point chief somewhere other than its defaults. When set at generation time
/// they are written into every hook command and the MCP server's env, so sessions started by
/// Claude Code's supervisor (which doesn't inherit our environment) use the same home. The
/// layout's Rex blocks and tmux panes get them too (they start from the login shell's or the
/// tmux server's environment, §9).
/// `CHIEF_HOME` is always written, as the resolved root: the supervisor doesn't have our
/// `XDG_DATA_HOME` either.
/// `CLAUDE_CONFIG_DIR` isn't among them: it comes from the project's account, not from this
/// process (`passthrough_env`).
pub const PASSTHROUGH: [&str; 7] = [
    "CHIEF_HOME",
    "CHIEF_CLAUDE_BIN",
    "CHIEF_GH_BIN",
    "CHIEF_REX_BIN",
    "CHIEF_TMUX_BIN",
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
/// The guard entry's `timeout`. When Claude Code's hook timeout fires, the `|| exit 2` never runs
/// and the call proceeds, so the guard denies on its own at `hook::GUARD_DEADLINE`, well before.
pub const GUARD_HOOK_TIMEOUT_SECS: u64 = 30;
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

/// The `PASSTHROUGH` variables set in this process, with `CHIEF_HOME` always set to the resolved
/// root (`paths::root`), then `CLAUDE_CONFIG_DIR` for a project of a non-default `account`
/// (§3): Claude Code's per-account config folder, with its own supervisor, sessions, job state
/// and trust file, which Rex blocks and the supervisor don't inherit from this process. Paths
/// are made absolute: hooks run in the session's cwd.
pub fn passthrough_env(account: Option<&Path>) -> Vec<(String, String)> {
    let mut env = this_process_env();
    if let Some(dir) = account {
        env.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    env
}

fn this_process_env() -> Vec<(String, String)> {
    PASSTHROUGH
        .iter()
        .filter_map(|key| {
            if *key == "CHIEF_HOME" {
                let root = crate::paths::root();
                return Some((key.to_string(), root.to_string_lossy().into_owned()));
            }
            let value = std::env::var_os(key)?;
            let value = match *key {
                "CHIEF_NO_NOTIFY" => value.to_string_lossy().into_owned(),
                // A bare `claude` or `gh` is looked up on PATH; anything with a slash is a path.
                "CHIEF_CLAUDE_BIN" | "CHIEF_GH_BIN" | "CHIEF_REX_BIN" | "CHIEF_TMUX_BIN"
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

/// Writes all three files for the project in `dir` with these settings.
pub fn write_all(
    dir: &Path,
    config: &ProjectConfig,
    exe: &Path,
    env: &[(String, String)],
) -> Result<()> {
    let slug = config.slug.as_str();
    let repos: Vec<&Path> = config.repos.iter().map(|r| r.path.as_path()).collect();
    write_json(
        &coordinator_settings_path(dir),
        &coordinator_settings(
            exe,
            slug,
            env,
            &coordinator_deny_rules(
                dir,
                &repos,
                &account_settings_dir(config.claude_config_dir.as_deref()),
                &crate::paths::db(),
            ),
        ),
    )?;
    write_json(
        &thread_settings_path(dir),
        &hook_settings(exe, slug, Role::Thread, env),
    )?;
    write_json(&mcp_config_path(dir), &mcp_config(exe, slug, env))
}

/// `write_all` with this binary and environment, right before a session starts or respawns:
/// Claude Code reads the hook settings when a session starts, so any edit to them since the
/// last write (one that drops the guard, say) never reaches a later session (§3).
pub fn rewrite(dir: &Path, config: &ProjectConfig) -> Result<()> {
    let env = passthrough_env(config.claude_config_dir.as_deref());
    write_all(dir, config, &chief_exe()?, &env)
        .with_context(|| format!("rewriting {}", dir.join("generated").display()))
}

/// The coordinator's file: the hooks, worktree isolation off, its own MCP tools allowed, and
/// `deny` (from `coordinator_deny_rules`).
pub fn coordinator_settings(
    exe: &Path,
    slug: &str,
    env: &[(String, String)],
    deny: &[String],
) -> Value {
    let mut settings = hook_settings(exe, slug, Role::Coordinator, env);
    settings["permissions"]["deny"] = json!(deny);
    settings
}

/// Edit deny rules for the coordinator in `dir` (`<root>/projects/<slug>`), §3: one per
/// repository; every project's `generated/` folder and `project.toml`; its own `.claude/`
/// folder; the account's user settings (`<account dir>/settings*.json`); chief's database. A
/// routing slip must not edit a real checkout. `generated/*-settings.json` hold the guard and
/// project.toml is what they are rewritten from, in this project and any other; a project
/// `.claude/settings.json` in its cwd, or the account's user settings, could add hooks or
/// permissions to later sessions (§1: without a rule, a background session can write them).
/// Memory and instructions stay editable, here and in other projects. Paths are canonical. This is
/// the one place that builds them; the rule syntax itself is in `edit_deny_rule` and
/// `edit_deny_files`, and `config::validate` keeps glob characters out of repo paths.
pub fn coordinator_deny_rules(
    dir: &Path,
    repos: &[&Path],
    account_dir: &Path,
    db: &Path,
) -> Vec<String> {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let dir = canonical(dir);
    let projects = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    let db_dir = db.parent().map(canonical).unwrap_or_default();
    let db_name = db.file_name().unwrap_or_default().to_string_lossy();
    repos
        .iter()
        .map(|repo| canonical(repo))
        .chain([projects.join("*/generated")])
        .map(|path| edit_deny_rule(&path))
        .chain([
            edit_deny_files(&projects.join("*/project.toml")),
            edit_deny_rule(&dir.join(".claude")),
            edit_deny_files(&canonical(account_dir).join("settings*.json")),
            // `chief.db*` also covers the -wal and -shm files.
            edit_deny_files(&db_dir.join(format!("{db_name}*"))),
        ])
        .collect()
}

/// The folder whose `settings*.json` are the account's user settings: `claude_config_dir`, or
/// `~/.claude` for the default account. Real paths only, never a test override: these rules
/// guard the user's own files.
fn account_settings_dir(account: Option<&Path>) -> PathBuf {
    match account {
        Some(dir) => dir.to_path_buf(),
        None => dirs::home_dir().unwrap_or_default().join(".claude"),
    }
}

/// `Edit(//<absolute path>/**)`: Claude Code's permission rule for every file under an
/// absolute folder (a leading `//` means absolute). Verified live on 2.1.310 (§1): one such
/// `Edit` rule blocks both Edit and Write there, while `Write(…)` and `NotebookEdit(…)` rules
/// never match (Claude Code says so at startup), so only `Edit` rules are written.
pub fn edit_deny_rule(abs: &Path) -> String {
    format!("Edit(/{}/**)", abs.display())
}

/// `Edit(//<absolute path or file glob>)`: the same rule for one file, or a glob of files in one
/// folder.
pub fn edit_deny_files(abs: &Path) -> String {
    format!("Edit(/{})", abs.display())
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
            let guard = json!({
                "type": "command",
                "command": guard_command(exe, slug, role, env),
                "timeout": GUARD_HOOK_TIMEOUT_SECS,
            });
            entries.push(json!({"matcher": GUARD_MATCHER, "hooks": [guard]}));
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

/// The guard's hook command, which fails closed even when chief can't start (a missing binary
/// exits 127, which Claude Code treats as a non-blocking error): any non-zero exit becomes exit 2,
/// which blocks the tool call and shows the message to the model. chief itself always exits 0,
/// with the deny JSON or nothing.
pub fn guard_command(exe: &Path, slug: &str, role: Role, env: &[(String, String)]) -> String {
    let message = format!(
        "chief's message guard could not run ({}); message blocked. Run chief up again.",
        exe.display()
    );
    format!(
        "{} || {{ echo {} >&2; exit 2; }}",
        hook_command(exe, slug, role, "guard", env),
        shell_quote(&message)
    )
}

/// Whether `exe` sits in a cargo `target/` folder (an ancestor named `target` beside a
/// `Cargo.toml`), where `cargo clean` would delete it from under every hook.
pub fn in_cargo_target(exe: &Path) -> bool {
    exe.ancestors().any(|dir| {
        dir.file_name().is_some_and(|n| n == "target")
            && dir.parent().is_some_and(|p| p.join("Cargo.toml").is_file())
    })
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
            "'/opt/my tools/chief' hook guard --project demo --role thread || { echo \
             'chief'\\''s message guard could not run (/opt/my tools/chief); message blocked. Run \
             chief up again.' >&2; exit 2; }"
        );
        // Claude Code lets the call through when its hook timeout fires, so the guard's entry
        // says how long it may take, and the guard answers well before that.
        assert_eq!(pre[1]["hooks"][0]["timeout"], GUARD_HOOK_TIMEOUT_SECS);
        assert!(pre[0]["hooks"][0].get("timeout").is_none());
        let deadline = crate::hook::GUARD_DEADLINE.as_secs();
        assert!(crate::hook::GUARD_AGENTS_TIMEOUT.as_secs() < deadline);
        assert!(deadline + 5 <= GUARD_HOOK_TIMEOUT_SECS);
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
    fn cargo_target_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let exe = dir.path().join("target/release/chief");
        assert!(in_cargo_target(&exe));
        assert!(!in_cargo_target(&dir.path().join("bin/chief")));
        // A folder named target without a Cargo.toml beside it is just a folder.
        assert!(!in_cargo_target(Path::new("/nowhere/target/chief")));
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
    fn coordinator_settings_deny_edits_in_repos_and_generated() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects/demo");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let config = ProjectConfig::new(
            "Demo",
            "demo",
            None,
            vec![crate::config::Repo {
                name: "repo".into(),
                path: repo.clone(),
            }],
        );
        write_all(&dir, &config, Path::new("/bin/chief"), &[]).unwrap();
        let read = |p: PathBuf| -> Value {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let coord = read(coordinator_settings_path(&dir));
        // Canonical: macOS temp dirs live under /private.
        let repo_c = repo.canonicalize().unwrap();
        let dir_c = dir.canonicalize().unwrap();
        let projects = tmp.path().join("projects").canonicalize().unwrap();
        let db = crate::paths::db();
        let db_dir = db.parent().unwrap();
        let db_glob = db_dir
            .canonicalize()
            .unwrap_or_else(|_| db_dir.to_path_buf())
            .join("chief.db*");
        // The default account's folder, as Claude Code finds it (no test override).
        let home_claude = PathBuf::from(std::env::var("HOME").unwrap()).join(".claude");
        let home_claude = home_claude.canonicalize().unwrap_or(home_claude);
        // Every repo; every project's generated files and project.toml, the guard's and
        // settings' source (§3); the coordinator's own .claude folder (its project settings);
        // the account's user settings; the database.
        let deny = |account: &Path| {
            json!([
                format!("Edit(/{}/**)", repo_c.display()),
                format!("Edit(/{}/*/generated/**)", projects.display()),
                format!("Edit(/{}/*/project.toml)", projects.display()),
                format!("Edit(/{}/.claude/**)", dir_c.display()),
                format!("Edit(/{}/settings*.json)", account.display()),
                edit_deny_files(&db_glob),
            ])
        };
        assert_eq!(
            coord["permissions"],
            json!({"allow": ["mcp__chief"], "deny": deny(&home_claude)})
        );
        let account = tmp.path().join("claude-carlos");
        std::fs::create_dir_all(&account).unwrap();
        let mut carlos = config.clone();
        carlos.claude_config_dir = Some(account.clone());
        write_all(&dir, &carlos, Path::new("/bin/chief"), &[]).unwrap();
        let coord = read(coordinator_settings_path(&dir));
        assert_eq!(
            coord["permissions"]["deny"],
            deny(&account.canonicalize().unwrap())
        );
        let dir = dir_c;
        assert_eq!(coord["worktree"], json!({"bgIsolation": "none"}));
        assert!(
            read(thread_settings_path(&dir))
                .get("permissions")
                .is_none()
        );
        assert_eq!(
            edit_deny_rule(Path::new("/Users/me/repo")),
            "Edit(//Users/me/repo/**)"
        );
        assert_eq!(
            edit_deny_files(Path::new("/Users/me/chief/chief.db*")),
            "Edit(//Users/me/chief/chief.db*)"
        );
    }

    #[test]
    fn write_all_writes_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProjectConfig::new("Demo", "demo", None, vec![]);
        write_all(dir.path(), &config, Path::new("/bin/chief"), &[]).unwrap();
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
