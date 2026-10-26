//! Left pane loop (ARCHITECTURE §9): attach to `ui_state.pane_target`, re-read it after each detach.
//!
//! The board switches the pane by sending Ctrl+Z to it. Inside `claude attach` that detaches; if
//! it lands while chief itself has the terminal, the line discipline would turn it into SIGTSTP
//! and freeze the pane. So chief keeps the terminal in raw mode (no signal keys) whenever it is
//! in the foreground and hands it to attach with signal keys still off, so a Ctrl+Z that lands
//! before Claude's UI is up waits as input for it instead of being lost. It also ignores SIGTSTP
//! and gives `claude attach` the default disposition back, in case one gets through.

use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use rusqlite::Connection;

use crate::claude::Claude;
use crate::config::ProjectConfig;
use crate::db::{self, SessionRow};
use crate::mux::Mux;
use crate::{layout, names, paths, project, registry};

/// How often a failed attach is retried.
const RETRY: Duration = Duration::from_millis(500);
/// How long a respawned session gets to show a process.
const WAKE_WAIT: Duration = Duration::from_secs(20);
/// An attach that ends sooner than this counts as a failed start: wait before the next try so a
/// session that keeps refusing doesn't spin.
const SHORT_ATTACH: Duration = Duration::from_secs(1);

pub fn run(project: &str) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, project)?.is_none() {
        bail!("no chief project {project:?}");
    }
    let claude = Claude::from_env();
    let tty = Tty::take()?;
    let result = pane_loop(&conn, &claude, project, &tty);
    let _ = db::set_pane_showing(&conn, project, None);
    drop(tty);
    result
}

/// The session the pane should show: `pane_target`, or the coordinator when it is NULL or names
/// a session that is unknown, of another project, cleaned or removed, or never started (the
/// stale target is cleared). None when the coordinator hasn't been started either.
pub fn resolve_target(conn: &Connection, project: &str) -> Result<Option<SessionRow>> {
    if let Some(name) = db::get_pane_target(conn, project)? {
        match db::get_session(conn, &name)? {
            Some(row)
                if row.project_slug == project
                    && row.cleaned_at.is_none()
                    && row.short_id.is_some() =>
            {
                return Ok(Some(row));
            }
            _ => db::clear_pane_target_if(conn, project, &name)?,
        }
    }
    Ok(db::get_coordinator(conn, project)?.filter(|r| r.short_id.is_some()))
}

fn pane_loop(conn: &Connection, claude: &Claude, project: &str, tty: &Tty) -> Result<()> {
    loop {
        let Some(row) = resolve_target(conn, project)? else {
            tty.say(&format!(
                "chief · no coordinator yet: run chief up {project} · q quits"
            ))?;
            if tty.quit_pressed(RETRY)? {
                return Ok(());
            }
            continue;
        };
        let name = row.name.clone();
        let short_id = row.short_id.clone().context("target has no short id")?;
        tty.say(&format!("chief · {name} …"))?;
        if let Err(e) = wake(conn, claude, project, &row, tty) {
            tty.say(&format!("chief · {name}: {e:#} · retrying · q quits"))?;
            if tty.quit_pressed(RETRY)? {
                return Ok(());
            }
            continue;
        }

        // Claim the pane, then look again: a board that wrote a new target before this claim
        // saw no attach to interrupt and sent nothing.
        db::set_pane_showing(conn, project, Some(&name))?;
        if resolve_target(conn, project)?.map(|r| r.name).as_deref() != Some(name.as_str()) {
            db::set_pane_showing(conn, project, None)?;
            continue;
        }
        // Drops keys that arrived meanwhile, a switch's Ctrl+Z among them, so attach doesn't
        // read one and detach at once.
        if tty.quit_pressed(Duration::ZERO)? {
            return Ok(());
        }
        tty.release();
        let started = Instant::now();
        let attached = attach(claude, &short_id);
        tty.retake()?;
        db::set_pane_showing(conn, project, None)?;

        let failed = match attached {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("claude attach {short_id} exited with {status}")),
            Err(e) => Some(format!("{e:#}")),
        };
        if failed.is_some() || started.elapsed() < SHORT_ATTACH {
            let why = failed.unwrap_or_else(|| format!("claude attach {short_id} ended at once"));
            // Below whatever attach printed, which says why.
            tty.note(&format!("chief · {name}: {why} · retrying · q quits"))?;
            if tty.quit_pressed(RETRY)? {
                return Ok(());
            }
        }
    }
}

/// Respawns `row` when `claude agents` shows no process for it and waits for one. A failing
/// `claude agents` is no reason not to try attaching.
fn wake(
    conn: &Connection,
    claude: &Claude,
    project: &str,
    row: &SessionRow,
    tty: &Tty,
) -> Result<()> {
    let taken_at = db::now_ms();
    let Ok(snapshot) = claude.agents() else {
        return Ok(());
    };
    registry::sync(conn, project, &snapshot, taken_at, db::now_ms())?;
    let row =
        db::get_session(conn, &row.name)?.with_context(|| format!("{} vanished", row.name))?;
    if row.pid.is_some() {
        return Ok(());
    }
    tty.say(&format!("chief · waking {} …", row.name))?;
    let dir = project::project_dir(project);
    let config = ProjectConfig::load(&dir.join("project.toml"))?;
    project::respawn(conn, claude, &dir, &config, &row, WAKE_WAIT)?;
    Ok(())
}

/// Runs `claude attach <short_id>` on this terminal and waits for it.
fn attach(claude: &Claude, short_id: &str) -> Result<ExitStatus> {
    let mut cmd = Command::new(&claude.bin);
    cmd.args(["attach", short_id]);
    with_default_tstp(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("running {} attach {short_id}", claude.bin.display()))?;
    Ok(wait_continuing(child)?.0)
}

/// This process ignores SIGTSTP, and an ignored signal stays ignored across exec; the child
/// gets the default back so Claude Code's own job control works as usual.
fn with_default_tstp(cmd: &mut Command) {
    // SAFETY: signal() is async-signal-safe, the only kind of call allowed between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            Ok(())
        });
    }
}

/// Waits for `pid` to exit; returns its status and how many times it was stopped. A child
/// stopped before it took the terminal (a Ctrl+Z during attach start-up) is continued: nothing
/// else would, and the pane would hang.
fn wait_continuing(child: Child) -> std::io::Result<(ExitStatus, usize)> {
    // Reaped here rather than by `Child::wait`, which can't see a stop.
    let pid = child.id() as libc::pid_t;
    let mut stops = 0;
    loop {
        let mut status = 0;
        // SAFETY: plain syscalls on our own child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r == -1 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if libc::WIFSTOPPED(status) {
            stops += 1;
            unsafe { libc::kill(pid, libc::SIGCONT) };
            continue;
        }
        return Ok((ExitStatus::from_raw(status), stops));
    }
}

/// The pane's terminal while chief has it: raw mode (Ctrl+Z and Ctrl+C are plain bytes) and
/// SIGTSTP ignored. Keeps the mode the pane started in, to hand over and restore.
struct Tty {
    cooked: libc::termios,
}

impl Tty {
    fn take() -> Result<Tty> {
        // SAFETY: setting a signal disposition (no handler runs) and reading stdin's termios
        // into a zeroed struct of the right type.
        let cooked = unsafe {
            libc::signal(libc::SIGTSTP, libc::SIG_IGN);
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                bail!("chief pane needs a terminal");
            }
            t
        };
        enable_raw_mode().context("chief pane needs a terminal")?;
        Ok(Tty { cooked })
    }

    /// Hands the terminal to a child in the mode the pane started in, minus signal keys.
    fn release(&self) {
        let _ = disable_raw_mode();
        let mut t = self.cooked;
        t.c_lflag &= !libc::ISIG;
        // SAFETY: writing a termios read from the same terminal.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) };
    }

    fn retake(&self) -> Result<()> {
        enable_raw_mode().context("restoring raw mode")
    }

    /// Clears the pane and prints one line at the top.
    fn say(&self, line: &str) -> Result<()> {
        let mut out = std::io::stdout();
        execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;
        self.note(line)
    }

    /// Prints one line where the cursor is (raw mode needs the carriage return).
    fn note(&self, line: &str) -> Result<()> {
        let mut out = std::io::stdout();
        execute!(out, Print(line), Print("\r\n"))?;
        out.flush()?;
        Ok(())
    }

    /// Reads keys for `wait`, then whatever is still queued; true when `q` was pressed. Other
    /// keys are dropped.
    fn quit_pressed(&self, wait: Duration) -> Result<bool> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if !event::poll(left)? {
                return Ok(false);
            }
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('q')
                && key.modifiers == KeyModifiers::NONE
            {
                return Ok(true);
            }
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // SAFETY: as in `release`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.cooked) };
        println!();
    }
}

/// Asks the project's main pane to show `ui_state.pane_target` now. The board writes the target
/// first, then calls this. It sends Ctrl+Z to the layout's `main` block, which detaches
/// `claude attach` so `chief pane` attaches to the new target (§9). Does nothing when no layout
/// is open (a board run on its own), when the pane already shows the target, or when the pane is
/// between attaches (it reads the target before its next attach).
pub fn request_switch(mux: &dyn Mux, conn: &Connection, project: &str) -> Result<()> {
    let Some(showing) = db::get_pane_showing(conn, project)? else {
        return Ok(());
    };
    let target =
        db::get_pane_target(conn, project)?.unwrap_or_else(|| names::coordinator_name(project));
    if showing == target {
        return Ok(());
    }
    match layout::main_block(mux, project)? {
        Some(main) => mux.send_key(&main, "ctrl+z"),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::testing::{Call, FakeMux};
    use crate::state::Role;

    fn fresh() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open(&dir.path().join("chief.db")).unwrap();
        db::register_project(&conn, "demo", dir.path(), 0).unwrap();
        db::register_project(&conn, "other", dir.path(), 0).unwrap();
        (dir, conn)
    }

    fn insert(conn: &Connection, name: &str, project: &str, role: Role, short: Option<&str>) {
        let mut row = SessionRow::new(name, project, role, 1);
        row.short_id = short.map(String::from);
        db::insert_session(conn, &row).unwrap();
    }

    fn target(conn: &Connection) -> Option<String> {
        resolve_target(conn, "demo").unwrap().map(|r| r.name)
    }

    #[test]
    fn target_resolution() {
        let (_dir, conn) = fresh();
        assert_eq!(target(&conn), None, "no coordinator yet");
        insert(&conn, "chief-demo", "demo", Role::Coordinator, Some("c0"));
        assert_eq!(target(&conn).as_deref(), Some("chief-demo"));

        insert(&conn, "demo-a-1111", "demo", Role::Thread, Some("t1"));
        db::set_pane_target(&conn, "demo", Some("demo-a-1111")).unwrap();
        assert_eq!(target(&conn).as_deref(), Some("demo-a-1111"));

        // A resolved thread can still be shown; a cleaned one can't.
        let mut row = db::get_session(&conn, "demo-a-1111").unwrap().unwrap();
        row.resolved = true;
        db::update_session(&conn, &row).unwrap();
        assert_eq!(target(&conn).as_deref(), Some("demo-a-1111"));
        row.cleaned_at = Some(5);
        row.resolved_reason = Some("removed".into());
        db::update_session(&conn, &row).unwrap();
        assert_eq!(target(&conn).as_deref(), Some("chief-demo"));
        assert_eq!(db::get_pane_target(&conn, "demo").unwrap(), None, "cleared");

        // Unknown, another project's, never started: the coordinator, and the target cleared.
        insert(&conn, "other-b-2222", "other", Role::Thread, Some("t2"));
        insert(&conn, "demo-c-3333", "demo", Role::Thread, None);
        for name in ["ghost", "other-b-2222", "demo-c-3333"] {
            db::set_pane_target(&conn, "demo", Some(name)).unwrap();
            assert_eq!(target(&conn).as_deref(), Some("chief-demo"), "{name}");
            assert_eq!(db::get_pane_target(&conn, "demo").unwrap(), None, "{name}");
        }
    }

    #[test]
    fn switch_sends_one_ctrl_z_to_main() {
        let (_dir, conn) = fresh();
        let mux = FakeMux::with_session("chief-demo", &[("board", true), ("main", true)]);
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        db::set_pane_target(&conn, "demo", Some("demo-a-1111")).unwrap();
        request_switch(&mux, &conn, "demo").unwrap();
        assert_eq!(mux.calls(), [Call::Key("b1".into(), "ctrl+z".into())]);

        // Back to the coordinator while a thread shows.
        let mux = FakeMux::with_session("chief-demo", &[("main", true)]);
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        db::set_pane_target(&conn, "demo", None).unwrap();
        request_switch(&mux, &conn, "demo").unwrap();
        assert_eq!(mux.calls(), [Call::Key("b0".into(), "ctrl+z".into())]);
    }

    #[test]
    fn switch_skips_when_nothing_would_change() {
        let (_dir, conn) = fresh();
        let mux = FakeMux::with_session("chief-demo", &[("main", true)]);
        // Already showing it, coordinator or thread.
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        request_switch(&mux, &conn, "demo").unwrap();
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        db::set_pane_target(&conn, "demo", Some("demo-a-1111")).unwrap();
        request_switch(&mux, &conn, "demo").unwrap();
        // Between attaches, or no pane at all.
        db::set_pane_showing(&conn, "demo", None).unwrap();
        request_switch(&mux, &conn, "demo").unwrap();
        assert_eq!(mux.calls(), []);

        // No layout (a board run on its own), or a layout without `main`.
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        let none = FakeMux::default();
        request_switch(&none, &conn, "demo").unwrap();
        let no_main = FakeMux::with_session("chief-demo", &[("board", true)]);
        request_switch(&no_main, &conn, "demo").unwrap();
        assert_eq!(none.calls(), []);
        assert_eq!(no_main.calls(), []);
    }

    /// The child of a pane that ignores SIGTSTP gets the default back (it stops when it sends
    /// itself SIGTSTP), and the wait continues it instead of hanging.
    #[test]
    fn attach_child_can_stop_and_is_continued() {
        // SAFETY: as in the pane; harmless for the test process.
        unsafe { libc::signal(libc::SIGTSTP, libc::SIG_IGN) };
        // Its own process group: stop signals are discarded in an orphaned one, which the test
        // runner's group may be.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "kill -TSTP $$; exit 3"]).process_group(0);
        with_default_tstp(&mut cmd);
        let child = cmd.spawn().unwrap();
        let (status, stops) = wait_continuing(child).unwrap();
        assert_eq!((status.code(), stops), (Some(3), 1));

        // Without the reset the child inherits the ignore and never stops.
        let child = Command::new("sh")
            .args(["-c", "kill -TSTP $$; exit 4"])
            .process_group(0)
            .spawn()
            .unwrap();
        let (status, stops) = wait_continuing(child).unwrap();
        assert_eq!((status.code(), stops), (Some(4), 0));
    }
}
