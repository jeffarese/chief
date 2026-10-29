//! `chief mcp` over stdio: the rmcp adapter lists the five tools and returns JSON text.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use common::Home;
use serde_json::{Value, json};

struct Client {
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Client {
    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// The response to request `id`, skipping notifications.
    fn response(&self, id: i64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("no response from chief mcp");
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("not JSON on stdout: {line}"));
            if v["id"] == id {
                return v;
            }
        }
    }

    fn call(&mut self, id: i64, tool: &str, args: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}}));
        self.response(id)["result"].clone()
    }
}

/// Reads the server's stdout and completes the MCP handshake.
fn connect(child: &mut Child) -> Client {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut client = Client {
        stdin: child.stdin.take().unwrap(),
        lines: rx,
    };
    client.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        }}),
    );
    let init = client.response(1);
    assert_eq!(init["result"]["serverInfo"]["name"], "chief", "{init}");
    client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    client
}

/// The tools ask the project's account, whatever account the server's environment names.
#[test]
fn tools_run_in_the_projects_account() {
    let home = Home::new();
    home.fake
        .set("version.stdout", "2.1.310 (Claude Code)\n")
        .set("agents.stdout", "[]");
    let carlos = common::account(home.path(), "claude-carlos");
    let mut init = home.command(&["init", "Demo"]);
    init.env("CLAUDE_CONFIG_DIR", &carlos);
    assert!(init.output().unwrap().status.success());
    std::fs::write(carlos.join("fake-agents.stdout"), "[]").unwrap();
    let before = home.fake.accounts().len();

    let mut child = home.command(&["mcp", "--project", "demo"]).spawn().unwrap();
    let mut client = connect(&mut child);
    let result = client.call(2, "list_threads", Value::Null);
    assert_eq!(result["isError"], false, "{result}");
    drop(client);
    assert!(child.wait().unwrap().success());
    let asked = home.fake.accounts()[before..].to_vec();
    assert_eq!(
        asked,
        [("agents".to_string(), carlos.to_string_lossy().into_owned())]
    );
}

#[test]
fn serves_the_tools_over_stdio() {
    let home = Home::new();
    home.fake
        .set("version.stdout", "2.1.310 (Claude Code)\n")
        .set("agents.stdout", "[]");
    assert!(home.run(&["init", "Demo"], "").status.success());

    let mut child = home.command(&["mcp", "--project", "demo"]).spawn().unwrap();
    let mut client = connect(&mut child);

    client.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = client.response(2);
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "list_threads",
            "read_report",
            "resolve_thread",
            "start_thread",
            "wake_thread"
        ]
    );
    let start = tools.iter().find(|t| t["name"] == "start_thread").unwrap();
    let mut required: Vec<_> = start["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    required.sort();
    assert_eq!(required, ["task", "title"]);
    assert!(start["inputSchema"]["properties"]["repo"].is_object());

    // No arguments at all: the defaults apply.
    let result = client.call(3, "list_threads", Value::Null);
    assert_eq!(result["isError"], false, "{result}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, json!({"threads": []}));

    // Errors come back as tool errors with a plain message.
    let result = client.call(4, "read_report", json!({"name": "nope"}));
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a thread of this project")
    );

    drop(client);
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "chief mcp exits cleanly when stdin closes: {status}"
    );
}

/// A `start_thread` the client cancels while it runs starts no session: the tool checks rmcp's
/// cancellation right before the spawn (§7).
#[test]
fn a_cancelled_start_thread_spawns_nothing() {
    let home = Home::new();
    home.fake
        .set("version.stdout", "2.1.310 (Claude Code)\n")
        .set("agents.stdout", "[]")
        .set("bg.stdout", "backgrounded · abc123 · x\n");
    assert!(home.run(&["init", "Demo"], "").status.success());
    let repo = home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let toml = home.path().join("projects/demo/project.toml");
    let mut config = chief::config::ProjectConfig::load(&toml).unwrap();
    config.repos = vec![chief::config::Repo {
        name: "repo".into(),
        path: repo.clone(),
    }];
    config.save(&toml).unwrap();
    common::trust_file(&home.trust, &[&repo]);
    // Each listing now takes a second: time to cancel before the spawn.
    home.fake.set("agents.sleep", "1");

    let mut child = home.command(&["mcp", "--project", "demo"]).spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, errors) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut client = connect(&mut child);
    client.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "start_thread",
            "arguments": {"title": "Fix", "task": "Fix it", "repo": "repo"}}}));
    client.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "test"}}),
    );
    let mut seen = Vec::new();
    loop {
        let line = errors
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("chief mcp never said the start was cancelled: {seen:?}"));
        if line.contains("was cancelled") {
            break;
        }
        seen.push(line);
    }
    assert!(home.fake.calls_of("--bg").is_empty());
    let threads = chief::db::list_sessions(&home.db(), "demo", Some(chief::state::Role::Thread));
    assert!(threads.unwrap().is_empty());
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn unknown_project_fails_fast() {
    let home = Home::new();
    let out = home.run(&["mcp", "--project", "nope"], "");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}
