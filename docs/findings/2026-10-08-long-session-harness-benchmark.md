# Long-session harness payload benchmark: 2026-10-08

Benchmark for catdesk-ojt.10 (epic catdesk-ojt). Question: what does the same
representative long-session workload cost per harness — instruction bytes,
exposed schema bytes, inline tool-result bytes, externalized bytes,
retrieval-call count, and wall-clock overhead — on ONE server revision, with
host overhead separated from server payload?

## Method and provenance

- **Server revision:** `main` @ 266dd05 (worktree
  `catdesk-ojt-10`, branch `feat/catdesk-ojt-10-harness-benchmark`); the live
  binary's own build string confirms it: `0.9.2+g266dd05
  feat/catdesk-ojt-10-harness-benchmark 2026-10-08` (recorded per run in the
  server's diagnostics log).
- **Fixture:** the same five workflow families the ojt.9 transcript gate
  pins (src/mcp/e2e_transcript_gate.rs), byte-identical generators: (a)
  `run_command` emitting 2 × 24,000 filler lines (~1.4 MB stdout+stderr), (b)
  a 17,000-line file (`gate/big.txt`, ~494 KB) + needle search + full
  read-back, (c) a large DevTools network body, (d) a failing command whose
  diagnostic must stay visible, (e) repeated polling of a `printf` job.
- **Server-side measurement (deterministic, 1:1):**
  `ops/measure-harness-payload.sh` starts the release binary headless (pty,
  throwaway HOME/workspace/port, `mode = "computer"`), then drives the
  fixture through the REAL HTTP transport — the exact JSON-RPC payloads a
  harness receives (`POST /bench/mcp` with the 2026-07-28 `_meta` block and
  `Mcp-Method`/`Mcp-Name` headers). Every step records the serialized JSON-RPC
  result bytes, the full HTTP body bytes, wall-clock seconds around the POST,
  and `responseBudget.originalBytes`/`outputRef`; every externalized result
  is pulled back through `read_result` until EOF, counting retrieval calls
  and retrieved bytes (byte-for-byte asserts, same as the gate). The mascot
  seed is pinned (`partnerBinagotchySeed`) so the `catdesk_instruction`
  widget payload is deterministic across runs — unpinned, it drifts with
  `rand::random::<u64>()` (src/state.rs:1097). Byte figures are identical
  across three repeat runs; wall-clock is quoted as the observed range.
- **Harness-side measurement (live runs):** `ops/run-codex-harness.sh` and
  `ops/run-claude-harness.sh` start the same release binary and drive a REAL
  headless harness session against it over the same endpoint, each with an
  isolated harness home and the real client credentials copied in. Codex:
  `codex exec --enable mcp_2026_07_28` (codex-cli 0.161.0, ChatGPT auth;
  approvals bypassed — the first live attempt measured that `approval_policy
  = never` rejects every MCP tool call). Claude Code: `claude -p
  --output-format json` (2.1.270). One run per harness; artifacts
  (events/session transcripts, server connections.jsonl) land in the --out
  directory.
- Workflow (c) needs a live DevTools bridge; its footprint comes from the
  ojt.9 gate run on this same revision (fake peer, same handler the HTTP
  path calls — only the transport differs), not from the HTTP script.
- **Environment:** WSL2 (Linux 6.18.40.1-microsoft-standard-WSL2), release
  build, 2026-10-08. Wall-clock is single-run, shared-machine — treat as
  order-of-magnitude, not absolute.
- Sizes are UTF-8 bytes; tokens use the compact-JSON chars/4 heuristic
  except where a harness reports real token counts.

## Server payload, same fixture, per tool-mode profile

### MultiTools (`toolMode = "multiTools"`, default)

Schema + instruction (paid once per session):

| item | bytes | ~tokens |
| --- | ---: | ---: |
| `tools/list` — 14 tool objects | 25,348 | 6,337 |
| `tools/list` — serialized array | 25,363 | — |
| `tools/list` — result envelope | 151 | — |
| `catdesk_instruction` — instruction text | 3,451 | 863 |
| `catdesk_instruction` — full result (incl. widget `_meta`) | 12,278 | — |

Per-step fixture payload over HTTP (3 runs; bytes identical every run, walls
across runs):

| step | inline result B | HTTP body B | raw (externalized) B | reduction | read_result calls | wall s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| (a) run_command large | 15,191 | 15,231 | 1,397,651 | 98.9% | 11 | 0.171–0.262 |
| (b) write ack | 2,758 | 2,798 | — | — | — | 0.004–0.034 |
| (b) needle search | 1,366 | 1,406 | — | — | — | 0.009–0.038 |
| (b) read big.txt | 6,175 | 6,215 | 493,988 | 98.7% | 4 | 0.003–0.007 |
| (d) failing command | 1,424–1,426 | 1,464–1,466 | — | — | — | 0.099–0.153 |
| (e) polling | max 1,436–1,439 / poll | — | — | — | — | 0.091–0.150 |

Poll counts at `wait_ms = 0` were 87, 96 and 100 across the three runs
(timing-dependent — every answer stayed inside the inline budget in all
runs; per-poll bytes are stable). Sum of all poll answers: 123,571–142,048 B.

Aggregates (acceptance-criteria view):

| metric | value |
| --- | ---: |
| externalized original bytes (a + b-read) | 1,891,639 |
| externalized inline bytes (previews) | 21,366 |
| externalization reduction | 98.9% |
| retrieval calls (read_result) | 15 |
| retrieved bytes (byte-for-byte reconstructed) | 1,891,639 |
| fixture wall-clock total (sum of steps, 3 runs) | 0.44–0.67 s |

For scale: inline tool-result bytes for the whole fixture (bounded steps +
all poll answers) were 129,121–147,598 B depending on the poll-count drift,
while the two externalizing results alone carried 1,891,639 B — the transcript
paid ~0.11 per 10 for them (21,366 B) and bought the rest back on demand in
15 small calls.

### ReadOnly (`toolMode = "readOnly"`)

| item | bytes |
| --- | ---: |
| `tools/list` — 7 tool objects | 12,581 |
| `catdesk_instruction` — instruction text | 2,281 |
| `catdesk_instruction` — full result | 11,081 |
| (b) needle search result | 1,355 |
| (b) read big.txt inline / raw | 6,166 / 493,988 (98.8%) |
| (b) read retrieval | 4 calls, 493,988 B byte-for-byte |
| fixture wall-clock total | 0.028 s |

Not applicable by construction (server answers `isError` "Tool run_command
is disabled in read-only mode" — measured, not assumed): (a), (d), (e) have
no command tools in ReadOnly; (b)-write has no write tool, so the read/search
subset ran over a file prepared directly in the workspace with byte-identical
content.

### Workflow (c): DevTools body (gate measurement, fake peer)

From `cargo test transcript_gate -- --nocapture` on this revision:

| row | raw B | inline B | reduction |
| --- | ---: | ---: | ---: |
| snapshot + listing (input bounds) | 0 | 74 | — |
| large network body | 131,141 | 4,744 | 96.4% |

(For reference the same gate run measured (a) 1,392,623 → 9,770 B and (b)
read 493,378 → 5,166 B — consistent with the HTTP figures above; the small
deltas are transport decorations server.rs adds after the gate's
accounting point.)

## Harness-side: live runs

### Codex CLI 0.161.0 — connected, measured live

Setup that connected (repeatable): isolated `CODEX_HOME` with
`features.mcp_2026_07_28 = true` and
`[mcp_servers.catdesk] url = "http://127.0.0.1:<port>/bench/mcp"`. Server
log: `server/discover` then `tools/list` — both answered; 14 tools exposed.
With default flags there is no connection at all (initialize-era fallback →
400; measured again in this environment, matching ojt.8).

Schema cost, host-side: Codex did NOT present the 25,348 B (≈6,337 token)
JSON catalog as-is. It runs MCP tools through code_mode (JS interpreter;
`code_mode` stable/true in 0.161.0) with tools declared as TypeScript, and
the model's first act was to list the catdesk declarations
(`ALL_TOOLS.filter(...)`). That listing came back truncated by the host with
"original token count: 13,280" — the TS-declaration presentation is ~2.1×
the raw JSON schema tokens. The full catalog fetch happened server-side
(tools/list 200), so the 25,348 B is what Codex pulled; what the model
context carries is the host's larger TS rendering.

Tool-result cost and the load-bearing finding: the model walked the fixture
in 7 code_mode tool invocations. When the model introspected the raw result
(`Object.keys(r)`), it got exactly `["content","structuredContent"]` —
**Codex's code_mode strips `responseBudget` (and widget `_meta`) from MCP
tool results.** Consequences, all measured in the transcript:

- The server DID externalize the 1.4 MB run_command output (the model saw a
  4,092-char `structuredContent.stdout` preview), but `stdoutTruncated` was
  `false` and no `outputRef` reached the model, so the preview is
  indistinguishable from the whole result.
- `read_result` was structurally unreachable: 0 retrieval calls, not because
  the model declined but because the result id never arrived. The model's
  own final summary: "Both raw results lacked `responseBudget`, so neither
  supplied `outputRef`".
- Transcript cost of tool results: 7 outputs, 46,929 B total (largest single
  output 41,520 B — the tool-catalog listing; regular tool results were
  host-truncated to a few KB).

Token usage (reported by Codex, real counts): 8 model requests; per-request
input grew 14,540 → 26,533 tokens (transcript accumulation), total 195,732
input tokens of which 166,912 were cache reads, 686 output tokens. Wall
clock 36 s (model + tools; the server-side cost of the same fixture steps
was ~0.4 s — the 90× difference is model/host, not server payload).

Verdict for this harness: server-side externalization saves the model
transcript (the 1.4 MB payload never entered it — host truncation would
have bitten at ~4 KB anyway), but the retrieval primitive is DEAD on Codex
0.161.0 code_mode: the harness drops the exact fields (`responseBudget`)
that carry the retrieval address. Capability loss is host-side, not
server-side (the same revision reconstructs byte-for-byte over plain HTTP).

### Claude Code 2.1.270 — no connection, measured live

Single request in the server log: `initialize` → 400 ("modern MCP requests
require params._meta"). Claude Code speaks the initialize-era protocol;
CatDesk implements modern MCP 2026-07-28 only (no `initialize` handler, GET
SSE disabled). The client surfaced the 400 body verbatim to the model:
"catdesk (400): Streamable HTTP error …". The model (routed via a
Claude-compatible proxy to `glm-5.3`) correctly reported the connector as
failed and did not fall back to local shell (the prompt forbade it).

Session cost with zero CatDesk tools available: 15,578 input tokens +
640 cached-read + 1,031 output tokens, wall 31 s — the price of a failed
attach attempt is already ≈ half of CatDesk's entire eager native schema
(6,337 tokens) and it buys nothing.

This upgrades ojt.8's "Claude Code connectivity: UNVERIFIED" to measured:
2.1.270 cannot consume CatDesk today. No schema, instruction, or tool-result
payload reached this harness at all.

### ChatGPT Web connector — documented, not live-tested (unchanged from ojt.8)

No logged-in ChatGPT Plus session was operated here, same as ojt.8. The
flow remains: schemas fetched at connector add/refresh, cached between
manual refreshes; per-conversation context injection UNVERIFIED with no
public documentation. Everything measured above is server payload — an
upper bound on what ChatGPT could place before the model.

## Host overhead vs server payload (the split the bead asks for)

| layer | measured | where |
| --- | ---: | --- |
| Server schema payload (native, MultiTools) | 25,348 B ≈ 6,337 tok / tools/list | this benchmark, HTTP |
| Server schema payload (ReadOnly) | 12,581 B ≈ 3,145 tok | this benchmark, HTTP |
| Server instruction (paid once) | 3,451 B (MultiTools) / 2,281 B (ReadOnly) | this benchmark, HTTP |
| Server tool results, whole fixture | 21,366 B inline for 1,891,639 B raw (98.9% externalized) | this benchmark, HTTP |
| Codex schema presentation | 13,280 tokens (TS declarations) vs 6,337 tokens JSON ≈ 2.1× | Codex transcript |
| Codex result transport | strips `responseBudget`/`_meta`; truncates outputs (41,520 B listing cap) | Codex transcript |
| Codex session total | 195,732 input tok (166,912 cached), 686 out, 36 s | Codex usage |
| Claude attach failure | 15,578 input + 1,031 output tok, 0 tools | Claude usage |

## Limitations

- Wall-clock numbers are single-machine, single-run, under a loaded
  multi-agent host; bytes are exact and repeated, seconds are indicative.
- Poll-count drift at `wait_ms = 0` (87–100 across runs; 167 in the debug
  gate) is scheduler timing, not payload variance — every poll answer was
  bounded in every run.
- One live run per harness. The Codex findings (stripped `responseBudget`,
  TS-declaration inflation, host truncation caps) are single-run
  observations of stable host behavior — deterministic code paths, not
  sampling noise, but they were not re-run N times.
- Workflow (c) numbers come from the gate's fake DevTools peer, not a real
  chrome-devtools-mcp bridge; no browser was attached.
- Codex ran with approvals bypassed (`--dangerously-bypass-approvals-and-sandbox`);
  the first attempt measured that default `exec` approval policy rejects
  every MCP tool call ("MCP tool call requires approval, but approval policy
  is never") — worth knowing before trusting any headless Codex MCP numbers.
- The DevTools passthrough (`chrome-devtools-mcp@latest`) was not part of
  this benchmark's live path; its schema footprint drift is documented in
  the ojt.8 evaluation.

## Follow-up candidates (evidence-backed; for the orchestrator to decide — none created)

1. **Retrieval hint outside `responseBudget` for stripping hosts** — Codex
   0.161.0 code_mode removes `responseBudget` from MCP results, which makes
   `read_result` unreachable there (0 retrieval calls on a 1.4 MB result
   while the same revision reconstructs byte-for-byte over plain HTTP). If
   large-result discoverability matters on that harness, the address would
   have to ride in a field the host preserves (`structuredContent` or
   content text). Product decision first: Codex may be out of scope for
   CatDesk's retrieval contract.
2. **`tool_result_bytes` diagnostics never fire for tools/call** — handlers
   run on `spawn_blocking` (src/request_workers.rs:98) outside the
   `REQUEST` task-local scope that `diagnostics::tool_result_bytes`
   (src/diagnostics.rs:1745) writes into, so the per-call byte records that
   `ops/tool-result-bytes.sh` (bead catdesk-ojt.6) reports on are silently
   dropped: this run's connections.jsonl has 6 `mcp_request` records and 0
   `tool_result_bytes` records. The in-memory registry still accounts
   bytes; only the persisted per-call record path is dead. This looks like a
   telemetry regression worth its own bead.
3. **Headless Codex approval policy** — `codex exec` rejects all MCP tool
   calls without an approvals bypass; if Codex becomes a supported harness,
   the documented invocation needs `--dangerously-bypass-approvals-and-sandbox`
   (or a trust mechanism). Documentation-shaped, not code.

## Verification on this branch

- `cargo test -j 8`: 635 passed, 0 failed (one first-run failure of
  `mcp::tests::read_result_max_range_token_estimate_stays_bounded` re-ran
  green solo and on a second full-suite pass — the known flake family,
  catdesk-d94).
- `cargo fmt` clean (run before commit).
- Server-side benchmark: `ops/measure-harness-payload.sh` — byte-identical
  results across 3 MultiTools runs + 1 ReadOnly run.
- Harness runs: artifacts under /tmp/catdesk-ojt10-codex and
  /tmp/catdesk-ojt10-claude (session transcripts, events, server
  connections.jsonl); runners are committed and repeatable.
