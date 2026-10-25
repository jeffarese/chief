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
    {
        let conn = home.db();
        let row = db::SessionRow::new("demo-x-1111", "demo", chief::state::Role::Thread, 0);
        db::insert_session(&conn, &row).unwrap();
    }
    let payload = json!({"session_id": "s", "session_title": "demo-x-1111", "cwd": "/", "last_assistant_message": "hi"});
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir("/")
        .env_remove("CHIEF_HOME")
        .env_remove("CHIEF_CLAUDE_BIN")
        .env_remove("CHIEF_CLAUDE_CONFIG")
        .env_remove("CHIEF_NO_NOTIFY")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    let row = db::get_session(&home.db(), "demo-x-1111").unwrap().unwrap();
    assert_eq!(row.last_report.as_deref(), Some("hi"));
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
