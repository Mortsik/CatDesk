# Connection diagnostics

CatDesk writes metadata-only JSON Lines to `~/.catdesk/logs/connections.jsonl`.
The current file and two rotated files (`connections.1.jsonl` and
`connections.2.jsonl`) each hold at most 5 MiB. On Unix, files have mode `0600`.
Only one process can own the log writer; a second process continues serving
without diagnostics and prints a warning. An unwritable directory also disables
diagnostics without preventing startup.

Records contain Unix milliseconds (`timestamp_ms`), process ID (`pid`), and:

- `http_started`: generated `request_id`, HTTP method, `route_matched`, and the
  number of active requests. Paths, query strings and headers are not saved.
- `mcp_request`: the same generated ID and an allowlisted `rpc_method`.
  Unknown methods become `other`. Client IDs, arguments, tool names, resource
  names, commands, output, credentials and connector URLs are never saved.
- `http_finished`: HTTP `status`, numeric `rpc_error_code` when provided by the
  MCP handler, `tool_error` and `content_items` for tool responses, and `elapsed_ms`.
  The latter two fields record only the error flag and content count, not content.
  This means the handler produced its response;
  it does not prove that ChatGPT received it.
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
