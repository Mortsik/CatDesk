# CatDesk unbounded concurrency design

Date: 2026-09-21
Issue: `catdesk-vmi.2`
Status: proposed / user-approved direction, pending written-spec review

## Intent

CatDesk should not impose artificial concurrency limits on agent/chat work.

The user explicitly does not want CPU, RAM, RSS, load average, queue depth, browser occupancy, or any other host-pressure signal to reduce CatDesk concurrency. CatDesk should not throttle, queue, reject, or delay work because it believes the machine is busy.

The target behavior is:

- no per-user, per-session, per-project, per-tool-class, per-process, or per-background-job concurrency quota;
- no CPU/RAM/load-based governor;
- no adaptive physical concurrency;
- no fail-fast `Busy` caused by CatDesk worker/process semaphores;
- `start_command` accepts and starts new jobs without an active-job cap;
- `run_command` starts without a shared process permit;
- request work is allowed to start immediately rather than waiting in a CatDesk fairness queue;
- the OS/runtime is the only practical resource-pressure boundary;
- existing command cancellation, command timeout, process-tree termination, persistence, dedupe/idempotency, output bounds, path safety, and session/project isolation remain intact.

Windows native computer-use/UIA remains out of scope.

## Explicit non-goal: resource-aware throttling

This task must not introduce or consult:

- CPU utilization;
- logical CPU count for concurrency decisions;
- system available memory;
- CatDesk RSS;
- load average;
- process-count thresholds;
- browser occupancy thresholds;
- queue depth thresholds used to reduce execution concurrency;
- dynamic `set_limit()` behavior;
- host-pressure sampling or a `ResourceGovernor`.

CatDesk may continue to expose unrelated diagnostics if they already exist, but those values must not influence admission or concurrency in this design.

## Current state

There are two independent artificial saturation mechanisms today.

### Request workers

`src/request_workers.rs` uses fixed Tokio semaphores for request classes. A request calls `try_acquire_owned()` and returns `RequestFailure::Busy` when the class pool is full.

Current class limits are effectively:

- control: 8;
- filesystem: 16;
- process: 12;
- browser: 4;
- general: 8.

These limits are to be removed as execution gates.

### Command jobs

`src/command_jobs.rs` currently has:

- `MAX_ACTIVE_JOBS = 8`;
- `MAX_ACTIVE_PROCESSES = 12`;
- `process_budget: Semaphore`;
- `try_acquire_process()`.

`start_command` rejects additional work when either the active-job cap or process semaphore is exhausted. Foreground `run_command` shares the same process budget.

These concurrency limits are to be removed.

## Design principles

### 1. Start work immediately

If a request passes normal validation/authorization/path checks, CatDesk should start it without waiting for a CatDesk concurrency slot.

There is no fair queue because fairness is unnecessary when CatDesk is not serializing or throttling competing sessions.

### 2. No concurrency-related `Busy`

Normal request handling must not return `Busy` because another CatDesk request or command is active.

If `RequestFailure::Busy` becomes unused after removing worker semaphores, remove it and its HTTP/diagnostic mappings rather than keeping dead concurrency semantics.

Errors unrelated to concurrency remain valid, including malformed input, path violations, command-policy rejection, tool-specific failures, and request deadlines.

### 3. Preserve request deadlines without reclaiming running work incorrectly

The current important timeout invariant remains:

- the response may time out;
- already-started synchronous/blocking work must continue to own its real execution until it exits;
- CatDesk must not pretend timed-out work stopped if it is still running.

Removing semaphores must not regress this lifecycle behavior.

### 4. Remove command-process admission gates

For background jobs:

1. validate/dedupe exactly as today;
2. create/persist the job using the existing durable lifecycle;
3. spawn the process immediately;
4. return the job id;
5. retain existing poll/cancel/timeout/output handling.

There is no `Queued` state in this design because CatDesk is not deliberately queueing command execution.

For foreground `run_command`, remove the process-permit acquisition and execute directly after normal validation.

### 5. Keep durability conservative across restart

The existing recovery invariant remains:

- a job recorded as `Running` when CatDesk restarts becomes `Interrupted`;
- CatDesk does not auto-replay uncertain work;
- terminal records restore as today.

This task does not introduce durable queued jobs because there is no scheduler queue to persist.

### 6. Keep cancellation semantics

Background command cancellation still terminates the full child process tree.

Session disconnect cancellation behavior remains unchanged except that there is no process permit to release.

### 7. Keep non-concurrency safety bounds

Removing concurrency limits does **not** mean deleting unrelated correctness/safety bounds such as:

- request body size limits;
- command timeout requested by the caller;
- output-buffer size limits;
- retained terminal-job count/TTL;
- path containment and symlink hardening;
- poll response caps;
- command-policy/security checks.

These do not throttle the number of concurrently running agents or processes and therefore remain in scope to preserve.

## Request path architecture

Simplify `src/request_workers.rs` so it no longer owns per-class semaphores.

Two acceptable implementation shapes are:

1. keep `RequestScheduler`/`RequestWorkers` as thin deadline/execution wrappers for minimal call-site churn; or
2. remove the redundant worker wrapper and move the existing timeout/spawn-blocking lifecycle into the server call path.

Prefer the smaller diff that preserves current timeout/disconnect semantics.

`RequestClass` may remain for diagnostics/deadline selection even though it no longer controls concurrency.

`ping`/health fast paths should remain as currently structured. They should not be routed through a new throttling mechanism.

## Command-job architecture

Simplify `CommandJobManager` by removing concurrency-only state:

- `MAX_ACTIVE_JOBS`;
- `MAX_ACTIVE_PROCESSES`;
- `process_budget`;
- `process_limit` where it exists only for admission;
- `try_acquire_process()`;
- test-only constructors whose only purpose is setting a process concurrency limit.

`active_job_count()` remains useful as telemetry/status, but no longer controls admission.

`MAX_RETAINED_JOBS` and terminal TTL remain because they bound completed-history retention, not active concurrency.

Background job start should no longer perform:

```text
if active_count >= MAX_ACTIVE_JOBS -> reject
try_acquire_process() -> reject
```

Instead it proceeds to normal job creation/spawn immediately.

Foreground `run_command` similarly removes `try_acquire_process()`.

## Observability

This task does not add CPU/RAM/load monitoring.

Useful existing counters may remain:

- active request count;
- active background-job count;
- connected chats/sessions;
- tool latency/errors;
- command lifecycle state.

No concurrency target, queue depth, host-pressure state, or adaptive governor telemetry is required because those mechanisms do not exist in this design.

## Testing strategy

Implementation is test-driven.

Required behavioral tests:

1. more requests than every former request-worker limit can be accepted concurrently without `Busy`;
2. request timeout semantics still hold when many requests are active;
3. a timed-out caller does not cause already-started blocking work to be treated as finished early;
4. more than eight background jobs can be accepted without rejection;
5. more than twelve background/foreground command processes can be started without CatDesk process-budget rejection;
6. foreground `run_command` no longer depends on a shared process semaphore;
7. background cancellation still terminates the process tree;
8. session disconnect still cancels owned background jobs as today;
9. idempotency/dedupe still returns the same job for duplicate request keys;
10. restart recovery still maps uncertain `Running` jobs to `Interrupted` without replay;
11. terminal-job retention/output bounds remain unchanged;
12. synthetic 1/5/10/20/25-session verification completes without CatDesk-generated concurrency `Busy` errors.

The synthetic verification must not assert CPU, RAM, RSS, load, active-process ceilings, or adaptive targets. It should only verify functional progress, absence of CatDesk concurrency rejections, control-call availability, and correct lifecycle behavior.

## Expected files/components

Primary:

- `src/request_workers.rs` — remove semaphore admission / `Busy` behavior while preserving deadlines;
- `src/server.rs` — remove `Busy` HTTP/diagnostic mappings if no longer used;
- `src/command_jobs.rs` — remove active-job/process concurrency caps and process semaphore;
- `src/mcp.rs` — remove foreground `run_command` process-permit acquisition and update tests;
- existing server/command-job tests — replace saturation expectations with unbounded-concurrency expectations.

No new scheduler, fair-queue, queue persistence, or resource-governor module should be created.

## Safety invariants

The implementation must preserve:

- no replay of uncertain running commands after restart;
- process-tree termination on cancellation/timeout where currently guaranteed;
- command idempotency/dedupe across retried requests;
- path containment/symlink protections;
- request/body/output bounds unrelated to concurrency;
- tool-specific authorization/policy checks;
- multi-project/session isolation;
- persistence behavior already implemented for telemetry and command jobs.

The implementation must **not** introduce:

- CatDesk concurrency semaphores replacing the removed ones under a different name;
- CPU/RAM/load-based throttling;
- adaptive concurrency;
- per-session/project fairness queues;
- a global live-job safety ceiling used as a concurrency admission limit;
- a durable `Queued` command state solely to defer execution.

## Out of scope

- Windows native computer-use/UIA and stale-action hardening;
- performance dashboard UI (`catdesk-vmi.4`);
- model-aware cost telemetry (`catdesk-vmi.5`);
- broad `mcp.rs`/`main.rs` structural refactor (`catdesk-vmi.6`);
- AgentForge policy changes outside CatDesk.

## Acceptance criteria

The task is complete when:

- fixed request-worker semaphore limits no longer gate execution;
- normal request load no longer returns `RequestFailure::Busy` because worker slots are occupied;
- `MAX_ACTIVE_JOBS = 8` no longer limits `start_command`;
- `MAX_ACTIVE_PROCESSES = 12` and shared process-permit admission no longer limit foreground/background command execution;
- CatDesk does not sample or consult CPU/RAM/RSS/load to decide concurrency;
- no fair/adaptive scheduler or host governor exists for this task;
- 1/5/10/20/25 concurrent-session verification shows forward progress without CatDesk-generated concurrency rejection;
- cancellation, timeout, dedupe, restart recovery, path safety, output bounds, and existing persistence behavior remain correct;
- full test suite passes;
- release build succeeds.
