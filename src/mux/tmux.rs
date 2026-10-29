//! `Mux` over the `tmux` CLI (ARCHITECTURE §9). A chief "session" is one tmux window and a block
//! is one of its panes, labelled with the pane option `@chief-block`. Every command names the
//! server by socket (`-S`), so chief never talks to a server it wasn't asked about.
//! `CHIEF_TMUX_BIN` overrides the binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{BlockInfo, BlockRef, BlockSpec, Mux, SessionRef, Split, kill_on_error};

const TIMEOUT: Duration = Duration::from_secs(15);
/// The pane option holding a block's label. Programs can retitle a pane; they can't set this.
const LABEL: &str = "@chief-block";

pub struct Tmux {
    pub bin: PathBuf,
    /// The server's socket.
    pub socket: PathBuf,
    /// The pane `chief up` runs in: the layout's window opens right after its window.
    pub after: Option<String>,
}

/// A stored layout: `<window id> <server pid> <socket>`. Window ids restart with each server, so
/// the server pid proves an id still means the window chief opened.
struct Stored<'a> {
    window: &'a str,
    server: &'a str,
    socket: &'a str,
}

fn parse(session: &SessionRef) -> Option<Stored<'_>> {
    let mut parts = session.0.splitn(3, ' ');
    Some(Stored {
        window: parts.next()?,
        server: parts.next()?,
        socket: parts.next().filter(|s| !s.is_empty())?,
    })
}

fn bin_from_env() -> PathBuf {
    std::env::var_os("CHIEF_TMUX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

impl Tmux {
    /// The tmux server this process runs inside (`$TMUX` is `<socket>,<pid>,<session>`), and
    /// the pane it runs in (`$TMUX_PANE`). None outside tmux.
    pub fn current() -> Option<Tmux> {
        let tmux = std::env::var("TMUX").ok()?;
        let socket = tmux.split(',').next().filter(|s| !s.is_empty())?;
        Some(Tmux {
            bin: bin_from_env(),
            socket: PathBuf::from(socket),
            after: std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()),
        })
    }

    /// The server a stored layout id (`chief up` stored it) lives on, from any terminal.
    pub fn for_layout(session: &SessionRef) -> Option<Tmux> {
        Some(Tmux {
            bin: bin_from_env(),
            socket: PathBuf::from(parse(session)?.socket),
            after: None,
        })
    }

    /// Runs `tmux -S <socket> <args>`; a non-zero exit is an error carrying stderr.
    fn run(&self, args: &[&str]) -> Result<Output> {
        let out = self.try_run(args)?;
        if !out.status.success() {
            bail!(
                "`tmux {}` failed ({}): {}",
                args.first().copied().unwrap_or_default(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// Like `run`, but a non-zero exit is returned rather than an error.
    fn try_run(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-S").arg(&self.socket).args(args);
        // The server is named by `-S` and targets explicitly; the caller's own tmux variables
        // would only add a second opinion.
        cmd.env_remove("TMUX").env_remove("TMUX_PANE");
        let word = args.first().copied().unwrap_or_default();
        match crate::proc::output_with_timeout(&mut cmd, TIMEOUT) {
            Ok(Some(out)) => Ok(out),
            Ok(None) => bail!("`tmux {word}` took longer than {}s", TIMEOUT.as_secs()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("tmux isn't installed ({} not found)", self.bin.display())
            }
            Err(e) => Err(e).with_context(|| format!("running `tmux {word}`")),
        }
    }

    fn stdout(&self, args: &[&str]) -> Result<String> {
        let out = self.run(args)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Labels a new pane and keeps it open after its program ends, so a crash message stays
    /// readable and `chief up` can tell it ended.
    fn mark(&self, pane: &str, label: &str) -> Result<()> {
        self.run(&[
            "set-option",
            "-p",
            "-t",
            pane,
            LABEL,
            label,
            ";",
            "set-option",
            "-p",
            "-t",
            pane,
            "remain-on-exit",
            "on",
        ])?;
        Ok(())
    }

    fn window<'a>(&self, session: &'a SessionRef) -> Result<&'a str> {
        parse(session)
            .map(|s| s.window)
            .with_context(|| format!("not a tmux layout id: {:?}", session.0))
    }
}

/// `-c <cwd>` and `-- <command>`, shared by `new-window` and `split-window`.
fn command_args<'a>(spec: &'a BlockSpec, cwd: &'a str, args: &mut Vec<&'a str>) {
    if spec.cwd.is_some() {
        args.extend(["-c", cwd]);
    }
    args.push("--");
    args.extend(spec.command.iter().map(String::as_str));
}

impl Mux for Tmux {
    fn has_session(&self, session: &SessionRef) -> Result<bool> {
        let Some(stored) = parse(session) else {
            return Ok(false);
        };
        if Path::new(stored.socket) != self.socket {
            return Ok(false);
        }
        let out = self.try_run(&["list-windows", "-a", "-F", "#{pid} #{window_id}"])?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // No server on that socket: nothing of chief's is open there.
            if stderr.contains("no server running") || stderr.contains("error connecting") {
                return Ok(false);
            }
            bail!(
                "`tmux list-windows` failed ({}): {}",
                out.status,
                stderr.trim()
            );
        }
        let want = format!("{} {}", stored.server, stored.window);
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == want))
    }

    fn create_session(&self, label: &str, first: &BlockSpec) -> Result<(SessionRef, BlockRef)> {
        let cwd = first
            .cwd
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        // `new-window -t` takes a window, not a pane ("can't specify pane here"): open after the
        // window holding the pane chief up runs in.
        let after = match &self.after {
            Some(pane) => {
                Some(self.stdout(&["display-message", "-p", "-t", pane, "#{window_id}"])?)
            }
            None => None,
        };
        let mut args = vec!["new-window", "-n", label];
        if let Some(window) = &after {
            args.extend(["-a", "-t", window]);
        }
        if !first.focus {
            args.push("-d");
        }
        args.extend(["-P", "-F", "#{pane_id} #{window_id} #{pid}"]);
        command_args(first, &cwd, &mut args);
        let out = self.stdout(&args)?;
        let mut ids = out.split_whitespace();
        let (Some(pane), Some(window), Some(server)) = (ids.next(), ids.next(), ids.next()) else {
            bail!("`tmux new-window` reported no pane: {out:?}");
        };
        let session = SessionRef(format!("{window} {server} {}", self.socket.display()));
        kill_on_error(self, &session, self.mark(pane, &first.label))?;
        Ok((session, BlockRef(pane.to_string())))
    }

    fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef> {
        let window = self.window(session)?;
        let cwd = spec
            .cwd
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = format!("{}%", spec.ratio);
        let mut args = vec!["split-window"];
        match &spec.anchor {
            Some((anchor, split)) => {
                args.push(match split {
                    Split::Left | Split::Right => "-h",
                    Split::Above | Split::Below => "-v",
                });
                if matches!(split, Split::Left | Split::Above) {
                    args.push("-b");
                }
                args.extend(["-l", &size, "-t", &anchor.0]);
            }
            // A window is the whole layout here: "a new window" is another split of it.
            None => args.extend(["-t", window]),
        }
        if !spec.focus {
            args.push("-d");
        }
        args.extend(["-P", "-F", "#{pane_id}"]);
        command_args(spec, &cwd, &mut args);
        let pane = self.stdout(&args)?;
        if pane.is_empty() {
            bail!("`tmux split-window` reported no pane");
        }
        self.mark(&pane, &spec.label)?;
        Ok(BlockRef(pane))
    }

    fn blocks(&self, session: &SessionRef) -> Result<Vec<BlockInfo>> {
        let window = self.window(session)?;
        let out = self.stdout(&[
            "list-panes",
            "-t",
            window,
            "-F",
            &format!("#{{pane_id}} #{{{LABEL}}}"),
        ])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (id, label) = l.split_once(' ').unwrap_or((l, ""));
                (!id.is_empty()).then(|| BlockInfo {
                    block: BlockRef(id.to_string()),
                    label: label.trim().to_string(),
                })
            })
            .collect())
    }

    fn is_running(&self, block: &BlockRef) -> Result<bool> {
        Ok(self.stdout(&["display-message", "-p", "-t", &block.0, "#{pane_dead}"])? == "0")
    }

    fn close_block(&self, block: &BlockRef) -> Result<()> {
        self.run(&["kill-pane", "-t", &block.0])?;
        Ok(())
    }

    fn focus(&self, block: &BlockRef) -> Result<()> {
        self.run(&["select-pane", "-t", &block.0])?;
        Ok(())
    }

    fn show(&self, session: &SessionRef) -> Result<bool> {
        let window = self.window(session)?;
        Ok(self.run(&["select-window", "-t", window]).is_ok())
    }

    fn kill_session(&self, session: &SessionRef) -> Result<()> {
        let window = self.window(session)?;
        self.run(&["kill-window", "-t", window])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Against a real tmux server on a private socket (`-L chief-test-…`, no config), which
    //! every test kills when it ends, failed or not. The user's own servers are never touched.

    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::layout::{self, Closed, Opened};

    struct Server {
        bin: PathBuf,
        socket: PathBuf,
        dir: tempfile::TempDir,
    }

    impl Server {
        /// None when tmux isn't installed: the test has nothing to run against.
        fn start() -> Option<Server> {
            let bin = bin_from_env();
            let name = format!("chief-test-{}-{}", std::process::id(), uuid::Uuid::new_v4());
            let ok = Command::new(&bin)
                .args([
                    "-L",
                    &name,
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "t",
                ])
                .args(["-x", "200", "-y", "50", "sleep 600"])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .status()
                .ok()?
                .success();
            let out = Command::new(&bin)
                .args(["-L", &name, "display-message", "-p", "#{socket_path}"])
                .env_remove("TMUX")
                .output()
                .unwrap();
            let socket = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            let server = Server {
                bin,
                socket,
                dir: tempfile::tempdir().unwrap(),
            };
            // Asserted after `server` exists, so its drop kills whatever did start.
            assert!(ok, "starting a private tmux server");
            assert!(server.socket.is_absolute(), "{:?}", server.socket);
            Some(server)
        }

        fn tmux(&self) -> Tmux {
            Tmux {
                bin: self.bin.clone(),
                socket: self.socket.clone(),
                after: None,
            }
        }

        /// Stands in for the chief binary: whatever the arguments, it stays up.
        fn exe(&self) -> PathBuf {
            let exe = self.dir.path().join("chief");
            std::fs::write(&exe, "#!/bin/sh\nexec sleep 600\n").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            exe
        }

        /// `list-panes` of `window`: (id, label, left edge, width).
        fn panes(&self, window: &str) -> Vec<(String, String, u32, u32)> {
            let out = self
                .tmux()
                .stdout(&[
                    "list-panes",
                    "-t",
                    window,
                    "-F",
                    "#{pane_id}|#{@chief-block}|#{pane_left}|#{pane_width}",
                ])
                .unwrap();
            out.lines()
                .map(|l| {
                    let w: Vec<&str> = l.split('|').collect();
                    (
                        w[0].to_string(),
                        w[1].to_string(),
                        w[2].parse().unwrap(),
                        w[3].parse().unwrap(),
                    )
                })
                .collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if self.socket.is_absolute() {
                let _ = Command::new(&self.bin)
                    .arg("-S")
                    .arg(&self.socket)
                    .arg("kill-server")
                    .env_remove("TMUX")
                    .status();
                // The server leaves its socket file behind.
                let _ = std::fs::remove_file(&self.socket);
            }
        }
    }

    fn open(server: &Server, ours: Option<&SessionRef>) -> layout::Layout {
        let dir = server.dir.path().canonicalize().unwrap();
        layout::open(&server.tmux(), "demo", &dir, &server.exe(), &[], ours).unwrap()
    }

    fn window(session: &SessionRef) -> String {
        parse(session).unwrap().window.to_string()
    }

    #[test]
    fn opens_reuses_repairs_and_closes_a_window() {
        let Some(server) = Server::start() else {
            eprintln!("tmux isn't installed; skipped");
            return;
        };
        let tmux = server.tmux();
        let layout = open(&server, None);
        assert_eq!(layout.opened, Opened::Created);
        assert!(layout.shown);
        let w = window(&layout.session);
        let name = tmux
            .stdout(&["display-message", "-p", "-t", &w, "#{window_name}"])
            .unwrap();
        assert_eq!(name, "chief-demo");
        let panes = server.panes(&w);
        let labels: Vec<&str> = panes.iter().map(|p| p.1.as_str()).collect();
        assert_eq!(labels, ["main", "board"]);
        let (main, board) = (&panes[0], &panes[1]);
        assert_eq!(main.2, 0, "main on the left");
        assert!(board.2 > main.2, "board on the right");
        // 32 % of 200 columns, give or take the border.
        assert!((62..=66).contains(&board.3), "board width {}", board.3);
        let cwd = tmux
            .stdout(&[
                "display-message",
                "-p",
                "-t",
                &main.0,
                "#{pane_current_path}",
            ])
            .unwrap();
        assert_eq!(Path::new(&cwd), server.dir.path().canonicalize().unwrap());

        // Both running: reused as is.
        assert_eq!(open(&server, Some(&layout.session)).opened, Opened::Reused);
        assert_eq!(server.panes(&w).len(), 2);

        // A missing board comes back on the right.
        tmux.close_block(&BlockRef(board.0.clone())).unwrap();
        assert_eq!(
            open(&server, Some(&layout.session)).opened,
            Opened::Repaired
        );
        let labels: Vec<String> = server.panes(&w).into_iter().map(|p| p.1).collect();
        assert_eq!(labels, ["main", "board"]);

        // A pane of the user's keeps the window open: only chief's two close.
        tmux.run(&["split-window", "-d", "-t", &w, "--", "sleep", "600"])
            .unwrap();
        assert_eq!(
            layout::close(&tmux, Some(&layout.session)).unwrap(),
            Closed::Blocks(layout.session.clone())
        );
        let left: Vec<String> = server.panes(&w).into_iter().map(|p| p.1).collect();
        assert_eq!(left, [""]);
        tmux.run(&["kill-window", "-t", &w]).unwrap();

        // Only chief's panes: the window goes, and is known gone.
        let layout = open(&server, None);
        assert_eq!(
            layout::close(&tmux, Some(&layout.session)).unwrap(),
            Closed::Killed(layout.session.clone())
        );
        assert!(!tmux.has_session(&layout.session).unwrap());
    }

    /// `chief up` inside tmux opens its window next to the one it runs in, found from
    /// `$TMUX_PANE` (a pane id, which `new-window -t` doesn't accept).
    #[test]
    fn opens_after_the_window_of_the_pane_it_runs_in() {
        let Some(server) = Server::start() else {
            return;
        };
        let pane = server
            .tmux()
            .stdout(&["display-message", "-p", "-t", "t:0", "#{pane_id}"])
            .unwrap();
        let tmux = Tmux {
            after: Some(pane),
            ..server.tmux()
        };
        let dir = server.dir.path().canonicalize().unwrap();
        let layout = layout::open(&tmux, "demo", &dir, &server.exe(), &[], None).unwrap();
        assert_eq!(layout.opened, Opened::Created);
        let index = tmux
            .stdout(&[
                "display-message",
                "-p",
                "-t",
                &window(&layout.session),
                "#{window_index}",
            ])
            .unwrap();
        assert_eq!(index, "1", "right after window 0");
    }

    /// Window ids restart with every server: an id stored for another server, or another
    /// socket, is never taken for chief's.
    #[test]
    fn a_stored_id_from_another_server_is_not_ours() {
        let Some(server) = Server::start() else {
            return;
        };
        let tmux = server.tmux();
        let layout = open(&server, None);
        assert!(tmux.has_session(&layout.session).unwrap());
        let w = window(&layout.session);
        let socket = server.socket.display();
        let old_server = SessionRef(format!("{w} 1 {socket}"));
        assert!(!tmux.has_session(&old_server).unwrap());
        let other_socket = SessionRef(format!("{w} 1 /nowhere/sock"));
        assert!(!tmux.has_session(&other_socket).unwrap());
        assert!(!tmux.has_session(&SessionRef("garbage".into())).unwrap());

        // A socket with no server behind it: nothing open, not an error.
        let gone = Tmux {
            socket: server.dir.path().join("no-server"),
            ..server.tmux()
        };
        let stale = SessionRef(format!("{w} 1 {}", gone.socket.display()));
        assert!(!gone.has_session(&stale).unwrap());
        assert_eq!(layout::close(&gone, Some(&stale)).unwrap(), Closed::Nothing);
    }

    #[test]
    fn for_layout_takes_the_socket_from_the_stored_id() {
        let stored = SessionRef("@3 4242 /tmp/tmux 501/default".into());
        let tmux = Tmux::for_layout(&stored).unwrap();
        assert_eq!(tmux.socket, PathBuf::from("/tmp/tmux 501/default"));
        assert!(Tmux::for_layout(&SessionRef("@3 4242".into())).is_none());
    }
}
