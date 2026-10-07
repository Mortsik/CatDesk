# Deferred tool/schema exposure: evaluation on 2026-10-07

Evaluation for catdesk-ojt.8 (epic catdesk-ojt). Question: how much tool-schema
text does CatDesk inject eagerly per harness, which schemas are already
host-deferred, and can CatDesk reduce the eager footprint without duplicating
host-side deferred discovery?

## Method and reproducibility

- Numbers come from the live release binary, not from re-implementing the
  descriptors. `ops/measure-tool-schemas.sh` starts CatDesk headless (pty,
  throwaway HOME/workspace/port, `mode = "computer"`), issues a modern-MCP
  `tools/list`, and reports per-tool byte sizes plus a per-field breakdown.
  Command: `ops/measure-tool-schemas.sh --out /tmp/catdesk-ojt8`
  (binary/tag: main @ 96af79f, worktree catdesk-ojt-8).
- Token figures use the compact-JSON chars/4 heuristic. Real GPT-family
  tokenizers on schema-dense JSON land within roughly ±10–15% of that.
- The browser toolset was measured separately by speaking stdio JSON-RPC
  (`initialize` + `tools/list`) to `npx chrome-devtools-mcp@latest` directly;
  that set is version-floating and is labeled dynamic wherever it appears.
- Reproduction environment: CatDesk speaks exactly one protocol, modern MCP
  `2026-07-28`: every POST must carry `params._meta` with
  `io.modelcontextprotocol/protocolVersion` + `clientCapabilities` and a
  matching `MCP-Protocol-Version` header (`validate_modern_request`,
  src/server.rs:363); the GET SSE endpoint is disabled (`get_mcp` returns 405);
  there is no `initialize` handler (src/mcp.rs dispatch).

## Measured eager schema footprint

### Native tools, default tool mode (`toolMode = "multiTools"`), widget detail on

14 tools, **25,373 bytes ≈ 6,343 tokens** eager per `tools/list`
(`result` envelope beyond the tools array: 166 bytes).

| tool | bytes | ~tokens | desc | in | out | anno | meta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| run_command | 2,645 | 661 | 390 | 450 | 1,331 | 66 | 294 |
| read | 2,562 | 640 | 75 | 359 | 1,675 | 67 | 280 |
| search | 2,338 | 584 | 97 | 1,120 | 661 | 67 | 284 |
| create_handoff | 2,317 | 579 | 390 | 909 | 523 | 67 | 300 |
| poll_command | 2,311 | 578 | 573 | 451 | 807 | 68 | 296 |
| edit | 2,265 | 566 | 271 | 1,162 | 380 | 67 | 280 |
| start_command | 2,183 | 546 | 374 | 519 | 808 | 66 | 298 |
| search_result | 1,769 | 442 | 216 | 471 | 894 | 67 | 0 |
| cancel_command | 1,533 | 383 | 92 | 145 | 809 | 67 | 300 |
| read_image | 1,389 | 347 | 104 | 830 | 0 | 67 | 292 |
| read_result | 1,344 | 336 | 174 | 351 | 634 | 67 | 0 |
| write | 960 | 240 | 42 | 201 | 261 | 67 | 282 |
| catdesk_instruction | 887 | 222 | 136 | 33 | 208 | 67 | 310 |
| delete | 870 | 218 | 40 | 155 | 215 | 67 | 284 |
| **TOTAL** | **25,373** | **6,343** | 2,974 | 7,156 | 9,206 | 937 | 3,500 |

Field shares: outputSchema 36.3%, inputSchema 28.2%, widget `_meta` 13.8%
(twelve tools carry ≈292 B of `openai/outputTemplate`/`ui.resourceUri` each;
`read_result`/`search_result` carry none), descriptions 11.7%, annotations
3.7%. `read_image` intentionally has no outputSchema so ChatGPT keeps exposing
native image content (comment at src/mcp/tool_catalog.rs:25).

### Read-only tool mode (`toolMode = "readOnly"`)

7 tools, **12,606 bytes ≈ 3,152 tokens** (read, search, create_handoff,
search_result, read_image, read_result, catdesk_instruction). The command-job
trio and the write/delete trio are absent by construction
(`ToolMode::read_only`, src/state.rs:683).

### Browser/DevTools tools (dynamic passthrough)

`chrome-devtools-mcp@latest`, measured 2026-10-07: **30 tools, 26,357 bytes ≈
6,589 tokens**. Largest: emulate (1,724 B), list_console_messages (1,562),
evaluate_script (1,465), list_network_requests (1,289), navigate_page (1,225).
CatDesk forwards this set verbatim per `tools/list`
(`fetch_devtools_tools`, src/mcp/commands.rs:759); the count floats with the
upstream `@latest` tag (an earlier revision exposed 28 tools, hence the
"42-tool catalog" in the issue: 14 native + 28 DevTools).

### Combined default session (default config: `Mode::Both` + `MultiTools`)

**44 tools, 51,730 bytes ≈ 12,932 tokens** eager per conversation-bearing
`tools/list` — the DevTools passthrough is roughly half (51%) of the footprint
and is owned upstream, not by CatDesk.

For scale: ChatGPT Plus sessions budget 128K input context (README "Context
window" table), so the combined eager schema cost is ≈10% of a Plus input
window per conversation where the connector is active.

### Deferred, not eager: the operating guidance

`catdesk_instruction` returns 5,364 chars ≈ **1,341 tokens** of usage guidance
(measured via a real `tools/call`). It is paid once per session as a tool
result because every other tool call is gated on it
(src/mcp.rs:172 `catdesk_instruction_required_response`). This text is not
baked into tool descriptions.

## Per-harness: when schemas are sent, and can a server defer?

### ChatGPT Web Custom Connector (the primary, tested harness)

- `tools/list` is fetched at connector setup and on manual refresh, then
  cached by ChatGPT. CatDesk's own product flow encodes this: schema-changing
  releases bump `CURRENT_CHATGPT_CONNECTOR_REVISION` (currently 8,
  src/state.rs:83) and force a "remove CatDesk, add it again" modal
  (src/tui/connector_notice.rs:161). README (line 170) says the same for any
  MCP-setting change: start a new chat and refresh in ChatGPT settings; the
  most reliable way is remove + re-add.
- Community evidence agrees and adds a connector-details "refresh" control
  (r/MCPservers: "ChatGPT added full support for MCP tools"; refresh-unreliability
  reports in community.openai.com threads 1392201 and 1358796). Exact re-fetch
  timing and whether ChatGPT ever implements the 2026-07-28
  `subscriptions/listen` stream are **UNVERIFIED** — no public documentation
  found (OpenAI help-center article returned 403 to fetching).
- Consequence: the model can only call tools that were in the registry at the
  last connector refresh. A server cannot hand the model a new tool schema
  mid-conversation on this harness — deferred tool *exposure* is not
  collectable here at all. CatDesk declares `tools.listChanged: false` in
  `server/discover` (src/mcp/resources.rs:42), which is honest.
- What ChatGPT does pay eagerly: the cached schemas are injected into every
  conversation where the connector is active, i.e. the measured ~12.9k
  tokens/day-conversation cost above. ChatGPT consumes `outputSchema` for
  structured projection (in-repo evidence: the `read_image` exception comment),
  so shrinking output schemas changes model-visible behavior.

### Claude harnesses (Claude Code / claude.ai MCP)

- Whether Claude clients can connect to CatDesk today is **UNVERIFIED**:
  Claude Code has historically spoken the initialize-era protocol, while
  CatDesk implements only modern 2026-07-28 (no `initialize`, GET SSE
  disabled). A spec-compliant 2026-07-28 client probes with a modern request
  and reads the modern error bodies (spec "Backward Compatibility");
  initialize-era clients fall through to the disabled GET stream and fail.
- Host-side deferred loading already exists and is mature on this harness: the
  Anthropic Tool Search Tool defers tool definitions out of the context window
  (`defer_loading` per tool; for MCP, `default_config`/`configs` on the
  `mcp_toolset` entry), with reported reductions of ~46–85% of schema tokens
  (platform.claude.com tool-search-tool docs, fetched 2026-10-07; Anthropic
  engineering "Introducing advanced tool use"). Claude's own threshold
  guidance: use tool search at ≥10 tools or >10k schema tokens — CatDesk's
  combined catalog (44 tools, ~12.9k tokens) sits just past it; the native-only
  catalog (14 tools, ~6.3k) does not.
- Server-driven deferral would not work here either: Claude Code currently
  does not act on `notifications/tools/list_changed` mid-session (open issues
  anthropics/claude-code #62058 and #77314, retrieved 2026-10-07).

### Codex CLI (and other initialize-era clients)

- Codex supports stdio and Streamable-HTTP MCP servers and builds an "initial
  tool catalog" at session start (`mcp_optional_startup_grace_ms`); it "reads
  the MCP `instructions` field returned during initialization" — an
  initialize-era client (developers.openai.com/codex/mcp, fetched
  2026-10-07). No `tools/list_changed` handling is documented. Nothing is
  refetched mid-session.
- CatDesk's modern-only endpoint means Codex CLI cannot connect today; the
  eager question is moot until someone builds a protocol bridge. If bridged,
  schemas would be paid once per session start and no deferral mechanism
  exists on the client.

## What is already deferred by CatDesk (no host help needed)

1. Operating guidance (~1.3k tokens) is behind the mandatory
   `catdesk_instruction` call instead of baked into every descriptor.
2. Large tool results: the shared 64-KiB inline budget
   (`DEFAULT_INLINE_RESPONSE_BYTES`, src/mcp/response_budget.rs:6) externalizes
   oversized results into the result store; retrieval costs two small schemas
   bought once (`read_result` + `search_result` = 3,113 B ≈ 778 tokens) while
   unbounded output stays out of the transcript. This is output-side
   deferral — the epic's core primitive — and it is landed.
3. `run_command` interception of directory-listing commands replaces raw
   shell dumps with bounded structured listings.
4. Browser tools are fetched live from the devtools bridge on each
   `tools/list`; they are never stored, and in read-only tool mode they are
   filtered to `readOnlyHint` tools server-side.
5. `ToolMode::ReadOnly` halves the native set (7 vs 14 tools) — a deliberate,
   documented exposure profile, though README's counts are stale (see side
   findings).
6. `ShowDetailMode::Disable` drops the widget `_meta` block from every
   descriptor (src/mcp/widget.rs:118): −3,500 B ≈ −875 tokens of the native
   footprint, an existing user setting.

Nothing on the schema side is deferred today, and nothing can be on the only
harness that connects (see above).

## Recommendation: keep exposure as-is; no deferred-exposure follow-up

CatDesk should not implement deferred tool exposure now:

- On ChatGPT (the only working harness) the tool registry is frozen between
  manual connector refreshes; tools absent from the registry are uncallable,
  so "expose a search tool, load schemas on demand" cannot function. The
  schema bytes are also already near-minimal for what the model must decide:
  36% of the native footprint is outputSchema that ChatGPT projects results
  through.
- On Claude harnesses, host-native Tool Search already provides deferral, and
  CatDesk-side deferral would duplicate host discovery — precisely what the
  epic's design excludes ("prefer host-native deferred loading when already
  present"). Claude Code additionally ignores mid-session list-changed
  notifications, so a dynamic scheme would not be honored.
- Codex CLI cannot connect at all (protocol era), and builds its catalog once
  per session even if it could.

Measured ceiling of the alternatives, for the record:

| lever | saving (native, per conversation) | assessment |
| --- | --- | --- |
| `ShowDetailMode::Disable` (existing setting) | −3,500 B ≈ −875 tok (−13.8%) | already available; UI-only cost |
| deduplicate poll/abandon guidance repeated in run/start/poll/cancel descriptions and in the instruction text | ≤ ~700 B ≈ −175 tok (−2.8%) | marginal; descriptions should stay self-sufficient for tool selection |
| trim outputSchema descriptions/fields | bounded by 9,206 B ≈ −2,302 tok | high risk: ChatGPT projects structuredContent through outputSchema; spec makes conforming output a MUST when the schema is declared |
| merge start/poll/cancel into one `command_job` tool | ≈ −2 descriptors ≈ −3.8k B ≈ −960 tok | harms tool-selection ergonomics, breaks the widget attach list and annotations mapping, triggers the connector re-add dance |

Every schema-byte change on the ChatGPT harness costs a user-visible
remove-and-re-add connector round trip (revision gate), which dwarfs a
2–14% token saving. Not worth it at the current catalog size.

### Follow-up candidates worth recording (not implemented here)

1. **Docs fix (tiny):** README.md:201 says `multi-tools` exposes 12 and
   `read-only` 5 tools; measured reality is 14 and 7 (`read_result` and
   `search_result` were added later).
2. **Consider pinning the DevTools MCP version:** `chrome-devtools-mcp@latest`
  (src/devtools.rs:26) floats 51% of the combined eager footprint (28 → 30
  tools across recent versions). Pinning stabilizes the number and the ChatGPT
  refresh cadence, at the cost of upstream fixes — a product trade-off, not a
  token win.

## Side findings

- `ops/soak-real-duration.sh` is currently broken: it seeds
  `mode = "Computer"` / `toolMode = "MultiTools"`, but the config serde
  expects camelCase variants (`"computer"` / `"multiTools"`), and
  `load_from_path` aborts startup on the unknown variant (verified by running
  the binary against a seeded config). The measurement script seeds the
  lowercase forms.
- `cargo build --release` emits a dead-code warning for
  `DevtoolsBridge::from_child` (src/devtools.rs:158).

## UNVERIFIED / open questions

- Whether ChatGPT re-fetches `tools/list` at any point other than connector
  add/refresh, and whether it implements `subscriptions/listen` (no public
  documentation found; OpenAI help center blocked fetching).
- Whether Claude Code / Codex CLI now speak modern 2026-07-28 (both were
  initialize-era in the sources retrieved 2026-10-07). If a future client
  generation adopts 2026-07-28 broadly, the deferral question should be
  re-opened against `subscriptions/listen` support specifically.
- The DevTools toolset size drifts with `@latest` (28 and 30 tools observed in
  two environments on the same day); the combined "per harness" numbers
  involving browser mode are therefore ranges, not constants.

## Sources

- MCP spec 2025-06-18, Tools (listChanged semantics, outputSchema):
  https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- MCP spec 2026-07-28 changelog (stateless MCP, `server/discover`,
  `subscriptions/listen`, `ttlMs`/`cacheScope`, deterministic tool order):
  https://modelcontextprotocol.io/specification/2026-07-28/changelog
- MCP spec 2026-07-28, Streamable HTTP (GET stream removal, header/body
  validation, backward-compat probing):
  https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
- Anthropic Tool Search Tool docs (defer_loading, MCP `default_config`,
  thresholds, reported savings):
  https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool
- Anthropic engineering, "Introducing advanced tool use":
  https://www.anthropic.com/engineering/advanced-tool-use
- Claude Code list-changed issues:
  https://github.com/anthropics/claude-code/issues/62058,
  https://github.com/anthropics/claude-code/issues/77314
- Codex MCP configuration (transports, startup catalog, initialize
  instructions): https://developers.openai.com/codex/mcp
  (redirects to https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
- ChatGPT connector caching/refresh behavior (community):
  https://www.reddit.com/r/MCPservers/comments/1ndqn3f/chatgpt_added_full_support_for_mcp_tools_finally,
  https://community.openai.com/t/custom-mcp-tools-disappear-after-initial-successful-load-in-chatgpt-tool-visibility-session-issue/1392201,
  https://community.openai.com/t/chatgpt-only-uses-search-tool-in-mcp-server/1358796
- In-repo: src/mcp/tool_catalog.rs, src/mcp/resources.rs, src/mcp/widget.rs,
  src/mcp/instruction.rs, src/mcp/response_budget.rs, src/state.rs,
  src/tui/connector_notice.rs, src/server.rs, src/devtools.rs, README.md
  (all at 96af79f).
