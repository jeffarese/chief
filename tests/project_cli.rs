//! `chief init`, `chief up` and `chief status` with a temp home and the fake `claude`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use chief::config::ProjectConfig;
use chief::db;
use common::{Home, agent, stderr, stdout, trust_file};
use serde_json::{Value, json};

const SLUG: &str = "payments-latency";

fn version_ok(home: &Home) {
    home.fake.set("version.stdout", "2.1.310 (Claude Code)\n");
}

fn project_dir(home: &Home) -> PathBuf {
    home.path()
        .join("projects")
        .join(SLUG)
        .canonicalize()
        .unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Two repos with the same folder name; only the first is trusted.
fn repos(home: &Home) -> (PathBuf, PathBuf) {
    let a = home.path().join("work/api");
    let b = home.path().join("other/api");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    (a.canonicalize().unwrap(), b.canonicalize().unwrap())
}

fn init(home: &Home, a: &Path, b: &Path) -> std::process::Output {
    home.run(
        &[
            "init",
            "Payments Latency",
            "--goal",
            "Hold p95 under 200 ms",
            "--repo",
            a.to_str().unwrap(),
            "--repo",
            b.to_str().unwrap(),
        ],
        "",
    )
}

#[test]
fn init_creates_the_project() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    trust_file(&home.trust, &[&a]);

    let out = init(&home, &a, &b);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let dir = project_dir(&home);
    assert!(
        text.contains("Created project \"Payments Latency\" (payments-latency)"),
        "{text}"
    );
    // stdin isn't a terminal: no prompt, just the commands.
    assert!(!text.contains("[Y/n]"), "{text}");
    assert!(
        text.contains(&format!("cd {} && claude", dir.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("cd {} && claude", b.display())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("cd {} && claude", a.display())),
        "{text}"
    );
    assert!(
        text.ends_with("Next: chief up payments-latency\n"),
        "{text}"
    );

    let config = ProjectConfig::load(&dir.join("project.toml")).unwrap();
    assert_eq!(config.goal.as_deref(), Some("Hold p95 under 200 ms"));
    let names: Vec<_> = config
        .repos
        .iter()
        .map(|r| (r.name.as_str(), r.path.clone()))
        .collect();
    assert_eq!(names, [("api", a.clone()), ("api-2", b.clone())]);
    assert_eq!(config.threads.default_repo.as_deref(), Some("api"));
    assert!(
        std::fs::read_to_string(dir.join("instructions.md"))
            .unwrap()
            .starts_with("<!--")
    );
    assert!(
        std::fs::read_to_string(dir.join("memory/MEMORY.md"))
            .unwrap()
            .starts_with("# Memory")
    );

    // Generated files point at this binary and carry the test environment.
    let exe = Path::new(env!("CARGO_BIN_EXE_chief"))
        .canonicalize()
        .unwrap();
    let mcp = read_json(&dir.join("generated/mcp.json"));
    let server = &mcp["mcpServers"]["chief"];
    assert_eq!(server["command"], exe.to_str().unwrap());
    assert_eq!(server["args"], json!(["mcp", "--project", SLUG]));
    assert_eq!(server["env"]["CHIEF_HOME"], home.path().to_str().unwrap());
    assert_eq!(server["env"]["CHIEF_NO_NOTIFY"], "1");
    assert_eq!(
        server["env"]["CHIEF_CLAUDE_BIN"],
        home.fake.bin().to_str().unwrap()
    );
    let coord = read_json(&dir.join("generated/coordinator-settings.json"));
    assert_eq!(coord["worktree"]["bgIsolation"], "none");
    // The coordinator can't edit the repositories or its own generated files; memory and
    // instructions stay editable.
    assert_eq!(
        coord["permissions"]["deny"],
        json!([
            format!("Edit(/{}/**)", a.display()),
            format!("Edit(/{}/**)", b.display()),
            format!("Edit(/{}/generated/**)", dir.display()),
            format!("Edit(/{}/project.toml)", dir.display()),
            format!(
                "Edit(/{}/chief.db*)",
                home.path().canonicalize().unwrap().display()
            ),
        ])
    );
    let thread = read_json(&dir.join("generated/thread-settings.json"));
    assert!(thread.get("worktree").is_none());
    assert!(thread.get("permissions").is_none());

    let conn = home.db();
    assert_eq!(
        db::project_dir(&conn, SLUG).unwrap().as_deref(),
        dir.to_str()
    );

    // Same slug again.
    let again = init(&home, &a, &b);
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
}

/// With `CLAUDE_CONFIG_DIR` set (one folder per account), trust comes from that folder's
/// `.claude.json`, and every generated command carries the folder: each account has its own
/// supervisor, sessions and job state.
#[test]
fn init_follows_claude_config_dir() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    let account = home.path().join("claude-account");
    std::fs::create_dir_all(&account).unwrap();
    trust_file(&account.join(".claude.json"), &[&a, &b]);
    let mut cmd = home.command(&[
        "init",
        "Payments Latency",
        "--repo",
        a.to_str().unwrap(),
        "--repo",
        b.to_str().unwrap(),
    ]);
    cmd.env_remove("CHIEF_CLAUDE_CONFIG")
        .env("CLAUDE_CONFIG_DIR", &account);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let dir = project_dir(&home);
    // Only the new project folder is untrusted; the repos are trusted in the account's file.
    assert!(
        text.contains(&format!("cd {} && claude", dir.display())),
        "{text}"
    );
    for repo in [&a, &b] {
        assert!(
            !text.contains(&format!("cd {} && claude", repo.display())),
            "{text}"
        );
    }
    let mcp = read_json(&dir.join("generated/mcp.json"));
    assert_eq!(
        mcp["mcpServers"]["chief"]["env"]["CLAUDE_CONFIG_DIR"],
        account.to_str().unwrap()
    );
    let coord = std::fs::read_to_string(dir.join("generated/coordinator-settings.json")).unwrap();
    assert!(
        coord.contains(&format!("CLAUDE_CONFIG_DIR={}", account.display())),
        "{coord}"
    );
}

#[test]
fn init_rejects_bad_input_without_leaving_files() {
    let home = Home::new();
    version_ok(&home);
    let out = home.run(&["init", "Demo", "--repo", "/does/not/exist"], "");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("/does/not/exist"));
    assert!(!home.path().join("projects/demo").exists());

    let out = home.run(&["init", "!!!"], "");
    assert!(stderr(&out).contains("letter or digit"), "{}", stderr(&out));

    home.fake.set("version.stdout", "2.1.300 (Claude Code)\n");
    let out = home.run(&["init", "Demo"], "");
    assert!(stderr(&out).contains("claude update"), "{}", stderr(&out));
    assert!(!home.path().join("projects/demo").exists());

    // No repos is fine: threads then need an explicit absolute repo path.
    version_ok(&home);
    let out = home.run(&["init", "Demo", "--no-trust-prompt"], "");
    assert!(out.status.success(), "{}", stderr(&out));
}

/// Hook commands written by init run from any cwd, without CHIEF_HOME in the environment.
#[test]
fn generated_hook_commands_reach_this_home() {
    let home = Home::new();
    version_ok(&home);
    assert!(home.run(&["init", "Demo"], "").status.success());
    let dir = home.path().join("projects/demo");
    let settings = read_json(&dir.join("generated/thread-settings.json"));
    let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.contains("CHIEF_GH_BIN="), "{command}");
    {
        let conn = home.db();
        let row = db::SessionRow::new("demo-x-1111", "demo", chief::state::Role::Thread, 0);
        db::insert_session(&conn, &row).unwrap();
    }
    let payload = json!({"session_id": "s", "session_title": "demo-x-1111", "cwd": "/", "last_assistant_message": "hi"});
    assert!(run_hook(command, &payload).status.success());
    let row = db::get_session(&home.db(), "demo-x-1111").unwrap().unwrap();
    assert_eq!(row.last_report.as_deref(), Some("hi"));

    // The guard exits 0 whether it allows (no output) or denies (the deny JSON).
    let guard = settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let out = run_hook(
        guard,
        &json!({"session_id": "s", "tool_name": "Read", "tool_input": {}}),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let out = run_hook(
        guard,
        &json!({"session_id": "s", "tool_name": "ListAgents", "tool_input": {}}),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let decision: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");

    // A guard whose chief is gone blocks the call (exit 2) and says why; the state hooks don't.
    let exe = Path::new(env!("CARGO_BIN_EXE_chief"))
        .canonicalize()
        .unwrap();
    let missing = guard.replace(exe.to_str().unwrap(), "/nonexistent/chief");
    let out = run_hook(&missing, &json!({"tool_name": "SendMessage"}));
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).ends_with(
            "chief's message guard could not run (/nonexistent/chief); message blocked. Run chief \
             up again.\n"
        ),
        "{}",
        stderr(&out)
    );
    let out = run_hook(
        &command.replace(exe.to_str().unwrap(), "/nonexistent/chief"),
        &payload,
    );
    assert_eq!(out.status.code(), Some(127), "fails open: {}", stderr(&out));
}

/// With only `XDG_DATA_HOME` set, the generated files still carry the resolved home: sessions
/// started by Claude Code's supervisor don't have our `XDG_DATA_HOME`.
#[test]
fn generated_files_carry_the_resolved_home() {
    let home = Home::new();
    version_ok(&home);
    let xdg = home.path().join("xdg");
    let root = xdg.join("chief");
    let out = home
        .command(&["init", "Demo"])
        .env_remove("CHIEF_HOME")
        .env("XDG_DATA_HOME", &xdg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let dir = root.join("projects/demo");
    let mcp = read_json(&dir.join("generated/mcp.json"));
    assert_eq!(
        mcp["mcpServers"]["chief"]["env"]["CHIEF_HOME"],
        root.to_str().unwrap()
    );
    for file in ["thread-settings.json", "coordinator-settings.json"] {
        let settings = read_json(&dir.join("generated").join(file));
        for (event, entries) in settings["hooks"].as_object().unwrap() {
            for entry in entries.as_array().unwrap() {
                let command = entry["hooks"][0]["command"].as_str().unwrap();
                assert!(
                    command.starts_with(&format!("CHIEF_HOME={} ", root.display())),
                    "{file} {event}: {command}"
                );
            }
        }
    }
    // The hook reaches this home without XDG_DATA_HOME (or CHIEF_HOME) in its environment.
    {
        let conn = db::open(&root.join("chief.db")).unwrap();
        let row = db::SessionRow::new("demo-x-1111", "demo", chief::state::Role::Thread, 0);
        db::insert_session(&conn, &row).unwrap();
    }
    let settings = read_json(&dir.join("generated/thread-settings.json"));
    let stop = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let payload = json!({"session_id": "s", "session_title": "demo-x-1111", "cwd": "/", "last_assistant_message": "hi"});
    assert!(run_hook(stop, &payload).status.success());
    let row = db::get_session(&db::open(&root.join("chief.db")).unwrap(), "demo-x-1111")
        .unwrap()
        .unwrap();
    assert_eq!(row.last_report.as_deref(), Some("hi"));
}

/// Runs a generated hook command through `sh` from `/`, without chief's variables in the
/// environment, as Claude Code would.
fn run_hook(command: &str, payload: &Value) -> std::process::Output {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir("/")
        .env_remove("CHIEF_HOME")
        .env_remove("CHIEF_CLAUDE_BIN")
        .env_remove("CHIEF_GH_BIN")
        .env_remove("CHIEF_REX_BIN")
        .env_remove("CHIEF_CLAUDE_CONFIG")
        .env_remove("CHIEF_NO_NOTIFY")
        .env_remove("XDG_DATA_HOME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The fake's next `agents` call number.
fn next_agents_call(home: &Home) -> usize {
    std::fs::read_to_string(home.fake.dir.path().join("agents.count"))
        .map(|s| s.trim().parse::<usize>().unwrap())
        .unwrap_or(0)
        + 1
}

#[test]
fn up_starts_then_leaves_then_respawns_the_coordinator() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    assert!(init(&home, &a, &b).status.success());
    let dir = project_dir(&home);

    // The project folder isn't trusted yet.
    let out = home.run(&["up", SLUG], "");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("accept the trust prompt"),
        "{}",
        stderr(&out)
    );
    assert!(home.fake.calls_of("--bg").is_empty());
    assert!(db::get_coordinator(&home.db(), SLUG).unwrap().is_none());

    trust_file(&home.trust, &[&dir]);
    let name = "chief-payments-latency";
    let awake = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    home.fake
        .set(&format!("agents.stdout.{}", next_agents_call(&home)), "[]")
        .set(
            "bg.stdout",
            "backgrounded · c0ffee · chief-payments-latency\n",
        )
        .set("agents.stdout", &json!([awake]).to_string());
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Coordinator chief-payments-latency started."),
        "{text}"
    );
    assert!(text.contains("claude attach c0ffee"), "{text}");

    let calls = home.fake.calls();
    let i = calls.iter().position(|c| c[0] == "--bg").unwrap();
    let bg = &calls[i];
    let s = |x: &str| x.to_string();
    let prompt = bg.last().unwrap();
    assert!(
        prompt.starts_with(
            "You are the coordinator of the chief project \"Payments Latency\". Goal: Hold p95"
        ),
        "{prompt}"
    );
    assert!(prompt.contains(&format!("{}/memory/", dir.display())));
    assert_eq!(
        bg[..bg.len() - 1],
        [
            s("--bg"),
            s("--name"),
            s(name),
            s("--model"),
            s("opus"),
            s("--effort"),
            s("low"),
            s("--permission-mode"),
            s("auto"),
            s("--settings"),
            dir.join("generated/coordinator-settings.json")
                .to_string_lossy()
                .into_owned(),
            s("--mcp-config"),
            dir.join("generated/mcp.json")
                .to_string_lossy()
                .into_owned(),
            s("--disallowedTools"),
            s("Bash"),
            s("--add-dir"),
            a.to_string_lossy().into_owned(),
            s("--add-dir"),
            b.to_string_lossy().into_owned(),
            s("--append-system-prompt"),
        ]
    );
    assert_eq!(home.fake.cwd(i), dir);
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(
        (row.short_id.as_deref(), row.session_id.as_deref(), row.pid),
        (Some("c0ffee"), Some("coord-sid"), Some(74112))
    );

    // Running: left alone.
    let out = home.run(&["up", SLUG], "");
    assert!(stdout(&out).contains("already running"), "{}", stdout(&out));
    assert_eq!(home.fake.calls_of("--bg").len(), 1);
    assert!(home.fake.calls_of("respawn").is_empty());

    // Process gone: respawned, then it shows up with a pid.
    let asleep = agent("c0ffee", "coord-sid", name, "done", None);
    home.fake.set(
        &format!("agents.stdout.{}", next_agents_call(&home)),
        &json!([asleep]).to_string(),
    );
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("respawned"), "{}", stdout(&out));
    assert_eq!(home.fake.calls_of("respawn"), [vec!["respawn", "c0ffee"]]);
    assert_eq!(home.fake.calls_of("--bg").len(), 1);

    // status shows the coordinator and the threads by group.
    {
        let conn = home.db();
        let mut t = db::SessionRow::new(
            "payments-latency-fix-1111",
            SLUG,
            chief::state::Role::Thread,
            db::now_ms(),
        );
        t.title = Some("Fix".into());
        t.short_id = Some("t1".into());
        t.session_id = Some("t-sid".into());
        t.last_report = Some("Fixed it.\nBranch x.".into());
        db::insert_session(&conn, &t).unwrap();
    }
    home.fake.set(
        "agents.stdout",
        &json!([
            awake,
            agent(
                "t1",
                "t-sid",
                "payments-latency-fix-1111",
                "blocked",
                Some(5)
            )
        ])
        .to_string(),
    );
    let out = home.run(&["status", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("payments-latency\n  coordinator chief-payments-latency (idle, claude attach c0ffee)\n  Waiting on you (1)\n    payments-latency-fix-1111 · \"Fix\" · question · "), "{text}");
    assert!(text.contains("      Fixed it. Branch x.\n"), "{text}");
    let all = home.run(&["status"], "");
    assert_eq!(stdout(&all), text);
    assert!(!home.run(&["status", "nope"], "").status.success());
}

/// The respawn `chief up` and the pane use rewrites `generated/*` from project.toml first: a
/// respawned session reads its hooks again.
#[test]
fn respawn_rewrites_the_generated_files_first() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    assert!(init(&home, &a, &b).status.success());
    let dir = project_dir(&home);
    let settings = dir.join("generated/coordinator-settings.json");
    std::fs::write(&settings, "{}").unwrap();
    let conn = home.db();
    let name = "chief-payments-latency";
    let mut row = db::SessionRow::new(name, SLUG, chief::state::Role::Coordinator, db::now_ms());
    row.short_id = Some("c0ffee".into());
    row.session_id = Some("coord-sid".into());
    db::insert_session(&conn, &row).unwrap();
    home.fake.set(
        "agents.stdout",
        &json!([agent("c0ffee", "coord-sid", name, "done", Some(7))]).to_string(),
    );
    let config = ProjectConfig::load(&dir.join("project.toml")).unwrap();
    let woke = chief::project::respawn(
        &conn,
        &home.fake.claude(),
        &dir,
        &config,
        &row,
        std::time::Duration::from_millis(500),
    )
    .unwrap();
    assert_eq!(woke.pid, Some(7));
    assert_eq!(home.fake.calls_of("respawn"), [vec!["respawn", "c0ffee"]]);
    let v = read_json(&settings);
    assert_eq!(
        v["hooks"]["PreToolUse"][1]["matcher"],
        "SendMessage|ListAgents"
    );
    assert_eq!(v["permissions"]["deny"].as_array().unwrap().len(), 5);

    // Can't rewrite: no respawn.
    std::fs::remove_dir_all(dir.join("generated")).unwrap();
    std::fs::write(dir.join("generated"), "not a folder").unwrap();
    let err = chief::project::respawn(
        &conn,
        &home.fake.claude(),
        &dir,
        &config,
        &row,
        std::time::Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("generated"), "{err:#}");
    assert_eq!(home.fake.calls_of("respawn").len(), 1);
}

/// A coordinator spawn that printed no id may have started the session anyway: `up` adopts it
/// rather than deleting the row (the next `up` would then refuse the name as taken).
#[test]
fn up_adopts_a_coordinator_that_started_anyway() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    assert!(init(&home, &a, &b).status.success());
    trust_file(&home.trust, &[&project_dir(&home)]);
    let name = "chief-payments-latency";
    // An old, stopped session with the same name is not the new one.
    let old = agent("01d", "old-sid", name, "stopped", None);
    let new = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    home.fake
        .set(
            &format!("agents.stdout.{}", next_agents_call(&home)),
            &json!([old]).to_string(),
        )
        .set("bg.stdout", "Something new happened\n")
        .set("agents.stdout", &json!([old, new]).to_string());
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("started anyway"), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("claude attach c0ffee"),
        "{}",
        stdout(&out)
    );
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(
        (row.short_id.as_deref(), row.session_id.as_deref(), row.pid),
        (Some("c0ffee"), Some("coord-sid"), Some(74112))
    );
    assert_eq!(home.fake.calls_of("--bg").len(), 1);
}

/// A coordinator row that a failed spawn left without a short id (chief couldn't tell whether
/// it started) is adopted by the next `up` when its session is listed, instead of refusing the
/// name as taken.
#[test]
fn up_adopts_a_coordinator_left_by_an_unclear_spawn() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    assert!(init(&home, &a, &b).status.success());
    trust_file(&home.trust, &[&project_dir(&home)]);
    let name = "chief-payments-latency";
    {
        let row = db::SessionRow::new(name, SLUG, chief::state::Role::Coordinator, db::now_ms());
        db::insert_session(&home.db(), &row).unwrap();
    }
    home.fake.set(
        "agents.stdout",
        &json!([agent("c0ffee", "coord-sid", name, "done", Some(74112))]).to_string(),
    );
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Coordinator chief-payments-latency already running."),
        "{}",
        stdout(&out)
    );
    assert!(home.fake.calls_of("--bg").is_empty());
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(row.short_id.as_deref(), Some("c0ffee"));
}

/// §2: the cleanup state comes after the PR, and every thread line ends with how long ago it was
/// active.
#[test]
fn status_lines_show_cleanup_then_end_with_the_age() {
    use chief::state::Role;
    let now = 10 * 60 * 1000;
    let thread = |name: &str| {
        let mut t = db::SessionRow::new(name, SLUG, Role::Thread, 0);
        t.resolved = true;
        t.group_name = "resolved".into();
        t.branch = Some("worktree-x".into());
        t.pr_url = Some("https://github.com/o/r/pull/3".into());
        t.last_activity_at = now - 5 * 60 * 1000;
        t
    };
    let mut cleaned = thread("payments-latency-a-1111");
    cleaned.cleaned_at = Some(1);
    let mut blocked = thread("payments-latency-b-2222");
    blocked.cleanup_blocked = Some("2 unpushed commits".into());
    let text = chief::project::render_status(SLUG, &[cleaned, blocked], now);
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("    ")).collect();
    assert_eq!(lines.len(), 2, "{text}");
    for (line, state) in lines
        .iter()
        .zip(["cleaned up", "cleanup blocked: 2 unpushed commits"])
    {
        assert!(
            line.contains(&format!("PR https://github.com/o/r/pull/3 · {state} · ")),
            "{line}"
        );
        assert!(line.ends_with(" ago"), "{line}");
    }
}

/// A project whose coordinator runs (started by a first `up` that found no Rex).
fn running_project(home: &Home) -> PathBuf {
    version_ok(home);
    let (a, b) = repos(home);
    assert!(init(home, &a, &b).status.success());
    let dir = project_dir(home);
    trust_file(&home.trust, &[&dir]);
    home.fake
        .set(&format!("agents.stdout.{}", next_agents_call(home)), "[]")
        .set(
            "bg.stdout",
            "backgrounded · c0ffee · chief-payments-latency\n",
        )
        .set(
            "agents.stdout",
            &json!([agent(
                "c0ffee",
                "coord-sid",
                "chief-payments-latency",
                "done",
                Some(74112)
            )])
            .to_string(),
        );
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("No Rex layout: "), "{text}");
    assert!(text.contains("no Rex server is running"), "{text}");
    assert!(text.contains("claude attach c0ffee"), "{text}");
    assert!(
        text.contains("Board, in another terminal: chief board --project payments-latency"),
        "{text}"
    );
    home.rex
        .unset("list.exit")
        .unset("list.stderr")
        .unset("new.exit")
        .unset("new.stderr");
    dir
}

/// `env K=V… <chief> <sub> --project <slug>`, as the layout's blocks run it.
fn block_command(home: &Home, sub: &str) -> Vec<String> {
    let exe = Path::new(env!("CARGO_BIN_EXE_chief"))
        .canonicalize()
        .unwrap();
    let mut cmd = vec![
        "env".to_string(),
        format!("CHIEF_HOME={}", home.path().display()),
        format!("CHIEF_CLAUDE_BIN={}", home.fake.bin().display()),
        format!("CHIEF_GH_BIN={}", home.gh.bin().display()),
        format!("CHIEF_REX_BIN={}", home.rex.bin().display()),
        format!("CHIEF_CLAUDE_CONFIG={}", home.trust.display()),
        "CHIEF_NO_NOTIFY=1".to_string(),
        exe.to_string_lossy().into_owned(),
    ];
    cmd.extend([sub, "--project", SLUG].map(String::from));
    cmd
}

#[test]
fn up_opens_then_reuses_the_rex_layout() {
    let home = Home::new();
    let dir = running_project(&home);
    let d = dir.to_string_lossy().into_owned();
    home.rex
        .set(
            "list.stdout",
            // Labelled like chief's, but chief never created it: not even looked at.
            r#"{"sessions": [{"session_id": "session:theirs", "label": "chief-payments-latency"}]}"#,
        )
        .set(
            "new.stdout",
            r#"{"session_id": "session:new", "initial_windows": [{"window_id": "window:1", "block_ids": ["block:main"]}]}"#,
        )
        .set(
            "run.stdout",
            r#"{"window_id": "window:1", "block_ids": ["block:board"], "revision": 2}"#,
        );
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Coordinator chief-payments-latency already running."),
        "{text}"
    );
    assert!(
        text.contains("Rex session chief-payments-latency opened."),
        "{text}"
    );
    assert!(!text.contains("claude attach"), "{text}");
    assert!(!text.contains("didn't switch"), "{text}");

    let mut new = [
        "new",
        "chief-payments-latency",
        "--autostart=false",
        "--cwd",
        &d,
    ]
    .map(String::from)
    .to_vec();
    new.extend(["--keep-open", "--json", "--"].map(String::from));
    new.extend(block_command(&home, "pane"));
    let mut run = [
        "run",
        "--autostart=false",
        "-s",
        "session:new",
        "--label",
        "board",
        "--focus=false",
        "-b",
        "block:main",
        "--split",
        "right",
        "--ratio",
        "32",
        "--cwd",
        &d,
        "--keep-open",
        "--json",
        "--",
    ]
    .map(String::from)
    .to_vec();
    run.extend(block_command(&home, "board"));
    let calls: Vec<Vec<String>> = home.rex.calls().into_iter().skip(1).collect();
    assert_eq!(
        calls,
        [
            new,
            [
                "block",
                "rename",
                "--autostart=false",
                "-s",
                "session:new",
                "block:main",
                "main"
            ]
            .map(String::from)
            .to_vec(),
            run,
            ["focus", "block:main", "--autostart=false"]
                .map(String::from)
                .to_vec(),
            [
                "do",
                "session.select",
                "session_id=session:new",
                "--autostart=false"
            ]
            .map(String::from)
            .to_vec(),
        ]
    );
    assert_eq!(
        db::get_rex_session(&home.db(), SLUG).unwrap().as_deref(),
        Some("session:new")
    );

    // Open and running: shown again, nothing created. This time no single app takes it.
    // One canned reply serves both `block ls` and `block call process`.
    home.rex
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:new", "label": "chief-payments-latency"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}],
                "child": {"pid": 1}}"#,
        )
        .set("do.exit", "1");
    let before = home.rex.calls().len();
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Rex session chief-payments-latency already open."),
        "{text}"
    );
    assert!(
        text.contains("rex -C <app> do session.select session_id=session:new"),
        "{text}"
    );
    let firsts: Vec<String> = home.rex.calls()[before..]
        .iter()
        .map(|c| c[..2].join(" "))
        .collect();
    assert_eq!(
        firsts,
        [
            "list --autostart=false",
            "block ls",
            "block call",
            "block ls",
            "block call",
            "focus block:main",
            "do session.select",
        ]
    );
    // The first `up` without a Rex server, then the one that created it.
    assert_eq!(home.rex.calls_of("new").len(), 2);
    assert_eq!(home.rex.calls_of("run").len(), 1);
}

#[test]
fn down_closes_the_layout_and_stops_sessions() {
    let home = Home::new();
    running_project(&home);
    {
        let conn = home.db();
        for (name, short) in [
            ("payments-latency-a-1111", "t1"),
            ("payments-latency-b-2222", "t2"),
        ] {
            let mut t = db::SessionRow::new(name, SLUG, chief::state::Role::Thread, db::now_ms());
            t.short_id = Some(short.into());
            t.session_id = Some(format!("{short}-sid"));
            db::insert_session(&conn, &t).unwrap();
        }
        db::set_pane_showing(&conn, SLUG, Some("chief-payments-latency")).unwrap();
        db::set_rex_session(&conn, SLUG, Some("session:7")).unwrap();
        // The pane showed a thread: the next `chief up` would wake it right after
        // `--stop-threads` stopped it.
        db::set_pane_target(&conn, SLUG, Some("payments-latency-a-1111")).unwrap();
    }
    home.fake.set(
        "agents.stdout",
        &json!([
            agent(
                "c0ffee",
                "coord-sid",
                "chief-payments-latency",
                "done",
                Some(74112)
            ),
            agent(
                "t1",
                "t1-sid",
                "payments-latency-a-1111",
                "working",
                Some(5)
            ),
            agent("t2", "t2-sid", "payments-latency-b-2222", "done", None),
        ])
        .to_string(),
    );
    home.rex
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:theirs", "label": "chief-payments-latency"},
                             {"session_id": "session:7", "label": "chief-payments-latency"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        );

    let out = home.run(&["down", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text,
        "Closed Rex session chief-payments-latency.\n\
         Stopped coordinator chief-payments-latency.\n\
         1 thread is still running; chief down payments-latency --stop-threads stops them.\n"
    );
    assert_eq!(
        home.rex.calls_of("kill"),
        [vec!["kill", "session:7", "--autostart=false"]]
    );
    assert_eq!(home.fake.calls_of("stop"), [vec!["stop", "c0ffee"]]);
    assert_eq!(db::get_pane_showing(&home.db(), SLUG).unwrap(), None);
    assert_eq!(db::get_rex_session(&home.db(), SLUG).unwrap(), None);
    assert_eq!(
        db::get_pane_target(&home.db(), SLUG).unwrap(),
        None,
        "back to the coordinator"
    );

    // With --stop-threads only the live thread is stopped; nothing is ever removed. The layout
    // is gone now.
    home.rex.set(
        "list.stdout",
        r#"{"sessions": [{"session_id": "session:theirs", "label": "work"}]}"#,
    );
    let out = home.run(&["down", SLUG, "--stop-threads"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("No Rex session chief-payments-latency was open.\n"),
        "{text}"
    );
    assert!(
        text.contains("Stopped thread payments-latency-a-1111."),
        "{text}"
    );
    assert!(!text.contains("payments-latency-b-2222"), "{text}");
    assert_eq!(
        home.fake.calls_of("stop"),
        [
            vec!["stop", "c0ffee"],
            vec!["stop", "c0ffee"],
            vec!["stop", "t1"]
        ]
    );
    assert!(home.fake.calls_of("rm").is_empty());
    assert_eq!(home.rex.calls_of("kill").len(), 1);

    // chief's session now holds a block of the user's too: only main and board close.
    db::set_rex_session(&home.db(), SLUG, Some("session:7")).unwrap();
    home.rex
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:7", "label": "chief-payments-latency"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:logs", "label": "logs"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        );
    let out = home.run(&["down", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with(
            "Closed chief's blocks in Rex session chief-payments-latency; it holds other blocks \
             too, so it stays open.\n"
        ),
        "{text}"
    );
    assert_eq!(home.rex.calls_of("kill").len(), 1);
    let closed: Vec<Vec<String>> = home
        .rex
        .calls_of("block")
        .into_iter()
        .filter(|c| c[1] == "close")
        .collect();
    assert_eq!(
        closed,
        [
            vec!["block", "close", "block:main", "--autostart=false"],
            vec!["block", "close", "block:board", "--autostart=false"]
        ]
    );
    assert_eq!(
        db::get_rex_session(&home.db(), SLUG).unwrap().as_deref(),
        Some("session:7")
    );
}
