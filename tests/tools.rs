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
        Env {
            root,
            conn,
            config,
            fake: Fake::new(),
            dir,
            repo,
            trust,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&ToolCtx) -> T) -> T {
        let claude = self.fake.claude();
        let ctx = ToolCtx {
            conn: &self.conn,
            config: &self.config,
            claude: &claude,
            project_dir: &self.dir,
            trust_config: &self.trust,
            now,
            resolve_timeout: Duration::from_millis(300),
            wake_timeout: Duration::from_millis(300),
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
        json!({"name": "demo-w-1111", "alive": true, "respawned": true})
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
