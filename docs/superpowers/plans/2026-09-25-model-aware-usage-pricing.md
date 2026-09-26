# Model-aware usage pricing — implementation plan (catdesk-vmi.5)

Date: 2026-09-25 · Base: origin/main @ 76df1b1 · Branch: feat/vmi5-model-pricing-20260926

## Goal

Make token/cost telemetry extensible past the single `through-gpt-5.6` bucket: an explicit
pricing registry, an attribution path for future model metadata, and a dashboard that renders
unknown buckets as unpriced (`N/A`) instead of panicking (`main.rs` `estimate_usage_by_model_cost_usd`
matched `_ => panic!("missing pricing for usage bucket ...")`). Existing `usageByModel` history
keeps rendering; tool input/output direction becomes explicit in UI labels and payloads.

## Non-goals

- No write-side migration of existing `config.toml` buckets (read-side continuity only: the
  frozen legacy key stays and the registry still prices it).
- No model metadata plumbing (JsonRpcRequest carries no model); `bucket_for_model(None)` is
  the wired-in placeholder and `unattributed` is the real primary bucket until metadata lands.
- No new price-list sources (per-model entries are future registry rows).
- No touching vmi.2/3/4 regions beyond the shared status panel lines: build_info, perf_metrics,
  PERF/SYS rows, REQ rows stay untouched.

## Design

### src/usage_pricing.rs (new module)

- `GPT_5_6_AND_EARLIER_USAGE_BUCKET = "through-gpt-5.6"` — frozen, historic only; re-exported
  from `state` so existing import paths keep working.
- `FALLBACK_USAGE_BUCKET = "unattributed"` — new turns without model metadata.
- `ModelPricing { llm_input_usd_per_1m, llm_output_usd_per_1m, estimated }` — rates plus an
  `estimated` flag distinguishing a legacy-rate estimate from a model-specific entry.
- `CostEstimate { priced_usd, unpriced_tokens }` — the whole-map pricing outcome.
- `pricing_for_bucket(&str) -> Option<ModelPricing>` — `through-gpt-5.6` → (5, 30, estimated=false),
  `unattributed` → (5, 30, estimated=true), unknown → `None`.
- `bucket_for_model(Option<&str>) -> String` — `None` → fallback, `Some(m)` → `model:{m}`.
- `estimate_usage_cost_usd(&UsageTotals, &ModelPricing) -> f64` — arguments charged at the
  LLM output rate, results at the LLM input rate (documented; this is the existing,
  semantically correct asymmetry).
- `estimate_usage_by_model_cost(&BTreeMap<String, UsageTotals>) -> CostEstimate` — replaces
  the panicking aggregate; unknown buckets contribute `total_tokens` to `unpriced_tokens`.

### Per-bucket accumulation (state.rs, server.rs)

- `record_turn_usage` / `_at` / `_at_day` take an explicit `bucket: &str`; `CURRENT_USAGE_BUCKET`
  is deleted. `server.rs` passes `bucket_for_model(None)` with a comment marking the metadata
  placeholder. Test call sites updated (~15).
- `usage_by_model` / `daily_usage_by_model` shapes and `config.toml` format unchanged — legacy
  keys survive verbatim (acceptance: existing history keeps rendering).

### Dashboard (main.rs)

- `estimate_usage_by_model_cost_usd` (panic) and the single-bucket helper are deleted; call
  sites use the registry. Rolling/session/flow single-bucket paths price through
  `FALLBACK_MODEL_PRICING` (same rates as today).
- `format_cost_estimate_usd`: `unpriced_tokens == 0` → `$X`; priced > 0 with unpriced →
  `$X +N/A`; priced == 0 with unpriced → `N/A`. Chosen over `$X*` because the status panel has
  no room for a legend and `+N/A` is self-describing. COST TODAY/TOTAL/TRACKED SPENT use it;
  AVG per call/day divides the priced part only (documented) and renders `N/A/call` when
  everything is unpriced.
- Direction labels: TOKENS 60s row and flow usage spans read `↓REQ … ↑RES … Σ`
  (zh-TW `↓請求 … ↑回應`); flow spans take `ui_language`.
- Payload direction is additive: `turnTokenUsage` (mcp.rs) and `historyTurnTokenUsage`
  (server.rs) gain `"inputRole": "request"`, `"outputRole": "response"`. Existing fields
  untouched, so Apps SDK widgets keep parsing.

## Commit sequence (RED/GREEN)

1. `docs: add model-aware usage pricing implementation plan` — this file.
2. `feat: add model pricing registry module` — usage_pricing.rs + `mod` declaration + unit
   tests (known/legacy/fallback/unknown pricing, direction arithmetic, mixed map split).
3. `feat(tui): render unknown-bucket usage cost without panicking` — dashboard CostEstimate
   rendering, REQ/RES labels, payload roles, doc-comments, new dashboard tests
   (unknown-only, mixed priced+unpriced, mixed priced buckets) + updated flow/live
   assertions. Landed *before* the write-side switch so every commit stays green: with the
   old writer still targeting the legacy bucket, the registry prices it unchanged.
4. `feat: record turn usage into explicit pricing buckets` — state.rs bucket parameter,
   delete CURRENT_USAGE_BUCKET, server.rs fallback wiring, test call sites. Green because
   step 3 already prices the fallback bucket.
5. Full verify: fmt own regions only, `cargo test`, `cargo test --release`,
   `cargo build --release`.
