use serde_json::{Value, json};
use tiktoken_rs::o200k_base_singleton;

use crate::mcp::jsonrpc::{JsonRpcRequest, tool_arguments, tool_name_from_request};

#[derive(Clone, Default)]
pub(crate) struct TokenUsage {
    pub(crate) tool_input_tokens: u64,
    pub(crate) tool_output_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl TokenUsage {
    pub(crate) fn from_counts(tool_input_tokens: u64, tool_output_tokens: u64) -> Self {
        Self {
            tool_input_tokens,
            tool_output_tokens,
            total_tokens: tool_input_tokens.saturating_add(tool_output_tokens),
        }
    }
}


pub(crate) fn build_turn_token_payload(req: &JsonRpcRequest, tool_name: &str) -> Value {
    json!({
        "name": tool_name,
        "arguments": tool_arguments(req),
    })
}

pub(crate) fn estimate_tokens_o200k(text: &str) -> u64 {
    o200k_base_singleton()
        .encode_with_special_tokens(text)
        .len()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn estimate_value_tokens_o200k(value: &Value) -> u64 {
    match serde_json::to_string(value) {
        Ok(serialized) => estimate_tokens_o200k(&serialized),
        Err(_) => 0,
    }
}

pub(crate) fn estimate_turn_token_usage(req: &JsonRpcRequest, tool_name: &str, result: &Value) -> TokenUsage {
    let tool_input_payload = build_turn_token_payload(req, tool_name);
    let tool_input_tokens = estimate_value_tokens_o200k(&tool_input_payload);
    let tool_output_payload = sanitize_result_for_turn_token_count(result);
    let tool_output_tokens = estimate_value_tokens_o200k(&tool_output_payload);
    TokenUsage::from_counts(tool_input_tokens, tool_output_tokens)
}

pub(crate) fn estimate_turn_token_counts(req: &JsonRpcRequest, result: &Value) -> (u64, u64) {
    let tool_name = tool_name_from_request(req);
    let usage = estimate_turn_token_usage(req, &tool_name, result);
    (usage.tool_input_tokens, usage.tool_output_tokens)
}

pub(crate) fn sanitize_result_for_turn_token_count(result: &Value) -> Value {
    let mut sanitized = result.clone();
    let Some(obj) = sanitized.as_object_mut() else {
        return sanitized;
    };
    obj.remove("_meta");
    if let Some(content) = obj.get_mut("content").and_then(Value::as_array_mut) {
        for entry in content {
            if entry.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(entry) = entry.as_object_mut() {
                    entry.remove("data");
                }
            }
        }
    }
    sanitized
}

