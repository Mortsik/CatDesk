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
use crate::devtools::DevtoolsBridge;
use crate::handoff;
use crate::workspace_tools;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use super::agents_state::cached_file_value;
use super::commands::{
    apply_devtools_request_defaults, change_scope_for_request, command_job_output_text,
    forward_to_devtools, handle_poll_command,
};
use super::file_tools::{handle_create_handoff_for_project, image_tool_analyzed_response};
use super::instruction::{
    CATDESK_INSTRUCTION_REQUIRED_CODE, CATDESK_INSTRUCTION_REQUIRED_MESSAGE,
    CATDESK_INSTRUCTION_REQUIRED_WIDGET_MESSAGE,
    catdesk_instruction_required_response_with_show_detail_mode, catdesk_instruction_structured,
    catdesk_instruction_text, catdesk_instruction_text_for_project,
    catdesk_instruction_widget_payload_with_cards,
    handle_catdesk_instruction_with_show_detail_mode,
};
use super::result_tools::{
    MAX_SEARCH_RESULT_QUERY_CHARS, handle_read_result, handle_search_result,
};
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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

/// Unix-only: the foreground probe runs `printf`, which does not exist on the
/// Windows PowerShell dispatch path (`process_runner.rs` shells through
/// powershell.exe), and the exact-stdout assertion assumes unix output.
#[cfg(unix)]
#[tokio::test]
async fn foreground_run_command_works_while_many_background_commands_run() {
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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

/// Unix-only: `printf` does not exist on the Windows PowerShell dispatch path
/// (`process_runner.rs` shells through powershell.exe).
#[cfg(unix)]
#[tokio::test]
async fn run_command_success_keeps_content_empty() {
    let _env = env_lock();
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
async fn read_result_max_range_token_estimate_stays_bounded() {
    let _env = env_lock();
    // Audit F6: read_result is exempt from the response budget, so the turn
    // usage estimate used to o200k-tokenize the FULL range. A max-range read
    // (128 KiB -> ~175 KB of newline-free base64, effectively one giant BPE
    // pre-token) blocked tools/call for tens of seconds per request.
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-read-timing-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new_default().expect("create result store");
    // Over a mebibyte of 'x' so run_command's budget must externalize it.
    let command = concat!(
        "printf 'HEAD\\n'; ",
        "head -c 1048576 /dev/zero | tr '\\0' x; ",
        "printf '\\nTAIL\\n'"
    );
    let run_req = tool_call_request("run_command", json!({ "command": command }));
    let run_response = handle_tools_call_with_result_store(
        &run_req,
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
    let output_ref = run_response
        .result
        .as_ref()
        .and_then(|result| result.get("responseBudget"))
        .and_then(|budget| budget.get("outputRef"))
        .and_then(Value::as_str)
        .expect("externalized outputRef")
        .to_string();

    // The regression probe: a full max-range read of exempt bytes.
    let read_req = tool_call_request(
        "read_result",
        json!({
            "result_id": output_ref,
            "offset": 0,
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
    // Structural replacement for the old wall-clock bound (<2 s), which a
    // healthy run could exceed under -j 8 CPU starvation while the real
    // regression it guarded against measured tens of seconds. The invariant
    // is the estimator PATH, not the time: an exempt retrieval result must
    // be estimated through the pure bytes/4 heuristic over the sanitized
    // payload — the same call the pipeline's turn-usage accounting runs for
    // this response (server.rs falls back to estimate_turn_token_counts).
    // Only the bytewise branch produces this equality; a reintroduced exact
    // o200k encode of the pathological homogeneous base64 range diverges
    // from it deterministically, on an unloaded machine too.
    let read_result_value = read_response.result.as_ref().expect("missing result");
    let usage =
        super::token_usage::estimate_turn_token_usage(&read_req, "read_result", read_result_value);
    let sanitized = sanitize_result_for_turn_token_count(read_result_value);
    let bytewise = serde_json::to_string(&sanitized)
        .expect("serialize sanitized result")
        .len() as u64
        / 4;
    assert_eq!(
        usage.tool_output_tokens, bytewise,
        "an exempt max-range result must be estimated bytewise over the \
         sanitized payload; an exact BPE pass here is the regression"
    );

    // Sanity: the probe measured the real full-range path.
    let range_bytes = read_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("dataBase64"))
        .and_then(Value::as_str)
        .and_then(|encoded| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .map(|bytes| bytes.len())
        .expect("decoded range bytes");
    assert_eq!(
        range_bytes,
        crate::result_store::DEFAULT_MAX_RANGE_BYTES,
        "the probe must read the full max range"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_command_large_stdout_and_stderr_are_compact_inline_and_fully_retrievable() {
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    // Zero-match branch: no handoff is not an error.
    assert!(
        instruction.contains("If none are found, continue normally"),
        "the zero-match branch must keep its explicit continuation: {instruction}"
    );
    // Multi-match branch: ask, then read/verify/delete ONLY the chosen one,
    // still gated on a successful read.
    assert!(instruction.contains("If multiple matching handoffs are found"));
    assert!(
        instruction.contains("ask the user which to use"),
        "the multi-match branch must ask before touching any handoff: {instruction}"
    );
    assert!(
        instruction.contains("delete only that chosen handoff after a successful read"),
        "the multi-match branch must delete only the chosen handoff after a successful read: {instruction}"
    );
    assert!(
        instruction.contains("delete that Library file only after a successful read"),
        "handoff deletion must stay gated on a successful read: {instruction}"
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
        instruction.contains("has no internet connection"),
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
    // Semantic boundary: only the CatDesk-qualified trailer is forbidden.
    // Runtime enforcement (command::contains_catdesk_co_author_marker) is
    // CatDesk-specific, so an unqualified "a `Co-Authored-By` trailer" ban
    // would wrongly prohibit legitimate non-CatDesk co-authors.
    let mentions = instruction.matches("Co-Authored-By").count();
    let qualified = instruction.matches("Co-Authored-By: CatDesk").count();
    assert!(
        qualified >= 1 && mentions == qualified,
        "every Co-Authored-By mention must be CatDesk-qualified (got {qualified} qualified \
         of {mentions} mentions): {instruction}"
    );

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

#[test]
fn oversized_agents_text_layer_is_delivered_as_bounded_head_preview() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-agents-text-cap-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let agents_path = workspace_root.join("AGENTS.md");
    let mut oversized = String::from("HEAD-SENTINEL\n");
    while oversized.len() < 100_000 {
        oversized.push_str("mid-padding-rule\n");
    }
    oversized.push_str("MIDDLE-SENTINEL\n");
    while oversized.len() < 200_000 {
        oversized.push_str("tail-padding-rule\n");
    }
    oversized.push_str("\nTAIL-SENTINEL");
    std::fs::write(&agents_path, &oversized).expect("write agents");

    let preview = super::agents_state::cached_agents_text(&agents_path)
        .expect("a non-empty oversized layer must still deliver a preview");

    assert!(preview.contains("HEAD-SENTINEL"));
    assert!(
        !preview.contains("MIDDLE-SENTINEL"),
        "bytes past the reader cap must never ride the instruction payload"
    );
    assert!(
        !preview.contains("TAIL-SENTINEL"),
        "the file tail is deliberately unread: no synthetic tail may appear"
    );
    assert!(
        preview.contains("full text not retained"),
        "the omission note must be self-contained: this surface has no outputRef: {preview:.200}"
    );
    assert!(
        preview.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "preview must stay on the shared inline-budget scale, got {} bytes",
        preview.len()
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn small_agents_text_layer_is_kept_verbatim_without_a_truncation_note() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-agents-text-small-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let agents_path = workspace_root.join("AGENTS.md");
    std::fs::write(&agents_path, "short-layer-rule\n").expect("write agents");

    let text = super::agents_state::cached_agents_text(&agents_path)
        .expect("non-empty layer must resolve");

    assert_eq!(text, "short-layer-rule");
    assert!(
        !text.contains("full text not retained"),
        "files within the cap must not gain a truncation note"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn invalid_utf8_agents_layer_cannot_expand_past_the_cap() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-agents-text-utf8-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let agents_path = workspace_root.join("AGENTS.md");
    // One lone continuation byte per line: every byte decodes into a 3-byte
    // U+FFFD, so the lossy result is ~3x the raw file size.
    let invalid = vec![0x80_u8; super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES];
    std::fs::write(&agents_path, &invalid).expect("write invalid utf8 agents");

    let preview = super::agents_state::cached_agents_text(&agents_path)
        .expect("a non-empty invalid-utf8 layer must still deliver a preview");

    assert!(
        preview.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "the cap must hold on the FINAL UTF-8 text, got {} bytes from a {}-byte file",
        preview.len(),
        invalid.len()
    );
    assert!(
        preview.contains("full text not retained"),
        "a file whose decoded text exceeds the cap must carry the truncation note"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn invalid_utf8_within_the_cap_stays_lossy_without_a_note() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-agents-text-utf8-small-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let agents_path = workspace_root.join("AGENTS.md");
    let invalid = vec![0x80_u8; 1024];
    std::fs::write(&agents_path, &invalid).expect("write small invalid utf8 agents");

    let text = super::agents_state::cached_agents_text(&agents_path)
        .expect("non-empty layer must resolve");

    assert_eq!(text.len(), 3 * 1024, "lossy decode expands 1 KiB to 3 KiB");
    assert!(
        text.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "sub-cap files stay within the cap"
    );
    assert!(
        !text.contains("full text not retained"),
        "a sub-cap file must not gain a truncation note merely for invalid UTF-8"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn oversized_agents_preview_stays_within_the_cap_for_huge_file_sizes() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-agents-text-huge-size-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let agents_path = workspace_root.join("AGENTS.md");
    let mut content = String::from("HEAD-SENTINEL\n");
    while content.len() < 70 * 1024 {
        content.push_str("rule\n");
    }
    std::fs::write(&agents_path, &content).expect("write oversized agents");
    // A sparse extension reports a ~100 GB metadata length: the note's size
    // digits grow with the reported size, so the reserved note room must
    // already account for the largest possible note.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&agents_path)
        .expect("open agents for sparse extension");
    file.set_len(100_000_000_000)
        .expect("extend file to 100 GB");
    drop(file);

    let preview = super::agents_state::cached_agents_text(&agents_path)
        .expect("a non-empty oversized layer must still deliver a preview");

    assert!(
        preview.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "the cap must hold even with a 12-digit size in the note, got {} bytes",
        preview.len()
    );
    assert!(preview.contains("HEAD-SENTINEL"));
    assert!(preview.contains("full text not retained"));
    assert!(
        preview.contains("100000000000"),
        "the note must name the real size bound it saw: {preview:.200}"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn whitespace_padded_oversized_agents_layer_keeps_a_bounded_preview() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-agents-text-whitespace-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");

    // The read window (cap + 1 bytes) is ALL spaces with real content living
    // past it: emptiness must be classified only for inline-sized files,
    // otherwise this layer would silently vanish.
    let agents_path = workspace_root.join("AGENTS.md");
    let mut padded = String::new();
    while padded.len() < 70 * 1024 {
        padded.push(' ');
    }
    padded.push_str("HIDDEN-GUARANTEE");
    std::fs::write(&agents_path, &padded).expect("write whitespace-padded agents");

    let preview = super::agents_state::cached_agents_text(&agents_path)
        .expect("an oversized layer must not vanish behind whitespace padding");

    assert!(
        preview.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "the whitespace head plus note must stay within the cap, got {} bytes",
        preview.len()
    );
    assert!(
        preview.contains("full text not retained"),
        "the bounded preview must say the full text was not kept"
    );

    // Inline-sized whitespace stays semantically empty, as before.
    let spaces_path = workspace_root.join("spaces-only.md");
    std::fs::write(&spaces_path, " \n\t".repeat(256)).expect("write spaces-only layer");
    assert!(
        super::agents_state::cached_agents_text(&spaces_path).is_none(),
        "an inline-sized whitespace-only layer is still no layer at all"
    );

    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn oversized_agents_layer_never_rides_catdesk_instruction_inline() {
    let workspace_root = std::env::temp_dir().join(format!(
        "catdesk-mcp-agents-instruction-cap-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let mut oversized = String::from("HEAD-SENTINEL\n");
    while oversized.len() < 300_000 {
        oversized.push_str("padding-rule\n");
    }
    oversized.push_str("\nTAIL-SENTINEL");
    std::fs::write(workspace_root.join("AGENTS.md"), &oversized)
        .expect("write oversized agents layer");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    let instruction =
        catdesk_instruction_text(&workspace_root_str, Mode::Both, ToolMode::MultiTools)
            .expect("build instruction");

    assert!(instruction.contains("HEAD-SENTINEL"));
    assert!(
        !instruction.contains("TAIL-SENTINEL"),
        "an arbitrarily large AGENTS.md must not be inlined into the instruction"
    );
    assert!(instruction.contains("full text not retained"));
    assert!(
        instruction.len() < 128 * 1024,
        "instruction must stay near the template plus one bounded layer, got {} bytes",
        instruction.len()
    );

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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    let _env = env_lock();
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
    assert_eq!(
        search
            .get("inputSchema")
            .and_then(|schema| schema.get("properties"))
            .and_then(|properties| properties.get("query"))
            .and_then(|query| query.get("maxLength"))
            .and_then(Value::as_u64),
        Some(MAX_SEARCH_RESULT_QUERY_CHARS as u64)
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
async fn search_result_rejects_oversized_query_and_caps_echo() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-result-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = mcp_result_store();
    let stored = store
        .put(
            Some("session-a"),
            &workspace_root,
            b"alpha beta",
            Some("text/plain"),
        )
        .expect("store result");

    // An oversized query is rejected before the search runs and cannot leak
    // into the response (F3: store-range tools self-bound their responses).
    let oversized = "x".repeat(MAX_SEARCH_RESULT_QUERY_CHARS + 1);
    let oversized_req = tool_call_request(
        "search_result",
        json!({
            "result_id": stored.metadata.result_id,
            "query": oversized
        }),
    );
    let oversized_response = handle_search_result(
        &oversized_req,
        &workspace_root_str,
        &store,
        Some("session-a"),
    );
    let oversized_result = oversized_response.result.as_ref().expect("missing result");
    assert_eq!(
        oversized_result.get("isError").and_then(Value::as_bool),
        Some(true)
    );
    let oversized_structured = oversized_result
        .get("structuredContent")
        .expect("missing oversized structured content");
    assert_eq!(
        oversized_structured
            .get("errorCode")
            .and_then(Value::as_str),
        Some("invalid_arguments")
    );
    assert!(
        !serde_json::to_string(oversized_result)
            .expect("serialize response")
            .contains(&"x".repeat(256)),
        "oversized query leaked into the response"
    );

    // A query exactly at the cap stays legal and echoes back within it.
    let at_cap = "n".repeat(MAX_SEARCH_RESULT_QUERY_CHARS);
    let cap_req = tool_call_request(
        "search_result",
        json!({
            "result_id": stored.metadata.result_id,
            "query": at_cap
        }),
    );
    let cap_response =
        handle_search_result(&cap_req, &workspace_root_str, &store, Some("session-a"));
    let cap_structured = cap_response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .expect("missing capped search structured content");
    assert_eq!(
        cap_structured
            .get("query")
            .and_then(Value::as_str)
            .expect("missing echoed query")
            .chars()
            .count(),
        MAX_SEARCH_RESULT_QUERY_CHARS
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

/// Finding F2: when both output streams are full relative to the store's
/// entry cap, the store rejects the combined result and — before this fix —
/// the swallowed error sent the oversized payload inline. The rejected
/// response must instead be reduced until it fits the cap and externalized,
/// so the inline answer stays bounded no matter how large the raw output is.
#[cfg(unix)]
#[tokio::test]
async fn oversized_run_command_with_both_streams_full_is_reduced_not_sent_inline() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-entry-cap-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    // Entry cap well above the inline budget: the ~180 KiB combined response
    // is rejected as oversized, and one halving of the largest stream lands
    // the reduced result between the inline budget and the cap, so it is
    // externalized rather than passing through inline.
    let store = LargeResultStore::new(LargeResultStoreConfig {
        ttl: std::time::Duration::from_secs(3600),
        max_entry_bytes: 120 * 1024,
        max_total_bytes: 1024 * 1024,
        max_range_bytes: 128 * 1024,
        max_search_matches: 100,
        search_chunk_bytes: 64 * 1024,
        tombstone_limit: 1024,
    })
    .expect("create capped store");
    let req = tool_call_request(
        "run_command",
        json!({
            "command": concat!(
                "printf 'HEAD-OUT-SENTINEL\\n'; ",
                "yes x | head -c 36000 | tr 'x' '\\001'; ",
                "printf '\\nTAIL-OUT-SENTINEL\\n'; ",
                "{ printf 'HEAD-ERR-SENTINEL\\n'; ",
                "yes ERR | head -c 30000; ",
                "printf '\\nTAIL-ERR-SENTINEL\\n'; } 1>&2"
            )
        }),
    );

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
            <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "the inline answer must stay bounded even when the raw output is rejected"
    );
    let output_ref = inline
        .pointer("/responseBudget/outputRef")
        .and_then(Value::as_str)
        .expect("the reduced result must still be externalized")
        .to_string();

    let mut rebuilt = Vec::new();
    let mut offset = 0_u64;
    loop {
        let range = store
            .read_range(
                Some("session-a"),
                &workspace_root,
                &output_ref,
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
    assert!(
        rebuilt.len() as u64 <= store.max_entry_bytes(),
        "the stored (reduced) payload must fit the entry cap, saw {} bytes",
        rebuilt.len()
    );
    let stored: Value = serde_json::from_slice(&rebuilt).expect("stored result json");
    // stdout (six-fold escaping) is the largest payload, so the reducer
    // reaches it first; one halving brings the whole result under the cap and
    // stderr survives intact.
    let stdout = stored
        .pointer("/structuredContent/stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        stdout.contains("HEAD-OUT-SENTINEL") && stdout.contains("TAIL-OUT-SENTINEL"),
        "the reduced stdout must keep both ends"
    );
    assert!(
        stdout.contains("response exceeded the entry cap"),
        "the reduced stdout must carry a quantified truncation marker"
    );
    let stderr = stored
        .pointer("/structuredContent/stderr")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        stderr.contains("HEAD-ERR-SENTINEL") && stderr.contains("TAIL-ERR-SENTINEL"),
        "the smaller stream must keep both ends"
    );
    assert!(
        !stderr.contains("response exceeded the entry cap"),
        "stderr must stay intact once the largest stream alone fits the budget"
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

/// The reducer must converge even when JSON escaping multiplies the payload:
/// control characters expand six-fold, so a raw-under-cap string can still
/// serialize far above the cap.
#[test]
fn entry_cap_reduction_bounds_pathological_escaping_and_keeps_ends() {
    let mut result = json!({
        "content": [],
        "structuredContent": {
            "toolName": "run_command",
            "success": true,
            "exitCode": 0,
            "stdout": format!("HEAD\u{1}{}\u{1}TAIL", "\u{1}".repeat(24_000)),
            "stderr": ""
        }
    });
    let cap = 8 * 1024_u64;

    reduce_result_to_entry_cap(&mut result, cap);

    let serialized = serde_json::to_vec(&result).unwrap();
    assert!(
        serialized.len() as u64 <= cap,
        "the reduced result must serialize under the cap, saw {} bytes",
        serialized.len()
    );
    let stdout = result
        .pointer("/structuredContent/stdout")
        .and_then(Value::as_str)
        .expect("stdout survives reduction");
    assert!(stdout.contains("HEAD"), "the head must survive");
    assert!(stdout.contains("TAIL"), "the tail must survive");
    assert!(stdout.contains("response exceeded the entry cap"));
    assert_eq!(
        result
            .pointer("/structuredContent/exitCode")
            .and_then(Value::as_i64),
        Some(0),
        "non-payload fields must survive untouched"
    );
}

/// A lossy entry-cap reduction must be disclosed inline before anything
/// reads the outputRef: the reduced payload is what gets stored, and it
/// still exceeds the inline budget here, so the preview is compacted again —
/// a marker inside a string could be previewed away, while the manifest is
/// rebuilt after every compaction level and must keep the disclosure.
#[cfg(unix)]
#[tokio::test]
async fn entry_cap_reduction_is_disclosed_in_the_inline_manifest() {
    // Spawn-dependent (sh/yes/head through PATH) and PATH is process-global:
    // hold the crate env lock so an env-rewriting test cannot interleave
    // (the dr6-sweep idiom); a broken spawn shrinks stdout below the entry
    // cap and the disclosure assertions lose their subject.
    let _env = env_lock();
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-entry-cap-disclose-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    // Cap above the inline budget, payload above the cap: the reducer lands
    // between the two, so the externalized payload itself goes through
    // preview compaction — exactly the shape the disclosure must survive.
    let store = LargeResultStore::new(LargeResultStoreConfig {
        ttl: std::time::Duration::from_secs(3600),
        max_entry_bytes: 120 * 1024,
        max_total_bytes: 1024 * 1024,
        max_range_bytes: 128 * 1024,
        max_search_matches: 100,
        search_chunk_bytes: 64 * 1024,
        tombstone_limit: 1024,
    })
    .expect("create capped store");
    let req = tool_call_request(
        "run_command",
        json!({
            "command": concat!(
                "printf 'HEAD-OUT-SENTINEL\\n'; ",
                "yes x | head -c 140000; ",
                "printf '\\nTAIL-OUT-SENTINEL\\n'"
            )
        }),
    );

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
    let serialized = serde_json::to_vec(inline).unwrap();
    assert!(
        serialized.len() <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "the inline answer must stay bounded, saw {} bytes",
        serialized.len()
    );
    let budget = inline
        .pointer("/responseBudget")
        .expect("externalized answer must carry the budget manifest");
    assert_eq!(
        budget.get("entryCapTruncated").and_then(Value::as_bool),
        Some(true),
        "the client must see the loss before using the outputRef"
    );
    let original_bytes = budget
        .get("entryCapOriginalBytes")
        .and_then(Value::as_u64)
        .expect("entryCapOriginalBytes must disclose the pre-reduction size");
    let omitted_bytes = budget
        .get("entryCapOmittedBytes")
        .and_then(Value::as_u64)
        .expect("entryCapOmittedBytes must quantify the dropped bytes");
    assert!(
        original_bytes > store.max_entry_bytes(),
        "the disclosed original size must exceed the cap, saw {original_bytes}"
    );

    let output_ref = budget
        .get("outputRef")
        .and_then(Value::as_str)
        .expect("the reduced result must still be externalized")
        .to_string();
    let mut rebuilt = Vec::new();
    let mut offset = 0_u64;
    loop {
        let range = store
            .read_range(
                Some("session-a"),
                &workspace_root,
                &output_ref,
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
    assert!(
        (rebuilt.len() as u64) <= store.max_entry_bytes(),
        "the stored (reduced) payload must fit the entry cap, saw {} bytes",
        rebuilt.len()
    );
    assert_eq!(
        omitted_bytes,
        original_bytes - rebuilt.len() as u64,
        "the disclosure must equal the difference between the original and the stored payload"
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

/// A result only slightly over the entry cap can halve down below the inline
/// budget: nothing is externalized, so no manifest appears and the
/// disclosure must ride on the inline result itself — all three fields,
/// with original-minus-omitted still inside the inline budget.
#[cfg(unix)]
#[tokio::test]
async fn sub_budget_entry_cap_reduction_discloses_loss_inline_without_a_manifest() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-entry-cap-inline-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    let store = LargeResultStore::new(LargeResultStoreConfig {
        ttl: std::time::Duration::from_secs(3600),
        max_entry_bytes: 120 * 1024,
        max_total_bytes: 1024 * 1024,
        max_range_bytes: 128 * 1024,
        max_search_matches: 100,
        search_chunk_bytes: 64 * 1024,
        tombstone_limit: 1024,
    })
    .expect("create capped store");
    // One newline-free ~126 KiB stdout: over the 120 KiB cap with margin,
    // while a single halving of that stream lands the reduced result below
    // the 64 KiB inline budget.
    let req = tool_call_request(
        "run_command",
        json!({
            "command": concat!(
                "printf 'HEAD-OUT-SENTINEL\\n'; ",
                "head -c 126000 /dev/zero | tr '\\0' 'x'; ",
                "printf '\\nTAIL-OUT-SENTINEL\\n'"
            )
        }),
    );

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
    assert_eq!(
        inline.get("entryCapTruncated").and_then(Value::as_bool),
        Some(true),
        "a sub-budget lossy reduction must still disclose the loss"
    );
    let original_bytes = inline
        .get("entryCapOriginalBytes")
        .and_then(Value::as_u64)
        .expect("entryCapOriginalBytes must disclose the pre-reduction size");
    let omitted_bytes = inline
        .get("entryCapOmittedBytes")
        .and_then(Value::as_u64)
        .expect("entryCapOmittedBytes must quantify the dropped bytes");
    assert!(
        original_bytes > store.max_entry_bytes(),
        "the disclosed original size must exceed the cap, saw {original_bytes}"
    );
    assert!(omitted_bytes > 0, "the halving must drop bytes");
    let reduced_bytes = original_bytes - omitted_bytes;
    assert!(
        reduced_bytes <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES as u64,
        "the reduced size must sit below the inline budget, saw {reduced_bytes}"
    );
    assert!(
        inline.pointer("/responseBudget").is_none(),
        "a sub-budget result must stay inline without an outputRef manifest"
    );
    let stdout = inline
        .pointer("/structuredContent/stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        stdout.contains("HEAD-OUT-SENTINEL") && stdout.contains("TAIL-OUT-SENTINEL"),
        "the reduced stdout must keep both ends"
    );

    std::fs::remove_dir_all(workspace_root).ok();
}

#[test]
fn entry_cap_telemetry_reports_the_full_pre_reduction_result_as_raw() {
    use crate::tool_result_metrics::{ResponseClass, ToolResultMeasurement};

    // Externalized retry: raw is the pre-reduction size, externalized the
    // stored reduced payload.
    let outcome = response_budget::BudgetOutcome {
        output_ref: "lr_reduced".to_string(),
        raw_bytes: 70_000,
        externalized_bytes: 70_000,
    };
    let (raw, externalized) =
        super::resolve_raw_and_externalized_bytes(Some(outcome), Some(126_500), 6_400);
    assert_eq!((raw, externalized), (126_500, 70_000));
    let class = ToolResultMeasurement {
        raw_bytes: raw,
        inline_bytes: 6_400,
        externalized_bytes: externalized,
        is_error: false,
    }
    .classify();
    assert_eq!(class, ResponseClass::Externalized);

    // Inline retry below the budget: nothing is stored and bytes were
    // dropped in-band, so the record must classify as compacted, not small.
    let (raw, externalized) =
        super::resolve_raw_and_externalized_bytes(None, Some(126_500), 63_500);
    assert_eq!((raw, externalized), (126_500, 0));
    let class = ToolResultMeasurement {
        raw_bytes: raw,
        inline_bytes: 63_500,
        externalized_bytes: externalized,
        is_error: false,
    }
    .classify();
    assert_eq!(class, ResponseClass::Compacted);

    // Untouched result: raw equals inline and the record stays small.
    let (raw, externalized) = super::resolve_raw_and_externalized_bytes(None, None, 1_000);
    assert_eq!((raw, externalized), (1_000, 0));
    let class = ToolResultMeasurement {
        raw_bytes: raw,
        inline_bytes: 1_000,
        externalized_bytes: externalized,
        is_error: false,
    }
    .classify();
    assert_eq!(class, ResponseClass::Small);
}

/// Both entry-cap reduction paths must keep reporting the full pre-reduction
/// result as raw bytes: the externalized retry stores only the reduced
/// payload, and the sub-budget retry sends it inline — classified compacted,
/// never small.
#[cfg(unix)]
#[tokio::test]
async fn entry_cap_telemetry_counts_the_pre_reduction_size_as_raw() {
    // Spawn-dependent (sh/head/tr through PATH) and PATH is process-global:
    // hold the crate env lock so an env-rewriting test cannot interleave
    // (the dr6-sweep idiom); a broken spawn shrinks stdout below the entry
    // cap and the disclosure assertions lose their subject.
    let _env = env_lock();
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-mcp-entry-cap-metrics-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();
    // Cap above the inline budget: one payload lands between the two after
    // one halving (externalized retry), the other below the inline budget
    // (inline retry).
    let store = LargeResultStore::new(LargeResultStoreConfig {
        ttl: std::time::Duration::from_secs(3600),
        max_entry_bytes: 120 * 1024,
        max_total_bytes: 1024 * 1024,
        max_range_bytes: 128 * 1024,
        max_search_matches: 100,
        search_chunk_bytes: 64 * 1024,
        tombstone_limit: 1024,
    })
    .expect("create capped store");
    let before = tool_result_metrics::snapshot();

    let externalized_response = handle_tools_call_with_result_store(
        &tool_call_request(
            "run_command",
            json!({
                "command": concat!(
                    "printf 'HEAD-OUT-SENTINEL\\n'; ",
                    "head -c 140000 /dev/zero | tr '\\0' 'x'; ",
                    "printf '\\nTAIL-OUT-SENTINEL\\n'"
                )
            }),
        ),
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
    let externalized_inline = externalized_response
        .result
        .as_ref()
        .expect("missing result");
    let pre_externalized = externalized_inline
        .pointer("/responseBudget/entryCapOriginalBytes")
        .and_then(Value::as_u64)
        .expect("externalized retry must disclose the pre-reduction size");
    let stored_reduced = externalized_inline
        .pointer("/responseBudget/originalBytes")
        .and_then(Value::as_u64)
        .expect("externalized retry must report the stored payload size");

    let inline_response = handle_tools_call_with_result_store(
        &tool_call_request(
            "run_command",
            json!({
                "command": concat!(
                    "printf 'HEAD-OUT-SENTINEL\\n'; ",
                    "head -c 126000 /dev/zero | tr '\\0' 'x'; ",
                    "printf '\\nTAIL-OUT-SENTINEL\\n'"
                )
            }),
        ),
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
    let inline_result = inline_response.result.as_ref().expect("missing result");
    let pre_inline = inline_result
        .get("entryCapOriginalBytes")
        .and_then(Value::as_u64)
        .expect("inline retry must disclose the pre-reduction size");

    // The registry is process-global and parallel tests observe their own
    // calls too, so only this test's contribution (>=) is asserted; the
    // per-class resolution itself is covered by the unit test above.
    let after = tool_result_metrics::snapshot();
    let slot = perf_metrics::tool_index(Some("run_command"));
    let delta = tool_result_metrics::totals_delta(&before[slot], &after[slot]);
    assert!(
        delta.raw_bytes >= pre_externalized + pre_inline,
        "raw totals must count both pre-reduction sizes, saw {} < {} + {}",
        delta.raw_bytes,
        pre_externalized,
        pre_inline
    );
    assert!(
        delta.externalized_bytes >= stored_reduced,
        "externalized totals must count the stored reduced payload, saw {} < {}",
        delta.externalized_bytes,
        stored_reduced
    );
    assert!(
        delta.externalized_count >= 1,
        "the externalized retry must land in the externalized class"
    );
    assert!(
        delta.compacted_count >= 1,
        "the inline retry must classify as compacted, not small"
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

// ── Instruction payload budget (catdesk-ojt.7) ──────────────────────────────
//
// The bootstrap instruction is sent before every workspace task. The 2026-10-07
// shrink (catdesk-ojt.7) cut it by ~35%; these tests keep the payload materially
// below the pre-shrink baseline and pin every enforceable guarantee the shrink
// kept in the text (the rest moved to runtime enforcement or tool schemas —
// see docs/findings/2026-10-07-instruction-shrink.md).

/// Browser-only guidance that the pre-shrink Both payloads carried and the
/// Computer payloads never did (89 B sentence + its separator), so each
/// Computer baseline is its Both counterpart minus this delta.
const PRE_SHRINK_BROWSER_ONLY_DELTA: usize = 90;

/// Pre-shrink instruction payload baselines for every (Mode, ToolMode)
/// pair, measured 2026-10-07 and recorded in
/// docs/findings/2026-10-07-instruction-shrink.md (workspace-scoped
/// fragments included; ~±150 B variance from the dynamic handoff
/// prefix/filename between runs).
fn instruction_baseline_bytes(mode: Mode, tool_mode: ToolMode) -> usize {
    match (mode, tool_mode) {
        (Mode::Both, ToolMode::MultiTools) => 5548,
        (Mode::Both, ToolMode::ReadOnly) => 3645,
        (Mode::Computer, ToolMode::MultiTools) => 5548 - PRE_SHRINK_BROWSER_ONLY_DELTA,
        (Mode::Computer, ToolMode::ReadOnly) => 3645 - PRE_SHRINK_BROWSER_ONLY_DELTA,
        (Mode::Browser, ToolMode::MultiTools) | (Mode::Browser, ToolMode::ReadOnly) => 1472,
    }
}

/// Headroom for the dynamic handoff prefix/filename inside a test workspace.
/// Only computer-enabled payloads carry those fragments; the Browser header
/// is fully static, so its budget needs no headroom.
const INSTRUCTION_DYNAMIC_FRAGMENT_HEADROOM: usize = 300;
/// The payload must stay at least 25% below the per-mode pre-shrink baseline.
fn instruction_budget_bytes(mode: Mode, tool_mode: ToolMode) -> usize {
    let headroom = if mode.computer_enabled() {
        INSTRUCTION_DYNAMIC_FRAGMENT_HEADROOM
    } else {
        0
    };
    instruction_baseline_bytes(mode, tool_mode) * 3 / 4 - headroom
}

/// Scope tags: `all` = every mode, `computer` = computer-enabled modes,
/// `computer+run` = computer-enabled multi-tools mode.
const INSTRUCTION_GUARANTEE_PHRASES: &[(&str, &str)] = &[
    // Safety: workspace boundary (runtime-enforced by workspace path checks).
    ("all", "inside the workspace root"),
    // Safety: sandbox vs Workspace priority; never silently fall back.
    ("all", "has no internet connection"),
    ("all", "use Workspace first"),
    ("all", "explicitly report the raw error to the user"),
    ("all", "Do NOT fall back to the sandbox container"),
    // Workflow: retry, connector refresh, attribution, push hygiene.
    ("all", "call the same tool again with the same parameters"),
    ("all", "refresh with api_tool.list_resources"),
    ("all", "Match recent commit style"),
    ("all", "CatDesk manages that automatically"),
    ("all", "Always specify the branch explicitly"),
    // Workflow: dedicated tools before shell.
    ("all", "Prefer dedicated MCP tools"),
    // Workflow: images via read_image; server-side vision fallback.
    ("computer", "read_image"),
    ("computer", "native image content"),
    ("computer", "structuredContent.analysis.description"),
    // Workflow: handoff discovery is gated, untrusted, verified against the
    // workspace, delete-after-read, and never overrides higher-priority
    // instructions.
    ("computer", "files.search"),
    ("computer", "persistent ChatGPT Library"),
    ("computer", "untrusted session context"),
    ("computer", "verify it against the workspace"),
    ("computer", "never overrides the current user request"),
    ("computer", "If none are found, continue normally"),
    (
        "computer",
        "delete that Library file only after a successful read",
    ),
    (
        "computer",
        "delete only that chosen handoff after a successful read",
    ),
    ("computer", "Library Search must be enabled"),
    ("computer", "use create_handoff"),
    // Save workflow: exact-name replacement in the Library; the agent-level
    // prohibition covers both the repository and the workspace (the tool
    // itself not writing the workspace is only a tool fact, not the ban).
    ("computer", "replacing any older exact-name copy"),
    (
        "computer",
        "Do not leave a handoff in the repository or workspace",
    ),
    // Safety: no secrets in handoffs.
    (
        "computer",
        "never put credentials, tokens, or other secrets",
    ),
    // Workflow: long commands must be background jobs (120 s ceiling is
    // runtime-enforced by command::clamp_timeout/MAX_TIMEOUT_MS).
    ("computer+run", "run_command is a last resort"),
    ("computer+run", "more than about 20 seconds"),
    ("computer+run", "must never run through run_command"),
    ("computer+run", "start_command"),
    ("computer+run", "poll_command"),
    ("computer+run", "hasMoreOutput"),
    ("computer+run", "survive a CatDesk restart"),
    ("computer+run", "interrupted"),
    ("computer+run", "abandoned"),
    ("computer+run", "cancel_command"),
    (
        "computer+run",
        "never run duplicates of a still-running job",
    ),
];

fn instruction_scope_matches(scope: &str, mode: Mode, tool_mode: ToolMode) -> bool {
    match scope {
        "all" => true,
        "computer" => mode.computer_enabled(),
        "computer+run" => mode.computer_enabled() && tool_mode.run_command_enabled(),
        other => panic!("unknown instruction guarantee scope: {other}"),
    }
}

/// Full-sentence rules that must appear EXACTLY once — substring guarantees
/// above cannot see accidental duplication (a shrink regression once glued
/// the git-push rule to itself). Same scopes as the guarantee phrases.
const INSTRUCTION_UNIQUE_RULES: &[(&str, &str)] = &[
    ("all", "Always specify the branch explicitly in `git push`"),
    ("all", "Do not manually add CatDesk co-author attribution"),
    ("computer", "If exactly one is found"),
    ("computer", "If none are found, continue normally"),
    ("computer", "If multiple matching handoffs are found"),
];

#[test]
fn instruction_payload_stays_materially_below_baseline() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-instruction-budget-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    for mode in [Mode::Both, Mode::Computer, Mode::Browser] {
        for tool_mode in [ToolMode::MultiTools, ToolMode::ReadOnly] {
            let text = catdesk_instruction_text(&workspace_root_str, mode, tool_mode)
                .expect("build instruction");
            let baseline = instruction_baseline_bytes(mode, tool_mode);
            let budget = instruction_budget_bytes(mode, tool_mode);
            assert!(
                text.len() < budget,
                "{}/{} instruction payload must stay materially below the {}-byte \
                 pre-shrink baseline (budget {} bytes), got {} bytes",
                mode.label(),
                tool_mode.label(),
                baseline,
                budget,
                text.len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn instruction_payload_keeps_every_enforceable_guarantee() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-instruction-guarantee-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let workspace_root_str = workspace_root.to_string_lossy().into_owned();

    for mode in [Mode::Both, Mode::Computer, Mode::Browser] {
        for tool_mode in [ToolMode::MultiTools, ToolMode::ReadOnly] {
            let text = catdesk_instruction_text(&workspace_root_str, mode, tool_mode)
                .expect("build instruction");
            for (scope, phrase) in INSTRUCTION_GUARANTEE_PHRASES {
                if !instruction_scope_matches(scope, mode, tool_mode) {
                    continue;
                }
                assert!(
                    text.contains(phrase),
                    "{}/{} instruction lost the guarantee `{phrase}` during shrinking: {text}",
                    mode.label(),
                    tool_mode.label(),
                );
            }
            for (scope, rule) in INSTRUCTION_UNIQUE_RULES {
                if !instruction_scope_matches(scope, mode, tool_mode) {
                    continue;
                }
                let occurrences = text.matches(rule).count();
                assert_eq!(
                    occurrences,
                    1,
                    "{}/{} instruction rule `{rule}` must appear exactly once, found {occurrences}: {text}",
                    mode.label(),
                    tool_mode.label(),
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[test]
fn devtools_listing_requests_default_to_bounded_pages_without_overriding_explicit_page_size() {
    for tool_name in ["list_console_messages", "list_network_requests"] {
        let req = tool_call_request(tool_name, json!({ "pageIdx": 7 }));
        let bounded = apply_devtools_request_defaults(tool_name, &req.params);
        assert_eq!(bounded["arguments"]["pageSize"], json!(100));
        assert_eq!(bounded["arguments"]["pageIdx"], json!(7));

        let explicit = tool_call_request(tool_name, json!({ "pageIdx": 7, "pageSize": 250 }));
        let bounded = apply_devtools_request_defaults(tool_name, &explicit.params);
        assert_eq!(bounded["arguments"]["pageSize"], json!(250));
    }
}

#[test]
fn devtools_snapshot_defaults_to_non_verbose_without_overriding_explicit_verbose() {
    let compact = tool_call_request("take_snapshot", json!({}));
    let bounded = apply_devtools_request_defaults("take_snapshot", &compact.params);
    assert_eq!(bounded["arguments"]["verbose"], json!(false));

    let verbose = tool_call_request("take_snapshot", json!({ "verbose": true }));
    let bounded = apply_devtools_request_defaults("take_snapshot", &verbose.params);
    assert_eq!(bounded["arguments"]["verbose"], json!(true));
}

#[cfg(unix)]
#[tokio::test]
async fn devtools_forwarding_sends_bounded_defaults_to_upstream_peer() {
    // python3 resolves through the process-global PATH; take the env lock so a
    // concurrent sandbox test cannot swap PATH to stub-only while we spawn.
    let _env = env_lock();
    let child = tokio::process::Command::new("python3")
        .args([
            "-c",
            r#"import json,sys
for line in sys.stdin:
    req=json.loads(line)
    args=req.get('params',{}).get('arguments',{})
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':{'content':[{'type':'text','text':json.dumps(args,sort_keys=True)}]}}), flush=True)
"#,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn fake DevTools peer");
    let bridge = DevtoolsBridge::from_child(child).expect("create DevTools bridge");
    let devtools = Some(bridge);

    let list_req = tool_call_request("list_network_requests", json!({ "pageIdx": 3 }));
    let list_response = forward_to_devtools(
        &list_req,
        "list_network_requests",
        ToolMode::MultiTools,
        &devtools,
    )
    .await;
    let list_args: Value = serde_json::from_str(
        list_response
            .result
            .as_ref()
            .and_then(|value| value.pointer("/content/0/text"))
            .and_then(Value::as_str)
            .expect("echoed list arguments"),
    )
    .expect("decode list arguments");
    assert_eq!(list_args["pageSize"], json!(100));
    assert_eq!(list_args["pageIdx"], json!(3));

    let snapshot_req = tool_call_request("take_snapshot", json!({}));
    let snapshot_response = forward_to_devtools(
        &snapshot_req,
        "take_snapshot",
        ToolMode::MultiTools,
        &devtools,
    )
    .await;
    let snapshot_args: Value = serde_json::from_str(
        snapshot_response
            .result
            .as_ref()
            .and_then(|value| value.pointer("/content/0/text"))
            .and_then(Value::as_str)
            .expect("echoed snapshot arguments"),
    )
    .expect("decode snapshot arguments");
    assert_eq!(snapshot_args["verbose"], json!(false));
}

#[cfg(unix)]
#[tokio::test]
async fn devtools_forwarding_externalizes_large_network_body_through_shared_budget() {
    // python3 resolves through the process-global PATH; take the env lock so a
    // concurrent sandbox test cannot swap PATH to stub-only while we spawn.
    let _env = env_lock();
    let workspace_root = read_workspace("devtools-forward-large-body");
    let store = LargeResultStore::new_default().expect("create result store");
    let child = tokio::process::Command::new("python3")
        .args([
            "-c",
            r#"import json,sys
req=json.loads(sys.stdin.readline())
body='HEAD-SENTINEL\\n' + ('0123456789abcdef' * 5000) + '\\nTAIL-SENTINEL'
print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':{'content':[{'type':'text','text':body}],'isError':False}}), flush=True)
"#,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn fake DevTools peer");
    let bridge = DevtoolsBridge::from_child(child).expect("create DevTools bridge");
    let devtools = Some(bridge);
    let jobs = CommandJobManager::new();
    let req = tool_call_request("get_network_request", json!({ "reqid": 17 }));

    let response = handle_tools_call_with_result_store(
        &req,
        workspace_root.to_str().expect("workspace path"),
        0,
        Mode::Browser,
        ToolMode::MultiTools,
        false,
        &jobs,
        &devtools,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;

    let result = response.result.as_ref().expect("forwarded result");
    assert_eq!(
        result
            .pointer("/responseBudget/policy")
            .and_then(Value::as_str),
        Some("catdesk-mcp-v1")
    );
    let result_id = result
        .pointer("/responseBudget/outputRef")
        .and_then(Value::as_str)
        .expect("large DevTools result should be externalized");
    let encoded_len = serde_json::to_vec(result)
        .expect("encode forwarded preview")
        .len();
    assert!(
        encoded_len <= super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "forwarded preview was {encoded_len} bytes"
    );

    let read_req = tool_call_request(
        "read_result",
        json!({ "result_id": result_id, "max_bytes": store.max_range_bytes() }),
    );
    let read_response = handle_tools_call_with_result_store(
        &read_req,
        workspace_root.to_str().expect("workspace path"),
        0,
        Mode::Computer,
        ToolMode::MultiTools,
        false,
        &jobs,
        &None,
        ShowDetailMode::Disable,
        &store,
        Some("session-a"),
        None,
    )
    .await;
    let restored_text = read_response
        .result
        .as_ref()
        .and_then(|value| value.pointer("/structuredContent/text"))
        .and_then(Value::as_str)
        .expect("retrieved JSON text");
    let restored: Value =
        serde_json::from_str(restored_text).expect("decode stored DevTools result");
    let body = restored
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .expect("stored response body");
    assert!(body.starts_with("HEAD-SENTINEL"));
    assert!(body.ends_with("TAIL-SENTINEL"));
    assert!(body.len() > super::response_budget::DEFAULT_INLINE_RESPONSE_BYTES);

    let _ = std::fs::remove_dir_all(workspace_root);
}

// ── Tool payload audit inventory (catdesk-ojt.5) ────────────────────────────
//
// Single source of truth pairing every locally exposed tool with its audited
// bounding class. The companion document is
// docs/findings/2026-10-07-tool-payload-audit.md; these tests keep the
// catalog, this list, and that document in lockstep so a new tool cannot ship
// without a payload audit.

/// (tool, bounding class) for every tool the local catalog can expose. Must
/// match the local tool inventory table in the audit document exactly —
/// including the per-tool bounding class, which the document test compares
/// column by column.
const TOOL_PAYLOAD_AUDIT: &[(&str, &str)] = &[
    ("run_command", "shared-budget+pre-cap"),
    ("start_command", "shared-budget+pre-cap"),
    ("poll_command", "shared-budget+pre-cap"),
    ("cancel_command", "shared-budget+pre-cap"),
    // Not inherent-static: the template is fixed, but the response embeds
    // uncapped host-controlled AGENTS.md text and Binagotchy card images
    // (finding F5). Only the shared-budget gate bounds it today.
    ("catdesk_instruction", "shared-budget"),
    ("read", "shared-budget+pre-cap"),
    ("read_image", "multimodal-exempt"),
    ("search", "shared-budget+pre-cap"),
    ("read_result", "store-range"),
    ("search_result", "store-range"),
    ("write", "inherent-static"),
    ("edit", "inherent-static"),
    ("create_handoff", "shared-budget+pre-cap"),
    ("delete", "inherent-static"),
];

/// The bounding classes the audit recognizes. A mechanism string outside this
/// list means the inventory entry was hand-waved, not audited.
const AUDITED_BOUNDING_CLASSES: &[&str] = &[
    "shared-budget",
    "shared-budget+pre-cap",
    "store-range",
    "inherent-static",
    "multimodal-exempt",
    "devtools-passthrough",
];

/// The exact tool list (order included) the catalog must expose for one
/// (mode, tool_mode) pair with the DevTools bridge absent.
fn audit_expected_tools(mode: Mode, tool_mode: ToolMode) -> &'static [&'static str] {
    match (mode, tool_mode) {
        (Mode::Browser, _) => &["catdesk_instruction"],
        (Mode::Computer | Mode::Both, ToolMode::ReadOnly) => &[
            "catdesk_instruction",
            "read",
            "read_image",
            "search",
            "read_result",
            "search_result",
            "create_handoff",
        ],
        (Mode::Computer | Mode::Both, ToolMode::MultiTools) => &[
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
        ],
    }
}

/// Names the catalog exposes for one (mode, tool_mode) pair with the given
/// DevTools bridge.
async fn audit_tools_list_names(
    mode: Mode,
    tool_mode: ToolMode,
    devtools: &Option<std::sync::Arc<tokio::sync::Mutex<crate::devtools::DevtoolsBridge>>>,
) -> Vec<String> {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!("req-tool-payload-audit")),
        method: "tools/list".into(),
        params: json!({}),
    };
    handle_tools_list(&req, mode, tool_mode, devtools)
        .await
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("missing tools")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn tool_payload_audit_covers_every_exposed_tool() {
    let mut exposed_union = std::collections::BTreeSet::new();
    for (mode, tool_mode) in [
        (Mode::Both, ToolMode::MultiTools),
        (Mode::Both, ToolMode::ReadOnly),
        (Mode::Computer, ToolMode::MultiTools),
        (Mode::Computer, ToolMode::ReadOnly),
        (Mode::Browser, ToolMode::MultiTools),
        (Mode::Browser, ToolMode::ReadOnly),
    ] {
        let expected = audit_expected_tools(mode, tool_mode);
        let exposed = audit_tools_list_names(mode, tool_mode, &None).await;
        let expected_owned: Vec<String> = expected.iter().map(|name| (*name).to_string()).collect();
        assert_eq!(
            exposed,
            expected_owned,
            "tools/list exposure drifted for {}/{}; audit the change, then update \
             audit_expected_tools, TOOL_PAYLOAD_AUDIT and \
             docs/findings/2026-10-07-tool-payload-audit.md together",
            mode.label(),
            tool_mode.label(),
        );
        for name in expected {
            assert!(
                TOOL_PAYLOAD_AUDIT
                    .iter()
                    .any(|(audited, _)| audited == name),
                "tool `{name}` is expected in {}/{} but has no payload-audit entry; \
                 add it to TOOL_PAYLOAD_AUDIT and to the inventory document",
                mode.label(),
                tool_mode.label(),
            );
            exposed_union.insert((*name).to_string());
        }
    }

    let audited: std::collections::BTreeSet<String> = TOOL_PAYLOAD_AUDIT
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    let stale: Vec<String> = audited.difference(&exposed_union).cloned().collect();
    assert!(
        stale.is_empty(),
        "payload audit inventory lists tools that the catalog never exposes: {stale:?}"
    );
}

#[test]
fn tool_payload_audit_mechanisms_use_audited_classes() {
    for (tool, mechanism) in TOOL_PAYLOAD_AUDIT {
        assert!(
            AUDITED_BOUNDING_CLASSES.contains(mechanism),
            "tool `{tool}` claims bounding class `{mechanism}`, which is not one of \
             the audited classes {AUDITED_BOUNDING_CLASSES:?}"
        );
    }
}

#[test]
fn tool_payload_audit_document_lists_every_audited_tool() {
    let document = include_str!("../../docs/findings/2026-10-07-tool-payload-audit.md");
    let inventory = document
        .split("## Local tool inventory")
        .nth(1)
        .expect("missing '## Local tool inventory' section")
        .split("## DevTools passthrough")
        .next()
        .expect("missing '## DevTools passthrough' section");

    // Parse the full record of every local-tool row: name, exposure tokens,
    // and bounding class. Row shape: | `tool` | C+M, C+R | ... | `class` | ...
    let mut rows: std::collections::BTreeMap<String, (Vec<String>, String)> =
        std::collections::BTreeMap::new();
    for line in inventory.lines().filter(|line| line.starts_with("| `")) {
        let columns: Vec<&str> = line.split('|').collect();
        assert!(
            columns.len() >= 5,
            "inventory row must expose Tool/Exposed in/Bounding columns: {line}"
        );
        let tool = columns[1].trim().trim_matches('`').to_string();
        assert!(!tool.is_empty(), "inventory row has no tool name: {line}");
        let exposure: Vec<String> = columns[2]
            .trim()
            .split(", ")
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect();
        // Columns: Tool | Exposed in | Payload surface(s) | Bounding | ...
        let bounding = columns[4].trim().trim_matches('`').to_string();
        assert!(
            !bounding.is_empty(),
            "inventory row has no bounding class: {line}"
        );
        rows.insert(tool, (exposure, bounding));
    }

    let audited: std::collections::BTreeMap<&str, &str> =
        TOOL_PAYLOAD_AUDIT.iter().copied().collect();
    let documented: std::collections::BTreeSet<String> = rows.keys().cloned().collect();
    let audited_names: std::collections::BTreeSet<String> =
        audited.keys().map(|name| (*name).to_string()).collect();
    assert_eq!(
        documented, audited_names,
        "audit document rows and audited inventory drifted out of sync"
    );

    for (tool, (exposure, bounding)) in &rows {
        let audited_class = audited
            .get(tool.as_str())
            .unwrap_or_else(|| panic!("tool `{tool}` missing from TOOL_PAYLOAD_AUDIT"));
        assert_eq!(
            bounding, *audited_class,
            "bounding class for `{tool}` drifted between the audit document and \
             TOOL_PAYLOAD_AUDIT"
        );

        let mut computed_exposure: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for (mode, tool_mode, token) in [
            (Mode::Both, ToolMode::MultiTools, "C+M"),
            (Mode::Both, ToolMode::ReadOnly, "C+R"),
            (Mode::Browser, ToolMode::MultiTools, "B"),
        ] {
            if audit_expected_tools(mode, tool_mode).contains(&tool.as_str()) {
                computed_exposure.insert(token.to_string());
            }
        }
        let parsed_exposure: std::collections::BTreeSet<String> =
            exposure.iter().cloned().collect();
        assert_eq!(
            parsed_exposure, computed_exposure,
            "`Exposed in` column for `{tool}` drifted from the enumerated catalog exposure"
        );
    }
}

// ── DevTools passthrough payload contract (catdesk-ojt.5) ───────────────────
//
// Dynamic DevTools tool names are not statically enumerable, so the audit pins
// the passthrough behavior instead: listing with arbitrary names, the
// read-only filter, and the shared response-budget gate on forwarded results.

/// A fake chrome-devtools-mcp process: echoes the request `id`, serves a fixed
/// tool list with one read-only and one mutating tool, and answers any
/// tools/call with a large payload.
///
/// Unix-only, matching `DevtoolsBridge::bridge_for_test` (which spawns
/// python3); without this gate the Windows test build would fail to resolve
/// the bridge method.
#[cfg(unix)]
const FAKE_DEVTOOLS_SERVER_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "tools/list":
        result = {"tools": [
            {"name": "fake_dt_read", "annotations": {"readOnlyHint": True}},
            {"name": "fake_dt_write", "annotations": {"readOnlyHint": False}},
        ]}
    elif method == "tools/call":
        result = {
            "content": [{"type": "text", "text": "A" * 200_000}],
            "structuredContent": {"toolName": "fake_dt_big", "data": "B" * 200_000, "success": True},
        }
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "result": result}) + "\n")
    sys.stdout.flush()
"#;

#[cfg(unix)]
async fn fake_devtools_bridge()
-> std::sync::Arc<tokio::sync::Mutex<crate::devtools::DevtoolsBridge>> {
    crate::devtools::DevtoolsBridge::bridge_for_test(FAKE_DEVTOOLS_SERVER_SCRIPT)
        .await
        .expect("spawn fake devtools bridge")
}

#[tokio::test]
#[cfg(unix)]
async fn devtools_passthrough_lists_dynamic_tools_and_filters_read_only() {
    let _env = env_lock();
    let bridge = fake_devtools_bridge().await;

    let multi =
        audit_tools_list_names(Mode::Browser, ToolMode::MultiTools, &Some(bridge.clone())).await;
    assert_eq!(
        multi,
        vec![
            "catdesk_instruction".to_string(),
            "fake_dt_read".to_string(),
            "fake_dt_write".to_string(),
        ],
        "browser tools/list must expose every dynamic DevTools tool by its own name"
    );

    let read_only = audit_tools_list_names(Mode::Browser, ToolMode::ReadOnly, &Some(bridge)).await;
    assert_eq!(
        read_only,
        vec![
            "catdesk_instruction".to_string(),
            "fake_dt_read".to_string(),
        ],
        "read-only mode must filter dynamic DevTools tools by readOnlyHint"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn devtools_passthrough_big_result_goes_through_shared_budget() {
    let _env = env_lock();
    let bridge = fake_devtools_bridge().await;
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-audit-devtools-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let req = tool_call_request("fake_dt_big", json!({}));

    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Browser,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &Some(bridge),
    )
    .await;

    let result = response.result.expect("forwarded result");
    let serialized = serde_json::to_vec(&result).expect("serialize result");
    assert!(
        serialized.len() <= crate::mcp::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "a large DevTools result must be compacted below the inline budget, got {} bytes",
        serialized.len()
    );
    assert!(
        result
            .pointer("/responseBudget/outputRef")
            .and_then(Value::as_str)
            .is_some_and(|output_ref| !output_ref.is_empty()),
        "a large DevTools result must carry a responseBudget manifest with a lossless outputRef"
    );
    assert!(
        result
            .pointer("/responseBudget/preview/omissionCount")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0),
        "the manifest must report the omissions made to the DevTools payload"
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

// ── catdesk_instruction externalization (catdesk-ojt.5, finding F5) ─────────

#[tokio::test]
async fn oversized_catdesk_instruction_is_externalized_not_inlined() {
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-audit-instruction-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    // ~1 MiB of AGENTS.md: the template itself is static, but the AGENTS.md
    // layers (finding F5) are host-controlled and uncapped, so only the
    // shared-budget gate may bound this response.
    std::fs::write(
        workspace_root.join("AGENTS.md"),
        "INSTRUCTIONS-".repeat(80_000),
    )
    .expect("write oversized AGENTS.md");
    let req = tool_call_request("catdesk_instruction", json!({}));

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

    let result = response.result.expect("instruction result");
    let serialized = serde_json::to_vec(&result).expect("serialize result");
    assert!(
        serialized.len() <= crate::mcp::response_budget::DEFAULT_INLINE_RESPONSE_BYTES,
        "an oversized instruction must be externalized, not inlined; got {} bytes",
        serialized.len()
    );
    assert_eq!(
        result
            .pointer("/responseBudget/retrieval/tool")
            .and_then(Value::as_str),
        Some("read_result"),
        "the manifest must point at the lossless retrieval tool"
    );
    assert!(
        result
            .pointer("/responseBudget/outputRef")
            .and_then(Value::as_str)
            .is_some_and(|output_ref| !output_ref.is_empty()),
        "the manifest must carry a lossless outputRef"
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

// ── widget meta rides the budget gate (catdesk-8o1, finding F4) ─────────────

/// Fake bridge returning a `structuredContent.data` string of caller-chosen
/// length: the test calibrates the fixed JSON framing from a small call and
/// then lands the final answer just above the inline cap.
#[cfg(unix)]
const FAKE_META_FIT_SERVER_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "tools/list":
        result = {"tools": [
            {"name": "fake_dt_meta_fit", "annotations": {"readOnlyHint": True}},
        ]}
    elif method == "tools/call":
        n = request["params"]["arguments"].get("n", 0)
        result = {
            "content": [],
            "structuredContent": {"toolName": "fake_dt_meta_fit", "data": "B" * n, "success": True},
        }
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "result": result}) + "\n")
    sys.stdout.flush()
"#;

#[cfg(unix)]
async fn fake_meta_fit_bridge()
-> std::sync::Arc<tokio::sync::Mutex<crate::devtools::DevtoolsBridge>> {
    crate::devtools::DevtoolsBridge::bridge_for_test(FAKE_META_FIT_SERVER_SCRIPT)
        .await
        .expect("spawn fake meta-fit devtools bridge")
}

#[cfg(unix)]
async fn meta_fit_call(
    bridge: &std::sync::Arc<tokio::sync::Mutex<crate::devtools::DevtoolsBridge>>,
    workspace_root: &std::path::Path,
    n: usize,
) -> serde_json::Value {
    let req = tool_call_request("fake_dt_meta_fit", json!({ "n": n }));
    let response = handle_tools_call(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Browser,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &Some(bridge.clone()),
    )
    .await;
    response.result.expect("meta-fit result")
}

#[tokio::test]
#[cfg(unix)]
async fn inline_result_with_widget_meta_stays_within_inline_budget() {
    let bridge = fake_meta_fit_bridge().await;
    let workspace_root = std::env::temp_dir().join(format!("catdesk-meta-fit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let cap = crate::mcp::response_budget::DEFAULT_INLINE_RESPONSE_BYTES;

    // First call measures the framing around `data` for this flow, widget
    // meta already included. The second call lands the final answer
    // META_OVERSHOOT bytes ABOVE the cap — above it by less than the
    // turnTokenUsage/toolCallCount attachment costs. A gate that decides on
    // the meta-less bytes (the F4 bug) sees an in-budget result and inlines
    // it, and the post-gate attachment pushes the shipped answer past the
    // cap; a gate that sees the meta first externalizes the answer and the
    // shipped inline form stays within the cap.
    let probe = meta_fit_call(&bridge, &workspace_root, 1_024).await;
    let probe_len = serde_json::to_vec(&probe).expect("serialize probe").len();
    const META_OVERSHOOT: usize = 48;
    let n = 1_024 + (cap + META_OVERSHOOT - probe_len);

    let result = meta_fit_call(&bridge, &workspace_root, n).await;
    let serialized = serde_json::to_vec(&result).expect("serialize final result");
    assert!(
        serialized.len() <= cap,
        "a tool result with widget meta must stay within the inline budget, got {} bytes",
        serialized.len()
    );
    let meta = result
        .get("_meta")
        .and_then(|meta| meta.get(WIDGET_PAYLOAD_META_KEY))
        .expect("widget meta missing from the final response");
    assert!(
        meta.get("turnTokenUsage").is_some() && meta.get("toolCallCount").is_some(),
        "widget meta must keep carrying the usage fields after the budget gate"
    );
    let _ = std::fs::remove_dir_all(workspace_root);
}

// ── read_result worst-case size (catdesk-ojt.5) ─────────────────────────────

/// Puts `payload` in the dispatcher's result store and reads one maximal range
/// back through the FULL tools/call dispatcher, so the exemption of read_result
/// from `apply_response_budget` (src/mcp.rs) is exercised, not just the handler.
///
/// Runs with `ShowDetailMode::Disable`: the budget gate is independent of the
/// detail mode, but widget/token enrichment is not — and the o200k estimate
/// over an untrimmed retrieval payload takes tens of seconds (finding F6).
async fn maximal_read_result_through_dispatcher(payload: &[u8]) -> (serde_json::Value, usize) {
    let store = fallback_result_store();
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-audit-read-result-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let range_bytes = crate::result_store::DEFAULT_MAX_RANGE_BYTES;
    let stored = store
        .put(None, &workspace_root, payload, None)
        .expect("store maximal range payload");
    let req = tool_call_request(
        "read_result",
        json!({
            "result_id": stored.metadata.result_id,
            "offset": 0,
            "max_bytes": range_bytes,
        }),
    );

    let response = handle_tools_call_with_session(
        &req,
        &workspace_root.to_string_lossy(),
        1,
        Mode::Both,
        ToolMode::MultiTools,
        false,
        &CommandJobManager::new(),
        &None,
        ShowDetailMode::Disable,
        None,
        None,
    )
    .await;

    let result = response.result.expect("read_result payload");
    let serialized = serde_json::to_vec(&result).expect("serialize result");
    let _ = std::fs::remove_dir_all(workspace_root);
    (result, serialized.len())
}

/// Reads one maximal range straight from the handler (fast path, no dispatcher
/// overhead) to pin the serialized worst-case size for a payload shape.
fn maximal_read_result_from_handler(payload: &[u8]) -> usize {
    let store = LargeResultStore::new_default().expect("create result store");
    let workspace_root =
        std::env::temp_dir().join(format!("catdesk-audit-read-range-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&workspace_root).expect("create workspace");
    let range_bytes = crate::result_store::DEFAULT_MAX_RANGE_BYTES;
    let stored = store
        .put(None, &workspace_root, payload, None)
        .expect("store maximal range payload");
    let req = tool_call_request(
        "read_result",
        json!({
            "result_id": stored.metadata.result_id,
            "offset": 0,
            "max_bytes": range_bytes,
        }),
    );
    let response = handle_read_result(&req, &workspace_root.to_string_lossy(), &store, None);
    let result = response.result.expect("read_result payload");
    let len = serde_json::to_vec(&result).expect("serialize result").len();
    let _ = std::fs::remove_dir_all(workspace_root);
    len
}

#[tokio::test]
async fn read_result_bypasses_dispatcher_budget_gate() {
    let (result, serialized) = maximal_read_result_through_dispatcher(&vec![
        b'a';
        crate::result_store::DEFAULT_MAX_RANGE_BYTES
    ])
    .await;
    // One maximal ASCII range: dataBase64 ≈ 171 KiB + the same bytes mirrored
    // as text ≈ 128 KiB + metadata ≈ 306 KiB total — well over the 64 KiB
    // inline budget, so only the dispatcher exemption keeps this inline.
    assert!(
        (250 * 1024..=450 * 1024).contains(&serialized),
        "one maximal ASCII range must serialize to roughly 306 KiB, got {serialized} bytes"
    );
    assert!(
        result.get("responseBudget").is_none(),
        "read_result is the retrieval instrument and must bypass the shared response-budget \
         gate; a responseBudget manifest here means the dispatcher exemption (src/mcp.rs) \
         was removed and read_result now re-externalizes its own retrieval payload"
    );
}

#[test]
fn read_result_max_range_serialized_size_stays_bounded() {
    let serialized =
        maximal_read_result_from_handler(&vec![b'a'; crate::result_store::DEFAULT_MAX_RANGE_BYTES]);
    assert!(
        (250 * 1024..=450 * 1024).contains(&serialized),
        "one maximal ASCII range must serialize to roughly 306 KiB, got {serialized} bytes"
    );
}

#[test]
fn read_result_control_byte_range_serializes_bounded_escaped_text() {
    let serialized =
        maximal_read_result_from_handler(&vec![0x01; crate::result_store::DEFAULT_MAX_RANGE_BYTES]);
    // Control bytes are valid UTF-8, so the range is mirrored as text and
    // JSON-escaped as \u00XX (6 bytes per byte): base64 ≈ 171 KiB + escaped
    // text mirror ≈ 768 KiB + metadata ≈ 962 KiB — still bounded by the
    // store-side range validation.
    assert!(
        (800 * 1024..=1100 * 1024).contains(&serialized),
        "one maximal control-byte range must serialize to roughly 962 KiB, got {serialized} bytes"
    );
}
