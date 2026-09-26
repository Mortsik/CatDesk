//! Model-aware usage pricing registry.
//!
//! Token usage is accumulated into per-model buckets of
//! [`UsageTotals`](crate::state::UsageTotals). This module maps a bucket name to its
//! [`ModelPricing`] and prices usage maps without ever panicking: buckets without a
//! registry entry are reported as unpriced so the dashboard can render `N/A`.
//!
//! New buckets join the registry by adding a `pricing_for_bucket` arm plus a price
//! constant; no dashboard or persistence change should be needed.

use std::collections::BTreeMap;

use crate::state::UsageTotals;

/// Frozen bucket holding every turn recorded before per-model attribution existed.
/// New turns must never be written here; the entry only keeps historic totals priced.
pub const GPT_5_6_AND_EARLIER_USAGE_BUCKET: &str = "through-gpt-5.6";

/// Bucket for new turns whose model is unknown. ChatGPT connectors do not report the
/// model yet, so this is the current primary bucket; it is priced at the legacy rate
/// and flagged [`ModelPricing::estimated`].
pub const FALLBACK_USAGE_BUCKET: &str = "unattributed";

const GPT_5_6_AND_EARLIER_INPUT_USD_PER_1M: f64 = 5.0;
const GPT_5_6_AND_EARLIER_OUTPUT_USD_PER_1M: f64 = 30.0;

/// Legacy history is real billed usage, not an estimate.
const GPT_5_6_AND_EARLIER_MODEL_PRICING: ModelPricing = ModelPricing {
    llm_input_usd_per_1m: GPT_5_6_AND_EARLIER_INPUT_USD_PER_1M,
    llm_output_usd_per_1m: GPT_5_6_AND_EARLIER_OUTPUT_USD_PER_1M,
    estimated: false,
};

/// Turns without model metadata reuse the legacy rate as an estimate.
pub const FALLBACK_MODEL_PRICING: ModelPricing = ModelPricing {
    llm_input_usd_per_1m: GPT_5_6_AND_EARLIER_INPUT_USD_PER_1M,
    llm_output_usd_per_1m: GPT_5_6_AND_EARLIER_OUTPUT_USD_PER_1M,
    estimated: true,
};

/// Per-1M-token pricing for one usage bucket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    /// USD per 1M tokens the LLM reads (prompt side — tool *results*, counted as
    /// `tool_output_tokens`).
    pub llm_input_usd_per_1m: f64,
    /// USD per 1M tokens the LLM writes (completion side — tool call *arguments*,
    /// counted as `tool_input_tokens`).
    pub llm_output_usd_per_1m: f64,
    /// True when the rate is a legacy-rate estimate rather than a model-specific
    /// price-list entry.
    pub estimated: bool,
}

/// Outcome of pricing a whole usage map.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostEstimate {
    /// Sum over buckets that have a registry entry.
    pub priced_usd: f64,
    /// Total tokens sitting in buckets with no registry entry; never priced.
    pub unpriced_tokens: u64,
}

impl CostEstimate {
    /// True when nothing in the map could be priced but tokens were recorded.
    pub fn is_unpriced(&self) -> bool {
        self.priced_usd == 0.0 && self.unpriced_tokens > 0
    }
}

/// Registry lookup. `None` means "unknown bucket" — callers render it as unpriced
/// instead of guessing a rate.
pub fn pricing_for_bucket(bucket: &str) -> Option<ModelPricing> {
    match bucket {
        GPT_5_6_AND_EARLIER_USAGE_BUCKET => Some(GPT_5_6_AND_EARLIER_MODEL_PRICING),
        FALLBACK_USAGE_BUCKET => Some(FALLBACK_MODEL_PRICING),
        _ => None,
    }
}

/// Usage bucket for turns produced by `model`. `None` (model metadata is not reported
/// yet) falls back to [`FALLBACK_USAGE_BUCKET`].
pub fn bucket_for_model(model: Option<&str>) -> String {
    match model {
        Some(model) => format!("model:{model}"),
        None => FALLBACK_USAGE_BUCKET.to_string(),
    }
}

/// Price one bucket's totals.
///
/// The tool boundary is not the LLM I/O boundary, so the rates are deliberately
/// swapped: `tool_input_tokens` are tool call arguments — text the LLM *produced*
/// (charged at [`ModelPricing::llm_output_usd_per_1m`]) — while `tool_output_tokens`
/// are tool results — text the LLM *consumed* (charged at
/// [`ModelPricing::llm_input_usd_per_1m`]).
pub fn estimate_usage_cost_usd(usage: &UsageTotals, pricing: &ModelPricing) -> f64 {
    (usage.tool_input_tokens as f64 * pricing.llm_output_usd_per_1m
        + usage.tool_output_tokens as f64 * pricing.llm_input_usd_per_1m)
        / 1_000_000.0
}

/// Price a usage map, splitting priced dollars from unpriced tokens instead of
/// panicking on unknown buckets.
pub fn estimate_usage_by_model_cost(
    usage_by_model: &BTreeMap<String, UsageTotals>,
) -> CostEstimate {
    let mut estimate = CostEstimate::default();
    for (bucket, usage) in usage_by_model {
        match pricing_for_bucket(bucket) {
            Some(pricing) => estimate.priced_usd += estimate_usage_cost_usd(usage, &pricing),
            None => {
                estimate.unpriced_tokens =
                    estimate.unpriced_tokens.saturating_add(usage.total_tokens);
            }
        }
    }
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(tool_input_tokens: u64, tool_output_tokens: u64) -> UsageTotals {
        let mut usage = UsageTotals::default();
        usage.accumulate(tool_input_tokens, tool_output_tokens, 1);
        usage
    }

    #[test]
    fn pricing_for_bucket_covers_legacy_and_fallback_only() {
        let legacy = pricing_for_bucket(GPT_5_6_AND_EARLIER_USAGE_BUCKET)
            .expect("legacy bucket must stay priced");
        assert_eq!(legacy.llm_input_usd_per_1m, 5.0);
        assert_eq!(legacy.llm_output_usd_per_1m, 30.0);
        assert!(!legacy.estimated, "legacy history is real billed usage");

        let fallback =
            pricing_for_bucket(FALLBACK_USAGE_BUCKET).expect("fallback bucket must stay priced");
        assert_eq!(fallback.llm_input_usd_per_1m, 5.0);
        assert_eq!(fallback.llm_output_usd_per_1m, 30.0);
        assert!(
            fallback.estimated,
            "fallback turns are estimated, not billed"
        );

        assert_eq!(pricing_for_bucket("model:gpt-5.7"), None);
        assert_eq!(pricing_for_bucket(""), None);
    }

    #[test]
    fn bucket_for_model_falls_back_without_model_metadata() {
        assert_eq!(bucket_for_model(None), FALLBACK_USAGE_BUCKET);
        assert_eq!(bucket_for_model(Some("gpt-5.7")), "model:gpt-5.7");
    }

    #[test]
    fn estimate_usage_cost_usd_charges_arguments_at_the_output_rate() {
        // 1M tokens of generated tool arguments -> 1M LLM output tokens -> $30.
        let arguments = estimate_usage_cost_usd(&totals(1_000_000, 0), &FALLBACK_MODEL_PRICING);
        assert_eq!(arguments, 30.0);
        // 1M tokens of tool results read back -> 1M LLM input tokens -> $5.
        let results = estimate_usage_cost_usd(&totals(0, 1_000_000), &FALLBACK_MODEL_PRICING);
        assert_eq!(results, 5.0);
    }

    #[test]
    fn estimate_usage_by_model_cost_splits_priced_from_unpriced() {
        let mut usage_by_model = BTreeMap::new();
        usage_by_model.insert(
            GPT_5_6_AND_EARLIER_USAGE_BUCKET.to_string(),
            totals(2_000_000, 1_000_000),
        );
        usage_by_model.insert(FALLBACK_USAGE_BUCKET.to_string(), totals(1_000_000, 0));
        usage_by_model.insert(
            "model:gpt-9-future".to_string(),
            totals(2_000_000, 3_000_000),
        );

        let estimate = estimate_usage_by_model_cost(&usage_by_model);
        // Legacy 2M out-rate + 1M in-rate = $65, fallback 1M out-rate = $30.
        assert!((estimate.priced_usd - 95.0).abs() < 1e-9, "{estimate:?}");
        assert_eq!(estimate.unpriced_tokens, 5_000_000);
        assert!(!estimate.is_unpriced());
    }

    #[test]
    fn estimate_usage_by_model_cost_reports_all_unpriced_maps() {
        let mut usage_by_model = BTreeMap::new();
        usage_by_model.insert(
            "model:gpt-9-future".to_string(),
            totals(1_000_000, 1_000_000),
        );

        let estimate = estimate_usage_by_model_cost(&usage_by_model);
        assert_eq!(estimate.priced_usd, 0.0);
        assert_eq!(estimate.unpriced_tokens, 2_000_000);
        assert!(estimate.is_unpriced());
    }

    #[test]
    fn estimate_usage_by_model_cost_handles_empty_maps() {
        assert_eq!(
            estimate_usage_by_model_cost(&BTreeMap::new()),
            CostEstimate::default()
        );
    }
}
