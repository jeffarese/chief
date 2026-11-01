//! Left pane loop (ARCHITECTURE §9): attach to `ui_state.pane_target`, re-read it after each detach.
//!
//! The board switches the pane by signalling it (SIGUSR1, see `request_switch`): the pane ends
//! its `claude attach` with SIGTERM, undoes the screen modes the killed client left behind, and
//! attaches to the new target. That needs no terminal multiplexer. The board's Ctrl+Q asks the
//! pane to quit the same way (SIGUSR2, `request_quit`).
//!
//! `claude attach` runs on a pseudo-terminal of its own whose bytes chief relays to this terminal
//! untouched (`relay`): Claude draws on the user's terminal as if it had it, and chief sees every
//! key on the way, so Ctrl+Q quits chief whatever Claude is doing. chief keeps this terminal in
//! raw mode throughout (no signal keys: a Ctrl+Z or Ctrl+C is a byte for Claude or for chief).
//!
//! The user can still leave Claude by hand: Ctrl+Z inside attach, or quitting Claude's agent
//! view. The pane then says so and waits for ⏎, a board selection or `q`, rather than attaching
//! again, which would put the user straight back in with no way out.

use std::cell::{Cell, RefCell};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use rusqlite::Connection;

use crate::claude::{AGENTS_TIMEOUT, Claude};
use crate::config::ProjectConfig;
use crate::db::{self, SessionRow};
use crate::mux::rex::Rex;
use crate::mux::{BlockRef, Mux};
use crate::relay::{self, Ended};
use crate::state::Role;
use crate::{accounts, names, paths, proc, project, rex_status};

/// How often a failed attach is retried.
pub(crate) const RETRY: Duration = Duration::from_millis(500);
/// How long a respawned session gets to show a process.
const WAKE_WAIT: Duration = Duration::from_secs(20);
/// An attach that ends sooner than this counts as a failed start: wait before the next try so a
/// session that keeps refusing doesn't spin.
pub(crate) const SHORT_ATTACH: Duration = Duration::from_secs(1);
/// What a `claude attach` ended by a signal leaves on: synchronized output, the alternate
/// screen, a hidden cursor, mouse reporting, bracketed paste, focus events, kitty keyboard flags
/// and modifyOtherKeys. End synchronized output first (the terminal holds back everything else
/// until then), then leave or turn off each, reset attributes, clear.
const TERMINAL_RESET: &str = "\x1b[?2026l\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\
                              \x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[>4m\x1b[0m\x1b[2J\x1b[H";
/// How often the pane reads the shown session's row again for its Rex status.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// How often an unusable project.toml is read again.
pub(crate) const CONFIG_RETRY: Duration = Duration::from_secs(5);
/// After the user leaves a session, keys this soon are still the ones that closed Claude (a
/// third Ctrl+C), not an answer to the pane.
pub(crate) const SETTLE: Duration = Duration::from_millis(300);
/// The longest one look for keys lasts. A signal interrupts a look, but one that lands just
/// before it starts is only seen once it ends.
const KEY_SLICE: Duration = Duration::from_millis(200);
/// Ctrl+C and Ctrl+Q, plain bytes in raw mode.
const CTRL_C: u8 = 0x03;
const CTRL_Q: u8 = 0x11;
const ESC: u8 = 0x1b;

/// Set by SIGHUP or SIGTERM.
static HUNG_UP: AtomicBool = AtomicBool::new(false);
/// Set by SIGUSR1: the board asks for another target.
static SWITCH: AtomicBool = AtomicBool::new(false);
/// Set by SIGUSR2: the board's Ctrl+Q, which quits the pane as its own Ctrl+Q does.
static QUIT: AtomicBool = AtomicBool::new(false);
/// Set by SIGWINCH: the relay gives attach's terminal the new size.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_hangup(_: libc::c_int) {
    HUNG_UP.store(true, Ordering::SeqCst);
}

extern "C" fn on_switch(_: libc::c_int) {
    SWITCH.store(true, Ordering::SeqCst);
}

extern "C" fn on_quit(_: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

extern "C" fn on_resize(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
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

/// The board's switch and quit requests, caught the same way. They must be caught before the
/// pane records its pid: the default action of SIGUSR1 and SIGUSR2 would end it.
fn catch_switch() {
    catch(&[libc::SIGUSR1], on_switch);
    catch(&[libc::SIGUSR2], on_quit);
}

/// Both, for a pane that isn't `chief pane`: the built-in layout's conversation (§9).
pub fn catch_signals() {
    catch_hangup();
    catch_switch();
}

/// SIGHUP or SIGTERM arrived.
pub fn hung_up() -> bool {
    HUNG_UP.load(Ordering::SeqCst)
}

/// Whether a switch request arrived since the last call.
pub fn take_switch() -> bool {
    SWITCH.swap(false, Ordering::SeqCst)
}

/// Whether the board asked chief to quit.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

/// The user quit through the board, or the terminal hung up.
fn ending() -> bool {
    HUNG_UP.load(Ordering::SeqCst) || quit_requested()
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

/// What the pane closes when it ends, so the board isn't left on its own beside nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnQuit {
    /// The user runs the board themselves.
    Nothing,
    /// The pane is the `main` block of the layout `chief up` opened: a `q` closes that layout.
    Layout,
    /// The pane runs in the user's own Rex block, beside this board block (§9): closed when the
    /// pane ends, the user's block goes back to its shell.
    Board(BlockRef),
}

pub fn run(project: &str, on_quit: OnQuit) -> Result<()> {
    if let Some(code) = run_as_claude(project, &on_quit) {
        std::process::exit(code);
    }
    let conn = db::open(&paths::db())?;
    if db::project_dir(&conn, project)?.is_none() {
        bail!("no chief project {project:?}");
    }
    catch_hangup();
    catch_switch();
    // The relay passes a new size on to attach's terminal.
    catch(&[libc::SIGWINCH], on_resize);
    let tty = Tty::take()?;
    // Where the board's switch requests go, with the start time that proves the pid is still
    // this pane. Without a start time the board can't switch this pane, but it still works.
    let pid = std::process::id() as i32;
    let me = match proc::start_time(pid) {
        Some(started) => {
            db::set_pane_process(&conn, project, pid, &started)?;
            Some(pid)
        }
        None => None,
    };
    let result = pane_loop(&conn, project, me, &tty, &on_quit);
    // Only while the record is still this pane's: a newer pane's stays.
    let _ = db::set_pane_showing_as(&conn, project, me, None);
    if let Some(me) = me {
        let _ = db::clear_pane_process_if(&conn, project, me);
    }
    drop(tty);
    match &on_quit {
        // The user quit (a hangup means the layout is closing already). Closing it hangs up
        // this terminal too, so it comes last.
        OnQuit::Layout if result.is_ok() && !HUNG_UP.load(Ordering::SeqCst) => {
            // Not `println!`: it panics once the terminal has hung up.
            project::close_layout(&conn, project, &mut |note| {
                let (project::Note::Did(line)
                | project::Note::Info(line)
                | project::Note::Warn(line)) = note;
                let _ = writeln!(std::io::stdout(), "{line}");
            })?;
        }
        // Even after a hangup (the user closed this block): a board beside nothing is no use.
        OnQuit::Board(board) => {
            if let Err(e) = Rex::from_env().close_block(board) {
                let _ = writeln!(std::io::stdout(), "chief · the board stays open: {e:#}");
            }
        }
        _ => {}
    }
    result
}

/// The name the pane runs under in Rex, which shows a block's agent icon by its foreground
/// process's argv0: the pane stands for the Claude session it relays, whose own process runs on
/// chief's pty, out of Rex's sight.
pub const REX_ARGV0: &str = "claude";

/// A signal for the pane `run_as_claude` waits on, to pass on to it.
static FORWARD: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_forward(sig: libc::c_int) {
    FORWARD.store(sig, Ordering::SeqCst);
}

/// In Rex, runs the pane as a new process, `chief pane` under `REX_ARGV0` in a process group of
/// its own that it makes the terminal's foreground, as a shell runs a command, and waits for it:
/// its exit code, to exit with. Rex settles a block's icon once per process, so renaming this
/// one in place (exec) wouldn't show. None when it already is that pane, isn't in Rex, has no
/// terminal, or can't start it: the pane then runs here, without the icon. Every caller runs the
/// pane last, so nothing after it is skipped.
fn run_as_claude(project: &str, on_quit: &OnQuit) -> Option<i32> {
    use std::os::unix::process::CommandExt;
    // SAFETY: isatty only reads the descriptor's state.
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    if !rex_status::enabled() || !tty || std::env::args_os().next().is_some_and(|a| a == REX_ARGV0)
    {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg0(REX_ARGV0).args(["pane", "--project", project]);
    match on_quit {
        OnQuit::Nothing => {}
        OnQuit::Layout => {
            cmd.arg("--layout");
        }
        OnQuit::Board(board) => {
            cmd.args(["--board", &board.0]);
        }
    }
    // SAFETY: between fork and exec, only async-signal-safe calls on the child's own group and
    // its terminal. It takes the terminal itself too, so it never reads it from the background
    // (and stops) before this process hands it over; SIGTTOU is ignored only while it does.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    // SIGTERM or SIGHUP for this process goes on to the pane; the terminal's own signals reach
    // the foreground pane directly.
    catch(&[libc::SIGTERM, libc::SIGHUP, libc::SIGINT], on_forward);
    let child = cmd.spawn().ok()?;
    let pid = child.id() as libc::pid_t;
    let mut status = 0;
    // SAFETY: our own child's group and our terminal, as above; waitpid on our own child, which
    // nothing else reaps (the Child is never waited on).
    unsafe {
        libc::setpgid(pid, pid);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(libc::STDIN_FILENO, pid);
        loop {
            if libc::waitpid(pid, &mut status, 0) == pid {
                break;
            }
            if std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
                status = 0;
                break;
            }
            let sig = FORWARD.swap(0, Ordering::SeqCst);
            if sig != 0 {
                libc::kill(pid, sig);
            }
        }
        // The terminal back to this process's group, for the shell it returns to.
        libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
    }
    Some(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    })
}

/// What the pane shows once the user left `name` by hand.
pub(crate) fn left_lines(name: &str, coordinator: bool, on_quit: &OnQuit) -> Vec<String> {
    let what = if coordinator {
        format!("the coordinator ({name})")
    } else {
        format!("thread {name}")
    };
    vec![
        format!("chief · you left {what}. It keeps running in the background."),
        String::new(),
        match on_quit {
            OnQuit::Nothing => "  ⏎  back to it".into(),
            _ => "  ⏎  back to it, or pick a thread on the board".into(),
        },
        match on_quit {
            OnQuit::Nothing => "  q  quit (sessions keep running)".into(),
            OnQuit::Layout => "  q  close chief's layout (sessions keep running)".into(),
            OnQuit::Board(_) => "  q  close chief and its board (sessions keep running)".into(),
        },
        String::new(),
        "Next time, Ctrl+Q quits at once, even inside Claude.".into(),
    ]
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
pub(crate) fn project_config(dir: &Path) -> Result<ProjectConfig, Vec<String>> {
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

/// `me`: this pane's pid when it recorded itself in `pane_pid`; it writes `pane_showing` only
/// while that record is still its own.
fn pane_loop(
    conn: &Connection,
    project: &str,
    me: Option<i32>,
    tty: &Tty,
    on_quit: &OnQuit,
) -> Result<()> {
    let dir = project::project_dir(project);
    let showing = |name: Option<&str>| db::set_pane_showing_as(conn, project, me, name);
    loop {
        if ending() {
            return Ok(());
        }
        let config = match project_config(&dir) {
            Ok(config) => config,
            Err(lines) => {
                tty.say(&lines[0])?;
                for line in &lines[1..] {
                    tty.note(line)?;
                }
                if tty.quit_pressed(CONFIG_RETRY) {
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
            if tty.quit_pressed(RETRY) {
                return Ok(());
            }
            continue;
        };
        let name = row.name.clone();
        let short_id = row.short_id.clone().context("target has no short id")?;
        tty.say(&format!("chief · {name} …"))?;
        let mut say = |line: &str| {
            let _ = tty.say(line);
        };
        if let Err(e) = wake(conn, claude, (&dir, &config), &row, &mut say) {
            tty.say(&format!("chief · {name}: {e:#} · retrying · q quits"))?;
            if tty.quit_pressed(RETRY) {
                return Ok(());
            }
            continue;
        }

        // Claim the pane, then look again: a board that wrote a new target before this claim
        // saw no attach to interrupt and sent nothing.
        showing(Some(&name))?;
        if resolve_target(conn, project)?.map(|r| r.name).as_deref() != Some(name.as_str()) {
            showing(None)?;
            continue;
        }
        // Drops keys that arrived meanwhile, so attach doesn't read one meant for chief (a
        // Ctrl+Z would detach it at once).
        if tty.quit_pressed(Duration::ZERO) {
            return Ok(());
        }
        let started = Instant::now();
        // A request for the target this attach already shows (the board saw the pane just
        // before it moved on) is dropped.
        let switch = || {
            SWITCH.swap(false, Ordering::SeqCst)
                && resolve_target(conn, project)
                    .map(|r| r.map(|r| r.name).as_deref() != Some(name.as_str()))
                    .unwrap_or(true)
        };
        // In the session's own account: no other account's supervisor knows it.
        let attached = attach(conn, &claude.for_row(&row), &row, &switch);
        let failed = match attached {
            Ok(Ended::HungUp) => {
                // The relay may have seen the hangup on the terminal itself, before any signal.
                HUNG_UP.store(true, Ordering::SeqCst);
                // Nothing left to draw on; `run` clears `pane_showing`.
                return Ok(());
            }
            // attach was killed and restored nothing: undo the screen modes it left on.
            Ok(Ended::Quit) => {
                tty.reset();
                return Ok(());
            }
            Ok(Ended::Switched) => {
                tty.reset();
                showing(None)?;
                continue;
            }
            Ok(Ended::Exited(Some(status))) if status.success() => None,
            Ok(Ended::Exited(Some(status))) => {
                Some(format!("claude attach {short_id} exited with {status}"))
            }
            Ok(Ended::Exited(None)) => {
                Some(format!("claude attach {short_id} closed its terminal"))
            }
            Err(e) => Some(format!("{e:#}")),
        };
        showing(None)?;
        if failed.is_some() || started.elapsed() < SHORT_ATTACH {
            let why = failed.unwrap_or_else(|| format!("claude attach {short_id} ended at once"));
            // Below whatever attach printed, which says why.
            tty.note(&format!("chief · {name}: {why} · retrying · q quits"))?;
            if tty.quit_pressed(RETRY) {
                return Ok(());
            }
            continue;
        }

        // The user left by hand: attaching again at once would put them straight back in.
        let coordinator = row.role == Role::Coordinator.as_str();
        let lines = left_lines(&name, coordinator, on_quit);
        tty.say(&lines[0])?;
        for line in &lines[1..] {
            tty.note(line)?;
        }
        if tty.pick() == Pick::Quit {
            return Ok(());
        }
    }
}

/// Respawns `row` when `claude agents` shows no process for it and waits for one, from the
/// project.toml just read (`claude` is the project's account). A failing `claude agents` is no
/// reason not to try attaching.
pub(crate) fn wake(
    conn: &Connection,
    claude: &Claude,
    (dir, config): (&Path, &ProjectConfig),
    row: &SessionRow,
    say: &mut dyn FnMut(&str),
) -> Result<()> {
    // Its own account's `claude agents` is the one that can see it.
    if accounts::refresh_row(conn, claude, row, AGENTS_TIMEOUT, db::now_ms).is_err() {
        return Ok(());
    }
    let row =
        db::get_session(conn, &row.name)?.with_context(|| format!("{} vanished", row.name))?;
    if row.pid.is_some() {
        return Ok(());
    }
    say(&format!("chief · waking {} …", row.name));
    project::respawn(conn, claude, dir, config, &row, WAKE_WAIT)?;
    Ok(())
}

/// Relays `claude attach` for `row` between a pseudo-terminal of its own and this one until it
/// exits, the board wants another target (`switch`), the user presses Ctrl+Q or the terminal
/// hangs up; chief ends it in all but the first (SIGTERM, a grace period, then SIGKILL), which
/// leaves its session running. In Rex it reports the session's state meanwhile (`rex_status`),
/// and clears the report once it shows the session no more.
fn attach(
    conn: &Connection,
    claude: &Claude,
    row: &SessionRow,
    switch: &dyn Fn() -> bool,
) -> Result<Ended> {
    let short_id = row.short_id.as_deref().context("target has no short id")?;
    let mut cmd = claude.command();
    cmd.args(["attach", short_id]);
    let rex = rex_status::enabled();
    let sent: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    let looked: Cell<Option<Instant>> = Cell::new(None);
    // The row as the hooks and the board's sync keep it, read again every STATUS_EVERY.
    let status = || {
        if !rex || looked.get().is_some_and(|at| at.elapsed() < STATUS_EVERY) {
            return None;
        }
        looked.set(Some(Instant::now()));
        let now = db::get_session(conn, &row.name).ok().flatten()?;
        let report = rex_status::report(&now);
        if sent.borrow().as_ref() == Some(&report) {
            return None;
        }
        sent.replace(Some(report.clone()));
        Some(report)
    };
    let signals = relay::Signals {
        hung_up: &|| HUNG_UP.load(Ordering::SeqCst),
        quit: &quit_requested,
        switch,
        resized: &|| RESIZED.swap(false, Ordering::SeqCst),
        status: &status,
    };
    let ended = relay::relay(cmd, &signals)
        .with_context(|| format!("running {} attach {short_id}", claude.bin.display()));
    if rex {
        // An ST first ends whatever string attach was cut off in. The terminal may be gone.
        let mut out = std::io::stdout();
        let _ = out
            .write_all(b"\x1b\\")
            .and_then(|()| out.write_all(rex_status::CLEAR))
            .and_then(|()| out.flush());
    }
    ended
}

#[derive(Debug, PartialEq)]
enum Pick {
    Back,
    Quit,
}

/// The pane's terminal, in raw mode for as long as the pane runs (Ctrl+Z and Ctrl+C are plain
/// bytes), with SIGTSTP ignored in case one gets through. Keeps the mode the pane started in, to
/// restore.
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

    /// After a `claude attach` chief ended, which restored nothing: undoes the screen modes it
    /// turned on in this terminal through the relay.
    fn reset(&self) {
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

    /// Waits for the user's answer after they left a session: ⏎ or a board selection (a switch
    /// request) attaches again, `q`, Ctrl+C, Ctrl+Q or a hangup quits. Keys in the first `SETTLE` are
    /// dropped.
    fn pick(&self) -> Pick {
        let settle = Instant::now() + SETTLE;
        loop {
            if ending() {
                return Pick::Quit;
            }
            if SWITCH.swap(false, Ordering::SeqCst) {
                return Pick::Back;
            }
            // Short looks: a switch request only sets a flag.
            let keys = read_keys(KEY_SLICE);
            if Instant::now() < settle {
                continue;
            }
            for key in keys {
                match key {
                    b'\r' | b'\n' => return Pick::Back,
                    b'q' | CTRL_C | CTRL_Q => return Pick::Quit,
                    _ => {}
                }
            }
        }
    }

    /// Reads keys for `wait`, then whatever is still queued; true when `q` or Ctrl+Q was pressed,
    /// the board asked chief to quit or the terminal hung up. Other keys are dropped. A switch request doesn't end the wait: it stays
    /// for the next attach, which drops it when it is for the target being attached.
    fn quit_pressed(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if ending() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let keys = read_keys(left.min(KEY_SLICE));
            if keys.contains(&b'q') || keys.contains(&CTRL_Q) {
                return true;
            }
            if keys.is_empty() && left.is_zero() {
                return ending();
            }
        }
    }
}

/// Waits up to `wait` for keys on the pane's terminal and returns them; nothing on a timeout or
/// a signal. A terminal that hung up (POLLHUP, POLLERR or POLLNVAL, end of file, EIO) sets
/// `HUNG_UP`. Not crossterm's reader: on a hung-up terminal it retries a read that returns 0 or
/// EIO forever, so its poll never returns, the flag is never looked at, and the pane spins at
/// full CPU until it is killed. The pane needs only `q`, Enter and Ctrl+C, so it reads bytes:
/// from an ESC on they are an escape sequence (an arrow key, a mouse report, a paste), not keys.
fn read_keys(wait: Duration) -> Vec<u8> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = wait.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: polls one descriptor, passed by a pointer to a live pollfd.
    let ready = unsafe { libc::poll(&mut fd, 1, ms) };
    if ready == 0 {
        return Vec::new();
    }
    if ready < 0 {
        // A signal: the caller looks at the flags. Any other failure would repeat at once.
        if std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            HUNG_UP.store(true, Ordering::SeqCst);
        }
        return Vec::new();
    }
    if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        HUNG_UP.store(true, Ordering::SeqCst);
        return Vec::new();
    }
    let mut buf = [0u8; 64];
    // SAFETY: reads at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        let read = &buf[..n as usize];
        return read
            .split(|&b| b == ESC)
            .next()
            .unwrap_or_default()
            .to_vec();
    }
    // A killed `claude attach` may leave the terminal non-blocking: EAGAIN is no hangup.
    let kind = std::io::Error::last_os_error().kind();
    if n == 0 || !matches!(kind, ErrorKind::Interrupted | ErrorKind::WouldBlock) {
        HUNG_UP.store(true, Ordering::SeqCst);
    }
    Vec::new()
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // SAFETY: writing a termios read from the same terminal.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.cooked) };
        // Not `println!`: it panics when the terminal has hung up.
        let _ = writeln!(std::io::stdout());
    }
}

/// Asks the project's main pane to show `ui_state.pane_target` now. The board writes the target
/// first, then calls this. It sends SIGUSR1 to the `chief pane` process, which ends its
/// `claude attach` and attaches to the new target (§9), in whatever terminal it runs. A pane
/// that shows nothing gets the signal too: it may be waiting after the user left a session, and
/// between attaches the pane drops a request for the target it is about to attach. Does nothing
/// when the pane already shows the target or when no pane is recorded. A recorded pid whose start time
/// no longer matches belongs to a pane that is gone, and may be another process's by now: it is
/// never signalled, and the record is cleared.
pub fn request_switch(conn: &Connection, project: &str) -> Result<()> {
    let showing = db::get_pane_showing(conn, project)?;
    let target =
        db::get_pane_target(conn, project)?.unwrap_or_else(|| names::coordinator_name(project));
    if showing.as_deref() == Some(target.as_str()) {
        return Ok(());
    }
    let Some(pid) = live_pane(conn, project)? else {
        return Ok(());
    };
    // SAFETY: a plain syscall, to the process just proven to be the pane.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    Ok(())
}

/// The board's Ctrl+Q: asks the running pane to quit as its own Ctrl+Q does (SIGUSR2), which closes
/// the layout, and waits up to `wait` for it to end. A board that quit at once would close its
/// own block under the pane, which closes it too.
pub fn request_quit(conn: &Connection, project: &str, wait: Duration) -> Result<()> {
    let Some(pid) = live_pane(conn, project)? else {
        return Ok(());
    };
    let started = proc::start_time(pid);
    // SAFETY: a plain syscall, to the process just proven to be the pane.
    unsafe { libc::kill(pid, libc::SIGUSR2) };
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline && proc::start_time(pid) == started {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// The recorded pane's pid while that process still runs: its start time matches
/// `pane_started`. A record whose process is gone (a pane killed with SIGKILL never clears it)
/// is cleared, with what it showed, and None returned: the pid may be another process's by now.
pub fn live_pane(conn: &Connection, project: &str) -> Result<Option<i32>> {
    let Some((pid, started)) = db::get_pane_process(conn, project)? else {
        return Ok(None);
    };
    if proc::start_time(pid).as_deref() != Some(started.as_str()) {
        db::clear_pane_process_if(conn, project, pid)?;
        return Ok(None);
    }
    Ok(Some(pid))
}

/// The session the pane shows, for §12 cleanup: `pane_showing`, unless the pane that recorded it
/// is gone (`live_pane`), which would otherwise keep cleanup away from that thread for good. A
/// pane that couldn't record itself is trusted, since nothing proves it gone.
pub fn showing(conn: &Connection, project: &str) -> Result<Option<String>> {
    live_pane(conn, project)?;
    db::get_pane_showing(conn, project)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command};

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

        // Showing nothing: waiting after the user left, even the target it left.
        let mut pane = fake_pane(&conn);
        db::set_pane_showing(&conn, "demo", None).unwrap();
        request_switch(&conn, "demo").unwrap();
        assert_eq!(ended_by(&mut pane), Some(libc::SIGUSR1));
    }

    /// The board's Ctrl+Q: SIGUSR2 to the pane, then a wait that ends with it.
    #[test]
    fn quit_signals_the_pane_and_waits_for_it() {
        let (_dir, conn) = fresh();
        let mut pane = fake_pane(&conn);
        let pid = pane.id();
        // Reaped as it ends, as the pane's own parent would.
        let reaper = std::thread::spawn(move || pane.wait().unwrap());
        let start = Instant::now();
        request_quit(&conn, "demo", Duration::from_secs(5)).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{pid} ended sooner"
        );
        assert_eq!(reaper.join().unwrap().signal(), Some(libc::SIGUSR2));

        // No pane: nothing to wait for.
        let start = Instant::now();
        request_quit(&conn, "demo", Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// A pane killed with SIGKILL never clears its record: once its process is gone, what it
    /// showed no longer keeps §12 cleanup away from that thread.
    #[test]
    fn a_gone_panes_showing_is_not_trusted() {
        let (_dir, conn) = fresh();
        let mut pane = fake_pane(&conn);
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        assert_eq!(
            showing(&conn, "demo").unwrap().as_deref(),
            Some("demo-a-1111")
        );
        pane.kill().unwrap();
        pane.wait().unwrap();
        assert_eq!(showing(&conn, "demo").unwrap(), None);
        assert_eq!(
            db::get_pane_process(&conn, "demo").unwrap(),
            None,
            "forgotten"
        );

        // A pane that couldn't record itself: nothing proves it gone.
        db::set_pane_showing(&conn, "demo", Some("demo-a-1111")).unwrap();
        assert_eq!(
            showing(&conn, "demo").unwrap().as_deref(),
            Some("demo-a-1111")
        );
    }

    #[test]
    fn leaving_says_how_to_go_back_or_quit() {
        let lines = left_lines("chief-demo", true, &OnQuit::Layout);
        assert_eq!(
            lines[0],
            "chief · you left the coordinator (chief-demo). It keeps running in the background."
        );
        assert!(lines[2].contains("pick a thread on the board"));
        assert!(lines[3].contains("close chief's layout"));
        let lines = left_lines("demo-a-1111", false, &OnQuit::Nothing);
        assert!(lines[0].contains("you left thread demo-a-1111"));
        assert_eq!(lines[2], "  ⏎  back to it");
        assert_eq!(lines[3], "  q  quit (sessions keep running)");
        assert!(lines[5].contains("Ctrl+Q quits at once"));
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
}
