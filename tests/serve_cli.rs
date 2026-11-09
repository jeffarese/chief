//! `chief serve` end to end (ARCHITECTURE §14) against the fake `claude`: sign-in, the API, the
//! live board stream, the terminal socket, the routine scheduler, and start/stop.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use chief::config::{ProjectConfig, Repo};
use chief::db::{self, SessionRow};
use chief::state::Role;
use common::{Home, agent, stderr, stdout, trust_file};
use serde_json::{Value, json};

/// A running `chief serve` on a port of its own, ended when dropped.
struct Server {
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A project with one trusted repository; `dashboard` turns its dashboard on.
fn project(home: &Home, slug: &str, dashboard: bool) -> PathBuf {
    let repo = home.path().join(format!("repo-{slug}"));
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let dir = home.path().join("projects").join(slug);
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    let mut config = ProjectConfig::new(
        slug,
        slug,
        Some("A goal"),
        vec![Repo {
            name: "repo".into(),
            path: repo.clone(),
        }],
    );
    config.dashboard.enabled = dashboard;
    config.save(&dir.join("project.toml")).unwrap();
    db::register_project(&home.db(), slug, &dir, 0).unwrap();
    repo
}

fn serve(home: &Home) -> Server {
    let child = home
        .command(&["serve", "--port", "0"])
        .env("CHIEF_SERVE_TICK_MS", "100")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let state = home.path().join("serve.json");
    let port = wait_for("chief serve to listen", || {
        let text = std::fs::read_to_string(&state).ok()?;
        let info: Value = serde_json::from_str(&text).ok()?;
        info["port"].as_u64().map(|p| p as u16)
    });
    let token = std::fs::read_to_string(home.path().join("serve-token")).unwrap();
    Server { child, port, token }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

impl Server {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// One request on a connection of its own.
    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Reply {
        let mut stream = TcpStream::connect(self.host()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut text = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.host()
        );
        for (k, v) in headers {
            text.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            text.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        text.push_str("\r\n");
        text.push_str(&body);
        stream.write_all(text.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // Chunked bodies are small here: drop the chunk sizes.
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body.split("\r\n").skip(1).step_by(2).collect::<String>()
        } else {
            body.to_string()
        };
        Reply {
            status,
            head: head.to_string(),
            body,
        }
    }

    fn cookie(&self) -> String {
        format!("chief_token={}", self.token)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.host())
    }

    /// As the page does it: the cookie, and for a change the page's own origin.
    fn page(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let (cookie, origin) = (self.cookie(), self.origin());
        self.request(
            method,
            path,
            &[("Cookie", &cookie), ("Origin", &origin)],
            body,
        )
    }
}

#[test]
fn the_dashboard_needs_the_token_and_its_own_origin() {
    let home = Home::new();
    project(&home, "demo", true);
    project(&home, "hidden", false);
    let server = serve(&home);

    // Nothing without the token, but the sign-in page and the static files.
    let api = server.request("GET", "/api/projects", &[], None);
    assert_eq!(api.status, 401);
    assert!(
        api.json()["error"]
            .as_str()
            .unwrap()
            .contains("chief dashboard")
    );
    let page = server.request("GET", "/p/demo", &[], None);
    assert_eq!(page.status, 401);
    assert!(page.body.contains("This dashboard needs its link"));
    assert_eq!(
        server.request("GET", "/assets/app.js", &[], None).status,
        200
    );
    assert_eq!(
        server.request("GET", "/assets/nope.js", &[], None).status,
        404
    );
    let wrong = server.request(
        "GET",
        "/api/projects",
        &[("Cookie", "chief_token=wrong")],
        None,
    );
    assert_eq!(wrong.status, 401);

    // The link trades the token for a cookie and goes on, only ever to a path here.
    assert_eq!(
        server.request("GET", "/login?token=nope", &[], None).status,
        401
    );
    let login = server.request(
        "GET",
        &format!("/login?token={}&next=/p/demo", server.token),
        &[],
        None,
    );
    assert_eq!(login.status, 303);
    assert_eq!(login.header("location"), Some("/p/demo"));
    let cookie = login.header("set-cookie").unwrap();
    assert!(cookie.starts_with(&format!("chief_token={}", server.token)));
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let away = server.request(
        "GET",
        &format!("/login?token={}&next=//evil.example", server.token),
        &[],
        None,
    );
    assert_eq!(away.header("location"), Some("/"));

    // Signed in: the page, and only the projects whose dashboard is on.
    let signed = [("Cookie", server.cookie())];
    let signed: Vec<(&str, &str)> = signed.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let page = server.request("GET", "/p/demo", &signed, None);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("/assets/app.js"));
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    let projects = server.request("GET", "/api/projects", &signed, None).json();
    let slugs: Vec<&str> = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["demo"]);
    let hidden = server.request("GET", "/api/p/hidden/board", &signed, None);
    assert_eq!(hidden.status, 404);
    assert!(
        hidden.json()["error"]
            .as_str()
            .unwrap()
            .contains("chief dashboard hidden")
    );
    assert_eq!(
        server
            .request("GET", "/api/p/nope/board", &signed, None)
            .status,
        404
    );
    assert_eq!(
        server
            .request("GET", "/api/p/..%2Fx/board", &signed, None)
            .status,
        404
    );

    // A change with the cookie must come from the dashboard's own page.
    let draft = json!({"title": "Nightly", "prompt": "Check things.", "schedule": "@daily"});
    let bare = server.request("POST", "/api/p/demo/routines", &signed, Some(&draft));
    assert_eq!(bare.status, 403);
    let elsewhere = server.request(
        "POST",
        "/api/p/demo/routines",
        &[
            ("Cookie", &server.cookie()),
            ("Origin", "http://evil.example"),
        ],
        Some(&draft),
    );
    assert_eq!(elsewhere.status, 403);
    assert!(
        chief::routines::list(&home.db(), "demo")
            .unwrap()
            .is_empty()
    );
    let made = server.page("POST", "/api/p/demo/routines", Some(&draft));
    assert_eq!(made.status, 200, "{}", made.body);
    assert_eq!(made.json()["schedule_text"], "Daily at 00:00");
    // A client that writes the token itself isn't a browser being steered.
    let bearer = format!("Bearer {}", server.token);
    let paused = server.request(
        "PATCH",
        "/api/p/demo/routines/nightly",
        &[("Authorization", &bearer)],
        Some(&json!({"enabled": false})),
    );
    assert_eq!(paused.status, 200, "{}", paused.body);
    assert_eq!(paused.json()["enabled"], false);
}

#[test]
fn the_api_shows_the_board_and_runs_its_actions() {
    let home = Home::new();
    project(&home, "demo", true);
    let conn = home.db();
    // Recent: a thread idle for a week is resolved as inactive.
    let mut row = SessionRow::new("demo-fix-it-1a2b", "demo", Role::Thread, db::now_ms());
    row.title = Some("Fix it".into());
    row.task = Some("Fix the thing.".into());
    row.short_id = Some("t1".into());
    row.session_id = Some("sid-t1".into());
    row.routine = Some("nightly".into());
    row.last_report = Some("Done: **fixed**.".into());
    row.last_report_at = Some(2_000);
    db::insert_session(&conn, &row).unwrap();
    db::insert_report(&conn, "demo-fix-it-1a2b", 2_000, "Done: **fixed**.").unwrap();
    home.fake.set(
        "agents.stdout",
        &json!([agent(
            "t1",
            "sid-t1",
            "demo-fix-it-1a2b",
            "done",
            Some(4242)
        )])
        .to_string(),
    );
    let server = serve(&home);

    // The stream publishes the board, synced with `claude agents`.
    let mut stream = TcpStream::connect(server.host()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(
        stream,
        "GET /api/p/demo/stream HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
        server.host(),
        server.cookie()
    )
    .unwrap();
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    let board = wait_for("a board event", || {
        let n = stream.read(&mut buf).ok()?;
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        let at = seen.find("event: board")?;
        let data = seen[at..].split("data: ").nth(1)?;
        let line = data.split('\n').next()?;
        serde_json::from_str::<Value>(line.trim()).ok()
    });
    drop(stream);
    assert_eq!(board["project"]["name"], "demo");
    assert_eq!(board["coordinator"], Value::Null);
    let thread = &board["threads"][0];
    assert_eq!(thread["title"], "Fix it");
    assert_eq!(thread["group"], "idle");
    assert_eq!(thread["routine"], "nightly");
    assert_eq!(thread["asleep"], false);

    let detail = server
        .page("GET", "/api/p/demo/sessions/demo-fix-it-1a2b", None)
        .json();
    assert_eq!(detail["task"], "Fix the thing.");
    assert_eq!(detail["reports"][0]["text"], "Done: **fixed**.");
    let unknown = server.page("GET", "/api/p/demo/sessions/other-project-thread", None);
    assert_eq!(unknown.status, 400);

    let resolved = server.page("POST", "/api/p/demo/threads/demo-fix-it-1a2b/resolve", None);
    assert_eq!(resolved.json()["message"], "Resolved demo-fix-it-1a2b.");
    let row = db::get_session(&conn, "demo-fix-it-1a2b").unwrap().unwrap();
    assert_eq!(
        (row.resolved, row.resolved_reason.as_deref()),
        (true, Some("user"))
    );
    let reopened = server.page("POST", "/api/p/demo/threads/demo-fix-it-1a2b/reopen", None);
    assert_eq!(reopened.status, 200);
    let stopped = server.page("POST", "/api/p/demo/threads/demo-fix-it-1a2b/stop", None);
    assert!(
        stopped.json()["message"]
            .as_str()
            .unwrap()
            .starts_with("Stopped demo-fix-it-1a2b.")
    );
    assert_eq!(home.fake.calls_of("stop"), [["stop", "t1"]]);
    let odd = server.page("POST", "/api/p/demo/threads/demo-fix-it-1a2b/explode", None);
    assert_eq!(odd.status, 400);

    let files = server.page("GET", "/api/p/demo/files", None).json();
    assert_eq!(files["instructions"], Value::Null);

    // A routine with what its run found and what it tracks: on the board in brief, on its own
    // page in full.
    let made = server.page(
        "POST",
        "/api/p/demo/routines",
        Some(&json!({"name": "nightly", "title": "Nightly", "prompt": "Check.", "schedule": "@daily"})),
    );
    assert_eq!(made.status, 200, "{}", made.body);
    let notes = home.path().join("projects/demo/routines/nightly/NOTES.md");
    assert!(
        std::fs::read_to_string(&notes)
            .unwrap()
            .starts_with("# Nightly")
    );
    std::fs::write(&notes, "# Nightly\nWatching the build.\n").unwrap();
    conn.execute_batch(
        "INSERT INTO routine_outcomes (project_slug, routine, thread, ts, outcome, headline)
           VALUES ('demo', 'nightly', 'demo-fix-it-1a2b', 5, 'attention', 'The build broke');
         INSERT INTO routine_points (project_slug, routine, series, ts, value, unit)
           VALUES ('demo', 'nightly', 'Build minutes', 4, 12.5, 'min'),
                  ('demo', 'nightly', 'Build minutes', 5, 14, 'min');",
    )
    .unwrap();
    let board = server.page("GET", "/api/p/demo/board", None).json();
    let routine = &board["routines"][0];
    assert_eq!(routine["last_finding"]["outcome"], "attention");
    assert_eq!(routine["last_finding"]["headline"], "The build broke");
    assert_eq!(routine["series"][0]["latest"], 14.0);
    assert_eq!(routine["series"][0]["previous"], 12.5);
    assert_eq!(board["threads"][0]["finding"]["outcome"], "attention");
    let page = server
        .page("GET", "/api/p/demo/routines/nightly", None)
        .json();
    assert_eq!(page["notes"], "# Nightly\nWatching the build.\n");
    assert_eq!(page["findings"].as_array().unwrap().len(), 1);
    assert_eq!(page["series"][0]["points"], json!([[4, 12.5], [5, 14.0]]));
    assert_eq!(
        server.page("GET", "/api/p/demo/routines/nope", None).status,
        400
    );
}

/// A server-to-client WebSocket frame: its opcode and payload.
fn frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).ok()?;
    let len = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            stream.read_exact(&mut b).ok()?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            stream.read_exact(&mut b).ok()?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).ok()?;
    Some((head[0] & 0x0f, payload))
}

#[test]
fn the_terminal_socket_carries_claude_attach() {
    let home = Home::new();
    project(&home, "demo", true);
    let mut row = SessionRow::new("demo-fix-it-1a2b", "demo", Role::Thread, 1_000);
    row.short_id = Some("t1".into());
    row.session_id = Some("sid-t1".into());
    db::insert_session(&home.db(), &row).unwrap();
    home.fake
        .set(
            "agents.stdout",
            &json!([agent(
                "t1",
                "sid-t1",
                "demo-fix-it-1a2b",
                "done",
                Some(4242)
            )])
            .to_string(),
        )
        .set("attach.stdout", "hello from attach\n")
        .set("attach.sleep", "30");
    let server = serve(&home);

    let upgrade = |origin: &str| {
        let mut stream = TcpStream::connect(server.host()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(
            stream,
            "GET /api/p/demo/term?target=demo-fix-it-1a2b&cols=90&rows=20 HTTP/1.1\r\nHost: {}\r\n\
             Cookie: {}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            server.host(),
            server.cookie()
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
            head.push(byte[0]);
        }
        (stream, String::from_utf8_lossy(&head).into_owned())
    };

    // Another site's page can't open a terminal with the browser's cookie.
    let (_, refused) = upgrade("http://evil.example");
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");

    let (mut stream, head) = upgrade(&server.origin());
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let mut ready = false;
    let mut screen = Vec::new();
    wait_for("attach's output", || {
        let (opcode, payload) = frame(&mut stream)?;
        match opcode {
            1 => {
                let message: Value = serde_json::from_slice(&payload).ok()?;
                assert_ne!(message["type"], "error", "{message}");
                if message["type"] == "ready" {
                    assert_eq!(message["name"], "demo-fix-it-1a2b");
                    ready = true;
                }
            }
            2 => screen.extend_from_slice(&payload),
            _ => {}
        }
        String::from_utf8_lossy(&screen)
            .contains("hello from attach")
            .then_some(())
    });
    assert!(ready);
    let attach = home.fake.calls_of("attach");
    assert_eq!(attach, [["attach", "t1"]]);
    // Leaving ends attach, not the session: nothing stopped it.
    drop(stream);
    assert!(home.fake.calls_of("stop").is_empty());
}

#[test]
fn a_due_routine_fires_while_the_server_runs() {
    let home = Home::new();
    let repo = project(&home, "demo", false);
    trust_file(&home.trust, &[&repo]);
    home.fake
        .set("agents.stdout", "[]")
        .set("bg.stdout", "backgrounded · r1 · x\n");
    let conn = home.db();

    let added = home.run(
        &[
            "routine",
            "add",
            "demo",
            "--title",
            "Nightly check",
            "--schedule",
            "every 30m",
            "--prompt",
            "Check things.",
        ],
        "",
    );
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(
        stdout(&added)
            .contains("Added nightly-check  \"Nightly check\" · Every 30 minutes · next ")
    );
    assert!(stdout(&added).contains("chief serve isn't running"));

    // Nothing runs yet, so nothing fires, however due it is.
    conn.execute("UPDATE routines SET next_run_at = 1", [])
        .unwrap();
    let status = home.run(&["serve", "status"], "");
    assert!(stdout(&status).contains("chief serve isn't running"));
    assert!(stdout(&status).contains("demo: 1 of 1 routines enabled"));

    let server = serve(&home);
    let thread = wait_for("the routine's run", || {
        chief::routines::get(&conn, "demo", "nightly-check")
            .ok()??
            .last_thread
    });
    let row = db::get_session(&conn, &thread).unwrap().unwrap();
    assert_eq!(row.routine.as_deref(), Some("nightly-check"));
    assert_eq!(row.short_id.as_deref(), Some("r1"));
    // Once: the next run is half an hour away.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(home.fake.calls_of("--bg").len(), 1);

    // The run finishes while the coordinator sleeps: the server wakes it for a run that needs
    // the user, once per report, and its own start-up hook's watcher then tells it (§13 "Who
    // hears of a run").
    let mut coordinator = SessionRow::new("chief-demo", "demo", Role::Coordinator, db::now_ms());
    coordinator.short_id = Some("c1".into());
    coordinator.session_id = Some("sid-c1".into());
    db::insert_session(&conn, &coordinator).unwrap();
    assert!(home.fake.calls_of("respawn").is_empty());
    db::immediate(&conn, |conn| {
        let mut run = db::get_session(conn, &thread)?.unwrap();
        run.group_name = "idle".into();
        run.last_hook = Some("Stop".into());
        run.last_report = Some("Two things need you.".into());
        run.last_report_at = Some(db::now_ms() - 60_000);
        db::update_session(conn, &run)
    })
    .unwrap();
    // News that can wait wakes nobody: the coordinator hears on its next turn.
    conn.execute(
        "INSERT INTO routine_outcomes (project_slug, routine, thread, ts, outcome, headline)
         VALUES ('demo', 'nightly-check', ?1, 1, 'update', 'One thing changed')",
        [&thread],
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert!(home.fake.calls_of("respawn").is_empty());
    // A run that needs the user does.
    conn.execute(
        "INSERT INTO routine_outcomes (project_slug, routine, thread, ts, outcome, headline)
         VALUES ('demo', 'nightly-check', ?1, 2, 'attention', 'Two things need you')",
        [&thread],
    )
    .unwrap();
    wait_for("the coordinator's respawn", || {
        (!home.fake.calls_of("respawn").is_empty()).then_some(())
    });
    // The respawn waits 5 s for a process the fake never shows; later looks leave it alone.
    std::thread::sleep(Duration::from_millis(6500));
    assert_eq!(home.fake.calls_of("respawn"), [["respawn", "c1"]]);

    let listed = home.run(&["routine", "list", "demo"], "");
    assert!(stdout(&listed).contains(&format!("· {thread}")));
    assert!(!stdout(&listed).contains("chief serve isn't running"));
    let status = home.run(&["status", "demo"], "");
    assert!(
        stdout(&status).contains("routine nightly-check"),
        "{}",
        stdout(&status)
    );

    // One server per chief home; `stop` ends it and it leaves nothing behind.
    let second = home.run(&["serve", "--port", "0"], "");
    assert!(!second.status.success());
    assert!(
        stderr(&second).contains("already runs"),
        "{}",
        stderr(&second)
    );
    let running = home.run(&["serve", "status"], "");
    assert!(stdout(&running).contains(&format!("at http://127.0.0.1:{}", server.port)));
    let stopped = home.run(&["serve", "stop"], "");
    assert!(
        stdout(&stopped).contains("Stopped chief serve."),
        "{}",
        stdout(&stopped)
    );
    assert!(!home.path().join("serve.json").exists());
    let again = home.run(&["serve", "stop"], "");
    assert!(stdout(&again).contains("isn't running"));
}

#[test]
fn chief_dashboard_turns_it_on_and_prints_the_link() {
    let home = Home::new();
    project(&home, "demo", false);
    // Any free port: the default may be taken by the developer's own server.
    std::fs::write(home.path().join("serve.toml"), "port = 0\n").unwrap();
    let on = home.run(&["dashboard", "demo", "--no-open"], "");
    assert!(on.status.success(), "{}", stderr(&on));
    let out = stdout(&on);
    let token = std::fs::read_to_string(home.path().join("serve-token")).unwrap();
    assert!(
        out.contains(&format!("/login?token={token}&next=/p/demo")),
        "{out}"
    );
    let config = ProjectConfig::load(&home.path().join("projects/demo/project.toml")).unwrap();
    assert!(config.dashboard.enabled);
    // It started the server: stop it.
    let stopped = home.run(&["serve", "stop"], "");
    assert!(
        stdout(&stopped).contains("Stopped chief serve."),
        "{}",
        stdout(&stopped)
    );

    let off = home.run(&["dashboard", "demo", "--off"], "");
    assert!(stdout(&off).contains("is off"));
    let config = ProjectConfig::load(&home.path().join("projects/demo/project.toml")).unwrap();
    assert!(!config.dashboard.enabled);
    assert!(!home.run(&["dashboard", "nope"], "").status.success());
}
