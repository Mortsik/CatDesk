# Lossless Large-Result Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a lossless, bounded, session/workspace-isolated large-result store plus ranged/search MCP retrieval.

**Architecture:** A new disk-backed `LargeResultStore` keeps complete payloads in a private CatDesk temp directory and an in-memory metadata/tombstone index. MCP adds only read/search retrieval; later beads can call the internal `put` API to externalize oversized responses.

**Tech Stack:** Rust 2024, std filesystem/I/O, Tokio/MCP existing infrastructure, serde_json, uuid/base64 already present.

**Spec:** `docs/superpowers/specs/2026-10-07-large-result-store-design.md`

## Global Constraints

- Never expose arbitrary host paths; public contract is opaque result IDs only.
- Preserve every accepted payload byte; no store-side truncation.
- Scope every result to originating session namespace and workspace root.
- Retrieval is bounded, deterministic and explicitly reports EOF/eviction/expiry.
- Payload storage stays outside the repository and uses private Unix permissions.
- No payload/session secrets in logs.
- No new dependency unless the standard library/existing dependencies cannot meet the contract.

## Review Focus

- A byte range begins/ends inside a multi-byte UTF-8 code point: base64 remains lossless and pagination still advances exactly by bytes.
- A search match spans the internal read-buffer boundary: it is returned exactly once at the correct byte offset.
- Two sessions reuse the same JSON-RPC ID/result lookup pattern: neither can observe the other's result or tombstone.
- Global-cap eviction and TTL cleanup race with retrieval: locking/file lifecycle yields only complete data or an explicit unavailable state.
- A large payload is reconstructed using many max-sized calls: no response exceeds the configured per-call range cap and concatenated bytes equal the original.

---

### Task 1: Core disk-backed result store

**Files:**
- Create: `src/result_store.rs`
- Modify: `src/main.rs`
- Test: `src/result_store.rs`

**Interfaces:**
- Consumes: workspace path and optional session namespace.
- Produces: `LargeResultStore::{new, put, read_range, search, remove_session}`, serializable result metadata, explicit store errors/states.

- [ ] **Step 1: Write failing unit tests** for create/read/range/EOF, UTF-8 and binary metadata, session/workspace isolation, deterministic caps/eviction/expiry, private permissions, cross-buffer search, and multi-megabyte reconstruction.
- [ ] **Step 2: Run** `cargo test result_store::tests -- --nocapture`.
  **Expected:** FAIL because the store API/implementation does not exist.
- [ ] **Step 3: Implement the minimal disk-backed store** in `src/result_store.rs` with the exact limits and semantics from the spec.
- [ ] **Step 4: Run** `cargo test result_store::tests -- --nocapture`.
  **Expected:** all result-store tests PASS.
- [ ] **Step 5: Commit** as `feat(storage): add lossless large result store`.

### Task 2: Bounded MCP range/search retrieval

**Files:**
- Create: `src/mcp/result_tools.rs`
- Modify: `src/mcp.rs`
- Modify: `src/mcp/tool_catalog.rs`
- Modify: `src/mcp/tests.rs`

**Interfaces:**
- Consumes: `LargeResultStore` from Task 1, current workspace root and session namespace.
- Produces: local read-only `read_result` and `search_result` MCP tools with structured bounded responses.

- [ ] **Step 1: Write failing MCP tests** for tool descriptors/output schemas, read pagination/EOF, text/binary response fields, search pagination, foreign-session rejection, and invalid limits.
- [ ] **Step 2: Run** targeted `cargo test mcp::tests::*result* -- --nocapture` tests individually.
  **Expected:** FAIL because retrieval tools are not advertised/dispatched.
- [ ] **Step 3: Implement handlers/catalog/dispatcher wiring** without adding any write/store-public MCP tool.
- [ ] **Step 4: Run** the targeted MCP result-tool tests.
  **Expected:** PASS.
- [ ] **Step 5: Commit** as `feat(mcp): add large result retrieval tools`.

### Task 3: Server lifecycle integration and end-to-end proof

**Files:**
- Modify: `src/server.rs`
- Modify: `src/mcp.rs`
- Test: `src/server.rs` and/or `src/mcp/tests.rs`

**Interfaces:**
- Consumes: shared `LargeResultStore` and Task 2 retrieval handlers.
- Produces: one store shared across requests; production request dispatch passes current session/workspace; `DELETE /mcp` evicts that session's live results.

- [ ] **Step 1: Write failing integration tests** proving same-session retrieval across separate calls, DELETE-session cleanup, cross-session isolation, and multi-megabyte reconstruction without a single oversized MCP range response.
- [ ] **Step 2: Run targeted tests.**
  **Expected:** FAIL because server lifecycle does not yet own/pass/clean the store.
- [ ] **Step 3: Wire store lifetime through server session state** while avoiding unrelated changes to command-job behavior.
- [ ] **Step 4: Run targeted integration tests and `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, then `cargo test`.
  **Expected:** targeted tests, format and clippy PASS; full suite PASS except any independently reproduced pre-existing baseline failure, which must be reported by exact test name.
- [ ] **Step 5: Commit** as `feat(mcp): wire large result store lifecycle`.
