# CatDesk Live Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add live active-job, connected-chat, and trailing-60-second token/USD burn telemetry to the main CatDesk TUI dashboard.

**Architecture:** Extend `AppState` with a privacy-safe set of active hashed flow IDs plus a bounded deque of timestamped usage samples. Add a read-only async active-job counter to `CommandJobManager`, sampled by the TUI at 1 Hz. Render the resulting metrics in the existing status panel while reusing the current pricing function and token formatting helpers.

**Tech Stack:** Rust 2024, Tokio, Ratatui, existing CatDesk state/server/job manager.

**Spec:** `docs/superpowers/specs/2026-09-21-catdesk-live-telemetry.md`

## Global Constraints

- No new dependency.
- No raw MCP session IDs stored in UI telemetry.
- Active-job counting must not run at the 60 Hz TUI redraw rate; sample at most once per second.
- Existing `Session` and `All-time` billing totals and reset behavior remain unchanged.
- Existing request/flow lifecycle remains unchanged.

## Review Focus

- A closed named session must decrement chat count without disconnecting surviving named sessions; test flow-A/flow-B lifecycle independently.
- `SetRemoteConnected(false)` must clear stale chat IDs if a close event was lost; test explicit disconnect clearing.
- Usage exactly outside the 60-second window must not contribute to burn rate; test the boundary with explicit timestamps.
- Usage samples must not alter persisted/all-time accounting semantics; assert the normal totals still accumulate identically.
- Active-job count must drop after a job reaches a terminal state; exercise start → terminal in the real manager.

---

### Task 1: Rolling 60-second usage telemetry

**Files:**
- Modify: `src/state.rs`
- Test: `src/state.rs` test module

**Interfaces:**
- Produces: `AppState::rolling_usage_totals(now_ms: u128, window_ms: u128) -> UsageTotals`
- Produces: private `record_turn_usage_at(tool_input_tokens: u64, tool_output_tokens: u64, now_ms: u128)` used by production `record_turn_usage`
- Consumed by Task 3: `rolling_usage_totals(..., 60_000)`

- [ ] **Step 1: Write the failing tests**

Add tests that record explicit samples at `1_000`, `30_000`, and `61_001` ms, then assert a 60,000 ms query at `61_001` includes the latter two but excludes the oldest. Also assert normal session/all-time totals still include every usage event.

```rust
#[test]
fn rolling_usage_totals_only_include_the_requested_window() {
    let (mut app, workspace, config_path) = test_app("catdesk-rolling-usage");
    app.record_turn_usage_at(100, 10, 1_000);
    app.record_turn_usage_at(200, 20, 30_000);
    app.record_turn_usage_at(300, 30, 61_001);

    let rolling = app.rolling_usage_totals(61_001, 60_000);
    assert_eq!(rolling.tool_input_tokens, 500);
    assert_eq!(rolling.tool_output_tokens, 50);
    assert_eq!(rolling.total_tokens, 550);
    assert_eq!(rolling.tool_call_count, 2);

    let session = app.session_usage_totals.clone();
    assert_eq!(session.tool_input_tokens, 600);
    assert_eq!(session.tool_output_tokens, 60);
    assert_eq!(session.tool_call_count, 3);

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_dir_all(workspace);
}
```

Add a second test proving old samples are pruned after a later event so the deque cannot grow indefinitely.

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test rolling_usage -- --nocapture`

Expected: compilation/test failure because `record_turn_usage_at` and `rolling_usage_totals` do not exist.

- [ ] **Step 3: Implement minimal rolling-window state**

In `src/state.rs` add:

```rust
#[derive(Clone, Debug)]
struct UsageRateSample {
    recorded_at_ms: u128,
    tool_input_tokens: u64,
    tool_output_tokens: u64,
}

const LIVE_USAGE_WINDOW_MS: u128 = 60_000;
```

Add `usage_rate_samples: VecDeque<UsageRateSample>` to `AppState`, initialize it empty, route `record_turn_usage` through `record_turn_usage_at(..., now_unix_millis())`, prune samples older than `LIVE_USAGE_WINDOW_MS`, and implement `rolling_usage_totals` by summing samples with age `<= window_ms` and incrementing tool-call count once per included sample.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test rolling_usage -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

Commit message: `feat: track rolling usage telemetry`

---

### Task 2: Connected-chat and active-job counts

**Files:**
- Modify: `src/state.rs`
- Modify: `src/command_jobs.rs`
- Test: `src/state.rs` and `src/command_jobs.rs`

**Interfaces:**
- Produces: `AppState::connected_chat_count() -> usize`
- Produces: `CommandJobManager::active_job_count(&self) -> usize`
- Consumed by Task 3: both values are rendered in the main status panel; job count is sampled at 1 Hz.

- [ ] **Step 1: Write failing state tests for chat lifecycle**

Add a test that records flow `session:a` and `session:b`, asserts count `2`, closes only `session:a`, asserts `1`, then applies `SetRemoteConnected(false)` and asserts `0`.

```rust
#[test]
fn connected_chat_count_tracks_independent_flow_lifecycles() {
    let (mut app, workspace, config_path) = test_app("catdesk-connected-chats");
    app.record_flow("session:a", &["tools/call:read".into()], FlowDirection::Forward);
    app.record_flow("session:b", &["tools/call:read".into()], FlowDirection::Forward);
    assert_eq!(app.connected_chat_count(), 2);

    app.begin_flow_close("session:a");
    assert_eq!(app.connected_chat_count(), 1);

    app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected(false));
    assert_eq!(app.connected_chat_count(), 0);

    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_dir_all(workspace);
}
```

- [ ] **Step 2: Run the state test and verify RED**

Run: `cargo test connected_chat_count -- --nocapture`

Expected: FAIL because `connected_chat_count` is missing.

- [ ] **Step 3: Implement connected-chat tracking**

Add `connected_chat_ids: HashSet<String>` to `AppState`. Insert `flow_id` in `record_flow`, remove it in `begin_flow_close`, clear it when handling `SetRemoteConnected(false)`, and expose `connected_chat_count()`.

- [ ] **Step 4: Run state test and verify GREEN**

Run: `cargo test connected_chat_count -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Write failing real-manager test for active jobs**

Add an async test that starts `sleep 0.3` (PowerShell equivalent on Windows), asserts `active_job_count() == 1`, waits terminal with existing helper, then asserts `0`.

- [ ] **Step 6: Run job test and verify RED**

Run: `cargo test active_job_count_tracks_running_jobs -- --nocapture`

Expected: compilation failure because `active_job_count` does not exist.

- [ ] **Step 7: Implement `active_job_count`**

Clone the current job `Arc`s under the manager read lock, release it, lock each runtime, and count `CommandJobState::Running`. Do not call cleanup and do not add a timer inside the manager; Task 3 owns the 1 Hz sampling cadence.

- [ ] **Step 8: Run job test and verify GREEN**

Run: `cargo test active_job_count_tracks_running_jobs -- --nocapture`

Expected: PASS.

- [ ] **Step 9: Commit**

Commit message: `feat: expose live workload counts`

---

### Task 3: Render live telemetry with 1 Hz job sampling

**Files:**
- Modify: `src/main.rs`
- Test: `src/main.rs` test module

**Interfaces:**
- Consumes: `AppState::rolling_usage_totals(now_ms, 60_000) -> UsageTotals`
- Consumes: `AppState::connected_chat_count() -> usize`
- Consumes: `CommandJobManager::active_job_count().await -> usize`
- Produces: main status rows showing chats, active jobs, token/min and USD/min/hour.

- [ ] **Step 1: Write failing rendering test**

Extend a focused `draw_ui` test fixture with two active flow IDs and explicit rolling usage. Render to a `TestBackend` and assert the text contains `Chats 2`, `Jobs 3`, `60s rate`, `/min`, and `/h`. Pass an explicit `active_job_count: usize` parameter into `draw_ui` so rendering stays deterministic.

- [ ] **Step 2: Run rendering test and verify RED**

Run: `cargo test main_dashboard_renders_live_telemetry -- --nocapture`

Expected: FAIL/compile failure because `draw_ui` has no active-job-count parameter and does not render live telemetry.

- [ ] **Step 3: Implement rendering helpers and row**

Add a helper that formats the 60-second usage line using existing `format_token_compact`, `estimate_gpt_5_6_and_earlier_usage_cost_usd`, and `format_usd_compact`. Render chats/jobs on the `Remote connected` line and add the `60s rate` line before `Session`.

- [ ] **Step 4: Add 1 Hz job sampling in `run_tui`**

Define `const LIVE_TELEMETRY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);`. Keep `last_live_telemetry_refresh: Instant` and cached `active_job_count: usize` local to `run_tui`. When one second has elapsed, call `app.command_jobs.active_job_count().await`, update the cache, and pass the cached value to `draw_ui`. Do not query the manager on every redraw.

Update existing `draw_ui` call sites/tests to pass an explicit count.

- [ ] **Step 5: Run rendering and localization tests**

Run: `cargo test main_dashboard_renders -- --nocapture`

Expected: PASS, including Traditional Chinese dashboard test.

- [ ] **Step 6: Run full suite**

Run: `cargo test`

Expected: PASS with zero failures.

- [ ] **Step 7: Commit**

Commit message: `feat: show live telemetry in dashboard`
