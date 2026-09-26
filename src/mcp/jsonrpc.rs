use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ── JSON-RPC types ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn error(id: Option<Value>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }
}

/// Best-effort human/model-readable text derived from a tool's structured
/// payload. Error responses whose raw text is empty (or a placeholder like
/// "(no output)") fall back to this so the model still sees why the call
/// failed instead of an empty content array.
pub(crate) fn structured_content_text(structured: &Value) -> String {
    let Some(structured) = structured.as_object() else {
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
                parts.push(text.to_string());
            }
        }
    }

    if let Some(files) = structured.get("files").and_then(Value::as_array) {
        for file in files {
            let Some(error) = file.get("error").and_then(Value::as_str) else {
                continue;
            };
            let error = error.trim();
            if error.is_empty() {
                continue;
            }
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty());
            parts.push(match path {
                Some(path) => format!("{path}: {error}"),
                None => error.to_string(),
            });
        }
    }

    if parts.is_empty() && structured.get("timedOut").and_then(Value::as_bool) == Some(true) {
        parts.push("Command timed out.".to_string());
    } else if parts.is_empty() && structured.get("success").and_then(Value::as_bool) == Some(false)
    {
        if let Some(exit_code) = structured.get("exitCode").and_then(Value::as_i64) {
            parts.push(format!("Command failed with exit code {exit_code}."));
        }
    }

    parts.join("\n")
}

pub(crate) fn tool_response(
    req: &JsonRpcRequest,
    text: String,
    structured: Option<Value>,
    is_error: bool,
) -> JsonRpcResponse {
    let structured =
        structured.unwrap_or_else(|| tool_message_structured(req, text.clone(), is_error));
    let content_text = if is_error {
        let text = text.trim();
        if text.is_empty() || text == "(no output)" {
            let structured_text = structured_content_text(&structured);
            if structured_text.is_empty() {
                text.to_string()
            } else {
                structured_text
            }
        } else {
            text.to_string()
        }
    } else {
        String::new()
    };
    let content = if content_text.is_empty() {
        json!([])
    } else {
        json!([{ "type": "text", "text": content_text }])
    };
    let mut result = json!({
        "content": content,
        "structuredContent": structured
    });
    if let Some(obj) = result.as_object_mut() {
        if is_error {
            obj.insert("isError".to_string(), Value::Bool(true));
        }
    }
    JsonRpcResponse::success(req.id.clone(), result)
}

pub(crate) fn image_tool_success_response(
    req: &JsonRpcRequest,
    data: &[u8],
    mime_type: &str,
) -> JsonRpcResponse {
    // ChatGPT's MCP client drops the native image block when a tool result also
    // carries structuredContent (it projects the result through the metadata
    // object instead). take_screenshot proves the content-only shape survives
    // the bridge, so read_image must answer with pure multimodal content and no
    // structuredContent/outputSchema. Image dimensions are recoverable by the
    // client from the decoded bytes themselves.
    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "content": [{
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(data),
                "mimeType": mime_type,
            }],
        }),
    )
}

pub(crate) fn tool_message_structured(req: &JsonRpcRequest, message: String, is_error: bool) -> Value {
    json!({
        "toolName": tool_name_from_request(req),
        "message": message,
        "success": !is_error,
    })
}

pub(crate) fn tool_success_response_with_structured(
    req: &JsonRpcRequest,
    text: String,
    structured: Value,
) -> JsonRpcResponse {
    tool_response(req, text, Some(structured), false)
}

pub(crate) fn tool_error_response_with_structured(
    req: &JsonRpcRequest,
    text: String,
    structured: Value,
) -> JsonRpcResponse {
    tool_response(req, text, Some(structured), true)
}

pub(crate) fn tool_error_response(req: &JsonRpcRequest, text: String) -> JsonRpcResponse {
    tool_response(req, text, None, true)
}

pub(crate) fn tool_arguments(req: &JsonRpcRequest) -> Value {
    req.params.get("arguments").cloned().unwrap_or(json!({}))
}

pub(crate) fn tool_name_from_request(req: &JsonRpcRequest) -> String {
    req.params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown_tool")
        .to_string()
}

