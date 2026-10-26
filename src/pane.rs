//! Left pane loop (ARCHITECTURE §9): attach to `ui_state.pane_target`, re-read it after each detach.
//!
//! The board switches the pane by signalling it (SIGUSR1, see `request_switch`): the pane ends
//! its `claude attach` with SIGTERM, undoes the screen modes the killed client left behind, and
//! attaches to the new target. That needs no terminal multiplexer.
//!
//! The user can still detach by hand with Ctrl+Z inside attach. If a Ctrl+Z lands while chief
//! itself has the terminal, the line discipline would turn it into SIGTSTP and freeze the pane.
//! So chief keeps the terminal in raw mode (no signal keys) whenever it is in the foreground and
//! hands it to attach with signal keys still off, so a Ctrl+Z that lands before Claude's UI is up
//! waits as input for it instead of being lost. It also ignores SIGTSTP and gives `claude attach`
//! the default disposition back, in case one gets through.

use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::{names, paths, proc, project, registry};

/// How often a failed attach is retried.
const RETRY: Duration = Duration::from_millis(500);
/// How long a respawned session gets to show a process.
const WAKE_WAIT: Duration = Duration::from_secs(20);
/// An attach that ends sooner than this counts as a failed start: wait before the next try so a
/// session that keeps refusing doesn't spin.
const SHORT_ATTACH: Duration = Duration::from_secs(1);
/// How long attach gets to exit after a hangup or a switch before it is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(2);
/// How often the wait for attach looks at the child and the signal flags.
const WAIT_TICK: Duration = Duration::from_millis(50);
/// What a `claude attach` ended by a signal leaves on: the alternate screen, a hidden cursor,
/// mouse reporting, bracketed paste and focus events. Leave and turn off each, reset attributes,
/// clear.
const TERMINAL_RESET: &str = "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\
                              \x1b[?2004l\x1b[?1004l\x1b[0m\x1b[2J\x1b[H";
/// How often an unusable project.toml is read again.
const CONFIG_RETRY: Duration = Duration::from_secs(5);

/// Set by SIGHUP or SIGTERM.
static HUNG_UP: AtomicBool = AtomicBool::new(false);
/// Set by SIGUSR1: the board asks for another target.
static SWITCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_hangup(_: libc::c_int) {
    HUNG_UP.store(true, Ordering::SeqCst);
}

extern "C" fn on_switch(_: libc::c_int) {
    SWITCH.store(true, Ordering::SeqCst);
}

/// The multiplexer closing the pane or killing the session hangs up the pty, and the kernel sends
/// SIGHUP to the session leader: this process, not `claude attach`. Its default action would end
/// chief at once, leaving `pane_showing` set (which also keeps cleanup away from that thread,
/// §12) and attach unreaped. The handler only records it; without `SA_RESTART` a blocking call is
/// interrupted, the wait for attach sees the flag, and the loop winds down. SIGTERM (`kill
/// <pid>`) is handled the same way. A caught
/// signal reverts to the default across exec, so attach keeps the usual behaviour.
fn catch_hangup() {
    catch(&[libc::SIGHUP, libc::SIGTERM], on_hangup);
}

/// The board's switch request, caught the same way. It must be caught before the pane records
/// its pid: SIGUSR1's default action would end it.
fn catch_switch() {
    catch(&[libc::SIGUSR1], on_switch);
}

fn catch(signals: &[libc::c_int], handler: extern "C" fn(libc::c_int)) {
    // SAFETY: installing a handler that only stores to an atomic (async-signal-safe).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        for &sig in signals {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

pub fn run(project: &str) -> Result<()> {
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, project)?.is_none() {
        bail!("no chief project {project:?}");
    }
    catch_hangup();
    catch_switch();
    let tty = Tty::take()?;
    // Where the board's switch requests go, with the start time that proves the pid is still
    // this pane. Without a start time the board can't switch this pane, but it still works.
    let me = std::process::id() as i32;
    if let Some(started) = proc::start_time(me) {
        db::set_pane_process(&conn, project, me, &started)?;
    }
    let result = pane_loop(&conn, project, &tty);
    let _ = db::set_pane_showing(&conn, project, None);
    let _ = db::clear_pane_process_if(&conn, project, me);
    drop(tty);
    result
}

/// The session the pane should show: `pane_target`, or the coordinator when it is NULL or names
/// a session that is unknown, of another project, cleaned, removed outside chief (its worktree
/// may remain, its session doesn't), or never started (the stale target is cleared). None when
/// the coordinator hasn't been started either.
pub fn resolve_target(conn: &Connection, project: &str) -> Result<Option<SessionRow>> {
    if let Some(name) = db::get_pane_target(conn, project)? {
        match db::get_session(conn, &name)? {
            Some(row)
                if row.project_slug == project
                    && row.cleaned_at.is_none()
                    && row.removed_at.is_none()
                    && row.short_id.is_some() =>
            {
                return Ok(Some(row));
            }
            _ => db::clear_pane_target_if(conn, project, &name)?,
        }
    }
    Ok(db::get_coordinator(conn, project)?.filter(|r| r.short_id.is_some()))
}

/// project.toml as it is now, or the lines the pane shows while it can't be read or doesn't
/// validate: the pane then neither attaches nor rewrites or respawns from stale files.
fn project_config(dir: &Path) -> Result<ProjectConfig, Vec<String>> {
    ProjectConfig::load(&dir.join("project.toml")).map_err(|e| {
        let mut lines = vec!["chief · project.toml can't be used:".to_string()];
        lines.extend(format!("{e:#}").lines().map(|l| format!("  {l}")));
        lines.push(format!(
            "Fix project.toml; retrying every {} s · q quits",
            CONFIG_RETRY.as_secs()
        ));
        lines
    })
}

fn pane_loop(conn: &Connection, project: &str, tty: &Tty) -> Result<()> {
    let dir = project::project_dir(project);
    loop {
        if HUNG_UP.load(Ordering::SeqCst) {
            return Ok(());
        }
        let config = match project_config(&dir) {
            Ok(config) => config,
            Err(lines) => {
                tty.say(&lines[0])?;
                for line in &lines[1..] {
                    tty.note(line)?;
                }
                if tty.quit_pressed(CONFIG_RETRY)? {
                    return Ok(());
                }
                continue;
            }
        };
        // The project's account, whatever the block's shell names (§3).
        let claude = &Claude::for_account(config.claude_config_dir.clone());
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
        if let Err(e) = wake(conn, claude, project, (&dir, &config), &row, tty) {
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
        // A request for the target this attach already shows (the board saw the pane just
        // before it moved on) is dropped.
        let switch = || {
            SWITCH.swap(false, Ordering::SeqCst)
                && resolve_target(conn, project)
                    .map(|r| r.map(|r| r.name).as_deref() != Some(name.as_str()))
                    .unwrap_or(true)
        };
        let attached = attach(claude, &short_id, &switch);
        if HUNG_UP.load(Ordering::SeqCst) {
            // Nothing left to draw on; `run` clears `pane_showing`.
            return Ok(());
        }
        let switched = matches!(attached, Ok(Waited { switched: true, .. }));
        if switched {
            tty.reset();
        }
        tty.retake()?;
        db::set_pane_showing(conn, project, None)?;
        if switched {
            continue;
        }

        let failed = match attached {
            Ok(w) if w.status.success() => None,
            Ok(w) => Some(format!("claude attach {short_id} exited with {}", w.status)),
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

/// Respawns `row` when `claude agents` shows no process for it and waits for one, from the
/// project.toml just read. A failing `claude agents` is no reason not to try attaching.
fn wake(
    conn: &Connection,
    claude: &Claude,
    project: &str,
    (dir, config): (&Path, &ProjectConfig),
    row: &SessionRow,
    tty: &Tty,
) -> Result<()> {
    let taken_at = db::now_ms();
    let Ok(snapshot) = claude.agents() else {
        return Ok(());
    };
    registry::sync_in(
        conn,
        project,
        &snapshot,
        taken_at,
        db::now_ms(),
        &claude.config_dir,
    )?;
    let row =
        db::get_session(conn, &row.name)?.with_context(|| format!("{} vanished", row.name))?;
    if row.pid.is_some() {
        return Ok(());
    }
    tty.say(&format!("chief · waking {} …", row.name))?;
    project::respawn(conn, claude, dir, config, &row, WAKE_WAIT)?;
    Ok(())
}

/// Runs `claude attach <short_id>` on this terminal and waits for it, or until `switch` says the
/// board wants another target.
fn attach(claude: &Claude, short_id: &str, switch: &dyn Fn() -> bool) -> Result<Waited> {
    let mut cmd = claude.command();
    cmd.args(["attach", short_id]);
    with_default_tstp(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("running {} attach {short_id}", claude.bin.display()))?;
    Ok(wait_continuing(child, &HUNG_UP, switch)?)
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

/// How a wait for attach ended.
#[derive(Debug)]
struct Waited {
    status: ExitStatus,
    /// How many times the child was stopped (and continued). Only tests look.
    #[cfg_attr(not(test), allow(dead_code))]
    stops: usize,
    /// Ended for a switch.
    switched: bool,
}

/// Waits for `pid` to exit. A child stopped before it took the terminal (a Ctrl+Z during attach
/// start-up) is continued: nothing else would, and the pane would hang. Once `hung_up` is set the
/// child gets SIGHUP; once `switch()` says so, SIGTERM (which ends `claude attach` and leaves its
/// session running). Either way it gets `HANGUP_GRACE` to exit, then SIGKILL, and is reaped. The
/// wait polls every `WAIT_TICK` instead of blocking in `waitpid`: a signal landing between the
/// flag check and a blocking `waitpid` would go unseen until attach exits, which for a switch is
/// never.
fn wait_continuing(
    child: Child,
    hung_up: &AtomicBool,
    switch: &dyn Fn() -> bool,
) -> std::io::Result<Waited> {
    // Reaped here rather than by `Child::wait`, which can't see a stop.
    let pid = child.id() as libc::pid_t;
    let mut stops = 0;
    loop {
        if hung_up.load(Ordering::SeqCst) {
            let status = end_child(pid, libc::SIGHUP);
            return Ok(Waited {
                status,
                stops,
                switched: false,
            });
        }
        if switch() {
            let status = end_child(pid, libc::SIGTERM);
            return Ok(Waited {
                status,
                stops,
                switched: true,
            });
        }
        let mut status = 0;
        // SAFETY: plain syscalls on our own child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == 0 {
            std::thread::sleep(WAIT_TICK);
            continue;
        }
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
        return Ok(Waited {
            status: ExitStatus::from_raw(status),
            stops,
            switched: false,
        });
    }
}

/// Sends `signal` to `pid`, waits up to `HANGUP_GRACE` for it to exit, then kills it, and reaps
/// it.
fn end_child(pid: libc::pid_t, signal: libc::c_int) -> ExitStatus {
    // SAFETY: signals and waits on our own, not yet reaped, child.
    unsafe {
        libc::kill(pid, signal);
        libc::kill(pid, libc::SIGCONT);
    }
    let deadline = Instant::now() + HANGUP_GRACE;
    loop {
        let mut status = 0;
        // SAFETY: as above.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid && !libc::WIFSTOPPED(status) {
            return ExitStatus::from_raw(status);
        }
        if r == -1 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // Already reaped elsewhere: nothing left to wait for.
            return ExitStatus::from_raw(0);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    loop {
        let mut status = 0;
        // SAFETY: as above.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            return ExitStatus::from_raw(status);
        }
        if r == -1 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return ExitStatus::from_raw(0);
        }
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

    /// After a `claude attach` ended by a signal, which restored nothing: puts back the mode the
    /// pane started in and undoes the client's screen modes.
    fn reset(&self) {
        // SAFETY: as in `release`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.cooked) };
        let mut out = std::io::stdout();
        let _ = out.write_all(TERMINAL_RESET.as_bytes());
        let _ = out.flush();
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
            if HUNG_UP.load(Ordering::SeqCst) {
                return Ok(true);
            }
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
        // Not `println!`: it panics when the terminal has hung up.
        let _ = writeln!(std::io::stdout());
    }
}

/// Asks the project's main pane to show `ui_state.pane_target` now. The board writes the target
/// first, then calls this. It sends SIGUSR1 to the `chief pane` process, which ends its
/// `claude attach` and attaches to the new target (§9), in whatever terminal it runs. Does
/// nothing when the pane already shows the target, when it is between attaches (it reads the
/// target before its next attach), or when no pane is recorded. A recorded pid whose start time
/// no longer matches belongs to a pane that is gone, and may be another process's by now: it is
/// never signalled, and the record is cleared.
pub fn request_switch(conn: &Connection, project: &str) -> Result<()> {
    let Some(showing) = db::get_pane_showing(conn, project)? else {
        return Ok(());
    };
    let target =
        db::get_pane_target(conn, project)?.unwrap_or_else(|| names::coordinator_name(project));
    if showing == target {
        return Ok(());
    }
    let Some((pid, started)) = db::get_pane_process(conn, project)? else {
        return Ok(());
    };
    if proc::start_time(pid).as_deref() != Some(started.as_str()) {
        db::clear_pane_process_if(conn, project, pid)?;
        return Ok(());
    }
    // SAFETY: a plain syscall, to the process just proven to be the pane.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The reviewer's probe: a malformed project.toml made every wake fail, and the error flashed
    /// between retries. The pane says what is wrong and what to do, and uses nothing else.
    #[test]
    fn an_unusable_project_toml_is_shown_with_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("project.toml");
        std::fs::write(&toml, "name = \"Demo\"\nslug = [\n").unwrap();
        let lines = project_config(dir.path()).unwrap_err();
        assert_eq!(lines[0], "chief · project.toml can't be used:");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("parsing") && l.contains("project.toml")),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "Fix project.toml; retrying every 5 s · q quits"
        );
        assert!(lines.iter().all(|l| !l.contains('\n')), "{lines:?}");

        // A valid one is used as it is now.
        crate::config::ProjectConfig::new("Demo", "demo", None, vec![])
            .save(&toml)
            .unwrap();
        assert_eq!(project_config(dir.path()).unwrap().slug, "demo");
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

        // Removed outside chief, its worktree still there: no session to attach to either.
        insert(&conn, "demo-r-4444", "demo", Role::Thread, Some("t4"));
        let mut removed = db::get_session(&conn, "demo-r-4444").unwrap().unwrap();
        removed.removed_at = Some(5);
        removed.resolved = true;
        db::update_session(&conn, &removed).unwrap();
        db::set_pane_target(&conn, "demo", Some("demo-r-4444")).unwrap();
        assert_eq!(target(&conn).as_deref(), Some("chief-demo"));
        assert_eq!(db::get_pane_target(&conn, "demo").unwrap(), None);

        // Unknown, another project's, never started: the coordinator, and the target cleared.
        insert(&conn, "other-b-2222", "other", Role::Thread, Some("t2"));
        insert(&conn, "demo-c-3333", "demo", Role::Thread, None);
        for name in ["ghost", "other-b-2222", "demo-c-3333"] {
            db::set_pane_target(&conn, "demo", Some(name)).unwrap();
            assert_eq!(target(&conn).as_deref(), Some("chief-demo"), "{name}");
            assert_eq!(db::get_pane_target(&conn, "demo").unwrap(), None, "{name}");
        }
    }

    /// A stand-in for a running pane, recorded as `chief pane` records itself: a child that
    /// SIGUSR1 ends (its default action).
    fn fake_pane(conn: &Connection) -> Child {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let started = proc::start_time(pid).unwrap();
        db::set_pane_process(conn, "demo", pid, &started).unwrap();
        child
    }

    /// The signal that ended `child`, waiting up to 2 s; None while it still runs.
    fn ended_by(child: &mut Child) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.signal();
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn switch_signals_the_pane() {
        let (_dir, conn) = fresh();
        let mut pane = fake_pane(&conn);
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        db::set_pane_target(&conn, "demo", Some("demo-a-1111")).unwrap();
        request_switch(&conn, "demo").unwrap();
        assert_eq!(ended_by(&mut pane), Some(libc::SIGUSR1));

        // Back to the coordinator while a thread shows.
        let mut pane = fake_pane(&conn);
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        db::set_pane_target(&conn, "demo", None).unwrap();
        request_switch(&conn, "demo").unwrap();
        assert_eq!(ended_by(&mut pane), Some(libc::SIGUSR1));
    }

    #[test]
    fn switch_skips_when_nothing_would_change() {
        let (_dir, conn) = fresh();
        let mut pane = fake_pane(&conn);
        // Already showing it, coordinator or thread.
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        request_switch(&conn, "demo").unwrap();
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        db::set_pane_target(&conn, "demo", Some("demo-a-1111")).unwrap();
        request_switch(&conn, "demo").unwrap();
        // Between attaches.
        db::set_pane_showing(&conn, "demo", None).unwrap();
        request_switch(&conn, "demo").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(pane.try_wait().unwrap(), None, "never signalled");

        // A record that doesn't match the process's start time is a reused pid: never
        // signalled, and forgotten.
        let pid = pane.id() as i32;
        db::set_pane_process(&conn, "demo", pid, "1.000000").unwrap();
        db::set_pane_showing(&conn, "demo", Some("chief-demo")).unwrap();
        request_switch(&conn, "demo").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            pane.try_wait().unwrap(),
            None,
            "a stale pid isn't signalled"
        );
        assert_eq!(db::get_pane_process(&conn, "demo").unwrap(), None);

        // No pane recorded (a board run on its own).
        request_switch(&conn, "demo").unwrap();
        pane.kill().unwrap();
        pane.wait().unwrap();
    }

    /// A switch that still applies ends attach with SIGTERM and reaps it; one that no longer
    /// applies leaves it running.
    #[test]
    fn a_switch_ends_and_reaps_attach() {
        let calm = AtomicBool::new(false);
        let asked = AtomicBool::new(false);
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let waited = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                asked.store(true, Ordering::SeqCst);
            });
            wait_continuing(child, &calm, &|| asked.swap(false, Ordering::SeqCst)).unwrap()
        });
        assert!(waited.switched);
        assert_eq!(waited.status.signal(), Some(libc::SIGTERM));
        // SAFETY: probing a pid; it is reaped, so nothing has it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "reaped");

        let looked = std::sync::atomic::AtomicUsize::new(0);
        let child = Command::new("sh")
            .args(["-c", "sleep 0.3; exit 5"])
            .spawn()
            .unwrap();
        let waited = wait_continuing(child, &calm, &|| {
            looked.fetch_add(1, Ordering::SeqCst);
            false
        })
        .unwrap();
        assert!(!waited.switched);
        assert_eq!(waited.status.code(), Some(5));
        assert!(looked.load(Ordering::SeqCst) > 1, "asked while it waited");
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
        let calm = AtomicBool::new(false);
        let waited = wait_continuing(child, &calm, &|| false).unwrap();
        assert_eq!((waited.status.code(), waited.stops), (Some(3), 1));

        // Without the reset the child inherits the ignore and never stops.
        let child = Command::new("sh")
            .args(["-c", "kill -TSTP $$; exit 4"])
            .process_group(0)
            .spawn()
            .unwrap();
        let waited = wait_continuing(child, &calm, &|| false).unwrap();
        assert_eq!((waited.status.code(), waited.stops), (Some(4), 0));
    }

    /// Rex closing the block hangs up the pty, and only the session leader (chief) gets SIGHUP.
    /// chief must live on to reap attach (here one that ignores SIGHUP, so it gets SIGKILL after
    /// the grace period) and then clear `pane_showing`.
    #[test]
    fn a_hangup_reaps_the_attach_child() {
        use std::os::unix::thread::JoinHandleExt;
        catch_hangup();
        let child = Command::new("sh")
            .args(["-c", "trap '' HUP; exec sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let waiter = std::thread::spawn(move || wait_continuing(child, &HUNG_UP, &|| false));
        // Let it block in waitpid, then hang up on that thread.
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        // SAFETY: signalling a live thread of this process; the handler only stores a flag.
        unsafe { libc::pthread_kill(waiter.as_pthread_t(), libc::SIGHUP) };
        let waited = waiter.join().unwrap().unwrap();
        assert_eq!(waited.status.signal(), Some(libc::SIGKILL));
        assert!(started.elapsed() < HANGUP_GRACE + Duration::from_secs(2));
        // SAFETY: probing a pid; it is reaped, so nothing has it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "reaped");
        assert!(HUNG_UP.load(Ordering::SeqCst));
    }
}
