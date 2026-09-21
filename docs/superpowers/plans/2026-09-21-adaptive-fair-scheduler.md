# Adaptive Fair Scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace CatDesk's temporary-saturation `Busy` failures and eight-job admission cap with fair queued admission plus host-safe adaptive physical execution for 10–25 concurrent sessions.

**Architecture:** Add a reusable fair permit queue keyed by session and project. Route request workers and command-process execution through it, persist background jobs in a new `Queued` state before any side effect can start, and let a lightweight host governor adjust only physical concurrency. A process-wide request scheduler remains global, while the existing `CommandJobManager` keeps durable command ownership. Control traffic remains isolated from heavy queues.

**Tech Stack:** Rust 2024, Tokio, Axum, serde/serde_json, existing CatDesk diagnostics/job-store/process-runner modules; no new heavyweight runtime dependency.

**Spec:** `docs/superpowers/specs/2026-09-21-adaptive-fair-scheduler-design.md`

## Global Constraints

- Normal temporary saturation must queue rather than return fail-fast `Busy`.
- No normal hard quota per user, session, or project; fairness changes ordering, not admission.
- A high global safety ceiling may reject work only as an OOM/circuit-breaker protection.
- Use one shared heavy-request queue budget of `8192` waiters across filesystem/process/browser/general classes; control has its own reserved queue budget of `512` waiters.
- Command-job admission has a separate global emergency ceiling of `4096` live nonterminal jobs; this is a memory safety circuit breaker, not a normal throughput control.
- Control-plane operations (`ping`, health, `poll_command`, `cancel_command`, discovery/list/read control calls) must remain responsive under heavy queues.
- Queued background jobs are durable and safe to resume after restart because they have not executed side effects.
- Running background jobs recovered after restart remain `Interrupted` and are never auto-replayed.
- Command runtime timeout begins when physical execution starts, not while the job is queued.
- Caller deadline/disconnect before a synchronous request starts must prevent later surprise execution.
- Existing process-tree termination, idempotency, output bounds, symlink hardening, telemetry persistence, and multi-project isolation must remain intact.
- Windows native computer-use/UIA and stale-action hardening remain out of scope.
- Do not perform the broad `mcp.rs` / `main.rs` refactor from `catdesk-vmi.6` in this plan.

## Review Focus

- Cancellation race between queue grant and task spawn: a timed-out/disconnected request must never execute later.
- Recovery race for persisted `Queued` versus `Running`: only provably unstarted jobs may auto-resume.
- Fairness under one noisy session/project with hundreds of waiters: another session/project must still make progress.
- Dynamic capacity decrease while permits are active: running work must keep its permit and no permit accounting may go negative.
- Global queue safety ceiling under burst load: memory stays bounded and the exceptional overload path is observable rather than silent.

---

### Task 1: Reusable Fair Permit Queue

**Files:**
- Create: `src/fair_queue.rs`
- Modify: `src/main.rs:1-30`
- Test: inline unit tests in `src/fair_queue.rs`

**Interfaces:**
- Consumes: Tokio one-shot channels/timers and standard synchronization primitives.
- Produces:
  - `pub(crate) struct SchedulingKey { pub session: String, pub project: Option<String> }`
  - `pub(crate) struct QueueBudget`
  - `pub(crate) struct FairGate`
  - `pub(crate) struct FairPermit`
  - `pub(crate) enum FairAcquireError { Deadline, Overloaded, Closed }`
  - `pub(crate) struct FairGateSnapshot { queued, active, limit, rejected_overload, cancelled_before_start, completed_waits, total_wait_ms }`
  - `QueueBudget::new(max_queued: usize) -> Arc<QueueBudget>`
  - `FairGate::new(limit: usize, budget: Arc<QueueBudget>) -> Self`
  - `FairGate::acquire(&self, key: SchedulingKey, deadline: std::time::Instant) -> Result<FairPermit, FairAcquireError>`
  - `FairGate::acquire_unbounded(&self, key: SchedulingKey) -> Result<FairPermit, FairAcquireError>` for durable jobs whose lifecycle is controlled by cancellation/abandonment rather than an HTTP deadline
  - `FairGate::set_limit(&self, limit: usize)`
  - `FairGate::snapshot(&self) -> FairGateSnapshot`
  - `FairGate::close(&self)`

- [ ] **Step 1: Write failing fairness, cancellation, and budget tests**

Add tests that pin session-first/project-aware rotation, eager timeout cancellation, shared queue-budget enforcement, and shrink semantics. Use deterministic release channels; bounded sleeps are watchdogs only.

```rust
#[tokio::test]
async fn saturated_gate_queues_instead_of_failing_busy() {
    let budget = QueueBudget::new(32);
    let gate = FairGate::new(1, budget);
    let held = gate
        .acquire(SchedulingKey::new("a", Some("p")), deadline())
        .await
        .unwrap();

    let queued_gate = gate.clone();
    let queued = tokio::spawn(async move {
        queued_gate
            .acquire(SchedulingKey::new("b", Some("q")), deadline())
            .await
    });

    wait_until(|| gate.snapshot().queued == 1).await;
    drop(held);
    assert!(queued.await.unwrap().is_ok());
}

#[tokio::test]
async fn noisy_session_cannot_starve_another_session() {
    // Hold capacity; enqueue session-a/project-1 three times, then session-b/project-9 once.
    // Release one permit at a time and assert session-b is granted before session-a fully drains.
}

#[tokio::test]
async fn projects_rotate_within_one_session() {
    // Hold capacity; enqueue a/p1, a/p1, a/p2.
    // Assert p2 is granted before both p1 requests drain.
}

#[tokio::test]
async fn deadline_removes_waiter_and_returns_budget_immediately() {
    let budget = QueueBudget::new(1);
    let gate = FairGate::new(1, budget.clone());
    let held = gate.acquire(SchedulingKey::anonymous(), deadline()).await.unwrap();
    let result = gate
        .acquire(
            SchedulingKey::new("late", None),
            Instant::now() + Duration::from_millis(20),
        )
        .await;
    assert_eq!(result, Err(FairAcquireError::Deadline));
    assert_eq!(gate.snapshot().queued, 0);
    assert_eq!(budget.queued(), 0);
    drop(held);
}

#[tokio::test]
async fn shared_budget_is_global_across_gates() {
    let budget = QueueBudget::new(1);
    let a = FairGate::new(0, budget.clone());
    let b = FairGate::new(0, budget.clone());
    let first = tokio::spawn({
        let a = a.clone();
        async move { a.acquire(SchedulingKey::new("a", None), deadline()).await }
    });
    wait_until(|| a.snapshot().queued == 1).await;
    assert_eq!(
        b.acquire(SchedulingKey::new("b", None), deadline()).await,
        Err(FairAcquireError::Overloaded)
    );
    first.abort();
}

#[tokio::test]
async fn close_wakes_all_waiters_without_granting_work() {
    // Hold capacity, enqueue two waiters, call close(), assert both receive Closed,
    // queued budget returns to zero, and neither receives a permit later.
}

#[tokio::test]
async fn lower_limit_waits_for_active_permits_to_drain_without_revocation() {
    let gate = FairGate::new(2, QueueBudget::new(32));
    let a = gate.acquire(SchedulingKey::new("a", None), deadline()).await.unwrap();
    let b = gate.acquire(SchedulingKey::new("b", None), deadline()).await.unwrap();
    gate.set_limit(1);
    assert_eq!(gate.snapshot().active, 2);
    drop(a);
    assert_eq!(gate.snapshot().active, 1);
    drop(b);
    assert_eq!(gate.snapshot().active, 0);
}
```

- [ ] **Step 2: Run the new tests and verify RED**

Run:

```bash
cargo test --locked fair_queue::tests -- --nocapture
```

Expected: compile/test failure because `fair_queue` and its interfaces do not exist yet.

- [ ] **Step 3: Implement the fair queue**

Use `std::sync::Mutex` for the tiny queue state so `FairPermit::drop()` can immediately return capacity without `await`. Queue order stores waiter IDs, while waiter payloads live in a map; this lets deadline/drop cancellation remove the live waiter and release `QueueBudget` immediately even if stale IDs remain in rotation deques.

```rust
struct Waiter {
    id: u64,
    key: SchedulingKey,
    enqueued_at: Instant,
    grant: oneshot::Sender<FairPermitToken>,
    _budget: QueueReservation,
}

struct SessionQueue {
    project_order: VecDeque<Option<String>>,
    projects: HashMap<Option<String>, VecDeque<u64>>,
}

struct GateState {
    limit: usize,
    active: usize,
    waiters: HashMap<u64, Waiter>,
    session_order: VecDeque<String>,
    sessions: HashMap<String, SessionQueue>,
    closed: bool,
    next_waiter_id: u64,
    stats: GateStats,
}
```

`acquire()` flow:

1. If capacity is immediately available and no older waiter exists, grant directly.
2. Otherwise reserve one slot from shared `QueueBudget`; if reservation fails return `Overloaded`.
3. Insert waiter ID into session/project rotation and waiter map.
4. Await `oneshot` until absolute deadline.
5. If timeout or caller future is dropped before grant, a `WaitRegistration` drop guard removes the waiter map entry and releases its queue-budget reservation.
6. `dispatch()` skips stale IDs, rotates sessions and projects, and grants while `active < limit`.
7. If grant send fails because receiver vanished, decrement `active` and continue dispatch.

`FairPermit::drop()` decrements `active` exactly once and dispatches the next waiter. `set_limit()` may set `0` for tests/critical pressure; it never revokes active permits.

- [ ] **Step 4: Run fair queue tests GREEN**

Run:

```bash
cargo test --locked fair_queue::tests -- --nocapture
```

Expected: all fair queue tests pass.

- [ ] **Step 5: Run checks and commit**

```bash
git diff --check
git status --short
git log --oneline -n 5
git add src/fair_queue.rs src/main.rs
git commit -m "feat: add fair queued capacity gate"
```

---

### Task 2: Queue Request Workers with Session/Project Fairness

**Files:**
- Modify: `src/request_workers.rs`
- Modify: `src/server.rs`
- Test: inline tests in both files

**Interfaces:**
- Consumes: `SchedulingKey`, `QueueBudget`, `FairGate`, `FairAcquireError`, `FairGateSnapshot` from Task 1.
- Produces:
  - `RequestScheduler::run(class, key, work, deadline)` instead of `run(class, work, deadline)`.
  - `RequestSchedulerSnapshot` containing per-class queue/active/limit data.
  - `global_request_scheduler() -> &'static RequestScheduler` in `request_workers.rs`.
  - `request_scheduling_key(session, gate) -> SchedulingKey` helper in `server.rs`.

- [ ] **Step 1: Replace fail-fast behavior tests with queued behavior tests**

Delete/replace expectations that saturation returns `RequestFailure::Busy`. Add tests for queueing, deadline-before-start, disconnect-before-start, control isolation, shared heavy queue budget, and session fairness.

```rust
#[tokio::test]
async fn saturated_filesystem_work_waits_and_control_stays_responsive() {
    let scheduler = RequestScheduler::with_limits_for_test(RequestLimits {
        control: 1,
        filesystem: 1,
        process: 1,
        browser: 1,
        general: 1,
    });
    let release = Arc::new(Notify::new());

    let blocker = spawn_blocking_request(
        &scheduler,
        RequestClass::Filesystem,
        key("session-a"),
        release.clone(),
    );
    wait_until_active(&scheduler, RequestClass::Filesystem, 1).await;

    let queued = tokio::spawn({
        let scheduler = scheduler.clone();
        async move {
            scheduler
                .run(
                    RequestClass::Filesystem,
                    key("session-b"),
                    async { 8u8 },
                    Duration::from_secs(2),
                )
                .await
        }
    });

    assert_eq!(
        scheduler
            .run(
                RequestClass::Control,
                key("session-c"),
                async { 42u8 },
                Duration::from_secs(1),
            )
            .await,
        Ok(42)
    );
    release.notify_waiters();
    assert_eq!(queued.await.unwrap(), Ok(8));
    assert!(blocker.await.unwrap().is_ok());
}

#[tokio::test]
async fn queued_request_that_times_out_never_runs_later() {
    // Hold the only filesystem permit.
    // Enqueue work that flips AtomicBool with a short deadline.
    // Assert Deadline, release the blocker, yield until queue drains, assert AtomicBool=false.
}

#[tokio::test]
async fn aborted_caller_before_grant_never_runs_later() {
    // Same shape as timeout, but abort the task waiting in RequestScheduler::run.
    // Release blocker and prove side effect never executes.
}
```

Add server identity coverage:

```rust
#[test]
fn scheduling_key_uses_named_session_and_active_project() {
    let session = ClientSession::from_headers(&headers_for("session-a"));
    gate.set_active_project(&session, PathBuf::from("/workspace/project-a"));
    let key = request_scheduling_key(&session, &gate);
    assert_eq!(key.session, "session-a");
    assert_eq!(key.project.as_deref(), Some("/workspace/project-a"));
}
```

- [ ] **Step 2: Run targeted tests RED**

```bash
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked server::tests::scheduling_key_uses_named_session_and_active_project -- --exact
```

Expected: failures because current worker pools still use `try_acquire_owned()` and scheduler identity is not passed.

- [ ] **Step 3: Replace request semaphore admission with `FairGate`**

`RequestScheduler::new()` creates:

- one control gate with its own `QueueBudget::new(512)`;
- one shared `QueueBudget::new(8192)` used by filesystem/process/browser/general gates.

Keep the existing `spawn_blocking` safety rule: once synchronous work starts, the permit lives inside the blocking task until that work truly finishes.

```rust
pub(crate) async fn run<F>(
    &self,
    key: SchedulingKey,
    work: F,
    deadline: Duration,
) -> Result<F::Output, RequestFailure>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let absolute_deadline = Instant::now() + deadline;
    let permit = self
        .gate
        .acquire(key, absolute_deadline)
        .await
        .map_err(map_acquire_error)?;
    let remaining = absolute_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        drop(permit);
        return Err(RequestFailure::Deadline);
    }

    let runtime = Handle::current();
    let task = spawn_blocking(move || {
        let _permit = permit;
        runtime.block_on(work)
    });
    match timeout(remaining, task).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(RequestFailure::Failed),
        Err(_) => Err(RequestFailure::Deadline),
    }
}
```

Keep `RequestFailure::Busy` only as the exceptional `FairAcquireError::Overloaded` mapping. Change its text to say the scheduler safety queue is full, not that all workers are temporarily occupied.

- [ ] **Step 4: Pass scheduling identity from `post_mcp_http` without moving the process-wide scheduler into `ServerState`**

Keep the request scheduler process-global because its capacity and fairness are host-global. Replace the function-local static with `request_workers::global_request_scheduler()` so later tasks can snapshot/update it without changing every `ServerState` test literal.

```rust
let client_session = ClientSession::from_headers(&headers);
let active_project = s.catdesk_instruction_called.active_project(&client_session);
let scheduling_key = request_scheduling_key(&client_session, &s.catdesk_instruction_called);

request_workers::global_request_scheduler()
    .run(
        class,
        scheduling_key,
        async move { post_mcp_inner(State(s), body_bytes, &headers, None).await },
        request_deadline(class),
    )
    .await
```

`request_scheduling_key()` uses `client_session.namespace()` when named, otherwise `flow_id`; project is the current validated/stored active project string if present. The first request that selects a project may be scheduled with `project=None`; later requests use the selected project, which is acceptable because fairness is advisory ordering rather than authorization.

- [ ] **Step 5: Run targeted and server concurrency tests GREEN**

```bash
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked server::tests::request_classification_keeps_control_plane_separate_from_heavy_work -- --exact
cargo test --locked server::tests::parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated -- --exact
```

Expected: all pass; no ordinary saturation path returns `Busy`.

- [ ] **Step 6: Commit**

```bash
git diff --check
git status --short
git log --oneline -n 5
git add src/request_workers.rs src/server.rs
git commit -m "feat: queue requests with session fairness"
```

---

### Task 3: Durable `Queued` Command Jobs and Awaited Process Capacity

**Files:**
- Modify: `src/command_jobs.rs`
- Modify: `src/job_store.rs`
- Modify: `src/mcp.rs`
- Test: inline unit/integration tests in those files

**Interfaces:**
- Consumes: `SchedulingKey`, `QueueBudget`, and `FairGate` from Task 1.
- Produces:
  - `CommandJobState::Queued`
  - `CommandJobSnapshot.queue_wait_ms: u64`
  - `CommandJobManager::acquire_process(key, deadline)` for foreground `run_command`
  - `CommandJobManager::set_process_limit(limit)` for Task 4
  - background start that persists/returns a job promptly, then waits for process capacity asynchronously.

- [ ] **Step 1: Write RED tests for >8 accepted jobs, queued cancellation, timeout semantics, and recovery**

Make the queued-state fixture deterministic by occupying the only physical process permit before creating background jobs.

```rust
#[tokio::test]
async fn more_than_eight_background_jobs_are_accepted_and_stay_queued_when_capacity_is_held() {
    let root = workspace("queued-capacity");
    let manager = CommandJobManager::new_with_process_limit(1);
    let held = manager
        .acquire_process_for_test(SchedulingKey::new("holder", None))
        .await;

    let mut snapshots = Vec::new();
    for index in 0..16 {
        let started = manager
            .start_with_change_session(
                long_command(),
                root.clone(),
                root.clone(),
                10_000,
                Some(format!("request-{index}")),
                None,
                Some("session-a"),
            )
            .await
            .unwrap();
        snapshots.push(started.snapshot);
    }

    assert!(snapshots.iter().all(|s| s.state == CommandJobState::Queued));
    assert_eq!(manager.queued_job_count().await, 16);
    drop(held);
    manager.cancel_all().await;
}

#[tokio::test]
async fn cancelling_queued_job_guarantees_no_process_side_effect() {
    // Occupy process capacity with a test permit.
    // Start queued command that would create sentinel.
    // Cancel while Queued, release held permit, wait for scheduler drain, prove sentinel absent.
}

#[tokio::test]
async fn queued_job_timeout_starts_when_process_starts_not_when_enqueued() {
    // Hold process capacity longer than the command's runtime timeout.
    // Release it, wait for Running, then assert timeout occurs relative to execution start.
}

#[tokio::test]
async fn queued_job_recovery_reschedules_unstarted_work() {
    // Write a schema-v2 Queued JobRecord directly into a temp JobStore so no old runner exists.
    // Construct a new manager on that same store and trigger recovery.
    // Assert the job is restored Queued and eventually starts once capacity is available.
}

#[tokio::test]
async fn running_job_recovery_still_becomes_interrupted_without_replay() {
    // Existing recovery invariant updated to schema v2.
}

#[tokio::test]
async fn duplicate_request_key_reuses_the_same_queued_job() {
    // Hold physical capacity so the first job stays Queued.
    // Call start twice with the same request key and identical arguments.
    // Assert second.deduplicated=true, same job id, and queued_job_count()==1.
}
```

Also test global emergency admission ceiling separately with a tiny test-only ceiling override; do not allocate 4096 real jobs in a unit test.

- [ ] **Step 2: Run command-job tests RED**

```bash
cargo test --locked command_jobs::tests -- --nocapture
```

Expected: missing `Queued` variant / ninth+ jobs still rejected / process acquisition still fail-fast.

- [ ] **Step 3: Evolve durable record schema compatibly**

Bump new writes to schema version `2`, but parse versions `1` and `2`. Add serde-defaulted fields:

```rust
pub struct JobRecord {
    pub schema_version: u32,
    // existing fields...
    pub started_at_ms: u64, // acceptance time retained for compatibility
    #[serde(default)]
    pub execution_started_at_ms: Option<u64>,
    #[serde(default)]
    pub queue_wait_ms: Option<u64>,
}
```

`JobStore::parse()`:

```rust
match record.schema_version {
    1 | 2 => Some(record),
    _ => None,
}
```

Schema-v1 `Running` recovery remains `Interrupted`. Schema-v1 has no queued state.

- [ ] **Step 4: Add `Queued` lifecycle and remove `MAX_ACTIVE_JOBS` as a normal admission limit**

Use a shared command-process gate with `QueueBudget::new(4096)`. New jobs are inserted/persisted as `Queued` before their runner is spawned. Add a `MAX_LIVE_NONTERMINAL_JOBS = 4096` emergency circuit breaker checked only to bound job metadata memory.

`CommandJobState::is_terminal()` is false for `Queued` and `Running`. `active_job_count()` counts only `Running`; add `queued_job_count()`.

The runner flow is:

```rust
async fn run_job(
    job: Arc<CommandJob>,
    mut cancel_rx: watch::Receiver<bool>,
    process_gate: FairGate,
    key: SchedulingKey,
) {
    let acquire = process_gate.acquire_unbounded(key);
    tokio::pin!(acquire);
    let permit = loop {
        let idle_deadline = job.current_abandon_deadline();
        tokio::select! {
            _ = cancel_rx.changed() => {
                job.finish(CommandJobState::Cancelled, Some(EXIT_CODE_CANCELLED)).await;
                return;
            }
            permit = &mut acquire => match permit {
                Ok(permit) => break permit,
                Err(FairAcquireError::Closed) => {
                    job.finish(CommandJobState::Interrupted, None).await;
                    return;
                }
                Err(FairAcquireError::Overloaded | FairAcquireError::Deadline) => {
                    job.finish(CommandJobState::Failed, Some(EXIT_CODE_INTERNAL_ERROR)).await;
                    return;
                }
            },
            _ = tokio::time::sleep_until(idle_deadline) => {
                if job.is_abandoned_now() {
                    job.finish(CommandJobState::Abandoned, Some(EXIT_CODE_ABANDONED)).await;
                    return;
                }
            }
        }
    };

    if *cancel_rx.borrow() {
        drop(permit);
        job.finish(CommandJobState::Cancelled, Some(EXIT_CODE_CANCELLED)).await;
        return;
    }
    job.mark_running().await; // persists execution_started_at_ms + queue_wait_ms + Running
    // Existing spawn/output/runtime-timeout/termination code starts here.
}
```

For durable background jobs, queue waiting is unbounded by request/runtime timeout. The existing poll-heartbeat abandonment rule still applies while queued: regular polling keeps the queued job alive, while an owner that stops polling for `DEFAULT_ABANDON_AFTER_MS` lets CatDesk cancel the queue wait and persist `Abandoned`. The job's own `timeout_ms` begins only after `mark_running()`.

Recovery behavior:

- `Queued` record -> restore job, insert it, spawn a new queue-waiting runner because side effects provably never began.
- `Running` record -> restore as `Interrupted`; do not spawn.
- terminal -> restore unchanged.

Cancellation/session-disconnect/cancel-all must signal both `Queued` and `Running`. A queued cancellation becomes terminal without spawning a shell.

- [ ] **Step 5: Make foreground `run_command` await the same process gate**

Thread `session_namespace` into `handle_run_command` and derive a key from session + resolved cwd/project.

Replace:

```rust
let _process_permit = command_jobs.try_acquire_process()?;
```

with:

```rust
let _process_permit = command_jobs
    .acquire_process(
        SchedulingKey::new(
            session_namespace.unwrap_or("stateless"),
            Some(cwd.to_string_lossy().into_owned()),
        ),
        Instant::now() + effective_timeout,
    )
    .await
    .map_err(...)?;
```

If the caller's request deadline expires before permit grant, `RequestScheduler` drops the outer future; `FairGate` registration cancellation must ensure the command body never starts later.

- [ ] **Step 6: Update MCP queued rendering**

Queued structured output uses `commandSuccess: null`, `state: "queued"`, `queueWaitMs`, and explanatory text.

```rust
match snapshot.state {
    CommandJobState::Queued => "(command queued; waiting for safe process capacity)".to_string(),
    CommandJobState::Running => "(no new output; command is still running)".to_string(),
    // existing terminal text...
}
```

- [ ] **Step 7: Run command/MCP durability tests GREEN**

```bash
cargo test --locked command_jobs::tests -- --nocapture
cargo test --locked job_store::tests -- --nocapture
cargo test --locked mcp::tests::run_command_shares_process_budget_with_background_jobs -- --exact
cargo test --locked server::tests::background_command_survives_separate_stateless_http_requests -- --exact
```

Expected: all pass; more than eight jobs accepted; excess jobs queued rather than rejected/spawned.

- [ ] **Step 8: Commit**

```bash
git diff --check
git status --short
git log --oneline -n 5
git add src/command_jobs.rs src/job_store.rs src/mcp.rs
git commit -m "feat: persist queued command jobs"
```

---

### Task 4: Adaptive Host Resource Governor

**Files:**
- Create: `src/resource_governor.rs`
- Modify: `src/main.rs`
- Modify: `src/request_workers.rs`
- Modify: `src/command_jobs.rs`
- Modify: `src/server.rs`
- Test: inline tests in `src/resource_governor.rs` plus scheduler/job integration tests

**Interfaces:**
- Consumes: `FairGate::set_limit()` and scheduler/process-gate snapshots.
- Produces:
  - `HostPressureSample { logical_cpus, total_memory_bytes, available_memory_bytes, process_rss_bytes, load_ratio }`
  - `CapacityTargets { filesystem, process_requests, browser, general, command_processes }`
  - `ResourceGovernor::new() -> Self`
  - `ResourceGovernor::targets() -> CapacityTargets`
  - `ResourceGovernor::refresh_if_due() -> CapacityTargets`
  - test-only deterministic `ResourceGovernor::with_sampler(...)`.

- [ ] **Step 1: Write RED policy tests with fake samples**

```rust
#[test]
fn healthy_host_increases_capacity_only_after_three_healthy_samples() {
    let mut policy = AdaptivePolicy::new(base_targets());
    let healthy = sample_with(0.50, 0.60); // load ratio, available memory ratio
    let a = policy.observe(&healthy);
    let b = policy.observe(&healthy);
    let c = policy.observe(&healthy);
    assert_eq!(a, b);
    assert!(c.command_processes >= b.command_processes);
}

#[test]
fn critical_memory_pressure_steps_capacity_down_immediately() {
    let mut policy = AdaptivePolicy::new(high_targets());
    let pressured = sample_with(0.80, 0.05);
    let target = policy.observe(&pressured);
    assert!(target.command_processes < high_targets().command_processes);
    assert!(target.command_processes >= 1);
}

#[tokio::test]
async fn lowering_governor_target_does_not_cancel_running_permits() {
    // Acquire N process permits, lower gate target below N, prove active=N survives;
    // a new waiter starts only after enough active permits drain.
}
```

- [ ] **Step 2: Run governor tests RED**

```bash
cargo test --locked resource_governor::tests -- --nocapture
```

Expected: module/types missing.

- [ ] **Step 3: Implement lightweight sampler and deterministic policy**

Sampling interval: `1s` minimum between OS reads. Never sample while holding `AppState` or command-job runtime locks.

Linux sampler reads:

- `/proc/meminfo`: `MemTotal`, `MemAvailable`;
- `/proc/self/status`: `VmRSS`;
- `/proc/loadavg`: one-minute load average divided by logical CPU count.

Non-Linux fallback uses `std::thread::available_parallelism()` and returns `None` for memory/load fields; policy then keeps conservative base targets rather than pretending the host is healthy. This plan does not add a third-party system-metrics crate.

Target bounds for `cpu = logical_cpus.max(1)`:

```rust
minimum = CapacityTargets {
    filesystem: 1,
    process_requests: 1,
    browser: 1,
    general: 1,
    command_processes: 1,
};

base = CapacityTargets {
    filesystem: cpu.clamp(2, 16),
    process_requests: (cpu / 2).clamp(1, 12),
    browser: (cpu / 8).clamp(1, 2),
    general: (cpu / 2).clamp(1, 8),
    command_processes: (cpu / 2).clamp(1, 12),
};

maximum = CapacityTargets {
    filesystem: (cpu * 2).clamp(2, 32),
    process_requests: cpu.clamp(1, 24),
    browser: (cpu / 4).clamp(1, 6),
    general: cpu.clamp(1, 16),
    command_processes: cpu.clamp(1, 24),
};
```

Pressure policy:

- **critical** if available-memory ratio `< 0.08` or load ratio `>= 1.50`: step each heavy target down immediately by `max(1, current/4)` toward minimum;
- **healthy** if available-memory ratio `>= 0.20` and load ratio `<= 0.75`: after three consecutive healthy samples, step each target up by `1` toward maximum;
- otherwise hold target and reset healthy streak;
- if memory/load data are unavailable, hold the exact `base` targets defined above;
- never revoke running permits when target decreases.

- [ ] **Step 4: Wire governor to global request scheduler and command process gate**

Use a process-global `ResourceGovernor` accessor. At the top of non-ping MCP handling, call a cheap `refresh_if_due()` (which returns cached targets within the 1s interval), then:

```rust
let targets = resource_governor::global_resource_governor().refresh_if_due();
request_workers::global_request_scheduler().apply_targets(targets);
s.command_jobs.set_process_limit(targets.command_processes);
```

Control gate capacity is not reduced by heavy-pressure targets. `apply_targets()` changes only filesystem/process/browser/general physical limits.

- [ ] **Step 5: Run governor + saturation tests GREEN**

```bash
cargo test --locked resource_governor::tests -- --nocapture
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked command_jobs::tests::shared_process_budget_is_held_by_background_jobs_until_termination -- --exact
```

Expected: all pass and active permits survive target decreases.

- [ ] **Step 6: Commit**

```bash
git diff --check
git status --short
git log --oneline -n 5
git add src/resource_governor.rs src/main.rs src/request_workers.rs src/command_jobs.rs src/server.rs
git commit -m "feat: adapt execution capacity to host pressure"
```

---

### Task 5: Scheduler Telemetry and 1/5/10/20/25 Session Verification

**Files:**
- Modify: `src/request_workers.rs`
- Modify: `src/command_jobs.rs`
- Modify: `src/diagnostics.rs` and/or `src/server.rs` for bounded event emission
- Test: scheduler stats tests plus one ignored synthetic load test in `src/server.rs`

**Interfaces:**
- Consumes: snapshots from fair request gates, command job queue, and resource governor.
- Produces:
  - `RequestScheduler::snapshot()` with queued/active/limit/wait counters per class.
  - `CommandJobManager::scheduler_snapshot()` with queued/running/process-limit counters.
  - diagnostic counters/events for overload, cancellation-before-start, queue wait, and target changes.

- [ ] **Step 1: Write RED consistency tests for metrics**

```rust
#[tokio::test]
async fn scheduler_snapshot_tracks_queue_and_wait_completion_consistently() {
    // Hold one slot, enqueue two, verify queued=2 active=1.
    // Release/drain, verify queued=0 active=0 completed_waits>=2.
}

#[tokio::test]
async fn command_scheduler_snapshot_distinguishes_queued_and_running_jobs() {
    // Hold/limit physical process capacity, start three jobs, assert running/queued split.
}
```

- [ ] **Step 2: Add ignored 1/5/10/20/25-session load probe before implementation**

Add:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "synthetic scheduler load probe"]
async fn scheduler_load_probe_1_5_10_20_25_sessions() {
    for sessions in [1usize, 5, 10, 20, 25] {
        let result = run_scheduler_load_scenario(sessions).await;
        println!("{result:?}");
        assert_eq!(result.overload_rejections, 0);
        assert_eq!(result.completed_sessions, sessions);
        assert!(result.control_probe_ok);
        assert!(result.peak_active_processes <= result.peak_process_target);
    }
}
```

`run_scheduler_load_scenario()` uses named sessions, repeated JSON-RPC ids, and a mix of control, file read/search, and short command jobs. It captures:

- completed session count;
- overload rejections;
- p50/p95 queue wait from scheduler wait counters/timings;
- peak queued/active request counts;
- peak queued/running command counts;
- current/peak governor targets;
- CatDesk RSS from `ResourceGovernor` sample where Linux provides it;
- active process count;
- control probe latency/success.

Do not assert machine-specific latency or RSS thresholds; print them for audit. Safety assertions are only completion, zero normal overload, responsive control, and active physical work not exceeding target.

- [ ] **Step 3: Run telemetry/load tests RED**

```bash
cargo test --locked request_workers::tests::scheduler_snapshot_tracks_queue_and_wait_completion_consistently -- --exact
cargo test --locked command_jobs::tests::command_scheduler_snapshot_distinguishes_queued_and_running_jobs -- --exact
cargo test --locked server::tests::scheduler_load_probe_1_5_10_20_25_sessions -- --ignored --exact --nocapture
```

Expected: missing snapshot/counter/load-harness interfaces.

- [ ] **Step 4: Implement telemetry snapshots and bounded diagnostics**

Do not build the final dashboard UI here. Expose internal counters and emit bounded diagnostics only for state transitions worth debugging:

```text
scheduler_request_queued
scheduler_request_cancelled_before_start
scheduler_overload_rejected
scheduler_capacity_changed
command_job_queued
command_job_started
```

Do not emit per-poll events. Queue-wait totals/counts live in snapshots so `catdesk-vmi.4` can aggregate them later.

- [ ] **Step 5: Run targeted tests, full suite, and ignored load probe**

```bash
cargo test --locked request_workers::tests -- --nocapture
cargo test --locked command_jobs::tests -- --nocapture
cargo test --locked server::tests -- --nocapture
cargo test --locked
cargo test --locked server::tests::scheduler_load_probe_1_5_10_20_25_sessions -- --ignored --exact --nocapture
```

Expected:
- all ordinary Rust tests pass;
- ignored load probe succeeds for 1/5/10/20/25 sessions;
- zero normal overload rejections in those scenarios;
- control probe succeeds under the 25-session scenario;
- physical active counts never exceed current target.

- [ ] **Step 6: Release build and final diff audit**

```bash
git diff --check
git status --short
cargo build --release --locked
git log --oneline -n 10
```

Expected: release build exit 0; only intentional branch changes present; pre-existing main `.gitignore` / `.omo/` are untouched.

- [ ] **Step 7: Commit final telemetry/load harness**

```bash
git add src/request_workers.rs src/command_jobs.rs src/diagnostics.rs src/server.rs
git commit -m "test: verify scheduler under 25 sessions"
```

- [ ] **Step 8: Whole-branch review and finish**

Use `superpowers:requesting-code-review` for a whole-branch review if a reviewer/subagent is available; otherwise record self-review in the SDD ledger as required by `executing-plans`. Grade findings before changes. Critical/Important findings get one RED→GREEN fix pass plus full-suite rerun; Minor findings go to the ledger.

Then use `superpowers:finishing-a-development-branch`. Do not merge/push until the normal side-effect gate is satisfied. When merge is permitted, merge to current `main`, rerun `cargo test --locked` and `cargo build --release --locked` on merged `main`, then close `catdesk-vmi.2`.
