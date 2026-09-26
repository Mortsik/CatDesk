pub(crate) mod browser_select;
pub(crate) mod chrome;
pub(crate) mod clipboard;
pub(crate) mod connector_notice;
pub(crate) mod dashboard;
pub(crate) mod flow;
pub(crate) mod logs;
pub(crate) mod ngrok_setup;
pub(crate) mod settings;
pub(crate) mod text;

// Wiring hub: only items the crate root (run_app/run_tui) still calls.
pub(crate) use browser_select::{
    find_available_remote_debug_port, mode_is_browser_enabled, run_browser_select,
    sanitize_for_filename,
};
pub(crate) use chrome::{centered_rect, draw_mode_select};
pub(crate) use clipboard::clipboard_copy;
pub(crate) use connector_notice::run_chatgpt_connector_refresh_notice;
pub(crate) use dashboard::draw_ui;
pub(crate) use flow::build_animation_snapshot;
pub(crate) use logs::{
    LogView, Selection, export_logs, extract_from_screen, is_secret_log_message,
    post_mcp_path, secret_log_copy_value,
};
pub(crate) use ngrok_setup::{run_ngrok_auth_setup, run_ngrok_domain_setup};
pub(crate) use settings::run_settings;
