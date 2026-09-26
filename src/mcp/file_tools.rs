use base64::Engine as _;
use std::path::Path;
use serde_json::{Value, json};

use crate::handoff;
use crate::vision;
use crate::workspace_tools;

use crate::mcp::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, image_tool_success_response, tool_arguments,
    tool_error_response, tool_error_response_with_structured,
    tool_success_response_with_structured,
};
use crate::mcp::instruction::instruction_context_root;

pub(crate) fn parse_read_paths(arguments: &Value) -> Result<Vec<String>, String> {
    let items = arguments
        .get("paths")
        .ok_or_else(|| "Missing required parameter: paths".to_string())?
        .as_array()
        .ok_or_else(|| "Parameter paths must be an array".to_string())?;
    let mut paths = Vec::with_capacity(items.len());
    for item in items {
        let path = item
            .as_str()
            .ok_or_else(|| "Parameter paths must contain only strings".to_string())?;
        if path.is_empty() {
            return Err("Parameter paths must not contain empty strings".into());
        }
        paths.push(path.to_string());
    }
    Ok(paths)
}

pub(crate) fn handle_read_files(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let paths = match parse_read_paths(&tool_arguments(req)) {
        Ok(paths) => paths,
        Err(error) => return tool_error_response(req, error),
    };
    match workspace_tools::read_files(workspace_root, &paths) {
        Ok(output) => {
            let structured = json!({
                "toolName": "read",
                // The batch's byte and line counts are billed to this path, so
                // it has to be a file that contributed them -- not a failed
                // entry, not an empty file.
                "path": output
                    .files
                    .iter()
                    .find(|file| !file.truncated && file.bytes > 0)
                    .or_else(|| output.files.iter().find(|file| file.bytes > 0))
                    .or_else(|| output.files.iter().find(|file| file.error.is_none()))
                    .or_else(|| output.files.first())
                    .map(|file| file.path.clone())
                    .unwrap_or_default(),
                "bytes": output.total_bytes,
                "sizeBytes": output.files.iter().map(|f| f.size_bytes).sum::<u64>(),
                "lineCount": output.total_line_count,
                "fileCount": output.files.len(),
                "batchTruncated": output.batch_truncated,
                "files": output.files,
            });
            // tool_response drops `text` whenever structured content is given.
            if output.files.iter().all(|file| file.error.is_some()) {
                // Per-entry errors are right for a batch, but a batch where
                // nothing was read is a failed call, not a successful empty one.
                tool_error_response_with_structured(req, String::new(), structured)
            } else {
                tool_success_response_with_structured(req, String::new(), structured)
            }
        }
        Err(e) => tool_error_response(req, e),
    }
}

fn parse_optional_image_dimension(arguments: &Value, name: &str) -> Result<Option<u32>, String> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| format!("Parameter {name} must be a positive integer"))?;
    let value = u32::try_from(value).map_err(|_| format!("Parameter {name} is too large"))?;
    Ok(Some(value))
}

pub(crate) async fn handle_read_image(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let path = match arguments.get("path").and_then(Value::as_str) {
        Some(path) if !path.is_empty() => path,
        Some(_) => return tool_error_response(req, "Parameter path must not be empty".into()),
        None => return tool_error_response(req, "Missing required parameter: path".into()),
    };
    let max_width = match parse_optional_image_dimension(&arguments, "max_width") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let max_height = match parse_optional_image_dimension(&arguments, "max_height") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let analyze_prompt = match parse_optional_analysis_prompt(&arguments) {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };

    let output = match workspace_tools::read_image(workspace_root, path, max_width, max_height) {
        Ok(output) => output,
        Err(error) => return tool_error_response(req, error),
    };

    match analyze_prompt {
        None => image_tool_success_response(req, &output.data, &output.mime_type),
        Some(prompt) => {
            let config = match vision::vision_config_from_env() {
                Ok(config) => config,
                Err(error) => return tool_error_response(req, error),
            };
            let image_base64 = base64::engine::general_purpose::STANDARD.encode(&output.data);
            let analysis = match vision::analyze_image(
                &config,
                prompt
                    .as_deref()
                    .unwrap_or(vision::default_analysis_prompt()),
                &image_base64,
                &output.mime_type,
            )
            .await
            {
                Ok(description) => description,
                Err(error) => return tool_error_response(req, error),
            };
            image_tool_analyzed_response(req, &output, &config, analysis)
        }
    }
}

/// `analyze` accepts `false`/absent (image only), `true` (default prompt) or a
/// non-empty custom prompt string. Anything else is a client error.
fn parse_optional_analysis_prompt(arguments: &Value) -> Result<Option<Option<String>>, String> {
    match arguments.get("analyze") {
        None | Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) => Ok(Some(None)),
        Some(Value::String(prompt)) => {
            let prompt = prompt.trim();
            if prompt.is_empty() {
                return Err("Parameter analyze must not be an empty string".to_string());
            }
            Ok(Some(Some(prompt.to_string())))
        }
        Some(_) => Err("Parameter analyze must be a boolean or a string".to_string()),
    }
}

fn image_tool_analyzed_response(
    req: &JsonRpcRequest,
    output: &workspace_tools::ReadImageOutput,
    config: &vision::VisionConfig,
    analysis: String,
) -> JsonRpcResponse {
    // ChatGPT drops image blocks from tool results but does deliver
    // structuredContent text to the model, so the vision description travels
    // in structuredContent while the image stays in content[] for clients
    // with native multimodal support (Claude, Cline, MCP Inspector).
    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "content": [{
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(&output.data),
                "mimeType": output.mime_type,
            }],
            "structuredContent": {
                "toolName": "read_image",
                "path": output.path,
                "mimeType": output.mime_type,
                "sizeBytes": output.size_bytes,
                "width": output.width,
                "height": output.height,
                "originalWidth": output.original_width,
                "originalHeight": output.original_height,
                "resized": output.resized,
                "analysis": {
                    "backend": config.backend.as_str(),
                    "model": config.model,
                    "description": analysis,
                },
                "message": format!("Read image {} (analyzed with {})", output.path, config.model),
                "success": true,
            },
        }),
    )
}

pub(crate) fn handle_write_file(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return tool_error_response(req, "Missing required parameter: path".into()),
    };
    let content = match arguments.get("content").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return tool_error_response(req, "Missing required parameter: content".into()),
    };
    let create_dirs = arguments
        .get("create_dirs")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match workspace_tools::write_file(workspace_root, path, content, create_dirs) {
        Ok(text) => {
            let message = text.clone();
            tool_success_response_with_structured(
                req,
                text,
                json!({
                    "toolName": "write",
                    "path": path,
                    "bytesWritten": content.len(),
                    "createDirs": create_dirs,
                    "message": message,
                }),
            )
        }
        Err(e) => tool_error_response(req, e),
    }
}

pub(crate) fn handle_create_handoff_for_project(
    req: &JsonRpcRequest,
    workspace_root: &str,
    active_project: Option<&Path>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let goal = match required_string_argument(&arguments, "goal") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        Ok(_) => return tool_error_response(req, "Parameter goal must not be empty".into()),
        Err(error) => return tool_error_response(req, error),
    };
    let completed = match optional_string_list_argument(&arguments, "completed") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let decisions = match optional_string_list_argument(&arguments, "decisions") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let validation = match optional_string_list_argument(&arguments, "validation") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let next_steps = match optional_string_list_argument(&arguments, "next_steps") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let notes = match optional_string_argument(&arguments, "notes") {
        Ok(value) => value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        Err(error) => return tool_error_response(req, error),
    };

    let input = handoff::HandoffInput {
        goal,
        completed,
        decisions,
        validation,
        next_steps,
        notes,
    };
    let context_root = instruction_context_root(workspace_root, active_project);
    let context_root_str = context_root.to_string_lossy();
    match handoff::create_handoff(&context_root_str, &input) {
        Ok(output) => {
            let message = format!(
                "Prepared session handoff {} for ChatGPT Library",
                output.filename
            );
            tool_success_response_with_structured(
                req,
                message.clone(),
                json!({
                    "toolName": "create_handoff",
                    "filename": output.filename,
                    "searchPrefix": output.search_prefix,
                    "content": output.content,
                    "bytes": output.bytes,
                    "gitAvailable": output.git.available,
                    "gitStatusAvailable": output.git.status_available,
                    "gitBranch": output.git.branch,
                    "gitStatus": output.git.status,
                    "recentCommits": output.git.recent_commits,
                    "message": message,
                    "success": true,
                }),
            )
        }
        Err(error) => tool_error_response(req, error),
    }
}

fn parse_edit_operations(arguments: &Value) -> Result<Vec<workspace_tools::EditOperation>, String> {
    let edits = arguments
        .get("edits")
        .ok_or_else(|| "Missing required parameter: edits".to_string())?
        .as_array()
        .ok_or_else(|| "Parameter edits must be an array".to_string())?;
    if edits.is_empty() {
        return Err("Parameter edits must contain at least one operation".into());
    }

    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let operation_number = index + 1;
            let edit = edit.as_object().ok_or_else(|| {
                format!("Edit operation {operation_number} must be an object")
            })?;
            let operation_type = edit
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Edit operation {operation_number} is missing string field type"))?;

            match operation_type {
                "replace" => {
                    let old_string = edit
                        .get("old_string")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!("Edit operation {operation_number} is missing string field old_string")
                        })?;
                    let new_string = edit
                        .get("new_string")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!("Edit operation {operation_number} is missing string field new_string")
                        })?;
                    let replace_all = match edit.get("replace_all") {
                        Some(value) => value.as_bool().ok_or_else(|| {
                            format!("Edit operation {operation_number} field replace_all must be a boolean")
                        })?,
                        None => false,
                    };
                    Ok(workspace_tools::EditOperation::Replace {
                        old_string: old_string.to_string(),
                        new_string: new_string.to_string(),
                        replace_all,
                    })
                }
                "range" => {
                    let read_line = |field: &str| -> Result<usize, String> {
                        edit.get(field)
                            .and_then(Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .filter(|value| *value > 0)
                            .ok_or_else(|| {
                                format!(
                                    "Edit operation {operation_number} field {field} must be a positive integer"
                                )
                            })
                    };
                    let start_line = read_line("start_line")?;
                    let end_line = read_line("end_line")?;
                    let old_text = edit
                        .get("old_text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!("Edit operation {operation_number} is missing string field old_text")
                        })?;
                    let new_text = edit
                        .get("new_text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!("Edit operation {operation_number} is missing string field new_text")
                        })?;
                    Ok(workspace_tools::EditOperation::Range {
                        start_line,
                        end_line,
                        old_text: old_text.to_string(),
                        new_text: new_text.to_string(),
                    })
                }
                other => Err(format!(
                    "Edit operation {operation_number} has unsupported type: {other}"
                )),
            }
        })
        .collect()
}

pub(crate) fn handle_edit_file(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return tool_error_response(req, "Missing required parameter: path".into()),
    };
    let operations = match parse_edit_operations(&arguments) {
        Ok(operations) => operations,
        Err(error) => return tool_error_response(req, error),
    };
    match workspace_tools::edit_file(workspace_root, path, &operations) {
        Ok(output) => {
            let text = output.render_text();
            let message = text.clone();
            tool_success_response_with_structured(
                req,
                text,
                json!({
                    "toolName": "edit",
                    "path": output.path,
                    "operationCount": output.operation_count,
                    "appliedOperations": output.applied_operations,
                    "replacedOccurrences": output.replaced_occurrences,
                    "bytesWritten": output.bytes_written,
                    "message": message,
                    "success": true,
                }),
            )
        }
        Err(e) => tool_error_response(req, e),
    }
}

pub(crate) fn handle_search_text(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let pattern = match required_string_argument(&arguments, "pattern") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let path = match optional_string_argument(&arguments, "path") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let glob = match optional_string_argument(&arguments, "glob") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let fixed_strings = match optional_bool_argument(&arguments, "fixed_strings", false) {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let case_insensitive = match optional_bool_argument(&arguments, "case_insensitive", false) {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let context = match optional_usize_argument(&arguments, "context") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let before = match optional_usize_argument(&arguments, "before") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let after = match optional_usize_argument(&arguments, "after") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let max_matches = match optional_usize_argument(&arguments, "max_matches") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let max_matches_per_file = match optional_usize_argument(&arguments, "max_matches_per_file") {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let include_hidden = match optional_bool_argument(&arguments, "include_hidden", false) {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    let no_ignore = match optional_bool_argument(&arguments, "no_ignore", false) {
        Ok(value) => value,
        Err(e) => return tool_error_response(req, e),
    };
    match workspace_tools::search_text(
        workspace_root,
        workspace_tools::SearchTextOptions {
            pattern,
            path,
            glob,
            fixed_strings,
            case_insensitive,
            context,
            before,
            after,
            max_matches,
            max_matches_per_file,
            include_hidden,
            no_ignore,
        },
    ) {
        Ok(output) => tool_success_response_with_structured(
            req,
            output.render_text(),
            json!({
                "toolName": "search",
                "searchPattern": output.pattern,
                "searchPath": output.path,
                "searchBackend": output.backend,
                "searchBackendNote": output.backend_note,
                "matchCount": output.match_count,
                "searchTruncated": output.truncated,
                "searchLimit": output.limit,
                "searchResults": output.results,
            }),
        ),
        Err(e) => tool_error_response(req, e),
    }
}

pub(crate) fn required_string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    match arguments.get(name) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("Parameter {name} must be a string")),
        None => Err(format!("Missing required parameter: {name}")),
    }
}

pub(crate) fn optional_string_argument<'a>(
    arguments: &'a Value,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match arguments.get(name) {
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("Parameter {name} must be a string")),
        None => Ok(None),
    }
}

fn optional_string_list_argument(arguments: &Value, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = arguments.get(name) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| format!("Parameter {name} must be an array of strings"))?;
    if items.len() > handoff::MAX_HANDOFF_LIST_ITEMS {
        return Err(format!(
            "Parameter {name} has too many items: {} (max {})",
            items.len(),
            handoff::MAX_HANDOFF_LIST_ITEMS
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let item = value
                .as_str()
                .ok_or_else(|| format!("Parameter {name}[{index}] must be a string"))?;
            let item = item.trim();
            if item.is_empty() {
                return Err(format!("Parameter {name}[{index}] must not be empty"));
            }
            Ok(item.to_string())
        })
        .collect()
}

fn optional_bool_argument(
    arguments: &Value,
    name: &str,
    default_value: bool,
) -> Result<bool, String> {
    match arguments.get(name) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("Parameter {name} must be a boolean")),
        None => Ok(default_value),
    }
}

fn optional_usize_argument(arguments: &Value, name: &str) -> Result<Option<usize>, String> {
    match arguments.get(name) {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("Parameter {name} must be a non-negative integer")),
        None => Ok(None),
    }
}

pub(crate) fn handle_delete_path(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return tool_error_response(req, "Missing required parameter: path".into()),
    };
    let recursive = arguments
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match workspace_tools::delete_path(workspace_root, path, recursive) {
        Ok(text) => {
            let message = text.clone();
            tool_success_response_with_structured(
                req,
                text,
                json!({
                    "toolName": "delete",
                    "path": path,
                    "recursive": recursive,
                    "message": message,
                }),
            )
        }
        Err(e) => tool_error_response(req, e),
    }
}

