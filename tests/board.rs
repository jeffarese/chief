//! The board's refresher and workers (`chief::board::{refresh, jobs}`) against the fake `claude`
//! and `gh`, a temp database and real temporary git repositories. No terminal involved.

mod common;

use std::path::PathBuf;

use chief::board::app::{Action, Msg, Snapshot};
use chief::board::jobs;
use chief::board::refresh::{self, Sources};
use chief::cleanup::Mode;
use chief::config::{ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::pr::Gh;
use chief::state::Role;
use common::{Fake, GitRepo, agent};
use rusqlite::Connection;
use serde_json::Value;

const SHORT: &str = "b65c3d73";
const SID: &str = "sid-b65c3d73";
const NAME: &str = "demo-notes-1a2b";

struct Env {
    _root: tempfile::TempDir,
    src: Sources,
    conn: Connection,
    claude: Fake,
    gh: Fake,
    rex: Fake,
    git: GitRepo,
    worktree: PathBuf,
}

impl Env {
    /// A project with a coordinator and one resolved, stopped thread whose worktree holds one
    /// pushed commit.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("projects/demo");
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        let db_path = root.path().join("chief.db");
        let conn = db::open(&db_path).unwrap();
        db::register_project(&conn, "demo", &dir, 0).unwrap();
        let git = GitRepo::new();
        let worktree = git.worktree("add-notes");
        git.commit(&worktree, "notes.txt", "Add notes");
        git.push(&worktree);
        let config = ProjectConfig::new(
            "Demo",
            "demo",
            None,
            vec![Repo {
                name: "repo".into(),
                path: git.repo.clone(),
            }],
        );
        config.save(&dir.join("project.toml")).unwrap();

        let claude = Fake::new();
        let gh = Fake::named("gh");
        gh.set("pr.stdout", "[]");
        let rex = Fake::named("rex");
        rex.set("list.stdout", r#"{"sessions": []}"#);
        let src = Sources {
            slug: "demo".into(),
            dir,
            db: db_path,
            claude_bin: claude.bin(),
            gh_bin: gh.bin(),
            rex_bin: rex.bin(),
        };
        let env = Env {
            _root: root,
            src,
            conn,
            claude,
            gh,
            rex,
            git,
            worktree,
        };
        let now = db::now_ms();
        let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, now);
        coord.short_id = Some("c0".into());
        coord.session_id = Some("sid-c0".into());
        db::insert_session(&env.conn, &coord).unwrap();
        let mut row = SessionRow::new(NAME, "demo", Role::Thread, now - 60_000);
        row.short_id = Some(SHORT.into());
        row.session_id = Some(SID.into());
        row.repo_path = Some(env.git.repo.to_string_lossy().into_owned());
        row.cwd = Some(env.worktree.to_string_lossy().into_owned());
        row.branch = Some("worktree-add-notes".into());
        row.last_hook = Some("Stop".into());
        row.resolved = true;
        row.resolved_reason = Some("user".into());
        row.group_name = "resolved".into();
        db::insert_session(&env.conn, &row).unwrap();
        env.agents(&[
            agent("c0", "sid-c0", "chief-demo", "working", Some(11)),
            agent(SHORT, SID, NAME, "done", None),
        ]);
        env.claude.set("rm.stdout", &format!("removed {SHORT}\n"));
        env
    }

    fn agents(&self, rows: &[Value]) {
        self.claude
            .set("agents.stdout", &Value::from(rows.to_vec()).to_string());
    }

    fn row(&self) -> SessionRow {
        db::get_session(&self.conn, NAME).unwrap().unwrap()
    }

    fn tick(&self) -> Snapshot {
        match refresh::tick(&self.conn, &self.claude.claude(), "demo") {
            Msg::Rows(s) => s,
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

#[test]
fn tick_syncs_and_reads_the_project() {
    let env = Env::new();
    db::set_pane_target(&env.conn, "demo", Some(NAME)).unwrap();
    let snap = env.tick();
    assert_eq!(snap.error, None);
    assert_eq!(snap.sessions.len(), 2);
    assert_eq!(snap.pane_target.as_deref(), Some(NAME));
    let coord = snap
        .sessions
        .iter()
        .find(|r| r.name == "chief-demo")
        .unwrap();
    assert_eq!(coord.pid, Some(11), "synced from claude agents");
    assert_eq!(env.claude.calls_of("agents").len(), 1);
}

#[test]
fn a_failing_claude_agents_keeps_the_stored_rows() {
    let env = Env::new();
    env.claude
        .set("agents.exit", "1")
        .set("agents.stderr", "boom");
    let snap = env.tick();
    assert_eq!(snap.sessions.len(), 2, "stored rows still shown");
    let error = snap.error.unwrap();
    assert!(error.contains("boom"), "{error}");
    assert!(!error.contains('\n'), "one line for the status bar");
}

#[test]
fn pr_tick_reports_the_gh_hint_once() {
    let env = Env::new();
    let missing = Gh::new(env.gh.dir.path().join("no-such-gh"));
    let first = refresh::pr_tick(&env.conn, &env.claude.claude(), &missing, &env.src.dir).unwrap();
    assert!(first.hint.unwrap().contains("isn't installed"));
    let second = refresh::pr_tick(&env.conn, &env.claude.claude(), &missing, &env.src.dir).unwrap();
    assert_eq!(second.hint, None);

    let gh = env.gh.gh();
    let summary = refresh::pr_tick(&env.conn, &env.claude.claude(), &gh, &env.src.dir).unwrap();
    assert_eq!((summary.errors, summary.blocked), (0, 0));
}

#[test]
fn old_events_are_pruned() {
    let env = Env::new();
    for ts in [1, db::now_ms()] {
        db::insert_event(
            &env.conn,
            &db::Event {
                session_name: None,
                session_id: None,
                project_slug: "demo".into(),
                hook: "Stop".into(),
                ts,
                summary: None,
            },
        )
        .unwrap();
    }
    let removed = db::prune_events(&env.conn, db::now_ms() - refresh::EVENTS_KEPT_MS).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(db::recent_events(&env.conn, "demo", 10).unwrap().len(), 1);
}

/// Showing a thread while the layout is open and the pane shows the coordinator sends exactly one
/// Ctrl+Z to the `main` block; showing what is already there sends nothing.
#[test]
fn show_switches_the_main_pane() {
    let env = Env::new();
    env.rex
        .set(
            "list.stdout",
            r#"{"sessions": [{"session_id": "session:other", "label": "chief-demo-2"},
                             {"session_id": "session:1", "label": "chief-demo"}]}"#,
        )
        .set(
            "block.stdout",
            r#"{"blocks": [{"block_id": "block:main", "label": "main"},
                           {"block_id": "block:board", "label": "board"}]}"#,
        );
    db::set_pane_showing(&env.conn, "demo", Some("chief-demo")).unwrap();
    let msg = jobs::run(&env.src, Action::Show(Some(NAME.into())));
    assert_eq!(msg, Msg::Notice(format!("Main pane: {NAME}")));
    assert_eq!(
        env.rex.calls_of("send-key"),
        [vec![
            "send-key",
            "--autostart=false",
            "-b",
            "block:main",
            "ctrl+z"
        ]]
    );
    assert_eq!(
        env.rex.calls_of("block")[0],
        [
            "block",
            "ls",
            "--autostart=false",
            "-s",
            "session:1",
            "--json"
        ]
    );

    jobs::run(&env.src, Action::Show(None));
    assert_eq!(env.rex.calls_of("send-key").len(), 1, "already showing it");
}

#[test]
fn show_resolve_and_reopen() {
    let env = Env::new();
    let msg = jobs::run(&env.src, Action::Show(Some(NAME.into())));
    assert_eq!(msg, Msg::Notice(format!("Main pane: {NAME}")));
    assert_eq!(
        db::get_pane_target(&env.conn, "demo").unwrap().as_deref(),
        Some(NAME)
    );
    jobs::run(&env.src, Action::Show(None));
    assert_eq!(db::get_pane_target(&env.conn, "demo").unwrap(), None);

    let msg = jobs::run(
        &env.src,
        Action::SetResolved {
            name: NAME.into(),
            resolved: false,
        },
    );
    assert_eq!(msg, Msg::Notice(format!("Reopened {NAME}.")));
    assert!(!env.row().resolved);
    jobs::run(
        &env.src,
        Action::SetResolved {
            name: NAME.into(),
            resolved: true,
        },
    );
    let row = env.row();
    assert!(row.resolved);
    assert_eq!(row.resolved_reason.as_deref(), Some("user"));
}

#[test]
fn safe_cleanup_cleans_a_thread_that_loses_nothing() {
    let env = Env::new();
    let msg = jobs::run(
        &env.src,
        Action::Clean {
            name: NAME.into(),
            mode: Mode::Safe,
        },
    );
    assert_eq!(msg, Msg::Done(format!("Cleaned up {NAME}.")));
    assert!(env.row().cleaned_at.is_some());

    // Cleaned: reopening and waking are refused with the shared message.
    let msg = jobs::run(
        &env.src,
        Action::SetResolved {
            name: NAME.into(),
            resolved: false,
        },
    );
    let Msg::Notice(text) = msg else {
        panic!("{msg:?}")
    };
    assert!(text.contains("was cleaned up"), "{text}");
    let Msg::Done(text) = jobs::run(&env.src, Action::Wake(NAME.into())) else {
        panic!("wake answers with Done")
    };
    assert!(text.contains("was cleaned up"), "{text}");
}

#[test]
fn blocked_cleanup_asks_then_discards() {
    let env = Env::new();
    env.git.commit(&env.worktree, "draft.txt", "Unpushed draft");
    std::fs::write(env.worktree.join("scratch.txt"), "wip\n").unwrap();

    let msg = jobs::run(
        &env.src,
        Action::Clean {
            name: NAME.into(),
            mode: Mode::Safe,
        },
    );
    let Msg::CleanBlocked { name, loss } = msg else {
        panic!("expected a confirmation, got {msg:?}")
    };
    assert_eq!(name, NAME);
    assert!(
        loss.starts_with("Cleanup stopped: 1 unpushed commit"),
        "{loss}"
    );
    assert!(loss.contains("Unpushed draft"), "{loss}");
    assert!(loss.contains("scratch.txt"), "{loss}");
    assert!(
        env.claude.calls_of("rm").is_empty(),
        "safe never ran claude rm"
    );
    assert!(env.row().cleanup_blocked.is_some());

    let msg = jobs::run(
        &env.src,
        Action::Clean {
            name: NAME.into(),
            mode: Mode::Discard,
        },
    );
    assert_eq!(msg, Msg::Done(format!("Cleaned up {NAME}.")));
    assert!(
        !env.worktree.join("scratch.txt").exists(),
        "reset and cleaned"
    );
    assert!(env.row().cleaned_at.is_some());
}

#[test]
fn wake_reports_an_awake_thread() {
    let env = Env::new();
    jobs::run(
        &env.src,
        Action::SetResolved {
            name: NAME.into(),
            resolved: false,
        },
    );
    env.agents(&[
        agent("c0", "sid-c0", "chief-demo", "working", Some(11)),
        agent(SHORT, SID, NAME, "done", Some(22)),
    ]);
    assert_eq!(
        jobs::run(&env.src, Action::Wake(NAME.into())),
        Msg::Done(format!("{NAME} is already awake."))
    );
    assert!(env.claude.calls_of("respawn").is_empty());
}

#[test]
fn save_writes_settings_and_regenerates() {
    let env = Env::new();
    let path = env.src.dir.join("project.toml");
    let mut config = ProjectConfig::load(&path).unwrap();
    config.threads.model = "haiku".into();
    let msg = jobs::run(&env.src, Action::SaveSettings(Box::new(config.clone())));
    assert_eq!(msg, Msg::Saved(Box::new(config)));
    assert_eq!(ProjectConfig::load(&path).unwrap().threads.model, "haiku");
    for f in [
        "coordinator-settings.json",
        "thread-settings.json",
        "mcp.json",
    ] {
        assert!(env.src.dir.join("generated").join(f).exists(), "{f}");
    }

    let mut bad = ProjectConfig::load(&path).unwrap();
    bad.threads.max_parallel = 0;
    let Msg::SaveFailed(text) = jobs::run(&env.src, Action::SaveSettings(Box::new(bad))) else {
        panic!("invalid settings aren't saved")
    };
    assert!(text.contains("max_parallel"), "{text}");
}
