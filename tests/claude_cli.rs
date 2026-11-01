//! `chief::claude` against `tests/fake-claude.sh`; no real Claude session is started.

mod common;

use std::path::Path;
use std::time::Duration;

use chief::claude::{BgSpawn, Claude, ClaudeError};
use common::Fake;

fn spec(cwd: &Path) -> BgSpawn {
    BgSpawn {
        cwd: cwd.to_path_buf(),
        name: "demo-fix-it-1a2b".into(),
        model: "sonnet".into(),
        effort: "high".into(),
        permission_mode: "auto".into(),
        settings: "/root/projects/demo/generated/thread-settings.json".into(),
        mcp_config: None,
        add_dirs: vec!["/root/projects/demo/memory".into()],
        disallowed_tools: vec![],
        append_system_prompt: "You are one thread.\nReport at the end.".into(),
        prompt: Some("Fix it".into()),
    }
}

const AGENTS: &str = include_str!("fixtures/agents/agents-sample.json");

#[test]
fn spawn_bg_runs_in_cwd_with_the_contract_args() {
    let fake = Fake::new();
    fake.set("bg.stdout", "backgrounded · f8627ea8 · demo-fix-it-1a2b\n");
    let repo = tempfile::tempdir().unwrap();
    let id = fake.claude().spawn_bg(&spec(repo.path())).unwrap();
    assert_eq!(id, "f8627ea8");
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        [
            "--bg",
            "--name",
            "demo-fix-it-1a2b",
            "--model",
            "sonnet",
            "--effort",
            "high",
            "--permission-mode",
            "auto",
            "--settings",
            "/root/projects/demo/generated/thread-settings.json",
            "--add-dir",
            "/root/projects/demo/memory",
            "--append-system-prompt",
            "You are one thread.\nReport at the end.",
            "Fix it",
        ]
    );
    assert_eq!(fake.cwd(0), repo.path().canonicalize().unwrap());
}

#[test]
fn spawn_bg_reports_untrusted_folders() {
    let fake = Fake::new();
    fake.set(
        "bg.stderr",
        "Error: Workspace not trusted. Run claude here first.\n",
    )
    .set("bg.exit", "1");
    let repo = tempfile::tempdir().unwrap();
    let err = fake.claude().spawn_bg(&spec(repo.path())).unwrap_err();
    let typed = err
        .downcast_ref::<ClaudeError>()
        .expect("a NotTrusted error");
    assert!(matches!(typed, ClaudeError::NotTrusted(p) if p == repo.path()));
    let msg = err.to_string();
    assert!(msg.contains(&repo.path().display().to_string()), "{msg}");
    assert!(msg.contains("accept the trust prompt"), "{msg}");
}

#[test]
fn spawn_bg_without_an_id_includes_stderr() {
    let fake = Fake::new();
    fake.set("bg.stdout", "something unexpected\n")
        .set("bg.stderr", "rate limited\n")
        .set("bg.exit", "2");
    let repo = tempfile::tempdir().unwrap();
    let msg = format!(
        "{:#}",
        fake.claude().spawn_bg(&spec(repo.path())).unwrap_err()
    );
    assert!(msg.contains("rate limited"), "{msg}");
    assert!(msg.contains("something unexpected"), "{msg}");
}

#[test]
fn agents_and_control_commands() {
    let fake = Fake::new();
    fake.set("agents.stdout", AGENTS);
    let claude = fake.claude();
    let rows = claude.agents().unwrap();
    assert_eq!(rows.len(), 5);
    claude.respawn("f8627ea8").unwrap();
    claude.stop("bf6db904").unwrap();
    assert_eq!(
        fake.calls(),
        [
            vec!["agents", "--json", "--all"],
            vec!["respawn", "f8627ea8"],
            vec!["stop", "bf6db904"],
        ]
    );

    fake.set("stop.exit", "1")
        .set("stop.stderr", "no such session\n");
    let msg = claude.stop("nope").unwrap_err().to_string();
    assert!(msg.contains("no such session"), "{msg}");
}

#[test]
fn agents_failure_and_bad_json() {
    let fake = Fake::new();
    fake.set("agents.stdout", "not json");
    assert!(fake.claude().agents().is_err());
    fake.set("agents.exit", "1")
        .set("agents.stderr", "supervisor down");
    let msg = fake.claude().agents().unwrap_err().to_string();
    assert!(msg.contains("supervisor down"), "{msg}");
}

#[test]
fn resolve_session_retries_until_listed() {
    let fake = Fake::new();
    fake.set("agents.stdout.1", "[]")
        .set("agents.stdout.2", "[]")
        .set("agents.stdout", AGENTS);
    let row = fake
        .claude()
        .resolve_session("bf6db904", Duration::from_secs(5))
        .unwrap();
    assert_eq!(row.session_id, "bf6db904-3919-4adc-956d-c30266432600");
    assert_eq!(fake.calls().len(), 3);
}

#[test]
fn resolve_session_gives_up() {
    let fake = Fake::new();
    fake.set("agents.stdout", "[]");
    let msg = fake
        .claude()
        .resolve_session("abc", Duration::from_millis(300))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("abc"), "{msg}");
}

#[test]
fn find_started_looks_for_a_new_background_session_by_exact_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    // As `claude agents` lists it: the canonical folder (macOS temp dirs are under /private).
    let listed = repo.canonicalize().unwrap();
    let row = |id: Option<&str>, sid: &str, name: &str, cwd: &Path| {
        let mut v = serde_json::json!({"sessionId": sid, "name": name, "cwd": cwd});
        if let Some(id) = id {
            v["id"] = id.into();
        }
        v
    };
    let fake = Fake::new();
    let before = chief::claude::parse_agents(
        &serde_json::json!([row(Some("01d"), "old", "demo-x-1111", &listed)]).to_string(),
    )
    .unwrap();
    let elsewhere = tmp.path().to_path_buf();
    let new = row(Some("n3w"), "new", "demo-x-1111", &listed);
    let rows = [
        row(Some("01d"), "old", "demo-x-1111", &listed),
        row(None, "interactive", "demo-x-1111", &listed),
        row(Some("n0pe"), "lookalike", "demo-x-1111 ", &listed),
        // The same name in another folder: another chief home's, or the user's own session.
        row(Some("0th3r"), "other", "demo-x-1111", &elsewhere),
    ];
    let mut all = rows.to_vec();
    all.push(new);
    fake.set("agents.stdout", &serde_json::json!(all).to_string());
    let claude = fake.claude();
    // Asked for with the path as given; matched canonically.
    let found = claude
        .find_started("demo-x-1111", &before, &repo, Duration::from_millis(100))
        .unwrap()
        .unwrap();
    assert_eq!(found.session_id, "new");

    // Only the one in another folder: none.
    fake.set("agents.stdout", &serde_json::json!(rows).to_string());
    assert_eq!(
        claude
            .find_started("demo-x-1111", &before, &repo, Duration::from_millis(100))
            .unwrap(),
        None
    );

    // Listed, but not there: none.
    fake.set("agents.stdout", "[]");
    assert_eq!(
        claude
            .find_started("demo-x-1111", &before, &repo, Duration::from_millis(100))
            .unwrap(),
        None
    );
    // Never listed: chief can't tell.
    fake.set("agents.exit", "1").set("agents.stderr", "down");
    let err = claude
        .find_started("demo-x-1111", &before, &repo, Duration::from_millis(100))
        .unwrap_err();
    assert!(err.to_string().contains("down"), "{err}");
}

#[test]
fn version_check() {
    let fake = Fake::new();
    fake.set("version.stdout", "2.1.316 (Claude Code)\n");
    fake.claude().check_version().unwrap();
    fake.set("version.stdout", "2.2.0 (Claude Code)\n");
    fake.claude().check_version().unwrap();
    fake.set("version.stdout", "2.1.315 (Claude Code)\n");
    let msg = fake.claude().check_version().unwrap_err().to_string();
    assert!(msg.contains("2.1.315") && msg.contains("2.1.316"), "{msg}");
    fake.set("version.stdout", "garbage\n");
    assert!(fake.claude().check_version().is_err());
    assert_eq!(fake.calls()[0], ["--version"]);
}

#[test]
fn missing_binary_is_an_error() {
    let claude = Claude::new("/nonexistent/claude".into());
    assert!(claude.agents().is_err());
}
