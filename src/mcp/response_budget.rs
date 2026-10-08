use serde_json::{Map, Value, json};
use std::path::Path;

use crate::result_store::{LargeResultStore, StoreError};

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
        omitted_bytes: usize,
    ) {
        self.push(json!({
            "path": path,
            "kind": kind,
            "originalBytes": original_bytes,
            "previewBytes": preview_bytes,
            "omittedBytes": omitted_bytes
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

/// Disclosure for a result that was lossy-reduced to fit the store's entry
/// cap before externalization: the payload behind `outputRef` is not the
/// full result, so the inline manifest must say so at every compaction
/// level — otherwise retrieval tools look complete while data was
/// irreversibly dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryCapReduction {
    /// Serialized size of the result before the reducer touched it.
    pub(crate) original_bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct BudgetCandidate {
    original: Vec<u8>,
    preview: Value,
    omissions: OmissionTracker,
    is_error: bool,
    entry_cap: Option<EntryCapReduction>,
}

/// Byte accounting for one externalized result, straight from the policy's
/// own serialization — callers must not re-measure a stored payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BudgetOutcome {
    pub(crate) output_ref: String,
    /// Serialized size of the original result retained in the store.
    pub(crate) raw_bytes: u64,
    /// Bytes actually written to the large-result store.
    pub(crate) externalized_bytes: u64,
}

impl BudgetCandidate {
    pub(crate) fn payload(&self) -> &[u8] {
        &self.original
    }

    pub(crate) fn finish(mut self, output_ref: &str) -> Value {
        self.attach_manifest(output_ref);
        self.attach_retrieval_hint(output_ref);

        if serialized_len(&self.preview) > DEFAULT_INLINE_RESPONSE_BYTES {
            self.detach_manifest();
            reduce_to_essentials(&mut self.preview, self.is_error, &mut self.omissions);
            self.attach_manifest(output_ref);
            self.attach_retrieval_hint(output_ref);
        }

        if serialized_len(&self.preview) > DEFAULT_INLINE_RESPONSE_BYTES {
            self.detach_manifest();
            hard_minimal_preview(&mut self.preview, self.is_error, &mut self.omissions);
            self.attach_manifest(output_ref);
            self.attach_retrieval_hint(output_ref);
        }

        self.preview
    }

    /// Duplicate the retrieval address into the surfaces harnesses actually
    /// show the model. The manifest rides at the result root, where ChatGPT's
    /// outputSchema projection and Codex's code_mode both drop it (measured,
    /// ojt.10): only `structuredContent` — and, failing that, a content text
    /// note — reliably reaches the model. Idempotent, so every compaction
    /// level can re-attach; the field names are essential-key shaped, so the
    /// fallback reductions keep them.
    fn attach_retrieval_hint(&mut self, output_ref: &str) {
        if let Some(structured) = self
            .preview
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            if structured.get("outputTruncated").and_then(Value::as_bool) != Some(true) {
                structured.insert("outputTruncated".to_string(), Value::Bool(true));
            }
            structured
                .entry("outputRef".to_string())
                .or_insert_with(|| json!(output_ref));
            structured
                .entry("outputBytes".to_string())
                .or_insert_with(|| json!(self.original.len()));
            return;
        }
        if let Some(content) = self
            .preview
            .get_mut("content")
            .and_then(Value::as_array_mut)
        {
            content.push(json!({
                "type": "text",
                "text": format!(
                    "CatDesk: this result was too large to show in full ({} bytes). Call read_result with result_id \"{}\" to read it in bounded ranges; search_result can find text inside it.",
                    self.original.len(),
                    output_ref
                )
            }));
        }
    }

    fn detach_manifest(&mut self) {
        if let Some(object) = self.preview.as_object_mut() {
            object.remove("responseBudget");
        }
    }

    fn attach_manifest(&mut self, output_ref: &str) {
        let preview_bytes = serialized_len(&self.preview);
        if let Some(object) = self.preview.as_object_mut() {
            let mut manifest = json!({
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
            });
            // Entry-cap fields live here, not in the payload, because the
            // manifest is rebuilt after every compaction level while string
            // fields are subject to halving and head+tail previewing.
            if let Some(reduction) = self.entry_cap
                && let Some(manifest) = manifest.as_object_mut()
            {
                manifest.insert("entryCapTruncated".to_string(), Value::Bool(true));
                manifest.insert(
                    "entryCapOriginalBytes".to_string(),
                    json!(reduction.original_bytes),
                );
                manifest.insert(
                    "entryCapOmittedBytes".to_string(),
                    json!(
                        reduction
                            .original_bytes
                            .saturating_sub(self.original.len() as u64)
                    ),
                );
            }
            object.insert("responseBudget".to_string(), manifest);
        }
    }
}

pub(crate) fn apply_response_budget(
    result: &mut Value,
    is_error: bool,
    store: &LargeResultStore,
    owner_session: Option<&str>,
    workspace_root: &Path,
    entry_cap_reduction: Option<EntryCapReduction>,
) -> Result<Option<BudgetOutcome>, StoreError> {
    let Some(mut candidate) = prepare_response_budget(result, is_error) else {
        return Ok(None);
    };
    candidate.entry_cap = entry_cap_reduction;

    let raw_bytes = candidate.payload().len() as u64;
    let stored = store.put(
        owner_session,
        workspace_root,
        candidate.payload(),
        Some("application/json"),
    )?;
    let metadata = stored.metadata;
    *result = candidate.finish(&metadata.result_id);
    Ok(Some(BudgetOutcome {
        output_ref: metadata.result_id,
        raw_bytes,
        externalized_bytes: metadata.size_bytes,
    }))
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
        entry_cap: None,
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
                let (preview, omitted_bytes) =
                    head_tail_preview_with_omitted(text, limit, BUDGET_REFERENCE_NOTE);
                let preview_bytes = preview.len();
                let kind = if is_blob_field(field_name) {
                    "blob"
                } else {
                    "text"
                };
                *text = preview;
                omissions.text(path, kind, original_bytes, preview_bytes, omitted_bytes);
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
                    let (preview, omitted_bytes) = head_tail_preview_with_omitted(
                        text,
                        FALLBACK_TEXT_PREVIEW_BYTES,
                        BUDGET_REFERENCE_NOTE,
                    );
                    let preview_bytes = preview.len();
                    let path = json_pointer_child("/structuredContent", key);
                    let kind = if is_blob_field(Some(key)) {
                        "blob"
                    } else {
                        "text"
                    };
                    *text = preview;
                    omissions.text(&path, kind, original_bytes, preview_bytes, omitted_bytes);
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

    let (preview, omitted_bytes) =
        head_tail_preview_with_omitted(&text, limit, BUDGET_REFERENCE_NOTE);
    let preview_bytes = preview.len();
    if let Some(text_value) = content
        .first_mut()
        .and_then(Value::as_object_mut)
        .and_then(|item| item.get_mut("text"))
    {
        *text_value = Value::String(preview);
    }
    omissions.text(
        "/content/0/text",
        "text",
        text.len(),
        preview_bytes,
        omitted_bytes,
    );
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
    head_tail_preview_with_omitted(text, max_bytes, BUDGET_REFERENCE_NOTE).0
}

/// Where a previewed text's remainder lives. Budgeted surfaces store the full
/// payload in the large-result store, so their marker points at outputRef.
const BUDGET_REFERENCE_NOTE: &str = "see responseBudget.outputRef";

fn head_tail_preview_with_omitted(
    text: &str,
    max_bytes: usize,
    reference_note: &str,
) -> (String, usize) {
    if text.len() <= max_bytes {
        return (text.to_string(), 0);
    }

    let mut marker = format!("\n… <omitted bytes; {reference_note}> …\n");
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
        marker = format!("\n… <omitted {omitted} bytes; {reference_note}> …\n");
    }

    let mut preview = String::with_capacity(max_bytes);
    preview.push_str(&text[..head_end]);
    preview.push_str(&marker);
    preview.push_str(&text[tail_start..]);

    while preview.len() > max_bytes && head_end > 0 {
        head_end = previous_char_boundary(text, head_end);
        let omitted = tail_start.saturating_sub(head_end);
        marker = format!("\n… <omitted {omitted} bytes; {reference_note}> …\n");
        preview.clear();
        preview.push_str(&text[..head_end]);
        preview.push_str(&marker);
        preview.push_str(&text[tail_start..]);
    }

    (preview, tail_start.saturating_sub(head_end))
}

/// Cap for text the shared budget cannot reach: the multimodal-exempt
/// `read_image` analysis travels inline next to native image content, so its
/// model-generated description needs a deterministic bound of its own. Tied to
/// the error-text preview size so one budget scale governs both surfaces.
pub(crate) const ANALYSIS_DESCRIPTION_PREVIEW_BYTES: usize = ERROR_TEXT_PREVIEW_BYTES;

/// Deterministic head+tail preview for budget-exempt surfaces. The omission
/// note is self-contained because these surfaces have no stored outputRef.
pub(crate) fn preview_text_without_output_ref(text: &str, max_bytes: usize) -> String {
    head_tail_preview_with_omitted(text, max_bytes, "full text not retained").0
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

    fn marker_omitted_bytes(preview: &str) -> u64 {
        let marker_start = preview.find("<omitted ").expect("missing omission marker");
        let count_start = marker_start + "<omitted ".len();
        let count_end = preview[count_start..]
            .find(" bytes;")
            .map(|offset| count_start + offset)
            .expect("missing omission byte suffix");
        preview[count_start..count_end]
            .parse()
            .expect("invalid omission byte count")
    }

    #[test]
    fn text_omission_metadata_counts_removed_source_bytes_not_preview_marker_bytes() {
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
        let preview = candidate.finish("lr_exact_omission");
        let stdout = preview
            .pointer("/structuredContent/stdout")
            .and_then(Value::as_str)
            .expect("stdout preview");
        let omission = manifest(&preview)
            .pointer("/preview/omissions")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("path").and_then(Value::as_str) == Some("/structuredContent/stdout")
                })
            })
            .expect("stdout omission");

        assert_eq!(
            omission.get("omittedBytes").and_then(Value::as_u64),
            Some(marker_omitted_bytes(stdout)),
            "omittedBytes must count source bytes removed from the middle, excluding marker bytes"
        );
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

        let outcome = apply_response_budget(
            &mut inline,
            false,
            &store,
            Some("session-a"),
            &workspace,
            None,
        )
        .expect("budget application")
        .expect("must externalize");

        assert!(serde_json::to_vec(&inline).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);
        assert_eq!(
            inline
                .pointer("/responseBudget/outputRef")
                .and_then(Value::as_str),
            Some(outcome.output_ref.as_str())
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
                    &outcome.output_ref,
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
    fn budget_outcome_reports_raw_and_externalized_bytes_from_the_policy() {
        let workspace =
            std::env::temp_dir().join(format!("catdesk-budget-outcome-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let store = crate::result_store::LargeResultStore::new_default().expect("create store");
        let original = json!({
            "content": [],
            "structuredContent": {
                "toolName": "run_command",
                "success": true,
                "stdout": "o".repeat(200_000),
                "stderr": "e".repeat(4_000)
            }
        });
        let expected_bytes = serde_json::to_vec(&original).unwrap().len() as u64;
        let mut inline = original;

        let outcome = apply_response_budget(
            &mut inline,
            false,
            &store,
            Some("session-a"),
            &workspace,
            None,
        )
        .expect("budget application")
        .expect("must externalize");

        assert_eq!(
            outcome.raw_bytes, expected_bytes,
            "raw is the policy's own serialization of the full result"
        );
        assert_eq!(
            outcome.externalized_bytes, expected_bytes,
            "the store retains the full raw payload"
        );
        assert_eq!(
            inline
                .pointer("/responseBudget/originalBytes")
                .and_then(Value::as_u64),
            Some(outcome.raw_bytes),
            "the inline manifest and the telemetry outcome must agree"
        );
        assert!(serde_json::to_vec(&inline).unwrap().len() <= DEFAULT_INLINE_RESPONSE_BYTES);

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

        let error = apply_response_budget(
            &mut inline,
            false,
            &store,
            Some("session-a"),
            &workspace,
            None,
        )
        .expect_err("store must reject oversized entry");

        assert_eq!(error.code(), "entry_too_large");
        assert_eq!(
            inline, before,
            "failed storage must not replace the original result"
        );

        std::fs::remove_dir_all(workspace).ok();
    }

    /// Passthrough results (DevTools tools) carry no structuredContent, so
    /// the retrieval hint falls back to a content text note — the only
    /// model-visible surface such a result has.
    #[test]
    fn results_without_structured_content_get_a_content_text_retrieval_note() {
        let result = json!({
            "content": [{
                "type": "text",
                "text": oversized_text("BODY-HEAD", "-BODY-TAIL")
            }]
        });
        let candidate = prepare_response_budget(&result, false).expect("must budget");
        let original_bytes = candidate.original.len();
        let preview = candidate.finish("lr_passthrough");

        let notes: Vec<&Value> = preview
            .get("content")
            .and_then(Value::as_array)
            .expect("content array")
            .iter()
            .filter(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("read_result"))
            })
            .collect();
        assert!(
            notes.len() == 1,
            "exactly one retrieval note expected, got {notes:?}"
        );
        let note = notes[0]["text"].as_str().expect("note text");
        assert!(note.contains("lr_passthrough"), "note names the id: {note}");
        assert!(
            note.contains(&original_bytes.to_string()),
            "note quantifies the original size: {note}"
        );
    }

    /// The hint is idempotent across the compaction ladder: with forty large
    /// fields the first compaction still exceeds the inline budget, so the
    /// answer really descends into reduce_to_essentials — and the hint is
    /// attached again on that deeper level.
    #[test]
    fn retrieval_hint_survives_the_full_compaction_ladder() {
        let mut structured = serde_json::Map::new();
        structured.insert("toolName".to_string(), json!("run_command"));
        structured.insert("success".to_string(), json!(true));
        for index in 0..40 {
            structured.insert(
                format!("field{index}"),
                json!(oversized_text("HEAD", "-TAIL")),
            );
        }
        let result = json!({
            "content": [],
            "structuredContent": Value::Object(structured)
        });
        let mut candidate = prepare_response_budget(&result, false).expect("must budget");
        candidate.entry_cap = Some(EntryCapReduction {
            original_bytes: serialized_len(&result) as u64,
        });
        let preview = candidate.finish("lr_ladder");

        assert_eq!(
            preview
                .pointer("/structuredContent/outputRef")
                .and_then(Value::as_str),
            Some("lr_ladder")
        );
        assert_eq!(
            preview
                .pointer("/structuredContent/outputTruncated")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            preview
                .pointer("/structuredContent/outputBytes")
                .and_then(Value::as_u64)
                .is_some(),
            "outputBytes must survive the ladder"
        );
        let kept = preview
            .pointer("/structuredContent")
            .and_then(Value::as_object)
            .expect("structured object");
        assert!(
            kept.len() < 10,
            "the deep compaction must have run: {} keys kept",
            kept.len()
        );
    }

    /// Entry-cap disclosure must live in the manifest, not the payload: the
    /// manifest is rebuilt after every compaction level, so the disclosure
    /// survives while string markers can be previewed away. The pre-reduction
    /// size comes from the reducer's caller, not from the reduced payload.
    #[test]
    fn entry_cap_disclosure_survives_compaction_and_quantifies_the_loss() {
        let mut result = json!({
            "content": [],
            "structuredContent": {
                "toolName": "run_command",
                "success": true,
                "exitCode": 0,
                "stdout": oversized_text("HEAD-SENTINEL\n", "\nTAIL-SENTINEL"),
                "stderr": ""
            }
        });
        let pre_reduction_bytes = serialized_len(&result) as u64;
        // The production sequence: the reducer drops bytes down to the entry
        // cap, and the reduced payload still exceeds the inline budget so the
        // preview is compacted again.
        super::super::reduce_result_to_entry_cap(&mut result, 120 * 1024);
        let reduced_bytes = serialized_len(&result) as u64;
        assert!(
            reduced_bytes < pre_reduction_bytes,
            "reduction must be lossy"
        );
        let mut candidate = prepare_response_budget(&result, false).expect("must budget");
        candidate.entry_cap = Some(EntryCapReduction {
            original_bytes: pre_reduction_bytes,
        });

        let preview = candidate.finish("lr_entry_cap_disclosed");

        let budget = manifest(&preview);
        assert_eq!(
            budget.get("entryCapTruncated").and_then(Value::as_bool),
            Some(true),
            "the inline answer must disclose the lossy reduction"
        );
        assert_eq!(
            budget.get("entryCapOriginalBytes").and_then(Value::as_u64),
            Some(pre_reduction_bytes),
            "the disclosed original size must be measured before the reducer ran"
        );
        let omitted = budget
            .get("entryCapOmittedBytes")
            .and_then(Value::as_u64)
            .expect("entryCapOmittedBytes must quantify the dropped bytes");
        assert!(omitted > 0, "reduction must drop bytes, saw {omitted}");
        assert!(
            omitted < pre_reduction_bytes,
            "the loss must be partial, saw {omitted}/{pre_reduction_bytes}"
        );
    }
}
