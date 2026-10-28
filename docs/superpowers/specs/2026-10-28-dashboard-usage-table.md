# Dashboard Usage Table Spec

## Goal

Make the Status panel readable at a glance and show token/request usage per
session and per day, not only cost and a 60-second token window.

## Layout

Three sections, separated by blank lines only while everything — including
the flow row — fits (short terminals drop the separators before losing the
flow row or clipping the PERF/HOST tail). At the minimum panel height (13
content lines) the sections fit exactly; a live `504 STREAK` line then pushes
`HOST` out, the least important line:

1. `SYSTEM` (MCP / NGROK / DEVTOOLS / mode) and `ACTIVITY` (chats, jobs, last
   call); the `504 STREAK` warning stays a dedicated line in this section.
2. `USAGE` — one aligned table, columns `REQS CALLS ↓REQ ↑RES Σ TOK COST $/CALL`,
   rows `Last 60s`, `Session`, `Today`, `Avg/day`, `All-time`; then `BURN`
   (live and session-average $/h), `TRACKED` (days, cost and tokens covered by
   the daily usage tracker; days and total covered by the request tracker) and
   `LAST 7 DAYS` (Σ tokens per local day as a sparkline, peak day, weekly
   total). Every cell keeps at least one separating space, and the widest
   realistic row (7-digit requests, `$0.0123+N/A`) fits 72 cells.
3. `PERF` and `HOST` (renamed from `SYS`, which collided with `SYSTEM` in
   Traditional Chinese).

Dashboard dollars use `format_usd_short` (whole dollars from $100, cents from
$1, three significant digits below, `N/A` when not finite); the flow row keeps
the exact formatter. A partially priced cost renders `$X+N/A` (no inner space,
one table cell).

## Requirements

- Persist MCP requests per local calendar day in `daily_request_counts`
  (`serde(default)`, so older configs load unchanged). Existing installs have no
  per-day request history; `total_request_count` is never back-filled.
- `Avg/day` divides each tracker by the days it covers: usage by tracked usage
  days, requests by tracked request days (`—` until one exists); `TRACKED`
  names both day counts. `$/CALL` on that row is tracked cost over tracked
  calls.
- `All-time` keeps legacy usage separate from the tracked-day average (see
  `2026-09-23-dashboard-cost-today-single-flow.md`).
- Resetting token billing does not touch request counters.
- `Last 60s` has no request counter and renders `—` in `REQS`.
- English and Traditional Chinese labels stay coherent and fit the 120-column
  mascot layout.
