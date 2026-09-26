mod binagotchy_gen;
mod browser;
mod build_info;
mod change_tracking;
mod command;
mod command_jobs;
mod command_policy;
mod job_store;
mod devtools;
mod diagnostics;
mod handoff;
#[cfg(target_os = "linux")]
mod linux_sandbox;
mod macos_terminal;
mod mascot;
mod mcp;
mod request_workers;
mod ngrok;
mod perf_metrics;
mod process_exit;
mod process_runner;
mod project_scope;
mod server;
mod session_context;
mod startup;
mod state;
mod theme;
mod tui;

mod usage_persistence;
mod usage_pricing;
mod vision;
mod workspace_tools;

use crossterm::{
    ExecutableCommand,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use devtools::DevtoolsBridge;
use mascot::{TUI_MASCOT_BLOCK_HEIGHT, TUI_MASCOT_BLOCK_WIDTH, render_tui_lines};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use state::{
    AppState, FlowLane, LIVE_USAGE_WINDOW_MS, Mode, ServerUiEvent, SharedState, UiLanguage,
    load_macos_terminal_profile, save_macos_terminal_profile, user_home_dir,
};
use tui::{
    run_settings,
    run_ngrok_auth_setup, run_ngrok_domain_setup,
    run_chatgpt_connector_refresh_notice,
    centered_rect, draw_mode_select, draw_tui_header, render_toast,
    clipboard_copy,
    LogView, MCP_URL_MASK, Selection, build_animation_snapshot, export_logs, extract_from_screen,
    active_bootstrap_status_flow, flow_bootstrap_status_lines, flow_lane_left_label,
    flow_lane_spans, flow_turn_usage_spans, latest_flow_action,
    should_display_flow_row, should_show_connect_guide,
    format_average_usage_cost_usd, format_cost_estimate_usd,
    format_session_duration, format_token_compact, format_usd_compact,
    is_secret_log_message, localize_log_message,
    mask_secret_log_message, mcp_url_reveal_bar_segments,
    mcp_url_reveal_seconds, pad_right_to_cell_width, post_mcp_path, secret_log_copy_value,
    session_cost_rates, trim_line, wrap_log_message,
};
use std::collections::HashMap;
use std::io::{Write, stdout};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

const REMOTE_CONNECT_UI_GRACE_MS: u128 = 8_000;
const UI_POLL_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
const LIVE_TELEMETRY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const UI_TIMED_REDRAW_INTERVAL: Duration = Duration::from_millis(100);
const UI_IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);
const MCP_URL_REVEAL_DURATION: Duration = Duration::from_secs(10);

fn redraw_due(dirty: bool, elapsed: Duration, interval: Option<Duration>) -> bool {
    dirty || interval.is_some_and(|interval| elapsed >= interval)
}
const STATUS_PANEL_HEIGHT: u16 = TUI_MASCOT_BLOCK_HEIGHT + 6;
const STATUS_LABEL_WIDTH: usize = 13;
const NGROK_SETUP_URL: &str = "https://dashboard.ngrok.com/get-started/setup";
const CHATGPT_CONNECTOR_SETTINGS_URL: &str = "https://chatgpt.com/apps#settings/Connectors";
const CHATGPT_PLUGIN_SETTINGS_URL: &str = "https://chatgpt.com/#settings/Plugins";

fn normalize_ngrok_authtoken_input(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if let Some(idx) = parts.iter().position(|part| *part == "add-authtoken") {
        if let Some(token) = parts.get(idx + 1) {
            return token.trim_matches(['"', '\'']).to_string();
        }
    }

    trimmed.to_string()
}

fn drain_server_ui_events(app: &mut AppState, ui_events: &mut Receiver<ServerUiEvent>) -> bool {
    // An ongoing request stream must not postpone keyboard handling forever.
    let mut changed = false;
    for _ in 0..256 {
        let Ok(event) = ui_events.try_recv() else { break };
        app.apply_server_ui_event(event);
        changed = true;
    }
    changed
}

// ── Main ────────────────────────────────────────────────────

fn parse_terminal_profile_choice(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn prompt_macos_terminal_profile() -> std::io::Result<bool> {
    loop {
        println!("CatDesk can apply its Terminal.app profile for the best TUI appearance.");
        print!("Use the CatDesk Terminal.app profile? [Y/n]: ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if let Some(enabled) = parse_terminal_profile_choice(&input) {
            return Ok(enabled);
        }
        eprintln!("Please answer y/yes or n/no.");
    }
}

fn macos_terminal_profile_enabled() -> std::io::Result<bool> {
    if !macos_terminal::should_prompt_for_terminal_profile() {
        return Ok(true);
    }
    if let Some(enabled) = load_macos_terminal_profile()? {
        return Ok(enabled);
    }

    let enabled = prompt_macos_terminal_profile()?;
    save_macos_terminal_profile(enabled)?;
    Ok(enabled)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(async_main());
    // A filesystem mounted over 9p/NFS can remain blocked in the kernel. Do not
    // let Tokio's implicit infinite wait for blocking workers prevent exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let session_started_at = Instant::now();

    // rustls 0.23 refuses to pick a process-level CryptoProvider when more than
    // one provider feature is enabled, and panics on first use. Both end up
    // enabled here through feature unification: ngrok requires aws-lc-rs, while
    // reqwest's rustls-tls pulls in ring. Install one explicitly instead of
    // relying on automatic selection. aws-lc-rs is chosen because ngrok already
    // requires it, so it is always present.
    //
    // An error means a provider was already installed, which is equally fine.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let terminal_profile_enabled = macos_terminal_profile_enabled()?;
    match macos_terminal::maybe_relaunch_in_terminal_profile(terminal_profile_enabled) {
        Ok(macos_terminal::LaunchAction::Continue) => {}
        #[cfg(target_os = "macos")]
        Ok(macos_terminal::LaunchAction::ExitAfterProfileBootstrap) => {
            eprintln!(
                "CatDesk applied the Terminal.app profile. Run the same command again in this tab."
            );
            return Ok(());
        }
        Err(error) => {
            return Err(std::io::Error::other(format!(
                "CatDesk: macOS Terminal profile bootstrap failed: {error}"
            ))
            .into());
        }
    }

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3200);
    let workspace_root = match std::env::var("WORKSPACE_ROOT") {
        Ok(path) => path,
        Err(_) => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    let diagnostics_dir = user_home_dir()?.join(".catdesk").join("logs");
    let _diagnostics_guard = match diagnostics::init(&diagnostics_dir) {
        Ok(guard) => Some(guard),
        Err(_) => {
            eprintln!(
                "CatDesk: connection diagnostics unavailable (log directory unwritable or in use)"
            );
            None
        }
    };
    let state: SharedState = Arc::new(Mutex::new(AppState::new(port, workspace_root)?));
    {
        let mut app = state.lock().await;
        app.persist_state_with_log();
    }

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableBracketedPaste)?;
    stdout().execute(EnableMouseCapture)?;

    // Restore the terminal if the thread driving the TUI panics. The normal
    // teardown below only runs on the ordinary exit path, so without this a
    // panic leaves raw mode and mouse capture enabled: the terminal keeps
    // emitting SGR mouse reports such as `35;81;24M` that nothing consumes, and
    // the shell stays unusable until the user runs `reset`.
    //
    // The hook is process-global, but tokio catches panics in spawned tasks and
    // keeps the rest of the runtime alive. start_services launches axum before
    // the TUI, so tearing the terminal down for any panic would corrupt a
    // display that is still running. Restore only when the panicking thread is
    // the one that set the terminal up.
    {
        let terminal_thread = std::thread::current().id();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() == terminal_thread {
                let _ = stdout().execute(DisableBracketedPaste);
                let _ = stdout().execute(DisableMouseCapture);
                let _ = disable_raw_mode();
                let _ = stdout().execute(LeaveAlternateScreen);
            }
            default_hook(info);
        }));
    }

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let (startup_theme, startup_mascot) = {
        let app = state.lock().await;
        (app.current_theme(), app.mascot.clone())
    };
    startup::run_startup_intro(&mut terminal, startup_theme, &startup_mascot).await?;

    let result = run_app(&mut terminal, state.clone(), session_started_at).await;

    diagnostics::event("process_stopping");
    diagnostics::event("server_stopping");

    stdout().execute(DisableBracketedPaste)?;
    stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    // Cleanup after the TUI is gone so quit never appears frozen on screen.
    let cleanup = async {
        let command_jobs = { state.lock().await.command_jobs.clone() };
        command_jobs.cancel_all().await;
        let mut app = state.lock().await;
        if let Some(handle) = app.server_handle.take() {
            handle.abort();
        }
        if let Some(handle) = app.ngrok_task.take() {
            handle.abort();
        }
        if let Some(child) = app.remote_browser_child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(child) = app.devtools_child.as_mut() {
            let _ = child.start_kill();
        }
        app.server_running = false;
        app.ngrok_running = false;
        app.ngrok_url = None;
        app.remote_connected = false;
        app.last_remote_activity_ms = None;
    };
    if tokio::time::timeout(Duration::from_secs(6), cleanup).await.is_err() {
        diagnostics::event("shutdown_cleanup_timeout");
    }

    result
}

// ── Phase 1: Mode selection ─────────────────────────────────

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    session_started_at: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    // Draw mode selection screen
    let mut redraw = true;
    loop {
        let (current_theme, current_tool_mode, current_ui_language) = {
            let app = state.lock().await;
            (app.current_theme(), app.tool_mode, app.ui_language)
        };
        if redraw {
            terminal.draw(|f| {
                draw_mode_select(f, current_theme, current_tool_mode, current_ui_language)
            })?;
            redraw = false;
        }

        if event::poll(UI_POLL_INTERVAL)? {
            redraw = true;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let mode = match key.code {
                    KeyCode::Char('1') => Mode::Computer,
                    KeyCode::Char('2') => Mode::Browser,
                    KeyCode::Char('3') => Mode::Both,
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('l') | KeyCode::Char('L') => {
                        let mut app = state.lock().await;
                        app.ui_language = app.ui_language.toggled();
                        let language = app.ui_language.label();
                        app.log("INFO", format!("UI language: {language}"));
                        app.persist_state_with_log();
                        continue;
                    }
                    KeyCode::Char('s') => {
                        run_settings(terminal, state.clone()).await?;
                        continue;
                    }
                    _ => continue,
                };
                {
                    let mut app = state.lock().await;
                    app.mode = mode;
                    app.log("INFO", format!("Mode: {}", mode.label()));
                    app.persist_state_with_log();
                }
                break;
            }
        }
    }

    if mode_is_browser_enabled(state.clone()).await {
        let continue_run = run_browser_select(terminal, state.clone()).await?;
        if !continue_run {
            return Ok(());
        }
    }

    let continue_run = run_ngrok_auth_setup(terminal, state.clone()).await?;
    if !continue_run {
        return Ok(());
    }

    let continue_run = run_ngrok_domain_setup(terminal, state.clone()).await?;
    if !continue_run {
        return Ok(());
    }

    // Start services
    let (ui_event_tx, mut ui_event_rx) = channel(crate::state::UI_EVENT_CAPACITY);
    let devtools_bridge = start_services(state.clone(), ui_event_tx).await;

    run_chatgpt_connector_refresh_notice(terminal, state.clone(), &mut ui_event_rx).await?;

    // Phase 2: main TUI loop
    run_tui(
        terminal,
        state,
        devtools_bridge,
        ui_event_rx,
        session_started_at,
    )
    .await
}

async fn run_prompt(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    prompt_title: &str,
    initial_value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut input = initial_value.to_string();
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|f| {
                let area = centered_rect(60, 20, f.area());
                let block = Block::default()
                    .title(prompt_title)
                    .borders(Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .style(Style::default().fg(Color::Yellow));

                let text = Paragraph::new(format!("> {}_", input))
                    .block(block)
                    .wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(ratatui::widgets::Clear, area);
                f.render_widget(text, area);
            })?;
            redraw = false;
        }

        if crossterm::event::poll(std::time::Duration::from_millis(100))? {
            redraw = true;
            let event = crossterm::event::read()?;
            match event {
                crossterm::event::Event::Paste(text) => {
                    input.push_str(&text);
                }
                crossterm::event::Event::Key(key) => {
                    if key.kind != crossterm::event::KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        crossterm::event::KeyCode::Enter => return Ok(Some(input)),
                        crossterm::event::KeyCode::Esc => return Ok(None),
                        crossterm::event::KeyCode::Backspace => {
                            input.pop();
                        }
                        crossterm::event::KeyCode::Char(c) => {
                            input.push(c);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

async fn mode_is_browser_enabled(state: SharedState) -> bool {
    state.lock().await.mode.browser_enabled()
}

fn browser_identity_matches(
    browser: &browser::DetectedBrowser,
    selected: &browser::DetectedBrowser,
) -> bool {
    browser.path == selected.path && browser.binary == selected.binary
}

fn selected_supported_browser_idx(
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

async fn run_browser_select(
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

async fn persist_selected_browser(state: SharedState, selected: browser::DetectedBrowser) {
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

fn draw_browser_select(
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

fn find_available_remote_debug_port(start: u16, end: u16) -> Option<u16> {
    (start..=end).find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
}

fn sanitize_for_filename(input: &str) -> String {
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

async fn wait_remote_debug_ready(port: u16, timeout: Duration) -> bool {
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/json/version");
    let started = Instant::now();
    while started.elapsed() < timeout {
        let result = client
            .get(&endpoint)
            .timeout(Duration::from_millis(600))
            .send()
            .await;
        if let Ok(response) = result {
            if response.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

async fn ensure_selected_browser_remote_debugging(
    state: SharedState,
    selected_browser: Option<browser::DetectedBrowser>,
) -> Option<browser::DetectedBrowser> {
    let Some(mut selected) = selected_browser else {
        return None;
    };
    if !selected.mcp_supported {
        state.lock().await.log(
            "ERROR",
            format!(
                "Selected browser {} is not supported yet for chrome-devtools-mcp",
                selected.name
            ),
        );
        return None;
    }
    if selected.remote_debug_active && selected.remote_debug_target.is_some() {
        return Some(selected);
    }

    let Some(port) = find_available_remote_debug_port(9222, 9322) else {
        state.lock().await.log(
            "ERROR",
            "No available local port in range 9222-9322 for remote debugging".into(),
        );
        return Some(selected);
    };

    let user_data_dir = format!(
        "/tmp/catdesk-remote-debug-{}",
        sanitize_for_filename(&selected.binary)
    );
    if let Err(e) = std::fs::create_dir_all(&user_data_dir) {
        state.lock().await.log(
            "WARN",
            format!("Failed to create user data dir {user_data_dir}: {e}"),
        );
    }

    let mut command = tokio::process::Command::new(&selected.path);
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--user-data-dir={user_data_dir}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            state.lock().await.log(
                "ERROR",
                format!(
                    "Failed to launch {} with remote debugging: {}",
                    selected.name, e
                ),
            );
            return Some(selected);
        }
    };
    let launched_pid = child.id();

    let existing_child = {
        let mut app = state.lock().await;
        app.remote_browser_child.take()
    };
    if let Some(mut old_child) = existing_child {
        let _ = old_child.kill().await;
    }

    {
        let mut app = state.lock().await;
        app.remote_browser_child = Some(child);
        app.log(
            "INFO",
            format!(
                "Launched {} with remote debugging on 127.0.0.1:{}",
                selected.name, port
            ),
        );
    }

    if wait_remote_debug_ready(port, Duration::from_secs(10)).await {
        selected.remote_debug_active = true;
        selected.remote_debug_target = Some(format!("127.0.0.1:{port}"));
        selected.remote_debug_pid = launched_pid;
        {
            let mut app = state.lock().await;
            app.selected_browser = Some(selected.clone());
            app.log(
                "INFO",
                format!(
                    "Remote debugging ready for {} at 127.0.0.1:{}",
                    selected.name, port
                ),
            );
            app.persist_state_with_log();
        }
        Some(selected)
    } else {
        state.lock().await.log(
            "WARN",
            format!(
                "Remote debugging endpoint for {} did not become ready in time",
                selected.name
            ),
        );
        Some(selected)
    }
}

// ── Start services ──────────────────────────────────────────

async fn reserve_mcp_listener(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await
}

async fn start_services(
    state: SharedState,
    ui_events: Sender<ServerUiEvent>,
) -> Option<Arc<Mutex<DevtoolsBridge>>> {
    let (port, mode, mut detected_browsers, mut selected_browser) = {
        let app = state.lock().await;
        (
            app.port,
            app.mode,
            app.detected_browsers.clone(),
            app.selected_browser.clone(),
        )
    };

    // Reserve the origin port before browser/DevTools startup. Those steps may
    // legitimately take several seconds, and leaving the port unbound makes an
    // otherwise healthy external tunnel fail immediately with connection refused.
    let listener = match reserve_mcp_listener(port).await {
        Ok(listener) => listener,
        Err(error) => {
            diagnostics::event("server_bind_failed");
            state
                .lock()
                .await
                .log("ERROR", format!("Failed to bind port {port}: {error}"));
            return None;
        }
    };

    if mode.browser_enabled() && detected_browsers.is_empty() {
        detected_browsers = browser::detect_browsers();
    }
    if mode.browser_enabled() {
        selected_browser =
            ensure_selected_browser_remote_debugging(state.clone(), selected_browser).await;
        detected_browsers = browser::detect_browsers();
        if let Some(selected) = &selected_browser {
            if let Some(refreshed) = detected_browsers
                .iter()
                .find(|b| b.path == selected.path && b.binary == selected.binary)
                .cloned()
            {
                selected_browser = Some(refreshed);
            }
        }
        let mut app = state.lock().await;
        app.detected_browsers = detected_browsers.clone();
        app.selected_browser = selected_browser.clone();
        app.persist_state_with_log();
    }

    let browser_summary = browser::format_browser_names(&detected_browsers);
    let remote_support_summary = browser::format_remote_debug_names(&detected_browsers);
    let remote_active_summary = browser::format_active_remote_debug_names(&detected_browsers);
    let browser_details: Vec<String> = detected_browsers
        .iter()
        .map(|b| {
            format!(
                "Browser: {} (binary: {}, path: {}, support: {}, remote debug flag: {}, remote debug active: {}, pid: {})",
                b.name,
                b.binary,
                b.path,
                b.support_note,
                b.remote_debug_hint,
                b.remote_debug_target.as_deref().unwrap_or("no"),
                b.remote_debug_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "--".into())
            )
        })
        .collect();
    {
        let mut app = state.lock().await;
        app.detected_browsers = detected_browsers;
        if browser_summary == "--" {
            app.log("WARN", "No local browser found in PATH".into());
        } else {
            app.log("INFO", format!("Local browsers: {browser_summary}"));
        }
        if remote_support_summary == "--" {
            app.log(
                "WARN",
                "No detected browser supports remote debugging".into(),
            );
        } else {
            app.log(
                "INFO",
                format!("Remote debugging supported: {remote_support_summary}"),
            );
        }
        if remote_active_summary == "--" {
            app.log(
                "WARN",
                "No browser currently runs with remote debugging".into(),
            );
        } else {
            app.log(
                "INFO",
                format!("Remote debugging active: {remote_active_summary}"),
            );
        }
        if mode.browser_enabled() {
            if let Some(selected) = &selected_browser {
                let target = selected
                    .remote_debug_target
                    .as_deref()
                    .unwrap_or("launch new browser instance");
                app.log(
                    "INFO",
                    format!(
                        "Using browser: {} ({}) -> {}",
                        selected.name, selected.path, target
                    ),
                );
            } else {
                app.log("WARN", "No browser was selected before startup".into());
            }
        }
        for detail in browser_details {
            app.log("INFO", detail);
        }
    }

    // Start MCP HTTP server
    let devtools_bridge = if mode.browser_enabled() {
        if selected_browser.is_none() {
            state.lock().await.log(
                "ERROR",
                "Browser mode requires selecting a supported Chromium browser".into(),
            );
            None
        } else {
            state
                .lock()
                .await
                .log("INFO", "Starting chrome-devtools-mcp...".into());
            match DevtoolsBridge::start(selected_browser.as_ref()).await {
                Ok(bridge) => {
                    let mut app = state.lock().await;
                    app.devtools_running = true;
                    app.log("INFO", "chrome-devtools-mcp started".into());
                    Some(bridge)
                }
                Err(e) => {
                    let mut app = state.lock().await;
                    app.log("ERROR", format!("chrome-devtools-mcp: {e}"));
                    None
                }
            }
        }
    } else {
        None
    };

    let (mcp_path, command_jobs) = {
        let app = state.lock().await;
        (app.mcp_path(), app.command_jobs.clone())
    };
    let router = server::router(
        state.clone(),
        devtools_bridge.clone(),
        command_jobs,
        mcp_path,
        ui_events,
    );
    // Feed the dashboard SYS line; the sampler only ever writes atomics.
    perf_metrics::spawn_system_sampler();
    let handle = tokio::spawn(async move {
        diagnostics::event("server_started");
        match axum::serve(listener, router).await {
            Ok(()) => diagnostics::event("server_stopped"),
            Err(_) => diagnostics::event("server_failed"),
        }
    });

    {
        let mut app = state.lock().await;
        app.server_running = true;
        app.server_handle = Some(handle);
        app.log("INFO", format!("MCP Server started on port {port}"));
    }

    // Start ngrok
    if let Err(e) = ngrok::start(state.clone()).await {
        diagnostics::event("tunnel_start_failed");
        state.lock().await.log("ERROR", format!("ngrok: {e}"));
    }

    devtools_bridge
}

// ── Phase 2: Main TUI ──────────────────────────────────────

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    mut ui_events: Receiver<ServerUiEvent>,
    session_started_at: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut log_scroll: usize = 0;
    let mut log_follow_tail = true;
    let mut last_log_max_scroll: usize = 0;
    let mut last_log_effective_scroll: usize = 0;
    let mut last_log_view: Option<LogView> = None;
    let mut selection = Selection::new();
    // (message, position (col, row), created_at)
    let mut toast: Option<(&str, (u16, u16), Instant)> = None;
    #[allow(unused_assignments)]
    let mut screen_lines: Vec<String> = vec![];
    let mut last_animation_snapshot = String::new();
    #[allow(unused_assignments)]
    let mut last_mcp_url: Option<String> = None;
    let mut mcp_url_revealed_until: Option<Instant> = None;
    let mut log_secret_revealed_until: HashMap<u64, Instant> = HashMap::new();
    let mut current_ui_language = UiLanguage::English;
    let mut active_job_count = 0usize;
    let mut last_live_telemetry_refresh = Instant::now()
        .checked_sub(LIVE_TELEMETRY_REFRESH_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut redraw_dirty = true;
    let mut last_draw = Instant::now()
        .checked_sub(UI_IDLE_REDRAW_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_frame_width = 0u16;

    loop {
        if last_live_telemetry_refresh.elapsed() >= LIVE_TELEMETRY_REFRESH_INTERVAL {
            let command_jobs = state.try_lock().ok().map(|app| app.command_jobs.clone());
            if let Some(command_jobs) = command_jobs {
                let refreshed_active_job_count = command_jobs.active_job_count().await;
                if refreshed_active_job_count != active_job_count {
                    active_job_count = refreshed_active_job_count;
                    redraw_dirty = true;
                }
                last_live_telemetry_refresh = Instant::now();
            }
        }

        if let Some((_, _, t)) = &toast {
            if t.elapsed().as_secs() >= 2 {
                toast = None;
                redraw_dirty = true;
            }
        }

        // Persistence or another service may temporarily own the state. Keep
        // polling the keyboard against the last frame, especially the quit key.
        if let Ok(mut app) = state.try_lock() {
            if let Some(bridge) = devtools.as_ref() {
                if let Ok(bridge) = bridge.try_lock() {
                    let connected = bridge.is_connected();
                    if app.devtools_running != connected {
                        app.devtools_running = connected;
                        redraw_dirty = true;
                    }
                }
            }
            current_ui_language = app.ui_language;
            if drain_server_ui_events(&mut app, &mut ui_events) {
                redraw_dirty = true;
            }
            let flow_count_before = app.flows.len();
            app.prune_closed_flows();
            if app.flows.len() != flow_count_before {
                redraw_dirty = true;
            }
            let reveal_remaining = mcp_url_revealed_until
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()));
            if mcp_url_revealed_until.is_some() && reveal_remaining.is_none() {
                mcp_url_revealed_until = None;
                redraw_dirty = true;
            }
            let now = Instant::now();
            let revealed_log_count = log_secret_revealed_until.len();
            log_secret_revealed_until
                .retain(|_, deadline| deadline.checked_duration_since(now).is_some());
            if log_secret_revealed_until.len() != revealed_log_count {
                redraw_dirty = true;
            }
            last_mcp_url = app.public_mcp_url();
            let toast_ref = toast
                .as_ref()
                .filter(|(_, _, t)| t.elapsed().as_secs() < 2)
                .map(|(m, pos, _)| (*m, *pos));
            let has_flow_animation = app
                .flows
                .iter()
                .any(|flow| !flow.anim_queue.is_empty() || flow.closing_started_ms.is_some());
            let timed_overlay_active = reveal_remaining.is_some()
                || !log_secret_revealed_until.is_empty()
                || toast_ref.is_some();
            let redraw_interval = if has_flow_animation {
                UI_POLL_INTERVAL
            } else if last_frame_width >= 120 && app.mascot.tui_frames.len() > 1 {
                Duration::from_millis(app.mascot.frame_ms.max(1))
            } else if timed_overlay_active {
                UI_TIMED_REDRAW_INTERVAL
            } else {
                UI_IDLE_REDRAW_INTERVAL
            };

            if redraw_due(redraw_dirty, last_draw.elapsed(), Some(redraw_interval)) {
                let mut new_lines: Vec<String> = Vec::new();
                let mut latest_log_view: Option<LogView> = None;
                terminal.draw(|f| {
                    draw_ui(
                        f,
                        &app,
                        active_job_count,
                        session_started_at.elapsed(),
                        log_scroll,
                        log_follow_tail,
                        &mut latest_log_view,
                        toast_ref,
                        reveal_remaining,
                        &log_secret_revealed_until,
                    );

                    if let Some(((c0, r0), (c1, r1))) = selection.range() {
                        let palette = app.current_theme().palette;
                        let area = f.area();
                        for row in r0..=r1 {
                            if row >= area.height {
                                break;
                            }
                            let cs = if row == r0 { c0 } else { 0 };
                            let ce = if row == r1 {
                                c1
                            } else {
                                area.width.saturating_sub(1)
                            };
                            for col in cs..=ce {
                                if col >= area.width {
                                    break;
                                }
                                if let Some(cell) = f.buffer_mut().cell_mut((col, row)) {
                                    cell.set_style(
                                        Style::default()
                                            .bg(palette.selection_bg)
                                            .fg(palette.selection_fg),
                                    );
                                }
                            }
                        }
                    }

                    let area = f.area();
                    last_frame_width = area.width;
                    let buf = f.buffer_mut();
                    for row in 0..area.height {
                        let mut line = String::new();
                        for col in 0..area.width {
                            line.push_str(buf[(col, row)].symbol());
                        }
                        new_lines.push(line);
                    }
                })?;
                if let Some(log_view) = latest_log_view {
                    last_log_max_scroll = log_view.max_scroll;
                    last_log_effective_scroll = log_view.effective_scroll;
                    last_log_view = Some(log_view);
                    if !log_follow_tail && log_scroll > last_log_max_scroll {
                        log_scroll = last_log_max_scroll;
                    }
                }
                screen_lines = new_lines;
                let snapshots = build_animation_snapshot(&app);
                if !snapshots.is_empty() {
                    let snapshot_joined = snapshots.join("\n");
                    if snapshot_joined != last_animation_snapshot {
                        last_animation_snapshot = snapshot_joined;
                    }
                }
                redraw_dirty = false;
                last_draw = Instant::now();
            }
        }

        if event::poll(UI_POLL_INTERVAL)? {
            redraw_dirty = true;
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    selection.clear();
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('e') => {
                            let export_result = {
                                let app = state.lock().await;
                                export_logs(&app.logs)
                            };
                            let mut app = state.lock().await;
                            match export_result {
                                Ok(path) => {
                                    app.log(
                                        "INFO",
                                        format!("Exported logs to {}", path.to_string_lossy()),
                                    );
                                    toast = Some((
                                        current_ui_language.text("Logs exported", "紀錄已匯出"),
                                        (2, 2),
                                        Instant::now(),
                                    ));
                                }
                                Err(error) => {
                                    app.log("ERROR", format!("Failed to export logs: {error}"));
                                    toast = Some((
                                        current_ui_language
                                            .text("Log export failed", "紀錄匯出失敗"),
                                        (2, 2),
                                        Instant::now(),
                                    ));
                                }
                            }
                        }
                        KeyCode::Up => {
                            if log_follow_tail {
                                log_follow_tail = false;
                                log_scroll = last_log_effective_scroll.saturating_sub(1);
                            } else {
                                log_scroll = log_scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if !log_follow_tail {
                                log_scroll = (log_scroll + 1).min(last_log_max_scroll);
                                if log_scroll >= last_log_max_scroll {
                                    log_follow_tail = true;
                                }
                            }
                        }
                        KeyCode::End => {
                            log_follow_tail = true;
                            log_scroll = last_log_max_scroll;
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        selection.start = Some((mouse.column, mouse.row));
                        selection.end = Some((mouse.column, mouse.row));
                        selection.dragging = true;
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if selection.dragging {
                            selection.end = Some((mouse.column, mouse.row));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if selection.dragging {
                            selection.end = Some((mouse.column, mouse.row));
                            selection.dragging = false;
                            if let Some((start, end)) = selection.range() {
                                if start != end {
                                    let text = extract_from_screen(&screen_lines, start, end);
                                    if !text.is_empty() {
                                        let message = if clipboard_copy(&text) {
                                            current_ui_language.text("Copied!", "已複製！")
                                        } else {
                                            current_ui_language.text("Copy failed", "複製失敗")
                                        };
                                        toast = Some((
                                            message,
                                            (mouse.column, mouse.row),
                                            Instant::now(),
                                        ));
                                    }
                                } else {
                                    let row = start.1 as usize;
                                    if row < screen_lines.len() {
                                        let line = &screen_lines[row];
                                        let clicked_log = last_log_view.as_ref().and_then(|view| {
                                            view.log_id_at(mouse.column, mouse.row)
                                        });
                                        let clicked_log = if let Some(log_id) = clicked_log {
                                            let app = state.lock().await;
                                            app.logs
                                                .iter()
                                                .find(|entry| entry.id == log_id)
                                                .map(|entry| (entry.id, entry.message.clone()))
                                        } else {
                                            None
                                        };
                                        let copy_value = if let Some((log_id, message)) =
                                            clicked_log.as_ref().filter(|(_, message)| {
                                                is_secret_log_message(message)
                                            }) {
                                            let revealed = log_secret_revealed_until
                                                .get(log_id)
                                                .and_then(|deadline| {
                                                    deadline.checked_duration_since(Instant::now())
                                                })
                                                .is_some();
                                            if revealed {
                                                secret_log_copy_value(message)
                                            } else {
                                                let now = Instant::now();
                                                log_secret_revealed_until
                                                    .insert(*log_id, now + MCP_URL_REVEAL_DURATION);
                                                let reveal_message = if message
                                                    .starts_with("Auto-saved ngrok static domain: ")
                                                {
                                                    current_ui_language.text(
                                                        "Domain revealed for 10s",
                                                        "網域顯示 10 秒",
                                                    )
                                                } else if post_mcp_path(message).is_some() {
                                                    current_ui_language.text(
                                                        "MCP path revealed for 10s",
                                                        "MCP 路徑顯示 10 秒",
                                                    )
                                                } else {
                                                    current_ui_language.text(
                                                        "URL revealed for 10s",
                                                        "URL 顯示 10 秒",
                                                    )
                                                };
                                                toast = Some((
                                                    reveal_message,
                                                    (mouse.column, mouse.row),
                                                    now,
                                                ));
                                                None
                                            }
                                        } else if line.contains("chatgpt.com/apps") {
                                            Some(CHATGPT_CONNECTOR_SETTINGS_URL.to_string())
                                        } else if let Some(ref url) = last_mcp_url {
                                            let prefix = &url[..url.len().min(30)];
                                            if line.contains("MCP Server URL")
                                                || line.contains("MCP 伺服器 URL")
                                                || line.contains(prefix)
                                            {
                                                let revealed = mcp_url_revealed_until
                                                    .and_then(|deadline| {
                                                        deadline
                                                            .checked_duration_since(Instant::now())
                                                    })
                                                    .is_some();
                                                if revealed {
                                                    Some(url.clone())
                                                } else {
                                                    let now = Instant::now();
                                                    mcp_url_revealed_until =
                                                        Some(now + MCP_URL_REVEAL_DURATION);
                                                    toast = Some((
                                                        current_ui_language.text(
                                                            "URL revealed for 10s",
                                                            "URL 顯示 10 秒",
                                                        ),
                                                        (mouse.column, mouse.row),
                                                        now,
                                                    ));
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                        .or_else(|| {
                                            if line.contains("\u{2502}") {
                                                if line.contains("Name") || line.contains("名稱")
                                                {
                                                    Some("CatDesk".to_string())
                                                } else if line.contains("Authentication")
                                                    || line.contains("驗證方式")
                                                {
                                                    Some("None".to_string())
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });
                                        if let Some(text) = copy_value {
                                            let message = if clipboard_copy(&text) {
                                                current_ui_language.text("Copied!", "已複製！")
                                            } else {
                                                current_ui_language.text("Copy failed", "複製失敗")
                                            };
                                            toast = Some((
                                                message,
                                                (mouse.column, mouse.row),
                                                Instant::now(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if log_follow_tail {
                            log_follow_tail = false;
                            log_scroll = last_log_effective_scroll.saturating_sub(1);
                        } else {
                            log_scroll = log_scroll.saturating_sub(1);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if !log_follow_tail {
                            log_scroll = (log_scroll + 1).min(last_log_max_scroll);
                            if log_scroll >= last_log_max_scroll {
                                log_follow_tail = true;
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    Ok(())
}

// ── Draw main UI ────────────────────────────────────────────

fn draw_ui(
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

#[cfg(test)]
mod test_serialization {
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Serialize against every other env-dependent test across modules.
    /// Poisoning is tolerated: a failed test must not cascade into the rest.
    pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests;
