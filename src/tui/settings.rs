use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::{
    SharedState, ShowDetailMode, ToolMode, UiLanguage, UsageTotals, WidgetCornerStyle,
    load_app_config, save_widget_corner_style,
};
use crate::theme;

use crate::run_prompt;
use crate::UI_POLL_INTERVAL;
use crate::tui::chrome::draw_tui_header;

pub(crate) async fn run_settings(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<(), Box<dyn std::error::Error>> {
    let themes = theme::all();
    let tool_modes = ToolMode::all();
    let show_detail_modes = ShowDetailMode::all();
    let widget_corner_styles = WidgetCornerStyle::all();
    let mut current_widget_corner_style = load_app_config()
        .map(|config| config.widget_corner_style)
        .unwrap_or_default();
    let mut confirm_reset_token_billing = false;
    let mut selected_row = {
        let app = state.lock().await;
        themes.iter().position(|t| t.id == app.theme).unwrap_or(0)
    };
    let total_rows =
        themes.len() + tool_modes.len() + show_detail_modes.len() + widget_corner_styles.len() + 4;

    let mut redraw = true;
    loop {
        let (
            current_theme,
            current_tool_mode,
            current_show_detail_mode,
            current_ui_language,
            usage_totals,
            set_catdesk_as_co_author,
            mcp_slug,
            ngrok_domain,
        ) = {
            let app = state.lock().await;
            (
                app.current_theme(),
                app.tool_mode,
                app.show_detail_mode,
                app.ui_language,
                app.all_time_usage_totals(),
                app.set_catdesk_as_co_author,
                app.mcp_slug.clone(),
                app.ngrok_domain.clone(),
            )
        };
        if redraw {
            terminal.draw(|f| {
                draw_settings(
                    f,
                    current_theme,
                    current_tool_mode,
                    current_show_detail_mode,
                    current_widget_corner_style,
                    current_ui_language,
                    set_catdesk_as_co_author,
                    &mcp_slug,
                    ngrok_domain.as_deref(),
                    &usage_totals,
                    selected_row,
                    confirm_reset_token_billing,
                )
            })?;
            redraw = false;
        }

        if event::poll(UI_POLL_INTERVAL)? {
            redraw = true;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Up => {
                        confirm_reset_token_billing = false;
                        selected_row = selected_row.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        confirm_reset_token_billing = false;
                        if selected_row + 1 < total_rows {
                            selected_row += 1;
                        }
                    }
                    KeyCode::Enter => {
                        confirm_reset_token_billing = false;
                        let mut app = state.lock().await;
                        if selected_row < themes.len() {
                            let picked = themes[selected_row];
                            if app.theme != picked.id {
                                app.theme = picked.id.to_string();
                                app.log("INFO", format!("Theme changed to {}", picked.label));
                                app.persist_state_with_log();
                            }
                        } else {
                            let tool_mode_start = themes.len();
                            let tool_mode_end = tool_mode_start + tool_modes.len();
                            let detail_mode_start = tool_mode_end;
                            let detail_mode_end = detail_mode_start + show_detail_modes.len();

                            if selected_row < tool_mode_end {
                                let picked = tool_modes[selected_row - tool_mode_start];
                                if app.tool_mode != picked {
                                    app.tool_mode = picked;
                                    app.log("INFO", format!("Tool mode: {}", picked.label()));
                                    app.persist_state_with_log();
                                }
                            } else if selected_row < detail_mode_end {
                                let picked = show_detail_modes[selected_row - detail_mode_start];
                                if app.show_detail_mode != picked {
                                    app.show_detail_mode = picked;
                                    app.log(
                                        "INFO",
                                        format!("Widget detail mode: {}", picked.label()),
                                    );
                                    app.persist_state_with_log();
                                }
                            } else {
                                let corner_start = detail_mode_end;
                                let corner_end = corner_start + widget_corner_styles.len();
                                if selected_row < corner_end {
                                    let picked = widget_corner_styles[selected_row - corner_start];
                                    if current_widget_corner_style != picked {
                                        match save_widget_corner_style(picked) {
                                            Ok(_) => {
                                                current_widget_corner_style = picked;
                                                app.log(
                                                    "INFO",
                                                    format!(
                                                        "Widget corner style: {}",
                                                        picked.label_for(current_ui_language)
                                                    ),
                                                );
                                            }
                                            Err(error) => {
                                                app.log(
                                                    "ERROR",
                                                    format!(
                                                        "Failed to save widget corner style: {error}"
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                } else if selected_row == corner_end {
                                    app.set_catdesk_as_co_author = !app.set_catdesk_as_co_author;
                                    let enabled = app.set_catdesk_as_co_author;
                                    app.log(
                                        "INFO",
                                        format!(
                                            "Set CatDesk as co-author: {}",
                                            if enabled { "enabled" } else { "disabled" }
                                        ),
                                    );
                                    app.persist_state_with_log();
                                } else if selected_row == corner_end + 1 {
                                    // Keep existing slug, do nothing
                                } else if selected_row == corner_end + 2 {
                                    app.regenerate_mcp_slug();
                                    app.log("INFO", "Generated new random MCP slug".into());
                                    app.persist_state_with_log();
                                } else if selected_row == corner_end + 3 {
                                    let current_domain =
                                        app.ngrok_domain.clone().unwrap_or_default();
                                    drop(app);
                                    if let Some(new_domain) = run_prompt(
                                        terminal,
                                        current_ui_language.text(
                                            "Enter ngrok static domain (with/without https://, empty to clear):",
                                            "輸入 ngrok 固定網域（可含或不含 https://，留空可清除）：",
                                        ),
                                        &current_domain,
                                    )
                                    .await?
                                    {
                                        let mut cleaned = new_domain.trim();
                                        if let Some(stripped) = cleaned.strip_prefix("https://") {
                                            cleaned = stripped;
                                        } else if let Some(stripped) = cleaned.strip_prefix("http://") {
                                            cleaned = stripped;
                                        }
                                        cleaned = cleaned.trim_end_matches('/');
                                        let mut app = state.lock().await;
                                        app.ngrok_domain = if cleaned.is_empty() { None } else { Some(cleaned.to_string()) };
                                        app.log("INFO", "Updated ngrok static domain".into());
                                        app.persist_state_with_log();
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if !confirm_reset_token_billing {
                            confirm_reset_token_billing = true;
                            continue;
                        }
                        let mut app = state.lock().await;
                        app.reset_usage_billing();
                        app.log("INFO", "Token billing totals reset".into());
                        app.persist_state_with_log();
                        confirm_reset_token_billing = false;
                    }
                    _ => {
                        confirm_reset_token_billing = false;
                    }
                }
            }
        }
    }
}

pub(crate) fn draw_settings(
    f: &mut Frame,
    current_theme: &theme::ThemeDef,
    current_tool_mode: ToolMode,
    current_show_detail_mode: ShowDetailMode,
    current_widget_corner_style: WidgetCornerStyle,
    ui_language: UiLanguage,
    set_catdesk_as_co_author: bool,
    mcp_slug: &str,
    ngrok_domain: Option<&str>,
    usage_totals: &UsageTotals,
    selected_row: usize,
    confirm_reset_token_billing: bool,
) {
    let themes = theme::all();
    let tool_modes = ToolMode::all();
    let show_detail_modes = ShowDetailMode::all();
    let widget_corner_styles = WidgetCornerStyle::all();
    let palette = current_theme.palette;
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    draw_tui_header(f, chunks[0], &palette, ui_language.text("Settings", "設定"));

    let mut selected_line_idx = 0;
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            ui_language.text("  Choose a theme", "  選擇主題"),
            Style::default()
                .fg(palette.title_fg)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (idx, theme) in themes.iter().enumerate() {
        let selected = idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(
                " {} [{}] {}",
                marker,
                idx + 1,
                theme.label_for(ui_language.is_traditional_chinese())
            ),
            name_style,
        )];
        if theme.id == current_theme.id {
            spans.push(Span::styled(
                ui_language.text("  [current]", "  [目前]"),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!(
                "     {}",
                theme.description_for(ui_language.is_traditional_chinese())
            ),
            Style::default().fg(palette.muted_fg),
        )]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text("  Choose a tool mode", "  選擇工具模式"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    for (idx, tool_mode) in tool_modes.iter().enumerate() {
        let row_idx = themes.len() + idx;
        let selected = row_idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(
                " {} [{}] {}",
                marker,
                row_idx + 1,
                tool_mode.label_for(ui_language)
            ),
            name_style,
        )];
        if *tool_mode == current_tool_mode {
            spans.push(Span::styled(
                ui_language.text("  [current]", "  [目前]"),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!("     {}", tool_mode.description_for(ui_language)),
            Style::default().fg(palette.muted_fg),
        )]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text("  Choose a widget detail mode", "  選擇 Widget 詳細程度"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    for (idx, detail_mode) in show_detail_modes.iter().enumerate() {
        let row_idx = themes.len() + tool_modes.len() + idx;
        let selected = row_idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(
                " {} [{}] {}",
                marker,
                row_idx + 1,
                detail_mode.label_for(ui_language)
            ),
            name_style,
        )];
        if *detail_mode == current_show_detail_mode {
            spans.push(Span::styled(
                ui_language.text("  [current]", "  [目前]"),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!("     {}", detail_mode.description_for(ui_language)),
            Style::default().fg(palette.muted_fg),
        )]));
    }

    let widget_corner_start = themes.len() + tool_modes.len() + show_detail_modes.len();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text("  Choose a widget corner style", "  選擇 Widget 邊角樣式"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    for (idx, corner_style) in widget_corner_styles.iter().enumerate() {
        let row_idx = widget_corner_start + idx;
        let selected = row_idx == selected_row;
        let marker = if selected { ">" } else { " " };
        let name_style = if selected {
            Style::default()
                .fg(palette.key_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.primary_fg)
        };
        lines.push(Line::from(""));
        if selected {
            selected_line_idx = lines.len();
        }
        let mut spans = vec![Span::styled(
            format!(
                " {} [{}] {}",
                marker,
                row_idx + 1,
                corner_style.label_for(ui_language)
            ),
            name_style,
        )];
        if *corner_style == current_widget_corner_style {
            spans.push(Span::styled(
                ui_language.text("  [current]", "  [目前]"),
                Style::default()
                    .fg(palette.secondary_fg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![Span::styled(
            format!("     {}", corner_style.description_for(ui_language)),
            Style::default().fg(palette.muted_fg),
        )]));
    }

    let co_author_row = widget_corner_start + widget_corner_styles.len();
    let co_author_selected = co_author_row == selected_row;
    let co_author_marker = if co_author_selected { ">" } else { " " };
    let co_author_name_style = if co_author_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text("  Commit attribution", "  Commit 署名"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    if co_author_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] {}",
            co_author_marker,
            co_author_row + 1,
            ui_language.text("Set CatDesk as co-author", "將 CatDesk 設為共同作者")
        ),
        co_author_name_style,
    )]));
    lines.push(Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            if set_catdesk_as_co_author {
                ui_language.text("[enabled]", "[已啟用]")
            } else {
                ui_language.text("[disabled]", "[已停用]")
            },
            Style::default().fg(if set_catdesk_as_co_author {
                palette.success_fg
            } else {
                palette.muted_fg
            }),
        ),
    ]));

    lines.push(Line::from(vec![Span::styled(
        ui_language.text(
            "     When enabled, CatDesk automatically appends \"Co-Authored-By: CatDesk\" to git commits and blocks manually written CatDesk co-author trailers.",
            "     啟用後，CatDesk 會自動在 Git commit 加上 \"Co-Authored-By: CatDesk\"，並阻止手動加入重複的 CatDesk co-author trailer。",
        ),
        Style::default().fg(palette.muted_fg),
    )]));

    let slug_keep_row = co_author_row + 1;
    let slug_keep_selected = slug_keep_row == selected_row;
    let slug_keep_marker = if slug_keep_selected { ">" } else { " " };
    let slug_keep_name_style = if slug_keep_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };

    let slug_new_row = co_author_row + 2;
    let slug_new_selected = slug_new_row == selected_row;
    let slug_new_marker = if slug_new_selected { ">" } else { " " };
    let slug_new_name_style = if slug_new_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };

    let domain_row = co_author_row + 3;
    let domain_selected = domain_row == selected_row;
    let domain_marker = if domain_selected { ">" } else { " " };
    let domain_name_style = if domain_selected {
        Style::default()
            .fg(palette.key_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.primary_fg)
    };

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text("  Connection Security URL", "  連線安全 URL"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )]));
    if slug_keep_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] {}",
            slug_keep_marker,
            slug_keep_row + 1,
            ui_language.text("Keep current recorded slug", "保留目前記錄的 slug")
        ),
        slug_keep_name_style,
    )]));
    lines.push(Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            format!("[{}]", mcp_slug),
            Style::default().fg(palette.muted_fg),
        ),
    ]));
    if slug_new_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] {}",
            slug_new_marker,
            slug_new_row + 1,
            ui_language.text("Generate new random slug", "產生新的隨機 slug")
        ),
        slug_new_name_style,
    )]));
    if domain_selected {
        selected_line_idx = lines.len();
    }
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} [{}] {}",
            domain_marker,
            domain_row + 1,
            ui_language.text("Set ngrok static domain", "設定 ngrok 固定網域")
        ),
        domain_name_style,
    )]));
    lines.push(Line::from(vec![
        Span::styled("     ", Style::default()),
        Span::styled(
            if let Some(domain) = ngrok_domain {
                format!("[{}]", domain)
            } else {
                ui_language.text("[not set]", "[未設定]").to_string()
            },
            Style::default().fg(palette.muted_fg),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        ui_language.text(
            "     Pro tip: Your permanent ngrok-free.dev domain is auto-saved above.",
            "     提示：你的永久 ngrok-free.dev 網域會自動儲存在上方。",
        ),
        Style::default().fg(palette.muted_fg),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        ui_language.text("  Token billing", "  Token 計費"),
        Style::default()
            .fg(palette.title_fg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled(
            ui_language.text("  Input ", "  輸入 "),
            Style::default().fg(palette.muted_fg),
        ),
        Span::styled(
            usage_totals.tool_input_tokens.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
        Span::styled(
            ui_language.text("   Output ", "   輸出 "),
            Style::default().fg(palette.muted_fg),
        ),
        Span::styled(
            usage_totals.tool_output_tokens.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            ui_language.text("  Total ", "  總計 "),
            Style::default().fg(palette.muted_fg),
        ),
        Span::styled(
            usage_totals.total_tokens.to_string(),
            Style::default()
                .fg(palette.secondary_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            ui_language.text("   Tool calls ", "   工具呼叫 "),
            Style::default().fg(palette.muted_fg),
        ),
        Span::styled(
            usage_totals.tool_call_count.to_string(),
            Style::default().fg(palette.primary_fg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  [r]", Style::default().fg(palette.warning_fg)),
        Span::styled(
            if confirm_reset_token_billing {
                ui_language.text(
                    " Press again to confirm token billing reset",
                    " 再按一次確認重設 Token 計費",
                )
            } else {
                ui_language.text(" Reset token billing totals", " 重設 Token 計費總計")
            },
            Style::default().fg(if confirm_reset_token_billing {
                palette.danger_fg
            } else {
                palette.muted_fg
            }),
        ),
    ]));

    let visible_height = chunks[1].height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(visible_height);
    let target_scroll = (selected_line_idx as u16).saturating_sub(visible_height / 2);
    let scroll_y = target_scroll.min(max_scroll);

    let body = Paragraph::new(lines).scroll((scroll_y, 0)).block(
        Block::default()
            .title(ui_language.text(" Theme, Tool Mode & Billing ", " 主題、工具模式與計費 "))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(body, chunks[1]);

    let keys = Paragraph::new(Line::from(vec![
        Span::styled("  [Up/Down]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(" Select  ", " 選擇  ")),
        Span::styled("[Enter]", Style::default().fg(palette.success_fg)),
        Span::raw(ui_language.text(" Apply  ", " 套用  ")),
        Span::styled(
            "[r]",
            Style::default().fg(if confirm_reset_token_billing {
                palette.danger_fg
            } else {
                palette.warning_fg
            }),
        ),
        Span::raw(if confirm_reset_token_billing {
            ui_language.text(" Confirm reset  ", " 確認重設  ")
        } else {
            ui_language.text(" Reset token billing  ", " 重設 Token 計費  ")
        }),
        Span::styled("[q/Esc]", Style::default().fg(palette.danger_fg)),
        Span::raw(ui_language.text(" Back", " 返回")),
    ]))
    .block(
        Block::default()
            .title(ui_language.text(" Keys ", " 按鍵 "))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(keys, chunks[2]);
}

