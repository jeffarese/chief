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
    let thread = read_json(&dir.join("generated/thread-settings.json"));
    assert!(thread.get("worktree").is_none());

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
    home.rex.unset("list.exit").unset("list.stderr");
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
            r#"{"sessions": [{"session_id": "session:theirs", "label": "chief-payments-latency-x"}]}"#,
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
            ["list", "--autostart=false", "--json"]
                .map(String::from)
                .to_vec(),
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
    assert_eq!(home.rex.calls_of("new").len(), 1);
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
    home.rex.set(
        "list.stdout",
        r#"{"sessions": [{"session_id": "session:theirs", "label": "work"},
                         {"session_id": "session:7", "label": "chief-payments-latency"}]}"#,
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
}
