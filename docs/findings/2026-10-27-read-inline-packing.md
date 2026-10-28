# Read inline packing — 2026-10-27

## Problem

`read` went through the shared 64 KiB response budget (`2b6b7f7e`). Any reply
over ~60 KiB serialized was externalized, and the preview cut **every** text
field to a 4 KiB head+tail — whatever the file size and however much of the
budget was left. Upstream (`Xeift/CatDesk`) has no response budget: it
returns up to 512 KiB per file / per batch inline and truncates the rest.

Measured on `62f63ed7` (realistic Rust source lines, through
`handle_tools_call`):

| case | upstream | fork before | fork after |
|---|---|---|---|
| file ≤ 56 KiB | whole, 1 call | whole, 1 call | whole, 1 call |
| file 64 / 100 KiB | whole, 1 call | 4 KiB preview + outputRef | whole, 1 call (70 / 110 KB inline) |
| file 200 KiB | whole, 1 call | 4 KiB preview | 2 calls, whole, line-addressed |
| file 500 KiB | 512 KiB prefix | 4 KiB preview | 5 calls, whole |
| batch 8 × 10 KiB | all whole | **each cut to 4 KiB** (38 KB inline) | all whole (89 KB inline) |
| batch 20 × 10 KiB | all whole | each cut to 4 KiB | 11 whole + 1 cut at a line + 8 named as not returned |

Recovering a 100 KiB file through `read_result` cost 269 KB with the default
`format=both` (122 KB with `format=text`), as double-escaped JSON without line
numbers. Production logs (2026-10-12..27): 32 of 1072 reads externalized,
27 of them under 512 KiB; `read_result` followed only a handful of them.

## Change

- `read` packs its own inline budget: `DEFAULT_READ_INLINE_BYTES` = 128 KiB
  (384 KiB since 2026-10-28, see the measurement below) of
  JSON-escaped text (`src/workspace_tools.rs`, `pack_inline`). Files that fit
  come back whole, smallest first; the next ones are cut at a line boundary in
  request order with `startLine`/`endLine`, `truncated` and `budgetTruncated`
  set, and `batchTruncated` on the batch. Files left with no room report no
  text and no bounds.
- `structuredContent.retrievalAdvice` (already declared in every budgetable
  outputSchema, so ChatGPT's projection keeps it) names each continuation:
  `{"path": ..., "start_line": endLine + 1}`.
- A cut ranged request keeps its own end in the advice
  (`{"path": ..., "start_line": endLine + 1, "end_line": <requested>}`).
- The dispatcher gate uses a per-tool ceiling (`inline_response_limit`): the
  read budget + 32 KiB for metadata, reported as `responseBudget.limitBytes`.
  It remains the backstop for the two cases the packer deliberately leaves
  whole: agent guidance files (never cut, per the 2026-10-18 requirement) and
  a single line wider than the whole budget (no line range can address it).
  When either forces a batch past the budget, nothing in that batch is cut:
  the store keeps the complete reply for `read_result`, and the preview's
  `retrievalAdvice` always leads with the read_result warning (a tool's own
  advice is appended, never substituted).
- The 32 MiB per-call safety ceiling still rejects oversized raw reads with a
  per-file error; the descriptions say so. Bounding the raw read by the inline
  budget (so a 20 MiB file is not read whole just to return 128 KiB) is a
  possible follow-up.
- `read_usage` gains `budget_cut_files`; full vs ranged files are classified by
  the request, not by the reported bounds.
- `CURRENT_CHATGPT_CONNECTOR_REVISION` 9 → 10: no new fields, but the cached
  descriptions said `budgetTruncated` was always false and oversized reads
  errored — a model trusting them would take cut text as complete.

## Choosing the budget: measure the host

Why not simply upstream's 512 KiB: hosts cut tool results on their own and
silently — Codex code_mode truncated regular results to a few KB in
`2026-10-08-long-session-harness-benchmark.md` — and every inline byte stays
in the transcript for the rest of the conversation (~32k tokens at 128 KiB).
The ChatGPT connector limit has not been measured yet. To measure:

```bash
ops/read-inline-probe.sh ~/catdesk-probe          # 64..512 KiB probe files
CATDESK_READ_INLINE_BYTES=1048576 catdesk ...     # server never cuts
```

Ask the host to read each probe in one call and report the last line it sees,
the `END-OF-PROBE` token and `budgetTruncated`. A last line earlier than the
script's expected line with `budgetTruncated=false` is the host's own cut. If
ChatGPT passes 256 KiB intact, raising `DEFAULT_READ_INLINE_BYTES` is a
one-constant change (consider the transcript cost first, or key it on
`clientInfo`, which the server already sees per request).

### Measurement (2026-10-28, ChatGPT web connector)

Run with `CATDESK_READ_INLINE_BYTES=1048576`, one fresh conversation per probe:

| probe | bytes | last line seen | END token | `budgetTruncated` |
|---|---|---|---|---|
| 256k | 262 119 | L004096 (expected) | yes | false |
| 384k | 393 191 | L006144 (expected) | yes | false |
| 512k | 524 263 | L008192 (expected) | yes | false |

For 512k the model also listed L004000–L004100 with no gaps, so the host
neither cut the tail nor elided the middle. ChatGPT therefore passes at least
512 KiB of `read` text. `DEFAULT_READ_INLINE_BYTES` is now 384 KiB: a margin
under the measured ceiling, and a cap on the transcript cost of one read
(~96k tokens) — larger files continue through ranged reads. Codex was not
re-measured; its code_mode cut regular results to a few KB already at 128 KiB.
