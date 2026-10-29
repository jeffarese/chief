//! What chief's TUIs (the board, §10; the launcher, §10a) share: one look (framed panels, a key
//! bar that always fits), width-aware text cutting, and a terminal with the mouse turned on.

use std::cell::RefCell;
use std::io::stdout;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::CellWidth;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};
use ratatui::{DefaultTerminal, Terminal};

/// The one colour for focus, keys and the selection mark. It reads on dark and light terminals.
pub const ACCENT: Color = Color::Cyan;

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// The highlighted row of a list.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// A rounded panel titled ` title `; `focused` draws its border in the accent colour.
pub fn panel(title: &str, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(ACCENT)
    } else {
        dim()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    if title.is_empty() {
        block
    } else {
        block.title(Span::styled(format!(" {title} "), bold()))
    }
}

/// The inside of `block` drawn over `area`, with a column of margin on each side when the
/// panel is wide enough to spare it.
pub fn inside(block: &Block, area: Rect) -> Rect {
    let inner = block.inner(area);
    if inner.width < 34 {
        return inner;
    }
    Rect {
        x: inner.x + 1,
        width: inner.width - 2,
        ..inner
    }
}

/// `⏎ show  w wake  ? help`: as many of `hints` (most important first) as fit in `width`, whole
/// hints only, never one cut in half.
pub fn key_bar(hints: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, what) in hints {
        let gap = if spans.is_empty() { 0 } else { 2 };
        let need = gap + cols(key) + 1 + cols(what);
        if used + need > width {
            break;
        }
        if gap > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}"), dim()));
        used += need;
    }
    Line::from(spans)
}

/// The graphemes of `s` with the columns each takes, as ratatui splits and measures them when it
/// draws: emoji with a variation selector or a zero-width joiner are one grapheme, not several
/// characters.
fn graphemes(s: &str) -> Vec<(String, usize)> {
    Span::raw(s)
        .styled_graphemes(Style::new())
        .map(|g| (g.symbol.to_string(), usize::from(g.symbol.cell_width())))
        .collect()
}

/// Terminal columns `s` takes.
pub fn cols(s: &str) -> usize {
    Span::raw(s)
        .styled_graphemes(Style::new())
        .map(|g| usize::from(g.symbol.cell_width()))
        .sum()
}

/// Cuts `s` to `width` terminal columns, ending in "…" when cut.
pub fn truncate(s: &str, width: usize) -> String {
    if cols(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for (g, w) in graphemes(s) {
        if used + w > width - 1 {
            break;
        }
        out.push_str(&g);
        used += w;
    }
    out.push('…');
    out
}

/// Cuts `s` from the left to `width` columns, so text being typed shows its end (the cursor).
pub fn tail(s: &str, width: usize) -> String {
    if cols(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for (g, w) in graphemes(s).into_iter().rev() {
        if used + w > width {
            break;
        }
        kept.push(g);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// `path` with the home folder as `~`, cut from the left to `width` columns: the end of a
/// path is the part that tells folders apart.
pub fn path(path: &std::path::Path, width: usize) -> String {
    let text = path.display().to_string();
    let home = dirs::home_dir().map(|h| h.display().to_string());
    let text = match home.as_deref().and_then(|h| text.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => text,
    };
    tail(&text, width)
}

/// `text` with the home folder written `~`, for messages that hold paths.
pub fn home_relative(text: &str) -> String {
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(home) if home.len() > 1 => relative_to(text, &home),
        _ => text.to_string(),
    }
}

/// `text` with `home/` written `~/` where a path starts: at the start, or after whitespace, a
/// quote or `(`. Elsewhere it is part of a longer path (`/private/tmp/…` with a home under
/// `/tmp`), and `~` there would name another folder.
fn relative_to(text: &str, home: &str) -> String {
    let needle = format!("{home}/");
    let mut out = String::new();
    let mut rest = text;
    let mut before: Option<char> = None;
    while let Some(i) = rest.find(&needle) {
        let prev = rest[..i].chars().next_back().or(before);
        out.push_str(&rest[..i]);
        let starts_path =
            prev.is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '('));
        out.push_str(if starts_path { "~/" } else { &needle });
        before = Some('/');
        rest = &rest[i + needle.len()..];
    }
    out.push_str(rest);
    out
}

/// `~` and `~/…` as the home folder; any other text as a path.
pub fn expand_home(text: &str) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    match text.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if text == "~" => home(),
        None => PathBuf::from(text),
    }
}

/// A box centred in `area`, at most `w`×`h`, never larger than `area`.
pub fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Whether chief's terminal is set up (between `init` and `restore`).
static ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Where this thread last panicked, for `catch`'s message.
    static PANICKED_AT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// What the panic hook does with a panic on `current` while the terminal belongs to `owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnPanic {
    /// The UI thread: the program is going down, so give the shell its terminal back first.
    Restore,
    /// A worker while the UI runs: the UI goes on and `catch` reports it. Printed, the message
    /// would land in the middle of the screen, which raw mode doesn't scroll.
    Quiet,
    /// No UI: the usual report.
    Report,
}

fn on_panic(owner: ThreadId, current: ThreadId, active: bool) -> OnPanic {
    match (active, owner == current) {
        (true, true) => OnPanic::Restore,
        (true, false) => OnPanic::Quiet,
        (false, _) => OnPanic::Report,
    }
}

/// The alternate screen, raw mode and mouse reporting, with a panic hook that undoes them for a
/// panic on this thread only. Set up here rather than with `ratatui::init`, whose hook restores
/// the terminal for a panic on any thread: a worker's panic would leave the board drawing over
/// the shell with raw mode off.
pub fn init() -> Result<DefaultTerminal> {
    let owner = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        PANICKED_AT.with(|p| *p.borrow_mut() = at);
        let current = std::thread::current().id();
        match on_panic(owner, current, ACTIVE.load(Ordering::SeqCst)) {
            OnPanic::Restore => {
                restore();
                previous(info);
            }
            OnPanic::Quiet => {}
            OnPanic::Report => previous(info),
        }
    }));
    ACTIVE.store(true, Ordering::SeqCst);
    let setup = || -> Result<DefaultTerminal> {
        enable_raw_mode().context("turning on raw mode")?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
            .context("entering the alternate screen")?;
        Terminal::new(CrosstermBackend::new(stdout())).context("opening the terminal")
    };
    setup().inspect_err(|_| restore())
}

/// Mouse reporting, the alternate screen and raw mode off. Mouse reporting left on would print
/// escape codes into the shell at every mouse move.
pub fn restore() {
    ACTIVE.store(false, Ordering::SeqCst);
    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen);
}

/// Runs `job`, turning a panic into its message (`boom at src/x.rs:12`), so a worker that panics
/// still sends its result and the UI doesn't wait on it for good.
pub fn catch<T>(job: impl FnOnce() -> T) -> std::result::Result<T, String> {
    std::panic::catch_unwind(AssertUnwindSafe(job)).map_err(|payload| {
        let what = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic".to_string());
        match PANICKED_AT.with(|p| p.borrow_mut().take()) {
            Some(at) => format!("{what} at {at}"),
            None => what,
        }
    })
}

/// Hands the terminal to a program for a while (an editor, the coordinator's pane): mouse
/// reporting, the alternate screen and raw mode go, then come back after `run`.
pub fn suspended<T>(terminal: &mut DefaultTerminal, run: impl FnOnce() -> T) -> T {
    let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let out = run();
    let _ = enable_raw_mode();
    let _ = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture);
    let _ = terminal.clear();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_key_bar_drops_whole_hints() {
        let hints = [("⏎", "show"), ("w", "wake"), ("?", "help")];
        assert_eq!(text(&key_bar(&hints, 80)), "⏎ show  w wake  ? help");
        assert_eq!(text(&key_bar(&hints, 15)), "⏎ show  w wake");
        assert_eq!(text(&key_bar(&hints, 5)), "");
    }

    #[test]
    fn cutting_text() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(truncate("日本語", 4), "日…");
    }

    /// Emoji with a variation selector or a joiner take the columns ratatui draws them in.
    #[test]
    fn widths_are_counted_per_grapheme() {
        assert_eq!(cols("✔\u{fe0f} done"), 2 + 5);
        assert_eq!(cols("👩\u{200d}💻"), 2);
        assert_eq!(truncate("👩\u{200d}💻 coder", 4), "👩\u{200d}💻 …");
        assert_eq!(tail("coder 👩\u{200d}💻", 4), "… 👩\u{200d}💻");
    }

    #[test]
    fn home_becomes_tilde_only_where_a_path_starts() {
        let home = "/tmp/x/home";
        assert_eq!(
            relative_to("Created /private/tmp/x/home/work/payments-api.", home),
            "Created /private/tmp/x/home/work/payments-api."
        );
        assert_eq!(
            relative_to(
                "/tmp/x/home/a and /tmp/x/home/b (/tmp/x/home/c) '/tmp/x/home/d'",
                home
            ),
            "~/a and ~/b (~/c) '~/d'"
        );
        assert_eq!(
            relative_to("/tmp/x/home/tmp/x/home/a", home),
            "~/tmp/x/home/a"
        );
    }

    #[test]
    fn tilde_alone_is_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/code"), home.join("code"));
        assert_eq!(expand_home("~code"), PathBuf::from("~code"));
        assert_eq!(expand_home("/r/api"), PathBuf::from("/r/api"));
    }

    /// Only a panic on the thread that set the terminal up restores it; a worker's panic leaves
    /// the UI running, and with no UI a panic is reported as usual.
    #[test]
    fn only_the_ui_thread_restores_the_terminal() {
        let ui = std::thread::current().id();
        let worker = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        assert_eq!(on_panic(ui, ui, true), OnPanic::Restore);
        assert_eq!(on_panic(ui, worker, true), OnPanic::Quiet);
        assert_eq!(on_panic(ui, ui, false), OnPanic::Report);
        assert_eq!(on_panic(ui, worker, false), OnPanic::Report);
    }

    #[test]
    fn catch_turns_a_panic_into_its_message() {
        assert_eq!(catch(|| 7), Ok(7));
        assert_eq!(catch(|| -> u8 { panic!("boom") }), Err("boom".to_string()));
        let n = 3;
        assert_eq!(
            catch(|| -> u8 { panic!("{n} booms") }),
            Err("3 booms".to_string())
        );
    }
}
