//! Freezes compile-time build identity into the crate.
//!
//! Identity priority per field: `CATDESK_BUILD_*` env override, then a git
//! probe rooted at `CARGO_MANIFEST_DIR`, then the literal `"unknown"` — so a
//! tarball build without `.git` is a first-class outcome. `git describe` is
//! deliberately avoided because it fails in shallow CI checkouts.

use std::env;
use std::process::Command;

const UNKNOWN: &str = "unknown";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_watch_paths();
    println!(
        "cargo:rustc-env=CATDESK_GIT_SHA={}",
        env_override("CATDESK_BUILD_SHA")
            .or_else(git_sha)
            .unwrap_or_else(|| UNKNOWN.to_string())
    );
    println!(
        "cargo:rustc-env=CATDESK_GIT_BRANCH={}",
        env_override("CATDESK_BUILD_BRANCH").unwrap_or_else(git_branch)
    );
    println!(
        "cargo:rustc-env=CATDESK_BUILD_TIMESTAMP={}",
        env_override("CATDESK_BUILD_TIMESTAMP")
            .or_else(git_timestamp)
            .unwrap_or_else(|| UNKNOWN.to_string())
    );
}

/// Env overrides; `unknown` and empty both mean "unset" so CI can force the
/// fallback path.
fn env_override(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != UNKNOWN)
}

/// One `git` invocation rooted at the crate, trimmed stdout or `None`.
fn git_output(args: &[&str]) -> Option<String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn git_sha() -> Option<String> {
    git_output(&["rev-parse", "HEAD"])
}

fn git_branch() -> String {
    match git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).as_deref() {
        Some("HEAD") => env::var("GITHUB_REF_NAME")
            .ok()
            .map(|branch| branch.trim().to_string())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| UNKNOWN.to_string()),
        Some(branch) => branch.to_string(),
        None => UNKNOWN.to_string(),
    }
}

fn git_timestamp() -> Option<String> {
    git_output(&["log", "-1", "--format=%cI"])
}

/// Rerun the build script only when THIS checkout's HEAD identity changes:
/// the checkout's own `HEAD` file plus the one loose ref file it points at.
/// Worktrees share the common ref store, so watching `refs/heads` wholesale
/// (the previous behavior) rebuilt on every sibling-worktree commit.
///
/// Absolute paths are required in worktrees, where `.git` is a pointer file;
/// older gits without those flags degrade to crate-relative `.git` paths.
/// Watching the loose ref path even while it does not exist is safe: cargo
/// fingerprints creation too, so a packed ref going loose on its next commit
/// still reruns the script.
fn emit_git_watch_paths() {
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        if let Some(head_ref) = git_output(&["symbolic-ref", "HEAD"]) {
            println!(
                "cargo:rerun-if-changed={}",
                head_ref_file(&git_dir, &head_ref)
            );
        }
    } else {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/refs/heads");
    }
}

/// Filesystem path of one ref's loose file — here, the `refs/heads/<branch>`
/// this checkout's `HEAD` points at. Detached HEAD never reaches this: the
/// `HEAD` file itself carries the commit and is already watched.
fn head_ref_file(git_dir: &str, head_ref: &str) -> String {
    if let Some(path) = git_output(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        head_ref,
    ]) {
        return path;
    }
    // Pre-2.31 gits: branch refs live in the shared common dir, reported
    // absolute in worktrees and as a bare relative dir in the main checkout,
    // where the absolute git dir is already the right base.
    let common = git_output(&["rev-parse", "--git-common-dir"]);
    compose_ref_file(git_dir, common.as_deref(), head_ref)
}

/// Compose the loose ref file path when `--git-path` cannot. Git prints
/// host-flavored paths, so absoluteness has to cover Windows drive and UNC
/// forms too — not just the POSIX leading slash — or a linked Windows
/// worktree would watch its private per-worktree dir and miss its own
/// commits, embedding a stale SHA.
fn compose_ref_file(git_dir: &str, common: Option<&str>, head_ref: &str) -> String {
    match common {
        Some(common) if is_absolute(common) => format!("{common}/{head_ref}"),
        _ => format!("{git_dir}/{head_ref}"),
    }
}

/// Absolute per either path flavor: a POSIX leading `/` (which also covers
/// the `//server/share` UNC form git prints), a Windows drive prefix like
/// `C:/`, or a raw `\\server` UNC form.
fn is_absolute(path: &str) -> bool {
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
            compose_ref_file(
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
            compose_ref_file(
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
            compose_ref_file(
                "/repo/.git/worktrees/wt",
                Some("/repo/.git"),
                "refs/heads/main"
            ),
            "/repo/.git/refs/heads/main",
        );
    }

    #[test]
    fn compose_falls_back_to_git_dir_for_relative_common() {
        // Main checkout: a bare `.git` is relative and git_dir is already
        // the absolute base.
        assert_eq!(
            compose_ref_file("/repo/.git", Some(".git"), "refs/heads/main"),
            "/repo/.git/refs/heads/main",
        );
    }

    #[test]
    fn compose_falls_back_when_common_dir_unknown() {
        assert_eq!(
            compose_ref_file("/repo/.git", None, "refs/heads/main"),
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
