use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::SystemTime;

use crate::state::{
    AppState, FLOW_ANIM_CELLS, FlowAnimKind, FlowAnimSegment, FlowDirection, FlowLane,
    ShowDetailMode, UiLanguage, flow_anim_lit_count,
};
use crate::REMOTE_CONNECT_UI_GRACE_MS;
use crate::tui::text::{
    format_token_compact, format_usd_compact, terminal_cell_width, trim_line,
};
use crate::theme;
use crate::usage_pricing;

pub(crate) const FLOW_ROW_CELLS: usize = FLOW_ANIM_CELLS;
pub(crate) const FLOW_LANE_LEFT_LABEL: &str = "Your computer ";

pub(crate) fn current_anim_segment(flow: &FlowLane, now_millis: u128) -> Option<FlowAnimSegment> {
    if let Some(seg) = flow
        .anim_queue
        .iter()
        .find(|seg| seg.started_ms <= now_millis && now_millis < seg.ends_ms)
    {
        return Some(*seg);
    }
    flow.anim_queue.front().copied()
}

pub(crate) fn should_display_flow_row(flow: &FlowLane, remote_connected: bool) -> bool {
    remote_connected || flow.closing_started_ms.is_some() || !flow.anim_queue.is_empty()
}

pub(crate) fn flow_direction(flow: Option<&FlowLane>, now_millis: u128) -> FlowDirection {
    if let Some(flow) = flow {
        if let Some(seg) = current_anim_segment(flow, now_millis) {
            return seg.direction;
        }
        return flow.last_direction;
    }
    FlowDirection::Forward
}

pub(crate) fn flow_lit_count(flow: Option<&FlowLane>, now_millis: u128, cells: usize) -> usize {
    let Some(flow) = flow else {
        return 0;
    };
    if flow.closing_started_ms.is_some() {
        return 0;
    }
    current_anim_segment(flow, now_millis)
        .map(|seg| flow_anim_lit_count(seg, now_millis).min(cells))
        .unwrap_or(0)
}

pub(crate) fn debug_lane(direction: Option<FlowDirection>, lit_count: usize, cells: usize) -> String {
    let mut out = String::with_capacity(cells);
    for i in 0..cells {
        let lit_here = match direction {
            Some(FlowDirection::Forward) => lit_count > 0 && i < lit_count,
            Some(FlowDirection::Backward) => lit_count > 0 && i >= cells.saturating_sub(lit_count),
            None => false,
        };
        out.push(if lit_here { '#' } else { '-' });
    }
    out
}

pub(crate) fn flow_lane_spans(
    active: bool,
    flow: Option<&FlowLane>,
    palette: &theme::Palette,
    now_millis: u128,
) -> Vec<Span<'static>> {
    const CELLS: usize = FLOW_ROW_CELLS;
    let unlit = Style::default().fg(palette.muted_fg);
    let lit = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);

    let direction = flow.map(|flow| flow_direction(Some(flow), now_millis));
    let lit_count = if active {
        flow_lit_count(flow, now_millis, CELLS)
    } else {
        0
    };

    if lit_count == 0 || direction.is_none() {
        return vec![Span::styled("─".repeat(CELLS), unlit), Span::raw(" ")];
    }

    let direction = direction.unwrap_or(FlowDirection::Forward);
    let mut spans = Vec::with_capacity(CELLS + 1);
    for i in 0..CELLS {
        let lit_here = match direction {
            FlowDirection::Forward => i < lit_count,
            FlowDirection::Backward => i >= CELLS.saturating_sub(lit_count),
        };
        let style = if lit_here { lit } else { unlit };
        spans.push(Span::styled("─".to_string(), style));
    }
    spans.push(Span::raw(" "));
    spans
}

pub(crate) fn flow_lane_left_label(ui_language: UiLanguage) -> &'static str {
    ui_language.text(FLOW_LANE_LEFT_LABEL, "你的電腦 ")
}

pub(crate) fn flow_call_offset(text: &str, left_label: &str) -> String {
    let text_width = terminal_cell_width(text);
    let centered_in_lane = FLOW_ROW_CELLS.saturating_sub(text_width) / 2;
    " ".repeat(terminal_cell_width(left_label) + centered_in_lane)
}

pub(crate) fn flow_turn_usage_spans(
    flow: &FlowLane,
    ui_language: UiLanguage,
    palette: &theme::Palette,
) -> Vec<Span<'static>> {
    let label_style = Style::default().fg(palette.muted_fg);
    let value_style = Style::default()
        .fg(palette.secondary_fg)
        .add_modifier(Modifier::BOLD);
    let price_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);
    let request_label = ui_language.text("↓REQ", "↓請求");
    let response_label = ui_language.text("↑RES", "↑回應");

    match flow.turn_usage.as_ref() {
        Some(usage) => {
            let input = format_token_compact(usage.tool_input_tokens);
            let output = format_token_compact(usage.tool_output_tokens);
            // Flow usage is always freshly recorded turns, priced at the fallback
            // estimate until per-turn model metadata exists.
            let cost = format_usd_compact(usage_pricing::estimate_usage_cost_usd(
                usage,
                &usage_pricing::FALLBACK_MODEL_PRICING,
            ));
            vec![
                Span::styled(request_label, label_style),
                Span::raw(" "),
                Span::styled(input, value_style),
                Span::raw(" "),
                Span::styled(response_label, label_style),
                Span::raw(" "),
                Span::styled(output, value_style),
                Span::raw(" "),
                Span::styled("$", label_style),
                Span::styled(cost, price_style),
            ]
        }
        None => vec![Span::styled(
            ui_language.text("↓REQ -- ↑RES -- $--", "↓請求 -- ↑回應 -- $--"),
            label_style,
        )],
    }
}

pub(crate) fn flow_phase(flow: &FlowLane, now_millis: u128) -> &'static str {
    if flow.closing_started_ms.is_some() {
        return "close";
    }
    if let Some(seg) = current_anim_segment(flow, now_millis) {
        return match seg.kind {
            FlowAnimKind::Turn => "turn",
            FlowAnimKind::Move => match seg.direction {
                FlowDirection::Forward => "request",
                FlowDirection::Backward => "response",
            },
        };
    }
    "idle"
}

pub(crate) fn latest_flow_action(flow: &FlowLane) -> String {
    flow.events
        .iter()
        .rev()
        .find_map(|event| {
            if let Some(tool) = event.strip_prefix("tools/call:") {
                if tool.is_empty() {
                    None
                } else {
                    Some(tool.to_string())
                }
            } else if event.is_empty() {
                None
            } else {
                Some(event.clone())
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowPhaseStepState {
    Future,
    Pending,
    Complete,
}

pub(crate) struct FlowPhaseStepView {
    label: String,
    state: FlowPhaseStepState,
}

pub(crate) struct FlowPhaseView {
    title: &'static str,
    complete: bool,
    steps: Vec<FlowPhaseStepView>,
}

pub(crate) fn flow_event_pending(flow: &FlowLane, event: &str, now_millis: u128) -> bool {
    current_anim_segment(flow, now_millis).is_some() && latest_flow_action(flow) == event
}

pub(crate) fn flow_phase_step_view(
    flow: Option<&FlowLane>,
    event: &str,
    label: String,
    complete: bool,
    now_millis: u128,
) -> FlowPhaseStepView {
    let state = if complete {
        FlowPhaseStepState::Complete
    } else if flow.is_some_and(|flow| flow_event_pending(flow, event, now_millis)) {
        FlowPhaseStepState::Pending
    } else {
        FlowPhaseStepState::Future
    };
    FlowPhaseStepView { label, state }
}

pub(crate) fn flow_phase_views(
    flow: Option<&FlowLane>,
    mode: ShowDetailMode,
    ui_language: UiLanguage,
    now_millis: u128,
) -> Vec<FlowPhaseView> {
    let discover_complete = flow.is_some_and(|flow| flow.bootstrap_progress.discover_complete);
    let tools_list_complete = flow.is_some_and(|flow| flow.bootstrap_progress.tools_list_complete);
    let mut phases = vec![FlowPhaseView {
        title: ui_language.text("Connecting", "連線中"),
        complete: discover_complete && tools_list_complete,
        steps: vec![
            flow_phase_step_view(
                flow,
                "server/discover",
                "discover".to_string(),
                discover_complete,
                now_millis,
            ),
            flow_phase_step_view(
                flow,
                "tools/list",
                "tools/list".to_string(),
                tools_list_complete,
                now_millis,
            ),
        ],
    }];

    if mode != ShowDetailMode::Disable {
        let steps = flow
            .map(|flow| {
                flow.bootstrap_progress
                    .expected_widgets
                    .iter()
                    .map(|widget| {
                        let event = format!("resources/read:{}", widget.tool_name);
                        flow_phase_step_view(
                            Some(flow),
                            &event,
                            widget.label.clone(),
                            flow.bootstrap_progress
                                .loaded_widget_tool_names
                                .contains(&widget.tool_name),
                            now_millis,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let complete = flow.is_some_and(|flow| {
            flow.bootstrap_progress.tools_list_complete
                && flow.bootstrap_progress.widgets_complete()
        });
        phases.push(FlowPhaseView {
            title: ui_language.text("Loading widgets", "載入 Widget"),
            complete,
            steps,
        });
    }

    phases
}

pub(crate) fn flow_phase_status_label(phase: &FlowPhaseView) -> Option<String> {
    if phase.complete {
        return Some("✓".to_string());
    }
    if let Some(step) = phase
        .steps
        .iter()
        .find(|step| step.state == FlowPhaseStepState::Pending)
    {
        return Some(step.label.clone());
    }
    phase
        .steps
        .iter()
        .rev()
        .find(|step| step.state == FlowPhaseStepState::Complete)
        .map(|step| step.label.clone())
}

pub(crate) fn flow_phase_lines(
    flow: Option<&FlowLane>,
    mode: ShowDetailMode,
    palette: &theme::Palette,
    status_style: Style,
    ui_language: UiLanguage,
    now_millis: u128,
) -> Vec<Line<'static>> {
    const TITLE_STATUS_GAP: usize = 4;
    const STATUS_ANIM_GAP: usize = 4;
    let phases = flow_phase_views(flow, mode, ui_language, now_millis);
    let title_width = phases
        .iter()
        .enumerate()
        .map(|(phase_index, phase)| {
            format!(
                "    {} {}  {}",
                ui_language.text("Phase", "階段"),
                phase_index + 1,
                phase.title
            )
        })
        .map(|title| terminal_cell_width(&title))
        .max()
        .unwrap_or(0);
    let status_width = phases
        .iter()
        .flat_map(|phase| {
            std::iter::once("✓".to_string())
                .chain(phase.steps.iter().map(|step| step.label.clone()))
                .map(|status| terminal_cell_width(&format!("[{status}]")))
        })
        .max()
        .unwrap_or(0);
    let pending_style = Style::default()
        .fg(palette.info_fg)
        .add_modifier(Modifier::BOLD);
    let complete_style = Style::default()
        .fg(palette.success_fg)
        .add_modifier(Modifier::BOLD);
    let future_style = Style::default().fg(palette.muted_fg);
    let label_style = Style::default().fg(palette.primary_fg);

    phases
        .iter()
        .enumerate()
        .map(|(phase_index, phase)| {
            let title = format!(
                "    {} {}  {}",
                ui_language.text("Phase", "階段"),
                phase_index + 1,
                phase.title
            );
            let title_padding = title_width.saturating_sub(terminal_cell_width(&title));
            let status_text = flow_phase_status_label(phase)
                .map(|label| format!("[{label}]"))
                .unwrap_or_default();
            let status_padding = status_width.saturating_sub(terminal_cell_width(&status_text));
            let mut spans = vec![
                Span::styled(title, label_style),
                Span::styled(" ".repeat(title_padding + TITLE_STATUS_GAP), future_style),
                Span::styled(status_text, status_style),
                Span::styled(" ".repeat(status_padding + STATUS_ANIM_GAP), future_style),
            ];
            for (step_offset, step) in phase.steps.iter().enumerate() {
                if step_offset > 0 {
                    spans.push(Span::raw(" "));
                }
                match step.state {
                    FlowPhaseStepState::Future => {
                        spans.push(Span::styled("✧", future_style));
                    }
                    FlowPhaseStepState::Pending => {
                        spans.push(Span::styled("✧", pending_style));
                    }
                    FlowPhaseStepState::Complete => {
                        spans.push(Span::styled("✦", complete_style));
                    }
                }
            }
            Line::from(spans)
        })
        .collect()
}

pub(crate) fn flow_bootstrap_complete(flow: &FlowLane) -> bool {
    flow.bootstrap_progress.is_complete()
}

pub(crate) fn flow_bootstrap_status_visible(flow: &FlowLane, now_millis: u128) -> bool {
    if !flow_bootstrap_complete(flow) {
        return true;
    }
    if current_anim_segment(flow, now_millis).is_some() {
        return true;
    }
    flow.bootstrap_status_close_deadline_ms
        .is_some_and(|deadline| now_millis < deadline)
}

pub(crate) fn flow_bootstrap_countdown_remaining_seconds(flow: &FlowLane, now_millis: u128) -> Option<u128> {
    let deadline = flow.bootstrap_status_close_deadline_ms?;
    if now_millis >= deadline {
        return Some(0);
    }
    Some((deadline.saturating_sub(now_millis) + 999) / 1000)
}

pub(crate) fn active_bootstrap_status_flow<'a>(app: &'a AppState, now_millis: u128) -> Option<&'a FlowLane> {
    app.flows.iter().find(|flow| {
        should_display_flow_row(flow, app.remote_connected)
            && flow.bootstrap_status_active
            && flow.closing_started_ms.is_none()
            && flow_bootstrap_status_visible(flow, now_millis)
    })
}

pub(crate) fn should_show_connect_guide(app: &AppState, now_millis: u128) -> bool {
    let both_running = app.server_running && app.ngrok_running;
    let has_url = app.ngrok_url.is_some();
    let visible_flow_count = app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow, app.remote_connected))
        .count() as u16;
    let within_connect_grace = app
        .last_remote_activity_ms
        .map(|t| now_millis.saturating_sub(t) < REMOTE_CONNECT_UI_GRACE_MS)
        .unwrap_or(false);
    !app.is_returning_user
        && both_running
        && has_url
        && !app.remote_connected
        && visible_flow_count == 0
        && !within_connect_grace
}

pub(crate) fn flow_bootstrap_status_lines(
    app: &AppState,
    flow: &FlowLane,
    palette: &theme::Palette,
    now_millis: u128,
) -> Vec<Line<'static>> {
    let action_label = latest_flow_action(flow);
    let bootstrap_complete = flow_bootstrap_complete(flow);
    let ui_language = app.ui_language;
    let header_title = if bootstrap_complete {
        ui_language.text("Bootstrap completed", "初始化完成")
    } else {
        ui_language.text("Connector bootstrap in progress", "Connector 初始化進行中")
    };
    let call_text = trim_line(&action_label, FLOW_ROW_CELLS);
    let call_offset = flow_call_offset(&call_text, flow_lane_left_label(ui_language));

    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {header_title}"),
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", Style::default().fg(palette.muted_fg)),
            Span::styled(call_offset, Style::default().fg(palette.muted_fg)),
            Span::styled(
                call_text,
                Style::default()
                    .fg(palette.info_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from({
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
            let mut row = vec![Span::styled(
                format!("  {}", flow_lane_left_label(ui_language)),
                computer_role_style,
            )];
            row.extend(flow_lane_spans(true, Some(flow), palette, now_millis));
            row.push(Span::styled("ChatGPT Web", chatgpt_role_style));
            row
        }),
        Line::from(""),
    ];
    lines.extend(flow_phase_lines(
        Some(flow),
        app.show_detail_mode,
        palette,
        Style::default()
            .fg(palette.info_fg)
            .add_modifier(Modifier::BOLD),
        ui_language,
        now_millis,
    ));
    lines.push(Line::from(""));

    let footer_text = if bootstrap_complete && current_anim_segment(flow, now_millis).is_none() {
        match flow_bootstrap_countdown_remaining_seconds(flow, now_millis) {
            Some(0) => ui_language.text("Completed.", "已完成。").to_string(),
            Some(seconds) => {
                if ui_language.is_traditional_chinese() {
                    format!("已完成，{seconds} 秒後關閉...")
                } else {
                    format!("Completed. Closing in {seconds}s...")
                }
            }
            None => ui_language.text("Completed.", "已完成。").to_string(),
        }
    } else {
        ui_language
            .text(
                "Auto closes after bootstrap is completed.",
                "初始化完成後會自動關閉。",
            )
            .to_string()
    };
    lines.push(Line::from(Span::styled(
        format!("  {footer_text}"),
        Style::default().fg(palette.muted_fg),
    )));
    lines
}

pub(crate) fn build_animation_snapshot(app: &AppState) -> Vec<String> {
    if app.flows.is_empty() {
        return Vec::new();
    }
    let now_millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut rows = Vec::new();
    for flow in app
        .flows
        .iter()
        .filter(|flow| should_display_flow_row(flow, app.remote_connected))
    {
        let latest_action = latest_flow_action(flow);
        let closing = flow.closing_started_ms.is_some();
        let lane_active = closing
            || !flow.anim_queue.is_empty()
            || (app.server_running && app.ngrok_running && app.remote_connected);
        let direction = Some(flow_direction(Some(flow), now_millis)).filter(|_| lane_active);
        let phase = flow_phase(flow, now_millis);
        let lit = flow_lit_count(Some(flow), now_millis, FLOW_ROW_CELLS);
        let lane = debug_lane(direction, lit, FLOW_ROW_CELLS);
        rows.push(format!(
            "flow {} phase={:<8} tool={:<16} Your computer {} ChatGPT Web (via Ngrok)",
            flow.short_id, phase, latest_action, lane
        ));
    }
    if rows.is_empty() {
        return Vec::new();
    }
    rows
}
