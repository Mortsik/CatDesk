use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::change_tracking::{ChangeSession, FileChange};
use crate::command;
use crate::command_jobs::{
    CommandJobManager, DEFAULT_ABANDON_AFTER_MS, DEFAULT_JOB_TIMEOUT_MS, DEFAULT_POLL_WAIT_MS,
    MAX_JOB_TIMEOUT_MS, MAX_POLL_WAIT_MS,
};
use crate::devtools::DevtoolsBridge;
use crate::handoff;
use crate::state::{
    Mode, ShowDetailMode, TokenStatsLayout, ToolMode, WidgetCornerStyle,
};
use crate::workspace_tools;

mod jsonrpc;
mod token_usage;
mod agents_state;
mod resources;
mod instruction;
mod commands;
mod file_tools;
mod widget;

use widget::{
    attach_tool_call_count, attach_turn_token_usage,
    ensure_tool_descriptor_widget_template_with_show_detail_mode,
    enrich_tool_result_with_show_detail_mode, widget_payload_meta_mut,
};

use file_tools::{
    handle_create_handoff_for_project, handle_delete_path, handle_edit_file,
    handle_read_files, handle_read_image, handle_search_text, handle_write_file,
};

use commands::{
    change_scope_for_request, command_job_id_from_response, fetch_devtools_tools,
    forward_to_devtools, handle_cancel_command_with_session, handle_poll_command_with_session,
    handle_run_command, handle_start_command,
};

use instruction::{
    catdesk_instruction_required_response_with_show_detail_mode,
    handle_catdesk_instruction_with_show_detail_mode,
};

pub(crate) use resources::{
    MODERN_MCP_PROTOCOL_VERSION, WIDGET_PAYLOAD_META_KEY, decorate_modern_result,
    is_catdesk_widget_resource_uri,
};
use resources::{
    handle_resources_list_with_show_detail_mode, handle_server_discover,
    handle_resources_read_with_show_detail_mode,
};

pub(crate) use agents_state::agents_widget_state_payload;
use agents_state::cached_app_config;

pub(crate) use token_usage::estimate_turn_token_counts;
use token_usage::estimate_turn_token_usage;

pub use jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use jsonrpc::{tool_error_response, tool_name_from_request};

#[derive(Clone)]
struct AutoWidgetContext {
    is_error: bool,
    turn_files: Vec<FileChange>,
}

// ── Handler ─────────────────────────────────────────────────

#[cfg(test)]
pub async fn handle_request(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    public_base_url: Option<&str>,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    catdesk_instruction_called: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
) -> Option<JsonRpcResponse> {
    handle_request_with_show_detail_mode(
        req,
        workspace_root,
        mascot_seed,
        public_base_url,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        catdesk_instruction_called,
        command_jobs,
        devtools,
        current_show_detail_mode(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn handle_request_with_show_detail_mode(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    public_base_url: Option<&str>,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    catdesk_instruction_called: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
) -> Option<JsonRpcResponse> {
    handle_request_with_session(
        req,
        workspace_root,
        mascot_seed,
        public_base_url,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        catdesk_instruction_called,
        command_jobs,
        devtools,
        show_detail_mode,
        None,
        None,
    )
    .await
}

pub(crate) async fn handle_request_with_session(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    public_base_url: Option<&str>,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    catdesk_instruction_called: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
    session_namespace: Option<&str>,
    active_project: Option<&Path>,
) -> Option<JsonRpcResponse> {
    match req.method.as_str() {
        "server/discover" => Some(handle_server_discover(req, show_detail_mode)),
        m if m.starts_with("notifications/") => None,
        "tools/list" => Some(
            handle_tools_list_with_show_detail_mode(
                req,
                mode,
                tool_mode,
                devtools,
                show_detail_mode,
            )
            .await,
        ),
        "tools/call" => {
            let tool_name = tool_name_from_request(req);
            if tool_name != "catdesk_instruction" && !catdesk_instruction_called {
                Some(catdesk_instruction_required_response_with_show_detail_mode(
                    req,
                    show_detail_mode,
                ))
            } else {
                Some(
                    handle_tools_call_with_session(
                        req,
                        workspace_root,
                        mascot_seed,
                        mode,
                        tool_mode,
                        set_catdesk_as_co_author,
                        command_jobs,
                        devtools,
                        show_detail_mode,
                        session_namespace,
                        active_project,
                    )
                    .await,
                )
            }
        }
        "resources/list" => Some(handle_resources_list_with_show_detail_mode(
            req,
            public_base_url,
            show_detail_mode,
        )),
        "resources/read" => Some(handle_resources_read_with_show_detail_mode(
            req,
            public_base_url,
            mascot_seed,
            show_detail_mode,
        )),
        "ping" => Some(JsonRpcResponse::success(req.id.clone(), json!({}))),
        _ => Some(JsonRpcResponse::error(
            req.id.clone(),
            -32601,
            format!("Method not found: {}", req.method),
        )),
    }
}

fn local_tool_output_schema(name: &str) -> Option<Value> {
    // Keep read_image as a native multimodal tool result. ChatGPT otherwise
    // projects the call through outputSchema and only exposes structuredContent,
    // hiding content[type=image] from the model.
    if name == "read_image" {
        return None;
    }

    let mut properties = Map::new();
    properties.insert(
        "toolName".to_string(),
        json!({ "type": "string", "const": name }),
    );
    properties.insert("message".to_string(), json!({ "type": "string" }));
    properties.insert("success".to_string(), json!({ "type": "boolean" }));

    match name {
        "catdesk_instruction" => {
            properties.insert("instructionText".to_string(), json!({ "type": "string" }));
        }
        "read" => {
            properties.insert(
                "path".to_string(),
                json!({
                    "type": "string",
                    "description": "One file the totals below are headed by. Every file read is in files[]."
                }),
            );
            properties.insert(
                "bytes".to_string(),
                json!({ "type": "integer", "minimum": 0, "description": "Total across the batch." }),
            );
            properties.insert(
                "sizeBytes".to_string(),
                json!({ "type": "integer", "minimum": 0, "description": "Total across the batch." }),
            );
            properties.insert(
                "lineCount".to_string(),
                json!({ "type": "integer", "minimum": 0, "description": "Total across the batch." }),
            );
            properties.insert(
                "fileCount".to_string(),
                json!({
                    "type": "integer",
                    "minimum": 0,
                    "description": "Entries in files[], including failures."
                }),
            );
            properties.insert(
                "batchTruncated".to_string(),
                json!({
                    "type": "boolean",
                    "description": "The shared budget cut something short, so asking for fewer files returns more."
                }),
            );
            properties.insert(
                "files".to_string(),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "error": { "type": "string" },
                            "bytes": {
                                "type": "integer",
                                "minimum": 0,
                                "description": "Bytes of text returned. Not comparable to sizeBytes: undecodable bytes are replaced and take more room than they did on disk."
                            },
                            "sizeBytes": {
                                "type": "integer",
                                "minimum": 0,
                                "description": "Size on disk. Use truncated, not a comparison with bytes, to tell whether this file came back whole."
                            },
                            "lineCount": {
                                "type": "integer",
                                "minimum": 0,
                                "description": "Lines in the text returned, not in the whole file."
                            },
                            "text": { "type": "string" },
                            "truncated": { "type": "boolean" },
                            "budgetTruncated": {
                                "type": "boolean",
                                "description": "Cut by the shared budget, so asking for fewer files returns more of this one. A file truncated without this is over the per-file cap and no retry returns the rest."
                            }
                        },
                        "required": ["path", "bytes", "sizeBytes", "lineCount", "text", "truncated", "budgetTruncated"]
                    }
                }),
            );
        }
        "search" => {
            properties.insert("searchPattern".to_string(), json!({ "type": "string" }));
            properties.insert("searchPath".to_string(), json!({ "type": "string" }));
            properties.insert("searchBackend".to_string(), json!({ "type": "string" }));
            properties.insert("searchBackendNote".to_string(), json!({ "type": "string" }));
            properties.insert(
                "matchCount".to_string(),
                json!({ "type": "integer", "minimum": 0 }),
            );
            properties.insert("searchTruncated".to_string(), json!({ "type": "boolean" }));
            properties.insert(
                "searchLimit".to_string(),
                json!({ "type": "integer", "minimum": 0 }),
            );
            properties.insert(
                "searchResults".to_string(),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "line": { "type": "integer", "minimum": 0 },
                            "text": { "type": "string" },
                            "isContext": { "type": "boolean" }
                        },
                        "required": ["path", "line", "text", "isContext"]
                    }
                }),
            );
        }
        "write" => {
            properties.insert("path".to_string(), json!({ "type": "string" }));
            properties.insert(
                "bytesWritten".to_string(),
                json!({ "type": "integer", "minimum": 0 }),
            );
            properties.insert("createDirs".to_string(), json!({ "type": "boolean" }));
        }
        "edit" => {
            properties.insert("path".to_string(), json!({ "type": "string" }));
            for field in [
                "operationCount",
                "appliedOperations",
                "replacedOccurrences",
                "bytesWritten",
            ] {
                properties.insert(
                    field.to_string(),
                    json!({ "type": "integer", "minimum": 0 }),
                );
            }
        }
        "create_handoff" => {
            properties.insert("filename".to_string(), json!({ "type": "string" }));
            properties.insert("searchPrefix".to_string(), json!({ "type": "string" }));
            properties.insert("content".to_string(), json!({ "type": "string" }));
            properties.insert(
                "bytes".to_string(),
                json!({ "type": "integer", "minimum": 0 }),
            );
            properties.insert("gitAvailable".to_string(), json!({ "type": "boolean" }));
            properties.insert(
                "gitStatusAvailable".to_string(),
                json!({ "type": "boolean" }),
            );
            properties.insert(
                "gitBranch".to_string(),
                json!({ "type": ["string", "null"] }),
            );
            for field in ["gitStatus", "recentCommits"] {
                properties.insert(
                    field.to_string(),
                    json!({ "type": "array", "items": { "type": "string" } }),
                );
            }
        }
        "delete" => {
            properties.insert("path".to_string(), json!({ "type": "string" }));
            properties.insert("recursive".to_string(), json!({ "type": "boolean" }));
        }
        "start_command" | "poll_command" | "cancel_command" => {
            for field in ["jobId", "command", "cwd", "state"] {
                properties.insert(field.to_string(), json!({ "type": "string" }));
            }
            for field in ["elapsedMs", "nextCursor", "timeoutMs"] {
                properties.insert(
                    field.to_string(),
                    json!({ "type": "integer", "minimum": 0 }),
                );
            }
            properties.insert(
                "exitCode".to_string(),
                json!({ "type": ["integer", "null"] }),
            );
            properties.insert(
                "commandSuccess".to_string(),
                json!({ "type": ["boolean", "null"] }),
            );
            properties.insert("hasMoreOutput".to_string(), json!({ "type": "boolean" }));
            properties.insert("outputTruncated".to_string(), json!({ "type": "boolean" }));
            properties.insert("deduplicated".to_string(), json!({ "type": "boolean" }));
            properties.insert(
                "events".to_string(),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "seq": { "type": "integer", "minimum": 1 },
                            "stream": { "type": "string", "enum": ["stdout", "stderr"] },
                            "text": { "type": "string" }
                        },
                        "required": ["seq", "stream", "text"]
                    }
                }),
            );
        }
        "run_command" => {
            for field in [
                "command",
                "cwd",
                "stdout",
                "stderr",
                "interceptedToolName",
                "interceptedCommandName",
                "from",
                "to",
                "resolvedFrom",
                "resolvedTo",
                "destinationOperand",
                "listPath",
            ] {
                properties.insert(field.to_string(), json!({ "type": "string" }));
            }
            for field in [
                "elapsedMs",
                "listItemCount",
                "listDirectoryCount",
                "listFileCount",
                "listOtherCount",
                "listLimit",
            ] {
                properties.insert(
                    field.to_string(),
                    json!({ "type": "integer", "minimum": 0 }),
                );
            }
            properties.insert(
                "exitCode".to_string(),
                json!({ "type": ["integer", "null"] }),
            );
            for field in [
                "destinationOperandWasDirectory",
                "overwrite",
                "skipped",
                "listTruncated",
                "timedOut",
                "stdoutTruncated",
                "stderrTruncated",
            ] {
                properties.insert(field.to_string(), json!({ "type": "boolean" }));
            }
            properties.insert(
                "listEntries".to_string(),
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "name": { "type": "string" },
                            "kind": { "type": "string" },
                            "depth": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["path", "name", "kind", "depth"]
                    }
                }),
            );
        }
        _ => return None,
    }

    Some(json!({
        "type": "object",
        "properties": properties,
        "required": ["toolName"]
    }))
}

fn ensure_local_tool_output_schema(tool: &mut Value) {
    let Some(tool_obj) = tool.as_object_mut() else {
        return;
    };
    if tool_obj.contains_key("outputSchema") {
        return;
    }
    let Some(name) = tool_obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let Some(schema) = local_tool_output_schema(&name) else {
        return;
    };
    tool_obj.insert("outputSchema".to_string(), schema);
}

fn catdesk_instruction_tool_descriptor() -> Value {
    json!({
        "name": "catdesk_instruction",
        "title": "Get usage instructions",
        "description": "Read CatDesk operating guidance. You must call this tool successfully once after CatDesk starts before calling any other CatDesk tool.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false, "destructiveHint": false }
    })
}

fn create_handoff_tool_descriptor() -> Value {
    json!({
        "name": "create_handoff",
        "title": "Create session handoff",
        "description": "Prepare a workspace-specific Markdown handoff for persistent storage in ChatGPT Library. CatDesk returns a filename and content but does not write the workspace. After this tool succeeds, save the returned artifact to Library. CatDesk automatically records the current Git branch, status, and recent commits. Do not include credentials, tokens, passwords, or other secrets in the handoff.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "goal": { "type": "string", "minLength": 1, "description": "The current task or overall goal that the next session should continue" },
                "completed": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "maxItems": handoff::MAX_HANDOFF_LIST_ITEMS,
                    "description": "Work already completed in this session"
                },
                "decisions": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "maxItems": handoff::MAX_HANDOFF_LIST_ITEMS,
                    "description": "Important implementation decisions or constraints that should be preserved"
                },
                "validation": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "maxItems": handoff::MAX_HANDOFF_LIST_ITEMS,
                    "description": "Tests, builds, checks, or other validation already performed"
                },
                "next_steps": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "maxItems": handoff::MAX_HANDOFF_LIST_ITEMS,
                    "description": "Concrete next actions for the next session"
                },
                "notes": { "type": "string", "description": "Optional free-form context that does not fit the structured sections" }
            },
            "required": ["goal"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false, "destructiveHint": false }
    })
}

#[cfg(test)]
async fn handle_tools_list(
    req: &JsonRpcRequest,
    mode: Mode,
    tool_mode: ToolMode,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
) -> JsonRpcResponse {
    handle_tools_list_with_show_detail_mode(
        req,
        mode,
        tool_mode,
        devtools,
        current_show_detail_mode(),
    )
    .await
}

async fn handle_tools_list_with_show_detail_mode(
    req: &JsonRpcRequest,
    mode: Mode,
    tool_mode: ToolMode,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
) -> JsonRpcResponse {
    let mut tools: Vec<Value> = Vec::new();

    // Computer tools
    if mode.computer_enabled() {
        if tool_mode.run_command_enabled() {
            tools.push(json!({
                "name": "run_command",
                "title": "Run command",
                "description": "Execute a shell command inside the workspace root. Common directory-listing commands are parsed before execution and may return structured workspace listings instead of raw shell output. Returns stdout and stderr for non-intercepted commands.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute" },
                        "cwd": { "type": "string", "description": "Working directory relative to workspace root or absolute path within it" },
                        "timeout": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": command::MAX_TIMEOUT_MS,
                            "description": format!(
                                "Timeout in milliseconds for short commands. Maximum {}; use start_command for long-running work.",
                                command::MAX_TIMEOUT_MS
                            )
                        }
                    },
                    "required": ["command"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": true, "destructiveHint": true }
            }));
            tools.push(json!({
                "name": "start_command",
                "title": "Start command",
                "description": format!(
                    "Start a long-running shell command inside the workspace and return a job ID immediately. Prefer this for builds, compilation, dependency installation, long test suites, and development servers instead of keeping run_command open. Polling keeps a job alive: a running job with no poll_command for {} minutes is ended as state \"abandoned\", so poll while you still need it.",
                    DEFAULT_ABANDON_AFTER_MS / 60_000
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to start" },
                        "cwd": { "type": "string", "description": "Working directory relative to workspace root or absolute path within it" },
                        "timeout": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_JOB_TIMEOUT_MS,
                            "description": format!(
                                "Maximum command runtime in milliseconds. Defaults to {} ms; maximum is {} ms. The job's state and exit code survive a CatDesk restart; a job the restart took down reports state \"interrupted\".",
                                DEFAULT_JOB_TIMEOUT_MS,
                                MAX_JOB_TIMEOUT_MS
                            )
                        }
                    },
                    "required": ["command"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": true, "destructiveHint": true }
            }));
            tools.push(json!({
                "name": "poll_command",
                "title": "Poll command",
                "description": format!(
                    "Read incremental output and current status from a command previously started with start_command. Pass the returned nextCursor as after on the next poll so output is not repeated. If hasMoreOutput is true, poll again even if the job is already terminal so the remaining buffered output can be drained. A job that was running when CatDesk exited reports state \"interrupted\" with no further output; finished job state and exit code survive a restart. Each poll also refreshes the job's keep-alive: a running job nobody polls for {} minutes is ended as state \"abandoned\".",
                    DEFAULT_ABANDON_AFTER_MS / 60_000
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "Opaque command job ID returned by start_command" },
                        "after": { "type": "integer", "minimum": 0, "description": "Return only output events after this cursor (default 0)" },
                        "wait_ms": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": MAX_POLL_WAIT_MS,
                            "description": format!(
                                "Wait for new output or completion before returning (default {DEFAULT_POLL_WAIT_MS} ms, maximum {MAX_POLL_WAIT_MS} ms). Pass 0 for a non-blocking check."
                            )
                        }
                    },
                    "required": ["job_id"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }
            }));
            tools.push(json!({
                "name": "cancel_command",
                "title": "Cancel command",
                "description": "Cancel a command started with start_command and terminate its complete child process tree.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "Opaque command job ID returned by start_command" }
                    },
                    "required": ["job_id"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }
            }));
        }

        tools.push(catdesk_instruction_tool_descriptor());
        tools.push(json!({
            "name": "read",
            "title": "Read files",
            "description": "Read text files from the workspace. Name every file you need in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                        "maxItems": workspace_tools::MAX_READ_BATCH_FILES,
                        "description": format!(
                            "File paths relative to workspace root, or absolute paths within it. Paths that resolve to the same file are read once. Combined text is capped at {} bytes; files past that return metadata only.",
                            workspace_tools::MAX_READ_BATCH_BYTES
                        )
                    }
                },
                "required": ["paths"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false, "destructiveHint": false }
        }));
        tools.push(json!({
            "name": "read_image",
            "title": "Read image",
            "description": "Read an image from the workspace and return it as native MCP image content for visual/vision analysis.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "Image path relative to workspace root, or an absolute path within it" },
                    "max_width": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": workspace_tools::MAX_IMAGE_OUTPUT_DIMENSION,
                        "description": format!(
                            "Maximum returned image width in pixels (default {}, maximum {})",
                            workspace_tools::DEFAULT_IMAGE_MAX_WIDTH,
                            workspace_tools::MAX_IMAGE_OUTPUT_DIMENSION
                        )
                    },
                    "max_height": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": workspace_tools::MAX_IMAGE_OUTPUT_DIMENSION,
                        "description": format!(
                            "Maximum returned image height in pixels (default {}, maximum {})",
                            workspace_tools::DEFAULT_IMAGE_MAX_HEIGHT,
                            workspace_tools::MAX_IMAGE_OUTPUT_DIMENSION
                        )
                    },
                    "analyze": {
                        "oneOf": [
                            { "type": "boolean" },
                            { "type": "string", "minLength": 1 }
                        ],
                        "description": "Optionally describe the image with a server-side vision model and return the description as text (structuredContent.analysis.description). true uses a default prompt; a non-empty string is used as the custom prompt. Requires the vision backend to be configured (GEMINI_API_KEY)."
                    }
                },
                "required": ["path"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false, "destructiveHint": false }
        }));
        tools.push(json!({
            "name": "search",
            "title": "Search text",
            "description": "Search text across files in workspace. Uses rg when available, then grep, then built-in search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Ripgrep regex pattern" },
                    "path": { "type": "string", "description": "File or directory path (default: workspace root)" },
                    "glob": { "type": "string", "description": "Ripgrep glob filter, for example '*.rs' or 'src/**/*.ts'" },
                    "fixed_strings": { "type": "boolean", "description": "Treat pattern as a literal string" },
                    "case_insensitive": { "type": "boolean", "description": "Use case-insensitive matching" },
                    "context": { "type": "integer", "description": "Context lines before and after each match (0..20). When set, before/after are ignored." },
                    "before": { "type": "integer", "description": "Context lines before each match (0..20)" },
                    "after": { "type": "integer", "description": "Context lines after each match (0..20)" },
                    "max_matches": { "type": "integer", "description": "Max returned matches (1..500, default 100)" },
                    "max_matches_per_file": { "type": "integer", "description": "Max matches per file (1..500)" },
                    "include_hidden": { "type": "boolean", "description": "Include dotfiles and dot-directories" },
                    "no_ignore": { "type": "boolean", "description": "Do not respect ignore files" }
                },
                "required": ["pattern"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false, "destructiveHint": false }
        }));

        if tool_mode.write_tools_enabled() {
            tools.push(json!({
                "name": "write",
                "title": "Write file",
                "description": "Create or overwrite a file in workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "create_dirs": { "type": "boolean", "description": "Create parent directories if missing" }
                    },
                    "required": ["path", "content"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }
            }));
            tools.push(json!({
                "name": "edit",
                "title": "Edit file",
                "description": "Apply one or more guarded edits to a workspace file atomically. Operations run in order in memory and the file is written only if every operation succeeds. Use replace for exact literal replacement and range for a 1-based inclusive line range guarded by exact old_text.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "description": "Ordered edit operations. The whole batch is atomic.",
                            "items": {
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": {
                                            "type": { "type": "string", "const": "replace" },
                                            "old_string": { "type": "string", "description": "Exact literal text to replace" },
                                            "new_string": { "type": "string", "description": "Exact literal replacement text" },
                                            "replace_all": { "type": "boolean", "description": "Replace all occurrences of old_string (default false)" }
                                        },
                                        "required": ["type", "old_string", "new_string"]
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "type": { "type": "string", "const": "range" },
                                            "start_line": { "type": "integer", "minimum": 1, "description": "1-based first line of the guarded range" },
                                            "end_line": { "type": "integer", "minimum": 1, "description": "1-based inclusive last line of the guarded range" },
                                            "old_text": { "type": "string", "description": "Exact current text spanning the selected complete lines, including existing line endings" },
                                            "new_text": { "type": "string", "description": "Replacement text for the selected line range" }
                                        },
                                        "required": ["type", "start_line", "end_line", "old_text", "new_text"]
                                    }
                                ]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }
            }));
            tools.push(create_handoff_tool_descriptor());
            tools.push(json!({
                "name": "delete",
                "title": "Delete path",
                "description": "Delete file or directory in workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "recursive": { "type": "boolean", "description": "Delete directories recursively" }
                    },
                    "required": ["path"]
                },
                "annotations": { "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }
            }));
        }
        if tool_mode.read_only() {
            tools.push(create_handoff_tool_descriptor());
        }
    }

    if !mode.computer_enabled() && mode.browser_enabled() {
        tools.push(catdesk_instruction_tool_descriptor());
    }

    // Browser tools — get from devtools bridge
    if mode.browser_enabled() {
        if let Some(bridge) = devtools {
            if let Some(dt_tools) = fetch_devtools_tools(bridge).await {
                if tool_mode.read_only() {
                    tools.extend(dt_tools.into_iter().filter(tool_is_read_only));
                } else {
                    tools.extend(dt_tools);
                }
            }
        }
    }

    for tool in &mut tools {
        ensure_local_tool_output_schema(tool);
        ensure_tool_descriptor_widget_template_with_show_detail_mode(tool, show_detail_mode);
    }

    JsonRpcResponse::success(req.id.clone(), json!({ "tools": tools }))
}

// ── tools/call ──────────────────────────────────────────────

#[cfg(test)]
async fn handle_tools_call(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
) -> JsonRpcResponse {
    handle_tools_call_with_show_detail_mode(
        req,
        workspace_root,
        mascot_seed,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        command_jobs,
        devtools,
        current_show_detail_mode(),
    )
    .await
}

#[cfg(test)]
async fn handle_tools_call_with_show_detail_mode(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
) -> JsonRpcResponse {
    handle_tools_call_with_session(
        req,
        workspace_root,
        mascot_seed,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        command_jobs,
        devtools,
        show_detail_mode,
        None,
        None,
    )
    .await
}

async fn handle_tools_call_with_session(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
    session_namespace: Option<&str>,
    active_project: Option<&Path>,
) -> JsonRpcResponse {
    let params = &req.params;
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let change_session = (show_detail_mode != ShowDetailMode::Disable).then(|| {
        ChangeSession::begin(
            Path::new(workspace_root),
            change_scope_for_request(req, workspace_root, active_project),
        )
    });

    let mut response = {
        if tool_name == "catdesk_instruction" {
            handle_catdesk_instruction_with_show_detail_mode(
                req,
                workspace_root,
                mascot_seed,
                mode,
                tool_mode,
                show_detail_mode,
                active_project,
            )
        // Local computer tools
        } else if mode.computer_enabled() {
            if matches!(
                tool_name.as_str(),
                "run_command" | "start_command" | "poll_command" | "cancel_command"
            ) {
                if tool_mode.run_command_enabled() {
                    match tool_name.as_str() {
                        "run_command" => {
                            handle_run_command(
                                req,
                                workspace_root,
                                set_catdesk_as_co_author,
                                active_project,
                            )
                            .await
                        }
                        "start_command" => {
                            handle_start_command(
                                req,
                                workspace_root,
                                set_catdesk_as_co_author,
                                command_jobs,
                                show_detail_mode,
                                session_namespace,
                                active_project,
                            )
                            .await
                        }
                        "poll_command" => {
                            handle_poll_command_with_session(req, command_jobs, session_namespace).await
                        }
                        "cancel_command" => {
                            handle_cancel_command_with_session(req, command_jobs, session_namespace)
                                .await
                        }
                        _ => unreachable!(),
                    }
                } else if tool_mode.read_only() {
                    read_only_blocked_response(req, &tool_name)
                } else {
                    tool_error_response(req, format!("Unknown tool: {tool_name}"))
                }
            } else {
                match tool_name.as_str() {
                    "read" => handle_read_files(req, workspace_root),
                    "read_image" => handle_read_image(req, workspace_root).await,
                    "search" => handle_search_text(req, workspace_root),
                    "create_handoff" =>
                        handle_create_handoff_for_project(req, workspace_root, active_project),
                    _ => {
                        if tool_mode.write_tools_enabled() {
                            match tool_name.as_str() {
                                "write" => handle_write_file(req, workspace_root),
                                "edit" => handle_edit_file(req, workspace_root),
                                "delete" => handle_delete_path(req, workspace_root),
                                _ => {
                                    if mode.browser_enabled() {
                                        forward_to_devtools(req, &tool_name, tool_mode, devtools)
                                            .await
                                    } else {
                                        tool_error_response(
                                            req,
                                            format!("Unknown tool: {tool_name}"),
                                        )
                                    }
                                }
                            }
                        } else if tool_mode.read_only() && is_local_destructive_tool(&tool_name) {
                            read_only_blocked_response(req, &tool_name)
                        } else if mode.browser_enabled() {
                            forward_to_devtools(req, &tool_name, tool_mode, devtools).await
                        } else {
                            tool_error_response(req, format!("Unknown tool: {tool_name}"))
                        }
                    }
                }
            }
        } else if mode.browser_enabled() {
            forward_to_devtools(req, &tool_name, tool_mode, devtools).await
        } else {
            tool_error_response(req, format!("Unknown tool: {tool_name}"))
        }
    };

    let mut turn_files = change_session
        .as_ref()
        .map(ChangeSession::changes)
        .unwrap_or_default();
    if show_detail_mode != ShowDetailMode::Disable
        && matches!(
            tool_name.as_str(),
            "start_command" | "poll_command" | "cancel_command"
        )
    {
        if let Some(job_id) = command_job_id_from_response(&response) {
            if let Ok(job_changes) = command_jobs
                .current_changes_for_session(job_id, session_namespace)
                .await
            {
                turn_files = job_changes;
            }
        }
    }
    let is_error = response
        .result
        .as_ref()
        .and_then(|v| v.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_turn_changes = !turn_files.is_empty();
    let widget_context = AutoWidgetContext {
        is_error,
        turn_files,
    };

    let tool_name = tool_name_from_request(req);
    if let Some(result) = response.result.take() {
        if has_turn_changes {
            response.result = Some(enrich_tool_result_with_show_detail_mode(
                req,
                result,
                Some(&widget_context),
                show_detail_mode,
            ));
        } else {
            response.result = Some(enrich_tool_result_with_show_detail_mode(
                req,
                result,
                None,
                show_detail_mode,
            ));
        }
    }

    if let Some(result) = response.result.as_mut() {
        if widget_payload_meta_mut(result).is_some() {
            let turn_token_usage = estimate_turn_token_usage(req, &tool_name, result);
            attach_turn_token_usage(result, &turn_token_usage);
            attach_tool_call_count(result, 1);
        }
    }

    response
}

fn read_only_blocked_response(req: &JsonRpcRequest, tool_name: &str) -> JsonRpcResponse {
    tool_error_response(
        req,
        format!("Tool '{tool_name}' is disabled in read-only mode"),
    )
}

fn current_token_stats_layout() -> TokenStatsLayout {
    cached_app_config()
        .map(|config| config.token_stats_layout)
        .unwrap_or_default()
}

fn current_widget_corner_style() -> WidgetCornerStyle {
    cached_app_config()
        .map(|config| config.widget_corner_style)
        .unwrap_or_default()
}

#[cfg(test)]
fn current_show_detail_mode() -> ShowDetailMode {
    ShowDetailMode::Expanded
}

fn is_local_destructive_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "run_command"
            | "start_command"
            | "poll_command"
            | "cancel_command"
            | "write"
            | "edit"
            | "delete"
    )
}

fn tool_is_read_only(tool: &Value) -> bool {
    tool.get("annotations")
        .and_then(|v| v.get("readOnlyHint"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
