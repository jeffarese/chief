//! Drawing the board. It lives in a narrow right-hand pane, so every line is cut to the width with
//! an ellipsis (never wrapped) except the detail panel's text.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::db::SessionRow;
use crate::state::Group;
use crate::text::ago;

use super::app::{App, Confirm, Item, Screen, group_of};
use super::settings::{Field, SettingsForm};

/// Below this many body lines the detail panel replaces the list (Tab) instead of sharing.
const SPLIT_MIN_BODY: u16 = 16;
const TARGET_MARK: &str = "▸";

pub const HELP: &[(&str, &str)] = &[
    ("↑↓ j k", "move"),
    ("Enter", "show thread in main pane"),
    ("Esc c", "show coordinator"),
    ("w", "wake (respawn) thread"),
    ("r", "resolve / reopen"),
    ("o", "open pull request"),
    ("x", "clean up resolved thread"),
    ("R", "show / hide resolved"),
    ("Tab", "detail (short terminals)"),
    ("s", "settings"),
    ("?", "this help"),
    ("q", "quit the board"),
];

fn style_of(group: Group) -> Style {
    let s = Style::new();
    match group {
        Group::Waiting => s.fg(Color::Red).add_modifier(Modifier::BOLD),
        Group::Working => s.fg(Color::Green),
        Group::Review => s.fg(Color::Blue),
        Group::Landing => s.fg(Color::Magenta),
        Group::Idle => s,
        Group::Failed => s.fg(Color::Red),
        Group::Resolved => s.add_modifier(Modifier::DIM),
    }
}

fn glyph(group: Group) -> &'static str {
    match group {
        Group::Waiting => "!",
        Group::Working => "●",
        Group::Review => "◆",
        Group::Landing => "▲",
        Group::Idle => "○",
        Group::Failed => "✗",
        Group::Resolved => "✓",
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Cuts `s` to `width` terminal columns, ending in "…" when cut.
pub fn truncate(s: &str, width: usize) -> String {
    let full: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if full <= width {
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
fn tail(s: &str, width: usize) -> String {
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

fn cols(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Short tags after a thread's title.
pub fn tags(row: &SessionRow) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(reason) = &row.waiting_reason {
        tags.push(reason.clone());
    }
    if row.pid.is_none() && !row.resolved && row.short_id.is_some() {
        tags.push("asleep".to_string());
    }
    if let Some(state) = row.pr_state.as_deref() {
        let pr = match (state, row.pr_draft, row.pr_review.as_deref()) {
            ("OPEN", Some(true), _) => "PR draft",
            ("OPEN", _, Some("APPROVED")) => "PR approved",
            ("OPEN", _, Some("CHANGES_REQUESTED")) => "PR changes",
            ("OPEN", _, _) => "PR open",
            ("MERGED", _, _) => "PR merged",
            ("CLOSED", _, _) => "PR closed",
            _ => "PR",
        };
        tags.push(pr.to_string());
    }
    if row.cleaned_at.is_some() {
        tags.push("cleaned up".to_string());
    } else if row.cleanup_blocked.is_some() {
        tags.push("cleanup blocked".to_string());
    }
    tags
}

pub fn display_title(row: &SessionRow) -> String {
    row.title
        .as_deref()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| row.name.clone())
}

/// `▸● Title  asleep · PR open`, cut to `width`. The title gives way first, down to a few
/// columns, so the tags stay readable.
fn thread_line(row: &SessionRow, width: usize, selected: bool, target: bool) -> Line<'static> {
    let group = group_of(row);
    let mark = if target { TARGET_MARK } else { " " };
    let tags = tags(row).join(" · ");
    let avail = width.saturating_sub(3);
    let title = display_title(row);
    let title_room = if tags.is_empty() {
        avail
    } else {
        avail.saturating_sub(cols(&tags) + 2).max(avail.min(12))
    };
    let title = truncate(&title, title_room);
    let tag_room = avail.saturating_sub(cols(&title) + 2);
    let mut spans = vec![
        Span::raw(mark.to_string()),
        Span::styled(glyph(group).to_string(), style_of(group)),
        Span::raw(" "),
        Span::styled(
            title,
            if group == Group::Resolved {
                dim()
            } else {
                Style::new()
            },
        ),
    ];
    if !tags.is_empty() && tag_room > 1 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(truncate(&tags, tag_room), dim()));
    }
    let mut line = Line::from(spans);
    if selected {
        let used: usize = line.spans.iter().map(|s| cols(&s.content)).sum();
        line.spans
            .push(Span::raw(" ".repeat(width.saturating_sub(used))));
        line = line.style(Style::new().add_modifier(Modifier::REVERSED));
    }
    line
}

fn header_line(group: Group, count: usize, collapsed: bool, width: usize) -> Line<'static> {
    let mut text = format!("{} {count}", group.label());
    if collapsed {
        text.push_str(" · R shows");
    }
    let style = match group {
        Group::Waiting => style_of(group),
        _ => Style::new().add_modifier(Modifier::BOLD),
    };
    Line::styled(truncate(&text, width), style)
}

pub fn coordinator_state(app: &App) -> String {
    match &app.coordinator {
        None => "not started (chief up)".to_string(),
        Some(c) if c.pid.is_none() => "asleep".to_string(),
        Some(c) => {
            let mut s = group_of(c).label().to_lowercase();
            if let Some(r) = &c.waiting_reason {
                s.push_str(&format!(" ({r})"));
            }
            s
        }
    }
}

fn header(app: &App, width: usize, room: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        truncate(&app.config.name, width),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(goal) = app.config.goal.as_deref().filter(|g| !g.trim().is_empty()) {
        lines.push(Line::styled(truncate(goal, width), dim()));
    }
    let mark = if app.pane_target.is_none() {
        TARGET_MARK
    } else {
        " "
    };
    let mut coord = format!("{mark}coordinator: {}", coordinator_state(app));
    if app.denials > 0 {
        let noun = if app.denials == 1 {
            "denial"
        } else {
            "denials"
        };
        coord.push_str(&format!(" · {} guard {noun}", app.denials));
    }
    lines.push(Line::raw(truncate(&coord, width)));
    // On a tiny terminal keep the name and the coordinator line.
    if lines.len() as u16 > room && lines.len() == 3 {
        lines.remove(1);
    }
    lines.truncate(room as usize);
    lines
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    let mut prefix = String::new();
    if app.pr.blocked > 0 || app.pr.errors > 0 {
        let mut parts = Vec::new();
        if app.pr.blocked > 0 {
            parts.push(format!("{} blocked", app.pr.blocked));
        }
        if app.pr.errors > 0 {
            parts.push(format!("{} errors", app.pr.errors));
        }
        prefix = format!("PR {} · ", parts.join(", "));
    }
    let (text, style) = if let Some(busy) = &app.busy {
        (busy.clone(), Style::new().fg(Color::Blue))
    } else if let Some(status) = &app.status {
        (status.clone(), Style::new())
    } else if let Some(e) = &app.refresh_error {
        (format!("refresh failed: {e}"), Style::new().fg(Color::Red))
    } else if let Some(e) = &app.pr_error {
        (
            format!("PR refresh failed: {e}"),
            Style::new().fg(Color::Red),
        )
    } else if let Some(hint) = &app.pr_hint {
        (hint.clone(), dim())
    } else {
        (
            "⏎ show · w wake · r resolve · x clean · s settings · ? help · q quit".to_string(),
            dim(),
        )
    };
    let text = format!(
        "{prefix}{}",
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    Line::styled(truncate(&text, width), style)
}

fn detail(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        let text = if app.threads.is_empty() {
            "Ask the coordinator for work; its threads show up here."
        } else {
            "No thread selected."
        };
        return vec![Line::styled(text, dim())];
    };
    let group = group_of(row);
    let mut lines = vec![
        Line::styled(
            display_title(row),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::styled(truncate(&row.name, width), dim()),
    ];
    let mut state = group.label().to_string();
    if let Some(r) = &row.waiting_reason {
        state.push_str(&format!(": {r}"));
    }
    if let (Group::Resolved, Some(r)) = (group, &row.resolved_reason) {
        state.push_str(&format!(" ({r})"));
    }
    if row.pid.is_none() && row.short_id.is_some() && row.cleaned_at.is_none() {
        state.push_str(" · asleep");
    }
    lines.push(Line::styled(state, style_of(group)));
    let field = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label} "), dim()),
            Span::raw(value),
        ])
    };
    if let Some(b) = &row.branch {
        lines.push(field("branch", b.clone()));
    }
    if let Some(url) = &row.pr_url {
        lines.push(field("PR", url.clone()));
    }
    let model = match (&row.model, &row.effort) {
        (Some(m), Some(e)) => Some(format!("{m} · {e}")),
        (Some(m), None) => Some(m.clone()),
        (None, Some(e)) => Some(e.clone()),
        (None, None) => None,
    };
    if let Some(m) = model {
        lines.push(field("model", m));
    }
    lines.push(field("active", ago(app.now, row.last_activity_at)));
    if row.cleaned_at.is_some() {
        lines.push(field("cleanup", "cleaned up".to_string()));
    } else if let Some(why) = &row.cleanup_blocked {
        lines.push(field("cleanup blocked", why.clone()));
    }
    match row.last_report.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(report) => {
            lines.push(Line::raw(""));
            lines.extend(report.lines().map(|l| Line::raw(l.to_string())));
        }
        None => lines.push(Line::styled("No report yet.", dim())),
    }
    lines
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Screen::Settings(form) = &app.screen {
        draw_settings(f, area, app, form);
        return;
    }
    let width = area.width as usize;
    let head = header(app, width, (area.height.saturating_sub(2) / 3).clamp(1, 3));
    let [head_area, body, foot] = Layout::vertical([
        Constraint::Length(head.len() as u16),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(head), head_area);
    f.render_widget(Paragraph::new(status_line(app, width)), foot);

    if body.height >= SPLIT_MIN_BODY {
        // The list takes what it needs up to 3/5 of the body; the detail gets the rest.
        let wanted = if app.loaded { app.items().len() } else { 1 } as u16;
        let list_h = wanted.clamp(3, body.height * 3 / 5);
        let [list_area, detail_area] =
            Layout::vertical([Constraint::Length(list_h), Constraint::Min(0)]).areas(body);
        draw_list(f, list_area, app);
        draw_detail(f, detail_area, app);
    } else if app.detail_open {
        draw_detail(f, body, app);
    } else {
        draw_list(f, body, app);
    }

    match &app.screen {
        Screen::Help => draw_help(f, area),
        Screen::Confirm(c) => draw_confirm(f, area, c),
        Screen::Board | Screen::Settings(_) => {}
    }
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    if !app.loaded {
        f.render_widget(Paragraph::new(Line::styled("Loading…", dim())), area);
        return;
    }
    let items = app.items();
    let selected = app.selected.as_deref();
    let target = app.pane_target.as_deref();
    let mut sel_index = None;
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            Item::Header {
                group,
                count,
                collapsed,
            } => header_line(*group, *count, *collapsed, width),
            Item::Thread(row) => {
                let is_sel = Some(row.name.as_str()) == selected;
                if is_sel {
                    sel_index = Some(i);
                }
                thread_line(row, width, is_sel, Some(row.name.as_str()) == target)
            }
            Item::Empty => Line::styled(truncate("No threads yet", width), dim()),
        })
        .collect();
    let h = area.height as usize;
    let mut top = app.scroll.get().min(lines.len().saturating_sub(h));
    if let Some(s) = sel_index {
        if s < top {
            // Show the group header above the first row of a group when scrolling up.
            top = s.saturating_sub(1);
        } else if s >= top + h {
            top = s + 1 - h;
        }
    }
    app.scroll.set(top);
    let shown: Vec<Line> = lines.into_iter().skip(top).take(h).collect();
    f.render_widget(Paragraph::new(shown), area);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(dim())
        .title(Span::styled(" detail ", dim()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = detail(app, inner.width as usize);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A box centred in `area`, at most `w`×`h`, never larger than `area`.
fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rect = centred(area, 44, HELP.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered().title(" keys · any key closes ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let width = inner.width as usize;
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, what)| {
            let text = format!("{k:<7} {what}");
            Line::raw(truncate(&text, width))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let rect = centred(area, area.width, area.height);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Red))
        .title(" Discard unsaved work? ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    // The prompt and the input sit on fixed lines at the bottom; only the loss text is cut.
    let [loss_area, prompt_area, input_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(3.min(inner.height.saturating_sub(1))),
        Constraint::Length(1),
    ])
    .areas(inner);
    let loss: Vec<Line> = c.loss.lines().map(|l| Line::raw(l.to_string())).collect();
    f.render_widget(Paragraph::new(loss).wrap(Wrap { trim: false }), loss_area);
    f.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "Type {} and press Enter to discard it and clean up. Esc cancels.",
                c.name
            ),
            Style::new().add_modifier(Modifier::BOLD),
        ))
        .wrap(Wrap { trim: true }),
        prompt_area,
    );
    let input = tail(&format!("> {}▏", c.input), input_area.width as usize);
    f.render_widget(Paragraph::new(input), input_area);
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App, form: &SettingsForm) {
    let width = area.width as usize;
    let [head, body, msg, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(if area.height >= 8 { 2 } else { 1 }),
        Constraint::Length(1),
    ])
    .areas(area);
    let title = format!(
        "Settings · {}{}",
        app.config.name,
        if form.dirty() { " · unsaved" } else { "" }
    );
    f.render_widget(
        Paragraph::new(Line::styled(
            truncate(&title, width),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        head,
    );

    let fields = form.fields();
    let label_w = (width / 2).min(19);
    let lines: Vec<Line> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let current = i == form.cursor;
            let value = match (&form.editing, current) {
                (Some(text), true) => format!("{text}▏"),
                _ => form.value(*field),
            };
            let label = match field {
                Field::Repo(_) => "  repo",
                other => other.label(),
            };
            let text = if value.is_empty() {
                label.to_string()
            } else {
                format!("{:<label_w$} {value}", truncate(label, label_w))
            };
            let text = if current && form.editing.is_some() {
                tail(&text, width)
            } else {
                truncate(&text, width)
            };
            let mut style = Style::new();
            if matches!(field, Field::Save) {
                style = style.add_modifier(Modifier::BOLD);
            }
            if current {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::styled(text, style)
        })
        .collect();
    let h = body.height as usize;
    let top = form.cursor.saturating_sub(h.saturating_sub(1));
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(top).take(h).collect::<Vec<_>>()),
        body,
    );

    if let Some(m) = &form.message {
        let style = if m.starts_with("Not saved") || m.contains("doesn't exist") {
            Style::new().fg(Color::Red)
        } else {
            Style::new()
        };
        f.render_widget(
            Paragraph::new(Line::styled(m.clone(), style)).wrap(Wrap { trim: true }),
            msg,
        );
    }
    let help = if form.editing.is_some() {
        "type · ⏎ apply · Esc cancel · ^U clear"
    } else {
        match form.current() {
            Field::Repo(_) => "d remove · ↑↓ move · Esc back",
            _ => "↑↓ move · ←→ change · ⏎ edit/run · Esc back",
        }
    };
    f.render_widget(
        Paragraph::new(Line::styled(truncate(help, width), dim())),
        foot,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::app::Msg;
    use crate::board::app::tests::{NOW, app_with, ch, key, thread};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..h {
            let mut line = String::new();
            for x in 0..w {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    fn busy_board() -> App {
        let mut waiting = thread("demo-perm-1a2b", Group::Waiting, 10);
        waiting.title = Some("Approve the migration".into());
        waiting.waiting_reason = Some("permission".into());
        let mut working = thread("demo-long-3c4d", Group::Working, 9);
        working.title = Some(
            "A very long title that will never fit in a narrow board pane at all, honestly".into(),
        );
        working.branch = Some("worktree-long".into());
        let mut review = thread("demo-pr-5e6f", Group::Review, 8);
        review.title = Some("Speed up checkout".into());
        review.pr_url = Some("https://github.com/o/r/pull/42".into());
        review.pr_state = Some("OPEN".into());
        review.branch = Some("worktree-speed".into());
        review.pid = None;
        review.last_report = Some("Opened PR 42.\nAll tests pass.".into());
        review.model = Some("sonnet".into());
        review.effort = Some("high".into());
        let mut done = thread("demo-old-7a8b", Group::Resolved, 1);
        done.title = Some("Old work".into());
        done.cleanup_blocked = Some("2 unpushed commits".into());
        let mut app = app_with(vec![waiting, working, review, done]);
        app.denials = 2;
        app
    }

    #[test]
    fn narrow_board_shows_groups_tags_and_detail() {
        let mut app = busy_board();
        app.on_key(ch('j'));
        app.on_key(ch('j'));
        let out = render(&app, 40, 24);
        for want in [
            "Demo",
            "Ship it",
            "▸coordinator: asleep · 2 guard denials",
            "Waiting on you 1",
            "Approve the migration  permission",
            "Working 1",
            "Ready for review 1",
            "Speed up checkout  asleep · PR open",
            "Resolved 1 · R shows",
            "branch",
            "https://github.com/o/r/pull/42",
            "sonnet · high",
            "All tests pass.",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(!out.contains("Old work"), "resolved collapsed:\n{out}");
        // The long title is cut, never wrapped.
        let long = out.lines().find(|l| l.contains("A very long")).unwrap();
        assert!(long.ends_with('…'), "{long:?}");
        assert!(cols(long) <= 40);
        assert!(!out.contains("honestly"));
        for line in out.lines() {
            assert!(cols(line) <= 40, "{line:?}");
        }
    }

    #[test]
    fn wide_board_shows_resolved_tags_when_expanded() {
        let mut app = busy_board();
        app.on_key(ch('R'));
        app.apply(Msg::Done("Reopened x.".into()));
        let out = render(&app, 70, 40);
        assert!(out.contains("Old work  cleanup blocked"), "{out}");
        assert!(out.contains("Reopened x."), "{out}");
        assert!(
            out.contains("A very long title that will never fit"),
            "{out}"
        );
    }

    #[test]
    fn target_marker_moves_with_enter() {
        let mut app = busy_board();
        app.on_key(key(KeyCode::Enter));
        let out = render(&app, 40, 24);
        assert!(out.contains("▸! Approve the migration"), "{out}");
        assert!(out.contains(" coordinator: asleep"), "{out}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = busy_board();
        for (w, h) in [(30, 10), (10, 3), (1, 1), (30, 2)] {
            render(&app, w, h);
        }
        // Short terminal: Tab swaps the list for the detail.
        let out = render(&app, 30, 10);
        assert!(out.contains("Waiting on you"), "{out}");
        app.on_key(key(KeyCode::Tab));
        let out = render(&app, 30, 10);
        assert!(out.contains("demo-perm-1a2b"), "{out}");
        assert!(!out.contains("Working 1"), "{out}");
        for screen in ['?', 's'] {
            let mut app = busy_board();
            app.on_key(ch(screen));
            for (w, h) in [(30, 10), (5, 2), (70, 40)] {
                render(&app, w, h);
            }
        }
        let mut app = busy_board();
        app.apply(Msg::CleanBlocked {
            name: "demo-old-7a8b".into(),
            loss: "This loses:\n".repeat(30),
        });
        let out = render(&app, 30, 10);
        assert!(out.contains("> ▏"), "the input stays visible:\n{out}");
        render(&app, 4, 2);
    }

    #[test]
    fn empty_and_loading_states() {
        let config = crate::config::ProjectConfig::new("Demo", "demo", None, Vec::new());
        let app = App::new("demo", "/x".into(), config, NOW);
        assert!(render(&app, 40, 24).contains("Loading…"));
        let app = app_with(vec![]);
        let out = render(&app, 40, 24);
        assert!(out.contains("No threads yet"), "{out}");
        assert!(out.contains("Ask the coordinator"), "{out}");
    }

    #[test]
    fn status_line_priorities() {
        let mut app = busy_board();
        app.apply(Msg::Rows(crate::board::app::Snapshot {
            sessions: app.threads.clone(),
            error: Some("`claude agents` timed out".into()),
            ..Default::default()
        }));
        app.pr.blocked = 1;
        let out = render(&app, 70, 24);
        assert!(
            out.contains("PR 1 blocked · refresh failed: `claude agents` timed out"),
            "{out}"
        );
        app.busy = Some("waking demo-perm-1a2b…".into());
        assert!(render(&app, 70, 24).contains("waking demo-perm-1a2b…"));
    }

    #[test]
    fn settings_screen_renders_fields() {
        let mut app = busy_board();
        app.on_key(ch('s'));
        let out = render(&app, 40, 24);
        for want in ["Settings · Demo", "Coordinator model", "opus", "Save"] {
            assert!(out.contains(want), "missing {want:?}\n{out}");
        }
        app.on_key(key(KeyCode::Enter));
        let out = render(&app, 40, 24);
        assert!(out.contains("Ship it▏"), "{out}");
    }

    #[test]
    fn truncation_counts_columns() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("漢字漢字", 5), "漢字…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(tail("> abcdef▏", 5), "…def▏");
        assert_eq!(tail("abc", 5), "abc");
    }
}
