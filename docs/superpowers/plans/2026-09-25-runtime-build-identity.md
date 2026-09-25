# Runtime Build Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose package version + git SHA + build timestamp + branch in the running CatDesk process (TUI header, splash, health endpoint, process_started diagnostics) so a running binary can be told apart from a freshly built one without filesystem inspection.

**Architecture:** A `build.rs` probes git at compile time (env override → `git rev-parse`/`git log` → `"unknown"` fallback) and emits `cargo:rustc-env` constants that `src/build_info.rs` freezes into `const`s at compile time. Pure formatting functions (`short_sha`, `version_label`, `identity_line`) own every display shape and are unit-tested inline. Consumers: TUI header (compact `v0.9.2+g1a2b3c4`), startup splash (same label), `GET /health` full branch (flat `version`/`git_sha`/`git_branch`/`build_time`), diagnostics `process_started` record (version + full identity line).

**Tech Stack:** Rust 2024, cargo `build.rs`, `serde_json` (health/diagnostics), ratatui (header layout), no new dependencies.

**Spec:** bead `catdesk-vmi.3` (no separate spec file; the bead description and epic `catdesk-vmi` invariant are the authority — compact MCP payloads: identity appears once at process start and in `GET /health` only; per-request diagnostic records stay untouched).

## Global Constraints

- Identity is compile-time only; no `git` invocation at runtime, ever.
- `build.rs` must always emit all three env vars (`CATDESK_GIT_SHA`, `CATDESK_GIT_BRANCH`, `CATDESK_BUILD_TIMESTAMP`) — a tarball build without `.git` is a first-class outcome and yields clean `v0.9.2` with no `+gunknown` suffix.
- Never truncate the literal string `"unknown"` as if it were a hex SHA; `short_sha` maps non-40-hex input to `""`.
- `state.rs` `last_started_version` keeps storing pure semver (`CARGO_PKG_VERSION`) — it is compared across runs and must not absorb build metadata.
- Health busy fast-path (lock unavailable) stays minimal: `status`/`name`/`description`/`busy` only, no identity fields.
- Per-request diagnostics records keep their existing shape; only `process_started` gains identity fields.
- Tests live inline (`#[cfg(test)] mod tests` in `src/` files); no new `tests/*.rs`.

## Review Focus

- Narrow terminals: header layout is `Min(0)` title + `Length(version_width)` label, so the title shrinks — accepted trade-off, verify no panic at width 20.
- Tarball build (no `.git`): compact label is clean `v0.9.2`, full line omits `unknown` segments instead of printing them.
- `last_started_version` still pure semver.
- Rebuild without a HEAD change does not recompile (rerun-if-changed pins `.git` HEAD + `refs/heads`, not mtime of the whole tree).
- Health busy fast-path payload unchanged in size and shape.

---

### Task 1: `build_info` module with compile-time identity (skeleton)

**Files:**
- Add: `build.rs`, `src/build_info.rs`
- Modify: `src/main.rs` (single `mod build_info;` line)

**Interfaces:**
- Produces: `const VERSION`, `const GIT_SHA`, `const GIT_BRANCH`, `const BUILD_TIMESTAMP`; `fn short_sha(&str) -> String`; `fn version_label(version: &str, git_sha: &str) -> String`; `fn identity_line(version: &str, git_sha: &str, git_branch: &str, build_timestamp: &str) -> String`.

- [ ] **Step 1: Write failing formatting tests**

`src/build_info.rs` with `#[cfg(test)] mod tests` covering: `short_sha` truncates 40-hex to 7; `short_sha("unknown")` → `""`; `short_sha("")` → `""`; never contains `"unknown"`; `version_label` → `v0.9.2+g1a2b3c4` with SHA, clean `v0.9.2` without; `identity_line` uses `ts[..10]` date, space-separated, `unknown` segments omitted, all-unknown → bare version.

- [ ] **Step 2: Run targeted test and verify RED**

Run: `cargo test build_info`
Expected: FAIL — module referenced but functions not implemented yet (compile error).

- [ ] **Step 3: Implement + skeleton build.rs**

`build.rs` emitting `cargo:rustc-env` for all three vars with literal `"unknown"` (no git yet); consts via `env!()`; pure fns; `mod build_info;` in `main.rs` (alphabetical position near `mod browser;`).

- [ ] **Step 4: Run targeted test and verify GREEN**

Run: `cargo test build_info`
Expected: PASS.

### Task 2: derive git identity in build.rs with safe fallbacks

**Files:**
- Modify: `build.rs`

**Interfaces:**
- Consumes: env overrides `CATDESK_BUILD_SHA` / `CATDESK_BUILD_BRANCH` / `CATDESK_BUILD_TIMESTAMP` (highest priority), then git from `$CARGO_MANIFEST_DIR`, then `"unknown"`.
- Produces: real SHA/branch/timestamp in the emitted env vars; `cargo:rerun-if-changed` for `build.rs` + `<gitdir>/HEAD` + `<gitcommondir>/refs/heads`.

- [ ] **Step 1: manual probe verification (RED-ish)**

In a scratch copy of the crate without `.git`, `cargo build` and assert `build_info::GIT_SHA == "unknown"`; in the worktree, build and assert SHA matches `git rev-parse HEAD`; rebuild unchanged → no recompile.

- [ ] **Step 2: implement git probe**

`Command::new("git")` with `.current_dir(env CARGO_MANIFEST_DIR)`: `rev-parse HEAD` → SHA; `rev-parse --abbrev-ref HEAD` → branch (`"HEAD"` → env `GITHUB_REF_NAME` → `"unknown"`); `log -1 --format=%cI` → timestamp. No `git describe` (fails in shallow CI). Rerun-if-changed via `git rev-parse --absolute-git-dir` and `git rev-parse --path-format=absolute --git-common-dir`, falling back to relative `.git/HEAD` / `.git/refs/heads` when git is unavailable. Skip `.git/index`.

- [ ] **Step 3: re-verify manual probes (GREEN)**

All three manual checks from Step 1 hold with the probe in place; `cargo test build_info` still green.

### Task 3: show build sha in TUI header and splash

**Files:**
- Modify: `src/main.rs` (`draw_tui_header` + its test), `src/startup.rs` (`version_label`)

**Interfaces:**
- Consumes: `build_info::version_label(build_info::VERSION, build_info::GIT_SHA)`.
- Produces: header right edge `v0.9.2+g1a2b3c4` (or clean `v0.9.2` without git); splash delegates to the same fn.

- [ ] **Step 1: update header test (RED)**

Test expects `row.ends_with(&format!("{} │", build_info::version_label(...)))`.

- [ ] **Step 2: run test, verify FAIL** on the old `v{CARGO_PKG_VERSION}` rendering.

- [ ] **Step 3: implement** — `draw_tui_header` uses `format!("{} ", build_info::version_label(...))`; `startup.rs` `version_label()` delegates to `build_info`.

- [ ] **Step 4: GREEN** — targeted header/splash tests pass; existing `startup.rs` `contains("v0.9.2")` assertions pass unchanged.

### Task 4: expose identity in health and process_started diagnostics

**Files:**
- Modify: `src/server.rs` (full health branch + new test), `src/diagnostics.rs` (`init` + pure record fn + test)

**Interfaces:**
- Consumes: `build_info::{VERSION, GIT_SHA, GIT_BRANCH, BUILD_TIMESTAMP, identity_line}`.
- Produces: health full branch flat `version`=`"0.9.2+g1a2b3c4"`, `git_sha`, `git_branch`, `build_time` (full RFC3339); `process_started` record `{"event", "version", "build": identity_line}` via pure `process_started_record()`.

- [ ] **Step 1: write failing tests** — health (unlocked, `AppState::new_for_test` pattern from `health_remains_responsive_while_app_state_is_locked`) asserts 4 non-empty string fields; busy-path test asserts `version` absent; diagnostics test pins `process_started_record` shape.

- [ ] **Step 2: run, verify RED.**

- [ ] **Step 3: implement** both call sites (busy fast-path untouched).

- [ ] **Step 4: GREEN** — targeted tests pass, then full `cargo test`, then `cargo test --release` + `cargo build --release` + `cargo fmt`.
