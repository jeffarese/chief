//! The pane's program status for Rex (ARCHITECTURE §9): Rex shows a block's state (working,
//! blocked and why) from OSC 7501 reports, which Claude Code sends when it runs in the block
//! itself. In chief's pane the block runs chief, relaying `claude attach`, so the pane reports
//! for the session it shows, as Claude Code would (`app=claude-code`), from the session's row.
//! The block's agent icon comes from its process's name instead (`pane::REX_ARGV0`).

use crate::db::SessionRow;
use crate::registry;
use crate::state::{Group, Role};

/// Ends the record: the pane shows no session.
pub const CLEAR: &[u8] = b"\x1b]7501;state=clear\x1b\\";

/// Rex sets `REX_BLOCK` for the programs in its blocks; other terminals get no reports.
pub fn enabled() -> bool {
    std::env::var_os("REX_BLOCK").is_some_and(|b| !b.is_empty())
}

/// The report for `row`: its state (a waiting session is `blocked`, with what it waits for), its
/// title, and its background work as the message. An idle session that wrote its last message
/// after `since` (unix ms; the pane attached it then) ended a turn as the user watched: `done`, as
/// Claude Code reports a finished turn, or `blocked` on a question when that message asks one.
pub fn report(row: &SessionRow, since: i64) -> Vec<u8> {
    let group = Group::parse(&row.group_name).unwrap_or(Group::Idle);
    let replied = group == Group::Idle && row.last_report_at.is_some_and(|at| at > since);
    let asks = replied && row.last_report.as_deref().is_some_and(asks_a_question);
    let state = match group {
        Group::Working => "working",
        Group::Waiting => "blocked",
        // Waits on a usage limit: nothing for the user to answer.
        Group::Limited => "blocked",
        Group::Idle if asks => "blocked",
        Group::Idle if replied => "done",
        Group::Review | Group::Landing | Group::Resolved => "done",
        Group::Failed => "error",
        Group::Idle => "idle",
    };
    let mut fields = vec![format!("state={state}"), "app=claude-code".to_string()];
    let kind = match (group, row.waiting_reason.as_deref()) {
        (Group::Waiting, Some(kind @ ("permission" | "question"))) => Some(kind),
        (Group::Idle, _) if asks => Some("question"),
        _ => None,
    };
    if let Some(kind) = kind {
        fields.push(format!("kind={kind}"));
    }
    let title = match row.title.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(title) => title,
        None if row.role == Role::Coordinator.as_str() => "Coordinator",
        None => &row.name,
    };
    fields.push(format!("title={}", base64(title)));
    if let Some(work) = registry::background_work(row) {
        fields.push(format!("msg={}", base64(&format!("{work} running"))));
    }
    format!("\x1b]7501;{}\x1b\\", fields.join(":")).into_bytes()
}

/// A message that ends by asking something, as Claude Code tells one: a question mark at its end.
fn asks_a_question(text: &str) -> bool {
    let text = text.trim_end();
    text.ends_with(['?', '？']) && text.trim_end_matches(['?', '？']).trim_end().len() >= 4
}

/// Standard base64 with padding, as the protocol carries titles and messages.
fn base64(s: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in s.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Follows the relayed output far enough to tell when a report can go in without splitting an
/// escape sequence or a UTF-8 character of Claude's.
#[derive(Debug, Default)]
pub struct Boundary {
    state: Scan,
    /// Continuation bytes the current UTF-8 character still needs.
    utf8: u8,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    /// After ESC, or ESC and intermediates.
    Esc,
    /// In a CSI, until its final byte.
    Csi,
    /// In an OSC, DCS, SOS, PM or APC string, until BEL or ST.
    Str,
    /// ESC inside a string: ST if a backslash follows.
    StrEsc,
}

impl Boundary {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state = match (self.state, b) {
                (_, 0x18 | 0x1a) => Scan::Ground,
                (Scan::Str, 0x07) => Scan::Ground,
                (Scan::Str, 0x1b) => Scan::StrEsc,
                (Scan::Str, _) => Scan::Str,
                (Scan::StrEsc, b'\\') => Scan::Ground,
                (Scan::StrEsc, 0x1b) => Scan::StrEsc,
                (Scan::StrEsc, _) => Scan::Str,
                (_, 0x1b) => Scan::Esc,
                (Scan::Esc, b'[') => Scan::Csi,
                (Scan::Esc, b']' | b'P' | b'X' | b'^' | b'_') => Scan::Str,
                (Scan::Esc, 0x20..=0x2f) => Scan::Esc,
                (Scan::Esc, _) => Scan::Ground,
                (Scan::Csi, 0x40..=0x7e) => Scan::Ground,
                (Scan::Csi, _) => Scan::Csi,
                (Scan::Ground, _) => {
                    self.utf8 = match b {
                        0x80..=0xbf => self.utf8.saturating_sub(1),
                        0xc0..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf7 => 3,
                        _ => 0,
                    };
                    Scan::Ground
                }
            };
        }
    }

    /// Nothing of Claude's is cut in two by bytes written now.
    pub fn safe(&self) -> bool {
        self.state == Scan::Ground && self.utf8 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(group: Group, reason: Option<&str>) -> SessionRow {
        let mut row = SessionRow::new("demo-api-1a2b", "demo", crate::state::Role::Thread, 0);
        row.group_name = group.as_str().to_string();
        row.waiting_reason = reason.map(Into::into);
        row.title = Some("Rate limits".into());
        row
    }

    fn report_now(row: &SessionRow) -> Vec<u8> {
        report(row, 0)
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn base64_pads_as_the_protocol_expects() {
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("Rate limits"), "UmF0ZSBsaW1pdHM=");
    }

    /// The session's group gives the state, a waiting one says what it waits for, and its
    /// background work is the message.
    #[test]
    fn a_report_tells_the_shown_sessions_state() {
        assert_eq!(
            text(report_now(&row(Group::Working, None))),
            "\x1b]7501;state=working:app=claude-code:title=UmF0ZSBsaW1pdHM=\x1b\\"
        );
        assert_eq!(
            text(report_now(&row(Group::Waiting, Some("permission")))),
            "\x1b]7501;state=blocked:app=claude-code:kind=permission:title=UmF0ZSBsaW1pdHM=\x1b\\"
        );
        assert!(text(report_now(&row(Group::Review, None))).contains("state=done"));
        assert!(text(report_now(&row(Group::Failed, None))).contains("state=error"));
        assert!(text(report_now(&row(Group::Idle, None))).contains("state=idle"));

        let mut busy = row(Group::Working, None);
        busy.subagents_busy = 2;
        busy.title = None;
        let out = text(report(&busy, 0));
        assert!(
            out.contains(&format!("title={}", base64("demo-api-1a2b"))),
            "{out}"
        );
        assert!(
            out.contains(&format!("msg={}", base64("2 subagents running"))),
            "{out}"
        );
    }

    /// A turn that ended while the pane showed it is `done`, or `blocked` on a question when its
    /// last message asks one; an idle session last heard from before that is just idle.
    #[test]
    fn a_turn_that_ended_on_screen_is_done_or_asks() {
        let mut idle = row(Group::Idle, None);
        idle.last_report = Some("Opened PR #482 for the flaky test.".into());
        idle.last_report_at = Some(2_000);
        assert!(text(report(&idle, 1_000)).contains("state=done:"));
        assert!(
            text(report(&idle, 3_000)).contains("state=idle:"),
            "before the attach"
        );

        idle.last_report = Some("Should rate limits be per key or per plan? ".into());
        let out = text(report(&idle, 1_000));
        assert!(
            out.contains("state=blocked:app=claude-code:kind=question:"),
            "{out}"
        );
        assert!(text(report(&idle, 3_000)).contains("state=idle:"));

        let mut working = row(Group::Working, None);
        working.last_report = idle.last_report.clone();
        working.last_report_at = Some(2_000);
        assert!(text(report(&working, 1_000)).contains("state=working:"));
        assert!(!asks_a_question("Ok?"), "too short to be a question");
    }

    #[test]
    fn a_report_waits_out_sequences_and_characters() {
        let mut b = Boundary::default();
        assert!(b.safe());
        for (bytes, safe) in [
            (&b"plain"[..], true),
            (b"\x1b", false),
            (b"[38;2;1", false),
            (b"m", true),
            (b"\x1b]0;title", false),
            (b"\x1b", false),
            (b"\\", true),
            (b"\x1b]8;;url\x07", true),
            (b"\x1b(", false),
            (b"B", true),
            ("é".as_bytes().split_at(1).0, false),
            ("é".as_bytes().split_at(1).1, true),
            ("✻".as_bytes().split_at(2).0, false),
            ("✻".as_bytes().split_at(2).1, true),
            (b"\x1bP+q\x1b\\", true),
        ] {
            b.feed(bytes);
            assert_eq!(b.safe(), safe, "after {bytes:?}");
        }
    }
}
