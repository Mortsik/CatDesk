use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;

use crate::CHATGPT_CONNECTOR_SETTINGS_URL;
use crate::CHATGPT_PLUGIN_SETTINGS_URL;
use crate::MCP_URL_MASK;
use crate::MCP_URL_REVEAL_DURATION;
use crate::UI_POLL_INTERVAL;
use crate::state::{ServerUiEvent, SharedState, UiLanguage};
use crate::theme;
use crate::drain_server_ui_events;
use crate::tui::chrome::{centered_rect, draw_mode_select, rect_contains, render_toast};
use crate::tui::clipboard::clipboard_copy;
use crate::tui::text::{mcp_url_reveal_bar_segments, mcp_url_reveal_seconds};

pub(crate) async fn run_chatgpt_connector_refresh_notice(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    ui_events: &mut Receiver<ServerUiEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !state.lock().await.chatgpt_connector_refresh_required {
        return Ok(());
    }

    let mut toast: Option<(&str, (u16, u16), Instant)> = None;
    let mut mcp_url_revealed_until: Option<Instant> = None;
    loop {
        {
            let mut app = state.lock().await;
            drain_server_ui_events(&mut app, ui_events);
            app.prune_closed_flows();
        }
        if let Some((_, _, created_at)) = &toast {
            if created_at.elapsed().as_secs() >= 2 {
                toast = None;
            }
        }

        let (current_theme, current_tool_mode, current_ui_language, mcp_url) = {
            let app = state.lock().await;
            (
                app.current_theme(),
                app.tool_mode,
                app.ui_language,
                app.public_mcp_url(),
            )
        };
        let reveal_remaining = mcp_url_revealed_until
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()));
        if mcp_url_revealed_until.is_some() && reveal_remaining.is_none() {
            mcp_url_revealed_until = None;
        }
        let toast_ref = toast
            .as_ref()
            .filter(|(_, _, created_at)| created_at.elapsed().as_secs() < 2)
            .map(|(message, position, _)| (*message, *position));
        let mut mcp_url_click_area = Rect::default();
        terminal.draw(|f| {
            draw_mode_select(f, current_theme, current_tool_mode, current_ui_language);
            mcp_url_click_area = chatgpt_connector_refresh_mcp_url_area(f.area());
            draw_chatgpt_connector_refresh_notice(
                f,
                current_theme,
                current_ui_language,
                mcp_url.as_deref(),
                reveal_remaining,
            );
            if let Some((message, position)) = toast_ref {
                render_toast(f, current_theme.palette, message, position);
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
                    KeyCode::Enter => {
                        if mcp_url.is_none() {
                            toast = Some((
                                current_ui_language.text("MCP URL not ready", "MCP URL 尚未就緒"),
                                (2, 2),
                                Instant::now(),
                            ));
                            continue;
                        }
                        let mut app = state.lock().await;
                        app.acknowledge_chatgpt_connector_refresh();
                        app.log("INFO", "ChatGPT connector refresh acknowledged".into());
                        app.persist_state_with_log();
                        return Ok(());
                    }
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Char('s') => {
                        let message = if clipboard_copy(CHATGPT_PLUGIN_SETTINGS_URL) {
                            current_ui_language.text("Settings link copied", "設定連結已複製")
                        } else {
                            current_ui_language.text("Copy failed", "複製失敗")
                        };
                        toast = Some((message, (2, 2), Instant::now()));
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    && rect_contains(mcp_url_click_area, mouse.column, mouse.row) =>
            {
                let now = Instant::now();
                let revealed = mcp_url_revealed_until
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .is_some();
                let message = match mcp_url.as_deref() {
                    Some(url) if revealed && clipboard_copy(url) => {
                        current_ui_language.text("Copied!", "已複製！")
                    }
                    Some(_) if revealed => current_ui_language.text("Copy failed", "複製失敗"),
                    Some(_) => {
                        mcp_url_revealed_until = Some(now + MCP_URL_REVEAL_DURATION);
                        current_ui_language.text("URL revealed for 10s", "URL 顯示 10 秒")
                    }
                    None => current_ui_language.text("MCP URL not ready", "MCP URL 尚未就緒"),
                };
                toast = Some((message, (mouse.column, mouse.row), now));
            }
            _ => {}
        }
    }
}

fn chatgpt_connector_refresh_modal_area(frame_area: Rect) -> Rect {
    centered_rect(94, 24, frame_area)
}

fn chatgpt_connector_refresh_content_area(frame_area: Rect) -> Rect {
    let area = chatgpt_connector_refresh_modal_area(frame_area);
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
    .inner(Margin {
        horizontal: 2,
        vertical: 0,
    })
}

fn chatgpt_connector_refresh_mcp_url_area(frame_area: Rect) -> Rect {
    let content = chatgpt_connector_refresh_content_area(frame_area);
    Rect::new(content.x, content.y.saturating_add(12), content.width, 1)
}

pub(crate) fn draw_chatgpt_connector_refresh_notice(
    f: &mut Frame,
    theme: &theme::ThemeDef,
    ui_language: UiLanguage,
    mcp_url: Option<&str>,
    mcp_url_reveal_remaining: Option<Duration>,
) {
    let palette = theme.palette;
    let modal_bg = Color::Rgb(34, 38, 47);
    let modal_fg = Color::Rgb(232, 236, 242);
    let area = chatgpt_connector_refresh_modal_area(f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(ui_language.text(
            " CatDesk Connector Refresh Required ",
            " 需要重新整理 CatDesk Connector ",
        ))
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.warning_fg))
        .style(Style::default().bg(modal_bg));
    let inner = chatgpt_connector_refresh_content_area(f.area());
    f.render_widget(block, area);

    let strong = Style::default()
        .fg(palette.title_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(modal_fg).bg(modal_bg);
    let muted = Style::default().fg(palette.muted_fg).bg(modal_bg);
    let key = Style::default()
        .fg(palette.key_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD);
    let copyable = Style::default()
        .fg(palette.primary_fg)
        .bg(modal_bg)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mcp_url_is_revealed = mcp_url.is_some() && mcp_url_reveal_remaining.is_some();
    let displayed_mcp_url = match (mcp_url, mcp_url_is_revealed) {
        (Some(url), true) => url.to_string(),
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

    let lines = vec![
        Line::from(Span::styled(
            ui_language.text(
                "The connector changed in this update. Please folow below step:",
                "此更新變更了 Connector。請依照以下步驟操作：",
            ),
            normal,
        )),
        Line::from(""),
        Line::from(Span::styled(
            ui_language.text("Remove CatDesk", "移除 CatDesk"),
            strong,
        )),
        Line::from(Span::styled(
            format!(
                "1. {} {CHATGPT_PLUGIN_SETTINGS_URL}",
                ui_language.text("Open", "開啟")
            ),
            normal,
        )),
        Line::from(Span::styled(
            ui_language.text("2. Find CatDesk and click it", "2. 找到 CatDesk 並點擊它"),
            normal,
        )),
        Line::from(Span::styled(
            ui_language.text(
                "3. Click the ... button on upper right corner",
                "3. 點擊右上角的 ... 按鈕",
            ),
            normal,
        )),
        Line::from(Span::styled(
            ui_language.text("4. Click delete", "4. 點擊 Delete"),
            normal,
        )),
        Line::from(""),
        Line::from(Span::styled(
            ui_language.text("Add CatDesk Again", "重新加入 CatDesk"),
            strong,
        )),
        Line::from(Span::styled(
            ui_language.text("5. Open connector settings:", "5. 開啟 Connector 設定："),
            normal,
        )),
        Line::from(Span::styled(
            format!("   {CHATGPT_CONNECTOR_SETTINGS_URL}"),
            muted,
        )),
        Line::from(Span::styled(
            ui_language.text("6. Click Create app", "6. 點擊 Create app"),
            normal,
        )),
        Line::from(vec![
            Span::styled(
                ui_language.text("7. Fill in the form: ", "7. 填寫表單："),
                normal,
            ),
            Span::styled(
                ui_language.text("(URL reveals before copy)", "（複製前會顯示 URL）"),
                muted,
            ),
        ]),
        Line::from(Span::styled(
            ui_language.text("   Name           │ CatDesk", "   名稱           │ CatDesk"),
            muted,
        )),
        {
            let mut spans = vec![
                Span::styled(
                    ui_language.text("   MCP Server URL │ ", "   MCP 伺服器 URL │ "),
                    muted,
                ),
                Span::styled(
                    displayed_mcp_url.clone(),
                    if mcp_url_is_revealed { copyable } else { muted },
                ),
            ];
            if mcp_url.is_some() {
                spans.push(Span::raw("  "));
                let security_text = mcp_url_security_status
                    .as_deref()
                    .unwrap_or(ui_language.text("Click to reveal", "點擊顯示"));
                let security_color = match mcp_url_reveal_remaining {
                    Some(remaining) if mcp_url_reveal_seconds(remaining) <= 3 => palette.danger_fg,
                    Some(_) => palette.warning_fg,
                    None => palette.muted_fg,
                };
                spans.push(Span::styled(
                    security_text.to_string(),
                    Style::default()
                        .fg(security_color)
                        .bg(modal_bg)
                        .add_modifier(Modifier::BOLD),
                ));
                if let Some(remaining) = mcp_url_reveal_remaining {
                    let (remaining_bar, elapsed_bar) = mcp_url_reveal_bar_segments(remaining);
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        remaining_bar,
                        Style::default()
                            .fg(security_color)
                            .bg(modal_bg)
                            .add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        elapsed_bar,
                        Style::default().fg(palette.muted_fg).bg(modal_bg),
                    ));
                }
            }
            Line::from(spans)
        },
        Line::from(Span::styled(
            ui_language.text("   Authentication │ None", "   驗證方式       │ None"),
            muted,
        )),
        Line::from(Span::styled(
            ui_language.text(
                "8. Click I understand and want to continue",
                "8. 點擊 I understand and want to continue",
            ),
            normal,
        )),
        Line::from(Span::styled(
            ui_language.text("9. Click Create", "9. 點擊 Create"),
            normal,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[s]", key),
            Span::styled(
                ui_language.text(" Copy settings link   ", " 複製設定連結   "),
                muted,
            ),
            Span::styled(
                "[Enter]",
                Style::default().fg(palette.success_fg).bg(modal_bg),
            ),
            Span::styled(
                ui_language.text(" I've re-added CatDesk   ", " 我已重新加入 CatDesk   "),
                muted,
            ),
            Span::styled(
                "[Esc]",
                Style::default().fg(palette.warning_fg).bg(modal_bg),
            ),
            Span::styled(
                ui_language.text(" Remind me next launch", " 下次啟動再提醒我"),
                muted,
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(modal_bg))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

