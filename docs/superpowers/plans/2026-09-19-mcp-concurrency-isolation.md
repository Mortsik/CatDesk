# MCP Concurrency Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Increase safe parallel MCP throughput while ensuring slow filesystem, process, or browser work cannot starve CatDesk control operations or freeze unrelated clients.

**Architecture:** Introduce session-aware request context, resource-class admission pools, and one shared process budget. Keep control-plane work separately provisioned, keep blocking filesystem work off the async reactor, bound mount traversal and persistence races, and retain stateless-client compatibility.

**Tech Stack:** Rust, Tokio, Axum, ignore/WalkBuilder, existing CatDesk process runner and MCP server.

**Spec:** `docs/superpowers/specs/2026-09-19-mcp-concurrency-isolation-design.md`

## Global Constraints

- Preserve pure-HTTP MCP and current clients that do not send `Mcp-Session-Id`.
- `ping` remains outside admission pools.
- Do not lower useful aggregate MCP concurrency as the primary overload mechanism.
- No unbounded queues, session registries, output buffers, or process counts.
- Every production behavior change follows RED → GREEN TDD.
- Existing `cargo test` suite must remain green; existing project-wide clippy debt is not part of this patch.

## Review Focus

- A saturated filesystem class must not prevent `cancel_command`, `poll_command`, `server/discover`, or `ping` from completing.
- Two clients using the same JSON-RPC IDs must never accidentally share command-job ownership or idempotency.
- A client disconnect/timeout must not leak admission or process capacity after underlying work actually terminates.
- Recursive workspace operations must not enter a nested foreign mount on Linux/WSL.
- Concurrent configuration updates must never expose truncated TOML or lose an unrelated field update.

---

### Task 1: Session-aware MCP request context

**Files:**
- Modify: `src/server.rs`
- Modify: `src/mcp.rs`
- Test: `src/server.rs`, `src/mcp.rs`

**Interfaces:**
- Produces: `ClientSession`/request context carrying optional stable session namespace and UI-safe flow id.
- Consumes later: Task 2 request classification and Task 3 command idempotency namespace.

- [ ] **Step 1: Write failing tests** proving two stable sessions keep instruction state separate, same session keeps state across requests, raw session ids are not used as UI flow ids, and anonymous fallback stays compatible.
- [ ] **Step 2: Run targeted tests** and verify they fail because session-aware state does not exist yet.
- [ ] **Step 3: Implement bounded session registry** with TTL/cap, optional `Mcp-Session-Id` extraction, hashed flow identity, and anonymous fallback.
- [ ] **Step 4: Route instruction gating and UI flow events through request session context.**
- [ ] **Step 5: Run targeted tests and full `cargo test`.**
- [ ] **Step 6: Commit** with `fix: isolate MCP client session state`.

### Task 2: Resource-class admission scheduler

**Files:**
- Modify: `src/request_workers.rs`
- Modify: `src/server.rs`
- Test: `src/request_workers.rs`, `src/server.rs`

**Interfaces:**
- Produces: `RequestClass` and `RequestScheduler::run(class, future, deadline)`.
- Consumes: Task 1 request parsing/session context only for diagnostics, not capacity ownership.

- [ ] **Step 1: Write failing starvation tests** that saturate filesystem/process capacity while control capacity remains usable and ping remains outside scheduling.
- [ ] **Step 2: Run targeted tests** and verify the current single pool fails the isolation expectation.
- [ ] **Step 3: Implement class-specific worker pools** with reserved control capacity, high filesystem parallelism, dedicated process/browser pools, and bounded general fallback.
- [ ] **Step 4: Classify MCP requests before dispatch** (`poll`, `cancel`, instruction/bootstrap as control; local FS tools as filesystem; command launch/run as process; DevTools as browser/general).
- [ ] **Step 5: Emit class-specific overload diagnostics and fast errors without waiting in a queue.**
- [ ] **Step 6: Run targeted tests and full `cargo test`.**
- [ ] **Step 7: Commit** with `fix: isolate MCP workload admission pools`.

### Task 3: Shared foreground/background process budget and job ownership

**Files:**
- Modify: `src/command_jobs.rs`
- Modify: `src/mcp.rs`
- Modify: `src/process_runner.rs` only if permit lifetime requires runner integration
- Test: `src/command_jobs.rs`, `src/mcp.rs`

**Interfaces:**
- Produces: shared `ProcessBudget` exposed by `CommandJobManager`, total capacity 12; background cap remains 8.
- Consumes: Task 1 stable session namespace for idempotency/ownership.

- [ ] **Step 1: Write failing tests** for same-session retry dedupe, different-session same-id/same-args distinct jobs, anonymous same-id/same-args distinct jobs, and foreground/background sharing one capacity budget.
- [ ] **Step 2: Run targeted tests** and verify expected failures.
- [ ] **Step 3: Namespace `start_command` request keys by stable session and disable request-id dedupe for anonymous clients.**
- [ ] **Step 4: Add shared process semaphore** held for the complete process-tree lifetime for foreground and background commands.
- [ ] **Step 5: Ensure poll/cancel never consume process permits and capacity recovers after terminate/cancel/timeout.**
- [ ] **Step 6: Run targeted tests and full `cargo test`.**
- [ ] **Step 7: Commit** with `fix: bound shared command process capacity`.

### Task 4: Mount-safe and memory-bounded recursive workspace operations

**Files:**
- Modify: `src/workspace_tools.rs`
- Test: `src/workspace_tools.rs`
- Reference: `src/change_tracking/snapshot.rs`

**Interfaces:**
- Produces: recursive search/list semantics that remain inside the starting filesystem.

- [ ] **Step 1: Write failing tests** for walker configuration/helper behavior and bounded built-in file scanning; use an existing foreign filesystem path when available without requiring privileged mounts.
- [ ] **Step 2: Run targeted tests** and verify failures on current traversal behavior.
- [ ] **Step 3: Add `same_file_system(true)` to built-in search walker and one-filesystem behavior to ripgrep path.**
- [ ] **Step 4: Prevent recursive list BFS from descending across device boundaries on Unix/WSL while preserving existing sorting/filter/limit behavior.**
- [ ] **Step 5: Bound built-in search reads instead of `fs::read` of arbitrary-size files.**
- [ ] **Step 6: Run targeted tests and full `cargo test`.**
- [ ] **Step 7: Commit** with `fix: keep workspace scans inside local filesystem`.

### Task 5: Persistence and lossy-UI hardening

**Files:**
- Modify: `src/state.rs`
- Modify: `src/server.rs` only if request persistence snapshotting changes
- Test: `src/state.rs`

**Interfaces:**
- Produces: serialized config mutation, temp-file replacement, order-independent UI telemetry application.

- [ ] **Step 1: Write failing tests** for concurrent unrelated config field updates preserving both values, parseable config during repeated writes, and turn-usage event for a missing/dropped flow not panicking.
- [ ] **Step 2: Run targeted tests** and verify current behavior fails or panics.
- [ ] **Step 3: Serialize config read-modify-write operations** with a short process-local lock and implement same-directory temp-file replacement.
- [ ] **Step 4: Make flow usage telemetry tolerant of dropped predecessor events.**
- [ ] **Step 5: Minimize synchronous persistence time under `AppState` lock where safely possible without introducing stale snapshots.**
- [ ] **Step 6: Run targeted tests and full `cargo test`.**
- [ ] **Step 7: Commit** with `fix: harden concurrent state persistence`.

### Task 6: Multi-client concurrency stress verification

**Files:**
- Modify/Create tests in: `src/server.rs`, `src/request_workers.rs`, `src/command_jobs.rs`
- Modify: `docs/connection-diagnostics.md` only if new diagnostic event names are added

**Interfaces:**
- Consumes all prior task interfaces.

- [ ] **Step 1: Add stress regression test** spawning many independent request tasks with repeated JSON-RPC ids across stable sessions while mixing control, filesystem, and process-class admission.
- [ ] **Step 2: Verify bounded overload behavior**: some workload requests may fail fast as busy, but control requests complete and no task hangs.
- [ ] **Step 3: Verify no capacity leaks** after cancellations/timeouts and ensure final process/job counts return below limits.
- [ ] **Step 4: Run `cargo test` from a fresh command and read the terminal result.**
- [ ] **Step 5: Run `cargo build` and targeted concurrency tests repeatedly to catch flakiness.**
- [ ] **Step 6: Review `git diff` for accidental unrelated changes.**
- [ ] **Step 7: Commit** with `test: stress parallel MCP workload isolation`.

## Self-review

- Spec coverage: session identity, admission isolation, process budget, mount safety, persistence and lossy UI are each assigned to a task.
- Deferred by spec: automatic per-session worktree creation, fully concurrent DevTools multiplexing, and ngrok supervisor are not silently included in this patch.
- Interface consistency: Task 1 session namespace feeds Task 3 idempotency; Task 2 scheduler is independent of process lifetime budget in Task 3.
- No task relies on a global serial request mutex; aggregate concurrency remains resource-class based.