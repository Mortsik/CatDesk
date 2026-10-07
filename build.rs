//! Freezes compile-time build identity into the crate.
//!
//! Identity priority per field: `CATDESK_BUILD_*` env override, then a git
//! probe rooted at `CARGO_MANIFEST_DIR`, then the literal `"unknown"` — so a
//! tarball build without `.git` is a first-class outcome. `git describe` is
//! deliberately avoided because it fails in shallow CI checkouts.

use std::env;
use std::path::Path;
use std::process::Command;

// Shared with the crate's test run: a build script never compiles
// #[cfg(test)] code, so src/main.rs includes the same file as a test-only
// module to give the watch-path composition tests a `cargo test` home.
#[path = "build_paths.rs"]
mod build_paths;

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
/// the checkout's own `HEAD` file (checkouts, detached commits) plus the
/// branch-ref files that exist right now — see [`watch_branch_files`].
/// Worktrees share the common ref store, so watching `refs/heads` wholesale
/// (the original behavior) rebuilt on every sibling-worktree commit.
///
/// Absolute paths are required in worktrees, where `.git` is a pointer file;
/// older gits without those flags degrade to crate-relative `.git` paths.
fn emit_git_watch_paths() {
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        if let Some(head_ref) = git_output(&["symbolic-ref", "HEAD"]) {
            let common = git_output(&["rev-parse", "--git-common-dir"]);
            watch_branch_files(&git_dir, common.as_deref(), &head_ref);
        }
    } else {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/refs/heads");
    }
}

/// Watch the branch-ref files that change when this branch's commit changes:
/// its loose ref, its reflog, and the shared `packed-refs`. Each is emitted
/// only while the file exists — cargo treats a missing watched path as
/// permanently dirty, so a ref packed away by `git pack-refs` must leave the
/// watch set instead.
///
/// The reflog is what keeps coverage full while the ref is packed: every
/// commit appends to it even when the loose ref file is gone, so the first
/// post-pack commit still triggers a rerun. `packed-refs` catches the
/// loose→packed transition (pack-refs/gc rewrite it) and lets the watch set
/// re-adapt on the next script run; those rewrites are rare, so the extra
/// reruns are rare. With reflogs disabled, a commit onto a packed branch can
/// go unnoticed until the next trigger — a narrow gap (non-bare repos
/// default to reflogs on).
fn watch_branch_files(git_dir: &str, common: Option<&str>, head_ref: &str) {
    let relatives = [
        head_ref.to_string(),
        format!("logs/{head_ref}"),
        "packed-refs".to_string(),
    ];
    for rel in &relatives {
        let path = git_file_path(git_dir, common, rel);
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Filesystem path of one git-managed file, e.g. the `refs/heads/<branch>`
/// loose file this checkout's `HEAD` points at, its reflog, or `packed-refs`.
fn git_file_path(git_dir: &str, common: Option<&str>, rel: &str) -> String {
    if let Some(path) = git_output(&["rev-parse", "--path-format=absolute", "--git-path", rel]) {
        return path;
    }
    build_paths::compose_git_path(git_dir, common, rel)
}
