use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use crate::mascot::{TUI_MASCOT_BLOCK_HEIGHT, TUI_MASCOT_BLOCK_WIDTH, render_tui_lines};
use crate::state::{AppState, FlowLane, LIVE_USAGE_WINDOW_MS};
use crate::perf_metrics;
use crate::usage_pricing;
use crate::tui::chrome::{draw_tui_header, render_toast};
use crate::tui::flow::{
    active_bootstrap_status_flow, flow_bootstrap_status_lines,
    flow_lane_left_label, flow_lane_spans, flow_turn_usage_spans, latest_flow_action,
    should_display_flow_row,
    should_show_connect_guide,
};
use crate::CHATGPT_CONNECTOR_SETTINGS_URL;
use crate::tui::logs::MCP_URL_MASK;
use crate::tui::logs::{LogView, localize_log_message, mask_secret_log_message, wrap_log_message};
use crate::tui::text::{
    pad_right_to_cell_width,
    format_average_usage_cost_usd, format_cost_estimate_usd, format_session_duration,
    format_token_compact, format_usd_compact, mcp_url_reveal_bar_segments,
    mcp_url_reveal_seconds, session_cost_rates, trim_line,
};

pub(crate) const STATUS_PANEL_HEIGHT: u16 = TUI_MASCOT_BLOCK_HEIGHT + 6;
pub(crate) const STATUS_LABEL_WIDTH: usize = 13;

pub(crate) fn draw_ui(
    f: &mut Frame,
    app: &AppState,
    active_job_count: usize,
    session_elapsed: Duration,
    log_scroll: usize,
    log_follow_tail: bool,
    log_view: &mut Option<LogView>,
    toast: Option<(&str, (u16, u16))>,
    mcp_url_reveal_remaining: Option<Duration>,
    log_secret_revealed_until: &HashMap<u64, Instant>,
) {
    let palette = app.current_theme().palette;
    let ui_language = app.ui_language;
    let area = f.area();
    let now_millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let has_url = app.ngrok_url.is_some();
    let visible_flow_count = app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow, app.remote_connected))
        .count() as u16;
    let show_guide = should_show_connect_guide(app, now_millis);
    let show_flow_panel = !show_guide;
    let bootstrap_status_flow = active_bootstrap_status_flow(app, now_millis);
    let logs_min_height = if show_guide { 3 } else { 5 };
    let max_status_height = area.height.saturating_sub(6 + logs_min_height).max(17);
    // Keep the main panel deterministic: mascot size must not drive layout.
    let status_height = STATUS_PANEL_HEIGHT.min(max_status_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(status_height),
            Constraint::Length(3),
            Constraint::Min(logs_min_height),
        ])
        .split(area);

    // ── Header ──
    draw_tui_header(
        f,
        chunks[0],
        &palette,
        ui_language.text(
            "CatDesk - Turns ChatGPT Web into a coding agent =w=",
            "CatDesk - 讓 ChatGPT Web 變成程式代理 =w=",
        ),
    );

    // ── Status ──
    let mode_label = app.mode.label_for(ui_language);
    let tool_mode_label = app.tool_mode.label_for(ui_language);
    let full_mcp_url = app.public_mcp_url();
    let mcp_url_is_revealed = full_mcp_url.is_some() && mcp_url_reveal_remaining.is_some();
    let mcp_url = match (&full_mcp_url, mcp_url_is_revealed) {
        (Some(url), true) => url.clone(),
        (Some(_), false) => MCP_URL_MASK.to_string(),
        (None, _) => "--".to_string(),
    };
    let mcp_url_security_status = mcp_url_reveal_remaining.map(|remaining| {
        let seconds = mcp_url_reveal_seconds(remaining);
        if ui_language.is_traditional_chinese() {
            format!("[ 已顯示 {:>2}秒 ]", seconds)
        } else {
            format!("[ EXPOSED {:>2}s ]", seconds)
        }
    });
    let computer_role_style = Style::default()
        .fg(if app.server_running {
            palette.success_fg
        } else {
            palette.muted_fg
        })
        .add_modifier(Modifier::BOLD);
    let chatgpt_role_style = Style::default()
        .fg(if app.remote_connected {
            palette.success_fg
        } else {
            palette.muted_fg
        })
        .add_modifier(Modifier::BOLD);
    let flow_meta_style = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);
    let lane_for = |active: bool, flow: Option<&FlowLane>| -> Vec<Span<'static>> {
        flow_lane_spans(active, flow, &palette, now_millis)
    };
    let status_label_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD);
    let status_label = |label: &'static str| -> Span<'static> {
        Span::styled(
            format!("  {} ", pad_right_to_cell_width(label, STATUS_LABEL_WIDTH)),
            status_label_style,
        )
    };
    let status_content_height = status_height.saturating_sub(4) as usize;
    let flow_block_lines = 1;

    let rolling_usage_totals = app.rolling_usage_totals(now_millis, LIVE_USAGE_WINDOW_MS);
    // Rolling, session, and flow totals only ever hold freshly recorded turns, so
    // the fallback estimate is the applicable rate for them.
    let rolling_usage_cost_usd = usage_pricing::estimate_usage_cost_usd(
        &rolling_usage_totals,
        &usage_pricing::FALLBACK_MODEL_PRICING,
    );
    let (live_cost_per_min_usd, live_cost_per_hour_usd) = session_cost_rates(
        rolling_usage_cost_usd,
        Duration::from_millis(LIVE_USAGE_WINDOW_MS as u64),
    );
    let session_usage_cost_usd = usage_pricing::estimate_usage_cost_usd(
        &app.session_usage_totals,
        &usage_pricing::FALLBACK_MODEL_PRICING,
    );
    let (_session_cost_per_min_usd, session_cost_per_hour_usd) =
        session_cost_rates(session_usage_cost_usd, session_elapsed);
    let all_time_usage_cost = usage_pricing::estimate_usage_by_model_cost(&app.usage_by_model);
    let today_usage_by_model = app.today_usage_by_model();
    let today_usage_cost = today_usage_by_model
        .map(usage_pricing::estimate_usage_by_model_cost)
        .unwrap_or_default();
    let today_tool_call_count = today_usage_by_model
        .map(|usage_by_model| {
            usage_by_model
                .values()
                .map(|usage| usage.tool_call_count)
                .sum::<u64>()
        })
        .unwrap_or_default();
    let tracked_usage_day_count = app.tracked_usage_day_count();
    let tracked_daily_usage_cost = app
        .daily_usage_by_model
        .values()
        .map(usage_pricing::estimate_usage_by_model_cost)
        .fold(usage_pricing::CostEstimate::default(), |mut total, day| {
            total.priced_usd += day.priced_usd;
            total.unpriced_tokens = total.unpriced_tokens.saturating_add(day.unpriced_tokens);
            total
        });
    let muted_style = Style::default().fg(palette.muted_fg);
    let value_style = Style::default()
        .fg(palette.secondary_fg)
        .add_modifier(Modifier::BOLD);
    let cost_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);
    let health_style = |healthy: bool| {
        Style::default()
            .fg(if healthy {
                palette.success_fg
            } else {
                palette.danger_fg
            })
            .add_modifier(Modifier::BOLD)
    };
    let devtools_indicator = if app.devtools_running {
        ("✓", Style::default().fg(palette.success_fg))
    } else if app.mode.browser_enabled() {
        ("×", Style::default().fg(palette.danger_fg))
    } else {
        ("-", Style::default().fg(palette.muted_fg))
    };
    // One bounded snapshot per redraw: copy under the lock, sort on the stack.
    let perf = perf_metrics::snapshot();

    let mut status_lines: Vec<Line> = vec![
        Line::from(vec![
            status_label(ui_language.text("REQ NOW", "即時請求")),
            Span::styled(format!("{} ", ui_language.text("CHATS", "聊天")), muted_style),
            Span::styled(app.connected_chat_count().to_string(), value_style),
            Span::styled(format!("      {} ", ui_language.text("JOBS", "工作")), muted_style),
            Span::styled(active_job_count.to_string(), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("REQ SESSION", "工作階段請求")),
            Span::styled(app.request_count.to_string(), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("REQ TOTAL", "累計請求")),
            Span::styled(app.total_request_count.to_string(), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("PERF", "效能")),
            Span::styled(perf_metrics::format_perf_line(&perf), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("SYS", "系統")),
            Span::styled(perf_metrics::format_system_line(&perf), muted_style),
        ]),
        Line::from(""),
        Line::from(vec![
            status_label(ui_language.text("COST NOW", "即時成本")),
            Span::styled(
                format!("${}/h", format_usd_compact(live_cost_per_hour_usd)),
                cost_style,
            ),
            Span::raw("      "),
            Span::styled(
                format!("${}/min", format_usd_compact(live_cost_per_min_usd)),
                cost_style,
            ),
        ]),
        Line::from(vec![
            status_label(ui_language.text("COST SESSION", "工作階段成本")),
            Span::styled(ui_language.text("AVG ", "平均 "), muted_style),
            Span::styled(
                format!("${}/h", format_usd_compact(session_cost_per_hour_usd)),
                cost_style,
            ),
            Span::styled(ui_language.text("      SPENT ", "      已花費 "), muted_style),
            Span::styled(
                format!("${}", format_usd_compact(session_usage_cost_usd)),
                cost_style,
            ),
            Span::raw("      "),
            Span::styled(format_session_duration(session_elapsed), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("COST TODAY", "今日成本")),
            Span::styled(ui_language.text("SPENT ", "已花費 "), muted_style),
            Span::styled(format_cost_estimate_usd(today_usage_cost), cost_style),
            Span::styled(ui_language.text("      AVG ", "      平均 "), muted_style),
            Span::styled(
                format!(
                    "{}{}",
                    format_average_usage_cost_usd(today_usage_cost, today_tool_call_count),
                    ui_language.text("/call", "/次")
                ),
                cost_style,
            ),
            Span::styled(ui_language.text("      CALLS ", "      呼叫 "), muted_style),
            Span::styled(today_tool_call_count.to_string(), value_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("COST TOTAL", "累計成本")),
            Span::styled(ui_language.text("SPENT ", "已花費 "), muted_style),
            Span::styled(format_cost_estimate_usd(all_time_usage_cost), cost_style),
        ]),
        Line::from(vec![
            status_label(ui_language.text("COST TRACKED", "追蹤成本")),
            Span::styled(ui_language.text("SPENT ", "已花費 "), muted_style),
            Span::styled(
                format_cost_estimate_usd(tracked_daily_usage_cost),
                cost_style,
            ),
            Span::styled(ui_language.text("      DAYS ", "      天數 "), muted_style),
            Span::styled(tracked_usage_day_count.to_string(), value_style),
            Span::styled(ui_language.text("      AVG ", "      平均 "), muted_style),
            Span::styled(
                format!(
                    "{}{}",
                    format_average_usage_cost_usd(
                        tracked_daily_usage_cost,
                        tracked_usage_day_count as u64,
                    ),
                    ui_language.text("/day", "/天")
                ),
                cost_style,
            ),
        ]),
        Line::from(vec![
            status_label(ui_language.text("TOKENS 60s", "Token 60秒")),
            Span::styled(ui_language.text("↓REQ", "↓請求"), muted_style),
            Span::styled(
                format_token_compact(rolling_usage_totals.tool_input_tokens),
                value_style,
            ),
            Span::raw("      "),
            Span::styled(ui_language.text("↑RES", "↑回應"), muted_style),
            Span::styled(
                format_token_compact(rolling_usage_totals.tool_output_tokens),
                value_style,
            ),
            Span::raw("      "),
            Span::styled("Σ", muted_style),
            Span::styled(
                format_token_compact(rolling_usage_totals.total_tokens),
                value_style,
            ),
        ]),
        Line::from(vec![
            status_label(ui_language.text("SYSTEM", "系統")),
            Span::styled("MCP ", muted_style),
            Span::styled(if app.server_running { "✓" } else { "×" }, health_style(app.server_running)),
            Span::styled("      NGROK ", muted_style),
            Span::styled(if app.ngrok_running { "✓" } else { "×" }, health_style(app.ngrok_running)),
            Span::styled("      DEVTOOLS ", muted_style),
            Span::styled(devtools_indicator.0, devtools_indicator.1.add_modifier(Modifier::BOLD)),
            Span::styled(format!("      {} ", ui_language.text("MODE", "模式")), muted_style),
            Span::styled(mode_label, value_style),
            Span::styled(" / ", muted_style),
            Span::styled(tool_mode_label, value_style),
        ]),
    ];

    let visible_flow_slots = if show_flow_panel {
        status_content_height.saturating_sub(status_lines.len() + 1) / flow_block_lines.max(1)
    } else {
        0
    };

    if show_flow_panel && visible_flow_slots > 0 {
        status_lines.push(Line::from(""));
        if visible_flow_count == 0 {
            let call_text = if app.remote_connected {
                ui_language.text("awaiting request", "等待請求")
            } else {
                ui_language.text("awaiting connection", "等待連線")
            };
            let lane = lane_for(false, None);
            let mut row = vec![
                Span::styled("    ", Style::default().fg(palette.muted_fg)),
                Span::styled(flow_lane_left_label(ui_language), computer_role_style),
            ];
            row.extend(lane);
            row.push(Span::styled("ChatGPT Web", chatgpt_role_style));
            row.push(Span::raw("   "));
            row.push(Span::styled(call_text, flow_meta_style));
            status_lines.push(Line::from(row));
        } else if let Some(flow) = app
            .flows
            .iter()
            .find(|flow| should_display_flow_row(flow, app.remote_connected))
        {
            let latest_action = latest_flow_action(flow);
            let call_text = trim_line(&latest_action, 36);
            let closing = flow.closing_started_ms.is_some();
            let lane_active = closing
                || !flow.anim_queue.is_empty()
                || (app.server_running && app.ngrok_running && app.remote_connected);
            let lane = lane_for(lane_active, Some(flow));
            let mut row = vec![
                Span::styled("    ", Style::default().fg(palette.muted_fg)),
                Span::styled(flow_lane_left_label(ui_language), computer_role_style),
            ];
            row.extend(lane);
            row.push(Span::styled("ChatGPT Web", chatgpt_role_style));
            row.push(Span::raw("   "));
            row.push(Span::styled(call_text, flow_meta_style));
            row.push(Span::raw("   "));
            row.extend(flow_turn_usage_spans(flow, ui_language, &palette));
            status_lines.push(Line::from(row));
        }
    }

    if let Some(flow) = bootstrap_status_flow {
        status_lines = flow_bootstrap_status_lines(app, flow, &palette, now_millis);
    }

    let guide_step_style = Style::default()
        .fg(palette.title_fg)
        .add_modifier(Modifier::BOLD);
    let guide_text_style = Style::default().fg(palette.primary_fg);
    let guide_detail_style = Style::default().fg(palette.secondary_fg);
    let guide_strong_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD);
    let guide_separator_style = Style::default().fg(palette.secondary_fg);
    let guide_copyable_style = Style::default()
        .fg(palette.primary_fg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let guide_lines = if show_guide {
        if app.is_returning_user {
            vec![
                Line::from(vec![
                    Span::styled("  ✅ ", guide_step_style),
                    Span::styled(
                        ui_language.text(
                            "Connection URL is fixed and ready!",
                            "連線 URL 已固定並準備完成！",
                        ),
                        guide_strong_style,
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        ui_language.text("     You do ", "     你"),
                        guide_text_style,
                    ),
                    Span::styled(ui_language.text("NOT", "不需要"), guide_strong_style),
                    Span::styled(
                        ui_language.text(
                            " need to recreate the app in ChatGPT.",
                            "在 ChatGPT 裡重新建立 App。",
                        ),
                        guide_text_style,
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    ui_language.text(
                        "     Simply go to your ChatGPT conversation and send a message.",
                        "     直接回到 ChatGPT 對話並傳送一則訊息即可。",
                    ),
                    guide_text_style,
                )]),
                Line::from(vec![Span::styled(
                    ui_language.text(
                        "     CatDesk will instantly connect and this screen will disappear.",
                        "     CatDesk 會立即連線，這個畫面也會自動消失。",
                    ),
                    guide_detail_style,
                )]),
            ]
        } else {
            vec![
                Line::from(vec![
                    Span::styled("  1. ", guide_step_style),
                    Span::styled(
                        ui_language.text("Open connector settings: ", "開啟 Connector 設定："),
                        guide_text_style,
                    ),
                    Span::styled(
                        ui_language.text("(click to copy)", "（點擊複製）"),
                        guide_detail_style,
                    ),
                ]),
                Line::from(vec![
                    Span::styled("     ", guide_text_style),
                    Span::styled(CHATGPT_CONNECTOR_SETTINGS_URL, guide_copyable_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  2. ", guide_step_style),
                    Span::styled(ui_language.text("Click ", "點擊 "), guide_text_style),
                    Span::styled("Create app", guide_strong_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  3. ", guide_step_style),
                    Span::styled(
                        ui_language.text("Fill in the form: ", "填寫表單："),
                        guide_text_style,
                    ),
                    Span::styled(
                        ui_language.text("(URL reveals before copy)", "（複製前會顯示 URL）"),
                        guide_detail_style,
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        ui_language.text("     Name          ", "     名稱          "),
                        guide_detail_style,
                    ),
                    Span::styled(" │ ", guide_separator_style),
                    Span::styled("CatDesk", guide_copyable_style),
                ]),
                {
                    let mut spans = vec![
                        Span::styled(
                            ui_language.text("     MCP Server URL", "     MCP 伺服器 URL"),
                            guide_detail_style,
                        ),
                        Span::styled(" │ ", guide_separator_style),
                        Span::styled(
                            mcp_url.clone(),
                            if mcp_url_is_revealed {
                                guide_copyable_style
                            } else {
                                guide_detail_style
                            },
                        ),
                    ];
                    if has_url {
                        spans.push(Span::raw("  "));
                        let security_text = mcp_url_security_status
                            .as_deref()
                            .unwrap_or(ui_language.text("Click to reveal", "點擊顯示"));
                        let security_color = match mcp_url_reveal_remaining {
                            Some(remaining) if mcp_url_reveal_seconds(remaining) <= 3 => {
                                palette.danger_fg
                            }
                            Some(_) => palette.warning_fg,
                            None => palette.muted_fg,
                        };
                        spans.push(Span::styled(
                            security_text.to_string(),
                            Style::default()
                                .fg(security_color)
                                .add_modifier(Modifier::BOLD),
                        ));
                        if let Some(remaining) = mcp_url_reveal_remaining {
                            let (remaining_bar, elapsed_bar) =
                                mcp_url_reveal_bar_segments(remaining);
                            spans.push(Span::raw("  "));
                            spans.push(Span::styled(
                                remaining_bar,
                                Style::default()
                                    .fg(security_color)
                                    .add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::styled(
                                elapsed_bar,
                                Style::default().fg(palette.muted_fg),
                            ));
                        }
                    }
                    Line::from(spans)
                },
                Line::from(vec![
                    Span::styled(
                        ui_language.text("     Authentication", "     驗證方式"),
                        guide_detail_style,
                    ),
                    Span::styled(" │ ", guide_separator_style),
                    Span::styled("None", guide_copyable_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  4. ", guide_step_style),
                    Span::styled(ui_language.text("Click ", "點擊 "), guide_text_style),
                    Span::styled("I understand and want to continue", guide_strong_style),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  5. ", guide_step_style),
                    Span::styled(ui_language.text("Click ", "點擊 "), guide_text_style),
                    Span::styled("Create", guide_strong_style),
                ]),
            ]
        }
    } else {
        Vec::new()
    };
    if show_guide {
        status_lines = guide_lines;
    }

    let show_mascot = area.width >= 120;
    let status_columns = if show_mascot {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(TUI_MASCOT_BLOCK_WIDTH),
            ])
            .split(chunks[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0)])
            .split(chunks[1])
    };
    let status_title = if show_guide {
        ui_language.text(" What to do next? ", " 接下來怎麼做？ ")
    } else if bootstrap_status_flow.is_some() {
        ui_language.text(" MCP bootstrap ", " MCP 初始化 ")
    } else {
        ui_language.text(" Status ", " 狀態 ")
    };
    let status_block = Block::default()
        .title(status_title)
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg));
    let status_inner = status_block.inner(status_columns[0]);
    f.render_widget(status_block, status_columns[0]);
    if show_mascot {
        let mascot_block = Block::default()
            .title(" Binagotchy ")
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg));
        let mascot_inner = mascot_block.inner(status_columns[1]);
        f.render_widget(mascot_block, status_columns[1]);
        let mascot = Paragraph::new(render_tui_lines(
            app.mascot.current_tui_frame(now_millis),
            mascot_inner.height,
        ))
        .alignment(Alignment::Center);
        f.render_widget(mascot, mascot_inner);
    }

    let status_content = status_inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let status = if show_guide || bootstrap_status_flow.is_some() {
        Paragraph::new(status_lines).wrap(Wrap { trim: false })
    } else {
        Paragraph::new(status_lines)
    };
    f.render_widget(status, status_content);

    // ── Keys ──
    let key_spans = vec![
        Span::styled("  [q]", Style::default().fg(palette.danger_fg)),
        Span::raw(ui_language.text(" Quit  ", " 離開  ")),
        Span::styled("[Up/Down/Wheel]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(" Scroll  ", " 捲動  ")),
        Span::styled("[End]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(" Latest  ", " 最新  ")),
        Span::styled("[e]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(" Export logs", " 匯出紀錄")),
    ];
    let keys = Paragraph::new(Line::from(key_spans)).block(
        Block::default()
            .title(ui_language.text(" Keys ", " 按鍵 "))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(keys, chunks[2]);

    // ── Logs ──
    const LOG_PREFIX_WIDTH: usize = 16;
    let log_content_width = chunks[3].width.saturating_sub(2) as usize;
    let message_width = log_content_width.saturating_sub(LOG_PREFIX_WIDTH).max(1);
    let mut log_rows: Vec<(u64, ListItem<'static>)> = Vec::new();
    for entry in &app.logs {
        let color = match entry.level {
            "ERROR" => palette.danger_fg,
            "WARN" => palette.warning_fg,
            _ => palette.muted_fg,
        };
        let message = mask_secret_log_message(
            &entry.message,
            log_secret_revealed_until.contains_key(&entry.id),
        );
        let message = localize_log_message(&message, ui_language);
        let wrapped = wrap_log_message(&message, message_width);
        for (index, line) in wrapped.into_iter().enumerate() {
            let item = if index == 0 {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {} ", entry.time),
                        Style::default().fg(palette.muted_fg),
                    ),
                    Span::styled(format!("{:5} ", entry.level), Style::default().fg(color)),
                    Span::styled(line, Style::default().fg(palette.primary_fg)),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::raw(" ".repeat(LOG_PREFIX_WIDTH)),
                    Span::styled(line, Style::default().fg(palette.primary_fg)),
                ]))
            };
            log_rows.push((entry.id, item));
        }
    }

    let visible_height = chunks[3].height.saturating_sub(2) as usize;
    let total = log_rows.len();
    let max_scroll = total.saturating_sub(visible_height);
    let effective_scroll = if log_follow_tail {
        max_scroll
    } else {
        log_scroll.min(max_scroll)
    };
    let visible_log_ids = log_rows
        .iter()
        .skip(effective_scroll)
        .take(visible_height)
        .map(|(log_id, _)| *log_id)
        .collect();
    *log_view = Some(LogView {
        max_scroll,
        effective_scroll,
        area: chunks[3],
        visible_log_ids,
    });
    let visible_items: Vec<ListItem> = log_rows
        .into_iter()
        .skip(effective_scroll)
        .take(visible_height)
        .map(|(_, item)| item)
        .collect();
    let logs = List::new(visible_items).block(
        Block::default()
            .title(ui_language.text(" Logs ", " 紀錄 "))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(logs, chunks[3]);

    // ── Floating toast (top-most layer) ──
    if let Some((msg, pos)) = toast {
        render_toast(f, palette, msg, pos);
    }
}

