use base64::Engine as _;
use serde_json::{Value, json};

use crate::mcp::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, tool_arguments, tool_error_response_with_structured,
    tool_success_response_with_structured,
};
use crate::result_store::{LargeResultStore, ResultKind, StoreError};

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

    match store.read_range(
        session_namespace,
        std::path::Path::new(workspace_root),
        result_id,
        offset,
        max_bytes,
    ) {
        Ok(range) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&range.bytes);
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
                "dataBase64": encoded,
                "nextOffset": range.next_offset,
                "eof": range.eof,
                "message": format!(
                    "Read {} bytes from stored result at offset {}.",
                    range.bytes.len(),
                    range.offset
                ),
                "success": true
            });
            if let Some(text) = range.text {
                structured
                    .as_object_mut()
                    .expect("structured result must be object")
                    .insert("text".to_string(), Value::String(text));
            }
            tool_success_response_with_structured(req, String::new(), structured)
        }
        Err(error) => store_error_response(req, error),
    }
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
            let structured = json!({
                "toolName": "search_result",
                "resultId": result.metadata.result_id,
                "sizeBytes": result.metadata.size_bytes,
                "kind": kind_name(result.metadata.kind),
                "contentType": result.metadata.content_type,
                "createdAtMs": result.metadata.created_at_ms,
                "expiresAtMs": result.metadata.expires_at_ms,
                "query": query,
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
