//! Routines (`chief::routines`, ARCHITECTURE §13) against the fake `claude`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use chief::config::{ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::routines::{self, Author, Draft, Outcome, Patch, Trigger};
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
        // No desktop notification from a test, whatever a run says.
        let mut config = config;
        config.ui.notify = false;
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
        routines::create(&self.conn, &self.config, draft, Author::User, NOW)
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
        routines::update(&env.conn, &env.config, "nightly", &patch, Author::User, at).unwrap()
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
        Author::User,
        NOW,
    );
    assert!(bad.is_err());
    assert_eq!(env.routine("nightly").schedule, "every 2h");
    let missing = routines::update(
        &env.conn,
        &env.config,
        "nope",
        &Patch::default(),
        Author::User,
        NOW,
    );
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
    // The brief names the run's memory and how it ends.
    let folder = env
        .dir
        .join("routines/triage-new-issues")
        .canonicalize()
        .unwrap();
    let notes = folder.join("NOTES.md");
    assert!(task.contains(&format!("what they left for you is {}", notes.display())));
    assert!(task.contains("record_value") && task.contains("report_outcome"));
    assert!(
        std::fs::read_to_string(&notes)
            .unwrap()
            .starts_with("# Triage new issues\n")
    );

    // It starts as a thread does, plus its own folder, its tools and the settings that allow
    // them.
    let spawn = &env.fake.calls_of("--bg")[0];
    assert!(spawn.contains(&"haiku".to_string()) && spawn.contains(&"low".to_string()));
    let after = |flag: &str| -> Vec<String> {
        spawn
            .windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    };
    assert_eq!(
        after("--settings"),
        [env.dir
            .join("generated/run-settings.json")
            .display()
            .to_string()]
    );
    let mcp = env.dir.join("generated/run-mcp-triage-new-issues.json");
    assert_eq!(after("--mcp-config"), [mcp.display().to_string()]);
    let config: Value = serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["chief"]["args"],
        serde_json::json!(["mcp", "--project", "demo", "--routine", "triage-new-issues"])
    );
    assert!(after("--add-dir").contains(&folder.display().to_string()));

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
    assert_eq!(made["last_finding"], Value::Null);
    assert_eq!(made["series"], serde_json::json!([]));
    assert!(
        made["notes"]
            .as_str()
            .unwrap()
            .ends_with("routines/weekday-triage/NOTES.md")
    );
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

fn finish(env: &Env, thread: &str, at: i64) {
    db::immediate(&env.conn, |conn| {
        let mut row = db::get_session(conn, thread)?.unwrap();
        row.group_name = "idle".into();
        row.last_hook = Some("Stop".into());
        row.last_report = Some("Done.".into());
        row.last_report_at = Some(at);
        db::update_session(conn, &row)
    })
    .unwrap();
}

/// What a run leaves: values that join into a line across runs, a verdict that decides who
/// hears, and notes the coordinator can read without opening a thread.
#[test]
fn a_run_records_values_and_says_what_it_came_to() {
    let mut env = Env::new();
    let pushed = env.dir.join("pushed");
    env.config.push.command = Some(format!(
        "printf '%s|%s|%s' \"$CHIEF_KIND\" \"$CHIEF_TITLE\" \"$CHIEF_BODY\" >> '{}'",
        pushed.display()
    ));
    env.create(&draft("Flights", "@daily")).unwrap();
    env.will_start("r1");
    let thread = started(
        env.with(|ctx| routines::run(ctx, "flights", Trigger::Manual))
            .unwrap(),
    );

    let record = |series: &str, value: f64, unit: Option<&str>| {
        env.with(|ctx| {
            routines::record_value_tool(
                ctx,
                "flights",
                &routines::RecordValueArgs {
                    series: series.into(),
                    value,
                    unit: unit.map(str::to_string),
                    note: None,
                },
            )
        })
    };
    let first = record("BCN-NYC lowest fare", 498.0, Some("EUR")).unwrap();
    assert_eq!(first["previous"], Value::Null);
    // The same thing under another case and spacing is the same line.
    let second = record(" bcn-nyc  LOWEST fare ", 412.0, Some("EUR")).unwrap();
    assert_eq!(second["previous"], 498.0);
    record("BCN-TYO lowest fare", 820.0, Some("EUR")).unwrap();
    let long = "long ".repeat(20);
    for (series, value) in [("", 1.0), ("x", f64::NAN), (long.as_str(), 1.0)] {
        assert!(record(series, value, None).is_err(), "{series:?} {value}");
    }
    let series = routines::series(&env.conn, "demo", "flights", 10).unwrap();
    // Most recently recorded first.
    let names: Vec<&str> = series.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["BCN-TYO lowest fare", "BCN-NYC lowest fare"]);
    let values: Vec<f64> = series[1].points.iter().map(|p| p.1).collect();
    assert_eq!(values, [498.0, 412.0]);
    assert_eq!(series[1].unit.as_deref(), Some("EUR"));

    let say = |outcome: routines::Verdict, headline: &str| {
        env.with(|ctx| {
            routines::report_outcome_tool(
                ctx,
                "flights",
                &routines::ReportOutcomeArgs {
                    outcome,
                    headline: headline.into(),
                    details: Some("Vueling, 14 Mar.".into()),
                },
            )
        })
    };
    assert!(!routines::reported(&env.conn, &thread).unwrap());
    assert!(say(routines::Verdict::Update, "  ").is_err());
    // News that can wait isn't pushed unless the settings ask for updates; nothing never is.
    let nothing = say(routines::Verdict::Nothing, "No change").unwrap();
    assert_eq!(nothing["pushed"], false);
    let update = say(routines::Verdict::Update, "BCN-TYO is at 820 EUR").unwrap();
    assert_eq!(update["pushed"], false);
    assert!(routines::reported(&env.conn, &thread).unwrap());
    let said = say(
        routines::Verdict::Attention,
        "BCN-NYC dropped to 412 EUR\n(was 498)",
    )
    .unwrap();
    assert_eq!(said["pushed"], true);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let want = "attention|Demo · Flights|BCN-NYC dropped to 412 EUR (was 498)";
    while std::fs::read_to_string(&pushed).ok().as_deref() != Some(want) {
        assert!(std::time::Instant::now() < deadline, "the push never ran");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A run's later word replaces its earlier one, and belongs to its thread.
    let verdicts = routines::verdicts(&env.conn, "demo").unwrap();
    assert_eq!(verdicts[&thread].verdict, routines::Verdict::Attention);
    assert_eq!(
        verdicts[&thread].details.as_deref(),
        Some("Vueling, 14 Mar.")
    );

    // The coordinator reads it all without a thread.
    std::fs::write(
        env.dir.join("routines/flights/NOTES.md"),
        "# Flights\nWatching BCN-NYC under 450.\n",
    )
    .unwrap();
    let read = env
        .with(|ctx| {
            routines::read_tool(
                ctx,
                &routines::RoutineNameArgs {
                    name: "flights".into(),
                },
            )
        })
        .unwrap();
    assert_eq!(read["notes"], "# Flights\nWatching BCN-NYC under 450.\n");
    assert_eq!(read["last_finding"]["outcome"], "attention");
    assert_eq!(read["findings"].as_array().unwrap().len(), 3);
    assert_eq!(read["series"][1]["latest"], 412.0);
    assert_eq!(read["series"][1]["previous"], 498.0);
    assert_eq!(read["series"][1]["points"].as_array().unwrap().len(), 2);
}

/// Only a run that needs the user is worth a coordinator's wake; the rest wait for its next
/// turn, and one that found nothing is told to nobody.
#[test]
fn only_a_run_that_needs_the_user_wakes_the_coordinator() {
    let env = Env::new();
    env.create(&draft("Flights", "@daily")).unwrap();
    env.will_start("r1");
    let thread = started(
        env.with(|ctx| routines::run(ctx, "flights", Trigger::Manual))
            .unwrap(),
    );
    let old = NOW - routines::WAKE_AFTER_MS;
    finish(&env, &thread, old);
    let unheard = || routines::unheard(&env.conn, "demo", NOW).unwrap();
    let due = || {
        let threads = db::list_sessions(&env.conn, "demo", Some(Role::Thread)).unwrap();
        let verdicts = routines::verdicts(&env.conn, "demo").unwrap();
        chief::waiting::routine_due(&threads, &[], &verdicts).len()
    };
    let say = |outcome| {
        env.with(|ctx| {
            routines::report_outcome_tool(
                ctx,
                "flights",
                &routines::ReportOutcomeArgs {
                    outcome,
                    headline: "h".into(),
                    details: None,
                },
            )
        })
        .unwrap()
    };
    // No verdict at all (it crashed, or forgot): told on the next turn, no wake.
    assert_eq!((due(), unheard().len()), (1, 0));
    say(routines::Verdict::Nothing);
    assert_eq!((due(), unheard().len()), (0, 0));
    say(routines::Verdict::Update);
    assert_eq!((due(), unheard().len()), (1, 0));
    say(routines::Verdict::Attention);
    assert_eq!((due(), unheard()), (1, vec![(thread.clone(), old)]));
    // Too fresh: the Stop hook may still be keeping its turn going.
    finish(&env, &thread, NOW - 1_000);
    assert!(unheard().is_empty());
}

/// A project that isn't about code has no repositories: its threads and runs work in its own
/// workspace folder.
#[test]
fn a_project_without_repositories_works_in_its_workspace() {
    let mut env = Env::new();
    env.config.repos.clear();
    env.config.threads.default_repo = None;
    // The project folder is trusted (chief made it), which covers the workspace under it.
    trust_file(&env.trust, &[&env.dir]);
    env.create(&draft("Mortgage", "@weekly")).unwrap();
    env.will_start("r1");
    let thread = started(
        env.with(|ctx| routines::run(ctx, "mortgage", Trigger::Manual))
            .unwrap(),
    );
    let row = env.row(&thread);
    let workspace = env.dir.join("workspace").canonicalize().unwrap();
    assert_eq!(row.repo_name.as_deref(), Some("workspace"));
    assert_eq!(row.repo_path.as_deref(), Some(workspace.to_str().unwrap()));
    // A named repository that the project doesn't have is still refused.
    let named = env.create(&Draft {
        repo: Some("nope".into()),
        ..draft("Other", "@daily")
    });
    assert!(named.is_err());
}

/// A run that is done but hasn't said what it came to is kept going once, by the Stop hook.
#[test]
fn a_run_that_ends_without_a_verdict_is_asked_for_one() {
    let env = Env::new();
    env.create(&draft("Flights", "@daily")).unwrap();
    env.will_start("r1");
    let thread = started(
        env.with(|ctx| routines::run(ctx, "flights", Trigger::Manual))
            .unwrap(),
    );
    let stop = serde_json::json!({"hook_event_name": "Stop"});
    let ask = |payload: &Value| chief::hook::outcome_block(&env.conn, "demo", &thread, payload);

    let block: Value = serde_json::from_str(&ask(&stop).unwrap().unwrap()).unwrap();
    assert_eq!(block["decision"], "block");
    let reason = block["reason"].as_str().unwrap();
    assert!(reason.contains("report_outcome") && reason.contains("routines/flights/NOTES.md"));
    // Once per chain of Stops: the Stop that ends the turn the block started isn't blocked.
    let again = serde_json::json!({"hook_event_name": "Stop", "stop_hook_active": true});
    assert_eq!(ask(&again).unwrap(), None);

    // Not while its subagents still work: that Stop isn't the end of the run.
    db::immediate(&env.conn, |conn| {
        let mut row = db::get_session(conn, &thread)?.unwrap();
        row.subagents_busy = 1;
        db::update_session(conn, &row)
    })
    .unwrap();
    assert_eq!(ask(&stop).unwrap(), None);
    db::immediate(&env.conn, |conn| {
        let mut row = db::get_session(conn, &thread)?.unwrap();
        row.subagents_busy = 0;
        db::update_session(conn, &row)
    })
    .unwrap();

    // A plain thread is never asked, and a run that said its piece isn't asked again.
    let mut plain = SessionRow::new("demo-plain-1a2b", "demo", Role::Thread, NOW);
    plain.short_id = Some("p1".into());
    db::insert_session(&env.conn, &plain).unwrap();
    let other = chief::hook::outcome_block(&env.conn, "demo", "demo-plain-1a2b", &stop);
    assert_eq!(other.unwrap(), None);
    env.with(|ctx| {
        routines::report_outcome_tool(
            ctx,
            "flights",
            &routines::ReportOutcomeArgs {
                outcome: routines::Verdict::Nothing,
                headline: "No change".into(),
                details: None,
            },
        )
    })
    .unwrap();
    assert_eq!(ask(&stop).unwrap(), None);
}

// ---- Commands (§13 "A command") ------------------------------------------------------------------

fn command_draft(title: &str, command: &str) -> Draft {
    Draft {
        title: title.into(),
        command: Some(command.into()),
        schedule: "* * * * *".into(),
        ..Default::default()
    }
}

/// Runs the routine now and waits for its command.
fn ran(env: &Env, name: &str) -> chief::command::Finished {
    match env
        .with(|ctx| routines::run_here(ctx, name, Trigger::Manual))
        .unwrap()
    {
        Outcome::Ran(done) => *done,
        other => panic!("no command ran: {other:?}"),
    }
}

fn state_key(env: &Env, name: &str) -> &'static str {
    routines::state(&env.conn, &env.routine(name), NOW)
        .unwrap()
        .key
}

#[test]
fn a_command_runs_with_no_session() {
    let env = Env::new();
    let r = env
        .create(&command_draft(
            "Disk space",
            "echo \"$CHIEF_PROJECT/$CHIEF_ROUTINE run $CHIEF_RUN\"; pwd; \
             echo kept > \"$CHIEF_ROUTINE_DIR/state\"; echo oops >&2",
        ))
        .unwrap();
    // A command costs a process, so it may run every minute; it is the user's own, so it is
    // approved as given.
    assert_eq!(r.kind(), routines::Kind::Command);
    assert!(!r.needs_approval());
    assert_eq!(r.next_run_at, Some(NOW + 60_000 - NOW % 60_000));
    assert_eq!(state_key(&env, "disk-space"), "new");

    let done = ran(&env, "disk-space");
    assert_eq!(done.run.status, chief::command::Status::Ok);
    assert_eq!(done.run.exit_code, Some(0));
    let repo = env.config.repos[0].path.display().to_string();
    assert_eq!(
        done.run.output.as_deref(),
        Some(format!("demo/disk-space run {}\n{repo}\noops\n", done.run.id).as_str())
    );
    // Nothing new: nobody is told, and no session started.
    assert_eq!((done.news, done.thread), (None, None));
    assert!(env.fake.calls_of("--bg").is_empty());
    assert!(
        routines::findings(&env.conn, "demo", "disk-space", 5)
            .unwrap()
            .is_empty()
    );
    // What it kept and what it printed are in its folder.
    let folder = env.dir.join("routines/disk-space");
    assert_eq!(
        std::fs::read_to_string(folder.join("state")).unwrap(),
        "kept\n"
    );
    assert!(
        std::fs::read_to_string(folder.join("last-run.log"))
            .unwrap()
            .ends_with("oops\n")
    );

    let routine = env.routine("disk-space");
    assert_eq!(routine.last_run_at, Some(NOW));
    assert_eq!((&routine.last_thread, &routine.last_note), (&None, &None));
    assert_eq!(state_key(&env, "disk-space"), "ok");
    let view = routines::view(&env.conn, &routine).unwrap();
    assert_eq!(view["kind"], "command");
    assert_eq!(view["state"]["label"], "OK");
    assert_eq!(view["last_exec"]["status"], "ok");
    assert_eq!(view["last_exec"]["fired_by"], "manual");
    assert_eq!(view["needs_approval"], false);
    let read = env
        .with(|ctx| {
            routines::read_tool(
                ctx,
                &routines::RoutineNameArgs {
                    name: "disk-space".into(),
                },
            )
        })
        .unwrap();
    assert!(
        read["runs"][0]["output"]
            .as_str()
            .unwrap()
            .ends_with("oops\n")
    );
    assert!(read["log_file"].as_str().unwrap().ends_with("last-run.log"));
}

#[test]
fn a_routine_is_a_prompt_a_command_or_both() {
    let env = Env::new();
    for (bad, why) in [
        (
            Draft {
                title: "X".into(),
                schedule: "@daily".into(),
                ..Default::default()
            },
            "give a prompt",
        ),
        (
            Draft {
                timeout_secs: Some(0),
                ..command_draft("X", "true")
            },
            "timeout_secs must be between 1 and 86400",
        ),
        (
            Draft {
                timeout_secs: Some(100_000),
                ..command_draft("X", "true")
            },
            "timeout_secs",
        ),
        // A session a minute is a typo; a command a minute isn't.
        (draft("X", "* * * * *"), "less than 5 minutes apart"),
        (
            Draft {
                schedule: "every 0m".into(),
                ..command_draft("X", "true")
            },
            "at least a minute",
        ),
    ] {
        let err = format!("{:#}", env.create(&bad).unwrap_err());
        assert!(err.contains(why), "{why}: {err}");
    }
    let check = env
        .create(&Draft {
            prompt: "Find out why it fails.".into(),
            timeout_secs: Some(20),
            ..command_draft("Health", " curl -f https://example.test/health ")
        })
        .unwrap();
    assert_eq!(check.kind(), routines::Kind::Check);
    assert_eq!(
        check.command.as_deref(),
        Some("curl -f https://example.test/health")
    );
    assert_eq!(check.timeout(), Duration::from_secs(20));

    let update = |patch: Patch| {
        routines::update(&env.conn, &env.config, "health", &patch, Author::User, NOW)
    };
    // Without its command it is a session, which may not run every minute.
    let err = update(Patch {
        command: Some(String::new()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("less than 5 minutes apart"));
    let session = update(Patch {
        command: Some(String::new()),
        schedule: Some("@hourly".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(session.kind(), routines::Kind::Session);
    assert_eq!((session.command, session.timeout_secs), (None, None));
    // And without its prompt, only a command; with neither, nothing.
    let command = update(Patch {
        command: Some("true".into()),
        prompt: Some(String::new()),
        timeout_secs: Some(0),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(command.kind(), routines::Kind::Command);
    assert_eq!(command.timeout(), Duration::from_secs(300));
    assert!(
        update(Patch {
            command: Some(String::new()),
            ..Default::default()
        })
        .is_err()
    );
}

/// A command repeats itself, so only a change reaches anyone: the first failure, then the
/// first pass after it.
#[test]
fn a_failing_command_is_told_once_and_so_is_its_recovery() {
    let mut env = Env::new();
    let pushed = env.dir.join("pushed");
    env.config.push.command = Some(format!(
        "printf '%s|%s|%s\\n' \"$CHIEF_KIND\" \"$CHIEF_TITLE\" \"$CHIEF_BODY\" >> '{}'",
        pushed.display()
    ));
    env.config.push.on = chief::config::PushOn::Updates;
    env.create(&command_draft(
        "Staging health",
        "test -f \"$CHIEF_ROUTINE_DIR/up\" || { echo checking; echo 'curl: (22) 503' >&2; exit 22; }",
    ))
    .unwrap();
    let findings = || routines::findings(&env.conn, "demo", "staging-health", 10).unwrap();
    let wait_pushed = |want: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&pushed).ok().as_deref() != Some(want) {
            assert!(std::time::Instant::now() < deadline, "the push never ran");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let first = ran(&env, "staging-health");
    assert_eq!(first.run.status, chief::command::Status::Failed);
    assert_eq!(first.run.exit_code, Some(22));
    let news = first.news.unwrap();
    assert_eq!(news.verdict, routines::Verdict::Attention);
    assert_eq!(news.headline, "Failed: exit 22: curl: (22) 503");
    let found = findings();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run, Some(first.run.id));
    assert_eq!(found[0].thread, None);
    assert_eq!(
        found[0].details.as_deref(),
        Some("```\nchecking\ncurl: (22) 503\n```")
    );
    wait_pushed("attention|Demo · Staging health|Failed: exit 22: curl: (22) 503\n");
    // How it stands is the run's own line: the finding only repeats it.
    let state = routines::state(&env.conn, &env.routine("staging-health"), NOW).unwrap();
    assert_eq!(
        (state.key, state.line.as_str(), state.at),
        ("failing", "exit 22: curl: (22) 503", Some(NOW))
    );

    // It fails again: its run is kept, and nobody hears it twice.
    let second = ran(&env, "staging-health");
    assert_eq!(second.run.status, chief::command::Status::Failed);
    assert_eq!(second.news, None);
    assert_eq!(findings().len(), 1);
    let state = routines::state(&env.conn, &env.routine("staging-health"), NOW).unwrap();
    assert_eq!((state.key, state.label), ("failing", "Failing"));
    assert!(state.needs_user());

    // The coordinator is told of what a command found, though no thread carries it; a
    // sleeping one is woken for it once it has stood a moment.
    let untold = routines::untold(&env.conn, "demo").unwrap();
    assert_eq!(untold.len(), 1);
    assert_eq!(
        chief::waiting::finding_line(&untold[0], Some("Staging health")),
        format!(
            "- routine staging-health \"Staging health\" · command · at {} · attention: \
             Failed: exit 22: curl: (22) 503",
            chief::text::local_hhmm(NOW)
        )
    );
    assert!(
        routines::unheard(&env.conn, "demo", NOW)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        routines::unheard(&env.conn, "demo", NOW + routines::WAKE_AFTER_MS).unwrap(),
        [(format!("finding {}", untold[0].id), NOW)]
    );
    routines::mark_told(&env.conn, &untold, NOW).unwrap();
    assert!(routines::untold(&env.conn, "demo").unwrap().is_empty());
    assert!(
        routines::unheard(&env.conn, "demo", NOW + routines::WAKE_AFTER_MS)
            .unwrap()
            .is_empty()
    );

    // It passes again: said once, as news that can wait.
    std::fs::write(env.dir.join("routines/staging-health/up"), "").unwrap();
    let third = ran(&env, "staging-health");
    assert_eq!(third.run.status, chief::command::Status::Ok);
    let news = third.news.unwrap();
    assert_eq!(news.verdict, routines::Verdict::Update);
    assert_eq!(news.headline, "Passes again after 2 failed runs");
    wait_pushed(
        "attention|Demo · Staging health|Failed: exit 22: curl: (22) 503\n\
         update|Demo · Staging health|Passes again after 2 failed runs\n",
    );
    assert_eq!(state_key(&env, "staging-health"), "ok");
    assert_eq!(env.routine("staging-health").last_note, None);
    assert_eq!(ran(&env, "staging-health").news, None);
    assert_eq!(findings().len(), 2);
    // No session was ever part of it.
    assert!(env.fake.calls_of("--bg").is_empty());

    // A routine that tells nobody keeps its findings to its own page.
    routines::update(
        &env.conn,
        &env.config,
        "staging-health",
        &Patch {
            notify: Some(false),
            ..Default::default()
        },
        Author::User,
        NOW,
    )
    .unwrap();
    routines::mark_told(
        &env.conn,
        &routines::untold(&env.conn, "demo").unwrap(),
        NOW,
    )
    .unwrap();
    std::fs::remove_file(env.dir.join("routines/staging-health/up")).unwrap();
    assert!(ran(&env, "staging-health").news.is_some());
    assert_eq!(findings().len(), 3);
    assert!(routines::untold(&env.conn, "demo").unwrap().is_empty());
}

/// The coordinator can't run commands itself, so one it writes waits for the user's yes, and
/// waits again whenever it changes the words.
#[test]
fn a_command_the_coordinator_wrote_waits_for_approval() {
    let env = Env::new();
    let made = env
        .with(|ctx| routines::create_tool(ctx, &command_draft("Backup", "echo backing up")))
        .unwrap();
    assert_eq!(made["needs_approval"], true);
    assert_eq!(made["state"]["key"], "approval");
    assert!(
        made["approval"]
            .as_str()
            .unwrap()
            .contains("chief routine approve demo backup")
    );
    // Its time comes and goes: the occurrence is spent, nothing ran.
    env.make_due("backup");
    let skipped = env
        .with(|ctx| routines::run(ctx, "backup", Trigger::Schedule))
        .unwrap();
    let Outcome::Skipped(why) = skipped else {
        panic!("it ran: {skipped:?}");
    };
    assert!(why.contains("waits for the user's approval"), "{why}");
    assert!(env.routine("backup").next_run_at.unwrap() > NOW);
    assert!(
        chief::command::latest(&env.conn, "demo", "backup", 5)
            .unwrap()
            .is_empty()
    );
    // Asking for it by hand changes nothing: asking isn't approving.
    assert!(matches!(
        env.with(|ctx| routines::run_here(ctx, "backup", Trigger::Manual))
            .unwrap(),
        Outcome::Skipped(_)
    ));

    // The words shown must be the words approved.
    let stale = routines::approve(&env.conn, "demo", "backup", Some("echo other"), NOW);
    assert!(
        stale
            .unwrap_err()
            .to_string()
            .contains("changed since it was shown")
    );
    let approved =
        routines::approve(&env.conn, "demo", "backup", Some("echo backing up"), NOW).unwrap();
    assert!(!approved.needs_approval());
    assert_eq!(approved.last_note, None);
    assert_eq!(
        ran(&env, "backup").run.output.as_deref(),
        Some("backing up\n")
    );

    // What the coordinator changes besides the command leaves the approval; new words wait.
    let update = |changes: Patch| {
        env.with(|ctx| {
            routines::update_tool(
                ctx,
                &routines::UpdateRoutineArgs {
                    name: "backup".into(),
                    changes,
                },
            )
        })
        .unwrap()
    };
    let retitled = update(Patch {
        title: Some("Nightly backup".into()),
        timeout_secs: Some(60),
        ..Default::default()
    });
    assert_eq!(retitled["needs_approval"], false);
    let changed = update(Patch {
        command: Some("echo backing up; rm -rf /tmp/x".into()),
        ..Default::default()
    });
    assert_eq!(changed["needs_approval"], true);
    assert!(changed["approval"].is_string());
    // Back to the words the user approved: nothing to ask.
    let back = update(Patch {
        command: Some("echo backing up".into()),
        ..Default::default()
    });
    assert_eq!(back["needs_approval"], false);

    // The user's own edit is their approval.
    update(Patch {
        command: Some("echo other".into()),
        ..Default::default()
    });
    let mine = routines::update(
        &env.conn,
        &env.config,
        "backup",
        &Patch {
            command: Some("echo other".into()),
            ..Default::default()
        },
        Author::User,
        NOW,
    )
    .unwrap();
    assert!(!mine.needs_approval());
    // A routine with no command has nothing to approve.
    env.create(&draft("Plain", "@daily")).unwrap();
    assert!(routines::approve(&env.conn, "demo", "plain", None, NOW).is_err());
}

/// A command with a prompt is a check: the session starts only when the check has news, and
/// is handed what the check printed.
#[test]
fn a_check_starts_a_session_only_for_news() {
    let env = Env::new();
    env.create(&Draft {
        prompt: "Find out why staging is down and say what to do.".into(),
        model: Some("haiku".into()),
        ..command_draft(
            "Staging",
            "test -f \"$CHIEF_ROUTINE_DIR/up\" || { echo 'curl: (22) 503'; exit 22; }",
        )
    })
    .unwrap();
    let up = env.dir.join("routines/staging/up");

    // It passes: no session, nobody told.
    std::fs::create_dir_all(up.parent().unwrap()).unwrap();
    std::fs::write(&up, "").unwrap();
    let done = ran(&env, "staging");
    assert_eq!((done.news, done.thread), (None, None));
    assert!(env.fake.calls_of("--bg").is_empty());

    // It fails: a session looks into it, and says what it finds itself.
    std::fs::remove_file(&up).unwrap();
    env.will_start("r1");
    let done = ran(&env, "staging");
    let thread = done.thread.clone().expect("a session");
    assert_eq!(done.run.thread.as_deref(), Some(thread.as_str()));
    assert!(
        routines::findings(&env.conn, "demo", "staging", 5)
            .unwrap()
            .is_empty()
    );
    let row = env.row(&thread);
    assert_eq!(row.routine.as_deref(), Some("staging"));
    let task = row.task.unwrap();
    assert!(task.contains("Why this run started."), "{task}");
    assert!(task.contains("came to this: exit 22."), "{task}");
    assert!(task.contains("    curl: (22) 503"), "{task}");
    assert!(
        task.contains("    test -f \"$CHIEF_ROUTINE_DIR/up\""),
        "{task}"
    );
    assert!(task.ends_with("Find out why staging is down and say what to do."));
    assert!(env.fake.calls_of("--bg")[0].contains(&"haiku".to_string()));
    let routine = env.routine("staging");
    assert_eq!(routine.last_thread.as_deref(), Some(thread.as_str()));
    assert_eq!(state_key(&env, "staging"), "failing");

    // While that session works, the occurrence is skipped whole.
    let skipped = env
        .with(|ctx| routines::run_here(ctx, "staging", Trigger::Manual))
        .unwrap();
    assert!(matches!(skipped, Outcome::Skipped(_)), "{skipped:?}");

    // It keeps failing after the session is done: the same failure starts no second one.
    finish(&env, &thread, NOW);
    let done = ran(&env, "staging");
    assert_eq!((done.news, done.thread), (None, None));
    assert_eq!(env.fake.calls_of("--bg").len(), 1);

    // What the session said stands for the failure wherever the routine is shown.
    env.with(|ctx| {
        routines::report_outcome_tool(
            ctx,
            "staging",
            &routines::ReportOutcomeArgs {
                outcome: routines::Verdict::Attention,
                headline: "Staging is down: the deploy at 09:12 ran out of disk".into(),
                details: None,
            },
        )
    })
    .unwrap();
    let state = routines::state(&env.conn, &env.routine("staging"), NOW).unwrap();
    assert_eq!(
        (state.key, state.line.as_str()),
        (
            "failing",
            "Staging is down: the deploy at 09:12 ran out of disk"
        )
    );

    // Passing again is said to people, not handed to a session.
    std::fs::write(&up, "").unwrap();
    let done = ran(&env, "staging");
    assert!(done.news.unwrap().recovery);
    assert_eq!(done.thread, None);
    assert_eq!(env.fake.calls_of("--bg").len(), 1);
    assert_eq!(state_key(&env, "staging"), "ok");

    // News with no session to take it still reaches the user: here the start fails.
    std::fs::remove_file(&up).unwrap();
    env.fake
        .unset("bg.stdout")
        .set("bg.exit", "1")
        .set("bg.stderr", "no such model\n");
    let done = ran(&env, "staging");
    assert_eq!(done.thread, None);
    let found = routines::findings(&env.conn, "demo", "staging", 1).unwrap();
    assert_eq!(found[0].verdict, routines::Verdict::Attention);
    assert!(
        found[0]
            .details
            .as_deref()
            .unwrap()
            .contains("No session looked into it: chief couldn't start a session for it"),
        "{:?}",
        found[0].details
    );
}

#[test]
fn a_command_gets_its_time_and_no_more() {
    let env = Env::new();
    env.create(&Draft {
        timeout_secs: Some(1),
        ..command_draft("Slow", "echo started; sleep 30; echo never")
    })
    .unwrap();
    let began = std::time::Instant::now();
    let done = ran(&env, "slow");
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(done.run.status, chief::command::Status::Timeout);
    assert_eq!(done.run.exit_code, None);
    assert_eq!(done.run.output.as_deref(), Some("started\n"));
    assert!(
        done.news
            .unwrap()
            .headline
            .starts_with("Failed: timed out after")
    );
    assert_eq!(state_key(&env, "slow"), "failing");
}

/// One run of a command at a time; a run whose process is gone doesn't hold the next one up.
#[test]
fn a_command_still_running_holds_the_next_run() {
    let env = Env::new();
    env.create(&command_draft("Sync", "echo synced")).unwrap();
    let begin = |pid: Option<u32>| {
        let id = chief::command::begin(&env.conn, "demo", "sync", Trigger::Schedule, NOW).unwrap();
        env.conn
            .execute(
                "UPDATE routine_runs SET pid = ?2 WHERE id = ?1",
                rusqlite::params![id, pid],
            )
            .unwrap();
        id
    };
    // This test's own process stands in for a run under way.
    let live = begin(Some(std::process::id()));
    assert_eq!(state_key(&env, "sync"), "running");
    let skipped = env
        .with(|ctx| routines::run_here(ctx, "sync", Trigger::Manual))
        .unwrap();
    let Outcome::Skipped(why) = skipped else {
        panic!("it ran: {skipped:?}");
    };
    assert!(why.contains("its command still runs"), "{why}");

    // What a command says of itself is kept on its run while it runs, and only then.
    chief::command::say(
        &env.conn,
        live,
        routines::Verdict::Update,
        " 3 files\nsynced ",
        Some("a, b, c"),
    )
    .unwrap();
    let said = chief::command::get(&env.conn, live).unwrap().unwrap();
    assert_eq!(said.outcome, Some(routines::Verdict::Update));
    assert_eq!(said.headline.as_deref(), Some("3 files synced"));
    assert!(chief::command::say(&env.conn, live, routines::Verdict::Update, " ", None).is_err());
    chief::command::abandon(&env.conn, live, "stopped", NOW).unwrap();
    assert!(
        chief::command::say(&env.conn, live, routines::Verdict::Update, "late", None)
            .unwrap_err()
            .to_string()
            .contains("isn't running")
    );

    // A run whose process is gone was lost, and the routine runs again.
    let gone = begin(Some(0x3fff_fff0));
    let done = ran(&env, "sync");
    assert_eq!(done.run.output.as_deref(), Some("synced\n"));
    assert_eq!(
        chief::command::get(&env.conn, gone)
            .unwrap()
            .unwrap()
            .status,
        chief::command::Status::Lost
    );
    // The failed and the lost run before it say nothing about a pass.
    assert_eq!(done.news.map(|n| n.recovery), Some(true));
}
