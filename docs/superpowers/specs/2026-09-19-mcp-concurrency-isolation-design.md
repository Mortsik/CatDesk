# MCP Concurrency Isolation Design

## Goal

Maximize CatDesk throughput and the number of parallel MCP clients while preventing a slow filesystem, browser call, command, or disconnected client from freezing unrelated control operations or the TUI.

## Constraints

- Preserve current pure-HTTP MCP compatibility; `initialize` remains unsupported.
- `ping`, cancellation, polling, discovery, and connector bootstrap must remain responsive during heavy workload saturation.
- Do not solve overload by globally serializing requests or by reducing the current useful concurrency.
- Blocking filesystem work must stay off the async reactor.
- Background and foreground commands must share one process budget so process-tree count is bounded independently of HTTP request count.
- Stateless clients without a stable session identifier must continue to work.
- No unbounded queues, retained output, session maps, or job registries.

## Request identity

CatDesk accepts an optional `Mcp-Session-Id` header. When present it is the namespace for session-local state: instruction gate, UI flow identity, job ownership/idempotency, and diagnostics. The raw identifier is never rendered or logged; CatDesk derives a short stable hash for UI/diagnostics.

Clients that do not send a stable session identifier remain compatible through an anonymous/stateless fallback. Anonymous requests keep the process-wide instruction fallback, but `start_command` retry deduplication is disabled for them so unrelated clients reusing JSON-RPC IDs can never share a job accidentally.

Session metadata is bounded by TTL and maximum entry count. It is small synchronous metadata protected by a short standard mutex; no lock spans I/O or await points.

## Admission control

Replace the single 12-slot worker pool with resource classes:

- `Control`: discovery/list/bootstrap, `catdesk_instruction`, `poll_command`, `cancel_command`; reserved capacity and short deadlines.
- `Filesystem`: read/search/list/write/edit/delete and other local synchronous workspace work; high parallel capacity.
- `Process`: foreground `run_command` and `start_command` request setup; separate from filesystem stalls.
- `Browser`: DevTools-backed calls; small dedicated capacity because the current bridge serializes browser protocol work.
- `General`: bounded fallback for unknown/non-tool MCP methods.

`ping` stays outside all worker pools. Pool saturation fails fast with a class-specific busy response instead of queuing indefinitely. This preserves throughput while preventing one workload class from starving another.

## Process budget

`run_command` and `start_command` share a `ProcessBudget` owned by `CommandJobManager`. The total active command process-tree count is bounded at 12, while background jobs retain their existing maximum of 8. A background job reserves a process permit before being accepted and holds it until its process tree terminates. A foreground command acquires from the same budget for its complete execution. Poll/cancel do not consume process permits.

The process budget is deliberately independent of MCP worker slots: HTTP concurrency may be high while expensive child-process concurrency remains bounded.

## Filesystem boundaries

Recursive search and listing must not cross filesystem mount boundaries from their starting directory. Built-in walkers use `same_file_system(true)`; ripgrep uses its one-filesystem mode. Recursive list traversal skips directories on a different device on Unix/WSL. This prevents 9p/network/archive mounts nested below a workspace from consuming workers forever.

The built-in search fallback must not read arbitrarily large files into memory; file reads are capped and streamed/bounded.

## Persistence

Config read-modify-write operations are serialized through one process-local config lock. Writes use a temporary file in the same directory and atomic rename on supported platforms, so readers do not observe truncated TOML and concurrent single-field updates do not silently overwrite one another.

Request hot paths should minimize time holding `AppState`; synchronous persistence must not expand into unrelated request scheduling locks.

## UI resilience

The UI queue remains bounded and non-blocking. Telemetry may be dropped under load, so applying a later telemetry event must never require an earlier event to have arrived. In particular, turn-usage updates for a missing flow are ignored rather than panicking. Session-derived flow IDs prevent independent clients from merging into one timeline when stable session identity exists.

## Testing

Add regression tests for:

1. control requests remaining admissible while filesystem/process pools are saturated;
2. independent sessions reusing the same JSON-RPC ID and command getting distinct jobs;
3. same-session retries deduplicating correctly;
4. anonymous clients not cross-deduplicating jobs;
5. global foreground/background process budget recovery after completion/cancel;
6. search/list mount-boundary behavior where testable without privileges;
7. concurrent config updates preserving both fields and always producing parseable TOML;
8. dropped/out-of-order UI telemetry not panicking;
9. multi-client stress with repeated parallel control/read/process requests.

## Deferred follow-ups

Automatic per-session Git worktree creation and true per-page concurrent DevTools multiplexing are valuable but are separate product-level behaviors. This change prepares session identity and resource isolation without forcing automatic repository topology changes on existing users. An ngrok reconnect supervisor is also independent of local MCP scheduler correctness and should follow as a separate resilience patch.