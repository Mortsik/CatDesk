use ratatui::layout::Rect;
use std::io::Write as _;
use unicode_width::UnicodeWidthChar;

use crate::state::{LogEntry, UiLanguage, local_now, user_home_dir};

pub(crate) const MCP_URL_MASK: &str = "https://▓▓▓▓▓▓▓▓/▓▓▓▓▓▓▓▓/mcp";
pub(crate) const MCP_PATH_MASK: &str = "/▓▓▓▓▓▓▓▓/mcp";
pub(crate) const NGROK_URL_MASK: &str = "https://▓▓▓▓▓▓▓▓";
pub(crate) const NGROK_DOMAIN_MASK: &str = "▓▓▓▓▓▓▓▓";

// ── Selection ───────────────────────────────────────────────

pub(crate) struct Selection {
    pub(crate) start: Option<(u16, u16)>,
    pub(crate) end: Option<(u16, u16)>,
    pub(crate) dragging: bool,
}

#[derive(Clone)]
pub(crate) struct LogView {
    pub(crate) max_scroll: usize,
    pub(crate) effective_scroll: usize,
    pub(crate) area: Rect,
    pub(crate) visible_log_ids: Vec<u64>,
}

impl LogView {
    pub(crate) fn log_id_at(&self, column: u16, row: u16) -> Option<u64> {
        let inner_left = self.area.x.saturating_add(1);
        let inner_right = self
            .area
            .x
            .saturating_add(self.area.width.saturating_sub(1));
        let inner_top = self.area.y.saturating_add(1);
        let inner_bottom = self
            .area
            .y
            .saturating_add(self.area.height.saturating_sub(1));
        if column < inner_left || column >= inner_right || row < inner_top || row >= inner_bottom {
            return None;
        }
        self.visible_log_ids
            .get(row.saturating_sub(inner_top) as usize)
            .copied()
    }
}

impl Selection {
    pub(crate) fn new() -> Self {
        Self {
            start: None,
            end: None,
            dragging: false,
        }
    }
    pub(crate) fn clear(&mut self) {
        self.start = None;
        self.end = None;
        self.dragging = false;
    }
    pub(crate) fn range(&self) -> Option<((u16, u16), (u16, u16))> {
        match (self.start, self.end) {
            (Some(s), Some(e)) => {
                let (r0, c0, r1, c1) = if (s.1, s.0) <= (e.1, e.0) {
                    (s.1, s.0, e.1, e.0)
                } else {
                    (e.1, e.0, s.1, s.0)
                };
                Some(((c0, r0), (c1, r1)))
            }
            _ => None,
        }
    }
}

pub(crate) fn extract_from_screen(lines: &[String], start: (u16, u16), end: (u16, u16)) -> String {
    let (c0, r0) = start;
    let (c1, r1) = end;
    let mut result = String::new();
    for row in r0..=r1 {
        let idx = row as usize;
        if idx >= lines.len() {
            break;
        }
        let line: Vec<char> = lines[idx].chars().collect();
        let cs = if row == r0 { c0 as usize } else { 0 };
        let ce = if row == r1 {
            (c1 as usize).min(line.len().saturating_sub(1))
        } else {
            line.len().saturating_sub(1)
        };
        for col in cs..=ce {
            if col < line.len() {
                result.push(line[col]);
            }
        }
        if row != r1 {
            result.push('\n');
        }
    }
    result
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn post_mcp_path(message: &str) -> Option<&str> {
    let rest = message
        .strip_prefix("POST ")
        .or_else(|| message.strip_prefix("→ POST "))
        .or_else(|| message.strip_prefix("← POST "))?;
    let (path, _) = rest.split_once(' ')?;
    let mut parts = path.split('/');
    let is_mcp_path = parts.next() == Some("")
        && parts.next().is_some_and(|slug| !slug.is_empty())
        && parts.next() == Some("mcp")
        && parts.next().is_none();
    is_mcp_path.then_some(path)
}

pub(crate) fn mask_mcp_path_in_log(message: &str, revealed: bool) -> String {
    if revealed {
        return message.to_string();
    }
    let Some(path) = post_mcp_path(message) else {
        return message.to_string();
    };
    message.replacen(path, MCP_PATH_MASK, 1)
}

pub(crate) fn is_secret_log_message(message: &str) -> bool {
    message.starts_with("MCP Server URL: ")
        || message.starts_with("ngrok URL: ")
        || message.starts_with("Auto-saved ngrok static domain: ")
        || post_mcp_path(message).is_some()
}

pub(crate) fn mask_secret_log_message(message: &str, revealed: bool) -> String {
    if revealed {
        return message.to_string();
    }
    if message.starts_with("MCP Server URL: ") {
        return format!("MCP Server URL: {MCP_URL_MASK}");
    }
    if message.starts_with("ngrok URL: ") {
        return format!("ngrok URL: {NGROK_URL_MASK}");
    }
    if message.starts_with("Auto-saved ngrok static domain: ") {
        return format!("Auto-saved ngrok static domain: {NGROK_DOMAIN_MASK}");
    }
    mask_mcp_path_in_log(message, false)
}

fn localize_runtime_value(value: &str) -> String {
    match value {
        "Computer" => "電腦".into(),
        "Browser" => "瀏覽器".into(),
        "Both" => "兩者".into(),
        "multi-tools" => "多工具".into(),
        "read-only" => "唯讀".into(),
        "Disable" => "停用".into(),
        "Expanded" => "展開".into(),
        "Collapsed" => "收合".into(),
        "enabled" => "已啟用".into(),
        "disabled" => "已停用".into(),
        "not active" => "未啟用".into(),
        "concise" => "簡潔".into(),
        "neon" => "霓虹".into(),
        _ => value
            .replace("launch new browser instance", "啟動新的瀏覽器執行個體")
            .replace("Chromium (supported)", "Chromium（支援）")
            .replace(
                "Not supported yet (CDP bridge for Firefox not wired)",
                "尚未支援（Firefox 的 CDP bridge 尚未接上）",
            ),
    }
}

pub(crate) fn localize_log_message(message: &str, ui_language: UiLanguage) -> String {
    if !ui_language.is_traditional_chinese() {
        return message.to_string();
    }

    match message {
        "No local browser found in PATH" => "在 PATH 中找不到本機瀏覽器".into(),
        "No detected browser supports remote debugging" => "偵測到的瀏覽器都不支援遠端除錯".into(),
        "No browser currently runs with remote debugging" => {
            "目前沒有瀏覽器以遠端除錯模式執行".into()
        }
        "No browser was selected before startup" => "啟動前未選取瀏覽器".into(),
        "Browser mode requires selecting a supported Chromium browser" => {
            "瀏覽器模式需要選取受支援的 Chromium 瀏覽器".into()
        }
        "No available local port in range 9222-9322 for remote debugging" => {
            "9222-9322 範圍內沒有可用的本機遠端除錯連接埠".into()
        }
        "Starting chrome-devtools-mcp..." => "正在啟動 chrome-devtools-mcp...".into(),
        "chrome-devtools-mcp started" => "chrome-devtools-mcp 已啟動".into(),
        "ChatGPT connector refresh acknowledged" => "已確認重新整理 ChatGPT Connector".into(),
        "ngrok SDK tunnel started" => "ngrok SDK 隧道已啟動".into(),
        "ngrok tunnel exited" => "ngrok 隧道已結束".into(),
        "Generated new random MCP slug" => "已產生新的隨機 MCP slug".into(),
        "Updated ngrok static domain" => "已更新 ngrok 固定網域".into(),
        "Token billing totals reset" => "已重設 Token 計費總計".into(),
        "DELETE mcp endpoint: stateless reset" => "DELETE mcp endpoint：無狀態重設".into(),
        _ => {
            for (prefix, localized_prefix, localize_value) in [
                (
                    "MCP Server started on port ",
                    "MCP 伺服器已啟動，連接埠 ",
                    false,
                ),
                ("MCP Server URL: ", "MCP 伺服器 URL：", false),
                ("ngrok URL: ", "ngrok URL：", false),
                (
                    "Auto-saved ngrok static domain: ",
                    "已自動儲存 ngrok 固定網域：",
                    false,
                ),
                (
                    "Saved ngrok authtoken to ",
                    "已儲存 ngrok authtoken 至 ",
                    false,
                ),
                ("Saved ngrok domain to ", "已儲存 ngrok 網域至 ", false),
                ("Selected browser: ", "選取的瀏覽器：", false),
                (
                    "Selected browser remote debugging: ",
                    "選取瀏覽器的遠端除錯：",
                    true,
                ),
                ("UI language: ", "介面語言：", true),
                ("Mode: ", "模式：", true),
                ("Theme changed to ", "主題已切換為 ", true),
                ("Tool mode: ", "工具模式：", true),
                ("Widget detail mode: ", "Widget 詳細模式：", true),
                (
                    "Set CatDesk as co-author: ",
                    "將 CatDesk 設為共同作者：",
                    true,
                ),
                ("Local browsers: ", "本機瀏覽器：", false),
                ("Remote debugging supported: ", "支援遠端除錯：", false),
                ("Remote debugging active: ", "已啟用遠端除錯：", false),
                ("Using browser: ", "使用瀏覽器：", true),
                (
                    "Failed to create user data dir ",
                    "無法建立使用者資料目錄 ",
                    false,
                ),
                ("Failed to bind port ", "無法綁定連接埠 ", false),
                ("Exported logs to ", "紀錄已匯出至 ", false),
                ("Failed to export logs: ", "紀錄匯出失敗：", false),
                (
                    "Failed to persist app state: ",
                    "無法儲存應用程式狀態：",
                    false,
                ),
                ("ngrok tunnel failed: ", "ngrok 隧道失敗：", false),
                (
                    "ngrok tunnel join failed: ",
                    "ngrok 隧道結束等待失敗：",
                    false,
                ),
                ("chrome-devtools-mcp: ", "chrome-devtools-mcp：", false),
                ("ngrok: ", "ngrok：", false),
            ] {
                if let Some(rest) = message.strip_prefix(prefix) {
                    let rest = if localize_value {
                        localize_runtime_value(rest)
                    } else {
                        rest.to_string()
                    };
                    return format!("{localized_prefix}{rest}");
                }
            }

            if let Some(rest) = message.strip_prefix("Selected browser ")
                && let Some(browser) =
                    rest.strip_suffix(" is not supported yet for chrome-devtools-mcp")
            {
                return format!("選取的瀏覽器 {browser} 尚未支援 chrome-devtools-mcp");
            }
            if let Some(rest) = message.strip_prefix("Failed to launch ")
                && let Some((browser, error)) = rest.split_once(" with remote debugging: ")
            {
                return format!("無法以遠端除錯模式啟動 {browser}：{error}");
            }
            if let Some(rest) = message.strip_prefix("Launched ")
                && let Some((browser, target)) = rest.split_once(" with remote debugging on ")
            {
                return format!("已以遠端除錯模式啟動 {browser}，位置 {target}");
            }
            if let Some(rest) = message.strip_prefix("Remote debugging ready for ")
                && let Some((browser, target)) = rest.split_once(" at ")
            {
                return format!("{browser} 的遠端除錯已就緒，位置 {target}");
            }
            if let Some(rest) = message.strip_prefix("Remote debugging endpoint for ")
                && let Some(browser) = rest.strip_suffix(" did not become ready in time")
            {
                return format!("{browser} 的遠端除錯端點未能及時就緒");
            }
            if let Some(rest) = message.strip_prefix("Browser: ") {
                return format!(
                    "瀏覽器：{}",
                    localize_runtime_value(rest)
                        .replace(" (binary: ", "（執行檔：")
                        .replace(", path: ", "，路徑：")
                        .replace(", support: ", "，支援：")
                        .replace(", remote debug flag: ", "，遠端除錯參數：")
                        .replace(", remote debug active: ", "，遠端除錯啟用：")
                        .replace(", pid: ", "，PID：")
                );
            }

            message
                .replace("parse error", "解析錯誤")
                .replace("invalid request", "無效請求")
                .replace("invalid-request", "無效請求")
                .replace("validation-error", "驗證錯誤")
                .replace("non-request JSON-RPC", "非請求 JSON-RPC")
                .replace("stateless reset", "無狀態重設")
        }
    }
}

pub(crate) fn secret_log_copy_value(message: &str) -> Option<String> {
    message
        .strip_prefix("MCP Server URL: ")
        .or_else(|| message.strip_prefix("ngrok URL: "))
        .or_else(|| message.strip_prefix("Auto-saved ngrok static domain: "))
        .map(str::to_string)
}

pub(crate) fn wrap_log_message(message: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for logical_line in message.split('\n') {
        if logical_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let chars = logical_line.chars().collect::<Vec<_>>();
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = start;
            let mut used_width = 0usize;
            while end < chars.len() {
                let ch_width = chars[end].width().unwrap_or(0);
                if used_width.saturating_add(ch_width) > width {
                    break;
                }
                used_width = used_width.saturating_add(ch_width);
                end += 1;
            }

            if end == chars.len() {
                wrapped.push(chars[start..].iter().collect());
                break;
            }
            if end == start {
                end += 1;
            }

            let split = if chars.get(end).is_some_and(|ch| ch.is_whitespace()) {
                end
            } else {
                chars[start..end]
                    .iter()
                    .rposition(|ch| ch.is_whitespace())
                    .filter(|offset| *offset > 0)
                    .map(|offset| start + offset)
                    .unwrap_or(end)
            };
            let line = chars[start..split]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string();
            wrapped.push(line);
            start = split;
            while start < chars.len() && chars[start].is_whitespace() {
                start += 1;
            }
        }
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

pub(crate) fn format_log_export_filename(now: time::OffsetDateTime) -> std::io::Result<String> {
    let stamp = now
        .format(time::macros::format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .map_err(std::io::Error::other)?;
    let offset_seconds = now.offset().whole_seconds();
    let offset_suffix = if offset_seconds == 0 {
        "Z".to_string()
    } else {
        let sign = if offset_seconds < 0 { '-' } else { '+' };
        let absolute = offset_seconds.unsigned_abs();
        let hours = absolute / 3600;
        let minutes = (absolute % 3600) / 60;
        format!("{sign}{hours:02}{minutes:02}")
    };
    Ok(format!(
        "catdesk-{stamp}-{:03}{offset_suffix}.log",
        now.millisecond()
    ))
}

pub(crate) fn export_logs_to_dir(
    logs: &[LogEntry],
    directory: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }

    let now = local_now();
    let path = directory.join(format_log_export_filename(now)?);
    let mut file = std::fs::File::create(&path)?;
    for entry in logs {
        let message = mask_secret_log_message(&entry.message, false);
        writeln!(file, "{} {:5} {}", entry.time, entry.level, message)?;
    }
    file.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

pub(crate) fn export_logs(logs: &[LogEntry]) -> std::io::Result<std::path::PathBuf> {
    export_logs_to_dir(logs, &user_home_dir()?.join(".catdesk").join("logs"))
}
