# ChatGPT Compact Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce ChatGPT-side rendering and widget payload cost for CatDesk-heavy conversations without reducing model-visible tool results.

**Architecture:** Split CatDesk’s current single heavy widget resource into a full dashboard used by `catdesk_instruction` and a small compact widget used by ordinary tool calls. Keep `structuredContent` unchanged; only compact `_meta.catdesk/widgetPayload` previews and resource HTML. Preserve existing tool schemas and full dashboard behavior.

**Tech Stack:** Rust, serde_json, MCP Apps widget HTML/JS, cargo test.

**Spec:** `docs/superpowers/specs/2026-09-20-chatgpt-compact-widget.md`

## Global Constraints

- Full model-visible `structuredContent` must remain unchanged for GUI optimization.
- `ShowDetailMode::Disable` must still attach no widget.
- `catdesk_instruction` retains the full dashboard.
- Compact widget source stays below 20 KiB.
- Command preview <= 4,000 chars; per-file diff preview <= 1,500 chars; <= 12 changed files; <= 40 listing rows.

## Review Focus

- Tool descriptors for ordinary tools must point at the compact resource while `catdesk_instruction` keeps the full dashboard.
- Direct resource reads must accept both resource URI families and render the correct HTML.
- Large `structuredContent` values must remain intact even when widget payloads are compacted.
- Empty/error tool results must still produce a valid compact card rather than failing validation.
- Compact caps must not make `hasChanges` inconsistent with the changed-file array.

---

### Task 1: Split full and compact widget resources

**Files:**
- Create: `src/widget/catdesk_compact.html`
- Modify: `src/mcp.rs`
- Test: `src/mcp.rs` test module

**Interfaces:**
- Consumes: existing `WIDGET_PAYLOAD_META_KEY`, `window.openai.toolResponseMetadata`, `ShowDetailMode` behavior.
- Produces: compact resource URI/template for ordinary tool descriptors; existing dashboard resource remains for `catdesk_instruction`.

- [ ] **Step 1: Write failing tests**

Add tests proving:
- `catdesk_instruction` output template still uses `ui://widget/catdesk-dashboard.html`.
- `run_command`, `read`, `search`, `write`, `edit`, `start_command`, `poll_command`, `cancel_command`, `create_handoff`, `delete`, and `read_image` use a compact resource URI.
- the compact resource is readable, contains the widget payload metadata key, and is < 20 KiB.
- direct dashboard resource reads still return the full dashboard.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test compact_widget -- --nocapture`
Expected: FAIL because no compact resource/template exists.

- [ ] **Step 3: Implement minimal split**

In `src/mcp.rs` add a compact template URI and embedded compact HTML. Route ordinary tool descriptors to it; route `catdesk_instruction` to the existing dashboard URI. Extend widget-resource URI detection and rendering to both resources. Keep `ShowDetailMode::Disable` unchanged.

Create `src/widget/catdesk_compact.html` with minimal CSS/JS. It reads only `window.openai.toolResponseMetadata["catdesk/widgetPayload"]`, listens for `ui/notifications/tool-result` and `openai:set_globals`, and renders only title/tool/state plus small optional summary fields.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test compact_widget -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

Run `git log --oneline -n 5`, then commit the Task 1 files with repository-consistent style.

### Task 2: Bound widget-only payload previews while preserving model payloads

**Files:**
- Modify: `src/mcp.rs`
- Test: `src/mcp.rs` test module

**Interfaces:**
- Consumes: existing `structuredContent`, `AutoWidgetContext`, `build_*_widget_payload` helpers.
- Produces: bounded `_meta.catdesk/widgetPayload` while model-visible data remains unchanged.

- [ ] **Step 1: Write failing tests**

Add tests proving:
- a `run_command` result with >4,000 characters keeps full `structuredContent.stdout` but widget `output` <= 4,000 chars.
- changed-file widget entries are capped at 12, each `diff` <= 1,500 chars, while `hasChanges` remains true.
- list widget payload contains at most 40 `listEntries`, but structured total counts and full `structuredContent.listEntries` remain unchanged.
- read/search structured payloads remain unchanged by enrichment.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test widget_payload_is_compact -- --nocapture`
Expected: FAIL on current 24,000-char command preview, unbounded changed-file diffs/count, or unbounded listing rows.

- [ ] **Step 3: Implement minimal caps**

Add widget-only constants and helpers. Apply caps only while building `_meta.catdesk/widgetPayload`; do not mutate `structuredContent`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test widget_payload_is_compact -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run full suite**

Run: `cargo test`
Expected: PASS with no failures.

- [ ] **Step 6: Commit**

Run `git log --oneline -n 5`, then commit Task 2 with repository-consistent style.
