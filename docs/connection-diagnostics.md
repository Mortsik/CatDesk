# Connection diagnostics

CatDesk writes metadata-only JSON Lines to `~/.catdesk/logs/connections.jsonl`.
The current file and two rotated files (`connections.1.jsonl` and
`connections.2.jsonl`) each hold at most 5 MiB. On Unix, files have mode `0600`.
Only one process can own each log writer. An overlapping second process uses
`~/.catdesk/logs/concurrent/`, with the same rotation limits. If both slots are
occupied, or neither directory is writable, startup prints a warning and
continues without diagnostics. Check both directories when comparing restarts.

Records contain Unix milliseconds (`timestamp_ms`), process ID (`pid`), and:

- `http_started`: generated `request_id`, HTTP method, `route_matched`, and the
  number of active requests. Paths, query strings and headers are not saved.
- `mcp_request`: the same generated ID, an allowlisted `rpc_method`, and for tool
  calls an allowlisted local `rpc_tool`. Unknown methods/tool names become
  `other`. Client IDs, arguments, custom/browser tool names, resource
  names, commands, output, credentials and connector URLs are never saved.
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
- `http_cancelled`: the request future ended without producing a response.
- Process, server and tunnel lifecycle events such as `process_started`,
  `server_started`, `tunnel_started`, `tunnel_failed` and `process_stopping`.
  Raw error messages are deliberately omitted because they can contain URLs.

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

The ngrok SDK may reconnect internally without completing its forwarder task;
these transient reconnects are not captured by the lifecycle events. To identify
an upstream ngrok error, retain its HTTP response body or `ngrok-error-code`
header at the time of failure. Never publish the secret connector URL.

Use `tail -n 100 ~/.catdesk/logs/connections.jsonl` to inspect recent activity.
New logging starts only after restarting CatDesk with the updated binary.

## Stalls, busy responses and memory

Synchronous tool and filesystem operations run outside the async network
workers. Capacity is isolated by request class (control, filesystem, process,
browser and general), while temporary saturation queues fairly by session/project
instead of immediately failing. Queue safety budgets remain bounded; exhausting a
budget returns 503 with a class-specific `request_*_busy` event. Response deadlines
are 45 seconds for control, 60 seconds for general, and 120 seconds for filesystem,
process and browser work, so every scheduled MCP request has a hard 120-second
response ceiling. A deadline returns 504 with `request_worker_timeout`.

Each `http_started`, `http_finished`, and `http_cancelled` record also includes
`active_requests` and `oldest_active_request_ms`. The latter is recomputed from the
requests that are still active, so it can be correlated with client-side stream/resume
failures without persisting MCP payloads or session secrets.

Check `scheduler_deadline_stage` to see whether it expired in `queue` or during
`execution`. Once execution starts, the worker continues to own its slot until the
operation actually ends, including after client disconnection or response timeout.
**A timeout does not prove that a command or write stopped.** Inspect the result
or poll an existing command job before retrying. MCP `ping` stays independent of
this scheduler and of the shared application-state lock, with normal MCP validation.

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
