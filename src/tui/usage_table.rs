//! Usage section of the Status panel: one aligned table of requests, tool
//! calls, tokens and cost per time window, followed by the burn rate, the
//! tracked-history coverage and a seven-day token sparkline.

use ratatui::prelude::*;
use std::time::Duration;

use crate::state::{
    AppState, LIVE_USAGE_WINDOW_MS, UiLanguage, UsageTotals, local_now, local_usage_day_key,
};
use crate::theme::Palette;
use crate::tui::text::{
    format_average_usage_cost_usd, format_cost_estimate_usd, format_session_duration,
    format_token_compact, format_usd_short, pad_right_to_cell_width, session_cost_rates,
    terminal_cell_width,
};
use crate::usage_pricing::{self, CostEstimate};

/// Width of the row label column, including the two-cell indent. Matches the
/// `STATUS_LABEL_WIDTH` prefix of the info lines so both sections align.
const LABEL_CELLS: usize = 16;
/// Right-aligned numeric columns: requests, calls, ↓, ↑, Σ, cost, cost/call.
/// 16 + 53 = 69 cells, inside the ~72-cell status content of the 120-column
/// mascot layout. Compact token counts are at most 5 cells (`99.9K`); the cost
/// columns fit a partially priced `$0.0123+N/A`.
const COLUMN_CELLS: [usize; 7] = [6, 6, 6, 6, 6, 11, 12];
const SPARKLINE_DAYS: i64 = 7;
const SPARKLINE_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

struct UsageRow {
    label: String,
    /// `None` renders as `—`: the window has no request counter (60 s) or no
    /// per-day request history yet.
    requests: Option<u64>,
    totals: UsageTotals,
    cost: CostEstimate,
    /// Cost per tool call over the row's own window. The Avg/day row divides
    /// the whole tracked history, not its rounded averages.
    per_call: String,
}

impl UsageRow {
    fn new(label: String, requests: Option<u64>, totals: UsageTotals, cost: CostEstimate) -> Self {
        let per_call = format_average_usage_cost_usd(cost, totals.tool_call_count);
        Self {
            label,
            requests,
            totals,
            cost,
            per_call,
        }
    }
}

pub(crate) fn usage_table_lines(
    app: &AppState,
    now_millis: u128,
    session_elapsed: Duration,
    palette: &Palette,
    ui_language: UiLanguage,
) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(palette.muted_fg)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD);
    let muted_style = Style::default().fg(palette.muted_fg);
    let value_style = Style::default()
        .fg(palette.secondary_fg)
        .add_modifier(Modifier::BOLD);
    let cost_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);

    // Rolling and session totals only ever hold freshly recorded turns, so the
    // fallback estimate is the applicable rate for them.
    let rolling = app.rolling_usage_totals(now_millis, LIVE_USAGE_WINDOW_MS);
    let rolling_cost_usd =
        usage_pricing::estimate_usage_cost_usd(&rolling, &usage_pricing::FALLBACK_MODEL_PRICING);
    let session_cost_usd = usage_pricing::estimate_usage_cost_usd(
        &app.session_usage_totals,
        &usage_pricing::FALLBACK_MODEL_PRICING,
    );
    let today_by_model = app.today_usage_by_model();

    let mut tracked_totals = UsageTotals::default();
    let mut tracked_cost = CostEstimate::default();
    for usage_by_model in app.daily_usage_by_model.values() {
        tracked_totals.merge(&merged_totals(usage_by_model.values()));
        add_cost(
            &mut tracked_cost,
            usage_pricing::estimate_usage_by_model_cost(usage_by_model),
        );
    }
    // Usage and request history started at different releases, so each average
    // divides by the days its own tracker actually covers.
    let usage_days = app.tracked_usage_day_count() as u64;
    let request_days = app.tracked_request_day_count() as u64;
    let tracked_requests: u64 = app.daily_request_counts.values().sum();

    let mut average_row = UsageRow::new(
        ui_language.text("Avg/day", "每日平均").to_string(),
        (request_days > 0).then(|| div_round(tracked_requests, request_days)),
        average_totals(&tracked_totals, usage_days),
        average_cost(tracked_cost, usage_days),
    );
    average_row.per_call =
        format_average_usage_cost_usd(tracked_cost, tracked_totals.tool_call_count);
    let rows = [
        UsageRow::new(
            ui_language.text("Last 60s", "最近 60 秒").to_string(),
            None,
            rolling,
            CostEstimate {
                priced_usd: rolling_cost_usd,
                unpriced_tokens: 0,
            },
        ),
        UsageRow::new(
            ui_language.text("Session", "工作階段").to_string(),
            Some(app.request_count),
            app.session_usage_totals.clone(),
            CostEstimate {
                priced_usd: session_cost_usd,
                unpriced_tokens: 0,
            },
        ),
        UsageRow::new(
            ui_language.text("Today", "今日").to_string(),
            Some(app.today_request_count()),
            today_by_model
                .map(|usage| merged_totals(usage.values()))
                .unwrap_or_default(),
            today_by_model
                .map(usage_pricing::estimate_usage_by_model_cost)
                .unwrap_or_default(),
        ),
        average_row,
        UsageRow::new(
            ui_language.text("All-time", "累計").to_string(),
            Some(app.total_request_count),
            app.all_time_usage_totals(),
            usage_pricing::estimate_usage_by_model_cost(&app.usage_by_model),
        ),
    ];

    let headers = [
        ui_language.text("REQS", "請求"),
        ui_language.text("CALLS", "呼叫"),
        ui_language.text("↓REQ", "↓請求"),
        ui_language.text("↑RES", "↑回應"),
        ui_language.text("Σ TOK", "合計"),
        ui_language.text("COST", "成本"),
        ui_language.text("$/CALL", "$/次"),
    ];
    let mut header = vec![Span::styled(
        pad_label(ui_language.text("USAGE", "用量")),
        label_style,
    )];
    for (text, cells) in headers.iter().zip(COLUMN_CELLS) {
        header.push(Span::styled(pad_left(text, cells), header_style));
    }
    let mut lines = vec![Line::from(header)];

    for row in rows {
        let requests = row.requests.map(|count| count.to_string());
        let cells = [
            (requests.unwrap_or_else(|| "—".to_string()), value_style),
            (row.totals.tool_call_count.to_string(), value_style),
            (
                format_token_compact(row.totals.tool_input_tokens),
                value_style,
            ),
            (
                format_token_compact(row.totals.tool_output_tokens),
                value_style,
            ),
            (format_token_compact(row.totals.total_tokens), value_style),
            (format_cost_estimate_usd(row.cost), cost_style),
            (row.per_call, cost_style),
        ];
        let mut spans = vec![Span::styled(pad_label(&row.label), label_style)];
        for ((text, style), cells) in cells.into_iter().zip(COLUMN_CELLS) {
            let style = if text == "—" { muted_style } else { style };
            spans.push(Span::styled(pad_left(&text, cells), style));
        }
        lines.push(Line::from(spans));
    }

    let (live_per_min, live_per_hour) = session_cost_rates(
        rolling_cost_usd,
        Duration::from_millis(LIVE_USAGE_WINDOW_MS as u64),
    );
    let (_, session_per_hour) = session_cost_rates(session_cost_usd, session_elapsed);
    lines.push(Line::from(vec![
        Span::styled(pad_label(ui_language.text("BURN", "消耗速率")), label_style),
        Span::styled(ui_language.text("now ", "即時 "), muted_style),
        Span::styled(
            format!("${}/h", format_usd_short(live_per_hour)),
            cost_style,
        ),
        Span::styled(
            format!(" · ${}/min", format_usd_short(live_per_min)),
            muted_style,
        ),
        Span::styled(ui_language.text("   session ", "   本次 "), muted_style),
        Span::styled(
            format!("${}/h", format_usd_short(session_per_hour)),
            cost_style,
        ),
        Span::styled(
            format!(" · {}", format_session_duration(session_elapsed)),
            value_style,
        ),
    ]));

    // The two trackers started at different releases, so the Avg/day row
    // divides each by its own day count; this line names both.
    lines.push(Line::from(vec![
        Span::styled(pad_label(ui_language.text("TRACKED", "追蹤")), label_style),
        Span::styled(ui_language.text("usage ", "用量 "), muted_style),
        Span::styled(
            format!("{usage_days}{}", ui_language.text("d", "天")),
            value_style,
        ),
        Span::styled(" Σ ", muted_style),
        Span::styled(format_cost_estimate_usd(tracked_cost), cost_style),
        Span::styled(" · ", muted_style),
        Span::styled(
            format_token_compact(tracked_totals.total_tokens),
            value_style,
        ),
        Span::styled(ui_language.text(" tok", " Token"), muted_style),
        Span::styled(ui_language.text("  req ", "  請求 "), muted_style),
        Span::styled(
            format!("{request_days}{}", ui_language.text("d", "天")),
            value_style,
        ),
        Span::styled(" Σ ", muted_style),
        Span::styled(tracked_requests.to_string(), value_style),
    ]));

    lines.push(sparkline_line(
        app,
        ui_language,
        label_style,
        muted_style,
        value_style,
    ));
    lines
}

/// `LAST 7 DAYS  ▁▃·█▂▁▄  peak 2.6M 10-27 · Σ 5.6M` over Σ tokens per local
/// calendar day, oldest first, today last. Days without usage render as `·`.
fn sparkline_line(
    app: &AppState,
    ui_language: UiLanguage,
    label_style: Style,
    muted_style: Style,
    value_style: Style,
) -> Line<'static> {
    let now = local_now();
    let days: Vec<(String, u64)> = (0..SPARKLINE_DAYS)
        .rev()
        .map(|offset| {
            let key = local_usage_day_key(now - time::Duration::days(offset));
            let tokens = app
                .daily_usage_by_model
                .get(&key)
                .map(|usage| merged_totals(usage.values()).total_tokens)
                .unwrap_or_default();
            (key, tokens)
        })
        .collect();
    let label = Span::styled(
        pad_label(ui_language.text("LAST 7 DAYS", "最近 7 天")),
        label_style,
    );
    let Some((peak_day, peak)) = days
        .iter()
        .filter(|(_, tokens)| *tokens > 0)
        .max_by_key(|(_, tokens)| *tokens)
    else {
        return Line::from(vec![
            label,
            Span::styled(ui_language.text("no usage yet", "尚無用量"), muted_style),
        ]);
    };
    let sparkline: String = days
        .iter()
        .map(|(_, tokens)| sparkline_glyph(*tokens, *peak))
        .collect();
    let week_total: u64 = days.iter().map(|(_, tokens)| tokens).sum();
    Line::from(vec![
        label,
        Span::styled(sparkline, value_style),
        Span::styled(ui_language.text("  peak ", "  峰值 "), muted_style),
        Span::styled(format_token_compact(*peak), value_style),
        Span::styled(
            format!(" {}", peak_day.get(5..).unwrap_or(peak_day)),
            muted_style,
        ),
        Span::styled(" · Σ ", muted_style),
        Span::styled(format_token_compact(week_total), value_style),
    ])
}

fn sparkline_glyph(tokens: u64, peak: u64) -> char {
    if tokens == 0 || peak == 0 {
        return '·';
    }
    let level = (tokens as u128 * SPARKLINE_GLYPHS.len() as u128).div_ceil(peak as u128);
    SPARKLINE_GLYPHS[(level as usize).clamp(1, SPARKLINE_GLYPHS.len()) - 1]
}

fn merged_totals<'a>(usages: impl Iterator<Item = &'a UsageTotals>) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for usage in usages {
        totals.merge(usage);
    }
    totals
}

fn add_cost(total: &mut CostEstimate, day: CostEstimate) {
    total.priced_usd += day.priced_usd;
    total.unpriced_tokens = total.unpriced_tokens.saturating_add(day.unpriced_tokens);
}

fn div_round(value: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    value.saturating_add(count / 2) / count
}

fn average_totals(totals: &UsageTotals, days: u64) -> UsageTotals {
    let mut average = UsageTotals::default();
    average.accumulate(
        div_round(totals.tool_input_tokens, days),
        div_round(totals.tool_output_tokens, days),
        div_round(totals.tool_call_count, days),
    );
    average
}

fn average_cost(cost: CostEstimate, days: u64) -> CostEstimate {
    if days == 0 {
        return CostEstimate::default();
    }
    CostEstimate {
        priced_usd: cost.priced_usd / days as f64,
        unpriced_tokens: cost.unpriced_tokens.div_ceil(days),
    }
}

fn pad_label(label: &str) -> String {
    format!("  {}", pad_right_to_cell_width(label, LABEL_CELLS - 2))
}

/// Right-align `text` in `cells`, always keeping at least one separating
/// space so an overflowing value never glues onto the previous column.
fn pad_left(text: &str, cells: usize) -> String {
    format!(
        "{}{text}",
        " ".repeat(cells.saturating_sub(terminal_cell_width(text)).max(1))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_to_peak_and_marks_empty_days() {
        assert_eq!(sparkline_glyph(0, 100), '·');
        assert_eq!(sparkline_glyph(100, 100), '█');
        assert_eq!(sparkline_glyph(1, 100), '▁');
        assert_eq!(sparkline_glyph(50, 100), '▄');
    }

    #[test]
    fn overflowing_cells_keep_a_separating_space() {
        assert_eq!(pad_left("12", 6), "    12");
        assert_eq!(pad_left("1234567", 6), " 1234567");
    }

    #[test]
    fn averages_round_to_nearest_and_tolerate_zero_days() {
        assert_eq!(div_round(10, 4), 3);
        assert_eq!(div_round(9, 4), 2);
        assert_eq!(div_round(9, 0), 0);
        let cost = average_cost(
            CostEstimate {
                priced_usd: 9.0,
                unpriced_tokens: 5,
            },
            3,
        );
        assert_eq!(cost.priced_usd, 3.0);
        assert_eq!(cost.unpriced_tokens, 2);
    }
}
