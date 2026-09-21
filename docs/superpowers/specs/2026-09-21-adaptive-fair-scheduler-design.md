# CatDesk adaptive fair scheduler design

Date: 2026-09-21
Issue: `catdesk-vmi.2`
Status: proposed / user-approved direction, pending written-spec review

## Intent

CatDesk should support roughly 10–25 concurrent agent/chat sessions without making agents reason about internal worker limits. From the caller's perspective, work should normally be accepted rather than rejected with `Busy` just because another session currently occupies a worker slot.

At the same time, CatDesk must not turn “no artificial limits” into unbounded physical execution. The host still needs protection against CPU saturation, memory pressure, process storms, browser storms, and an unbounded in-memory queue.

The target behavior is therefore:

- no per-user, per-session, or per-project hard quota as a normal admission rule;
- no ordinary fail-fast `Busy` solely because a worker semaphore is currently full;
- fair queuing across sessions, with project awareness so one repo cannot monopolize the host;
- a protected control plane (`poll`, `cancel`, health/session lifecycle) that remains responsive under heavy work;
- dynamic physical concurrency based on host pressure;
- bounded memory even if logical demand is much larger than current execution capacity;
- durable lifecycle for queued background command jobs so a CatDesk restart does not silently lose work that never started.

Windows native computer-use/UIA remains out of scope.

## Current state

There are two independent saturation mechanisms today.

### Request workers

`src/request_workers.rs` has fixed pools:

- control: 8
- filesystem: 16
- process: 12
- browser: 4
- general: 8

`RequestWorkers::run()` uses `Semaphore::try_acquire_owned()`. If the pool is full, the request fails immediately with `RequestFailure::Busy`.

This protects the host, but it makes contention visible to agents as an error and provides no fairness between sessions.

### Command jobs

`src/command_jobs.rs` has:

- `MAX_ACTIVE_JOBS = 8`
- `MAX_ACTIVE_PROCESSES = 12`

`start_command` rejects a ninth active job and process acquisition is also fail-fast through `try_acquire_owned()`.

This is a logical admission cap rather than just a physical execution cap. It prevents CatDesk from accepting additional durable work even when it could safely queue it.

## Design principles

### 1. Separate admission from execution

CatDesk may accept substantially more work than it can execute simultaneously.

“Accepted” means CatDesk owns the lifecycle of the request/job. “Running” means it currently consumes a scarce host resource. These must no longer be treated as the same thing.

### 2. No ordinary user-visible `Busy` for temporary saturation

Temporary saturation should result in waiting/queuing, not an immediate error.

`Busy` remains valid only for exceptional protection cases such as:

- scheduler shutdown;
- queue memory/safety circuit breaker activation;
- host in critical memory pressure where admitting more non-durable request work would itself be unsafe;
- unrecoverable scheduler failure.

It should not mean “all 12 slots happen to be occupied right now”.

### 3. Fairness is session-first, project-aware

The scheduler should use session identity as the primary fairness key because chats/agents are the actual independent producers of work.

Within that model, project identity is an additional scheduling dimension. A project with many sessions should receive substantial throughput, but should not permanently starve sessions working in another project.

No hard quota is assigned to either session or project. Fairness affects ordering, not admission limits.

### 4. Protect the control plane

Control requests must not sit behind filesystem/process/browser work.

Control operations use a dedicated reserved execution path and bypass the heavy-work queue where safe. This includes at minimum health/ping, poll/cancel, session disconnect/cleanup, and scheduler introspection needed to recover from overload.

### 5. Physical execution remains bounded by a host governor

Removing logical limits does not mean unlimited process creation.

A resource governor chooses the current execution capacity for heavy classes. It uses conservative defaults and host-pressure signals to increase or reduce concurrency.

Initial pressure inputs:

- total/logical CPU count;
- CatDesk process RSS;
- system available memory where the platform exposes it cheaply;
- recent queue depth and worker utilization;
- active child process count / active command process permits;
- browser-class occupancy.

The governor should favor stability over perfect utilization. It may reduce new starts under pressure but should not kill already-running work solely because the target concurrency fell.

No new heavyweight monitoring dependency is required if the existing platform APIs/procfs can supply enough data. If cross-platform host memory/CPU collection becomes disproportionately complex, implementation may isolate the governor behind a trait and initially use a conservative adaptive policy with Linux metrics plus safe fallback defaults on other platforms.

## Scheduler architecture

### Request scheduler

Replace the current “one semaphore per class + try acquire” model with a scheduler that owns queued request descriptors.

Each queued descriptor includes at least:

- request class;
- session key;
- optional project key;
- enqueue sequence/time;
- response deadline / cancellation state;
- one-shot mechanism used to grant execution.

The queue should be logically fair rather than a single global FIFO. A practical policy is deficit/round-robin style rotation:

1. rotate among sessions with pending work;
2. within a session, rotate among projects/classes that have pending work;
3. respect class-specific physical capacity and control-plane reservations;
4. skip entries that cannot currently run and continue searching for runnable work.

Exact algorithm choice can remain implementation-local as long as tests prove starvation resistance and deterministic ordering properties.

### Queue memory protection

The scheduler cannot accept infinite non-durable HTTP work into RAM.

Use a high global safety ceiling based primarily on memory footprint, not a small per-session quota. Reaching it is an exceptional circuit-breaker condition and may return `Busy`/overload.

This ceiling should be far above normal 10–25-session demand and should be observable in telemetry. It exists to prevent OOM, not to shape normal throughput.

Timed-out/disconnected queued requests must be removed or marked cancelled without consuming a future execution slot.

### Command job lifecycle

Background commands need an explicit queued state.

Extend `CommandJobState` with `Queued`.

`start_command` flow becomes:

1. validate/dedupe;
2. create and persist a queued job record immediately;
3. return its job id promptly;
4. scheduler waits for process execution capacity;
5. atomically transition `Queued -> Running` before spawn;
6. execute using the existing durable output/timeout/cancel machinery;
7. terminal state remains unchanged from the existing model.

There is no `MAX_ACTIVE_JOBS = 8` admission rule.

The current process semaphore concept may remain internally as one implementation mechanism, but acquisition becomes awaited/scheduled rather than fail-fast and its permit count is controlled by the host governor.

### Restart behavior for queued jobs

A queued job has never executed side effects. Therefore it can safely remain queued across a CatDesk restart.

Recovery rules:

- persisted `Queued` -> restore as `Queued` and make eligible for scheduling;
- persisted `Running` -> keep the current conservative behavior and recover as `Interrupted`, because CatDesk cannot prove whether an external side effect completed before the restart;
- terminal states -> restore as today.

This distinction is important: CatDesk may automatically resume work that provably never started, but must not blindly replay work whose execution outcome is uncertain.

### Cancellation

Cancellation must work in both states.

For `Queued`:

- remove/disable the queue entry;
- transition directly to `Cancelled`;
- never spawn a process.

For `Running`:

- retain the existing process-tree termination behavior.

Session disconnect cancellation should similarly handle queued and running jobs owned by that session.

## Deadlines and waiting semantics

Request deadline and queue waiting are distinct from underlying work lifetime.

For ordinary synchronous MCP requests:

- queue wait counts toward the response deadline;
- if the caller deadline expires before execution starts, the queued work must not later execute as a surprise side effect;
- if execution has already started and the response deadline expires, preserve the current rule: the execution slot remains owned until the real work ends.

For durable `start_command` jobs:

- the MCP call only needs enough time to durably accept the job;
- queued waiting occurs after the call returns and does not consume the request-response deadline;
- the command's own runtime timeout should begin when the process actually starts, not while the job is merely queued;
- elapsed metadata should expose queue wait separately from execution duration where practical.

## Adaptive host governor

The first implementation should be intentionally conservative.

Suggested policy shape:

- define a safe minimum concurrency for each heavy class;
- define a generous maximum based on CPU count / platform defaults;
- periodically sample pressure, not on every tool call;
- increase capacity slowly after a sustained healthy period;
- decrease admission of new executions quickly under memory pressure or severe CPU saturation;
- never shrink by revoking permits from work already running;
- browser concurrency remains lower than filesystem/process concurrency because browser instances are disproportionately expensive;
- control plane is excluded from heavy-resource throttling except under catastrophic process shutdown.

The exact thresholds are configuration details, not public API. They should be observable and tunable later from performance telemetry (`catdesk-vmi.4`).

## Observability required by this change

Even before the full performance dashboard task, the scheduler must expose enough counters to debug itself:

- queued requests total and by class;
- active requests by class;
- queue wait time for completed admissions;
- queued/running command job counts;
- current governor concurrency targets;
- overload/circuit-breaker rejection count;
- cancellation-before-start count.

These counters may initially live in diagnostics/runtime state and do not need the final dashboard UI yet.

## Files / components expected to change

Primary:

- `src/request_workers.rs` — fair queued request scheduler and capacity control;
- `src/command_jobs.rs` — `Queued` lifecycle, durable admission, scheduled process start;
- `src/server.rs` — pass session/project scheduling identity and classify control-plane work;
- job persistence/serialization tests as needed for `Queued` state;
- lightweight governor module, preferably isolated from request scheduling logic.

Possible new modules:

- `src/resource_governor.rs`
- `src/fair_queue.rs` or equivalent if `request_workers.rs` becomes too large.

This task should not perform the broad `mcp.rs`/`main.rs` structural refactor from `catdesk-vmi.6`.

## Testing strategy

Implementation is test-driven.

Required behavioral tests:

1. saturated request pool queues work instead of returning `Busy`;
2. queued work starts when capacity is released;
3. one noisy session cannot starve another session;
4. one noisy project cannot permanently starve a different project;
5. control-plane work remains responsive while heavy queues are saturated;
6. a queued request whose deadline expires never executes later;
7. caller disconnect before start prevents later execution;
8. once execution starts, timeout/disconnect does not prematurely release capacity;
9. more than eight background jobs can be accepted;
10. excess background jobs remain `Queued` rather than spawning extra process trees;
11. queued job cancellation guarantees no side effect occurs;
12. queued jobs survive restart and later execute;
13. running jobs still recover as `Interrupted`, never auto-replayed;
14. idempotency/dedupe works across queued jobs;
15. governor target can increase/decrease without killing active work;
16. queue/counter telemetry stays internally consistent under concurrent load.

A synthetic load test should exercise at least 1, 5, 10, 20 and 25 sessions and record throughput, queue wait, active concurrency, CPU/RSS and overload events. The purpose is to validate stability, not to hard-code a benchmark threshold that will be flaky across machines.

## Safety invariants

The implementation must preserve these invariants:

- never execute a cancelled queued side-effecting request later;
- never replay a previously running command after uncertain restart outcome;
- never let client disconnect free a slot while underlying synchronous work is still running;
- never let heavy work starve poll/cancel/health indefinitely;
- never allow queue metadata itself to grow without a global safety bound;
- never hold global application state locks while waiting for scheduler capacity;
- preserve existing command process-tree termination guarantees;
- preserve command idempotency behavior across concurrent/retried requests.

## Out of scope

- Windows native computer-use/UIA and stale-action hardening;
- final performance dashboard UI (`catdesk-vmi.4`);
- model-aware cost telemetry (`catdesk-vmi.5`);
- broad source-file refactor (`catdesk-vmi.6`);
- AgentForge policy changes outside CatDesk itself.

## Acceptance criteria

The task is complete when:

- normal temporary saturation no longer produces fail-fast `Busy` for ordinary supported workload;
- CatDesk accepts substantially more than eight background jobs and queues excess execution safely;
- 10–25 concurrent sessions make forward progress without starvation;
- control-plane calls remain responsive under saturation;
- physical process/browser concurrency stays governed and host-safe;
- queued durable jobs survive restart while uncertain running jobs are not replayed;
- scheduler state is observable enough to diagnose queueing/governor behavior;
- full existing test suite plus new scheduler tests pass;
- release build succeeds;
- synthetic 1/5/10/20/25-session load verification shows stable bounded execution rather than fail-fast saturation or runaway resource creation.
