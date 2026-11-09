//! Routines (`chief::routines`, ARCHITECTURE §13) against the fake `claude`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use chief::config::{ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::routines::{self, Draft, Outcome, Patch, Trigger};
use chief::state::Role;
use chief::tools::ToolCtx;
use common::{Fake, agent, trust_file};
use rusqlite::Connection;
use serde_json::Value;

const NOW: i64 = 1_800_000_000_000;

fn now() -> i64 {
    NOW
}

struct Env {
    _root: tempfile::TempDir,
    conn: Connection,
    config: ProjectConfig,
    fake: Fake,
    agp: Fake,
    dir: PathBuf,
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
        // Not installed: threads stay in the project's account.
        let agp = Fake::named("agp");
        std::fs::remove_file(agp.bin()).unwrap();
        Env {
            _root: root,
            conn,
            config,
            fake: Fake::new(),
            agp,
            dir,
            trust,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&ToolCtx) -> T) -> T {
        let agp = chief::agp::Agp::new(self.agp.bin());
        let claude = self.fake.claude();
        let ctx = ToolCtx {
            conn: &self.conn,
            config: &self.config,
            claude: &claude,
            agp: &agp,
            project_dir: &self.dir,
            trust_config: &self.trust,
            now,
            resolve_timeout: Duration::from_millis(300),
            wake_timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(10),
        };
        f(&ctx)
    }

    fn create(&self, draft: &Draft) -> anyhow::Result<routines::Routine> {
        routines::create(&self.conn, &self.config, draft, NOW)
    }

    /// Makes `name` due a minute ago.
    fn make_due(&self, name: &str) {
        self.conn
            .execute(
                "UPDATE routines SET next_run_at = ?1 WHERE name = ?2",
                rusqlite::params![NOW - 60_000, name],
            )
            .unwrap();
    }

    fn routine(&self, name: &str) -> routines::Routine {
        routines::get(&self.conn, "demo", name).unwrap().unwrap()
    }

    fn row(&self, name: &str) -> SessionRow {
        db::get_session(&self.conn, name).unwrap().unwrap()
    }

    /// The fake starts a session `short` and lists it under whatever name chief gave it.
    fn will_start(&self, short: &str) {
        self.fake
            .set("agents.stdout", "[]")
            .set("bg.stdout", &format!("backgrounded · {short} · x\n"));
    }
}

fn draft(title: &str, schedule: &str) -> Draft {
    Draft {
        title: title.into(),
        prompt: "Look at new issues and label them.".into(),
        schedule: schedule.into(),
        ..Default::default()
    }
}

fn started(outcome: Outcome) -> String {
    match outcome {
        Outcome::Started { thread, .. } => thread,
        other => panic!("nothing started: {other:?}"),
    }
}

#[test]
fn create_names_validates_and_schedules() {
    let env = Env::new();
    let r = env
        .create(&draft("Triage new issues", "every 30m"))
        .unwrap();
    assert_eq!(r.name, "triage-new-issues");
    assert!(r.enabled && r.notify);
    assert_eq!(r.next_run_at, Some(NOW + 30 * 60_000));
    // The same title again gets a name of its own; an explicit name that is taken is refused.
    let again = env.create(&draft("Triage new issues", "@daily")).unwrap();
    assert_eq!(again.name, "triage-new-issues-2");
    let mut named = draft("Other", "@daily");
    named.name = Some("triage-new-issues".into());
    assert!(
        env.create(&named)
            .unwrap_err()
            .to_string()
            .contains("exists already")
    );
    named.name = Some("Not A Slug".into());
    assert!(env.create(&named).is_err());

    let mut paused = draft("Paused one", "@hourly");
    paused.enabled = Some(false);
    let paused = env.create(&paused).unwrap();
    assert!(!paused.enabled);
    assert_eq!(paused.next_run_at, None);

    for (bad, why) in [
        (draft("", "@daily"), "title is empty"),
        (draft("X", "whenever"), "schedule"),
        (draft("X", "* * * * *"), "schedule"),
        (
            Draft {
                prompt: "  ".into(),
                ..draft("X", "@daily")
            },
            "prompt is empty",
        ),
        (
            Draft {
                effort: Some("extreme".into()),
                ..draft("X", "@daily")
            },
            "effort",
        ),
        (
            Draft {
                repo: Some("elsewhere".into()),
                ..draft("X", "@daily")
            },
            "not a repository of this project",
        ),
    ] {
        let err = format!("{:#}", env.create(&bad).unwrap_err());
        assert!(err.contains(why), "{why}: {err}");
    }
    assert_eq!(routines::list(&env.conn, "demo").unwrap().len(), 3);
}

#[test]
fn a_routine_needs_somewhere_to_run() {
    let mut env = Env::new();
    env.config.threads.default_repo = None;
    let err = format!("{:#}", env.create(&draft("X", "@daily")).unwrap_err());
    assert!(err.contains("no default repository"), "{err}");
    let ok = env.create(&Draft {
        repo: Some("repo".into()),
        ..draft("X", "@daily")
    });
    assert!(ok.is_ok());
}

#[test]
fn update_changes_only_what_is_given() {
    let env = Env::new();
    env.create(&Draft {
        model: Some("haiku".into()),
        ..draft("Nightly", "every 30m")
    })
    .unwrap();
    let update = |patch: Patch, at: i64| {
        routines::update(&env.conn, &env.config, "nightly", &patch, at).unwrap()
    };

    // A new prompt leaves the next run where it was.
    let r = update(
        Patch {
            prompt: Some("Do it differently.".into()),
            ..Default::default()
        },
        NOW + 60_000,
    );
    assert_eq!(r.prompt, "Do it differently.");
    assert_eq!(r.next_run_at, Some(NOW + 30 * 60_000));
    assert_eq!(r.model.as_deref(), Some("haiku"));

    // A new schedule counts from now; an empty string drops an override.
    let r = update(
        Patch {
            schedule: Some("every 2h".into()),
            model: Some(String::new()),
            ..Default::default()
        },
        NOW + 60_000,
    );
    assert_eq!(r.next_run_at, Some(NOW + 60_000 + 2 * 3_600_000));
    assert_eq!(r.model, None);

    let r = update(
        Patch {
            enabled: Some(false),
            ..Default::default()
        },
        NOW,
    );
    assert_eq!((r.enabled, r.next_run_at), (false, None));
    // Resuming counts from the moment it resumes: nothing missed is made up.
    let r = update(
        Patch {
            enabled: Some(true),
            ..Default::default()
        },
        NOW + 10 * 3_600_000,
    );
    assert_eq!(r.next_run_at, Some(NOW + 12 * 3_600_000));

    let bad = routines::update(
        &env.conn,
        &env.config,
        "nightly",
        &Patch {
            schedule: Some("nope".into()),
            ..Default::default()
        },
        NOW,
    );
    assert!(bad.is_err());
    assert_eq!(env.routine("nightly").schedule, "every 2h");
    let missing = routines::update(&env.conn, &env.config, "nope", &Patch::default(), NOW);
    assert!(format!("{:#}", missing.unwrap_err()).contains("this project has nightly"));
}

#[test]
fn a_due_routine_starts_a_thread_of_its_own() {
    let env = Env::new();
    env.create(&Draft {
        model: Some("haiku".into()),
        effort: Some("low".into()),
        ..draft("Triage new issues", "every 30m")
    })
    .unwrap();

    // Not due yet: nothing starts.
    let early = env.with(|ctx| routines::run(ctx, "triage-new-issues", Trigger::Schedule));
    assert_eq!(early.unwrap(), Outcome::Skipped("not due".into()));
    assert!(env.fake.calls_of("--bg").is_empty());

    env.make_due("triage-new-issues");
    env.will_start("r1");
    let ran = env.with(routines::run_due).unwrap();
    assert_eq!(ran.len(), 1);
    let thread = started(ran.into_iter().next().unwrap().1.unwrap());

    let row = env.row(&thread);
    assert_eq!(row.routine.as_deref(), Some("triage-new-issues"));
    assert_eq!(row.title.as_deref(), Some("Triage new issues"));
    assert_eq!(row.short_id.as_deref(), Some("r1"));
    assert_eq!(row.model.as_deref(), Some("haiku"));
    let task = row.task.unwrap();
    assert!(task.starts_with(
        "This is a scheduled run of the routine \"Triage new issues\" (Every 30 minutes)."
    ));
    assert!(task.ends_with("Look at new issues and label them."));

    let spawn = &env.fake.calls_of("--bg")[0];
    assert!(spawn.contains(&"haiku".to_string()) && spawn.contains(&"low".to_string()));

    // The occurrence is claimed: the next run counts from now, and it isn't due again.
    let r = env.routine("triage-new-issues");
    assert_eq!(r.next_run_at, Some(NOW + 30 * 60_000));
    assert_eq!(r.last_run_at, Some(NOW));
    assert_eq!(r.last_thread.as_deref(), Some(thread.as_str()));
    assert_eq!(r.last_note, None);
    assert!(env.with(routines::run_due).unwrap().is_empty());
}

#[test]
fn a_run_is_skipped_while_the_last_one_still_works() {
    let env = Env::new();
    env.create(&draft("Nightly", "every 30m")).unwrap();
    env.will_start("r1");
    let first = started(
        env.with(|ctx| routines::run(ctx, "nightly", Trigger::Manual))
            .unwrap(),
    );
    // A manual run leaves the schedule alone.
    assert_eq!(env.routine("nightly").next_run_at, Some(NOW + 30 * 60_000));
    // A fresh row is provisionally working.
    assert_eq!(env.row(&first).group_name, "working");

    env.make_due("nightly");
    let second = env
        .with(|ctx| routines::run(ctx, "nightly", Trigger::Schedule))
        .unwrap();
    let Outcome::Skipped(why) = second else {
        panic!("started over a working run");
    };
    assert!(
        why.contains(&first) && why.contains("still working"),
        "{why}"
    );
    assert_eq!(env.fake.calls_of("--bg").len(), 1);
    let r = env.routine("nightly");
    assert_eq!(r.last_note.as_deref(), Some(why.as_str()));
    // The skipped occurrence is spent all the same.
    assert_eq!(r.next_run_at, Some(NOW + 30 * 60_000));
    // So is a manual run.
    let manual = env
        .with(|ctx| routines::run(ctx, "nightly", Trigger::Manual))
        .unwrap();
    assert!(matches!(manual, Outcome::Skipped(_)));
}

#[test]
fn the_next_run_puts_finished_runs_away() {
    let env = Env::new();
    env.create(&draft("Nightly", "@daily")).unwrap();
    let old = |name: &str, f: &dyn Fn(&mut SessionRow)| {
        let mut row = SessionRow::new(name, "demo", Role::Thread, NOW - 3_600_000);
        row.routine = Some("nightly".into());
        row.short_id = Some(format!("s-{name}"));
        row.group_name = "idle".into();
        row.last_hook = Some("Stop".into());
        f(&mut row);
        db::insert_session(&env.conn, &row).unwrap();
    };
    old("demo-nightly-idle", &|_| {});
    old("demo-nightly-pr", &|row| {
        row.group_name = "review".into();
        row.pr_state = Some("OPEN".into());
        row.pr_url = Some("https://example.test/pr/1".into());
    });
    // Another routine's run, and a plain thread, are not this routine's to touch.
    old("demo-other-idle", &|row| row.routine = Some("other".into()));
    old("demo-plain-idle", &|row| row.routine = None);

    env.will_start("r2");
    started(
        env.with(|ctx| routines::run(ctx, "nightly", Trigger::Manual))
            .unwrap(),
    );

    let idle = env.row("demo-nightly-idle");
    assert!(idle.resolved);
    assert_eq!(idle.resolved_reason.as_deref(), Some("routine"));
    for kept in ["demo-nightly-pr", "demo-other-idle", "demo-plain-idle"] {
        assert!(!env.row(kept).resolved, "{kept}");
    }
}

#[test]
fn a_start_that_fails_is_noted_and_not_retried() {
    let env = Env::new();
    env.create(&draft("Nightly", "every 30m")).unwrap();
    env.make_due("nightly");
    env.fake
        .set("agents.stdout", "[]")
        .set("bg.exit", "1")
        .set("bg.stderr", "boom\n");
    let ran = env.with(routines::run_due).unwrap();
    let err = format!("{:#}", ran[0].1.as_ref().unwrap_err());
    assert!(err.contains("starting a run of routine nightly"), "{err}");
    let r = env.routine("nightly");
    assert!(r.last_note.as_deref().unwrap().starts_with("failed at "));
    assert_eq!(r.last_thread, None);
    // The occurrence is spent: a broken routine doesn't start a session every tick.
    assert_eq!(r.next_run_at, Some(NOW + 30 * 60_000));
    assert!(env.with(routines::run_due).unwrap().is_empty());
}

#[test]
fn paused_routines_run_only_on_request() {
    let env = Env::new();
    env.create(&Draft {
        enabled: Some(false),
        ..draft("Nightly", "every 30m")
    })
    .unwrap();
    env.make_due("nightly");
    assert!(env.with(routines::run_due).unwrap().is_empty());
    env.will_start("r1");
    started(
        env.with(|ctx| routines::run(ctx, "nightly", Trigger::Manual))
            .unwrap(),
    );
}

#[test]
fn the_tools_speak_json_and_say_when_nothing_fires() {
    let env = Env::new();
    let made: Value = env
        .with(|ctx| routines::create_tool(ctx, &draft("Weekday triage", "0 9 * * 1-5")))
        .unwrap();
    assert_eq!(made["name"], "weekday-triage");
    assert_eq!(made["schedule_text"], "Weekdays at 09:00");
    assert!(made["next_run"].as_str().unwrap().ends_with("09:00"));
    // No `chief serve` runs for this test's home.
    assert!(
        made["scheduler"]
            .as_str()
            .unwrap()
            .contains("chief serve isn't running")
    );

    let listed = env.with(routines::list_tool).unwrap();
    assert_eq!(listed["routines"].as_array().unwrap().len(), 1);

    let updated = env
        .with(|ctx| {
            routines::update_tool(
                ctx,
                &serde_json::from_value(serde_json::json!({
                    "name": "weekday-triage",
                    "enabled": false,
                    "notify": false,
                }))
                .unwrap(),
            )
        })
        .unwrap();
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["next_run"], Value::Null);
    assert_eq!(
        routines::quiet(&env.conn, "demo").unwrap(),
        ["weekday-triage"]
    );

    env.will_start("r1");
    let ran = env
        .with(|ctx| {
            routines::run_tool(
                ctx,
                &routines::RoutineNameArgs {
                    name: " weekday-triage ".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(ran["started"], true);
    assert!(ran["next"].as_str().unwrap().contains("read_report"));

    let gone = env
        .with(|ctx| {
            routines::delete_tool(
                ctx,
                &routines::RoutineNameArgs {
                    name: "weekday-triage".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(gone["deleted"], true);
    // Its run stays, still tagged.
    let thread = ran["thread"].as_str().unwrap();
    assert_eq!(env.row(thread).routine.as_deref(), Some("weekday-triage"));
    let _ = agent;
}
