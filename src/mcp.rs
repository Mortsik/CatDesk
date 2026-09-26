use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::change_tracking::{ChangeSession, FileChange};
use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::state::{
    Mode, ShowDetailMode, TokenStatsLayout, ToolMode, WidgetCornerStyle,
};

mod jsonrpc;
mod token_usage;
mod agents_state;
mod resources;
mod instruction;
mod commands;
mod file_tools;
mod widget;
mod tool_catalog;

use tool_catalog::handle_tools_list_with_show_detail_mode;

use widget::{
    attach_tool_call_count, attach_turn_token_usage, enrich_tool_result_with_show_detail_mode,
    widget_payload_meta_mut,
};

use file_tools::{
    handle_create_handoff_for_project, handle_delete_path, handle_edit_file,
    handle_read_files, handle_read_image, handle_search_text, handle_write_file,
};

use commands::{
    change_scope_for_request, command_job_id_from_response, forward_to_devtools,
    handle_cancel_command_with_session, handle_poll_command_with_session, handle_run_command,
    handle_start_command,
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
