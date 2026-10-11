# Widget mascot rides the resource, identity rides the payload — 2026-10-11

Issue `catdesk-u4u` (P1) — "cache and delta-update widget media/assets without
stale UI". Extends the catdesk-2jk transport split from the archived card
imagery to the widget mascot.

## Problem

After catdesk-2jk the archived card imagery rode the widget resource, but the
transport mascot still rode the `catdesk_instruction` tool result — twice per
call, in fact: `catdesk_instruction_widget_payload_with_cards` built it
(instruction.rs) and `attach_catdesk_instruction_actions` rebuilt and
overwrote it (server.rs), so every call paid two full character-render passes
(a `create_character` per pose) and re-shipped the frames:

- a normal seed re-shipped ~6.6 KB of frames on every instruction call, and
- a **spirit** seed re-shipped its 720×420 hero background: **182,718 B**
  measured. On top of the ~20.9 KB post-2jk result that is ~203 KB — past the
  64 KiB inline budget — which would externalize the whole instruction behind
  `read_result` and reintroduce the catdesk-2jk failure mode (mandatory
  guidance hidden behind a retrieval address) for the 1 % spirit population.
  This was latent, not hypothetical: nothing in the budget path excludes
  spirit seeds.

## Change

Same separation of transports catdesk-2jk established for the cards:

| Surface | Before | After |
| --- | --- | --- |
| Instruction payload mascot | full `widgetMascot` (frames + palette + hero background), built twice per call | `widgetMascotSeed` identity (16-hex, same form as the partner seed); zero mascot builds on the instruction path |
| Mascot frames | inline in the tool result `_meta` | inlined into the widget HTML resource as `INITIAL_WIDGET_MASCOT` (`{seed, mascot}`), the same host-fetched template channel as the card imagery |
| Resource URI cache-busting | `widgetRevision=8` + layout/corner + `cardsRev` | `widgetRevision=9` + … + `mascotRev` = hash(seed, generator revision): partner set/reset moves the seed and with it the URI; an unchanged seed keeps the URI stable |
| Seed publication | seed threaded per call, invisible to URI builders | process-global `current_widget_mascot_seed` (AtomicU64) republished at MCP request entry; the server owns one effective seed at a time |
| Mascot generation | built fresh at every use | seed-keyed cache (`build_widget_mascot_cached`), invalidated by `WIDGET_MASCOT_GENERATOR_REVISION`, surfaced as `CacheKind::WidgetMascot` in perf metrics |
| Partner set/reset HTTP responses | full mascot, built fresh | full mascot, served from the cache (live update channel unchanged) |
| Dashboard mascot resolution | `widgetMascot` required in the instruction payload | optional: present → validated; absent → resource mascot when `INITIAL_WIDGET_MASCOT.seed === payload.widgetMascotSeed`, else the mascot area renders empty (no frames to animate) — an explicit-but-corrupt mascot is still a payload error, and a seed mismatch degrades to the empty hero instead of animating a stale mascot |

`ShowDetailMode::Disable` still attaches no widget payload and lists no
resources. The dashboard HTML change ships in the same binary as the server
(the HTML is `include_str!`-ed), so the `widgetRevision` 8→9 bump flushes
every host template exactly once on upgrade — no version skew window.

## Measurements

Deterministic, from the pinned tests (`widget_resource_inlines_widget_mascot_for_the_dashboard`,
`instruction_widget_payload_carries_mascot_identity_without_frames`,
`spirit_mascot_instruction_stays_fully_inline`):

| Metric | Before | After | Δ |
| --- | --- | --- | --- |
| Instruction result, normal seed (seed 1) | 12,235 B (legacy reconstruction with mascot) | 5,600 B, fully inline | −54 % |
| Instruction result, spirit seed (seed 0) | ~203 KB → externalized (latent) | 5,592 B, fully inline | budget breach closed |
| Mascot JSON per instruction call | 6,619 B re-shipped, ×2 CPU builds | 0 B, 0 builds | eliminated |
| Spirit hero background | in the tool result | 182,718 B in the resource (fetched once per template render, URI-addressed) | moved off the per-call path |
| Widget resource render | ~427 KB (no mascot) | 434,083 B | +1.5 % cold render |
| Mascot builds, warm session | 2 per instruction call | 0 (cache hit on resource render / partner actions) | CPU eliminated |

Token note: `_meta` is stripped from the model view by hosts, so shrinking the
widget payload costs the model nothing — same property catdesk-2jk relied on.

## Guarantee map

| Guarantee | Enforcement / test |
| --- | --- |
| First render / cache miss carries every asset (A) | `widget_resource_inlines_widget_mascot_for_the_dashboard` (identity + dimensions + frames + sequence in the resource), `spirit_mascot_instruction_stays_fully_inline` (hero background in the resource) |
| Unchanged updates avoid retransmitting media (A) | `instruction_widget_payload_carries_mascot_identity_without_frames` (identity-only payload, byte-delta pinned against the legacy reconstruction), `widget_mascot_frames_are_cached_per_seed` (miss→hit counter progression) |
| Deterministic invalidation, no stale UI (B) | `widget_resource_uri_tracks_the_mascot_revision` (rev deterministic per seed, moves across seeds; the URI embeds the revision of the published seed, so publish → bust holds by construction — the global is deliberately not flipped in tests to keep the suite parallel-safe); dashboard seed mismatch → empty hero, never a stale mascot |
| Reconnect / expired cache / non-caching hosts (C) | full-resource fallback is the only render path — every render inlines mascot + cards; `resources_read_includes_widget_csp_connect_domains` pins the new placeholder replacement |
| ShowDetailMode variants (C) | full suite runs the dispatcher gates in Disable/Expanded/Collapsed (699 passed); Disable path untouched |
| Session/workspace isolation (C) | mascot is a pure function of the seed (no workspace data); the seed cache keys on seed only; the cards cache keys on archive root — nothing conflates sessions or workspaces |
| No UI regression (D) | dashboard resolution keeps the explicit-mascot channel (partner HTTP responses) and the validator's strictness for corrupt explicit payloads; full suite green |

## Known edges

- `mascotRev` hashes the seed and the generator revision, never the frames:
  a generator change that maps a seed to identical frames still busts the
  cache (one extra full render) — cheap and deterministic in the safe
  direction.
- The current-seed global is republished at MCP request entry. A partner
  change followed by a `resources/read` *before* any MCP request would render
  the new seed while the URI still addresses the old one; the first request
  after the change republishes and busts. The dashboard-side seed check keeps
  even that window from showing a wrong mascot (empty hero instead).
- Hosts that never fetch MCP resources see an empty mascot area instead of an
  animated one — the same population that already sees card placeholders
  (catdesk-2jk known edge); ChatGPT's template channel fetches the resource.
- Validation strictness is asymmetric by design: an explicit-but-corrupt
  `widgetMascot` rejects the whole payload (it is server-authored content),
  while a `widgetMascotSeed` of the wrong type or an identity with no matching
  resource mascot degrades silently to the empty hero — the identity is a
  lookup key, not a value the UI must render verbatim. Independent review
  (post-merge) confirmed no correctness bugs; the doc wording above was
  tightened from its first version accordingly.
