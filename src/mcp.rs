use base64::Engine as _;
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
use crate::vision;
use crate::workspace_tools;

mod jsonrpc;
mod token_usage;
mod agents_state;
mod resources;
mod instruction;
mod commands;

use commands::{
    change_scope_for_request, command_job_id_from_response, fetch_devtools_tools,
    format_command_output_events, forward_to_devtools, handle_cancel_command_with_session,
    handle_poll_command_with_session, handle_run_command, handle_start_command,
};

use instruction::{
    catdesk_instruction_required_response_with_show_detail_mode, handle_catdesk_instruction_with_show_detail_mode,
    instruction_context_root,
};

pub(crate) use resources::{
    MODERN_MCP_PROTOCOL_VERSION, WIDGET_PAYLOAD_META_KEY, decorate_modern_result,
    is_catdesk_widget_resource_uri,
};
use resources::{
    current_widget_resource_uri, current_widget_resource_uri_for_tool,
    handle_resources_list_with_show_detail_mode, handle_server_discover,
    handle_resources_read_with_show_detail_mode,
};

pub(crate) use agents_state::agents_widget_state_payload;
use agents_state::cached_app_config;

pub(crate) use token_usage::estimate_turn_token_counts;
use token_usage::{TokenUsage, estimate_turn_token_usage};

pub use jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use jsonrpc::{
    image_tool_success_response, tool_arguments, tool_error_response,
    tool_error_response_with_structured, tool_success_response_with_structured,
    tool_name_from_request,
};

const MAX_WIDGET_COMMAND_OUTPUT_CHARS: usize = 4_000;
const MAX_WIDGET_DIFF_CHARS_PER_FILE: usize = 1_500;
const MAX_WIDGET_LIST_ENTRIES: usize = 100;

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

fn ensure_output_template_meta(meta_value: &mut Value) {
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

fn attach_widget_payload_meta(result: &mut Value, payload: Value) {
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

fn widget_payload_meta_mut(result: &mut Value) -> Option<&mut Map<String, Value>> {
    result
        .as_object_mut()?
        .get_mut("_meta")?
        .as_object_mut()?
        .get_mut(WIDGET_PAYLOAD_META_KEY)?
        .as_object_mut()
}

fn attach_turn_token_usage(result: &mut Value, usage: &TokenUsage) {
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

fn attach_tool_call_count(result: &mut Value, tool_call_count: u64) {
    if let Some(widget_payload) = widget_payload_meta_mut(result) {
        widget_payload.insert("toolCallCount".to_string(), json!(tool_call_count));
    }
}

fn tool_descriptor_should_attach_widget(name: &str) -> bool {
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

fn ensure_tool_descriptor_widget_template_with_show_detail_mode(
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

fn remove_text_content_from_tool_result(req: &JsonRpcRequest, result: &mut Value) {
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

fn base_widget_payload(
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
fn base_widget_payload_with_show_detail_mode(
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

fn build_list_files_widget_payload_from_structured(
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

fn build_command_job_widget_payload(
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

fn enrich_tool_result_with_show_detail_mode(
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
fn enrich_tool_result(
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

fn parse_read_paths(arguments: &Value) -> Result<Vec<String>, String> {
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

fn handle_read_files(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

async fn handle_read_image(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

fn handle_write_file(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

fn handle_create_handoff_for_project(
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

fn handle_edit_file(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

fn handle_search_text(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

fn handle_delete_path(req: &JsonRpcRequest, workspace_root: &str) -> JsonRpcResponse {
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

#[cfg(test)]
mod tests;
