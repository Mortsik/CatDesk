use base64::Engine as _;
use serde_json::{Value, json};

use crate::mcp::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, tool_arguments, tool_error_response_with_structured,
    tool_success_response_with_structured,
};
use crate::result_store::{LargeResultStore, ResultKind, StoreError};

// Store-range tools sit outside the dispatcher response budget and must
// self-bound everything they echo. The query is the only client-supplied
// string search_result returns, so it gets one deterministic limit enforced
// twice: as input validation (the schema's maxLength plus this handler) and
// as a cap on the echoed copy.
pub(crate) const MAX_SEARCH_RESULT_QUERY_CHARS: usize = 1024;

/// Output modes for read_result. `both` is the backwards-compatible default:
/// the lossless dataBase64 mirror always, plus `text` when the selected
/// bytes are valid UTF-8. `text` and `base64` each ship only one mirror so
/// transcript footprints drop by roughly half on UTF-8 ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadResultFormat {
    Text,
    Base64,
    Both,
}

impl ReadResultFormat {
    fn from_arguments(arguments: &Value) -> Result<Self, String> {
        match arguments.get("format") {
            None => Ok(Self::Both),
            Some(value) => match value.as_str() {
                Some("text") => Ok(Self::Text),
                Some("base64") => Ok(Self::Base64),
                Some("both") => Ok(Self::Both),
                _ => Err("Parameter format must be one of: text, base64, both".to_string()),
            },
        }
    }
}

pub(crate) fn handle_read_result(
    req: &JsonRpcRequest,
    workspace_root: &str,
    store: &LargeResultStore,
    session_namespace: Option<&str>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let result_id = match required_nonempty_string(&arguments, "result_id") {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    let offset = match optional_u64(&arguments, "offset", 0) {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    let max_bytes = match optional_usize(&arguments, "max_bytes", store.max_range_bytes()) {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    let format = match ReadResultFormat::from_arguments(&arguments) {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };

    match store.read_range(
        session_namespace,
        std::path::Path::new(workspace_root),
        result_id,
        offset,
        max_bytes,
    ) {
        Ok(range) => {
            // Text mode refuses ranges that are not complete valid UTF-8
            // instead of silently masking bytes or truncating a codepoint;
            // the client falls back to base64/both, which stay byte-exact.
            if format == ReadResultFormat::Text && range.text.is_none() {
                return text_range_not_utf8_response(req, range.offset, range.bytes.len());
            }
            let mut structured = json!({
                "toolName": "read_result",
                "resultId": range.metadata.result_id,
                "sizeBytes": range.metadata.size_bytes,
                "kind": kind_name(range.metadata.kind),
                "contentType": range.metadata.content_type,
                "createdAtMs": range.metadata.created_at_ms,
                "expiresAtMs": range.metadata.expires_at_ms,
                "offset": range.offset,
                "bytesReturned": range.bytes.len(),
                "nextOffset": range.next_offset,
                "eof": range.eof,
                "message": format!(
                    "Read {} bytes from stored result at offset {}.",
                    range.bytes.len(),
                    range.offset
                ),
                "success": true
            });
            let object = structured
                .as_object_mut()
                .expect("structured result must be object");
            if format != ReadResultFormat::Text {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&range.bytes);
                object.insert("dataBase64".to_string(), Value::String(encoded));
            }
            if format != ReadResultFormat::Base64 {
                if let Some(text) = range.text {
                    object.insert("text".to_string(), Value::String(text));
                }
            }
            tool_success_response_with_structured(req, String::new(), structured)
        }
        Err(error) => store_error_response(req, error),
    }
}

fn text_range_not_utf8_response(
    req: &JsonRpcRequest,
    offset: u64,
    bytes_returned: usize,
) -> JsonRpcResponse {
    let message = format!(
        "Requested range at offset {offset} ({bytes_returned} bytes) is not valid UTF-8; re-read it with format=base64 (or format=both) for lossless bytes."
    );
    tool_error_response_with_structured(
        req,
        message.clone(),
        json!({
            "toolName": crate::mcp::jsonrpc::tool_name_from_request(req),
            "errorCode": "text_range_not_utf8",
            "message": message,
            "success": false
        }),
    )
}

pub(crate) fn handle_search_result(
    req: &JsonRpcRequest,
    workspace_root: &str,
    store: &LargeResultStore,
    session_namespace: Option<&str>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let result_id = match required_nonempty_string(&arguments, "result_id") {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    let query = match required_nonempty_string(&arguments, "query") {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    if query.chars().count() > MAX_SEARCH_RESULT_QUERY_CHARS {
        return invalid_arguments(
            req,
            format!("Parameter query must not exceed {MAX_SEARCH_RESULT_QUERY_CHARS} characters"),
        );
    }
    let start_offset = match optional_u64(&arguments, "start_offset", 0) {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };
    let default_matches = store.max_search_matches().min(20);
    let max_matches = match optional_usize(&arguments, "max_matches", default_matches) {
        Ok(value) => value,
        Err(error) => return invalid_arguments(req, error),
    };

    match store.search(
        session_namespace,
        std::path::Path::new(workspace_root),
        result_id,
        query,
        start_offset,
        max_matches,
    ) {
        Ok(result) => {
            // Defense in depth: the input validation above already bounds the
            // query, but the echo must stay capped even if that check is ever
            // relaxed.
            let echoed_query: String = query.chars().take(MAX_SEARCH_RESULT_QUERY_CHARS).collect();
            let structured = json!({
                "toolName": "search_result",
                "resultId": result.metadata.result_id,
                "sizeBytes": result.metadata.size_bytes,
                "kind": kind_name(result.metadata.kind),
                "contentType": result.metadata.content_type,
                "createdAtMs": result.metadata.created_at_ms,
                "expiresAtMs": result.metadata.expires_at_ms,
                "query": echoed_query,
                "startOffset": start_offset,
                "matchCount": result.matches.len(),
                "matches": result.matches,
                "nextOffset": result.next_offset,
                "eof": result.eof,
                "message": format!("Found {} match(es) in stored result.", result.matches.len()),
                "success": true
            });
            tool_success_response_with_structured(req, String::new(), structured)
        }
        Err(error) => store_error_response(req, error),
    }
}

fn required_nonempty_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    let value = arguments
        .get(name)
        .ok_or_else(|| format!("Missing required parameter: {name}"))?
        .as_str()
        .ok_or_else(|| format!("Parameter {name} must be a string"))?;
    if value.is_empty() {
        return Err(format!("Parameter {name} must not be empty"));
    }
    Ok(value)
}

fn optional_u64(arguments: &Value, name: &str, default: u64) -> Result<u64, String> {
    match arguments.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("Parameter {name} must be a non-negative integer")),
    }
}

fn optional_usize(arguments: &Value, name: &str, default: usize) -> Result<usize, String> {
    let value = optional_u64(arguments, name, default as u64)?;
    usize::try_from(value).map_err(|_| format!("Parameter {name} is too large"))
}

fn invalid_arguments(req: &JsonRpcRequest, message: String) -> JsonRpcResponse {
    tool_error_response_with_structured(
        req,
        message.clone(),
        json!({
            "toolName": crate::mcp::jsonrpc::tool_name_from_request(req),
            "errorCode": "invalid_arguments",
            "message": message,
            "success": false
        }),
    )
}

fn store_error_response(req: &JsonRpcRequest, error: StoreError) -> JsonRpcResponse {
    let message = error.to_string();
    tool_error_response_with_structured(
        req,
        message.clone(),
        json!({
            "toolName": crate::mcp::jsonrpc::tool_name_from_request(req),
            "errorCode": error.code(),
            "message": message,
            "success": false
        }),
    )
}

fn kind_name(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::Text => "text",
        ResultKind::Binary => "binary",
    }
}
