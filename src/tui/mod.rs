mod logs;
mod text;

pub(crate) use logs::{
    LogView, MCP_URL_MASK, Selection, export_logs, export_logs_to_dir, extract_from_screen,
    format_log_export_filename,
    is_secret_log_message, localize_log_message, mask_mcp_path_in_log, mask_secret_log_message,
    post_mcp_path,
    secret_log_copy_value, wrap_log_message,
};
pub(crate) use text::{
    format_average_usage_cost_usd, format_cost_estimate_usd, format_session_duration,
    format_token_compact, format_usd_compact, mcp_url_reveal_bar_segments,
    mcp_url_reveal_seconds, pad_right_to_cell_width, session_cost_rates, terminal_cell_width,
    trim_line,
};
