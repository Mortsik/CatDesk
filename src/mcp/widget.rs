use serde_json::{Map, Value, json};

use crate::change_tracking::FileChange;
use crate::state::ShowDetailMode;

use crate::mcp::commands::format_command_output_events;
use crate::mcp::jsonrpc::{JsonRpcRequest, tool_name_from_request};
use crate::mcp::resources::{
    WIDGET_PAYLOAD_META_KEY, current_widget_resource_uri, current_widget_resource_uri_for_tool,
};
use crate::mcp::token_usage::TokenUsage;
#[cfg(test)]
use crate::mcp::current_show_detail_mode;
use crate::mcp::{AutoWidgetContext, current_token_stats_layout, current_widget_corner_style};

const MAX_WIDGET_COMMAND_OUTPUT_CHARS: usize = 4_000;
const MAX_WIDGET_DIFF_CHARS_PER_FILE: usize = 1_500;
const MAX_WIDGET_LIST_ENTRIES: usize = 100;

pub(crate) fn ensure_output_template_meta(meta_value: &mut Value) {
    let resource_uri = current_widget_resource_uri();
    ensure_output_template_meta_with_uri(meta_value, &resource_uri);
}

fn ensure_output_template_meta_with_uri(meta_value: &mut Value, resource_uri: &str) {
    if !meta_value.is_object() {
        *meta_value = json!({});
    }
    let Some(meta_obj) = meta_value.as_object_mut() else {
        return;
    };
    meta_obj.insert(
        "openai/outputTemplate".to_string(),
        Value::String(resource_uri.to_string()),
    );
    let ui_entry = meta_obj
        .entry("ui".to_string())
        .or_insert_with(|| json!({}));
    if !ui_entry.is_object() {
        *ui_entry = json!({});
    }
    if let Some(ui_obj) = ui_entry.as_object_mut() {
        ui_obj.insert(
            "resourceUri".to_string(),
            Value::String(resource_uri.to_string()),
        );
    }
}

pub(crate) fn attach_widget_payload_meta(result: &mut Value, payload: Value) {
    let Some(obj) = result.as_object_mut() else {
        return;
    };
    let meta_value = obj.entry("_meta".to_string()).or_insert_with(|| json!({}));
    if !meta_value.is_object() {
        *meta_value = json!({});
    }
    let Some(meta_obj) = meta_value.as_object_mut() else {
        return;
    };
    meta_obj.insert(WIDGET_PAYLOAD_META_KEY.to_string(), payload);
}

pub(crate) fn widget_payload_meta_mut(result: &mut Value) -> Option<&mut Map<String, Value>> {
    result
        .as_object_mut()?
        .get_mut("_meta")?
        .as_object_mut()?
        .get_mut(WIDGET_PAYLOAD_META_KEY)?
        .as_object_mut()
}

pub(crate) fn attach_turn_token_usage(result: &mut Value, usage: &TokenUsage) {
    if let Some(widget_payload) = widget_payload_meta_mut(result) {
        widget_payload.insert(
            "turnTokenUsage".to_string(),
            json!({
                "inputTokens": usage.tool_input_tokens,
                "outputTokens": usage.tool_output_tokens,
                "totalTokens": usage.total_tokens,
                // Direction markers: input = tool-call arguments the client sent
                // (request), output = tool results returned to the client (response).
                "inputRole": "request",
                "outputRole": "response",
            }),
        );
    }
}

pub(crate) fn attach_tool_call_count(result: &mut Value, tool_call_count: u64) {
    if let Some(widget_payload) = widget_payload_meta_mut(result) {
        widget_payload.insert("toolCallCount".to_string(), json!(tool_call_count));
    }
}

pub(crate) fn tool_descriptor_should_attach_widget(name: &str) -> bool {
    matches!(
        name,
        "run_command"
            | "start_command"
            | "poll_command"
            | "cancel_command"
            | "catdesk_instruction"
            | "search"
            | "read"
            | "read_image"
            | "write"
            | "edit"
            | "create_handoff"
            | "delete"
    )
}

pub(crate) fn ensure_tool_descriptor_widget_template_with_show_detail_mode(
    tool: &mut Value,
    show_detail_mode: ShowDetailMode,
) {
    if show_detail_mode == ShowDetailMode::Disable {
        return;
    }

    let Some(tool_obj) = tool.as_object_mut() else {
        return;
    };
    let Some(name) = tool_obj.get("name").and_then(Value::as_str) else {
        return;
    };
    let name = name.to_string();
    if !tool_descriptor_should_attach_widget(&name) {
        return;
    }
    let resource_uri = current_widget_resource_uri_for_tool(&name);
    let meta_value = tool_obj
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    ensure_output_template_meta_with_uri(meta_value, &resource_uri);
}

fn extract_tool_result_text(result: &Value) -> String {
    let content_text = extract_tool_result_content_text(result);
    if !content_text.is_empty() {
        return content_text;
    }

    extract_tool_result_structured_text(result)
}

fn extract_tool_result_content_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn extract_tool_result_structured_text(result: &Value) -> String {
    let Some(structured) = result.get("structuredContent").and_then(Value::as_object) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for key in [
        "message",
        "text",
        "instructionText",
        "stdout",
        "stderr",
        "value",
    ] {
        if let Some(text) = structured.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

pub(crate) fn remove_text_content_from_tool_result(req: &JsonRpcRequest, result: &mut Value) {
    let content_text = extract_tool_result_content_text(result);
    let Some(result_obj) = result.as_object_mut() else {
        return;
    };

    if !content_text.is_empty() && !result_obj.contains_key("structuredContent") {
        result_obj.insert(
            "structuredContent".to_string(),
            json!({
                "toolName": tool_name_from_request(req),
                "text": content_text,
            }),
        );
    }

    let Some(content) = result_obj.get_mut("content").and_then(Value::as_array_mut) else {
        result_obj.insert("content".to_string(), Value::Array(Vec::new()));
        return;
    };
    content.retain(|entry| {
        entry.get("type").and_then(Value::as_str) != Some("text") && entry.get("text").is_none()
    });
}

fn truncate_for_widget(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return "...".to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out = String::with_capacity(max_chars);
    out.extend(text.chars().take(keep));
    out.push_str("...");
    out
}

fn summarize_tool_detail(raw_text: &str, is_error: bool) -> String {
    let first_line = raw_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(if is_error {
            "Tool returned an error."
        } else {
            "Tool call completed."
        });
    truncate_for_widget(first_line, 220)
}

fn file_entry_json(file: &FileChange) -> Value {
    json!({
        "path": file.path,
        "status": file.status,
        "added": file.added,
        "removed": file.removed,
        "diff": truncate_for_widget(&file.diff, MAX_WIDGET_DIFF_CHARS_PER_FILE),
    })
}

fn widget_state(is_error: bool, widget_context: Option<&AutoWidgetContext>) -> &'static str {
    if let Some(ctx) = widget_context {
        if ctx.is_error {
            return "failed";
        }
        if ctx.turn_files.is_empty() {
            return "done";
        }
        return "changed";
    }
    if is_error { "failed" } else { "done" }
}

fn widget_changed_files(widget_context: Option<&AutoWidgetContext>) -> (Vec<Value>, bool) {
    let Some(ctx) = widget_context else {
        return (Vec::new(), false);
    };
    let changed_files = ctx
        .turn_files
        .iter()
        .map(file_entry_json)
        .collect::<Vec<_>>();
    let has_changes = !changed_files.is_empty();
    (changed_files, has_changes)
}

pub(crate) fn base_widget_payload(
    panel_mode: &str,
    title: &str,
    state: &str,
    tool_name: Option<&str>,
) -> Map<String, Value> {
    let mut payload = Map::new();
    let token_stats_layout = current_token_stats_layout();
    payload.insert("schema".to_string(), json!("catdesk.review.v1"));
    payload.insert("panelMode".to_string(), json!(panel_mode));
    payload.insert("title".to_string(), json!(title));
    payload.insert("state".to_string(), json!(state));
    payload.insert(
        "tokenStatsLayout".to_string(),
        json!(token_stats_layout.as_str()),
    );
    payload.insert(
        "widgetCornerStyle".to_string(),
        json!(current_widget_corner_style().as_str()),
    );
    if let Some(tool_name) = tool_name {
        payload.insert("toolName".to_string(), json!(tool_name));
    }
    payload
}

#[cfg(test)]
pub(crate) fn base_widget_payload_with_show_detail_mode(
    panel_mode: &str,
    title: &str,
    state: &str,
    tool_name: Option<&str>,
    show_detail_mode: ShowDetailMode,
) -> Map<String, Value> {
    let mut payload = base_widget_payload(panel_mode, title, state, tool_name);
    payload.insert(
        "showDetailMode".to_string(),
        json!(show_detail_mode.as_str()),
    );
    payload
}


fn attach_widget_changed_files(
    payload: &mut Map<String, Value>,
    widget_context: Option<&AutoWidgetContext>,
) {
    let (changed_files, has_changes) = widget_changed_files(widget_context);
    payload.insert("changedFiles".to_string(), Value::Array(changed_files));
    payload.insert("hasChanges".to_string(), Value::Bool(has_changes));
}

fn result_structured_content(result: &Value) -> Option<&Map<String, Value>> {
    result.get("structuredContent").and_then(Value::as_object)
}

pub(crate) fn build_list_files_widget_payload_from_structured(
    structured: &Map<String, Value>,
    title: &str,
    state: &str,
) -> Option<Value> {
    let mut payload = base_widget_payload("tool_call", title, state, Some("list_files"));
    payload.insert("listPath".to_string(), structured.get("listPath")?.clone());
    payload.insert(
        "listItemCount".to_string(),
        structured.get("listItemCount")?.clone(),
    );
    payload.insert(
        "listDirectoryCount".to_string(),
        structured.get("listDirectoryCount")?.clone(),
    );
    payload.insert(
        "listFileCount".to_string(),
        structured.get("listFileCount")?.clone(),
    );
    payload.insert(
        "listOtherCount".to_string(),
        structured.get("listOtherCount")?.clone(),
    );
    let list_entries = structured.get("listEntries")?.as_array()?;
    let widget_preview_truncated = list_entries.len() > MAX_WIDGET_LIST_ENTRIES;
    let source_truncated = structured.get("listTruncated")?.as_bool()?;
    payload.insert(
        "listTruncated".to_string(),
        json!(source_truncated || widget_preview_truncated),
    );
    let source_limit = structured.get("listLimit")?.as_u64()?;
    payload.insert(
        "listLimit".to_string(),
        json!(if widget_preview_truncated {
            MAX_WIDGET_LIST_ENTRIES as u64
        } else {
            source_limit
        }),
    );
    payload.insert(
        "listEntries".to_string(),
        Value::Array(
            list_entries
                .iter()
                .take(MAX_WIDGET_LIST_ENTRIES)
                .cloned()
                .collect(),
        ),
    );
    payload.insert("changedFiles".to_string(), json!([]));
    payload.insert("hasChanges".to_string(), json!(false));
    Some(Value::Object(payload))
}

fn build_search_text_widget_payload(result: &Value, is_error: bool) -> Option<Value> {
    let structured = result_structured_content(result)?;
    let mut payload = base_widget_payload(
        "tool_call",
        "Search",
        widget_state(is_error, None),
        Some("search"),
    );
    payload.insert(
        "searchPattern".to_string(),
        structured.get("searchPattern")?.clone(),
    );
    payload.insert(
        "searchPath".to_string(),
        structured.get("searchPath")?.clone(),
    );
    payload.insert(
        "searchBackend".to_string(),
        structured.get("searchBackend")?.clone(),
    );
    payload.insert(
        "matchCount".to_string(),
        structured.get("matchCount")?.clone(),
    );
    payload.insert(
        "searchTruncated".to_string(),
        structured.get("searchTruncated")?.clone(),
    );
    payload.insert("changedFiles".to_string(), json!([]));
    payload.insert("hasChanges".to_string(), json!(false));
    Some(Value::Object(payload))
}

fn build_read_files_widget_payload(result: &Value, is_error: bool) -> Option<Value> {
    let structured = result_structured_content(result)?;
    let mut payload = base_widget_payload(
        "tool_call",
        "Read Files",
        widget_state(is_error, None),
        Some("read"),
    );
    payload.insert("path".to_string(), structured.get("path")?.clone());
    // Failures get their own row below; do not count them twice.
    payload.insert(
        "renderedFileCount".to_string(),
        json!(
            structured
                .get("files")
                .and_then(Value::as_array)?
                .iter()
                .filter(|file| file.get("error").is_none())
                .count()
        ),
    );
    payload.insert("bytes".to_string(), structured.get("bytes")?.clone());
    payload.insert(
        "lineCount".to_string(),
        structured.get("lineCount")?.clone(),
    );
    // Only the failures: the full entries carry file contents.
    payload.insert("failedFiles".to_string(), failed_read_files(structured));
    payload.insert("changedFiles".to_string(), json!([]));
    payload.insert("hasChanges".to_string(), json!(false));
    Some(Value::Object(payload))
}

fn failed_read_files(structured: &Map<String, Value>) -> Value {
    let failures = structured
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    let error = file.get("error").and_then(Value::as_str)?;
                    let path = file.get("path").and_then(Value::as_str)?;
                    // The name is already shown; the tail pushes the row out of view.
                    let reason = error.split_once(": ").map_or(error, |(head, _)| head);
                    Some(json!({ "path": path, "error": reason }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(failures)
}

fn build_file_change_widget_payload(
    result: &Value,
    widget_context: Option<&AutoWidgetContext>,
    is_error: bool,
    tool_name: &str,
    title: &str,
) -> Option<Value> {
    let structured = result_structured_content(result)?;
    let mut payload = base_widget_payload(
        "tool_call",
        title,
        widget_state(is_error, widget_context),
        Some(tool_name),
    );
    payload.insert("path".to_string(), structured.get("path")?.clone());
    if let Some(bytes_written) = structured.get("bytesWritten") {
        payload.insert("bytesWritten".to_string(), bytes_written.clone());
    }
    for field in ["operationCount", "appliedOperations", "replacedOccurrences"] {
        if let Some(value) = structured.get(field) {
            payload.insert(field.to_string(), value.clone());
        }
    }
    attach_widget_changed_files(&mut payload, widget_context);
    Some(Value::Object(payload))
}

fn build_handoff_widget_payload(result: &Value, is_error: bool) -> Option<Value> {
    let structured = result_structured_content(result)?;
    let mut payload = base_widget_payload(
        "tool_call",
        "Session Handoff",
        widget_state(is_error, None),
        Some("create_handoff"),
    );
    payload.insert("filename".to_string(), structured.get("filename")?.clone());
    payload.insert(
        "searchPrefix".to_string(),
        structured.get("searchPrefix")?.clone(),
    );
    payload.insert("bytes".to_string(), structured.get("bytes")?.clone());
    payload.insert("changedFiles".to_string(), json!([]));
    payload.insert("hasChanges".to_string(), json!(false));
    Some(Value::Object(payload))
}

fn build_run_command_widget_payload(
    result: &Value,
    widget_context: Option<&AutoWidgetContext>,
    is_error: bool,
) -> Option<Value> {
    let structured = result_structured_content(result)?;
    if structured
        .get("interceptedToolName")
        .and_then(Value::as_str)
        == Some("list_files")
        && structured
            .get("interceptedCommandName")
            .and_then(Value::as_str)
            != Some("ls")
    {
        return build_list_files_widget_payload_from_structured(
            structured,
            "List Files",
            widget_state(is_error, widget_context),
        );
    }
    let mut payload = base_widget_payload(
        "tool_call",
        "Command Output",
        widget_state(is_error, widget_context),
        Some("run_command"),
    );
    payload.insert("command".to_string(), structured.get("command")?.clone());
    payload.insert(
        "output".to_string(),
        json!(truncate_for_widget(
            &extract_tool_result_text(result),
            MAX_WIDGET_COMMAND_OUTPUT_CHARS,
        )),
    );
    if let Some(elapsed) = structured.get("elapsedMs") {
        payload.insert("elapsedMs".to_string(), elapsed.clone());
    }
    attach_widget_changed_files(&mut payload, widget_context);
    Some(Value::Object(payload))
}

pub(crate) fn build_command_job_widget_payload(
    result: &Value,
    tool_name: &str,
    widget_context: Option<&AutoWidgetContext>,
) -> Option<Value> {
    let structured = result_structured_content(result)?;
    let command = structured.get("command")?.clone();
    let state = structured.get("state")?.as_str()?;
    let (title, widget_state) = match state {
        "queued" => ("Command Queued", "waiting"),
        "running" => (
            if tool_name == "start_command" {
                "Command Started"
            } else {
                "Command Running"
            },
            "waiting",
        ),
        "succeeded" => ("Command Complete", "done"),
        "cancelled" => ("Command Cancelled", "done"),
        "failed" => ("Command Failed", "failed"),
        "timed_out" => ("Command Timed Out", "failed"),
        "interrupted" => ("Command Interrupted", "failed"),
        "abandoned" => ("Command Abandoned", "failed"),
        _ => ("Command Job", "waiting"),
    };
    let mut output = structured
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            format_command_output_events(events.iter().map(|event| {
                (
                    event
                        .get("stream")
                        .and_then(Value::as_str)
                        .unwrap_or("stdout"),
                    event
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            }))
        })
        .unwrap_or_default();
    if output.is_empty() {
        output = format!(
            "job {} · {}",
            structured
                .get("jobId")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            state
        );
    }
    if structured.get("outputTruncated").and_then(Value::as_bool) == Some(true) {
        output.push_str("\n[older command output was truncated]\n");
    }
    if structured.get("hasMoreOutput").and_then(Value::as_bool) == Some(true) {
        output.push_str("\n[more buffered output available; poll again]\n");
    }
    let mut payload = base_widget_payload("tool_call", title, widget_state, Some(tool_name));
    payload.insert("command".to_string(), command);
    payload.insert(
        "output".to_string(),
        json!(truncate_for_widget(
            &output,
            MAX_WIDGET_COMMAND_OUTPUT_CHARS
        )),
    );
    if let Some(elapsed) = structured.get("elapsedMs") {
        payload.insert("elapsedMs".to_string(), elapsed.clone());
    }
    attach_widget_changed_files(&mut payload, widget_context);
    Some(Value::Object(payload))
}

fn build_generic_widget_payload(
    req: &JsonRpcRequest,
    result: &Value,
    widget_context: Option<&AutoWidgetContext>,
    is_error: bool,
) -> Value {
    let tool_name = tool_name_from_request(req);
    let mut payload = base_widget_payload(
        "tool_call",
        "Changed Files",
        widget_state(is_error, widget_context),
        Some(&tool_name),
    );
    if widget_context.is_some() {
        attach_widget_changed_files(&mut payload, widget_context);
    } else {
        payload.insert("call".to_string(), json!(format!("call {}", tool_name)));
        payload.insert(
            "detail".to_string(),
            json!(summarize_tool_detail(
                &extract_tool_result_text(result),
                is_error
            )),
        );
        payload.insert("changedFiles".to_string(), json!([]));
        payload.insert("hasChanges".to_string(), json!(false));
    }
    Value::Object(payload)
}

fn build_widget_payload_error(
    req: &JsonRpcRequest,
    widget_context: Option<&AutoWidgetContext>,
    message: String,
) -> Value {
    let tool_name = tool_name_from_request(req);
    let mut payload = base_widget_payload(
        "tool_call",
        "Widget Payload Error",
        "failed",
        Some(&tool_name),
    );
    payload.insert("payloadKind".to_string(), json!("widget_payload_error"));
    payload.insert("call".to_string(), json!(format!("call {}", tool_name)));
    payload.insert("detail".to_string(), json!(message));
    attach_widget_changed_files(&mut payload, widget_context);
    Value::Object(payload)
}

fn build_auto_widget_payload(
    req: &JsonRpcRequest,
    result: &Value,
    widget_context: Option<&AutoWidgetContext>,
) -> Value {
    let tool_name = tool_name_from_request(req);
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match tool_name.as_str() {
        "search" => match build_search_text_widget_payload(result, is_error) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build search widget payload from structuredContent.".into(),
            ),
        },
        "read" => match build_read_files_widget_payload(result, is_error) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build read widget payload from structuredContent.".into(),
            ),
        },
        "write" => match build_file_change_widget_payload(
            result,
            widget_context,
            is_error,
            "write",
            "Write File",
        ) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build write widget payload from structuredContent.".into(),
            ),
        },
        "edit" => match build_file_change_widget_payload(
            result,
            widget_context,
            is_error,
            "edit",
            "Edit File",
        ) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build edit widget payload from structuredContent.".into(),
            ),
        },
        "create_handoff" => match build_handoff_widget_payload(result, is_error) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build create_handoff widget payload from structuredContent.".into(),
            ),
        },
        "delete" => match build_file_change_widget_payload(
            result,
            widget_context,
            is_error,
            "delete",
            "Delete Path",
        ) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build delete widget payload from structuredContent.".into(),
            ),
        },
        "run_command" => match build_run_command_widget_payload(result, widget_context, is_error) {
            Some(payload) => payload,
            None if is_error => build_generic_widget_payload(req, result, widget_context, is_error),
            None => build_widget_payload_error(
                req,
                widget_context,
                "Failed to build run_command widget payload from structuredContent.".into(),
            ),
        },
        "start_command" | "poll_command" | "cancel_command" => {
            match build_command_job_widget_payload(result, &tool_name, widget_context) {
                Some(payload) => payload,
                None if is_error => {
                    build_generic_widget_payload(req, result, widget_context, is_error)
                }
                None => build_widget_payload_error(
                    req,
                    widget_context,
                    format!("Failed to build {tool_name} widget payload from structuredContent."),
                ),
            }
        }
        _ => build_generic_widget_payload(req, result, widget_context, is_error),
    }
}

pub(crate) fn enrich_tool_result_with_show_detail_mode(
    req: &JsonRpcRequest,
    mut result: Value,
    widget_context: Option<&AutoWidgetContext>,
    show_detail_mode: ShowDetailMode,
) -> Value {
    if show_detail_mode == ShowDetailMode::Disable {
        return result;
    }

    if !result.is_object() {
        let value = result;
        result = json!({
            "content": [],
            "structuredContent": {
                "toolName": tool_name_from_request(req),
                "value": value
            }
        });
    }
    let has_widget_payload = result
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .is_some();
    let widget_payload = if has_widget_payload {
        None
    } else {
        let mut payload = build_auto_widget_payload(req, &result, widget_context);
        if let Some(payload_obj) = payload.as_object_mut() {
            payload_obj.insert(
                "showDetailMode".to_string(),
                json!(show_detail_mode.as_str()),
            );
        }
        Some(payload)
    };
    if let Some(result_obj) = result.as_object_mut() {
        let meta_value = result_obj
            .entry("_meta".to_string())
            .or_insert_with(|| json!({}));
        ensure_output_template_meta(meta_value);
    }
    if let Some(widget_payload) = widget_payload {
        attach_widget_payload_meta(&mut result, widget_payload);
    }
    if let Some(widget_payload) = widget_payload_meta_mut(&mut result) {
        widget_payload.insert(
            "showDetailMode".to_string(),
            json!(show_detail_mode.as_str()),
        );
    }
    remove_text_content_from_tool_result(req, &mut result);
    result
}

#[cfg(test)]
pub(crate) fn enrich_tool_result(
    req: &JsonRpcRequest,
    result: Value,
    widget_context: Option<&AutoWidgetContext>,
) -> Value {
    enrich_tool_result_with_show_detail_mode(
        req,
        result,
        widget_context,
        current_show_detail_mode(),
    )
}
