# Performance metrics — implementation plan (catdesk-vmi.4)

Date: 2026-09-25 · Base: origin/main @ a925bc5 · Branch: feat/vmi4-perf-metrics-20260925

## Goal

Aggregate bounded in-memory performance diagnostics (catdesk-vmi.4) and surface actionable
metrics on the dashboard: p50/p95/p99 latency per request class, deadline and timeout counts,
in-flight depth + max, response byte rates, cache hit/miss counters, change-scan timing, and
best-effort process CPU/RSS. No raw event retention; never block request handling.

## Non-goals

- No changes to per-request JSONL diagnostics records (epic invariant).
- No scheduler (vmi.2 removed it); in-flight depth + max replaces queue depth.
- No health-endpoint exposure; metrics are RAM + TUI only.
- No touching vmi.3 regions (build.rs, src/build_info.rs, startup.rs:745-748, main.rs header
  ~2011, server.rs health 986-1006, diagnostics.rs init 522-529).

## Design

### src/perf_metrics.rs (new module)

- `static PERF: OnceLock<PerfMetrics>` + `global()` — lazy init, independent of
  `diagnostics::init`.
- Fixed class slots (7): `control | filesystem | process | browser | general | http | scan`.
  The five MCP classes arrive via `SchedulerTiming.class` in the HTTP middleware; plain HTTP
  traffic (health, binagotchy, GET/DELETE) aggregates under `http`; change-scan snapshots
  aggregate under `scan`.
- Tool counters (13, fixed whitelist from `request_metadata` + `other`): count / bytes /
  deadlines / errors only — bounded, no per-tool latency reservoirs.
- Per class a rolling window: ring of 15 buckets x 60 s. A bucket holds count/deadlines/
  failures/bytes plus three fixed reservoirs (total elapsed 32 samples, dispatch 16,
  execution 16, u32 ms, reservoir sampling). Buckets outside `[now_bucket-14, now_bucket]`
  are ignored on read; a write far past the window resets the ring (clock skew safe).
- One global `StdMutex` guards the registry; critical sections are sub-µs arithmetic and
  reservoir pushes, no IO, no await. Snapshots copy under the lock and sort on the stack
  (fixed 15x32 u32 scratch), so no allocation after warm-up.
- Percentiles: nearest-rank over pooled window samples; empty window -> None -> "—" in UI.
- Hard memory bound: < 40 KB steady state (`PerfMetrics` ~31 KB), asserted by test.
- Clock-injected test API: `observe_at(now_ms, …)`, `snapshot_at(now_ms)`.
- Global atomics: in-flight depth + max, cache hit/miss pairs x3 (app config, agents text,
  data URIs), cpu percent (basis points, sentinel = unknown), RSS KiB (sentinel = unknown).

### Wiring

1. `diagnostics::http_request` middleware: in-flight guard around the call; after the response
   (SchedulerTiming from response extensions) observe class/elapsed/dispatch/execution/
   deadline/failures plus `response.body().size_hint().exact()` bytes. `rpc_request` stores the
   whitelisted tool index in the task-local `RequestLog` so the finish path can bump the tool
   counter. Per-request JSONL records unchanged.
2. `mcp.rs` cache choke points: `cached_file_value` (app config + agents text) and
   `cached_data_uri` count hits/misses. Zero logic changes.
3. `change_tracking::ChangeSession::begin/changes`: time `collect_snapshot` into the `scan`
   class; call sites in mcp.rs unchanged.
4. CPU/RSS sampler: dedicated tokio task every 5 s, atomics only, never blocks requests.
   Linux: `/proc/self/stat` (utime+stime, CLK_TCK via sysconf) + `/proc/self/status` VmRSS.
   Windows: GetProcessTimes + GetProcessMemoryInfo (adds `Win32_System_ProcessStatus` to
   windows-sys features — the only manifest change). Parsers are pure and unit-tested.
5. Dashboard `draw_ui` status panel: two lines after "REQ TOTAL" —
   `PERF p50 .. p95 .. p99 .. ACT .. DL .. <rate>` and
   `SYS cpu .. rss .. CACHE ..% SCAN p95 ..`, with i18n labels ("效能", "系統").
   Formatting functions are pure and unit-tested. Sampler spawned next to the axum::serve task.

## Commit sequence (RED/GREEN)

1. `feat: add bounded perf metrics registry with rolling windows` — perf_metrics.rs types,
   registry, tests (percentiles / rollover / reservoir cap / counters / memory bound).
2. `feat: count cache hits and misses and scan timing` — mcp.rs choke points + change_tracking
   scan timing + tests.
3. `feat: feed http requests into perf metrics` — middleware wiring, rpc_tool, in-flight;
   new integration test (existing assertions untouched).
4. `feat: sample process cpu and rss` — sampler, pure parsers + tests, Cargo.toml feature,
   spawn.
5. `feat(tui): show perf and system lines on the dashboard` — formatting + two status lines +
   UI assertions.
6. Full verify: targeted `cargo fmt` (own regions only), `cargo test --release`,
   `cargo build --release`.
