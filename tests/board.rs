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
use chief::placement::{PoolTrust, Untrusted};
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
        let src = Sources {
            slug: "demo".into(),
            dir,
            db: db_path,
            claude_bin: claude.bin(),
            claude_account: None,
            claude_dir: claude.config_dir(),
            gh_bin: gh.bin(),
            // Missing: agp counts as not installed unless a test installs it.
            agp_bin: root.path().join("no-agp/agp"),
        };
        let env = Env {
            _root: root,
            src,
            conn,
            claude,
            gh,
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
        env.claude.job(SHORT, Some(&env.worktree));
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

/// Showing a thread while the pane shows the coordinator signals the recorded pane process once
/// (SIGUSR1); showing what is already there sends nothing. No multiplexer is involved.
#[test]
fn show_switches_the_main_pane() {
    use std::os::unix::process::ExitStatusExt;
    let env = Env::new();
    // Stands in for `chief pane`: SIGUSR1's default action ends it.
    let mut pane = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = pane.id() as i32;
    let started = chief::proc::start_time(pid).unwrap();
    db::set_pane_process(&env.conn, "demo", pid, &started).unwrap();
    db::set_pane_showing(&env.conn, "demo", Some("demo-notes-1a2b")).unwrap();

    jobs::run(&env.src, Action::Show(Some(NAME.into())));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(pane.try_wait().unwrap(), None, "already showing it");

    let msg = jobs::run(&env.src, Action::Show(None));
    assert_eq!(msg, Msg::Notice("Main pane: coordinator".into()));
    assert_eq!(pane.wait().unwrap().signal(), Some(libc::SIGUSR1));
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
    let Msg::CleanBlocked {
        name,
        loss,
        confirmed,
    } = msg
    else {
        panic!("expected a confirmation, got {msg:?}")
    };
    let confirmed = confirmed.expect("chief listed the loss, so it offers to discard it");
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
            mode: Mode::Discard(confirmed),
        },
    );
    assert_eq!(msg, Msg::Done(format!("Cleaned up {NAME}.")));
    assert!(
        !env.worktree.join("scratch.txt").exists(),
        "reset and cleaned"
    );
    assert!(env.row().cleaned_at.is_some());
}

/// `claude rm` refuses over work chief can't list itself: the modal says why and offers no
/// discard, and never "Nothing unsaved will be lost" beside a refusal.
#[test]
fn a_refusal_chief_cannot_list_offers_no_discard() {
    let env = Env::new();
    env.claude
        .set(
            "rm.stdout",
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/rm/unpushed-one.txt"
            ))
            .unwrap(),
        )
        .set("rm.exit", "1");
    let msg = jobs::run(
        &env.src,
        Action::Clean {
            name: NAME.into(),
            mode: Mode::Safe,
        },
    );
    let Msg::CleanBlocked {
        loss, confirmed, ..
    } = msg
    else {
        panic!("expected the blocked modal, got {msg:?}")
    };
    assert_eq!(confirmed, None);
    assert!(
        loss.starts_with("Cleanup blocked: 1 unpushed commit."),
        "{loss}"
    );
    assert!(!loss.contains("Nothing unsaved will be lost"), "{loss}");
    assert_eq!(env.claude.calls_of("rm").len(), 1);
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

/// The settings screen's trust check (§10): every pool account against the draft's repos, on a
/// worker; without agp there is no pool to check, and an agp that fails says why.
#[test]
fn checks_which_repos_the_pool_trusts() {
    let mut env = Env::new();
    let config = ProjectConfig::load(&env.src.dir.join("project.toml")).unwrap();
    let check = |src: &Sources| jobs::run(src, Action::CheckTrust(Box::new(config.clone())));
    assert_eq!(check(&env.src), Msg::Trust(Ok(PoolTrust::default())));

    let agp = Fake::named("agp");
    let cpb = common::account(env._root.path(), "claude-cpb");
    let cpe = common::account(env._root.path(), "claude-cpe");
    common::trust_file(&cpe.join(".claude.json"), &[&env.git.repo]);
    common::agp_accounts(
        &agp,
        &[
            common::agp_view("cpb", "PB2", true, 0.8),
            common::agp_view("cpe", "PB3", true, 0.7),
            common::agp_view("laura", "PB4", false, 0.0),
        ],
        &[("cpb", Some(&cpb)), ("cpe", Some(&cpe))],
    );
    env.src.agp_bin = agp.bin();
    let Msg::Trust(Ok(found)) = check(&env.src) else {
        panic!("checked")
    };
    assert_eq!(found.checked, ["PB2", "PB3"]);
    assert_eq!(
        found.untrusted,
        [Untrusted {
            id: "cpb".into(),
            label: "PB2".into(),
            dir: Some(cpb.clone()),
            folder: env.git.repo.clone(),
        }]
    );
    assert!(found.errors.is_empty(), "{:?}", found.errors);

    agp.set("status.exit", "1");
    let Msg::Trust(Err(e)) = check(&env.src) else {
        panic!("failed")
    };
    assert!(e.contains("`agp status --json` failed"), "{e}");
}
