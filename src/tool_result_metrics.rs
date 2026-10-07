//! Per-tool byte accounting for MCP tool results: raw vs inline vs
//! externalized. Fixed-slot cumulative counters keyed by the perf-metrics
//! tool whitelist. Only numbers and whitelisted names enter this module —
//! payload text is neither stored here nor persisted by its diagnostics
//! event (`tool_result_bytes`).
//!
//! Response classes: a result is `externalized` when the large-result store
//! holds its full payload (the inline copy is a bounded preview), `compacted`
//! when bytes were dropped in-band without storage (inline < raw), and
//! `small` when the result was sent byte-for-byte (inline == raw).

use std::sync::{Mutex as StdMutex, MutexGuard, OnceLock};

use crate::perf_metrics::{TOOL_COUNT, tool_index};

/// One tool slot's cumulative totals (process lifetime). The benchmark
/// (ops/tool-result-bytes.sh) compares before/after captures from two runs,
/// so counters never decay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolResultTotals {
    pub(crate) count: u64,
    pub(crate) error_count: u64,
    pub(crate) raw_bytes: u64,
    pub(crate) inline_bytes: u64,
    pub(crate) externalized_bytes: u64,
    pub(crate) small_count: u64,
    pub(crate) compacted_count: u64,
    pub(crate) externalized_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseClass {
    Small,
    Compacted,
    Externalized,
}

impl ResponseClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Compacted => "compacted",
            Self::Externalized => "externalized",
        }
    }
}

/// One measured tool result. `raw` is the serialized result the response
/// budget considered (the full retained stdout/stderr for command tools);
/// `inline` is the serialized result actually sent to the model; `externalized`
/// is the size parked in the large-result store, zero otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolResultMeasurement {
    pub(crate) raw_bytes: u64,
    pub(crate) inline_bytes: u64,
    pub(crate) externalized_bytes: u64,
    pub(crate) is_error: bool,
}

impl ToolResultMeasurement {
    /// An untouched result whose serialized size is known; raw equals inline.
    pub(crate) fn inline_only(inline_bytes: u64, is_error: bool) -> Self {
        Self {
            raw_bytes: inline_bytes,
            inline_bytes,
            externalized_bytes: 0,
            is_error,
        }
    }

    pub(crate) fn classify(self) -> ResponseClass {
        if self.externalized_bytes > 0 {
            ResponseClass::Externalized
        } else if self.inline_bytes < self.raw_bytes {
            ResponseClass::Compacted
        } else {
            ResponseClass::Small
        }
    }
}

struct Registry {
    tools: [ToolResultTotals; TOOL_COUNT],
}

static REGISTRY: OnceLock<StdMutex<Registry>> = OnceLock::new();

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .get_or_init(|| {
            StdMutex::new(Registry {
                tools: [ToolResultTotals::default(); TOOL_COUNT],
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Account one tool result in the fixed-slot aggregates. Unknown tool names
/// fold into "other".
pub(crate) fn observe(tool: Option<&str>, measurement: ToolResultMeasurement) {
    let slot = tool_index(tool);
    let class = measurement.classify();
    {
        let mut registry = registry();
        let totals = &mut registry.tools[slot];
        totals.count = totals.count.saturating_add(1);
        if measurement.is_error {
            totals.error_count = totals.error_count.saturating_add(1);
        }
        totals.raw_bytes = totals.raw_bytes.saturating_add(measurement.raw_bytes);
        totals.inline_bytes = totals.inline_bytes.saturating_add(measurement.inline_bytes);
        totals.externalized_bytes = totals
            .externalized_bytes
            .saturating_add(measurement.externalized_bytes);
        match class {
            ResponseClass::Small => totals.small_count = totals.small_count.saturating_add(1),
            ResponseClass::Compacted => {
                totals.compacted_count = totals.compacted_count.saturating_add(1)
            }
            ResponseClass::Externalized => {
                totals.externalized_count = totals.externalized_count.saturating_add(1)
            }
        }
    }
}

/// Copy of the cumulative registry; rows pair with `perf_metrics::tool_name`.
pub(crate) fn snapshot() -> [ToolResultTotals; TOOL_COUNT] {
    registry().tools
}

#[cfg(test)]
pub(crate) fn totals_delta(
    before: &ToolResultTotals,
    after: &ToolResultTotals,
) -> ToolResultTotals {
    ToolResultTotals {
        count: after.count.saturating_sub(before.count),
        error_count: after.error_count.saturating_sub(before.error_count),
        raw_bytes: after.raw_bytes.saturating_sub(before.raw_bytes),
        inline_bytes: after.inline_bytes.saturating_sub(before.inline_bytes),
        externalized_bytes: after
            .externalized_bytes
            .saturating_sub(before.externalized_bytes),
        small_count: after.small_count.saturating_sub(before.small_count),
        compacted_count: after.compacted_count.saturating_sub(before.compacted_count),
        externalized_count: after
            .externalized_count
            .saturating_sub(before.externalized_count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf_metrics;

    #[test]
    fn classification_covers_small_compacted_and_externalized_boundaries() {
        let small = |raw: u64, inline: u64| ToolResultMeasurement {
            raw_bytes: raw,
            inline_bytes: inline,
            externalized_bytes: 0,
            is_error: false,
        };

        // Empty result: both sizes zero is still a small, counted response.
        assert_eq!(small(0, 0).classify(), ResponseClass::Small);
        assert_eq!(
            ToolResultMeasurement::inline_only(0, false).classify(),
            ResponseClass::Small
        );
        // Exactly equal at any size stays small (inline == raw).
        assert_eq!(small(512, 512).classify(), ResponseClass::Small);
        assert_eq!(
            ToolResultMeasurement::inline_only(64 * 1024, false).classify(),
            ResponseClass::Small
        );
        // In-band reduction without storage is compacted.
        assert_eq!(small(100, 42).classify(), ResponseClass::Compacted);
        // Externalization wins over the size relation.
        let externalized = ToolResultMeasurement {
            raw_bytes: 100,
            inline_bytes: 42,
            externalized_bytes: 100,
            is_error: false,
        };
        assert_eq!(externalized.classify(), ResponseClass::Externalized);
        let stored_only = ToolResultMeasurement {
            raw_bytes: 0,
            inline_bytes: 0,
            externalized_bytes: 1,
            is_error: false,
        };
        assert_eq!(stored_only.classify(), ResponseClass::Externalized);
        assert_eq!(ResponseClass::Compacted.as_str(), "compacted");
        assert_eq!(ResponseClass::Externalized.as_str(), "externalized");
        assert_eq!(ResponseClass::Small.as_str(), "small");
    }

    #[test]
    fn observe_accumulates_per_tool_class_byte_and_error_totals() {
        let before = snapshot();

        // Small result on the read slot.
        observe(Some("read"), ToolResultMeasurement::inline_only(256, false));
        // Compacted result on the run_command slot (bytes dropped in-band).
        observe(
            Some("run_command"),
            ToolResultMeasurement {
                raw_bytes: 4_096,
                inline_bytes: 512,
                externalized_bytes: 0,
                is_error: false,
            },
        );
        // Externalized result on the search slot.
        observe(
            Some("search"),
            ToolResultMeasurement {
                raw_bytes: 8_192,
                inline_bytes: 128,
                externalized_bytes: 8_192,
                is_error: false,
            },
        );
        // Failed retrieval: read_result counts toward retrieval success.
        observe(
            Some("read_result"),
            ToolResultMeasurement::inline_only(96, true),
        );
        // Unknown tool names fold into the "other" slot.
        observe(
            Some("mystery-tool"),
            ToolResultMeasurement::inline_only(64, false),
        );

        let after = snapshot();
        let read = totals_delta(
            &before[tool_index(Some("read"))],
            &after[tool_index(Some("read"))],
        );
        assert_eq!(read.count, 1);
        assert_eq!(read.small_count, 1);
        assert_eq!(read.compacted_count, 0);
        assert_eq!(read.inline_bytes, 256);
        assert_eq!(read.raw_bytes, 256);

        let run = totals_delta(
            &before[tool_index(Some("run_command"))],
            &after[tool_index(Some("run_command"))],
        );
        assert_eq!(run.count, 1);
        assert_eq!(run.compacted_count, 1);
        assert_eq!(run.raw_bytes, 4_096);
        assert_eq!(run.inline_bytes, 512);
        assert!(run.inline_bytes < run.raw_bytes);

        let search = totals_delta(
            &before[tool_index(Some("search"))],
            &after[tool_index(Some("search"))],
        );
        assert_eq!(search.count, 1);
        assert_eq!(search.externalized_count, 1);
        assert_eq!(search.raw_bytes, 8_192);
        assert_eq!(search.externalized_bytes, 8_192);

        let retrieval = totals_delta(
            &before[tool_index(Some("read_result"))],
            &after[tool_index(Some("read_result"))],
        );
        assert_eq!(retrieval.count, 1);
        assert_eq!(retrieval.error_count, 1);
        // Retrieval success is derivable: successful = count - error_count.
        assert_eq!(retrieval.count - retrieval.error_count, 0);

        let other = totals_delta(
            &before[perf_metrics::tool_index(None)],
            &after[perf_metrics::tool_index(None)],
        );
        assert_eq!(other.count, 1);
        assert_eq!(after.len(), TOOL_COUNT);
    }
}
