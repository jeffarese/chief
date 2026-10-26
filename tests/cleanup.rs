//! Thread cleanup (`chief::cleanup`, `chief::pr`) against real temporary git repositories, the
//! fake `claude` and the fake `gh`. Nothing here starts a session or calls GitHub.

mod common;

use std::path::PathBuf;

use chief::cleanup::{self, Ctx, Mode, Outcome, Verdict};
use chief::config::{Cleanup, ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::pr;
use chief::state::Role;
use common::{Fake, GitRepo, Home, agent, git, stderr, stdout};
use rusqlite::Connection;
use serde_json::{Value, json};

const NOW: i64 = 1_800_000_000_000;
const SHORT: &str = "b65c3d73";
const SID: &str = "sid-b65c3d73";
const NAME: &str = "demo-notes-1a2b";

fn now() -> i64 {
    NOW
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/rm/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

struct Env {
    _root: tempfile::TempDir,
    conn: Connection,
    claude: Fake,
    gh: Fake,
    git: GitRepo,
    worktree: PathBuf,
}

impl Env {
    /// A project with one thread, resolved and asleep, whose worktree holds one pushed commit.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let conn = db::open(&root.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", root.path(), 0).unwrap();
        let git = GitRepo::new();
        let worktree = git.worktree("add-notes");
        git.commit(&worktree, "notes.txt", "Add notes");
        git.push(&worktree);
        let env = Env {
            _root: root,
            conn,
            claude: Fake::new(),
            gh: Fake::named("gh"),
            git,
            worktree,
        };
        env.insert(|_| {});
        env.claude.job(SHORT, Some(&env.worktree));
        // Listed without a pid: stopped, so neither working nor removed.
        env.agents(&[agent(SHORT, SID, NAME, "done", None)]);
        env.claude.set("rm.stdout", &format!("removed {SHORT}\n"));
        env
    }

    fn insert(&self, f: impl FnOnce(&mut SessionRow)) {
        let _ = db::delete_session(&self.conn, NAME);
        let mut row = SessionRow::new(NAME, "demo", Role::Thread, NOW - 60_000);
        row.short_id = Some(SHORT.into());
        row.session_id = Some(SID.into());
        row.repo_path = Some(self.git.repo.to_string_lossy().into_owned());
        row.cwd = Some(self.worktree.to_string_lossy().into_owned());
        row.branch = Some("worktree-add-notes".into());
        row.last_hook = Some("Stop".into());
        row.resolved = true;
        row.resolved_reason = Some("user".into());
        row.group_name = "resolved".into();
        f(&mut row);
        db::insert_session(&self.conn, &row).unwrap();
    }

    fn agents(&self, rows: &[Value]) {
        self.claude
            .set("agents.stdout", &Value::from(rows.to_vec()).to_string());
    }

    fn with<T>(&self, f: impl FnOnce(&Ctx) -> T) -> T {
        let claude = self.claude.claude();
        let gh = self.gh.gh();
        f(&Ctx {
            conn: &self.conn,
            claude: &claude,
            gh: &gh,
            now,
        })
    }

    fn clean(&self, mode: Mode) -> anyhow::Result<Outcome> {
        self.with(|ctx| cleanup::clean(ctx, NAME, mode))
    }

    /// What the user is shown and confirms before a discard.
    fn inspect(&self) -> cleanup::Inspection {
        cleanup::inspect(&self.claude.claude(), &self.row()).unwrap()
    }

    /// `claude rm` refusing with one unpushed commit at `head`.
    fn rm_refuses_unpushed(&self, k: usize, head: &str) {
        let text =
            fixture("unpushed-one").replace("a2a6266abcfc311c36e618d6f2c556188b528181", head);
        self.claude
            .set(&format!("rm.stdout.{k}"), &text)
            .set(&format!("rm.exit.{k}"), "1");
    }

    fn head(&self) -> String {
        git(&self.worktree, &["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    fn row(&self) -> SessionRow {
        db::get_session(&self.conn, NAME).unwrap().unwrap()
    }

    fn rm_calls(&self) -> Vec<Vec<String>> {
        self.claude.calls_of("rm")
    }

    fn config(&self, cleanup: Cleanup) -> ProjectConfig {
        let mut c = ProjectConfig::new(
            "Demo",
            "demo",
            None,
            vec![Repo {
                name: "repo".into(),
                path: self.git.repo.clone(),
            }],
        );
        c.threads.cleanup = cleanup;
        c
    }
}

fn ignore(git: &GitRepo, pattern: &str) {
    let exclude = git.repo.join(".git/info/exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str(&format!("{pattern}\n"));
    std::fs::write(exclude, text).unwrap();
}

#[test]
fn inspect_reads_the_worktree_from_the_job_state() {
    let env = Env::new();
    let claude = env.claude.claude();
    let row = env.row();
    let ins = cleanup::inspect(&claude, &row).unwrap();
    assert_eq!(ins.worktree.as_deref(), Some(env.worktree.as_path()));
    assert!(ins.exists);
    assert_eq!(ins.branch.as_deref(), Some("worktree-add-notes"));
    assert_eq!((ins.dirty.len(), ins.unpushed_count), (0, 0));
    assert_eq!(ins.blocker(), None);

    // Ignored files don't count; modified and untracked ones do.
    ignore(&env.git, "*.log");
    std::fs::write(env.worktree.join("debug.log"), "x").unwrap();
    std::fs::write(env.worktree.join("notes.txt"), "changed\n").unwrap();
    std::fs::write(env.worktree.join("todo.txt"), "new\n").unwrap();
    env.git.commit(&env.worktree, "a.txt", "Second");
    env.git.commit(&env.worktree, "b.txt", "Third");
    // The hooks' cwd follows the thread's shell anywhere; it never picks the worktree.
    let elsewhere = env.git.worktree("elsewhere");
    let mut wandered = row.clone();
    wandered.cwd = Some(elsewhere.join("src").to_string_lossy().into_owned());
    let ins = cleanup::inspect(&claude, &wandered).unwrap();
    assert_eq!(ins.worktree.as_deref(), Some(env.worktree.as_path()));
    let mut dirty = ins.dirty.clone();
    dirty.sort();
    assert_eq!(dirty, [" M notes.txt", "?? todo.txt"].map(String::from));
    assert_eq!(ins.unpushed_count, 2);
    assert_eq!(ins.unpushed.len(), 2);
    assert!(ins.unpushed[0].ends_with(" Third"), "{:?}", ins.unpushed);
    assert_eq!(
        ins.blocker().as_deref(),
        Some("2 unpushed commits, uncommitted changes: 2 files")
    );

    // The worktree is gone but its branch isn't: commits are still checked, in the repo.
    git(
        &env.git.repo,
        &[
            "worktree",
            "remove",
            "--force",
            env.worktree.to_str().unwrap(),
        ],
    );
    let ins = cleanup::inspect(&claude, &row).unwrap();
    assert!(!ins.exists);
    assert_eq!((ins.dirty.len(), ins.unpushed_count), (0, 2));

    // A folder there that isn't a worktree would let git answer for the checkout around it.
    std::fs::create_dir_all(&env.worktree).unwrap();
    let err = cleanup::inspect(&claude, &row).unwrap_err();
    assert!(err.to_string().contains("is not a git worktree"), "{err:#}");

    // A job without a worktree (it never left the checkout) has nothing of its own to lose, even
    // on a main with unpushed commits; nor does one whose worktree isn't under .claude/worktrees.
    env.git.commit(&env.git.repo, "local.txt", "Local only");
    let mut plain = row.clone();
    plain.cwd = plain.repo_path.clone();
    plain.branch = Some("main".into());
    for job in [None, Some(env.git.repo.as_path())] {
        env.claude.job(SHORT, job);
        assert_eq!(
            cleanup::inspect(&claude, &plain).unwrap(),
            cleanup::Inspection::default(),
            "{job:?}"
        );
    }
    env.claude.drop_job(SHORT);
    assert_eq!(
        cleanup::inspect(&claude, &row).unwrap(),
        cleanup::Inspection::default()
    );
}

/// The reviewer's probe: thread A's last hook ran in thread B's worktree (`cd ../b`). A Discard
/// of A must never reset B.
#[test]
fn discard_resets_only_the_threads_own_worktree() {
    let env = Env::new();
    let b = env.git.worktree("b");
    std::fs::write(b.join("precious.txt"), "B's work\n").unwrap();
    std::fs::write(b.join("README.md"), "B edited\n").unwrap();
    let mut row_b = SessionRow::new("demo-b-2222", "demo", Role::Thread, NOW);
    row_b.short_id = Some("bb22".into());
    row_b.session_id = Some("sid-bb22".into());
    row_b.cwd = Some(b.to_string_lossy().into_owned());
    db::insert_session(&env.conn, &row_b).unwrap();
    env.claude.job("bb22", Some(&b));
    let b_intact = || {
        assert_eq!(
            std::fs::read_to_string(b.join("precious.txt")).unwrap(),
            "B's work\n"
        );
        assert_eq!(
            std::fs::read_to_string(b.join("README.md")).unwrap(),
            "B edited\n"
        );
    };

    // A never got a worktree of its own: nothing is reset, wherever its cwd was.
    env.insert(|r| r.cwd = Some(b.join("src").to_string_lossy().into_owned()));
    env.claude.job(SHORT, None);
    assert_eq!(
        env.clean(Mode::Discard(env.inspect())).unwrap(),
        Outcome::Cleaned
    );
    b_intact();

    // A's job names B's worktree: another registered session works there, so no reset and no rm.
    env.insert(|_| {});
    env.claude.job(SHORT, Some(&b));
    let rm_before = env.rm_calls().len();
    let Outcome::Blocked(why) = env.clean(Mode::Discard(env.inspect())).unwrap() else {
        panic!("want blocked");
    };
    assert!(why.contains("demo-b-2222"), "{why}");
    b_intact();
    assert_eq!(env.rm_calls().len(), rm_before);
}

/// The reviewer's probe: two registered sessions whose job state names one worktree. `claude rm`
/// would delete it and its branch under the other session, so a safe cleanup refuses too, before
/// `claude rm`, even with nothing unsaved.
#[test]
fn safe_clean_refuses_a_worktree_another_session_names() {
    let env = Env::new();
    let mut other = SessionRow::new("demo-b-2222", "demo", Role::Thread, NOW);
    other.short_id = Some("bb22".into());
    other.session_id = Some("sid-bb22".into());
    db::insert_session(&env.conn, &other).unwrap();
    env.claude.job("bb22", Some(&env.worktree));
    let Outcome::Blocked(why) = env.clean(Mode::Safe).unwrap() else {
        panic!("want blocked");
    };
    assert!(why.contains("also the worktree of demo-b-2222"), "{why}");
    assert_eq!(env.row().cleanup_blocked.as_deref(), Some(why.as_str()));
    assert!(env.rm_calls().is_empty());
    assert!(env.worktree.is_dir());
}

#[test]
fn safe_clean_removes_a_thread_with_nothing_to_lose() {
    let env = Env::new();
    db::insert_report(&env.conn, NAME, NOW - 1000, "done: notes added").unwrap();
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::Cleaned);
    assert_eq!(env.rm_calls(), [vec!["rm".to_string(), SHORT.to_string()]]);
    let row = env.row();
    assert_eq!((row.cleaned_at, row.cleanup_blocked), (Some(NOW), None));
    assert!(row.resolved);
    assert_eq!(
        db::recent_reports(&env.conn, NAME, 5).unwrap().len(),
        1,
        "reports are kept"
    );
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::AlreadyCleaned);
    assert_eq!(env.rm_calls().len(), 1);
}

#[test]
fn safe_clean_never_stops_a_thread_it_would_not_remove() {
    let env = Env::new();
    env.git.commit(&env.worktree, "draft.txt", "Draft");
    assert_eq!(
        env.clean(Mode::Safe).unwrap(),
        Outcome::Blocked("1 unpushed commit".into())
    );
    assert!(
        env.rm_calls().is_empty(),
        "rm stops the session even when it refuses"
    );
    assert_eq!(
        (env.row().cleanup_blocked.as_deref(), env.row().cleaned_at),
        (Some("1 unpushed commit"), None)
    );

    // Pushed now, but dirty.
    env.git.push(&env.worktree);
    std::fs::write(env.worktree.join("scratch.txt"), "x").unwrap();
    assert_eq!(
        env.clean(Mode::Safe).unwrap(),
        Outcome::Blocked("uncommitted changes: 1 file".into())
    );
    assert!(env.rm_calls().is_empty());

    // Clean again: removed, and the old reason is cleared.
    std::fs::remove_file(env.worktree.join("scratch.txt")).unwrap();
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::Cleaned);
    assert_eq!(env.row().cleanup_blocked, None);
}

#[test]
fn a_refusal_from_claude_rm_is_recorded() {
    let env = Env::new();
    env.claude
        .set("rm.stdout", &fixture("dirty"))
        .set("rm.exit", "1");
    assert_eq!(
        env.clean(Mode::Safe).unwrap(),
        Outcome::Blocked("uncommitted changes".into())
    );
    env.claude
        .set("rm.stdout", &fixture("unpushed-many"))
        .set("rm.exit", "1");
    assert_eq!(
        env.clean(Mode::Safe).unwrap(),
        Outcome::Blocked("3 unpushed commits".into()),
        "Safe never retries with the token"
    );
    assert_eq!(env.rm_calls().len(), 2);
    env.claude
        .set("rm.stdout", "")
        .set("rm.stderr", "error: something new\n");
    let Outcome::Blocked(why) = env.clean(Mode::Safe).unwrap() else {
        panic!("want blocked");
    };
    assert_eq!(why, "claude rm: error: something new");
    assert_eq!(env.row().cleanup_blocked.as_deref(), Some(why.as_str()));
}

#[test]
fn clean_refuses_threads_that_are_open_or_busy() {
    let env = Env::new();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
        r.group_name = "idle".into();
    });
    let err = env
        .clean(Mode::Discard(Default::default()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not resolved"), "{err}");

    // Resolved (its PR merged) but still mid-turn: rm would stop it.
    env.insert(|_| {});
    env.agents(&[agent(SHORT, SID, NAME, "working", Some(42))]);
    let err = env.clean(Mode::Safe).unwrap_err().to_string();
    assert!(err.contains("is working"), "{err}");
    // Waiting on a permission prompt.
    let mut waiting = agent(SHORT, SID, NAME, "blocked", Some(42));
    waiting["status"] = "waiting".into();
    env.agents(&[waiting]);
    let err = env.clean(Mode::Safe).unwrap_err().to_string();
    assert!(err.contains("is waiting on you"), "{err}");
    assert!(env.rm_calls().is_empty());
    assert_eq!(env.row().cleanup_blocked, None);

    let err = env
        .with(|ctx| cleanup::clean(ctx, "chief-demo", Mode::Safe))
        .unwrap_err();
    assert!(err.to_string().contains("no thread named"), "{err}");
}

/// The board's 30 s refresh, `chief gc` and the board's `x` can clean the same thread at once.
#[test]
fn a_cleanup_another_process_holds_is_skipped() {
    let env = Env::new();
    assert_eq!(
        db::claim_cleanup(&env.conn, NAME, NOW - 60_000).unwrap(),
        db::Claim::Claimed
    );
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::InProgress);
    assert!(env.rm_calls().is_empty());
    assert_eq!(env.row().cleanup_blocked, None);
    let gc = env.with(|ctx| cleanup::gc(ctx, "demo", false)).unwrap();
    assert_eq!(
        gc.verdicts,
        [(
            NAME.to_string(),
            Verdict::Skipped("another chief process is cleaning it up".into())
        )]
    );
    assert!(env.rm_calls().is_empty());

    // A claim left by a process that died mid-cleanup expires.
    db::release_cleanup(&env.conn, NAME, NOW - 60_000).unwrap();
    assert_eq!(
        db::claim_cleanup(&env.conn, NAME, NOW - 11 * 60_000).unwrap(),
        db::Claim::Claimed
    );
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::Cleaned);
    assert_eq!(env.rm_calls().len(), 1);
    assert_eq!(
        db::claim_cleanup(&env.conn, NAME, NOW).unwrap(),
        db::Claim::Cleaned
    );

    // Every path releases its claim: a blocked cleanup, a refused one.
    env.insert(|_| {});
    std::fs::write(env.worktree.join("draft.txt"), "wip").unwrap();
    assert!(matches!(
        env.clean(Mode::Safe).unwrap(),
        Outcome::Blocked(_)
    ));
    assert_eq!(
        db::claim_cleanup(&env.conn, NAME, NOW).unwrap(),
        db::Claim::Claimed
    );
    db::release_cleanup(&env.conn, NAME, NOW).unwrap();
    env.insert(|r| r.resolved = false);
    assert!(env.clean(Mode::Safe).is_err());
    assert_eq!(
        db::claim_cleanup(&env.conn, NAME, NOW).unwrap(),
        db::Claim::Claimed
    );
}

/// The thread got a message (or the pane attached to it) after the first check: `claude rm`
/// would stop it mid-turn, so nothing is removed or reset.
#[test]
fn cleanup_checks_again_right_before_claude_rm() {
    let env = Env::new();
    std::fs::write(env.worktree.join("draft.txt"), "wip").unwrap();
    let confirmed = env.inspect();
    let idle = Value::from(vec![agent(SHORT, SID, NAME, "done", None)]).to_string();
    let busy = Value::from(vec![agent(SHORT, SID, NAME, "working", Some(42))]).to_string();
    env.claude.set("agents.stdout.1", &idle);
    env.claude.set("agents.stdout.2", &busy);
    let err = env
        .clean(Mode::Discard(confirmed.clone()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("is working"), "{err}");
    assert!(env.rm_calls().is_empty());
    assert!(env.worktree.join("draft.txt").exists(), "nothing reset");
    assert_eq!(env.row().cleanup_blocked, None);

    // Shown in the pane: the user may be typing into it.
    env.insert(|_| {});
    env.agents(&[agent(SHORT, SID, NAME, "done", None)]);
    db::set_pane_showing(&env.conn, "demo", Some(NAME)).unwrap();
    let err = env.clean(Mode::Safe).unwrap_err().to_string();
    assert!(err.contains("is showing in the pane"), "{err}");
    // Its PR merged: the board's refresh waits too, without an error line every 30 s.
    env.gh.set("pr.stdout", &merged_pr());
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert!(refresh.errors.is_empty(), "{:?}", refresh.errors);
    assert!(env.rm_calls().is_empty());
    db::set_pane_showing(&env.conn, "demo", None).unwrap();
    assert_eq!(
        env.clean(Mode::Discard(confirmed)).unwrap(),
        Outcome::Cleaned
    );
}

#[test]
fn discard_resets_the_worktree_and_retries_with_the_token() {
    let env = Env::new();
    ignore(&env.git, "*.log");
    std::fs::write(env.worktree.join("keep.log"), "ignored\n").unwrap();
    std::fs::write(env.worktree.join("notes.txt"), "changed\n").unwrap();
    std::fs::write(env.worktree.join("untracked.txt"), "x").unwrap();
    std::fs::create_dir(env.worktree.join("newdir")).unwrap();
    std::fs::write(env.worktree.join("newdir/f.txt"), "x").unwrap();
    env.git.commit(&env.worktree, "c.txt", "Unpushed");
    let confirmed = env.inspect();
    assert_eq!(confirmed.unpushed_count, 1);
    assert_eq!(confirmed.head.as_deref(), Some(env.head().as_str()));
    env.rm_refuses_unpushed(1, &env.head());
    env.claude.set(
        "rm.stdout.2",
        &fixture("removed").replace("58534bf3", SHORT),
    );

    assert_eq!(
        env.clean(Mode::Discard(confirmed)).unwrap(),
        Outcome::Cleaned
    );
    let token = format!("{}@03abe83c324a9c3e9a0db4020ffa195d", env.head());
    assert_eq!(
        env.rm_calls(),
        [
            vec!["rm".to_string(), SHORT.to_string()],
            ["rm", SHORT, "--discard-unpushed", token.as_str()]
                .map(String::from)
                .to_vec(),
        ]
    );
    // The fake rm doesn't delete the worktree, so what discard did is visible.
    assert_eq!(
        std::fs::read_to_string(env.worktree.join("notes.txt")).unwrap(),
        "Add notes\n"
    );
    assert!(!env.worktree.join("untracked.txt").exists());
    assert!(!env.worktree.join("newdir").exists());
    assert!(env.worktree.join("keep.log").exists(), "never git clean -x");
    assert_eq!(env.row().cleaned_at, Some(NOW));
}

/// `--discard-unpushed` only for commits chief listed itself, the user confirmed, at the head it
/// saw; anything else stops with rm's reason.
#[test]
fn discard_never_passes_the_token_for_commits_nobody_confirmed() {
    let env = Env::new();
    let blocked = |want: &str| {
        let outcome = env.clean(Mode::Discard(env.inspect()));
        assert_eq!(outcome.unwrap(), Outcome::Blocked(want.into()));
        assert_eq!(env.row().cleanup_blocked.as_deref(), Some(want));
        assert_eq!(env.row().cleaned_at, None);
    };

    // chief sees nothing unpushed (rm counts something it doesn't): no retry.
    env.rm_refuses_unpushed(1, &env.head());
    blocked("1 unpushed commit");
    assert_eq!(env.rm_calls(), [vec!["rm".to_string(), SHORT.to_string()]]);

    // Listed and confirmed, but the token is for another head.
    env.git.commit(&env.worktree, "late.txt", "Late");
    env.rm_refuses_unpushed(2, "0123456789abcdef0123456789abcdef01234567");
    blocked("1 unpushed commit");
    assert_eq!(env.rm_calls().len(), 2);
    assert!(
        env.rm_calls().iter().all(|c| c.len() == 2),
        "never the token"
    );

    // Confirmed, then the worktree changed: nothing is reset and rm doesn't run.
    let confirmed = env.inspect();
    env.git.commit(&env.worktree, "later.txt", "Later");
    std::fs::write(env.worktree.join("scratch.txt"), "x").unwrap();
    let Outcome::Blocked(why) = env.clean(Mode::Discard(confirmed)).unwrap() else {
        panic!("want blocked");
    };
    assert!(why.contains("changed"), "{why}");
    assert!(env.worktree.join("scratch.txt").exists());
    assert_eq!(env.rm_calls().len(), 2);
}

/// The reviewer's CLI repro: a thread chief can see no worktree for printed "Nothing unsaved will
/// be lost." and then discarded a commit `claude rm` reported.
#[test]
fn cli_discard_keeps_commits_it_did_not_show() {
    let home = Home::new();
    {
        let conn = home.db();
        db::register_project(&conn, "demo", home.path(), 0).unwrap();
        let mut row = SessionRow::new("demo-x-1111", "demo", Role::Thread, NOW);
        row.short_id = Some(SHORT.into());
        row.session_id = Some("t-sid".into());
        row.resolved = true;
        db::insert_session(&conn, &row).unwrap();
    }
    home.fake
        .set(
            "agents.stdout",
            &json!([
                agent(SHORT, "t-sid", "demo-x-1111", "done", None),
                agent("c0", "c-sid", "chief-demo", "done", Some(100)),
            ])
            .to_string(),
        )
        .set("rm.stdout.1", &fixture("unpushed-one"))
        .set("rm.exit.1", "1")
        .set("rm.stdout.2", &format!("removed {SHORT}\n"));

    let out = home.run(&["clean", "demo", "demo-x-1111", "--discard"], "");
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("Nothing unsaved will be lost."),
        "{}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("1 unpushed commit"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        home.fake.calls_of("rm"),
        [vec!["rm".to_string(), SHORT.to_string()]],
        "no --discard-unpushed"
    );
    let row = db::get_session(&home.db(), "demo-x-1111").unwrap().unwrap();
    assert_eq!(
        (row.cleaned_at, row.cleanup_blocked.as_deref()),
        (None, Some("1 unpushed commit"))
    );
}

#[test]
fn a_thread_that_never_started_is_cleaned_without_rm() {
    let env = Env::new();
    env.insert(|r| {
        r.short_id = None;
        r.session_id = None;
        r.cwd = r.repo_path.clone();
    });
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::Cleaned);
    assert!(env.rm_calls().is_empty());
}

#[test]
fn a_thread_removed_outside_chief_is_marked_cleaned() {
    let env = Env::new();
    let mut coord = SessionRow::new("chief-demo", "demo", Role::Coordinator, 0);
    coord.short_id = Some("c0ffee".into());
    coord.session_id = Some("coord".into());
    db::insert_session(&env.conn, &coord).unwrap();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
    });
    env.agents(&[agent("c0ffee", "coord", "chief-demo", "done", Some(1))]);

    // `claude rm` elsewhere left the worktree (its job state still names it): resolved, kept.
    let Outcome::Blocked(why) = env.clean(Mode::Safe).unwrap() else {
        panic!("want blocked");
    };
    let remains = format!(
        "session removed outside chief; worktree remains at {}",
        env.worktree.display()
    );
    assert_eq!(why, remains);
    let row = env.row();
    assert_eq!(
        (
            row.cleaned_at,
            row.resolved_reason.as_deref(),
            row.cleanup_blocked.as_deref()
        ),
        (None, Some("removed"), Some(remains.as_str()))
    );
    assert!(env.rm_calls().is_empty());

    // The worktree is gone: cleaned, still without `claude rm`.
    env.claude.drop_job(SHORT);
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::AlreadyCleaned);
    assert_eq!(env.row().cleaned_at, Some(NOW));
    assert!(env.rm_calls().is_empty());

    // Removed with its worktree in one go: cleaned at once.
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
    });
    assert_eq!(env.clean(Mode::Safe).unwrap(), Outcome::AlreadyCleaned);
    let row = env.row();
    assert_eq!(
        (row.cleaned_at, row.resolved_reason.as_deref()),
        (Some(NOW), Some("removed"))
    );
    assert!(env.rm_calls().is_empty());
}

/// Opened 30 s after the thread row (NOW - 60 s, 2027-01-15T07:59:00Z).
fn merged_pr() -> String {
    json!([{"url": "https://github.com/o/r/pull/3", "state": "MERGED", "isDraft": false,
            "reviewDecision": "APPROVED", "createdAt": "2027-01-15T07:59:30Z",
            "isCrossRepository": false}])
    .to_string()
}

/// A new thread whose worktree reuses an old branch name sees the old thread's merged PR, and a
/// fork's PR from a branch of the same name: neither is its own.
#[test]
fn a_reused_branch_name_ignores_older_and_cross_repository_prs() {
    let env = Env::new();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
        r.group_name = "idle".into();
    });
    env.gh.set(
        "pr.stdout",
        &json!([
            {"url": "https://github.com/fork/r/pull/9", "state": "MERGED", "isDraft": false,
             "reviewDecision": "", "createdAt": "2027-01-15T07:59:40Z",
             "isCrossRepository": true},
            {"url": "https://github.com/o/r/pull/1", "state": "MERGED", "isDraft": false,
             "reviewDecision": "APPROVED", "createdAt": "2026-03-02T10:00:00Z",
             "isCrossRepository": false}
        ])
        .to_string(),
    );
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert_eq!((refresh.polled, refresh.cleaned.len()), (1, 0));
    let row = env.row();
    assert_eq!(
        (
            row.pr_url,
            row.resolved,
            row.cleaned_at,
            row.resolved_reason
        ),
        (None, false, None, None)
    );
    assert!(env.rm_calls().is_empty());
}

/// The reviewer's probe: PRs are matched on what the job state names, never the hooks' cwd or
/// branch. A thread whose shell sits in the checkout on `main` is polled for its worktree's
/// branch; one whose job never entered a worktree isn't polled, wherever its hooks say it is.
#[test]
fn prs_are_polled_for_the_branch_the_job_state_names() {
    let env = Env::new();
    let open = |r: &mut SessionRow| {
        r.resolved = false;
        r.resolved_reason = None;
        r.group_name = "idle".into();
    };
    env.insert(|r| {
        open(r);
        r.cwd = r.repo_path.clone();
        r.branch = Some("main".into());
    });
    env.gh.set("pr.stdout", "[]");
    let polled = || {
        let refresh = env.with(|ctx| pr::poll_project(ctx, "demo")).unwrap();
        assert!(refresh.errors.is_empty(), "{:?}", refresh.errors);
        refresh.polled
    };
    assert_eq!(polled(), 1);
    assert_eq!(env.gh.calls()[0][3], "worktree-add-notes");

    // Its hooks say it works in a worktree on a branch; its job never entered one.
    env.insert(|r| {
        open(r);
        r.cwd = Some(env.worktree.join("src").to_string_lossy().into_owned());
    });
    env.claude.job(SHORT, None);
    assert_eq!(polled(), 0);
    // A worktree, but no branch recorded: nothing to match a PR on.
    env.claude.job_on(SHORT, Some(&env.worktree), None);
    assert_eq!(polled(), 0);
    assert_eq!(env.gh.calls().len(), 1);
}

#[test]
fn a_merged_pr_resolves_and_cleans_the_thread() {
    let env = Env::new();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
        r.group_name = "idle".into();
    });
    env.gh.set("pr.stdout", &merged_pr());

    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert_eq!(
        (refresh.polled, refresh.cleaned.as_slice()),
        (1, [NAME.to_string()].as_slice())
    );
    assert!(refresh.errors.is_empty(), "{:?}", refresh.errors);
    let calls = env.gh.calls();
    assert_eq!(
        calls[0],
        [
            "pr",
            "list",
            "--head",
            "worktree-add-notes",
            "--state",
            "all",
            "--json",
            "url,state,isDraft,reviewDecision,createdAt,isCrossRepository",
            "--limit",
            "10"
        ]
    );
    assert_eq!(env.gh.cwd(0), env.git.repo);
    let row = env.row();
    assert_eq!(
        (
            row.pr_state.as_deref(),
            row.resolved_reason.as_deref(),
            row.cleaned_at
        ),
        (Some("MERGED"), Some("merged"), Some(NOW))
    );

    // Cleaned threads aren't polled again.
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert_eq!(refresh.polled, 0);
}

#[test]
fn merged_cleanup_waits_for_idle_and_respects_manual() {
    let env = Env::new();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
    });
    env.gh.set("pr.stdout", &merged_pr());
    let manual = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Manual)))
        .unwrap();
    assert!(manual.cleaned.is_empty());
    assert_eq!(
        (env.row().resolved_reason.as_deref(), env.row().cleaned_at),
        (Some("merged"), None)
    );

    // Still working: stored, not cleaned, tried again on the next refresh.
    let mut busy = env.row();
    busy.pid = Some(42);
    busy.cc_state = Some("working".into());
    busy.cc_status = Some("busy".into());
    busy.cc_fresh = true;
    db::update_session(&env.conn, &busy).unwrap();
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert!(refresh.cleaned.is_empty() && refresh.errors.is_empty());
    assert!(env.rm_calls().is_empty());

    // Idle, but unpushed work: blocked once, then left for gc or the board.
    env.agents(&[agent(SHORT, SID, NAME, "done", None)]);
    let mut idle = env.row();
    idle.cc_status = Some("idle".into());
    db::update_session(&env.conn, &idle).unwrap();
    env.git.commit(&env.worktree, "late.txt", "Late");
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert_eq!(
        refresh.blocked,
        [(NAME.to_string(), "1 unpushed commit".to_string())]
    );
    let agents_calls = env.claude.calls_of("agents").len();
    let refresh = env
        .with(|ctx| pr::refresh_project(ctx, &env.config(Cleanup::Merged)))
        .unwrap();
    assert!(refresh.blocked.is_empty());
    assert_eq!(
        env.claude.calls_of("agents").len(),
        agents_calls,
        "not retried"
    );
}

#[test]
fn gh_trouble_is_a_hint_or_an_error() {
    let env = Env::new();
    env.insert(|r| {
        r.resolved = false;
        r.resolved_reason = None;
    });
    let missing = chief::pr::Gh::new("/nonexistent/gh".into());
    let claude = env.claude.claude();
    let ctx = Ctx {
        conn: &env.conn,
        claude: &claude,
        gh: &missing,
        now,
    };
    let first = pr::poll_project(&ctx, "demo").unwrap();
    assert!(first.hint.unwrap().contains("isn't installed"));
    assert!(first.errors.is_empty());
    assert!(
        pr::poll_project(&ctx, "demo").unwrap().hint.is_none(),
        "once"
    );

    env.gh
        .set("pr.stderr", "no git remotes found\n")
        .set("pr.exit", "1");
    let gh = env.gh.gh();
    assert_eq!(
        gh.poll(&env.git.repo, "worktree-add-notes", NOW).unwrap(),
        None
    );
    assert!(gh.take_hint().unwrap().contains("has no GitHub remote"));

    env.gh.set("pr.stderr", "HTTP 502\n");
    let refresh = env.with(|ctx| pr::poll_project(ctx, "demo")).unwrap();
    assert!(
        refresh.errors[0].contains("HTTP 502"),
        "{:?}",
        refresh.errors
    );
}

#[test]
fn gc_dry_run_then_clean() {
    let env = Env::new();
    // A second resolved thread with unpushed work, and an open one gc leaves alone.
    let wt = env.git.worktree("other");
    env.git.commit(&wt, "o.txt", "Other");
    let mut other = SessionRow::new("demo-other-2222", "demo", Role::Thread, NOW);
    other.short_id = Some("o2".into());
    other.session_id = Some("sid-o2".into());
    other.cwd = Some(wt.to_string_lossy().into_owned());
    other.repo_path = Some(env.git.repo.to_string_lossy().into_owned());
    other.branch = Some("worktree-other".into());
    other.resolved = true;
    db::insert_session(&env.conn, &other).unwrap();
    env.claude.job("o2", Some(&wt));
    let mut open = SessionRow::new("demo-open-3333", "demo", Role::Thread, NOW);
    open.short_id = Some("o3".into());
    db::insert_session(&env.conn, &open).unwrap();
    env.agents(&[
        agent(SHORT, SID, NAME, "done", None),
        agent("o2", "sid-o2", "demo-other-2222", "done", None),
    ]);
    env.gh.set("pr.stdout", "[]");

    let done = env.with(|ctx| cleanup::gc(ctx, "demo", true)).unwrap();
    assert!(done.warnings.is_empty(), "{:?}", done.warnings);
    let mut dry = done.verdicts;
    dry.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        dry,
        [
            (NAME.to_string(), Verdict::WouldClean),
            (
                "demo-other-2222".to_string(),
                Verdict::Blocked("1 unpushed commit".into())
            ),
        ]
    );
    assert!(env.rm_calls().is_empty());
    assert_eq!(env.row().cleaned_at, None);

    let verdicts = env
        .with(|ctx| cleanup::gc(ctx, "demo", false))
        .unwrap()
        .verdicts;
    assert!(
        verdicts.contains(&(NAME.to_string(), Verdict::Cleaned)),
        "{verdicts:?}"
    );
    assert_eq!(env.rm_calls().len(), 1);
}

/// `chief gc` and `chief clean` through the binary.
#[test]
fn cli_gc_and_clean() {
    let home = Home::new();
    let git = GitRepo::new();
    let wt = git.worktree("cli");
    git.commit(&wt, "c.txt", "Unpushed");
    std::fs::write(wt.join("dirty.txt"), "x").unwrap();
    {
        let conn = home.db();
        db::register_project(&conn, "demo", home.path(), 0).unwrap();
        let mut row = SessionRow::new("demo-cli-1111", "demo", Role::Thread, NOW);
        row.short_id = Some("cli1".into());
        row.session_id = Some("sid-cli1".into());
        row.cwd = Some(wt.to_string_lossy().into_owned());
        row.repo_path = Some(git.repo.to_string_lossy().into_owned());
        row.branch = Some("worktree-cli".into());
        row.resolved = true;
        db::insert_session(&conn, &row).unwrap();
    }
    home.fake
        .job("cli1", Some(&wt))
        .set(
            "agents.stdout",
            &json!([agent("cli1", "sid-cli1", "demo-cli-1111", "done", None)]).to_string(),
        )
        .set("rm.stdout", "removed cli1\n");

    let out = home.run(&["gc", "demo", "--dry-run"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "demo\n  blocked      demo-cli-1111  1 unpushed commit, uncommitted changes: 1 file\n"
    );

    let out = home.run(&["clean", "demo", "demo-cli-1111"], "");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--discard"), "{}", stderr(&out));

    // No terminal to confirm on: refuses, after showing what would be lost.
    let out = home.run(
        &["clean", "demo", "demo-cli-1111", "--discard"],
        "demo-cli-1111\n",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("needs a terminal"),
        "{}",
        stderr(&out)
    );
    let shown = stdout(&out);
    assert!(shown.contains("?? dirty.txt"), "{shown}");
    assert!(shown.contains("1 commit on no remote:"), "{shown}");
    assert!(home.fake.calls_of("rm").is_empty());
    assert!(wt.join("dirty.txt").exists());

    let out = home.run(
        &["clean", "demo", "demo-cli-1111", "--discard", "--yes"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Cleaned up demo-cli-1111."),
        "{}",
        stdout(&out)
    );
    assert!(!wt.join("dirty.txt").exists());
    assert_eq!(
        home.fake.calls_of("rm"),
        [vec!["rm".to_string(), "cli1".to_string()]]
    );

    let out = home.run(&["gc", "demo"], "");
    assert_eq!(stdout(&out), "demo\n  nothing to clean\n");
    let out = home.run(&["status", "demo"], "");
    assert!(stdout(&out).contains("cleaned up"), "{}", stdout(&out));

    let out = home.run(&["clean", "demo", "nope"], "");
    assert!(
        stderr(&out).contains("not a thread of demo"),
        "{}",
        stderr(&out)
    );
}
