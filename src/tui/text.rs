use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::MCP_URL_REVEAL_DURATION;
use crate::usage_pricing;

pub(crate) const MCP_URL_REVEAL_BAR_CELLS: usize = 10;
pub(crate) const PRICE_DISPLAY_DECIMALS: usize = 6;

pub(crate) fn terminal_cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn pad_right_to_cell_width(text: &str, width: usize) -> String {
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(terminal_cell_width(text)))
    )
}

pub(crate) fn trim_line(text: &str, max_cells: usize) -> String {
    if terminal_cell_width(text) <= max_cells {
        return text.to_string();
    }
    if max_cells <= 3 {
        return ".".repeat(max_cells);
    }

    let target_width = max_cells - 3;
    let mut kept = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width.saturating_add(ch_width) > target_width {
            break;
        }
        kept.push(ch);
        width = width.saturating_add(ch_width);
    }
    format!("{kept}...")
}

pub(crate) fn format_session_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }

    let minutes = total_seconds / 60;
    if minutes < 60 {
        return format!("{}m {}s", minutes, total_seconds % 60);
    }

    format!("{}h {}m", minutes / 60, minutes % 60)
}

pub(crate) fn format_token_compact(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }

    let (unit, suffix) = if value >= 1_000_000_000 {
        (1_000_000_000.0, "B")
    } else if value >= 1_000_000 {
        (1_000_000.0, "M")
    } else {
        (1_000.0, "K")
    };
    let scaled = value as f64 / unit;
    let decimals = if scaled >= 100.0 { 0 } else { 1 };
    let formatted = format!("{scaled:.prec$}", prec = decimals);
    format!("{}{}", formatted.trim_end_matches(".0"), suffix)
}

/// Renders usage cost with an explicit unpriced tail: `$X` when every bucket is
/// priced, `$X +N/A` when some buckets have no registry entry, `N/A` when nothing
/// could be priced, and `$0` when no usage was recorded at all. Chosen over a bare
/// `$X*` marker because the status panel has no legend line to explain it.
pub(crate) fn format_cost_estimate_usd(estimate: usage_pricing::CostEstimate) -> String {
    if estimate.priced_usd > 0.0 {
        if estimate.unpriced_tokens == 0 {
            format!("${}", format_usd_compact(estimate.priced_usd))
        } else {
            format!("${} +N/A", format_usd_compact(estimate.priced_usd))
        }
    } else if estimate.is_unpriced() {
        "N/A".to_string()
    } else {
        "$0".to_string()
    }
}

/// Average cost per call or per day, priced over `count` units of the matching
/// metric. Averages only cover the priced part; the unpriced tail (if any) keeps
/// the `+N/A` marker so a partially priced map never reads as fully billed.
pub(crate) fn format_average_usage_cost_usd(estimate: usage_pricing::CostEstimate, count: u64) -> String {
    if count == 0 {
        return "$0".to_string();
    }
    format_cost_estimate_usd(usage_pricing::CostEstimate {
        priced_usd: estimate.priced_usd / count as f64,
        unpriced_tokens: u64::from(estimate.unpriced_tokens > 0),
    })
}

pub(crate) fn format_usd_compact(usd: f64) -> String {
    let formatted = format!("{usd:.prec$}", prec = PRICE_DISPLAY_DECIMALS);
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn mcp_url_reveal_seconds(remaining: Duration) -> u64 {
    remaining
        .as_millis()
        .div_ceil(1_000)
        .min(MCP_URL_REVEAL_DURATION.as_secs() as u128) as u64
}

pub(crate) fn mcp_url_reveal_bar_segments(remaining: Duration) -> (String, String) {
    let total_millis = MCP_URL_REVEAL_DURATION.as_millis();
    let remaining_millis = remaining.as_millis().min(total_millis);
    let lit = remaining_millis
        .saturating_mul(MCP_URL_REVEAL_BAR_CELLS as u128)
        .div_ceil(total_millis) as usize;
    (
        "━".repeat(lit.min(MCP_URL_REVEAL_BAR_CELLS)),
        "─".repeat(MCP_URL_REVEAL_BAR_CELLS.saturating_sub(lit)),
    )
}

pub(crate) fn session_cost_rates(cost_usd: f64, elapsed: Duration) -> (f64, f64) {
    let elapsed_secs = elapsed.as_secs_f64();
    if elapsed_secs <= 0.0 {
        return (0.0, 0.0);
    }
    let cost_per_min_usd = cost_usd * 60.0 / elapsed_secs;
    (cost_per_min_usd, cost_per_min_usd * 60.0)
}
