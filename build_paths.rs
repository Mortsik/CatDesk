//! Pure watch-path composition shared by `build.rs` and the crate's test run.
//!
//! `build.rs` includes this file via `#[path = "build_paths.rs"]` (a build
//! script never compiles its `#[cfg(test)]` code, so `cargo test` alone would
//! never execute those tests), and `src/main.rs` includes the same file as a
//! `#[cfg(test)]` module — the unit tests below therefore join the normal
//! `cargo test` run. Keep this file std-only and free of build-script
//! orchestration so both compilation contexts stay valid.

/// Compose the file path when `--git-path` cannot (pre-2.31 gits). Branch
/// refs, their reflogs, and `packed-refs` all live in the shared common dir,
/// which git reports absolute in worktrees and as a bare relative dir in the
/// main checkout, where the absolute git dir is already the right base.
pub(crate) fn compose_git_path(git_dir: &str, common: Option<&str>, rel: &str) -> String {
    match common {
        Some(common) if is_absolute(common) => format!("{common}/{rel}"),
        _ => format!("{git_dir}/{rel}"),
    }
}

/// Absolute per either path flavor: a POSIX leading `/` (which also covers
/// the `//server/share` UNC form git prints), a Windows drive prefix like
/// `C:/`, or a raw `\\server` UNC form.
pub(crate) fn is_absolute(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_uses_windows_drive_common_dir_in_linked_worktree() {
        // A Windows linked worktree reports its common dir as `C:/repo/.git`;
        // a POSIX-only check would send commits to the private per-worktree
        // dir and embed a stale SHA.
        assert_eq!(
            compose_git_path(
                "C:/repo/.git/worktrees/wt",
                Some("C:/repo/.git"),
                "refs/heads/feat/x",
            ),
            "C:/repo/.git/refs/heads/feat/x",
        );
    }

    #[test]
    fn compose_accepts_backslash_windows_common_dir() {
        // Mixed separators in the composed path are fine: git and the Windows
        // APIs both accept them.
        assert_eq!(
            compose_git_path(
                r"C:\repo\.git\worktrees\wt",
                Some(r"C:\repo\.git"),
                "refs/heads/main",
            ),
            r"C:\repo\.git/refs/heads/main",
        );
    }

    #[test]
    fn compose_uses_posix_absolute_common_dir() {
        assert_eq!(
            compose_git_path(
                "/repo/.git/worktrees/wt",
                Some("/repo/.git"),
                "refs/heads/main"
            ),
            "/repo/.git/refs/heads/main",
        );
    }

    #[test]
    fn compose_resolves_reflog_and_packed_refs_relatives() {
        // The packed-window watchers share the same composition: the branch
        // reflog lives under logs/, packed-refs directly in the common dir.
        assert_eq!(
            compose_git_path(
                "/repo/.git/worktrees/wt",
                Some("/repo/.git"),
                "logs/refs/heads/feat/x",
            ),
            "/repo/.git/logs/refs/heads/feat/x",
        );
        assert_eq!(
            compose_git_path("/repo/.git/worktrees/wt", Some("/repo/.git"), "packed-refs"),
            "/repo/.git/packed-refs",
        );
    }

    #[test]
    fn compose_falls_back_to_git_dir_for_relative_common() {
        // Main checkout: a bare `.git` is relative and git_dir is already
        // the absolute base.
        assert_eq!(
            compose_git_path("/repo/.git", Some(".git"), "refs/heads/main"),
            "/repo/.git/refs/heads/main",
        );
    }

    #[test]
    fn compose_falls_back_when_common_dir_unknown() {
        assert_eq!(
            compose_git_path("/repo/.git", None, "refs/heads/main"),
            "/repo/.git/refs/heads/main",
        );
    }

    #[test]
    fn is_absolute_recognizes_both_path_flavors() {
        assert!(is_absolute("/repo/.git"));
        assert!(is_absolute("//server/share/.git"));
        assert!(is_absolute("C:/repo/.git"));
        assert!(is_absolute(r"C:\repo\.git"));
        assert!(is_absolute(r"\\server\share\.git"));
        assert!(!is_absolute(".git"));
        assert!(!is_absolute(""));
        // No separator after the drive marker, or not a drive letter.
        assert!(!is_absolute("C:repo/.git"));
        assert!(!is_absolute("1:/repo/.git"));
    }
}
