use base64::Engine as _;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

use crate::mcp::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::mascot;
use crate::perf_metrics::{self, CacheKind};
use crate::state::ShowDetailMode;

#[cfg(test)]
use crate::mcp::current_show_detail_mode;
use crate::mcp::{current_token_stats_layout, current_widget_corner_style};

const SERVER_NAME: &str = "catdesk";
const SERVER_VERSION: &str = "4.0.0";
pub(crate) const MODERN_MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";
pub(crate) const UI_TEMPLATE_URI: &str = "ui://widget/catdesk-dashboard.html";
const WIDGET_RESOURCE_REVISION: u32 = 6;
const UI_TEMPLATE_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub(crate) const WIDGET_PAYLOAD_META_KEY: &str = "catdesk/widgetPayload";
pub(crate) const CATDESK_WIDGET_HTML: &str = include_str!("../widget/catdesk_dashboard.html");
const REENABLE_WIDGET_PNG: &[u8] = include_bytes!("../widget/assets/reenable_widget.png");
const REFRESH_CATDESK_PNG: &[u8] = include_bytes!("../widget/assets/refresh_catdesk.png");
const REMOVE_CATDESK_PNG: &[u8] = include_bytes!("../widget/assets/remove_catdesk.png");
static REENABLE_WIDGET_IMAGE: OnceLock<String> = OnceLock::new();
static REFRESH_CATDESK_IMAGE: OnceLock<String> = OnceLock::new();
static REMOVE_CATDESK_IMAGE: OnceLock<String> = OnceLock::new();
const WIDGET_RESOURCE_URI_PLACEHOLDER: &str = "__catdeskWidgetResourceUriPlaceholder__";
pub(crate) const REENABLE_WIDGET_IMAGE_PLACEHOLDER: &str = "__catdeskReenableWidgetImageDataUriPlaceholder__";
pub(crate) const REFRESH_CATDESK_IMAGE_PLACEHOLDER: &str = "__catdeskRefreshCatdeskImageDataUriPlaceholder__";
pub(crate) const REMOVE_CATDESK_IMAGE_PLACEHOLDER: &str = "__catdeskRemoveCatdeskImageDataUriPlaceholder__";
pub(crate) const INITIAL_TOKEN_STATS_LAYOUT_PLACEHOLDER: &str =
    "__catdeskInitialTokenStatsLayoutPlaceholder__";
pub(crate) const INITIAL_TOOL_NAME_PLACEHOLDER: &str = "__catdeskInitialToolNamePlaceholder__";
pub(crate) const INITIAL_MASCOT_OUTLINE_PLACEHOLDER: &str = "__catdeskInitialMascotOutlinePlaceholder__";

fn server_capabilities(show_detail_mode: ShowDetailMode) -> Value {
    if show_detail_mode == ShowDetailMode::Disable {
        json!({
            "tools": { "listChanged": false }
        })
    } else {
        json!({
            "tools": { "listChanged": false },
            "resources": { "listChanged": false }
        })
    }
}

pub(crate) fn handle_server_discover(
    req: &JsonRpcRequest,
    show_detail_mode: ShowDetailMode,
) -> JsonRpcResponse {
    let mut result = json!({
        "supportedVersions": [MODERN_MCP_PROTOCOL_VERSION],
        "capabilities": server_capabilities(show_detail_mode),
    });
    decorate_modern_result("server/discover", &mut result);
    JsonRpcResponse::success(req.id.clone(), result)
}

pub(crate) fn decorate_modern_result(method: &str, result: &mut Value) {
    let Some(result_obj) = result.as_object_mut() else {
        return;
    };
    result_obj.insert("resultType".to_string(), json!("complete"));

    if matches!(
        method,
        "server/discover"
            | "tools/list"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
            | "prompts/list"
    ) {
        result_obj.insert("ttlMs".to_string(), json!(0));
        result_obj.insert("cacheScope".to_string(), json!("private"));
    }
    if result_obj.get("nextCursor").is_some_and(Value::is_null) {
        result_obj.remove("nextCursor");
    }

    let meta = result_obj
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    if let Some(meta_obj) = meta.as_object_mut() {
        meta_obj.insert(
            SERVER_INFO_META_KEY.to_string(),
            json!({ "name": SERVER_NAME, "version": SERVER_VERSION }),
        );
    }
}

fn widget_resource_ui_meta(public_base_url: Option<&str>) -> Value {
    let mut ui = Map::new();
    ui.insert("prefersBorder".to_string(), Value::Bool(false));
    if let Some(origin) = public_base_url.filter(|value| !value.is_empty()) {
        ui.insert(
            "csp".to_string(),
            json!({
                "connectDomains": [origin],
                "resourceDomains": [],
            }),
        );
    }
    Value::Object(ui)
}

pub(crate) fn handle_resources_list_with_show_detail_mode(
    req: &JsonRpcRequest,
    public_base_url: Option<&str>,
    show_detail_mode: ShowDetailMode,
) -> JsonRpcResponse {
    if show_detail_mode == ShowDetailMode::Disable {
        return JsonRpcResponse::success(
            req.id.clone(),
            json!({
                "resources": [],
                "nextCursor": null
            }),
        );
    }

    let ui_meta = widget_resource_ui_meta(public_base_url);
    let resource_uri = current_widget_resource_uri();
    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "resources": [
                {
                    "uri": resource_uri,
                    "name": "CatDesk dashboard widget",
                    "description": "Embedded ChatGPT widget for CatDesk status and timeline data.",
                    "mimeType": UI_TEMPLATE_MIME_TYPE,
                    "_meta": { "ui": ui_meta }
                }
            ],
            "nextCursor": null
        }),
    )
}

pub(crate) fn current_widget_resource_uri() -> String {
    current_widget_resource_uri_for_tool("")
}

pub(crate) fn is_catdesk_widget_resource_uri(uri: &str) -> bool {
    uri == UI_TEMPLATE_URI || uri.starts_with(&format!("{UI_TEMPLATE_URI}?"))
}

pub(crate) fn current_widget_resource_uri_for_tool(tool_name: &str) -> String {
    let token_stats_layout = current_token_stats_layout();
    let widget_corner_style = current_widget_corner_style();
    if tool_name.is_empty() {
        return format!(
            "{UI_TEMPLATE_URI}?widgetRevision={WIDGET_RESOURCE_REVISION}&tokenStatsLayout={}&widgetCornerStyle={}",
            token_stats_layout.as_str(),
            widget_corner_style.as_str()
        );
    }
    format!(
        "{UI_TEMPLATE_URI}?widgetRevision={WIDGET_RESOURCE_REVISION}&tokenStatsLayout={}&widgetCornerStyle={}&toolName={}",
        token_stats_layout.as_str(),
        widget_corner_style.as_str(),
        tool_name
    )
}
fn query_param_value<'a>(resource_uri: &'a str, key: &str) -> Option<&'a str> {
    let query = resource_uri.split_once('?')?.1;
    query.split('&').find_map(|part| {
        let (param_key, param_value) = part.split_once('=')?;
        if param_key == key {
            Some(param_value)
        } else {
            None
        }
    })
}

fn initial_tool_name_from_resource_uri(resource_uri: &str) -> &str {
    query_param_value(resource_uri, "toolName").unwrap_or_default()
}

pub(crate) fn cached_data_uri<'a>(cache: &'a OnceLock<String>, bytes: &[u8]) -> &'a str {
    let mut missed = false;
    let uri = cache.get_or_init(|| {
        missed = true;
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    });
    if missed {
        perf_metrics::record_cache_miss(CacheKind::DataUri);
    } else {
        perf_metrics::record_cache_hit(CacheKind::DataUri);
    }
    uri.as_str()
}

fn render_widget_html(resource_uri: &str, mascot_seed: u64) -> String {
    let initial_mascot_outline =
        serde_json::to_string(&mascot::build_widget_mascot_outline(mascot_seed))
            .unwrap_or_else(|_| "{}".to_string());
    let reenable_widget_image = cached_data_uri(&REENABLE_WIDGET_IMAGE, REENABLE_WIDGET_PNG);
    let refresh_catdesk_image = cached_data_uri(&REFRESH_CATDESK_IMAGE, REFRESH_CATDESK_PNG);
    let remove_catdesk_image = cached_data_uri(&REMOVE_CATDESK_IMAGE, REMOVE_CATDESK_PNG);
    CATDESK_WIDGET_HTML
        .replace(WIDGET_RESOURCE_URI_PLACEHOLDER, resource_uri)
        .replace(REENABLE_WIDGET_IMAGE_PLACEHOLDER, reenable_widget_image)
        .replace(REFRESH_CATDESK_IMAGE_PLACEHOLDER, refresh_catdesk_image)
        .replace(REMOVE_CATDESK_IMAGE_PLACEHOLDER, remove_catdesk_image)
        .replace(
            INITIAL_TOKEN_STATS_LAYOUT_PLACEHOLDER,
            current_token_stats_layout().as_str(),
        )
        .replace(
            INITIAL_TOOL_NAME_PLACEHOLDER,
            initial_tool_name_from_resource_uri(resource_uri),
        )
        .replace(INITIAL_MASCOT_OUTLINE_PLACEHOLDER, &initial_mascot_outline)
}

#[cfg(test)]
pub(crate) fn handle_resources_read(
    req: &JsonRpcRequest,
    public_base_url: Option<&str>,
    mascot_seed: u64,
) -> JsonRpcResponse {
    handle_resources_read_with_show_detail_mode(
        req,
        public_base_url,
        mascot_seed,
        current_show_detail_mode(),
    )
}

pub(crate) fn handle_resources_read_with_show_detail_mode(
    req: &JsonRpcRequest,
    public_base_url: Option<&str>,
    mascot_seed: u64,
    show_detail_mode: ShowDetailMode,
) -> JsonRpcResponse {
    let uri = req
        .params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if show_detail_mode == ShowDetailMode::Disable {
        return JsonRpcResponse::error(req.id.clone(), -32602, format!("Unknown resource: {uri}"));
    }
    let text = if is_catdesk_widget_resource_uri(uri) {
        render_widget_html(uri, mascot_seed)
    } else {
        return JsonRpcResponse::error(req.id.clone(), -32602, format!("Unknown resource: {uri}"));
    };
    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "contents": [{
                "uri": uri,
                "mimeType": UI_TEMPLATE_MIME_TYPE,
                "text": text,
                "_meta": { "ui": widget_resource_ui_meta(public_base_url) }
            }]
        }),
    )
}

// ── tools/list ──────────────────────────────────────────────
