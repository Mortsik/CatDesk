# Dashboard Cost Today + Single Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add persistent `COST TODAY`/per-day cost telemetry and collapse normal Status activity to one latest live flow row.

**Architecture:** Extend existing usage persistence with a local-calendar-day → model → `UsageTotals` map, keeping all-time totals unchanged and never backfilling legacy totals into today. Render daily and tracked-day aggregates in the existing TUI, and select only the most recently active visible `FlowLane` for the normal Status flow row.

**Tech Stack:** Rust, serde/TOML, ratatui, existing CatDesk usage persistence worker.

**Spec:** `docs/superpowers/specs/2026-09-23-dashboard-cost-today-single-flow.md`

## Global Constraints

- Preserve current all-time cost/token accounting and existing config migration behavior.
- Daily buckets are keyed by local calendar date and retain model buckets.
- Never fabricate historical daily data for installs that predate this feature.
- Normal Status shows exactly one live flow row; bootstrap/connect guide behavior remains separate.
- Token billing reset clears both all-time and per-day usage.

## Review Focus

- Existing config with all-time usage but no daily map must load without treating legacy spend as today.
- Two calls on the same day aggregate into one usage day; different date keys remain separate.
- Deferred usage persistence must write daily data together with all-time/request counters without stale snapshots overwriting newer full-state writes.
- Several simultaneously connected chat flows must still produce exactly one normal Status flow row, showing the most recently active flow.
- Narrow terminal layouts must keep request/cost rows stable and must not reintroduce multi-flow spam.

---

### Task 1: Single live Status flow row

**Files:**
- Modify/Test: `src/main.rs`

**Interfaces:**
- Consumes: `AppState::flows`, `should_display_flow_row`, newest-first ordering maintained by `AppState::record_flow`.
- Produces: normal Status renderer that chooses at most one visible `FlowLane`.

- [ ] **Step 1: Write the failing regression test**

Extend the dashboard flow test to record three active flows and assert that rendered text contains exactly one `Your computer` row and that it contains the latest flow's action.

- [ ] **Step 2: Run targeted test and verify RED**

Run: `cargo test main_dashboard_renders_only_latest_active_flow_row -- --nocapture`
Expected: FAIL because current renderer emits one row per visible flow.

- [ ] **Step 3: Implement minimal renderer change**

Replace the multi-flow `.filter(...).take(visible_flow_slots)` loop in normal Status with selection of the first/newest visible flow; preserve lane animation, action text and turn usage spans.

- [ ] **Step 4: Run targeted flow tests and verify GREEN**

Run: `cargo test main_dashboard_renders_only_latest_active_flow_row -- --nocapture`
Expected: PASS.

### Task 2: Persist per-day usage accounting

**Files:**
- Modify/Test: `src/state.rs`
- Modify/Test: `src/usage_persistence.rs`

**Interfaces:**
- Produces: `DailyUsageByModel = BTreeMap<String, BTreeMap<String, UsageTotals>>`, persisted in `AppConfig` and carried by deferred snapshots.
- Produces: helpers for current-day model usage, tracked usage-day count and deterministic day-key accumulation used by tests.

- [ ] **Step 1: Write failing persistence/accounting tests**

Add tests showing that usage recorded for explicit date keys aggregates per day/model, survives full/deferred persistence, and that an old config lacking the new field loads with an empty daily map.

- [ ] **Step 2: Run targeted state/persistence tests and verify RED**

Run the new named tests. They must fail against the current implementation because no daily field is serialized or retained.

- [ ] **Step 3: Implement minimal daily model buckets**

Add a serde-defaulted daily map to `AppConfig` and `AppState`, normalize nested totals, update `record_turn_usage` to accumulate in the local day bucket, and include the daily map in full/deferred persistence snapshots.

- [ ] **Step 4: Verify targeted persistence tests GREEN**

Run the new state/persistence tests and existing deferred persistence tests.

### Task 3: Cost TODAY / TOTAL UI

**Files:**
- Modify/Test: `src/main.rs`
- Modify/Test: `src/state.rs`

**Interfaces:**
- Consumes: current-day per-model usage and tracked daily buckets.
- Produces: `COST TODAY` row showing `SPENT`, `AVG .../CALL`, `CALLS`; `COST TOTAL` adds `DAYS` and `AVG .../DAY` based on tracked daily spend.

- [ ] **Step 1: Write failing dashboard telemetry assertions**

Update the live telemetry test to require `COST TODAY`, today's spend/calls/average, and `COST TOTAL` tracked days plus average/day. Add Traditional Chinese label assertion.

- [ ] **Step 2: Run targeted dashboard test and verify RED**

Run: `cargo test main_dashboard_renders_live_telemetry -- --nocapture`
Expected: FAIL because `COST TODAY` and daily aggregates do not exist yet.

- [ ] **Step 3: Implement cost aggregation/rendering**

Factor model-bucket cost estimation so it can price all-time and per-day maps. Render today's persisted usage and tracked daily average. When no tracked usage exists, render zero cost/calls/days and a zero average rather than inventing legacy history.

- [ ] **Step 4: Make billing reset coherent**

Clear the daily usage map whenever token billing totals are reset.

- [ ] **Step 5: Run dashboard/state tests GREEN**

Run the targeted telemetry, localization and persistence tests.

### Task 4: Verification and review

**Files:**
- All modified files.

- [ ] **Step 1: Format**

Run: `cargo fmt --check`

- [ ] **Step 2: Run full tests**

Run: `cargo test`

- [ ] **Step 3: Compile release/debug target**

Run: `cargo build`

- [ ] **Step 4: Review diff**

Check `git diff --check`, `git diff --stat`, and inspect the focused diff for config compatibility, stale persistence ordering and Status layout regressions.

- [ ] **Step 5: Commit in repository style**

Run `git log --oneline -n 5`, then commit only the feature/test/spec/plan changes with a matching English commit message.
