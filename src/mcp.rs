use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use tokio::sync::Mutex;

use crate::change_tracking::{ChangeSession, FileChange};
use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::result_store::{LargeResultStore, StoreError};
use crate::state::{Mode, ShowDetailMode, TokenStatsLayout, ToolMode, WidgetCornerStyle};
use crate::tool_result_metrics;

mod agents_state;
mod commands;
mod file_tools;
mod instruction;
mod jsonrpc;
mod resources;
mod response_budget;
mod result_tools;
mod token_usage;
mod tool_catalog;
mod widget;

use tool_catalog::handle_tools_list_with_show_detail_mode;

use widget::{
    attach_tool_call_count, attach_turn_token_usage, enrich_tool_result_with_show_detail_mode,
    widget_payload_meta_mut,
};

use file_tools::{
    handle_create_handoff_for_project, handle_delete_path, handle_edit_file, handle_read_files,
    handle_read_image, handle_search_text, handle_write_file,
};
use result_tools::{handle_read_result, handle_search_result};

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
    handle_resources_list_with_show_detail_mode, handle_resources_read_with_show_detail_mode,
    handle_server_discover,
};

pub(crate) use agents_state::agents_widget_state_payload;
use agents_state::cached_app_config;

pub(crate) use token_usage::estimate_turn_token_counts;
use token_usage::{estimate_turn_token_usage, exempt_from_response_budget};

#[cfg(test)]
use jsonrpc::tool_error_response_with_structured;
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
        fallback_result_store(),
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
    result_store: &LargeResultStore,
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
                    handle_tools_call_with_result_store(
                        req,
                        workspace_root,
                        mascot_seed,
                        mode,
                        tool_mode,
                        set_catdesk_as_co_author,
                        command_jobs,
                        devtools,
                        show_detail_mode,
                        result_store,
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

#[cfg(test)]
fn fallback_result_store() -> &'static LargeResultStore {
    static FALLBACK_RESULT_STORE: OnceLock<LargeResultStore> = OnceLock::new();
    FALLBACK_RESULT_STORE.get_or_init(|| {
        LargeResultStore::new_default().expect("create fallback large-result store")
    })
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

#[cfg(test)]
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
    handle_tools_call_with_result_store(
        req,
        workspace_root,
        mascot_seed,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        command_jobs,
        devtools,
        show_detail_mode,
        fallback_result_store(),
        session_namespace,
        active_project,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_tools_call_with_result_store(
    req: &JsonRpcRequest,
    workspace_root: &str,
    mascot_seed: u64,
    mode: Mode,
    tool_mode: ToolMode,
    set_catdesk_as_co_author: bool,
    command_jobs: &CommandJobManager,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
    show_detail_mode: ShowDetailMode,
    result_store: &LargeResultStore,
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
                            handle_poll_command_with_session(req, command_jobs, session_namespace)
                                .await
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
                    "read_result" => {
                        handle_read_result(req, workspace_root, result_store, session_namespace)
                    }
                    "search_result" => {
                        handle_search_result(req, workspace_root, result_store, session_namespace)
                    }
                    "create_handoff" => {
                        handle_create_handoff_for_project(req, workspace_root, active_project)
                    }
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

    let mut budget_outcome: Option<response_budget::BudgetOutcome> = None;
    // Serialized size before the lossy entry-cap reduction, if one happened:
    // telemetry must keep reporting the full result as raw.
    let mut entry_cap_raw_bytes: Option<u64> = None;
    if !exempt_from_response_budget(&tool_name)
        && let Some(result) = response.result.as_mut()
    {
        // Store first and replace only after a successful lossless write. If the
        // store is unavailable or rejects the payload, the original response is
        // left untouched rather than silently losing capability — except an
        // entry-cap rejection, which would otherwise send the oversized
        // payload inline (finding F2): the response is reduced until it fits
        // the cap and externalized like any other.
        budget_outcome = match response_budget::apply_response_budget(
            result,
            is_error,
            result_store,
            session_namespace,
            Path::new(workspace_root),
            None,
        ) {
            Ok(outcome) => outcome,
            Err(StoreError::EntryTooLarge { .. }) => {
                // The reducer is lossy and the retry stores the reduced
                // payload, so the inline manifest must disclose the
                // truncation before anything reads the outputRef.
                let reduction = response_budget::EntryCapReduction {
                    original_bytes: serde_json::to_vec(result)
                        .map_or(0, |bytes| bytes.len() as u64),
                };
                entry_cap_raw_bytes = Some(reduction.original_bytes);
                reduce_result_to_entry_cap(result, result_store.max_entry_bytes());
                match response_budget::apply_response_budget(
                    result,
                    is_error,
                    result_store,
                    session_namespace,
                    Path::new(workspace_root),
                    Some(reduction),
                ) {
                    Ok(Some(outcome)) => Some(outcome),
                    Ok(None) => {
                        // Reduced below the inline budget: nothing is
                        // externalized and no manifest appears, so the
                        // disclosure rides on the result directly. Attach it
                        // before the final size check — the added fields can
                        // push the answer over the budget again, and that
                        // overflow goes through the standard externalization
                        // path, whose rebuilt manifest re-attaches the
                        // disclosure at every compaction level.
                        attach_entry_cap_disclosure(result, reduction);
                        if serde_json::to_vec(result).map_or(0, |bytes| bytes.len())
                            > response_budget::DEFAULT_INLINE_RESPONSE_BYTES
                        {
                            response_budget::apply_response_budget(
                                result,
                                is_error,
                                result_store,
                                session_namespace,
                                Path::new(workspace_root),
                                Some(reduction),
                            )
                            .ok()
                            .flatten()
                        } else {
                            None
                        }
                    }
                    // Store unavailable after the reduction: the lossy
                    // answer travels inline without a manifest, so the
                    // disclosure rides on the result directly.
                    Err(_) => {
                        attach_entry_cap_disclosure(result, reduction);
                        None
                    }
                }
            }
            Err(_) => None,
        };
    }

    if let Some(result) = response.result.as_mut()
        && widget_payload_meta_mut(result).is_some()
    {
        let turn_token_usage = estimate_turn_token_usage(req, &tool_name, result);
        attach_turn_token_usage(result, &turn_token_usage);
        attach_tool_call_count(result, 1);
    }

    // Byte accounting: when the budget externalized the result, its own
    // serialization is the source of truth for the raw size; an untouched
    // result was sent byte-for-byte, so raw equals the final inline size.
    // Transport-level decorations applied later in server.rs are excluded.
    let inline_bytes = response
        .result
        .as_ref()
        .and_then(|result| serde_json::to_vec(result).ok())
        .map_or(0, |bytes| bytes.len() as u64);
    let (raw_bytes, externalized_bytes) =
        resolve_raw_and_externalized_bytes(budget_outcome, entry_cap_raw_bytes, inline_bytes);
    tool_result_metrics::observe(
        Some(&tool_name),
        tool_result_metrics::ToolResultMeasurement {
            raw_bytes,
            inline_bytes,
            externalized_bytes,
            is_error,
        },
    );

    response
}

fn read_only_blocked_response(req: &JsonRpcRequest, tool_name: &str) -> JsonRpcResponse {
    tool_error_response(
        req,
        format!("Tool '{tool_name}' is disabled in read-only mode"),
    )
}

// ── Entry-cap reduction (finding F2) ─────────────────────────

/// Resolve telemetry raw/externalized bytes for one tool result. Raw is the
/// largest form the result ever had: the pre-reduction size for an
/// entry-cap-reduced result (the externalized retry stores only the reduced
/// payload, and an unreduced retry that stays inline drops bytes in-band, so
/// it classifies as compacted rather than small), the stored payload for a
/// lossless externalization, or the inline size when nothing trimmed it.
fn resolve_raw_and_externalized_bytes(
    budget_outcome: Option<response_budget::BudgetOutcome>,
    entry_cap_raw_bytes: Option<u64>,
    inline_bytes: u64,
) -> (u64, u64) {
    let (raw_bytes, externalized_bytes) = match budget_outcome {
        Some(outcome) => (outcome.raw_bytes, outcome.externalized_bytes),
        None => (inline_bytes, 0),
    };
    (entry_cap_raw_bytes.unwrap_or(raw_bytes), externalized_bytes)
}

/// Smallest string worth a trim pass; below this the truncation marker costs
/// more than the pass saves.
const ENTRY_CAP_TRIM_MIN_BYTES: usize = 64;
/// Bound on trim passes before the metadata-only fallback replaces the
/// response; halving the largest payload converges in a handful of passes,
/// so this only guards pathological many-small-field shapes.
const ENTRY_CAP_TRIM_PASSES: usize = 64;

/// Bound a result the store rejected as oversized until its serialized form
/// fits the entry cap: each pass halves the largest string payload (keeping
/// a quarter of each end around a truncation marker). Numbers, statuses and
/// small fields survive untouched, and the reduced result is then
/// externalized like any other, so nothing above the cap ever travels
/// inline. Lossless retention above the cap was never possible — the store
/// rejects such entries outright.
fn reduce_result_to_entry_cap(result: &mut Value, cap_bytes: u64) {
    for _ in 0..ENTRY_CAP_TRIM_PASSES {
        let serialized = serde_json::to_vec(result).map_or(0, |bytes| bytes.len());
        if serialized as u64 <= cap_bytes {
            return;
        }
        let mut largest: Option<(String, usize)> = None;
        largest_string_path(result, String::new(), &mut largest);
        let Some((path, _)) = largest.filter(|(_, bytes)| *bytes >= ENTRY_CAP_TRIM_MIN_BYTES)
        else {
            break;
        };
        let Some(text) = result
            .pointer_mut(&path)
            .and_then(|leaf| leaf.as_str())
            .map(str::to_string)
        else {
            break;
        };
        if let Some(leaf) = result.pointer_mut(&path) {
            *leaf = Value::String(trim_middle_with_marker(&text));
        }
    }
    // Still oversized after the pass budget: send a bounded metadata-only
    // response instead of an over-cap payload.
    let tool_name = result
        .pointer("/structuredContent/toolName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let was_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    *result = json!({
        "content": [],
        "structuredContent": {
            "toolName": tool_name,
            "success": false,
            "responseExceedsEntryCap": true,
        },
        "isError": was_error,
    });
}

/// Entry-cap disclosure for a reduced result that could not be externalized
/// (store unavailable after the retry): no manifest exists on this path, so
/// the truncation metadata is attached to the result directly and survives
/// as-is — nothing compacts the response afterwards.
fn attach_entry_cap_disclosure(result: &mut Value, reduction: response_budget::EntryCapReduction) {
    let current_bytes = serde_json::to_vec(result).map_or(0, |bytes| bytes.len() as u64);
    let Some(object) = result.as_object_mut() else {
        return;
    };
    object.insert("entryCapTruncated".to_string(), Value::Bool(true));
    object.insert(
        "entryCapOriginalBytes".to_string(),
        json!(reduction.original_bytes),
    );
    object.insert(
        "entryCapOmittedBytes".to_string(),
        json!(reduction.original_bytes.saturating_sub(current_bytes)),
    );
}

/// Deepest JSON-pointer path of the largest string leaf, first on ties.
fn largest_string_path(value: &Value, base: String, largest: &mut Option<(String, usize)>) {
    match value {
        Value::String(text) => {
            if largest
                .as_ref()
                .is_none_or(|(_, bytes)| text.len() > *bytes)
            {
                *largest = Some((base, text.len()));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                largest_string_path(item, pointer_child(&base, &index.to_string()), largest);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                largest_string_path(child, pointer_child(&base, key), largest);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn pointer_child(base: &str, token: &str) -> String {
    let escaped = token.replace('~', "~0").replace('/', "~1");
    if base.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{base}/{escaped}")
    }
}

/// Keep a quarter of each end around a truncation marker; boundaries stay on
/// char edges so the surviving text is valid UTF-8.
fn trim_middle_with_marker(text: &str) -> String {
    let keep = text.len() / 4;
    let head_end = text.floor_char_boundary(keep);
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(keep));
    let omitted = tail_start.saturating_sub(head_end);
    let mut trimmed = String::with_capacity(head_end + text.len() - tail_start + 96);
    trimmed.push_str(&text[..head_end]);
    trimmed.push_str(&format!(
        "\n… <truncated {omitted} bytes; response exceeded the entry cap> …\n"
    ));
    trimmed.push_str(&text[tail_start..]);
    trimmed
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
