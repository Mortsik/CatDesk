# Instruction inline vs archived card imagery — 2026-10-11

Issue `catdesk-2jk` (P0) — "keep catdesk_instruction fully inline while
preserving Binagotchy cards".

## Problem

Measured live on ChatGPT (2026-10-09): the serialized `catdesk_instruction`
result was 100,267 B, of which eight archived `binagotchyCards` occupied
~84,869 B while `instructionText` was ~7,030 characters. The shared response
budget (`DEFAULT_INLINE_RESPONSE_BYTES` = 64 KiB) measured the whole result
including `_meta.catdesk/widgetPayload`, so it externalized the entire answer:

- the model had to call `read_result` to recover the mandatory startup
  guidance (`instructionText` is not an error field, so the first compaction
  pass previewed it to 4 KiB head+tail; the AGENTS.md input-side layer cap
  bounds layers at 8 KiB each but cannot see widget-payload mass at all), and
- the compacted preview mangled the card base64 data URIs, degrading the very
  dashboard imagery the cards rode the result to reach.

## Change

Separation of transports, per the issue's "deliver the assets on demand with
a compatible resource mechanism" direction:

| Surface | Before | After |
| --- | --- | --- |
| Instruction payload cards | 8 × full card incl. base64 image | summaries: `folder`, `seed` only (no PNG read on the tool path) |
| Card imagery | inline in the tool result `_meta` | inlined into the widget HTML resource as a folder → data-URI map (`INITIAL_BINAGOTCHY_CARDS`), the same host-fetched template channel that already carries the re-enable/refresh/remove screenshots |
| Resource URI cache-busting | `widgetRevision=7` + layout/corner params | `widgetRevision=8` + `cardsRev` (hash of newest folder names + `metadata.toml`/`character.png` size+mtime) so a changed archive forces a fresh template fetch |
| Resource-side image loading | n/a (images never touched resources) | revision-keyed in-process cache; unchanged archives are not re-read or re-encoded per render |
| Dashboard `<img>` resolution | `card.image` required | `card.image` optional → `INITIAL_BINAGOTCHY_CARDS[card.folder]` → placeholder |
| Missing/oversized card filtering | PNG read + decode-cap check | `fs::metadata` len check only; selection identical to the heavy feed |

`ShowDetailMode::Disable` still attaches no widget payload at all, and
`attach_catdesk_instruction_actions` (server.rs) stamps
`isPartner`/`saveFolderUrl`/`setPartnerUrl` on the summary cards unchanged —
identity fields were all it ever needed.

## Measurements

Deterministic fixture (pinned in
`catdesk_instruction_with_realistic_archive_stays_fully_inline`,
src/mcp/tests.rs): eight 48×48 noise PNGs (~85 KB base64 after the lossless
widget compaction) in the per-process test home archive, plus a max-size
(8,192 B) AGENTS.md layer so `instructionText` exceeds the diagnostic preview
bound.

| Metric | Before (legacy result through the budget) | After | Δ |
| --- | --- | --- | --- |
| Serialized instruction result | 106,914 B → externalized | 20,930 B, fully inline | −80% |
| `instructionText` visible to the model | 4,096 B preview + `outputRef` | 11,812 B = 100% | guidance complete |
| `read_result` follow-ups to recover guidance | 1+ | 0 | −1 round trip |
| Instruction-path archive I/O | readdir + 8 metadata + 8 PNG reads + 8 lossless PNG re-encodes | readdir + 8 metadata reads | decode/re-encode eliminated |
| Widget resource (`resources/read`) | unchanged template | template + inlined card map; 34 ms cold in tests, cache-served while `cardsRev` holds | UI capability preserved |

Token note: the complete inline instruction costs more model-visible tokens
than the truncated preview did (o200k estimate ~1,145 → ~3,628 for the
fixture text) — that is the acceptance-A trade: the mandatory guidance is
delivered by the structured response instead of being hidden behind a
retrieval address (the catdesk-t05 failure mode). `_meta` was already stripped
from the model view by hosts, so moving card bytes out of the result costs the
model nothing.

## Guarantee map

| Guarantee | Enforcement / test after the change |
| --- | --- |
| 100% inline instructionText, no read_result (A) | `catdesk_instruction_with_realistic_archive_stays_fully_inline` (result ≤ 64 KiB, byte-for-byte text equality, no `responseBudget`, no `outputRef`) |
| Cards still display, refresh/reconnect-safe (B) | `widget_resource_inlines_archived_card_images_for_the_dashboard` (map parse + 8 data URIs), `archived_card_images_are_cached_per_revision` (reload on revision change), `archived_cards_revision_tracks_the_folder_set`, `archived_card_summaries_carry_identity_without_images` (selection parity with the heavy feed) |
| ShowDetailMode Enable/Disable (C) | acceptance test runs both modes through the dispatcher; existing `catdesk_instruction_disable_skips_dedicated_widget_payload`, `widget_resources_follow_show_detail_mode` |
| outputSchema projection (C) | acceptance test pins `properties.instructionText.type = "string"` and full-text equality in `structuredContent`; pre-existing `local_tools_list_exposes_output_schemas_except_multimodal_read_image` |
| Before/after recorded, tests pass (D) | table above; full suite 685 passed at commit 78589c3 |

## Known edges

- A metadata-only seed change inside an existing folder does not move
  `cardsRev` (folder set + file size/mtime do); the summary feed itself is
  computed fresh per instruction call, so only the resource-side image map
  could lag until any file size/mtime change or a new archive folder appears.
- Hosts that never fetch MCP resources would show card placeholders instead
  of images; ChatGPT's widget template channel is the only supported host
  surface for the dashboard and it already fetches this resource.
