use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::change_tracking::{ChangeScope, ChangeSession, ChangeTarget};
use crate::command;
use crate::command_jobs::{
    CommandJobManager, CommandJobSnapshot, CommandJobState, DEFAULT_POLL_WAIT_MS,
    MAX_POLL_WAIT_MS,
};
use crate::devtools::DevtoolsBridge;
use crate::project_scope;
use crate::state::{ShowDetailMode, ToolMode};
use crate::workspace_tools;

use crate::mcp::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, tool_arguments, tool_error_response,
    tool_error_response_with_structured, tool_name_from_request,
    tool_success_response_with_structured,
};
use crate::mcp::{
    optional_string_argument, read_only_blocked_response, required_string_argument,
    tool_is_read_only,
};

pub(crate) async fn forward_to_devtools(
    req: &JsonRpcRequest,
    tool_name: &str,
    tool_mode: ToolMode,
    devtools: &Option<Arc<Mutex<DevtoolsBridge>>>,
) -> JsonRpcResponse {
    let params = &req.params;
    let Some(bridge) = devtools else {
        return tool_error_response(req, format!("Unknown tool: {tool_name}"));
    };

    if tool_mode.read_only() {
        match devtools_tool_is_read_only(bridge, tool_name).await {
            Some(true) => {}
            Some(false) => return read_only_blocked_response(req, tool_name),
            None => {
                return tool_error_response(
                    req,
                    format!(
                        "Tool '{tool_name}' is blocked in read-only mode (cannot verify readOnlyHint)"
                    ),
                );
            }
        }
    }

    let forward_req = json!({
        "jsonrpc": "2.0",
        "id": req.id,
        "method": "tools/call",
        "params": params
    });

    match DevtoolsBridge::call(bridge, &forward_req).await {
        Ok(resp) => {
            if let Some(result) = resp.get("result") {
                return JsonRpcResponse::success(req.id.clone(), result.clone());
            }
            if let Some(error) = resp.get("error") {
                let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
                let msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                return tool_error_response(
                    req,
                    format!("DevTools tool error (code {code}): {msg}"),
                );
            }
            tool_error_response(req, "DevTools bridge returned empty response".into())
        }
        Err(e) => tool_error_response(req, format!("DevTools bridge error: {e}")),
    }
}

pub(crate) fn format_command_output_events<'a, I>(events: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut output = String::new();
    for (stream, text) in events {
        if stream == "stderr" {
            output.push_str("[stderr] ");
        }
        output.push_str(text);
        if !text.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

pub(crate) fn command_job_output_text(snapshot: &CommandJobSnapshot) -> String {
    if snapshot.events.is_empty() {
        return match snapshot.state {
            CommandJobState::Queued => "(no new output; command is starting)".to_string(),
            CommandJobState::Running => "(no new output; command is still running)".to_string(),
            CommandJobState::Interrupted => {
                "(command interrupted: CatDesk exited before the command finished; output was not retained)"
                    .to_string()
            }
            CommandJobState::Abandoned => {
                "(job abandoned: no poll arrived within the keep-alive window; start the command again if its work is still needed)"
                    .to_string()
            }
            _ => "(no new output)".to_string(),
        };
    }
    let mut output = format_command_output_events(
        snapshot
            .events
            .iter()
            .map(|event| (event.stream, event.text.as_str())),
    );
    if snapshot.has_more_output {
        output.push_str("[more buffered output available; poll again with nextCursor]\n");
    }
    output
}

pub(crate) fn command_job_id_from_response(response: &JsonRpcResponse) -> Option<&str> {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("structuredContent"))
        .and_then(|structured| structured.get("jobId"))
        .and_then(Value::as_str)
}

fn command_job_structured(tool_name: &str, snapshot: &CommandJobSnapshot) -> Value {
    let command_success = match snapshot.state {
        CommandJobState::Succeeded => Some(true),
        CommandJobState::Failed
        | CommandJobState::Cancelled
        | CommandJobState::TimedOut
        | CommandJobState::Abandoned
        | CommandJobState::Interrupted => Some(false),
        CommandJobState::Queued | CommandJobState::Running => None,
    };
    json!({
        "toolName": tool_name,
        "jobId": snapshot.job_id,
        "command": snapshot.command,
        "cwd": snapshot.cwd,
        "state": snapshot.state.as_str(),
        "elapsedMs": snapshot.elapsed_ms,
        "exitCode": snapshot.exit_code,
        "events": snapshot.events,
        "nextCursor": snapshot.next_cursor,
        "hasMoreOutput": snapshot.has_more_output,
        "outputTruncated": snapshot.output_truncated,
        "timeoutMs": snapshot.timeout_ms,
        "commandSuccess": command_success,
        "success": true,
    })
}

pub(crate) async fn handle_start_command(
    req: &JsonRpcRequest,
    workspace_root: &str,
    set_catdesk_as_co_author: bool,
    command_jobs: &CommandJobManager,
    show_detail_mode: ShowDetailMode,
    session_namespace: Option<&str>,
    active_project: Option<&Path>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let command_text = match required_string_argument(&arguments, "command") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    if command::contains_catdesk_co_author_marker(command_text) {
        let message = if set_catdesk_as_co_author {
            "Rewrite the commit message normally and remove \"Co-Authored-By: CatDesk\". CatDesk will add that trailer automatically."
        } else {
            "Do not include \"Co-Authored-By: CatDesk\" in the commit message. The user does not want that attribution."
        };
        return tool_error_response(req, message.into());
    }
    let cwd_input = match optional_string_argument(&arguments, "cwd") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let cwd = match resolve_effective_command_cwd(workspace_root, cwd_input, active_project) {
        Ok(path) => path,
        Err(error) => {
            return tool_error_response(
                req,
                format!("code: PATH_OUTSIDE_WORKSPACE\nmessage: {error}"),
            );
        }
    };
    let requested_timeout = match arguments.get("timeout") {
        Some(value) => match value.as_u64() {
            Some(value) => Some(value),
            None => {
                return tool_error_response(
                    req,
                    "Parameter timeout must be a positive integer".into(),
                );
            }
        },
        None => None,
    };
    let timeout_ms = match CommandJobManager::normalize_timeout(requested_timeout) {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let effective_command =
        if set_catdesk_as_co_author && command::command_contains_git_commit(command_text) {
            command::inject_catdesk_co_author_trailer(command_text)
        } else {
            command_text.to_string()
        };
    let request_key = session_namespace.and_then(|session_namespace| {
        req.id.as_ref().map(|id| {
            let mut session_hasher = DefaultHasher::new();
            session_namespace.hash(&mut session_hasher);
            let session_hash = session_hasher.finish();
            let mut args_hasher = DefaultHasher::new();
            effective_command.hash(&mut args_hasher);
            cwd.hash(&mut args_hasher);
            timeout_ms.hash(&mut args_hasher);
            format!(
                "start_command:{session_hash:016x}:{id}:{:016x}",
                args_hasher.finish()
            )
        })
    });
    let change_session = (show_detail_mode != ShowDetailMode::Disable).then(|| {
        ChangeSession::begin(
            Path::new(workspace_root),
            command_change_scope(workspace_root, &cwd),
        )
    });
    match command_jobs
        .start_with_change_session(
            effective_command,
            Path::new(workspace_root).to_path_buf(),
            cwd,
            timeout_ms,
            request_key,
            change_session,
            session_namespace,
        )
        .await
    {
        Ok(started) => {
            let mut structured = command_job_structured("start_command", &started.snapshot);
            if let Some(object) = structured.as_object_mut() {
                object.insert("deduplicated".to_string(), json!(started.deduplicated));
            }
            let text = if started.deduplicated {
                format!("Command job already exists: {}", started.snapshot.job_id)
            } else {
                format!("Started command job: {}", started.snapshot.job_id)
            };
            tool_success_response_with_structured(req, text, structured)
        }
        Err(error) => tool_error_response(req, error),
    }
}

#[cfg(test)]
pub(crate) async fn handle_poll_command(
    req: &JsonRpcRequest,
    command_jobs: &CommandJobManager,
) -> JsonRpcResponse {
    handle_poll_command_with_session(req, command_jobs, None).await
}

pub(crate) async fn handle_poll_command_with_session(
    req: &JsonRpcRequest,
    command_jobs: &CommandJobManager,
    session_namespace: Option<&str>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let job_id = match required_string_argument(&arguments, "job_id") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    let after = match arguments.get("after") {
        Some(value) => match value.as_u64() {
            Some(value) => value,
            None => {
                return tool_error_response(
                    req,
                    "Parameter after must be a non-negative integer".into(),
                );
            }
        },
        None => 0,
    };
    let wait_ms = match arguments.get("wait_ms") {
        Some(value) => match value.as_u64() {
            Some(value) if value <= MAX_POLL_WAIT_MS => value,
            Some(_) => {
                return tool_error_response(
                    req,
                    format!("wait_ms must be at most {MAX_POLL_WAIT_MS}"),
                );
            }
            None => {
                return tool_error_response(
                    req,
                    "Parameter wait_ms must be a non-negative integer".into(),
                );
            }
        },
        None => DEFAULT_POLL_WAIT_MS,
    };
    match command_jobs
        .poll_for_session(job_id, after, wait_ms, session_namespace)
        .await
    {
        Ok(snapshot) => {
            let text = command_job_output_text(&snapshot);
            let structured = command_job_structured("poll_command", &snapshot);
            tool_success_response_with_structured(req, text, structured)
        }
        Err(error) => tool_error_response(req, error),
    }
}

pub(crate) async fn handle_cancel_command_with_session(
    req: &JsonRpcRequest,
    command_jobs: &CommandJobManager,
    session_namespace: Option<&str>,
) -> JsonRpcResponse {
    let arguments = tool_arguments(req);
    let job_id = match required_string_argument(&arguments, "job_id") {
        Ok(value) => value,
        Err(error) => return tool_error_response(req, error),
    };
    match command_jobs
        .cancel_for_session(job_id, session_namespace)
        .await
    {
        Ok(snapshot) => {
            let text = format!(
                "Command job {} is {}",
                snapshot.job_id,
                snapshot.state.as_str()
            );
            let structured = command_job_structured("cancel_command", &snapshot);
            tool_success_response_with_structured(req, text, structured)
        }
        Err(error) => tool_error_response(req, error),
    }
}

pub(crate) async fn handle_run_command(
    req: &JsonRpcRequest,
    workspace_root: &str,
    set_catdesk_as_co_author: bool,
    active_project: Option<&Path>,
) -> JsonRpcResponse {
    let params = &req.params;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let cmd = match arguments.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return tool_error_response(req, "Missing required parameter: command".into());
        }
    };

    let cwd_input = arguments.get("cwd").and_then(|v| v.as_str());
    let timeout_ms = arguments.get("timeout").and_then(|v| v.as_u64());
    if let Some(timeout_ms) = timeout_ms {
        if timeout_ms == 0 {
            return tool_error_response(req, "timeout must be at least 1 ms".into());
        }
        if timeout_ms > command::MAX_TIMEOUT_MS {
            return tool_error_response(
                req,
                format!(
                    "run_command supports at most {} ms. Use start_command for builds, compilation, dependency installation, long test suites, development servers, or other long-running commands.",
                    command::MAX_TIMEOUT_MS
                ),
            );
        }
    }

    if command::contains_catdesk_co_author_marker(cmd) {
        let message = if set_catdesk_as_co_author {
            "Rewrite the commit message normally and remove \"Co-Authored-By: CatDesk\". CatDesk will add that trailer automatically."
        } else {
            "Do not include \"Co-Authored-By: CatDesk\" in the commit message. The user does not want that attribution."
        };
        return tool_error_response(req, message.into());
    }

    let cwd = match resolve_effective_command_cwd(workspace_root, cwd_input, active_project) {
        Ok(p) => p,
        Err(e) => {
            return tool_error_response(req, format!("code: PATH_OUTSIDE_WORKSPACE\nmessage: {e}"));
        }
    };

    let effective_timeout = command::clamp_timeout(timeout_ms);
    let effective_command = if set_catdesk_as_co_author && command::command_contains_git_commit(cmd)
    {
        command::inject_catdesk_co_author_trailer(cmd)
    } else {
        cmd.to_string()
    };

    if let Some(intercept) = command::detect_list_files_intercept(&effective_command) {
        let listing_path =
            match command::resolve_command_path(workspace_root, &cwd, intercept.path.as_deref()) {
                Ok(path) => path,
                Err(e) => {
                    return tool_error_response(
                        req,
                        format!("code: PATH_OUTSIDE_WORKSPACE\nmessage: {e}"),
                    );
                }
            };
        let listing_path_str = listing_path.to_string_lossy().to_string();
        match workspace_tools::list_files_filtered(
            workspace_root,
            Some(&listing_path_str),
            intercept.include_hidden,
            None,
            intercept.filter,
        ) {
            Ok(listing) => {
                let output = listing.render_text();
                let structured = build_run_command_listing_structured(
                    &effective_command,
                    &cwd,
                    &output,
                    intercept.source,
                    &listing,
                );
                return tool_success_response_with_structured(req, output, structured);
            }
            Err(e) => return tool_error_response(req, e),
        }
    }

    if let Some(intercept) = command::detect_move_path_intercept(&effective_command) {
        return handle_run_command_move_path_intercept(
            req,
            workspace_root,
            &effective_command,
            &cwd,
            &intercept,
        );
    }

    let result = command::run_command(
        &effective_command,
        Path::new(workspace_root),
        &cwd,
        effective_timeout,
    )
    .await;
    let output = command::format_result(&result);
    let structured = json!({
        "toolName": "run_command",
        "command": effective_command,
        "cwd": cwd.to_string_lossy().to_string(),
        "stdout": result.stdout,
        "stderr": result.stderr,
        "success": result.success,
        "exitCode": result.exit_code,
        "elapsedMs": result.elapsed_ms,
        "timedOut": result.timed_out,
        "stdoutTruncated": result.stdout_truncated,
        "stderrTruncated": result.stderr_truncated,
    });

    if result.success {
        tool_success_response_with_structured(req, output, structured)
    } else {
        tool_error_response_with_structured(req, output, structured)
    }
}

struct ResolvedMovePathIntercept {
    from: PathBuf,
    to: PathBuf,
    destination_operand: PathBuf,
    destination_operand_was_dir: bool,
}

fn resolve_intercepted_move_path(
    workspace_root: &str,
    cwd: &Path,
    intercept: &command::InterceptedMovePathRequest,
) -> Result<ResolvedMovePathIntercept, String> {
    let from = command::resolve_command_path(workspace_root, cwd, Some(&intercept.from))
        .map_err(|e| format!("code: PATH_OUTSIDE_WORKSPACE\nmessage: {e}"))?;
    let destination_operand =
        command::resolve_command_path(workspace_root, cwd, Some(&intercept.to))
            .map_err(|e| format!("code: PATH_OUTSIDE_WORKSPACE\nmessage: {e}"))?;

    let source_meta = std::fs::symlink_metadata(&from)
        .map_err(|_| format!("Source path not found: {}", from.display()))?;
    let destination_operand_was_dir = std::fs::symlink_metadata(&destination_operand)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false);
    let to = if destination_operand_was_dir {
        let file_name = from
            .file_name()
            .ok_or_else(|| format!("Source path has no file name: {}", from.display()))?;
        destination_operand.join(file_name)
    } else {
        destination_operand.clone()
    };

    if intercept.overwrite && from != to {
        if let Ok(destination_meta) = std::fs::symlink_metadata(&to) {
            if source_meta.file_type().is_dir() || destination_meta.file_type().is_dir() {
                return Err(format!(
                    "mv intercept refuses to overwrite existing directories: {}",
                    to.display()
                ));
            }
        }
    }

    Ok(ResolvedMovePathIntercept {
        from,
        to,
        destination_operand,
        destination_operand_was_dir,
    })
}

fn handle_run_command_move_path_intercept(
    req: &JsonRpcRequest,
    workspace_root: &str,
    command_text: &str,
    cwd: &Path,
    intercept: &command::InterceptedMovePathRequest,
) -> JsonRpcResponse {
    let resolved = match resolve_intercepted_move_path(workspace_root, cwd, intercept) {
        Ok(resolved) => resolved,
        Err(error) => return tool_error_response(req, error),
    };

    if !intercept.overwrite && resolved.to.exists() {
        let output = format!(
            "skipped move because destination exists: {}",
            resolved.to.display()
        );
        let structured = build_run_command_move_path_structured(
            workspace_root,
            command_text,
            cwd,
            intercept,
            &resolved,
            &output,
            "",
            true,
            true,
        );
        return tool_success_response_with_structured(req, output, structured);
    }

    let from = resolved.from.to_string_lossy().to_string();
    let to = resolved.to.to_string_lossy().to_string();
    match workspace_tools::move_path(workspace_root, &from, &to, intercept.overwrite, false) {
        Ok(output) => {
            let structured = build_run_command_move_path_structured(
                workspace_root,
                command_text,
                cwd,
                intercept,
                &resolved,
                &output,
                "",
                true,
                false,
            );
            tool_success_response_with_structured(req, output, structured)
        }
        Err(error) => {
            let structured = build_run_command_move_path_structured(
                workspace_root,
                command_text,
                cwd,
                intercept,
                &resolved,
                "",
                &error,
                false,
                false,
            );
            tool_error_response_with_structured(req, error, structured)
        }
    }
}

fn to_relative(root: &Path, path: &Path) -> String {
    let value = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    #[cfg(windows)]
    {
        value.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn build_run_command_move_path_structured(
    workspace_root: &str,
    command_text: &str,
    cwd: &Path,
    intercept: &command::InterceptedMovePathRequest,
    resolved: &ResolvedMovePathIntercept,
    stdout: &str,
    stderr: &str,
    success: bool,
    skipped: bool,
) -> Value {
    let root = Path::new(workspace_root)
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .unwrap_or_else(|_| PathBuf::from(workspace_root));
    json!({
        "toolName": "run_command",
        "interceptedToolName": "move_path",
        "command": command_text,
        "cwd": cwd.to_string_lossy().to_string(),
        "stdout": stdout,
        "stderr": stderr,
        "success": success,
        "from": intercept.from.as_str(),
        "to": intercept.to.as_str(),
        "resolvedFrom": to_relative(&root, &resolved.from),
        "resolvedTo": to_relative(&root, &resolved.to),
        "destinationOperand": to_relative(&root, &resolved.destination_operand),
        "destinationOperandWasDirectory": resolved.destination_operand_was_dir,
        "overwrite": intercept.overwrite,
        "skipped": skipped,
    })
}

fn build_run_command_listing_structured(
    command_text: &str,
    cwd: &Path,
    stdout: &str,
    source: command::ListFilesInterceptSource,
    listing: &workspace_tools::ListFilesOutput,
) -> Value {
    json!({
        "toolName": "run_command",
        "interceptedToolName": "list_files",
        "interceptedCommandName": source.as_str(),
        "command": command_text,
        "cwd": cwd.to_string_lossy().to_string(),
        "stdout": stdout,
        "stderr": "",
        "success": true,
        "listPath": listing.path,
        "listItemCount": listing.item_count,
        "listDirectoryCount": listing.directory_count,
        "listFileCount": listing.file_count,
        "listOtherCount": listing.other_count,
        "listTruncated": listing.truncated,
        "listLimit": listing.limit,
        "listEntries": listing.entries,
    })
}


fn resolve_effective_command_cwd(
    workspace_root: &str,
    cwd_input: Option<&str>,
    active_project: Option<&Path>,
) -> Result<PathBuf, String> {
    let explicit_cwd = match cwd_input {
        Some(cwd) => Some(command::resolve_workspace_path(workspace_root, Some(cwd))?),
        None => None,
    };
    project_scope::select_effective_cwd(
        Path::new(workspace_root),
        explicit_cwd,
        active_project,
    )
    .map(|(cwd, _)| cwd)
}

fn command_change_scope(workspace_root: &str, cwd: &Path) -> ChangeScope {
    match project_scope::command_change_tracking_root(Path::new(workspace_root), cwd) {
        Ok(Some(project_root)) => {
            ChangeScope::single(ChangeTarget::discovered(project_root, true))
        }
        Ok(None) | Err(_) => ChangeScope::none(),
    }
}

pub(crate) fn change_scope_for_request(
    req: &JsonRpcRequest,
    workspace_root: &str,
    active_project: Option<&Path>,
) -> ChangeScope {
    let tool_name = tool_name_from_request(req);
    let arguments = tool_arguments(req);

    let resolve = |path: Option<&str>| {
        path.and_then(|value| command::resolve_workspace_path(workspace_root, Some(value)).ok())
    };

    match tool_name.as_str() {
        "write" | "edit" => resolve(arguments.get("path").and_then(Value::as_str))
            .map(|path| ChangeScope::single(ChangeTarget::explicit(path, false)))
            .unwrap_or_else(ChangeScope::none),
        "create_handoff" => ChangeScope::none(),
        "delete" => resolve(arguments.get("path").and_then(Value::as_str))
            .map(|path| ChangeScope::single(ChangeTarget::explicit(path, true)))
            .unwrap_or_else(ChangeScope::none),
        "run_command" => {
            let command_text = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if command::detect_list_files_intercept(command_text).is_some() {
                return ChangeScope::none();
            }

            if let Some(intercept) = command::detect_move_path_intercept(command_text) {
                let Ok(cwd) = resolve_effective_command_cwd(
                    workspace_root,
                    arguments.get("cwd").and_then(Value::as_str),
                    active_project,
                ) else {
                    return ChangeScope::none();
                };
                let Ok(resolved) = resolve_intercepted_move_path(workspace_root, &cwd, &intercept)
                else {
                    return ChangeScope::none();
                };
                return ChangeScope::many(vec![
                    ChangeTarget::explicit(resolved.from, true),
                    ChangeTarget::explicit(resolved.to, true),
                ]);
            }

            resolve_effective_command_cwd(
                workspace_root,
                arguments.get("cwd").and_then(Value::as_str),
                active_project,
            )
            .ok()
            .map(|cwd| command_change_scope(workspace_root, &cwd))
            .unwrap_or_else(ChangeScope::none)
        }
        _ => ChangeScope::none(),
    }
}


pub(crate) async fn fetch_devtools_tools(bridge: &Arc<Mutex<DevtoolsBridge>>) -> Option<Vec<Value>> {
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": "dt-tools-list",
        "method": "tools/list",
        "params": {}
    });
    let resp = DevtoolsBridge::call(bridge, &list_req).await.ok()?;
    let dt_tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array)?
        .to_vec();
    Some(dt_tools)
}

async fn devtools_tool_is_read_only(
    bridge: &Arc<Mutex<DevtoolsBridge>>,
    tool_name: &str,
) -> Option<bool> {
    let dt_tools = fetch_devtools_tools(bridge).await?;
    dt_tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
        .map(tool_is_read_only)
}

