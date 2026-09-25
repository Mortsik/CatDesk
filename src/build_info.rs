//! Compile-time build identity, frozen by `build.rs` before this crate builds.
//!
//! Display formatting lives here so it stays pure and testable; consumers
//! (TUI header, splash, health, process-started diagnostics) only call it.

// Consumers land across the build-identity commits; until then this module's
// surface is exercised by its tests only.
#![allow(dead_code)]

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const GIT_SHA: &str = env!("CATDESK_GIT_SHA");
pub(crate) const GIT_BRANCH: &str = env!("CATDESK_GIT_BRANCH");
pub(crate) const BUILD_TIMESTAMP: &str = env!("CATDESK_BUILD_TIMESTAMP");

fn short_sha(sha: &str) -> String {
    let is_full_hash = sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit());
    if is_full_hash {
        sha[..7].to_string()
    } else {
        String::new()
    }
}

fn version_with_sha(version: &str, git_sha: &str) -> String {
    match short_sha(git_sha) {
        short if short.is_empty() => version.to_string(),
        short => format!("{version}+g{short}"),
    }
}

pub(crate) fn version_label(version: &str, git_sha: &str) -> String {
    format!("v{}", version_with_sha(version, git_sha))
}

fn build_date(build_timestamp: &str) -> Option<&str> {
    if build_timestamp == "unknown" || build_timestamp.len() < 10 {
        return None;
    }
    let date = &build_timestamp[..10];
    // RFC3339 dates are ASCII; refuse to slice if an override strayed into
    // multibyte territory, so the date boundary always exists.
    if date.is_char_boundary(date.len()) {
        Some(date)
    } else {
        None
    }
}

pub(crate) fn identity_line(
    version: &str,
    git_sha: &str,
    git_branch: &str,
    build_timestamp: &str,
) -> String {
    let mut line = version_with_sha(version, git_sha);
    if !git_branch.is_empty() && git_branch != "unknown" {
        line.push(' ');
        line.push_str(git_branch);
    }
    if let Some(date) = build_date(build_timestamp) {
        line.push(' ');
        line.push_str(date);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_SHA: &str = "1a2b3c4d5e6f7890abcdef1234567890abcdef12";

    #[test]
    fn short_sha_truncates_full_hash_to_seven_chars() {
        assert_eq!(short_sha(FULL_SHA), "1a2b3c4");
    }

    #[test]
    fn short_sha_maps_unknown_and_empty_to_empty() {
        assert_eq!(short_sha("unknown"), "");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn short_sha_keeps_only_full_hash_shaped_input() {
        // Short and odd inputs are never truncation candidates.
        assert_eq!(short_sha("deadbee"), "");
        assert_eq!(short_sha("1a2b3c4"), "");
        assert_eq!(short_sha("zz2b3c4d5e6f7890abcdef1234567890abcdef12"), "");
    }

    #[test]
    fn version_label_appends_short_sha_when_known() {
        assert_eq!(version_label("0.9.2", FULL_SHA), "v0.9.2+g1a2b3c4");
    }

    #[test]
    fn version_label_stays_clean_without_git_metadata() {
        assert_eq!(version_label("0.9.2", "unknown"), "v0.9.2");
        assert_eq!(version_label("0.9.2", ""), "v0.9.2");
    }

    #[test]
    fn identity_line_joins_known_segments() {
        assert_eq!(
            identity_line("0.9.2", FULL_SHA, "main", "2026-09-25T08:15:42+02:00"),
            "0.9.2+g1a2b3c4 main 2026-09-25"
        );
    }

    #[test]
    fn identity_line_omits_unknown_segments() {
        assert_eq!(
            identity_line("0.9.2", "unknown", "unknown", "unknown"),
            "0.9.2"
        );
        assert_eq!(
            identity_line("0.9.2", "unknown", "feature/x", "unknown"),
            "0.9.2 feature/x"
        );
        assert_eq!(
            identity_line("0.9.2", FULL_SHA, "unknown", "2026-09-25T08:15:42Z"),
            "0.9.2+g1a2b3c4 2026-09-25"
        );
    }

    #[test]
    fn identity_line_omits_too_short_timestamp() {
        assert_eq!(
            identity_line("0.9.2", "unknown", "unknown", "2026-9"),
            "0.9.2"
        );
    }
}
