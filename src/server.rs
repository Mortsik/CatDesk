use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Form, Path, State},
    http::{HeaderMap, Response, StatusCode, header},
    response::Json,
    routing::{delete, get, post},
};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::build_info;
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration as StdDuration;
use tokio::sync::{Mutex, mpsc::Sender};

use crate::command;
use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::mcp::{self, JsonRpcRequest, WIDGET_PAYLOAD_META_KEY};
use crate::project_scope;
use crate::request_workers::{RequestClass, run_timed};
use crate::session_context::{ProjectStateChange, SessionContextStore};
use crate::state::{
    AgentsPathMode, FlowBootstrapWidget, FlowDirection, ServerUiEvent, SharedState, ShowDetailMode,
    TokenStatsLayout, UsageTotals, parse_seed_hex, save_agents_path_mode, save_show_detail_mode,
    save_token_stats_layout,
};
use crate::usage_pricing;

const STATELESS_FLOW_ID: &str = "stateless";
const MCP_HTTP_REQUEST_MAX_DURATION: StdDuration = StdDuration::from_secs(120);
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const OPENAI_SESSION_META_KEY: &str = "openai/session";
#[derive(Clone, Debug)]
struct ClientSession {
    id: Option<String>,
    flow_id: String,
}

impl ClientSession {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self::from_request(headers, None)
    }

    fn from_request(headers: &HeaderMap, body: Option<&Value>) -> Self {
        let header_id = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 512)
            .map(str::to_string);
        if let Some(id) = header_id {
            let flow_id = Self::hashed_flow_id(&id);
            return Self {
                id: Some(id),
                flow_id,
            };
        }

        let openai_session = body
            .and_then(|body| body.get("params"))
            .and_then(|params| params.get("_meta"))
            .and_then(|meta| meta.get(OPENAI_SESSION_META_KEY))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 512);
        if let Some(session_id) = openai_session {
            let mut hasher = DefaultHasher::new();
            OPENAI_SESSION_META_KEY.hash(&mut hasher);
            session_id.hash(&mut hasher);
            let digest = hasher.finish();
            return Self {
                id: Some(format!("openai:{digest:016x}")),
                flow_id: format!("session:{digest:016x}"),
            };
        }

        Self {
            id: None,
            flow_id: STATELESS_FLOW_ID.to_string(),
        }
    }

    fn hashed_flow_id(session_id: &str) -> String {
        let mut hasher = DefaultHasher::new();
        session_id.hash(&mut hasher);
        format!("session:{:016x}", hasher.finish())
    }

    fn namespace(&self) -> Option<&str> {
        self.id.as_deref()
    }
}

#[derive(Clone)]
struct InstructionGate {
    contexts: SessionContextStore,
    remote_connected_desired: Arc<AtomicBool>,
    remote_connected_update_pending: Arc<AtomicBool>,
}

impl InstructionGate {
    fn with_anonymous(called: bool) -> Self {
        Self {
            contexts: SessionContextStore::with_anonymous(called),
            remote_connected_desired: Arc::new(AtomicBool::new(false)),
            remote_connected_update_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    fn note_remote_connected(&self, connected: bool) {
        self.remote_connected_desired
            .store(connected, Ordering::Release);
    }

    fn set_remote_connected_nonblocking(&self, app: SharedState, connected: bool) {
        self.note_remote_connected(connected);
        if let Ok(mut state) = app.try_lock() {
            state.set_remote_connected(connected);
            return;
        }
        if self
            .remote_connected_update_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let desired = self.remote_connected_desired.clone();
        let pending = self.remote_connected_update_pending.clone();
        tokio::spawn(async move {
            loop {
                let connected = desired.load(Ordering::Acquire);
                app.lock().await.set_remote_connected(connected);
                pending.store(false, Ordering::Release);
                if desired.load(Ordering::Acquire) == connected
                    || pending
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    break;
                }
            }
        });
    }

    fn is_called(&self, session: &ClientSession) -> bool {
        self.contexts.instruction_called(session.namespace())
    }

    fn mark_called(&self, session: &ClientSession) {
        self.contexts.mark_instruction_called(session.namespace());
    }

    fn forget(&self, session: &ClientSession) {
        self.contexts.forget(session.namespace());
    }

    fn has_named_sessions(&self) -> bool {
        self.contexts.has_named_sessions()
    }

    fn active_project(&self, session: &ClientSession) -> Option<std::path::PathBuf> {
        self.contexts.active_project(session.namespace())
    }

    fn set_active_project(
        &self,
        session: &ClientSession,
        project: std::path::PathBuf,
    ) -> ProjectStateChange {
        self.contexts.set_active_project(session.namespace(), project)
    }

    fn clear_active_project(&self, session: &ClientSession) -> bool {
        self.contexts.clear_active_project(session.namespace())
    }

    #[cfg(test)]
    fn contains_named_session(&self, session_id: &str) -> bool {
        self.contexts.contains_named_session(session_id)
    }
}

#[derive(Clone)]
struct ServerState {
    app: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    ui_events: Sender<ServerUiEvent>,
    catdesk_instruction_called: InstructionGate,
}

/// Build the axum router.
pub fn router(
    app_state: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    mcp_path: String,
    ui_events: Sender<ServerUiEvent>,
) -> Router {
    let state = ServerState {
        app: app_state,
        devtools,
        command_jobs,
        ui_events,
        catdesk_instruction_called: InstructionGate::with_anonymous(false),
    };
    let secret_prefix = mcp_path
        .strip_suffix("/mcp")
        .expect("MCP path must end with /mcp");
    let health_path = secret_prefix.to_string();
    let archive_save_path = format!("{secret_prefix}/binagotchy/archive/{{folder}}/save");
    let partner_path = format!("{secret_prefix}/binagotchy/partner");
    let agents_path_mode = format!("{secret_prefix}/agents/path-mode");
    let agents_path_state = format!("{secret_prefix}/agents/path-state");
    let token_stats_layout = format!("{secret_prefix}/layout/token-stats");
    let show_detail_mode = format!("{secret_prefix}/layout/show-detail");

    Router::new()
        .route(&health_path, get(health))
        .route(
            &archive_save_path,
            post(post_save_binagotchy_folder).options(options_binagotchy_archive_save),
        )
        .route(
            &partner_path,
            post(post_binagotchy_partner)
                .delete(delete_binagotchy_partner)
                .options(options_binagotchy_partner),
        )
        .route(
            &agents_path_mode,
            post(post_agents_path_mode).options(options_agents_path_mode),
        )
        .route(
            &agents_path_state,
            get(get_agents_path_state).options(options_agents_path_state),
        )
        .route(
            &token_stats_layout,
            post(post_token_stats_layout).options(options_token_stats_layout),
        )
        .route(
            &show_detail_mode,
            post(post_show_detail_mode).options(options_show_detail_mode),
        )
        .route(&mcp_path, post(post_mcp_http))
        .route(&mcp_path, get(get_mcp))
        .route(&mcp_path, delete(delete_mcp))
        .route_layer(axum::middleware::from_fn_with_state(
            crate::diagnostics::global(),
            crate::diagnostics::http_request,
        ))
        .with_state(state)
}

fn with_widget_action_cors(
    mut builder: axum::http::response::Builder,
) -> axum::http::response::Builder {
    builder = builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    builder = builder.header(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, DELETE, OPTIONS",
    );
    builder = builder.header(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "content-type, ngrok-skip-browser-warning, mcp-session-id",
    );
    builder = builder.header(header::CACHE_CONTROL, "no-store");
    builder
}

fn jsonrpc_error_response(status: StatusCode, code: i64, msg: &str) -> Response<Body> {
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "error": {"code": code, "message": msg}
    }))
    .unwrap();
    Response::builder()
        .status(status)
        .extension(crate::diagnostics::RpcError(code))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn modern_jsonrpc_error_response(
    status: StatusCode,
    id: Option<&Value>,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> Response<Body> {
    let mut error = json!({ "code": code, "message": message });
    if let (Some(error_obj), Some(data)) = (error.as_object_mut(), data) {
        error_obj.insert("data".to_string(), data);
    }
    let body = json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "error": error,
    });
    Response::builder()
        .status(status)
        .extension(crate::diagnostics::RpcError(code))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn decode_mcp_name_header(value: &str) -> Result<String, String> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    else {
        return Ok(value.to_string());
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid base64 Mcp-Name header: {error}"))?;
    String::from_utf8(decoded).map_err(|error| format!("invalid UTF-8 Mcp-Name header: {error}"))
}

fn modern_request_name(body: &Value) -> Result<Option<&str>, &'static str> {
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = body.get("params").and_then(Value::as_object);
    match method {
        "tools/call" | "prompts/get" => params
            .and_then(|params| params.get("name"))
            .and_then(Value::as_str)
            .map(Some)
            .ok_or("request params.name must be a string"),
        "resources/read" => params
            .and_then(|params| params.get("uri"))
            .and_then(Value::as_str)
            .map(Some)
            .ok_or("request params.uri must be a string"),
        _ => Ok(None),
    }
}

fn validate_modern_request(body: &Value, headers: &HeaderMap) -> Result<(), Response<Body>> {
    let id = body.get("id");
    let params = body
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Invalid params: modern MCP requests require params._meta",
                None,
            )
        })?;
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Invalid params: modern MCP requests require params._meta",
                None,
            )
        })?;
    let requested_version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Invalid params: missing io.modelcontextprotocol/protocolVersion",
                None,
            )
        })?;
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .is_some_and(Value::is_object)
    {
        return Err(modern_jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            id,
            -32602,
            "Invalid params: io.modelcontextprotocol/clientCapabilities must be an object",
            None,
        ));
    }
    if let Some(client_info) = meta.get("io.modelcontextprotocol/clientInfo") {
        let valid = client_info.as_object().is_some_and(|client_info| {
            client_info.get("name").is_some_and(Value::is_string)
                && client_info.get("version").is_some_and(Value::is_string)
        });
        if !valid {
            return Err(modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32602,
                "Invalid params: io.modelcontextprotocol/clientInfo must contain string name and version",
                None,
            ));
        }
    }

    let header_version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());
    if header_version != Some(requested_version) {
        return Err(modern_jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "HeaderMismatch: MCP-Protocol-Version must match request _meta protocolVersion",
            Some(json!({
                "header": "MCP-Protocol-Version",
                "expected": requested_version,
                "actual": header_version,
            })),
        ));
    }
    if requested_version != mcp::MODERN_MCP_PROTOCOL_VERSION {
        return Err(modern_jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            id,
            -32022,
            "UnsupportedProtocolVersionError",
            Some(json!({
                "supported": [mcp::MODERN_MCP_PROTOCOL_VERSION],
                "requested": requested_version,
            })),
        ));
    }

    let method = body.get("method").and_then(Value::as_str).ok_or_else(|| {
        modern_jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            id,
            -32600,
            "Invalid request: method must be a string",
            None,
        )
    })?;
    let method_header = headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok());
    if method_header != Some(method) {
        return Err(modern_jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            id,
            -32020,
            "HeaderMismatch: Mcp-Method must match the JSON-RPC method",
            Some(json!({
                "header": "Mcp-Method",
                "expected": method,
                "actual": method_header,
            })),
        ));
    }

    let expected_name = modern_request_name(body).map_err(|message| {
        modern_jsonrpc_error_response(StatusCode::BAD_REQUEST, id, -32602, message, None)
    })?;
    if let Some(expected_name) = expected_name {
        let actual_name = headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                modern_jsonrpc_error_response(
                    StatusCode::BAD_REQUEST,
                    id,
                    -32020,
                    "HeaderMismatch: Mcp-Name is required for this method",
                    Some(json!({
                        "header": "Mcp-Name",
                        "expected": expected_name,
                        "actual": Value::Null,
                    })),
                )
            })?;
        let actual_name = decode_mcp_name_header(actual_name).map_err(|message| {
            modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32020,
                &format!("HeaderMismatch: {message}"),
                None,
            )
        })?;
        if actual_name != expected_name {
            return Err(modern_jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                id,
                -32020,
                "HeaderMismatch: Mcp-Name must match the request name or URI",
                Some(json!({
                    "header": "Mcp-Name",
                    "expected": expected_name,
                    "actual": actual_name,
                })),
            ));
        }
    }

    Ok(())
}

fn request_id(req: &Value) -> String {
    req.get("id").map_or("-".into(), |v| match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    })
}

fn request_tool_name(req: &Value) -> Option<String> {
    req.get("params")
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn request_tool_arguments(req: &Value) -> Option<&serde_json::Map<String, Value>> {
    req.get("params")
        .and_then(|v| v.get("arguments"))
        .and_then(Value::as_object)
}

fn request_class(req: &Value) -> RequestClass {
    let method = req.get("method").and_then(Value::as_str).unwrap_or_default();
    match method {
        "server/discover" | "tools/list" | "resources/list" | "resources/read" => {
            RequestClass::Control
        }
        "tools/call" => match request_tool_name(req).as_deref() {
            Some("catdesk_instruction" | "poll_command" | "cancel_command") => {
                RequestClass::Control
            }
            Some("run_command" | "start_command") => RequestClass::Process,
            Some(
                "read" | "read_image" | "search" | "write" | "edit" | "create_handoff"
                | "delete",
            ) => RequestClass::Filesystem,
            // Any tool not owned by CatDesk itself is a dynamically discovered
            // DevTools operation when browser mode is enabled. Isolate it from
            // local filesystem/process capacity even if the browser is slow.
            Some(_) => RequestClass::Browser,
            None => RequestClass::General,
        },
        _ => RequestClass::General,
    }
}

fn request_deadline(class: RequestClass) -> StdDuration {
    match class {
        // poll_command may legitimately wait up to 30 seconds. Leave headroom
        // without allowing a stalled control call to run unbounded.
        RequestClass::Control => StdDuration::from_secs(45),
        RequestClass::Filesystem | RequestClass::Process | RequestClass::Browser => {
            MCP_HTTP_REQUEST_MAX_DURATION
        }
        RequestClass::General => StdDuration::from_secs(60),
    }
}

fn request_failure_event(failure: &crate::request_workers::RequestFailure) -> &'static str {
    use crate::request_workers::RequestFailure;
    match failure {
        RequestFailure::Deadline => "request_worker_timeout",
        RequestFailure::Failed => "request_worker_failed",
    }
}

fn flow_file_name(path: &str) -> Option<String> {
    let trimmed = path.trim();
    let trimmed = trimmed.trim_end_matches(|c| c == '/' || c == '\\');
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn flow_argument_summary(tool: &str, arguments: &serde_json::Map<String, Value>) -> Option<String> {
    if tool == "read" {
        let paths = arguments.get("paths")?.as_array()?;
        let first = flow_file_name(paths.first()?.as_str()?)?;
        return Some(match paths.len() {
            1 => first,
            count => format!("{first} +{}", count - 1),
        });
    }
    let (key, file_name_only) = match tool {
        "run_command" | "start_command" => ("command", false),
        "poll_command" | "cancel_command" => ("job_id", false),
        "write" | "edit" | "delete" => ("path", true),
        "search" => ("pattern", false),
        _ => return None,
    };
    let value = arguments.get(key)?.as_str()?;
    if file_name_only {
        return flow_file_name(value);
    }
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!compact.is_empty()).then_some(compact)
}

fn request_resource_uri(req: &Value) -> Option<&str> {
    req.get("params")
        .and_then(|v| v.get("uri"))
        .and_then(Value::as_str)
}

fn query_param_value<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
    let query = uri.split_once('?')?.1;
    query.split('&').find_map(|part| {
        let (param_key, param_value) = part.split_once('=')?;
        if param_key == key {
            Some(param_value)
        } else {
            None
        }
    })
}

fn resource_read_flow_label(req: &Value) -> String {
    let Some(uri) = request_resource_uri(req) else {
        return "resources/read:?".to_string();
    };
    if let Some(tool_name) = query_param_value(uri, "toolName").filter(|value| !value.is_empty()) {
        return format!("resources/read:{tool_name}");
    }
    "resources/read:base".to_string()
}

fn bootstrap_widgets_from_tools_list_response(response: &Value) -> Vec<FlowBootstrapWidget> {
    let Some(tools) = response
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    tools
        .iter()
        .filter_map(|tool| {
            let tool_name = tool.get("name").and_then(Value::as_str)?;
            let uri = tool
                .get("_meta")
                .and_then(|meta| meta.get("openai/outputTemplate"))
                .and_then(Value::as_str)?;
            if !mcp::is_catdesk_widget_resource_uri(uri) {
                return None;
            }
            Some(FlowBootstrapWidget {
                uri: uri.to_string(),
                tool_name: tool_name.to_string(),
                label: if tool_name == "catdesk_instruction" {
                    "instruction".to_string()
                } else {
                    tool_name.to_string()
                },
            })
        })
        .collect()
}

fn request_flow_label(req: &Value) -> String {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-method>");
    if method == "tools/call" {
        let tool = request_tool_name(req).unwrap_or_else(|| "?".into());
        if let Some(summary) = request_tool_arguments(req)
            .and_then(|arguments| flow_argument_summary(&tool, arguments))
        {
            return format!("tools/call:{tool} › {summary}");
        }
        return format!("tools/call:{tool}");
    }
    if method == "resources/read" {
        return resource_read_flow_label(req);
    }
    method.to_string()
}

fn truncate_log_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn summarize_tool_arguments(arguments: &serde_json::Map<String, Value>) -> String {
    let mut keys = arguments.keys().collect::<Vec<_>>();
    keys.sort();
    keys.into_iter()
        .map(|key| {
            let value = &arguments[key];
            match (key.as_str(), value) {
                (
                    "content" | "old_string" | "new_string" | "old_text" | "new_text",
                    Value::String(text),
                ) => format!("{key}=<{} chars>", text.chars().count()),
                ("edits", Value::Array(items)) => format!("edits=<{} items>", items.len()),
                (_, Value::String(text)) => {
                    let encoded = serde_json::to_string(text).unwrap_or_else(|_| "\"?\"".into());
                    format!("{key}={}", truncate_log_text(&encoded, 240))
                }
                (_, Value::Array(items)) => format!("{key}=<{} items>", items.len()),
                (_, Value::Object(object)) => format!("{key}=<{} fields>", object.len()),
                _ => format!("{key}={value}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn summarize_request(req: &Value) -> String {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-method>");
    let id = request_id(req);
    match method {
        "server/discover" => {
            let meta = req.get("params").and_then(|params| params.get("_meta"));
            let protocol = meta
                .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
                .and_then(Value::as_str);
            let client_name = meta
                .and_then(|meta| meta.get("io.modelcontextprotocol/clientInfo"))
                .and_then(|client| client.get("name"))
                .and_then(Value::as_str);
            let client_version = meta
                .and_then(|meta| meta.get("io.modelcontextprotocol/clientInfo"))
                .and_then(|client| client.get("version"))
                .and_then(Value::as_str);
            let mut summary = format!("server/discover id={id}");
            if let Some(protocol) = protocol {
                summary.push_str(&format!(" protocol={protocol}"));
            }
            if let Some(client_name) = client_name {
                summary.push_str(&format!(" client={client_name}"));
                if let Some(client_version) = client_version {
                    summary.push('/');
                    summary.push_str(client_version);
                }
            }
            summary
        }
        "resources/read" => {
            let uri = request_resource_uri(req).unwrap_or("?");
            let tool = query_param_value(uri, "toolName").filter(|value| !value.is_empty());
            match tool {
                Some(tool) => format!("resources/read id={id} tool={tool} uri={uri}"),
                None => format!("resources/read id={id} uri={uri}"),
            }
        }
        "tools/call" => {
            let tool = request_tool_name(req).unwrap_or_else(|| "?".into());
            let arguments = request_tool_arguments(req)
                .map(summarize_tool_arguments)
                .unwrap_or_default();
            if arguments.is_empty() {
                format!("tools/call id={id} tool={tool}")
            } else {
                format!("tools/call id={id} tool={tool} args[{arguments}]")
            }
        }
        _ if id == "-" => method.to_string(),
        _ => format!("{method} id={id}"),
    }
}

fn response_id(resp: &Value) -> String {
    resp.get("id").map_or("-".into(), |v| match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    })
}

fn summarize_response(req: &Value, resp: &Value) -> String {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-method>");
    let id = response_id(resp);
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error");
        let context = match method {
            "tools/call" => request_tool_name(req)
                .map(|tool| format!(" tool={tool}"))
                .unwrap_or_default(),
            "resources/read" => request_resource_uri(req)
                .map(|uri| {
                    let tool = query_param_value(uri, "toolName")
                        .filter(|value| !value.is_empty())
                        .map(|tool| format!(" tool={tool}"))
                        .unwrap_or_default();
                    format!("{tool} uri={uri}")
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        return format!(
            "{method} id={id}{context} error={code} message={}",
            truncate_log_text(msg, 240)
        );
    }
    let Some(result) = resp.get("result") else {
        return format!("{method} id={id} unknown");
    };
    match method {
        "server/discover" => {
            let versions = result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .map(|versions| {
                    versions
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let mut capabilities = result
                .get("capabilities")
                .and_then(Value::as_object)
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            capabilities.sort();
            format!(
                "server/discover id={id} ok versions={} capabilities={}",
                if versions.is_empty() { "-" } else { &versions },
                if capabilities.is_empty() {
                    "-".to_string()
                } else {
                    capabilities.join(",")
                }
            )
        }
        "tools/list" => {
            let count = result
                .get("tools")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("tools/list id={id} ok tools={count}")
        }
        "resources/list" => {
            let count = result
                .get("resources")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("resources/list id={id} ok resources={count}")
        }
        "resources/read" => {
            let uri = request_resource_uri(req).unwrap_or("?");
            let tool = query_param_value(uri, "toolName").filter(|value| !value.is_empty());
            let contents = result
                .get("contents")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let chars = result
                .get("contents")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .map(|text| text.chars().count())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            match tool {
                Some(tool) => format!(
                    "resources/read id={id} ok tool={tool} contents={contents} textChars={chars}"
                ),
                None => format!("resources/read id={id} ok contents={contents} textChars={chars}"),
            }
        }
        "tools/call" => {
            let tool = request_tool_name(req).unwrap_or_else(|| "?".into());
            let structured = result.get("structuredContent");
            let success = structured
                .and_then(|value| value.get("success"))
                .and_then(Value::as_bool);
            let error_code = structured
                .and_then(|value| value.get("errorCode"))
                .and_then(Value::as_str);
            let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
            let mut fields = structured
                .and_then(Value::as_object)
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            fields.sort();
            let mut summary = format!("tools/call id={id} tool={tool}");
            if is_error {
                summary.push_str(" toolError=true");
            }
            if let Some(success) = success {
                summary.push_str(&format!(" success={success}"));
            }
            if let Some(error_code) = error_code {
                summary.push_str(&format!(" errorCode={error_code}"));
            }
            if !fields.is_empty() {
                summary.push_str(&format!(" fields={}", fields.join(",")));
            }
            summary
        }
        _ => format!("{method} id={id} ok"),
    }
}

fn extract_turn_token_usage(result: Option<&Value>) -> Option<(u64, u64)> {
    let usage = result
        .and_then(|value| value.get("_meta"))
        .and_then(Value::as_object)
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .and_then(Value::as_object)
        .and_then(|payload| payload.get("turnTokenUsage"))?;
    let tool_input_tokens = usage.get("inputTokens").and_then(Value::as_u64)?;
    let tool_output_tokens = usage.get("outputTokens").and_then(Value::as_u64)?;
    Some((tool_input_tokens, tool_output_tokens))
}

fn turn_token_usage_for_response(
    req: &JsonRpcRequest,
    result: Option<&Value>,
) -> Option<(u64, u64)> {
    let result = result?;
    Some(
        extract_turn_token_usage(Some(result))
            .unwrap_or_else(|| mcp::estimate_turn_token_counts(req, result)),
    )
}

fn attach_history_usage(result: &mut Option<Value>, usage_totals: &UsageTotals) {
    let Some(result_obj) = result.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    let history_usage = json!({
        "inputTokens": usage_totals.tool_input_tokens,
        "outputTokens": usage_totals.tool_output_tokens,
        "totalTokens": usage_totals.total_tokens,
        // Direction markers: input = tool-call arguments the client sent
        // (request), output = tool results returned to the client (response).
        "inputRole": "request",
        "outputRole": "response",
    });
    let history_tool_call_count = json!(usage_totals.tool_call_count);
    if let Some(widget_payload) = result_obj
        .get_mut("_meta")
        .and_then(Value::as_object_mut)
        .and_then(|meta| meta.get_mut(WIDGET_PAYLOAD_META_KEY))
        .and_then(Value::as_object_mut)
    {
        widget_payload.insert("historyTurnTokenUsage".to_string(), history_usage);
        widget_payload.insert("historyToolCallCount".to_string(), history_tool_call_count);
    }
}

// ── GET /<slug> — health ───────────────────────────────────

async fn health(State(s): State<ServerState>) -> Json<Value> {
    let Ok(app) = s.app.try_lock() else {
        return Json(json!({
            "status": "ok",
            "name": "CatDesk",
            "description": "MCP Tools for ChatGPT to control your computer and browser",
            "busy": true,
        }));
    };
    Json(json!({
        "status": "ok",
        "name": "CatDesk",
        "description": "MCP Tools for ChatGPT to control your computer and browser",
        "mode": app.mode.label(),
        "tool_mode": app.tool_mode.label(),
        "busy": false,
        "workspace": app.workspace_root,
        "version": build_info::version_identity(build_info::VERSION, build_info::GIT_SHA),
        "git_sha": build_info::GIT_SHA,
        "git_branch": build_info::GIT_BRANCH,
        "build_time": build_info::BUILD_TIMESTAMP,
    }))
}

fn attach_catdesk_instruction_actions(
    result: &mut Option<Value>,
    public_base_url: Option<&str>,
    mcp_path: &str,
    mascot_seed: u64,
    partner_binagotchy_seed: Option<&str>,
) {
    let Some(result_obj) = result.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    let Some(structured) = result_obj
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(tool_name) = structured.get("toolName").and_then(Value::as_str) else {
        return;
    };
    if tool_name != "catdesk_instruction" {
        return;
    }

    let Some(widget_payload) = result_obj
        .get_mut("_meta")
        .and_then(Value::as_object_mut)
        .and_then(|meta| meta.get_mut(WIDGET_PAYLOAD_META_KEY))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    let public_action_base_url = public_base_url
        .zip(mcp_path.strip_suffix("/mcp"))
        .map(|(base, secret_prefix)| format!("{base}{secret_prefix}"));
    let binagotchy_action_base_url = public_action_base_url
        .as_deref()
        .map(|base| format!("{base}/binagotchy"));
    widget_payload.insert(
        "binagotchyApiBaseUrl".to_string(),
        json!(binagotchy_action_base_url.clone().unwrap_or_default()),
    );
    widget_payload.insert(
        "agentsPathModeUrl".to_string(),
        json!(
            public_action_base_url
                .as_deref()
                .map(|base| format!("{base}/agents/path-mode"))
                .unwrap_or_default()
        ),
    );
    widget_payload.insert(
        "agentsPathStateUrl".to_string(),
        json!(
            public_action_base_url
                .as_deref()
                .map(|base| format!("{base}/agents/path-state"))
                .unwrap_or_default()
        ),
    );
    widget_payload.insert(
        "tokenStatsLayoutUrl".to_string(),
        json!(
            public_action_base_url
                .as_deref()
                .map(|base| format!("{base}/layout/token-stats"))
                .unwrap_or_default()
        ),
    );
    widget_payload.insert(
        "showDetailModeUrl".to_string(),
        json!(
            public_action_base_url
                .as_deref()
                .map(|base| format!("{base}/layout/show-detail"))
                .unwrap_or_default()
        ),
    );
    widget_payload.insert(
        "partnerBinagotchySeed".to_string(),
        json!(partner_binagotchy_seed.unwrap_or("")),
    );
    widget_payload.insert(
        "widgetMascot".to_string(),
        json!(crate::mascot::build_widget_mascot(mascot_seed)),
    );

    if let Some(cards) = widget_payload
        .get_mut("binagotchyCards")
        .and_then(Value::as_array_mut)
    {
        for card in cards.iter_mut() {
            let Some(card_obj) = card.as_object_mut() else {
                continue;
            };
            let Some(folder) = card_obj
                .get("folder")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            let is_partner = partner_binagotchy_seed
                .zip(card_obj.get("seed").and_then(Value::as_str))
                .is_some_and(|(partner_seed, card_seed)| partner_seed == card_seed);
            card_obj.insert("isPartner".to_string(), json!(is_partner));
            if let Some(base) = binagotchy_action_base_url.as_deref() {
                card_obj.insert(
                    "saveFolderUrl".to_string(),
                    json!(format!("{base}/archive/{folder}/save")),
                );
                card_obj.insert(
                    "setPartnerUrl".to_string(),
                    json!(format!("{base}/partner")),
                );
            }
        }
    }
}

async fn post_save_binagotchy_folder(
    Path(folder): Path<String>,
    State(_s): State<ServerState>,
) -> Response<Body> {
    match crate::mascot::save_archived_binagotchy_folder(&folder) {
        Ok(saved_path) => with_widget_action_cors(Response::builder())
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "ok": true,
                    "folder": folder,
                    "savedPath": saved_path.to_string_lossy(),
                })
                .to_string(),
            ))
            .unwrap(),
        Err(error) => with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap(),
    }
}

async fn options_binagotchy_archive_save(
    Path(_folder): Path<String>,
    State(_s): State<ServerState>,
) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

async fn options_binagotchy_partner(State(_s): State<ServerState>) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

fn parse_agents_path_mode(value: &str) -> Option<AgentsPathMode> {
    match value.trim() {
        "default" => Some(AgentsPathMode::Default),
        "workspace" => Some(AgentsPathMode::Workspace),
        "catdesk" => Some(AgentsPathMode::Catdesk),
        "codex" => Some(AgentsPathMode::Codex),
        "disabled" => Some(AgentsPathMode::Disabled),
        _ => None,
    }
}

fn parse_token_stats_layout(value: &str) -> Option<TokenStatsLayout> {
    match value.trim() {
        "disable" => Some(TokenStatsLayout::Disable),
        "right" => Some(TokenStatsLayout::Right),
        "bottom" => Some(TokenStatsLayout::Bottom),
        _ => None,
    }
}

fn parse_show_detail_mode(value: &str) -> Option<ShowDetailMode> {
    match value.trim() {
        "disable" => Some(ShowDetailMode::Disable),
        "expanded" => Some(ShowDetailMode::Expanded),
        "collapsed" => Some(ShowDetailMode::Collapsed),
        _ => None,
    }
}

async fn post_agents_path_mode(
    State(s): State<ServerState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let Some(mode_raw) = form.get("mode").map(String::as_str) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "missing mode" }).to_string(),
            ))
            .unwrap();
    };
    let Some(mode) = parse_agents_path_mode(mode_raw) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "invalid mode" }).to_string(),
            ))
            .unwrap();
    };

    let workspace_root = {
        let app = s.app.lock().await;
        app.workspace_root.clone()
    };

    if let Err(error) = save_agents_path_mode(mode) {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    agents_state_response(&workspace_root)
}

async fn post_token_stats_layout(
    State(_s): State<ServerState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let Some(layout_raw) = form.get("layout").map(String::as_str) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "missing layout" }).to_string(),
            ))
            .unwrap();
    };
    let Some(layout) = parse_token_stats_layout(layout_raw) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "invalid layout" }).to_string(),
            ))
            .unwrap();
    };

    if let Err(error) = save_token_stats_layout(layout) {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    with_widget_action_cors(Response::builder())
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "ok": true,
                "tokenStatsLayout": layout.as_str(),
            })
            .to_string(),
        ))
        .unwrap()
}

async fn post_show_detail_mode(
    State(s): State<ServerState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let Some(mode_raw) = form.get("mode").map(String::as_str) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "missing mode" }).to_string(),
            ))
            .unwrap();
    };
    let Some(mode) = parse_show_detail_mode(mode_raw) else {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "invalid mode" }).to_string(),
            ))
            .unwrap();
    };

    if let Err(error) = save_show_detail_mode(mode) {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    sync_show_detail_mode_state(&s, mode).await;

    with_widget_action_cors(Response::builder())
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "ok": true,
                "showDetailMode": mode.as_str(),
            })
            .to_string(),
        ))
        .unwrap()
}

async fn sync_show_detail_mode_state(s: &ServerState, mode: ShowDetailMode) {
    let mut app = s.app.lock().await;
    app.show_detail_mode = mode;
}

async fn options_agents_path_mode(State(_s): State<ServerState>) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

async fn options_token_stats_layout(State(_s): State<ServerState>) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

async fn options_show_detail_mode(State(_s): State<ServerState>) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

async fn get_agents_path_state(State(s): State<ServerState>) -> Response<Body> {
    let workspace_root = {
        let app = s.app.lock().await;
        app.workspace_root.clone()
    };
    agents_state_response(&workspace_root)
}

async fn options_agents_path_state(State(_s): State<ServerState>) -> Response<Body> {
    with_widget_action_cors(Response::builder())
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

fn agents_state_response(workspace_root: &str) -> Response<Body> {
    let agents_state = match mcp::agents_widget_state_payload(workspace_root) {
        Ok(value) => value,
        Err(error) => {
            return with_widget_action_cors(Response::builder())
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "ok": false, "error": error.to_string() }).to_string(),
                ))
                .unwrap();
        }
    };

    let mut payload = json!({ "ok": true });
    if let (Some(payload_obj), Some(agents_obj)) =
        (payload.as_object_mut(), agents_state.as_object())
    {
        for (key, value) in agents_obj {
            payload_obj.insert(key.clone(), value.clone());
        }
    }

    with_widget_action_cors(Response::builder())
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

async fn post_binagotchy_partner(
    State(s): State<ServerState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let Some(seed) = form
        .get("seed")
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": "missing seed" }).to_string(),
            ))
            .unwrap();
    };

    let parsed_seed = match parse_seed_hex(&seed) {
        Ok(value) => value,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "ok": false, "error": error.to_string() }).to_string(),
                ))
                .unwrap();
        }
    };

    let mut app = s.app.lock().await;
    app.partner_binagotchy_seed = Some(seed.clone());
    app.mascot_seed = parsed_seed;
    app.mascot = crate::mascot::build_workspace_mascot(parsed_seed);
    let widget_mascot = crate::mascot::build_widget_mascot(parsed_seed);
    if let Err(error) = app.persist_state() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "ok": true,
                "seed": seed,
                "message": "partner updated",
                "widgetMascot": widget_mascot
            })
            .to_string(),
        ))
        .unwrap()
}

async fn delete_binagotchy_partner(State(s): State<ServerState>) -> Response<Body> {
    let random_seed = rand::random::<u64>();
    let random_seed_hex = format!("{random_seed:016x}");
    let mascot = crate::mascot::build_workspace_mascot(random_seed);
    let widget_mascot = crate::mascot::build_widget_mascot(random_seed);

    #[cfg(not(test))]
    if let Err(error) = crate::mascot::archive_startup_mascot(random_seed) {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    let mut app = s.app.lock().await;
    app.partner_binagotchy_seed = None;
    app.mascot_seed = random_seed;
    app.mascot = mascot;
    if let Err(error) = app.persist_state() {
        return with_widget_action_cors(Response::builder())
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "ok": false, "error": error.to_string() }).to_string(),
            ))
            .unwrap();
    }

    with_widget_action_cors(Response::builder())
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "ok": true,
                "seed": random_seed_hex,
                "message": "partner reset",
                "widgetMascot": widget_mascot
            })
            .to_string(),
        ))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Mode, ToolMode};
    use axum::body::to_bytes;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{Mutex, mpsc::channel};

    #[test]
    fn tool_flow_label_includes_selected_argument_summary() {
        let run_command = json!({
            "method": "tools/call",
            "params": {
                "name": "run_command",
                "arguments": { "command": "cargo   build\n--release" }
            }
        });
        assert_eq!(
            request_flow_label(&run_command),
            "tools/call:run_command › cargo build --release"
        );

        let read = json!({
            "method": "tools/call",
            "params": {
                "name": "read",
                "arguments": { "paths": ["src/widget/catdesk_dashboard.html"] }
            }
        });
        assert_eq!(
            request_flow_label(&read),
            "tools/call:read › catdesk_dashboard.html"
        );

        let edit = json!({
            "method": "tools/call",
            "params": {
                "name": "edit",
                "arguments": { "path": "src\\widget\\catdesk_dashboard.html" }
            }
        });
        assert_eq!(
            request_flow_label(&edit),
            "tools/call:edit › catdesk_dashboard.html"
        );

        let search = json!({
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": { "pattern": "FLOW_ANIM_CELLS" }
            }
        });
        assert_eq!(
            request_flow_label(&search),
            "tools/call:search › FLOW_ANIM_CELLS"
        );
    }

    #[test]
    fn request_classification_keeps_control_plane_separate_from_heavy_work() {
        use crate::request_workers::RequestClass;

        let cases = [
            (mcp_request_body("server/discover", json!({})), RequestClass::Control),
            (mcp_request_body("tools/list", json!({})), RequestClass::Control),
            (mcp_request_body("resources/list", json!({})), RequestClass::Control),
            (mcp_request_body("resources/read", json!({ "uri": "ui://widget/catdesk-dashboard.html" })), RequestClass::Control),
            (tool_call_body("catdesk_instruction", json!({})), RequestClass::Control),
            (tool_call_body("poll_command", json!({ "job_id": "x" })), RequestClass::Control),
            (tool_call_body("cancel_command", json!({ "job_id": "x" })), RequestClass::Control),
            (tool_call_body("read", json!({ "paths": ["x"] })), RequestClass::Filesystem),
            (tool_call_body("search", json!({ "pattern": "x" })), RequestClass::Filesystem),
            (tool_call_body("edit", json!({ "path": "x", "edits": [] })), RequestClass::Filesystem),
            (tool_call_body("run_command", json!({ "command": "true" })), RequestClass::Process),
            (tool_call_body("start_command", json!({ "command": "true" })), RequestClass::Process),
            (tool_call_body("take_screenshot", json!({})), RequestClass::Browser),
            (mcp_request_body("catdesk/unknown", json!({})), RequestClass::General),
        ];

        for (body, expected) in cases {
            let body: Value = serde_json::from_slice(&body).expect("parse test request");
            assert_eq!(request_class(&body), expected, "unexpected class for {body}");
        }
    }

    #[test]
    fn request_deadlines_never_exceed_mcp_http_ceiling() {
        let ceiling = StdDuration::from_secs(120);
        for class in [
            RequestClass::Control,
            RequestClass::Filesystem,
            RequestClass::Process,
            RequestClass::Browser,
            RequestClass::General,
        ] {
            assert!(
                request_deadline(class) <= ceiling,
                "{} request deadline {:?} exceeds the MCP HTTP ceiling {:?}",
                class.as_str(),
                request_deadline(class),
                ceiling
            );
        }
    }

    #[test]
    fn client_session_falls_back_to_openai_session_metadata() {
        fn body_with_openai_session(session_id: &str) -> Value {
            let bytes = tool_call_body("read", json!({ "paths": ["x"] }));
            let mut body: Value = serde_json::from_slice(&bytes).expect("parse tool call");
            body["params"]["_meta"]["openai/session"] = json!(session_id);
            body
        }

        let body_a = body_with_openai_session("chat-a");
        let body_b = body_with_openai_session("chat-b");
        let session_a = ClientSession::from_request(&HeaderMap::new(), Some(&body_a));
        let same_session_a = ClientSession::from_request(&HeaderMap::new(), Some(&body_a));
        let session_b = ClientSession::from_request(&HeaderMap::new(), Some(&body_b));

        assert_eq!(session_a.flow_id, same_session_a.flow_id);
        assert_ne!(session_a.flow_id, session_b.flow_id);
        assert_ne!(session_a.flow_id, STATELESS_FLOW_ID);
        assert!(!session_a.flow_id.contains("chat-a"));
        assert_ne!(session_a.namespace(), Some("chat-a"));
        assert_ne!(session_a.namespace(), session_b.namespace());

        let mut headers = HeaderMap::new();
        headers.insert(MCP_SESSION_ID_HEADER, "header-session".parse().unwrap());
        let header_session = ClientSession::from_request(&headers, Some(&body_a));
        assert_eq!(header_session.namespace(), Some("header-session"));
    }

    #[test]
    fn tool_flow_label_keeps_plain_name_for_unlisted_arguments() {
        let instruction = json!({
            "method": "tools/call",
            "params": {
                "name": "catdesk_instruction",
                "arguments": {}
            }
        });
        assert_eq!(
            request_flow_label(&instruction),
            "tools/call:catdesk_instruction"
        );
    }

    #[test]
    fn detailed_mcp_log_summaries_include_bootstrap_context() {
        let discover_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": mcp::MODERN_MCP_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "ChatGPT",
                        "version": "test"
                    }
                }
            }
        });
        let discover_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "supportedVersions": [mcp::MODERN_MCP_PROTOCOL_VERSION],
                "capabilities": {"tools": {}, "resources": {}}
            }
        });
        assert_eq!(
            summarize_request(&discover_request),
            format!(
                "server/discover id=1 protocol={} client=ChatGPT/test",
                mcp::MODERN_MCP_PROTOCOL_VERSION
            )
        );
        assert_eq!(
            summarize_response(&discover_request, &discover_response),
            format!(
                "server/discover id=1 ok versions={} capabilities=resources,tools",
                mcp::MODERN_MCP_PROTOCOL_VERSION
            )
        );

        let resource_request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "resources/read",
            "params": {
                "uri": "ui://widget/catdesk-dashboard.html?toolName=run_command"
            }
        });
        let resource_response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "contents": [{"text": "hello", "mimeType": "text/html"}]
            }
        });
        assert_eq!(
            summarize_request(&resource_request),
            "resources/read id=2 tool=run_command uri=ui://widget/catdesk-dashboard.html?toolName=run_command"
        );
        assert_eq!(
            summarize_response(&resource_request, &resource_response),
            "resources/read id=2 ok tool=run_command contents=1 textChars=5"
        );
    }

    #[test]
    fn bootstrap_widget_discovery_uses_catdesk_output_templates_only() {
        let response = json!({
            "result": {
                "tools": [
                    {
                        "name": "read",
                        "_meta": {
                            "openai/outputTemplate": "ui://widget/catdesk-dashboard.html?widgetRevision=2&toolName=read"
                        }
                    },
                    {
                        "name": "catdesk_instruction",
                        "_meta": {
                            "openai/outputTemplate": "ui://widget/catdesk-dashboard.html?widgetRevision=2&toolName=catdesk_instruction"
                        }
                    },
                    {
                        "name": "browser_tool",
                        "_meta": {
                            "openai/outputTemplate": "ui://other/widget.html"
                        }
                    },
                    { "name": "plain_tool" }
                ]
            }
        });

        let widgets = bootstrap_widgets_from_tools_list_response(&response);
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[0].tool_name, "read");
        assert_eq!(widgets[0].label, "read");
        assert_eq!(widgets[1].tool_name, "catdesk_instruction");
        assert_eq!(widgets[1].label, "instruction");
    }

    #[test]
    fn tool_log_summary_omits_large_payload_text() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "write",
                "arguments": {
                    "path": "notes.txt",
                    "content": "very large body",
                    "create_dirs": true
                }
            }
        });
        let summary = summarize_request(&request);
        assert!(summary.contains("tool=write"));
        assert!(summary.contains("content=<15 chars>"));
        assert!(summary.contains("path=\"notes.txt\""));
        assert!(!summary.contains("very large body"));
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique}"))
    }

    #[tokio::test]
    async fn show_detail_mode_sync_updates_app_state() {
        let workspace_root = unique_temp_path("catdesk-show-detail-sync-workspace");
        let config_root = unique_temp_path("catdesk-show-detail-sync-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        sync_show_detail_mode_state(&server_state, ShowDetailMode::Disable).await;

        let app = app_state.lock().await;
        assert!(matches!(app.show_detail_mode, ShowDetailMode::Disable));
        drop(app);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_uses_app_state_show_detail_mode() {
        let workspace_root = unique_temp_path("catdesk-show-detail-runtime-workspace");
        let config_root = unique_temp_path("catdesk-show-detail-runtime-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let mut app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        app.show_detail_mode = ShowDetailMode::Disable;
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state),
            mcp_request_body("server/discover", json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read discover response");
        let payload: Value = serde_json::from_slice(&body).expect("parse discover response");
        let capabilities = payload
            .get("result")
            .and_then(|result| result.get("capabilities"))
            .expect("missing discover capabilities");
        assert!(capabilities.get("tools").is_some());
        assert!(
            capabilities.get("resources").is_none(),
            "runtime MCP handling must use AppState show_detail_mode"
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_logs_request_before_response() {
        let workspace_root = unique_temp_path("catdesk-log-order-workspace");
        let config_root = unique_temp_path("catdesk-log-order-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let mcp_path = app.mcp_path();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state),
            mcp_request_body("server/discover", json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut messages = Vec::new();
        while let Ok(event) = ui_rx.try_recv() {
            if let ServerUiEvent::Log { message, .. } = event {
                messages.push(message);
            }
        }
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            format!(
                "→ POST {mcp_path} server/discover id=req-mcp protocol={} client=catdesk-test/1.0.0",
                mcp::MODERN_MCP_PROTOCOL_VERSION
            )
        );
        assert_eq!(
            messages[1],
            format!(
                "← POST {mcp_path} server/discover id=req-mcp ok versions={} capabilities=resources,tools",
                mcp::MODERN_MCP_PROTOCOL_VERSION
            )
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_reports_runtime_widget_set_from_tools_list() {
        let workspace_root = unique_temp_path("catdesk-bootstrap-tools-workspace");
        let config_root = unique_temp_path("catdesk-bootstrap-tools-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state),
            mcp_request_body("tools/list", json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut tracked = None;
        while let Ok(event) = ui_rx.try_recv() {
            if let ServerUiEvent::RecordBootstrapToolsListResponse {
                success, widgets, ..
            } = event
            {
                tracked = Some((success, widgets));
            }
        }
        let (success, widgets) = tracked.expect("missing bootstrap tools/list event");
        assert!(success);
        assert_eq!(widgets.len(), 12);
        assert_eq!(
            widgets
                .iter()
                .map(|widget| widget.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "run_command",
                "start_command",
                "poll_command",
                "cancel_command",
                "catdesk_instruction",
                "read",
                "read_image",
                "search",
                "write",
                "edit",
                "create_handoff",
                "delete",
            ]
        );
        assert!(widgets.iter().all(|widget| {
            widget.uri.contains("ui://widget/catdesk-dashboard.html")
                && widget
                    .uri
                    .contains(&format!("toolName={}", widget.tool_name))
        }));

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_tracks_widget_read_by_tool_name_across_uri_variations() {
        let workspace_root = unique_temp_path("catdesk-bootstrap-read-workspace");
        let config_root = unique_temp_path("catdesk-bootstrap-read-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state),
            mcp_request_body(
                "resources/read",
                json!({
                    "uri": "ui://widget/catdesk-dashboard.html?widgetRevision=999&tokenStatsLayout=bottom&toolName=read"
                }),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut tracked = None;
        while let Ok(event) = ui_rx.try_recv() {
            if let ServerUiEvent::RecordBootstrapWidgetReadResponse {
                tool_name, success, ..
            } = event
            {
                tracked = Some((tool_name, success));
            }
        }
        let (tool_name, success) = tracked.expect("missing bootstrap resources/read event");
        assert!(success);
        assert_eq!(tool_name, "read");

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    fn tool_call_body(name: &str, arguments: Value) -> Bytes {
        mcp_request_body(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
    }

    fn raw_mcp_request_body(method: &str, params: Value) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": "req-mcp",
                "method": method,
                "params": params,
            }))
            .expect("serialize MCP request"),
        )
    }

    fn mcp_request_body(method: &str, mut params: Value) -> Bytes {
        let params_obj = params
            .as_object_mut()
            .expect("modern test params must be an object");
        params_obj.insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": mcp::MODERN_MCP_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {
                    "name": "catdesk-test",
                    "version": "1.0.0"
                }
            }),
        );
        raw_mcp_request_body(method, params)
    }

    fn modern_mcp_headers(method: &str, name: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "mcp-protocol-version",
            mcp::MODERN_MCP_PROTOCOL_VERSION.parse().unwrap(),
        );
        headers.insert("mcp-method", method.parse().unwrap());
        if let Some(name) = name {
            headers.insert("mcp-name", name.parse().unwrap());
        }
        headers
    }

    fn modern_mcp_headers_for_body(body: &Bytes) -> HeaderMap {
        let request: Value = serde_json::from_slice(body).expect("parse MCP request body");
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .expect("missing MCP method");
        let name = modern_request_name(&request).expect("invalid modern request name");
        modern_mcp_headers(method, name)
    }

    fn modern_mcp_headers_for_session(body: &Bytes, session_id: &str) -> HeaderMap {
        let mut headers = modern_mcp_headers_for_body(body);
        headers.insert("mcp-session-id", session_id.parse().expect("session header"));
        headers
    }

    async fn post_mcp(State(s): State<ServerState>, body: Bytes) -> Response<Body> {
        let headers = modern_mcp_headers_for_body(&body);
        post_mcp_inner(State(s), body, &headers, None).await
    }

    async fn post_mcp_json_with_show_detail_mode(
        server_state: &ServerState,
        body: Bytes,
        show_detail_mode: ShowDetailMode,
    ) -> Value {
        let headers = modern_mcp_headers_for_body(&body);
        let response = post_mcp_inner(
            State(server_state.clone()),
            body,
            &headers,
            Some(show_detail_mode),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read MCP response body");
        serde_json::from_slice(&body).expect("parse MCP response")
    }

    #[tokio::test]
    async fn health_remains_responsive_while_app_state_is_locked() {
        let root = unique_temp_path("catdesk-health-busy");
        std::fs::create_dir_all(&root).unwrap();
        let app = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let state = Arc::new(Mutex::new(app));
        let (ui_events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let server = ServerState {
            app: state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events,
            catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };
        let _locked = state.lock().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            health(State(server)),
        )
        .await
        .expect("health must not wait for AppState");
        assert_eq!(result.0.get("status").and_then(Value::as_str), Some("ok"));
        assert_eq!(result.0.get("busy").and_then(Value::as_bool), Some(true));
        assert!(
            result.0.get("version").is_none(),
            "busy fast-path must stay minimal: no build identity fields"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn health_exposes_build_identity_when_app_state_is_available() {
        let root = unique_temp_path("catdesk-health-identity");
        std::fs::create_dir_all(&root).unwrap();
        let app = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let state = Arc::new(Mutex::new(app));
        let (ui_events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let server = ServerState {
            app: state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events,
            catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), health(State(server)))
                .await
                .expect("health must not wait for AppState");
        assert_eq!(result.0.get("busy").and_then(Value::as_bool), Some(false));
        for field in ["version", "git_sha", "git_branch", "build_time"] {
            let value = result
                .0
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("health must expose a string {field}"));
            assert!(!value.is_empty(), "health {field} must not be empty");
        }
        let version = result.0.get("version").and_then(Value::as_str).unwrap();
        assert!(
            version.starts_with(crate::build_info::VERSION) && !version.starts_with('v'),
            "health version must be machine-parseable semver identity, got {version}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn ping_remains_responsive_while_app_state_is_locked() {
        let root = unique_temp_path("catdesk-ping-busy");
        std::fs::create_dir_all(&root).unwrap();
        let app = AppState::new_for_test(0, root.to_string_lossy().into_owned(), root.join("config.toml")).unwrap();
        let state = Arc::new(Mutex::new(app));
        let (ui_events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let server = ServerState {
            app: state.clone(), devtools: None, command_jobs: CommandJobManager::new(),
            ui_events, catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };
        let _locked = state.lock().await;
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), post_mcp_http(
            State(server), modern_mcp_headers("ping", None), mcp_request_body("ping", json!({})),
        )).await;
        assert_eq!(result.expect("ping must not wait for UI/persistence").status(), StatusCode::OK);
        drop(_locked);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.lock().await.remote_connected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("coalesced authoritative connection update must eventually apply");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn modern_2026_http_flow_validates_and_decorates_results() {
        let workspace_root = unique_temp_path("catdesk-modern-mcp-workspace");
        let config_root = unique_temp_path("catdesk-modern-mcp-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let discover = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers("server/discover", None),
            mcp_request_body("server/discover", json!({})),
        )
        .await;
        assert_eq!(discover.status(), StatusCode::OK);
        let discover_body = to_bytes(discover.into_body(), usize::MAX)
            .await
            .expect("read discover response");
        let discover_json: Value =
            serde_json::from_slice(&discover_body).expect("parse discover response");
        let discover_result = discover_json.get("result").expect("discover result");
        assert_eq!(
            discover_result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .and_then(|versions| versions.first())
                .and_then(Value::as_str),
            Some(mcp::MODERN_MCP_PROTOCOL_VERSION)
        );
        assert_eq!(
            discover_result.get("resultType").and_then(Value::as_str),
            Some("complete")
        );
        assert_eq!(
            discover_result.get("ttlMs").and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(
            discover_result.get("cacheScope").and_then(Value::as_str),
            Some("private")
        );
        assert_eq!(
            discover_result
                .get("_meta")
                .and_then(|meta| meta.get("io.modelcontextprotocol/serverInfo"))
                .and_then(|server| server.get("name"))
                .and_then(Value::as_str),
            Some("catdesk")
        );

        let tools = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers("tools/list", None),
            mcp_request_body("tools/list", json!({})),
        )
        .await;
        assert_eq!(tools.status(), StatusCode::OK);
        let tools_body = to_bytes(tools.into_body(), usize::MAX)
            .await
            .expect("read tools response");
        let tools_json: Value = serde_json::from_slice(&tools_body).expect("parse tools response");
        let tools_result = tools_json.get("result").expect("tools result");
        assert_eq!(
            tools_result.get("resultType").and_then(Value::as_str),
            Some("complete")
        );
        assert_eq!(tools_result.get("ttlMs").and_then(Value::as_u64), Some(0));
        assert_eq!(
            tools_result.get("cacheScope").and_then(Value::as_str),
            Some("private")
        );

        let mut read_result = json!({ "contents": [] });
        mcp::decorate_modern_result("resources/read", &mut read_result);
        assert_eq!(read_result.get("ttlMs").and_then(Value::as_u64), Some(0));
        assert_eq!(
            read_result.get("cacheScope").and_then(Value::as_str),
            Some("private")
        );

        let mut resource_list_result = json!({ "resources": [], "nextCursor": null });
        mcp::decorate_modern_result("resources/list", &mut resource_list_result);
        assert!(resource_list_result.get("nextCursor").is_none());

        let unknown = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers("catdesk/unknown", None),
            mcp_request_body("catdesk/unknown", json!({})),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let unknown_body = to_bytes(unknown.into_body(), usize::MAX)
            .await
            .expect("read unknown response");
        let unknown_json: Value =
            serde_json::from_slice(&unknown_body).expect("parse unknown response");
        assert_eq!(
            unknown_json
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(-32601)
        );

        let mut mismatched_headers = modern_mcp_headers("server/discover", None);
        mismatched_headers.insert("mcp-protocol-version", "1900-01-01".parse().unwrap());
        let mismatch = post_mcp_http(
            State(server_state),
            mismatched_headers,
            mcp_request_body("server/discover", json!({})),
        )
        .await;
        assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
        let mismatch_body = to_bytes(mismatch.into_body(), usize::MAX)
            .await
            .expect("read mismatch response");
        let mismatch_json: Value =
            serde_json::from_slice(&mismatch_body).expect("parse mismatch response");
        assert_eq!(
            mismatch_json
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(-32020)
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn initialize_is_not_a_supported_catdesk_mcp_method() {
        let workspace_root = unique_temp_path("catdesk-no-initialize-workspace");
        let config_root = unique_temp_path("catdesk-no-initialize-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let legacy_shape = post_mcp_http(
            State(server_state.clone()),
            HeaderMap::new(),
            raw_mcp_request_body(
                "initialize",
                json!({
                    "protocolVersion": "legacy-test",
                    "capabilities": {},
                    "clientInfo": {"name": "legacy-client", "version": "1.0.0"}
                }),
            ),
        )
        .await;
        assert_eq!(legacy_shape.status(), StatusCode::BAD_REQUEST);
        let legacy_body = to_bytes(legacy_shape.into_body(), usize::MAX)
            .await
            .expect("read legacy-shaped response");
        let legacy_json: Value =
            serde_json::from_slice(&legacy_body).expect("parse legacy-shaped response");
        assert_eq!(
            legacy_json
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(-32602)
        );

        let modern_shape = post_mcp_http(
            State(server_state),
            modern_mcp_headers("initialize", None),
            mcp_request_body("initialize", json!({})),
        )
        .await;
        assert_eq!(modern_shape.status(), StatusCode::NOT_FOUND);
        let modern_body = to_bytes(modern_shape.into_body(), usize::MAX)
            .await
            .expect("read unsupported initialize response");
        let modern_json: Value =
            serde_json::from_slice(&modern_body).expect("parse unsupported initialize response");
        assert_eq!(
            modern_json
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(-32601)
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn flow_argument_summary_labels_batched_reads() {
        let one = json!({ "paths": ["src/config.py"] });
        let many = json!({ "paths": ["src/config.py", "src/loader.py", "src/errors.py"] });
        assert_eq!(
            flow_argument_summary("read", one.as_object().unwrap()).as_deref(),
            Some("config.py")
        );
        assert_eq!(
            flow_argument_summary("read", many.as_object().unwrap()).as_deref(),
            Some("config.py +2")
        );
    }

    #[test]
    fn modern_mcp_name_header_decodes_base64_sentinel() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("測試 resource");
        assert_eq!(
            decode_mcp_name_header(&format!("=?base64?{encoded}?=")).as_deref(),
            Ok("測試 resource")
        );
    }

    #[tokio::test]
    async fn public_routes_require_secret_prefix() {
        let workspace_root = unique_temp_path("catdesk-route-guard");
        let config_root = unique_temp_path("catdesk-route-guard-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let router = router(
            app_state,
            None,
            CommandJobManager::new(),
            "/secret-slug/mcp".to_string(),
            ui_tx,
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve test router");
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let unprefixed = [
            ("GET", "/"),
            ("GET", "/agents/path-state"),
            ("POST", "/agents/path-mode"),
            ("POST", "/layout/token-stats"),
            ("POST", "/layout/show-detail"),
            ("POST", "/binagotchy/partner"),
        ];
        for (method, path) in unprefixed {
            let request = match method {
                "GET" => client.get(format!("{base}{path}")),
                "POST" => client.post(format!("{base}{path}")),
                _ => unreachable!(),
            };
            let response = request.send().await.expect("send unprefixed request");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "unprefixed route must be hidden: {method} {path}"
            );
        }

        let response = client
            .get(format!("{base}/secret-slug/agents/path-state"))
            .send()
            .await
            .expect("send prefixed request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn extract_turn_token_usage_reads_widget_payload_meta() {
        let result = json!({
            "structuredContent": {
                "schema": "catdesk.review.v1"
            },
            "_meta": {
                WIDGET_PAYLOAD_META_KEY: {
                    "schema": "catdesk.review.v1",
                    "turnTokenUsage": {
                        "inputTokens": 11,
                        "outputTokens": 22,
                        "totalTokens": 33
                    }
                }
            }
        });

        assert_eq!(extract_turn_token_usage(Some(&result)), Some((11, 22)));
    }

    #[test]
    fn turn_token_usage_falls_back_without_widget_payload() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "read",
                "arguments": { "path": "notes.txt" }
            }),
        };
        let result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "read",
                "text": "hello"
            }
        });

        assert!(extract_turn_token_usage(Some(&result)).is_none());
        let (input_tokens, output_tokens) =
            turn_token_usage_for_response(&req, Some(&result)).expect("missing usage");
        assert!(input_tokens > 0);
        assert!(output_tokens > 0);
    }

    #[test]
    fn attach_history_usage_updates_widget_payload_meta() {
        let mut result = Some(json!({
            "structuredContent": {
                "schema": "catdesk.review.v1"
            },
            "_meta": {
                "catdesk/widgetPayload": {
                    "schema": "catdesk.review.v1",
                    "turnTokenUsage": {
                        "inputTokens": 11,
                        "outputTokens": 22,
                        "totalTokens": 33
                    },
                    "toolCallCount": 4
                }
            }
        }));
        let usage_totals = UsageTotals {
            tool_input_tokens: 120,
            tool_output_tokens: 34,
            total_tokens: 154,
            tool_call_count: 7,
        };

        attach_history_usage(&mut result, &usage_totals);

        let structured = result
            .as_ref()
            .and_then(|value| value.get("structuredContent"))
            .expect("missing structuredContent");
        let widget_payload = result
            .as_ref()
            .and_then(|value| value.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .expect("missing widget payload");

        assert!(structured.get("historyTurnTokenUsage").is_none());
        assert!(structured.get("historyToolCallCount").is_none());
        assert_eq!(
            widget_payload
                .get("historyTurnTokenUsage")
                .and_then(|usage| usage.get("totalTokens"))
                .and_then(Value::as_u64),
            Some(154)
        );
        assert_eq!(
            widget_payload
                .get("historyToolCallCount")
                .and_then(Value::as_u64),
            Some(7)
        );
        // Direction markers must ride along so clients never confuse the tool
        // input/output axis with the request/response axis.
        assert_eq!(
            widget_payload
                .get("historyTurnTokenUsage")
                .and_then(|usage| usage.get("inputRole"))
                .and_then(Value::as_str),
            Some("request")
        );
        assert_eq!(
            widget_payload
                .get("historyTurnTokenUsage")
                .and_then(|usage| usage.get("outputRole"))
                .and_then(Value::as_str),
            Some("response")
        );
    }

    #[test]
    fn attach_catdesk_instruction_actions_injects_partner_and_urls() {
        let mut result = Some(json!({
            "structuredContent": {
                "schema": "catdesk.review.v1",
                "toolName": "catdesk_instruction"
            },
            "_meta": {
                WIDGET_PAYLOAD_META_KEY: {
                    "schema": "catdesk.review.v1",
                    "toolName": "catdesk_instruction",
                    "binagotchyCards": [{
                        "folder": "20260403T010203000Z_deadbeef",
                        "seed": "deadbeef"
                    }]
                }
            }
        }));

        attach_catdesk_instruction_actions(
            &mut result,
            Some("https://example.ngrok.app"),
            "/secret-slug/mcp",
            0xff,
            Some("deadbeef"),
        );

        let structured = result
            .as_ref()
            .and_then(|value| value.get("structuredContent"))
            .expect("missing structuredContent");
        let widget_payload = result
            .as_ref()
            .and_then(|value| value.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .expect("missing widget payload");
        let card = widget_payload
            .get("binagotchyCards")
            .and_then(Value::as_array)
            .and_then(|cards| cards.first())
            .expect("missing card");

        assert!(structured.get("binagotchyCards").is_none());
        assert!(structured.get("binagotchyApiBaseUrl").is_none());
        assert!(structured.get("partnerBinagotchySeed").is_none());
        assert_eq!(
            widget_payload
                .get("binagotchyApiBaseUrl")
                .and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/binagotchy")
        );
        assert_eq!(
            widget_payload
                .get("partnerBinagotchySeed")
                .and_then(Value::as_str),
            Some("deadbeef")
        );
        assert_eq!(
            widget_payload
                .get("agentsPathModeUrl")
                .and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/agents/path-mode")
        );
        assert_eq!(
            widget_payload
                .get("agentsPathStateUrl")
                .and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/agents/path-state")
        );
        assert_eq!(
            widget_payload
                .get("tokenStatsLayoutUrl")
                .and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/layout/token-stats")
        );
        assert_eq!(
            widget_payload
                .get("showDetailModeUrl")
                .and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/layout/show-detail")
        );
        assert!(widget_payload.get("widgetMascot").is_some());
        assert_eq!(card.get("isPartner").and_then(Value::as_bool), Some(true));
        assert_eq!(
            card.get("saveFolderUrl").and_then(Value::as_str),
            Some(
                "https://example.ngrok.app/secret-slug/binagotchy/archive/20260403T010203000Z_deadbeef/save"
            )
        );
        assert_eq!(
            card.get("setPartnerUrl").and_then(Value::as_str),
            Some("https://example.ngrok.app/secret-slug/binagotchy/partner")
        );
    }

    #[tokio::test]
    async fn background_command_survives_separate_stateless_http_requests() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-command-job");
        let config_root = unique_temp_path("catdesk-post-mcp-command-job-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let command_jobs = CommandJobManager::new();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 250; Write-Output http-job-done"
        } else {
            "sleep 0.25; printf 'http-job-done\\n'"
        };

        let start_response = post_mcp(
            State(server_state.clone()),
            tool_call_body(
                "start_command",
                json!({ "command": command, "timeout": 5_000 }),
            ),
        )
        .await;
        assert_eq!(start_response.status(), StatusCode::OK);
        let start_body = to_bytes(start_response.into_body(), usize::MAX)
            .await
            .expect("read start response");
        let start_payload: Value =
            serde_json::from_slice(&start_body).expect("parse start response");
        let job_id = start_payload
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("jobId"))
            .and_then(Value::as_str)
            .expect("start response job id")
            .to_string();

        let mut cursor = 0u64;
        let mut seen = String::new();
        let mut terminal = None;
        for _ in 0..20 {
            let poll_response = post_mcp(
                State(server_state.clone()),
                tool_call_body(
                    "poll_command",
                    json!({ "job_id": job_id, "after": cursor, "wait_ms": 250 }),
                ),
            )
            .await;
            assert_eq!(poll_response.status(), StatusCode::OK);
            let poll_body = to_bytes(poll_response.into_body(), usize::MAX)
                .await
                .expect("read poll response");
            let poll_payload: Value =
                serde_json::from_slice(&poll_body).expect("parse poll response");
            let structured = poll_payload
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .expect("poll structured content");
            if let Some(events) = structured.get("events").and_then(Value::as_array) {
                for event in events {
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        seen.push_str(text);
                    }
                }
            }
            cursor = structured
                .get("nextCursor")
                .and_then(Value::as_u64)
                .unwrap_or(cursor);
            let state = structured.get("state").and_then(Value::as_str);
            let has_more = structured
                .get("hasMoreOutput")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if state == Some("succeeded") && !has_more {
                terminal = Some(poll_payload);
                break;
            }
        }

        let terminal = terminal.expect("background job did not finish across HTTP requests");
        let structured = terminal
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .expect("terminal structured content");
        assert_eq!(
            structured.get("state").and_then(Value::as_str),
            Some("succeeded")
        );
        assert_eq!(
            structured.get("commandSuccess").and_then(Value::as_bool),
            Some(true)
        );
        assert!(seen.contains("http-job-done"));

        command_jobs.cancel_all().await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn catdesk_instruction_unlocks_tools_until_process_restart() {
        let workspace_root = unique_temp_path("catdesk-instruction-gate-workspace");
        let config_root = unique_temp_path("catdesk-instruction-gate-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let instruction_called = InstructionGate::with_anonymous(false);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: instruction_called.clone(),
        };

        let blocked_response = post_mcp(
            State(server_state.clone()),
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
        )
        .await;
        assert_eq!(blocked_response.status(), StatusCode::OK);
        let blocked_body = to_bytes(blocked_response.into_body(), usize::MAX)
            .await
            .expect("read blocked response body");
        let blocked_payload: Value =
            serde_json::from_slice(&blocked_body).expect("parse blocked response");
        assert_eq!(
            blocked_payload
                .get("result")
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            None
        );
        assert_eq!(
            blocked_payload
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.get("success"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            blocked_payload
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.get("errorCode"))
                .and_then(Value::as_str),
            Some("CATDESK_INSTRUCTION_REQUIRED")
        );
        assert!(!instruction_called.is_called(&ClientSession::from_headers(&HeaderMap::new())));

        let instruction_response = post_mcp(
            State(server_state.clone()),
            tool_call_body("catdesk_instruction", json!({})),
        )
        .await;
        assert_eq!(instruction_response.status(), StatusCode::OK);
        let instruction_body = to_bytes(instruction_response.into_body(), usize::MAX)
            .await
            .expect("read instruction response body");
        let instruction_payload: Value =
            serde_json::from_slice(&instruction_body).expect("parse instruction response");
        assert_ne!(
            instruction_payload
                .get("result")
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(instruction_called.is_called(&ClientSession::from_headers(&HeaderMap::new())));

        let allowed_response = post_mcp(
            State(server_state),
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
        )
        .await;
        assert_eq!(allowed_response.status(), StatusCode::OK);
        let allowed_body = to_bytes(allowed_response.into_body(), usize::MAX)
            .await
            .expect("read allowed response body");
        let allowed_payload: Value =
            serde_json::from_slice(&allowed_body).expect("parse allowed response");
        assert_eq!(
            allowed_payload
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.pointer("/files/0/text"))
                .and_then(Value::as_str),
            Some("hello world\n")
        );

        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn instruction_gate_is_isolated_between_mcp_sessions() {
        let workspace_root = unique_temp_path("catdesk-session-instruction-workspace");
        let config_root = unique_temp_path("catdesk-session-instruction-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };

        let instruction_body = tool_call_body("catdesk_instruction", json!({}));
        let instruction = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers_for_session(&instruction_body, "client-a-secret-session"),
            instruction_body,
        )
        .await;
        assert_eq!(instruction.status(), StatusCode::OK);

        let read_a_body = tool_call_body("read", json!({ "paths": ["hello.txt"] }));
        let read_a = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers_for_session(&read_a_body, "client-a-secret-session"),
            read_a_body,
        )
        .await;
        let read_a_body = to_bytes(read_a.into_body(), usize::MAX).await.unwrap();
        let read_a_payload: Value = serde_json::from_slice(&read_a_body).unwrap();
        assert_eq!(
            read_a_payload
                .pointer("/result/structuredContent/files/0/text")
                .and_then(Value::as_str),
            Some("hello world\n"),
            "the session that called catdesk_instruction must stay unlocked"
        );

        let read_b_body = tool_call_body("read", json!({ "paths": ["hello.txt"] }));
        let read_b = post_mcp_http(
            State(server_state),
            modern_mcp_headers_for_session(&read_b_body, "client-b-secret-session"),
            read_b_body,
        )
        .await;
        let read_b_body = to_bytes(read_b.into_body(), usize::MAX).await.unwrap();
        let read_b_payload: Value = serde_json::from_slice(&read_b_body).unwrap();
        assert_eq!(
            read_b_payload
                .pointer("/result/structuredContent/errorCode")
                .and_then(Value::as_str),
            Some("CATDESK_INSTRUCTION_REQUIRED"),
            "one MCP client's instruction call must not unlock another client"
        );

        while ui_rx.try_recv().is_ok() {}
        let session_flow_ids = app_state
            .lock()
            .await
            .flows
            .iter()
            .map(|flow| flow.flow_id.clone())
            .collect::<Vec<_>>();
        assert!(session_flow_ids.iter().any(|flow_id| flow_id != STATELESS_FLOW_ID));
        assert!(session_flow_ids.iter().all(|flow_id| !flow_id.contains("client-a-secret-session")));
        assert!(session_flow_ids.iter().all(|flow_id| !flow_id.contains("client-b-secret-session")));

        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn deleting_one_named_session_cancels_only_its_jobs_and_keeps_other_session_connected() {
        let workspace_root = unique_temp_path("catdesk-delete-session-workspace");
        let config_root = unique_temp_path("catdesk-delete-session-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let gate = InstructionGate::with_anonymous(false);
        let session_a = ClientSession::from_headers(&modern_mcp_headers_for_session(
            &tool_call_body("catdesk_instruction", json!({})),
            "session-a",
        ));
        let session_b = ClientSession::from_headers(&modern_mcp_headers_for_session(
            &tool_call_body("catdesk_instruction", json!({})),
            "session-b",
        ));
        gate.mark_called(&session_a);
        gate.mark_called(&session_b);

        let command_jobs = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let a_job = command_jobs
            .start_with_change_session(
                command.to_string(),
                workspace_root.clone(),
                workspace_root.clone(),
                10_000,
                None,
                None,
                Some("session-a"),
            )
            .await
            .expect("start session-a job");
        let b_job = command_jobs
            .start_with_change_session(
                command.to_string(),
                workspace_root.clone(),
                workspace_root.clone(),
                10_000,
                None,
                None,
                Some("session-b"),
            )
            .await
            .expect("start session-b job");

        app_state.lock().await.set_remote_connected(true);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
            catdesk_instruction_called: gate.clone(),
        };

        let delete_a = delete_mcp(
            State(server_state.clone()),
            modern_mcp_headers_for_session(&tool_call_body("ping", json!({})), "session-a"),
        )
        .await;
        assert_eq!(delete_a.status(), StatusCode::OK);
        assert!(!gate.contains_named_session("session-a"));
        assert!(gate.contains_named_session("session-b"));

        let a_terminal = loop {
            let snapshot = command_jobs
                .poll_for_session(&a_job.snapshot.job_id, 0, 100, Some("session-a"))
                .await
                .expect("poll session-a job after delete");
            if snapshot.state.is_terminal() {
                break snapshot;
            }
        };
        assert_eq!(a_terminal.state, crate::command_jobs::CommandJobState::Cancelled);
        let b_snapshot = command_jobs
            .poll_for_session(&b_job.snapshot.job_id, 0, 0, Some("session-b"))
            .await
            .expect("poll surviving session-b job");
        assert_eq!(b_snapshot.state, crate::command_jobs::CommandJobState::Running);

        assert!(
            app_state.lock().await.remote_connected,
            "deleting session-a must not disconnect session-b"
        );

        let delete_b = delete_mcp(
            State(server_state),
            modern_mcp_headers_for_session(&tool_call_body("ping", json!({})), "session-b"),
        )
        .await;
        assert_eq!(delete_b.status(), StatusCode::OK);
        assert!(
            !app_state.lock().await.remote_connected,
            "deleting the final named session must disconnect authoritative remote state"
        );

        command_jobs.cancel_all().await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn openai_session_metadata_tracks_parallel_chats_without_mcp_session_header() {
        fn with_openai_session(body: Bytes, session_id: &str) -> Bytes {
            let mut value: Value = serde_json::from_slice(&body).expect("parse test request");
            value["params"]["_meta"][OPENAI_SESSION_META_KEY] = json!(session_id);
            Bytes::from(serde_json::to_vec(&value).expect("serialize test request"))
        }

        let workspace_root = unique_temp_path("catdesk-openai-session-workspace");
        let config_root = unique_temp_path("catdesk-openai-session-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };

        for session_id in ["chat-a", "chat-b"] {
            let body = with_openai_session(
                tool_call_body("catdesk_instruction", json!({})),
                session_id,
            );
            let headers = modern_mcp_headers_for_body(&body);
            assert!(headers.get(MCP_SESSION_ID_HEADER).is_none());
            let response = post_mcp_http(State(server_state.clone()), headers, body).await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let app = app_state.lock().await;
        assert_eq!(app.connected_chat_count(), 2);
        assert!(app.flows.iter().all(|flow| !flow.flow_id.contains("chat-")));
        drop(app);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn authoritative_session_state_survives_a_full_ui_event_queue() {
        let workspace_root = unique_temp_path("catdesk-authoritative-session-workspace");
        let config_root = unique_temp_path("catdesk-authoritative-session-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        for _ in 0..crate::state::UI_EVENT_CAPACITY {
            ui_tx
                .try_send(ServerUiEvent::IncrementRequestCount)
                .expect("fill UI event queue");
        }
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(false),
        };
        let request = tool_call_body("catdesk_instruction", json!({}));
        let headers = modern_mcp_headers_for_session(&request, "queue-saturated-session");
        let response = post_mcp_http(
            State(server_state.clone()),
            headers.clone(),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        {
            let app = app_state.lock().await;
            assert!(app.remote_connected);
            assert_eq!(app.connected_chat_count(), 1);
        }

        let deleted = delete_mcp(State(server_state), headers).await;
        assert_eq!(deleted.status(), StatusCode::OK);
        {
            let app = app_state.lock().await;
            assert!(!app.remote_connected);
            assert_eq!(app.connected_chat_count(), 0);
        }

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn named_session_reuses_project_selected_by_successful_explicit_command_cwd() {
        let workspace_root = unique_temp_path("catdesk-session-project-workspace");
        let config_root = unique_temp_path("catdesk-session-project-config");
        let config_path = config_root.join("config.toml");
        let project = workspace_root.join("repo-a");
        std::fs::create_dir_all(project.join(".git")).expect("create project");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let gate = InstructionGate::with_anonymous(false);
        let session_id = "project-session-a";
        let instruction_body = tool_call_body("catdesk_instruction", json!({}));
        let session = ClientSession::from_headers(&modern_mcp_headers_for_session(
            &instruction_body,
            session_id,
        ));
        gate.mark_called(&session);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: gate.clone(),
        };

        let explicit_body = tool_call_body(
            "run_command",
            json!({ "command": "pwd", "cwd": project.to_string_lossy() }),
        );
        let explicit_response = post_mcp_http(
            State(server_state.clone()),
            modern_mcp_headers_for_session(&explicit_body, session_id),
            explicit_body,
        )
        .await;
        assert_eq!(explicit_response.status(), StatusCode::OK);
        let explicit_bytes = to_bytes(explicit_response.into_body(), usize::MAX)
            .await
            .expect("read explicit cwd response");
        let explicit_payload: Value =
            serde_json::from_slice(&explicit_bytes).expect("parse explicit cwd response");
        let explicit_cwd = explicit_payload
            .pointer("/result/structuredContent/cwd")
            .and_then(Value::as_str)
            .expect("explicit session cwd");
        assert_eq!(
            std::path::Path::new(explicit_cwd)
                .canonicalize()
                .expect("canonical explicit cwd"),
            project.canonicalize().expect("canonical expected project")
        );

        let implicit_body = tool_call_body("run_command", json!({ "command": "pwd" }));
        let implicit_response = post_mcp_http(
            State(server_state),
            modern_mcp_headers_for_session(&implicit_body, session_id),
            implicit_body,
        )
        .await;
        assert_eq!(implicit_response.status(), StatusCode::OK);
        let implicit_bytes = to_bytes(implicit_response.into_body(), usize::MAX)
            .await
            .expect("read implicit cwd response");
        let implicit_payload: Value =
            serde_json::from_slice(&implicit_bytes).expect("parse implicit cwd response");
        let implicit_cwd = implicit_payload
            .pointer("/result/structuredContent/cwd")
            .and_then(Value::as_str)
            .expect("implicit session cwd");
        assert_eq!(
            std::path::Path::new(implicit_cwd)
                .canonicalize()
                .expect("canonical implicit cwd"),
            project.canonicalize().expect("canonical explicit project"),
            "named session should reuse the project selected by the successful explicit cwd"
        );
        assert_eq!(
            gate.active_project(&session)
                .map(|path| path.canonicalize().expect("canonical selected project")),
            Some(project.canonicalize().expect("canonical expected project"))
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[test]
    fn project_signal_requires_unambiguous_file_scope_and_ignores_workspace_search_root() {
        let workspace_root = unique_temp_path("catdesk-project-signal-workspace");
        let repo_a = workspace_root.join("repo-a");
        let repo_b = workspace_root.join("repo-b");
        std::fs::create_dir_all(repo_a.join(".git")).expect("create repo-a");
        std::fs::create_dir_all(repo_b.join(".git")).expect("create repo-b");
        std::fs::write(repo_a.join("a.txt"), "a\n").expect("write repo-a file");
        std::fs::write(repo_b.join("b.txt"), "b\n").expect("write repo-b file");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let parse = |body: Bytes| -> JsonRpcRequest {
            serde_json::from_slice(&body).expect("parse tool request")
        };
        let same_repo = parse(tool_call_body(
            "read",
            json!({ "paths": [repo_a.join("a.txt").to_string_lossy()] }),
        ));
        assert_eq!(
            project_signal_for_request(&same_repo, &workspace_root_str),
            Some(repo_a.canonicalize().expect("canonical repo-a"))
        );

        let mixed = parse(tool_call_body(
            "read",
            json!({
                "paths": [
                    repo_a.join("a.txt").to_string_lossy(),
                    repo_b.join("b.txt").to_string_lossy()
                ]
            }),
        ));
        assert_eq!(project_signal_for_request(&mixed, &workspace_root_str), None);

        let root_search = parse(tool_call_body(
            "search",
            json!({ "pattern": "a", "path": workspace_root.to_string_lossy() }),
        ));
        assert_eq!(project_signal_for_request(&root_search, &workspace_root_str), None);

        let repo_search = parse(tool_call_body(
            "search",
            json!({ "pattern": "b", "path": repo_b.to_string_lossy() }),
        ));
        assert_eq!(
            project_signal_for_request(&repo_search, &workspace_root_str),
            Some(repo_b.canonicalize().expect("canonical repo-b"))
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn three_named_sessions_keep_independent_projects_across_command_read_and_search_signals() {
        let workspace_root = unique_temp_path("catdesk-three-session-project-workspace");
        let config_root = unique_temp_path("catdesk-three-session-project-config");
        let config_path = config_root.join("config.toml");
        let repo_a = workspace_root.join("repo-a");
        let repo_b = workspace_root.join("repo-b");
        let repo_c = workspace_root.join("repo-c");
        for repo in [&repo_a, &repo_b, &repo_c] {
            std::fs::create_dir_all(repo.join(".git")).expect("create repo git marker");
        }
        std::fs::write(repo_b.join("selected.txt"), "selected by read\n")
            .expect("write repo-b file");
        std::fs::write(repo_c.join("selected.txt"), "needle selected by search\n")
            .expect("write repo-c file");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let gate = InstructionGate::with_anonymous(false);
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: gate.clone(),
        };

        async fn call(
            state: ServerState,
            session_id: &str,
            tool: &str,
            arguments: Value,
        ) -> Value {
            let body = tool_call_body(tool, arguments);
            let response = post_mcp_http(
                State(state),
                modern_mcp_headers_for_session(&body, session_id),
                body,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "tool {tool}");
            let bytes = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read tool response");
            let payload: Value = serde_json::from_slice(&bytes).expect("parse tool response");
            assert_ne!(
                payload.pointer("/result/isError").and_then(Value::as_bool),
                Some(true),
                "tool {tool} failed: {payload}"
            );
            payload
        }

        for session_id in ["session-a", "session-b", "session-c"] {
            let body = tool_call_body("catdesk_instruction", json!({}));
            let session = ClientSession::from_headers(&modern_mcp_headers_for_session(
                &body,
                session_id,
            ));
            gate.mark_called(&session);
        }

        let _ = call(
            server_state.clone(),
            "session-a",
            "run_command",
            json!({ "command": "pwd", "cwd": repo_a.to_string_lossy() }),
        )
        .await;
        let _ = call(
            server_state.clone(),
            "session-b",
            "read",
            json!({ "paths": [repo_b.join("selected.txt").to_string_lossy()] }),
        )
        .await;
        let _ = call(
            server_state.clone(),
            "session-c",
            "search",
            json!({ "pattern": "needle", "path": repo_c.to_string_lossy() }),
        )
        .await;

        let a = call(
            server_state.clone(),
            "session-a",
            "run_command",
            json!({ "command": "pwd" }),
        );
        let b = call(
            server_state.clone(),
            "session-b",
            "run_command",
            json!({ "command": "pwd" }),
        );
        let c = call(
            server_state,
            "session-c",
            "run_command",
            json!({ "command": "pwd" }),
        );
        let (a, b, c) = tokio::join!(a, b, c);

        for (payload, expected) in [(a, &repo_a), (b, &repo_b), (c, &repo_c)] {
            let cwd = payload
                .pointer("/result/structuredContent/cwd")
                .and_then(Value::as_str)
                .expect("session cwd");
            assert_eq!(
                std::path::Path::new(cwd)
                    .canonicalize()
                    .expect("canonical session cwd"),
                expected.canonicalize().expect("canonical expected project")
            );
        }

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn http_start_command_idempotency_is_scoped_to_mcp_session() {
        let workspace_root = unique_temp_path("catdesk-http-session-job-workspace");
        let config_root = unique_temp_path("catdesk-http-session-job-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let gate = InstructionGate::with_anonymous(false);
        for session_id in ["session-a", "session-b"] {
            let body = tool_call_body("catdesk_instruction", json!({}));
            let session = ClientSession::from_headers(&modern_mcp_headers_for_session(&body, session_id));
            gate.mark_called(&session);
        }
        let command_jobs = CommandJobManager::new();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
            catdesk_instruction_called: gate,
        };
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let start_body = tool_call_body("start_command", json!({ "command": command }));

        async fn call_start(
            state: &ServerState,
            body: &Bytes,
            session_id: &str,
        ) -> Value {
            let response = post_mcp_http(
                State(state.clone()),
                modern_mcp_headers_for_session(body, session_id),
                body.clone(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read start response");
            serde_json::from_slice(&bytes).expect("parse start response")
        }

        let job_id = |payload: &Value| {
            payload
                .pointer("/result/structuredContent/jobId")
                .and_then(Value::as_str)
                .expect("missing job id")
                .to_string()
        };
        let deduplicated = |payload: &Value| {
            payload
                .pointer("/result/structuredContent/deduplicated")
                .and_then(Value::as_bool)
                .expect("missing deduplicated flag")
        };

        let a1 = call_start(&server_state, &start_body, "session-a").await;
        let a2 = call_start(&server_state, &start_body, "session-a").await;
        let b1 = call_start(&server_state, &start_body, "session-b").await;
        assert_eq!(job_id(&a1), job_id(&a2));
        assert!(!deduplicated(&a1));
        assert!(deduplicated(&a2));
        assert_ne!(job_id(&a1), job_id(&b1));
        assert!(!deduplicated(&b1));

        command_jobs.cancel_all().await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated() {
        let workspace_root = unique_temp_path("catdesk-parallel-session-stress-workspace");
        let config_root = unique_temp_path("catdesk-parallel-session-stress-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello parallel\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let gate = InstructionGate::with_anonymous(false);
        for index in 0..18 {
            let session_id = format!("stress-session-{index}");
            let body = tool_call_body("catdesk_instruction", json!({}));
            let session = ClientSession::from_headers(&modern_mcp_headers_for_session(
                &body,
                &session_id,
            ));
            gate.mark_called(&session);
        }
        let command_jobs = CommandJobManager::new();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
            catdesk_instruction_called: gate,
        };
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 150"
        } else {
            "sleep 0.15"
        };

        let mut tasks = Vec::new();
        for index in 0..18usize {
            let session_id = format!("stress-session-{index}");
            let body = match index % 3 {
                0 => mcp_request_body("tools/list", json!({})),
                1 => tool_call_body("read", json!({ "paths": ["hello.txt"] })),
                _ => tool_call_body("start_command", json!({ "command": command })),
            };
            let headers = modern_mcp_headers_for_session(&body, &session_id);
            let state = server_state.clone();
            tasks.push(tokio::spawn(async move {
                let response = post_mcp_http(State(state), headers, body).await;
                let status = response.status();
                let bytes = to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("read stress response");
                let payload: Value = serde_json::from_slice(&bytes).expect("parse stress response");
                (index, session_id, status, payload)
            }));
        }

        let mut process_jobs = Vec::new();
        for task in tasks {
            let (index, session_id, status, payload) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                task,
            )
            .await
            .expect("parallel MCP request hung")
            .expect("parallel MCP task panicked");
            assert_eq!(status, StatusCode::OK, "request failed for {session_id}: {payload}");
            assert_eq!(payload.get("id").and_then(Value::as_str), Some("req-mcp"));
            match index % 3 {
                0 => assert!(
                    payload
                        .pointer("/result/tools")
                        .and_then(Value::as_array)
                        .is_some_and(|tools| !tools.is_empty()),
                    "control response missing tools for {session_id}: {payload}"
                ),
                1 => assert_eq!(
                    payload
                        .pointer("/result/structuredContent/files/0/text")
                        .and_then(Value::as_str),
                    Some("hello parallel\n"),
                    "filesystem response crossed or lost session state for {session_id}"
                ),
                _ => {
                    let job_id = payload
                        .pointer("/result/structuredContent/jobId")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| panic!("process response missing job id for {session_id}: {payload}"));
                    process_jobs.push((session_id, job_id.to_string()));
                }
            }
        }

        assert_eq!(process_jobs.len(), 6);
        let unique_job_ids = process_jobs
            .iter()
            .map(|(_, job_id)| job_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            unique_job_ids.len(),
            process_jobs.len(),
            "different sessions reusing one JSON-RPC id must never share a command job"
        );
        for (session_id, job_id) in &process_jobs {
            command_jobs
                .poll_for_session(job_id, 0, 0, Some(session_id))
                .await
                .expect("job must remain visible to its owner session");
        }

        command_jobs.cancel_all().await;
        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn disabled_show_detail_mode_hides_widgets_across_mcp_flow() {
        let workspace_root = unique_temp_path("catdesk-disable-mcp-flow-workspace");
        let config_root = unique_temp_path("catdesk-disable-mcp-flow-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n")
            .expect("write workspace file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let instruction_called = InstructionGate::with_anonymous(false);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: instruction_called.clone(),
        };

        let discover = post_mcp_json_with_show_detail_mode(
            &server_state,
            mcp_request_body("server/discover", json!({})),
            ShowDetailMode::Disable,
        )
        .await;
        let capabilities = discover
            .get("result")
            .and_then(|result| result.get("capabilities"))
            .expect("missing discover capabilities");
        assert!(capabilities.get("tools").is_some());
        assert!(
            capabilities.get("resources").is_none(),
            "Disable must not advertise resources during discovery"
        );

        let tools_list = post_mcp_json_with_show_detail_mode(
            &server_state,
            mcp_request_body("tools/list", json!({})),
            ShowDetailMode::Disable,
        )
        .await;
        let tools = tools_list
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .expect("missing tools");
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(
                tool.get("_meta")
                    .and_then(|meta| meta.get("openai/outputTemplate"))
                    .is_none(),
                "Disable must not advertise a Widget output template"
            );
        }

        let blocked = post_mcp_json_with_show_detail_mode(
            &server_state,
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
            ShowDetailMode::Disable,
        )
        .await;
        assert_eq!(
            blocked
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.get("errorCode"))
                .and_then(Value::as_str),
            Some("CATDESK_INSTRUCTION_REQUIRED")
        );
        assert!(
            blocked
                .get("result")
                .and_then(|result| result.get("_meta"))
                .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
                .is_none()
        );

        let instruction = post_mcp_json_with_show_detail_mode(
            &server_state,
            tool_call_body("catdesk_instruction", json!({})),
            ShowDetailMode::Disable,
        )
        .await;
        assert!(
            instruction
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.get("instructionText"))
                .and_then(Value::as_str)
                .is_some()
        );
        assert!(
            instruction
                .get("result")
                .and_then(|result| result.get("_meta"))
                .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
                .is_none()
        );
        assert!(instruction_called.is_called(&ClientSession::from_headers(&HeaderMap::new())));

        let allowed = post_mcp_json_with_show_detail_mode(
            &server_state,
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
            ShowDetailMode::Disable,
        )
        .await;
        assert_eq!(
            allowed
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.pointer("/files/0/text"))
                .and_then(Value::as_str),
            Some("hello world\n")
        );
        assert!(
            allowed
                .get("result")
                .and_then(|result| result.get("_meta"))
                .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
                .is_none()
        );

        let resources_list = post_mcp_json_with_show_detail_mode(
            &server_state,
            mcp_request_body("resources/list", json!({})),
            ShowDetailMode::Disable,
        )
        .await;
        assert_eq!(
            resources_list
                .get("result")
                .and_then(|result| result.get("resources"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );

        let resources_read = post_mcp_json_with_show_detail_mode(
            &server_state,
            mcp_request_body(
                "resources/read",
                json!({ "uri": "ui://widget/catdesk-dashboard.html" }),
            ),
            ShowDetailMode::Disable,
        )
        .await;
        assert_eq!(
            resources_read
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(-32602)
        );

        let app = app_state.lock().await;
        let usage = app.all_time_usage_totals();
        assert!(usage.total_tokens > 0);
        assert_eq!(usage.tool_call_count, 3);
        assert_eq!(app.session_usage_totals, usage);
        drop(app);

        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_usage_does_not_persist_config_on_hot_path() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-usage-hot-path-workspace");
        let config_root = unique_temp_path("catdesk-post-mcp-usage-hot-path-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state.clone()),
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let app = app_state.lock().await;
        assert_eq!(app.all_time_usage_totals().tool_call_count, 1);
        drop(app);
        assert!(
            !config_path.exists(),
            "usage accounting must not durably rewrite config.toml on the request hot path"
        );

        drop(server_state);
        drop(app_state);
        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_usage_is_persisted_after_the_hot_path_returns() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-usage-deferred-workspace");
        let config_root = unique_temp_path("catdesk-post-mcp-usage-deferred-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state.clone()),
            tool_call_body("read", json!({ "paths": ["hello.txt"] })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!config_path.exists(), "deferred persistence wrote synchronously");

        for _ in 0..100 {
            if config_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(config_path.exists(), "deferred usage persistence never wrote config.toml");

        let reloaded = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("reload persisted app state");
        assert_eq!(reloaded.all_time_usage_totals().tool_call_count, 1);
        assert!(reloaded.all_time_usage_totals().total_tokens > 0);
        drop(reloaded);

        drop(server_state);
        drop(app_state);
        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_accumulates_usage_from_widget_payload_meta() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-workspace");
        let config_root = unique_temp_path("catdesk-post-mcp-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = channel(crate::state::UI_EVENT_CAPACITY);
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            catdesk_instruction_called: InstructionGate::with_anonymous(true),
        };

        let response = post_mcp(
            State(server_state),
            tool_call_body("run_command", json!({ "command": "find ." })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let payload: Value = serde_json::from_slice(&body).expect("parse json body");

        let widget_payload = payload
            .get("result")
            .and_then(|result| result.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .expect("missing widget payload");
        let history_usage = widget_payload
            .get("historyTurnTokenUsage")
            .expect("missing history usage");
        assert!(
            history_usage
                .get("totalTokens")
                .and_then(Value::as_u64)
                .expect("history total tokens")
                > 0
        );
        assert_eq!(
            widget_payload
                .get("historyToolCallCount")
                .and_then(Value::as_u64),
            Some(1)
        );

        let app = app_state.lock().await;
        let all_time_usage = app.all_time_usage_totals();
        assert!(all_time_usage.total_tokens > 0);
        assert_eq!(all_time_usage.tool_call_count, 1);
        assert_eq!(app.session_usage_totals, all_time_usage);
        assert!(matches!(app.mode, Mode::Both));
        assert!(matches!(app.tool_mode, ToolMode::MultiTools));
        drop(app);

        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }
}

// ── POST /<slug>/mcp ────────────────────────────────────────

async fn post_mcp_http(
    State(s): State<ServerState>,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response<Body> {
    // Parsing is bounded by axum's body limit. Keep ping independent of busy
    // tool workers so clients can distinguish overload from a dead server.
    let metadata = serde_json::from_slice::<Value>(&body_bytes).ok();
    if metadata.as_ref().and_then(|v| v.get("method")).and_then(Value::as_str) == Some("ping") {
        let body = metadata.as_ref().unwrap();
        crate::diagnostics::rpc_request(body);
        if serde_json::from_value::<JsonRpcRequest>(body.clone()).is_err() {
            return jsonrpc_error_response(StatusCode::BAD_REQUEST, -32600, "Invalid JSON-RPC request");
        }
        if let Err(response) = validate_modern_request(body, &headers) { return response; }
        let _ = s.ui_events.try_send(ServerUiEvent::IncrementRequestCount);
        s.catdesk_instruction_called
            .set_remote_connected_nonblocking(s.app.clone(), true);
        let Some(id) = body.get("id").filter(|v| !v.is_null()) else {
            return Response::builder().status(StatusCode::ACCEPTED).body(Body::empty()).unwrap();
        };
        let mut result = json!({});
        mcp::decorate_modern_result("ping", &mut result);
        return Response::builder().status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"jsonrpc":"2.0", "id":id, "result":result}).to_string())).unwrap();
    }
    if let Some(body) = &metadata {
        crate::diagnostics::rpc_request(body);
    }
    let class = metadata
        .as_ref()
        .map(request_class)
        .unwrap_or(RequestClass::General);
    let id = metadata.as_ref().and_then(|v| v.get("id").cloned());
    match run_timed(
        async move { post_mcp_inner(State(s), body_bytes, &headers, None).await },
        request_deadline(class),
    )
    .await
    {
        Ok(result) => {
            let mut response = result.value;
            response
                .extensions_mut()
                .insert(crate::diagnostics::SchedulerTiming {
                    class: class.as_str(),
                    queue_wait_ms: result.timing.queue_wait_ms,
                    execution_ms: result.timing.execution_ms,
                    deadline_stage: result.timing.deadline_stage.map(|stage| stage.as_str()),
                });
            response
        }
        Err(error) => {
            use crate::request_workers::RequestFailure;
            let status = match &error.failure {
                RequestFailure::Deadline => StatusCode::GATEWAY_TIMEOUT,
                RequestFailure::Failed => StatusCode::INTERNAL_SERVER_ERROR,
            };
            crate::diagnostics::event(request_failure_event(&error.failure));
            let payload = mcp::JsonRpcResponse::error(
                id,
                -32000,
                format!("CatDesk {} request: {}", class.as_str(), error.failure),
            );
            let mut response = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap();
            response
                .extensions_mut()
                .insert(crate::diagnostics::RpcError(-32000));
            response
                .extensions_mut()
                .insert(crate::diagnostics::SchedulerTiming {
                    class: class.as_str(),
                    queue_wait_ms: error.timing.queue_wait_ms,
                    execution_ms: error.timing.execution_ms,
                    deadline_stage: error.timing.deadline_stage.map(|stage| stage.as_str()),
                });
            response
        }
    }
}

fn inferred_project_for_request_path(workspace_root: &str, path: &str) -> Option<PathBuf> {
    let resolved = command::resolve_workspace_path(workspace_root, Some(path)).ok()?;
    project_scope::infer_project_root(FsPath::new(workspace_root), &resolved).ok()
}

fn project_signal_for_request(req: &JsonRpcRequest, workspace_root: &str) -> Option<PathBuf> {
    if req.method != "tools/call" {
        return None;
    }
    let tool_name = req.params.get("name").and_then(Value::as_str)?;
    let arguments = req.params.get("arguments").and_then(Value::as_object)?;

    match tool_name {
        "run_command" | "start_command" => arguments
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(|path| inferred_project_for_request_path(workspace_root, path)),
        "write" | "edit" | "delete" | "read_image" => arguments
            .get("path")
            .and_then(Value::as_str)
            .and_then(|path| inferred_project_for_request_path(workspace_root, path)),
        "read" => {
            let paths = arguments.get("paths")?.as_array()?;
            let mut projects = paths
                .iter()
                .map(Value::as_str)
                .map(|path| path.and_then(|path| inferred_project_for_request_path(workspace_root, path)));
            let first = projects.next()??;
            projects.all(|project| project.as_ref() == Some(&first)).then_some(first)
        }
        "search" => {
            let project = arguments
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| inferred_project_for_request_path(workspace_root, path))?;
            let workspace = FsPath::new(workspace_root).canonicalize().ok()?;
            (project != workspace).then_some(project)
        }
        _ => None,
    }
}

fn tool_call_succeeded(resp: &mcp::JsonRpcResponse) -> bool {
    resp.error.is_none()
        && resp.result.as_ref().is_some_and(|result| {
            result.get("isError").and_then(Value::as_bool) != Some(true)
        })
}

fn record_project_selection(
    gate: &InstructionGate,
    session: &ClientSession,
    project: PathBuf,
) {
    match gate.set_active_project(session, project) {
        ProjectStateChange::Selected => crate::diagnostics::event("session_project_selected"),
        ProjectStateChange::Changed => crate::diagnostics::event("session_project_changed"),
        ProjectStateChange::Ignored | ProjectStateChange::Unchanged => {}
    }
}

async fn post_mcp_inner(
    State(s): State<ServerState>,
    body_bytes: Bytes,
    headers: &HeaderMap,
    show_detail_mode: Option<ShowDetailMode>,
) -> Response<Body> {
    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            let _ = s.ui_events.try_send(ServerUiEvent::Log {
                level: "ERROR",
                message: format!(
                    "← JSON-RPC parse error bytes={} message={}",
                    body_bytes.len(),
                    truncate_log_text(&e.to_string(), 240)
                ),
            });
            return jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                -32700,
                &format!("Parse error: {e}"),
            );
        }
    };
    if !body.is_object() {
        let _ = s.ui_events.try_send(ServerUiEvent::Log {
            level: "ERROR",
            message: "← JSON-RPC invalid request: expected a single message object".into(),
        });
        return jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            -32600,
            "Invalid request: expected a single JSON-RPC message object",
        );
    }

    crate::diagnostics::rpc_request(&body);
    let _ = s.ui_events.try_send(ServerUiEvent::IncrementRequestCount);
    let client_session = ClientSession::from_request(headers, Some(&body));
    let flow_id = client_session.flow_id.clone();

    let has_method = body.get("method").and_then(Value::as_str).is_some();
    if !has_method {
        let kind = if body.get("result").is_some() {
            "result"
        } else if body.get("error").is_some() {
            "error"
        } else {
            "unknown"
        };
        let mcp_path = {
            let app = s.app.lock().await;
            app.mcp_path()
        };
        let _ = s.ui_events.try_send(ServerUiEvent::Log {
            level: "INFO",
            message: format!(
                "→ POST {mcp_path} non-request JSON-RPC id={} kind={kind}",
                request_id(&body)
            ),
        });
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap();
    }

    let request_summary = summarize_request(&body);
    let request_flow_event = request_flow_label(&body);

    s.catdesk_instruction_called.note_remote_connected(true);
    s.app.lock().await.record_flow(
        &flow_id,
        std::slice::from_ref(&request_flow_event),
        FlowDirection::Forward,
    );

    let req: JsonRpcRequest = match serde_json::from_value(body.clone()) {
        Ok(r) => r,
        Err(e) => {
            let _ = s.ui_events.try_send(ServerUiEvent::Log {
                level: "ERROR",
                message: format!(
                    "← {request_summary} invalid-request message={}",
                    truncate_log_text(&e.to_string(), 240)
                ),
            });
            return jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                -32600,
                &format!("Invalid request: {e}"),
            );
        }
    };

    let (
        workspace_root,
        mascot_seed,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        ngrok_url,
        mcp_path,
        partner_binagotchy_seed,
        app_show_detail_mode,
    ) = {
        let app = s.app.lock().await;
        (
            app.workspace_root.clone(),
            app.mascot_seed,
            app.mode,
            app.tool_mode,
            app.set_catdesk_as_co_author,
            app.ngrok_url.clone(),
            app.mcp_path(),
            app.partner_binagotchy_seed.clone(),
            app.show_detail_mode,
        )
    };

    let _ = s.ui_events.try_send(ServerUiEvent::Log {
        level: "INFO",
        message: format!("→ POST {mcp_path} {request_summary}"),
    });

    if let Err(response) = validate_modern_request(&body, headers) {
        let _ = s.ui_events.try_send(ServerUiEvent::Log {
            level: "ERROR",
            message: format!("← POST {mcp_path} {request_summary} validation-error"),
        });
        return response;
    }

    let show_detail_mode = show_detail_mode.unwrap_or(app_show_detail_mode);
    let stored_active_project = s.catdesk_instruction_called.active_project(&client_session);
    let active_project = project_scope::valid_active_project(
        FsPath::new(&workspace_root),
        stored_active_project.as_deref(),
    );
    if stored_active_project.is_some() && active_project.is_none() {
        if s.catdesk_instruction_called.clear_active_project(&client_session) {
            crate::diagnostics::event("session_project_cleared");
        }
    }
    let project_signal = project_signal_for_request(&req, &workspace_root);
    let response = mcp::handle_request_with_session(
        &req,
        &workspace_root,
        mascot_seed,
        ngrok_url.as_deref(),
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        s.catdesk_instruction_called.is_called(&client_session),
        &s.command_jobs,
        &s.devtools,
        show_detail_mode,
        client_session.namespace(),
        active_project.as_deref(),
    )
    .await;

    let mut response_json: Option<Value> = None;
    if let Some(resp) = response {
        let mut resp = resp;
        if req.method == "tools/call" {
            if tool_call_succeeded(&resp)
                && let Some(project) = project_signal.clone()
            {
                record_project_selection(
                    &s.catdesk_instruction_called,
                    &client_session,
                    project,
                );
            }
            if req.params.get("name").and_then(Value::as_str) == Some("catdesk_instruction")
                && resp.error.is_none()
                && resp.result.as_ref().is_some_and(|result| {
                    result.get("isError").and_then(Value::as_bool) != Some(true)
                })
            {
                s.catdesk_instruction_called.mark_called(&client_session);
            }
            let turn_token_usage = turn_token_usage_for_response(&req, resp.result.as_ref());
            let usage_totals = {
                let mut app = s.app.lock().await;
                if let Some((tool_input_tokens, tool_output_tokens)) = turn_token_usage {
                    // Responses carry no model metadata yet; until it lands every turn
                    // is attributed to the unattributed bucket, priced at the legacy
                    // rate as an estimate.
                    let usage_bucket = usage_pricing::bucket_for_model(None);
                    app.record_turn_usage(&usage_bucket, tool_input_tokens, tool_output_tokens);
                    app.schedule_usage_persistence();
                    let _ = s.ui_events.try_send(ServerUiEvent::RecordTurnUsage {
                        flow_id: flow_id.clone(),
                        tool_input_tokens,
                        tool_output_tokens,
                    });
                }
                app.all_time_usage_totals()
            };
            attach_history_usage(&mut resp.result, &usage_totals);
            attach_catdesk_instruction_actions(
                &mut resp.result,
                ngrok_url.as_deref(),
                &mcp_path,
                mascot_seed,
                partner_binagotchy_seed.as_deref(),
            );
        }
        if let Some(result) = resp.result.as_mut() {
            mcp::decorate_modern_result(&req.method, result);
        }
        response_json = Some(serde_json::to_value(resp).unwrap());
    }

    if req.id.is_some() {
        let response_succeeded = response_json
            .as_ref()
            .is_some_and(|response| response.get("error").is_none());
        match req.method.as_str() {
            "server/discover" => {
                let _ = s
                    .ui_events
                    .try_send(ServerUiEvent::RecordBootstrapDiscoverResponse {
                        flow_id: flow_id.clone(),
                        success: response_succeeded,
                    });
            }
            "tools/list" => {
                let widgets = response_json
                    .as_ref()
                    .map(bootstrap_widgets_from_tools_list_response)
                    .unwrap_or_default();
                let _ = s
                    .ui_events
                    .try_send(ServerUiEvent::RecordBootstrapToolsListResponse {
                        flow_id: flow_id.clone(),
                        success: response_succeeded,
                        widgets,
                    });
            }
            "resources/read" => {
                if let Some(tool_name) = request_resource_uri(&body)
                    .filter(|uri| mcp::is_catdesk_widget_resource_uri(uri))
                    .and_then(|uri| query_param_value(uri, "toolName"))
                    .filter(|tool_name| !tool_name.is_empty())
                {
                    let _ = s
                        .ui_events
                        .try_send(ServerUiEvent::RecordBootstrapWidgetReadResponse {
                            flow_id: flow_id.clone(),
                            tool_name: tool_name.to_string(),
                            success: response_succeeded,
                        });
                }
            }
            _ => {}
        }

        s.app.lock().await.record_flow(
            &flow_id,
            std::slice::from_ref(&request_flow_event),
            FlowDirection::Backward,
        );
    }
    if let Some(ref resp_json) = response_json {
        let response_summary = summarize_response(&body, resp_json);
        let _ = s.ui_events.try_send(ServerUiEvent::Log {
            level: "INFO",
            message: format!("← POST {mcp_path} {response_summary}"),
        });
    }

    if req.id.is_none() {
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap();
    }

    let Some(response_json) = response_json else {
        return jsonrpc_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            -32603,
            "Internal error: request did not produce a JSON-RPC response",
        );
    };
    let response_status = if response_json
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
        == Some(-32601)
    {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    let response_body = serde_json::to_string(&response_json).unwrap();

    let mut builder = Response::builder();
    if let Some(code) = response_json
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
    {
        builder = builder.extension(crate::diagnostics::RpcError(code));
    }
    if req.method == "tools/call" {
        if let Some(result) = response_json.get("result") {
            builder = builder.extension(crate::diagnostics::ToolResult {
                is_error: result.get("isError").and_then(Value::as_bool),
                content_items: result
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::len),
            });
        }
    }
    builder
        .status(response_status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response_body))
        .unwrap()
}

// ── GET /<slug>/mcp — pure HTTP mode (no SSE) ───────────────

async fn get_mcp() -> Response<Body> {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .extension(crate::diagnostics::RpcError(-32601))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"GET SSE stream is disabled in pure HTTP mode"}}"#,
        ))
        .unwrap()
}

// ── DELETE /<slug>/mcp ──────────────────────────────────────

async fn delete_mcp(State(s): State<ServerState>, headers: HeaderMap) -> Response<Body> {
    let client_session = ClientSession::from_headers(&headers);
    let cancelled_jobs = if let Some(session_id) = client_session.namespace() {
        s.command_jobs.cancel_session(session_id).await
    } else {
        0
    };
    s.catdesk_instruction_called.forget(&client_session);
    let should_disconnect =
        client_session.namespace().is_none() || !s.catdesk_instruction_called.has_named_sessions();
    if should_disconnect {
        s.catdesk_instruction_called.note_remote_connected(false);
    }
    {
        let mut app = s.app.lock().await;
        if should_disconnect {
            app.set_remote_connected(false);
        }
        app.begin_flow_close(&client_session.flow_id);
    }
    let _ = s.ui_events.try_send(ServerUiEvent::Log {
        level: "INFO",
        message: format!(
            "DELETE mcp endpoint: session reset; signalled {cancelled_jobs} command job(s)"
        ),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"status":"ok"}"#))
        .unwrap()
}
