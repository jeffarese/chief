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
    home.fake.set("version.stdout", "2.1.316 (Claude Code)\n");
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
    // chief made the project folder, so it trusts it without asking (§3 "Trust").
    assert!(
        text.contains(&format!("Trusted {}.", dir.display())),
        "{text}"
    );
    assert!(chief::claude::is_trusted_in(&home.trust, &dir).unwrap());
    // The user's untrusted repo is listed; stdin isn't a terminal, so no prompt, just the fix.
    assert!(!text.contains("[Y/n]"), "{text}");
    assert!(text.contains(&format!("\n  {}\n", b.display())), "{text}");
    assert!(!text.contains(&format!("\n  {}\n", a.display())), "{text}");
    assert!(!chief::claude::is_trusted_in(&home.trust, &b).unwrap());
    assert!(
        text.contains("To trust them later: chief trust payments-latency"),
        "{text}"
    );
    assert!(
        text.ends_with("Next: chief up payments-latency (or run chief and pick it)\n"),
        "{text}"
    );

    // `chief trust` is the yes: it marks the rest, and says when nothing is left.
    let out = home.run(&["trust", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("Trusted {}.\n", b.display()));
    assert!(chief::claude::is_trusted_in(&home.trust, &b).unwrap());
    assert!(chief::claude::is_trusted_in(&home.trust, &a).unwrap());
    let out = home.run(&["trust", SLUG], "");
    assert_eq!(
        stdout(&out),
        "Every folder of payments-latency is trusted already.\n"
    );

    let config = ProjectConfig::load(&dir.join("project.toml")).unwrap();
    assert_eq!(config.goal.as_deref(), Some("Hold p95 under 200 ms"));
    // Made without CLAUDE_CONFIG_DIR: the default account, and no key for it.
    assert_eq!(config.claude_config_dir, None);
    assert!(
        !std::fs::read_to_string(dir.join("project.toml"))
            .unwrap()
            .contains("claude_config_dir")
    );
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
    // The coordinator can't edit the repositories, any project's generated files or
    // project.toml, its own .claude folder, the account's user settings or the database; memory
    // and instructions stay editable.
    let root = home.path().canonicalize().unwrap();
    let user_claude = PathBuf::from(std::env::var("HOME").unwrap()).join(".claude");
    let user_claude = user_claude.canonicalize().unwrap_or(user_claude);
    assert_eq!(
        coord["permissions"]["deny"],
        json!([
            format!("Edit(/{}/**)", a.display()),
            format!("Edit(/{}/**)", b.display()),
            format!("Edit(/{}/projects/*/generated/**)", root.display()),
            format!("Edit(/{}/projects/*/project.toml)", root.display()),
            format!("Edit(/{}/.claude/**)", dir.display()),
            format!("Edit(/{}/settings*.json)", user_claude.display()),
            format!("Edit(/{}/chief.db*)", root.display()),
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

/// With agp, threads may run in any pool account, each with its own trust (§3): `init` lists
/// every (account, repo) pair that isn't trusted yet, leaving out the project's own account
/// (listed as before) and accounts outside the pool; `chief trust` marks them.
#[test]
fn init_checks_trust_in_every_pool_account() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    trust_file(&home.trust, &[&a, &b]);
    let cpb = common::account(home.path(), "claude-cpb");
    let cpe = common::account(home.path(), "claude-cpe");
    trust_file(&cpb.join(".claude.json"), &[&a]);
    trust_file(&cpe.join(".claude.json"), &[&a, &b]);
    let agp = home.install_agp();
    common::agp_accounts(
        agp,
        &[
            common::agp_view("default", "Main", true, 0.9),
            common::agp_view("cpb", "PB2", true, 0.8),
            common::agp_view("cpe", "PB3", true, 0.7),
            common::agp_view("laura", "PB4", false, 0.0),
        ],
        &[("default", None), ("cpb", Some(&cpb)), ("cpe", Some(&cpe))],
    );
    let out = init(&home, &a, &b);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Threads can also run in these agp accounts: Main, PB2, PB3."),
        "{text}"
    );
    assert!(
        text.contains(&format!("  {} (in PB2)\n", b.display())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("  {} (in PB2)", a.display())),
        "{text}"
    );
    assert!(!text.contains("(in PB3)"), "{text}");
    assert!(!text.contains("PB4"), "not in the pool: {text}");
    assert!(
        !text.contains("(in Main)"),
        "the project's own account: {text}"
    );
    assert!(
        text.contains("To trust them later: chief trust payments-latency"),
        "{text}"
    );
    let out = home.run(&["trust", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("Trusted {} (in PB2).", b.display())),
        "{}",
        stdout(&out)
    );
    assert!(chief::claude::is_trusted_in(&cpb.join(".claude.json"), &b).unwrap());

    // A project on an agp account: agp runs without this shell's CLAUDE_CONFIG_DIR, so the
    // default account's folder is never taken for the project's.
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    let cpe = common::account(home.path(), "claude-cpe");
    trust_file(&cpe.join(".claude.json"), &[&a, &b]);
    let agp = home.install_agp();
    common::agp_accounts(
        agp,
        &[
            common::agp_view("default", "Main", true, 0.9),
            common::agp_view("cpe", "PB3", true, 0.7),
        ],
        &[("default", None), ("cpe", Some(&cpe))],
    );
    let out = home
        .command(&[
            "init",
            "Payments Latency",
            "--repo",
            a.to_str().unwrap(),
            "--repo",
            b.to_str().unwrap(),
        ])
        .env("CLAUDE_CONFIG_DIR", &cpe)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!("  {} (in Main)", a.display())),
        "{text}"
    );
    assert!(
        !text.contains("(in PB3)"),
        "the project's own account: {text}"
    );
    let execs: Vec<_> = agp
        .accounts()
        .into_iter()
        .filter(|(first, _)| first == "exec")
        .collect();
    assert_eq!(execs.len(), 2, "{execs:?}");
    assert!(execs.iter().all(|(_, env)| env == "unset"), "{execs:?}");

    // Without agp, nothing changes.
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    trust_file(&home.trust, &[&a, &b]);
    let text = stdout(&init(&home, &a, &b));
    assert!(!text.contains("agp"), "{text}");
}

/// With `CLAUDE_CONFIG_DIR` set (one folder per account), the project belongs to that account:
/// project.toml records it, trust comes from that folder's `.claude.json`, and every generated
/// command carries the folder: each account has its own supervisor, sessions and job state.
#[test]
fn init_follows_claude_config_dir() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    let account = common::account(home.path(), "claude-account");
    trust_file(&account.join(".claude.json"), &[&a, &b]);
    // The default account's trust file trusts everything: it doesn't count for this project.
    trust_file(&home.trust, &[&a, &b, home.path()]);
    let mut cmd = home.command(&[
        "init",
        "Payments Latency",
        "--repo",
        a.to_str().unwrap(),
        "--repo",
        b.to_str().unwrap(),
    ]);
    cmd.env("CLAUDE_CONFIG_DIR", &account);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let dir = project_dir(&home);
    let config = ProjectConfig::load(&dir.join("project.toml")).unwrap();
    assert_eq!(config.claude_config_dir.as_deref(), Some(account.as_path()));
    // Only the new project folder was untrusted, and chief trusts it in the account's own file;
    // the repos are trusted there already.
    assert!(
        text.contains(&format!("Trusted {}.", dir.display())),
        "{text}"
    );
    assert!(chief::claude::is_trusted_in(&account.join(".claude.json"), &dir).unwrap());
    assert!(!text.contains("chief trust"), "{text}");
    assert!(
        home.fake
            .accounts()
            .iter()
            .all(|(_, env)| env == account.to_str().unwrap()),
        "{:?}",
        home.fake.accounts()
    );
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

/// The reviewer's probe: `chief up` from a shell set for another account (or none). The project
/// keeps its account: every Claude Code call runs with it, trust is checked in its file, and
/// stderr says so once.
#[test]
fn up_runs_every_claude_call_in_the_projects_account() {
    let home = Home::new();
    version_ok(&home);
    let carlos = common::account(home.path(), "claude-carlos");
    let mut cmd = home.command(&["init", "Payments Latency"]);
    cmd.env("CLAUDE_CONFIG_DIR", &carlos);
    assert!(cmd.output().unwrap().status.success());
    let dir = project_dir(&home);
    trust_file(&carlos.join(".claude.json"), &[&dir]);
    let name = "chief-payments-latency";
    let mut ours = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    ours["cwd"] = dir.to_string_lossy().into_owned().into();
    std::fs::write(
        carlos.join("fake-bg.stdout"),
        format!("backgrounded · c0ffee · {name}\n"),
    )
    .unwrap();
    // Carlos's supervisor lists nothing before the spawn, then the new coordinator; the default
    // account's lists nothing at all.
    home.fake.set("agents.stdout", "[]");
    std::fs::write(
        carlos.join(format!("fake-agents.stdout.{}", next_agents_call(&home))),
        "[]",
    )
    .unwrap();
    std::fs::write(carlos.join("fake-agents.stdout"), json!([ours]).to_string()).unwrap();
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(row.short_id.as_deref(), Some("c0ffee"));
    let accounts = home.fake.accounts();
    assert!(accounts.iter().any(|(first, _)| first == "--bg"));
    for (first, env) in &accounts {
        assert_eq!(env, carlos.to_str().unwrap(), "{first}");
    }
    let err = stderr(&out);
    assert_eq!(
        err.matches("belongs to the Claude Code account").count(),
        1,
        "{err}"
    );
    assert!(err.contains(&carlos.display().to_string()), "{err}");
    // Without a layout it prints chief's own commands, which take the account from
    // project.toml: nothing account-specific to type.
    assert!(
        stdout(&out).contains("  chief pane --project payments-latency\n"),
        "{}",
        stdout(&out)
    );

    // Run from the project's own account: nothing to say.
    let mut cmd = home.command(&["up", SLUG]);
    cmd.env("CLAUDE_CONFIG_DIR", &carlos);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("belongs to the Claude Code account"),
        "{}",
        stderr(&out)
    );
}

/// A project without the key belongs to the default account, even when chief runs under another.
#[test]
fn a_project_without_an_account_runs_in_the_default_one() {
    let home = Home::new();
    version_ok(&home);
    assert!(home.run(&["init", "Payments Latency"], "").status.success());
    let dir = project_dir(&home);
    trust_file(&home.trust, &[&dir]);
    let carlos = common::account(home.path(), "claude-carlos");
    let mut ours = agent(
        "c0ffee",
        "coord-sid",
        "chief-payments-latency",
        "done",
        Some(7),
    );
    ours["cwd"] = dir.to_string_lossy().into_owned().into();
    {
        let mut row = db::SessionRow::new(
            "chief-payments-latency",
            SLUG,
            chief::state::Role::Coordinator,
            db::now_ms(),
        );
        row.short_id = Some("c0ffee".into());
        db::insert_session(&home.db(), &row).unwrap();
    }
    home.fake.set("agents.stdout", &json!([ours]).to_string());
    std::fs::write(carlos.join("fake-agents.stdout"), "[]").unwrap();
    let mut cmd = home.command(&["up", SLUG]);
    cmd.env("CLAUDE_CONFIG_DIR", &carlos);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("already running"), "{}", stdout(&out));
    for (first, env) in home.fake.accounts() {
        assert_eq!(env, "unset", "{first}");
    }
    let err = stderr(&out);
    assert!(
        err.contains("belongs to the default Claude Code account"),
        "{err}"
    );
    let mcp = read_json(&dir.join("generated/mcp.json"));
    assert!(
        mcp["mcpServers"]["chief"]["env"]
            .get("CLAUDE_CONFIG_DIR")
            .is_none(),
        "{mcp}"
    );
}

/// `chief status` across projects of two accounts: one `claude agents` per account, each
/// project synced with its own.
#[test]
fn status_takes_one_snapshot_per_account() {
    let home = Home::new();
    version_ok(&home);
    assert!(home.run(&["init", "Alpha"], "").status.success());
    assert!(home.run(&["init", "Gamma"], "").status.success());
    let carlos = common::account(home.path(), "claude-carlos");
    let mut cmd = home.command(&["init", "Beta"]);
    cmd.env("CLAUDE_CONFIG_DIR", &carlos);
    assert!(cmd.output().unwrap().status.success());
    let conn = home.db();
    for (slug, short) in [("alpha", "a1"), ("beta", "b1"), ("gamma", "g1")] {
        let name = format!("chief-{slug}");
        let mut row =
            db::SessionRow::new(&name, slug, chief::state::Role::Coordinator, db::now_ms());
        row.short_id = Some(short.into());
        row.session_id = Some(format!("{short}-sid"));
        db::insert_session(&conn, &row).unwrap();
    }
    // Threads in accounts of their own (§3): Alpha's in Beta's account, Gamma's in a third.
    let cpb = common::account(home.path(), "claude-cpb");
    for (name, slug, short, id, dir) in [
        ("alpha-far-1111", "alpha", "f1", "carlos", &carlos),
        ("gamma-far-2222", "gamma", "f2", "cpb", &cpb),
    ] {
        let mut row = db::SessionRow::new(name, slug, chief::state::Role::Thread, db::now_ms());
        row.short_id = Some(short.into());
        row.session_id = Some(format!("{short}-sid"));
        common::in_account(&mut row, id, id, Some(dir));
        db::insert_session(&conn, &row).unwrap();
    }
    home.fake.set(
        "agents.stdout",
        &json!([
            agent("a1", "a1-sid", "chief-alpha", "done", Some(1)),
            agent("g1", "g1-sid", "chief-gamma", "done", Some(3)),
        ])
        .to_string(),
    );
    std::fs::write(
        carlos.join("fake-agents.stdout"),
        json!([
            agent("b1", "b1-sid", "chief-beta", "done", Some(2)),
            agent("f1", "f1-sid", "alpha-far-1111", "working", Some(4)),
        ])
        .to_string(),
    )
    .unwrap();
    common::account_agents(
        &cpb,
        &[agent("f2", "f2-sid", "gamma-far-2222", "working", Some(5))],
    );
    let before = home.fake.accounts().len();
    let out = home.run(&["status"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for slug in ["alpha", "beta", "gamma"] {
        assert!(
            text.contains(&format!("coordinator chief-{slug} (")),
            "{text}"
        );
        assert!(
            !text.contains(&format!("coordinator chief-{slug} (asleep")),
            "{text}"
        );
    }
    assert!(
        text.contains(&format!(
            "CLAUDE_CONFIG_DIR={} claude attach b1",
            carlos.display()
        )),
        "{text}"
    );
    let mut asked: Vec<_> = home.fake.accounts()[before..]
        .iter()
        .map(|(first, env)| (first.clone(), env.clone()))
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            ("agents".to_string(), carlos.to_string_lossy().into_owned()),
            ("agents".to_string(), cpb.to_string_lossy().into_owned()),
            ("agents".to_string(), "unset".to_string()),
        ]
    );
    for (name, pid) in [("alpha-far-1111", 4), ("gamma-far-2222", 5)] {
        let row = db::get_session(&home.db(), name).unwrap().unwrap();
        assert_eq!(row.pid, Some(pid), "{name}");
    }
    // Each thread line names the agp account it runs in.
    let far = text.lines().find(|l| l.contains("gamma-far-2222")).unwrap();
    assert!(far.contains("gamma-far-2222 · account cpb · "), "{far}");
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

    // A repo path the coordinator's deny rule would read as a glob.
    let globby = home.path().join("work[1]");
    std::fs::create_dir_all(&globby).unwrap();
    let out = home.run(&["init", "Demo", "--repo", globby.to_str().unwrap()], "");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("rename or move"), "{}", stderr(&out));
    assert!(!home.path().join("projects/demo").exists());

    home.fake.set("version.stdout", "2.1.300 (Claude Code)\n");
    let out = home.run(&["init", "Demo"], "");
    assert!(stderr(&out).contains("claude update"), "{}", stderr(&out));
    assert!(!home.path().join("projects/demo").exists());

    // No repos is fine: threads then need an explicit absolute repo path.
    version_ok(&home);
    let out = home.run(&["init", "Demo", "--no-trust-prompt"], "");
    assert!(out.status.success(), "{}", stderr(&out));
}

/// Once the project is made, a trust check that fails (an unreadable `.claude.json`) is a line,
/// and `init` still succeeds: failing then would leave a project a second `init` finds taken.
#[test]
fn init_finishes_when_trust_cant_be_checked() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    std::fs::write(&home.trust, "{ not json").unwrap();
    let out = init(&home, &a, &b);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Created project"), "{text}");
    assert!(
        text.contains("chief couldn't check which folders Claude Code trusts: parsing"),
        "{text}"
    );
    assert!(
        text.contains("To trust them later: chief trust payments-latency"),
        "{text}"
    );
    assert!(text.ends_with("(or run chief and pick it)\n"), "{text}");
    assert!(db::project_dir(&home.db(), SLUG).unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(&home.trust).unwrap(),
        "{ not json",
        "never replaced"
    );
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
        .env_remove("CHIEF_TMUX_BIN")
        .env_remove("CHIEF_AGP_BIN")
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

    // A project from before chief trusted its own folder: `up` trusts it first.
    trust_file(&home.trust, &[]);
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
    assert!(chief::claude::is_trusted_in(&home.trust, &dir).unwrap());
    let text = stdout(&out);
    assert!(
        text.contains("Coordinator chief-payments-latency started."),
        "{text}"
    );
    assert!(
        text.contains("  chief pane --project payments-latency\n"),
        "{text}"
    );

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

/// A coordinator whose session was removed outside chief can't be respawned: `up` starts a new
/// one under the same name. A respawn that fails while the session is still listed stays an
/// error.
#[test]
fn up_replaces_a_coordinator_whose_session_was_removed() {
    let home = Home::new();
    version_ok(&home);
    let (a, b) = repos(&home);
    assert!(init(&home, &a, &b).status.success());
    let name = "chief-payments-latency";
    let old = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    home.fake
        .set(&format!("agents.stdout.{}", next_agents_call(&home)), "[]")
        .set(
            "bg.stdout",
            "backgrounded · c0ffee · chief-payments-latency\n",
        )
        .set("agents.stdout", &json!([old]).to_string());
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));

    // Listed without a process, and the respawn fails: nothing is replaced.
    let asleep = agent("c0ffee", "coord-sid", name, "done", None);
    home.fake
        .set("agents.stdout", &json!([asleep]).to_string())
        .set("respawn.exit", "1")
        .set("respawn.stderr", "No job matching 'c0ffee'\n");
    let out = home.run(&["up", SLUG], "");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("No job matching"), "{}", stderr(&out));
    assert_eq!(home.fake.calls_of("--bg").len(), 1);

    // No longer listed: a new session takes the name.
    let new = agent("d00d00", "new-sid", name, "done", Some(74200));
    let k = next_agents_call(&home);
    for call in k..k + 3 {
        home.fake.set(&format!("agents.stdout.{call}"), "[]");
    }
    home.fake
        .set(
            "bg.stdout",
            "backgrounded · d00d00 · chief-payments-latency\n",
        )
        .set("agents.stdout", &json!([new]).to_string());
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("started"), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("no longer exists"),
        "{}",
        stderr(&out)
    );
    assert_eq!(home.fake.calls_of("--bg").len(), 2);
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(
        (row.short_id.as_deref(), row.session_id.as_deref(), row.pid),
        (Some("d00d00"), Some("new-sid"), Some(74200))
    );
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
        &chief::agp::Agp::new(home.agp.bin()),
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
        "SendMessage|ListAgents|Agent"
    );
    assert_eq!(v["permissions"]["deny"].as_array().unwrap().len(), 7);

    // Can't rewrite: no respawn.
    std::fs::remove_dir_all(dir.join("generated")).unwrap();
    std::fs::write(dir.join("generated"), "not a folder").unwrap();
    let err = chief::project::respawn(
        &conn,
        &home.fake.claude(),
        &chief::agp::Agp::new(home.agp.bin()),
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
    let mut new = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    new["cwd"] = project_dir(&home).to_string_lossy().into_owned().into();
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
    // A live session with the name in another folder (another chief home's coordinator, or the
    // user's own) is not adopted; the name is taken, so nothing starts.
    let mut theirs = agent("0th3r", "other-sid", name, "done", Some(5));
    theirs["cwd"] = home.path().to_string_lossy().into_owned().into();
    home.fake.set("agents.stdout", &json!([theirs]).to_string());
    let out = home.run(&["up", SLUG], "");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("already named chief-payments-latency"),
        "{}",
        stderr(&out)
    );
    let row = db::get_coordinator(&home.db(), SLUG).unwrap().unwrap();
    assert_eq!(row.short_id, None);

    let mut ours = agent("c0ffee", "coord-sid", name, "done", Some(74112));
    ours["cwd"] = project_dir(&home).to_string_lossy().into_owned().into();
    home.fake.set("agents.stdout", &json!([ours]).to_string());
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
    let text = chief::project::render_status(SLUG, false, &[cleaned, blocked], "", now);
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

/// A project whose coordinator runs (started by a first `up` that found no Rex and no tmux, so
/// it printed the two commands to run by hand).
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
    assert!(
        text.contains("No layout: Rex isn't available (`rex list` failed"),
        "{text}"
    );
    assert!(text.contains("no Rex server is running"), "{text}");
    assert!(
        text.contains(
            "and this terminal isn't inside tmux.\n\
             Run these side by side, in two terminals or splits of one:\n  \
             chief pane --project payments-latency\n  \
             chief board --project payments-latency\n"
        ),
        "{text}"
    );
    assert!(!text.contains("claude attach"), "{text}");
    assert!(home.tmux.calls().is_empty(), "outside tmux: never asked");
    assert_eq!(home.rex.calls_of("list").len(), 1);
    assert!(home.rex.calls_of("new").is_empty());
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
        format!("CHIEF_TMUX_BIN={}", home.tmux.bin().display()),
        format!("CHIEF_AGP_BIN={}", home.agp.bin().display()),
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
    // Inside tmux too: a running Rex comes first.
    let out = home.run_env(&["up", SLUG], &[("TMUX", "/tmp/tmux-sock,1,0")], "");
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
    assert!(home.tmux.calls().is_empty(), "{:?}", home.tmux.calls());
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
    new.push("--layout".into());
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
            // `rex run --ratio` leaves the split at half: the API sizes it.
            vec![
                "api".into(),
                "call".into(),
                "session.resize_pane".into(),
                json!({
                    "session_id": "session:new",
                    "direction": "horizontal",
                    "before_block_id": "block:main",
                    "after_block_id": "block:board",
                    "ratio": 0.68,
                })
                .to_string(),
                "--autostart=false".into(),
            ],
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

/// A session `up` created but couldn't finish (its first block not renamed in Rex, its pane not
/// marked in tmux) is killed: its id is never stored, so the next `up` would make another, and a
/// `q` in its pane would close the stored layout instead.
#[test]
fn up_kills_a_layout_it_could_not_finish() {
    let home = Home::new();
    running_project(&home);
    home.rex
        .set("list.stdout", r#"{"sessions": []}"#)
        .set(
            "new.stdout",
            r#"{"session_id": "session:new", "initial_windows": [{"window_id": "window:1", "block_ids": ["block:main"]}]}"#,
        )
        .set("block.rename.exit", "1");
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("No Rex layout: `rex block rename"), "{text}");
    assert_eq!(
        home.rex.calls_of("kill"),
        [vec!["kill", "session:new", "--autostart=false"]]
    );
    assert!(home.rex.calls_of("run").is_empty());
    assert_eq!(db::get_rex_session(&home.db(), SLUG).unwrap(), None);

    home.rex
        .set("list.exit", "1")
        .set("list.stderr", "Error: no Rex server is running\n");
    home.tmux
        .set("new-window.stdout", "%5 @3 4242\n")
        .set("set-option.exit", "1");
    let sock = "/tmp/tmux-501/default";
    let out = home.run_env(
        &["up", SLUG],
        &[("TMUX", "/tmp/tmux-501/default,4242,0")],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("No tmux layout: "),
        "{}",
        stdout(&out)
    );
    let calls = tmux_calls(&home, sock);
    assert_eq!(
        calls.last().unwrap(),
        &strings(&["kill-window", "-t", "@3"]),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|c| c[0] == "split-window"), "{calls:?}");
    assert_eq!(db::get_tmux_window(&home.db(), SLUG).unwrap(), None);
}

/// The fake tmux's calls, without the leading `-S <socket>` chief always passes.
fn tmux_calls(home: &Home, socket: &str) -> Vec<Vec<String>> {
    home.tmux
        .calls()
        .into_iter()
        .map(|c| {
            assert_eq!(c[..2], ["-S", socket], "{c:?}");
            c[2..].to_vec()
        })
        .collect()
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// Without Rex but inside tmux, `up` opens a window `chief-<slug>` in the current session right
/// after the user's window: the pane on the left, the board split right at 32 %, both carrying
/// the passthrough variables. It stores the window with its server, reuses it, and `down`
/// closes it.
#[test]
fn up_opens_a_tmux_window_and_down_closes_it() {
    let home = Home::new();
    let dir = running_project(&home);
    // No Rex server after all.
    home.rex
        .set("list.exit", "1")
        .set("list.stderr", "Error: no Rex server is running\n");
    let d = dir.to_string_lossy().into_owned();
    let sock = "/tmp/tmux-501/default";
    let tmux_env = [
        ("TMUX", "/tmp/tmux-501/default,4242,0"),
        ("TMUX_PANE", "%9"),
    ];
    home.tmux
        .set("display-message.stdout", "@1\n")
        .set("new-window.stdout", "%5 @3 4242\n")
        .set("split-window.stdout", "%6\n");
    let out = home.run_env(&["up", SLUG], &tmux_env, "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("tmux window chief-payments-latency opened."),
        "{text}"
    );
    assert!(!text.contains("side by side"), "{text}");

    let mut new = strings(&[
        "new-window",
        "-n",
        "chief-payments-latency",
        "-a",
        "-t",
        "@1",
        "-P",
        "-F",
        "#{pane_id} #{window_id} #{pid}",
        "-c",
        &d,
        "--",
    ]);
    new.extend(block_command(&home, "pane"));
    new.push("--layout".into());
    let mut split = strings(&[
        "split-window",
        "-h",
        "-l",
        "32%",
        "-t",
        "%5",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        &d,
        "--",
    ]);
    split.extend(block_command(&home, "board"));
    let mark = |pane: &str, label: &str| {
        strings(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "@chief-block",
            label,
            ";",
            "set-option",
            "-p",
            "-t",
            pane,
            "remain-on-exit",
            "on",
        ])
    };
    assert_eq!(
        tmux_calls(&home, sock),
        [
            // $TMUX_PANE's window: new-window -t takes a window, not a pane.
            strings(&["display-message", "-p", "-t", "%9", "#{window_id}"]),
            new,
            mark("%5", "main"),
            split,
            mark("%6", "board"),
            strings(&["select-pane", "-t", "%5"]),
            strings(&["select-window", "-t", "@3"]),
        ]
    );
    assert_eq!(
        db::get_tmux_window(&home.db(), SLUG).unwrap().as_deref(),
        Some("@3 4242 /tmp/tmux-501/default")
    );

    // Open and running: reused. One canned `display-message` answers both panes.
    home.tmux
        .set("list-windows.stdout", "4242 @1\n4242 @3\n")
        .set("list-panes.stdout", "%5 main\n%6 board\n")
        .set("display-message.stdout", "0\n");
    let before = home.tmux.calls().len();
    let out = home.run_env(&["up", SLUG], &tmux_env, "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("tmux window chief-payments-latency already open."),
        "{text}"
    );
    let firsts: Vec<String> = tmux_calls(&home, sock)[before..]
        .iter()
        .map(|c| c[0].clone())
        .collect();
    assert_eq!(
        firsts,
        [
            "list-windows",
            "list-panes",
            "display-message",
            "list-panes",
            "display-message",
            "select-pane",
            "select-window",
        ]
    );

    // `down` closes it from any terminal (no TMUX here): the stored socket names the server.
    let before = home.tmux.calls().len();
    let out = home.run(&["down", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("Closed tmux window chief-payments-latency.\n"),
        "{text}"
    );
    let calls = tmux_calls(&home, sock)[before..].to_vec();
    assert_eq!(
        calls.last().unwrap(),
        &strings(&["kill-window", "-t", "@3"])
    );
    assert_eq!(db::get_tmux_window(&home.db(), SLUG).unwrap(), None);
    assert!(home.rex.calls_of("kill").is_empty());

    // Nothing of chief's open any more: nothing to close.
    let before = home.tmux.calls().len();
    let out = home.run(&["down", SLUG], "");
    assert!(stdout(&out).starts_with("No layout of chief's is open.\n"));
    assert_eq!(home.tmux.calls().len(), before);
}

#[test]
fn down_stops_a_thread_in_its_own_account() {
    let home = Home::new();
    running_project(&home);
    let cpb = common::account(home.path(), "claude-cpb");
    {
        let mut t = db::SessionRow::new(
            "payments-latency-far-1111",
            SLUG,
            chief::state::Role::Thread,
            db::now_ms(),
        );
        t.short_id = Some("f1".into());
        t.session_id = Some("f1-sid".into());
        common::in_account(&mut t, "cpb", "PB2", Some(&cpb));
        db::insert_session(&home.db(), &t).unwrap();
    }
    common::account_agents(
        &cpb,
        &[agent(
            "f1",
            "f1-sid",
            "payments-latency-far-1111",
            "working",
            Some(5),
        )],
    );
    let before = home.fake.accounts().len();
    let out = home.run(&["down", SLUG, "--stop-threads"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Stopped thread payments-latency-far-1111."),
        "{text}"
    );
    let stops: Vec<_> = home.fake.accounts()[before..]
        .iter()
        .filter(|(first, _)| first == "stop")
        .cloned()
        .collect();
    assert_eq!(
        stops,
        [
            ("stop".to_string(), "unset".to_string()),
            ("stop".to_string(), cpb.to_string_lossy().into_owned()),
        ]
    );
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
    let out = home.run(&["down", SLUG, "--stop-threads"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("No layout of chief's is open.\n"),
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
            vec!["block", "close", "block:board", "--autostart=false"],
            vec!["block", "close", "block:main", "--autostart=false"]
        ],
        "main, the pane, last"
    );
    assert_eq!(
        db::get_rex_session(&home.db(), SLUG).unwrap().as_deref(),
        Some("session:7")
    );
}

/// `chief down` ends the running pane first, one it didn't open too (in the user's own Rex
/// block, or run by hand): left running, it would attach again and respawn the coordinator
/// seconds after `down` stopped it.
#[test]
fn down_ends_the_running_pane_first() {
    use std::os::unix::process::ExitStatusExt;
    let home = Home::new();
    running_project(&home);
    // A stand-in for the pane, reaped as soon as it ends, as its shell would. It never clears
    // its record: `down` sees it gone.
    let pane = |ignore_term: bool| {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        if ignore_term {
            use std::os::unix::process::CommandExt;
            // SAFETY: signal() is async-signal-safe; the ignore survives exec.
            unsafe {
                cmd.pre_exec(|| {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as i32;
        let started = chief::proc::start_time(pid).unwrap();
        let conn = home.db();
        db::set_pane_process(&conn, SLUG, pid, &started).unwrap();
        db::set_pane_showing(&conn, SLUG, Some("chief-payments-latency")).unwrap();
        (pid, std::thread::spawn(move || child.wait().unwrap()))
    };
    let (pid, waiter) = pane(false);
    let out = home.run(&["down", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with(&format!(
            "Ended chief pane (pid {pid}).\nNo layout of chief's is open.\n"
        )),
        "{text}"
    );
    assert!(
        text.contains("Stopped coordinator chief-payments-latency."),
        "{text}"
    );
    assert_eq!(waiter.join().unwrap().signal(), Some(libc::SIGTERM));
    assert_eq!(db::get_pane_process(&home.db(), SLUG).unwrap(), None);
    assert_eq!(db::get_pane_showing(&home.db(), SLUG).unwrap(), None);

    // A pane that doesn't end: `down` goes on, keeps what it shows, and fails (on stderr).
    let (pid, waiter) = pane(true);
    let out = home.run(&["down", SLUG], "");
    assert!(!out.status.success());
    assert!(
        stderr(&out).starts_with(&format!(
            "chief pane (pid {pid}) is still running after 3 s.\n"
        )),
        "{}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        text.contains("Stopped coordinator chief-payments-latency."),
        "{text}"
    );
    assert_eq!(
        db::get_pane_showing(&home.db(), SLUG).unwrap().as_deref(),
        Some("chief-payments-latency")
    );
    // SAFETY: our own child, not reaped until the waiter returns.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    waiter.join().unwrap();

    // Gone without clearing its record: nothing to end, and the record is forgotten.
    let out = home.run(&["down", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("No layout of chief's is open.\n"),
        "{}",
        stdout(&out)
    );
    assert_eq!(db::get_pane_process(&home.db(), SLUG).unwrap(), None);
    assert_eq!(db::get_pane_showing(&home.db(), SLUG).unwrap(), None);
}

/// `chief archive` is `down --stop-threads`, and archives only when that finished cleanly.
/// `status` then leaves the project out, `gc` still cleans it, and `unarchive` or `up` brings it
/// back.
#[test]
fn archive_stops_everything_then_puts_the_project_away() {
    let home = Home::new();
    running_project(&home);
    {
        let mut t = db::SessionRow::new(
            "payments-latency-a-1111",
            SLUG,
            chief::state::Role::Thread,
            db::now_ms(),
        );
        t.short_id = Some("t1".into());
        t.session_id = Some("t1-sid".into());
        db::insert_session(&home.db(), &t).unwrap();
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
        ])
        .to_string(),
    );

    // A stop that fails: still running, so not archived (nothing keeps running hidden).
    home.fake.set("stop.exit", "1");
    let out = home.run(&["archive", SLUG], "");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("Could not stop coordinator chief-payments-latency"),
        "{err}"
    );
    assert!(
        err.contains("Payments Latency isn't archived: chief down payments-latency didn't finish"),
        "{err}"
    );
    assert_eq!(db::archived_at(&home.db(), SLUG).unwrap(), None);

    home.fake.unset("stop.exit");
    let out = home.run(&["archive", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "No layout of chief's is open.\n\
         Stopped coordinator chief-payments-latency.\n\
         Stopped thread payments-latency-a-1111.\n\
         Archived Payments Latency. Nothing was deleted: chief unarchive payments-latency or \
         chief up payments-latency brings it back.\n"
    );
    assert_eq!(home.fake.calls_of("stop").len(), 4);
    assert!(home.fake.calls_of("rm").is_empty());
    assert!(db::archived_at(&home.db(), SLUG).unwrap().is_some());

    // Archived already: said, nothing stopped again.
    let out = home.run(&["archive", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "Payments Latency is archived already.\n");
    assert_eq!(home.fake.calls_of("stop").len(), 4);

    let out = home.run(&["status"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "1 archived project not shown; chief status <slug> shows one.\n"
    );
    let out = home.run(&["status", SLUG], "");
    assert!(
        stdout(&out).starts_with(
            "payments-latency (archived; chief unarchive payments-latency brings it back)\n"
        ),
        "{}",
        stdout(&out)
    );
    // Its resolved threads still hold worktrees: gc goes through archived projects too.
    let out = home.run(&["gc", "--dry-run"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("payments-latency\n"),
        "{}",
        stdout(&out)
    );

    let out = home.run(&["unarchive", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Unarchived Payments Latency. chief up payments-latency opens it.\n"
    );
    assert_eq!(db::archived_at(&home.db(), SLUG).unwrap(), None);
    assert!(home.fake.calls_of("respawn").is_empty(), "starts nothing");
    let out = home.run(&["unarchive", SLUG], "");
    assert_eq!(stdout(&out), "Payments Latency isn't archived.\n");

    // `up` on an archived project brings it back first.
    assert!(home.run(&["archive", SLUG], "").status.success());
    let out = home.run(&["up", SLUG], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("Unarchived Payments Latency.\n"),
        "{}",
        stdout(&out)
    );
    assert_eq!(db::archived_at(&home.db(), SLUG).unwrap(), None);
}

/// `chief` alone is the launcher (§10a); without a terminal it says so instead of drawing.
#[test]
fn the_launcher_needs_a_terminal() {
    let home = Home::new();
    for args in [&[][..], &["new"][..]] {
        let out = home.run(args, "");
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("chief needs a terminal"),
            "{}",
            stderr(&out)
        );
    }
}
