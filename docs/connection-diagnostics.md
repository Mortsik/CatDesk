# Connection diagnostics

CatDesk writes metadata-only JSON Lines to `~/.catdesk/logs/connections.jsonl`.
The current file and two rotated files (`connections.1.jsonl` and
`connections.2.jsonl`) each hold at most 5 MiB. On Unix, files have mode `0600`.
Only one process can own each log writer. An overlapping second process uses
`~/.catdesk/logs/concurrent/`, with the same rotation limits. If both slots are
occupied, or neither directory is writable, startup prints a warning and
continues without diagnostics. Check both directories when comparing restarts.

Records contain Unix milliseconds (`timestamp_ms`), process ID (`pid`), and:

- `http_started`: generated `request_id`, HTTP method, `route_matched`,
  `stage: "queued"`, and the number of active requests with the live `stage`
  of the oldest one (`oldest_active_stage`). Paths, query strings and headers
  are not saved.
- `mcp_request`: the same generated ID, an allowlisted `rpc_method`, and for tool
  calls an allowlisted local `rpc_tool`. For `poll_command`, an explicitly
  requested numeric `wait_ms` is recorded as `requested_wait_ms`; for
  `run_command`, an explicitly requested numeric `timeout` is recorded as
  `requested_timeout_ms`. Unknown methods/tool names become `other`. Client IDs,
  all other arguments, custom/browser tool names, resource names, commands,
  output, credentials and connector URLs are never saved.
- `http_finished`: HTTP `status`, numeric `rpc_error_code` when provided by the
  MCP handler, `tool_error` and `content_items` for tool responses, and `elapsed_ms`.
  Scheduled MCP calls additionally record `scheduler_class`,
  `scheduler_queue_wait_ms`, `scheduler_execution_ms`, and
  `scheduler_deadline_stage` (`queue`, `execution`, or null). These timing fields
  contain no arguments or payloads. They distinguish a 504 caused by waiting for
  capacity from one caused after tool execution had already started. `execution_ms`
  is the time CatDesk waited for the response, not necessarily the full lifetime of
  work that continues after a response deadline. The tool fields record only the
  error flag and content count, not content. An `http_finished` record means the
  handler produced its response; it does not prove that ChatGPT received it.
  Every finish also carries the terminal `stage` (`completed`) and the
  `terminal_reason`: `completed`, `deadline_timeout` (the scheduler deadline
  expired), or `worker_failed` (the request worker failed).
- `http_cancelled`: the request future ended without producing a response.
  Terminal `stage` is `cancelled` and `terminal_reason` is
  `client_disconnect`, or `server_shutdown` when the server had already begun
  stopping.
- Process, server and tunnel lifecycle events such as `process_started`,
  `server_started`, `tunnel_started`, `tunnel_failed` and `process_stopping`.
  Raw error messages are deliberately omitted because they can contain URLs.

## Request lifecycle stages

While a request is in flight, the in-memory lifecycle registry tracks its
`stage`: `queued` (accepted, not yet handed to the scheduler), `dispatch`
(handed to the scheduler, waiting for a blocking-pool thread), `executing`
(running on the blocking pool), and `responding` (work returned, response
being assembled). Stages are metadata only; they add no admission control.
The stage of the oldest active request is published on every
`http_started`/`http_finished`/`http_cancelled` record, so a stream of records
shows what stalled work was doing. `http_started` records carry the stage of
their own request (`queued`); later transitions are visible in memory
(`Diagnostics::active_requests_view`) rather than on disk to keep the log
compact.

## Attributing a client stream failure

When ChatGPT reports "Resume stream unavailable" or "Stream cache expired" at
time `T`, take a window (30 seconds each side absorbs client clock skew) and
correlate the records around `T` by `request_id`:

- any `http_finished` with `terminal_reason: "deadline_timeout"` (status 504):
  CatDesk missed the response deadline — the failure is explained server-side;
- any `http_cancelled` with `terminal_reason: "client_disconnect"`: the client
  dropped the connection; that drop is often the stream failure itself, not a
  CatDesk fault;
- `tunnel_started`/`tunnel_failed`/`tunnel_*` events or
  `terminal_reason: "server_shutdown"`: the transport or the process was
  disrupted;
- requests whose `http_started` precedes `T` with no terminal record by `T`
  were still in flight — their age is the leading stall indicator — unless a
  later `process_started` carries a different `pid`: the owning process died
  abruptly, so the request was lost at that restart boundary, not still active;
- none of the above: no CatDesk-side failure; suspect the client or network.

When several coincide, precedence for a single verdict is: server shutdown,
CatDesk timeout, worker failure, tunnel event, client cancellation, then
"no CatDesk-side failure" (`request_lifecycle::classify_stream_failure`
implements this over log records and is exercised by the test suite).
The dashboard also shows the time since the last observed tool call
("last call" on the REQ SESSION line), so a stream error long after the last
call is unlikely to be CatDesk's fault.

## `catdesk diagnose`

The classifier has a built-in caller. `catdesk diagnose` runs the correlation
offline over the log directory — the current file, both rotations, and the
`concurrent/` slot an overlapping restart wrote to, merged and sorted by
`timestamp_ms` — without starting the TUI or opening the diagnostics writer:

```
catdesk diagnose [--recent 30m] [--at 2026-09-22T14:03:00Z] [--window 30s] [--logs-dir ~/.catdesk/logs]
```

`--at` takes an RFC 3339 timestamp and `--recent` an offset back from now
(`ms`/`s`/`m`/`h`); the two are mutually exclusive and 30 minutes is the
default. `--window` defaults to 30 seconds. The report prints the ranked
verdict, the per-window evidence counts (deadline timeouts, client and
shutdown cancellations, worker failures, tunnel events, completed requests,
server-stopping events), then the full-history findings: requests lost at an
abrupt restart with their in-flight age to the boundary, and the oldest
requests still active at the reference time.

The disk writer runs on a separate thread with a bounded queue. When saturated,
requests continue and records are dropped; `dropped_records` on a later record
reports that loss. A write failure disables the writer and prints a warning.
Normal shutdown drains accepted records; abrupt termination can lose queued
records. Shutdown aborts the HTTP server first (`server_stopping`), so in-flight
requests may end as `http_cancelled` and records accepted after `process_stopping`
are best-effort. This is diagnostic logging, not a durable transaction journal.

## Investigating a reported 404

Note the incident time and compare the matching `http_started`, `mcp_request`
and `http_finished` entries by generated `request_id`:

- `route_matched: false`, status 404: the request reached CatDesk but used an
  unregistered path (for example, an obsolete connector slug).
- `route_matched: true`, status 404, `rpc_error_code: -32601`: CatDesk rejected
  an unsupported MCP method. In this version, `initialize` is unsupported;
  discovery uses `server/discover`. This does not mean the ngrok tunnel failed.
- `route_matched: true`, status 405, `rpc_error_code: -32601`: GET on the MCP
  path is disabled (SSE mode not supported) — same unsupported-method family.
- `tunnel_failed` or `tunnel_start_failed`: CatDesk observed a tunnel failure.
- Status 200, `tool_error: true`, `content_items: 0`: the tool returned an error
  with an empty content array. This version places error details in
  `structuredContent`, so a client displaying only `content` may report `[]`.
  This is distinct from an HTTP/tunnel failure; details are not persisted here.
- No matching HTTP record: the request may have failed upstream, but missing
  records alone are not proof (check writer warnings, restarts and dropped records).

The ngrok SDK reconnects its session internally after transport failures,
rebinding the same tunnel without ending the forwarder task. CatDesk wraps the
session connector around `ngrok::session::default_connect` (transport behavior
unchanged) and records every SDK dial: `tunnel_session_reconnect_attempt` each
time the SDK redials after a connection drop, and `tunnel_session_renewed` once
the transport is re-established and the SDK rebinds the tunnel on top of it.
The initial connect stays silent — the supervisor's `tunnel_starting` already
covers it. With the default connector the SDK retries reconnects indefinitely
and stops only when the session is canceled, so a prolonged outage produces a
stream of `tunnel_session_reconnect_attempt` records with no renewal (and no
`tunnel_failed`) for as long as the SDK keeps dialing, leaving the forwarder
pending. The supervisor's `tunnel_reconnect_*` records are a separate family:
they cover full supervisor-loop restarts after session setup failures or
forwarder exits, not in-session reconnects. To identify an upstream ngrok
error, retain its HTTP response body or `ngrok-error-code` header at the time
of failure. Never publish the secret connector URL.

Use `tail -n 100 ~/.catdesk/logs/connections.jsonl` to inspect recent activity.
New logging starts only after restarting CatDesk with the updated binary.

## Stalls and deadlines

Synchronous tool and filesystem operations run on the blocking pool outside the
async network workers. CatDesk enforces no admission control: concurrent work is
bounded only by the host OS and the Tokio runtime, and a saturated search or
command pipeline waits rather than fails with a busy error. Response deadlines
are 45 seconds for control, 60 seconds for general, and 120 seconds for
filesystem, process and browser work, so every scheduled MCP request has a hard
120-second response ceiling. `poll_command` caps a requested wait at 15 seconds,
well below the control deadline, and agents are instructed to move commands
likely to take more than about 20 seconds to `start_command` plus short polls
instead of holding one foreground `run_command` open. A scheduler deadline
returns 504 with `request_worker_timeout`.

Each `http_started`, `http_finished`, and `http_cancelled` record also includes
`active_requests` and `oldest_active_request_ms`. The latter is recomputed from the
requests that are still active, so it can be correlated with client-side stream/resume
failures without persisting MCP payloads or session secrets.

Check `scheduler_deadline_stage` to see whether the deadline expired before the
blocking task started (`queue`) or during `execution`. Work that already started
continues to completion, including after client disconnection or response
timeout. The terminal reason on the finish record names the outcome directly:
`deadline_timeout` for a 504, `worker_failed` when the request worker failed.
**A timeout does not prove that a command or write stopped.** Inspect the result
or poll an existing command job before retrying. MCP `ping` stays independent of
this pipeline and of the shared application-state lock, with normal MCP validation.

Browser calls wait at most two seconds for the serialized DevTools bridge.
Writing to its stdin is limited to ten seconds; a request has a 120-second total
deadline. EOF fails pending calls promptly. Look for `devtools_stdout_closed`,
`devtools_stdin_failed`, `devtools_request_timeout`, and
`devtools_response_too_large` (a response line exceeded 16 MiB). These events do
not reveal stderr, arguments, or response contents. Stderr is classified into
`devtools_stderr_memory_error`, `devtools_stderr_connection_error` or
`devtools_stderr_error`; its raw text is never persisted. Restart CatDesk if its
browser service disconnected; there is no automatic replay of browser actions.

The UI event queue is bounded and best-effort; events may be dropped when the
terminal falls behind. Keyboard polling continues when application state is
busy. Quit restores the terminal before cleanup, which has a six-second limit;
runtime shutdown waits at most one additional second for background work.

Change previews read files in fixed-size chunks while retaining only their
existing bounded preview. Automatic discovery does not descend into another
mounted filesystem. Explicitly choosing that mount as the command's working
directory still permits scanning it; explicit file operations and recursive
listing/search are not disabled by this preview policy. Prefer a specific
project as `WORKSPACE_ROOT`, rather than a directory containing archive disks.

These are application bounds, not an OS memory quota: arbitrary commands,
browser processes and other file operations can still consume substantial RAM.
On Linux, compare `/proc/<pid>/status`, `/proc/pressure/memory` and kernel OOM
records. A large swap allocation alone is not proof of an OOM kill.

## Soak scenarios

The production failure modes above are exercised by two layers of soak (the
`catdesk-43k.3` work):

- `src/soak.rs` runs deterministic in-suite scenarios on every `cargo test`:
  a long request outliving its response deadline (the 504 is classified
  `deadline_timeout` while the side-effectful command survives and stays
  pollable), concurrent tool calls across scheduler classes, client
  disconnects during side-effectful work, a mixed failure storm, and the
  classifier's tunnel branch. Each scenario asserts the failure budget:
  every induced failure carries its expected terminal classification, every
  started request has exactly one terminal record, and the lifecycle registry
  is empty afterwards. Deadlines are shortened only through a per-router
  request-extension override read by the HTTP handler; the production
  45/60/120-second policy is never modified and is pinned by
  `production_response_deadlines_stay_at_the_documented_defaults`.
- `ops/soak-real-duration.sh` runs the true durations against the real
  policy: a real 15-second stream-safe poll boundary, a near-ceiling ~2-minute
  request (`--full`), 24 parallel calls, real client disconnects, and the ngrok
  supervisor's real tunnel lifecycle records, with the same budget assertions
  over the produced JSONL. A mid-stream tunnel drop needs
  a live tunnel; see the script header for the `CLOUDFLARED_UNIT` opt-in.

## External cloudflared reliability

When CatDesk is exposed through a separately managed `cloudflared.service`, keep
that tunnel independently supervised. CatDesk reserves its local listener before
potentially slow browser/DevTools startup, so a reconnecting tunnel can establish a
TCP connection instead of receiving `connection refused` while those components
initialize.

Recent cloudflared releases run DNS, UDP/QUIC and TCP/HTTP2 connectivity prechecks
on startup. If the local precheck reports QUIC failure but HTTP/2 success, the
repository includes `ops/systemd/user/cloudflared.service.d/20-catdesk-reliability.conf`
as a user-service drop-in that forces HTTP/2. It also sets `MemoryLow=64M` (memory
protection, not a limit), restores a neutral `OOMScoreAdjust=0`, keeps automatic
restart enabled, and shortens `RestartSec` to one second. Install the drop-in only
for a dedicated CatDesk tunnel and verify that the journal shows four registered
`protocol=http2` connections after restart.

See the [2026-09-19 investigation](findings/2026-09-19-mcp-stalls.md) for the
evidence and remaining limitations.
