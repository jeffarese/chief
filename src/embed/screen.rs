//! An emulated screen drawn into a ratatui buffer. alacritty_terminal parses the child's output
//! and answers most of its queries; the rest, and the encoding of our keys, mouse and paste in
//! whatever modes the child asked for, are here: Alacritty keeps its encoders in the app, not in
//! the library.

use std::cell::RefCell;
use std::io::Write as _;
use std::mem;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::term::{Config, MIN_COLUMNS, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape, Processor, Rgb, Timeout};
use alacritty_terminal::vte::{Params, Parser, Perform};
use anyhow::Result;
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};

/// A program that opens a synchronized update and dies keeps it open forever; stop honouring a
/// hold after this long so the screen can't freeze.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

/// OSC 10/11 queries get an answer; programs pick light or dark themes from it.
const FOREGROUND: Rgb = Rgb {
    r: 0xd0,
    g: 0xd0,
    b: 0xd0,
};
const BACKGROUND: Rgb = Rgb {
    r: 0x10,
    g: 0x10,
    b: 0x10,
};

const XTVERSION: &[u8] = b"\x1bP>|chief\x1b\\";
const DA3: &[u8] = b"\x1bP!|00000000\x1b\\";

/// alacritty_terminal panics when a kitty keyboard push finds 4096 modes on the stack (it trims
/// the title stack instead, which may be empty), so pushes past this depth are dropped.
const KEYBOARD_DEPTH: usize = 1024;

type Reply = Box<dyn FnMut(&[u8])>;

pub struct Screen {
    term: Term<Events>,
    parser: Processor<Hold>,
    /// A second parser over the same bytes, for what alacritty_terminal doesn't see.
    probe: Parser,
    watch: Watch,
    events: Events,
    reply: Reply,
    size: (u16, u16),
    out: Vec<u8>,
}

impl Screen {
    /// `reply` carries the emulator's answers to the child's queries back to the pty.
    pub fn new(cols: u16, rows: u16, reply: impl FnMut(&[u8]) + 'static) -> Result<Screen> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let events = Events::default();
        let config = Config {
            scrolling_history: 10_000,
            // Claude Code only tells Shift+Enter from Enter through the kitty protocol.
            kitty_keyboard: true,
            ..Config::default()
        };
        Ok(Screen {
            term: Term::new(config, &Size::new(cols, rows), events.clone()),
            parser: Processor::new(),
            probe: Parser::new(),
            watch: Watch::default(),
            events,
            reply: Box::new(reply),
            size: (cols, rows),
            out: Vec::new(),
        })
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        self.end_stale_hold();
        while !bytes.is_empty() {
            // The probe stops right after anything it acts on, so its answer follows the ones
            // alacritty_terminal gave for the bytes before it.
            let n = self.probe.advance_until_terminated(&mut self.watch, bytes);
            let (now, rest) = bytes.split_at(n);
            match self.watch.hit.take() {
                Some(Hit::Reply(answer)) => {
                    self.parser.advance(&mut self.term, now);
                    self.answer();
                    (self.reply)(answer);
                }
                Some(Hit::Drop) => {
                    // A final byte nothing handles turns the push into a no-op.
                    self.parser.advance(&mut self.term, &now[..n - 1]);
                    self.parser.advance(&mut self.term, b"y");
                }
                None => self.parser.advance(&mut self.term, now),
            }
            bytes = rest;
        }
        self.answer();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.size == (cols, rows) {
            return Ok(());
        }
        self.size = (cols, rows);
        self.term.resize(Size::new(cols, rows));
        Ok(())
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// The wheel scrolled the view back into history; a key, a paste or the wheel brings it
    /// back to the live screen.
    pub fn in_history(&self) -> bool {
        self.term.grid().display_offset() != 0
    }

    /// Copies the screen into `area` of `buf`; returns where the cursor goes when it shows.
    /// During a synchronized update the terminal hasn't seen the update yet, so this is the last
    /// complete frame.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) -> Result<Option<Position>> {
        self.end_stale_hold();
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let grid = self.term.grid();
        let w = area.width.min(self.size.0);
        let h = area.height.min(self.size.1);
        for y in 0..h {
            let row = &grid[Line(i32::from(y) - offset)];
            for x in 0..w {
                paint(
                    &row[Column(usize::from(x))],
                    &mut buf[(area.x + x, area.y + y)],
                );
            }
        }
        let cursor = content.cursor;
        if cursor.shape == CursorShape::Hidden || offset != 0 {
            return Ok(None);
        }
        let (x, y) = (cursor.point.column.0, cursor.point.line.0);
        Ok((x < usize::from(w) && (0..i32::from(h)).contains(&y))
            .then(|| Position::new(area.x + x as u16, area.y + y as u16)))
    }

    /// The bytes a key sends, or `None` when it sends nothing in the child's modes.
    pub fn key(&mut self, ev: KeyEvent) -> Result<Option<&[u8]>> {
        self.out.clear();
        encode_key(ev, *self.term.mode(), &mut self.out);
        if self.out.is_empty() {
            return Ok(None);
        }
        // Typing returns to the live screen, like any terminal.
        self.term.scroll_display(Scroll::Bottom);
        Ok(Some(&self.out))
    }

    /// The bytes a mouse event sends when the child tracks the mouse. Without tracking the wheel
    /// scrolls our own history instead. `area` is where the screen was drawn.
    pub fn mouse(&mut self, ev: MouseEvent, area: Rect) -> Result<Option<&[u8]>> {
        let mode = *self.term.mode();
        let x10 = self.watch.x10;
        if !x10 && !mode.intersects(TermMode::MOUSE_MODE) {
            let lines = match ev.kind {
                MouseEventKind::ScrollUp => 3,
                MouseEventKind::ScrollDown => -3,
                _ => 0,
            };
            if lines != 0 {
                self.term.scroll_display(Scroll::Delta(lines));
            }
            return Ok(None);
        }
        let button = |b: MouseButton| match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        let drag = !x10 && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
        let moved = !x10 && mode.contains(TermMode::MOUSE_MOTION);
        let (code, release) = match ev.kind {
            MouseEventKind::Down(b) => (button(b), false),
            MouseEventKind::Up(b) if !x10 => (button(b), true),
            MouseEventKind::Drag(b) if drag => (32 + button(b), false),
            MouseEventKind::Moved if moved => (35, false),
            MouseEventKind::ScrollUp => (64, false),
            MouseEventKind::ScrollDown => (65, false),
            MouseEventKind::ScrollLeft => (66, false),
            MouseEventKind::ScrollRight => (67, false),
            _ => return Ok(None),
        };
        // X10 reports carry no modifiers.
        let mods = if x10 {
            0
        } else {
            [
                (KeyModifiers::SHIFT, 4),
                (KeyModifiers::ALT, 8),
                (KeyModifiers::CONTROL, 16),
            ]
            .into_iter()
            .filter(|(m, _)| ev.modifiers.contains(*m))
            .map(|(_, bit)| bit)
            .sum()
        };
        let (cols, rows) = self.size;
        let x = u32::from(ev.column.saturating_sub(area.x).min(cols - 1)) + 1;
        let y = u32::from(ev.row.saturating_sub(area.y).min(rows - 1)) + 1;
        self.out.clear();
        if mode.contains(TermMode::SGR_MOUSE) {
            let b = code + mods;
            let end = if release { 'm' } else { 'M' };
            let _ = write!(self.out, "\x1b[<{b};{x};{y}{end}");
            // A wheel step is a press and its release, as Ghostty reports it.
            if code >= 64 {
                let _ = write!(self.out, "\x1b[<{b};{x};{y}m");
            }
        } else {
            let b = if release { 3 } else { code } + mods;
            let utf8 = mode.contains(TermMode::UTF8_MOUSE);
            self.out.extend_from_slice(b"\x1b[M");
            for v in [b, x, y] {
                // Past what one byte holds, the legacy encoding has no way to say where.
                match char::from_u32(32 + v) {
                    Some(c) if utf8 && c <= '\u{7ff}' => {
                        let mut tmp = [0; 4];
                        self.out
                            .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    _ if 32 + v <= 255 && !utf8 => self.out.push(32 + v as u8),
                    _ => return Ok(None),
                }
            }
        }
        Ok(Some(&self.out))
    }

    /// A paste, bracketed when the child asked for it, with ESC neutralised inside the brackets.
    pub fn paste(&mut self, text: &str) -> Result<&[u8]> {
        self.out.clear();
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            self.out.extend_from_slice(b"\x1b[200~");
            // An ESC could end the paste early and pass the rest off as typed keys.
            self.out
                .extend(text.bytes().map(|b| if b == 0x1b { b' ' } else { b }));
            self.out.extend_from_slice(b"\x1b[201~");
        } else {
            // Unbracketed, a paste is typing, and a typed newline is Enter.
            let text = text.replace("\r\n", "\r").replace('\n', "\r");
            self.out.extend_from_slice(text.as_bytes());
        }
        self.term.scroll_display(Scroll::Bottom);
        Ok(&self.out)
    }

    /// The focus report, when the child asked for them (mode 1004).
    pub fn focus(&mut self, gained: bool) -> Result<Option<&[u8]>> {
        if !self.term.mode().contains(TermMode::FOCUS_IN_OUT) {
            return Ok(None);
        }
        Ok(Some(if gained { b"\x1b[I" } else { b"\x1b[O" }))
    }

    /// vte's processor holds an update's bytes until it ends, and leaves ending a stale one to
    /// its caller.
    fn end_stale_hold(&mut self) {
        if self.parser.sync_timeout().expired() {
            self.parser.stop_sync(&mut self.term);
            self.answer();
        }
    }

    /// Sends the answers alacritty_terminal queued, in order.
    fn answer(&mut self) {
        let events = mem::take(&mut *self.events.0.borrow_mut());
        for ev in events {
            let text = match ev {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => {
                    format(self.term.colors()[index].unwrap_or_else(|| default_color(index)))
                }
                Event::TextAreaSizeRequest(format) => format(WindowSize {
                    num_lines: self.size.1,
                    num_cols: self.size.0,
                    cell_width: 1,
                    cell_height: 1,
                }),
                _ => continue,
            };
            (self.reply)(text.as_bytes());
        }
    }
}

/// The events that ask for an answer, kept until the feed that raised them returns: the
/// terminal is borrowed while it raises them, and colour answers need it.
#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, ev: Event) {
        if matches!(
            ev,
            Event::PtyWrite(_) | Event::ColorRequest(..) | Event::TextAreaSizeRequest(_)
        ) {
            self.0.borrow_mut().push(ev);
        }
    }
}

/// The synchronized update's deadline, reset by each update that begins. vte asks for 150 ms; we
/// allow `HOLD_LIMIT`, since a stale hold only ends on our next feed or draw anyway (100 ms apart
/// when idle), and a busy child's large frame gets time to arrive whole.
#[derive(Default)]
struct Hold(Option<Instant>);

impl Hold {
    fn expired(&self) -> bool {
        self.0.is_some_and(|at| Instant::now() >= at)
    }
}

impl Timeout for Hold {
    fn set_timeout(&mut self, _: Duration) {
        self.0 = Some(Instant::now() + HOLD_LIMIT);
    }

    fn clear_timeout(&mut self) {
        self.0 = None;
    }

    fn pending_timeout(&self) -> bool {
        self.0.is_some()
    }
}

/// The terminal's dimensions; it needs two columns to hold a wide character.
struct Size {
    cols: usize,
    rows: usize,
}

impl Size {
    fn new(cols: u16, rows: u16) -> Size {
        Size {
            cols: usize::from(cols).max(MIN_COLUMNS),
            rows: usize::from(rows),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

enum Hit {
    Reply(&'static [u8]),
    /// A kitty keyboard push past `KEYBOARD_DEPTH`.
    Drop,
}

/// What the probe tracks: queries vte doesn't dispatch, X10 mouse mode (which alacritty_terminal
/// ignores) and the depth of both screens' kitty keyboard stacks.
#[derive(Default)]
struct Watch {
    hit: Option<Hit>,
    x10: bool,
    alt: bool,
    /// The active screen's stack first; switching screens swaps them, as alacritty_terminal does.
    keyboard: [usize; 2],
}

impl Perform for Watch {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let first = params.iter().next().map_or(0, |p| p[0]);
        match (action, intermediates) {
            ('q', [b'>']) if first == 0 => self.hit = Some(Hit::Reply(XTVERSION)),
            ('c', [b'=']) if first == 0 => self.hit = Some(Hit::Reply(DA3)),
            ('u', [b'>']) if self.keyboard[0] >= KEYBOARD_DEPTH => self.hit = Some(Hit::Drop),
            ('u', [b'>']) => self.keyboard[0] += 1,
            ('u', [b'<']) => {
                self.keyboard[0] = self.keyboard[0].saturating_sub(usize::from(first.max(1)))
            }
            ('h' | 'l', [b'?']) => {
                let on = action == 'h';
                for p in params.iter().map(|p| p[0]) {
                    match p {
                        9 => self.x10 = on,
                        // The other tracking modes replace X10's.
                        1000 | 1002 | 1003 if on => self.x10 = false,
                        1049 if on != self.alt => {
                            self.alt = on;
                            self.keyboard.swap(0, 1);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _: bool, byte: u8) {
        // RIS resets the terminal, both keyboard stacks included.
        if intermediates.is_empty() && byte == b'c' {
            *self = Watch::default();
        }
    }

    fn terminated(&self) -> bool {
        self.hit.is_some()
    }
}

/// Copies one terminal cell into a ratatui cell.
fn paint(cell: &TermCell, out: &mut Cell) {
    out.reset();
    let flags = cell.flags;
    if flags.contains(Flags::WIDE_CHAR_SPACER) {
        // ratatui skips the column after a wide symbol itself.
        out.set_symbol("");
        return;
    }
    match cell.zerowidth() {
        _ if flags.contains(Flags::HIDDEN) => out.set_char(' '),
        Some(marks) if !marks.is_empty() => {
            let symbol: String = std::iter::once(cell.c)
                .chain(marks.iter().copied())
                .collect();
            out.set_symbol(&symbol)
        }
        // alacritty_terminal keeps the tabs it was sent, for copying.
        _ if cell.c == '\t' => out.set_char(' '),
        _ => out.set_char(cell.c),
    };
    out.fg = color(cell.fg);
    out.bg = color(cell.bg);
    for (flag, add) in [
        (Flags::BOLD, Modifier::BOLD),
        (Flags::DIM, Modifier::DIM),
        (Flags::ITALIC, Modifier::ITALIC),
        (Flags::ALL_UNDERLINES, Modifier::UNDERLINED),
        (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
        (Flags::INVERSE, Modifier::REVERSED),
    ] {
        if flags.intersects(flag) {
            out.modifier |= add;
        }
    }
}

/// Palette colours stay indices so the outer terminal's theme applies; the default foreground and
/// background mean its own.
fn color(c: ansi::Color) -> Color {
    match c {
        ansi::Color::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        ansi::Color::Indexed(i) => Color::Indexed(i),
        ansi::Color::Named(n) => match n as usize {
            i @ 0..16 => Color::Indexed(i as u8),
            _ => Color::Reset,
        },
    }
}

/// The colour a query gets when the child hasn't set it: xterm's palette, and the dark defaults.
fn default_color(index: usize) -> Rgb {
    const BASE: [u32; 16] = [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let level = |v: usize| if v == 0 { 0 } else { 55 + 40 * v as u8 };
    match index {
        0..16 => {
            let [_, r, g, b] = BASE[index].to_be_bytes();
            Rgb { r, g, b }
        }
        16..232 => {
            let i = index - 16;
            Rgb {
                r: level(i / 36),
                g: level(i / 6 % 6),
                b: level(i % 6),
            }
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232) as u8;
            Rgb { r: v, g: v, b: v }
        }
        257 => BACKGROUND,
        _ => FOREGROUND,
    }
}

/// How a key is spelled, apart from its modifiers.
#[derive(Clone, Copy)]
enum Spell {
    Char(char),
    /// Enter, Tab, Backspace and Esc: their control byte, which is also their kitty code.
    Control(u8),
    /// Arrows, Home and End: `CSI 1;m X`, or unmodified `CSI X` (`SS3 X` under DECCKM).
    Cursor(u8),
    /// F1 to F4: `CSI 1;m X`, or unmodified `SS3 X`.
    Pf(u8),
    /// `CSI n;m ~`.
    Tilde(u16),
    /// Keys only the kitty protocol has a code for.
    Kitty(u32),
}

fn spell(code: KeyCode) -> Option<Spell> {
    Some(match code {
        KeyCode::Char(c) => Spell::Char(c),
        KeyCode::Enter => Spell::Control(b'\r'),
        KeyCode::Tab | KeyCode::BackTab => Spell::Control(b'\t'),
        KeyCode::Backspace => Spell::Control(0x7f),
        KeyCode::Esc => Spell::Control(0x1b),
        KeyCode::Up => Spell::Cursor(b'A'),
        KeyCode::Down => Spell::Cursor(b'B'),
        KeyCode::Right => Spell::Cursor(b'C'),
        KeyCode::Left => Spell::Cursor(b'D'),
        KeyCode::Home => Spell::Cursor(b'H'),
        KeyCode::End => Spell::Cursor(b'F'),
        KeyCode::Insert => Spell::Tilde(2),
        KeyCode::Delete => Spell::Tilde(3),
        KeyCode::PageUp => Spell::Tilde(5),
        KeyCode::PageDown => Spell::Tilde(6),
        KeyCode::F(n @ 1..=4) => Spell::Pf(b"PQRS"[usize::from(n) - 1]),
        KeyCode::F(n @ 5..=12) => {
            Spell::Tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5])
        }
        KeyCode::F(n @ 13..=35) => Spell::Kitty(57376 + u32::from(n) - 13),
        _ => return None,
    })
}

/// `CSI key[:shifted] [;mods[:event]] [;text] end`: the kitty protocol's form, and xterm's for
/// keys with modifiers. A key of 1 is the default and goes unsaid when nothing follows it.
struct Seq {
    key: u32,
    shifted: Option<u32>,
    mods: u8,
    event: Option<u8>,
    text: Option<char>,
    end: u8,
}

impl Seq {
    fn new(key: u32, mods: u8, end: u8) -> Seq {
        Seq {
            key,
            shifted: None,
            mods,
            event: None,
            text: None,
            end,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let more = self.mods != 0 || self.event.is_some() || self.text.is_some();
        out.extend_from_slice(b"\x1b[");
        if self.key != 1 || more {
            let _ = write!(out, "{}", self.key);
        }
        if let Some(s) = self.shifted {
            let _ = write!(out, ":{s}");
        }
        if more {
            let _ = write!(out, ";{}", self.mods + 1);
        }
        if let Some(e) = self.event {
            let _ = write!(out, ":{e}");
        }
        if let Some(t) = self.text {
            let _ = write!(out, ";{}", u32::from(t));
        }
        out.push(self.end);
    }
}

/// The modifiers as the kitty protocol numbers them (less one); xterm's are the first three.
fn mod_bits(ev: &KeyEvent) -> u8 {
    let mut m = ev.modifiers;
    if ev.code == KeyCode::BackTab {
        m |= KeyModifiers::SHIFT;
    }
    [
        (KeyModifiers::SHIFT, 1),
        (KeyModifiers::ALT, 2),
        (KeyModifiers::CONTROL, 4),
        (KeyModifiers::SUPER, 8),
        (KeyModifiers::HYPER, 16),
        (KeyModifiers::META, 32),
    ]
    .into_iter()
    .filter(|(k, _)| m.contains(*k))
    .map(|(_, bit)| bit)
    .sum()
}

const SHIFT: u8 = 1;
const ALT: u8 = 2;
const CTRL: u8 = 4;

fn encode_key(ev: KeyEvent, mode: TermMode, out: &mut Vec<u8>) {
    if ev.kind == KeyEventKind::Release && !mode.contains(TermMode::REPORT_EVENT_TYPES) {
        return;
    }
    let Some(spell) = spell(ev.code) else {
        return;
    };
    let kitty = TermMode::DISAMBIGUATE_ESC_CODES
        | TermMode::REPORT_EVENT_TYPES
        | TermMode::REPORT_ALL_KEYS_AS_ESC;
    if mode.intersects(kitty) {
        kitty_key(&ev, spell, mode, out);
    } else {
        legacy_key(&ev, spell, mode.contains(TermMode::APP_CURSOR), out);
    }
}

/// xterm's encoding. The outer terminal already turned Option into Alt, or into a character,
/// before crossterm saw it: Alt here always means an Alt combination, sent as an ESC prefix.
fn legacy_key(ev: &KeyEvent, spell: Spell, app_cursor: bool, out: &mut Vec<u8>) {
    let mods = mod_bits(ev) & (SHIFT | ALT | CTRL);
    let (shift, alt, ctrl) = (mods & SHIFT != 0, mods & ALT != 0, mods & CTRL != 0);
    let esc = |out: &mut Vec<u8>| {
        if alt {
            out.push(0x1b);
        }
    };
    match spell {
        Spell::Char(c) => {
            esc(out);
            match control(c).filter(|_| ctrl) {
                Some(b) => out.push(b),
                None => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        Spell::Control(b'\t') if shift && !ctrl => {
            esc(out);
            out.extend_from_slice(b"\x1b[Z");
        }
        // Legacy bytes can't tell Shift+Enter from Enter; xterm's modifyOtherKeys form can.
        Spell::Control(b @ (b'\r' | b'\t')) if shift || ctrl => {
            let _ = write!(out, "\x1b[27;{};{b}~", mods + 1);
        }
        Spell::Control(b) => {
            esc(out);
            out.push(if b == 0x7f && ctrl { 0x08 } else { b });
        }
        Spell::Cursor(x) if mods == 0 => {
            out.extend_from_slice(&[0x1b, if app_cursor { b'O' } else { b'[' }, x])
        }
        Spell::Pf(x) if mods == 0 => out.extend_from_slice(&[0x1b, b'O', x]),
        Spell::Cursor(x) | Spell::Pf(x) => Seq::new(1, mods, x).write(out),
        Spell::Tilde(n) => Seq::new(u32::from(n), mods, b'~').write(out),
        Spell::Kitty(_) => {}
    }
}

/// The C0 control Ctrl turns a character into, on xterm's table.
fn control(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_lowercase() as u8 & 0x1f,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// The kitty keyboard protocol, for the flags the child pushed. Whatever the flags leave in
/// legacy form goes through `legacy_key`.
fn kitty_key(ev: &KeyEvent, spell: Spell, mode: TermMode, out: &mut Vec<u8>) {
    let all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let release = ev.kind == KeyEventKind::Release;
    // A press is the default; repeats and releases say what they are.
    let event = (mode.contains(TermMode::REPORT_EVENT_TYPES) && ev.kind != KeyEventKind::Press)
        .then_some(if release { 3 } else { 2 });
    let mods = mod_bits(ev);
    let legacy =
        |out: &mut Vec<u8>| legacy_key(ev, spell, mode.contains(TermMode::APP_CURSOR), out);
    let mut seq = match spell {
        Spell::Char(c) => {
            // Text stays text unless a modifier other than Shift changes what it means.
            if !(all || release || (disambiguate && mods & !SHIFT != 0)) {
                return legacy(out);
            }
            let key = if mods & SHIFT != 0 { unshifted(c) } else { c };
            let mut seq = Seq::new(u32::from(key), mods, b'u');
            if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) && key != c {
                seq.shifted = Some(u32::from(c));
            }
            if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) && !release && mods & !SHIFT == 0 {
                seq.text = Some(c);
            }
            seq
        }
        Spell::Control(b) => {
            let escape = b == 0x1b;
            // The protocol leaves Enter, Tab and Backspace releases out unless all keys are
            // escape codes.
            if release && !all && !escape {
                return;
            }
            if !(all || release || (disambiguate && (escape || mods != 0))) {
                return legacy(out);
            }
            Seq::new(u32::from(b), mods, b'u')
        }
        Spell::Kitty(code) => Seq::new(code, mods, b'u'),
        _ if mods == 0 && event.is_none() && !all => return legacy(out),
        // `CSI R` is also the cursor position report.
        Spell::Pf(b'R') => Seq::new(13, mods, b'~'),
        Spell::Cursor(x) | Spell::Pf(x) => Seq::new(1, mods, x),
        Spell::Tilde(n) => Seq::new(u32::from(n), mods, b'~'),
    };
    seq.event = event;
    seq.write(out);
}

/// The character `c`'s key types without Shift on a US layout: the kitty protocol reports keys,
/// crossterm only characters.
fn unshifted(c: char) -> char {
    match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        c => c.to_lowercase().next().unwrap_or(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> (Screen, Rc<RefCell<Vec<u8>>>) {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = replies.clone();
        let s = Screen::new(20, 4, move |b| sink.borrow_mut().extend_from_slice(b)).unwrap();
        (s, replies)
    }

    fn replies(r: &Rc<RefCell<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&mem::take(&mut *r.borrow_mut())).into_owned()
    }

    fn drawn(s: &mut Screen) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 22, 5));
        s.draw(Rect::new(1, 1, 20, 4), &mut buf).unwrap();
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (1..21)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn key(s: &mut Screen, code: KeyCode, mods: KeyModifiers) -> String {
        sent(s, KeyEvent::new(code, mods))
    }

    fn sent(s: &mut Screen, ev: KeyEvent) -> String {
        let bytes = s.key(ev).unwrap().unwrap_or_default();
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn output_is_drawn_where_the_area_is_with_its_colours() {
        let (mut s, _) = screen();
        s.feed("\x1b[31mred\x1b[0m 日本\r\n\x1b[1;38;2;1;2;3mx".as_bytes());
        let buf = drawn(&mut s);
        assert_eq!(row(&buf, 1), "red 日本");
        assert_eq!(buf[(1, 1)].fg, Color::Indexed(1));
        assert_eq!(buf[(4, 1)].fg, Color::Reset);
        // The wide character's second column is left for ratatui to skip.
        assert_eq!(buf[(6, 1)].symbol(), "");
        assert_eq!(buf[(1, 2)].fg, Color::Rgb(1, 2, 3));
        assert!(buf[(1, 2)].modifier.contains(Modifier::BOLD));
        // Nothing outside the area.
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }

    #[test]
    fn styles_tabs_and_combining_marks_are_drawn() {
        let (mut s, _) = screen();
        s.feed("\x1b[2;3;4;7;9;38;5;200;42ma\x1b[0m\te\u{301}\x1b[8mh".as_bytes());
        let buf = drawn(&mut s);
        let a = &buf[(1, 1)];
        for m in [
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINED,
            Modifier::REVERSED,
            Modifier::CROSSED_OUT,
        ] {
            assert!(a.modifier.contains(m), "{m:?}");
        }
        assert_eq!((a.fg, a.bg), (Color::Indexed(200), Color::Indexed(2)));
        assert_eq!(buf[(2, 1)].symbol(), " ");
        assert_eq!(buf[(9, 1)].symbol(), "e\u{301}");
        // Hidden text stays hidden.
        assert_eq!(buf[(10, 1)].symbol(), " ");
    }

    #[test]
    fn the_cursor_shows_only_while_visible_on_the_live_screen() {
        let (mut s, _) = screen();
        s.feed(b"\x1b[2;5H");
        let mut buf = Buffer::empty(Rect::new(0, 0, 22, 5));
        let at = s.draw(Rect::new(1, 1, 20, 4), &mut buf).unwrap();
        assert_eq!(at, Some(Position::new(5, 2)));
        s.feed(b"\x1b[?25l");
        assert_eq!(s.draw(Rect::new(1, 1, 20, 4), &mut buf).unwrap(), None);
    }

    #[test]
    fn the_childs_queries_are_answered() {
        let (mut s, replies) = screen();
        s.feed(b"\x1b[c\x1b[2;3H\x1b[6n\x1b]11;?\x07\x1b[?2026$p\x1b[18t");
        let got = String::from_utf8_lossy(&replies.borrow()).into_owned();
        // alacritty_terminal's DA1 is a VT102's; programs only wait for it, they don't read it.
        assert!(got.starts_with("\x1b[?6c"), "{got:?}");
        assert!(got.contains("\x1b[2;3R"), "{got:?}");
        assert!(got.contains("rgb:1010/1010/1010"), "{got:?}");
        assert!(got.contains("\x1b[?2026;2$y"), "{got:?}");
        assert!(got.contains("\x1b[8;4;20t"), "{got:?}");
    }

    #[test]
    fn queries_alacritty_ignores_are_answered_in_order() {
        let (mut s, r) = screen();
        s.feed(b"\x1b]10;?\x1b\\\x1b[>q\x1b[=c\x1b[>c\x1b[14t\x1b]4;1;?\x07\x1b[c");
        assert_eq!(
            replies(&r),
            "\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\\
             \x1bP>|chief\x1b\\\
             \x1bP!|00000000\x1b\\\
             \x1b[>0;2600;1c\
             \x1b[4;4;20t\
             \x1b]4;1;rgb:cdcd/0000/0000\x07\
             \x1b[?6c"
        );
        // A query split across reads.
        s.feed(b"\x1b[>");
        s.feed(b"0q");
        assert_eq!(replies(&r), "\x1bP>|chief\x1b\\");
    }

    #[test]
    fn the_kitty_keyboard_query_reports_the_pushed_flags() {
        let (mut s, r) = screen();
        s.feed(b"\x1b[?u");
        assert_eq!(replies(&r), "\x1b[?0u");
        s.feed(b"\x1b[>1u\x1b[?u");
        assert_eq!(replies(&r), "\x1b[?1u");
        s.feed(b"\x1b[<u\x1b[?u");
        assert_eq!(replies(&r), "\x1b[?0u");
    }

    #[test]
    fn endless_kitty_pushes_are_dropped_instead_of_crashing() {
        let (mut s, r) = screen();
        s.feed(&b"\x1b[>1u".repeat(KEYBOARD_DEPTH));
        s.feed(&b"\x1b[>3u".repeat(5000));
        s.feed(b"\x1b[?u");
        assert_eq!(replies(&r), "\x1b[?1u");
        // The alternate screen has a stack of its own.
        s.feed(b"\x1b[?1049h\x1b[>3u\x1b[?u\x1b[?1049l\x1b[?u");
        assert_eq!(replies(&r), "\x1b[?3u\x1b[?1u");
    }

    #[test]
    fn keys_follow_the_modes_the_child_asked_for() {
        let (mut s, _) = screen();
        assert_eq!(
            key(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL),
            "\x03"
        );
        assert_eq!(key(&mut s, KeyCode::Char('b'), KeyModifiers::ALT), "\x1bb");
        assert_eq!(key(&mut s, KeyCode::Char('é'), KeyModifiers::NONE), "é");
        assert_eq!(key(&mut s, KeyCode::Char('?'), KeyModifiers::SHIFT), "?");
        assert_eq!(key(&mut s, KeyCode::Up, KeyModifiers::NONE), "\x1b[A");
        assert_eq!(
            key(&mut s, KeyCode::Enter, KeyModifiers::SHIFT),
            "\x1b[27;2;13~"
        );
        s.feed(b"\x1b[?1h");
        assert_eq!(key(&mut s, KeyCode::Up, KeyModifiers::NONE), "\x1bOA");
        s.feed(b"\x1b[>1u");
        assert_eq!(
            key(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL),
            "\x1b[99;5u"
        );
        assert_eq!(
            key(&mut s, KeyCode::Enter, KeyModifiers::SHIFT),
            "\x1b[13;2u"
        );
    }

    #[test]
    fn legacy_keys_use_xterms_table() {
        let (mut s, _) = screen();
        let none = KeyModifiers::NONE;
        let cases = [
            (KeyCode::Enter, none, "\r"),
            (KeyCode::Enter, KeyModifiers::ALT, "\x1b\r"),
            (KeyCode::Enter, KeyModifiers::CONTROL, "\x1b[27;5;13~"),
            (KeyCode::Tab, none, "\t"),
            (KeyCode::BackTab, KeyModifiers::SHIFT, "\x1b[Z"),
            (KeyCode::Backspace, none, "\x7f"),
            (KeyCode::Backspace, KeyModifiers::CONTROL, "\x08"),
            (KeyCode::Backspace, KeyModifiers::ALT, "\x1b\x7f"),
            (KeyCode::Esc, none, "\x1b"),
            (KeyCode::Char(' '), KeyModifiers::CONTROL, "\0"),
            (KeyCode::Char('['), KeyModifiers::CONTROL, "\x1b"),
            (KeyCode::Char('4'), KeyModifiers::CONTROL, "\x1c"),
            (
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                "\x03",
            ),
            (
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                "\x1b\x18",
            ),
            (KeyCode::Char('1'), KeyModifiers::CONTROL, "1"),
            (KeyCode::Left, KeyModifiers::CONTROL, "\x1b[1;5D"),
            (
                KeyCode::Right,
                KeyModifiers::SHIFT | KeyModifiers::ALT,
                "\x1b[1;4C",
            ),
            (KeyCode::Home, none, "\x1b[H"),
            (KeyCode::End, KeyModifiers::SHIFT, "\x1b[1;2F"),
            (KeyCode::Insert, none, "\x1b[2~"),
            (KeyCode::Delete, KeyModifiers::CONTROL, "\x1b[3;5~"),
            (KeyCode::PageUp, none, "\x1b[5~"),
            (KeyCode::PageDown, none, "\x1b[6~"),
            (KeyCode::F(1), none, "\x1bOP"),
            (KeyCode::F(3), KeyModifiers::SHIFT, "\x1b[1;2R"),
            (KeyCode::F(5), none, "\x1b[15~"),
            (KeyCode::F(12), KeyModifiers::CONTROL, "\x1b[24;5~"),
        ];
        for (code, mods, want) in cases {
            assert_eq!(key(&mut s, code, mods), want, "{code:?} {mods:?}");
        }
        assert_eq!(s.key(KeyEvent::new(KeyCode::F(13), none)).unwrap(), None);
        let release = KeyEvent::new_with_kind(KeyCode::Char('a'), none, KeyEventKind::Release);
        assert_eq!(s.key(release).unwrap(), None);
        s.feed(b"\x1b[?1h");
        assert_eq!(key(&mut s, KeyCode::Home, none), "\x1bOH");
        assert_eq!(key(&mut s, KeyCode::Up, KeyModifiers::CONTROL), "\x1b[1;5A");
    }

    #[test]
    fn kitty_keys_follow_the_pushed_flags() {
        let (mut s, _) = screen();
        let none = KeyModifiers::NONE;
        let kind = |code, mods, kind| KeyEvent::new_with_kind(code, mods, kind);
        s.feed(b"\x1b[>1u");
        assert_eq!(key(&mut s, KeyCode::Char('a'), none), "a");
        assert_eq!(key(&mut s, KeyCode::Char('A'), KeyModifiers::SHIFT), "A");
        assert_eq!(key(&mut s, KeyCode::Enter, none), "\r");
        assert_eq!(key(&mut s, KeyCode::Esc, none), "\x1b[27u");
        assert_eq!(
            key(&mut s, KeyCode::Char('b'), KeyModifiers::ALT),
            "\x1b[98;3u"
        );
        assert_eq!(
            key(
                &mut s,
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            "\x1b[99;6u"
        );
        assert_eq!(
            key(&mut s, KeyCode::BackTab, KeyModifiers::SHIFT),
            "\x1b[9;2u"
        );
        assert_eq!(
            key(&mut s, KeyCode::Backspace, KeyModifiers::CONTROL),
            "\x1b[127;5u"
        );
        assert_eq!(key(&mut s, KeyCode::Up, none), "\x1b[A");
        assert_eq!(key(&mut s, KeyCode::Up, KeyModifiers::SUPER), "\x1b[1;9A");
        assert_eq!(
            key(&mut s, KeyCode::F(3), KeyModifiers::CONTROL),
            "\x1b[13;5~"
        );
        assert_eq!(key(&mut s, KeyCode::F(13), none), "\x1b[57376u");
        // Event types: releases and repeats say so; Enter's release stays unreported.
        s.feed(b"\x1b[=3u");
        let release = |code| kind(code, none, KeyEventKind::Release);
        assert_eq!(sent(&mut s, release(KeyCode::Char('a'))), "\x1b[97;1:3u");
        assert_eq!(sent(&mut s, release(KeyCode::Enter)), "");
        assert_eq!(sent(&mut s, release(KeyCode::Esc)), "\x1b[27;1:3u");
        let repeat = kind(KeyCode::Up, KeyModifiers::SHIFT, KeyEventKind::Repeat);
        assert_eq!(sent(&mut s, repeat), "\x1b[1;2:2A");
        assert_eq!(key(&mut s, KeyCode::Char('a'), none), "a");
        // All keys as escape codes, with the shifted key and the text.
        s.feed(b"\x1b[=8u");
        assert_eq!(key(&mut s, KeyCode::Char('a'), none), "\x1b[97u");
        assert_eq!(key(&mut s, KeyCode::Enter, none), "\x1b[13u");
        assert_eq!(key(&mut s, KeyCode::F(1), none), "\x1b[P");
        s.feed(b"\x1b[=28u");
        assert_eq!(
            key(&mut s, KeyCode::Char('?'), KeyModifiers::SHIFT),
            "\x1b[47:63;2;63u"
        );
        assert_eq!(key(&mut s, KeyCode::Char('a'), none), "\x1b[97;1;97u");
        assert_eq!(
            key(&mut s, KeyCode::Char('a'), KeyModifiers::CONTROL),
            "\x1b[97;5u"
        );
        // Popping the flags goes back to legacy keys.
        s.feed(b"\x1b[<u");
        assert_eq!(key(&mut s, KeyCode::Esc, none), "\x1b");
    }

    #[test]
    fn paste_is_bracketed_and_cleaned_when_the_child_asks() {
        let (mut s, _) = screen();
        assert_eq!(s.paste("a\nb").unwrap(), b"a\rb");
        assert_eq!(s.paste("a\r\nb").unwrap(), b"a\rb");
        s.feed(b"\x1b[?2004h");
        assert_eq!(s.paste("a\x1b[0m").unwrap(), b"\x1b[200~a [0m\x1b[201~");
    }

    #[test]
    fn focus_is_reported_only_when_asked_for() {
        let (mut s, _) = screen();
        assert_eq!(s.focus(true).unwrap(), None);
        s.feed(b"\x1b[?1004h");
        assert_eq!(s.focus(true).unwrap(), Some(&b"\x1b[I"[..]));
        assert_eq!(s.focus(false).unwrap(), Some(&b"\x1b[O"[..]));
    }

    #[test]
    fn the_mouse_is_reported_only_while_tracked() {
        let (mut s, _) = screen();
        let area = Rect::new(10, 2, 20, 4);
        let ev = |kind| MouseEvent {
            kind,
            column: 13,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let click = ev(MouseEventKind::Down(MouseButton::Left));
        assert_eq!(s.mouse(click, area).unwrap(), None);
        s.feed(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(s.mouse(click, area).unwrap(), Some(&b"\x1b[<0;4;2M"[..]));
        let wheel = s.mouse(ev(MouseEventKind::ScrollDown), area).unwrap();
        assert_eq!(wheel, Some(&b"\x1b[<65;4;2M\x1b[<65;4;2m"[..]));
    }

    #[test]
    fn mouse_modes_and_encodings() {
        let (mut s, _) = screen();
        let area = Rect::new(0, 0, 20, 4);
        let ev = |kind, modifiers| MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers,
        };
        let none = KeyModifiers::NONE;
        let up = ev(MouseEventKind::Up(MouseButton::Left), none);
        let drag = ev(MouseEventKind::Drag(MouseButton::Left), none);
        let moved = ev(MouseEventKind::Moved, none);
        let ctrl_right = ev(
            MouseEventKind::Down(MouseButton::Right),
            KeyModifiers::CONTROL,
        );
        // X10: presses only, without modifiers, in the legacy encoding.
        s.feed(b"\x1b[?9h");
        assert_eq!(
            s.mouse(ctrl_right, area).unwrap(),
            Some(&b"\x1b[M\"#\""[..])
        );
        assert_eq!(s.mouse(up, area).unwrap(), None);
        s.feed(b"\x1b[?1000h");
        assert_eq!(s.mouse(ctrl_right, area).unwrap(), Some(&b"\x1b[M2#\""[..]));
        assert_eq!(s.mouse(up, area).unwrap(), Some(&b"\x1b[M##\""[..]));
        assert_eq!(s.mouse(drag, area).unwrap(), None);
        s.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(s.mouse(drag, area).unwrap(), Some(&b"\x1b[<32;3;2M"[..]));
        assert_eq!(s.mouse(up, area).unwrap(), Some(&b"\x1b[<0;3;2m"[..]));
        assert_eq!(s.mouse(moved, area).unwrap(), None);
        s.feed(b"\x1b[?1003h");
        assert_eq!(s.mouse(moved, area).unwrap(), Some(&b"\x1b[<35;3;2M"[..]));
        s.feed(b"\x1b[?1003l");
        assert_eq!(s.mouse(moved, area).unwrap(), None);
    }

    #[test]
    fn the_wheel_scrolls_history_until_a_key_comes() {
        let (mut s, _) = screen();
        let area = Rect::new(0, 0, 20, 4);
        let wheel = |kind| MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        for i in 0..20 {
            s.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(!s.in_history());
        s.mouse(wheel(MouseEventKind::ScrollUp), area).unwrap();
        assert!(s.in_history());
        assert_eq!(row(&drawn(&mut s), 1), "line 14");
        key(&mut s, KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!s.in_history());
    }

    #[test]
    fn a_synchronized_update_shows_only_when_it_ends() {
        let (mut s, _) = screen();
        s.feed(b"one");
        assert_eq!(row(&drawn(&mut s), 1), "one");
        s.feed(b"\x1b[?2026h\x1b[2J\x1b[Htwo");
        assert_eq!(row(&drawn(&mut s), 1), "one");
        s.feed(b"\x1b[?2026l");
        assert_eq!(row(&drawn(&mut s), 1), "two");
    }

    #[test]
    fn a_synchronized_update_that_never_ends_shows_after_the_limit() {
        let (mut s, _) = screen();
        s.feed(b"one");
        s.feed(b"\x1b[?2026h\x1b[2J\x1b[Htwo");
        assert_eq!(row(&drawn(&mut s), 1), "one");
        std::thread::sleep(HOLD_LIMIT + Duration::from_millis(20));
        assert_eq!(row(&drawn(&mut s), 1), "two");
        // Output after it is drawn as it comes.
        s.feed(b" three");
        assert_eq!(row(&drawn(&mut s), 1), "two three");
    }

    #[test]
    fn a_resize_reflows_and_reports_the_new_size() {
        let (mut s, replies) = screen();
        s.resize(10, 2).unwrap();
        assert_eq!(s.size(), (10, 2));
        s.feed(b"\x1b[18t");
        assert!(String::from_utf8_lossy(&replies.borrow()).contains("\x1b[8;2;10t"));
    }

    #[test]
    fn a_one_column_screen_takes_wide_characters() {
        let (mut s, _) = screen();
        s.resize(1, 2).unwrap();
        s.feed("日本".as_bytes());
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 2));
        s.draw(Rect::new(0, 0, 1, 2), &mut buf).unwrap();
    }
}
