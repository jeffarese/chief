//! Slugs and session names (ARCHITECTURE §7).

/// Lowercase ASCII letters and digits joined by single hyphens, at most `max` chars, no
/// leading or trailing hyphen.
pub fn slugify(s: &str, max: usize) -> String {
    let _ = (s, max);
    todo!("M1")
}

pub fn coordinator_name(project_slug: &str) -> String {
    format!("chief-{project_slug}")
}

/// `<project slug>-<title slug, ≤24>-<4 hex>`; `suffix` is the 4 hex chars.
pub fn thread_name(project_slug: &str, title: &str, suffix: &str) -> String {
    let _ = (project_slug, title, suffix);
    todo!("M1")
}
