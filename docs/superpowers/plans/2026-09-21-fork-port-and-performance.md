# Fork Port And Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the highest-value CatDesk fork improvements: workspace path hardening, cheaper background-job polling, MCP/search caching, Windows browser detection, lower-idle TUI rendering cost, and native Windows UI Automation computer-use support.

**Architecture:** Keep each change independently testable and commit-sized. Re-implement behavior against the current CatDesk architecture instead of merging divergent forks wholesale; preserve current multi-project/session isolation and current MCP payload contracts.

**Tech Stack:** Rust 1.93 / edition 2024, Tokio, Axum/MCP JSON-RPC, Ratatui/Crossterm, windows-sys.

**Spec:** Conversation-approved fork audit from 2026-09-21; no separate spec file.

## Global Constraints

- Do not weaken workspace containment or Linux sandboxing.
- Preserve final exact `changedFiles` behavior for terminal command jobs.
- Do not add periodic work to hot paths when caching can be invalidated by file metadata.
- Keep Windows-only computer-use code behind `cfg(windows)` and non-Windows builds green.
- Do not push or merge to `main` during implementation; integration is a separate operator choice.

## Review Focus

- Missing paths under symlinks must not escape the workspace.
- Running `poll_command` must not repeatedly perform recursive change snapshots; terminal results must remain exact.
- Config/AGENTS caches must invalidate after file changes rather than becoming stale.
- Windows browser detection must find standard Chrome/Edge/Brave installs without breaking PATH discovery.
- UI Automation and screenshot coordinate actions must reject stale targets and remain unavailable or error cleanly off Windows.

---

### Task 1: Workspace symlink containment

**Files:**
- Modify: `src/command.rs`
- Test: inline unit tests in `src/command.rs`

**Interfaces:**
- Produces: path resolver that follows existing symlink components even when the final path does not exist.

- [ ] Add regression tests for missing child below an external symlink and a dangling external symlink target.
- [ ] Run the focused tests and verify RED.
- [ ] Implement `canonicalize_allow_missing` with bounded symlink resolution and use it in workspace/command path resolution.
- [ ] Run focused tests and full command module tests; verify GREEN.
- [ ] Commit `fix: close workspace symlink escape`.

### Task 2: Terminal-only background-job change scans

**Files:**
- Modify: `src/command_jobs.rs`
- Test: inline unit tests in `src/command_jobs.rs`

**Interfaces:**
- Produces: `current_changes_for_session` returns empty while running and caches one exact final diff once terminal.

- [ ] Add a test that changes a file during a running job, observes no running change report, then gets the final report twice.
- [ ] Verify RED.
- [ ] Add a per-job `OnceLock<Vec<FileChange>>` final change cache and terminal-state gate.
- [ ] Verify focused tests and command-job suite GREEN.
- [ ] Commit `perf: defer background change scans until completion`.

### Task 3: MCP config/instruction caching

**Files:**
- Modify: `src/mcp.rs`
- Test: inline unit tests in `src/mcp.rs`

**Interfaces:**
- Produces: metadata-keyed caches for app config and AGENTS content; instruction text is computed once per tool response.

- [ ] Add tests proving AGENTS/config values refresh after a file metadata change.
- [ ] Verify RED.
- [ ] Add `FileStamp`, config cache, AGENTS text cache, and reuse structured instruction text.
- [ ] Verify focused tests GREEN.
- [ ] Commit `perf: cache repeated MCP instruction reads`.

### Task 4: Widget payload/static resource reuse

**Files:**
- Modify: `src/mcp.rs`
- Test: inline unit tests in `src/mcp.rs`

**Interfaces:**
- Produces: shared changed-file JSON values and one-time base64 encoding of embedded widget images.

- [ ] Add assertions that structured changedFiles and widget changedFiles remain equivalent and embedded image placeholders are fully rendered.
- [ ] Verify RED for the new reuse-oriented helper expectations.
- [ ] Add `OnceLock<String>` image encodings and shared changed-file JSON conversion path.
- [ ] Verify focused tests GREEN.
- [ ] Commit `perf: reuse widget payload and resource encodings`.

### Task 5: Search backend availability cache

**Files:**
- Modify: `src/workspace_tools.rs`
- Test: inline unit tests where practical; existing search tests cover fallback behavior.

**Interfaces:**
- Produces: process-lifetime cached availability checks for `rg` and `grep` only.

- [ ] Add a testable probe helper boundary or source-level behavior test that distinguishes cached named backends from generic probes.
- [ ] Verify RED.
- [ ] Add `OnceLock<bool>` caches for `rg` and `grep`, retaining existing execution fallback behavior.
- [ ] Verify search tests GREEN.
- [ ] Commit `perf: cache search backend probes`.

### Task 6: Windows installed-browser detection

**Files:**
- Modify: `src/browser.rs`
- Modify: `.github/workflows/windows-verify.yml` only if the current workflow lacks useful browser-specific verification.
- Test: inline Windows-gated tests in `src/browser.rs`

**Interfaces:**
- Produces: fallback lookup in Program Files / Program Files (x86) / LocalAppData for Chrome, Edge and Brave.

- [ ] Add Windows-gated mapping/path-resolution tests.
- [ ] Verify Windows-target compile/test expectations fail before implementation where locally checkable.
- [ ] Implement standard install-path resolver after PATH lookup.
- [ ] Run host suite plus `cargo check --target x86_64-pc-windows-gnu` or available Windows target check if installed; otherwise rely on cfg compile coverage in CI and report limitation.
- [ ] Commit `fix: detect Windows installed browsers`.

### Task 7: Event-driven / dirty TUI rendering

**Files:**
- Modify: `src/main.rs`
- Test: inline unit tests around redraw decision helpers/state where feasible.

**Interfaces:**
- Produces: redraw only when app/UI state, animation, reveal timer, toast, selection, or input state changes; keyboard polling remains responsive.

- [ ] Extract a small redraw-decision helper/state representation and add tests for unchanged vs changed frames.
- [ ] Verify RED.
- [ ] Convert steady-state loops, especially the main dashboard, to skip `terminal.draw` on unchanged ticks while preserving animation/reveal timing.
- [ ] Verify unit tests and full suite GREEN.
- [ ] Commit `perf: avoid unconditional TUI redraws`.

### Task 8: Native Windows UI Automation computer-use

**Files:**
- Create: `src/desktop.rs`
- Create: `src/uia.rs`
- Modify: `src/main.rs`
- Modify: `src/mcp.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `THIRD_PARTY_NOTICES.md` if code is adapted from third-party MIT sources.
- Test: inline module/MCP tests.

**Interfaces:**
- Produces: `screenshot`, `ui_tree`, `ui_click`, `mouse_move`, `mouse_click`, `mouse_scroll`, `type_text`, `key_press` for Windows computer mode; screenshot returns a monotonically increasing capture ID, coordinate actions optionally validate it.

- [ ] Add MCP descriptor/dispatch tests for Windows computer tools and stale-capture rejection tests for coordinate mapping logic that can run cross-platform.
- [ ] Verify RED.
- [ ] Implement platform-neutral screen-plan/capture-id state plus Windows GDI/SendInput backend under `cfg(windows)`.
- [ ] Implement Windows UI Automation foreground tree capture and indexed element resolution/clicking under `cfg(windows)` with clean non-Windows errors.
- [ ] Wire MCP descriptors, output schemas and read-only/write-tool gating without compact base64-in-text payloads or automatic screenshot files.
- [ ] Verify host full suite and available Windows target check GREEN.
- [ ] Commit `feat: add Windows UI Automation computer control`.

### Task 9: Whole-branch verification and review

**Files:** none unless review finds defects.

- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo test --release`.
- [ ] Run `cargo build --release`.
- [ ] Run available Windows target check.
- [ ] Review `git diff main...HEAD` for security regressions, payload growth, blocking hot paths, and duplicated fork-specific behavior that conflicts with current CatDesk architecture.
- [ ] Fix Critical/Important findings via RED→GREEN only.
