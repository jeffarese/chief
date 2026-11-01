//! The report view (ARCHITECTURE §10): a thread's reports in full, newest first, one at a time
//! below its task, where the detail panel shows only the start of the last one. `v` on a thread
//! opens it; the row's last report shows until a worker's read of them all comes back.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::db::{Report, SessionRow};
use crate::text::{ago, local_hhmm};
use crate::tui::{self, dim, key_bar, panel, truncate};

use super::ui::display_title;

/// How many of a thread's reports the view reads, newest first.
pub const REPORTS_READ: usize = 50;
/// Lines one turn of the wheel scrolls.
const WHEEL_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct ReportView {
    pub name: String,
    /// The thread's title, else its name.
    pub label: String,
    pub task: Option<String>,
    /// Newest first.
    pub reports: Vec<Report>,
    /// Which report shows: 0 is the newest.
    pub shown: usize,
    /// The first line shown; each draw brings it back in range.
    pub scroll: Cell<usize>,
    /// How many lines the last draw showed, for paging.
    pub page: Cell<usize>,
}

pub enum Outcome {
    Stay,
    Close,
}

impl ReportView {
    pub fn new(row: &SessionRow) -> ReportView {
        let last = row.last_report.as_deref().filter(|r| !r.trim().is_empty());
        ReportView {
            name: row.name.clone(),
            label: display_title(row),
            task: row.task.clone(),
            reports: last
                .map(|text| Report {
                    session_name: row.name.clone(),
                    ts: row.last_report_at.unwrap_or(row.updated_at),
                    text: text.to_string(),
                })
                .into_iter()
                .collect(),
            shown: 0,
            scroll: Cell::new(0),
            page: Cell::new(1),
        }
    }

    /// The worker's read of every report; the newest still shows.
    pub fn loaded(&mut self, reports: Vec<Report>) {
        if !reports.is_empty() {
            self.reports = reports;
        }
        self.shown = self.shown.min(self.reports.len().saturating_sub(1));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        let (scroll, page) = (self.scroll.get(), self.page.get());
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'v') => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll.set(scroll.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll.set(scroll.saturating_add(1)),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll.set(scroll.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll.set(scroll.saturating_add(page)),
            KeyCode::Home | KeyCode::Char('g') => self.scroll.set(0),
            KeyCode::End | KeyCode::Char('G') => self.scroll.set(usize::MAX),
            KeyCode::Left | KeyCode::Char('h') if self.shown + 1 < self.reports.len() => {
                self.turn(self.shown + 1);
            }
            KeyCode::Right | KeyCode::Char('l') if self.shown > 0 => self.turn(self.shown - 1),
            _ => {}
        }
        Outcome::Stay
    }

    pub fn on_mouse(&mut self, kind: MouseEventKind) {
        let scroll = self.scroll.get();
        match kind {
            MouseEventKind::ScrollUp => self.scroll.set(scroll.saturating_sub(WHEEL_LINES)),
            MouseEventKind::ScrollDown => self.scroll.set(scroll.saturating_add(WHEEL_LINES)),
            _ => {}
        }
    }

    fn turn(&mut self, to: usize) {
        self.shown = to;
        self.scroll.set(0);
    }
}

/// The view over the board's whole area: which report of how many in the title, the task, the
/// report with when it came, and the keys that do something here.
pub fn draw(f: &mut Frame, area: Rect, view: &ReportView, now: i64) {
    let [main, foot] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let title = match view.reports.len() {
        0 | 1 => view.label.clone(),
        n => format!("{} · report {} of {n}", view.label, view.shown + 1),
    };
    // Too small for a frame: the bare text.
    let inner = if main.height >= 3 && main.width >= 12 {
        let block = panel(
            &truncate(&title, usize::from(area.width).saturating_sub(4)),
            true,
        );
        let inner = tui::inside(&block, main);
        f.render_widget(block, main);
        inner
    } else {
        main
    };

    let mut lines = Vec::new();
    if let Some(task) = view.task.as_deref().filter(|t| !t.trim().is_empty()) {
        lines.push(Line::styled("task", dim()));
        lines.extend(task.lines().map(|l| Line::raw(l.to_string())));
        lines.push(Line::raw(""));
    }
    match view.reports.get(view.shown) {
        Some(r) => {
            let when = format!("report · {} · {}", local_hhmm(r.ts), ago(now, r.ts));
            lines.push(Line::styled(when, dim()));
            lines.extend(r.text.lines().map(|l| Line::raw(l.to_string())));
        }
        None => lines.push(Line::styled("No report yet.", dim())),
    }
    let text = Paragraph::new(lines).wrap(Wrap { trim: false });
    let (total, height) = (text.line_count(inner.width), usize::from(inner.height));
    let scroll = view.scroll.get().min(total.saturating_sub(height));
    view.scroll.set(scroll);
    // A page keeps one line of the last in view.
    view.page.set(height.saturating_sub(1).max(1));
    let offset = u16::try_from(scroll).unwrap_or(u16::MAX);
    f.render_widget(text.scroll((offset, 0)), inner);

    let mut keys = Vec::new();
    if total > height {
        keys.push(("↑↓ space", "scroll"));
    }
    if view.shown + 1 < view.reports.len() {
        keys.push(("←", "older"));
    }
    if view.shown > 0 {
        keys.push(("→", "newer"));
    }
    keys.push(("Esc", "back"));
    f.render_widget(
        Paragraph::new(key_bar(&keys, usize::from(area.width))),
        foot,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Role;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const NOW: i64 = 1_800_000_000_000;

    fn view(reports: &[&str]) -> ReportView {
        let mut row = SessionRow::new("demo-api-1a2b", "demo", Role::Thread, NOW);
        row.title = Some("Rate limits".into());
        row.task = Some("Add per-key rate limits.".into());
        row.last_report = reports.first().map(|r| r.to_string());
        row.last_report_at = Some(NOW - 60_000);
        ReportView::new(&row)
    }

    fn render(view: &ReportView, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, f.area(), view, NOW)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
                line.trim_end().to_string() + "\n"
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    /// The row's last report shows at once, below the task; the read brings the earlier ones,
    /// which ← and → turn through, each from its top.
    #[test]
    fn it_shows_the_last_report_then_turns_through_the_earlier_ones() {
        let mut v = view(&["Opened PR 42."]);
        let out = render(&v, 50, 12);
        for want in [
            "╭ Rate limits ",
            "task",
            "Add per-key rate limits.",
            "Opened PR 42.",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(!out.contains("older"), "{out}");

        let report = |text: &str, ts: i64| Report {
            session_name: "demo-api-1a2b".into(),
            ts,
            text: text.into(),
        };
        v.loaded(vec![
            report("Opened PR 42.", NOW - 60_000),
            report("Tests pass.", NOW - 3_600_000),
        ]);
        let out = render(&v, 50, 12);
        assert!(out.contains("Rate limits · report 1 of 2"), "{out}");
        assert!(out.contains("← older"), "{out}");
        v.scroll.set(5);
        v.on_key(key(KeyCode::Left));
        assert_eq!((v.shown, v.scroll.get()), (1, 0));
        let out = render(&v, 50, 12);
        assert!(out.contains("Tests pass."), "{out}");
        assert!(out.contains("1h ago") && out.contains("→ newer"), "{out}");
        // Past the oldest nothing turns.
        v.on_key(key(KeyCode::Left));
        assert_eq!(v.shown, 1);
        v.on_key(key(KeyCode::Right));
        assert_eq!(v.shown, 0);
        assert!(matches!(v.on_key(key(KeyCode::Esc)), Outcome::Close));
    }

    /// A long report scrolls by line, page and wheel, and never past its end.
    #[test]
    fn a_long_report_scrolls_within_its_lines() {
        let long: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let mut v = view(&[&long.join("\n")]);
        let shows = |out: &str, n: usize| {
            out.lines()
                .any(|l| l.trim_matches(['│', ' ']) == format!("line {n}"))
        };
        let out = render(&v, 40, 12);
        assert!(shows(&out, 1) && !shows(&out, 40), "{out}");
        assert!(out.contains("scroll"), "{out}");
        v.on_key(key(KeyCode::End));
        let out = render(&v, 40, 12);
        assert!(shows(&out, 40) && !shows(&out, 1), "{out}");
        let end = v.scroll.get();
        v.on_key(key(KeyCode::Down));
        render(&v, 40, 12);
        assert_eq!(v.scroll.get(), end, "kept at the end");
        v.on_key(key(KeyCode::PageUp));
        assert_eq!(v.scroll.get(), end - v.page.get());
        v.on_mouse(MouseEventKind::ScrollUp);
        assert_eq!(v.scroll.get(), end - v.page.get() - WHEEL_LINES);
        v.on_key(key(KeyCode::Char('g')));
        assert_eq!(v.scroll.get(), 0);
    }

    #[test]
    fn a_thread_without_a_report_says_so() {
        let out = render(&view(&[]), 40, 8);
        assert!(out.contains("No report yet."), "{out}");
        assert!(out.contains("Add per-key rate limits."), "{out}");
    }
}
