# Deferred tool/schema exposure: evaluation on 2026-10-07

Evaluation for catdesk-ojt.8 (epic catdesk-ojt). Question: how much tool-schema
text CatDesk serializes into `tools/list` per harness, which schemas are
already host-deferred, and can CatDesk reduce that footprint without
duplicating host-side deferred discovery?

## Method and reproducibility

- Numbers come from the live release binary, not from re-implementing the
  descriptors. `ops/measure-tool-schemas.sh` starts CatDesk headless (pty,
  throwaway HOME/workspace/port, `mode = "computer"`), issues a modern-MCP
  `tools/list`, validates the response (a JSON-RPC error, a missing result, or
  an empty `result.tools` fails the run instead of reporting an empty
  catalog), and reports per-tool sizes plus a per-field breakdown. With
  `--out`, every profile leaves its `tools/list` response next to the
  summaries for later auditing: native profiles store the raw HTTP response,
  the DevTools profile stores the full `tools/list` JSON-RPC message as
  received (every field preserved, re-serialized). Command:
  `ops/measure-tool-schemas.sh --out /tmp/catdesk-ojt8-r5`
  (binary/tag: post-rebase main @ ca01902, worktree catdesk-ojt-8).
- Sizes are UTF-8 **bytes**; char counts are reported alongside because token
  figures use the compact-JSON chars/4 heuristic (a GPT-family tokenizer lands
  within roughly ±10–15% of it). An earlier revision of this document reported
  Python `len()` char counts as "bytes"; every catalog measured here is pure
  ASCII, so those earlier figures are numerically identical to the byte counts
  now — nothing below changed value, only labeling. Footprints are quoted as
  the sum of individually serialized tool objects; the serialized `tools`
  array adds bracket and comma bytes and is quoted separately wherever it
  matters (14 tools add 15 bytes, 30 tools add 31).
- The browser toolset CatDesk forwards verbatim is measured by the same
  script over stdio JSON-RPC. The probe sends the same `initialize`
  parameters and the same protocol sequence as CatDesk's bridge
  (`src/devtools.rs`: protocolVersion `2025-03-26`, clientInfo
  `catdesk-bridge/4.0.0`, then `notifications/initialized` + `tools/list`);
  request IDs are local to the probe, since the bridge rewrites them to
  fresh UUIDs on the wire, so ID equality is not part of the claim. The
  `initialize` response is validated (id-scoped, error-free, string
  `serverInfo.name`/`version`) and the negotiated protocol must equal the
  requested one (echoed back: `2025-03-26`). A second probe requests
  protocolVersion `2025-06-18`, must itself negotiate `2025-06-18`, and its
  catalog must be byte-identical to the first, so the direct measurement can
  stand in for what CatDesk forwards. Resolved package version on
  2026-10-07: 1.10.1. That set is version-floating.
- Driving CatDesk's full Both path (native + browser tools in one
  `tools/list`) headlessly requires a detected browser and a multi-step TUI
  wizard; in this environment the app did not reach a healthy server on that
  path. The Combined figure below is therefore labeled a **computed sum** of
  the measured MultiTools and DevTools profiles, never a single capture.
- Reproduction environment: CatDesk speaks exactly one protocol, modern MCP
  `2026-07-28`: every POST must carry `params._meta` with
  `io.modelcontextprotocol/protocolVersion` + `clientCapabilities` and a
  matching `MCP-Protocol-Version` header (`validate_modern_request`,
  src/server.rs:363); the GET SSE endpoint is disabled (`get_mcp` returns 405);
  there is no `initialize` handler (src/mcp.rs dispatch).

## Measured eager schema footprint

### Native tools, default tool mode (`toolMode = "multiTools"`), widget detail on

14 tools, **25,415 bytes ≈ 6,354 tokens** of tool objects per `tools/list`
(the serialized `tools` array is 25,430 bytes; the `result` envelope beyond
that array: 151 bytes).

| tool | bytes | ~tokens | desc | in | out | anno | meta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| run_command | 2,645 | 661 | 390 | 450 | 1,331 | 66 | 294 |
| read | 2,562 | 640 | 75 | 359 | 1,675 | 67 | 280 |
| search | 2,338 | 584 | 97 | 1,120 | 661 | 67 | 284 |
| create_handoff | 2,317 | 579 | 390 | 909 | 523 | 67 | 300 |
| poll_command | 2,311 | 578 | 573 | 451 | 807 | 68 | 296 |
| edit | 2,265 | 566 | 271 | 1,162 | 380 | 67 | 280 |
| start_command | 2,183 | 546 | 374 | 519 | 808 | 66 | 298 |
| search_result | 1,811 | 453 | 216 | 513 | 894 | 67 | 0 |
| cancel_command | 1,533 | 383 | 92 | 145 | 809 | 67 | 300 |
| read_image | 1,389 | 347 | 104 | 830 | 0 | 67 | 292 |
| read_result | 1,344 | 336 | 174 | 351 | 634 | 67 | 0 |
| write | 960 | 240 | 42 | 201 | 261 | 67 | 282 |
| catdesk_instruction | 887 | 222 | 136 | 33 | 208 | 67 | 310 |
| delete | 870 | 218 | 40 | 155 | 215 | 67 | 284 |
| **TOTAL** | **25,415** | **6,354** | 2,974 | 7,198 | 9,206 | 937 | 3,500 |

Field shares: outputSchema 36.2%, inputSchema 28.3%, widget `_meta` 13.8%
(twelve tools carry ≈292 B of `openai/outputTemplate`/`ui.resourceUri` each;
`read_result`/`search_result` carry none), descriptions 11.7%, annotations
3.7%. `read_image` intentionally has no outputSchema so ChatGPT keeps exposing
native image content (comment at src/mcp/tool_catalog.rs:25).

### Read-only tool mode (`toolMode = "readOnly"`)

7 tools, **12,648 bytes ≈ 3,162 tokens** of tool objects (serialized array:
12,656 bytes; envelope: 151 bytes) (read, search, create_handoff,
search_result, read_image, read_result, catdesk_instruction). The command-job
trio and the write/delete trio are absent by construction
(`ToolMode::read_only`, src/state.rs:683).

### Browser/DevTools tools (dynamic passthrough)

`chrome-devtools-mcp@latest`, measured by the script on 2026-10-07 (resolved
version **1.10.1**, negotiated protocol `2025-03-26` — the same handshake
CatDesk's bridge uses, with a `2025-06-18` probe returning a byte-identical
catalog): **30 tools, 26,357 bytes ≈ 6,589 tokens** of tool objects
(serialized array: 26,388 bytes). Largest:
emulate (1,724 B), list_console_messages (1,562), evaluate_script (1,465),
list_network_requests (1,289), navigate_page (1,225). CatDesk forwards this
set verbatim per `tools/list` (`fetch_devtools_tools`, src/mcp/commands.rs:759)
without adding widget `_meta` to it; the count floats with the upstream
`@latest` tag (an earlier revision exposed 28 tools, hence the "42-tool
catalog" in the issue: 14 native + 28 DevTools).

### Combined default session (default config: `Mode::Both` + `MultiTools`)

**44 tools, 51,772 bytes ≈ 12,943 tokens** as tool objects (**51,818 bytes**
serialized as arrays) — a **computed sum** of the two measured profiles
(MultiTools + DevTools), not a single `tools/list` capture (see Method). The
DevTools passthrough is roughly half (51%) of the footprint and is owned
upstream, not by CatDesk.

For scale: this serialized registry/schema footprint is an **upper bound** on
what any harness could place before the model; whether ChatGPT Web injects all
of it into model context every conversation is UNVERIFIED (below).

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
  found (OpenAI help-center article returned 403 to fetching), and no live
  experiment was run: verifying mid-session list changes would need a logged-in
  ChatGPT Plus session with the connector installed, which this evaluation
  deliberately does not operate.
- Working assumption, explicitly conditional: **if** the observed flow is the
  whole story (registry fixed between manual refreshes), then the model can
  only call tools present at the last refresh and a server cannot deliver a
  new schema mid-conversation — deferred exposure would be uncollectable
  here. That mechanism-absence is documented above, but the mid-conversation
  behavior itself is UNVERIFIED, so this harness's contribution to the
  recommendation is "no known delivery mechanism", not "impossible". CatDesk
  declares `tools.listChanged: false` in `server/discover`
  (src/mcp/resources.rs:42), which is consistent with everything observed.
- What ChatGPT actually places before the model per conversation is
  **UNVERIFIED**: no OpenAI documentation found (help center blocked fetching;
  the Responses-API MCP page does not cover ChatGPT web connectors) states
  whether the full cached schemas are injected into model context in every
  conversation where the connector is active, or a reduced representation.
  The measured 51,772 bytes ≈ 12,943 tokens is therefore a serialized
  registry/schema footprint and an upper bound on per-conversation schema
  cost, not a measured context injection. ChatGPT consumes `outputSchema` for
  structured projection regardless (in-repo evidence: the `read_image`
  exception comment), so output-schema changes alter model-visible behavior
  independent of that open question.

### Claude harnesses (Claude Code / claude.ai MCP)

- Whether Claude clients can connect to CatDesk today is **UNVERIFIED**:
  Claude Code has historically spoken the initialize-era protocol, while
  CatDesk implements only modern 2026-07-28 (no `initialize`, GET SSE
  disabled). A spec-compliant 2026-07-28 client probes with a modern request
  and reads the modern error bodies (spec "Backward Compatibility");
  initialize-era clients fall through to the disabled GET stream and fail.
- Host-side deferred loading is documented by the vendor for this harness: the
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

### Codex CLI — measured live, both protocol eras (2026-10-07, local 0.160.1)

First-party sources and a real connection test change the picture from the
first revision of this document:

- Feature flags (`codex features list`, codex-cli 0.160.1; definitions in
  openai/codex `codex-rs/features/src/lib.rs` on main, retrieved 2026-10-07):
  - `mcp_2026_07_28` — "Enable MCP protocol version 2026-07-28 support";
    stage `UnderDevelopment`, **default disabled**.
  - `tool_search` — removed, retained as a no-op "now that tool_search is
    always enabled".
  - `tool_search_always_defer_mcp_tools` — removed flag whose documented
    semantic is now the behavior: "**MCP tools are always deferred when
    tool_search is available**" (effective true locally).
  - So current Codex ships host-side MCP tool deferral out of the box, and
    modern-protocol support exists but is opt-in.
- Live connection test against the measured CatDesk build (isolated
  `CODEX_HOME`, `[mcp_servers.catdesk]` pointed at the local headless server,
  verified from CatDesk's `connections.jsonl`):
  - Default flags: the client probed `GET` on the MCP endpoint → **405**,
    retried `GET` → **405**, then fell back to the initialize-era path and
    posted `initialize` → **400** (CatDesk has no `initialize`). No connection.
  - With `features.mcp_2026_07_28=true`: `server/discover` → **200** and
    `tools/list` → **200** — CatDesk's modern-only endpoint is fully
    consumable by Codex when the flag is on.
  - Caveat: both `exec` runs after the first failed at the OpenAI API layer
    (401 from the isolated auth copy), so the end-to-end "model sees deferred
    CatDesk tools" step was not exercised; the MCP-layer results above are
    from the server-side logs. Sourced-but-not-end-to-end-verified for the
    deferral behavior itself.
- Net effect for the deferral question: with the experimental flag enabled,
  the vendor's documented behavior is that CatDesk's schemas would be deferred
  host-side (tool search) — the deferred discovery of the CatDesk catalog
  itself was not exercised end-to-end (the test runs failed at the OpenAI API
  layer before the model responded); with default flags the measured outcome
  is no CatDesk tools at all, not eager ones. Either way CatDesk-side
  deferral has nothing to add on the evidence gathered.

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

Nothing on the schema side is deferred by CatDesk itself. Delivery mechanisms
for server-driven deferral are known on none of the three harnesses: ChatGPT's
mid-conversation behavior is UNVERIFIED with no documented update channel;
Claude's and Codex's host-side deferral is documented by their vendors but
was not exercised end-to-end against CatDesk (see below for what exactly was
measured).

Host-side deferred loading, as documented by each vendor, would cover the
harnesses that could speak to CatDesk:

- **Claude** harnesses (documentation only — Claude connectivity to CatDesk is
  UNVERIFIED): Anthropic's Tool Search Tool defers tool definitions
  out of the context window (`defer_loading` per tool; for MCP,
  `default_config`/`configs` on the `mcp_toolset` entry), with reported
  reductions of ~46–85% of schema tokens (platform.claude.com tool-search-tool
  docs, fetched 2026-10-07). Claude's own threshold guidance: use tool search
  at ≥10 tools or >10k schema tokens — the combined CatDesk catalog (44 tools,
  ~12.9k tokens) sits just past it; the native-only catalog (14 tools, ~6.3k)
  does not.
- **Codex CLI** (feature flags and connection measured; deferred discovery
  itself not exercised — see the Codex section): tool search is always
  enabled per first-party feature definitions ("MCP tools are always deferred
  when tool_search is available", retrieved 2026-10-07).

## Recommendation: keep exposure as-is for now; no deferred-exposure follow-up today

CatDesk should not implement deferred tool exposure now, and the verdict is
stated with its evidence base per harness:

- **ChatGPT** (the primary, working harness): **on the working assumption**
  that the registry is fixed between manual connector refreshes (observed
  flow; mid-conversation behavior UNVERIFIED), tools absent from the registry
  are uncallable and "expose a search tool, load schemas on demand" has no
  delivery path — no ChatGPT-side subscription/list-change consumer is
  documented anywhere this evaluation could reach. The schema bytes are also
  already near-minimal for what the model must decide: 36% of the native
  footprint is outputSchema that ChatGPT projects results through.
- **Claude** (documentation only): host-native Tool Search is documented to
  provide deferral, and CatDesk-side deferral would duplicate host discovery —
  precisely what the epic's design excludes ("prefer host-native deferred
  loading when already present"). Claude Code additionally reportedly ignores
  mid-session list-changed notifications (open issues #62058, #77314), so a
  dynamic scheme would not be honored there.
- **Codex** (connection measured at the MCP layer; deferral documented, not
  exercised end-to-end): with default flags there are no CatDesk tools to
  defer (modern-only server, measured); with the experimental 2026-07-28 flag
  the connection succeeds and the vendor's own documentation says MCP tools
  are deferred host-side when tool search is available. Building CatDesk-side
  deferral would again duplicate the host.

So the recommendation rests on: one harness with no known delivery mechanism
(ChatGPT; flow documented, mid-conversation behavior UNVERIFIED) and two
harnesses whose vendors document host-side deferral (Claude documentation
only; Codex connection measured to CatDesk at the MCP layer, deferred
discovery not exercised end-to-end). Re-open this evaluation if any of
the following lands: ChatGPT documenting or demonstrably honoring mid-session
tool-list updates (`subscriptions/listen`), a CatDesk-connected harness
without host-side deferral, or the native catalog growing past the point
where description/outputSchema trimming alone keeps it honest.

Measured ceiling of the alternatives, for the record:

| lever | saving (vs native eager footprint) | assessment |
| --- | --- | --- |
| `ShowDetailMode::Disable` (existing setting) | −3,500 B ≈ −875 tok (−13.8%) | already available; UI-only cost |
| deduplicate poll/abandon guidance repeated in run/start/poll/cancel descriptions and in the instruction text | ≤ ~700 B ≈ −175 tok (−2.8%) | marginal; descriptions should stay self-sufficient for tool selection |
| trim outputSchema descriptions/fields | bounded by 9,206 B ≈ −2,302 tok | high risk: ChatGPT projects structuredContent through outputSchema; spec makes conforming output a MUST when the schema is declared |
| merge start/poll/cancel into one `command_job` tool | ≈ −2 descriptors ≈ −3.8k B ≈ −960 tok | harms tool-selection ergonomics, breaks the widget attach list and annotations mapping, triggers the connector re-add dance |

The refresh cost on the ChatGPT harness is per-case, not uniform: the
in-repo guidance (comment at `CURRENT_CHATGPT_CONNECTOR_REVISION`,
src/state.rs:83) is to bump the revision — and thereby force the
remove-and-re-add modal — only for releases whose visible behavior actually
changes; smaller text-only edits ride the settings refresh, which README and
community reports describe as less reliable. Any of the descriptor edits
above would be a visible schema change, so the honest planning assumption
for them is the re-add round trip, which dwarfs a 2–14% token saving. Not
worth it at the current catalog size.

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

- Whether ChatGPT injects the full cached tool schemas into model context in
  every conversation where the connector is active, or a reduced
  representation; and whether it re-fetches `tools/list` at any point other
  than connector add/refresh, or implements `subscriptions/listen`. No public
  documentation found (OpenAI help center blocked fetching; the Responses-API
  MCP docs do not cover ChatGPT web connectors), and no live experiment was
  run — confirming or refuting mid-session list changes would need a
  logged-in ChatGPT Plus session with the connector installed, which this
  evaluation deliberately does not operate. All per-conversation token
  costs in this document are therefore upper bounds derived from the measured
  serialized registry.
- Whether Claude Code now speaks modern 2026-07-28 (it was initialize-era in
  the sources retrieved 2026-10-07). If a future client generation adopts
  2026-07-28 broadly, the deferral question should be re-opened against
  `subscriptions/listen` support specifically.
- Codex end-to-end deferral of the CatDesk catalog specifically: the MCP-layer
  connection with `mcp_2026_07_28=true` is verified (discover/list 200), but
  the final "model discovers deferred CatDesk tools via tool search" step was
  not exercised (OpenAI API 401 during the isolated-auth test runs).
- The DevTools toolset size drifts with `@latest` (28 and 30 tools observed in
  two environments on the same day; 1.10.1 resolved during the scripted run);
  the combined numbers involving browser mode are therefore ranges, not
  constants.

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
- openai/codex feature definitions (`mcp_2026_07_28`, `tool_search` no-op,
  `tool_search_always_defer_mcp_tools` semantics):
  https://github.com/openai/codex/blob/main/codex-rs/features/src/lib.rs
- openai/codex app-server README (`mcp_2026_07_28` flag behavior):
  https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- OpenAI Responses API MCP docs (allowed_tools; does not cover ChatGPT web
  connectors): https://developers.openai.com/api/docs/mcp
- Local verification: codex-cli 0.160.1 (`codex features list`, `codex exec`
  against a local headless CatDesk), 2026-10-07.
- ChatGPT connector caching/refresh behavior (community):
  https://www.reddit.com/r/MCPservers/comments/1ndqn3f/chatgpt_added_full_support_for_mcp_tools_finally,
  https://community.openai.com/t/custom-mcp-tools-disappear-after-initial-successful-load-in-chatgpt-tool-visibility-session-issue/1392201,
  https://community.openai.com/t/chatgpt-only-uses-search-tool-in-mcp-server/1358796
- In-repo: src/mcp/tool_catalog.rs, src/mcp/resources.rs, src/mcp/widget.rs,
  src/mcp/instruction.rs, src/mcp/response_budget.rs, src/state.rs,
  src/tui/connector_notice.rs, src/server.rs, src/devtools.rs, README.md
  (all at 96af79f).
