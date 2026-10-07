use serde_json::{Value, json};
use tiktoken_rs::o200k_base_singleton;

use crate::mcp::jsonrpc::{JsonRpcRequest, tool_arguments, tool_name_from_request};
use crate::mcp::response_budget::DEFAULT_INLINE_RESPONSE_BYTES;

/// Serialized payloads up to this size are tokenized exactly; anything larger
/// falls back to the cheap bytes/4 estimate. This is a defense-in-depth size
/// cap only: BPE encode cost depends on the payload's pre-token shape, not
/// its size, so size alone is never a safety guarantee (see
/// [`exempt_from_response_budget`] for the shape that must never be encoded).
const EXACT_TOKENIZATION_LIMIT_BYTES: usize = 2 * DEFAULT_INLINE_RESPONSE_BYTES;

/// Results these tools return are exempt from the response budget gate
/// (src/mcp.rs keeps them uncut), so whatever size the caller asked for
/// reaches the estimator in full. Their ranges are newline-free base64, and
/// an unbroken homogeneous run (e.g. zero padding encoding to `AAAA…`) makes
/// the BPE encode quadratic AT ANY SIZE: measured at 36 s for a legal 109 KiB
/// serialized range that sits well under the size cap, and 93 s at 180 KiB.
pub(crate) fn exempt_from_response_budget(tool_name: &str) -> bool {
    matches!(tool_name, "read_result" | "search_result")
}

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
        Ok(serialized) => {
            if serialized.len() > EXACT_TOKENIZATION_LIMIT_BYTES {
                estimate_bytewise(&serialized)
            } else {
                estimate_tokens_o200k(&serialized)
            }
        }
        Err(_) => 0,
    }
}

/// Serialized-bytes/4 estimate: o200k averages ~4 bytes per token on prose
/// and code, which keeps usage accounting in the right order of magnitude
/// wherever the exact encode would be unbounded or pathological.
fn estimate_bytewise(serialized: &str) -> u64 {
    serialized.len() as u64 / 4
}

pub(crate) fn estimate_turn_token_usage(
    req: &JsonRpcRequest,
    tool_name: &str,
    result: &Value,
) -> TokenUsage {
    let tool_input_payload = build_turn_token_payload(req, tool_name);
    let tool_input_tokens = estimate_value_tokens_o200k(&tool_input_payload);
    let tool_output_payload = sanitize_result_for_turn_token_count(result);
    // Path decision, not size: exempt retrieval results are estimated purely
    // bytewise at ANY size — their full uncut payload reaches this estimator,
    // and a pathological pre-token shape costs tens of seconds regardless of
    // whether the payload fits under the size cap.
    let tool_output_tokens = if exempt_from_response_budget(tool_name) {
        serde_json::to_string(&tool_output_payload)
            .map(|serialized| estimate_bytewise(&serialized))
            .unwrap_or(0)
    } else {
        estimate_value_tokens_o200k(&tool_output_payload)
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversize_payload_estimate_uses_bytes_per_four() {
        // Above the exact-tokenization limit the estimate is the cheap
        // bytes/4 heuristic over the whole serialized payload — no BPE pass,
        // so a max-range exempt result can never block tools/call on one
        // giant quadratic pre-token.
        let oversize = json!({ "dataBase64": "x".repeat(EXACT_TOKENIZATION_LIMIT_BYTES + 1) });
        let serialized = serde_json::to_string(&oversize).expect("serialize");
        assert!(serialized.len() > EXACT_TOKENIZATION_LIMIT_BYTES);
        assert_eq!(
            estimate_value_tokens_o200k(&oversize),
            serialized.len() as u64 / 4
        );
    }

    #[test]
    fn exempt_retrieval_results_estimate_bytewise_at_any_size() {
        // Path decision: an exempt tool's result goes bytewise even BELOW the
        // size cap — a homogeneous zero-padding range measured 36 s under the
        // cap — while the same payload for a budgeted tool stays exact.
        let small_result = json!({
            "structuredContent": { "dataBase64": "QUJDREVG" }
        });
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "read_result",
                "arguments": { "result_id": "probe", "offset": 0 }
            }),
        };
        let exempt_usage = estimate_turn_token_usage(&req, "read_result", &small_result);
        let bytewise = serde_json::to_string(&sanitize_result_for_turn_token_count(&small_result))
            .expect("serialize")
            .len() as u64
            / 4;
        assert_eq!(exempt_usage.tool_output_tokens, bytewise);

        let budgeted_usage = estimate_turn_token_usage(&req, "read", &small_result);
        let exact =
            estimate_value_tokens_o200k(&sanitize_result_for_turn_token_count(&small_result));
        assert_eq!(budgeted_usage.tool_output_tokens, exact);
        assert_ne!(bytewise, exact, "the two paths must be observably distinct");
    }

    #[test]
    fn bounded_payload_estimate_stays_exact_o200k() {
        // Small (and every budget-cut) result keeps the exact o200k count;
        // the heuristic only engages beyond twice the inline budget.
        let bounded = json!({
            "content": [],
            "structuredContent": { "toolName": "read", "text": "hello tokens" }
        });
        let serialized = serde_json::to_string(&bounded).expect("serialize");
        assert!(serialized.len() <= EXACT_TOKENIZATION_LIMIT_BYTES);
        assert_eq!(
            estimate_value_tokens_o200k(&bounded),
            estimate_tokens_o200k(&serialized)
        );
    }
}
