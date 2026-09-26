    use super::state::{AppState, ToolMode, UiLanguage};
    use super::{
        draw_chatgpt_connector_refresh_notice, draw_mode_select, draw_settings,
        draw_tui_header, draw_ui, format_session_duration,
        key_is_clipboard_paste,
        normalize_ngrok_authtoken_input, pad_right_to_cell_width, parse_terminal_profile_choice,
        redraw_due, terminal_cell_width, text_input_key_is_cancel, trim_line,
    };
    use crate::tui::{
        LogView, export_logs_to_dir, format_log_export_filename, localize_log_message,
        mask_mcp_path_in_log, wrap_log_message,
    };
    use crate::build_info;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn terminal_buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn redraw_gate_skips_idle_frames_but_honors_dirty_and_timed_ticks() {
        let elapsed = Duration::from_millis(40);
        assert!(redraw_due(true, elapsed, None));
        assert!(!redraw_due(false, elapsed, None));
        assert!(!redraw_due(false, elapsed, Some(Duration::from_millis(50))));
        assert!(redraw_due(
            false,
            Duration::from_millis(50),
            Some(Duration::from_millis(50))
        ));
    }

    #[test]
    fn session_duration_format_is_compact() {
        assert_eq!(format_session_duration(Duration::from_secs(12)), "12s");
        assert_eq!(
            format_session_duration(Duration::from_secs(8 * 60 + 14)),
            "8m 14s"
        );
        assert_eq!(
            format_session_duration(Duration::from_secs(60 * 60 + 23 * 60 + 45)),
            "1h 23m"
        );
    }

    #[test]
    fn mode_select_renders_english_and_traditional_chinese() {
        let theme = super::theme::all()[0];

        let mut english = Terminal::new(TestBackend::new(100, 24)).expect("create terminal");
        english
            .draw(|frame| {
                draw_mode_select(frame, &theme, ToolMode::MultiTools, UiLanguage::English)
            })
            .expect("draw english mode selection");
        let english_text = terminal_buffer_text(&english);
        assert!(english_text.contains("Select mode"));
        assert!(english_text.contains("Control Computer"));
        assert!(english_text.contains("Language: English"));

        let mut chinese = Terminal::new(TestBackend::new(100, 24)).expect("create terminal");
        chinese
            .draw(|frame| {
                draw_mode_select(
                    frame,
                    &theme,
                    ToolMode::MultiTools,
                    UiLanguage::TraditionalChinese,
                )
            })
            .expect("draw traditional chinese mode selection");
        let chinese_text = terminal_buffer_text(&chinese);
        let chinese_compact = chinese_text.replace(' ', "");
        assert!(chinese_compact.contains("選擇模式"));
        assert!(chinese_compact.contains("控制電腦"));
        assert!(chinese_compact.contains("控制瀏覽器"));
        assert!(chinese_compact.contains("語言：繁體中文"));
        assert!(chinese_compact.contains("主題簡潔"));
        assert!(chinese_compact.contains("工具模式多工具"));
        assert!(chinese_compact.contains("離開"));
    }

    #[test]
    fn settings_renders_traditional_chinese_theme_names_and_descriptions() {
        let theme = super::theme::all()[0];
        let mut terminal = Terminal::new(TestBackend::new(140, 50)).expect("create terminal");
        terminal
            .draw(|frame| {
                draw_settings(
                    frame,
                    &theme,
                    ToolMode::MultiTools,
                    super::ShowDetailMode::Expanded,
                    super::WidgetCornerStyle::Rounded,
                    UiLanguage::TraditionalChinese,
                    false,
                    "test-slug",
                    None,
                    &super::UsageTotals::default(),
                    0,
                    false,
                )
            })
            .expect("draw traditional chinese settings");

        let text = terminal_buffer_text(&terminal).replace(' ', "");
        for expected in [
            "選擇主題",
            "簡潔",
            "黑／灰／白的極簡介面，減少色彩使用。",
            "霓虹",
            "賽博龐克粉紅點綴與霓虹高亮。",
        ] {
            assert!(
                text.contains(expected),
                "missing translated theme text: {expected}"
            );
        }
    }

    #[test]
    fn ui_event_drain_yields_before_emptying_a_busy_queue() {
        let root = std::env::temp_dir().join(format!("catdesk-ui-drain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = AppState::new_for_test(0, root.to_string_lossy().into_owned(), root.join("config.toml")).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(crate::state::UI_EVENT_CAPACITY);
        for _ in 0..300 {
            sender.try_send(crate::state::ServerUiEvent::IncrementRequestCount).unwrap();
        }
        super::drain_server_ui_events(&mut app, &mut receiver);
        assert!(app.request_count > 0 && app.request_count < 300);
        super::drain_server_ui_events(&mut app, &mut receiver);
        assert_eq!(app.request_count, 300);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn main_dashboard_renders_traditional_chinese() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-zh-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");
        app.ui_language = UiLanguage::TraditionalChinese;
        app.log("WARN", "No local browser found in PATH".into());
        app.log("INFO", "MCP Server started on port 3200".into());

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::ZERO,
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw main dashboard");

        let text = terminal_buffer_text(&terminal).replace(' ', "");
        for expected in [
            "讓ChatGPTWeb變成程式代理",
            "狀態",
            "即時請求",
            "聊天",
            "工作",
            "工作階段請求",
            "累計請求",
            "效能",
            "即時成本",
            "工作階段成本",
            "今日成本",
            "平均",
            "已花費",
            "呼叫",
            "累計成本",
            "追蹤成本",
            "天數",
            "Token60秒",
            "系統",
            "等待連線",
            "你的電腦",
            "按鍵",
            "離開",
            "捲動",
            "最新",
            "匯出紀錄",
            "紀錄",
            "在PATH中找不到本機瀏覽器",
            "MCP伺服器已啟動，連接埠3200",
        ] {
            assert!(
                text.contains(expected),
                "missing translated text: {expected}"
            );
        }

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_renders_live_telemetry() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-live-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");
        app.record_flow(
            "session:a",
            &["tools/call:read".to_string()],
            super::FlowDirection::Forward,
        );
        app.record_flow(
            "session:b",
            &["tools/call:search".to_string()],
            super::FlowDirection::Forward,
        );
        app.usage_by_model
            .entry(super::usage_pricing::FALLBACK_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(0, 1_600_000, 20);
        app.record_turn_usage(super::usage_pricing::FALLBACK_USAGE_BUCKET, 0, 1_000_000);
        app.daily_usage_by_model
            .entry("1900-01-01".to_string())
            .or_default()
            .entry(super::usage_pricing::FALLBACK_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(0, 600_000, 3);
        app.request_count = 42;
        app.total_request_count = 142;

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    3,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw main dashboard");

        let text = terminal_buffer_text(&terminal);
        for expected in [
            "REQ NOW",
            "CHATS 2",
            "JOBS 3",
            "REQ SESSION",
            "42",
            "REQ TOTAL",
            "142",
            "COST NOW",
            "$300/h",
            "$5/min",
            "COST SESSION",
            "AVG $150/h",
            "SPENT $5",
            "2m 0s",
            "COST TODAY",
            "AVG $5/call",
            "CALLS 1",
            "COST TOTAL",
            "SPENT $13",
            "COST TRACKED",
            "SPENT $8",
            "DAYS 2",
            "AVG $4/day",
            "TOKENS 60s",
            "↓REQ",
            "↑RES",
            "SYSTEM",
        ] {
            assert!(
                text.contains(expected),
                "missing live telemetry text: {expected}"
            );
        }
        for hidden in ["Workspace", "All-time", "MCP Server URL"] {
            assert!(
                !text.contains(hidden),
                "normal status should hide low-priority field: {hidden}"
            );
        }
        for expected in ["PERF", "p50", "p95", "p99", "ACT", "DL", "CACHE", "SCAN p95"] {
            assert!(
                text.contains(expected),
                "missing perf dashboard text: {expected}"
            );
        }
        assert!(!text.contains("(tool input, llm output)"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_separates_legacy_total_from_tracked_daily_average() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-cost-history-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");

        // $1,924.70 of legacy usage has no trustworthy per-day history.
        app.usage_by_model
            .entry(super::state::GPT_5_6_AND_EARLIER_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(0, 384_940_000, 1_000);
        // The new daily tracker knows only about this $0.30 call today.
        app.record_turn_usage(super::usage_pricing::FALLBACK_USAGE_BUCKET, 0, 60_000);

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw main dashboard");

        let text = terminal_buffer_text(&terminal);
        let total_line = text
            .lines()
            .find(|line| line.contains("COST TOTAL"))
            .expect("COST TOTAL line");
        assert!(total_line.contains("SPENT $1925"), "{total_line}");
        assert!(!total_line.contains("DAYS"), "{total_line}");
        assert!(!total_line.contains("AVG"), "{total_line}");

        let tracked_line = text
            .lines()
            .find(|line| line.contains("COST TRACKED"))
            .expect("COST TRACKED line");
        assert!(tracked_line.contains("SPENT $0.3"), "{tracked_line}");
        assert!(tracked_line.contains("DAYS 1"), "{tracked_line}");
        assert!(tracked_line.contains("AVG $0.3/day"), "{tracked_line}");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_renders_unknown_usage_bucket_without_panicking() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-unknown-bucket-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");

        // A model bucket with no registry entry: nothing can be priced, the draw
        // below must render instead of panicking.
        app.usage_by_model
            .entry("model:gpt-9-future".to_string())
            .or_default()
            .accumulate(1_000_000, 1_000_000, 2);

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("unknown usage buckets must not panic the dashboard");

        let text = terminal_buffer_text(&terminal);
        let total_line = text
            .lines()
            .find(|line| line.contains("COST TOTAL"))
            .expect("COST TOTAL line");
        assert!(total_line.contains("SPENT N/A"), "{total_line}");
        assert!(!total_line.contains('$'), "{total_line}");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_marks_partially_priced_usage_with_unpriced_tail() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-mixed-pricing-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");

        // Legacy history prices at $65; the unknown future model adds unpriced
        // tokens that must surface as a "+N/A" tail instead of vanishing.
        app.usage_by_model
            .entry(super::state::GPT_5_6_AND_EARLIER_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(2_000_000, 1_000_000, 3);
        app.usage_by_model
            .entry("model:gpt-9-future".to_string())
            .or_default()
            .accumulate(4_000_000, 4_000_000, 2);

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw mixed pricing dashboard");

        let text = terminal_buffer_text(&terminal);
        let total_line = text
            .lines()
            .find(|line| line.contains("COST TOTAL"))
            .expect("COST TOTAL line");
        assert!(total_line.contains("SPENT $65 +N/A"), "{total_line}");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_combines_priced_model_buckets_into_one_total() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-combined-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");

        // Historic legacy usage ($65) plus current unattributed turns ($5): both
        // buckets are priced, so the total sums them without any N/A tail.
        app.usage_by_model
            .entry(super::state::GPT_5_6_AND_EARLIER_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(2_000_000, 1_000_000, 3);
        app.usage_by_model
            .entry(super::usage_pricing::FALLBACK_USAGE_BUCKET.to_string())
            .or_default()
            .accumulate(0, 1_000_000, 1);

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw combined pricing dashboard");

        let text = terminal_buffer_text(&terminal);
        let total_line = text
            .lines()
            .find(|line| line.contains("COST TOTAL"))
            .expect("COST TOTAL line");
        assert!(total_line.contains("SPENT $70"), "{total_line}");
        assert!(!total_line.contains("N/A"), "{total_line}");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_renders_only_latest_active_flow_row() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-flow-row-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");
        app.server_running = true;
        app.ngrok_running = true;
        app.record_flow(
            "session:a",
            &["tools/call:run_command › echo dashboard".to_string()],
            super::FlowDirection::Forward,
        );
        app.record_flow_turn_usage("session:a", 45, 2_100);
        app.record_flow(
            "session:b",
            &["tools/call:poll_command › job 123".to_string()],
            super::FlowDirection::Forward,
        );
        app.record_flow(
            "session:c",
            &["tools/call:search › dashboard status".to_string()],
            super::FlowDirection::Forward,
        );
        app.record_flow_turn_usage("session:c", 12, 345);

        let mut terminal = Terminal::new(TestBackend::new(180, 44)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw flow dashboard");

        let text = terminal_buffer_text(&terminal);
        let flow_rows = text
            .lines()
            .filter(|line| line.contains("Your computer"))
            .collect::<Vec<_>>();
        assert_eq!(
            flow_rows.len(),
            1,
            "normal status must collapse all active flows into one dashboard row: {flow_rows:?}"
        );
        let row = flow_rows[0];
        for expected in ["ChatGPT Web", "search", "↓REQ 12", "↑RES 345"] {
            assert!(
                row.contains(expected),
                "latest flow row missing {expected}: {row}"
            );
        }
        for stale in ["run_command", "poll_command"] {
            assert!(
                !row.contains(stale),
                "latest flow row must not show stale action {stale}: {row}"
            );
        }

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn main_dashboard_keeps_requests_visible_without_flow_slots() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("catdesk-main-requests-{unique}"));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let config_path = workspace.join("config.toml");
        let mut app =
            AppState::new_for_test(3200, workspace.to_string_lossy().into_owned(), config_path)
                .expect("create app");
        app.request_count = 42;
        app.total_request_count = 142;

        let mut terminal = Terminal::new(TestBackend::new(180, 28)).expect("create terminal");
        let mut log_view = None;
        let revealed_logs = HashMap::new();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    &app,
                    0,
                    Duration::from_secs(120),
                    0,
                    true,
                    &mut log_view,
                    None,
                    None,
                    &revealed_logs,
                )
            })
            .expect("draw compact dashboard");

        let text = terminal_buffer_text(&terminal);
        assert!(
            text.contains("REQ SESSION") && text.contains("42"),
            "session request counter must remain in the always-visible status area"
        );
        assert!(
            text.contains("REQ TOTAL") && text.contains("142"),
            "persisted total request counter must remain in the always-visible status area"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn parses_terminal_profile_choice() {
        assert_eq!(parse_terminal_profile_choice(""), Some(true));
        assert_eq!(parse_terminal_profile_choice(" y "), Some(true));
        assert_eq!(parse_terminal_profile_choice("YES"), Some(true));
        assert_eq!(parse_terminal_profile_choice("n"), Some(false));
        assert_eq!(parse_terminal_profile_choice(" No "), Some(false));
        assert_eq!(parse_terminal_profile_choice("maybe"), None);
    }

    #[test]
    fn normalizes_plain_ngrok_token() {
        assert_eq!(
            normalize_ngrok_authtoken_input("  test-token-123  "),
            "test-token-123"
        );
    }

    #[test]
    fn extracts_token_from_ngrok_command() {
        assert_eq!(
            normalize_ngrok_authtoken_input("ngrok config add-authtoken test-token-123"),
            "test-token-123"
        );
    }

    #[test]
    fn detects_ctrl_v_as_clipboard_paste() {
        assert!(key_is_clipboard_paste(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn detects_shift_insert_as_clipboard_paste() {
        assert!(key_is_clipboard_paste(&KeyEvent::new(
            KeyCode::Insert,
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn q_does_not_cancel_text_input() {
        assert!(!text_input_key_is_cancel(KeyCode::Char('q')));
        assert!(!text_input_key_is_cancel(KeyCode::Char('Q')));
        assert!(text_input_key_is_cancel(KeyCode::Esc));
    }

    #[test]
    fn masks_slug_in_post_mcp_logs_until_revealed() {
        let message = "POST /secret-slug/mcp flow=stateless [tools/list(id=1)]";
        assert_eq!(
            mask_mcp_path_in_log(message, false),
            "POST /▓▓▓▓▓▓▓▓/mcp flow=stateless [tools/list(id=1)]"
        );
        assert_eq!(mask_mcp_path_in_log(message, true), message);

        let directional = "→ POST /secret-slug/mcp tools/list id=1";
        assert_eq!(
            mask_mcp_path_in_log(directional, false),
            "→ POST /▓▓▓▓▓▓▓▓/mcp tools/list id=1"
        );
    }

    #[test]
    fn leaves_non_mcp_post_logs_unchanged() {
        let message = "POST /layout/show-detail ok";
        assert_eq!(mask_mcp_path_in_log(message, false), message);
    }

    #[test]
    fn log_view_maps_wrapped_rows_back_to_the_same_log_id() {
        let view = LogView {
            max_scroll: 5,
            effective_scroll: 2,
            area: Rect::new(10, 20, 80, 5),
            visible_log_ids: vec![41, 41, 42],
        };

        assert_eq!(view.log_id_at(11, 21), Some(41));
        assert_eq!(view.log_id_at(11, 22), Some(41));
        assert_eq!(view.log_id_at(11, 23), Some(42));
        assert_eq!(view.log_id_at(11, 20), None);
        assert_eq!(view.log_id_at(10, 21), None);
    }

    #[test]
    fn long_log_messages_wrap_without_losing_text() {
        let lines = wrap_log_message("alpha beta gamma delta", 10);
        assert_eq!(lines, vec!["alpha beta", "gamma", "delta"]);
        assert_eq!(lines.join(" "), "alpha beta gamma delta");

        let hard_wrapped = wrap_log_message("abcdefghijkl", 5);
        assert_eq!(hard_wrapped, vec!["abcde", "fghij", "kl"]);

        let cjk_wrapped = wrap_log_message("中文測試", 6);
        assert_eq!(cjk_wrapped, vec!["中文測", "試"]);
        assert!(
            cjk_wrapped
                .iter()
                .all(|line| terminal_cell_width(line) <= 6)
        );
    }

    #[test]
    fn display_width_helpers_use_terminal_cells_for_cjk_text() {
        assert_eq!(terminal_cell_width("abc"), 3);
        assert_eq!(terminal_cell_width("繁中"), 4);
        assert_eq!(terminal_cell_width(" 已複製！ "), 10);

        let padded = pad_right_to_cell_width("繁中", 6);
        assert_eq!(terminal_cell_width(&padded), 6);
        assert_eq!(padded, "繁中  ");

        let trimmed = trim_line("繁體中文測試", 7);
        assert_eq!(trimmed, "繁體...");
        assert_eq!(terminal_cell_width(&trimmed), 7);
    }

    #[test]
    fn traditional_chinese_runtime_logs_translate_operator_facing_messages() {
        let zh = UiLanguage::TraditionalChinese;
        for (english, expected) in [
            ("Mode: Both", "模式：兩者"),
            ("Theme changed to neon", "主題已切換為 霓虹"),
            ("Tool mode: read-only", "工具模式：唯讀"),
            ("Widget detail mode: Expanded", "Widget 詳細模式：展開"),
            (
                "Set CatDesk as co-author: enabled",
                "將 CatDesk 設為共同作者：已啟用",
            ),
            (
                "Saved ngrok domain to /tmp/config.toml",
                "已儲存 ngrok 網域至 /tmp/config.toml",
            ),
            ("Local browsers: Google Chrome", "本機瀏覽器：Google Chrome"),
            (
                "Using browser: Google Chrome (/Applications/Google Chrome.app) -> launch new browser instance",
                "使用瀏覽器：Google Chrome (/Applications/Google Chrome.app) -> 啟動新的瀏覽器執行個體",
            ),
            (
                "Failed to launch Google Chrome with remote debugging: denied",
                "無法以遠端除錯模式啟動 Google Chrome：denied",
            ),
            ("ngrok tunnel exited", "ngrok 隧道已結束"),
            (
                "← JSON-RPC parse error bytes=12 message=bad",
                "← JSON-RPC 解析錯誤 bytes=12 message=bad",
            ),
        ] {
            assert_eq!(localize_log_message(english, zh), expected, "{english}");
        }

        assert_eq!(
            localize_log_message("Mode: Both", UiLanguage::English),
            "Mode: Both"
        );
    }

    #[tokio::test]
    async fn reserved_mcp_listener_accepts_tcp_before_axum_serve_starts() {
        let listener = super::reserve_mcp_listener(0)
            .await
            .expect("reserve MCP listener");
        let address = listener.local_addr().expect("reserved listener address");
        let connected = tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .expect("TCP connect timed out before axum started")
        .expect("reserved listener refused TCP connect before axum started");
        drop(connected);
        drop(listener);
    }

    #[test]
    fn exported_log_filename_includes_utc_offset() {
        let utc = time::OffsetDateTime::from_unix_timestamp(0).expect("unix epoch");
        assert_eq!(
            format_log_export_filename(utc).expect("format UTC filename"),
            "catdesk-19700101-000000-000Z.log"
        );

        let seoul = utc.to_offset(time::UtcOffset::from_hms(9, 0, 0).expect("UTC+09"));
        assert_eq!(
            format_log_export_filename(seoul).expect("format local filename"),
            "catdesk-19700101-090000-000+0900.log"
        );
    }

    #[test]
    fn exported_logs_are_plain_text_and_mask_secrets() {
        let root = std::env::temp_dir().join(format!(
            "catdesk-log-export-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let logs = vec![super::state::LogEntry {
            id: 1,
            time: "12:34:56".into(),
            level: "INFO",
            message: "MCP Server URL: https://example.ngrok.app/secret/mcp".into(),
        }];

        let path = export_logs_to_dir(&logs, &root).expect("export logs");
        let text = std::fs::read_to_string(&path).expect("read exported logs");
        assert!(text.contains("12:34:56 INFO"));
        assert!(text.contains(super::MCP_URL_MASK));
        assert!(!text.contains("https://example.ngrok.app/secret/mcp"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn connector_refresh_notice_explains_remove_and_readd_flow() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let theme = super::theme::all()[0];

        terminal
            .draw(|frame| {
                draw_chatgpt_connector_refresh_notice(
                    frame,
                    &theme,
                    UiLanguage::English,
                    Some("https://example.ngrok.app/secret/mcp"),
                    None,
                )
            })
            .expect("draw connector refresh notice");

        let buffer = terminal.backend().buffer();
        let text = (0..24)
            .map(|row| {
                (0..100)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("CatDesk Connector Refresh Required"));
        assert!(text.contains("The connector changed in this update. Please folow below step:"));
        assert!(text.contains("https://chatgpt.com/#settings/Plugins"));
        assert!(text.contains("2. Find CatDesk and click it"));
        assert!(text.contains("3. Click the ... button on upper right corner"));
        assert!(text.contains("4. Click delete"));
        assert!(text.contains("5. Open connector settings:"));
        assert!(text.contains("6. Click Create app"));
        assert!(text.contains("7. Fill in the form:"));
        assert!(text.contains("8. Click I understand and want to continue"));
        assert!(text.contains("9. Click Create"));
        assert!(text.contains(super::MCP_URL_MASK));
        assert!(text.contains("Click to reveal"));
        assert!(!text.contains("https://example.ngrok.app/secret/mcp"));
        assert!(!text.contains("[c]"));
        assert!(text.contains("I've re-added CatDesk"));
        assert!(text.contains("Remind me next launch"));
    }

    #[test]
    fn connector_refresh_notice_renders_traditional_chinese() {
        let mut terminal = Terminal::new(TestBackend::new(120, 28)).expect("create terminal");
        let theme = super::theme::all()[0];

        terminal
            .draw(|frame| {
                draw_chatgpt_connector_refresh_notice(
                    frame,
                    &theme,
                    UiLanguage::TraditionalChinese,
                    Some("https://example.ngrok.app/secret/mcp"),
                    None,
                )
            })
            .expect("draw chinese connector refresh notice");

        let text = terminal_buffer_text(&terminal).replace(' ', "");
        for expected in [
            "需要重新整理CatDeskConnector",
            "此更新變更了Connector",
            "移除CatDesk",
            "找到CatDesk並點擊它",
            "重新加入CatDesk",
            "開啟Connector設定",
            "填寫表單",
            "名稱│CatDesk",
            "MCP伺服器URL",
            "點擊顯示",
            "複製設定連結",
            "我已重新加入CatDesk",
            "下次啟動再提醒我",
        ] {
            assert!(
                text.contains(expected),
                "missing translated text: {expected}"
            );
        }
    }

    #[test]
    fn connector_refresh_notice_reveals_mcp_url_with_same_security_ui() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let theme = super::theme::all()[0];
        let url = "https://example.ngrok.app/secret/mcp";

        terminal
            .draw(|frame| {
                draw_chatgpt_connector_refresh_notice(
                    frame,
                    &theme,
                    UiLanguage::English,
                    Some(url),
                    Some(std::time::Duration::from_secs(10)),
                )
            })
            .expect("draw revealed connector refresh notice");

        let buffer = terminal.backend().buffer();
        let text = (0..24)
            .map(|row| {
                (0..100)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(url));
        assert!(text.contains("[ EXPOSED 10s ]"));
        assert!(!text.contains("Click to reveal"));
    }

    #[test]
    fn bootstrap_phase_lines_follow_widget_detail_mode() {
        let palette = super::theme::all()[0].palette;
        let status_style = ratatui::style::Style::default();

        let disabled = super::flow_phase_lines(
            None,
            super::ShowDetailMode::Disable,
            &palette,
            status_style,
            UiLanguage::English,
            0,
        );
        let expanded = super::flow_phase_lines(
            None,
            super::ShowDetailMode::Expanded,
            &palette,
            status_style,
            UiLanguage::English,
            0,
        );
        let collapsed = super::flow_phase_lines(
            None,
            super::ShowDetailMode::Collapsed,
            &palette,
            status_style,
            UiLanguage::English,
            0,
        );

        assert_eq!(disabled.len(), 1);
        assert_eq!(expanded.len(), 2);
        assert_eq!(collapsed.len(), 2);
    }

    #[test]
    fn bootstrap_phase_lines_render_traditional_chinese() {
        let palette = super::theme::all()[0].palette;
        let lines = super::flow_phase_lines(
            None,
            super::ShowDetailMode::Expanded,
            &palette,
            ratatui::style::Style::default(),
            UiLanguage::TraditionalChinese,
            0,
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .replace(' ', "");
        assert!(text.contains("階段1連線中"));
        assert!(text.contains("階段2載入Widget"));
    }

    #[test]
    fn tui_header_places_package_version_at_top_right() {
        let backend = TestBackend::new(60, 3);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let palette = super::theme::all()[0].palette;

        terminal
            .draw(|frame| draw_tui_header(frame, frame.area(), &palette, "CatDesk"))
            .expect("draw header");

        let buffer = terminal.backend().buffer();
        let row = (0..60)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        let version = build_info::version_label(build_info::VERSION, build_info::GIT_SHA);

        assert!(row.contains("CatDesk"));
        assert!(row.ends_with(&format!("{version} │")));
    }
