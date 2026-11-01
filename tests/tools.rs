//! The coordinator's MCP tools (`chief::tools`) against the fake `claude`.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use chief::config::{ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::state::Role;
use chief::tools::{
    self, ListThreadsArgs, NameArgs, ReadReportArgs, ResolveThreadArgs, StartThreadArgs, ToolCtx,
};
use common::{Fake, agent, trust_file};
use rusqlite::Connection;
use serde_json::{Value, json};

const NOW: i64 = 1_800_000_000_000;

fn now() -> i64 {
    NOW
}

struct Env {
    root: tempfile::TempDir,
    conn: Connection,
    config: ProjectConfig,
    fake: Fake,
    agp: Fake,
    dir: PathBuf,
    repo: PathBuf,
    trust: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let dir = root.path().join("projects/demo");
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        let conn = db::open(&root.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", &dir, 0).unwrap();
        let config = ProjectConfig::new(
            "Demo",
            "demo",
            None,
            vec![Repo {
                name: "repo".into(),
                path: repo.clone(),
            }],
        );
        let trust = root.path().join("claude.json");
        trust_file(&trust, &[&repo]);
        // Not installed until a test installs it: threads stay in the project's account.
        let agp = Fake::named("agp");
        std::fs::remove_file(agp.bin()).unwrap();
        Env {
            root,
            conn,
            config,
            fake: Fake::new(),
            agp,
            dir,
            repo,
            trust,
        }
    }

    /// The fake `agp` with these pool accounts (`common::agp_view`) and folders.
    fn pool(&self, views: &[Value], dirs: &[(&str, Option<&Path>)]) {
        if !self.agp.bin().exists() {
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-claude.sh");
            std::os::unix::fs::symlink(script, self.agp.bin()).unwrap();
        }
        common::agp_accounts(&self.agp, views, dirs);
    }

    fn with<T>(&self, f: impl FnOnce(&ToolCtx) -> T) -> T {
        self.with_claude(&self.fake.claude(), f)
    }

    fn with_claude<T>(&self, claude: &chief::claude::Claude, f: impl FnOnce(&ToolCtx) -> T) -> T {
        let agp = chief::agp::Agp::new(self.agp.bin());
        let ctx = ToolCtx {
            conn: &self.conn,
            config: &self.config,
            claude,
            agp: &agp,
            project_dir: &self.dir,
            trust_config: &self.trust,
            now,
            resolve_timeout: Duration::from_millis(300),
            // Room for a few calls of the shell fake on a loaded machine.
            wake_timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(10),
        };
        f(&ctx)
    }

    fn agents(&self, rows: &[Value]) -> &Self {
        self.fake
            .set("agents.stdout", &Value::from(rows.to_vec()).to_string());
        self
    }

    fn insert(&self, name: &str, f: impl FnOnce(&mut SessionRow)) {
        let mut row = SessionRow::new(name, "demo", Role::Thread, NOW - 60_000);
        f(&mut row);
        db::insert_session(&self.conn, &row).unwrap();
    }

    fn row(&self, name: &str) -> Option<SessionRow> {
        db::get_session(&self.conn, name).unwrap()
    }
}

fn start_args(title: &str, task: &str) -> StartThreadArgs {
    StartThreadArgs {
        title: title.into(),
        task: task.into(),
        repo: None,
        model: None,
        effort: None,
        account: None,
    }
}

fn arg_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let i = args
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("{flag} missing: {args:?}"));
    &args[i + 1]
}

#[test]
fn start_thread_spawns_and_resolves() {
    let env = Env::new();
    env.fake
        .set("agents.stdout.1", "[]")
        .set("bg.stdout", "backgrounded · abc123 · whatever\n");
    env.agents(&[agent(
        "abc123",
        "sid-abc",
        "demo-fix-login-bug-xxxx",
        "working",
        Some(77),
    )]);

    let out = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
        .unwrap();
    let name = out["name"].as_str().unwrap().to_string();
    assert!(
        name.starts_with("demo-fix-login-bug-") && name.len() == "demo-fix-login-bug-".len() + 4,
        "{name}"
    );
    assert_eq!(out["short_id"], "abc123");
    assert_eq!(out["repo"], "repo");
    assert_eq!(
        (out["model"].as_str(), out["effort"].as_str()),
        (Some("sonnet"), Some("high"))
    );
    assert_eq!(
        out["next"],
        format!(
            "Subscribe now: call SendMessage with exactly {{\"to\": \"{name}\", \"notify_when_idle\": true}} and no message key at all."
        )
    );
    // No agp: the project's account, as before, and messaged by name.
    assert_eq!(
        (&out["account"], &out["address"]),
        (&Value::Null, &json!(name))
    );
    assert!(out.get("account_note").is_none());

    let row = env.row(&name).unwrap();
    assert_eq!(row.short_id.as_deref(), Some("abc123"));
    assert_eq!(row.session_id.as_deref(), Some("sid-abc"));
    assert_eq!(row.pid, Some(77));
    assert_eq!(row.title.as_deref(), Some("Fix login bug"));
    assert_eq!(row.task.as_deref(), Some("Fix it"));
    assert_eq!(row.repo_name.as_deref(), Some("repo"));
    assert_eq!(row.repo_path.as_deref(), Some(env.repo.to_str().unwrap()));
    assert_eq!(row.role, "thread");

    let calls = env.fake.calls();
    let bg_index = calls.iter().position(|c| c[0] == "--bg").unwrap();
    let bg = &calls[bg_index];
    assert_eq!(arg_after(bg, "--name"), name);
    assert_eq!(arg_after(bg, "--model"), "sonnet");
    assert_eq!(arg_after(bg, "--effort"), "high");
    assert_eq!(arg_after(bg, "--permission-mode"), "auto");
    assert_eq!(
        Path::new(arg_after(bg, "--settings")),
        env.dir.join("generated/thread-settings.json")
    );
    assert_eq!(
        Path::new(arg_after(bg, "--add-dir")),
        env.dir.join("memory")
    );
    let brief = arg_after(bg, "--append-system-prompt");
    assert!(
        brief.contains("`chief-demo`") && brief.contains("\"Demo\""),
        "{brief}"
    );
    assert_eq!(bg.last().unwrap(), "Fix it");
    assert!(!bg.contains(&"--mcp-config".to_string()));
    assert_eq!(env.fake.cwd(bg_index), env.repo);
    assert_eq!(
        calls[0],
        ["agents", "--json", "--all"],
        "names are checked first"
    );
}

/// Pool accounts for these tests: PB5 is limited, PB2 scores best but doesn't trust the repo,
/// PB3 trusts it; Main (the default account) isn't in the pool.
struct Pool {
    cpb: PathBuf,
    cpe: PathBuf,
    pb5: PathBuf,
}

fn pool(env: &Env) -> Pool {
    let cpb = common::account(env.root.path(), "claude-cpb");
    let cpe = common::account(env.root.path(), "claude-cpe");
    let pb5 = common::account(env.root.path(), "claude-pb5");
    trust_file(&cpe.join(".claude.json"), &[&env.repo]);
    trust_file(&pb5.join(".claude.json"), &[&env.repo]);
    let mut limited = common::agp_view("pb5", "PB5", true, 0.99);
    limited["limited"] = json!({"until": 1});
    env.pool(
        &[
            common::agp_view("default", "Main", false, 0.0),
            common::agp_view("cpe", "PB3", true, 0.6),
            common::agp_view("cpb", "PB2", true, 0.9),
            limited,
        ],
        &[
            ("default", None),
            ("cpb", Some(&cpb)),
            ("cpe", Some(&cpe)),
            ("pb5", Some(&pb5)),
        ],
    );
    Pool { cpb, cpe, pb5 }
}

/// §7 "Thread account": the best usable pool account that trusts the repo, with the skipped
/// ones in a note; the thread is messaged at its socket address there, and every call about it
/// runs in that account.
#[test]
fn start_thread_runs_in_the_best_pool_account_that_trusts_the_repo() {
    let env = Env::new();
    let pool = pool(&env);
    env.fake
        .set("bg.stdout", "backgrounded · abc123 · whatever\n")
        .set("agents.stdout", "[]");
    common::account_agents(
        &pool.cpe,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-anything-xxxx",
            "working",
            Some(77),
        )],
    );
    let out = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
        .unwrap();
    let name = out["name"].as_str().unwrap().to_string();
    assert_eq!(out["account"], "PB3");
    assert_eq!(out["address"], "uds:/tmp/cc-socks/77.sock");
    let next = out["next"].as_str().unwrap();
    assert!(
        next.starts_with(
            "Subscribe now: call SendMessage with exactly {\"to\": \"uds:/tmp/cc-socks/77.sock\", \
             \"notify_when_idle\": true}"
        ),
        "{next}"
    );
    assert!(next.contains("not by name"), "{next}");
    let note = out["account_note"].as_str().unwrap();
    assert!(
        note.starts_with("It runs in PB3; skipped: PB2 doesn't trust"),
        "{note}"
    );
    assert!(
        note.contains(&format!(
            "cd {} && agp exec cpb -- claude",
            env.repo.display()
        )),
        "{note}"
    );

    let row = env.row(&name).unwrap();
    assert_eq!(
        (
            row.account_id.as_deref(),
            row.account_label.as_deref(),
            row.account_dir.as_deref()
        ),
        (Some("cpe"), Some("PB3"), Some(pool.cpe.to_str().unwrap()))
    );
    assert_eq!(
        (row.session_id.as_deref(), row.pid),
        (Some("sid-abc"), Some(77))
    );
    let cpe = pool.cpe.to_string_lossy().into_owned();
    let accounts = env.fake.accounts();
    let bg: Vec<_> = accounts.iter().filter(|(f, _)| f == "--bg").collect();
    assert_eq!(bg.len(), 1);
    assert_eq!(bg[0].1, cpe, "spawned in PB3");
    let spawn = &env.fake.calls_of("--bg")[0];
    let brief = arg_after(spawn, "--append-system-prompt");
    assert!(
        brief.contains("You run in another Claude account than your coordinator"),
        "{brief}"
    );
    // Free-name check in both accounts, then resolving in PB3 only.
    let mut listed: Vec<_> = accounts
        .iter()
        .filter(|(f, _)| f == "agents")
        .map(|(_, a)| a.clone())
        .collect();
    listed.dedup();
    assert!(
        listed.contains(&"unset".to_string()) && listed.contains(&cpe),
        "{listed:?}"
    );
    // agp: its status, then the folders of the accounts tried (never the limited one's).
    // Then whether it watches background sessions: this one doesn't, so claude starts it.
    let agp: Vec<_> = env
        .agp
        .calls()
        .into_iter()
        .map(|c| c.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(agp, ["status --json", "exec cpb", "exec cpe", "--help"]);
    let _ = pool.pb5;

    // list_threads gives the same address and the account.
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    let t = &out["threads"][0];
    assert_eq!(
        (&t["account"], &t["address"]),
        (&json!("PB3"), &json!("uds:/tmp/cc-socks/77.sock"))
    );
}

/// The project's account, as before agp, whenever the pool can't take the thread: nothing
/// allowed, everything limited or over `max_usage`, nothing that trusts the repo, agp failing.
#[test]
fn start_thread_falls_back_to_the_project_account() {
    let run = |env: &Env| {
        env.fake
            .set("bg.stdout", "backgrounded · abc123 · whatever\n")
            .set(
                "agents.stdout",
                &json!([agent(
                    "abc123",
                    "sid-abc",
                    "demo-x-xxxx",
                    "working",
                    Some(7)
                )])
                .to_string(),
            );
        let out = env
            .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
            .unwrap();
        let name = out["name"].as_str().unwrap();
        assert_eq!(
            (&out["account"], &out["address"]),
            (&Value::Null, &json!(name))
        );
        let row = env.row(name).unwrap();
        assert_eq!(row.account_id, None);
        assert!(
            env.fake
                .accounts()
                .iter()
                .all(|(_, account)| account == "unset")
        );
        out.get("account_note")
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    // `accounts = []`: agp isn't asked for an account, only whether it can watch the thread
    // for limits (this one can't).
    let mut env = Env::new();
    pool(&env);
    env.config.threads.accounts = vec![];
    assert_eq!(run(&env), None);
    assert_eq!(env.agp.calls(), [vec!["--help".to_string()]]);

    // Only accounts outside the pool (or unknown) allowed: silently the project's.
    let mut env = Env::new();
    pool(&env);
    env.config.threads.accounts = vec!["default".into(), "nope".into()];
    assert_eq!(run(&env), None);

    // Only PB2 allowed, which doesn't trust the repo.
    let mut env = Env::new();
    pool(&env);
    env.config.threads.accounts = vec!["cpb".into()];
    let note = run(&env).unwrap();
    assert!(
        note.starts_with("No pool account could take it (PB2 doesn't trust"),
        "{note}"
    );
    assert!(
        note.ends_with("it runs in the project's account."),
        "{note}"
    );

    // Everything usable is at or over max_usage (10% of the 5-hour window here).
    let mut env = Env::new();
    pool(&env);
    env.config.threads.max_usage = Some(10);
    let note = run(&env).unwrap();
    assert!(
        note.starts_with("Every pool account (cpe (PB3), cpb (PB2), pb5 (PB5)) is limited or at 10% usage or more"),
        "{note}"
    );

    // agp fails.
    let env = Env::new();
    pool(&env);
    env.agp
        .set("status.exit", "1")
        .set("status.stderr", "agp: broken config");
    let note = run(&env).unwrap();
    assert!(note.contains("agp: broken config"), "{note}");
    assert!(note.ends_with("it runs in the project's account"), "{note}");

    // The project's account must trust the repo then, as before.
    let mut env = Env::new();
    pool(&env);
    env.config.threads.accounts = vec![];
    trust_file(&env.trust, &[]);
    let err = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
        .unwrap_err();
    assert!(err.to_string().contains("doesn't trust"), "{err}");
}

/// A pool account trusting the repo is enough: the project's account needn't.
#[test]
fn start_thread_needs_trust_only_in_the_thread_account() {
    let env = Env::new();
    let pool = pool(&env);
    trust_file(&env.trust, &[]);
    env.fake
        .set("bg.stdout", "backgrounded · abc123 · whatever\n")
        .set("agents.stdout", "[]");
    common::account_agents(
        &pool.cpe,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-x-xxxx",
            "working",
            Some(7),
        )],
    );
    let out = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
        .unwrap();
    assert_eq!(out["account"], "PB3");
    let _ = pool.cpb;
}

/// `account` pins one when the user asks: by id or label, from the pool only, trusting the
/// repo; a limited one is allowed with a note.
#[test]
fn start_thread_runs_where_asked() {
    let pinned = |env: &Env, account: &str| {
        let mut args = start_args("Fix login bug", "Fix it");
        args.account = Some(account.into());
        env.with(|ctx| tools::start_thread(ctx, &args))
    };
    let env = Env::new();
    let pool = pool(&env);
    env.fake
        .set("bg.stdout", "backgrounded · abc123 · whatever\n")
        .set("agents.stdout", "[]");
    common::account_agents(
        &pool.pb5,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-x-xxxx",
            "working",
            Some(7),
        )],
    );
    let out = pinned(&env, "PB5").unwrap();
    assert_eq!(out["account"], "PB5");
    assert_eq!(
        out["account_note"],
        "PB5 is limited right now; the thread waits for its limit to reset."
    );

    let err = pinned(&env, "default").unwrap_err();
    assert_eq!(
        err.to_string(),
        "default is not an account in this project's pool (pool: cpe (PB3), cpb (PB2), pb5 (PB5))"
    );
    let err = pinned(&env, "cpb").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("can't run it in PB2: PB2 doesn't trust"),
        "{err}"
    );
    let _ = pool.cpe;

    // Without agp nothing can be pinned.
    let env = Env::new();
    let err = pinned(&env, "PB3").unwrap_err();
    assert!(err.to_string().contains("agp isn't installed"), "{err}");
}

#[test]
fn start_thread_options_and_flag_like_tasks() {
    let env = Env::new();
    env.fake.set("bg.stdout", "backgrounded · s1 · x\n");
    env.agents(&[]);
    std::fs::write(env.dir.join("instructions.md"), "Use pnpm.\n").unwrap();
    let args = StartThreadArgs {
        repo: Some(env.repo.to_string_lossy().into_owned()),
        model: Some("haiku".into()),
        effort: Some("low".into()),
        ..start_args("Tiny", "--help is broken")
    };
    let out = env.with(|ctx| tools::start_thread(ctx, &args)).unwrap();
    assert_eq!(
        (out["model"].as_str(), out["effort"].as_str()),
        (Some("haiku"), Some("low"))
    );
    assert_eq!(
        out["repo"], "repo",
        "an absolute path maps back to the repo's name"
    );
    let bg = env.fake.calls_of("--bg").remove(0);
    assert_eq!(bg.last().unwrap(), "Task: --help is broken");
    assert!(
        arg_after(&bg, "--append-system-prompt")
            .ends_with("## Project instructions\n\nUse pnpm.\n")
    );

    // session id never showed up: still started, the hooks will match it by name.
    let row = env.row(out["name"].as_str().unwrap()).unwrap();
    assert_eq!(
        (row.short_id.as_deref(), row.session_id),
        (Some("s1"), None)
    );

    let bad = StartThreadArgs {
        effort: Some("ultra".into()),
        ..start_args("x", "y")
    };
    let err = env.with(|ctx| tools::start_thread(ctx, &bad)).unwrap_err();
    assert!(err.to_string().contains("ultra"), "{err}");
}

#[test]
fn start_thread_refuses_untrusted_repos() {
    let env = Env::new();
    std::fs::write(&env.trust, "{}").unwrap();
    let err = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it")))
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains(env.repo.to_str().unwrap()), "{msg}");
    assert!(msg.contains("accept the trust prompt"), "{msg}");
    assert!(env.fake.calls().is_empty(), "nothing spawned");
    assert!(
        db::list_sessions(&env.conn, "demo", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn start_thread_removes_the_row_when_spawn_fails() {
    let env = Env::new();
    env.agents(&[]);
    env.fake
        .set("bg.stdout", "oops\n")
        .set("bg.stderr", "rate limited\n")
        .set("bg.exit", "1");
    let err = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it")))
        .unwrap_err();
    assert!(format!("{err:#}").contains("rate limited"));
    assert!(
        db::list_sessions(&env.conn, "demo", None)
            .unwrap()
            .is_empty()
    );

    // `claude agents` down: no name can be checked, so nothing starts.
    env.fake.set("agents.exit", "1");
    assert!(
        env.with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it")))
            .is_err()
    );
    assert_eq!(env.fake.calls_of("--bg").len(), 1);
}

fn has_guard(settings: &Path) -> bool {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
    v["hooks"]["PreToolUse"][1]["matcher"] == "SendMessage|ListAgents|Agent"
}

/// Claude Code reads hooks when a session starts, so chief rewrites `generated/*` from the
/// settings right before every spawn and respawn: an edit (say, one that drops the guard) never
/// reaches a session started after it.
#[test]
fn spawns_and_respawns_rewrite_the_generated_files_first() {
    let env = Env::new();
    let generated = env.dir.join("generated");
    let settings = generated.join("thread-settings.json");
    std::fs::create_dir_all(&generated).unwrap();
    std::fs::write(&settings, "{}").unwrap();
    env.fake.set("bg.stdout", "backgrounded · s1 · x\n");
    env.agents(&[]);
    env.with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it")))
        .unwrap();
    assert!(has_guard(&settings), "rewritten before the spawn");

    std::fs::write(&settings, "{}").unwrap();
    env.insert("demo-w-1111", |r| {
        r.short_id = Some("w1".into());
        r.session_id = Some("sid-w1".into());
    });
    env.agents(&[agent("w1", "sid-w1", "demo-w-1111", "done", None)]);
    let wake = || {
        env.with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-w-1111".into(),
                },
            )
        })
    };
    assert_eq!(wake().unwrap()["respawned"], true);
    assert!(has_guard(&settings), "rewritten before the respawn");

    // When the files can't be rewritten, nothing starts.
    std::fs::remove_dir_all(&generated).unwrap();
    std::fs::write(&generated, "not a folder").unwrap();
    let rows = db::list_sessions(&env.conn, "demo", None).unwrap().len();
    assert!(
        env.with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Again")))
            .is_err()
    );
    assert!(wake().is_err());
    assert_eq!(env.fake.calls_of("--bg").len(), 1);
    assert_eq!(env.fake.calls_of("respawn").len(), 1);
    assert_eq!(
        db::list_sessions(&env.conn, "demo", None).unwrap().len(),
        rows
    );
}

/// A `claude` whose `--bg` prints no id (`bg_exit` as its exit code) and whose `agents` lists the
/// session that `--bg` named, as if it had started anyway, in the folder `--bg` ran in (or
/// `cwd` when given).
fn started_anyway(dir: &Path, bg_exit: u8) -> chief::claude::Claude {
    started_anyway_in(dir, bg_exit, None)
}

fn started_anyway_in(dir: &Path, bg_exit: u8, cwd: Option<&Path>) -> chief::claude::Claude {
    let bin = dir.join("claude-started-anyway");
    let listed_cwd = match cwd {
        Some(p) => format!("'{}'", p.display()),
        None => "\"$(cat \"$dir/started-cwd\")\"".to_string(),
    };
    let script = format!(
        r#"#!/bin/sh
dir=$(dirname "$0")
case "$1" in
  --bg)
    pwd -P > "$dir/started-cwd"
    while [ $# -gt 1 ]; do
      [ "$1" = "--name" ] && printf '%s' "$2" > "$dir/started-name"
      shift
    done
    echo "something chief can't read"
    exit {bg_exit} ;;
  agents)
    if [ -f "$dir/started-name" ]; then
      printf '[{{"id":"ad0pt","sessionId":"sid-adopt","name":"%s","cwd":"%s","state":"working","status":"busy","pid":42}}]' "$(cat "$dir/started-name")" {listed_cwd}
    else
      echo '[]'
    fi ;;
esac
"#
    );
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // No job state: these threads never entered a worktree.
    let config_dir = dir.join("claude-config");
    chief::claude::Claude {
        bin,
        account: None,
        config_dir,
    }
}

/// A `claude --bg` that timed out or printed no id may still have started the session: chief
/// adopts it instead of deleting the row, which would leave it working unseen and unreachable.
#[test]
fn start_thread_adopts_a_session_that_started_anyway() {
    for bg_exit in [0, 1] {
        let env = Env::new();
        let claude = started_anyway(env.fake.dir.path(), bg_exit);
        let out = env
            .with_claude(&claude, |ctx| {
                tools::start_thread(ctx, &start_args("Fix login bug", "Fix it"))
            })
            .unwrap();
        let name = out["name"].as_str().unwrap();
        assert_eq!(out["short_id"], "ad0pt");
        let warning = out["warning"].as_str().unwrap();
        assert!(warning.contains("started anyway"), "{warning}");
        assert!(warning.contains("something chief can't read"), "{warning}");
        let row = env.row(name).unwrap();
        assert_eq!(
            (row.short_id.as_deref(), row.session_id.as_deref(), row.pid),
            (Some("ad0pt"), Some("sid-adopt"), Some(42))
        );
    }
}

/// A session with the new name in another folder isn't the one this spawn started (another chief
/// home's thread, say): not adopted, and the row goes.
#[test]
fn start_thread_adopts_only_a_session_in_the_repo() {
    let env = Env::new();
    let elsewhere = env.root.path().canonicalize().unwrap();
    let claude = started_anyway_in(env.fake.dir.path(), 1, Some(&elsewhere));
    let err = env
        .with_claude(&claude, |ctx| {
            tools::start_thread(ctx, &start_args("Fix login bug", "Fix it"))
        })
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("printed no session id"),
        "{err:#}"
    );
    assert!(
        db::list_sessions(&env.conn, "demo", None)
            .unwrap()
            .is_empty()
    );
}

/// When `claude agents` can't say whether the session started, the row stays: hooks match it by
/// title if it did.
#[test]
fn start_thread_keeps_the_row_when_it_cannot_tell() {
    let env = Env::new();
    env.fake
        .set("agents.stdout.1", "[]")
        .set("agents.exit", "1")
        .set("agents.exit.1", "0")
        .set("agents.stderr", "supervisor down")
        .set("bg.stdout", "oops\n")
        .set("bg.exit", "1");
    let err = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it")))
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("couldn't check whether"), "{msg}");
    assert!(msg.contains("supervisor down"), "{msg}");
    let rows = db::list_sessions(&env.conn, "demo", None).unwrap();
    assert_eq!(rows.len(), 1, "kept");
    assert_eq!(rows[0].short_id, None);
}

/// Runs `start` while `during` works on its own connection to the database, from the moment the
/// fake's `--bg` starts until `start` returns (`during`'s receiver gets a message then). Give
/// the fake a `bg.sleep` so `during` has time to act before the spawn returns.
fn while_spawning<T>(
    env: &Env,
    start: impl FnOnce() -> T,
    during: impl FnOnce(&Connection, &std::sync::mpsc::Receiver<()>) + Send,
) -> T {
    let db_path = env.root.path().join("chief.db");
    let spawning = env.fake.dir.path().join("bg.count");
    let (done, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(move || {
            let conn = db::open(&db_path).unwrap();
            while !spawning.exists() {
                if done_rx.try_recv().is_ok() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            during(&conn, &done_rx);
        });
        let out = start();
        let _ = done.send(());
        out
    })
}

/// Once `claude --bg` returned an id the thread runs: a database that refuses its ids (locked,
/// here) must not turn that into a failure, or the coordinator would start it again.
#[test]
fn start_thread_succeeds_when_its_ids_cannot_be_stored() {
    let env = Env::new();
    env.conn.busy_timeout(Duration::from_millis(50)).unwrap();
    env.fake
        .set("agents.stdout.1", "[]")
        .set("bg.stdout", "backgrounded · abc123 · whatever\n")
        .set("bg.sleep", "0.5");
    env.agents(&[agent(
        "abc123",
        "sid-abc",
        "demo-fix-xxxx",
        "working",
        Some(77),
    )]);
    let out = while_spawning(
        &env,
        || env.with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it"))),
        |conn, done| {
            conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
            let _ = done.recv();
            conn.execute_batch("ROLLBACK").unwrap();
        },
    )
    .unwrap();
    let name = out["name"].as_str().unwrap();
    assert_eq!(out["short_id"], "abc123");
    assert_eq!(out["address"], name);
    let warning = out["warning"].as_str().unwrap();
    assert!(
        warning.contains("couldn't store its session ids"),
        "{warning}"
    );
    assert!(warning.contains("don't start it again"), "{warning}");
    // Kept, for its hooks to attach by name.
    let row = env.row(name).unwrap();
    assert_eq!((row.short_id, row.session_id), (None, None));
    assert_eq!(env.fake.calls_of("--bg").len(), 1);
}

/// A failed `claude --bg` whose session isn't listed yet, but whose hooks already attached the
/// row (they stored its session id), started: the row is kept and the call succeeds.
#[test]
fn start_thread_keeps_a_row_its_hooks_attached() {
    let env = Env::new();
    env.agents(&[]);
    env.fake
        .set("bg.stdout", "something chief can't read\n")
        .set("bg.sleep", "0.5");
    let out = while_spawning(
        &env,
        || env.with(|ctx| tools::start_thread(ctx, &start_args("Fix", "Fix it"))),
        |conn, _| {
            let mut rows = db::list_sessions(conn, "demo", Some(Role::Thread)).unwrap();
            let mut row = rows.pop().expect("the row is inserted before the spawn");
            row.session_id = Some("sid-hooked".into());
            db::update_session(conn, &row).unwrap();
        },
    )
    .unwrap();
    let name = out["name"].as_str().unwrap();
    assert_eq!(out["short_id"], Value::Null);
    assert_eq!(out["address"], name);
    let warning = out["warning"].as_str().unwrap();
    assert!(warning.contains("hooks already ran"), "{warning}");
    assert!(warning.contains("something chief can't read"), "{warning}");
    let row = env.row(name).unwrap();
    assert_eq!(row.session_id.as_deref(), Some("sid-hooked"));
}

/// A call the client cancelled starts nothing: nobody would get its address or `next`.
#[test]
fn a_cancelled_start_thread_starts_nothing() {
    let env = Env::new();
    env.agents(&[]);
    env.fake.set("bg.stdout", "backgrounded · abc123 · x\n");
    let err = env
        .with(|ctx| tools::start_thread_unless(ctx, &start_args("Fix", "Fix it"), &|| true))
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(env.fake.calls_of("--bg").is_empty());
    assert!(
        db::list_sessions(&env.conn, "demo", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn repo_resolution() {
    let env = Env::new();
    let c = &env.config;
    assert_eq!(
        tools::resolve_repo(c, None).unwrap(),
        ("repo".into(), env.repo.clone())
    );
    assert_eq!(tools::resolve_repo(c, Some("repo")).unwrap().1, env.repo);
    assert_eq!(
        tools::resolve_repo(c, Some(" ")).unwrap().0,
        "repo",
        "blank means default"
    );
    let other = env.root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    assert_eq!(
        tools::resolve_repo(c, Some(other.to_str().unwrap())).unwrap(),
        ("other".into(), other.canonicalize().unwrap())
    );
    let err = tools::resolve_repo(c, Some("nope"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("known repositories: repo"), "{err}");
    assert!(tools::resolve_repo(c, Some("/does/not/exist")).is_err());
    let bare = ProjectConfig::new("B", "b", None, vec![]);
    let err = tools::resolve_repo(&bare, None).unwrap_err().to_string();
    assert!(err.contains("no default repository"), "{err}");
}

#[test]
fn list_threads_refreshes_and_hides_resolved() {
    let env = Env::new();
    env.insert("demo-idle-1111", |r| {
        r.short_id = Some("i1".into());
        r.session_id = Some("sid-i1".into());
        r.title = Some("Idle one".into());
        r.last_report = Some("z".repeat(500));
        r.branch = Some("worktree-idle".into());
    });
    env.insert("demo-busy-2222", |r| {
        r.short_id = Some("b2".into());
        r.session_id = Some("sid-b2".into());
    });
    env.insert("demo-done-3333", |r| {
        r.resolved = true;
        r.resolved_reason = Some("user".into());
        r.group_name = "resolved".into();
    });
    env.agents(&[
        agent("i1", "sid-i1", "demo-idle-1111", "done", None),
        agent("b2", "sid-b2", "demo-busy-2222", "working", Some(5)),
    ]);
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    assert!(out.get("warning").is_none());
    let threads = out["threads"].as_array().unwrap();
    let names: Vec<_> = threads
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["demo-busy-2222", "demo-idle-1111"],
        "board order, resolved hidden"
    );
    assert_eq!(threads[0]["group"], "working");
    assert_eq!(threads[0]["asleep"], false);
    let idle = &threads[1];
    assert_eq!(idle["asleep"], true);
    assert_eq!(idle["group"], "idle");
    assert_eq!(idle["branch"], "worktree-idle");
    assert_eq!(
        idle["last_report_preview"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        300
    );
    assert_eq!(idle["last_activity"], "1m ago");

    let all = ListThreadsArgs {
        include_resolved: Some(true),
    };
    env.fake
        .set("agents.exit", "1")
        .set("agents.stderr", "supervisor down");
    let out = env.with(|ctx| tools::list_threads(ctx, &all)).unwrap();
    assert_eq!(out["threads"].as_array().unwrap().len(), 3);
    assert!(out["warning"].as_str().unwrap().contains("supervisor down"));
}

#[test]
fn read_report_returns_full_text() {
    let env = Env::new();
    env.insert("demo-a-1111", |r| r.title = Some("A".into()));
    for (ts, text) in [(NOW - 3_600_000, "first"), (NOW - 60_000, "second\nfull")] {
        db::insert_report(&env.conn, "demo-a-1111", ts, text).unwrap();
    }
    let read = |name: &str, history| {
        env.with(|ctx| {
            tools::read_report(
                ctx,
                &ReadReportArgs {
                    name: name.into(),
                    history,
                },
            )
        })
    };
    let out = read("demo-a-1111", None).unwrap();
    assert_eq!(
        out["reports"],
        json!([{"at": "1m ago", "text": "second\nfull"}])
    );
    let out = read("demo-a-1111 [ref]", Some(5)).unwrap();
    assert_eq!(out["reports"].as_array().unwrap().len(), 2);
    assert_eq!(out["reports"][1]["text"], "first");

    env.insert("demo-new-2222", |_| {});
    assert!(read("demo-new-2222", None).unwrap()["note"].is_string());
    // §5 "Background work": an idle notice from a turn that ended while its subagent works.
    env.insert("demo-wait-3333", |r| {
        r.pid = Some(42);
        r.subagents_busy = 1;
    });
    let out = read("demo-wait-3333", None).unwrap();
    assert_eq!(out["running"], "1 subagent");
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .starts_with("Not finished: 1 subagent still running.")
    );
    assert!(read("demo-a-1111", None).unwrap().get("running").is_none());
    assert!(read("nope", None).is_err());
    let coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
    db::insert_session(&env.conn, &coord).unwrap();
    assert!(
        read("chief-demo", None).is_err(),
        "the coordinator is not a thread"
    );
}

#[test]
fn wake_thread_respawns_and_waits_for_a_pid() {
    let env = Env::new();
    env.insert("demo-w-1111", |r| {
        r.short_id = Some("w1".into());
        r.session_id = Some("sid-w1".into());
    });
    let asleep = agent("w1", "sid-w1", "demo-w-1111", "done", None);
    let awake = agent("w1", "sid-w1", "demo-w-1111", "working", Some(9));
    let wake = || {
        env.with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-w-1111".into(),
                },
            )
        })
    };

    env.fake
        .set("agents.stdout.1", &json!([asleep]).to_string())
        .set("agents.stdout.2", &json!([asleep]).to_string());
    env.agents(std::slice::from_ref(&awake));
    assert_eq!(
        wake().unwrap(),
        json!({"name": "demo-w-1111", "alive": true, "respawned": true, "address": "demo-w-1111"})
    );
    assert_eq!(env.fake.calls_of("respawn"), [vec!["respawn", "w1"]]);
    assert_eq!(env.row("demo-w-1111").unwrap().pid, Some(9));

    // Already alive: nothing to do.
    assert_eq!(wake().unwrap()["respawned"], false);
    assert_eq!(env.fake.calls_of("respawn").len(), 1);

    // Never comes up.
    env.agents(&[asleep]);
    let out = wake().unwrap();
    assert_eq!(
        (out["alive"].as_bool(), out["respawned"].as_bool()),
        (Some(false), Some(true))
    );

    env.insert("demo-unspawned-2222", |_| {});
    let err = env
        .with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-unspawned-2222".into(),
                },
            )
        })
        .unwrap_err();
    assert!(err.to_string().contains("never started"), "{err}");
}

/// `stop_thread` stops a running thread in its own account, after moving the main pane off it
/// (a pane showing it would respawn it), and says when there is nothing to stop.
#[test]
fn stop_thread_stops_a_running_thread_and_moves_the_pane_off_it() {
    let env = Env::new();
    env.insert("demo-s-1111", |r| {
        r.short_id = Some("s1".into());
        r.session_id = Some("sid-s1".into());
    });
    let stop = || {
        env.with(|ctx| {
            tools::stop_thread(
                ctx,
                &NameArgs {
                    name: "demo-s-1111".into(),
                },
            )
        })
    };
    db::set_pane_target(&env.conn, "demo", Some("demo-s-1111")).unwrap();
    env.agents(&[agent("s1", "sid-s1", "demo-s-1111", "working", Some(9))]);
    assert_eq!(
        stop().unwrap(),
        json!({"name": "demo-s-1111", "stopped": true})
    );
    assert_eq!(env.fake.calls_of("stop"), [vec!["stop", "s1"]]);
    assert_eq!(db::get_pane_target(&env.conn, "demo").unwrap(), None);

    // Asleep: nothing to stop.
    env.agents(&[agent("s1", "sid-s1", "demo-s-1111", "done", None)]);
    assert_eq!(stop().unwrap()["stopped"], false);
    assert_eq!(env.fake.calls_of("stop").len(), 1);

    let err = env
        .with(|ctx| {
            tools::stop_thread(
                ctx,
                &NameArgs {
                    name: "chief-demo".into(),
                },
            )
        })
        .unwrap_err();
    assert!(err.to_string().contains("not a thread"), "{err}");
}

/// `wake_thread`'s wait is a bound (§7): a `claude agents` that hangs after the respawn gets only
/// the time left, not its own 15 s.
#[test]
fn wake_thread_stops_waiting_at_its_deadline() {
    let env = Env::new();
    env.insert("demo-w-1111", |r| {
        r.short_id = Some("w1".into());
        r.session_id = Some("sid-w1".into());
    });
    let bin = env.fake.dir.path().join("claude-slow-after-respawn");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
dir=$(dirname "$0")
case "$1" in
  respawn) : > "$dir/respawned" ;;
  agents)
    [ -f "$dir/respawned" ] && exec sleep 30
    echo '[{"id":"w1","sessionId":"sid-w1","name":"demo-w-1111","state":"done"}]' ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let claude = chief::claude::Claude {
        bin,
        account: None,
        config_dir: env.fake.config_dir(),
    };
    let start = std::time::Instant::now();
    let out = env
        .with_claude(&claude, |ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-w-1111".into(),
                },
            )
        })
        .unwrap();
    let took = start.elapsed();
    assert_eq!(
        (out["alive"].as_bool(), out["respawned"].as_bool()),
        (Some(false), Some(true)),
        "{out}"
    );
    // The wake timeout is 1 s here; the listing alone would take 15 s.
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// A thread in another account (§3): only that account's `claude agents` lists it, so it is
/// synced from there, and woken there; the project's listing says nothing about it.
#[test]
fn a_thread_in_another_account_is_synced_and_woken_there() {
    let env = Env::new();
    let cpb = common::account(env.root.path(), "cpb");
    env.insert("demo-far-1111", |r| {
        r.short_id = Some("f1".into());
        r.session_id = Some("sid-f1".into());
        common::in_account(r, "cpb", "PB2", Some(&cpb));
    });
    env.insert("demo-near-2222", |r| {
        r.short_id = Some("n2".into());
        r.session_id = Some("sid-n2".into());
    });
    env.agents(&[agent("n2", "sid-n2", "demo-near-2222", "done", None)]);
    common::account_agents(
        &cpb,
        &[agent("f1", "sid-f1", "demo-far-1111", "working", Some(9))],
    );
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    assert!(out.get("warning").is_none(), "{out}");
    let far = &out["threads"][0];
    assert_eq!(
        (&far["name"], &far["group"], &far["asleep"]),
        (&json!("demo-far-1111"), &json!("working"), &json!(false))
    );
    // Names don't reach across accounts: its address is its live socket there.
    assert_eq!(
        (&far["account"], &far["address"]),
        (&json!("PB2"), &json!("uds:/tmp/cc-socks/9.sock"))
    );
    let near = &out["threads"][1];
    assert_eq!(
        (&near["account"], &near["address"]),
        (&Value::Null, &json!("demo-near-2222"))
    );
    let mut listed: Vec<String> = env
        .fake
        .accounts()
        .into_iter()
        .filter(|(first, _)| first == "agents")
        .map(|(_, account)| account)
        .collect();
    listed.sort();
    assert_eq!(listed, [cpb.to_string_lossy().into_owned(), "unset".into()]);

    // Its account fails: its row keeps what it had, the others are synced, and it says so.
    std::fs::write(cpb.join("fake-agents.stdout"), "not json").unwrap();
    env.agents(&[agent("n2", "sid-n2", "demo-near-2222", "working", Some(3))]);
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    let warning = out["warning"].as_str().unwrap();
    assert!(
        warning.contains(&format!("in the account {}", cpb.display())),
        "{warning}"
    );
    assert_eq!(env.row("demo-far-1111").unwrap().pid, Some(9));
    assert_eq!(env.row("demo-near-2222").unwrap().pid, Some(3));
    let far = out["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "demo-far-1111")
        .unwrap();
    assert_eq!(
        far["address"],
        Value::Null,
        "never a stored, maybe stale pid"
    );

    // Asleep in its account: respawned in that account, and only that account is polled.
    let calls_before = env.fake.calls().len();
    common::account_agents(
        &cpb,
        &[agent("f1", "sid-f1", "demo-far-1111", "done", None)],
    );
    let k = std::fs::read_to_string(env.fake.dir.path().join("agents.count"))
        .unwrap()
        .trim()
        .parse::<usize>()
        .unwrap();
    std::fs::write(
        cpb.join(format!("fake-agents.stdout.{}", k + 2)),
        json!([agent("f1", "sid-f1", "demo-far-1111", "working", Some(11))]).to_string(),
    )
    .unwrap();
    let out = env
        .with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-far-1111".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(
        (&out["alive"], &out["respawned"]),
        (&json!(true), &json!(true))
    );
    let after: Vec<_> = env.fake.accounts()[calls_before..].to_vec();
    let cpb_path = cpb.to_string_lossy().into_owned();
    assert!(
        after.iter().all(|(_, account)| *account == cpb_path),
        "{after:?}"
    );
    assert!(
        after.iter().any(|(first, _)| first == "respawn"),
        "{after:?}"
    );
    assert_eq!(env.row("demo-far-1111").unwrap().pid, Some(11));
    // A new process, a new address: subscribe again there.
    assert_eq!(out["address"], "uds:/tmp/cc-socks/11.sock");
    let next = out["next"].as_str().unwrap();
    assert!(
        next.starts_with(
            "Subscribe again: call SendMessage with exactly {\"to\": \
             \"uds:/tmp/cc-socks/11.sock\", \"notify_when_idle\": true}"
        ),
        "{next}"
    );
}

#[test]
fn resolve_and_reopen() {
    let env = Env::new();
    env.insert("demo-r-1111", |_| {});
    let call = |reopen| {
        env.with(|ctx| {
            tools::resolve_thread(
                ctx,
                &ResolveThreadArgs {
                    name: "demo-r-1111".into(),
                    reopen,
                },
            )
        })
        .unwrap()
    };
    assert_eq!(
        call(None),
        json!({"name": "demo-r-1111", "resolved": true, "group": "resolved"})
    );
    assert_eq!(
        env.row("demo-r-1111").unwrap().resolved_reason.as_deref(),
        Some("coordinator")
    );
    let out = call(Some(true));
    assert_eq!(out["resolved"], false);
    assert_ne!(out["group"], "resolved");
}

/// Removed outside chief while its worktree remains: resolved, not cleaned. Its session is gone,
/// so it can't be reopened or woken (`claude respawn` would fail), with a message that says so.
#[test]
fn removed_threads_cannot_be_reopened_or_woken() {
    let env = Env::new();
    env.insert("demo-gone-1111", |r| {
        r.short_id = Some("g1".into());
        r.session_id = Some("sid-g1".into());
        r.resolved = true;
        r.resolved_reason = Some("removed".into());
        r.group_name = "resolved".into();
        r.removed_at = Some(NOW - 1000);
        r.cleanup_blocked = Some("session removed outside chief; worktree remains at /r".into());
    });
    // The coordinator is listed; the thread isn't.
    env.agents(&[agent("c1", "sid-c1", "chief-demo", "done", Some(1))]);
    let want = "demo-gone-1111's session was removed outside chief, so it can't be woken, \
                reopened or messaged; start a new thread (its reports are still readable with \
                read_report)";
    let reopen = env.with(|ctx| {
        tools::resolve_thread(
            ctx,
            &ResolveThreadArgs {
                name: "demo-gone-1111".into(),
                reopen: Some(true),
            },
        )
    });
    assert_eq!(reopen.unwrap_err().to_string(), want);
    assert!(env.row("demo-gone-1111").unwrap().resolved);
    let wake = env.with(|ctx| {
        tools::wake_thread(
            ctx,
            &NameArgs {
                name: "demo-gone-1111".into(),
            },
        )
    });
    assert_eq!(wake.unwrap_err().to_string(), want);
    assert!(env.fake.calls_of("respawn").is_empty());
}

#[test]
fn cleaned_threads_keep_their_reports_but_nothing_else() {
    let env = Env::new();
    env.insert("demo-gone-1111", |r| {
        r.short_id = Some("g1".into());
        r.session_id = Some("sid-g1".into());
        r.resolved = true;
        r.resolved_reason = Some("merged".into());
        r.group_name = "resolved".into();
        r.cleaned_at = Some(NOW - 1000);
    });
    db::insert_report(&env.conn, "demo-gone-1111", NOW - 2000, "merged it").unwrap();
    env.agents(&[]);
    let want = "demo-gone-1111 was cleaned up; start a new thread (its reports are still \
                readable with read_report)";

    let err = env
        .with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-gone-1111".into(),
                },
            )
        })
        .unwrap_err();
    assert_eq!(err.to_string(), want);
    assert!(env.fake.calls_of("respawn").is_empty());

    let reopen = |reopen| {
        env.with(|ctx| {
            tools::resolve_thread(
                ctx,
                &ResolveThreadArgs {
                    name: "demo-gone-1111 [ref]".into(),
                    reopen: Some(reopen),
                },
            )
        })
    };
    assert_eq!(reopen(true).unwrap_err().to_string(), want);
    assert_eq!(reopen(false).unwrap()["resolved"], true);

    let listed = |include_resolved| {
        env.with(|ctx| {
            tools::list_threads(
                ctx,
                &ListThreadsArgs {
                    include_resolved: Some(include_resolved),
                },
            )
        })
        .unwrap()
    };
    assert_eq!(listed(false)["threads"], json!([]));
    let all = listed(true);
    assert_eq!(all["threads"][0]["cleaned"], true);

    let out = env
        .with(|ctx| {
            tools::read_report(
                ctx,
                &ReadReportArgs {
                    name: "demo-gone-1111".into(),
                    history: None,
                },
            )
        })
        .unwrap();
    assert_eq!(out["reports"][0]["text"], "merged it");
}

/// An agp that watches background sessions: its help lists `agp bg`.
fn agp_with_bg(env: &Env) {
    env.agp.set(
        "help.stdout",
        "Usage:\n  agp [options] [--] [claude args]\n  agp bg [--json]   background sessions\n  \
         agp bg watch <session> [--as <id>]\n  agp bg move <session> [--to <id>] [--json]\n",
    );
}

/// `agp bg --json` with one session: `profile` (folder `dir`), moved there from `from` unless
/// `from` is None.
fn bg_record(
    state: &str,
    profile: (&str, &str, &Path),
    from: Option<(&str, &str, &Path)>,
    reason: &str,
) -> String {
    let moves = from.map_or(json!([]), |(id, label, dir)| {
        json!([{"at": NOW / 1000 - 30, "from": id, "from_label": label, "from_config_dir": dir,
                "to": profile.0, "to_label": profile.1, "to_config_dir": profile.2,
                "reason": reason}])
    });
    json!({"sessions": [{"session_id": "sid-abc", "short_id": "abc123", "name": "x",
        "profile": profile.0, "label": profile.1, "config_dir": profile.2, "cwd": "/r",
        "on_limit": "switch", "state": state, "limit": null, "moves": moves,
        "watcher_pid": 1, "watching": true, "updated_at": NOW / 1000}]})
    .to_string()
}

/// §5 "Limits and moves": with an agp that watches background sessions, agp starts the thread
/// in the account placement chose and watches it; chief never runs `claude --bg` itself. When agp
/// moves it, the next refresh points the row at its new account, and the coordinator gets the new
/// address.
#[test]
fn start_thread_runs_through_agp_and_follows_its_moves() {
    let env = Env::new();
    let pool = pool(&env);
    agp_with_bg(&env);
    env.agp
        .set("as.stdout", "backgrounded · abc123 · whatever\n");
    env.fake.set("agents.stdout", "[]");
    common::account_agents(
        &pool.cpe,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-x-xxxx",
            "working",
            Some(77),
        )],
    );
    let out = env
        .with(|ctx| tools::start_thread(ctx, &start_args("Fix login bug", "Fix it")))
        .unwrap();
    let name = out["name"].as_str().unwrap().to_string();
    assert_eq!(
        (&out["account"], &out["runner"], &out["address"]),
        (
            &json!("PB3"),
            &json!("agp"),
            &json!("uds:/tmp/cc-socks/77.sock")
        )
    );
    assert!(env.fake.calls_of("--bg").is_empty(), "agp started it");
    let launch = &env.agp.calls_of("--as")[0];
    assert_eq!(
        launch[..6],
        ["--as", "cpe", "--on-limit", "switch", "--", "--bg"].map(String::from)
    );
    assert_eq!(arg_after(launch, "--name"), name);
    let brief = arg_after(launch, "--append-system-prompt");
    assert!(
        brief.contains("You may be moved to another Claude account"),
        "{brief}"
    );
    let n = env.agp.calls().iter().position(|c| c[0] == "--as").unwrap();
    assert_eq!(env.agp.cwd(n), env.repo, "launched in the repo");
    let row = env.row(&name).unwrap();
    assert_eq!(
        (row.runner.as_deref(), row.account_id.as_deref()),
        (Some("agp"), Some("cpe"))
    );

    // agp moved it to PB5 after PB3's limit.
    env.agp.set(
        "bg.--json.stdout",
        &bg_record(
            "watching",
            ("pb5", "PB5", &pool.pb5),
            Some(("cpe", "PB3", &pool.cpe)),
            "5-hour limit",
        ),
    );
    common::account_agents(
        &pool.pb5,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-x-xxxx",
            "working",
            Some(88),
        )],
    );
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    let t = &out["threads"][0];
    assert_eq!(
        (&t["account"], &t["address"], &t["moved"]),
        (
            &json!("PB5"),
            &json!("uds:/tmp/cc-socks/88.sock"),
            &json!("moved from PB3 to PB5 after the 5-hour limit")
        )
    );
    let row = env.row(&name).unwrap();
    assert_eq!(row.account_dir.as_deref(), Some(pool.pb5.to_str().unwrap()));
    assert_eq!(
        row.prior_accounts()
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["cpe"]
    );
    assert_eq!(row.moved_at, Some(NOW - 30_000));
}

fn agp_thread(env: &Env, name: &str, dir: &Path, pid: Option<u32>) {
    env.insert(name, |r| {
        r.short_id = Some("abc123".into());
        r.session_id = Some("sid-abc".into());
        r.repo_path = Some(env.repo.to_string_lossy().into_owned());
        r.runner = Some("agp".into());
        r.pid = pid;
        common::in_account(r, "cpe", "PB3", Some(dir));
    });
}

/// A revive waits while agp moves the thread (it would start it again in the account agp just
/// stopped it in), then finds it running in the new one: no respawn at all.
#[test]
fn wake_thread_waits_out_a_move_instead_of_reviving_it() {
    let env = Env::new();
    let pool = pool(&env);
    agp_with_bg(&env);
    agp_thread(&env, "demo-t-1111", &pool.cpe, None);
    // Stopped in PB3 while agp moves it.
    common::account_agents(
        &pool.cpe,
        &[agent("abc123", "sid-abc", "demo-t-1111", "blocked", None)],
    );
    common::account_agents(
        &pool.pb5,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-t-1111",
            "working",
            Some(88),
        )],
    );
    let moving = bg_record(
        "moving",
        ("pb5", "PB5", &pool.pb5),
        Some(("cpe", "PB3", &pool.cpe)),
        "5-hour limit",
    );
    env.agp
        .set("bg.stdout.1", &moving)
        .set("bg.stdout.2", &moving)
        .set(
            "bg.stdout",
            &bg_record(
                "watching",
                ("pb5", "PB5", &pool.pb5),
                Some(("cpe", "PB3", &pool.cpe)),
                "5-hour limit",
            ),
        );
    let out = env
        .with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-t-1111".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(
        (&out["alive"], &out["respawned"], &out["address"]),
        (
            &json!(true),
            &json!(false),
            &json!("uds:/tmp/cc-socks/88.sock")
        )
    );
    assert_eq!(out["moved"], "moved from PB3 to PB5 after the 5-hour limit");
    assert!(
        out["next"]
            .as_str()
            .unwrap()
            .starts_with("It moved, so subscribe again: call SendMessage with exactly {\"to\": \"uds:/tmp/cc-socks/88.sock\""),
        "{out}"
    );
    assert!(env.fake.calls_of("respawn").is_empty());
    assert_eq!(
        env.row("demo-t-1111").unwrap().account_id.as_deref(),
        Some("pb5")
    );
}

/// A thread agp watches is revived in its own account and handed back to agp's watcher.
#[test]
fn wake_thread_rearms_agps_watch() {
    let env = Env::new();
    let pool = pool(&env);
    agp_with_bg(&env);
    agp_thread(&env, "demo-t-1111", &pool.cpe, None);
    common::account_agents(
        &pool.cpe,
        &[agent("abc123", "sid-abc", "demo-t-1111", "done", None)],
    );
    env.agp.set(
        "bg.--json.stdout",
        &bg_record("dormant", ("cpe", "PB3", &pool.cpe), None, ""),
    );
    // Listed with a pid from the second listing on (after the respawn).
    std::fs::write(
        pool.cpe.join("fake-agents.stdout.3"),
        Value::from(vec![agent(
            "abc123",
            "sid-abc",
            "demo-t-1111",
            "working",
            Some(90),
        )])
        .to_string(),
    )
    .unwrap();
    std::fs::copy(
        pool.cpe.join("fake-agents.stdout.3"),
        pool.cpe.join("fake-agents.stdout.4"),
    )
    .unwrap();
    let out = env
        .with(|ctx| {
            tools::wake_thread(
                ctx,
                &NameArgs {
                    name: "demo-t-1111".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(out["respawned"], true, "{out}");
    assert_eq!(env.fake.calls_of("respawn").len(), 1);
    let watch = &env
        .agp
        .calls_of("bg")
        .into_iter()
        .find(|c| c.get(1).map(String::as_str) == Some("watch"))
        .expect("agp bg watch");
    assert_eq!(
        watch[..5],
        ["bg", "watch", "sid-abc", "--as", "cpe"].map(String::from)
    );
    assert_eq!(arg_after(watch, "--on-limit"), "switch");
}

/// `move_thread`: agp moves it now, to the account the coordinator named, and chief follows.
#[test]
fn move_thread_moves_it_through_agp() {
    let env = Env::new();
    let pool = pool(&env);
    agp_with_bg(&env);
    agp_thread(&env, "demo-t-1111", &pool.cpe, Some(77));
    common::account_agents(
        &pool.cpe,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-t-1111",
            "working",
            Some(77),
        )],
    );
    common::account_agents(
        &pool.pb5,
        &[agent(
            "abc123",
            "sid-abc",
            "demo-t-1111",
            "working",
            Some(88),
        )],
    );
    env.agp
        .set(
            "bg.stdout.1",
            &bg_record("watching", ("cpe", "PB3", &pool.cpe), None, ""),
        )
        .set(
            "bg.stdout",
            &bg_record(
                "watching",
                ("pb5", "PB5", &pool.pb5),
                Some(("cpe", "PB3", &pool.cpe)),
                "manual move",
            ),
        )
        .set(
            "bg.move.stdout",
            &json!({"moved": true, "session_id": "sid-abc", "short_id": "abc123",
                    "from": "cpe", "from_label": "PB3", "to": "pb5", "to_label": "PB5",
                    "to_config_dir": pool.pb5})
            .to_string(),
        );
    let args = tools::MoveThreadArgs {
        name: "demo-t-1111".into(),
        account: Some("PB5".into()),
    };
    let out = env.with(|ctx| tools::move_thread(ctx, &args)).unwrap();
    assert_eq!(
        (&out["moved"], &out["account"], &out["address"]),
        (
            &json!("moved from PB3 to PB5 on request"),
            &json!("PB5"),
            &json!("uds:/tmp/cc-socks/88.sock")
        )
    );
    let mv = env
        .agp
        .calls_of("bg")
        .into_iter()
        .find(|c| c.get(1).map(String::as_str) == Some("move"))
        .expect("agp bg move");
    assert_eq!(mv, ["bg", "move", "sid-abc", "--to", "pb5", "--json"]);
    assert_eq!(
        env.row("demo-t-1111").unwrap().account_id.as_deref(),
        Some("pb5")
    );

    // Where it runs already, or an account outside the pool: refused before agp is asked.
    let moves = || {
        env.agp
            .calls_of("bg")
            .iter()
            .filter(|c| c.get(1).map(String::as_str) == Some("move"))
            .count()
    };
    let err = env
        .with(|ctx| {
            tools::move_thread(
                ctx,
                &tools::MoveThreadArgs {
                    name: "demo-t-1111".into(),
                    account: Some("PB5".into()),
                },
            )
        })
        .unwrap_err();
    assert!(err.to_string().contains("runs in PB5 already"), "{err}");
    let err = env
        .with(|ctx| {
            tools::move_thread(
                ctx,
                &tools::MoveThreadArgs {
                    name: "demo-t-1111".into(),
                    account: Some("Main".into()),
                },
            )
        })
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not an account in this project's pool"),
        "{err}"
    );
    assert_eq!(moves(), 1);
}

/// A thread whose job state says a usage limit stopped it is `limited`, with the limit, until a
/// sync finds it going on.
#[test]
fn a_rate_limited_thread_is_limited() {
    let env = Env::new();
    env.insert("demo-t-1111", |r| {
        r.short_id = Some("abc123".into());
        r.session_id = Some("sid-abc".into());
        r.pid = Some(77);
    });
    env.agents(&[agent(
        "abc123",
        "sid-abc",
        "demo-t-1111",
        "blocked",
        Some(77),
    )]);
    let jobs = env.fake.config_dir().join("jobs/abc123");
    std::fs::create_dir_all(&jobs).unwrap();
    let limited = json!({
        "state": "blocked",
        "needs": "rate limited — wait and retry · You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)",
        "detail": "You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)",
    });
    std::fs::write(jobs.join("state.json"), limited.to_string()).unwrap();
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    let t = &out["threads"][0];
    assert_eq!(
        (&t["group"], &t["limit"], &t["waiting_reason"]),
        (
            &json!("limited"),
            &json!("You've hit your 5-hour limit · resets 6:10am (Europe/Madrid)"),
            &Value::Null
        )
    );
    let row = env.row("demo-t-1111").unwrap();
    assert_eq!(row.limited_since, Some(NOW));

    // A message started a turn that hit the limit again: limited again, from that sync.
    let mut row = env.row("demo-t-1111").unwrap();
    row.limited_since = Some(NOW - 5_000);
    row.last_hook_at = Some(NOW - 1_000);
    db::update_session(&env.conn, &row).unwrap();
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    assert_eq!(out["threads"][0]["group"], "limited");
    assert_eq!(env.row("demo-t-1111").unwrap().limited_since, Some(NOW));

    // It went on after the reset.
    std::fs::write(
        jobs.join("state.json"),
        json!({"state": "working"}).to_string(),
    )
    .unwrap();
    env.agents(&[agent(
        "abc123",
        "sid-abc",
        "demo-t-1111",
        "working",
        Some(77),
    )]);
    let out = env
        .with(|ctx| tools::list_threads(ctx, &ListThreadsArgs::default()))
        .unwrap();
    let t = &out["threads"][0];
    assert_eq!(t["group"], "working");
    assert!(t.get("limit").is_none(), "{t}");
}
