//! What chief's TUIs (the board, §10; the launcher, §10a) share: one look (framed panels, a key
//! bar that always fits), width-aware text cutting, and a terminal with the mouse turned on.

use std::io::stdout;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};
use unicode_width::UnicodeWidthChar;

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

/// Terminal columns `s` takes.
pub fn cols(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
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
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
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
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        kept.push(c);
        used += w;
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
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
        Some(home) if home.len() > 1 => text.replace(&format!("{home}/"), "~/"),
        _ => text.to_string(),
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

/// `ratatui::init` (alternate screen, raw mode, a panic hook that restores both) plus mouse
/// reporting, which the panic hook turns off too: left on, every mouse move would print escape
/// codes into the shell.
pub fn init() -> DefaultTerminal {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous(info);
    }));
    let terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    terminal
}

pub fn restore() {
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
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
}
