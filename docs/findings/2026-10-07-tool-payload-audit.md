# CatDesk tool payload audit — 2026-10-07

Epic `catdesk-ojt` (context/output efficiency without capability loss), issue `catdesk-ojt.5`.

Every tool exposed by the CatDesk MCP tool catalog (`src/mcp/tool_catalog.rs`,
`handle_tools_list_with_show_detail_mode`), audited for unbounded inline
text/collections in tool results. The tool set was enumerated programmatically
(the inventory test below drives `handle_tools_list` through the mode/tool-mode
matrix), and each handler was traced to its bounding mechanism.

## Bounding classes

| Class | Meaning |
| --- | --- |
| `shared-budget` | Response flows through the canonical dispatcher gate `apply_response_budget` (`src/mcp.rs:493-506`): results over 64 KiB serialized (`src/mcp/response_budget.rs:6`) are losslessly stored in the large-result store and replaced by a bounded preview with a `responseBudget` manifest pointing at `read_result`/`search_result`. |
| `shared-budget+pre-cap` | Same dispatcher gate, plus a tool-level cap on the data before serialization (command buffers, search limits, read batch budget). Pre-caps bound what enters the store; the shared budget bounds the inline response. |
| `store-range` | The tool *is* the retrieval instrument, so it is excluded from the dispatcher gate (`src/mcp.rs:493`) and bounded inherently by store-side request validation (`src/result_store.rs`). |
| `inherent-static` | Response is generated from static templates or small scalars; no host-controlled payload surface. Still covered by the shared-budget gate as a backstop. |
| `multimodal-exempt` | Response carries native non-text MCP content, which the shared budget deliberately leaves intact (`response_budget.rs:168`, `has_native_non_text_content`) to preserve multimodal capability. Image bytes are capped by `MAX_IMAGE_BYTES`. |
| `devtools-passthrough` | Dynamic tools forwarded verbatim to the Chrome DevTools MCP bridge; names are not statically enumerable. Transport is capped at 16 MiB (`src/devtools.rs:13`), and results still pass through the shared-budget gate after forwarding. |

## Local tool inventory

`Exposed in`: C = `Mode::Computer`-enabled (also `Mode::Both`), M = `ToolMode::MultiTools`,
R = `ToolMode::ReadOnly`, B = `Mode::Browser` without computer.

| Tool | Exposed in | Payload surface(s) | Bounding | Evidence | Notes |
| --- | --- | --- | --- | --- | --- |
| `run_command` | C+M | `stdout`, `stderr`, text content, intercepted listing (`listEntries`) | `shared-budget+pre-cap` | `src/command.rs:8` (`MAX_BUFFER_BYTES` = 64 MiB per stream, `src/process_runner.rs:657`), listing caps `src/workspace_tools.rs:30-31` (default 200, hard 1000), budget gate `src/mcp.rs:499`, exact omitted-bytes reporting `src/command.rs:1138-1185` | Both ends of each stream are retained on truncation (`stdoutTruncated`/`stderrTruncated`). See finding F2 for the per-stream vs per-response cap gap. |
| `start_command` | C+M | `events[]` (early output), job scalars | `shared-budget+pre-cap` | retained-output cap 4 MiB/job `src/command_jobs.rs:31`, `src/mcp/commands.rs:163` | First snapshot may carry early output up to the retained cap; shared budget externalizes anything over 64 KiB. |
| `poll_command` | C+M | `events[]` (incremental output), text content | `shared-budget+pre-cap` | per-poll clip 128 KiB `src/command_jobs.rs:33,352-381`, retained 4 MiB `src/command_jobs.rs:31` | A single event larger than 128 KiB still passes whole (see discoveries); the shared budget bounds the final inline response. |
| `cancel_command` | C+M | `events[]` (terminal output) | `shared-budget+pre-cap` | terminal retained output 32 MiB `src/command_jobs.rs:32`, `src/mcp/commands.rs:330` | Bounded by retention cap + shared budget. |
| `catdesk_instruction` | C+M, C+R, B | `instructionText`, text content | `inherent-static` | static template lines `src/mcp/instruction.rs:142-260` | No host-controlled payload; shared-budget gate remains as backstop. |
| `read` | C+M, C+R | `files[].text`, text content | `shared-budget+pre-cap` | batch budget 512 KiB `src/workspace_tools.rs:23`, per-file cap 512 KiB `src/workspace_tools.rs:21`, batch size 32 `src/workspace_tools.rs:22`, per-file budget accounting `src/workspace_tools.rs:449-473`, flags `budgetTruncated`/`batchTruncated` `src/mcp/file_tools.rs:56-61` | Files past the batch budget return metadata only. |
| `read_image` | C+M, C+R | image content (base64), `structuredContent` (with `analyze`) | `multimodal-exempt` | input cap 20 MiB / 40 Mpx `src/workspace_tools.rs:27-28`, resize default 1600 max 4096 `src/workspace_tools.rs:24-26`, post-encode cap `src/workspace_tools.rs:425`, exemption `src/mcp/response_budget.rs:168` with test `src/mcp/response_budget.rs:879`, size-cap test `src/mcp/tests.rs:4014-4031` | Deliberate multimodal exception: native image content bypasses the budget so clients keep vision capability. With `analyze`, `analysis.description` has no cap — finding F1. |
| `search` | C+M, C+R | `searchResults[]` (path/line/text), text content | `shared-budget+pre-cap` | match cap default 100 hard 500 `src/workspace_tools.rs:32-33`, per-file cap `src/mcp/tool_catalog.rs:642`, deadline truncation `src/workspace_tools.rs:39-50`, fallback scan caps `src/workspace_tools.rs:62-64`, `searchTruncated` flag `src/mcp/file_tools.rs:506` | A single matched line can still be arbitrarily long (minified files); the shared budget externalizes such responses over 64 KiB. |
| `read_result` | C+M, C+R | `dataBase64`, `text` | `store-range` | request `max_bytes` rejected above 128 KiB (`RangeTooLarge`) `src/result_store.rs:363-368`, constant `src/result_store.rs:13` | Excluded from the dispatcher gate (`src/mcp.rs:493`) because it is the retrieval instrument for the store; bounded by store-side validation. Worst case ≈ 128 KiB raw → ~171 KiB base64 inline. |
| `search_result` | C+M, C+R | `matches[]` (snippets), `query` echo | `store-range` | `max_matches` rejected above 100 `src/result_store.rs:413-418`, snippet 256 B `src/result_store.rs:17`, default 20 `src/mcp/result_tools.rs:90` | Same gate exclusion as `read_result`. The `query` echo is client-controlled and unbounded — finding F3. |
| `write` | C+M | `bytesWritten`, message | `inherent-static` | input cap 512 KiB `src/workspace_tools.rs:29`, response `src/mcp/file_tools.rs:213-226` | Response is scalars + short message. |
| `edit` | C+M | operation counters, message | `inherent-static` | atomic batch, rendered summary `src/mcp/file_tools.rs:406-423` | Response is scalars + short message. |
| `create_handoff` | C+M, C+R | `content`, `gitStatus[]`, `recentCommits[]`, text content | `shared-budget+pre-cap` | list inputs capped at 100 items `src/handoff.rs:6`, rendered content capped at 128 KiB `src/handoff.rs:7,58-64`, git status capped at 80 lines `src/handoff.rs:8,241-252` | Over 64 KiB the shared budget externalizes; over 128 KiB the tool errors. |
| `delete` | C+M | message | `inherent-static` | `src/mcp/file_tools.rs:594-620` | Scalars only. |

## DevTools passthrough (dynamic set)

With `Mode::Browser` enabled and the DevTools bridge connected, every tool
published by the Chrome DevTools MCP server is appended to the catalog
(`src/mcp/tool_catalog.rs:778-788`) and dispatched verbatim via
`forward_to_devtools` (`src/mcp/commands.rs:26-79`). These tools are not
statically enumerable, so the inventory test pins the passthrough contract
instead of individual names:

| Surface | Bounding | Evidence |
| --- | --- | --- |
| DevTools JSON-RPC response (any shape: snapshots, console logs, network bodies, base64 screenshots) | `devtools-passthrough` | transport reader capped at 16 MiB `src/devtools.rs:13,195-201`; forwarded result passes the shared-budget gate `src/mcp.rs:499`; text/blob/array compaction `src/mcp/response_budget.rs:191-262` |

Native non-text content in a forwarded result (e.g. a screenshot) triggers the
same deliberate multimodal exemption as `read_image`
(`src/mcp/response_budget.rs:168`), so image bytes stay inline; they are bounded
upstream by the DevTools transport cap.

## Findings

### F1 — `read_image` with `analyze`: vision description is unbounded inline — severity: medium

- Tool: `read_image` (`analyze: true`/custom prompt)
- Location: `src/mcp/file_tools.rs:159-197` (analyzed response embeds `analysis.description`), `src/vision.rs:100-252` (no length cap on the description), exemption trigger `src/mcp/response_budget.rs:168`
- Problem: the analyzed response carries `content[0].type = "image"`, so `has_native_non_text_content` exempts the *entire* response from the shared budget — including `structuredContent.analysis.description`, which is model output with no cap. A verbose vision response travels inline at full size.
- Suggested direction: cap `analysis.description` when building the response (head/tail preview, e.g. 8 KiB), or extend the budget to compact `structuredContent` even when native content is present (leaving `content[]` intact).

### F2 — externalization fail-open can exceed the store entry cap for `run_command` — severity: low

- Tool: `run_command` (also any tool whose serialized result exceeds 64 MiB)
- Location: `src/command.rs:8` (`MAX_BUFFER_BYTES` = 64 MiB *per stream*), `src/result_store.rs:285-290` (`EntryTooLarge` above 64 MiB per entry), `src/mcp.rs:499` (`let _ =` leaves the original response untouched on store failure)
- Problem: `stdout` and `stderr` are each capped at 64 MiB, so a command flooding both streams produces a serialized result over the store's 64 MiB entry cap. `put` then fails with `entry_too_large` and the deliberate fail-open (`src/mcp.rs:496-498`, test `src/mcp/response_budget.rs:971`) returns the full ~128 MiB response inline.
- Suggested direction: budget `stdout`+`stderr` jointly below `DEFAULT_MAX_ENTRY_BYTES` (leaving room for JSON overhead), or make `apply_response_budget` fall back to `hard_minimal_preview` when `put` fails with `entry_too_large` (the preview is not lossless, but the full output already cannot be stored — today it is silently lost from the store while bloating the transcript).

### F3 — `search_result` echoes the client-supplied `query` without a cap — severity: low

- Tool: `search_result`
- Location: `src/mcp/result_tools.rs:113` (`"query": query` echoed into `structuredContent`), `src/mcp/result_tools.rs:78-85` (`required_nonempty_string` has no maximum)
- Problem: the tool is excluded from the dispatcher budget gate (it is a retrieval instrument), and the echoed query is client-controlled with no length cap. A multi-megabyte query (self-inflicted, but the server should defend its own response budget) is returned inline verbatim.
- Suggested direction: stop echoing the query, or cap the echo (e.g. first 1 KiB) — `query` is already known to the caller.

### F4 — widget/token metadata is attached after the budget gate — severity: info

- Location: `src/mcp.rs:508-514` (`attach_turn_token_usage` / `attach_tool_call_count` run after `apply_response_budget`)
- Problem: the `_meta` enrichment can push a budgeted response a few hundred bytes over the 64 KiB inline limit. Purely cosmetic today (fixed-size scalars).
- Suggested direction: attach metadata before the budget gate, or accept the documented overhead.

## Discoveries (no action required)

- `poll_command` clips each poll to 128 KiB, but the clip never splits a single event (`src/command_jobs.rs:356` `!events.is_empty()` guard), so one oversized output line passes whole; the shared budget still bounds the final inline response.
- `read_result` errors (`RangeTooLarge`, `OffsetPastEnd`) echo only numbers, and `resultId` echoes store-generated metadata, not client input — no unbounded error surface.
- The dispatcher budget gate (`src/mcp.rs:493-506`) is the single canonical truncator for tool results; no per-tool duplicate truncator bypassing the shared policy was found. `run_command` stream caps and search/read/handoff limits are pre-caps feeding the shared policy, not competing truncators.

## How the inventory test bites

`tool_payload_audit_covers_every_exposed_tool` (in `src/mcp/tests.rs`) drives
`handle_tools_list` across the full mode/tool-mode matrix with the DevTools
bridge absent (deterministic local set) and asserts the exposed set equals the
audited inventory exactly — in both directions. Adding a tool to the catalog
without an inventory entry fails the test; a stale inventory entry whose tool
was removed fails too. Companion tests validate that every mechanism string is
one of the audited bounding classes, and that this document lists every audited
tool (and no phantom ones), keeping the table and the test data in lockstep.
