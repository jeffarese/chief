//! Slugs and session names (ARCHITECTURE §7).

/// Lowercase ASCII letters and digits joined by single hyphens, at most `max` chars, no
/// leading or trailing hyphen.
pub fn slugify(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            gap = false;
        } else {
            gap = true;
        }
    }
    out.truncate(max);
    out.trim_end_matches('-').to_string()
}

pub fn coordinator_name(project_slug: &str) -> String {
    format!("chief-{project_slug}")
}

/// `<project slug>-<title slug, ≤24>-<4 hex>`; `suffix` is the 4 hex chars.
pub fn thread_name(project_slug: &str, title: &str, suffix: &str) -> String {
    let mut title = slugify(title, 24);
    if title.is_empty() {
        title = "thread".to_string();
    }
    format!("{project_slug}-{title}-{suffix}")
}

/// 4 random hex chars for `thread_name`.
pub fn random_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..4].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_cases() {
        let cases = [
            ("Payments latency", 64, "payments-latency"),
            ("  Fix: the  API!! ", 64, "fix-the-api"),
            ("--a--b--", 64, "a-b"),
            ("Ünïcode café 2", 64, "n-code-caf-2"),
            ("", 64, ""),
            ("!!!", 64, ""),
            ("abcdef ghij", 7, "abcdef"),
            ("abc def", 3, "abc"),
            ("UPPER", 64, "upper"),
        ];
        for (input, max, want) in cases {
            assert_eq!(slugify(input, max), want, "slugify({input:?}, {max})");
        }
    }

    #[test]
    fn slugify_never_exceeds_max() {
        let s = slugify("a very long title that keeps going and going", 24);
        assert!(s.len() <= 24);
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn thread_names() {
        assert_eq!(
            thread_name("payments", "Check p95 latency", "1a2b"),
            "payments-check-p95-latency-1a2b"
        );
        assert_eq!(
            thread_name(
                "p",
                "Investigate the flaky checkout integration test",
                "beef"
            ),
            "p-investigate-the-flaky-ch-beef"
        );
        assert_eq!(thread_name("p", "???", "0000"), "p-thread-0000");
        assert_eq!(coordinator_name("p"), "chief-p");
    }

    #[test]
    fn suffix_is_four_hex() {
        let s = random_suffix();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
