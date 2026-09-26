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
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use state::{
    AppState, Mode, ServerUiEvent, SharedState, UiLanguage,
    load_macos_terminal_profile, save_macos_terminal_profile, user_home_dir,
};
use tui::{
    LogView, Selection, build_animation_snapshot, centered_rect, clipboard_copy,
    draw_mode_select, post_mcp_path,
    draw_ui, export_logs, extract_from_screen, find_available_remote_debug_port,
    is_secret_log_message, mode_is_browser_enabled, run_browser_select,
    run_chatgpt_connector_refresh_notice, run_ngrok_auth_setup, run_ngrok_domain_setup,
    run_settings, sanitize_for_filename, secret_log_copy_value,
};
use std::collections::HashMap;
use std::io::{Write, stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
