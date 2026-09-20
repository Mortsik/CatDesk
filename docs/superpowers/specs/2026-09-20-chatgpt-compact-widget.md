# ChatGPT Compact Widget Spec

**Goal:** Keep CatDesk tool results fully available to the model while making ChatGPT tool cards substantially cheaper to load and render, especially in long CatDesk-heavy conversations on mobile.

## Requirements

- Do not reduce or truncate `structuredContent` solely for GUI performance. Model-visible tool data remains unchanged.
- Ordinary CatDesk tool calls use a lightweight ChatGPT widget resource instead of the full dashboard resource.
- `catdesk_instruction` keeps the full dashboard because it exposes settings/status that the compact card does not need to duplicate.
- The compact widget reads only `_meta.catdesk/widgetPayload` / `window.openai.toolResponseMetadata`; it must not depend on the full model payload.
- Compact cards render only a small summary: tool name/title, state, optional command/search/path/counts, short command/detail preview, and changed-file metadata.
- Widget-only payloads must not carry unbounded command output, full file diffs, or arbitrarily large directory listings.
- Existing full model output schemas and tool behavior remain backward compatible.
- `ShowDetailMode::Disable` continues to attach no widget.
- Keep the existing full dashboard resource URI behavior compatible for `catdesk_instruction` and direct resource reads.

## Performance targets

- Compact widget template source should stay below 20 KiB.
- Command preview in widget metadata should be capped at 4,000 characters.
- Per-file diff preview in widget metadata should be capped at 1,500 characters, and at most 12 changed files should be carried by a compact card.
- Directory-listing rows in widget metadata should be capped at 40 entries while preserving total counts from `structuredContent`.

## Non-goals

- Do not change model context/history semantics implemented by ChatGPT.
- Do not remove the full dashboard or its configuration UI.
- Do not change CatDesk command/read/search result budgets visible to the model.
