//! A session's real screen in the browser (ARCHITECTURE §14 "The conversation"): `claude
//! attach` on a pseudo-terminal of its own, its bytes carried both ways over a WebSocket to an
//! emulator in the page. What `chief pane` does for a terminal, for a browser tab.
//!
//! Frames: binary is terminal data, either way. Text is a JSON control message: from the page
//! `{"type":"resize","cols":…,"rows":…}`; to the page `{"type":"status","text":…}` while the
//! session is brought up, `{"type":"ready","name":…}` once attach runs, `{"type":"exit"}` when
//! attach ended, and `{"type":"error","text":…}`.

use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;

use super::http::in_project;
use crate::embed::{Output, Pty};
use crate::state::Role;
use crate::{db, generated, pane, project};

/// What the page hears while the session is brought up.
enum Step {
    Say(String),
    Ready { name: String, command: Command },
    Failed(String),
}

/// Brings `target` (a thread's name, or the coordinator for none) to a state `claude attach`
/// can show: the coordinator is started or revived as `chief up` would, a thread is woken when
/// it sleeps, as the pane does before it attaches (§9).
fn prepare(
    slug: &str,
    target: Option<&str>,
    say: &mut dyn FnMut(String),
) -> Result<(String, Command)> {
    in_project(slug, |ctx| {
        let row = match target {
            None => {
                say("Starting the coordinator…".to_string());
                // From the settings as they are now: the session reads its hooks at start.
                generated::rewrite(ctx.project_dir, ctx.config)?;
                let mut warn = |w: String| say(w);
                let (row, _) = project::ensure_coordinator(
                    ctx.conn,
                    ctx.claude,
                    ctx.agp,
                    ctx.config,
                    ctx.project_dir,
                    ctx.trust_config,
                    &mut warn,
                )?;
                row
            }
            Some(name) => {
                let row = db::get_session(ctx.conn, name)?
                    .filter(|r| r.project_slug == ctx.config.slug)
                    .with_context(|| format!("no session named {name} in this project"))?;
                if row.removed_at.is_some() {
                    bail!(crate::guard::removed_message(name));
                }
                if row.cleaned_at.is_some() {
                    bail!(crate::guard::cleaned_message(name));
                }
                if row.short_id.is_none() {
                    bail!("{name} hasn't started yet");
                }
                if row.role == Role::Thread.as_str() {
                    generated::rewrite(ctx.project_dir, ctx.config)?;
                }
                let mut note = |line: &str| say(line.trim_start_matches("chief · ").to_string());
                pane::wake(
                    ctx.conn,
                    ctx.claude,
                    (ctx.project_dir, ctx.config),
                    &row,
                    &mut note,
                )?;
                row
            }
        };
        let short_id = row
            .short_id
            .as_deref()
            .context("the session has no id yet")?;
        // In the session's own account: no other account's supervisor knows it (§3).
        let mut command = ctx.claude.for_row(&row).command();
        command.args(["attach", short_id]);
        Ok((row.name.clone(), command))
    })
}

fn control(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

pub async fn run(
    socket: WebSocket,
    slug: String,
    target: Option<String>,
    (cols, rows): (u16, u16),
) {
    let (mut to_page, mut from_page) = socket.split();

    // Waking a session takes seconds: say what happens as it happens.
    let (steps, mut step) = unbounded_channel::<Step>();
    tokio::task::spawn_blocking(move || {
        let said = steps.clone();
        let mut say = move |text: String| {
            let _ = said.send(Step::Say(text));
        };
        let _ = steps.send(match prepare(&slug, target.as_deref(), &mut say) {
            Ok((name, command)) => Step::Ready { name, command },
            Err(e) => Step::Failed(format!("{e:#}")),
        });
    });
    let (name, command) = loop {
        match step.recv().await {
            Some(Step::Say(text)) => {
                if to_page
                    .send(control(json!({"type": "status", "text": text})))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(Step::Ready { name, command }) => break (name, command),
            Some(Step::Failed(text)) => {
                let _ = to_page
                    .send(control(json!({"type": "error", "text": text})))
                    .await;
                return;
            }
            None => return,
        }
    };

    // The pty's reader thread speaks std channels; one more thread hands them to the runtime.
    let (out_tx, out_rx) = mpsc::channel::<Output>();
    let pty = match Pty::spawn(command, cols, rows, out_tx, |output| output) {
        Ok(pty) => pty,
        Err(e) => {
            let text = format!("couldn't run claude attach: {e}");
            let _ = to_page
                .send(control(json!({"type": "error", "text": text})))
                .await;
            return;
        }
    };
    let (output, mut screen) = unbounded_channel::<Output>();
    std::thread::spawn(move || {
        for chunk in out_rx {
            if output.send(chunk).is_err() {
                break;
            }
        }
    });
    if to_page
        .send(control(json!({"type": "ready", "name": name})))
        .await
        .is_ok()
    {
        loop {
            tokio::select! {
                chunk = screen.recv() => match chunk {
                    Some(Output::Bytes(bytes)) => {
                        if to_page.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }
                    // attach ended: the user detached, or the session went away.
                    Some(Output::Exited) | None => {
                        let _ = to_page.send(control(json!({"type": "exit"}))).await;
                        break;
                    }
                },
                message = from_page.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => pty.write(&bytes),
                    Some(Ok(Message::Text(text))) => {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if value["type"] == "resize" {
                            let cols = value["cols"].as_u64().unwrap_or(0).clamp(20, 500) as u16;
                            let rows = value["rows"].as_u64().unwrap_or(0).clamp(5, 300) as u16;
                            pty.resize(cols, rows);
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    // Ends attach only (SIGTERM, a grace period, SIGKILL); its session keeps running.
    let _ = tokio::task::spawn_blocking(move || pty.end()).await;
}
