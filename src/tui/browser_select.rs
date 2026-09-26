use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use crate::browser;
use crate::state::{SharedState, UiLanguage};
use crate::theme;
use crate::UI_POLL_INTERVAL;
use crate::tui::chrome::draw_tui_header;

pub(crate) async fn mode_is_browser_enabled(state: SharedState) -> bool {
    state.lock().await.mode.browser_enabled()
}

pub(crate) fn browser_identity_matches(
    browser: &browser::DetectedBrowser,
    selected: &browser::DetectedBrowser,
) -> bool {
    browser.path == selected.path && browser.binary == selected.binary
}

pub(crate) fn selected_supported_browser_idx(
    browsers: &[browser::DetectedBrowser],
    selected_browser: Option<&browser::DetectedBrowser>,
) -> usize {
    let supported_indices: Vec<usize> = browsers
        .iter()
        .enumerate()
        .filter(|(_, browser)| browser.mcp_supported)
        .map(|(idx, _)| idx)
        .collect();
    if supported_indices.is_empty() {
        return 0;
    }
    let Some(selected_browser) = selected_browser else {
        return 0;
    };
    let Some(browser_idx) = browsers
        .iter()
        .position(|browser| browser_identity_matches(browser, selected_browser))
    else {
        return 0;
    };
    supported_indices
        .iter()
        .position(|idx| *idx == browser_idx)
        .unwrap_or(0)
}

pub(crate) async fn run_browser_select(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut browsers = browser::detect_browsers();
    let mut selected_supported_idx = {
        let mut app = state.lock().await;
        app.detected_browsers = browsers.clone();
        let selected_missing = app.selected_browser.as_ref().is_some_and(|selected| {
            !browsers
                .iter()
                .any(|browser| browser_identity_matches(browser, selected))
        });
        if selected_missing {
            app.selected_browser = None;
            app.persist_state_with_log();
        }
        selected_supported_browser_idx(&browsers, app.selected_browser.as_ref())
    };
    let mut redraw = true;
    loop {
        let supported_indices: Vec<usize> = browsers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.mcp_supported)
            .map(|(idx, _)| idx)
            .collect();
        if !supported_indices.is_empty() {
            selected_supported_idx =
                selected_supported_idx.min(supported_indices.len().saturating_sub(1));
        } else {
            selected_supported_idx = 0;
        }

        let (current_theme, current_ui_language) = {
            let app = state.lock().await;
            (app.current_theme(), app.ui_language)
        };
        if redraw {
            terminal.draw(|f| {
                draw_browser_select(
                    f,
                    &browsers,
                    &supported_indices,
                    selected_supported_idx,
                    current_theme,
                    current_ui_language,
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
                    KeyCode::Char('q') => return Ok(false),
                    KeyCode::Char('r') => {
                        browsers = browser::detect_browsers();
                        let mut app = state.lock().await;
                        app.detected_browsers = browsers.clone();
                        let selected_missing =
                            app.selected_browser.as_ref().is_some_and(|selected| {
                                !browsers
                                    .iter()
                                    .any(|browser| browser_identity_matches(browser, selected))
                            });
                        if selected_missing {
                            app.selected_browser = None;
                            app.persist_state_with_log();
                        }
                        selected_supported_idx = selected_supported_browser_idx(
                            &browsers,
                            app.selected_browser.as_ref(),
                        );
                    }
                    KeyCode::Up => {
                        selected_supported_idx = selected_supported_idx.saturating_sub(1)
                    }
                    KeyCode::Down => {
                        if selected_supported_idx + 1 < supported_indices.len() {
                            selected_supported_idx += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(selected_idx) = supported_indices.get(selected_supported_idx) {
                            if let Some(selected) = browsers.get(*selected_idx).cloned() {
                                persist_selected_browser(state.clone(), selected).await;
                                return Ok(true);
                            }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let index = c.to_digit(10).unwrap_or(0) as usize;
                        if index == 0 {
                            continue;
                        }
                        let target_idx = index - 1;
                        if let Some(browser_idx) = supported_indices.get(target_idx) {
                            if let Some(selected) = browsers.get(*browser_idx).cloned() {
                                persist_selected_browser(state.clone(), selected).await;
                                return Ok(true);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

pub(crate) async fn persist_selected_browser(state: SharedState, selected: browser::DetectedBrowser) {
    let remote_info = selected
        .remote_debug_target
        .as_deref()
        .unwrap_or("not active");
    let mut app = state.lock().await;
    app.selected_browser = Some(selected.clone());
    app.log(
        "INFO",
        format!(
            "Selected browser: {} ({}, {})",
            selected.name, selected.binary, selected.path
        ),
    );
    app.log(
        "INFO",
        format!("Selected browser remote debugging: {remote_info}"),
    );
    app.persist_state_with_log();
}

pub(crate) fn draw_browser_select(
    f: &mut Frame,
    browsers: &[browser::DetectedBrowser],
    supported_indices: &[usize],
    selected_supported_idx: usize,
    theme: &theme::ThemeDef,
    ui_language: UiLanguage,
) {
    let palette = theme.palette;
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);

    draw_tui_header(
        f,
        chunks[0],
        &palette,
        ui_language.text(
            "Select Browser - Installed and Remote Debugging Status",
            "選擇瀏覽器 - 已安裝與遠端除錯狀態",
        ),
    );

    let active_summary = browser::format_active_remote_debug_names(browsers);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                ui_language.text("  Installed browsers ", "  已安裝瀏覽器 "),
                Style::default().fg(palette.muted_fg),
            ),
            Span::styled(
                browsers.len().to_string(),
                Style::default()
                    .fg(palette.title_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                ui_language.text("  Remote debugging active ", "  遠端除錯啟用 "),
                Style::default().fg(palette.muted_fg),
            ),
            Span::styled(active_summary, Style::default().fg(palette.success_fg)),
        ]),
        Line::from(vec![
            Span::styled(
                ui_language.text("  Selectable (Chromium) ", "  可選擇（Chromium） "),
                Style::default().fg(palette.muted_fg),
            ),
            Span::styled(
                supported_indices.len().to_string(),
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];

    if browsers.is_empty() {
        lines.push(Line::from(Span::styled(
            ui_language.text(
                "  No browser found in PATH. Press [r] to rescan, [q] to quit.",
                "  在 PATH 中找不到瀏覽器。按 [r] 重新掃描，[q] 離開。",
            ),
            Style::default().fg(palette.danger_fg),
        )));
    } else if supported_indices.is_empty() {
        lines.push(Line::from(Span::styled(
            ui_language.text(
                "  Only unsupported browsers found (e.g. Firefox). Chromium browsers are required.",
                "  只找到尚未支援的瀏覽器（例如 Firefox）。需要 Chromium 瀏覽器。",
            ),
            Style::default().fg(palette.danger_fg),
        )));
        lines.push(Line::from(""));
        for browser in browsers {
            lines.push(Line::from(vec![Span::styled(
                format!("   [x] {} ({})", browser.name, browser.binary),
                Style::default().fg(palette.muted_fg),
            )]));
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "     {} {}",
                    ui_language.text("status", "狀態"),
                    if ui_language.is_traditional_chinese() {
                        if browser.mcp_supported {
                            "Chromium（支援）"
                        } else {
                            "尚未支援（Firefox 的 CDP bridge 尚未接上）"
                        }
                    } else {
                        browser.support_note.as_str()
                    }
                ),
                Style::default().fg(palette.warning_fg),
            )]));
            lines.push(Line::from(""));
        }
    } else {
        let selected_browser_index = supported_indices
            .get(selected_supported_idx)
            .copied()
            .unwrap_or(supported_indices[0]);
        for (idx, browser) in browsers.iter().enumerate() {
            let selected = idx == selected_browser_index;
            let prefix = if selected { ">" } else { " " };
            let quick_pick_num = supported_indices
                .iter()
                .position(|candidate_idx| *candidate_idx == idx)
                .map(|v| v + 1);
            let title_style = if !browser.mcp_supported {
                Style::default().fg(palette.muted_fg)
            } else if selected {
                Style::default()
                    .fg(palette.key_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.primary_fg)
            };
            if let Some(num) = quick_pick_num {
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        " {} [{}] {} ({})",
                        prefix, num, browser.name, browser.binary
                    ),
                    title_style,
                )]));
            } else {
                lines.push(Line::from(vec![Span::styled(
                    format!("   [x] {} ({})", browser.name, browser.binary),
                    title_style,
                )]));
            }
            lines.push(Line::from(vec![Span::styled(
                format!("     {} {}", ui_language.text("path", "路徑"), browser.path),
                Style::default().fg(palette.muted_fg),
            )]));
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "     {} {}",
                    ui_language.text("status", "狀態"),
                    if ui_language.is_traditional_chinese() {
                        if browser.mcp_supported {
                            "Chromium（支援）"
                        } else {
                            "尚未支援（Firefox 的 CDP bridge 尚未接上）"
                        }
                    } else {
                        browser.support_note.as_str()
                    }
                ),
                Style::default().fg(if browser.mcp_supported {
                    palette.success_fg
                } else {
                    palette.warning_fg
                }),
            )]));
            if !browser.mcp_supported {
                lines.push(Line::from(vec![Span::styled(
                    ui_language.text(
                        "     remote debugging integration not supported yet",
                        "     尚未支援遠端除錯整合",
                    ),
                    Style::default().fg(palette.warning_fg),
                )]));
            } else if browser.remote_debug_active {
                let target = browser.remote_debug_target.as_deref().unwrap_or("unknown");
                let pid = browser
                    .remote_debug_pid
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "--".into());
                lines.push(Line::from(vec![Span::styled(
                    if ui_language.is_traditional_chinese() {
                        format!("     遠端除錯已啟用：{target}（PID {pid}）")
                    } else {
                        format!("     remote debugging ACTIVE at {target} (pid {pid})")
                    },
                    Style::default().fg(palette.success_fg),
                )]));
            } else {
                lines.push(Line::from(vec![Span::styled(
                    if ui_language.is_traditional_chinese() {
                        format!(
                            "     遠端除錯未啟用（支援參數 {}）",
                            browser.remote_debug_hint
                        )
                    } else {
                        format!(
                            "     remote debugging not active (supported flag {})",
                            browser.remote_debug_hint
                        )
                    },
                    Style::default().fg(palette.warning_fg),
                )]));
            }
            lines.push(Line::from(""));
        }
    }

    let body = Paragraph::new(lines).block(
        Block::default()
            .title(ui_language.text(" Browser List ", " 瀏覽器清單 "))
            .borders(Borders::ALL)
            .border_type(palette.border_type)
            .border_style(Style::default().fg(palette.border_fg)),
    );
    f.render_widget(body, chunks[1]);

    let keys = Paragraph::new(Line::from(vec![
        Span::styled("  [Up/Down]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(" Select  ", " 選擇  ")),
        Span::styled("[1-9]", Style::default().fg(palette.key_fg)),
        Span::raw(ui_language.text(
            " Quick select (Chromium only)  ",
            " 快速選擇（僅 Chromium）  ",
        )),
        Span::styled("[Enter]", Style::default().fg(palette.success_fg)),
        Span::raw(ui_language.text(" Confirm  ", " 確認  ")),
        Span::styled("[r]", Style::default().fg(palette.warning_fg)),
        Span::raw(ui_language.text(" Rescan  ", " 重新掃描  ")),
        Span::styled("[q]", Style::default().fg(palette.danger_fg)),
        Span::raw(ui_language.text(" Quit", " 離開")),
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

pub(crate) fn find_available_remote_debug_port(start: u16, end: u16) -> Option<u16> {
    (start..=end).find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
}

pub(crate) fn sanitize_for_filename(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if sanitized.is_empty() {
        "browser".into()
    } else {
        sanitized
    }
}

