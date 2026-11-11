//! Drawing the board. It lives in a narrow right-hand pane, so every list line is cut to the width
//! with an ellipsis (never wrapped); the detail panel's text wraps.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use crate::db::{BusySubagent, Event, SessionRow};
use crate::registry;
use crate::routines::{Kind, Routine, State};
use crate::state::Group;
use crate::text::{age, ago, local_hhmm, local_stamp, one_line};
use crate::tui::{self, bold, cols, dim, key_bar, panel, tail, truncate};

use super::app::{App, Confirm, Hit, Item, RoutineRow, Screen, group_of};
use super::report;
use super::settings::{Field, SettingsForm};

/// Below this many body lines the detail panel replaces the list (Tab) instead of sharing.
const SPLIT_MIN_BODY: u16 = 16;
const TARGET_MARK: &str = "▸";

pub const HELP: &[(&str, &str)] = &[
    ("↑↓ j k", "move (or the mouse wheel)"),
    ("⏎ click", "show it in the main pane"),
    ("Esc c", "back to the coordinator"),
    ("n", "show the next thread waiting on you"),
    ("w", "wake (respawn) an asleep thread"),
    ("S", "stop a running thread (asks first)"),
    ("r", "resolve / reopen"),
    ("o", "open the pull request"),
    ("v", "read the thread's reports in full"),
    ("x", "clean up a resolved thread"),
    ("X", "clean up every resolved thread that loses nothing"),
    ("a", "approve a routine's command (asks first)"),
    ("e", "run a routine now"),
    ("p", "pause / resume a routine"),
    ("R", "show / hide resolved"),
    ("Tab", "detail (short terminals)"),
    ("s", "settings"),
    ("q", "quit the board"),
    ("^Q", "quit chief: the board and the main pane"),
];

const LEGEND: &[(Group, &str)] = &[
    (Group::Waiting, "waiting on you"),
    (Group::Working, "working"),
    (Group::Limited, "limited"),
    (Group::Review, "ready for review"),
    (Group::Landing, "landing"),
    (Group::Idle, "idle"),
    (Group::Failed, "failed"),
    (Group::Resolved, "resolved"),
];

pub fn style_of(group: Group) -> Style {
    let s = Style::new();
    match group {
        Group::Waiting => s.fg(Color::Red).add_modifier(Modifier::BOLD),
        Group::Working => s.fg(Color::Green),
        Group::Limited => s.fg(Color::Yellow),
        Group::Review => s.fg(Color::Blue),
        Group::Landing => s.fg(Color::Magenta),
        Group::Idle => s,
        Group::Failed => s.fg(Color::Red),
        Group::Resolved => s.add_modifier(Modifier::DIM),
    }
}

pub fn glyph(group: Group) -> &'static str {
    match group {
        Group::Waiting => "!",
        Group::Working => "●",
        Group::Limited => "◷",
        Group::Review => "◆",
        Group::Landing => "▲",
        Group::Idle => "○",
        Group::Failed => "✗",
        Group::Resolved => "✓",
    }
}

/// Short tags after a thread's title.
pub fn tags(row: &SessionRow) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(reason) = &row.waiting_reason {
        tags.push(reason.clone());
    }
    // Its own turn may have ended while these run (§5 "Background work").
    if !row.resolved
        && let Some(work) = registry::background_work(row)
    {
        tags.push(work);
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
    // A run of a routine (§13).
    if let Some(routine) = &row.routine {
        tags.push(format!("routine {routine}"));
    }
    // The agp account it runs in (§3), last: the least urgent.
    if let Some(account) = row.account_shown() {
        tags.push(account.to_string());
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

/// A row's tags in their own styles, joined with ` · `.
type Tags = Vec<(String, Style)>;

/// Plain tags, dim.
fn dim_tags(tags: Vec<String>) -> Tags {
    tags.into_iter().map(|t| (t, dim())).collect()
}

/// `tags` cut to `room` columns exactly as their joined text would be, each part in its own
/// style and the separators dim.
fn tag_spans(tags: &Tags, room: usize) -> Vec<Span<'static>> {
    let texts: Vec<&str> = tags.iter().map(|(t, _)| t.as_str()).collect();
    let cut = truncate(&texts.join(" · "), room);
    let sep = (" · ".to_string(), dim());
    let parts = tags
        .iter()
        .enumerate()
        .flat_map(|(i, tag)| (i > 0).then_some(&sep).into_iter().chain([tag]));
    let mut rest = cut.as_str();
    let mut spans = Vec::new();
    for (text, style) in parts {
        let same: usize = text
            .chars()
            .zip(rest.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum();
        // Where the cut falls, what is left is that part's start and the ellipsis.
        let whole = same == text.len();
        let taken = if whole { same } else { rest.len() };
        if taken > 0 {
            spans.push(Span::styled(rest[..taken].to_string(), *style));
        }
        rest = &rest[taken..];
        if !whole || rest.is_empty() {
            break;
        }
    }
    spans
}

/// `<mark><glyph> <title>  <tags>`, cut to `width`; the title gives way first, down to a few
/// columns, so the tags stay readable. The selected line is highlighted across the width.
fn row_line(
    mark: bool,
    glyph: Span<'static>,
    title: String,
    title_style: Style,
    tags: Tags,
    width: usize,
    selected: bool,
) -> Line<'static> {
    let avail = width.saturating_sub(3);
    let tags_cols =
        tags.iter().map(|(t, _)| cols(t)).sum::<usize>() + 3 * tags.len().saturating_sub(1);
    let title_room = if tags.is_empty() {
        avail
    } else {
        avail.saturating_sub(tags_cols + 2).max(avail.min(12))
    };
    let title = truncate(&title, title_room);
    let tag_room = avail.saturating_sub(cols(&title) + 2);
    let mut spans = vec![
        Span::styled(
            if mark { TARGET_MARK } else { " " }.to_string(),
            Style::new().fg(tui::ACCENT),
        ),
        glyph,
        Span::raw(" "),
        Span::styled(title, title_style),
    ];
    if !tags.is_empty() && tag_room > 1 {
        spans.push(Span::raw("  "));
        spans.extend(tag_spans(&tags, tag_room));
    }
    let mut line = Line::from(spans);
    if selected {
        let used: usize = line.spans.iter().map(|s| cols(&s.content)).sum();
        line.spans
            .push(Span::raw(" ".repeat(width.saturating_sub(used))));
        line = line.style(tui::selected());
    }
    line
}

fn thread_line(row: &SessionRow, width: usize, selected: bool, target: bool) -> Line<'static> {
    let group = group_of(row);
    let title_style = if group == Group::Resolved {
        dim()
    } else {
        Style::new()
    };
    row_line(
        target,
        Span::styled(glyph(group).to_string(), style_of(group)),
        display_title(row),
        title_style,
        dim_tags(tags(row)),
        width,
        selected,
    )
}

/// A routine's glyph, by how it stands: the ones that need the user in Waiting's bold red.
pub fn routine_glyph(state: &State) -> (&'static str, Style) {
    match state.key {
        "approval" | "attention" => ("!", style_of(Group::Waiting)),
        "failing" => ("✗", style_of(Group::Waiting)),
        "running" => ("●", style_of(Group::Working)),
        "ok" => ("✓", Style::new().fg(Color::Green)),
        "update" => ("◆", style_of(Group::Review)),
        "nothing" => ("○", Style::new()),
        _ => ("○", dim()),
    }
}

/// Short tags after a routine's title: how it stands when that matters, `paused`, what kind of
/// run it is when it isn't a plain session, and when it next runs.
pub fn routine_tags(r: &RoutineRow, now: i64) -> Tags {
    let routine = &r.routine;
    let mut tags = Vec::new();
    let word = match r.state.key {
        "approval" => Some("needs approval"),
        "failing" => Some("failing"),
        "attention" => Some("needs you"),
        "running" => Some("running"),
        "update" => Some("update"),
        _ => None,
    };
    if let Some(word) = word {
        let style = if r.state.needs_user() {
            style_of(Group::Waiting)
        } else {
            routine_glyph(&r.state).1
        };
        tags.push((word.to_string(), style));
    }
    if !routine.enabled {
        tags.push(("paused".to_string(), dim()));
    }
    match routine.kind() {
        Kind::Session => {}
        Kind::Command => tags.push(("command".to_string(), dim())),
        Kind::Check => tags.push(("check".to_string(), dim())),
    }
    if routine.enabled {
        let when = match routine.next_run_at {
            None => "never due".to_string(),
            Some(at) if at <= now => "due now".to_string(),
            Some(at) => format!("in {}", age(at, now)),
        };
        tags.push((when, dim()));
    }
    tags
}

fn routine_line(r: &RoutineRow, width: usize, selected: bool, now: i64) -> Line<'static> {
    let (glyph, style) = routine_glyph(&r.state);
    let title_style = if r.routine.enabled {
        Style::new()
    } else {
        dim()
    };
    row_line(
        false,
        Span::styled(glyph.to_string(), style),
        one_line(&printable(&r.routine.title), usize::MAX),
        title_style,
        routine_tags(r, now),
        width,
        selected,
    )
}

/// `Routines · 3`, and `· 1 needs you` in Waiting's bold red when some do.
fn routines_header(count: usize, need_you: usize, width: usize) -> Line<'static> {
    let mut text = format!("Routines · {count}");
    let style = match need_you {
        0 => bold(),
        1 => {
            text.push_str(" · 1 needs you");
            style_of(Group::Waiting)
        }
        n => {
            text.push_str(&format!(" · {n} need you"));
            style_of(Group::Waiting)
        }
    };
    Line::styled(truncate(&text, width), style)
}

/// `▸◉ Coordinator  idle · 2 guard denials`: the conversation, always first.
fn coordinator_line(app: &App, width: usize) -> Line<'static> {
    let mut tags = coordinator_state(app);
    if app.denials > 0 {
        let noun = if app.denials == 1 {
            "denial"
        } else {
            "denials"
        };
        tags.push_str(&format!(" · {} guard {noun}", app.denials));
    }
    row_line(
        app.pane_target.is_none(),
        Span::styled("◉".to_string(), Style::new().fg(tui::ACCENT)),
        "Coordinator".to_string(),
        bold(),
        vec![(tags, dim())],
        width,
        app.on_coordinator,
    )
}

fn header_line(group: Group, count: usize, collapsed: bool, width: usize) -> Line<'static> {
    let mut text = format!("{} · {count}", group.label());
    if collapsed {
        text.push_str(" · R shows");
    }
    let style = match group {
        Group::Waiting => style_of(group),
        _ => bold(),
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

/// The line above the key bar: a running worker, else the last action's result, else refresh
/// errors, else the PR hint. None: the key bar alone.
fn status_line(app: &App, width: usize) -> Option<Line<'static>> {
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
    } else if prefix.is_empty() {
        return None;
    } else {
        (String::new(), dim())
    };
    let text = format!(
        "{prefix}{}",
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    Some(Line::styled(
        truncate(text.trim_end_matches(" · "), width),
        style,
    ))
}

/// The keys that do something for the selection, most useful first; help always ends it, after
/// one key for the conversation's side. Those two are kept when the rest doesn't fit
/// (`KEPT_KEYS`).
fn board_keys(app: &App) -> Vec<(&'static str, &'static str)> {
    let waiting = app.threads.iter().any(|r| group_of(r) == Group::Waiting);
    if let Some(r) = app.routine_row() {
        return routine_keys(&r.routine, waiting, app.embedded);
    }
    let mut keys = vec![("⏎", "show")];
    if waiting {
        keys.push(("n", "next waiting"));
    }
    match app.selected_row() {
        None => keys.push(("s", "settings")),
        Some(row) => {
            keys.push(("Esc", "coordinator"));
            if row.resolved {
                keys.push(("r", "reopen"));
                if row.cleaned_at.is_none() {
                    keys.push(("x", "clean up"));
                    keys.push(("X", "clean up all"));
                }
            } else {
                keys.push(("r", "resolve"));
                // A running thread needs no wake; an asleep one has nothing to stop.
                keys.push(if row.pid.is_some() {
                    ("S", "stop")
                } else {
                    ("w", "wake")
                });
            }
            if row.pr_url.is_some() {
                keys.push(("o", "PR"));
            }
            if row.last_report.is_some() {
                keys.push(("v", "report"));
            }
        }
    }
    keys.extend(kept_keys(app.embedded));
    keys
}

/// On a routine there is no session to show: its own keys come first (approve while its
/// command waits, else run now; pause or resume), then the board's.
fn routine_keys(
    routine: &Routine,
    waiting: bool,
    embedded: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut keys = vec![if routine.needs_approval() {
        ("a", "approve")
    } else {
        ("e", "run now")
    }];
    keys.push(("p", if routine.enabled { "pause" } else { "resume" }));
    if waiting {
        keys.push(("n", "next waiting"));
    }
    keys.push(("Esc", "coordinator"));
    keys.extend(kept_keys(embedded));
    keys
}

/// The keys every bar ends with (`KEPT_KEYS` of them). In the built-in layout, the side
/// switch, which nothing else on screen names (the bar below the conversation names Ctrl+Q);
/// beside a pane of its own, Ctrl+Q, which nothing in Claude's screen names.
fn kept_keys(embedded: bool) -> [(&'static str, &'static str); 2] {
    [
        if embedded {
            ("^\\", "conversation")
        } else {
            ("^Q", "quit")
        },
        ("?", "help"),
    ]
}

/// How many of `board_keys`' last keys the bar keeps.
const KEPT_KEYS: usize = 2;

/// The key bar with the last `kept` keys kept at the end when the rest doesn't fit.
fn keys_line(keys: &[(&str, &str)], kept: usize, width: usize) -> Line<'static> {
    let full = key_bar(keys, width);
    let shown: usize = full.spans.iter().map(|s| cols(&s.content)).sum();
    let bar_cols = |keys: &[(&str, &str)]| {
        keys.iter()
            .map(|(k, w)| cols(k) + 1 + cols(w))
            .sum::<usize>()
            + 2 * keys.len().saturating_sub(1)
    };
    if shown == bar_cols(keys) || keys.len() <= kept {
        return full;
    }
    let (rest, tail) = keys.split_at(keys.len() - kept);
    let room = width.saturating_sub(bar_cols(tail) + 2);
    let mut line = key_bar(rest, room);
    let sep = if line.spans.is_empty() { "" } else { "  " };
    line.spans.push(Span::raw(sep));
    line.spans.extend(key_bar(tail, width).spans);
    line
}

fn detail(app: &App, width: usize) -> Vec<Line<'static>> {
    let field = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label} "), dim()),
            Span::raw(value),
        ])
    };
    if let Some(r) = app.routine_row() {
        return routine_detail(r, app.now, width);
    }
    let Some(row) = app.selected_row() else {
        let mut lines = vec![Line::styled(coordinator_state(app), Style::new())];
        if let Some(c) = &app.coordinator {
            if let Some(m) = c.model.as_deref() {
                let effort = c
                    .effort
                    .as_deref()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default();
                lines.push(field("model", format!("{m}{effort}")));
            }
            lines.push(field("active", ago(app.now, c.last_activity_at)));
        }
        lines.push(Line::raw(""));
        let guide = if app.threads.is_empty() {
            "No threads yet. Tell the coordinator what you want done, in the main pane. It splits \
             the work into threads that run in the background; they show up here, grouped by \
             what they need from you."
        } else {
            "The conversation in the main pane. ⏎ on a thread shows that thread there instead; Esc \
             comes back here."
        };
        lines.push(Line::styled(guide, dim()));
        return lines;
    };
    let group = group_of(row);
    let mut lines = vec![Line::styled(truncate(&row.name, width), dim())];
    let mut state = group.label().to_string();
    if let Some(r) = &row.waiting_reason {
        state.push_str(&format!(": {r}"));
    }
    if let (Group::Resolved, Some(r)) = (group, &row.resolved_reason) {
        state.push_str(&format!(" ({r})"));
    }
    if !row.resolved
        && let Some(work) = registry::background_work(row)
    {
        state.push_str(&format!(" · {work} running"));
    }
    if row.pid.is_none() && row.short_id.is_some() && row.cleaned_at.is_none() {
        state.push_str(" · asleep");
    }
    lines.push(Line::styled(state, style_of(group)));
    if !row.resolved && registry::background_work(row).is_some() {
        let subagents = app.subagents.get(&row.name).map_or(&[][..], Vec::as_slice);
        lines.extend(subagent_tree(subagents, app.now, width));
    }
    let all = app.activity.get(&row.name).map_or(&[][..], Vec::as_slice);
    // A working thread's own calls: its subagents' are in the tree above. A permission prompt
    // may be for a subagent's call, so a waiting thread's detail reads them all.
    let own: Vec<Event>;
    let events = if group == Group::Working && all.iter().any(|e| e.agent_id.is_some()) {
        own = all
            .iter()
            .filter(|e| e.agent_id.is_none())
            .cloned()
            .collect();
        own.as_slice()
    } else {
        all
    };
    let doing = doing(row, group, events, app.now);
    if let Some((label, text, _)) = &doing {
        lines.push(field(label, text.clone()));
    }
    if let Some(task) = row.task.as_deref().filter(|t| !t.trim().is_empty()) {
        let room = width.saturating_sub(cols("task "));
        lines.push(field("task", truncate(&one_line(task, room + 1), room)));
    }
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
    if let Some(account) = row.account_shown() {
        lines.push(field("account", account.to_string()));
    }
    if let Some(note) = &row.move_note {
        let when = row.moved_at.map(|at| format!(" {}", ago(app.now, at)));
        lines.push(field(
            "moved",
            format!("{note}{}", when.unwrap_or_default()),
        ));
    }
    if registry::limited(row)
        && let Some(limit) = &row.limit_note
    {
        let next = match (row.via_agp(), row.agp_state.as_deref()) {
            (true, Some("waiting")) => " · agp resumes it at the reset",
            (true, Some("limited" | "moving")) => " · agp is moving it",
            _ => "",
        };
        lines.push(field("limit", format!("{limit}{next}")));
    }
    lines.push(field("active", ago(app.now, row.last_activity_at)));
    if row.cleaned_at.is_some() {
        lines.push(field("cleanup", "cleaned up".to_string()));
    } else if let Some(why) = &row.cleanup_blocked {
        lines.push(field("cleanup blocked", why.clone()));
    }
    // The calls before the one `doing` tells of, one line each.
    let since = doing.as_ref().map_or(0, |(_, _, rest)| *rest);
    let before: Vec<&Event> = events[since..]
        .iter()
        .filter(|e| e.hook == "PreToolUse")
        .take(EARLIER_CALLS)
        .collect();
    if !before.is_empty() {
        lines.push(Line::styled("earlier", dim()));
        for e in before {
            let call = e.summary.as_deref().unwrap_or(&e.hook);
            lines.push(Line::from(vec![
                Span::styled(format!("{:>3} ", age(app.now, e.ts)), dim()),
                Span::raw(truncate(call, width.saturating_sub(4))),
            ]));
        }
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

/// How many of the last lines a command printed the detail shows (and the refresher keeps).
pub const OUTPUT_LINES: usize = 5;

/// `next 14:05 (in 12m)`, `paused`, `never due` or `due now`.
fn next_run(r: &Routine, now: i64) -> String {
    match (r.enabled, r.next_run_at) {
        (false, _) => "paused".to_string(),
        (true, None) => "never due".to_string(),
        (true, Some(at)) if at <= now => "due now".to_string(),
        (true, Some(at)) => {
            let clock = if at - now < 12 * 3_600_000 {
                local_hhmm(at)
            } else {
                local_stamp(at)
            };
            format!("next {clock} (in {})", age(at, now))
        }
    }
}

/// A routine's detail: what it is and when it runs, how it stands, its command and what that
/// last printed, its latest finding, then how its sessions run.
fn routine_detail(r: &RoutineRow, now: i64, width: usize) -> Vec<Line<'static>> {
    let field = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label} "), dim()),
            Span::raw(value),
        ])
    };
    let routine = &r.routine;
    let head = [
        routine.name.clone(),
        routine.kind().label().to_string(),
        r.schedule.clone(),
        next_run(routine, now),
    ];
    let mut lines = vec![Line::styled(head.join(" · "), dim())];
    let mut state = r.state.label.to_string();
    if !r.state.line.is_empty() {
        state.push_str(&format!(": {}", printable(&r.state.line)));
    }
    match (r.state.key, r.state.at) {
        // Its line says since when already.
        ("running", _) | (_, None) => {}
        ("failing", Some(at)) => state.push_str(&format!(" · for {}", age(now, at))),
        (_, Some(at)) => state.push_str(&format!(" · {}", ago(now, at))),
    }
    let (_, style) = routine_glyph(&r.state);
    lines.push(Line::styled(state, style));
    if let Some(command) = &routine.command {
        let mut text = one_line(&printable(command), 200);
        if let Some(secs) = routine.timeout_secs {
            text.push_str(&format!("  (at most {secs}s)"));
        }
        lines.push(Line::from(vec![
            Span::styled("$ ", Style::new().fg(tui::ACCENT)),
            Span::raw(text),
        ]));
        if routine.needs_approval() {
            lines.push(Line::styled(
                "Waits for your approval: a shows it whole, y approves.",
                style_of(Group::Waiting),
            ));
        }
        if let Some(run) = &r.last_exec {
            let mut text = format!("{} · {}", ago(now, run.started_at), printable(&run.line()));
            if let Some(thread) = &run.thread {
                text.push_str(&format!(" · started {thread}"));
            }
            lines.push(field("last run", text));
            let output = run.output.as_deref().map(printable).unwrap_or_default();
            let printed: Vec<&str> = output.trim_end().lines().collect();
            for line in &printed[printed.len().saturating_sub(OUTPUT_LINES)..] {
                let room = width.saturating_sub(2);
                lines.push(Line::styled(format!("  {}", truncate(line, room)), dim()));
            }
        }
    } else if let (Some(at), Some(thread)) = (routine.last_run_at, &routine.last_thread) {
        lines.push(field("last run", format!("{} · {thread}", ago(now, at))));
    }
    // The state line tells of the latest finding already when it is what the state comes from.
    if let Some(f) = r.finding.as_ref().filter(|f| f.headline != r.state.line) {
        lines.push(field(
            "outcome",
            format!(
                "{}: {} · {}",
                f.verdict.as_str(),
                printable(&f.headline),
                ago(now, f.at)
            ),
        ));
    }
    if let Some(note) = &routine.last_note {
        lines.push(field("note", printable(note)));
    }
    if !routine.prompt.trim().is_empty() {
        let room = width.saturating_sub(cols("prompt "));
        let prompt = one_line(&printable(&routine.prompt), room + 1);
        lines.push(field("prompt", truncate(&prompt, room)));
    }
    if let Some(repo) = &routine.repo {
        lines.push(field("repo", repo.clone()));
    }
    let model = match (&routine.model, &routine.effort) {
        (Some(m), Some(e)) => Some(format!("{m} · {e}")),
        (Some(m), None) => Some(m.clone()),
        (None, Some(e)) => Some(e.clone()),
        (None, None) => None,
    };
    if let Some(m) = model {
        lines.push(field("model", m));
    }
    lines
}

/// `text` as the board can draw it: escape sequences and other control characters out (a
/// command's output may colour or move the cursor, which would garble the board), a tab as a
/// space, and of a line redrawn with `\r` (a progress bar) only its last drawing.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.trim_end_matches('\r');
        let line = line.rsplit('\r').next().unwrap_or(line);
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    // CSI: up to its final byte.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC: up to BEL or ESC \.
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' {
                                break;
                            }
                            if c == '\u{1b}' {
                                chars.next_if_eq(&'\\');
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\t' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
    }
    out
}

/// The widest a subagent's label gets in the tree, so its call still shows.
const SUBAGENT_LABEL: usize = 18;

/// A thread's subagents mid-turn, one line each under its state: its name (else what it was
/// asked to do, else its id) and the call it runs.
fn subagent_tree(subagents: &[BusySubagent], now: i64, width: usize) -> Vec<Line<'static>> {
    let label = |s: &BusySubagent| {
        let text = s
            .name
            .as_deref()
            .or(s.description.as_deref())
            .unwrap_or(&s.agent_id);
        one_line(text, SUBAGENT_LABEL)
    };
    let pad = subagents.iter().map(|s| cols(&label(s))).max().unwrap_or(0);
    subagents
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let branch = if i + 1 == subagents.len() {
                "└ "
            } else {
                "├ "
            };
            let name = label(s);
            let name = format!("{name}{}", " ".repeat(pad - cols(&name)));
            let call = match &s.call {
                Some((call, ts)) => format!("{call} · {}", age(now, *ts)),
                None => "starting".to_string(),
            };
            let room = width.saturating_sub(cols(branch) + pad + 2);
            Line::from(vec![
                Span::styled(branch, dim()),
                Span::raw(name),
                Span::raw("  "),
                Span::styled(truncate(&one_line(&call, room + 1), room), dim()),
            ])
        })
        .collect()
}

/// How many of a working or waiting thread's calls before the latest the detail lists.
const EARLIER_CALLS: usize = 3;

/// What a working or waiting thread does or asks, from its latest events (newest first): what a
/// waiting one asks (`waiting::asks`, shared with the coordinator's notice), or the call a
/// working one runs, else the one it ran last. The label, the text, and where the events before
/// that call start.
fn doing(
    row: &SessionRow,
    group: Group,
    events: &[Event],
    now: i64,
) -> Option<(&'static str, String, usize)> {
    let text = |e: &Event| e.summary.clone().unwrap_or_else(|| e.hook.clone());
    let is_call = |e: &Event| matches!(e.hook.as_str(), "PreToolUse" | "PostToolUse");
    // The first call to start at or after `i`, and where the events before it start.
    let started = |i: usize| {
        let at = i + events[i..].iter().position(|e| e.hook == "PreToolUse")?;
        Some((&events[at], at + 1))
    };
    match group {
        Group::Waiting => crate::waiting::asks(row, events),
        Group::Working => {
            let latest = events.iter().position(is_call)?;
            let e = &events[latest];
            if e.hook == "PreToolUse" {
                Some((
                    "now",
                    format!("{} · {}", text(e), age(now, e.ts)),
                    latest + 1,
                ))
            } else {
                // The call ended: the ones before start after its own start.
                let rest = started(latest).map_or(latest + 1, |(_, rest)| rest);
                Some((
                    "last",
                    format!("{} · {} ago", text(e), age(now, e.ts)),
                    rest,
                ))
            }
        }
        _ => None,
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    draw_in(f, f.area(), app);
}

/// The board in `area` of the frame: all of it for `chief board`, the right side in the built-in
/// layout. Mouse hits are recorded by row, so `area` spans the frame's height.
pub fn draw_in(f: &mut Frame, area: Rect, app: &App) {
    app.hits.borrow_mut().clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    match &app.screen {
        Screen::Settings(form) => return draw_settings(f, area, app, form),
        Screen::Report(view) => return report::draw(f, area, view, app.now),
        _ => {}
    }
    let width = area.width as usize;
    let status = status_line(app, width);
    let [body, status_area, foot] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(u16::from(status.is_some() && area.height > 2)),
        Constraint::Length(1),
    ])
    .areas(area);
    if let Some(line) = status {
        f.render_widget(Paragraph::new(line), status_area);
    }
    let keys = keys_line(&board_keys(app), KEPT_KEYS, width);
    f.render_widget(Paragraph::new(keys), foot);

    if body.height >= SPLIT_MIN_BODY {
        // The list takes what it needs up to 3/5 of the body; the detail gets the rest.
        let wanted = if app.loaded { app.items().len() } else { 1 } as u16 + goal_lines(app) + 2;
        let list_h = wanted.clamp(5, body.height * 3 / 5);
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
        Screen::Stop { label, .. } => draw_stop(f, area, label),
        Screen::Approve {
            title,
            command,
            schedule,
            ..
        } => draw_approve(f, area, app, title, command, schedule),
        Screen::Board | Screen::Settings(_) | Screen::Report(_) => {}
    }
}

fn goal_lines(app: &App) -> u16 {
    u16::from(
        app.config
            .goal
            .as_deref()
            .is_some_and(|g| !g.trim().is_empty()),
    )
}

/// The project's panel: its goal, the coordinator, then the threads by group.
fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    // Too small for a frame: the bare list.
    let framed = area.height >= 4 && area.width >= 12;
    let inner = if framed {
        let block = panel(&app.config.name, true);
        let inner = tui::inside(&block, area);
        f.render_widget(block, area);
        inner
    } else {
        area
    };
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<(Line, Option<Hit>)> = Vec::new();
    if let Some(goal) = app.config.goal.as_deref().filter(|g| !g.trim().is_empty())
        && inner.height >= 3
    {
        lines.push((Line::styled(truncate(goal, width), dim()), None));
    }
    let fixed = lines.len();
    if !app.loaded {
        lines.push((Line::styled("Loading…", dim()), None));
        f.render_widget(
            Paragraph::new(lines.into_iter().map(|(l, _)| l).collect::<Vec<_>>()),
            inner,
        );
        return;
    }
    let items = app.items();
    let selected = app.selected_row().map(|r| r.name.as_str());
    let selected_routine = app.routine_row().map(|r| r.routine.name.as_str());
    let target = app.pane_target.as_deref();
    let mut sel_index = None;
    let mut list: Vec<(Line, Option<Hit>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        list.push(match item {
            Item::Coordinator => {
                if app.on_coordinator {
                    sel_index = Some(i);
                }
                (coordinator_line(app, width), Some(Hit::Coordinator))
            }
            Item::Header {
                group,
                count,
                collapsed,
            } => (
                header_line(*group, *count, *collapsed, width),
                (*group == Group::Resolved).then_some(Hit::Resolved),
            ),
            Item::Thread(row) => {
                let is_sel = Some(row.name.as_str()) == selected;
                if is_sel {
                    sel_index = Some(i);
                }
                (
                    thread_line(row, width, is_sel, Some(row.name.as_str()) == target),
                    Some(Hit::Thread(row.name.clone())),
                )
            }
            Item::Empty => (Line::styled(truncate("No threads yet", width), dim()), None),
            Item::Routines { count, need_you } => (routines_header(*count, *need_you, width), None),
            Item::Routine(r) => {
                let is_sel = Some(r.routine.name.as_str()) == selected_routine;
                if is_sel {
                    sel_index = Some(i);
                }
                (
                    routine_line(r, width, is_sel, app.now),
                    Some(Hit::Routine(r.routine.name.clone())),
                )
            }
        });
    }
    // When the list overflows, the last line says what is below, so a collapsed Resolved header
    // past the fold is not invisible.
    let room = (inner.height as usize).saturating_sub(fixed);
    let overflow = list.len() > room && room > 1;
    let h = room - usize::from(overflow);
    let mut top = app.scroll.get().min(list.len().saturating_sub(h));
    if let Some(s) = sel_index {
        if s < h {
            // The first page holds it: show the coordinator and the first header too.
            top = 0;
        } else if s < top {
            // Show the group header above the first row of a group when scrolling up, when
            // there is room for both.
            top = s.saturating_sub(usize::from(h > 1));
        } else if s >= top + h {
            top = s + 1 - h;
        }
    }
    app.scroll.set(top);
    let below = items.get(top + h..).unwrap_or_default();
    lines.extend(list.into_iter().skip(top).take(h));
    if overflow {
        lines.push((more_line(below, width), None));
    }
    let mut hits = app.hits.borrow_mut();
    for (i, (_, hit)) in lines.iter().enumerate() {
        if let Some(hit) = hit {
            hits.push((inner.y + i as u16, hit.clone()));
        }
    }
    f.render_widget(
        Paragraph::new(lines.into_iter().map(|(l, _)| l).collect::<Vec<_>>()),
        inner,
    );
}

/// `↓ 3 more · Resolved 2 · R shows` for the items under the fold.
fn more_line(below: &[Item], width: usize) -> Line<'static> {
    let threads = below
        .iter()
        .filter(|i| matches!(i, Item::Thread(_) | Item::Routine(_)))
        .count();
    let mut text = if threads > 0 {
        format!("↓ {threads} more")
    } else {
        "↓ more".to_string()
    };
    for item in below {
        if let Item::Header {
            group,
            count,
            collapsed: true,
        } = item
        {
            text.push_str(&format!(" · {} {count} · R shows", group.label()));
        }
    }
    if below.is_empty() {
        text = "↑ more above".to_string();
    }
    Line::styled(truncate(&text, width), dim())
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let title = match (app.routine_row(), app.selected_row()) {
        (Some(r), _) => one_line(&printable(&r.routine.title), usize::MAX),
        (None, Some(row)) => display_title(row),
        (None, None) => "Coordinator".to_string(),
    };
    let block = panel(
        &truncate(&title, (area.width as usize).saturating_sub(4)),
        false,
    );
    let inner = tui::inside(&block, area);
    f.render_widget(block, area);
    let lines = detail(app, inner.width as usize);
    let text = Paragraph::new(lines).wrap(Wrap { trim: false });
    // The rest is in the report view: the last line says so when the text runs past it.
    let cut = text.line_count(inner.width) > usize::from(inner.height);
    let reported = app.selected_row().is_some_and(|r| r.last_report.is_some());
    if cut && reported && inner.height > 1 {
        let [body, more] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        f.render_widget(text, body);
        f.render_widget(Line::styled("↓ more · v reads it all", dim()), more);
    } else {
        f.render_widget(text, inner);
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!("{k:<8}"), Style::new().fg(tui::ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled("What the marks mean", bold()));
    lines.push(Line::from(vec![
        Span::styled(format!("{TARGET_MARK:<8}"), Style::new().fg(tui::ACCENT)),
        Span::raw("shown in the main pane"),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<8}", "◉"), Style::new().fg(tui::ACCENT)),
        Span::raw("the coordinator"),
    ]));
    lines.extend(LEGEND.iter().map(|(group, what)| {
        Line::from(vec![
            Span::styled(format!("{:<8}", glyph(*group)), style_of(*group)),
            Span::raw(*what),
        ])
    }));
    // A routine's marks mean what a thread's do, and two more.
    lines.push(Line::from(vec![
        Span::styled(format!("{:<8}", "✓"), Style::new().fg(Color::Green)),
        Span::raw("a routine that passed"),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<8}", "◆"), style_of(Group::Review)),
        Span::raw("a routine with news (update)"),
    ]));
    let rect = tui::centred(area, 46, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = panel("Help · any key closes", true);
    let inner = tui::inside(&block, rect);
    f.render_widget(block, rect);
    let width = inner.width as usize;
    let lines: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            if cols(&text) <= width {
                l
            } else {
                Line::raw(truncate(&text, width))
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let rect = tui::centred(area, area.width, area.height);
    f.render_widget(Clear, rect);
    if c.confirmed.is_none() {
        // chief can't list what a discard would lose, so there is nothing to confirm.
        let block = panel("Cleanup blocked", true).border_style(Style::new().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let [text_area, foot] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let text: Vec<Line> = c.loss.lines().map(|l| Line::raw(l.to_string())).collect();
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);
        f.render_widget(
            Paragraph::new(Line::styled("Any key closes.", bold())),
            foot,
        );
        return;
    }
    let block = panel("Discard unsaved work?", true).border_style(Style::new().fg(Color::Red));
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
            bold(),
        ))
        .wrap(Wrap { trim: true }),
        prompt_area,
    );
    let input = tail(&format!("> {}▏", c.input), input_area.width as usize);
    f.render_widget(Paragraph::new(input), input_area);
}

fn draw_stop(f: &mut Frame, area: Rect, label: &str) {
    let width = area.width.min(56);
    let text = "Its process ends, and the turn it is in with it: it sends no report for that \
                turn. Its conversation, worktree and branch stay, and w wakes it.";
    let block = panel(&format!("Stop {label}?"), true).border_style(Style::new().fg(Color::Yellow));
    let body = Paragraph::new(text).wrap(Wrap { trim: true });
    // The panel's borders and the foot line, around the text as it wraps.
    let inner_width = block.inner(Rect::new(0, 0, width, 3)).width;
    let lines = body.line_count(inner_width) as u16;
    let rect = tui::centred(area, width, lines + 3);
    f.render_widget(Clear, rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let [text_area, foot] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    f.render_widget(body, text_area);
    f.render_widget(
        Paragraph::new(Line::styled("y stops · any other key cancels", bold())),
        foot,
    );
}

/// Approving lets a shell command run unattended, so the modal shows the whole command, and
/// says when it can't: then y approves nothing (`App::approve_shown`).
fn draw_approve(f: &mut Frame, area: Rect, app: &App, title: &str, command: &str, schedule: &str) {
    let width = area.width.min(72);
    let intro = format!(
        "Once approved, chief runs this command with sh -c, as you and with nobody watching, on \
         its schedule ({schedule}) and whenever it is run, until its words change:"
    );
    let mut lines = vec![Line::raw(intro), Line::raw("")];
    for (i, line) in printable(command).lines().enumerate() {
        let prompt = if i == 0 { "$ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prompt, Style::new().fg(tui::ACCENT)),
            Span::styled(line.to_string(), bold()),
        ]));
    }
    let block = panel(
        &truncate(
            &format!(
                "Approve the command of {}?",
                one_line(&printable(title), usize::MAX)
            ),
            (width as usize).saturating_sub(4),
        ),
        true,
    )
    .border_style(Style::new().fg(Color::Red));
    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    // The panel's borders and the foot line, around the text as it wraps.
    let inner_width = block.inner(Rect::new(0, 0, width, 3)).width;
    let needed = body.line_count(inner_width) as u16;
    let rect = tui::centred(area, width, needed.saturating_add(3));
    f.render_widget(Clear, rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let [text_area, foot] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let fits = inner.height > 1 && needed <= text_area.height;
    app.approve_shown.set(fits);
    f.render_widget(body, text_area);
    let foot_text = if fits {
        "y approves · any other key cancels"
    } else {
        "Too long to show whole here: any key closes"
    };
    f.render_widget(Paragraph::new(Line::styled(foot_text, bold())), foot);
}

fn settings_keys(form: &SettingsForm) -> Vec<(&'static str, &'static str)> {
    if form.editing.is_some() {
        return vec![("⏎", "apply"), ("Esc", "cancel"), ("^U", "clear")];
    }
    let mut keys = match form.current() {
        Field::Repo(_) => vec![("d", "remove")],
        Field::Untrusted(_) => vec![("⏎", "trust it")],
        Field::CoordinatorEffort
        | Field::ThreadEffort
        | Field::PermissionMode
        | Field::DefaultRepo
        | Field::Cleanup
        | Field::Notify
        | Field::MaxParallel => vec![("←→", "change")],
        Field::PoolTrust => vec![("⏎", "check")],
        Field::EditInstructions | Field::EditMemory => vec![("⏎", "open in $EDITOR")],
        Field::AddRepo => vec![("⏎", "add")],
        Field::Save => vec![("⏎", "save")],
        Field::Account => vec![],
        _ => vec![("⏎", "edit")],
    };
    keys.extend([("^S", "save"), ("Esc", "back")]);
    keys
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App, form: &SettingsForm) {
    let title = format!(
        "Settings · {}{}",
        app.config.name,
        if form.dirty() { " · unsaved" } else { "" }
    );
    let framed = area.height >= 6 && area.width >= 12;
    let (inner, foot) = if framed {
        let [panel_area, foot] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let block = panel(
            &truncate(&title, (area.width as usize).saturating_sub(4)),
            true,
        );
        let inner = tui::inside(&block, panel_area);
        f.render_widget(block, panel_area);
        (inner, foot)
    } else {
        let [head, inner, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(Line::styled(truncate(&title, area.width as usize), bold())),
            head,
        );
        (inner, foot)
    };
    let width = inner.width as usize;
    let note_h = if inner.height >= 10 {
        2
    } else {
        u16::from(inner.height >= 4)
    };
    let [body, note] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(note_h)]).areas(inner);

    let fields = form.fields();
    let label_w = (width / 2).min(19);
    let mut lines: Vec<(Line, Option<usize>)> = Vec::new();
    let mut cursor_line = 0;
    let mut section = "";
    for (i, field) in fields.iter().enumerate() {
        if field.section() != section {
            section = field.section();
            if !lines.is_empty() {
                lines.push((Line::raw(""), None));
            }
            if !section.is_empty() {
                lines.push((
                    Line::styled(
                        truncate(section, width),
                        Style::new().fg(tui::ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    None,
                ));
            }
        }
        let current = i == form.cursor;
        if current {
            cursor_line = lines.len();
        }
        let edited = form
            .editing
            .as_ref()
            .filter(|(f, _)| f == field)
            .map(|(_, text)| text);
        let value = match edited {
            Some(text) => format!("{text}▏"),
            None => form.value(*field),
        };
        let label = match field {
            Field::Repo(_) => "repo",
            Field::Untrusted(_) => "untrusted",
            other => other.label(),
        };
        let text = if value.is_empty() {
            format!(" {label}")
        } else {
            format!(" {:<label_w$} {value}", truncate(label, label_w))
        };
        let text = if edited.is_some() {
            tail(&text, width)
        } else {
            truncate(&text, width)
        };
        let mut style = Style::new();
        if matches!(field, Field::Save) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if current {
            style = tui::selected();
        }
        let used = cols(&text);
        let text = if current {
            format!("{text}{}", " ".repeat(width.saturating_sub(used)))
        } else {
            text
        };
        lines.push((Line::styled(text, style), Some(i)));
    }
    let h = body.height as usize;
    // Keep the cursor's section heading in view when it fits.
    let top = cursor_line.saturating_sub(h.saturating_sub(1));
    let mut hits = app.hits.borrow_mut();
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(top)
        .take(h)
        .enumerate()
        .map(|(row, (line, field))| {
            if let Some(i) = field {
                hits.push((body.y + row as u16, Hit::Field(i)));
            }
            line
        })
        .collect();
    f.render_widget(Paragraph::new(shown), body);

    let (text, style) = match &form.message {
        Some(m)
            if m.starts_with("Not saved")
                || m.starts_with("Saved, but")
                || m.contains("doesn't exist") =>
        {
            (m.clone(), Style::new().fg(Color::Red))
        }
        Some(m) => (m.clone(), Style::new()),
        None => (form.current().help().to_string(), dim()),
    };
    f.render_widget(
        Paragraph::new(Line::styled(text, style)).wrap(Wrap { trim: true }),
        note,
    );
    f.render_widget(
        Paragraph::new(keys_line(&settings_keys(form), 1, area.width as usize)),
        foot,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::app::Action;
    use crate::board::app::Msg;
    use crate::board::app::tests::{NOW, app_with, ch, key, routine, thread};
    use crate::board::app::{RoutineRow, Snapshot};
    use crate::command::{CommandRun, Status};
    use crate::routines::{Finding, Verdict};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

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
        review.account_id = Some("cpb".into());
        review.account_label = Some("PB2".into());
        let mut done = thread("demo-old-7a8b", Group::Resolved, 1);
        done.title = Some("Old work".into());
        done.cleanup_blocked = Some("2 unpushed commits".into());
        let mut app = app_with(vec![waiting, working, review, done]);
        app.denials = 2;
        app
    }

    /// A command that fails, as the refresher reads it: its last run with the end of what it
    /// printed (colours, a progress bar redrawn with \r), and a session's word on it.
    fn failing_check() -> RoutineRow {
        let mut r = routine("uptime", "failing");
        r.routine.title = "Site uptime".into();
        r.routine.command = Some("curl -fsS https://example.com/health".into());
        r.routine.approved_command = r.routine.command.clone();
        r.routine.prompt = "Find out why the site is down,\nand who to tell.".into();
        r.routine.model = Some("sonnet".into());
        r.routine.effort = Some("high".into());
        r.routine.last_note = Some("skipped at 09:00: its command still runs".into());
        r.schedule = "Every 30 minutes".into();
        r.state.line = "exit 22: curl: (22) 503".into();
        r.state.at = Some(NOW - 3 * 3_600_000);
        r.last_exec = Some(CommandRun {
            id: 7,
            project_slug: "demo".into(),
            routine: "uptime".into(),
            fired_by: "schedule".into(),
            started_at: NOW - 5 * 60_000,
            finished_at: Some(NOW - 5 * 60_000 + 300),
            pid: None,
            status: Status::Failed,
            exit_code: Some(22),
            output: Some(
                "\u{1b}[31mGET /health\u{1b}[0m\nretrying 10%\r100%\ncurl: (22) 503\n".into(),
            ),
            outcome: None,
            headline: None,
            details: None,
            thread: None,
        });
        r.finding = Some(Finding {
            id: 3,
            routine: "uptime".into(),
            thread: Some("demo-uptime-1a2b".into()),
            run: None,
            at: NOW - 2 * 3_600_000,
            verdict: Verdict::Attention,
            headline: "Site down since 06:00".into(),
            details: None,
        });
        r
    }

    /// The busy board with four routines: a command waiting for approval, a failing check, a
    /// paused session and one with news that never comes due.
    fn routine_board() -> App {
        let mut app = busy_board();
        let mut backup = routine("backup", "approval");
        backup.routine.title = "Nightly backup".into();
        backup.state.line = "its command runs once you approve it".into();
        let mut triage = routine("triage", "nothing");
        triage.routine.title = "Triage issues".into();
        triage.routine.prompt = "Label new issues.".into();
        triage.routine.enabled = false;
        triage.routine.next_run_at = None;
        let mut prices = routine("prices", "update");
        prices.routine.title = "Price watch".into();
        prices.routine.next_run_at = None;
        let mut sessions = app.threads.clone();
        sessions.extend(app.coordinator.clone());
        app.apply(Msg::Rows(Snapshot {
            sessions,
            routines: vec![backup, prices, triage, failing_check()],
            ..Default::default()
        }));
        app.denials = 2;
        app
    }

    fn select_routine(app: &mut App, name: &str) {
        app.selected_routine = Some(name.into());
        app.on_coordinator = false;
    }

    /// The Routines section comes after Resolved, a row per routine with its state's glyph and
    /// tags; the ones that need the user are counted in a red header.
    #[test]
    fn routines_show_after_the_threads_with_tags() {
        let app = routine_board();
        let out = render(&app, 70, 34);
        for want in [
            "Routines · 4 · 2 need you",
            " ! Nightly backup  needs approval · command · in 12m",
            " ◆ Price watch  update · never due",
            " ○ Triage issues  paused",
            " ✗ Site uptime  failing · check · in 12m",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        let at = |text: &str| out.find(text).unwrap();
        assert!(at("Resolved · 1 · R shows") < at("Routines · 4"), "{out}");
        // Narrow: the title gives way to the tags, and nothing wraps.
        let out = render(&app, 36, 34);
        assert!(out.contains("! Nightly bac…  needs approval…"), "{out}");
        for line in out.lines() {
            assert!(cols(line) <= 36, "{line:?}");
        }
        // No routines, no section.
        assert!(!render(&busy_board(), 70, 34).contains("Routines"));
    }

    /// A failing command's detail: what it is and when it runs, since when it fails, the
    /// command, its last run with the end of what it printed (escapes taken out), what a
    /// session found, the note, and how its sessions run.
    #[test]
    fn the_detail_shows_a_failing_command_and_its_output() {
        let mut app = routine_board();
        select_routine(&mut app, "uptime");
        let out = render(&app, 96, 50);
        for want in [
            "╭ Site uptime ",
            "uptime · command, then session · Every 30 minutes · next ",
            "(in 12m)",
            "Failing: exit 22: curl: (22) 503 · for 3h",
            "$ curl -fsS https://example.com/health",
            "last run 5m ago · exit 22: curl: (22) 503",
            "  GET /health",
            "  100%",
            "  curl: (22) 503",
            "outcome attention: Site down since 06:00 · 2h ago",
            "note skipped at 09:00: its command still runs",
            "prompt Find out why the site is down, and who to tell.",
            "model sonnet · high",
            "e run now  p pause  n next waiting  Esc coordinator  ^Q quit  ? help",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(
            !out.contains("retrying"),
            "only the last drawing of a line: {out}"
        );
        assert!(!out.contains('\u{1b}') && !out.contains("[31m"), "{out}");
        assert!(!out.contains("waits for your approval"), "{out}");
    }

    /// A command the user hasn't approved says so, and which key approves it; the key bar
    /// follows the selection onto routines.
    #[test]
    fn the_detail_and_key_bar_of_a_routine_waiting_for_approval() {
        let mut app = routine_board();
        select_routine(&mut app, "backup");
        let out = render(&app, 90, 40);
        for want in [
            "╭ Nightly backup ",
            "backup · command · Every 30 minutes · next",
            "Needs approval: its command runs once you approve it",
            "$ restic backup ~/work",
            "Waits for your approval: a shows it whole, y approves.",
            "a approve  p pause  n next waiting  Esc coordinator  ^Q quit  ? help",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(!out.contains("⏎ show"), "{out}");
        select_routine(&mut app, "triage");
        let out = render(&app, 90, 40);
        assert!(
            out.contains("triage · session · Every 30 minutes · paused"),
            "{out}"
        );
        assert!(out.contains("e run now  p resume"), "{out}");
        assert!(out.contains("prompt Label new issues."), "{out}");
        // Back on a thread, the thread's keys.
        app.on_key(ch('g'));
        app.on_key(ch('j'));
        assert!(render(&app, 90, 40).contains("⏎ show  n next waiting  Esc coordinator"));
    }

    /// The approve modal shows the whole command; one too long for the screen can't be approved
    /// from it.
    #[test]
    fn approving_asks_first_and_shows_the_whole_command() {
        let mut app = routine_board();
        select_routine(&mut app, "backup");
        app.routines[0].routine.command = Some(
            "restic backup ~/work \\\n  --exclude target\nrestic forget --keep-daily 7".into(),
        );
        app.on_key(ch('a'));
        let out = render(&app, 70, 24);
        for want in [
            "Approve the command of Nightly backup?",
            "on its schedule (Every 30 minutes)",
            "$ restic backup ~/work \\",
            "    --exclude target",
            "  restic forget --keep-daily 7",
            "y approves · any other key cancels",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(app.approve_shown.get());
        let Action::Approve { command, .. } = &app.on_key(ch('y'))[0] else {
            panic!("approved")
        };
        assert!(command.ends_with("--keep-daily 7"));

        app.on_key(ch('a'));
        let out = render(&app, 30, 9);
        assert!(out.contains("Too long"), "{out}");
        assert!(!app.approve_shown.get());
        assert!(app.on_key(ch('y')).is_empty());
        assert!(app.status.as_deref().unwrap().contains("didn't fit"));
    }

    /// Below the fold the routines count among the rows; the selection on the last one stays in
    /// view; a click selects a routine and a second one says it has no session.
    #[test]
    fn routines_scroll_and_take_clicks() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = routine_board();
        let out = render(&app, 40, 8);
        assert!(out.contains("↓ 6 more · Resolved 1 · R shows"), "{out}");
        app.on_key(ch('G'));
        let out = render(&app, 40, 8);
        assert!(out.contains("Site uptime"), "{out}");
        assert!(out.contains("↑ more above"), "{out}");
        for h in 4..=9 {
            assert!(
                render(&app, 40, h + 1).contains("Site uptime"),
                "height {h}"
            );
        }

        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut app = routine_board();
        let out = render(&app, 70, 34);
        let y = out.lines().position(|l| l.contains("Price watch")).unwrap() as u16;
        assert!(app.on_mouse(click(y)).is_empty());
        assert_eq!(app.selected_routine.as_deref(), Some("prices"));
        render(&app, 70, 34);
        assert!(app.on_mouse(click(y)).is_empty(), "no session to show");
        assert!(app.status.as_deref().unwrap().contains("is a routine"));
        let thread_y = out.lines().position(|l| l.contains("Speed up")).unwrap() as u16;
        app.on_mouse(click(thread_y));
        assert_eq!(app.selected_routine, None);
        assert_eq!(app.selected.as_deref(), Some("demo-pr-5e6f"));
    }

    #[test]
    fn routine_screens_on_tiny_terminals_do_not_panic() {
        let mut app = routine_board();
        select_routine(&mut app, "uptime");
        for (w, h) in [(30, 10), (10, 3), (1, 1), (30, 2), (5, 20)] {
            render(&app, w, h);
        }
        app.on_key(key(KeyCode::Tab));
        let out = render(&app, 30, 10);
        assert!(out.contains("Site uptime"), "{out}");
        select_routine(&mut app, "backup");
        app.on_key(ch('a'));
        for (w, h) in [(30, 10), (10, 3), (1, 1), (4, 2), (70, 40)] {
            render(&app, w, h);
        }
        app.on_key(ch('?'));
        render(&app, 30, 10);
    }

    /// Tags cut as their joined text would be, each part keeping its style.
    #[test]
    fn styled_tags_cut_like_their_text() {
        let red = Style::new().fg(Color::Red);
        let tags: Tags = vec![
            ("needs approval".into(), red),
            ("command".into(), dim()),
            ("in 12m".into(), dim()),
        ];
        let joined = "needs approval · command · in 12m";
        for room in 0..40 {
            let text: String = tag_spans(&tags, room)
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(text, truncate(joined, room), "room {room}");
        }
        let spans = tag_spans(&tags, 40);
        assert_eq!(spans[0].style, red);
        assert_eq!(spans[1].content, " · ");
    }

    #[test]
    fn printable_takes_out_what_would_garble_the_board() {
        assert_eq!(printable("\u{1b}[1;31mred\u{1b}[0m ok"), "red ok");
        assert_eq!(printable("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(printable("\u{1b}]8;;http://x\u{1b}\\link"), "link");
        assert_eq!(printable("a\tb\u{8}c"), "a bc");
        assert_eq!(printable("10%\r50%\r100%\r\nnext"), "100%\nnext");
    }

    #[test]
    fn narrow_board_shows_groups_tags_and_detail() {
        let mut app = busy_board();
        app.on_key(ch('j'));
        app.on_key(ch('j'));
        // Inside the panel's frame and margin the list is 40 columns wide.
        let out = render(&app, 44, 24);
        for want in [
            "╭ Demo ",
            "Ship it",
            "▸◉ Coordinator  asleep · 2 guard denials",
            "Waiting on you · 1",
            "Approve the migration  permission",
            "Working · 1",
            "Ready for review · 1",
            "Speed up che…  asleep · PR open · PB2",
            "Resolved · 1 · R shows",
            "branch",
            "https://github.com/o/r/pull/42",
            "sonnet · high",
            "account PB2",
            "All tests pass.",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert!(!out.contains("Old work"), "resolved collapsed:\n{out}");
        // The long title is cut, never wrapped.
        let long = out.lines().find(|l| l.contains("A very long")).unwrap();
        assert!(long.trim_end_matches(['│', ' ']).ends_with('…'), "{long:?}");
        assert!(cols(long) <= 44);
        assert!(!out.contains("honestly"));
        for line in out.lines() {
            assert!(cols(line) <= 44, "{line:?}");
        }
    }

    /// Events as the hook records them, newest first: `(hook, summary, seconds ago)`.
    fn events(list: &[(&str, &str, i64)]) -> Vec<Event> {
        list.iter()
            .map(|(hook, summary, secs)| Event {
                session_name: Some("t".into()),
                session_id: None,
                project_slug: "demo".into(),
                hook: hook.to_string(),
                ts: NOW - secs * 1000,
                summary: Some(summary.to_string()),
                agent_id: None,
            })
            .collect()
    }

    /// What the line under a thread's state says, from its events: the call a permission prompt
    /// is for, an open question, the call running or the last one; and where earlier calls start.
    #[test]
    fn the_detail_says_what_a_thread_does_or_asks() {
        let mut perm = thread("t", Group::Waiting, 1);
        perm.waiting_reason = Some("permission".into());
        let asked = events(&[
            (
                "Notification",
                "permission_prompt: Claude needs your permission",
                1,
            ),
            ("PreToolUse", "Bash: npm publish", 2),
            ("PostToolUse", "Read: package.json", 3),
            ("PreToolUse", "Read: package.json", 4),
        ]);
        let said = doing(&perm, Group::Waiting, &asked, NOW);
        assert_eq!(said, Some(("wants to run", "npm publish".into(), 2)));
        let edit = events(&[
            (
                "Notification",
                "permission_prompt: Claude needs your permission",
                1,
            ),
            ("PreToolUse", "Edit: src/api.rs", 2),
        ]);
        let said = doing(&perm, Group::Waiting, &edit, NOW);
        assert_eq!(said, Some(("wants to use", "Edit: src/api.rs".into(), 2)));

        let mut question = thread("t", Group::Waiting, 1);
        question.waiting_reason = Some("question".into());
        let open = events(&[("PreToolUse", "AskUserQuestion: What limit per key?", 5)]);
        let said = doing(&question, Group::Waiting, &open, NOW);
        assert_eq!(said, Some(("asks", "What limit per key?".into(), 1)));
        let answered = events(&[
            ("PostToolUse", "AskUserQuestion: What limit per key?", 1),
            ("PreToolUse", "AskUserQuestion: What limit per key?", 5),
        ]);
        assert_eq!(doing(&question, Group::Waiting, &answered, NOW), None);

        let working = thread("t", Group::Working, 1);
        let running = events(&[("PreToolUse", "Bash: cargo test", 12)]);
        let said = doing(&working, Group::Working, &running, NOW);
        assert_eq!(said, Some(("now", "Bash: cargo test · 12s".into(), 1)));
        let ran = events(&[
            ("PostToolUse", "Edit: src/a.rs", 3),
            ("PreToolUse", "Edit: src/a.rs", 4),
            ("PreToolUse", "Read: src/a.rs", 9),
        ]);
        let said = doing(&working, Group::Working, &ran, NOW);
        assert_eq!(said, Some(("last", "Edit: src/a.rs · 3s ago".into(), 2)));

        let idle = thread("t", Group::Idle, 1);
        assert_eq!(doing(&idle, Group::Idle, &ran, NOW), None);
    }

    /// A working thread's busy subagents show as a tree under its state, each with its latest
    /// call; the thread's own line tells of its own calls, not theirs.
    #[test]
    fn the_detail_shows_subagents_as_a_tree() {
        let mut working = thread("demo-api-1a2b", Group::Working, 1);
        working.subagents_busy = 3;
        let mut app = app_with(vec![working]);
        let mut evs = events(&[
            ("PreToolUse", "Read: src/hook.rs", 2),
            ("PreToolUse", "Agent: Review the hook", 30),
        ]);
        evs[0].agent_id = Some("areviewer-1".into());
        app.activity.insert("demo-api-1a2b".into(), evs);
        let sub = |id: &str, name: Option<&str>, desc: Option<&str>, call: Option<(&str, i64)>| {
            BusySubagent {
                agent_id: id.into(),
                name: name.map(Into::into),
                description: desc.map(Into::into),
                call: call.map(|(c, secs)| (c.to_string(), NOW - secs * 1000)),
            }
        };
        app.subagents.insert(
            "demo-api-1a2b".into(),
            vec![
                sub(
                    "areviewer-1",
                    Some("reviewer"),
                    None,
                    Some(("Read: src/hook.rs", 2)),
                ),
                sub(
                    "a2",
                    None,
                    Some("Run the tests"),
                    Some(("Bash: cargo test", 7)),
                ),
                sub("ab-886b99f57d4badec", None, None, None),
            ],
        );
        app.on_key(ch('j'));
        let out = render(&app, 60, 30);
        for want in [
            "├ reviewer            Read: src/hook.rs · 2s",
            "├ Run the tests       Bash: cargo test · 7s",
            "└ ab-886b99f57d4bad…  starting",
            "now Agent: Review the hook · 30s",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        assert_eq!(out.matches("src/hook.rs").count(), 1, "{out}");
    }

    /// The detail of a working thread: what it runs under its state, its task on one line, and
    /// the calls before, newest first.
    #[test]
    fn the_detail_shows_a_threads_task_and_calls() {
        let mut working = thread("demo-api-1a2b", Group::Working, 1);
        working.title = Some("Rate limits".into());
        working.task =
            Some("Add per-key rate limits to the public API, with a\nlimit per plan".into());
        let mut app = app_with(vec![working]);
        app.activity.insert(
            "demo-api-1a2b".into(),
            events(&[
                ("PreToolUse", "Bash: cargo test limits", 12),
                ("PostToolUse", "Edit: src/api/router.rs", 40),
                ("PreToolUse", "Edit: src/api/router.rs", 41),
                ("PreToolUse", "Read: src/api/router.rs", 90),
            ]),
        );
        app.on_key(ch('j'));
        let out = render(&app, 60, 30);
        for want in [
            "now Bash: cargo test limits · 12s",
            "task Add per-key rate limits to the public API, with a …",
            "earlier",
            "41s Edit: src/api/router.rs",
            " 1m Read: src/api/router.rs",
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }
        // The running call isn't listed again.
        assert_eq!(out.matches("cargo test limits").count(), 1, "{out}");
    }

    /// A report longer than the detail ends in a line saying where the rest is; `v` is on the
    /// key bar for a thread with a report.
    #[test]
    fn a_cut_off_report_points_at_the_report_view() {
        let mut idle = thread("demo-api-1a2b", Group::Idle, 1);
        let long: Vec<String> = (1..=30).map(|i| format!("Step {i} done.")).collect();
        idle.last_report = Some(long.join("\n"));
        let mut app = app_with(vec![idle]);
        app.on_key(ch('j'));
        let out = render(&app, 70, 30);
        assert!(out.contains("↓ more · v reads it all"), "{out}");
        assert!(out.contains("v report"), "{out}");
        assert!(!out.contains("Step 30 done."), "{out}");
        // A short one doesn't.
        app.threads[0].last_report = Some("Done.".into());
        let out = render(&app, 70, 30);
        assert!(
            out.contains("Done.") && !out.contains("v reads it all"),
            "{out}"
        );
    }

    #[test]
    fn short_board_says_what_is_below_the_fold() {
        let app = busy_board();
        let out = render(&app, 40, 8);
        assert!(out.contains("↓ 2 more · Resolved 1 · R shows"), "{out}");
        assert!(!out.contains("Old work"), "{out}");
        for line in out.lines() {
            assert!(cols(line) <= 40, "{line:?}");
        }
    }

    /// At every list height from 4 to 8 the selection stays in view while it moves down and back
    /// up, and the last line says what is below, the collapsed Resolved header included.
    #[test]
    fn the_list_scrolls_and_says_what_is_below() {
        let rows = [
            "Coordinator",
            "Approve the migration",
            "A very long",
            "Speed up",
        ];
        for list_h in 4..=8u16 {
            let mut app = busy_board();
            app.on_key(ch('g'));
            // The list gets all but the key bar.
            let mut walk: Vec<usize> = (0..rows.len()).collect();
            walk.extend((0..rows.len() - 1).rev());
            for (step, &at) in walk.iter().enumerate() {
                if step > 0 {
                    let down = at > walk[step - 1];
                    app.on_key(ch(if down { 'j' } else { 'k' }));
                }
                let out = render(&app, 40, list_h + 1);
                assert!(
                    out.contains(rows[at]),
                    "height {list_h}, step {step}: {:?} not shown\n{out}",
                    rows[at]
                );
                let last = out.lines().nth(list_h as usize - 2).unwrap();
                assert!(
                    last.contains("↓") || last.contains("↑ more above"),
                    "height {list_h}, step {step}: no fold line\n{out}"
                );
                if !out.contains("Speed up") {
                    assert!(
                        last.contains("Resolved 1 · R shows"),
                        "height {list_h}: {last:?}\n{out}"
                    );
                }
                for line in out.lines() {
                    assert!(cols(line) <= 40, "{line:?}");
                }
            }
        }
        // At 8 the first page shows the goal, the coordinator and the first group.
        let app = busy_board();
        let out = render(&app, 40, 9);
        assert!(out.contains("Ship it"), "{out}");
        assert!(out.contains("Coordinator"), "{out}");
        assert!(out.contains("↓ 2 more · Resolved 1 · R shows"), "{out}");
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
        assert!(out.contains(" ◉ Coordinator  asleep"), "{out}");
        assert!(!out.contains("▸◉"), "{out}");
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
        assert!(!out.contains("Working · 1"), "{out}");
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
            confirmed: Some(Default::default()),
        });
        let out = render(&app, 30, 10);
        assert!(out.contains("> ▏"), "the input stays visible:\n{out}");
        render(&app, 4, 2);

        // Nothing chief can list: no discard prompt, no input.
        app.apply(Msg::CleanBlocked {
            name: "demo-old-7a8b".into(),
            loss: "Cleanup blocked: 1 unpushed commit.".into(),
            confirmed: None,
        });
        let out = render(&app, 70, 12);
        assert!(out.contains("Cleanup blocked"), "{out}");
        assert!(!out.contains("Discard"), "{out}");
        assert!(!out.contains("> ▏"), "{out}");
        assert!(out.contains("Any key closes"), "{out}");
    }

    #[test]
    fn empty_and_loading_states() {
        let config = crate::config::ProjectConfig::new("Demo", "demo", None, Vec::new());
        let app = App::new("demo", "/x".into(), config, NOW);
        assert!(render(&app, 40, 24).contains("Loading…"));
        let app = app_with(vec![]);
        let out = render(&app, 40, 24);
        assert!(out.contains("No threads yet"), "{out}");
        // The coordinator is selected, and its detail says how to start.
        assert!(out.contains("╭ Coordinator "), "{out}");
        assert!(out.contains("Tell the coordinator"), "{out}");
        assert!(out.contains("⏎ show  s settings  ^Q quit  ? help"), "{out}");
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
        let dir = tempfile::tempdir().unwrap();
        app.config.save(&dir.path().join("project.toml")).unwrap();
        app.dir = dir.path().into();
        app.on_key(ch('s'));
        let out = render(&app, 48, 40);
        for want in [
            "Settings · Demo",
            "Project",
            "Coordinator",
            "Coordinator model",
            "opus",
            "Accounts",
            "Save",
            "What the project is for",
            "⏎ edit  ^S save  Esc back",
        ] {
            assert!(out.contains(want), "missing {want:?}\n{out}");
        }
        app.on_key(key(KeyCode::Enter));
        let out = render(&app, 48, 40);
        assert!(out.contains("Ship it▏"), "{out}");
        assert!(out.contains("⏎ apply  Esc cancel"), "{out}");
        // The cursor stays in view on a short screen.
        app.on_key(key(KeyCode::Esc));
        for _ in 0..30 {
            app.on_key(ch('j'));
        }
        let out = render(&app, 40, 14);
        assert!(out.contains(" Save"), "{out}");
    }

    /// §5 "Background work": its own turn may have ended, so the tag says what still runs.
    #[test]
    fn background_work_is_a_tag() {
        let mut row = thread("demo-wait-1a2b", Group::Working, 10);
        row.subagents_busy = 2;
        assert!(
            tags(&row).contains(&"2 subagents".to_string()),
            "{:?}",
            tags(&row)
        );
        row.resolved = true;
        assert!(!tags(&row).iter().any(|t| t.contains("subagent")));
    }

    /// The key bar names what works on the selection, and always keeps `^Q quit` and `? help`: `n`
    /// while a thread waits, `S` on a running thread, `w` on an asleep one.
    #[test]
    fn the_key_bar_follows_the_selection() {
        let mut app = busy_board();
        let out = render(&app, 80, 24);
        assert!(
            out.contains(
                "⏎ show  n next waiting  Esc coordinator  r resolve  S stop  ^Q quit  ? help"
            ),
            "{out}"
        );
        let out = render(&app, 30, 24);
        let foot = out.lines().last().unwrap();
        assert!(foot.ends_with("^Q quit  ? help"), "{foot:?}");
        app.on_key(ch('g'));
        assert!(app.on_coordinator);
        assert!(
            render(&app, 70, 24).contains("⏎ show  n next waiting  s settings  ^Q quit  ? help")
        );
        app.selected = Some("demo-pr-5e6f".into());
        app.on_coordinator = false;
        assert!(render(&app, 90, 24).contains("r resolve  w wake  o PR"));
        app.on_key(ch('R'));
        app.on_key(ch('G'));
        let out = render(&app, 110, 30);
        assert!(
            out.contains("r reopen  x clean up  X clean up all"),
            "{out}"
        );
    }

    #[test]
    fn stopping_asks_first() {
        let mut app = busy_board();
        app.on_key(ch('S'));
        let out = render(&app, 70, 24);
        assert!(out.contains("Stop Approve the migration?"), "{out}");
        assert!(out.contains("y stops · any other key cancels"), "{out}");
        assert!(app.on_key(ch('n')).is_empty(), "n cancels here");
        assert_eq!(app.screen, Screen::Board);
        app.on_key(ch('S'));
        assert_eq!(
            app.on_key(ch('y')),
            vec![super::super::app::Action::Stop("demo-perm-1a2b".into())]
        );
    }

    /// Beside its conversation, the board's narrow key bar still names the side switch.
    #[test]
    fn the_embedded_key_bar_keeps_the_side_switch() {
        let mut app = busy_board();
        app.embedded = true;
        let out = render(&app, 110, 24);
        assert!(
            out.contains(
                "⏎ show  n next waiting  Esc coordinator  r resolve  S stop  ^\\ conversation  \
                 ? help"
            ),
            "{out}"
        );
        let out = render(&app, 41, 24);
        let foot = out.lines().last().unwrap();
        assert!(foot.ends_with("^\\ conversation  ? help"), "{foot:?}");
        assert!(foot.starts_with("⏎ show"), "{foot:?}");
    }

    #[test]
    fn clicks_select_then_show() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let mut app = busy_board();
        let out = render(&app, 40, 24);
        let y = |text: &str| out.lines().position(|l| l.contains(text)).unwrap() as u16;
        assert!(app.on_mouse(click(y("Speed up"))).is_empty());
        assert_eq!(app.selected.as_deref(), Some("demo-pr-5e6f"));
        assert_eq!(
            app.on_mouse(click(y("Speed up"))),
            [Action::Show(Some("demo-pr-5e6f".into()))]
        );
        app.on_mouse(click(y("Coordinator")));
        assert!(app.on_coordinator);
        assert_eq!(app.on_mouse(click(y("Coordinator"))), [Action::Show(None)]);
        // The collapsed Resolved header opens; the wheel moves the selection.
        app.on_mouse(click(y("Resolved · 1")));
        assert!(app.show_resolved);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..click(0)
        });
        assert_eq!(app.selected.as_deref(), Some("demo-perm-1a2b"));
        // A click where nothing is drawn does nothing.
        assert!(app.on_mouse(click(200)).is_empty());
    }

    #[test]
    fn help_explains_the_marks() {
        let mut app = busy_board();
        app.on_key(ch('?'));
        let out = render(&app, 50, 30);
        for want in [
            "shown in the main pane",
            "the coordinator",
            "waiting on you",
            "resolved",
        ] {
            assert!(out.contains(want), "missing {want:?}\n{out}");
        }
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
