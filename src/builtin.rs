//! The built-in layout (ARCHITECTURE §9): one process draws the conversation and the board side
//! by side, for a terminal with neither Rex nor tmux. The left side runs the `chief pane` loop on
//! an embedded terminal instead of this one; it records itself as the project's pane, so the
//! board's switches (`pane_target`, then SIGUSR1) reach it as they reach `chief pane`, the board
//! beside it included. The right side is the board.

use std::io::{IsTerminal, Write, stdout};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{SetTitle, supports_keyboard_enhancement};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::{DefaultTerminal, Frame};
use rusqlite::Connection;

use crate::board::app::{Action, App, group_of};
use crate::board::{self, Board, ui};
use crate::claude::Claude;
use crate::db::{self, SessionRow};
use crate::embed::{Output, Pty, Screen};
use crate::pane::{self, OnQuit};
use crate::state::{Group, Role};
use crate::{paths, proc, project, tui};

/// How long the loop waits for an event before it looks at timers and the board's workers.
const TICK: Duration = Duration::from_millis(100);
/// A burst of output within this is drawn as one frame.
const COALESCE: Duration = Duration::from_millis(8);
/// How long the input thread waits for a key before it looks whether it should pause.
const INPUT_TICK: Duration = Duration::from_millis(50);
/// Below this width the two sides don't fit next to each other: the focused one fills the screen.
const SPLIT_MIN_WIDTH: u16 = 100;
/// The board's share of a wide terminal, as in the Rex and tmux layouts, and its narrowest.
const BOARD_PERCENT: u16 = 32;
const BOARD_MIN_WIDTH: u16 = 36;
/// How long attach gets to be reaped once it closed its terminal.
const EXIT_WAIT: Duration = Duration::from_millis(500);

enum Ev {
    Input(Event),
    /// Output of the attach started for conversation generation `.0`.
    Pty(u64, Output),
    /// The wake worker of generation `.0`: a line to show.
    Waking(u64, String),
    /// The wake worker of generation `.0` is done.
    Woke(u64, Result<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Conversation,
    Board,
}

pub fn run(project: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !stdout().is_terminal() {
        bail!("chief's built-in layout needs a terminal");
    }
    let mut board = Board::open(project)?;
    board.app.embedded = true;
    let conn = db::open(&paths::db())?;
    pane::catch_signals();
    let mut terminal = tui::init()?;
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    modes_on(enhanced);
    // The terminal's own title comes back on exit (XTWINOPS push and pop title; terminals
    // without them ignore both).
    print_raw(b"\x1b[22;0t");
    let result = run_in(project, &conn, &mut terminal, &mut board, enhanced);
    print_raw(b"\x1b[23;0t");
    modes_off(enhanced);
    tui::restore();
    result
}

/// The layout on a terminal set up for it, recorded as the project's pane until it ends.
fn run_in(
    project: &str,
    conn: &Connection,
    terminal: &mut DefaultTerminal,
    board: &mut Board,
    enhanced: bool,
) -> Result<()> {
    // Where the board's switch requests go, as for `chief pane` (§9).
    let pid = std::process::id() as i32;
    let me = match proc::start_time(pid) {
        Some(started) => {
            db::set_pane_process(conn, project, pid, &started)?;
            Some(pid)
        }
        None => None,
    };
    let (tx, rx) = mpsc::channel();
    let input = Gate::spawn(tx.clone());
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let mut split = Split {
        project,
        conn,
        me,
        tx,
        focus: Side::Conversation,
        conv: Conversation::new(Areas::of(area, Side::Conversation).conv_size()),
        enders: Vec::new(),
        title: String::new(),
    };
    let result = split.run(terminal, board, &rx, &input, enhanced);

    split.conv.end(&mut split.enders);
    for ender in split.enders.drain(..) {
        let _ = ender.join();
    }
    // Only while the record is still this layout's: a newer pane's stays.
    let _ = db::set_pane_showing_as(conn, project, me, None);
    if let Some(me) = me {
        let _ = db::clear_pane_process_if(conn, project, me);
    }
    result
}

/// Bracketed paste and focus reports for the conversation, and, where the terminal has them
/// (`enhanced`), unambiguous key codes: Claude Code tells Shift+Enter from Enter only with those.
fn modes_on(enhanced: bool) {
    let _ = execute!(stdout(), EnableBracketedPaste, EnableFocusChange);
    if enhanced {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

fn modes_off(enhanced: bool) {
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableFocusChange, DisableBracketedPaste);
}

/// The terminal hung up: SIGHUP or SIGTERM arrived, or stdin reports the hangup itself.
/// crossterm's reader spins on a hung-up terminal instead of failing, so the loop must notice.
fn hung_up() -> bool {
    if pane::hung_up() {
        return true;
    }
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd on our own stdin, no wait.
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLHUP | libc::POLLNVAL) != 0
}

/// The input thread: it reads the terminal so the loop can wait on keys and output at once, and
/// stops reading while the board hands the terminal to an editor.
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    paused: bool,
    reading: bool,
}

impl Gate {
    fn spawn(tx: Sender<Ev>) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            state: Mutex::new(GateState::default()),
            changed: Condvar::new(),
        });
        let shared = gate.clone();
        std::thread::spawn(move || {
            loop {
                {
                    let Ok(mut state) = shared.state.lock() else {
                        return;
                    };
                    state.reading = false;
                    shared.changed.notify_all();
                    while state.paused {
                        state = match shared.changed.wait(state) {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                    }
                    state.reading = true;
                }
                let ev = match event::poll(INPUT_TICK) {
                    Ok(true) => event::read(),
                    Ok(false) => continue,
                    Err(e) => Err(e),
                };
                let Ok(ev) = ev else { return };
                if tx.send(Ev::Input(ev)).is_err() {
                    return;
                }
            }
        });
        gate
    }

    /// Waits (a second at most) until the thread is between reads, and keeps it there.
    fn pause(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.paused = true;
            let _ = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(1), |s| s.reading);
        }
    }

    fn resume(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.paused = false;
            self.changed.notify_all();
        }
    }
}

/// Where each side goes. Narrow terminals show only the focused side, over the whole screen; the
/// conversation keeps its size, so switching sides doesn't make Claude redraw.
struct Areas {
    conv: Rect,
    divider: Option<Rect>,
    board: Option<Rect>,
}

impl Areas {
    fn of(area: Rect, focus: Side) -> Areas {
        if area.width < SPLIT_MIN_WIDTH {
            return Areas {
                conv: area,
                divider: None,
                board: (focus == Side::Board).then_some(area),
            };
        }
        let board_w = (area.width * BOARD_PERCENT / 100).max(BOARD_MIN_WIDTH);
        let conv_w = area.width - board_w - 1;
        Areas {
            conv: Rect::new(area.x, area.y, conv_w, area.height),
            divider: Some(Rect::new(area.x + conv_w, area.y, 1, area.height)),
            board: Some(Rect::new(area.x + conv_w + 1, area.y, board_w, area.height)),
        }
    }

    fn conv_size(&self) -> (u16, u16) {
        (self.conv.width, self.conv.height)
    }

    /// Whether the conversation is on screen at all (not under a full-screen board).
    fn conv_shown(&self) -> bool {
        self.divider.is_some() || self.board.is_none()
    }
}

struct Split<'a> {
    project: &'a str,
    conn: &'a Connection,
    me: Option<i32>,
    tx: Sender<Ev>,
    focus: Side,
    conv: Conversation,
    /// Attaches being ended off the UI thread; joined before chief exits.
    enders: Vec<JoinHandle<()>>,
    /// The terminal title last set.
    title: String,
}

impl Split<'_> {
    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        board: &mut Board,
        rx: &Receiver<Ev>,
        input: &Gate,
        enhanced: bool,
    ) -> Result<()> {
        self.start();
        loop {
            if hung_up() {
                return Ok(());
            }
            if pane::take_switch() {
                self.switch();
            }
            if let State::Waiting { at } = self.conv.state
                && Instant::now() >= at
            {
                self.start();
            }
            board.app.now = db::now_ms();
            let title = title(&board.app);
            if title != self.title {
                let _ = execute!(stdout(), SetTitle(&title));
                self.title = title;
            }
            let size = terminal.size()?;
            let areas = Areas::of(Rect::new(0, 0, size.width, size.height), self.focus);
            self.conv.resize(areas.conv_size());
            terminal.draw(|f| self.draw(f, &areas, board))?;

            let mut events = Vec::new();
            if let Ok(ev) = rx.recv_timeout(TICK) {
                events.push(ev);
                let until = Instant::now() + COALESCE;
                while let Ok(ev) = rx.recv_timeout(until.saturating_duration_since(Instant::now()))
                {
                    events.push(ev);
                }
            }
            for ev in events {
                match ev {
                    Ev::Input(ev) => {
                        if self.on_input(ev, &areas, board, terminal, input, enhanced)? {
                            return Ok(());
                        }
                    }
                    Ev::Pty(generation, out) => self.on_output(generation, out),
                    Ev::Waking(generation, line) if generation == self.conv.generation => {
                        self.conv.say(&[line]);
                    }
                    Ev::Woke(generation, result) if generation == self.conv.generation => {
                        self.woke(result);
                    }
                    Ev::Waking(..) | Ev::Woke(..) => {}
                }
            }
            board.take_messages();
        }
    }

    fn draw(&mut self, f: &mut Frame, areas: &Areas, board: &Board) {
        if areas.conv_shown() {
            let cursor = self
                .conv
                .screen
                .draw(areas.conv, f.buffer_mut())
                .ok()
                .flatten();
            if self.conv.screen.in_history() {
                history_mark(f, areas.conv);
            } else if let Some(at) = cursor
                && self.focus == Side::Conversation
                && self.conv.attached()
            {
                f.set_cursor_position(at);
            }
        }
        if let Some(div) = areas.divider {
            for y in div.top()..div.bottom() {
                f.buffer_mut()[(div.x, y)]
                    .set_symbol("│")
                    .set_style(tui::dim());
            }
        }
        if let Some(area) = areas.board {
            ui::draw_in(f, area, &board.app);
            // The side without the keyboard is dimmed.
            if self.focus == Side::Conversation {
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        let cell = &mut f.buffer_mut()[(x, y)];
                        cell.modifier.insert(Modifier::DIM);
                    }
                }
            }
        }
    }

    /// Whether the layout should close.
    fn on_input(
        &mut self,
        ev: Event,
        areas: &Areas,
        board: &mut Board,
        terminal: &mut DefaultTerminal,
        input: &Gate,
        enhanced: bool,
    ) -> Result<bool> {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Release => {}
            Event::Key(key) if switches_side(&key) => self.set_focus(match self.focus {
                Side::Conversation => Side::Board,
                Side::Board => Side::Conversation,
            }),
            Event::Key(key) => match self.focus {
                Side::Conversation => return Ok(self.conv.key(key)),
                Side::Board => {
                    for action in board.app.on_key(key) {
                        match action {
                            Action::Quit => return Ok(true),
                            Action::Edit(path) => {
                                let msg = edit(terminal, &path, input, enhanced);
                                board.app.say(msg);
                                // The editor may have set a title of its own.
                                self.title.clear();
                            }
                            other => board.dispatch(other),
                        }
                    }
                }
            },
            Event::Mouse(m) => self.on_mouse(m, areas, board),
            Event::Paste(text) if self.focus == Side::Conversation => self.conv.paste(&text),
            Event::FocusGained | Event::FocusLost if self.focus == Side::Conversation => {
                self.conv.focus(ev == Event::FocusGained);
            }
            // The next frame reads the new size.
            _ => {}
        }
        Ok(false)
    }

    fn on_mouse(&mut self, m: MouseEvent, areas: &Areas, board: &mut Board) {
        let at = Position::new(m.column, m.row);
        let on_board = areas.board.is_some_and(|b| b.contains(at));
        let on_conv = areas.conv_shown() && !on_board && areas.conv.contains(at);
        if matches!(m.kind, MouseEventKind::Down(_)) {
            if on_board {
                self.set_focus(Side::Board);
            } else if on_conv {
                self.set_focus(Side::Conversation);
            }
        }
        if on_conv {
            self.conv.mouse(m, areas.conv);
        } else if on_board {
            // The board's own clicks; Show and the like go to its workers as for keys.
            for action in board.app.on_mouse(m) {
                if !matches!(action, Action::Quit | Action::Edit(_)) {
                    board.dispatch(action);
                }
            }
        }
    }

    fn set_focus(&mut self, side: Side) {
        if self.focus != side {
            self.focus = side;
            self.conv.focus(side == Side::Conversation);
        }
    }

    fn on_output(&mut self, generation: u64, out: Output) {
        if generation != self.conv.generation {
            return;
        }
        match out {
            Output::Bytes(bytes) => self.conv.screen.feed(&bytes),
            Output::Exited => self.attach_ended(),
        }
    }

    fn showing(&self, name: Option<&str>) {
        let _ = db::set_pane_showing_as(self.conn, self.project, self.me, name);
    }

    /// §9 steps 0 to 2: project.toml, the target, then waking it on a worker.
    fn start(&mut self) {
        let conv = &mut self.conv;
        conv.generation += 1;
        let dir = project::project_dir(self.project);
        let config = match pane::project_config(&dir) {
            Ok(config) => config,
            Err(lines) => {
                conv.say(&lines);
                conv.state = State::Waiting {
                    at: Instant::now() + pane::CONFIG_RETRY,
                };
                return;
            }
        };
        let row = match pane::resolve_target(self.conn, self.project) {
            Ok(Some(row)) => row,
            Ok(None) => {
                conv.wait(&format!(
                    "chief · no coordinator yet: run chief up {} · q quits",
                    self.project
                ));
                return;
            }
            Err(e) => {
                conv.wait(&format!("chief · {e:#} · retrying · q quits"));
                return;
            }
        };
        conv.say(&[format!("chief · {} …", row.name)]);
        // The project's account, whatever this terminal's shell names (§3).
        let claude = Claude::for_account(config.claude_config_dir.clone());
        let (tx, generation) = (self.tx.clone(), conv.generation);
        let worker_row = row.clone();
        let worker_claude = claude.clone();
        std::thread::spawn(move || {
            let woke = tui::catch(|| -> Result<()> {
                let conn = db::open(&paths::db())?;
                pane::wake(
                    &conn,
                    &worker_claude,
                    (&dir, &config),
                    &worker_row,
                    &mut |line| {
                        let _ = tx.send(Ev::Waking(generation, line.to_string()));
                    },
                )
            });
            let _ = tx.send(Ev::Woke(
                generation,
                woke.unwrap_or_else(|panic| Err(anyhow!("crashed: {panic}"))),
            ));
        });
        conv.state = State::Waking { row, claude };
    }

    /// §9 steps 3 and 4: claim the pane, look at the target again, attach.
    fn woke(&mut self, result: Result<()>) {
        let State::Waking { row, claude } = std::mem::replace(&mut self.conv.state, State::idle())
        else {
            return;
        };
        if let Err(e) = result {
            self.conv
                .wait(&format!("chief · {}: {e:#} · retrying · q quits", row.name));
            return;
        }
        self.showing(Some(&row.name));
        let now = pane::resolve_target(self.conn, self.project).ok().flatten();
        if now.map(|r| r.name).as_deref() != Some(row.name.as_str()) {
            self.showing(None);
            self.start();
            return;
        }
        let Some(short_id) = row.short_id.clone() else {
            self.showing(None);
            self.conv.wait(&format!(
                "chief · {}: no short id · retrying · q quits",
                row.name
            ));
            return;
        };
        // In the session's own account: no other account's supervisor knows it.
        let mut cmd = claude.for_row(&row).command();
        cmd.args(["attach", &short_id]);
        let generation = self.conv.generation;
        let (cols, rows) = self.conv.size;
        let attached = Pty::spawn(cmd, cols, rows, self.tx.clone(), move |out| {
            Ev::Pty(generation, out)
        })
        .map_err(anyhow::Error::from)
        .and_then(|pty| {
            let mut replies = pty.replies()?;
            let screen = Screen::new(cols, rows, move |bytes| {
                let _ = replies.write_all(bytes);
            })?;
            Ok((pty, screen))
        });
        match attached {
            Ok((pty, screen)) => {
                self.conv.screen = screen;
                self.conv.state = State::Attached(Attach {
                    row,
                    short_id,
                    pty,
                    started: Instant::now(),
                });
                if self.focus != Side::Conversation {
                    self.conv.focus(false);
                }
            }
            Err(e) => {
                self.showing(None);
                self.conv.wait(&format!(
                    "chief · {}: running claude attach {short_id}: {e:#} · retrying · q quits",
                    row.name
                ));
            }
        }
    }

    /// §9 steps 5 and 6: attach closed its terminal by itself.
    fn attach_ended(&mut self) {
        let State::Attached(mut a) = std::mem::replace(&mut self.conv.state, State::idle()) else {
            return;
        };
        let deadline = Instant::now() + EXIT_WAIT;
        let mut status = a.pty.exited();
        while status.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            status = a.pty.exited();
        }
        if status.is_none() {
            self.enders.push(std::thread::spawn(move || a.pty.end()));
        }
        self.showing(None);
        let failed = match status {
            Some(s) if s.success() => None,
            Some(s) => Some(format!("claude attach {} exited with {s}", a.short_id)),
            None => Some(format!("claude attach {} closed its terminal", a.short_id)),
        };
        let name = &a.row.name;
        if failed.is_some() || a.started.elapsed() < pane::SHORT_ATTACH {
            let why =
                failed.unwrap_or_else(|| format!("claude attach {} ended at once", a.short_id));
            // Below whatever attach printed, which says why.
            self.conv
                .note(&format!("chief · {name}: {why} · retrying · q quits"));
            self.conv.state = State::Waiting {
                at: Instant::now() + pane::RETRY,
            };
            return;
        }
        // The user left by hand: attaching again at once would put them straight back in.
        let coordinator = a.row.role == Role::Coordinator.as_str();
        self.conv
            .say(&pane::left_lines(name, coordinator, &OnQuit::Layout));
        self.conv.state = State::Left {
            since: Instant::now(),
        };
    }

    /// The board asked for another target (§9): only an attach to another one ends.
    fn switch(&mut self) {
        match &self.conv.state {
            State::Attached(a) => {
                let target = pane::resolve_target(self.conn, self.project)
                    .map(|r| r.map(|r| r.name))
                    .ok();
                if target.flatten().as_deref() == Some(a.row.name.as_str()) {
                    return;
                }
                self.conv.end(&mut self.enders);
                self.showing(None);
                self.start();
            }
            // The claim after waking reads the target again.
            State::Waking { .. } => {}
            State::Waiting { .. } | State::Left { .. } => self.start(),
        }
    }
}

/// Top right of the conversation while it shows history: otherwise the earlier output looks like
/// the live screen, and typing seems to go nowhere.
fn history_mark(f: &mut Frame, conv: Rect) {
    const MARK: &str = " ↑ history · a key returns ";
    let width = tui::cols(MARK) as u16;
    if conv.width < width || conv.height == 0 {
        return;
    }
    let at = Rect::new(conv.right() - width, conv.y, width, 1);
    f.render_widget(
        ratatui::widgets::Paragraph::new(MARK).style(tui::selected()),
        at,
    );
}

/// The terminal's title: the project, and how many threads wait on the user, so a terminal tab
/// in the background says when it needs them.
fn title(app: &App) -> String {
    let name = &app.config.name;
    match app
        .threads
        .iter()
        .filter(|r| group_of(r) == Group::Waiting)
        .count()
    {
        0 => format!("chief · {name}"),
        n => format!("chief · {name} · {n} waiting"),
    }
}

fn print_raw(bytes: &[u8]) {
    let mut out = stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Ctrl+\ switches sides; Claude Code doesn't bind it. Legacy terminals report it as Ctrl+4.
fn switches_side(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// The board's editor, with the input thread and this layout's terminal modes paused: they
/// would take the editor's keys or confuse it.
fn edit(terminal: &mut DefaultTerminal, path: &Path, input: &Gate, enhanced: bool) -> String {
    input.pause();
    modes_off(enhanced);
    let msg = board::edit(terminal, path);
    modes_on(enhanced);
    input.resume();
    msg
}

/// The left side: what it shows, and where it is in the §9 loop.
struct Conversation {
    screen: Screen,
    size: (u16, u16),
    /// Bumped for each pass of the loop, so a late worker result or an ended attach's last
    /// output is told apart from the current one's.
    generation: u64,
    state: State,
}

enum State {
    /// chief's own lines are shown until `at`, then it looks again. `q` quits.
    Waiting {
        at: Instant,
    },
    /// A worker syncs the target and respawns it when it has no process.
    Waking {
        row: SessionRow,
        claude: Claude,
    },
    Attached(Attach),
    /// The user left by hand (§9 step 6): ⏎ or a board pick goes back, `q` quits.
    Left {
        since: Instant,
    },
}

impl State {
    /// A placeholder while a state is being taken apart.
    fn idle() -> State {
        State::Waiting {
            at: Instant::now() + pane::RETRY,
        }
    }
}

struct Attach {
    row: SessionRow,
    short_id: String,
    pty: Pty,
    started: Instant,
}

impl Conversation {
    fn new((cols, rows): (u16, u16)) -> Conversation {
        Conversation {
            screen: blank(cols, rows),
            size: (cols, rows),
            generation: 0,
            state: State::idle(),
        }
    }

    fn attached(&self) -> bool {
        matches!(self.state, State::Attached(_))
    }

    fn resize(&mut self, size: (u16, u16)) {
        if size == self.size {
            return;
        }
        self.size = size;
        let _ = self.screen.resize(size.0, size.1);
        if let State::Attached(a) = &self.state {
            a.pty.resize(size.0, size.1);
        }
    }

    /// Lines on a cleared screen, like `chief pane` prints them.
    fn say(&mut self, lines: &[String]) {
        self.screen = blank(self.size.0, self.size.1);
        self.screen.feed(lines.join("\r\n").as_bytes());
    }

    /// A line below what is shown, leaving whatever screen mode attach left on.
    fn note(&mut self, line: &str) {
        self.screen
            .feed(format!("\x1b[?1049l\x1b[0m\x1b[?25l\r\n{line}").as_bytes());
    }

    fn wait(&mut self, line: &str) {
        self.say(&[line.to_string()]);
        self.state = State::Waiting {
            at: Instant::now() + pane::RETRY,
        };
    }

    /// A key for the conversation; whether the layout should close.
    fn key(&mut self, key: KeyEvent) -> bool {
        match &mut self.state {
            State::Attached(a) => {
                if let Ok(Some(bytes)) = self.screen.key(key) {
                    let bytes = bytes.to_vec();
                    a.pty.write(&bytes);
                }
                false
            }
            // Still the keys that closed Claude (a third Ctrl+C), not an answer.
            State::Left { since } if since.elapsed() < pane::SETTLE => false,
            State::Left { .. } if key.code == KeyCode::Enter => {
                // Back to it: the next tick starts the loop over.
                self.state = State::Waiting { at: Instant::now() };
                false
            }
            State::Left { .. } => {
                key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
            }
            _ => key.code == KeyCode::Char('q') && key.modifiers.is_empty(),
        }
    }

    fn paste(&mut self, text: &str) {
        if let State::Attached(a) = &mut self.state
            && let Ok(bytes) = self.screen.paste(text)
        {
            let bytes = bytes.to_vec();
            a.pty.write(&bytes);
        }
    }

    fn focus(&mut self, gained: bool) {
        if let State::Attached(a) = &mut self.state
            && let Ok(Some(bytes)) = self.screen.focus(gained)
        {
            let bytes = bytes.to_vec();
            a.pty.write(&bytes);
        }
    }

    fn mouse(&mut self, m: MouseEvent, area: Rect) {
        match &mut self.state {
            State::Attached(a) => {
                if let Ok(Some(bytes)) = self.screen.mouse(m, area) {
                    let bytes = bytes.to_vec();
                    a.pty.write(&bytes);
                }
            }
            // chief's own lines scroll like any screen.
            _ => {
                let _ = self.screen.mouse(m, area);
            }
        }
    }

    /// Ends the attach, if any, off the UI thread (SIGTERM, a grace period, SIGKILL).
    fn end(&mut self, enders: &mut Vec<JoinHandle<()>>) {
        if let State::Attached(a) = std::mem::replace(&mut self.state, State::idle()) {
            enders.push(std::thread::spawn(move || a.pty.end()));
        }
    }
}

/// An empty screen for chief's own lines, without a cursor.
fn blank(cols: u16, rows: u16) -> Screen {
    let mut screen = Screen::new(cols, rows, |_| {}).expect("libghostty terminal");
    screen.feed(b"\x1b[?25l");
    screen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_terminals_split_and_narrow_ones_show_the_focused_side() {
        let a = Areas::of(Rect::new(0, 0, 140, 40), Side::Conversation);
        assert_eq!(a.conv, Rect::new(0, 0, 95, 40));
        assert_eq!(a.divider, Some(Rect::new(95, 0, 1, 40)));
        assert_eq!(a.board, Some(Rect::new(96, 0, 44, 40)));
        assert!(a.conv_shown());
        // The board keeps a usable width.
        let a = Areas::of(Rect::new(0, 0, 100, 30), Side::Board);
        assert_eq!(a.board.map(|b| b.width), Some(BOARD_MIN_WIDTH));
        assert_eq!(a.conv.width, 100 - BOARD_MIN_WIDTH - 1);

        let full = Rect::new(0, 0, 80, 24);
        let a = Areas::of(full, Side::Conversation);
        assert_eq!((a.conv, a.divider, a.board), (full, None, None));
        let a = Areas::of(full, Side::Board);
        assert_eq!(a.board, Some(full));
        assert!(!a.conv_shown());
        // Claude keeps its size while the board is over it.
        assert_eq!(a.conv_size(), (80, 24));
    }

    #[test]
    fn the_title_counts_what_waits() {
        use crate::board::app::tests::{app_with, thread};
        let app = app_with(vec![thread("w-1", Group::Waiting, 1)]);
        assert_eq!(title(&app), "chief · Demo · 1 waiting");
        let app = app_with(vec![thread("w-1", Group::Working, 1)]);
        assert_eq!(title(&app), "chief · Demo");
    }

    #[test]
    fn ctrl_backslash_switches_sides_in_either_encoding() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(switches_side(&ctrl('\\')));
        assert!(switches_side(&ctrl('4')));
        assert!(!switches_side(&KeyEvent::from(KeyCode::Char('\\'))));
        assert!(!switches_side(&ctrl('c')));
    }
}
