//! An emulated screen drawn into a ratatui buffer. libghostty parses the child's output, answers
//! its queries and encodes our keys, mouse and paste in whatever modes the child asked for.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use libghostty_vt::key::{self, Key};
use libghostty_vt::render::{CellIterator, Dirty, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, StyleColor, Underline};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
    PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, SizeReportSize,
};
use libghostty_vt::{RenderState, Terminal, focus, mouse, paste};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};

/// A program that opens a synchronized update and dies keeps it open forever; stop honouring a
/// hold after this long so the screen can't freeze.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

pub struct Screen {
    term: Terminal<'static, 'static>,
    view: Rc<RefCell<View>>,
    size: Rc<Cell<(u16, u16)>>,
    keys: key::Encoder<'static>,
    key: key::Event<'static>,
    mice: mouse::Encoder<'static>,
    mouse: mouse::Event<'static>,
    out: Vec<u8>,
}

/// The last complete frame, copied out of libghostty so a draw never needs the terminal mid-hold.
struct View {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    frame: Buffer,
    cursor: Option<Position>,
    held_since: Option<Instant>,
    text: String,
}

impl Screen {
    /// `reply` carries the emulator's answers to the child's queries back to the pty.
    pub fn new(cols: u16, rows: u16, mut reply: impl FnMut(&[u8]) + 'static) -> Result<Screen> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = Terminal::new(cols, rows)?;
        term.set_scrollback_max_lines(Some(10_000))?
            .set_terminfo_name("xterm-256color")?
            // OSC 10/11 queries get an answer; programs pick light or dark themes from it.
            .set_default_fg_color(Some(RgbColor {
                r: 0xd0,
                g: 0xd0,
                b: 0xd0,
            }))?
            .set_default_bg_color(Some(RgbColor {
                r: 0x10,
                g: 0x10,
                b: 0x10,
            }))?;
        let view = Rc::new(RefCell::new(View {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            frame: Buffer::empty(Rect::new(0, 0, cols, rows)),
            cursor: None,
            held_since: None,
            text: String::new(),
        }));
        let size = Rc::new(Cell::new((cols, rows)));
        term.on_pty_write(move |_, data| reply(data))?
            .on_size({
                let size = size.clone();
                move |_| {
                    let (columns, rows) = size.get();
                    Some(SizeReportSize {
                        rows,
                        columns,
                        cell_width: 1,
                        cell_height: 1,
                    })
                }
            })?
            .on_device_attributes(|_| {
                Some(DeviceAttributes {
                    primary: PrimaryDeviceAttributes::new(
                        ConformanceLevel::VT220,
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ],
                    ),
                    secondary: SecondaryDeviceAttributes {
                        device_type: DeviceType::VT220,
                        firmware_version: 1,
                        rom_cartridge: 0,
                    },
                    tertiary: Default::default(),
                })
            })?
            .on_xtversion(|_| Some("chief"))?
            .on_render_hold({
                let view = view.clone();
                move |term, held| view.borrow_mut().hold(term, held)
            })?;
        let keys = key::Encoder::new()?;
        Ok(Screen {
            term,
            view,
            size,
            keys,
            key: key::Event::new()?,
            mice: mouse::Encoder::new()?,
            mouse: mouse::Event::new()?,
            out: Vec::new(),
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.size.get() == (cols, rows) {
            return Ok(());
        }
        self.size.set((cols, rows));
        self.term.resize(cols, rows, 1, 1)?;
        let mut view = self.view.borrow_mut();
        view.frame.resize(Rect::new(0, 0, cols, rows));
        view.frame.reset();
        Ok(())
    }

    pub fn size(&self) -> (u16, u16) {
        self.size.get()
    }

    /// The wheel scrolled the view back into history; a key, a paste or the wheel brings it
    /// back to the live screen.
    pub fn in_history(&self) -> bool {
        !self.term.viewport_active().unwrap_or(true)
    }

    /// Copies the screen into `area` of `buf`; returns where the cursor goes when it shows.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) -> Result<Option<Position>> {
        let mut view = self.view.borrow_mut();
        if !view.holding() {
            view.capture(&self.term)?;
        }
        let frame = &view.frame;
        let w = area.width.min(frame.area.width);
        let h = area.height.min(frame.area.height);
        for y in 0..h {
            for x in 0..w {
                buf[(area.x + x, area.y + y)] = frame[(x, y)].clone();
            }
        }
        Ok(view
            .cursor
            .filter(|c| c.x < w && c.y < h)
            .map(|c| Position::new(area.x + c.x, area.y + c.y)))
    }

    /// The bytes a key sends, or `None` when the encoder has nothing for it.
    pub fn key(&mut self, ev: KeyEvent) -> Result<Option<&[u8]>> {
        let action = match ev.kind {
            KeyEventKind::Press => key::Action::Press,
            KeyEventKind::Repeat => key::Action::Repeat,
            KeyEventKind::Release => key::Action::Release,
        };
        let mods = mods(ev.modifiers);
        let (k, unshifted, text) = match ev.code {
            KeyCode::Char(c) => {
                let (k, unshifted) = physical(c);
                // Control combinations are encoded from the key, not from text.
                let text = (!ev.modifiers.contains(KeyModifiers::CONTROL)
                    || k == Key::Unidentified)
                    .then(|| c.to_string());
                (k, unshifted, text)
            }
            KeyCode::Enter => (Key::Enter, '\0', None),
            KeyCode::Tab => (Key::Tab, '\0', None),
            KeyCode::BackTab => (Key::Tab, '\0', None),
            KeyCode::Backspace => (Key::Backspace, '\0', None),
            KeyCode::Esc => (Key::Escape, '\0', None),
            KeyCode::Left => (Key::ArrowLeft, '\0', None),
            KeyCode::Right => (Key::ArrowRight, '\0', None),
            KeyCode::Up => (Key::ArrowUp, '\0', None),
            KeyCode::Down => (Key::ArrowDown, '\0', None),
            KeyCode::Home => (Key::Home, '\0', None),
            KeyCode::End => (Key::End, '\0', None),
            KeyCode::PageUp => (Key::PageUp, '\0', None),
            KeyCode::PageDown => (Key::PageDown, '\0', None),
            KeyCode::Insert => (Key::Insert, '\0', None),
            KeyCode::Delete => (Key::Delete, '\0', None),
            KeyCode::F(n) => match FKEYS.get(usize::from(n).wrapping_sub(1)) {
                Some(k) => (*k, '\0', None),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let mods = if ev.code == KeyCode::BackTab {
            mods | key::Mods::SHIFT
        } else {
            mods
        };
        // Shift is what turned the key into its text, so the encoder must not apply it again.
        let consumed = if text.is_some() && unshifted != '\0' {
            mods & key::Mods::SHIFT
        } else {
            key::Mods::empty()
        };
        self.key
            .set_action(action)
            .set_key(k)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            .set_unshifted_codepoint(unshifted)
            .set_utf8(text);
        // Not encode_to_vec: its retry reserves too little and fails on longer sequences.
        let mut buf = [0u8; 128];
        // The outer terminal already turned Option into Alt, or into a character, before
        // crossterm saw it: Alt here always means an Alt combination.
        let n = self
            .keys
            .set_options_from_terminal(&self.term)
            .set_macos_option_as_alt(key::OptionAsAlt::True)
            .encode(&self.key, &mut buf)?;
        self.out.clear();
        self.out.extend_from_slice(&buf[..n]);
        if self.out.is_empty() {
            return Ok(None);
        }
        // Typing returns to the live screen, like any terminal.
        self.term.scroll_viewport(ScrollViewport::Bottom);
        Ok(Some(&self.out))
    }

    /// The bytes a mouse event sends when the child tracks the mouse. Without tracking the wheel
    /// scrolls our own history instead. `area` is where the screen was drawn.
    pub fn mouse(&mut self, ev: MouseEvent, area: Rect) -> Result<Option<&[u8]>> {
        let tracking = self.term.is_mouse_tracking()?;
        let wheel = match ev.kind {
            MouseEventKind::ScrollUp => Some((mouse::Button::Four, -3)),
            MouseEventKind::ScrollDown => Some((mouse::Button::Five, 3)),
            MouseEventKind::ScrollLeft => Some((mouse::Button::Six, 0)),
            MouseEventKind::ScrollRight => Some((mouse::Button::Seven, 0)),
            _ => None,
        };
        if !tracking {
            if let Some((_, delta)) = wheel
                && delta != 0
            {
                self.term.scroll_viewport(ScrollViewport::Delta(delta));
            }
            return Ok(None);
        }
        let button = |b: MouseButton| match b {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Right => mouse::Button::Right,
            MouseButton::Middle => mouse::Button::Middle,
        };
        let (action, btn, pressed) = match ev.kind {
            MouseEventKind::Down(b) => (mouse::Action::Press, Some(button(b)), true),
            MouseEventKind::Up(b) => (mouse::Action::Release, Some(button(b)), false),
            MouseEventKind::Drag(b) => (mouse::Action::Motion, Some(button(b)), true),
            MouseEventKind::Moved => (mouse::Action::Motion, None, false),
            _ => (mouse::Action::Press, wheel.map(|(b, _)| b), false),
        };
        let (cols, rows) = self.size.get();
        self.mouse
            .set_mods(mods(ev.modifiers))
            .set_position(mouse::Position {
                x: f32::from(ev.column.saturating_sub(area.x)) + 0.5,
                y: f32::from(ev.row.saturating_sub(area.y)) + 0.5,
            })
            .set_action(action)
            .set_button(btn);
        self.mice
            .set_options_from_terminal(&self.term)
            .set_size(mouse::EncoderSize {
                screen_width: u32::from(cols),
                screen_height: u32::from(rows),
                cell_width: 1,
                cell_height: 1,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(pressed)
            .set_track_last_cell(true);
        let mut buf = [0u8; 64];
        self.out.clear();
        let n = self.mice.encode(&self.mouse, &mut buf)?;
        self.out.extend_from_slice(&buf[..n]);
        if wheel.is_some() {
            self.mouse.set_action(mouse::Action::Release);
            let n = self.mice.encode(&self.mouse, &mut buf)?;
            self.out.extend_from_slice(&buf[..n]);
        }
        Ok((!self.out.is_empty()).then_some(&self.out[..]))
    }

    /// A paste, bracketed when the child asked for it, with unsafe control bytes neutralised.
    pub fn paste(&mut self, text: &str) -> Result<&[u8]> {
        let bracketed = self.term.mode(Mode::BRACKETED_PASTE)?;
        let mut data = text.as_bytes().to_vec();
        self.out.resize(data.len() * 2 + 16, 0);
        let n = paste::encode(&mut data, bracketed, &mut self.out)?;
        self.out.truncate(n);
        self.term.scroll_viewport(ScrollViewport::Bottom);
        Ok(&self.out)
    }

    /// The focus report, when the child asked for them (mode 1004).
    pub fn focus(&mut self, gained: bool) -> Result<Option<&[u8]>> {
        if !self.term.mode(Mode::FOCUS_EVENT)? {
            return Ok(None);
        }
        let ev = if gained {
            focus::Event::Gained
        } else {
            focus::Event::Lost
        };
        self.out.resize(8, 0);
        let n = ev.encode(&mut self.out)?;
        self.out.truncate(n);
        Ok(Some(&self.out))
    }
}

impl View {
    fn holding(&self) -> bool {
        self.held_since.is_some_and(|t| t.elapsed() < HOLD_LIMIT)
    }

    /// A hold begins with the last complete frame still on the terminal: keep it.
    fn hold(&mut self, term: &Terminal<'static, 'static>, held: bool) {
        if held {
            if self.held_since.is_none() {
                let _ = self.capture(term);
                self.held_since = Some(Instant::now());
            }
        } else {
            self.held_since = None;
        }
    }

    fn capture(&mut self, term: &Terminal<'static, 'static>) -> Result<()> {
        let snap = self.state.update(term)?;
        let (w, h) = (self.frame.area.width, self.frame.area.height);
        if snap.dirty()? == Dirty::Full {
            self.frame.reset();
        }
        let mut rows = self.rows.update(&snap)?;
        while let Some((y, row)) = rows.next_dirty() {
            let mut cells = self.cells.update(row)?;
            let mut x = 0u16;
            while let Some(cell) = cells.next() {
                if x >= w || y >= h {
                    break;
                }
                let out = &mut self.frame[(x, y)];
                out.reset();
                x += 1;
                let raw = cell.raw_cell()?;
                if matches!(raw.wide()?, CellWide::SpacerTail | CellWide::SpacerHead) {
                    // ratatui skips the column after a wide symbol itself.
                    out.set_symbol("");
                    continue;
                }
                let style = cell.style()?;
                let mut fg = color(style.fg_color);
                let mut bg = match style.bg_color {
                    StyleColor::None => cell.bg_color()?.map(rgb).unwrap_or(Color::Reset),
                    c => color(c),
                };
                let mut m = Modifier::empty();
                for (on, add) in [
                    (style.bold, Modifier::BOLD),
                    (style.faint, Modifier::DIM),
                    (style.italic, Modifier::ITALIC),
                    (style.blink, Modifier::SLOW_BLINK),
                    (style.strikethrough, Modifier::CROSSED_OUT),
                    (style.underline != Underline::None, Modifier::UNDERLINED),
                ] {
                    if on {
                        m |= add;
                    }
                }
                if style.inverse {
                    m |= Modifier::REVERSED;
                }
                if cell.is_selected()? {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.graphemes_len()? == 0 || style.invisible {
                    out.set_symbol(" ");
                } else {
                    cell.graphemes_utf8(&mut self.text)?;
                    out.set_symbol(&self.text);
                }
                out.fg = fg;
                out.bg = bg;
                out.modifier = m;
            }
            row.set_dirty(false)?;
        }
        self.cursor = match (snap.cursor_visible()?, snap.cursor_viewport()?) {
            (true, Some(c)) => Some(Position::new(c.x, c.y)),
            _ => None,
        };
        snap.set_dirty(Dirty::Clean)?;
        Ok(())
    }
}

/// Palette colours stay indices so the outer terminal's theme applies; unset means its default.
fn color(c: StyleColor) -> Color {
    match c {
        StyleColor::None => Color::Reset,
        StyleColor::Palette(i) => Color::Indexed(i.0),
        StyleColor::Rgb(c) => rgb(c),
    }
}

fn rgb(c: RgbColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn mods(m: KeyModifiers) -> key::Mods {
    let mut out = key::Mods::empty();
    for (from, to) in [
        (KeyModifiers::SHIFT, key::Mods::SHIFT),
        (KeyModifiers::ALT, key::Mods::ALT),
        (KeyModifiers::CONTROL, key::Mods::CTRL),
        (KeyModifiers::SUPER, key::Mods::SUPER),
    ] {
        if m.contains(from) {
            out |= to;
        }
    }
    out
}

const FKEYS: [Key; 12] = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
];

const LETTERS: [Key; 26] = [
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
];

const DIGITS: [Key; 10] = [
    Key::Digit0,
    Key::Digit1,
    Key::Digit2,
    Key::Digit3,
    Key::Digit4,
    Key::Digit5,
    Key::Digit6,
    Key::Digit7,
    Key::Digit8,
    Key::Digit9,
];

/// The key that types `c` on a US layout, and the character it types unshifted. crossterm only
/// reports characters; the encoder needs keys for control combinations and the kitty protocol.
fn physical(c: char) -> (Key, char) {
    let base = match c {
        'A'..='Z' => c.to_ascii_lowercase(),
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
        c => c,
    };
    let key = match base {
        'a'..='z' => LETTERS[(base as u8 - b'a') as usize],
        '0'..='9' => DIGITS[(base as u8 - b'0') as usize],
        ' ' => Key::Space,
        '`' => Key::Backquote,
        '-' => Key::Minus,
        '=' => Key::Equal,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        '\'' => Key::Quote,
        ',' => Key::Comma,
        '.' => Key::Period,
        '/' => Key::Slash,
        _ => return (Key::Unidentified, '\0'),
    };
    (key, base)
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
        let bytes = s
            .key(KeyEvent::new(code, mods))
            .unwrap()
            .unwrap_or_default();
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
    fn the_childs_queries_are_answered() {
        let (mut s, replies) = screen();
        s.feed(b"\x1b[c\x1b[2;3H\x1b[6n\x1b]11;?\x07\x1b[?2026$p\x1b[18t");
        let got = String::from_utf8_lossy(&replies.borrow()).into_owned();
        assert!(got.starts_with("\x1b[?62;"), "{got:?}");
        assert!(got.contains("\x1b[2;3R"), "{got:?}");
        assert!(got.contains("rgb:1010/1010/1010"), "{got:?}");
        assert!(got.contains("\x1b[?2026;2$y"), "{got:?}");
        assert!(got.contains("\x1b[8;4;20t"), "{got:?}");
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
        // Longer than encode_to_vec's first buffer.
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
    fn paste_is_bracketed_and_cleaned_when_the_child_asks() {
        let (mut s, _) = screen();
        assert_eq!(s.paste("a\nb").unwrap(), b"a\rb");
        s.feed(b"\x1b[?2004h");
        assert_eq!(s.paste("a\x1b[0m").unwrap(), b"\x1b[200~a [0m\x1b[201~");
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
    fn a_resize_reflows_and_reports_the_new_size() {
        let (mut s, replies) = screen();
        s.resize(10, 2).unwrap();
        assert_eq!(s.size(), (10, 2));
        s.feed(b"\x1b[18t");
        assert!(String::from_utf8_lossy(&replies.borrow()).contains("\x1b[8;2;10t"));
    }
}
