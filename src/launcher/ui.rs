//! Drawing the launcher: one centred card per screen, the key bar under it.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::project::{Note, Where};
use crate::tui::{self, ACCENT, bold, cols, dim, key_bar, panel, tail, truncate};

use super::app::{Hit, Launcher, Screen, Starting};
use super::wizard::{Setting, Step, Wizard};

const CARD_WIDTH: u16 = 76;
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

thread_local! {
    static FRAME: Cell<usize> = const { Cell::new(0) };
}

/// A screen's content: its lines (each with what a click on it hits) and whether they wrap.
struct Card {
    title: String,
    lines: Vec<(Line<'static>, Option<Hit>)>,
    wrap: bool,
}

impl Card {
    fn new(title: impl Into<String>, lines: Vec<Line<'static>>) -> Self {
        Card {
            title: title.into(),
            lines: lines.into_iter().map(|l| (l, None)).collect(),
            wrap: true,
        }
    }

    /// Lines it takes at `width`, counting wrapped ones.
    fn height(&self, width: usize) -> usize {
        self.lines
            .iter()
            .map(|(l, _)| {
                let w: usize = l.spans.iter().map(|s| cols(&s.content)).sum();
                if self.wrap && width > 0 {
                    w.div_ceil(width).max(1)
                } else {
                    1
                }
            })
            .sum()
    }
}

pub fn draw(f: &mut Frame, app: &Launcher) {
    app.hits.borrow_mut().clear();
    let area = f.area();
    if area.width < 4 || area.height < 3 {
        return;
    }
    FRAME.with(|n| n.set(n.get().wrapping_add(1)));
    let width = area.width.min(CARD_WIDTH);
    let x = area.x + (area.width - width) / 2;
    let inner_w = width.saturating_sub(4) as usize;
    let (card, keys) = match &app.screen {
        Screen::List => (
            list_card(app, inner_w),
            vec![
                ("⏎", "open"),
                ("n", "new project"),
                ("?", "help"),
                ("q", "quit"),
            ],
        ),
        Screen::Help => (help_card(), vec![("any key", "back")]),
        Screen::Wizard(w) => (wizard_card(w, inner_w), wizard_keys(w)),
        Screen::Starting(s) => (starting_card(s), starting_keys(s)),
    };
    // The card fits its content, a line below the top, with the key bar right under it.
    let top = u16::from(area.height > 12);
    let room = area.height.saturating_sub(top + 1);
    let height = (card.height(inner_w) as u16 + 2).min(room);
    let rect = Rect {
        x,
        y: area.y + top,
        width,
        height,
    };
    render_card(f, rect, &card, app);
    let foot = Rect {
        x,
        y: rect.y + rect.height,
        width,
        height: 1,
    };
    f.render_widget(Paragraph::new(key_bar(&keys, width as usize)), foot);
}

/// Draws the card in its frame, keeping the selected list row in view, and records where each
/// line with a hit landed.
fn render_card(f: &mut Frame, area: Rect, card: &Card, app: &Launcher) {
    let block = panel(
        &truncate(&card.title, (area.width as usize).saturating_sub(4)),
        true,
    );
    let inner = tui::inside(&block, area);
    f.render_widget(block, area);
    if card.wrap {
        let lines: Vec<Line> = card.lines.iter().map(|(l, _)| l.clone()).collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }
    let h = inner.height as usize;
    let at = card
        .lines
        .iter()
        .rposition(|(_, hit)| *hit == Some(Hit::Row(app.cursor)))
        .unwrap_or(0);
    let top = (at + 1).saturating_sub(h);
    let mut hits = app.hits.borrow_mut();
    let shown: Vec<Line> = card
        .lines
        .iter()
        .skip(top)
        .take(h)
        .enumerate()
        .map(|(row, (line, hit))| {
            if let Some(hit) = hit {
                hits.push((inner.y + row as u16, *hit));
            }
            line.clone()
        })
        .collect();
    f.render_widget(Paragraph::new(shown), inner);
}

fn list_card(app: &Launcher, width: usize) -> Card {
    let mut lines: Vec<(Line<'static>, Option<Hit>)> = vec![
        (Line::styled("Your projects", bold()), None),
        (Line::raw(""), None),
    ];
    if app.projects.is_empty() {
        lines.push((Line::styled("No projects yet.", dim()), None));
    }
    for (i, p) in app.projects.iter().enumerate() {
        let selected = i == app.cursor;
        let summary_style = if p.waiting {
            Style::new().fg(Color::Red)
        } else {
            dim()
        };
        let summary = truncate(&p.summary, width / 2);
        let name_room = width.saturating_sub(cols(&summary) + 4);
        let name = truncate(&p.name, name_room);
        let gap = width.saturating_sub(2 + cols(&name) + cols(&summary));
        let mut first = Line::from(vec![
            Span::styled(if selected { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
            Span::styled(name, bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(summary, summary_style),
        ]);
        if selected {
            first = first.style(tui::selected());
            pad(&mut first, width);
        }
        lines.push((first, Some(Hit::Row(i))));
        if let Some(goal) = p.goal.as_deref() {
            let mut second = Line::from(vec![
                Span::styled(if selected { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                Span::styled(truncate(goal, width.saturating_sub(2)), dim()),
            ]);
            if selected {
                second = second.style(tui::selected());
                pad(&mut second, width);
            }
            lines.push((second, Some(Hit::Row(i))));
        }
    }
    lines.push((Line::raw(""), None));
    let new = app.on_new_row();
    let mut add = Line::from(vec![
        Span::styled(if new { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
        Span::styled("+ New project", Style::new().fg(ACCENT)),
    ]);
    if new {
        add = add.style(tui::selected());
        pad(&mut add, width);
    }
    lines.push((add, Some(Hit::Row(app.projects.len()))));
    if let Some(status) = &app.status {
        lines.push((Line::raw(""), None));
        lines.push((
            Line::styled(status.clone(), Style::new().fg(Color::Red)),
            None,
        ));
    }

    Card {
        title: "chief".into(),
        lines,
        wrap: false,
    }
}

fn pad(line: &mut Line<'static>, width: usize) {
    let used: usize = line.spans.iter().map(|s| cols(&s.content)).sum();
    line.spans
        .push(Span::raw(" ".repeat(width.saturating_sub(used))));
}

fn help_card() -> Card {
    let lines = vec![
        Line::styled("What chief does", bold()),
        Line::raw(
            "Each project has one coordinator: a Claude session you talk to. It plans the \
             work and hands it to threads, background Claude Code sessions that work in git \
             worktrees of your repositories, and reports back what they did.",
        ),
        Line::raw(""),
        Line::styled("Opening a project", bold()),
        Line::raw(
            "shows the conversation with the coordinator on the left and its threads on the \
             right (in Rex or tmux). On the threads board, ⏎ shows a thread on the left, Esc \
             comes back to the coordinator, s opens settings and ? lists every key.",
        ),
        Line::raw(""),
        Line::styled("Keys here", bold()),
        Line::raw("↑↓ or the mouse wheel move · ⏎ or a click opens · n new project · q quit"),
    ];
    Card::new("chief · help", lines)
}

/// `✓ About  ● Code  ○ Models  ○ Review`.
fn steps_line(w: &Wizard) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, step) in Step::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let (mark, style) = match (i + 1).cmp(&w.number()) {
            std::cmp::Ordering::Less => ("✓", Style::new().fg(Color::Green)),
            std::cmp::Ordering::Equal => (
                "●",
                Style::new()
                    .fg(ACCENT)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            std::cmp::Ordering::Greater => ("○", dim()),
        };
        spans.push(Span::styled(format!("{mark} {}", step.label()), style));
    }
    Line::from(spans)
}

/// A text input: `> text▏` when focused, the placeholder dimmed when empty.
fn input(text: &str, placeholder: &str, focused: bool, width: usize) -> Line<'static> {
    let prompt = Span::styled(if focused { "> " } else { "  " }, Style::new().fg(ACCENT));
    if text.is_empty() && !focused {
        return Line::from(vec![prompt, Span::styled(placeholder.to_string(), dim())]);
    }
    if text.is_empty() {
        return Line::from(vec![
            prompt,
            Span::styled("▏", Style::new().fg(ACCENT)),
            Span::styled(truncate(placeholder, width.saturating_sub(3)), dim()),
        ]);
    }
    let shown = if focused {
        tail(&format!("{text}▏"), width.saturating_sub(2))
    } else {
        truncate(text, width.saturating_sub(2))
    };
    Line::from(vec![prompt, Span::raw(shown)])
}

fn label(text: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::new()
            .fg(ACCENT)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        bold()
    };
    Line::styled(text.to_string(), style)
}

fn wizard_card(w: &Wizard, width: usize) -> Card {
    let mut lines = vec![steps_line(w), Line::raw("")];
    match w.step {
        Step::About => {
            lines.push(Line::raw("What is this project about?"));
            lines.push(Line::raw(""));
            lines.push(label("Name", !w.on_goal));
            lines.push(input(&w.name, "Payments latency", !w.on_goal, width));
            let slug = w.slug();
            lines.push(Line::styled(
                if slug.is_empty() {
                    String::new()
                } else {
                    format!("  {slug}")
                },
                dim(),
            ));
            lines.push(label("Goal (optional)", w.on_goal));
            lines.push(input(&w.goal, "Hold p95 under 200 ms", w.on_goal, width));
            lines.push(Line::styled(
                "  The coordinator and every thread read it.",
                dim(),
            ));
        }
        Step::Code => {
            lines.push(Line::raw("Which code will its threads work in?"));
            lines.push(Line::styled(
                "Each thread works in its own git worktree of one of these. None is fine too.",
                dim(),
            ));
            lines.push(Line::raw(""));
            if w.repos.is_empty() {
                lines.push(Line::styled("  No folders yet.", dim()));
            }
            for (i, repo) in w.repos.iter().enumerate() {
                let on = w.repo_cursor == Some(i);
                let mut line = Line::from(vec![
                    Span::styled(if on { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                    Span::raw(tui::path(repo, width.saturating_sub(2))),
                ]);
                if on {
                    line = line.style(tui::selected());
                    pad(&mut line, width);
                }
                lines.push(line);
            }
            lines.push(Line::raw(""));
            lines.push(label("Add a folder", w.repo_cursor.is_none()));
            lines.push(input(
                &w.path,
                "~/workspace/my-repo",
                w.repo_cursor.is_none(),
                width,
            ));
        }
        Step::Models => {
            lines.push(Line::raw("Quick settings"));
            lines.push(Line::styled(
                "Defaults suit most projects. Everything else is on the board: press s.",
                dim(),
            ));
            lines.push(Line::raw(""));
            for (i, s) in Setting::ALL.iter().enumerate() {
                let on = i == w.setting;
                let value = format!("‹ {} ›", w.value(*s));
                let mut line = Line::from(vec![
                    Span::styled(if on { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                    Span::raw(format!("{:<20}", s.label())),
                    Span::styled(value, if on { bold() } else { Style::new() }),
                ]);
                if on {
                    line = line.style(tui::selected());
                    pad(&mut line, width);
                }
                lines.push(line);
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                Setting::ALL[w.setting].help().to_string(),
                dim(),
            ));
        }
        Step::Review => {
            let d = w.draft();
            let row = |k: &str, v: String| {
                Line::from(vec![
                    Span::styled(format!("{k:<8}"), dim()),
                    Span::raw(truncate(&v, width.saturating_sub(8))),
                ])
            };
            lines.push(Line::raw("Ready to create"));
            lines.push(Line::raw(""));
            lines.push(row("Name", format!("{} ({})", d.name, w.slug())));
            lines.push(row("Goal", d.goal.clone().unwrap_or_else(|| "—".into())));
            let code = match d.repos.as_slice() {
                [] => "no repositories".to_string(),
                [one] => tui::path(one, width.saturating_sub(8)),
                many => format!("{} folders", many.len()),
            };
            lines.push(row("Code", code));
            lines.push(row(
                "Models",
                format!(
                    "coordinator {} · {}, threads {} · {}",
                    d.coordinator_model, d.coordinator_effort, d.thread_model, d.thread_effort
                ),
            ));
            if !w.untrusted.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "Claude Code doesn't trust these yet, and background sessions refuse them:",
                ));
                for folder in &w.untrusted {
                    lines.push(Line::styled(
                        format!("  {}", tui::path(folder, width.saturating_sub(2))),
                        dim(),
                    ));
                }
                let check = if w.trust { "[x]" } else { "[ ]" };
                lines.push(Line::from(vec![
                    Span::styled(format!("{check} "), Style::new().fg(ACCENT)),
                    Span::raw("Trust them, as accepting Claude's trust prompt would"),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "⏎ creates the project and opens the conversation with its coordinator.",
                bold(),
            ));
        }
    }
    if let Some(m) = &w.message {
        lines.push(Line::raw(""));
        let style = if m.starts_with("Added") || m.starts_with("Removed") {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::Red)
        };
        lines.push(Line::styled(m.clone(), style));
    }
    Card::new(format!("New project · step {} of 4", w.number()), lines)
}

fn wizard_keys(w: &Wizard) -> Vec<(&'static str, &'static str)> {
    let back = if w.step == Step::About {
        ("Esc", "cancel")
    } else {
        ("Esc", "back")
    };
    match w.step {
        Step::About => vec![("⏎", "next"), ("Tab", "switch field"), back],
        Step::Code if w.repo_cursor.is_some() => {
            vec![("⏎", "next"), ("d", "remove"), ("↑↓", "move"), back]
        }
        Step::Code => vec![
            (
                "⏎",
                if w.path.trim().is_empty() {
                    "next"
                } else {
                    "add"
                },
            ),
            ("Tab", "complete"),
            ("↑", "listed folders"),
            back,
        ],
        Step::Models => vec![("⏎", "next"), ("←→", "change"), ("↑↓", "move"), back],
        Step::Review if !w.untrusted.is_empty() => {
            vec![("⏎", "create"), ("Space", "toggle trust"), back]
        }
        Step::Review => vec![("⏎", "create"), back],
    }
}

fn starting_card(s: &Starting) -> Card {
    let mut lines: Vec<Line<'static>> = s
        .notes
        .iter()
        .map(|n| match n {
            Note::Did(t) => Line::from(vec![
                Span::styled("✓ ", Style::new().fg(Color::Green)),
                Span::raw(tui::home_relative(t)),
            ]),
            Note::Info(t) => Line::from(vec![
                Span::styled("· ", dim()),
                Span::styled(tui::home_relative(t), dim()),
            ]),
            Note::Warn(t) => Line::from(vec![
                Span::styled("! ", Style::new().fg(Color::Yellow)),
                Span::raw(tui::home_relative(t)),
            ]),
        })
        .collect();
    lines.push(Line::raw(""));
    let label = crate::layout::session_label(&s.slug);
    match &s.done {
        None => {
            let frame = FRAME.with(Cell::get) / 2 % SPINNER.len();
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", SPINNER[frame]), Style::new().fg(ACCENT)),
                Span::styled("Starting the coordinator…", dim()),
            ]));
        }
        Some(Ok(Where::Rex { shown: true })) => lines.push(Line::styled(
            format!(
                "Open in Rex session {label}: the conversation with the coordinator on the left, \
                 its threads on the right."
            ),
            bold(),
        )),
        Some(Ok(Where::Rex { shown: false })) => lines.push(Line::styled(
            format!("Ready in Rex session {label}; switch to it in the Rex app."),
            bold(),
        )),
        Some(Ok(Where::Tmux)) => lines.push(Line::styled(
            format!(
                "Open in tmux window {label}: the conversation on the left, its threads on the \
                 right."
            ),
            bold(),
        )),
        Some(Ok(Where::Manual)) => {
            lines.push(Line::styled(
                "⏎ shows the conversation with the coordinator in this terminal.",
                bold(),
            ));
            lines.push(Line::styled(
                "For its threads, run this in another terminal or split:",
                dim(),
            ));
            lines.push(Line::raw(format!("  chief board --project {}", s.slug)));
        }
        Some(Err(e)) => {
            lines.push(Line::styled(e.clone(), Style::new().fg(Color::Red)));
            if s.wizard.is_some() {
                lines.push(Line::styled(
                    "Nothing was created. ⏎ goes back to the wizard.",
                    dim(),
                ));
            }
        }
    }
    Card::new(format!("Starting {}", s.name), lines)
}

fn starting_keys(s: &Starting) -> Vec<(&'static str, &'static str)> {
    match &s.done {
        None => vec![("q", "quit")],
        Some(Ok(Where::Manual)) => vec![("⏎", "open here"), ("Esc", "projects"), ("q", "quit")],
        Some(Err(_)) if s.wizard.is_some() => vec![("⏎", "back to the wizard"), ("q", "quit")],
        Some(_) => vec![("⏎", "projects"), ("q", "quit")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use crate::launcher::app::Entry;
    use crate::launcher::wizard::Probe;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::Path;

    struct Free;

    impl Probe for Free {
        fn slug_problem(&self, _: &str) -> Option<String> {
            None
        }
        fn trusted(&self, folder: &Path) -> bool {
            !folder.ends_with("web")
        }
    }

    fn render(app: &Launcher, w: u16, h: u16) -> String {
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

    fn press(app: &mut Launcher, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE), &Free);
    }

    fn typed(app: &mut Launcher, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn the_list_shows_projects_and_new() {
        let config = ProjectConfig::new("Payments latency", "pay", Some("Hold p95"), Vec::new());
        let app = Launcher::new(vec![Entry::new("pay", Ok(config), &[])], None, false);
        let out = render(&app, 80, 20);
        for want in [
            "╭ chief ",
            "Your projects",
            "▌ Payments latency",
            "no threads yet",
            "Hold p95",
            "+ New project",
            "⏎ open  n new project  ? help  q quit",
        ] {
            assert!(out.contains(want), "missing {want:?}\n{out}");
        }
        for (w, h) in [(20, 5), (3, 2), (120, 50)] {
            render(&app, w, h);
        }
    }

    #[test]
    fn every_wizard_step_renders() {
        let web = tempfile::tempdir().unwrap();
        let web_dir = web.path().join("web");
        std::fs::create_dir(&web_dir).unwrap();
        let mut app = Launcher::new(Vec::new(), None, false);
        let out = render(&app, 80, 24);
        assert!(out.contains("New project · step 1 of 4"), "{out}");
        assert!(out.contains("● About  ○ Code  ○ Models  ○ Review"), "{out}");
        assert!(
            out.contains("⏎ next  Tab switch field  Esc cancel"),
            "{out}"
        );
        typed(&mut app, "Payments Latency");
        let out = render(&app, 80, 24);
        assert!(out.contains("> Payments Latency▏"), "{out}");
        assert!(out.contains("payments-latency"), "{out}");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        let out = render(&app, 80, 24);
        assert!(out.contains("✓ About  ● Code"), "{out}");
        assert!(out.contains("No folders yet."), "{out}");
        typed(&mut app, &web_dir.to_string_lossy());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        let out = render(&app, 80, 24);
        assert!(out.contains("Coordinator model   ‹ opus ›"), "{out}");
        press(&mut app, KeyCode::Enter);
        let out = render(&app, 80, 24);
        for want in [
            "Ready to create",
            "Payments Latency (payments-latency)",
            "coordinator opus · low, threads sonnet · high",
            "doesn't trust these yet",
            "[x] Trust them",
            "⏎ create  Space toggle trust  Esc back",
        ] {
            assert!(out.contains(want), "missing {want:?}\n{out}");
        }
        for (w, h) in [(20, 5), (3, 2)] {
            render(&app, w, h);
        }
    }

    #[test]
    fn starting_says_where_the_conversation_is() {
        let config = ProjectConfig::new("Pay", "pay", None, Vec::new());
        let mut app = Launcher::new(vec![Entry::new("pay", Ok(config), &[])], None, false);
        press(&mut app, KeyCode::Enter);
        assert!(render(&app, 80, 20).contains("Starting the coordinator…"));
        app.apply(crate::launcher::app::Msg::Note(Note::Did(
            "Coordinator chief-pay started.".into(),
        )));
        app.apply(crate::launcher::app::Msg::Done(Ok(Where::Manual)));
        let out = render(&app, 80, 20);
        assert!(out.contains("✓ Coordinator chief-pay started."), "{out}");
        assert!(out.contains("in this terminal"), "{out}");
        assert!(out.contains("chief board --project pay"), "{out}");
        assert!(out.contains("⏎ open here"), "{out}");
    }
}
