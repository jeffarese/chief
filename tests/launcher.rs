//! The launcher's create step (`chief::launcher::create`, §10a) in this process, against the
//! fake `claude`. It reads chief's environment, so this file holds one test: a test binary of its
//! own, with nothing else setting the variables.

mod common;

use chief::config::ProjectConfig;
use chief::launcher::app::Msg;
use chief::launcher::wizard::Draft;
use chief::project::{Note, Where};
use common::{Home, agent};
use serde_json::json;

#[test]
fn the_wizard_creates_trusts_and_opens_the_project() {
    let home = Home::new();
    // SAFETY: the only test in this binary, before any thread of chief's starts.
    unsafe {
        for (k, v) in [
            ("CHIEF_HOME", home.path().to_path_buf()),
            ("CHIEF_CLAUDE_BIN", home.fake.bin()),
            ("CHIEF_GH_BIN", home.gh.bin()),
            ("CHIEF_REX_BIN", home.rex.bin()),
            ("CHIEF_TMUX_BIN", home.tmux.bin()),
            ("CHIEF_AGP_BIN", home.agp.bin()),
            ("CHIEF_CLAUDE_CONFIG", home.trust.clone()),
            ("CHIEF_CLAUDE_DIR", home.fake.config_dir()),
            ("CHIEF_NO_NOTIFY", "1".into()),
        ] {
            std::env::set_var(k, v);
        }
        for k in ["CLAUDE_CONFIG_DIR", "TMUX", "TMUX_PANE"] {
            std::env::remove_var(k);
        }
    }
    let api = home.path().join("work/api");
    let web = home.path().join("work/web");
    std::fs::create_dir_all(&api).unwrap();
    std::fs::create_dir_all(&web).unwrap();
    let (api, web) = (api.canonicalize().unwrap(), web.canonicalize().unwrap());
    common::trust_file(&home.trust, &[&api]);
    let name = "chief-checkout";
    home.fake
        .set("version.stdout", "2.1.310 (Claude Code)\n")
        .set("agents.stdout.1", "[]")
        .set("bg.stdout", &format!("backgrounded · c0ffee · {name}\n"))
        .set(
            "agents.stdout",
            &json!([agent("c0ffee", "coord-sid", name, "done", Some(74112))]).to_string(),
        );

    let draft = Draft {
        name: "Checkout".into(),
        goal: Some("Cut p95 in half".into()),
        repos: vec![api.clone(), web.clone()],
        coordinator_model: "sonnet".into(),
        coordinator_effort: "medium".into(),
        thread_model: "haiku".into(),
        thread_effort: "low".into(),
        trust: true,
    };
    let mut msgs = Vec::new();
    let shown = chief::launcher::create(&draft, &mut |m| msgs.push(m)).unwrap();
    // No Rex server and not inside tmux: the launcher shows the pane itself.
    assert_eq!(shown, Where::Manual);
    assert_eq!(msgs[0], Msg::Created("checkout".into()));
    let did: Vec<String> = msgs
        .iter()
        .filter_map(|m| match m {
            Msg::Note(Note::Did(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let dir = home
        .path()
        .join("projects/checkout")
        .canonicalize()
        .unwrap();
    for want in [
        "Created project Checkout (checkout).".to_string(),
        format!("Trusted {}.", dir.display()),
        format!("Trusted {}.", web.display()),
        format!("Coordinator {name} started."),
    ] {
        assert!(did.contains(&want), "missing {want:?} in {did:?}");
    }
    // Not a step that worked: an aside, so the launcher doesn't tick it.
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Msg::Note(Note::Info(t)) if t.starts_with("No layout"))),
        "{msgs:?}"
    );
    for folder in [&dir, &api, &web] {
        assert!(chief::claude::is_trusted_in(&home.trust, folder).unwrap());
    }

    // The quick settings are the project's, and the coordinator started with them.
    let config = ProjectConfig::load(&dir.join("project.toml")).unwrap();
    assert_eq!(config.goal.as_deref(), Some("Cut p95 in half"));
    assert_eq!(
        (
            config.coordinator.model.as_str(),
            config.coordinator.effort.as_str()
        ),
        ("sonnet", "medium")
    );
    assert_eq!(
        (
            config.threads.model.as_str(),
            config.threads.effort.as_str()
        ),
        ("haiku", "low")
    );
    let bg = home.fake.calls_of("--bg");
    assert_eq!(bg.len(), 1);
    let at = bg[0].iter().position(|a| a == "--model").unwrap();
    assert_eq!(bg[0][at + 1..at + 4], ["sonnet", "--effort", "medium"]);

    // The same name again is refused before anything is written.
    let mut msgs = Vec::new();
    let err = chief::launcher::create(&draft, &mut |m| msgs.push(m)).unwrap_err();
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    assert!(msgs.is_empty(), "{msgs:?}");
}
