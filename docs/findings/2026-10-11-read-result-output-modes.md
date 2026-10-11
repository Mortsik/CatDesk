# read_result output modes — measured format cost, 2026-10-11

Issue `catdesk-kd7` (P1). The live-measured problem: a 100267-byte UTF-8 range
yielded a ~234749-byte `read_result` tool response because `structuredContent`
always shipped the lossless `dataBase64` mirror **and**, for valid UTF-8, the
`text` mirror of the same bytes.

`read_result` now takes an optional `format` argument (`text`, `base64`,
`both`; default `both` keeps the exact legacy shape), so a client that only
needs one mirror stops paying for the duplicate.

## Measured serialized size (JSON-RPC `result`, bytes)

Pinned by `read_result_text_format_cuts_max_range_serialized_size` and
`read_result_text_format_shrinks_transcript_baseline_payload`
(`src/mcp/tests.rs`, handler fast path, `serde_json` default map = sorted keys).

| Payload | `both` (legacy) | `base64` | `text` |
| --- | ---: | ---: | ---: |
| 100267 B ASCII transcript line (field baseline) | 235602 | — | 101894 |
| 131072 B maximal ASCII range (`DEFAULT_MAX_RANGE_BYTES`) | 306226 | 175144 | 131446 |

- `text` mode: −57% vs legacy on UTF-8-safe ranges; the residual over the raw
  payload is metadata + JSON escaping headroom.
- `base64` mode: −43% on maximal ranges (drops the text mirror; base64 itself
  is a fixed 4/3 inflation and stays byte-exact).
- Legacy `both` matches the field report (235602 vs reported ~234749 — the
  delta is per-store metadata such as the random result id length).

## Latency / token footprint

Latency tracks the serialized size: the exempt-path token estimate is bytewise
(`serialized_len / 4`, see `turn_token_usage_fallback_stays_bounded_for_…`
tests in `src/server.rs`), so halving the bytes halves the estimate and removes
the duplicated-mirror BPE/quadratic hazards documented in the audit (F6) for
the mirrors that no longer ship. No new serialization path was introduced —
`text` mode reuses the store's existing `from_utf8` validation
(`src/result_store.rs`, `read_range`), so there is no extra pass over the bytes
compared with `both`.

## Failure semantics

`format=text` on a range that is not complete valid UTF-8 (binary payloads,
ranges ending mid-codepoint) returns a bounded structured error
(`errorCode: "text_range_not_utf8"`) instead of silently masking bytes;
`format=base64`/`both` on the same range stay byte-exact. `max_bytes`,
`nextOffset`, `eof`, `sizeBytes`, session scoping and expired-reference
semantics are untouched — the modes change only which mirrors are attached.
