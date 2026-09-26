use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::time::Instant;

use crate::NGROK_SETUP_URL;
use crate::UI_POLL_INTERVAL;
use crate::state::{
    SharedState, UiLanguage, app_config_path, load_ngrok_authtoken, load_ngrok_domain,
    save_ngrok_authtoken, save_ngrok_domain,
};
use crate::theme;
use crate::selected_supported_browser_idx;
use crate::normalize_ngrok_authtoken_input;
use crate::tui::clipboard::{clipboard_paste, key_is_clipboard_paste, text_input_key_is_cancel};
use crate::draw_browser_select;
use crate::tui::chrome::{centered_rect, draw_mode_select, rect_contains, render_toast};
use crate::tui::clipboard::clipboard_copy;

pub(crate) async fn run_ngrok_auth_setup(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<bool, Box<dyn std::error::Error>> {
    if load_ngrok_authtoken()?.is_some() {
        return Ok(true);
    }

    let config_path = app_config_path()?;
    let config_path_text = config_path.to_string_lossy().into_owned();
    let mut input = String::new();
    let mut error_message: Option<String> = None;
    let mut toast: Option<(&str, (u16, u16), Instant)> = None;

    loop {
        if let Some((_, _, t)) = &toast {
            if t.elapsed().as_secs() >= 2 {
                toast = None;
            }
        }

        let (
            current_theme,
            current_tool_mode,
            current_ui_language,
            current_mode,
            browsers,
            selected_browser,
        ) = {
            let app = state.lock().await;
            (
                app.current_theme(),
                app.tool_mode,
                app.ui_language,
                app.mode,
                app.detected_browsers.clone(),
                app.selected_browser.clone(),
            )
        };
        let supported_indices: Vec<usize> = browsers
            .iter()
            .enumerate()
            .filter(|(_, browser)| browser.mcp_supported)
            .map(|(idx, _)| idx)
            .collect();
        let selected_supported_idx =
            selected_supported_browser_idx(&browsers, selected_browser.as_ref());
        let toast_ref = toast
            .as_ref()
            .filter(|(_, _, t)| t.elapsed().as_secs() < 2)
            .map(|(m, pos, _)| (*m, *pos));
        let mut ngrok_setup_copy_area = Rect::default();
        terminal.draw(|f| {
            let anchor_area = if current_mode.browser_enabled() {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(10),
                        Constraint::Length(3),
                    ])
                    .split(f.area())[1]
            } else {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Length(17),
                        Constraint::Min(0),
                    ])
                    .split(f.area())[1]
            };
            ngrok_setup_copy_area = ngrok_auth_setup_copy_area(anchor_area);
            if current_mode.browser_enabled() {
                draw_browser_select(
                    f,
                    &browsers,
                    &supported_indices,
                    selected_supported_idx,
                    current_theme,
                    current_ui_language,
                );
            } else {
                draw_mode_select(f, current_theme, current_tool_mode, current_ui_language);
            }
            draw_ngrok_auth_setup(
                f,
                current_theme,
                current_ui_language,
                anchor_area,
                &config_path_text,
                &masked_secret_preview(&input),
                error_message.as_deref(),
            );
            if let Some((message, pos)) = toast_ref {
                render_toast(f, current_theme.palette, message, pos);
            }
        })?;

        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    code if text_input_key_is_cancel(code) => return Ok(false),
                    KeyCode::Enter => {
                        let token = normalize_ngrok_authtoken_input(&input);
                        if token.is_empty() {
                            error_message = Some(
                                current_ui_language
                                    .text(
                                        "NGROK_AUTHTOKEN cannot be empty",
                                        "NGROK_AUTHTOKEN 不可為空白",
                                    )
                                    .into(),
                            );
                            continue;
                        }
                        match save_ngrok_authtoken(&token) {
                            Ok(saved_path) => {
                                let mut app = state.lock().await;
                                app.log(
                                    "INFO",
                                    format!(
                                        "Saved ngrok authtoken to {}",
                                        saved_path.to_string_lossy()
                                    ),
                                );
                                return Ok(true);
                            }
                            Err(e) => {
                                error_message =
                                    Some(if current_ui_language.is_traditional_chinese() {
                                        format!("無法儲存 ~/.catdesk/config.toml：{e}")
                                    } else {
                                        format!("Failed to save ~/.catdesk/config.toml: {e}")
                                    });
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        error_message = None;
                    }
                    KeyCode::Char(c) => {
                        if key_is_clipboard_paste(&key) {
                            if let Some(text) = clipboard_paste() {
                                input.push_str(&normalize_ngrok_authtoken_input(&text));
                                error_message = None;
                            }
                        } else {
                            input.push(c);
                            error_message = None;
                        }
                    }
                    KeyCode::Insert if key_is_clipboard_paste(&key) => {
                        if let Some(text) = clipboard_paste() {
                            input.push_str(&normalize_ngrok_authtoken_input(&text));
                            error_message = None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                input.push_str(&normalize_ngrok_authtoken_input(&text));
                error_message = None;
            }
            Event::Mouse(mouse) => {
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    && rect_contains(ngrok_setup_copy_area, mouse.column, mouse.row)
                {
                    let message = if clipboard_copy(NGROK_SETUP_URL) {
                        current_ui_language.text("Copied!", "已複製！")
                    } else {
                        current_ui_language.text("Copy failed", "複製失敗")
                    };
                    toast = Some((message, (mouse.column, mouse.row), Instant::now()));
                }
            }
            _ => {}
        }
    }
}

pub(crate) async fn run_ngrok_domain_setup(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
) -> Result<bool, Box<dyn std::error::Error>> {
    if load_ngrok_domain()?.is_some() {
        return Ok(true);
    }

    let mut input = String::new();
    let mut error_message: Option<String> = None;

    loop {
        let (
            current_theme,
            current_tool_mode,
            current_ui_language,
            current_mode,
            browsers,
            selected_browser,
        ) = {
            let app = state.lock().await;
            (
                app.current_theme(),
                app.tool_mode,
                app.ui_language,
                app.mode,
                app.detected_browsers.clone(),
                app.selected_browser.clone(),
            )
        };
        let supported_indices: Vec<usize> = browsers
            .iter()
            .enumerate()
            .filter(|(_, browser)| browser.mcp_supported)
            .map(|(idx, _)| idx)
            .collect();
        let selected_supported_idx =
            selected_supported_browser_idx(&browsers, selected_browser.as_ref());
        terminal.draw(|f| {
            let anchor_area = if current_mode.browser_enabled() {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(10),
                        Constraint::Length(3),
                    ])
                    .split(f.area())[1]
            } else {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Length(17),
                        Constraint::Min(0),
                    ])
                    .split(f.area())[1]
            };
            if current_mode.browser_enabled() {
                draw_browser_select(
                    f,
                    &browsers,
                    &supported_indices,
                    selected_supported_idx,
                    current_theme,
                    current_ui_language,
                );
            } else {
                draw_mode_select(f, current_theme, current_tool_mode, current_ui_language);
            }
            draw_ngrok_domain_setup(
                f,
                current_theme,
                current_ui_language,
                anchor_area,
                &input,
                error_message.as_deref(),
            );
        })?;

        if !event::poll(UI_POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    code if text_input_key_is_cancel(code) => return Ok(false),
                    KeyCode::Enter => {
                        let domain = normalize_ngrok_domain_input(&input);
                        if domain.is_empty() {
                            error_message = Some(
                                current_ui_language
                                    .text("ngrok domain cannot be empty", "ngrok 網域不可為空白")
                                    .into(),
                            );
                            continue;
                        }
                        match save_ngrok_domain(&domain) {
                            Ok(saved_path) => {
                                let mut app = state.lock().await;
                                app.ngrok_domain = Some(domain.clone());
                                app.log(
                                    "INFO",
                                    format!(
                                        "Saved ngrok domain to {}",
                                        saved_path.to_string_lossy()
                                    ),
                                );
                                return Ok(true);
                            }
                            Err(e) => {
                                error_message =
                                    Some(if current_ui_language.is_traditional_chinese() {
                                        format!("無法儲存 ~/.catdesk/config.toml：{e}")
                                    } else {
                                        format!("Failed to save ~/.catdesk/config.toml: {e}")
                                    });
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        error_message = None;
                    }
                    KeyCode::Char(c) => {
                        if key_is_clipboard_paste(&key) {
                            if let Some(text) = clipboard_paste() {
                                input.push_str(&normalize_ngrok_domain_input(&text));
                                error_message = None;
                            }
                        } else {
                            input.push(c);
                            error_message = None;
                        }
                    }
                    KeyCode::Insert if key_is_clipboard_paste(&key) => {
                        if let Some(text) = clipboard_paste() {
                            input.push_str(&normalize_ngrok_domain_input(&text));
                            error_message = None;
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                input.push_str(&normalize_ngrok_domain_input(&text));
                error_message = None;
            }
            _ => {}
        }
    }
}

pub(crate) fn normalize_ngrok_domain_input(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        if let Some(host) = url.host_str() {
            return host.to_string();
        }
    }
    trimmed.to_string()
}

pub(crate) fn draw_ngrok_domain_setup(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    ui_language: UiLanguage,
    anchor_area: Rect,
    domain_value: &str,
    error_message: Option<&str>,
) {
    let palette = theme.palette;
    let modal_bg = Color::Rgb(34, 38, 47);
    let modal_fg = Color::Rgb(232, 236, 242);

    let modal_area = centered_rect(90, 12, anchor_area);
    f.render_widget(Clear, modal_area);
    let modal_block = Block::default()
        .title(ui_language.text(" ngrok domain ", " ngrok 網域 "))
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg))
        .style(Style::default().bg(modal_bg));
    f.render_widget(modal_block, modal_area);

    let inner = Rect::new(
        modal_area.x.saturating_add(1),
        modal_area.y.saturating_add(1),
        modal_area.width.saturating_sub(2),
        modal_area.height.saturating_sub(2),
    );
    let content_area = inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let modal_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);

    let step_style = Style::default()
        .fg(palette.title_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let body_lines = vec![
        Line::from(Span::styled(
            ui_language.text("ngrok domain setup", "ngrok 網域設定"),
            step_style,
        )),
        Line::from(""),
        Line::from(Span::styled(
            ui_language.text(
                "Enter your ngrok static domain (e.g. my-app.ngrok-free.dev)",
                "輸入你的 ngrok 固定網域（例如 my-app.ngrok-free.dev）",
            ),
            step_style,
        )),
    ];
    let body = Paragraph::new(body_lines)
        .style(Style::default().fg(modal_fg).bg(modal_bg))
        .wrap(Wrap { trim: false });
    f.render_widget(body, modal_chunks[0]);

    let input_line = if domain_value.is_empty() {
        "_".to_string()
    } else {
        domain_value.to_string()
    };
    let input_widget = Paragraph::new(format!("  {input_line}"))
        .style(Style::default().fg(palette.title_fg).bg(modal_bg))
        .block(
            Block::default()
                .title(" NGROK_DOMAIN ")
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg))
                .style(Style::default().bg(modal_bg)),
        );
    f.render_widget(input_widget, modal_chunks[1]);

    let footer = if let Some(message) = error_message {
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(palette.danger_fg).bg(modal_bg),
        )))
    } else {
        Paragraph::new(Line::from(Span::styled(
            ui_language.text(
                "[Enter] Save  [Esc] Quit  [Paste/Ctrl+V] Insert domain",
                "[Enter] 儲存  [Esc] 離開  [Paste/Ctrl+V] 貼上網域",
            ),
            Style::default().fg(palette.muted_fg).bg(modal_bg),
        )))
    };
    f.render_widget(footer, modal_chunks[2]);
}

pub(crate) fn masked_secret_preview(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    let visible = chars.len().min(4);
    let masked_len = chars.len().saturating_sub(visible);
    let mut preview = "*".repeat(masked_len);
    preview.extend(chars[chars.len() - visible..].iter());
    preview
}

pub(crate) fn ngrok_auth_setup_modal_area(anchor_area: Rect) -> Rect {
    centered_rect(90, 15, anchor_area)
}

pub(crate) fn ngrok_auth_setup_content_area(anchor_area: Rect) -> Rect {
    let modal_area = ngrok_auth_setup_modal_area(anchor_area);
    let inner = Rect::new(
        modal_area.x.saturating_add(1),
        modal_area.y.saturating_add(1),
        modal_area.width.saturating_sub(2),
        modal_area.height.saturating_sub(2),
    );
    inner.inner(Margin {
        horizontal: 2,
        vertical: 1,
    })
}

pub(crate) fn ngrok_auth_setup_copy_area(anchor_area: Rect) -> Rect {
    let content_area = ngrok_auth_setup_content_area(anchor_area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);
    let body = chunks[0];
    if body.height <= 2 {
        return Rect::new(body.x, body.y, 0, 0);
    }
    Rect::new(body.x, body.y.saturating_add(2), body.width, 2)
}

pub(crate) fn draw_ngrok_auth_setup(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    ui_language: UiLanguage,
    anchor_area: Rect,
    _config_path: &str,
    masked_value: &str,
    error_message: Option<&str>,
) {
    let palette = theme.palette;
    let modal_bg = Color::Rgb(34, 38, 47);
    let modal_fg = Color::Rgb(232, 236, 242);

    let modal_area = ngrok_auth_setup_modal_area(anchor_area);
    f.render_widget(Clear, modal_area);
    let modal_block = Block::default()
        .title(ui_language.text(" ngrok auth ", " ngrok 驗證 "))
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border_fg))
        .style(Style::default().bg(modal_bg));
    f.render_widget(modal_block, modal_area);
    let content_area = ngrok_auth_setup_content_area(anchor_area);

    let modal_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(content_area);

    let link_style = Style::default()
        .fg(palette.primary_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let step_style = Style::default()
        .fg(palette.title_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let body_lines = vec![
        Line::from(Span::styled(
            ui_language.text("ngrok setup required", "需要設定 ngrok"),
            step_style,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                ui_language.text(
                    "1. Open in browser and get your authtoken",
                    "1. 在瀏覽器開啟頁面並取得 authtoken",
                ),
                step_style,
            ),
            Span::raw(" "),
            Span::styled(
                ui_language.text("(click to copy)", "（點擊複製）"),
                Style::default().fg(palette.secondary_fg).bg(modal_bg),
            ),
        ]),
        Line::from(vec![
            Span::raw("   "),
            Span::styled(NGROK_SETUP_URL, link_style),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            ui_language.text(
                "2. Paste the token or ngrok config command below",
                "2. 在下方貼上 token 或 ngrok 設定指令",
            ),
            step_style,
        )),
    ];
    let body = Paragraph::new(body_lines)
        .style(Style::default().fg(modal_fg).bg(modal_bg))
        .wrap(Wrap { trim: false });
    f.render_widget(body, modal_chunks[0]);

    let input_line = if masked_value.is_empty() {
        "_".to_string()
    } else {
        masked_value.to_string()
    };
    let input = Paragraph::new(format!("  {input_line}"))
        .style(Style::default().fg(palette.title_fg).bg(modal_bg))
        .block(
            Block::default()
                .title(" NGROK_AUTHTOKEN ")
                .borders(Borders::ALL)
                .border_type(palette.border_type)
                .border_style(Style::default().fg(palette.border_fg))
                .style(Style::default().bg(modal_bg)),
        );
    f.render_widget(input, modal_chunks[1]);

    let footer = if let Some(message) = error_message {
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(palette.danger_fg).bg(modal_bg),
        )))
    } else {
        Paragraph::new(Line::from(Span::styled(
            ui_language.text(
                "[Enter] Save  [Esc] Quit  [Paste/Ctrl+V] Insert token",
                "[Enter] 儲存  [Esc] 離開  [Paste/Ctrl+V] 貼上 token",
            ),
            Style::default().fg(palette.muted_fg).bg(modal_bg),
        )))
    };
    f.render_widget(footer, modal_chunks[2]);
}

