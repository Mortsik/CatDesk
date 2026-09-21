# Unbounded CatDesk Concurrency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove CatDesk's artificial request/job/process concurrency limits so concurrent work starts immediately without CPU/RAM/load-based throttling, while preserving timeout, cancellation, durability, dedupe, process-tree cleanup, and output bounds.

**Architecture:** Delete the existing request-class semaphores and command-process/job admission caps instead of replacing them with another scheduler. Keep `spawn_blocking` for synchronous MCP work and keep each command job's existing lifecycle machinery; only remove admission throttles. The OS/runtime becomes the only physical concurrency limiter.

**Tech Stack:** Rust 2024, Tokio, Axum, existing CatDesk MCP/request-worker/command-job modules; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-21-adaptive-fair-scheduler-design.md`

## Global Constraints

- Do not inspect CPU, RAM, RSS, load average, queue depth, browser occupancy, or process counts to decide whether work may start.
- Do not add a replacement semaphore, fair queue, resource governor, adaptive target, or hidden concurrency cap.
- No normal hard quota per user, session, project, request class, foreground command, background command, or process tree.
- Preserve request deadlines. A timed-out/disconnected synchronous request may keep running underneath, but it must not block unrelated requests through a CatDesk capacity slot.
- Preserve `start_command` durability, request-id dedupe, owner-session isolation, timeout, heartbeat abandonment, cancellation, shutdown cancellation, process-tree termination, and restart behavior (`Running -> Interrupted`).
- Preserve bounded per-job output, bounded poll response size, terminal-job retention/TTL, and terminal-output retention budget. Those are storage/response bounds, not concurrency controls.
- Preserve `catdesk_instruction` gating, project scoping, symlink/path hardening, and usage telemetry behavior.
- Windows native computer-use/UIA and stale-action hardening remain out of scope.
- Do not perform the broad `mcp.rs` / `main.rs` refactor from `catdesk-vmi.6`.

## Review Focus

- A request that times out while its underlying `spawn_blocking` work continues must not prevent another same-class request from starting immediately.
- Aborting a caller must not cancel or corrupt already-started synchronous work, and must not create any CatDesk admission lock for later calls.
- More than eight long-running `start_command` jobs must all be accepted and observable concurrently; dedupe must still return one job for one request key.
- Foreground `run_command` must execute successfully while many background jobs are running; no shared process permit may remain indirectly in MCP code.
- Shutdown racing with many concurrent starts must still either reject a late start as shutting down or cancel any start that won the race; removing `start_lock` is not allowed.

---

### Task 1: Remove Request-Worker Admission Limits

**Files:**
- Modify: `src/request_workers.rs:1-418`
- Modify: `src/server.rs:23,487-512,3867-3923`
- Test: inline tests in `src/request_workers.rs` and existing HTTP tests in `src/server.rs`

**Interfaces:**
- Consumes: existing `RequestClass`, `RequestScheduler::run(class, work, deadline)`, and `request_deadline(class)` call shape.
- Produces:
  - `RequestFailure::{Deadline, Failed}` only; remove `Busy`.
  - `RequestScheduler::new() -> Self` remains so `server.rs` can keep a small stable call site.
  - `RequestScheduler::run(class, work, deadline)` remains, but `class` is classification/diagnostic metadata only and does not select a capacity pool.
  - synchronous work still runs inside `tokio::task::spawn_blocking` and is wrapped by the existing response deadline.

- [ ] **Step 1: Replace capacity-oriented unit tests with RED unbounded-concurrency tests**

Remove tests whose expected result is `RequestFailure::Busy` and add tests that require same-class work to start concurrently beyond all former limits.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filesystem_requests_start_beyond_the_former_pool_limit() {
    let scheduler = RequestScheduler::new();
    let started = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let mut tasks = Vec::new();

    for index in 0..24usize {
        let scheduler = scheduler.clone();
        let started = started.clone();
        let release = release.clone();
        tasks.push(tokio::spawn(async move {
            scheduler
                .run(
                    RequestClass::Filesystem,
                    async move {
                        started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        index
                    },
                    Duration::from_secs(5),
                )
                .await
        }));
    }

    let all_started = tokio::time::timeout(Duration::from_secs(2), async {
        while started.load(std::sync::atomic::Ordering::SeqCst) != 24 {
            tokio::task::yield_now().await;
        }
    })
    .await;

    // Always release already-started blocking workers before asserting RED/GREEN,
    // so the failing pre-change test cannot strand spawn_blocking threads.
    release.add_permits(24);
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }

    assert!(
        all_started.is_ok(),
        "former filesystem limit still blocked request starts"
    );
    for (index, result) in results.into_iter().enumerate() {
        assert_eq!(result, Ok(index));
    }
}

#[tokio::test]
async fn timed_out_work_does_not_gate_a_later_same_class_request() {
    let scheduler = RequestScheduler::new();
    let (release, wait) = std::sync::mpsc::channel();
    let watchdog = release.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let _ = watchdog.send(());
    });

    let timed_out = scheduler
        .run(
            RequestClass::Filesystem,
            async move { let _ = wait.recv(); },
            Duration::from_millis(20),
        )
        .await;
    assert_eq!(timed_out, Err(RequestFailure::Deadline));

    let later = scheduler
        .run(
            RequestClass::Filesystem,
            async { 42u8 },
            Duration::from_secs(1),
        )
        .await;
    assert_eq!(later, Ok(42));
    let _ = release.send(());
}

#[tokio::test]
async fn aborted_caller_does_not_gate_a_later_request() {
    let scheduler = RequestScheduler::new();
    let (release, wait) = std::sync::mpsc::channel();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let running = scheduler.clone();
    let caller = tokio::spawn(async move {
        running
            .run(
                RequestClass::Filesystem,
                async move {
                    let _ = started_tx.send(());
                    let _ = wait.recv_timeout(Duration::from_secs(2));
                },
                Duration::from_secs(3),
            )
            .await
    });
    started_rx.await.unwrap();
    caller.abort();
    let _ = caller.await;

    assert_eq!(
        scheduler
            .run(
                RequestClass::Filesystem,
                async { 7u8 },
                Duration::from_secs(1),
            )
            .await,
        Ok(7)
    );
    let _ = release.send(());
}
```

Keep the existing `synchronous_tool_does_not_starve_runtime_timer` test because `spawn_blocking` remains a requirement.

- [ ] **Step 2: Run targeted request-worker tests RED**

Run:

```bash
cargo test --locked request_workers::tests -- --nocapture
```

Expected before implementation: at least the >16 filesystem test fails because only 16 filesystem permits exist, and old tests still refer to `Busy` semantics until replaced.

- [ ] **Step 3: Remove semaphores and fixed request limits with the smallest API change**

Change `src/request_workers.rs` to a stateless scheduler. Delete `Arc<Semaphore>`, `RequestLimits`, per-class `RequestWorkers`, `with_limits`, and `workers(class)`.

The core implementation should become equivalent to:

```rust
use std::{future::Future, time::Duration};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    Deadline,
    Failed,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RequestScheduler;

impl RequestScheduler {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) async fn run<F>(
        &self,
        _class: RequestClass,
        work: F,
        deadline: Duration,
    ) -> Result<F::Output, RequestFailure>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || runtime.block_on(work));
        match tokio::time::timeout(deadline, task).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RequestFailure::Failed),
            Err(_) => Err(RequestFailure::Deadline),
        }
    }
}
```

Do not replace the deleted semaphore with a Tokio task semaphore, channel, queue, mutex count, `JoinSet` cap, or CPU-derived limit.

- [ ] **Step 4: Remove dead `Busy` HTTP/diagnostic handling**

In `src/server.rs`:

1. keep `RequestClass` and `request_deadline(class)`;
2. keep the process-global `LazyLock<RequestScheduler>` if desired for minimal change;
3. simplify `request_failure_event()` to only map `Deadline` and `Failed`;
4. remove `StatusCode::SERVICE_UNAVAILABLE` mapping for `RequestFailure::Busy`;
5. retain `GATEWAY_TIMEOUT` for `Deadline` and `INTERNAL_SERVER_ERROR` for `Failed`;
6. update the comment above `post_mcp_http` so it no longer describes busy worker pools.

Expected mapping:

```rust
fn request_failure_event(
    _class: RequestClass,
    failure: &crate::request_workers::RequestFailure,
) -> &'static str {
    use crate::request_workers::RequestFailure;
    match failure {
        RequestFailure::Deadline => "request_worker_timeout",
        RequestFailure::Failed => "request_worker_failed",
    }
}
```

- [ ] **Step 5: Run targeted request/server tests GREEN**

Run:

```bash
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked server::tests::ping_does_not_wait_for_ui_state_lock -- --exact
cargo test --locked server::tests::parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated -- --exact
```

Expected: all pass, with no `RequestFailure::Busy` branch remaining in production code.

- [ ] **Step 6: Audit for hidden request admission limits**

Run:

```bash
rg -n "RequestFailure::Busy|Semaphore::new|try_acquire_owned|RequestLimits|with_limits" src/request_workers.rs src/server.rs
```

Expected: no concurrency-admission match in `request_workers.rs`; no `RequestFailure::Busy` mapping in `server.rs`. Matches elsewhere must be unrelated to request concurrency or handled in Task 2.

- [ ] **Step 7: Commit Task 1**

Before commit run:

```bash
git diff --check
git status --short
git log --oneline -n 5
```

Then:

```bash
git add src/request_workers.rs src/server.rs
git commit -m "perf: remove request concurrency limits"
```

---

### Task 2: Remove Background/Foreground Command Admission Limits

**Files:**
- Modify: `src/command_jobs.rs:1-2422`
- Modify: `src/mcp.rs:1839-1849,5361-5399`
- Test: inline tests in `src/command_jobs.rs` and `src/mcp.rs`

**Interfaces:**
- Consumes: existing `CommandJobManager::{new, with_store, start_with_change_session, active_job_count, cancel, cancel_session, cancel_all}` and `run_job` lifecycle.
- Produces:
  - `CommandJobManager` without `process_budget`, `process_limit`, `with_process_limit`, `new_with_process_limit`, or `try_acquire_process`.
  - `start_with_change_session(...)` accepts any number of running jobs unless shutdown/dedupe/validation rejects for a non-concurrency reason.
  - `run_job(job, cancel_rx)` no longer accepts or owns an `OwnedSemaphorePermit`.
  - foreground `run_command` calls `command::run_command(...)` directly after existing validation/intercepts.

- [ ] **Step 1: Replace old cap/budget tests with RED unlimited-command tests**

Delete `shared_process_budget_is_held_by_background_jobs_until_termination` and `active_job_limit_is_enforced_and_recovers_after_cancel` because those assert the behavior being removed.

Add:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_than_eight_background_jobs_run_concurrently() {
    let root = workspace("unbounded-active");
    let manager = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 3"
    } else {
        "sleep 3"
    };

    let mut ids = Vec::new();
    for index in 0..16usize {
        let started = manager
            .start(
                command.to_string(),
                root.clone(),
                10_000,
                Some(format!("unbounded-{index}")),
            )
            .await
            .expect("artificial active-job limit rejected a start");
        assert_eq!(started.snapshot.state, CommandJobState::Running);
        ids.push(started.snapshot.job_id);
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if manager.active_job_count().await >= 16 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all accepted jobs did not become active");

    assert_eq!(manager.active_job_count().await, 16);
    manager.cancel_all().await;
    for id in ids {
        assert!(manager.poll(&id, 0, 0).await.is_ok());
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn duplicate_request_key_still_deduplicates_under_many_active_jobs() {
    let root = workspace("unbounded-dedup");
    let manager = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 3"
    } else {
        "sleep 3"
    };

    for index in 0..12usize {
        manager
            .start(
                command.to_string(),
                root.clone(),
                10_000,
                Some(format!("other-{index}")),
            )
            .await
            .unwrap();
    }

    let first = manager
        .start(
            command.to_string(),
            root.clone(),
            10_000,
            Some("same-key".into()),
        )
        .await
        .unwrap();
    let second = manager
        .start(
            command.to_string(),
            root.clone(),
            10_000,
            Some("same-key".into()),
        )
        .await
        .unwrap();

    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(first.snapshot.job_id, second.snapshot.job_id);
    manager.cancel_all().await;
    let _ = std::fs::remove_dir_all(root);
}
```

Also keep the existing tests for shutdown races, cancellation, restart recovery, output bounds, heartbeat abandonment, and session ownership unchanged.

- [ ] **Step 2: Run command-job tests RED**

Run:

```bash
cargo test --locked command_jobs::tests::more_than_eight_background_jobs_run_concurrently -- --exact --nocapture
cargo test --locked command_jobs::tests::duplicate_request_key_still_deduplicates_under_many_active_jobs -- --exact --nocapture
```

Expected before implementation: the ninth start fails with the current `MAX_ACTIVE_JOBS = 8` error and/or process budget.

- [ ] **Step 3: Remove process/job admission state from `CommandJobManager`**

In `src/command_jobs.rs`:

1. remove `MAX_ACTIVE_JOBS` and `MAX_ACTIVE_PROCESSES`;
2. remove `OwnedSemaphorePermit` and `Semaphore` imports;
3. remove `process_budget` and `process_limit` fields;
4. remove `with_process_limit`, test constructor `new_with_process_limit`, and `try_acquire_process`;
5. retain `start_lock` because dedupe and shutdown/start atomicity still need serialization;
6. in `start_with_change_session`, delete the `active_count >= MAX_ACTIVE_JOBS` check;
7. delete `let process_permit = self.try_acquire_process()?;`;
8. spawn `run_job(job.clone(), cancel_rx)` directly.

Construct the manager directly:

```rust
impl Default for CommandJobManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(ManagerState::default())),
            start_lock: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            abandon_after_ms: DEFAULT_ABANDON_AFTER_MS,
            store: JobStore::disabled(),
            recovery: Arc::new(OnceCell::new()),
        }
    }
}

impl CommandJobManager {
    pub fn with_store(dir: PathBuf) -> Self {
        Self {
            store: JobStore::open(dir),
            ..Self::default()
        }
    }
}
```

Change `run_job` to:

```rust
async fn run_job(
    job: Arc<CommandJob>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    // existing cancelled-before-spawn check and process lifecycle unchanged
}
```

Update direct test calls from `run_job(job.clone(), cancel_rx, None).await` to `run_job(job.clone(), cancel_rx).await`.

Do not alter these non-concurrency constants:

```rust
MAX_RETAINED_JOBS
TERMINAL_JOB_TTL
MAX_OUTPUT_BYTES_PER_JOB
MAX_TERMINAL_OUTPUT_BYTES
MAX_POLL_OUTPUT_BYTES
MAX_JOB_TIMEOUT_MS
DEFAULT_ABANDON_AFTER_MS
```

- [ ] **Step 4: Replace MCP shared-process-budget test with concurrent foreground/background success**

Remove the production permit acquisition in `handle_run_command`:

```rust
let _process_permit = match command_jobs.try_acquire_process() { ... };
```

`command::run_command(...)` should execute directly after the existing validation/intercept code.

Replace `run_command_shares_process_budget_with_background_jobs` with:

```rust
#[tokio::test]
async fn run_command_is_not_blocked_by_running_background_jobs() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-unbounded-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let background = if cfg!(windows) {
        "Start-Sleep -Seconds 3"
    } else {
        "sleep 3"
    };

    let mut ids = Vec::new();
    for index in 0..16usize {
        let started = command_jobs
            .start(
                background.to_string(),
                workspace_root.clone(),
                10_000,
                Some(format!("bg-{index}")),
            )
            .await
            .expect("start background job");
        ids.push(started.snapshot.job_id);
    }

    let foreground = if cfg!(windows) {
        "Write-Output foreground-ok"
    } else {
        "printf foreground-ok"
    };
    let req = tool_call_request("run_command", json!({ "command": foreground }));
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;

    assert_ne!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(result_text(&response).contains("foreground-ok"));
    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}
```


- [ ] **Step 5: Run targeted command/MCP tests GREEN**

Run:

```bash
cargo test --locked command_jobs::tests::more_than_eight_background_jobs_run_concurrently -- --exact --nocapture
cargo test --locked command_jobs::tests::duplicate_request_key_still_deduplicates_under_many_active_jobs -- --exact --nocapture
cargo test --locked mcp::tests::run_command_is_not_blocked_by_running_background_jobs -- --exact --nocapture
cargo test --locked command_jobs::tests::start_racing_with_shutdown_cannot_escape_cancellation -- --exact --nocapture
cargo test --locked command_jobs::tests::recovery_marks_running_records_interrupted_and_rewrites_them -- --exact --nocapture
```

Expected: all pass.

- [ ] **Step 6: Audit for hidden command concurrency caps**

Run:

```bash
rg -n "MAX_ACTIVE_JOBS|MAX_ACTIVE_PROCESSES|process_budget|process_limit|try_acquire_process|new_with_process_limit|with_process_limit|too many active command" src/command_jobs.rs src/mcp.rs
```

Expected: zero matches.

Also search broadly for replacement concurrency admission logic:

```bash
rg -n "Semaphore::new|try_acquire_owned|acquire_owned|available_permits|active.*maximum|maximum.*active" src/command_jobs.rs src/mcp.rs
```

Any match must be reviewed and must not be a replacement process/job concurrency throttle.

- [ ] **Step 7: Commit Task 2**

Before commit run:

```bash
git diff --check
git status --short
git log --oneline -n 5
```

Then:

```bash
git add src/command_jobs.rs src/mcp.rs
git commit -m "perf: remove command concurrency limits"
```

---

### Task 3: 25-Session HTTP Verification and Final Hardening

**Files:**
- Modify: `src/server.rs:3384-3504`
- Test: existing `parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated`
- No new production module.

**Interfaces:**
- Consumes: unbounded `RequestScheduler` from Task 1 and unbounded `CommandJobManager` from Task 2.
- Produces: one deterministic HTTP-level regression proving 25 named sessions complete without CatDesk-generated concurrency rejection while session/job isolation remains intact.

- [ ] **Step 1: Expand the existing parallel session test so it crosses both former caps**

Change `parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated` from 18 sessions / 6 background jobs to 25 sessions with 10 simultaneous `start_command` calls. Use a 3-5 second sleep command in this fixture so all ten background jobs remain active until admission is complete; `cancel_all()` already terminates them at the end.

Use this distribution:

```rust
for index in 0..25usize {
    let body = match index % 5 {
        0 => mcp_request_body("tools/list", json!({})),
        1 | 4 => tool_call_body("read", json!({ "paths": ["hello.txt"] })),
        2 | 3 => tool_call_body("start_command", json!({ "command": command })),
        _ => unreachable!(),
    };
    // existing per-session headers + post_mcp_http task spawn
}
```

Update the result assertions to match the same distribution and expect:

```rust
assert_eq!(process_jobs.len(), 10);
```

The ten command jobs must all have unique ids despite every session reusing the same JSON-RPC request id.

- [ ] **Step 2: Add an explicit no-concurrency-rejection assertion**

When each parallel response is collected, keep the existing `StatusCode::OK` assertion and include the response payload in its failure message. For `start_command`, assert a job id is present; do not accept an MCP result containing text such as `too many active command jobs`, `too many active command processes`, or `CatDesk is busy`.

Example assertion:

```rust
let rendered = payload.to_string();
assert!(!rendered.contains("too many active command"));
assert!(!rendered.contains("CatDesk is busy"));
```

This is a regression guard, not user-facing error parsing.

- [ ] **Step 3: Run the final 25-session verification GREEN**

Tasks 1-2 already established RED coverage for the request and command caps before production changes. This HTTP test is the final integration regression across both completed changes.

Run:

```bash
cargo test --locked server::tests::parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated -- --exact --nocapture
```

Expected: PASS; all 25 sessions return HTTP 200; exactly 10 unique process jobs are owned by their respective sessions.

- [ ] **Step 4: Run complete concurrency-related regression groups**

Run:

```bash
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked command_jobs::tests -- --nocapture
cargo test --locked mcp::tests -- --nocapture
cargo test --locked server::tests -- --nocapture
```

Expected: all pass. If any existing test still expects `Busy`, an active-job cap, or a process permit, update/remove that obsolete expectation only after confirming it asserts the intentionally removed behavior.

- [ ] **Step 5: Run full suite and source audit**

Run:

```bash
cargo test --locked
rg -n "MAX_ACTIVE_JOBS|MAX_ACTIVE_PROCESSES|RequestFailure::Busy|process_budget|process_limit|try_acquire_process|new_with_process_limit|with_process_limit|resource_governor|HostPressure|loadavg|MemAvailable|VmRSS" src
```

Expected:
- full suite passes;
- zero matches for removed command/request admission symbols;
- no newly added resource-governor/CPU/RAM concurrency code.

A generic CPU/RAM metric used by unrelated diagnostics is not a failure if it already existed and is not consulted for admission; inspect any such match instead of deleting unrelated observability.

- [ ] **Step 6: Release build and final diff audit**

Run:

```bash
git diff --check
git status --short
git log --oneline -n 10
cargo build --release --locked
```

Expected: release build exits 0. Pre-existing local `.gitignore` and `.omo/` remain untouched.

- [ ] **Step 7: Commit final HTTP regression if it was not included earlier**

Before commit:

```bash
git log --oneline -n 5
```

Then:

```bash
git add src/server.rs
git commit -m "test: verify unbounded concurrent sessions"
```

Skip this commit only if `src/server.rs` was already included in Task 1 with the final 25-session test and there is no remaining diff.

- [ ] **Step 8: Whole-branch review**

Use `superpowers:requesting-code-review` if an independent reviewer is available. Otherwise perform a fresh whole-branch self-review against the spec, focusing on:

1. any surviving hidden concurrency admission path;
2. loss of cancellation/shutdown ownership after process-permit removal;
3. accidental removal of output/retention/timeout safety bounds;
4. request timeout/disconnect behavior after removing permits;
5. any CPU/RAM/load-based decision logic introduced contrary to the spec.

Critical/Important findings receive a RED -> GREEN fix and full-suite rerun.

- [ ] **Step 9: Finish and merge**

Use `superpowers:finishing-a-development-branch` after fresh verification. The user already requested autonomous native execution, so after the normal merge side-effect gate is satisfied:

1. merge the implementation branch into current `main`;
2. rerun `cargo test --locked` on merged `main`;
3. rerun `cargo build --release --locked` on merged `main` so `/home/morts/dev/CatDesk/target/release/catdesk` is current;
4. close `catdesk-vmi.2` only after those commands succeed;
5. do not merge `feat/fork-audit-port-20260921` because Windows native computer-use remains explicitly deferred;
6. do not push unless separately authorized.
