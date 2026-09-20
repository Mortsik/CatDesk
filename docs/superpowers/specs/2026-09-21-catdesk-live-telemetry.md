# CatDesk Live Telemetry — Design

**Date:** 2026-09-21

## Goal

Extend the main CatDesk TUI dashboard with live operational telemetry for variant B:

- current active background command jobs,
- current connected ChatGPT/MCP chats,
- trailing 60-second token burn rate,
- trailing 60-second USD burn rate shown as `$ / min` and extrapolated `$ / h`.

Existing `Session` and `All-time` token/cost totals remain unchanged.

## Semantics

### Connected chats

A connected chat is a unique MCP `flow_id` observed by `AppState::record_flow` and not yet closed by `BeginFlowClose`. Named MCP session IDs are already hashed into `flow_id` before reaching UI state, so the dashboard must not retain raw MCP session IDs. When `SetRemoteConnected(false)` is received, the connected-chat set is cleared because the server emits that only for the final named-session disconnect or stateless disconnect.

### Active jobs

An active job is a `CommandJob` whose runtime state is `Running`. The TUI may sample this count from `CommandJobManager` at most once per second. It must not scan jobs at the 60 Hz draw rate.

### 60-second burn rate

Every successful usage accounting event contributes one timestamped sample containing tool-input and tool-output token deltas. The dashboard sums samples whose age is less than or equal to 60 seconds.

- `↓ input/min`: trailing-60-second `tool_input_tokens`
- `↑ output/min`: trailing-60-second `tool_output_tokens`
- `Σ tokens/min`: trailing-60-second total
- `$ / min`: cost of the trailing-60-second usage using CatDesk's existing GPT-5.6-and-earlier pricing function
- `$ / h`: current burn extrapolated as `($ / min) * 60`

Samples are session-only telemetry and are not persisted to `config.toml`. Old samples are pruned to keep memory bounded.

## UI

Keep existing status rows. Change `Remote connected` to also show `Chats N` and `Jobs N`, then add one `60s rate` row immediately before `Session` and `All-time`.

Example:

`Remote connected     V   Chats 3   Jobs 2`

`60s rate             ↓12.4K/min  ↑2.1K/min  Σ14.5K/min  $0.124/min  $7.44/h`

Traditional Chinese labels should follow the existing localization style.

## Constraints

- No new dependency.
- No raw session IDs stored in UI telemetry.
- No high-frequency job-manager polling.
- Existing billing totals and reset behavior must remain unchanged.
- Existing request/flow behavior must remain unchanged.
