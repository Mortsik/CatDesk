# Lossless Large-Result Store Design

**Bead:** `catdesk-ojt.1`  
**Date:** 2026-10-07

## Goal

Provide a bounded CatDesk-owned store for tool payloads that are too large to inline, without losing any bytes. Later response-compaction work can return a compact preview plus an opaque result reference while an agent can still reconstruct or search the complete payload through bounded MCP calls.

## Contract

- Stored payloads are complete bytes. Storage never truncates an accepted entry.
- Public references are opaque IDs; no host path is exposed.
- Every entry is bound to both the originating MCP session namespace and workspace root. A caller from another session or workspace cannot distinguish an existing foreign entry from an unknown reference.
- Retrieval is byte-addressed and bounded. Every range response reports the returned byte count, `nextOffset`, and `eof` so repeated calls deterministically reconstruct the original payload.
- Every range includes `dataBase64`; UTF-8 entries may additionally expose `text` when the selected byte range is valid UTF-8. The optional `format` argument (`text`, `base64`, `both`; default `both`) drops one mirror per response: `text` ships only the lossless UTF-8 mirror and fails with `text_range_not_utf8` when the range is not valid UTF-8, `base64` ships only the byte-exact `dataBase64` mirror.
- Search is available only for UTF-8 entries. It performs bounded literal search over the stored file without reading the entire artifact into memory, returns byte offsets and bounded snippets, and supports pagination.
- Expired or capacity-evicted references owned by the caller remain explicit tombstones for a bounded period, so retrieval says `expired` or `evicted` instead of implying that omitted data is still available.
- Session deletion removes that session's live entries and records them as evicted.
- Payload files live under a CatDesk-owned temporary directory outside the workspace/repository. On Unix the directory is private (0700) and payload files are private (0600).
- Payload bytes, raw session IDs, and internal storage paths are never logged or returned.

## Limits

Production defaults:

- per-entry cap: 64 MiB
- global live-data cap: 256 MiB
- TTL: 60 minutes from creation (non-sliding)
- maximum range payload per retrieval call: 128 KiB
- maximum search matches per call: 100
- maximum search snippet: 256 bytes around a match
- bounded tombstones: 1024 newest states

An entry larger than the per-entry/global cap is rejected before writing. When an accepted entry would exceed the global cap, oldest live entries are evicted until it fits.

## Internal API

`LargeResultStore` is cloneable shared state with a configurable constructor for tests.

- `put(session, workspace, bytes, content_type) -> StoredResult`
- `read_range(session, workspace, result_id, offset, max_bytes) -> RangeResult`
- `search(session, workspace, result_id, query, start_offset, max_matches) -> SearchResult`
- `remove_session(session) -> usize`

Metadata includes opaque result ID, total size, UTF-8/binary kind, optional content type, creation/expiry timestamps, and any IDs evicted by the insertion.

## MCP retrieval tools

Two local read-only tools are always advertised when local computer tools are available:

- `read_result`: `result_id`, optional `offset`, optional `max_bytes`, optional `format` (`text`/`base64`/`both`, default `both`)
- `search_result`: `result_id`, `query`, optional `start_offset`, optional `max_matches`

Both use the request's current session namespace and workspace root; neither accepts a path or session argument.

## Error semantics

- foreign-session/workspace or never-known reference: `unavailable`
- own expired tombstone: `expired`
- own capacity/session-removed tombstone: `evicted`
- range offset past EOF: validation error; offset exactly at EOF is a successful empty range with `eof=true`
- binary search: explicit unsupported error
- empty search query and over-limit range/search arguments: validation error

## Verification

TDD must cover create/read/range/EOF, byte-for-byte multi-megabyte reconstruction, text and binary metadata, session/workspace isolation, TTL expiry, global eviction, per-entry rejection, private permissions, streaming search including a match crossing an I/O chunk boundary, MCP schemas/dispatch, and session deletion cleanup.
