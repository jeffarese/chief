//! `Mux` over the `rex` CLI. Use `--json` outputs; never touch sessions chief didn't create.
//! `CHIEF_REX_BIN` overrides the binary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use super::{BlockInfo, BlockRef, BlockSpec, Mux, SessionRef, Split};

const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Rex {
    pub bin: PathBuf,
}

#[derive(Deserialize)]
struct Sessions {
    sessions: Vec<SessionEntry>,
}

#[derive(Deserialize)]
struct SessionEntry {
    session_id: String,
}

#[derive(Deserialize)]
struct Created {
    session_id: String,
    initial_windows: Vec<Window>,
}

#[derive(Deserialize)]
struct Window {
    block_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Blocks {
    blocks: Vec<BlockEntry>,
}

#[derive(Deserialize)]
struct BlockEntry {
    block_id: String,
    label: Option<String>,
}

impl Rex {
    pub fn from_env() -> Self {
        let bin = std::env::var_os("CHIEF_REX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rex"));
        Rex { bin }
    }

    /// Runs `rex <args>`; a non-zero exit is an error carrying stderr. `--autostart=false`
    /// keeps chief from starting a Rex server nobody asked for: no server means no layout.
    fn run(&self, args: &[OsString]) -> Result<Output> {
        // Before the first option, so it never lands after `--` in the block's command.
        let at = args
            .iter()
            .position(|a| a.to_string_lossy().starts_with('-'))
            .unwrap_or(args.len());
        let words = args[..at]
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = Command::new(&self.bin);
        cmd.args(&args[..at])
            .arg("--autostart=false")
            .args(&args[at..]);
        let out = match crate::proc::output_with_timeout(&mut cmd, TIMEOUT) {
            Ok(Some(out)) => out,
            Ok(None) => bail!("`rex {words}` took longer than {}s", TIMEOUT.as_secs()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("rex isn't installed ({} not found)", self.bin.display())
            }
            Err(e) => return Err(e).with_context(|| format!("running `rex {words}`")),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let why = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!("`rex {words}` failed ({}): {why}", out.status);
        }
        Ok(out)
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, args: &[OsString]) -> Result<T> {
        let out = self.run(args)?;
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(&text).with_context(|| format!("reading rex output {:?}", text.trim()))
    }
}

/// `rex new` and `rex run` options shared by both, then `-- <command>`.
fn block_args(spec: &BlockSpec, args: &mut Vec<OsString>) {
    if let Some(cwd) = &spec.cwd {
        args.push("--cwd".into());
        args.push(cwd.into());
    }
    // Kept open so a crash message stays readable; `chief up` replaces a block whose program
    // ended.
    args.push("--keep-open".into());
    args.push("--json".into());
    args.push("--".into());
    args.extend(spec.command.iter().map(OsString::from));
}

fn split_name(split: Split) -> &'static str {
    match split {
        Split::Left => "left",
        Split::Right => "right",
        Split::Above => "above",
        Split::Below => "below",
    }
}

impl Mux for Rex {
    fn has_session(&self, session: &SessionRef) -> Result<bool> {
        let list: Sessions = self.json(&["list".into(), "--json".into()])?;
        Ok(list.sessions.iter().any(|s| s.session_id == session.0))
    }

    fn create_session(&self, label: &str, first: &BlockSpec) -> Result<(SessionRef, BlockRef)> {
        let mut args: Vec<OsString> = vec!["new".into(), label.into()];
        block_args(first, &mut args);
        let created: Created = self.json(&args)?;
        let session = SessionRef(created.session_id);
        let block = created
            .initial_windows
            .first()
            .and_then(|w| w.block_ids.first())
            .map(|id| BlockRef(id.clone()))
            .with_context(|| format!("`rex new {label}` reported no block"))?;
        // `rex new` has no block label option; the first block starts as "shell".
        self.run(&[
            "block".into(),
            "rename".into(),
            "-s".into(),
            (&session.0).into(),
            (&block.0).into(),
            (&first.label).into(),
        ])?;
        Ok((session, block))
    }

    fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef> {
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "-s".into(),
            (&session.0).into(),
            "--label".into(),
            (&spec.label).into(),
            format!("--focus={}", spec.focus).into(),
        ];
        if let Some((anchor, split)) = &spec.anchor {
            args.extend([
                "-b".into(),
                (&anchor.0).into(),
                "--split".into(),
                split_name(*split).into(),
                "--ratio".into(),
                spec.ratio.to_string().into(),
            ]);
        }
        block_args(spec, &mut args);
        let out: Value = self.json(&args)?;
        out["block_ids"][0]
            .as_str()
            .map(|id| BlockRef(id.to_string()))
            .with_context(|| format!("`rex run` reported no block: {out}"))
    }

    fn blocks(&self, session: &SessionRef) -> Result<Vec<BlockInfo>> {
        let list: Blocks = self.json(&[
            "block".into(),
            "ls".into(),
            "-s".into(),
            (&session.0).into(),
            "--json".into(),
        ])?;
        Ok(list
            .blocks
            .into_iter()
            .map(|b| BlockInfo {
                block: BlockRef(b.block_id),
                label: b.label.unwrap_or_default(),
            })
            .collect())
    }

    fn is_running(&self, block: &BlockRef) -> Result<bool> {
        // `process` lists `child` only while the program runs.
        let out: Value = self.json(&[
            "block".into(),
            "call".into(),
            "-b".into(),
            (&block.0).into(),
            "process".into(),
        ])?;
        Ok(out.get("child").is_some_and(|c| !c.is_null()))
    }

    fn close_block(&self, block: &BlockRef) -> Result<()> {
        self.run(&["block".into(), "close".into(), (&block.0).into()])?;
        Ok(())
    }

    fn send_key(&self, block: &BlockRef, key: &str) -> Result<()> {
        self.run(&[
            "send-key".into(),
            "-b".into(),
            (&block.0).into(),
            key.into(),
        ])?;
        Ok(())
    }

    fn focus(&self, block: &BlockRef) -> Result<()> {
        self.run(&["focus".into(), (&block.0).into()])?;
        Ok(())
    }

    fn show(&self, session: &SessionRef) -> Result<bool> {
        // A client action: Rex picks the app when exactly one is connected, and refuses
        // otherwise.
        Ok(self
            .run(&[
                "do".into(),
                "session.select".into(),
                format!("session_id={}", session.0).into(),
            ])
            .is_ok())
    }

    fn kill_session(&self, session: &SessionRef) -> Result<()> {
        self.run(&["kill".into(), (&session.0).into()])?;
        Ok(())
    }
}
