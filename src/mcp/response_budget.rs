use serde_json::{Map, Value, json};
use std::path::Path;

use crate::result_store::{LargeResultStore, ResultMetadata, StoreError};

pub(crate) const DEFAULT_INLINE_RESPONSE_BYTES: usize = 64 * 1024;
const POLICY_NAME: &str = "catdesk-mcp-v1";
const TEXT_PREVIEW_BYTES: usize = 4 * 1024;
const ERROR_TEXT_PREVIEW_BYTES: usize = 8 * 1024;
const ARRAY_PREVIEW_ITEMS: usize = 12;
const MAX_OMISSION_DETAILS: usize = 16;
const FALLBACK_TEXT_PREVIEW_BYTES: usize = 2 * 1024;

#[derive(Clone, Debug, Default)]
struct OmissionTracker {
    total: usize,
    details: Vec<Value>,
}

impl OmissionTracker {
    fn push(&mut self, detail: Value) {
        self.total = self.total.saturating_add(1);
        if self.details.len() < MAX_OMISSION_DETAILS {
            self.details.push(detail);
        }
    }

    fn text(
        &mut self,
        path: &str,
        kind: &'static str,
        original_bytes: usize,
        preview_bytes: usize,
    ) {
        self.push(json!({
            "path": path,
            "kind": kind,
            "originalBytes": original_bytes,
            "previewBytes": preview_bytes,
            "omittedBytes": original_bytes.saturating_sub(preview_bytes)
        }));
    }

    fn array(
        &mut self,
        path: &str,
        original_items: usize,
        preview_items: usize,
        original_bytes: usize,
        preview_bytes: usize,
    ) {
        self.push(json!({
            "path": path,
            "kind": "array",
            "originalItems": original_items,
            "previewItems": preview_items,
            "omittedItems": original_items.saturating_sub(preview_items),
            "originalBytes": original_bytes,
            "previewBytes": preview_bytes
        }));
    }

    fn object_fields(&mut self, path: &str, omitted_fields: usize) {
        if omitted_fields > 0 {
            self.push(json!({
                "path": path,
                "kind": "object",
                "omittedFields": omitted_fields
            }));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BudgetCandidate {
    original: Vec<u8>,
    preview: Value,
    omissions: OmissionTracker,
    is_error: bool,
}

impl BudgetCandidate {
    pub(crate) fn payload(&self) -> &[u8] {
        &self.original
    }

    pub(crate) fn finish(mut self, output_ref: &str) -> Value {
        self.attach_manifest(output_ref);

        if serialized_len(&self.preview) > DEFAULT_INLINE_RESPONSE_BYTES {
            self.detach_manifest();
            reduce_to_essentials(&mut self.preview, self.is_error, &mut self.omissions);
            self.attach_manifest(output_ref);
        }

        if serialized_len(&self.preview) > DEFAULT_INLINE_RESPONSE_BYTES {
            self.detach_manifest();
            hard_minimal_preview(&mut self.preview, self.is_error, &mut self.omissions);
            self.attach_manifest(output_ref);
        }

        self.preview
    }

    fn detach_manifest(&mut self) {
        if let Some(object) = self.preview.as_object_mut() {
            object.remove("responseBudget");
        }
    }

    fn attach_manifest(&mut self, output_ref: &str) {
        let preview_bytes = serialized_len(&self.preview);
        if let Some(object) = self.preview.as_object_mut() {
            object.insert(
                "responseBudget".to_string(),
                json!({
                    "policy": POLICY_NAME,
                    "limitBytes": DEFAULT_INLINE_RESPONSE_BYTES,
                    "originalBytes": self.original.len(),
                    "previewBytes": preview_bytes,
                    "omittedBytes": self.original.len().saturating_sub(preview_bytes),
                    "outputRef": output_ref,
                    "contentType": "application/json",
                    "retrieval": {
                        "tool": "read_result",
                        "arguments": { "result_id": output_ref }
                    },
                    "search": {
                        "tool": "search_result",
                        "arguments": { "result_id": output_ref }
                    },
                    "preview": {
                        "strategy": "structured-head-tail",
                        "omissionCount": self.omissions.total,
                        "omissions": self.omissions.details
                    }
                }),
            );
        }
    }
}

pub(crate) fn apply_response_budget(
    result: &mut Value,
    is_error: bool,
    store: &LargeResultStore,
    owner_session: Option<&str>,
    workspace_root: &Path,
) -> Result<Option<ResultMetadata>, StoreError> {
    let Some(candidate) = prepare_response_budget(result, is_error) else {
        return Ok(None);
    };

    let stored = store.put(
        owner_session,
        workspace_root,
        candidate.payload(),
        Some("application/json"),
    )?;
    let metadata = stored.metadata;
    *result = candidate.finish(&metadata.result_id);
    Ok(Some(metadata))
}

pub(crate) fn prepare_response_budget(result: &Value, is_error: bool) -> Option<BudgetCandidate> {
    let original = serde_json::to_vec(result).ok()?;
    if original.len() <= DEFAULT_INLINE_RESPONSE_BYTES || has_native_non_text_content(result) {
        return None;
    }

    let mut preview = result.clone();
    let mut omissions = OmissionTracker::default();
    compact_value(
        &mut preview,
        "",
        None,
        is_error,
        TEXT_PREVIEW_BYTES,
        &mut omissions,
    );

    Some(BudgetCandidate {
        original,
        preview,
        omissions,
        is_error,
    })
}

fn compact_value(
    value: &mut Value,
    path: &str,
    field_name: Option<&str>,
    is_error: bool,
    text_limit: usize,
    omissions: &mut OmissionTracker,
) {
    match value {
        Value::String(text) => {
            let limit = if is_error && is_diagnostic_field(field_name, path) {
                ERROR_TEXT_PREVIEW_BYTES.max(text_limit)
            } else {
                text_limit
            };
            if text.len() > limit {
                let original_bytes = text.len();
                let preview = head_tail_preview(text, limit);
                let preview_bytes = preview.len();
                let kind = if is_blob_field(field_name) {
                    "blob"
                } else {
                    "text"
                };
                *text = preview;
                omissions.text(path, kind, original_bytes, preview_bytes);
            }
        }
        Value::Array(items) => {
            let original_items = items.len();
            let original_bytes = serde_json::to_vec(&*items).map_or(0, |bytes| bytes.len());

            if original_items > ARRAY_PREVIEW_ITEMS {
                let edge = ARRAY_PREVIEW_ITEMS / 2;
                let tail_start = original_items.saturating_sub(edge);
                let mut kept = Vec::with_capacity(ARRAY_PREVIEW_ITEMS);
                kept.extend(items.iter().take(edge).cloned());
                kept.extend(items.iter().skip(tail_start).cloned());
                *items = kept;
            }

            for (index, item) in items.iter_mut().enumerate() {
                let child_path = json_pointer_child(path, &index.to_string());
                compact_value(item, &child_path, None, is_error, text_limit, omissions);
            }

            if original_items > items.len() {
                omissions.array(
                    path,
                    original_items,
                    items.len(),
                    original_bytes,
                    serde_json::to_vec(&*items).map_or(0, |bytes| bytes.len()),
                );
            }
        }
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let child_path = json_pointer_child(path, key);
                compact_value(
                    child,
                    &child_path,
                    Some(key),
                    is_error,
                    text_limit,
                    omissions,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn reduce_to_essentials(value: &mut Value, is_error: bool, omissions: &mut OmissionTracker) {
    compact_value(
        value,
        "",
        None,
        is_error,
        FALLBACK_TEXT_PREVIEW_BYTES,
        omissions,
    );

    let Some(root) = value.as_object_mut() else {
        return;
    };

    if let Some(structured) = root
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        let original_fields = structured.len();
        structured.retain(|key, value| {
            is_essential_key(key)
                || (is_error && is_diagnostic_key(key))
                || value_is_small_scalar(value)
        });
        omissions.object_fields(
            "/structuredContent",
            original_fields.saturating_sub(structured.len()),
        );

        for (key, child) in structured.iter_mut() {
            if let Value::String(text) = child {
                if text.len() > FALLBACK_TEXT_PREVIEW_BYTES {
                    let original_bytes = text.len();
                    let preview = head_tail_preview(text, FALLBACK_TEXT_PREVIEW_BYTES);
                    let preview_bytes = preview.len();
                    let path = json_pointer_child("/structuredContent", key);
                    let kind = if is_blob_field(Some(key)) {
                        "blob"
                    } else {
                        "text"
                    };
                    *text = preview;
                    omissions.text(&path, kind, original_bytes, preview_bytes);
                }
            }
        }
    }

    if !is_error {
        if root
            .get("content")
            .is_some_and(|content| serialized_len(content) > 2048)
        {
            root.insert("content".to_string(), Value::Array(Vec::new()));
            omissions.object_fields("/content", 1);
        }
    } else {
        compact_error_content(root, FALLBACK_TEXT_PREVIEW_BYTES, omissions);
    }
}

fn hard_minimal_preview(value: &mut Value, is_error: bool, omissions: &mut OmissionTracker) {
    let mut replacement = Map::new();

    if let Some(root) = value.as_object() {
        if let Some(is_error_value) = root.get("isError") {
            replacement.insert("isError".to_string(), is_error_value.clone());
        }

        if is_error {
            if let Some(content) = root.get("content") {
                let mut content = content.clone();
                if let Some(items) = content.as_array_mut() {
                    items.truncate(1);
                }
                compact_value(
                    &mut content,
                    "/content",
                    Some("content"),
                    true,
                    FALLBACK_TEXT_PREVIEW_BYTES,
                    omissions,
                );
                replacement.insert("content".to_string(), content);
            }
        } else {
            replacement.insert("content".to_string(), Value::Array(Vec::new()));
        }

        if let Some(structured) = root.get("structuredContent").and_then(Value::as_object) {
            let mut minimal = Map::new();
            for (key, child) in structured {
                if !is_essential_key(key) && !(is_error && is_diagnostic_key(key)) {
                    continue;
                }
                if minimal.len() >= 32 {
                    break;
                }
                match child {
                    Value::String(text) => {
                        minimal.insert(key.clone(), Value::String(head_tail_preview(text, 1024)));
                    }
                    Value::Null | Value::Bool(_) | Value::Number(_) => {
                        minimal.insert(key.clone(), child.clone());
                    }
                    Value::Array(_) | Value::Object(_) => {}
                }
            }
            replacement.insert("structuredContent".to_string(), Value::Object(minimal));
        }
    }

    omissions.object_fields("/", 1);
    *value = Value::Object(replacement);
}

fn compact_error_content(
    root: &mut Map<String, Value>,
    limit: usize,
    omissions: &mut OmissionTracker,
) {
    let Some(content) = root.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };

    if content.len() > 1 {
        let original = content.len();
        content.truncate(1);
        omissions.array("/content", original, 1, 0, 0);
    }

    let original_text = content
        .first()
        .and_then(Value::as_object)
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(text) = original_text else {
        return;
    };
    if text.len() <= limit {
        return;
    }

    let preview = head_tail_preview(&text, limit);
    let preview_bytes = preview.len();
    if let Some(text_value) = content
        .first_mut()
        .and_then(Value::as_object_mut)
        .and_then(|item| item.get_mut("text"))
    {
        *text_value = Value::String(preview);
    }
    omissions.text("/content/0/text", "text", text.len(), preview_bytes);
}

fn has_native_non_text_content(result: &Value) -> bool {
    result
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "text")
            })
        })
}

fn is_blob_field(field_name: Option<&str>) -> bool {
    field_name.is_some_and(|name| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "data"
                | "blob"
                | "body"
                | "payload"
                | "base64"
                | "html"
                | "snapshot"
                | "binary"
                | "bytes"
        )
    })
}

fn is_diagnostic_field(field_name: Option<&str>, path: &str) -> bool {
    field_name.is_some_and(is_diagnostic_key) || path.starts_with("/content/")
}

fn is_diagnostic_key(key: &str) -> bool {
    matches!(
        key,
        "message" | "error" | "stderr" | "stdout" | "text" | "instructionText"
    )
}

fn is_essential_key(key: &str) -> bool {
    matches!(
        key,
        "toolName"
            | "success"
            | "status"
            | "state"
            | "exitCode"
            | "timedOut"
            | "jobId"
            | "id"
            | "path"
            | "cwd"
            | "command"
            | "mode"
            | "nextCursor"
            | "hasMoreOutput"
            | "mimeType"
            | "width"
            | "height"
    ) || key.ends_with("Id")
        || key.ends_with("Ref")
        || key.ends_with("Count")
        || key.ends_with("Bytes")
        || key.ends_with("Truncated")
}

fn value_is_small_scalar(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(text) => text.len() <= 256,
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn head_tail_preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut marker = "\n… <omitted bytes; see responseBudget.outputRef> …\n".to_string();
    let mut head_end = 0;
    let mut tail_start = text.len();

    for _ in 0..3 {
        let remaining = max_bytes.saturating_sub(marker.len());
        let head_budget = remaining / 2;
        let tail_budget = remaining.saturating_sub(head_budget);
        head_end = floor_char_boundary(text, head_budget);
        tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_budget));
        if tail_start < head_end {
            tail_start = head_end;
        }
        let omitted = tail_start.saturating_sub(head_end);
        marker = format!("\n… <omitted {omitted} bytes; see responseBudget.outputRef> …\n");
    }

    let mut preview = String::with_capacity(max_bytes);
    preview.push_str(&text[..head_end]);
    preview.push_str(&marker);
    preview.push_str(&text[tail_start..]);

    while preview.len() > max_bytes && head_end > 0 {
        head_end = previous_char_boundary(text, head_end);
        preview.clear();
        preview.push_str(&text[..head_end]);
        preview.push_str(&marker);
        preview.push_str(&text[tail_start..]);
    }
    preview
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn previous_char_boundary(text: &str, index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    floor_char_boundary(text, index.saturating_sub(1))
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn json_pointer_child(path: &str, token: &str) -> String {
    let escaped = token.replace('~', "~0").replace('/', "~1");
    if path.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{path}/{escaped}")
    }
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest<'a>(value: &'a Value) -> &'a Value {
        value.get("responseBudget").expect("missing responseBudget")
    }

    fn oversized_text(prefix: &str, suffix: &str) -> String {
        format!("{prefix}{}{}", "中🙂".repeat(24_000), suffix)
    }

    #[test]
    fn small_results_are_left_byte_for_byte_unchanged() {
        let result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "search",
                "success": true,
                "matchCount": 2,
                "searchResults": [{"path": "a.rs"}, {"path": "b.rs"}]
            }
        });
        let before = serde_json::to_vec(&result).unwrap();

        assert!(prepare_response_budget(&result, false).is_none());
        assert_eq!(serde_json::to_vec(&result).unwrap(), before);
    }

    #[test]
    fn oversized_text_keeps_unicode_safe_head_and_tail_and_retains_full_payload() {
        let full = oversized_text("HEAD-SENTINEL\n", "\nTAIL-SENTINEL");
        let result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "run_command",
                "success": true,
                "exitCode": 0,
                "stdout": full,
                "stderr": ""
            }
        });

        let candidate = prepare_response_budget(&result, false).expect("must budget");
        assert_eq!(
            serde_json::from_slice::<Value>(candidate.payload()).unwrap(),
            result
        );

        let preview = candidate.finish("lr_test_text");
        let stdout = preview
            .pointer("/structuredContent/stdout")
            .and_then(Value::as_str)
            .expect("stdout preview");
        assert!(stdout.starts_with("HEAD-SENTINEL"));
        assert!(stdout.ends_with("TAIL-SENTINEL"));
        assert!(
            stdout.len()
                < result
                    .pointer("/structuredContent/stdout")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .len()
        );
        assert!(serde_json::to_vec(&preview).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
        assert_eq!(
            manifest(&preview).get("outputRef").and_then(Value::as_str),
            Some("lr_test_text")
        );
        assert!(
            manifest(&preview)
                .pointer("/preview/omissions")
                .and_then(Value::as_array)
                .is_some_and(|items| items
                    .iter()
                    .any(|item| item.get("kind") == Some(&json!("text"))))
        );
    }

    #[test]
    fn oversized_arrays_keep_status_identity_and_both_ends() {
        let entries = (0..4_000)
            .map(|index| json!({"index": index, "path": format!("src/file-{index:04}.rs")}))
            .collect::<Vec<_>>();
        let result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "search",
                "success": true,
                "matchCount": 4_000,
                "searchTruncated": false,
                "searchResults": entries
            }
        });

        let candidate = prepare_response_budget(&result, false).expect("must budget");
        let preview = candidate.finish("lr_test_array");
        let structured = preview.get("structuredContent").unwrap();
        assert_eq!(
            structured.get("toolName").and_then(Value::as_str),
            Some("search")
        );
        assert_eq!(
            structured.get("success").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            structured.get("matchCount").and_then(Value::as_u64),
            Some(4_000)
        );

        let kept = structured
            .get("searchResults")
            .and_then(Value::as_array)
            .unwrap();
        assert!(kept.len() < 4_000);
        assert_eq!(
            kept.first().unwrap().get("index").and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(
            kept.last().unwrap().get("index").and_then(Value::as_u64),
            Some(3_999)
        );
        assert!(serde_json::to_vec(&preview).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);

        let omission = manifest(&preview)
            .pointer("/preview/omissions")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|item| item.get("kind") == Some(&json!("array")))
            .expect("array omission");
        assert_eq!(
            omission.get("originalItems").and_then(Value::as_u64),
            Some(4_000)
        );
        assert!(
            omission
                .get("previewItems")
                .and_then(Value::as_u64)
                .unwrap()
                < 4_000
        );
    }

    #[test]
    fn oversized_errors_keep_actionable_diagnostics_inline() {
        let diagnostic = oversized_text(
            "fatal: could not open project\nCAUSE-SENTINEL\n",
            "\nREMEDIATION-SENTINEL\nexit status 17",
        );
        let result = json!({
            "content": [{"type": "text", "text": diagnostic}],
            "structuredContent": {
                "toolName": "run_command",
                "success": false,
                "exitCode": 17,
                "stderr": diagnostic,
                "stdout": ""
            },
            "isError": true
        });

        let candidate = prepare_response_budget(&result, true).expect("must budget");
        let preview = candidate.finish("lr_test_error");
        assert_eq!(preview.get("isError").and_then(Value::as_bool), Some(true));
        assert_eq!(
            preview
                .pointer("/structuredContent/exitCode")
                .and_then(Value::as_i64),
            Some(17)
        );
        let stderr = preview
            .pointer("/structuredContent/stderr")
            .and_then(Value::as_str)
            .expect("stderr preview");
        assert!(stderr.contains("CAUSE-SENTINEL"));
        assert!(stderr.contains("REMEDIATION-SENTINEL"));
        let content = preview
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .expect("error content preview");
        assert!(content.contains("fatal: could not open project"));
        assert!(content.contains("exit status 17"));
        assert!(serde_json::to_vec(&preview).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
    }

    #[test]
    fn unicode_cut_points_never_split_utf8() {
        for extra in 0..16 {
            let body = format!(
                "HEAD-{}{}-TAIL",
                "🙂".repeat(20_000 + extra),
                "ź中".repeat(10_000 + extra)
            );
            let result = json!({
                "content": [],
                "structuredContent": {"toolName": "read", "success": true, "text": body}
            });
            let preview = prepare_response_budget(&result, false)
                .expect("must budget")
                .finish("lr_unicode");
            let text = preview
                .pointer("/structuredContent/text")
                .and_then(Value::as_str)
                .expect("text preview");
            assert!(text.starts_with("HEAD-"));
            assert!(text.ends_with("-TAIL"));
            assert!(std::str::from_utf8(text.as_bytes()).is_ok());
            assert!(serde_json::to_vec(&preview).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
        }
    }

    #[test]
    fn oversized_blob_like_fields_are_classified_and_kept_retrievable() {
        let result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "browser_snapshot",
                "success": true,
                "data": "A".repeat(180_000)
            }
        });

        let candidate = prepare_response_budget(&result, false).expect("must budget");
        let original = serde_json::from_slice::<Value>(candidate.payload()).unwrap();
        assert_eq!(original, result);
        let preview = candidate.finish("lr_blob");
        let data = preview
            .pointer("/structuredContent/data")
            .and_then(Value::as_str)
            .expect("data preview");
        assert!(data.len() < 180_000);
        assert!(
            manifest(&preview)
                .pointer("/preview/omissions")
                .and_then(Value::as_array)
                .is_some_and(|items| items
                    .iter()
                    .any(|item| item.get("kind") == Some(&json!("blob"))))
        );
        assert!(serde_json::to_vec(&preview).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
    }

    #[test]
    fn native_non_text_content_is_exempt_to_preserve_multimodal_capability() {
        let result = json!({
            "content": [{
                "type": "image",
                "mimeType": "image/png",
                "data": "A".repeat(180_000)
            }]
        });

        assert!(
            prepare_response_budget(&result, false).is_none(),
            "native MCP media must remain intact rather than becoming a text preview"
        );
    }

    #[test]
    fn stored_budget_result_reconstructs_exact_original_through_bounded_ranges() {
        let workspace =
            std::env::temp_dir().join(format!("catdesk-response-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let store = crate::result_store::LargeResultStore::new_default().expect("create store");
        let original = json!({
            "content": [],
            "structuredContent": {
                "toolName": "search",
                "success": true,
                "matchCount": 12_000,
                "searchResults": (0..12_000)
                    .map(|index| json!({
                        "index": index,
                        "path": format!("src/very-long-file-{index:05}.rs"),
                        "text": format!("record-{index}-{}", "ż中🙂".repeat(8))
                    }))
                    .collect::<Vec<_>>()
            }
        });
        let expected = serde_json::to_vec(&original).unwrap();
        let mut inline = original;

        let metadata =
            apply_response_budget(&mut inline, false, &store, Some("session-a"), &workspace)
                .expect("budget application")
                .expect("must externalize");

        assert!(serde_json::to_vec(&inline).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
        assert_eq!(
            inline
                .pointer("/responseBudget/outputRef")
                .and_then(Value::as_str),
            Some(metadata.result_id.as_str())
        );
        assert_eq!(
            inline
                .pointer("/responseBudget/retrieval/tool")
                .and_then(Value::as_str),
            Some("read_result")
        );
        assert_eq!(
            inline
                .pointer("/responseBudget/search/tool")
                .and_then(Value::as_str),
            Some("search_result")
        );

        let mut rebuilt = Vec::new();
        let mut offset = 0_u64;
        loop {
            let range = store
                .read_range(
                    Some("session-a"),
                    &workspace,
                    &metadata.result_id,
                    offset,
                    store.max_range_bytes(),
                )
                .expect("read range");
            rebuilt.extend_from_slice(&range.bytes);
            offset = range.next_offset;
            if range.eof {
                break;
            }
        }
        assert_eq!(rebuilt, expected);
        assert_eq!(
            serde_json::from_slice::<Value>(&rebuilt).unwrap(),
            serde_json::from_slice::<Value>(&expected).unwrap()
        );

        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn store_failure_leaves_original_response_untouched() {
        let workspace = std::env::temp_dir().join(format!(
            "catdesk-response-budget-fail-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let mut config = crate::result_store::LargeResultStoreConfig::default();
        config.max_entry_bytes = 1024;
        config.max_total_bytes = 4096;
        let store = crate::result_store::LargeResultStore::new(config).expect("create store");
        let original = json!({
            "content": [],
            "structuredContent": {
                "toolName": "run_command",
                "success": true,
                "stdout": "x".repeat(100_000)
            }
        });
        let before = original.clone();
        let mut inline = original;

        let error =
            apply_response_budget(&mut inline, false, &store, Some("session-a"), &workspace)
                .expect_err("store must reject oversized entry");

        assert_eq!(error.code(), "entry_too_large");
        assert_eq!(
            inline, before,
            "failed storage must not replace the original result"
        );

        std::fs::remove_dir_all(workspace).ok();
    }
}
