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
        env_override("CATDESK_BUILD_SHA").or_else(git_sha).unwrap_or_else(|| UNKNOWN.to_string())
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

/// Rerun the build script when HEAD or a local branch ref changes, without
/// watching `.git/index` (every build would touch it).
///
/// Absolute paths are required in worktrees, where `.git` is a pointer file;
/// older gits without those flags degrade to crate-relative `.git` paths.
fn emit_git_watch_paths() {
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        let heads = git_output(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(|common| format!("{common}/refs/heads"))
            .unwrap_or_else(|| ".git/refs/heads".to_string());
        println!("cargo:rerun-if-changed={heads}");
    } else {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/refs/heads");
    }
}
