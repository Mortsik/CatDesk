use super::resources::{
    CATDESK_WIDGET_HTML, INITIAL_MASCOT_OUTLINE_PLACEHOLDER,
    INITIAL_TOKEN_STATS_LAYOUT_PLACEHOLDER, INITIAL_TOOL_NAME_PLACEHOLDER,
    REENABLE_WIDGET_IMAGE_PLACEHOLDER, REFRESH_CATDESK_IMAGE_PLACEHOLDER,
    REMOVE_CATDESK_IMAGE_PLACEHOLDER, UI_TEMPLATE_URI, cached_data_uri,
    current_widget_resource_uri_for_tool, handle_resources_list_with_show_detail_mode,
    handle_resources_read, handle_resources_read_with_show_detail_mode, handle_server_discover,
};
use super::*;
use base64::Engine as _;

use crate::command;
use crate::handoff;
use crate::workspace_tools;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use super::agents_state::cached_file_value;
use super::commands::{change_scope_for_request, command_job_output_text, handle_poll_command};
use super::file_tools::{handle_create_handoff_for_project, image_tool_analyzed_response};
use super::instruction::{
    CATDESK_INSTRUCTION_REQUIRED_CODE, CATDESK_INSTRUCTION_REQUIRED_MESSAGE,
    CATDESK_INSTRUCTION_REQUIRED_WIDGET_MESSAGE,
    catdesk_instruction_required_response_with_show_detail_mode, catdesk_instruction_structured,
    catdesk_instruction_text, catdesk_instruction_text_for_project,
    catdesk_instruction_widget_payload_with_cards,
    handle_catdesk_instruction_with_show_detail_mode,
};
use super::result_tools::{handle_read_result, handle_search_result};
use super::token_usage::{TokenUsage, sanitize_result_for_turn_token_count};
use super::tool_catalog::handle_tools_list;
use super::widget::{
    attach_tool_call_count, attach_turn_token_usage, base_widget_payload_with_show_detail_mode,
    build_command_job_widget_payload, build_list_files_widget_payload_from_structured,
    enrich_tool_result, enrich_tool_result_with_show_detail_mode,
    tool_descriptor_should_attach_widget,
};
use crate::command_jobs::{CommandJobSnapshot, CommandJobState};
use crate::mascot;
use crate::perf_metrics;
use crate::perf_metrics::CacheKind;
use crate::result_store::{LargeResultStore, LargeResultStoreConfig};

/// `git` is resolved through `PATH` at spawn time and `PATH` is
/// process-global: linux_sandbox tests rewrite it while they run, which
/// breaks git spawns mid-test (ENOENT surfaces as a panic on the git
/// setup `expect`). Git-spawning tests take this guard so they never
/// interleave with an env rewrite. Unrelated to the local ENV_LOCK in
/// `read_image_analyze_requires_configured_backend`, which only
/// serializes the GEMINI_API_KEY env mutation.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_serialization::lock_env()
}

#[test]
fn queued_job_output_text_is_neutral_about_scheduling() {
    let snapshot = CommandJobSnapshot {
        job_id: "job-under-test".into(),
        command: "true".into(),
        cwd: "/tmp".into(),
        state: CommandJobState::Queued,
        elapsed_ms: 0,
        exit_code: None,
        events: Vec::new(),
        next_cursor: 0,
        has_more_output: false,
        output_truncated: false,
        timeout_ms: 1_000,
    };
    let text = command_job_output_text(&snapshot);
    assert!(
        text.contains("command is starting"),
        "queued-state text must describe the start state: {text}"
    );
    assert!(
        !text.contains("process capacity"),
        "queued-state text must not advertise a concurrency queue: {text}"
    );
}

#[test]
fn static_widget_image_encoding_is_reused() {
    let cache = OnceLock::new();
    let first = cached_data_uri(&cache, b"static-png");
    let second = cached_data_uri(&cache, b"static-png");

    assert_eq!(first, second);
    assert!(
        std::ptr::eq(first, second),
        "cached URI must reuse one allocation"
    );
}

#[test]
fn metadata_cache_reuses_unchanged_value_and_reloads_after_file_changes() {
    let root = std::env::temp_dir().join(format!("catdesk-mcp-metadata-cache-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create cache test root");
    let path = root.join("value.txt");
    std::fs::write(&path, "one").expect("write first value");
    let cache = std::sync::Mutex::new(HashMap::new());
    let loads = std::cell::Cell::new(0usize);
    let load = || {
        loads.set(loads.get() + 1);
        std::fs::read_to_string(&path)
    };

    assert_eq!(
        cached_file_value(CacheKind::AppConfig, &cache, &path, load).unwrap(),
        "one"
    );
    assert_eq!(
        cached_file_value(CacheKind::AppConfig, &cache, &path, load).unwrap(),
        "one"
    );
    assert_eq!(loads.get(), 1, "unchanged file should reuse cached value");

    std::fs::write(&path, "three-three").expect("rewrite cached value");
    assert_eq!(
        cached_file_value(CacheKind::AppConfig, &cache, &path, load).unwrap(),
        "three-three"
    );
    assert_eq!(loads.get(), 2, "metadata change must invalidate cache");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn metadata_cache_counts_hits_and_misses_for_perf_metrics() {
    use crate::perf_metrics::CacheKind;

    let root = std::env::temp_dir().join(format!("catdesk-mcp-cache-counters-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create cache test root");
    let path = root.join("value.txt");
    std::fs::write(&path, "one").expect("write first value");
    let cache = std::sync::Mutex::new(HashMap::new());
    let load = || std::fs::read_to_string(&path);
    let kind = CacheKind::AgentsText;
    let hits_slot = kind as usize;
    let before = perf_metrics::snapshot();

    assert_eq!(
        cached_file_value(kind, &cache, &path, load).unwrap(),
        "one",
        "first lookup is a miss that loads"
    );
    assert_eq!(
        cached_file_value(kind, &cache, &path, load).unwrap(),
        "one",
        "second lookup hits the cached value"
    );
    let after = perf_metrics::snapshot();
    assert_eq!(
        after.cache_misses[hits_slot],
        before.cache_misses[hits_slot] + 1
    );
    assert_eq!(
        after.cache_hits[hits_slot],
        before.cache_hits[hits_slot] + 1
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn workspace_root_command_scope_skips_recursive_change_tracking() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-root-scope-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let tracked = workspace_root.join("tracked.txt");
    std::fs::write(&tracked, "before\n").expect("write tracked file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request(
        "run_command",
        json!({ "command": "printf noop", "cwd": workspace_root_str }),
    );

    let session = ChangeSession::begin(
        &workspace_root,
        change_scope_for_request(&req, &workspace_root.to_string_lossy(), None),
    );
    std::fs::write(&tracked, "after\n").expect("modify tracked file");

    assert!(
        session.changes().is_empty(),
        "workspace-root commands must not recursively snapshot a broad workspace"
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn project_command_scope_still_tracks_project_changes() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-project-scope-{}-{unique}",
        std::process::id()
    ));
    let project = workspace_root.join("project");
    std::fs::create_dir_all(&project).expect("create project");
    let tracked = project.join("tracked.txt");
    std::fs::write(&tracked, "before\n").expect("write tracked file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let project_str = project.to_string_lossy().into_owned();
    let req = tool_call_request(
        "run_command",
        json!({ "command": "printf noop", "cwd": project_str }),
    );

    let session = ChangeSession::begin(
        &workspace_root,
        change_scope_for_request(&req, &workspace_root_str, None),
    );
    std::fs::write(&tracked, "after\n").expect("modify tracked file");

    let changes = session.changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "project/tracked.txt");
    let _ = std::fs::remove_dir_all(workspace_root);
}
use image::GenericImageView;
use uuid::Uuid;

fn resources_list_request() -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-resources")),
        method: "resources/list".into(),
        params: json!({}),
    }
}

fn resources_read_request(uri: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-resource")),
        method: "resources/read".into(),
        params: json!({
            "uri": uri,
        }),
    }
}

fn tool_call_request(name: &str, arguments: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tool")),
        method: "tools/call".into(),
        params: json!({
            "name": name,
            "arguments": arguments,
        }),
    }
}

fn result_text(response: &JsonRpcResponse) -> &str {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(Value::as_object)
        .and_then(|structured| {
            structured
                .get("message")
                .or_else(|| structured.get("text"))
                .or_else(|| structured.get("instructionText"))
                .or_else(|| {
                    structured
                        .get("files")
                        .and_then(Value::as_array)
                        .and_then(|files| files.first())
                        .and_then(|file| file.get("text"))
                })
        })
        .and_then(Value::as_str)
        .expect("missing result text")
}

fn assert_no_text_content(response: &JsonRpcResponse) {
    let content = response
        .result
        .as_ref()
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .expect("missing content array");
    assert!(
        content.iter().all(|entry| entry.get("text").is_none()
            && entry.get("type").and_then(Value::as_str) != Some("text")),
        "tool result content must not contain text entries: {content:?}"
    );
}

fn content_text(response: &JsonRpcResponse) -> &str {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|content| {
            content
                .iter()
                .find(|entry| entry.get("type").and_then(Value::as_str) == Some("text"))
        })
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .expect("missing text content")
}

fn write_test_image(path: &Path, format: image::ImageFormat, width: u32, height: u32) {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([23, 67, 101]));
    image
        .save_with_format(path, format)
        .expect("write test image");
}

async fn read_image_response(
    workspace_root: &Path,
    arguments: Value,
    tool_mode: ToolMode,
) -> JsonRpcResponse {
    let req = tool_call_request("read_image", arguments);
    handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        tool_mode,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await
}

fn decoded_image_dimensions(content: &Value) -> (u32, u32) {
    let data = content["data"].as_str().expect("missing base64 data");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("valid base64");
    image::load_from_memory(&bytes)
        .expect("decodable image")
        .dimensions()
}

fn assert_no_structured_content(response: &JsonRpcResponse) {
    assert!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .is_none(),
        "read_image must answer with pure multimodal content; structuredContent makes ChatGPT drop the image block"
    );
}

fn image_content(response: &JsonRpcResponse) -> &Value {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .expect("missing image content")
}

#[test]
fn server_discover_advertises_only_2026_07_28() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-discover")),
        method: "server/discover".into(),
        params: json!({}),
    };

    for mode in [ShowDetailMode::Expanded, ShowDetailMode::Collapsed] {
        let response = handle_server_discover(&req, mode);
        let result = response.result.as_ref().expect("missing discover result");
        assert_eq!(
            result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .and_then(|versions| versions.first())
                .and_then(Value::as_str),
            Some(MODERN_MCP_PROTOCOL_VERSION)
        );
        assert_eq!(
            result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert!(
            result
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("resources"))
                .is_some(),
            "Widget-enabled modes must advertise resources"
        );
    }

    let disabled = handle_server_discover(&req, ShowDetailMode::Disable);
    let disabled_capabilities = disabled
        .result
        .as_ref()
        .and_then(|result| result.get("capabilities"))
        .expect("missing Disable capabilities");
    assert!(disabled_capabilities.get("tools").is_some());
    assert!(
        disabled_capabilities.get("resources").is_none(),
        "Disable must not advertise resources"
    );
}

#[tokio::test]
async fn command_job_tools_start_poll_and_report_terminal_success() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-command-job-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 150; Write-Output job-done"
    } else {
        "sleep 0.15; printf 'job-done\\n'"
    };

    let start_req = tool_call_request(
        "start_command",
        json!({ "command": command, "timeout": 5_000 }),
    );
    let start_response = handle_tools_call(
        &start_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;
    assert_no_text_content(&start_response);
    let start_structured = start_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing start structured content");
    let job_id = start_structured
        .get("jobId")
        .and_then(Value::as_str)
        .expect("missing job id")
        .to_string();
    assert_eq!(
        start_structured.get("state").and_then(Value::as_str),
        Some("queued")
    );
    assert_eq!(
        start_response
            .result
            .as_ref()
            .and_then(|result| result.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .and_then(|payload| payload.get("toolName"))
            .and_then(Value::as_str),
        Some("start_command")
    );

    let mut terminal = None;
    let mut cursor = 0;
    let mut seen_output = String::new();
    for _ in 0..20 {
        let poll_req = tool_call_request(
            "poll_command",
            json!({ "job_id": job_id, "after": cursor, "wait_ms": 250 }),
        );
        let response = handle_tools_call(
            &poll_req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &None,
        )
        .await;
        let structured = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .expect("missing poll structured content");
        if let Some(events) = structured.get("events").and_then(Value::as_array) {
            for event in events {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    seen_output.push_str(text);
                }
            }
        }
        cursor = structured
            .get("nextCursor")
            .and_then(Value::as_u64)
            .unwrap_or(cursor);
        if structured.get("state").and_then(Value::as_str) == Some("succeeded")
            && structured.get("hasMoreOutput").and_then(Value::as_bool) != Some(true)
        {
            terminal = Some(response);
            break;
        }
    }
    let terminal = terminal.expect("job did not reach succeeded state");
    assert!(
        terminal
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .is_none(),
        "successful command polling must not be an MCP tool error"
    );
    let structured = terminal
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing terminal structured content");
    assert_eq!(
        structured.get("commandSuccess").and_then(Value::as_bool),
        Some(true)
    );
    assert!(seen_output.contains("job-done"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn start_command_idempotency_is_namespaced_by_client_session() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-session-dedupe-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 5"
    } else {
        "sleep 5"
    };
    let req = tool_call_request("start_command", json!({ "command": command }));

    async fn start_for_session(
        req: &JsonRpcRequest,
        workspace_root: &str,
        command_jobs: &CommandJobManager,
        session: Option<&str>,
    ) -> JsonRpcResponse {
        handle_tools_call_with_session(
            req,
            workspace_root,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            command_jobs,
            &None,
            ShowDetailMode::Disable,
            session,
            None,
        )
        .await
    }
    let job_id = |response: &JsonRpcResponse| {
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("jobId"))
            .and_then(Value::as_str)
            .expect("missing job id")
            .to_string()
    };
    let deduplicated = |response: &JsonRpcResponse| {
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("deduplicated"))
            .and_then(Value::as_bool)
            .expect("missing deduplicated flag")
    };

    let session_a_first =
        start_for_session(&req, &workspace_root_str, &command_jobs, Some("session-a")).await;
    let session_a_retry =
        start_for_session(&req, &workspace_root_str, &command_jobs, Some("session-a")).await;
    assert_eq!(job_id(&session_a_first), job_id(&session_a_retry));
    assert!(!deduplicated(&session_a_first));
    assert!(deduplicated(&session_a_retry));

    let session_b =
        start_for_session(&req, &workspace_root_str, &command_jobs, Some("session-b")).await;
    assert_ne!(job_id(&session_a_first), job_id(&session_b));
    assert!(!deduplicated(&session_b));

    let anonymous_first = start_for_session(&req, &workspace_root_str, &command_jobs, None).await;
    let anonymous_second = start_for_session(&req, &workspace_root_str, &command_jobs, None).await;
    assert_ne!(job_id(&anonymous_first), job_id(&anonymous_second));
    assert!(!deduplicated(&anonymous_first));
    assert!(!deduplicated(&anonymous_second));

    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn command_jobs_cannot_be_polled_or_cancelled_across_client_sessions() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-session-job-owner-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 5"
    } else {
        "sleep 5"
    };

    let start_req = tool_call_request("start_command", json!({ "command": command }));
    let started = handle_tools_call_with_session(
        &start_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
        ShowDetailMode::Disable,
        Some("session-a"),
        None,
    )
    .await;
    let job_id = started
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("jobId"))
        .and_then(Value::as_str)
        .expect("missing job id")
        .to_string();

    for tool_name in ["poll_command", "cancel_command"] {
        let req = tool_call_request(
            tool_name,
            if tool_name == "poll_command" {
                json!({ "job_id": job_id, "wait_ms": 0 })
            } else {
                json!({ "job_id": job_id })
            },
        );
        let response = handle_tools_call_with_session(
            &req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &None,
            ShowDetailMode::Disable,
            Some("session-b"),
            None,
        )
        .await;
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true),
            "{tool_name} from another session must be rejected"
        );
        assert!(
            result_text(&response).contains("unknown or expired command job"),
            "cross-session rejection must not reveal job ownership: {}",
            result_text(&response)
        );
    }

    let owner_poll = tool_call_request("poll_command", json!({ "job_id": job_id, "wait_ms": 0 }));
    let owner_response = handle_tools_call_with_session(
        &owner_poll,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
        ShowDetailMode::Disable,
        Some("session-a"),
        None,
    )
    .await;
    assert!(
        owner_response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .is_none(),
        "owner must still be able to poll its job"
    );

    let owner_cancel = tool_call_request("cancel_command", json!({ "job_id": job_id }));
    let cancel_response = handle_tools_call_with_session(
        &owner_cancel,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
        ShowDetailMode::Disable,
        Some("session-a"),
        None,
    )
    .await;
    assert!(
        cancel_response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .is_none(),
        "owner must still be able to cancel its job"
    );

    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn reused_json_rpc_id_with_different_start_arguments_creates_distinct_jobs() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-id-reuse-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let first_command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 500"
    } else {
        "sleep 0.5"
    };
    let second_command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 600"
    } else {
        "sleep 0.6"
    };

    // tool_call_request deliberately reuses the same JSON-RPC id. Stateless
    // clients are allowed to do this across independent calls.
    let first_req = tool_call_request("start_command", json!({ "command": first_command }));
    let second_req = tool_call_request("start_command", json!({ "command": second_command }));
    let first = handle_tools_call(
        &first_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;
    let second = handle_tools_call(
        &second_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;

    let job_id = |response: &JsonRpcResponse| {
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("jobId"))
            .and_then(Value::as_str)
            .expect("missing job id")
            .to_string()
    };
    assert_ne!(job_id(&first), job_id(&second));
    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn interrupted_snapshot_text_explains_restart() {
    let snapshot = CommandJobSnapshot {
        job_id: "j".into(),
        command: "sleep 1".into(),
        cwd: "/w".into(),
        state: CommandJobState::Interrupted,
        elapsed_ms: 5,
        exit_code: None,
        events: Vec::new(),
        next_cursor: 0,
        has_more_output: false,
        output_truncated: false,
        timeout_ms: 1_000,
    };
    assert!(command_job_output_text(&snapshot).contains("interrupted"));
}

#[test]
fn abandoned_snapshot_text_explains_missing_polls() {
    let snapshot = CommandJobSnapshot {
        job_id: "j".into(),
        command: "sleep 1".into(),
        cwd: "/w".into(),
        state: CommandJobState::Abandoned,
        elapsed_ms: 5,
        exit_code: Some(131),
        events: Vec::new(),
        next_cursor: 0,
        has_more_output: false,
        output_truncated: false,
        timeout_ms: 1_000,
    };
    let text = command_job_output_text(&snapshot);
    assert!(text.contains("abandoned"), "{text}");
    assert!(text.contains("no poll"), "{text}");
}

#[test]
fn command_job_widget_state_matrix_preserves_command_ui_contract() {
    let cases = [
        ("start_command", "queued", "Command Queued", "waiting"),
        ("start_command", "running", "Command Started", "waiting"),
        ("poll_command", "running", "Command Running", "waiting"),
        ("poll_command", "succeeded", "Command Complete", "done"),
        ("poll_command", "failed", "Command Failed", "failed"),
        ("cancel_command", "cancelled", "Command Cancelled", "done"),
        ("poll_command", "timed_out", "Command Timed Out", "failed"),
        (
            "poll_command",
            "interrupted",
            "Command Interrupted",
            "failed",
        ),
        ("poll_command", "abandoned", "Command Abandoned", "failed"),
    ];

    for (tool_name, state, expected_title, expected_widget_state) in cases {
        let result = json!({
            "structuredContent": {
                "toolName": tool_name,
                "jobId": "job-123",
                "command": "cargo build",
                "cwd": "E:/CatDesk",
                "state": state,
                "elapsedMs": 123,
                "exitCode": null,
                "events": [],
                "nextCursor": 0,
                "outputTruncated": false,
                "timeoutMs": 5000,
                "commandSuccess": null,
                "success": true
            }
        });
        let payload = build_command_job_widget_payload(&result, tool_name, None)
            .unwrap_or_else(|| panic!("missing widget payload for {tool_name}/{state}"));
        assert_eq!(
            payload.get("toolName").and_then(Value::as_str),
            Some(tool_name)
        );
        assert_eq!(
            payload.get("title").and_then(Value::as_str),
            Some(expected_title)
        );
        assert_eq!(
            payload.get("state").and_then(Value::as_str),
            Some(expected_widget_state)
        );
        assert_eq!(
            payload.get("command").and_then(Value::as_str),
            Some("cargo build")
        );
        assert_eq!(payload.get("elapsedMs").and_then(Value::as_u64), Some(123));
        assert_eq!(
            payload.get("hasChanges").and_then(Value::as_bool),
            Some(false)
        );
    }
}

#[test]
fn command_job_widget_formats_stderr_and_truncation_without_new_styles() {
    let result = json!({
        "structuredContent": {
            "toolName": "poll_command",
            "jobId": "job-123",
            "command": "cargo build",
            "cwd": "E:/CatDesk",
            "state": "failed",
            "elapsedMs": 456,
            "exitCode": 1,
            "events": [
                {"seq": 4, "stream": "stdout", "text": "compiling\n"},
                {"seq": 5, "stream": "stderr", "text": "error: nope\n"}
            ],
            "nextCursor": 5,
            "hasMoreOutput": true,
            "outputTruncated": true,
            "timeoutMs": 5000,
            "commandSuccess": false,
            "success": true
        }
    });
    let payload = build_command_job_widget_payload(&result, "poll_command", None)
        .expect("command job widget payload");
    let output = payload
        .get("output")
        .and_then(Value::as_str)
        .expect("missing widget output");
    assert!(output.contains("compiling"));
    assert!(output.contains("[stderr] error: nope"));
    assert!(output.contains("[older command output was truncated]"));
    assert!(output.contains("[more buffered output available; poll again]"));
    assert_eq!(
        payload.get("title").and_then(Value::as_str),
        Some("Command Failed")
    );
    assert_eq!(payload.get("state").and_then(Value::as_str), Some("failed"));
}

#[test]
fn original_run_command_widget_shape_is_unchanged_by_new_runtime_metadata() {
    let req = tool_call_request("run_command", json!({ "command": "cargo check" }));
    let raw = json!({
        "content": [],
        "structuredContent": {
            "toolName": "run_command",
            "command": "cargo check",
            "cwd": "E:/CatDesk",
            "stdout": "Finished dev profile\n",
            "stderr": "",
            "success": true,
            "exitCode": 0,
            "elapsedMs": 321,
            "timedOut": false,
            "stdoutTruncated": false,
            "stderrTruncated": false
        }
    });
    let result = enrich_tool_result(&req, raw, None);
    let payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing run_command widget payload");
    assert_eq!(
        payload.get("toolName").and_then(Value::as_str),
        Some("run_command")
    );
    assert_eq!(
        payload.get("title").and_then(Value::as_str),
        Some("Command Output")
    );
    assert_eq!(payload.get("state").and_then(Value::as_str), Some("done"));
    assert_eq!(
        payload.get("command").and_then(Value::as_str),
        Some("cargo check")
    );
    assert_eq!(payload.get("elapsedMs").and_then(Value::as_u64), Some(321));
    assert!(payload.get("exitCode").is_none());
    assert!(payload.get("timedOut").is_none());
    assert!(payload.get("stdoutTruncated").is_none());
    assert!(payload.get("stderrTruncated").is_none());
}

#[tokio::test]
async fn read_only_mode_blocks_all_command_job_calls_even_if_invoked_directly() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-command-read-only-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();

    for (tool_name, arguments) in [
        ("start_command", json!({"command": "echo blocked"})),
        ("poll_command", json!({"job_id": "blocked"})),
        ("cancel_command", json!({"job_id": "blocked"})),
    ] {
        let req = tool_call_request(tool_name, arguments);
        let response = handle_tools_call(
            &req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::ReadOnly,
            false,
            &command_jobs,
            &None,
        )
        .await;
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true),
            "{tool_name} should be blocked in read-only mode"
        );
        assert!(result_text(&response).contains("disabled in read-only mode"));
    }

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn failed_background_command_is_pollable_without_mcp_error() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-command-fail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let start_req = tool_call_request("start_command", json!({ "command": "exit 7" }));
    let start_response = handle_tools_call(
        &start_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;
    let job_id = start_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("jobId"))
        .and_then(Value::as_str)
        .expect("missing job id")
        .to_string();

    let mut terminal = None;
    for _ in 0..20 {
        let poll_req =
            tool_call_request("poll_command", json!({ "job_id": job_id, "wait_ms": 250 }));
        let response = handle_tools_call(
            &poll_req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &None,
        )
        .await;
        let state = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("state"))
            .and_then(Value::as_str);
        let has_more = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("hasMoreOutput"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if state == Some("failed") && !has_more {
            terminal = Some(response);
            break;
        }
    }
    let terminal = terminal.expect("job did not reach failed state");
    let result = terminal.result.as_ref().expect("missing result");
    assert!(result.get("isError").is_none());
    let structured = result
        .get("structuredContent")
        .expect("missing structured content");
    assert_eq!(
        structured.get("state").and_then(Value::as_str),
        Some("failed")
    );
    assert_eq!(
        structured.get("commandSuccess").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(structured.get("exitCode").and_then(Value::as_i64), Some(7));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn foreground_run_command_works_while_many_background_commands_run() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();
    let background = if cfg!(windows) {
        "Start-Sleep -Seconds 5"
    } else {
        "sleep 5"
    };
    for _ in 0..13 {
        command_jobs
            .start(background.to_string(), workspace_root.clone(), 10_000, None)
            .await
            .expect("background admission must accept every command");
    }

    let req = tool_call_request("run_command", json!({ "command": "printf budget-test" }));
    let jobs = command_jobs.clone();
    let root = workspace_root_str.clone();
    let task = tokio::spawn(async move {
        handle_tools_call(
            &req,
            &root,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &jobs,
            &None,
        )
        .await
    });

    let response = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("foreground run_command must not wait on background commands")
        .expect("run_command task panicked");

    assert_ne!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("stdout"))
            .and_then(Value::as_str),
        Some("budget-test")
    );
    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn twenty_five_concurrent_named_sessions_complete_without_concurrency_rejection() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-sessions-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command_jobs = CommandJobManager::new();

    const SESSIONS: usize = 25;
    let mut tasks = Vec::new();
    for session in 0..SESSIONS {
        let command = if cfg!(windows) {
            format!("Write-Output out-{session}")
        } else {
            format!("printf out-{session}")
        };
        let req = tool_call_request("run_command", json!({ "command": command }));
        let jobs = command_jobs.clone();
        let root = workspace_root_str.clone();
        let session_namespace = format!("session-{session}");
        tasks.push(tokio::spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                handle_tools_call_with_session(
                    &req,
                    &root,
                    1,
                    Mode::Both,
                    ToolMode::MultiTools,
                    false,
                    &jobs,
                    &None,
                    current_show_detail_mode(),
                    Some(&session_namespace),
                    None,
                ),
            )
            .await
            .expect("named session request must finish")
        }));
    }

    for (session, task) in tasks.into_iter().enumerate() {
        let response = task.await.expect("named session task panicked");
        assert_ne!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true),
            "named session {session} was rejected or failed"
        );
        #[cfg(unix)]
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("structuredContent"))
                .and_then(|structured| structured.get("stdout"))
                .and_then(Value::as_str),
            Some(format!("out-{session}").as_str()),
            "named session {session} produced unexpected output"
        );
    }

    command_jobs.cancel_all().await;
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_rejects_long_timeout_and_points_to_start_command() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-timeout-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request(
        "run_command",
        json!({ "command": "echo short", "timeout": command::MAX_TIMEOUT_MS + 1 }),
    );
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(result_text(&response).contains("Use start_command"));
    assert!(content_text(&response).contains("Use start_command"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_failure_returns_error_text_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-failure-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command = if cfg!(windows) {
        "Write-Error 'boom'; exit 7"
    } else {
        "printf 'boom\\n' >&2; exit 7"
    };
    let req = tool_call_request("run_command", json!({ "command": command }));
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    let result = response.result.as_ref().expect("missing result");
    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    let structured = result
        .get("structuredContent")
        .expect("missing structured content");
    assert_eq!(
        structured.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(structured.get("exitCode").and_then(Value::as_i64), Some(7));
    assert!(
        structured
            .get("stderr")
            .and_then(Value::as_str)
            .is_some_and(|stderr| stderr.contains("boom"))
    );
    assert!(content_text(&response).contains("boom"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_silent_failure_returns_exit_code_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-silent-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let (command, expected_exit_code) = if cfg!(windows) {
        ("exit 7", 7)
    } else {
        ("false", 1)
    };
    let req = tool_call_request("run_command", json!({ "command": command }));
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    let result = response.result.as_ref().expect("missing result");
    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    assert_eq!(
        result
            .get("structuredContent")
            .and_then(|structured| structured.get("exitCode"))
            .and_then(Value::as_i64),
        Some(expected_exit_code)
    );
    assert_eq!(
        content_text(&response),
        format!("Command failed with exit code {expected_exit_code}.")
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn silent_timeout_error_uses_timed_out_metadata_in_content() {
    let req = tool_call_request("run_command", json!({ "command": "sleep forever" }));
    let response = tool_error_response_with_structured(
        &req,
        "(no output)".to_string(),
        json!({
            "toolName": "run_command",
            "command": "sleep forever",
            "stdout": "",
            "stderr": "",
            "success": false,
            "exitCode": null,
            "timedOut": true
        }),
    );

    assert_eq!(content_text(&response), "Command timed out.");
}

#[tokio::test]
async fn run_command_timeout_with_stdout_keeps_timeout_reason_in_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-timeout-output-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let command = if cfg!(windows) {
        "Write-Output 'before-timeout'; Start-Sleep -Seconds 1"
    } else {
        "printf 'before-timeout\\n'; sleep 1"
    };
    let req = tool_call_request("run_command", json!({ "command": command, "timeout": 100 }));
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    let result = response.result.as_ref().expect("missing result");
    assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    assert_eq!(
        result
            .get("structuredContent")
            .and_then(|structured| structured.get("timedOut"))
            .and_then(Value::as_bool),
        Some(true)
    );
    let content = content_text(&response);
    assert!(
        content.contains("before-timeout"),
        "missing command output: {content}"
    );
    assert!(
        content.to_ascii_lowercase().contains("timed out"),
        "missing timeout reason: {content}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_success_keeps_content_empty() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-success-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request("run_command", json!({ "command": "printf noop" }));
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_ne!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_no_text_content(&response);

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_command_large_stdout_and_stderr_are_compact_inline_and_fully_retrievable() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-large-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    let command = concat!(
        "printf 'STDOUT-HEAD\\n'; ",
        "head -c 1100000 /dev/zero | tr '\\0' x; ",
        "printf '\\nSTDOUT-TAIL\\n'; ",
        "{ printf 'STDERR-HEAD\\n'; ",
        "head -c 1100000 /dev/zero | tr '\\0' e; ",
        "printf '\\nSTDERR-TAIL\\n'; } >&2"
    );
    let req = tool_call_request("run_command", json!({ "command": command }));

    let response = handle_tools_call_with_result_store(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;

    let inline = response
        .result
        .as_ref()
        .expect("missing run_command result");
    let inline_bytes = serde_json::to_vec(inline).expect("serialize inline result");
    assert!(
        inline_bytes.len() <= 64 * 1024,
        "run_command response must fit the shared inline budget, got {} bytes",
        inline_bytes.len()
    );

    let structured = inline
        .get("structuredContent")
        .expect("missing structuredContent");
    let stdout = structured
        .get("stdout")
        .and_then(Value::as_str)
        .expect("missing stdout preview");
    let stderr = structured
        .get("stderr")
        .and_then(Value::as_str)
        .expect("missing stderr preview");
    assert!(stdout.starts_with("STDOUT-HEAD\n"), "stdout head missing");
    assert!(stdout.ends_with("\nSTDOUT-TAIL\n"), "stdout tail missing");
    assert!(stderr.starts_with("STDERR-HEAD\n"), "stderr head missing");
    assert!(stderr.ends_with("\nSTDERR-TAIL\n"), "stderr tail missing");
    assert_eq!(
        structured.get("stdoutTruncated").and_then(Value::as_bool),
        Some(false),
        "response budgeting is not source-stream truncation"
    );
    assert_eq!(
        structured.get("stderrTruncated").and_then(Value::as_bool),
        Some(false),
        "response budgeting is not source-stream truncation"
    );

    let budget = inline
        .get("responseBudget")
        .expect("missing responseBudget metadata");
    let output_ref = budget
        .get("outputRef")
        .and_then(Value::as_str)
        .expect("missing outputRef")
        .to_string();
    let omissions = budget
        .pointer("/preview/omissions")
        .and_then(Value::as_array)
        .expect("missing omission details");
    for path in ["/structuredContent/stdout", "/structuredContent/stderr"] {
        let omission = omissions
            .iter()
            .find(|item| item.get("path").and_then(Value::as_str) == Some(path))
            .unwrap_or_else(|| panic!("missing omission for {path}"));
        let original = omission
            .get("originalBytes")
            .and_then(Value::as_u64)
            .expect("missing originalBytes");
        let preview = omission
            .get("previewBytes")
            .and_then(Value::as_u64)
            .expect("missing previewBytes");
        let omitted = omission
            .get("omittedBytes")
            .and_then(Value::as_u64)
            .expect("missing omittedBytes");
        assert!(omitted > 0, "{path} should omit bytes inline");
        assert!(
            omitted > original - preview,
            "{path} omittedBytes must exclude the inserted preview marker"
        );
        let stream_preview = structured
            .pointer(
                path.strip_prefix("/structuredContent")
                    .expect("structured path"),
            )
            .and_then(Value::as_str)
            .expect("missing stream preview");
        let count_start = stream_preview
            .find("<omitted ")
            .map(|index| index + "<omitted ".len())
            .expect("missing omission marker");
        let count_end = stream_preview[count_start..]
            .find(" bytes;")
            .map(|offset| count_start + offset)
            .expect("missing omission byte suffix");
        let marker_omitted = stream_preview[count_start..count_end]
            .parse::<u64>()
            .expect("invalid omission count");
        assert_eq!(omitted, marker_omitted, "{path} omitted byte count");
    }

    let mut rebuilt = Vec::new();
    let mut offset = 0_u64;
    loop {
        let read_req = tool_call_request(
            "read_result",
            json!({
                "result_id": output_ref,
                "offset": offset,
                "max_bytes": crate::result_store::DEFAULT_MAX_RANGE_BYTES
            }),
        );
        let read_response = handle_tools_call_with_result_store(
            &read_req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &CommandJobManager::new(),
            &None,
            ShowDetailMode::Disable,
            &store,
            Some("session-a"),
            None,
        )
        .await;
        let range = read_response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .expect("missing read_result structuredContent");
        let encoded = range
            .get("dataBase64")
            .and_then(Value::as_str)
            .expect("missing range bytes");
        rebuilt.extend(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("decode range"),
        );
        offset = range
            .get("nextOffset")
            .and_then(Value::as_u64)
            .expect("missing nextOffset");
        if range.get("eof").and_then(Value::as_bool) == Some(true) {
            break;
        }
    }

    let full: Value = serde_json::from_slice(&rebuilt).expect("parse retained full result");
    let full_structured = full
        .get("structuredContent")
        .expect("retained result missing structuredContent");
    let full_stdout = full_structured
        .get("stdout")
        .and_then(Value::as_str)
        .expect("retained stdout missing");
    let full_stderr = full_structured
        .get("stderr")
        .and_then(Value::as_str)
        .expect("retained stderr missing");
    assert!(full_stdout.len() > 1_000_000);
    assert!(full_stdout.starts_with("STDOUT-HEAD\n"));
    assert!(full_stdout.ends_with("\nSTDOUT-TAIL\n"));
    assert!(full_stderr.len() > 1_000_000);
    assert!(full_stderr.starts_with("STDERR-HEAD\n"));
    assert!(full_stderr.ends_with("\nSTDERR-TAIL\n"));
    assert_eq!(
        full_structured
            .get("stdoutTruncated")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        full_structured
            .get("stderrTruncated")
            .and_then(Value::as_bool),
        Some(false)
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn command_tool_descriptors_keep_silent_waits_stream_safe() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list-stream-safe")),
        method: "tools/list".into(),
        params: json!({}),
    };
    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");

    let run = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("run_command"))
        .expect("missing run_command");
    let run_description = run
        .get("description")
        .and_then(Value::as_str)
        .expect("missing run_command description");
    assert!(run_description.contains("20 seconds"), "{run_description}");
    assert!(
        run_description.contains("start_command"),
        "{run_description}"
    );

    let poll = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("poll_command"))
        .expect("missing poll_command");
    assert_eq!(
        poll["inputSchema"]["properties"]["wait_ms"]["maximum"],
        json!(15_000),
        "poll_command must not advertise a silent wait above 15 seconds"
    );
}

#[tokio::test]
async fn command_job_tools_document_restart_durability() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list-durable-jobs")),
        method: "tools/list".into(),
        params: json!({}),
    };
    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");
    for name in ["start_command", "poll_command"] {
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("missing {name}"));
        let text = tool.to_string();
        assert!(
            text.contains("restart"),
            "{name} must document restart durability: {text}"
        );
        assert!(
            text.contains("interrupted"),
            "{name} must document interrupted state: {text}"
        );
        assert!(
            text.contains("abandoned"),
            "{name} must document abandoned state: {text}"
        );
    }
}

#[tokio::test]
async fn multi_tools_list_exposes_run_command_mv_without_move_path_tool() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let names = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "run_command",
            "start_command",
            "poll_command",
            "cancel_command",
            "catdesk_instruction",
            "read",
            "read_image",
            "search",
            "read_result",
            "search_result",
            "write",
            "edit",
            "create_handoff",
            "delete",
        ]
    );
}

#[tokio::test]
async fn local_tools_list_exposes_output_schemas_except_multimodal_read_image() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");

    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("missing tool name");
        if name == "read_image" {
            assert!(
                tool.get("outputSchema").is_none(),
                "read_image must omit outputSchema so native image content reaches MCP hosts"
            );
            continue;
        }
        let schema = tool
            .get("outputSchema")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("missing output schema for {name}"));
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("missing output schema properties");
        assert_eq!(
            properties
                .get("toolName")
                .and_then(|property| property.get("const"))
                .and_then(Value::as_str),
            Some(name)
        );
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("success"));
        assert!(
            schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|field| field == "toolName"))
        );
    }

    for (tool_name, field) in [
        ("run_command", "stdout"),
        ("catdesk_instruction", "instructionText"),
        ("read", "files"),
        ("search", "searchResults"),
        ("read_result", "dataBase64"),
        ("search_result", "matches"),
        ("write", "bytesWritten"),
        ("edit", "operationCount"),
        ("create_handoff", "content"),
        ("delete", "recursive"),
    ] {
        let properties = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            .and_then(|tool| tool.get("outputSchema"))
            .and_then(|schema| schema.get("properties"))
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("missing output properties for {tool_name}"));
        assert!(
            properties.contains_key(field),
            "missing {field} in output schema for {tool_name}"
        );
    }
}

#[tokio::test]
async fn tools_list_output_templates_include_initial_tool_name() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");

    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("missing tool name");
        if !tool_descriptor_should_attach_widget(name) {
            continue;
        }
        let output_template = tool
            .get("_meta")
            .and_then(|meta| meta.get("openai/outputTemplate"))
            .and_then(Value::as_str)
            .expect("missing output template");
        assert!(
            output_template.contains(&format!("toolName={name}")),
            "output template should include initial tool name for {name}: {output_template}"
        );
    }
}

#[tokio::test]
async fn browser_only_tools_list_exposes_required_catdesk_instruction() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Browser, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");
    assert_eq!(tools.len(), 1);
    let instruction = &tools[0];
    assert_eq!(
        instruction.get("name").and_then(Value::as_str),
        Some("catdesk_instruction")
    );
    assert!(
        instruction
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| description.contains("must call this tool successfully"))
    );
}

#[tokio::test]
async fn handle_request_requires_instruction_before_other_tools() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-instruction-gate-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "hello\n").expect("write file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request("read", json!({ "paths": ["notes.txt"] }));

    let blocked = handle_request(
        &req,
        &workspace_root_str,
        1,
        None,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await
    .expect("blocked tool response");
    assert_eq!(
        blocked
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        None
    );
    let blocked_structured = blocked
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing blocked structured content");
    assert_eq!(
        blocked_structured.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        blocked_structured.get("errorCode").and_then(Value::as_str),
        Some(CATDESK_INSTRUCTION_REQUIRED_CODE)
    );
    assert_eq!(
        blocked_structured.get("message").and_then(Value::as_str),
        Some(CATDESK_INSTRUCTION_REQUIRED_MESSAGE)
    );
    assert!(result_text(&blocked).contains("Call catdesk_instruction successfully"));
    let blocked_widget = blocked
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing instruction-required widget payload");
    assert_eq!(
        blocked_widget.get("payloadKind").and_then(Value::as_str),
        Some("instruction_required")
    );
    assert_eq!(
        blocked_widget.get("title").and_then(Value::as_str),
        Some("read")
    );
    assert_eq!(
        blocked_widget.get("state").and_then(Value::as_str),
        Some("failed")
    );
    assert_eq!(
        blocked_widget.get("toolName").and_then(Value::as_str),
        Some("read")
    );
    assert_eq!(
        blocked_widget.get("title").and_then(Value::as_str),
        Some("read")
    );
    assert!(blocked_widget.get("call").is_none());
    assert_eq!(
        blocked_widget.get("detail").and_then(Value::as_str),
        Some(CATDESK_INSTRUCTION_REQUIRED_WIDGET_MESSAGE)
    );
    assert_eq!(
        blocked_widget.get("hasChanges").and_then(Value::as_bool),
        Some(false)
    );

    let allowed = handle_request(
        &req,
        &workspace_root_str,
        1,
        None,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        true,
        &CommandJobManager::new(),
        &None,
    )
    .await
    .expect("allowed tool response");
    assert_eq!(
        allowed
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        None
    );
    assert_eq!(result_text(&allowed), "hello\n");

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn instruction_required_disable_skips_widget_payload() {
    let req = tool_call_request("read", json!({ "paths": ["notes.txt"] }));
    let response =
        catdesk_instruction_required_response_with_show_detail_mode(&req, ShowDetailMode::Disable);
    let result = response.result.as_ref().expect("missing result");
    let structured = result
        .get("structuredContent")
        .expect("missing structured content");

    assert_eq!(
        structured.get("errorCode").and_then(Value::as_str),
        Some(CATDESK_INSTRUCTION_REQUIRED_CODE)
    );
    assert_eq!(
        structured.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        result
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .is_none(),
        "Disable must not attach the instruction-required widget payload"
    );
}

#[test]
fn instruction_required_widget_uses_dedicated_detail_renderer() {
    assert!(CATDESK_WIDGET_HTML.contains("payloadKind === \"instruction_required\""));
    assert!(CATDESK_WIDGET_HTML.contains("renderInstructionRequiredPanel(view)"));
    assert!(CATDESK_WIDGET_HTML.contains("esc(current.toolName)"));
    assert!(CATDESK_WIDGET_HTML.contains("instruction-required-message"));
    assert!(CATDESK_WIDGET_HTML.contains("!isInstructionRequired && (view.call || view.detail)"));
}

#[tokio::test]
async fn browser_only_mode_can_call_catdesk_instruction() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-browser-instruction-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request("catdesk_instruction", json!({}));

    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Browser,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        None
    );
    assert!(result_text(&response).contains("CatDesk usage instructions"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_only_tools_list_exposes_only_local_read_tools() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::ReadOnly, &None).await;
    let names = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "catdesk_instruction",
            "read",
            "read_image",
            "search",
            "read_result",
            "search_result",
            "create_handoff"
        ]
    );
}

#[tokio::test]
async fn search_tool_schema_uses_pattern_and_ripgrep_options() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let search_tool = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("search"))
        .expect("missing search tool");
    let schema = search_tool
        .get("inputSchema")
        .and_then(Value::as_object)
        .expect("missing search schema");
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("missing search properties");

    assert!(properties.contains_key("pattern"));
    assert!(properties.contains_key("glob"));
    assert!(properties.contains_key("fixed_strings"));
    assert!(properties.contains_key("case_insensitive"));
    assert!(properties.contains_key("max_matches"));
    assert!(!properties.contains_key("query"));
    assert!(!properties.contains_key("limit"));
    assert_eq!(
        schema
            .get("required")
            .and_then(Value::as_array)
            .and_then(|required| required.first())
            .and_then(Value::as_str),
        Some("pattern")
    );
}

#[tokio::test]
async fn edit_tool_schema_uses_atomic_edits_array() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };

    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let edit_tool = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("edit"))
        .expect("missing edit tool");
    let schema = edit_tool
        .get("inputSchema")
        .and_then(Value::as_object)
        .expect("missing edit schema");
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("missing edit properties");

    assert!(properties.contains_key("path"));
    assert!(properties.contains_key("edits"));
    assert!(!properties.contains_key("old_string"));
    assert!(!properties.contains_key("new_string"));
    assert!(!properties.contains_key("replace_all"));
    assert_eq!(
        properties
            .get("edits")
            .and_then(|edits| edits.get("minItems"))
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        properties
            .get("edits")
            .and_then(|edits| edits.get("items"))
            .and_then(|items| items.get("oneOf"))
            .and_then(Value::as_array)
            .map(|variants| variants.len()),
        Some(2)
    );
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("missing edit required fields");
    assert!(required.iter().any(|field| field == "path"));
    assert!(required.iter().any(|field| field == "edits"));
}

#[tokio::test]
async fn edit_tool_rejects_legacy_top_level_replace_fields() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-edit-legacy-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "alpha\n").expect("write file");

    let req = tool_call_request(
        "edit",
        json!({
            "path": "notes.txt",
            "old_string": "alpha",
            "new_string": "ALPHA",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(content_text(&response), result_text(&response));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(result_text(&response), "Missing required parameter: edits");
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
        "alpha\n"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn search_tool_rejects_legacy_query_parameter() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-search-query-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    let req = tool_call_request(
        "search",
        json!({
            "query": "needle",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(content_text(&response), result_text(&response));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result_text(&response),
        "Missing required parameter: pattern"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn search_tool_rejects_invalid_optional_parameter_types() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-search-args-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    let req = tool_call_request(
        "search",
        json!({
            "pattern": "needle",
            "max_matches": "10",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(content_text(&response), result_text(&response));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result_text(&response),
        "Parameter max_matches must be a non-negative integer"
    );

    let req = tool_call_request(
        "search",
        json!({
            "pattern": "needle",
            "max_matches": 0,
        }),
    );
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(content_text(&response), result_text(&response));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result_text(&response),
        "max_matches must be between 1 and 500"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn search_tool_returns_matches_in_structured_and_widget_payloads() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-search-rg-{}", Uuid::new_v4()));
    std::fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "alpha1\n").expect("write notes");
    std::fs::write(
        workspace_root.join("src").join("main.rs"),
        "alpha1\nbeta\nalpha2\n",
    )
    .expect("write source");

    let req = tool_call_request(
        "search",
        json!({
            "pattern": "alpha[0-9]",
            "path": ".",
            "glob": "*.rs",
            "max_matches": 1,
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured.get("searchPattern").and_then(Value::as_str),
        Some("alpha[0-9]")
    );
    assert_eq!(
        structured.get("matchCount").and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        structured
            .get("searchBackend")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        structured
            .get("searchBackendNote")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(
        structured
            .get("searchResults")
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.get("path"))
            .and_then(Value::as_str),
        Some("src/main.rs")
    );

    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("searchPattern").and_then(Value::as_str),
        Some("alpha[0-9]")
    );
    assert!(
        widget_payload
            .get("searchBackend")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(
        widget_payload.get("searchPath").and_then(Value::as_str),
        Some(".")
    );
    assert_eq!(
        widget_payload
            .get("searchTruncated")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        widget_payload.get("matchCount").and_then(Value::as_u64),
        Some(1)
    );
    assert!(widget_payload.get("searchBackendNote").is_none());
    assert!(widget_payload.get("searchResults").is_none());
    assert!(widget_payload.get("searchQuery").is_none());
    assert!(widget_payload.get("filesScanned").is_none());

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn write_file_widget_payload_includes_changed_files_after_tool_call() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-write-file-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    let req = tool_call_request(
        "write",
        json!({
            "path": "notes.txt",
            "content": "hello world\n",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("write")
    );
    assert_eq!(
        widget_payload.get("path").and_then(Value::as_str),
        Some("notes.txt")
    );
    assert_eq!(
        widget_payload.get("bytesWritten").and_then(Value::as_u64),
        Some(12)
    );
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        widget_payload
            .get("changedFiles")
            .and_then(Value::as_array)
            .map(|files| files.len()),
        Some(1)
    );
    assert_eq!(
        widget_payload
            .get("changedFiles")
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .and_then(|file| file.get("path"))
            .and_then(Value::as_str),
        Some("notes.txt")
    );

    let _ = std::fs::remove_file(workspace_root.join("notes.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn create_handoff_prepares_library_artifact_without_workspace_changes() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-handoff-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let req = tool_call_request(
        "create_handoff",
        json!({
            "goal": "Finish session handoff support",
            "completed": ["Added the MCP tool"],
            "decisions": ["Store handoffs in ChatGPT Library"],
            "validation": ["cargo test handoff"],
            "next_steps": ["Update documentation"],
            "notes": "Keep the handoff concise."
        }),
    );
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("create_handoff")
    );
    let filename = structured
        .get("filename")
        .and_then(Value::as_str)
        .expect("missing filename");
    let search_prefix = structured
        .get("searchPrefix")
        .and_then(Value::as_str)
        .expect("missing search prefix");
    let content = structured
        .get("content")
        .and_then(Value::as_str)
        .expect("missing content");
    assert!(filename.starts_with(search_prefix));
    assert!(filename.ends_with(".md"));
    assert!(search_prefix.starts_with("catdesk_handoff_"));
    assert!(content.contains("## Goal\n\nFinish session handoff support"));
    assert!(content.contains("- Added the MCP tool"));
    assert!(content.contains("## Git context\n\n_Git repository not detected._"));
    assert!(content.contains("- Update documentation"));
    assert_eq!(
        structured.get("gitAvailable").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        structured
            .get("gitStatusAvailable")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        structured.get("bytes").and_then(Value::as_u64),
        Some(content.len() as u64)
    );
    assert!(!workspace_root.join(".catdesk").exists());

    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("create_handoff")
    );
    assert_eq!(
        widget_payload.get("filename").and_then(Value::as_str),
        Some(filename)
    );
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(false)
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn create_handoff_is_available_in_read_only_mode() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-handoff-read-only-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request(
        "create_handoff",
        json!({
            "goal": "Prepare context without changing the workspace"
        }),
    );

    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::ReadOnly,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;
    assert!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .is_none()
    );
    assert!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("filename"))
            .and_then(Value::as_str)
            .is_some_and(|filename| filename.starts_with("catdesk_handoff_"))
    );
    assert!(!workspace_root.join(".catdesk").exists());

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn project_instruction_layers_workspace_then_project_agents_and_uses_project_handoff_identity() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-instruction-project-{}", Uuid::new_v4()));
    let project = workspace_root.join("repo-a");
    std::fs::create_dir_all(project.join(".git")).expect("create project git marker");
    std::fs::write(workspace_root.join("AGENTS.md"), "workspace-layer-rule\n")
        .expect("write workspace agents");
    std::fs::write(project.join("AGENTS.md"), "project-layer-rule\n")
        .expect("write project agents");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let instruction = catdesk_instruction_text_for_project(
        &workspace_root_str,
        Mode::Both,
        ToolMode::MultiTools,
        Some(&project),
    )
    .expect("build project instruction");

    let workspace_pos = instruction
        .find("workspace-layer-rule")
        .expect("workspace AGENTS layer");
    let project_pos = instruction
        .find("project-layer-rule")
        .expect("project AGENTS layer");
    assert!(
        workspace_pos < project_pos,
        "project instructions must be more specific"
    );
    let project_prefix = handoff::handoff_search_prefix(project.to_string_lossy().as_ref())
        .expect("project handoff prefix");
    assert!(instruction.contains(&project_prefix));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn project_handoff_uses_active_project_identity_and_git_context() {
    let _env = env_lock();
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-handoff-project-{}", Uuid::new_v4()));
    let project = workspace_root.join("repo-a");
    std::fs::create_dir_all(&project).expect("create project");
    let git_status = std::process::Command::new("git")
        .args(["init", "-b", "project-branch"])
        .current_dir(&project)
        .status()
        .expect("git init");
    assert!(git_status.success());
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request("create_handoff", json!({ "goal": "continue project" }));

    let response = handle_create_handoff_for_project(&req, &workspace_root_str, Some(&project));
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("handoff structured content");
    let expected_filename = handoff::handoff_filename(project.to_string_lossy().as_ref())
        .expect("project handoff filename");
    assert_eq!(
        structured.get("filename").and_then(Value::as_str),
        Some(expected_filename.as_str())
    );
    assert_eq!(
        structured.get("gitBranch").and_then(Value::as_str),
        Some("project-branch")
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_mentions_durable_command_results() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-durable-jobs-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(instruction.contains("survive a CatDesk restart"));
    assert!(instruction.contains("interrupted"));
    assert!(instruction.contains("abandoned"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_mentions_read_image_for_image_reading() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-read-image-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(instruction.contains("read_image"));
    assert!(instruction.contains("native image content"));

    // read_image is a read-only tool, so the read-only mode instruction
    // must mention it too, not just the full MultiTools one.
    let read_only = catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::ReadOnly)
        .expect("build read-only instruction");
    assert!(read_only.contains("read_image"));
}

#[test]
fn read_image_analyze_requires_configured_backend() {
    // Testy biegają równolegle w jednym procesie, a edition 2024 wymaga
    // jawnego unsafe dla mutacji env. Mutex serializuje ten test; żaden
    // inny test nie czyta tych zmiennych, więc mutacja jest izolowana.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("CATDESK_GEMINI_API_KEY");
    }

    let workspace_root = read_workspace("image-analyze-unconfigured");
    write_test_image(
        &workspace_root.join("pic.png"),
        image::ImageFormat::Png,
        16,
        8,
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let response = runtime.block_on(read_image_response(
        &workspace_root,
        json!({ "path": "pic.png", "analyze": true }),
        ToolMode::MultiTools,
    ));

    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    let text = result_text(&response);
    assert!(
        text.contains("Vision analysis is not configured"),
        "unexpected error text: {text}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_analyze_rejects_non_boolean_or_string_values() {
    let workspace_root = read_workspace("image-analyze-bad-param");
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "pic.png", "analyze": 123 }),
        ToolMode::MultiTools,
    )
    .await;
    let text = result_text(&response);
    assert!(
        text.contains("analyze must be a boolean or a string"),
        "unexpected error text: {text}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_points_new_sessions_to_library_handoff_search() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-handoff-instruction-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let search_prefix =
        handoff::handoff_search_prefix(&workspace_root_str).expect("handoff search prefix");
    let filename = handoff::handoff_filename(&workspace_root_str).expect("handoff filename");

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(instruction.contains("files.search"));
    assert!(instruction.contains("persistent ChatGPT Library"));
    assert!(instruction.contains(&search_prefix));
    assert!(instruction.contains(&filename));
    assert!(instruction.contains("If exactly one is found"));
    assert!(instruction.contains("If multiple matching handoffs are found"));
    assert!(
        instruction.contains("delete that Library file only after it has been read successfully")
    );
    assert!(instruction.contains("Library Search must be enabled"));
    assert!(instruction.contains("use create_handoff"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_describes_offline_sandbox_and_connector_error_reporting() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-sandbox-offline-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(
        instruction.contains("which does not provide an internet connection"),
        "sandbox must be described as offline: {instruction}"
    );
    assert!(instruction.contains("use Workspace first"));
    assert!(
        instruction.contains("explicitly report the raw error to the user"),
        "connector errors must be reported to the user: {instruction}"
    );
    assert!(instruction.contains("Do NOT fall back to the sandbox container"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_tells_agents_not_to_write_catdesk_trailers() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-co-author-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(instruction.contains("Do not manually add CatDesk co-author attribution"));
    assert!(instruction.contains("CatDesk manages that automatically"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_steers_long_commands_to_start_and_poll() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-long-command-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(
        instruction.contains("more than about 20 seconds"),
        "commands likely to create a long silent foreground call must be steered to background jobs: {instruction}"
    );
    assert!(
        instruction.contains("longer than about two minutes must never run through run_command"),
        "the hard synchronous ceiling must remain documented: {instruction}"
    );
    assert!(
        instruction
            .contains("start them with start_command and read their progress with poll_command"),
        "long commands must be steered to start_command + poll_command: {instruction}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_drops_link_segment_guidance_without_code_support() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-no-link-segment-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    assert!(
        !instruction.contains("link_"),
        "link_ segment guidance must stay removed: no code path produces such tool paths: {instruction}"
    );
    assert!(!instruction.contains("api_tool returns"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_keeps_richer_divergent_content() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-rich-guard-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(
        workspace_root.join("AGENTS.md"),
        "workspace-layer-guard-rule\n",
    )
    .expect("write workspace agents");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");
    // AGENTS.md layering (upstream catdesk_instruction_text has no layers).
    // The exact label depends on agents_path_mode (an existing workspace
    // AGENTS.md is reported as "Configured AGENTS.md instructions:"),
    // so assert the label family and the layered content itself.
    assert!(instruction.contains("AGENTS.md instructions:"));
    assert!(instruction.contains("workspace-layer-guard-rule"));
    // read_image vision guidance.
    assert!(instruction.contains("read_image"));
    assert!(instruction.contains("native image content"));
    // Handoff Library search.
    assert!(instruction.contains("files.search"));
    assert!(instruction.contains("persistent ChatGPT Library"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn edit_file_applies_atomic_batch_and_reports_changed_file() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-edit-file-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "alpha\nbeta\ngamma\n").expect("write file");

    let req = tool_call_request(
        "edit",
        json!({
            "path": "notes.txt",
            "edits": [
                {
                    "type": "replace",
                    "old_string": "alpha",
                    "new_string": "ALPHA",
                },
                {
                    "type": "range",
                    "start_line": 2,
                    "end_line": 3,
                    "old_text": "beta\ngamma\n",
                    "new_text": "BETA\nGAMMA\n",
                }
            ],
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
        "ALPHA\nBETA\nGAMMA\n"
    );
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("edit")
    );
    assert_eq!(
        structured.get("operationCount").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        structured.get("appliedOperations").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        structured
            .get("replacedOccurrences")
            .and_then(Value::as_u64),
        Some(2)
    );

    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("edit")
    );
    assert_eq!(
        widget_payload.get("path").and_then(Value::as_str),
        Some("notes.txt")
    );
    assert_eq!(
        widget_payload.get("bytesWritten").and_then(Value::as_u64),
        Some(17)
    );
    assert_eq!(
        widget_payload.get("operationCount").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        widget_payload
            .get("appliedOperations")
            .and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        widget_payload
            .get("replacedOccurrences")
            .and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        widget_payload
            .get("changedFiles")
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .and_then(|file| file.get("path"))
            .and_then(Value::as_str),
        Some("notes.txt")
    );

    let _ = std::fs::remove_file(workspace_root.join("notes.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn edit_file_rejects_multiple_matches_without_replace_all() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-edit-multi-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "same\nsame\n").expect("write file");

    let req = tool_call_request(
        "edit",
        json!({
            "path": "notes.txt",
            "edits": [{
                "type": "replace",
                "old_string": "same",
                "new_string": "diff",
            }],
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(content_text(&response), result_text(&response));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        result_text(&response).contains("old_string matched 2 occurrences"),
        "unexpected result text: {}",
        result_text(&response)
    );
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
        "same\nsame\n"
    );

    let _ = std::fs::remove_file(workspace_root.join("notes.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_listing_intercept_uses_list_widget_payload() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-command-list-{}", Uuid::new_v4()));
    std::fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
    std::fs::write(workspace_root.join("src/lib.rs"), "pub fn ping() {}\n").expect("write file");

    let req = tool_call_request(
        "run_command",
        json!({
            "command": "find src",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("run_command")
    );
    assert_eq!(
        structured
            .get("interceptedToolName")
            .and_then(Value::as_str),
        Some("list_files")
    );
    assert_eq!(
        structured
            .get("interceptedCommandName")
            .and_then(Value::as_str),
        Some("find")
    );
    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("list_files")
    );
    assert_eq!(
        widget_payload.get("listPath").and_then(Value::as_str),
        Some("src")
    );
    assert_eq!(
        widget_payload
            .get("listEntries")
            .and_then(Value::as_array)
            .map(|entries| entries.len()),
        Some(1)
    );

    let _ = std::fs::remove_file(workspace_root.join("src/lib.rs"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_ls_listing_intercept_uses_run_command_widget_payload() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-command-ls-{}", Uuid::new_v4()));
    std::fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
    std::fs::write(workspace_root.join("src/lib.rs"), "pub fn ping() {}\n").expect("write file");

    let req = tool_call_request(
        "run_command",
        json!({
            "command": "ls -Ra src",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("run_command")
    );
    assert_eq!(
        structured
            .get("interceptedToolName")
            .and_then(Value::as_str),
        Some("list_files")
    );
    assert_eq!(
        structured
            .get("interceptedCommandName")
            .and_then(Value::as_str),
        Some("ls")
    );
    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("run_command")
    );
    assert_eq!(
        widget_payload.get("command").and_then(Value::as_str),
        Some("ls -Ra src")
    );
    assert!(
        widget_payload
            .get("output")
            .and_then(Value::as_str)
            .is_some_and(|output| output.contains("file src/lib.rs"))
    );

    let _ = std::fs::remove_file(workspace_root.join("src/lib.rs"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_mv_intercept_moves_into_directory_and_reports_changed_files() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-run-command-mv-{}", Uuid::new_v4()));
    std::fs::create_dir_all(workspace_root.join("dest")).expect("create workspace");
    std::fs::write(workspace_root.join("old.txt"), "hello\n").expect("write file");

    let req = tool_call_request(
        "run_command",
        json!({
            "command": "mv old.txt dest",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    assert!(!workspace_root.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("dest/old.txt")).expect("read moved file"),
        "hello\n"
    );
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured
            .get("interceptedToolName")
            .and_then(Value::as_str),
        Some("move_path")
    );
    assert_eq!(
        structured.get("success").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        structured
            .get("destinationOperandWasDirectory")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        structured.get("resolvedTo").and_then(Value::as_str),
        Some("dest/old.txt")
    );

    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(true)
    );
    let changed_paths = widget_payload
        .get("changedFiles")
        .and_then(Value::as_array)
        .expect("missing changed files")
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(changed_paths.contains(&"old.txt"));
    assert!(changed_paths.contains(&"dest/old.txt"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn run_command_mv_intercept_no_clobber_skips_existing_destination() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-run-command-mv-no-clobber-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("old.txt"), "old\n").expect("write source");
    std::fs::write(workspace_root.join("new.txt"), "new\n").expect("write destination");

    let req = tool_call_request(
        "run_command",
        json!({
            "command": "mv -n old.txt new.txt",
        }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("old.txt")).expect("read source"),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("new.txt")).expect("read destination"),
        "new\n"
    );
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured
            .get("interceptedToolName")
            .and_then(Value::as_str),
        Some("move_path")
    );
    assert_eq!(
        structured.get("overwrite").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        structured.get("skipped").and_then(Value::as_bool),
        Some(true)
    );

    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(false)
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn catdesk_instruction_result_does_not_emit_text_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-instruction-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    let req = tool_call_request("catdesk_instruction", json!({}));
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert!(
        structured
            .get("instructionText")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .and_then(|payload| payload.get("showDetailMode"))
            .and_then(Value::as_str),
        Some("expanded")
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn catdesk_instruction_disable_skips_dedicated_widget_payload() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-instruction-disable-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let req = tool_call_request("catdesk_instruction", json!({}));

    let response = handle_catdesk_instruction_with_show_detail_mode(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        ShowDetailMode::Disable,
        None,
    );

    assert!(result_text(&response).contains("CatDesk usage instructions"));
    assert!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("_meta"))
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .is_none(),
        "Disable must skip the dedicated catdesk_instruction widget payload"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_returns_structured_text_without_text_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-read-file-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "hello world\n").expect("write file");

    let req = tool_call_request("read", json!({ "paths": ["notes.txt"] }));
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured["files"][0]["text"].as_str(),
        Some("hello world\n")
    );

    let _ = std::fs::remove_file(workspace_root.join("notes.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_mixed_batch_stays_a_success_without_text_content() {
    let workspace_root = read_workspace("mixed-batch");
    std::fs::write(workspace_root.join("notes.txt"), "hello world\n").expect("write file");

    let req = tool_call_request("read", json!({ "paths": ["notes.txt", "missing.txt"] }));
    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    // A partially-successful batch is not a failed call: per-entry errors
    // belong to structuredContent.files[], not to the call-level error
    // channel, so no text content is attached.
    assert_ne!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured["files"][0]["text"].as_str(),
        Some("hello world\n")
    );
    assert!(
        structured["files"][1]["error"].as_str().is_some(),
        "the failed entry must still report its error: {structured}"
    );

    let _ = std::fs::remove_file(workspace_root.join("notes.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_success_batch_keeps_content_empty() {
    let workspace_root = read_workspace("success-batch");
    std::fs::write(workspace_root.join("alpha.txt"), "alpha\n").expect("write file");
    std::fs::write(workspace_root.join("beta.txt"), "beta\n").expect("write file");

    let req = tool_call_request("read", json!({ "paths": ["alpha.txt", "beta.txt"] }));
    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_ne!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    for (index, expected) in ["alpha\n", "beta\n"].iter().enumerate() {
        assert_eq!(structured["files"][index]["text"].as_str(), Some(*expected));
        assert!(
            structured["files"][index].get("error").is_none()
                || structured["files"][index]["error"].is_null(),
            "successful entries must not carry errors: {structured}"
        );
    }

    let _ = std::fs::remove_file(workspace_root.join("alpha.txt"));
    let _ = std::fs::remove_file(workspace_root.join("beta.txt"));
    let _ = std::fs::remove_dir_all(workspace_root);
}

async fn read_batch(workspace_root: &Path, paths: Value) -> Value {
    let req = tool_call_request("read", json!({ "paths": paths }));
    handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await
    .result
    .and_then(|result| result.get("structuredContent").cloned())
    .expect("missing structured content")
}

// Line breaks keep the tokenizer off one huge pre-token; BPE is quadratic there.
fn filler(bytes: usize) -> String {
    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n".repeat(bytes / 41 + 1)[..bytes].to_string()
}

fn read_workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("catdesk-mcp-read-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create workspace");
    root
}

#[tokio::test]
async fn read_image_returns_native_mcp_image_content() {
    let workspace_root = read_workspace("image-native-content");
    write_test_image(
        &workspace_root.join("visual.bin"),
        image::ImageFormat::Png,
        64,
        32,
    );

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "visual.bin" }),
        ToolMode::MultiTools,
    )
    .await;
    let content = image_content(&response);
    assert_eq!(content["type"], json!("image"));
    assert_eq!(content["mimeType"], json!("image/png"));
    let data = content["data"].as_str().expect("missing base64 data");
    assert!(!data.is_empty());
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("valid base64");
    assert!(!decoded.is_empty());
    assert_eq!(
        image::load_from_memory(&decoded)
            .expect("decodable image")
            .dimensions(),
        (64, 32)
    );

    assert_no_structured_content(&response);
    assert!(
        !response
            .result
            .as_ref()
            .and_then(|result| result.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|content| content
                .iter()
                .any(|entry| entry.get("type").and_then(Value::as_str) == Some("text"))),
        "metadata text blocks would be lifted back into structuredContent by the shared tool-result post-processing"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_supports_jpeg_and_webp_by_detected_format() {
    let workspace_root = read_workspace("image-formats");
    write_test_image(
        &workspace_root.join("photo.png"),
        image::ImageFormat::Jpeg,
        48,
        24,
    );
    write_test_image(
        &workspace_root.join("texture.jpg"),
        image::ImageFormat::WebP,
        40,
        20,
    );

    for (path, expected_mime) in [("photo.png", "image/jpeg"), ("texture.jpg", "image/webp")] {
        let response = read_image_response(
            &workspace_root,
            json!({ "path": path }),
            ToolMode::MultiTools,
        )
        .await;
        assert_eq!(image_content(&response)["mimeType"], json!(expected_mime));
        assert_no_structured_content(&response);
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            None
        );
    }

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_is_available_in_read_only_mode() {
    let workspace_root = read_workspace("image-read-only");
    write_test_image(
        &workspace_root.join("safe.png"),
        image::ImageFormat::Png,
        32,
        16,
    );

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "safe.png" }),
        ToolMode::ReadOnly,
    )
    .await;
    assert_eq!(image_content(&response)["type"], json!("image"));
    assert_no_structured_content(&response);

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_rejects_non_image_missing_file_and_directory() {
    let workspace_root = read_workspace("image-errors");
    std::fs::write(workspace_root.join("notes.txt"), "not an image\n").expect("write non-image");
    std::fs::create_dir_all(workspace_root.join("folder")).expect("create directory");

    for (arguments, expected) in [
        (
            json!({ "path": "notes.txt" }),
            "Unsupported or invalid image",
        ),
        (json!({ "path": "missing.png" }), "File not found"),
        (json!({ "path": "folder" }), "Not a file"),
    ] {
        let response = read_image_response(&workspace_root, arguments, ToolMode::MultiTools).await;
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            result_text(&response).contains(expected),
            "expected error containing {expected:?}, got {:?}",
            result_text(&response)
        );
    }

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_rejects_path_outside_workspace() {
    let workspace_root = read_workspace("image-outside");
    let outside_path = workspace_root
        .parent()
        .expect("workspace parent")
        .join(format!("catdesk-outside-{}.png", Uuid::new_v4()));
    write_test_image(&outside_path, image::ImageFormat::Png, 16, 16);

    let relative = format!(
        "../{}",
        outside_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("outside filename")
    );
    for path in [relative, outside_path.to_string_lossy().into_owned()] {
        let response = read_image_response(
            &workspace_root,
            json!({ "path": path }),
            ToolMode::MultiTools,
        )
        .await;

        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(result_text(&response).contains("Path escapes workspace root"));
    }

    let _ = std::fs::remove_file(outside_path);
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[cfg(unix)]
#[tokio::test]
async fn read_image_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let workspace_root = read_workspace("image-symlink");
    let outside_path = workspace_root
        .parent()
        .expect("workspace parent")
        .join(format!("catdesk-symlink-outside-{}.png", Uuid::new_v4()));
    write_test_image(&outside_path, image::ImageFormat::Png, 16, 16);
    symlink(&outside_path, workspace_root.join("escape.png")).expect("create symlink");

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "escape.png" }),
        ToolMode::MultiTools,
    )
    .await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(result_text(&response).contains("Path escapes workspace root"));

    let _ = std::fs::remove_file(workspace_root.join("escape.png"));
    let _ = std::fs::remove_file(outside_path);
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_rejects_oversized_input_before_decoding() {
    let workspace_root = read_workspace("image-size-limit");
    let oversized = workspace_root.join("huge.png");
    let file = std::fs::File::create(&oversized).expect("create oversized file");
    file.set_len(workspace_tools::MAX_IMAGE_BYTES + 1)
        .expect("size oversized file");

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "huge.png" }),
        ToolMode::MultiTools,
    )
    .await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(result_text(&response).contains("Image is too large"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_rejects_excessive_requested_dimensions() {
    let workspace_root = read_workspace("image-output-limit");
    write_test_image(
        &workspace_root.join("small.png"),
        image::ImageFormat::Png,
        32,
        32,
    );

    let response = read_image_response(
        &workspace_root,
        json!({
            "path": "small.png",
            "max_width": workspace_tools::MAX_IMAGE_OUTPUT_DIMENSION + 1,
        }),
        ToolMode::MultiTools,
    )
    .await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(result_text(&response).contains("max_width must not exceed"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_resizes_large_image_and_preserves_aspect_ratio() {
    let workspace_root = read_workspace("image-resize");
    write_test_image(
        &workspace_root.join("portrait.png"),
        image::ImageFormat::Png,
        450,
        1800,
    );

    let response = read_image_response(
        &workspace_root,
        json!({ "path": "portrait.png" }),
        ToolMode::MultiTools,
    )
    .await;
    let (width, height) = decoded_image_dimensions(image_content(&response));

    assert_eq!((width, height), (400, 1600));
    assert_eq!(u64::from(width) * 1800, u64::from(height) * 450);
    assert_no_structured_content(&response);

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_image_respects_custom_bounds_without_upscaling() {
    let workspace_root = read_workspace("image-custom-bounds");
    write_test_image(
        &workspace_root.join("wide.png"),
        image::ImageFormat::Png,
        800,
        400,
    );
    write_test_image(
        &workspace_root.join("small.png"),
        image::ImageFormat::Png,
        120,
        80,
    );

    let resized = read_image_response(
        &workspace_root,
        json!({ "path": "wide.png", "max_width": 300, "max_height": 300 }),
        ToolMode::MultiTools,
    )
    .await;
    assert_eq!(
        decoded_image_dimensions(image_content(&resized)),
        (300, 150)
    );

    let unchanged = read_image_response(
        &workspace_root,
        json!({ "path": "small.png", "max_width": 1600, "max_height": 1600 }),
        ToolMode::MultiTools,
    )
    .await;
    // No upscaling: the 120x80 original comes back byte-identical.
    assert_eq!(
        decoded_image_dimensions(image_content(&unchanged)),
        (120, 80)
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_returns_every_named_file_in_one_call() {
    let workspace_root = read_workspace("batch");
    for (name, body) in [("a.txt", "alpha\n"), ("b.txt", "beta\n")] {
        std::fs::write(workspace_root.join(name), body).expect("write file");
    }

    std::fs::write(workspace_root.join("empty.txt"), "").expect("write file");

    let structured = read_batch(&workspace_root, json!(["a.txt", "empty.txt", "b.txt"])).await;
    let files = structured["files"].as_array().expect("missing files");

    assert_eq!(structured["fileCount"], json!(3));
    assert_eq!(files[0]["text"], json!("alpha\n"));
    assert_eq!(files[1]["text"], json!(""));
    assert_eq!(
        files[1]["truncated"],
        json!(false),
        "an empty file is whole"
    );
    assert_eq!(files[2]["text"], json!("beta\n"));
    assert_eq!(structured["batchTruncated"], json!(false));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_stops_reading_once_the_batch_budget_is_spent() {
    let workspace_root = read_workspace("batch-budget");
    // The first file alone spends the whole budget, so the rest must come
    // back with metadata and no text.
    let names = ["a.txt", "b.txt", "c.txt"];
    for name in names {
        std::fs::write(
            workspace_root.join(name),
            filler(workspace_tools::MAX_READ_BATCH_BYTES),
        )
        .expect("write file");
    }

    let structured = read_batch(&workspace_root, json!(names)).await;
    let files = structured["files"].as_array().expect("missing files");
    let total: usize = files
        .iter()
        .map(|file| file["text"].as_str().unwrap_or_default().len())
        .sum();

    assert!(
        total <= workspace_tools::MAX_READ_BATCH_BYTES,
        "combined text {total} exceeded the batch cap"
    );
    assert_eq!(structured["batchTruncated"], json!(true));
    for skipped in &files[1..] {
        assert_eq!(
            skipped["bytes"],
            json!(0),
            "over-budget file was still read"
        );
        assert_eq!(
            skipped["lineCount"],
            json!(0),
            "over-budget file was scanned"
        );
        assert_eq!(skipped["truncated"], json!(true));
        assert_eq!(
            skipped["budgetTruncated"],
            json!(true),
            "a file the budget never reached must still say a smaller retry helps"
        );
        assert!(
            skipped["sizeBytes"].as_u64().unwrap_or(0) > 0,
            "missing metadata"
        );
    }

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_widget_payload_carries_per_file_failures() {
    let workspace_root = read_workspace("widget-failures");
    std::fs::write(workspace_root.join("a.txt"), "alpha\n").expect("write file");

    let req = tool_call_request("read", json!({ "paths": ["a.txt", "missing.txt"] }));
    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    let payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    let failed = payload["failedFiles"]
        .as_array()
        .expect("missing failedFiles in widget payload");

    assert_eq!(failed.len(), 1);
    assert_eq!(payload["path"], json!("a.txt"));
    assert_eq!(payload["renderedFileCount"], json!(1));
    assert_eq!(failed[0]["path"], json!("missing.txt"));
    assert_eq!(failed[0]["error"], json!("File not found"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_rejects_a_directory_the_same_way_wherever_it_lands() {
    let workspace_root = read_workspace("dir-entry");
    std::fs::create_dir_all(workspace_root.join("subdir")).expect("create dir");
    std::fs::write(workspace_root.join("a.txt"), "alpha\n").expect("write file");

    let structured = read_batch(&workspace_root, json!(["subdir", "a.txt"])).await;
    let files = structured["files"].as_array().expect("missing files");

    assert!(
        files[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Not a file"),
        "a directory must be an error, not an empty file: {:?}",
        files[0]
    );
    assert_eq!(files[1]["text"], json!("alpha\n"));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[cfg(unix)]
#[tokio::test]
async fn read_tool_does_not_let_an_unreadable_file_shrink_the_others() {
    let workspace_root = read_workspace("unreadable");
    let big = workspace_tools::MAX_READ_BATCH_BYTES - 8192;
    std::fs::write(workspace_root.join("app.js"), filler(big)).expect("write file");
    std::fs::write(workspace_root.join("locked.txt"), filler(100 * 1024)).expect("write file");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        workspace_root.join("locked.txt"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("lock file");

    let structured = read_batch(&workspace_root, json!(["locked.txt", "app.js"])).await;
    let files = structured["files"].as_array().expect("missing files");

    assert!(files[0]["error"].is_string());
    assert_eq!(
        files[1]["bytes"].as_u64().unwrap(),
        big as u64,
        "the readable file lost budget to one that never opened"
    );
    assert_eq!(files[1]["truncated"], json!(false));

    let _ = std::fs::set_permissions(
        workspace_root.join("locked.txt"),
        std::fs::Permissions::from_mode(0o644),
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_does_not_blame_the_budget_for_lossy_expansion() {
    let workspace_root = read_workspace("lossy-big");
    // Lossy conversion triples these past the cap.
    let mut bytes = Vec::new();
    while bytes.len() < 200 * 1024 {
        bytes.extend(std::iter::repeat_n(0xE9_u8, 40));
        bytes.push(b'\n');
    }
    std::fs::write(workspace_root.join("bin.dat"), bytes).expect("write file");

    let structured = read_batch(&workspace_root, json!(["bin.dat"])).await;

    assert_eq!(structured["files"][0]["truncated"], json!(true));
    assert_eq!(structured["batchTruncated"], json!(false));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_counts_lines_over_what_it_returned() {
    let workspace_root = read_workspace("lines");
    let cap = workspace_tools::MAX_READ_BATCH_BYTES;
    std::fs::write(workspace_root.join("a.txt"), filler(cap - 1)).expect("write file");
    std::fs::write(workspace_root.join("b.txt"), "line\n".repeat(cap / 5 + 200))
        .expect("write file");

    let structured = read_batch(&workspace_root, json!(["a.txt", "b.txt"])).await;
    let b = structured["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == json!("b.txt"))
        .expect("missing b.txt");

    assert_eq!(b["bytes"], json!(1));
    assert_eq!(
        b["lineCount"],
        json!(1),
        "lines of text that was thrown away"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_charges_one_file_once_however_many_times_it_is_named() {
    let workspace_root = read_workspace("dup");
    std::fs::write(workspace_root.join("a.txt"), filler(300 * 1024)).expect("write file");

    let structured = read_batch(&workspace_root, json!(["a.txt", "a.txt"])).await;
    let files = structured["files"].as_array().expect("missing files");

    assert_eq!(files.len(), 1, "one file, one entry");
    assert_eq!(files[0]["truncated"], json!(false));
    assert_eq!(structured["batchTruncated"], json!(false));

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[cfg(unix)]
#[tokio::test]
async fn read_tool_still_reports_an_unreadable_file_past_the_budget() {
    let workspace_root = read_workspace("locked-past-budget");
    std::fs::write(
        workspace_root.join("big.txt"),
        filler(workspace_tools::MAX_READ_BATCH_BYTES),
    )
    .expect("write file");
    std::fs::write(workspace_root.join("locked.txt"), filler(600 * 1024)).expect("write file");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        workspace_root.join("locked.txt"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod");

    let structured = read_batch(&workspace_root, json!(["big.txt", "locked.txt"])).await;
    let locked = &structured["files"][1];

    // Skipping the open would have called this a budget cut, which tells
    // the model a smaller retry returns the file. It never will.
    assert!(
        locked["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Permission denied"),
        "an unreadable file must say so even with the budget gone: {locked:?}"
    );
    assert_eq!(locked["budgetTruncated"], json!(false));

    let _ = std::fs::set_permissions(
        workspace_root.join("locked.txt"),
        std::fs::Permissions::from_mode(0o644),
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_budget_goes_to_the_smallest_files_first() {
    let workspace_root = read_workspace("batch-order");
    std::fs::write(
        workspace_root.join("big.log"),
        filler(workspace_tools::MAX_READ_BATCH_BYTES),
    )
    .expect("write file");
    std::fs::write(workspace_root.join("a.txt"), "alpha\n").expect("write file");
    std::fs::write(workspace_root.join("b.txt"), "beta\n").expect("write file");

    let structured = read_batch(&workspace_root, json!(["big.log", "a.txt", "b.txt"])).await;
    let files = structured["files"].as_array().expect("missing files");

    assert_eq!(files[0]["path"], json!("big.log"));
    assert_eq!(files[1]["text"], json!("alpha\n"));
    assert_eq!(files[2]["text"], json!("beta\n"));
    assert!(
        files[0]["bytes"].as_u64().unwrap() < workspace_tools::MAX_READ_BATCH_BYTES as u64,
        "the big file should have left room for the small ones"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn poll_command_rejects_wait_above_stream_safe_ceiling() {
    let jobs = CommandJobManager::new();
    let req = tool_call_request(
        "poll_command",
        json!({"job_id": "missing-job", "wait_ms": 15_001}),
    );
    let response = handle_poll_command(&req, &jobs).await;
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        result_text(&response).contains("wait_ms must be at most 15000"),
        "unexpected error: {}",
        result_text(&response)
    );
}

#[tokio::test]
async fn poll_command_waits_for_progress_unless_told_not_to() {
    let jobs = CommandJobManager::new();
    let workspace_root = read_workspace("poll-default");
    let command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 400"
    } else {
        "sleep 0.4"
    };
    let started = jobs
        .start(command.into(), workspace_root.clone(), 60_000, None)
        .await
        .expect("start job");
    let job_id = started.snapshot.job_id.clone();

    let poll = |args: Value| {
        let req = tool_call_request("poll_command", args);
        let jobs = jobs.clone();
        async move {
            handle_poll_command(&req, &jobs)
                .await
                .result
                .and_then(|result| result.get("structuredContent").cloned())
                .expect("missing structured content")
        }
    };

    let immediate = poll(json!({ "job_id": job_id, "wait_ms": 0 })).await;
    assert!(
        matches!(immediate["state"].as_str(), Some("queued" | "running")),
        "0 must return immediately with the current nonterminal state"
    );

    let waited = poll(json!({ "job_id": job_id })).await;
    assert_ne!(
        waited["state"], immediate["state"],
        "omitting wait_ms must block until there is progress"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_says_which_truncations_a_retry_would_fix() {
    let workspace_root = read_workspace("retryable");
    let cap = workspace_tools::MAX_READ_BATCH_BYTES;
    std::fs::write(workspace_root.join("a.txt"), filler(cap - 1)).expect("write file");
    std::fs::write(workspace_root.join("b.txt"), filler(cap + 4096)).expect("write file");

    let structured = read_batch(&workspace_root, json!(["a.txt", "b.txt"])).await;
    let files = structured["files"].as_array().expect("missing files");
    let b = files
        .iter()
        .find(|file| file["path"] == json!("b.txt"))
        .expect("missing b.txt");

    assert_eq!(b["truncated"], json!(true));
    assert_eq!(b["budgetTruncated"], json!(true), "a smaller retry helps");

    // The same file alone is cut by the per-file cap instead.
    let alone = read_batch(&workspace_root, json!(["b.txt"])).await;
    assert_eq!(alone["files"][0]["truncated"], json!(true));
    assert_eq!(
        alone["files"][0]["budgetTruncated"],
        json!(false),
        "no retry returns the rest"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_does_not_head_the_result_with_a_failure() {
    let workspace_root = read_workspace("head-failure");
    std::fs::write(workspace_root.join("__init__.py"), "").expect("write file");

    let structured = read_batch(&workspace_root, json!(["missing.txt", "__init__.py"])).await;

    assert_eq!(
        structured["path"],
        json!("__init__.py"),
        "an empty file still beats one that could not be read"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_does_not_head_the_result_with_an_empty_file() {
    let workspace_root = read_workspace("head-empty");
    std::fs::write(workspace_root.join("__init__.py"), "").expect("write file");
    std::fs::write(workspace_root.join("whole.txt"), "alpha\n").expect("write file");

    let structured = read_batch(&workspace_root, json!(["__init__.py", "whole.txt"])).await;

    assert_eq!(structured["bytes"], json!(6));
    assert_eq!(
        structured["path"],
        json!("whole.txt"),
        "an empty file contributed none of those bytes"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_heads_the_result_with_a_whole_file() {
    let workspace_root = read_workspace("head-whole");
    let cap = workspace_tools::MAX_READ_BATCH_BYTES;
    // Sorted smallest first, pad.txt is read whole and huge.txt gets a sliver.
    std::fs::write(workspace_root.join("pad.txt"), filler(cap - 4)).expect("write file");
    std::fs::write(workspace_root.join("huge.txt"), filler(cap + 4096)).expect("write file");

    let structured = read_batch(&workspace_root, json!(["huge.txt", "pad.txt"])).await;

    assert_eq!(
        structured["path"],
        json!("pad.txt"),
        "the batch's bytes belong to pad.txt"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_is_an_error_when_nothing_was_read() {
    let workspace_root = read_workspace("all-failed");
    let req = tool_call_request("read", json!({ "paths": ["a.txt", "b.txt"] }));
    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|result| result.get("isError")),
        Some(&json!(true)),
        "a batch where nothing was read is a failed call"
    );
    let content = content_text(&response);
    assert!(
        content.contains("a.txt"),
        "missing first failed path: {content}"
    );
    assert!(
        content.contains("b.txt"),
        "missing second failed path: {content}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_rejects_argument_shapes_the_schema_forbids() {
    let workspace_root = read_workspace("bad-args");
    for (label, args) in [
        ("not an array", json!({ "paths": "a.txt" })),
        ("not strings", json!({ "paths": [1] })),
        ("empty string", json!({ "paths": [""] })),
        ("empty array", json!({ "paths": [] })),
        (
            "too many",
            json!({ "paths": vec!["a.txt"; workspace_tools::MAX_READ_BATCH_FILES + 1] }),
        ),
    ] {
        let req = tool_call_request("read", args);
        let response = handle_tools_call(
            &req,
            &workspace_root.to_string_lossy(),
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &CommandJobManager::new(),
            &None,
        )
        .await;
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError")),
            Some(&json!(true)),
            "{label} should be rejected"
        );
    }

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn read_tool_schema_requires_a_non_empty_paths_array() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tools-list")),
        method: "tools/list".into(),
        params: json!({}),
    };
    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let schema = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("read"))
        .and_then(|tool| tool.get("inputSchema"))
        .expect("missing read schema")
        .clone();

    assert_eq!(schema["required"], json!(["paths"]));
    assert!(
        schema["properties"].get("path").is_none(),
        "path was removed"
    );
    assert_eq!(schema["properties"]["paths"]["minItems"], json!(1));
    assert_eq!(
        schema["properties"]["paths"]["items"]["minLength"],
        json!(1)
    );
}

#[tokio::test]
async fn delete_tool_returns_structured_message_without_text_content() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-delete-file-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("notes.txt"), "hello world\n").expect("write file");

    let req = tool_call_request("delete", json!({ "path": "notes.txt" }));
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;

    assert_no_text_content(&response);
    let structured = response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing structured content");
    assert_eq!(
        structured.get("message").and_then(Value::as_str),
        Some("deleted file: notes.txt")
    );
    let widget_payload = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("toolName").and_then(Value::as_str),
        Some("delete")
    );
    assert_eq!(
        widget_payload.get("path").and_then(Value::as_str),
        Some("notes.txt")
    );
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        widget_payload
            .get("changedFiles")
            .and_then(Value::as_array)
            .and_then(|files| files.first())
            .and_then(|file| file.get("status"))
            .and_then(Value::as_str),
        Some("deleted")
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn run_command_keeps_full_model_output_but_bounds_widget_preview() {
    let req = tool_call_request("run_command", json!({ "command": "verbose-test" }));
    let full_output = "x".repeat(20_000);
    let expected_output = full_output.clone();
    let raw = json!({
        "content": [],
        "structuredContent": {
            "toolName": "run_command",
            "command": "verbose-test",
            "cwd": "/tmp",
            "stdout": full_output,
            "stderr": "",
            "success": true,
            "exitCode": 0,
            "elapsedMs": 1,
            "timedOut": false,
            "stdoutTruncated": false,
            "stderrTruncated": false
        }
    });

    let result = enrich_tool_result(&req, raw, None);
    let structured = result
        .get("structuredContent")
        .expect("missing structured content");
    let widget_payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        structured.get("stdout").and_then(Value::as_str),
        Some(expected_output.as_str()),
        "model-visible command output must remain complete"
    );
    let preview = widget_payload
        .get("output")
        .and_then(Value::as_str)
        .expect("missing widget output preview");
    assert!(
        preview.chars().count() <= 4_000,
        "widget command preview was {} chars",
        preview.chars().count()
    );
}

#[test]
fn changed_file_widget_keeps_metadata_but_bounds_diff_preview() {
    let req = tool_call_request("write", json!({ "path": "src/example.rs" }));
    let raw = json!({
        "content": [],
        "structuredContent": {
            "toolName": "write",
            "path": "src/example.rs",
            "bytesWritten": 12,
            "success": true
        }
    });
    let widget_context = AutoWidgetContext {
        is_error: false,
        turn_files: vec![FileChange {
            path: "src/example.rs".into(),
            status: "modified".into(),
            added: 1,
            removed: 1,
            diff: "d".repeat(10_000),
        }],
    };

    let result = enrich_tool_result(&req, raw, Some(&widget_context));
    let widget_payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    let file = widget_payload
        .get("changedFiles")
        .and_then(Value::as_array)
        .and_then(|files| files.first())
        .expect("missing changed file");

    assert_eq!(
        file.get("path").and_then(Value::as_str),
        Some("src/example.rs")
    );
    assert_eq!(file.get("status").and_then(Value::as_str), Some("modified"));
    assert_eq!(file.get("added").and_then(Value::as_u64), Some(1));
    assert_eq!(file.get("removed").and_then(Value::as_u64), Some(1));
    let diff = file
        .get("diff")
        .and_then(Value::as_str)
        .expect("missing diff preview");
    assert!(
        diff.chars().count() <= 1_500,
        "widget diff preview was {} chars",
        diff.chars().count()
    );
}

#[test]
fn list_files_widget_keeps_full_counts_but_bounds_rendered_entries() {
    let entries = (0..250)
        .map(|index| {
            json!({
                "path": format!("file-{index}.txt"),
                "name": format!("file-{index}.txt"),
                "kind": "file",
                "depth": 0
            })
        })
        .collect::<Vec<_>>();
    let structured_value = json!({
        "toolName": "run_command",
        "interceptedToolName": "list_files",
        "interceptedCommandName": "find",
        "listPath": ".",
        "listItemCount": 250,
        "listDirectoryCount": 0,
        "listFileCount": 250,
        "listOtherCount": 0,
        "listTruncated": false,
        "listLimit": 500,
        "listEntries": entries
    });
    let structured = structured_value
        .as_object()
        .expect("structured listing must be an object");

    let payload = build_list_files_widget_payload_from_structured(structured, "List Files", "done")
        .expect("list widget payload");

    assert_eq!(
        structured
            .get("listEntries")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(250),
        "model-visible listing must remain complete"
    );
    assert_eq!(
        payload.get("listItemCount").and_then(Value::as_u64),
        Some(250)
    );
    let rendered = payload
        .get("listEntries")
        .and_then(Value::as_array)
        .expect("missing widget list entries");
    assert!(
        rendered.len() <= 100,
        "widget rendered {} listing rows",
        rendered.len()
    );
}

#[test]
fn read_file_separates_model_payload_from_widget_payload() {
    let req = tool_call_request("read", json!({ "paths": ["README.md"] }));
    let raw = json!({
        "structuredContent": {
            "toolName": "read",
            "path": "README.md",
            "bytes": 11,
            "sizeBytes": 99,
            "lineCount": 1,
            "fileCount": 1,
            "batchTruncated": false,
            "files": [{
                "path": "README.md",
                "bytes": 11,
                "sizeBytes": 11,
                "lineCount": 1,
                "text": "hello world",
                "truncated": false,
                "budgetTruncated": false
            }]
        },
        "content": [{
            "type": "text",
            "text": "path: README.md
bytes: 11

hello world"
        }]
    });

    let result = enrich_tool_result(&req, raw, None);
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .expect("missing content array");
    assert!(content.is_empty());
    let structured = result
        .get("structuredContent")
        .expect("missing structuredContent");
    let widget_payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("read")
    );
    assert_eq!(
        structured.get("path").and_then(Value::as_str),
        Some("README.md")
    );
    assert_eq!(structured.get("bytes").and_then(Value::as_u64), Some(11));
    assert_eq!(
        structured.get("sizeBytes").and_then(Value::as_u64),
        Some(99)
    );
    assert_eq!(structured.get("lineCount").and_then(Value::as_u64), Some(1));
    assert_eq!(structured["files"][0]["text"], json!("hello world"));
    assert_eq!(
        structured.get("batchTruncated").and_then(Value::as_bool),
        Some(false)
    );
    assert!(structured.get("schema").is_none());
    assert!(structured.get("panelMode").is_none());
    assert!(structured.get("title").is_none());
    assert!(structured.get("state").is_none());
    assert!(structured.get("changedFiles").is_none());
    assert!(structured.get("hasChanges").is_none());
    assert_eq!(
        widget_payload.get("title").and_then(Value::as_str),
        Some("Read Files")
    );
    assert_eq!(
        widget_payload.get("panelMode").and_then(Value::as_str),
        Some("tool_call")
    );
    assert_eq!(
        widget_payload.get("path").and_then(Value::as_str),
        Some("README.md")
    );
    assert_eq!(
        widget_payload.get("bytes").and_then(Value::as_u64),
        Some(11)
    );
    assert_eq!(
        widget_payload.get("lineCount").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        widget_payload
            .get("renderedFileCount")
            .and_then(Value::as_u64),
        Some(1)
    );
    // The widget payload must not reuse a structured key with a different
    // meaning; renaming these two was how that stopped happening.
    assert!(widget_payload.get("sizeBytes").is_none());
    assert!(widget_payload.get("fileCount").is_none());
    assert!(widget_payload.get("text").is_none());
    assert!(widget_payload.get("files").is_none());
}

#[test]
fn read_file_missing_path_emits_widget_payload_error_panel() {
    let req = tool_call_request(
        "read",
        json!({
            "path": "README.md",
        }),
    );
    let raw = json!({
        "structuredContent": {
            "toolName": "read",
            "bytes": 11,
            "sizeBytes": 11,
            "lineCount": 1,
            "text": "hello world",
            "truncated": false
        },
        "content": [{
            "type": "text",
            "text": "path: README.md\nbytes: 11"
        }]
    });

    let result = enrich_tool_result(&req, raw, None);
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .expect("missing content array");
    assert!(content.is_empty());
    let widget_payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert_eq!(
        widget_payload.get("payloadKind").and_then(Value::as_str),
        Some("widget_payload_error")
    );
    assert_eq!(
        widget_payload.get("title").and_then(Value::as_str),
        Some("Widget Payload Error")
    );
    assert_eq!(
        widget_payload.get("state").and_then(Value::as_str),
        Some("failed")
    );
    assert_eq!(
        widget_payload.get("call").and_then(Value::as_str),
        Some("call read")
    );
    assert_eq!(
        widget_payload.get("detail").and_then(Value::as_str),
        Some("Failed to build read widget payload from structuredContent.")
    );
}

#[test]
fn widget_resource_uri_includes_revision_for_cache_busting() {
    let uri = current_widget_resource_uri_for_tool("catdesk_instruction");
    assert!(uri.contains("widgetRevision=6"));
    assert!(uri.contains("toolName=catdesk_instruction"));
}

#[test]
fn widget_resources_follow_show_detail_mode() {
    for mode in [ShowDetailMode::Expanded, ShowDetailMode::Collapsed] {
        let list_response = handle_resources_list_with_show_detail_mode(
            &resources_list_request(),
            Some("https://example.ngrok.app"),
            mode,
        );
        assert_eq!(
            list_response
                .result
                .as_ref()
                .and_then(|result| result.get("resources"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        let read_response = handle_resources_read_with_show_detail_mode(
            &resources_read_request(UI_TEMPLATE_URI),
            Some("https://example.ngrok.app"),
            1,
            mode,
        );
        assert!(read_response.error.is_none());
        assert!(read_response.result.is_some());
    }

    let list_response = handle_resources_list_with_show_detail_mode(
        &resources_list_request(),
        Some("https://example.ngrok.app"),
        ShowDetailMode::Disable,
    );
    assert_eq!(
        list_response
            .result
            .as_ref()
            .and_then(|result| result.get("resources"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );

    let read_response = handle_resources_read_with_show_detail_mode(
        &resources_read_request(UI_TEMPLATE_URI),
        Some("https://example.ngrok.app"),
        1,
        ShowDetailMode::Disable,
    );
    assert!(read_response.result.is_none());
    assert_eq!(
        read_response.error.as_ref().map(|error| error.code),
        Some(-32602)
    );
    assert!(
        read_response
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("Unknown resource"))
    );
}

#[test]
fn resources_read_includes_widget_csp_connect_domains() {
    let resource_resp = handle_resources_read(
        &resources_read_request(UI_TEMPLATE_URI),
        Some("https://example.ngrok.app"),
        1,
    );
    let ui_meta = resource_resp
        .result
        .as_ref()
        .and_then(|result| result.get("contents"))
        .and_then(Value::as_array)
        .and_then(|contents| contents.first())
        .and_then(|entry| entry.get("_meta"))
        .and_then(|meta| meta.get("ui"))
        .expect("missing widget ui meta");
    let text = resource_resp
        .result
        .as_ref()
        .and_then(|result| result.get("contents"))
        .and_then(Value::as_array)
        .and_then(|contents| contents.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
        .expect("missing widget html");

    assert_eq!(
        ui_meta.get("prefersBorder").and_then(Value::as_bool),
        Some(false)
    );
    assert!(text.contains("var INITIAL_TOKEN_STATS_LAYOUT ="));
    assert!(!text.contains(INITIAL_TOKEN_STATS_LAYOUT_PLACEHOLDER));
    assert!(text.contains("var INITIAL_TOOL_NAME = \"\";"));
    assert!(!text.contains(INITIAL_TOOL_NAME_PLACEHOLDER));
    assert!(text.contains("var INITIAL_MASCOT_OUTLINE = {"));
    assert!(!text.contains(INITIAL_MASCOT_OUTLINE_PLACEHOLDER));
    assert!(text.contains("Disable CatDesk widget?"));
    assert!(text.contains("Widget disabled"));
    assert!(text.contains("https://chatgpt.com/#settings/Plugins"));
    assert!(text.contains("data:image/png;base64,"));
    assert!(!text.contains(REENABLE_WIDGET_IMAGE_PLACEHOLDER));
    assert!(!text.contains(REFRESH_CATDESK_IMAGE_PLACEHOLDER));
    assert!(!text.contains(REMOVE_CATDESK_IMAGE_PLACEHOLDER));
    assert_eq!(
        ui_meta
            .get("csp")
            .and_then(|csp| csp.get("connectDomains"))
            .and_then(Value::as_array)
            .and_then(|domains| domains.first())
            .and_then(Value::as_str),
        Some("https://example.ngrok.app")
    );
    assert_eq!(
        ui_meta
            .get("csp")
            .and_then(|csp| csp.get("resourceDomains"))
            .and_then(Value::as_array)
            .map(|domains| domains.len()),
        Some(0)
    );
}

#[test]
fn token_usage_sanitizer_drops_native_image_base64() {
    let result = json!({
        "content": [{
            "type": "image",
            "data": "aGVsbG8=".repeat(10_000),
            "mimeType": "image/png",
        }],
        "structuredContent": {
            "toolName": "read_image",
            "mimeType": "image/png",
        },
        "_meta": {
            WIDGET_PAYLOAD_META_KEY: {
                "toolName": "read_image"
            }
        }
    });

    let sanitized = sanitize_result_for_turn_token_count(&result);
    let image = sanitized
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .expect("missing image content");
    assert_eq!(image.get("type").and_then(Value::as_str), Some("image"));
    assert_eq!(
        image.get("mimeType").and_then(Value::as_str),
        Some("image/png")
    );
    assert!(image.get("data").is_none());
    assert!(sanitized.get("_meta").is_none());

    let original_data = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|image| image.get("data"))
        .and_then(Value::as_str)
        .expect("original image data missing");
    assert!(
        !original_data.is_empty(),
        "sanitizer must not mutate the source"
    );
}

#[test]
fn attach_current_usage_updates_widget_payload_meta() {
    let mut result = json!({
        "structuredContent": {
            "toolName": "read"
        },
        "_meta": {
            WIDGET_PAYLOAD_META_KEY: {
                "schema": "catdesk.review.v1",
                "toolName": "read"
            }
        }
    });

    let usage = TokenUsage::from_counts(123, 45);
    attach_turn_token_usage(&mut result, &usage);
    attach_tool_call_count(&mut result, 1);

    let structured = result
        .get("structuredContent")
        .expect("missing structuredContent");
    let widget_payload = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");

    assert!(structured.get("turnTokenUsage").is_none());
    assert!(structured.get("toolCallCount").is_none());
    assert_eq!(
        widget_payload
            .get("turnTokenUsage")
            .and_then(|entry| entry.get("totalTokens"))
            .and_then(Value::as_u64),
        Some(168)
    );
    // Direction markers must ride along so clients never confuse the tool
    // input/output axis with the request/response axis.
    assert_eq!(
        widget_payload
            .get("turnTokenUsage")
            .and_then(|entry| entry.get("inputRole"))
            .and_then(Value::as_str),
        Some("request")
    );
    assert_eq!(
        widget_payload
            .get("turnTokenUsage")
            .and_then(|entry| entry.get("outputRole"))
            .and_then(Value::as_str),
        Some("response")
    );
    assert_eq!(
        widget_payload.get("toolCallCount").and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn usage_attachment_does_not_create_widget_payload() {
    let mut result = json!({
        "structuredContent": { "toolName": "read" },
        "_meta": { "unrelated": true }
    });
    let usage = TokenUsage::from_counts(123, 45);

    attach_turn_token_usage(&mut result, &usage);
    attach_tool_call_count(&mut result, 1);

    let meta = result
        .get("_meta")
        .and_then(Value::as_object)
        .expect("missing meta");
    assert_eq!(meta.get("unrelated").and_then(Value::as_bool), Some(true));
    assert!(meta.get(WIDGET_PAYLOAD_META_KEY).is_none());
}

#[test]
fn catdesk_instruction_puts_binagotchy_cards_in_meta_only() {
    let structured =
        catdesk_instruction_structured("/tmp/workspace", Mode::Both, ToolMode::MultiTools)
            .expect("structured payload");
    let widget_payload = catdesk_instruction_widget_payload_with_cards(
        "/tmp/workspace",
        1,
        Mode::Both,
        ToolMode::MultiTools,
        vec![mascot::ArchivedBinagotchyCard {
            folder: "20260403T010203000Z_deadbeef".to_string(),
            seed: "deadbeef".to_string(),
            image: "data:image/png;base64,AA==".to_string(),
        }],
    )
    .expect("widget payload");

    assert_eq!(
        structured.get("toolName").and_then(Value::as_str),
        Some("catdesk_instruction")
    );
    assert!(
        structured
            .get("instructionText")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(structured.get("workspacePath").is_none());
    assert!(structured.get("agentsPath").is_none());
    assert!(structured.get("configPath").is_none());
    assert!(structured.get("binagotchyPath").is_none());
    assert!(structured.get("binagotchyCards").is_none());
    assert!(widget_payload.get("instructionText").is_none());
    assert_eq!(
        widget_payload.get("title").and_then(Value::as_str),
        Some("CatDesk Instruction")
    );
    assert_eq!(
        widget_payload.get("workspacePath").and_then(Value::as_str),
        Some("/tmp/workspace")
    );
    assert_eq!(
        widget_payload
            .get("workspacePathDisplay")
            .and_then(Value::as_str),
        Some("/tmp/workspace")
    );
    assert!(widget_payload.get("agentsPathMode").is_some());
    assert!(widget_payload.get("tokenStatsLayout").is_some());
    assert!(widget_payload.get("widgetCornerStyle").is_some());
    assert!(widget_payload.get("showDetailMode").is_none());
    assert_eq!(
        widget_payload
            .get("tokenStatsLayoutUrl")
            .and_then(Value::as_str),
        Some("")
    );
    assert_eq!(
        widget_payload
            .get("showDetailModeUrl")
            .and_then(Value::as_str),
        Some("")
    );
    assert!(widget_payload.get("agentsWorkspacePath").is_some());
    assert!(widget_payload.get("agentsCatdeskPath").is_some());
    assert!(widget_payload.get("agentsCodexPath").is_some());
    assert_eq!(
        widget_payload
            .get("binagotchyCards")
            .and_then(Value::as_array)
            .map(|cards| cards.len()),
        Some(1)
    );
    assert_eq!(
        widget_payload
            .get("binagotchyCards")
            .and_then(Value::as_array)
            .and_then(|cards| cards.first())
            .and_then(|card| card.get("seed"))
            .and_then(Value::as_str),
        Some("deadbeef")
    );
    assert!(widget_payload.get("widgetMascot").is_some());
}

#[test]
fn show_detail_modes_are_injectable_for_widget_enrichment() {
    let req = tool_call_request("unknown_tool", json!({}));
    let raw = json!({
        "content": [{ "type": "text", "text": "hello" }],
        "structuredContent": { "toolName": "unknown_tool" }
    });

    let disabled =
        enrich_tool_result_with_show_detail_mode(&req, raw.clone(), None, ShowDetailMode::Disable);
    assert_eq!(
        disabled, raw,
        "Disable must leave the tool result untouched"
    );

    for (mode, expected) in [
        (ShowDetailMode::Expanded, "expanded"),
        (ShowDetailMode::Collapsed, "collapsed"),
    ] {
        let result = enrich_tool_result_with_show_detail_mode(&req, raw.clone(), None, mode);
        let payload = result
            .get("_meta")
            .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
            .expect("missing injected widget payload");
        assert_eq!(
            payload.get("showDetailMode").and_then(Value::as_str),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn run_command_change_tracking_excludes_vcs_admin_paths() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-vcs-diff-{}", Uuid::new_v4()));
    let project = workspace_root.join("repo");
    std::fs::create_dir_all(project.join(".git")).expect("create git metadata");
    std::fs::write(project.join(".git/index"), "before\n").expect("write git index");
    std::fs::write(project.join("visible.txt"), "before\n").expect("write visible file");
    let command = if cfg!(windows) {
        "Set-Content -Path .git/index -Value after; Set-Content -Path visible.txt -Value after"
    } else {
        "printf 'after\\n' > .git/index; printf 'after\\n' > visible.txt"
    };
    let req = tool_call_request(
        "run_command",
        json!({ "command": command, "cwd": project.to_string_lossy() }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let response = handle_tools_call(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
    )
    .await;
    let changed_files = response
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .and_then(|payload| payload.get("changedFiles"))
        .and_then(Value::as_array)
        .expect("missing changed files");
    let paths = changed_files
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(paths.contains(&"repo/visible.txt"));
    assert!(paths.iter().all(|path| !path.starts_with("repo/.git/")));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn background_command_reports_cumulative_changes_without_vcs_admin_noise() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-job-diff-{}", Uuid::new_v4()));
    let project = workspace_root.join("repo");
    std::fs::create_dir_all(project.join(".git")).expect("create git metadata");
    std::fs::write(project.join(".git/index"), "before\n").expect("write git index");
    std::fs::write(project.join("visible.txt"), "before\n").expect("write visible file");
    let command_jobs = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Set-Content -Path visible.txt -Value after; Set-Content -Path .git/index -Value after; Start-Sleep -Milliseconds 100"
    } else {
        "printf 'after\\n' > visible.txt; printf 'after\\n' > .git/index; sleep 0.1"
    };
    let start_req = tool_call_request(
        "start_command",
        json!({ "command": command, "cwd": project.to_string_lossy() }),
    );
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let start_response = handle_tools_call(
        &start_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
    )
    .await;
    let job_id = start_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("jobId"))
        .and_then(Value::as_str)
        .expect("missing job id")
        .to_string();

    let mut terminal = None;
    let mut cursor = 0u64;
    for _ in 0..20 {
        let poll_req = tool_call_request(
            "poll_command",
            json!({ "job_id": job_id, "after": cursor, "wait_ms": 250 }),
        );
        let response = handle_tools_call(
            &poll_req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &None,
        )
        .await;
        let structured = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .expect("missing poll structured content");
        cursor = structured
            .get("nextCursor")
            .and_then(Value::as_u64)
            .unwrap_or(cursor);
        if structured.get("state").and_then(Value::as_str) == Some("succeeded")
            && structured.get("hasMoreOutput").and_then(Value::as_bool) != Some(true)
        {
            terminal = Some(response);
            break;
        }
    }
    let terminal = terminal.expect("background command did not finish");
    let widget_payload = terminal
        .result
        .as_ref()
        .and_then(|result| result.get("_meta"))
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("missing widget payload");
    assert_eq!(
        widget_payload.get("hasChanges").and_then(Value::as_bool),
        Some(true)
    );
    let paths = widget_payload
        .get("changedFiles")
        .and_then(Value::as_array)
        .expect("missing changed files")
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(paths.contains(&"repo/visible.txt"));
    assert!(paths.iter().all(|path| !path.starts_with("repo/.git/")));
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn disabled_show_detail_mode_skips_background_change_tracking() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-disable-job-diff-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("visible.txt"), "before\n").expect("write visible file");
    let command_jobs = CommandJobManager::new();
    let command = if cfg!(windows) {
        "Set-Content -Path visible.txt -Value after; Start-Sleep -Milliseconds 100"
    } else {
        "printf 'after\\n' > visible.txt; sleep 0.1"
    };
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let start_req = tool_call_request("start_command", json!({ "command": command }));
    let start_response = handle_tools_call_with_show_detail_mode(
        &start_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &command_jobs,
        &None,
        ShowDetailMode::Disable,
    )
    .await;
    let job_id = start_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("jobId"))
        .and_then(Value::as_str)
        .expect("missing job id")
        .to_string();

    let mut cursor = 0u64;
    let mut completed = false;
    for _ in 0..20 {
        let poll_req = tool_call_request(
            "poll_command",
            json!({ "job_id": job_id, "after": cursor, "wait_ms": 250 }),
        );
        let response = handle_tools_call_with_show_detail_mode(
            &poll_req,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &None,
            ShowDetailMode::Disable,
        )
        .await;
        assert!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("_meta"))
                .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
                .is_none()
        );
        let structured = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .expect("missing poll structured content");
        cursor = structured
            .get("nextCursor")
            .and_then(Value::as_u64)
            .unwrap_or(cursor);
        if structured.get("state").and_then(Value::as_str) == Some("succeeded")
            && structured.get("hasMoreOutput").and_then(Value::as_bool) != Some(true)
        {
            completed = true;
            break;
        }
    }
    assert!(completed, "background command did not finish");
    assert!(
        std::fs::read_to_string(workspace_root.join("visible.txt"))
            .expect("read visible file")
            .contains("after")
    );
    assert!(
        command_jobs
            .current_changes(&job_id)
            .await
            .expect("read job changes")
            .is_empty(),
        "Disable must not retain a change session for background commands"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn base_widget_payload_serializes_all_show_detail_modes() {
    for (mode, expected) in [
        (ShowDetailMode::Expanded, "expanded"),
        (ShowDetailMode::Collapsed, "collapsed"),
        (ShowDetailMode::Disable, "disable"),
    ] {
        let payload = base_widget_payload_with_show_detail_mode(
            "tool_call",
            "Test",
            "done",
            Some("read"),
            mode,
        );
        assert_eq!(
            payload.get("showDetailMode").and_then(Value::as_str),
            Some(expected)
        );
    }
}

fn mcp_result_store() -> LargeResultStore {
    let config = LargeResultStoreConfig {
        max_range_bytes: 16,
        max_search_matches: 4,
        search_chunk_bytes: 8,
        ..LargeResultStoreConfig::default()
    };
    LargeResultStore::new(config).expect("create result store")
}

#[tokio::test]
async fn result_retrieval_tool_schemas_are_read_only_and_bounded() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-result-tools")),
        method: "tools/list".into(),
        params: json!({}),
    };
    let response = handle_tools_list(&req, Mode::Both, ToolMode::MultiTools, &None).await;
    let tools = response
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools");

    let read = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("read_result"))
        .expect("missing read_result");
    assert_eq!(
        read.get("annotations")
            .and_then(|value| value.get("readOnlyHint"))
            .and_then(Value::as_bool),
        Some(true)
    );
    let read_schema = read
        .get("inputSchema")
        .and_then(Value::as_object)
        .expect("missing read_result input schema");
    assert_eq!(
        read_schema
            .get("required")
            .and_then(Value::as_array)
            .expect("missing required fields"),
        &vec![json!("result_id")]
    );
    assert!(
        read_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("missing read properties")
            .contains_key("max_bytes")
    );

    let search = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("search_result"))
        .expect("missing search_result");
    assert_eq!(
        search
            .get("annotations")
            .and_then(|value| value.get("readOnlyHint"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        search
            .get("inputSchema")
            .and_then(|schema| schema.get("required"))
            .and_then(Value::as_array)
            .expect("missing search required"),
        &vec![json!("result_id"), json!("query")]
    );
}

#[tokio::test]
async fn result_retrieval_dispatches_lossless_ranges_and_paginated_search() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-result-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = mcp_result_store();
    let payload = b"alpha needle beta needle omega";
    let stored = store
        .put(
            Some("session-a"),
            &workspace_root,
            payload,
            Some("text/plain"),
        )
        .expect("store result");

    let read_req = tool_call_request(
        "read_result",
        json!({
            "result_id": stored.metadata.result_id,
            "offset": 6,
            "max_bytes": 12
        }),
    );
    let read_response = handle_tools_call_with_result_store(
        &read_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;
    let read_structured = read_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing read structured content");
    assert_eq!(
        read_structured.get("toolName").and_then(Value::as_str),
        Some("read_result")
    );
    assert_eq!(
        read_structured.get("bytesReturned").and_then(Value::as_u64),
        Some(12)
    );
    assert_eq!(
        read_structured.get("nextOffset").and_then(Value::as_u64),
        Some(18)
    );
    let encoded = read_structured
        .get("dataBase64")
        .and_then(Value::as_str)
        .expect("missing lossless data");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode range"),
        payload[6..18]
    );

    let search_req = tool_call_request(
        "search_result",
        json!({
            "result_id": stored.metadata.result_id,
            "query": "needle",
            "max_matches": 1
        }),
    );
    let first = handle_tools_call_with_result_store(
        &search_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;
    let first_structured = first
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing search structured content");
    assert_eq!(
        first_structured.get("matchCount").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        first_structured
            .get("matches")
            .and_then(Value::as_array)
            .and_then(|matches| matches.first())
            .and_then(|entry| entry.get("offset"))
            .and_then(Value::as_u64),
        Some(6)
    );
    assert_eq!(
        first_structured.get("eof").and_then(Value::as_bool),
        Some(false)
    );

    let next_offset = first_structured
        .get("nextOffset")
        .and_then(Value::as_u64)
        .expect("missing next offset");
    let second_req = tool_call_request(
        "search_result",
        json!({
            "result_id": stored.metadata.result_id,
            "query": "needle",
            "start_offset": next_offset,
            "max_matches": 4
        }),
    );
    let second = handle_search_result(&second_req, &workspace_root_str, &store, Some("session-a"));
    let second_structured = second
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing second search structured content");
    assert_eq!(
        second_structured
            .get("matches")
            .and_then(Value::as_array)
            .and_then(|matches| matches.first())
            .and_then(|entry| entry.get("offset"))
            .and_then(Value::as_u64),
        Some(18)
    );
    assert_eq!(
        second_structured.get("eof").and_then(Value::as_bool),
        Some(true)
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[test]
fn result_retrieval_hides_foreign_refs_and_reports_bounded_errors() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-result-scope-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = mcp_result_store();
    let stored = store
        .put(Some("session-a"), &workspace_root, b"private text", None)
        .expect("store result");

    let foreign_req = tool_call_request(
        "read_result",
        json!({ "result_id": stored.metadata.result_id, "max_bytes": 4 }),
    );
    let foreign = handle_read_result(&foreign_req, &workspace_root_str, &store, Some("session-b"));
    let foreign_structured = foreign
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing foreign structured content");
    assert_eq!(
        foreign_structured.get("success").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        foreign_structured.get("errorCode").and_then(Value::as_str),
        Some("unavailable")
    );

    let oversized_req = tool_call_request(
        "read_result",
        json!({ "result_id": stored.metadata.result_id, "max_bytes": 17 }),
    );
    let oversized = handle_read_result(
        &oversized_req,
        &workspace_root_str,
        &store,
        Some("session-a"),
    );
    let oversized_structured = oversized
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing oversized structured content");
    assert_eq!(
        oversized_structured
            .get("errorCode")
            .and_then(Value::as_str),
        Some("range_too_large")
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[tokio::test]
async fn shared_tools_call_boundary_externalizes_oversized_result_losslessly() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let full_text = format!("HEAD\n{}\nTAIL", "ż中🙂".repeat(30_000));
    std::fs::write(workspace_root.join("big.txt"), &full_text).expect("write file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    let req = tool_call_request("read", json!({ "paths": ["big.txt"] }));

    let response = handle_tools_call_with_result_store(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;

    let inline = response.result.as_ref().expect("missing result");
    assert!(
        serde_json::to_vec(inline).unwrap().len()
            <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES
    );
    let output_ref = inline
        .pointer("/responseBudget/outputRef")
        .and_then(Value::as_str)
        .expect("missing outputRef");
    assert_eq!(
        inline
            .pointer("/responseBudget/retrieval/tool")
            .and_then(Value::as_str),
        Some("read_result")
    );

    let mut rebuilt = Vec::new();
    let mut offset = 0_u64;
    loop {
        let range = store
            .read_range(
                Some("session-a"),
                &workspace_root,
                output_ref,
                offset,
                store.max_range_bytes(),
            )
            .expect("read stored result");
        rebuilt.extend_from_slice(&range.bytes);
        offset = range.next_offset;
        if range.eof {
            break;
        }
    }
    let original_result: Value = serde_json::from_slice(&rebuilt).expect("stored result json");
    assert_eq!(
        original_result
            .pointer("/structuredContent/files/0/text")
            .and_then(Value::as_str),
        Some(full_text.as_str())
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[tokio::test]
async fn byte_accounting_reports_externalized_results_from_the_budget_policy() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-accounting-external-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(
        workspace_root.join("big.txt"),
        format!("HEAD\n{}\nTAIL", "ż中🙂".repeat(30_000)),
    )
    .expect("write file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    let before = tool_result_metrics::snapshot();
    let req = tool_call_request("read", json!({ "paths": ["big.txt"] }));

    let response = handle_tools_call_with_result_store(
        &req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;

    let inline = response.result.as_ref().expect("missing result");
    let expected_raw = inline
        .pointer("/responseBudget/originalBytes")
        .and_then(Value::as_u64)
        .expect("externalized result must carry the budget manifest");
    let my_inline = serde_json::to_vec(inline).unwrap().len() as u64;

    // The registry is process-global and parallel tests observe their own
    // calls too, so only this call's contribution (>=) is asserted here;
    // exact per-class and per-byte accounting is covered by unit tests.
    let after = tool_result_metrics::snapshot();
    let slot = perf_metrics::tool_index(Some("read"));
    let delta = tool_result_metrics::totals_delta(&before[slot], &after[slot]);
    assert!(delta.count >= 1, "the externalized read must be counted");
    assert!(
        delta.externalized_count >= 1,
        "the externalized read must land in the externalized class"
    );
    assert_eq!(
        delta.raw_bytes >= expected_raw && delta.externalized_bytes >= expected_raw,
        true,
        "raw and externalized totals must include the stored payload"
    );
    assert!(
        delta.inline_bytes >= my_inline,
        "inline totals must include the sent preview"
    );
    assert!(
        delta.raw_bytes >= delta.inline_bytes,
        "raw >= inline must hold per record and therefore under summation"
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[tokio::test]
async fn byte_accounting_records_small_and_failed_retrieval_results() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-accounting-small-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    std::fs::write(workspace_root.join("tiny.txt"), "tiny payload").expect("write file");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    let before = tool_result_metrics::snapshot();

    let small_req = tool_call_request("read", json!({ "paths": ["tiny.txt"] }));
    let small = handle_tools_call_with_result_store(
        &small_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;
    let small_inline = small
        .result
        .as_ref()
        .map(|result| serde_json::to_vec(result).unwrap().len() as u64)
        .unwrap_or(0);
    assert!(
        small
            .result
            .as_ref()
            .is_some_and(|result| result.get("responseBudget").is_none()),
        "a small result must be sent untouched"
    );

    let retrieval_req = tool_call_request(
        "read_result",
        json!({ "result_id": "result_missing-accounting-probe", "max_bytes": 4 }),
    );
    let retrieval = handle_tools_call_with_result_store(
        &retrieval_req,
        &workspace_root_str,
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;
    assert_eq!(
        retrieval
            .result
            .as_ref()
            .and_then(|result| result.get("isError")),
        Some(&json!(true)),
        "an unknown result id must surface a tool error"
    );

    let after = tool_result_metrics::snapshot();
    let read_slot = perf_metrics::tool_index(Some("read"));
    let read_delta = tool_result_metrics::totals_delta(&before[read_slot], &after[read_slot]);
    assert!(read_delta.count >= 1);
    assert!(
        read_delta.small_count >= 1,
        "the untouched small read must land in the small class"
    );
    assert!(
        read_delta.inline_bytes >= small_inline && read_delta.raw_bytes >= small_inline,
        "small responses must account raw == inline == the sent size"
    );
    assert!(
        read_delta.raw_bytes >= read_delta.inline_bytes,
        "raw >= inline must hold per record and therefore under summation"
    );

    // Retrieval success is derivable from the retrieval tools' error counts.
    let retrieval_slot = perf_metrics::tool_index(Some("read_result"));
    let retrieval_delta =
        tool_result_metrics::totals_delta(&before[retrieval_slot], &after[retrieval_slot]);
    assert!(retrieval_delta.count >= 1);
    assert!(
        retrieval_delta.error_count >= 1,
        "the failed retrieval must count toward the read_result error total"
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[tokio::test]
async fn result_retrieval_reconstructs_multi_megabyte_payload_through_bounded_mcp_calls() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-result-rebuild-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    let payload = (0..(3 * 1024 * 1024 + 37))
        .map(|index| ((index * 37) % 251) as u8)
        .collect::<Vec<_>>();
    let stored = store
        .put(Some("session-a"), &workspace_root, &payload, None)
        .expect("store multi-megabyte payload");
    let command_jobs = CommandJobManager::new();
    let devtools = None;

    let mut rebuilt = Vec::with_capacity(payload.len());
    let mut offset = 0_u64;
    loop {
        let request = tool_call_request(
            "read_result",
            json!({
                "result_id": stored.metadata.result_id,
                "offset": offset,
                "max_bytes": crate::result_store::DEFAULT_MAX_RANGE_BYTES
            }),
        );
        let response = handle_tools_call_with_result_store(
            &request,
            &workspace_root_str,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &command_jobs,
            &devtools,
            ShowDetailMode::Disable,
            &store,
            Some("session-a"),
            None,
        )
        .await;
        let structured = response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .expect("missing range structured content");
        let bytes_returned = structured
            .get("bytesReturned")
            .and_then(Value::as_u64)
            .expect("missing bytesReturned") as usize;
        assert!(
            bytes_returned <= crate::result_store::DEFAULT_MAX_RANGE_BYTES,
            "one retrieval returned {bytes_returned} bytes"
        );
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(
                structured
                    .get("dataBase64")
                    .and_then(Value::as_str)
                    .expect("missing lossless data"),
            )
            .expect("decode range");
        assert_eq!(chunk.len(), bytes_returned);
        rebuilt.extend_from_slice(&chunk);
        offset = structured
            .get("nextOffset")
            .and_then(Value::as_u64)
            .expect("missing nextOffset");
        if structured.get("eof").and_then(Value::as_bool) == Some(true) {
            break;
        }
    }

    assert_eq!(offset, payload.len() as u64);
    assert_eq!(rebuilt, payload);
    std::fs::remove_dir_all(workspace_root).ok();
}

// ── read_image vision description cap (catdesk-080, audit finding F1) ───────

fn analyzed_response_for_description(
    workspace_root: &Path,
    analysis: String,
) -> (JsonRpcResponse, workspace_tools::ReadImageOutput) {
    let image_path = workspace_root.join("cap-check.png");
    write_test_image(&image_path, image::ImageFormat::Png, 24, 12);
    let output = workspace_tools::read_image(
        &workspace_root.to_string_lossy(),
        "cap-check.png",
        None,
        None,
    )
    .expect("read test image");
    let config = crate::vision::VisionConfig {
        backend: crate::vision::VisionBackend::Gemini,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
    };
    let req = tool_call_request("read_image", json!({}));
    let response = image_tool_analyzed_response(&req, &output, &config, analysis);
    (response, output)
}

#[test]
fn analyzed_read_image_caps_vision_description_and_keeps_image_bytes() {
    let workspace_root = read_workspace("vision-description-cap");
    let long_analysis = format!("VISION-HEAD{}", "detail ".repeat(40_000));

    let (response, output) = analyzed_response_for_description(&workspace_root, long_analysis);
    let result = response.result.expect("analyzed result");

    // The serialized response stays bounded even though the model emitted a
    // ~280 KB description: only the capped preview travels inline.
    let serialized = serde_json::to_vec(&result).expect("serialize result");
    let cap = crate::mcp::response_budget::ANALYSIS_DESCRIPTION_PREVIEW_BYTES;
    assert!(
        serialized.len() < cap + 8 * 1024,
        "analyzed response must stay near the description cap, got {} bytes",
        serialized.len()
    );

    // Multimodal exemption invariant: the native image content is untouched.
    let content_image = &result["content"][0];
    assert_eq!(content_image["type"], "image");
    let encoded = content_image["data"].as_str().expect("image data");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode image"),
        output.data,
        "image bytes must survive untouched"
    );

    // The description is the deterministic head+tail preview with a
    // self-contained omission note (there is no outputRef on this surface).
    let description = result["structuredContent"]["analysis"]["description"]
        .as_str()
        .expect("capped description");
    assert!(description.starts_with("VISION-HEAD"));
    assert!(description.contains("full text not retained"));
    assert!(
        description.len() <= cap + 256,
        "capped description must not exceed the preview budget, got {} bytes",
        description.len()
    );
    assert_eq!(
        result["structuredContent"]["analysis"]["analysisTruncated"],
        json!(true)
    );

    // Deterministic: the same input produces the same capped description.
    let (replay, _) = analyzed_response_for_description(
        &workspace_root,
        format!("VISION-HEAD{}", "detail ".repeat(40_000)),
    );
    assert_eq!(
        replay.result.expect("replayed result")["structuredContent"]["analysis"]["description"],
        description
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn analyzed_read_image_short_description_passes_through_untouched() {
    let workspace_root = read_workspace("vision-description-short");
    let analysis = "A crisp red rectangle on white.".to_string();

    let (response, _) = analyzed_response_for_description(&workspace_root, analysis.clone());
    let analysis_struct =
        &response.result.expect("analyzed result")["structuredContent"]["analysis"];
    assert_eq!(analysis_struct["description"], json!(analysis));
    assert_eq!(analysis_struct["analysisTruncated"], json!(false));

    let _ = std::fs::remove_dir_all(workspace_root);
}
