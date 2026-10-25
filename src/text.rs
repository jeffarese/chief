//! Small text helpers shared by the hook, the MCP tools and `chief status`.

/// Collapses whitespace and cuts to `max` chars with an ellipsis.
pub fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `ts` relative to `now` (both unix ms): `just now`, `5m ago`, `3h ago`, `2d ago`.
pub fn ago(now: i64, ts: i64) -> String {
    let secs = (now - ts).max(0) / 1000;
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// Local wall-clock `HH:MM` for a unix ms timestamp.
pub fn local_hhmm(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ts) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%H:%M").to_string()
        }
        chrono::LocalResult::None => "??:??".to_string(),
    }
}

/// Quotes one word for `sh`: safe characters pass through, anything else goes in single quotes.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if safe {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_cuts_and_flattens() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        assert_eq!(one_line("ééééé", 5), "ééééé");
    }

    #[test]
    fn ago_buckets() {
        let m = 60_000;
        assert_eq!(ago(10 * m, 10 * m), "just now");
        assert_eq!(ago(10 * m, 20 * m), "just now", "clock skew");
        assert_eq!(ago(10 * m, 5 * m), "5m ago");
        assert_eq!(ago(200 * m, 0), "3h ago");
        assert_eq!(ago(3 * 1440 * m, 0), "3d ago");
    }

    #[test]
    fn hhmm_shape() {
        let s = local_hhmm(1_792_940_000_000);
        assert_eq!(s.len(), 5);
        assert_eq!(&s[2..3], ":");
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/usr/bin/chief"), "/usr/bin/chief");
        assert_eq!(shell_quote("--project"), "--project");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        // Round trip through a real shell.
        let word = "a 'b' $c \"d\" \\e";
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf %s {}", shell_quote(word))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), word);
    }
}
