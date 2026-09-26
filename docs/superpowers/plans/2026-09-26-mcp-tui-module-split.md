# MCP and TUI module split — implementation plan (catdesk-vmi.6)

Date: 2026-09-26 · Base: origin/main @ 3ce8af1 · Branch: feat/vmi6-module-split-20260926

## Goal

Split the two CatDesk monoliths into focused modules with clear interfaces, preserving
behavior exactly: `src/mcp.rs` (9338 lines) becomes a coordinating root plus `src/mcp/*`
submodules, `src/main.rs` (6637 lines) becomes a coordinating root plus `src/tui/*`
submodules. Target: mcp.rs ~700 lines, main.rs ~1400 lines. Zero user-visible change;
the full suite (487 tests) stays green after every stage; no import cycles.

## Non-goals

- No behavior changes, no API/schema changes, no test-assertion edits (test bodies move
  verbatim; only import paths may change).
- No restructuring of `src/widget/` assets, `src/theme/`, or any other existing module.
- No reformatting (rustfmt) of moved code — moves keep indentation as-is so
  `--color-moved=dimmed-zebra` shows clean relocations.

## Move discipline (§3 rules)

Pure moves of whole items only. Allowed edits per stage:

1. `private` → `pub(crate)` (visibility only, no signature change).
2. `use` statements.
3. `mod` declarations plus explicit re-exports in the roots (only for items the root or
   `server.rs` still references).
4. `include_str!`/`include_bytes!` path prefixes — exclusively in M4 (`src/mcp/` sits one
   level deeper than `src/`, so `widget/...` becomes `../widget/...`).

Statics move 1:1 (same name, same type, same initializer). After 0a/0b move the test
modules wholesale; later stages only update import paths inside them.

## Layering (§4 rules)

Import only from layers ≤ own, plus `crate::state`, `crate::command*`,
`crate::workspace_tools`, `crate::vision`, `crate::mascot`, `crate::perf_metrics`,
`crate::change_tracking`, `crate::handoff`, `crate::devtools`, `crate::command_jobs`,
`crate::project_scope`, and the root mcp.rs config readers.

- L0 (leaves): `jsonrpc`, `token_usage`, `text`, `clipboard`, `chrome`
- L1: `agents_state`, `resources`, `logs`, `flow`
- L2: `instruction`, `commands`, `file_tools`, `widget`, `settings`, `connector_notice`,
  `ngrok_setup`, `browser_select`
- L3: `tool_catalog`, `dashboard`
- Wiring hubs: root `mcp.rs`, root `main.rs`

No A↔B cycles anywhere.

## Stage map

Line numbers are against the pre-split files at base 3ce8af1 (mcp.rs 9338, main.rs 6637).

### Phase 0 — quick wins

| Stage | Commit | Content |
|-------|--------|---------|
| 0a | `refactor(mcp): move tests to src/mcp/tests.rs` | whole `mod tests` (mcp.rs 4397–9338) → `src/mcp/tests.rs`; root keeps `#[cfg(test)] mod tests;` |
| 0b | `refactor(tui): move tests to src/tests.rs` | whole `mod tests` (main.rs 2899–3992) → `src/tests.rs`; `mod test_serialization` stays in root |

### Phase 1 — MCP (root mcp.rs keeps: dispatch, `handle_tools_call_with_session`,
config readers, cfg(test) wrappers, gating, re-exports for server.rs)

| Stage | Module | Content (mcp.rs lines) |
|-------|--------|------------------------|
| M1 | `mcp/jsonrpc.rs` | JSON-RPC types 63–107 + response builders 2067–2209 (leaf) |
| M2 | `mcp/token_usage.rs` | `TokenUsage` 109–124 + o200k estimation 2749–2802 |
| M3 | `mcp/agents_state.rs` | metadata cache 2210–2451 (statics 1:1; hit/miss counters watched by tests) |
| M4 | `mcp/resources.rs` | consts 34–52 + discover/resources/widget HTML 269–505; only content exception: include paths get `../widget/` prefix |
| M5 | `mcp/instruction.rs` | 2137–2190 + 2448–2748 (depends on M3) |
| M6 | `mcp/commands.rs` | devtools forward + command handlers + move intercept + scope: 1414–2066 + 3665–3750 (~800 lines) |
| M7 | `mcp/file_tools.rs` | read/image/write/edit/search/delete + parsers 3798–4396 |
| M8 | `mcp/widget.rs` | enrichment 2803–3650 (~870 lines; 15+ tests) |
| M9 | `mcp/tool_catalog.rs` | schemas 508–808 + descriptors + tools_list 809–1176 — must land after M4 + M8 (tools_list → ensure_tool_descriptor_widget_template → current_widget_resource_uri_for_tool) |

### Phase 2 — TUI (root main.rs keeps: main/async_main, run_app, run_tui, run_prompt,
start_services, reserve_mcp_listener, drain_server_ui_events, macOS helpers,
browser-launch helpers, cadence consts)

| Stage | Module | Content (main.rs lines) |
|-------|--------|------------------------|
| T1 | `tui/mod.rs` + `tui/text.rs` | pure formatters 273–385 + 726–746 |
| T2 | `tui/logs.rs` | Selection/LogView/extract 91–185 + masking/zh-TW localization/wrap/export 386–725 (+ mask consts 77–81) |
| T3 | `tui/flow.rs` | 186–272 + 747–1230 |
| T4 | `tui/clipboard.rs` | 1231–1372 |
| T5 | `tui/chrome.rs` | draw_tui_header 2029–2060, draw_mode_select 2061–2226, render_toast 2851–2883, centered_rect 3993–4005 |
| T6 | `tui/connector_notice.rs` | 1673–2028 (3 snapshots) |
| T7 | `tui/ngrok_setup.rs` | 2227–2850 (~660 lines) |
| T8 | `tui/settings.rs` | 4061–4751 (~760 lines) |
| T9 | `tui/browser_select.rs` | 4752–5173 |
| T10 | `tui/dashboard.rs` | draw_ui 5950–6637 + layout consts 82–87 (8 snapshots) |

### Phase 3 — hygiene

| Stage | Commit | Content |
|-------|--------|---------|
| F1 | `chore: verify module split hygiene` | clippy --all-targets (record warnings, no pre-existing fixes), line counts, cycle audit, snapshot integrity |

Optional merges to bound review count: M1+M2, T4+T5, T6+T7. Stages M4, M8, M9, T10 are
never merged with others.

## Verification (GREEN gate, after every stage)

1. `cargo test --offline` — 487 passed, 0 failed.
2. `cargo build` — clean.
3. `git show --color-moved=dimmed-zebra <sha>` — residuum is only mod/use/visibility.

Commit bodies list the moved symbols and their old line ranges.

## Verification results (2026-09-26, branch tip)

Line counts (move plan -> outcome):

- `src/mcp.rs` 9338 -> 491 (target ~700); + `src/mcp/`: jsonrpc 135, token_usage 77,
  agents_state 249, resources 275, instruction 382, commands 794, file_tools 614,
  widget 864, tool_catalog 686, tests 4980.
- `src/main.rs` 6637 -> 1221 (target ~1400); + `src/tui/`: mod 27, text 142, logs 447,
  flow 587, clipboard 145, chrome 259, connector_notice 374, ngrok_setup 636,
  settings 705, browser_select 442, dashboard 718; `src/tests.rs` 1090.

Green gates: `cargo test --offline` 487/487 after every stage; final `cargo test --release`
487/487 (2.7 s); `cargo build --release` clean except the pre-existing
`devtools::DevtoolsBridge::from_child` never-used warning (present at base).

Clippy `--all-targets`: 106 lint warnings at base @3ce8af1, 106 at branch tip with an
identical category distribution (43 collapsed_if, 7 too-many-arguments(8/7), 6 div_ceil,
4 items_after_test_module, ...). Zero new lints introduced by the split; per plan none
were fixed.

Import graph: acyclic. mcp edges: jsonrpc<-token_usage<-{instruction,widget,resources},
agents_state<-instruction, commands->{file_tools->instruction}, widget->commands, and all
layers <- tool_catalog; root mcp.rs is the only hub (server.rs consumes only the planned
re-exports: MODERN_MCP_PROTOCOL_VERSION, decorate_modern_result, JsonRpc*, WIDGET_PAYLOAD_
META_KEY, is_catdesk_widget_resource_uri, handle_request_with_session,
estimate_turn_token_counts, agents_widget_state_payload). tui edges point strictly
downward (dashboard->flow/logs/chrome/text; ngrok_setup->browser_select/chrome/clipboard;
chrome/text are leaves); no A<->B pairs.

Test integrity: diff of `src/mcp/tests.rs` vs 0a (7e75548) and `src/tests.rs` vs 0b
(411aae6) contains import-path and `super::X` -> `crate::tui::X` re-qualifications only;
no assertion or test-body edits anywhere. The single planned content exception landed in
M4: include_str!/include_bytes! paths prefixed `../widget/` inside src/mcp/resources.rs.

Known flake note: one transient 486/487 run occurred during stage T5 verification
(fully serial `workspace_tools` deadline test under parallel load); 3 consecutive full
runs passed 487/487 right after, and the release run passed. Pre-existing timing
sensitivity, unrelated to the move (test bodies unchanged).
