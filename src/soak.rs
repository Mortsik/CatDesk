//! Reproducible in-suite soak scenarios for production failure modes.
//!
//! These scenarios cover the shapes the 43k epic was opened for: long
//! requests that outlive their response deadline, many simultaneous tool
//! calls, client disconnects during side-effectful work, and tunnel-level
//! disruption. Every scenario asserts the failure budget of bead
//! catdesk-43k.3: each induced failure carries the expected terminal
//! classification (via `request_lifecycle::classify_stream_failure`), every
//! started request has exactly one terminal record, and the lifecycle
//! registry is empty afterwards — no leaked active-request state.
//!
//! Real 45/60/120-second deadlines cannot run inline in the fast suite, so
//! the deadline scenarios shorten only the response deadline through a
//! per-router request-extension override (see `post_mcp_http`); the
//! production defaults stay pinned by
//! `production_response_deadlines_stay_at_the_documented_defaults`. True-
//! duration scenarios, and the tunnel path that needs a live tunnel, run in
//! `ops/soak-real-duration.sh`.

use crate::diagnostics::{Diagnostics, Guard};
use crate::request_lifecycle::classify_stream_failure;
use axum::{extract::Request, middleware::Next};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::channel;

/// Poll budget for liveness waits; generous enough for a loaded CI runner
/// while staying far below any production deadline being modeled.
const DRAIN_BUDGET_MS: u64 = 10_000;
const DRAIN_POLL_MS: u64 = 25;

async fn spawn_soak_server(
    root: &Path,
    response_deadline: Option<Duration>,
) -> (
    String,
    crate::command_jobs::CommandJobManager,
    Diagnostics,
    Guard,
    tokio::task::JoinHandle<()>,
) {
    use crate::{command_jobs::CommandJobManager, state::AppState};
    use tokio::sync::Mutex;
    let (log, guard) = Diagnostics::start(&root.join("logs")).unwrap();
    let state = AppState::new_for_test(
        0,
        root.to_string_lossy().into_owned(),
        root.join("config.toml"),
    )
    .unwrap();
    let (events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
    let command_jobs = CommandJobManager::new();
    let app = crate::server::router(
        std::sync::Arc::new(Mutex::new(state)),
        None,
        command_jobs.clone(),
        "/secret-slug/mcp".into(),
        events,
    );
    // Soak-only layer: publish this router's shortened response deadline as a
    // request extension read by `post_mcp_http`. Production routers never
    // install it, so their deadlines always come from `request_deadline`.
    let app = match response_deadline {
        Some(deadline) => app.layer(axum::middleware::from_fn(
            move |mut request: Request, next: Next| async move {
                request.extensions_mut().insert(deadline);
                next.run(request).await
            },
        )),
        None => app,
    };
    let app = app.route_layer(axum::middleware::from_fn_with_state(
        Some(log.clone()),
        crate::diagnostics::http_request,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, command_jobs, log, guard, server)
}

/// Start a background job directly on the manager, bypassing HTTP. Scenarios
/// with a shortened response deadline use this for their setup so the
/// (process-class) start_command call is not itself exposed to the shortened
/// deadline — in debug builds its real work can exceed the soak override,
/// which is about the poll path, not about job creation.
async fn start_job_on_manager(
    manager: &crate::command_jobs::CommandJobManager,
    root: &Path,
    command: &str,
) -> String {
    let started = manager
        .start(command.to_string(), root.to_path_buf(), 60_000, None)
        .await
        .expect("manager job start must succeed");
    started.snapshot.job_id
}

async fn post_tools_call_raw(
    client: &reqwest::Client,
    base: &str,
    tool: &'static str,
    arguments: Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/secret-slug/mcp"))
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("MCP-Method", "tools/call")
        .header("Mcp-Name", tool)
        .json(
            &json!({"jsonrpc": "2.0", "id": "soak-id", "method": "tools/call",
            "params": {"name": tool, "arguments": arguments, "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}}}}),
        )
        .send()
        .await
        .expect("soak tool call request must complete")
}

async fn post_tools_call(
    client: &reqwest::Client,
    base: &str,
    tool: &'static str,
    arguments: Value,
) -> Value {
    post_tools_call_raw(client, base, tool, arguments)
        .await
        .json::<Value>()
        .await
        .expect("soak tool call response must be JSON")
}

/// Pay the process-global catdesk_instruction warm-up (tokenizer, first
/// widget build) against a router with production deadlines. Without this,
/// the first gate call on a deadline-shortened router can exceed the soak
/// override (about 2.5 s cold in debug builds) and pollute the scenario's
/// failure budget with its own deadline_timeout record.
async fn warm_up_instruction_path(root: &Path) {
    let warmup_root = root.join("warmup");
    std::fs::create_dir_all(&warmup_root).unwrap();
    let (base, _manager, _log, _guard, server) = spawn_soak_server(&warmup_root, None).await;
    let client = reqwest::Client::new();
    let response = post_tools_call_raw(&client, &base, "catdesk_instruction", json!({})).await;
    assert_eq!(
        response.status().as_u16(),
        200,
        "the warm-up gate call must succeed under production deadlines"
    );
    server.abort();
    let _ = server.await;
}

/// Open the anonymous instruction gate so subsequent tool calls are accepted.
/// The process-global warm-up is paid by [`warm_up_instruction_path`]; the
/// retry loop is a defensive net, not the expected path.
async fn open_instruction_gate(client: &reqwest::Client, base: &str) {
    let mut last: Option<(reqwest::StatusCode, Value)> = None;
    for _ in 0..6 {
        let response = post_tools_call_raw(client, base, "catdesk_instruction", json!({})).await;
        let status = response.status();
        let instruction = response.json::<Value>().await.expect("gate JSON");
        if status.as_u16() == 200
            && instruction["result"]["structuredContent"]["errorCode"].is_null()
        {
            return;
        }
        last = Some((status, instruction));
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("catdesk_instruction never opened the session gate: {last:?}");
}

async fn start_job(client: &reqwest::Client, base: &str, command: &str) -> String {
    let start = post_tools_call(client, base, "start_command", json!({"command": command})).await;
    start["result"]["structuredContent"]["jobId"]
        .as_str()
        .expect("start_command must return a jobId")
        .to_string()
}

/// Poll a job until it reports a terminal state, then assert it succeeded.
/// Bridges the microseconds between a side effect landing on disk and the
/// job's terminal transition, so a fast follow-up poll never races "running".
async fn await_job_succeeded(
    client: &reqwest::Client,
    base: &str,
    job_id: &str,
    wait_ms: u64,
    attempts: usize,
) {
    for attempt in 0..attempts {
        let snapshot = post_tools_call(
            client,
            base,
            "poll_command",
            json!({"job_id": job_id, "wait_ms": wait_ms}),
        )
        .await;
        let state = snapshot["result"]["structuredContent"]["state"]
            .as_str()
            .unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "timed_out" | "abandoned" | "interrupted"
        ) {
            assert_eq!(
                state, "succeeded",
                "job {job_id} ended as {state}: {snapshot}"
            );
            return;
        }
        assert!(attempt + 1 < attempts, "job {job_id} never terminated");
    }
    unreachable!("the assertion above fires before attempts are exhausted");
}

/// Failure-budget gate: the lifecycle registry must be empty. The registry is
/// updated synchronously when a request future completes or is dropped, so
/// this proves the server retired every request without waiting for disk.
async fn assert_registry_drained(log: &Diagnostics) {
    let mut remaining = DRAIN_BUDGET_MS / DRAIN_POLL_MS;
    loop {
        let view = log.active_requests_view();
        if view.is_empty() {
            return;
        }
        assert!(remaining > 0, "leaked active requests: {view:?}");
        remaining -= 1;
        tokio::time::sleep(Duration::from_millis(DRAIN_POLL_MS)).await;
    }
}

/// Wait until `path` exists with exactly `expected` as its trimmed content.
async fn await_file_content(path: &Path, expected: &str) {
    let mut remaining = DRAIN_BUDGET_MS / DRAIN_POLL_MS;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.trim() == expected {
                return;
            }
        }
        assert!(
            remaining > 0,
            "side effect never landed at {}: the long command did not finish",
            path.display()
        );
        remaining -= 1;
        tokio::time::sleep(Duration::from_millis(DRAIN_POLL_MS)).await;
    }
}

/// Read and parse the scenario's drained connection log.
fn read_records(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("logs/connections.jsonl"))
        .expect("soak diagnostics log must exist")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Failure-budget gate: every `http_started` request has exactly one terminal
/// record (`http_finished` or `http_cancelled`), no duplicates, none missing.
fn assert_every_request_terminated_exactly_once(records: &[Value]) {
    let mut started_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut terminals_by_id: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for record in records {
        match record["event"].as_str().unwrap_or_default() {
            "http_started" => {
                assert!(
                    started_ids.insert(record["request_id"].as_str().unwrap_or_default()),
                    "duplicate http_started for {}",
                    record["request_id"]
                );
            }
            "http_finished" | "http_cancelled" => {
                *terminals_by_id
                    .entry(
                        record["request_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    )
                    .or_default() += 1;
            }
            _ => {}
        }
    }
    let duplicated: Vec<_> = terminals_by_id
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(id, _)| id.clone())
        .collect();
    assert!(
        duplicated.is_empty(),
        "requests with more than one terminal record: {duplicated:?}"
    );
    assert_eq!(
        started_ids.len(),
        terminals_by_id.len(),
        "every started request must have exactly one terminal record"
    );
}

fn terminals(records: &[Value], event: &str, reason: &str) -> Vec<Value> {
    records
        .iter()
        .filter(|record| record["event"] == event && record["terminal_reason"] == reason)
        .cloned()
        .collect()
}

fn timestamp_ms(record: &Value) -> u64 {
    record["timestamp_ms"].as_u64().expect("record timestamp")
}

/// A long request whose blocking work outlives the response deadline: the
/// client receives the 504 `deadline_timeout` classification while the
/// side-effectful command keeps running, lands its side effect, and stays
/// pollable — a timeout never proves the work stopped.
#[tokio::test]
async fn soak_long_request_deadline_leaves_surviving_pollable_work() {
    let root = std::env::temp_dir().join(format!("catdesk-soak-deadline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let side_effect = root.join("side-effect.txt");
    warm_up_instruction_path(&root).await;
    // Deadline far below any production policy value. Only the poll path is
    // exposed to it; the job is created directly on the manager.
    let (base, manager, log, guard, server) =
        spawn_soak_server(&root, Some(Duration::from_millis(1_500))).await;
    let client = reqwest::Client::new();
    open_instruction_gate(&client, &base).await;

    let job_id =
        start_job_on_manager(&manager, &root, "sleep 2 && echo ok > side-effect.txt").await;
    let poll = {
        let client = client.clone();
        let base = base.clone();
        let job_id = job_id.clone();
        tokio::spawn(async move {
            let response = post_tools_call_raw(
                &client,
                &base,
                "poll_command",
                json!({"job_id": job_id, "wait_ms": 8_000}),
            )
            .await;
            let status = response.status();
            let body = response
                .json::<Value>()
                .await
                .expect("poll response must be JSON");
            (status, body)
        })
        .await
        .expect("poll task panicked")
    };
    assert_eq!(
        poll.0.as_u16(),
        504,
        "the long poll must hit the response deadline: {}",
        poll.1
    );
    assert_eq!(poll.1["error"]["code"], -32000);

    assert_registry_drained(&log).await;
    // The work already started cannot be cancelled: it finishes, its side
    // effect lands, and the job remains pollable past the HTTP deadline.
    await_file_content(&side_effect, "ok").await;
    await_job_succeeded(&client, &base, &job_id, 500, 8).await;
    assert_registry_drained(&log).await;

    server.abort();
    let _ = server.await;
    drop(guard);
    let records = read_records(&root);
    assert_every_request_terminated_exactly_once(&records);
    let timeouts = terminals(&records, "http_finished", "deadline_timeout");
    assert_eq!(timeouts.len(), 1, "exactly the long poll times out");
    assert_eq!(timeouts[0]["status"], 504);
    assert_eq!(timeouts[0]["stage"], "completed");
    assert_eq!(timeouts[0]["scheduler_class"], "control");
    assert_eq!(timeouts[0]["scheduler_deadline_stage"], "execution");
    assert!(
        timeouts[0]["elapsed_ms"].as_u64().unwrap() >= 1_400,
        "the deadline must have been the shortened policy, not a fast failure"
    );
    // The lifecycle classifier attributes the induced failure server-side.
    let at_ms = timestamp_ms(&timeouts[0]);
    let correlation = classify_stream_failure(&records, at_ms, 10_000);
    assert_eq!(correlation.verdict(), "catdesk_timeout");
    assert_eq!(
        correlation.deadline_timeouts,
        vec![
            timeouts[0]["request_id"]
                .as_str()
                .expect("timeout request id")
                .to_string()
        ]
    );
    assert!(
        correlation.active_at_failure.is_empty(),
        "no request may still be in flight once the budget is checked"
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// Many simultaneous chats and tool calls across scheduler classes: every
/// request completes with its own correlated lifecycle record, concurrency is
/// visible in the registry, and nothing leaks.
#[tokio::test]
async fn soak_concurrent_tool_calls_across_scheduler_classes_stay_correlated() {
    let root =
        std::env::temp_dir().join(format!("catdesk-soak-concurrent-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("notes.txt"), "soak\n").unwrap();
    let (base, _manager, log, guard, server) = spawn_soak_server(&root, None).await;
    let client = reqwest::Client::new();
    open_instruction_gate(&client, &base).await;

    let mut jobs = Vec::new();
    for _ in 0..4 {
        jobs.push(start_job(&client, &base, "sleep 2").await);
    }

    // 16 blocking polls plus fast filesystem and control calls, all in
    // flight at once. Production deadlines apply unchanged.
    let mut tasks = Vec::new();
    for index in 0..24 {
        let client = client.clone();
        let base = base.clone();
        let job_id = jobs[index % jobs.len()].clone();
        tasks.push(tokio::spawn(async move {
            match index % 4 {
                0 | 1 => (
                    "poll",
                    post_tools_call_raw(
                        &client,
                        &base,
                        "poll_command",
                        json!({"job_id": job_id, "wait_ms": 1_500}),
                    )
                    .await,
                ),
                2 => (
                    "read",
                    post_tools_call_raw(&client, &base, "read", json!({"paths": ["notes.txt"]}))
                        .await,
                ),
                _ => (
                    "poll-missing",
                    post_tools_call_raw(
                        &client,
                        &base,
                        "poll_command",
                        json!({"job_id": "soak-missing-job", "wait_ms": 0}),
                    )
                    .await,
                ),
            }
        }));
    }
    let mut responses = Vec::new();
    for task in tasks {
        responses.push(task.await.expect("concurrent soak task panicked"));
    }
    for (kind, response) in &responses {
        assert_eq!(response.status().as_u16(), 200, "{kind} call failed");
    }
    assert_registry_drained(&log).await;

    server.abort();
    let _ = server.await;
    drop(guard);
    let records = read_records(&root);
    assert_every_request_terminated_exactly_once(&records);
    let completed = terminals(&records, "http_finished", "completed");
    // Gate + 4 starts + 24 concurrent calls.
    assert_eq!(completed.len(), 29, "every soak request completes");
    assert_eq!(
        terminals(&records, "http_finished", "deadline_timeout").len()
            + terminals(&records, "http_finished", "worker_failed").len()
            + terminals(&records, "http_cancelled", "client_disconnect").len()
            + terminals(&records, "http_cancelled", "server_shutdown").len(),
        0,
        "a healthy concurrency soak admits no failure"
    );
    let classes: Vec<_> = completed
        .iter()
        .filter_map(|record| record["scheduler_class"].as_str())
        .collect();
    for expected in ["control", "process", "filesystem"] {
        assert!(
            classes.contains(&expected),
            "scheduler class {expected} missing from {classes:?}"
        );
    }
    let peak = records
        .iter()
        .filter(|record| record["event"] == "http_started")
        .filter_map(|record| record["active_requests"].as_u64())
        .max()
        .unwrap_or(0);
    assert!(
        peak >= 8,
        "the concurrent polls must genuinely overlap in the lifecycle registry (peak {peak})"
    );
    let at_ms = timestamp_ms(completed.last().unwrap());
    let correlation = classify_stream_failure(&records, at_ms, 120_000);
    assert_eq!(correlation.verdict(), "no_catdesk_failure");
    assert!(correlation.active_at_failure.is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

/// A client that disconnects while side-effectful work executes: the drop is
/// classified as `client_disconnect`, the work survives the disconnect, and
/// the job is still pollable afterwards.
#[tokio::test]
async fn soak_client_disconnect_during_side_effectful_work_survives() {
    // The job's `sleep` is resolved through the process-global `PATH` at
    // spawn time, and the linux_sandbox tests rewrite `PATH` to their stubs
    // while they run; an interleaved rewrite would make the long command
    // vanish (ENOENT) and fail as "did not finish". Hold the env lock so the
    // spawn never interleaves with an env rewrite. #[tokio::test] is
    // current-thread, so holding a std MutexGuard across awaits cannot
    // deadlock (same idiom as mcp/tests.rs).
    let _env = crate::test_serialization::lock_env();
    let root =
        std::env::temp_dir().join(format!("catdesk-soak-disconnect-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let side_effect = root.join("disconnect-effect.txt");
    let (base, _manager, log, guard, server) = spawn_soak_server(&root, None).await;
    let client = reqwest::Client::new();
    open_instruction_gate(&client, &base).await;

    let job_id = start_job(
        &client,
        &base,
        "sleep 1 && echo survived > disconnect-effect.txt",
    )
    .await;
    let poll = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        let job_id = job_id.clone();
        async move {
            post_tools_call_raw(
                &client,
                &base,
                "poll_command",
                json!({"job_id": job_id, "wait_ms": 5_000}),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    poll.abort();

    assert_registry_drained(&log).await;
    await_file_content(&side_effect, "survived").await;
    await_job_succeeded(&client, &base, &job_id, 2_000, 4).await;
    assert_registry_drained(&log).await;

    server.abort();
    let _ = server.await;
    drop(guard);
    let records = read_records(&root);
    assert_every_request_terminated_exactly_once(&records);
    let cancelled = terminals(&records, "http_cancelled", "client_disconnect");
    assert_eq!(cancelled.len(), 1, "only the dropped poll is cancelled");
    assert_eq!(cancelled[0]["stage"], "cancelled");
    assert!(cancelled[0]["elapsed_ms"].as_u64().unwrap() < 10_000);
    let at_ms = timestamp_ms(&cancelled[0]);
    let correlation = classify_stream_failure(&records, at_ms, 10_000);
    assert_eq!(correlation.verdict(), "client_cancellation");
    assert!(correlation.active_at_failure.is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

/// Mixed failure storm: concurrent deadline timeouts, client disconnects and
/// successful calls. Each induced failure must keep its own classification
/// under load, every request gets exactly one terminal, and nothing leaks.
#[tokio::test]
async fn soak_failure_storm_keeps_attributions_separated() {
    let root = std::env::temp_dir().join(format!("catdesk-soak-storm-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    warm_up_instruction_path(&root).await;
    let (base, manager, log, guard, server) =
        spawn_soak_server(&root, Some(Duration::from_millis(1_200))).await;
    let client = reqwest::Client::new();
    open_instruction_gate(&client, &base).await;
    // The job is created directly on the manager so only the poll paths are
    // exposed to the shortened deadline.
    let job_id =
        start_job_on_manager(&manager, &root, "sleep 3 && echo storm > storm-effect.txt").await;

    let mut tasks = Vec::new();
    for index in 0..14 {
        let client = client.clone();
        let base = base.clone();
        let job_id = job_id.clone();
        tasks.push(tokio::spawn(async move {
            let blocking = matches!(index % 7, 0..=3);
            let response = if blocking {
                // Blocking polls that will exceed the shortened deadline.
                post_tools_call_raw(
                    &client,
                    &base,
                    "poll_command",
                    json!({"job_id": job_id, "wait_ms": 2_600}),
                )
                .await
            } else {
                post_tools_call_raw(
                    &client,
                    &base,
                    "poll_command",
                    json!({"job_id": "soak-missing-job", "wait_ms": 0}),
                )
                .await
            };
            (blocking, response)
        }));
    }
    // Four more long polls, aborted by their client mid-request well before
    // the 1200 ms deadline can expire (the last abort lands at ~800 ms).
    for _ in 0..4 {
        let client = client.clone();
        let base = base.clone();
        let job_id = job_id.clone();
        let dropped = tokio::spawn(async move {
            post_tools_call_raw(
                &client,
                &base,
                "poll_command",
                json!({"job_id": job_id, "wait_ms": 2_600}),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        dropped.abort();
    }
    for task in tasks {
        let (blocking, response) = task.await.expect("storm task panicked");
        let status = response.status().as_u16();
        if blocking {
            assert_eq!(status, 504, "a blocking poll must hit the deadline");
        } else {
            assert_eq!(status, 200, "a fast call must not hit the deadline");
        }
    }
    assert_registry_drained(&log).await;

    server.abort();
    let _ = server.await;
    drop(guard);
    let records = read_records(&root);
    assert_every_request_terminated_exactly_once(&records);

    let timeouts = terminals(&records, "http_finished", "deadline_timeout");
    let disconnects = terminals(&records, "http_cancelled", "client_disconnect");
    // Eight blocking polls run to the shortened deadline (indices 0-3 and
    // 7-10); the four aborted polls never reach it. Fast calls, gate and
    // start complete normally.
    assert_eq!(
        timeouts.len(),
        8,
        "exactly the non-aborted blocking polls time out"
    );
    assert_eq!(
        disconnects.len(),
        4,
        "exactly the aborted polls are cancelled"
    );
    for record in &timeouts {
        assert_eq!(record["status"], 504);
        assert_eq!(record["scheduler_deadline_stage"], "execution");
    }
    let completed = terminals(&records, "http_finished", "completed");
    assert_eq!(
        completed.len(),
        7,
        "the gate and the six fast calls complete"
    );
    // The ranked verdict prefers the CatDesk-attributable timeout, while the
    // full correlation keeps both failure buckets separated.
    let at_ms = timestamp_ms(timeouts.last().unwrap());
    let correlation = classify_stream_failure(&records, at_ms, 15_000);
    assert_eq!(correlation.verdict(), "catdesk_timeout");
    assert_eq!(correlation.deadline_timeouts.len(), 8);
    assert_eq!(correlation.client_cancellations.len(), 4);
    assert!(correlation.active_at_failure.is_empty());
    // Let the detached blocking polls (wait 2600 ms) and the job (sleep 3)
    // finish before the per-test runtime shuts down, so no blocking task is
    // still inside runtime.block_on when the runtime ends.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    std::fs::remove_dir_all(root).unwrap();
}

/// Tunnel-level disruption: the ngrok supervisor needs a live session, so the
/// end-to-end path runs in `ops/soak-real-duration.sh`. In-suite, the
/// classifier's tunnel branch is asserted over the exact record shapes the
/// supervisor emits around a mid-stream failure and reconnect cycle.
#[test]
fn soak_tunnel_reconnect_window_classifies_as_tunnel_event() {
    let records = vec![
        json!({"event": "http_started", "request_id": "in-flight", "timestamp_ms": 9_000}),
        json!({"event": "tunnel_started", "timestamp_ms": 9_100}),
        json!({"event": "tunnel_failed", "timestamp_ms": 9_400}),
        json!({"event": "tunnel_reconnect_waiting", "timestamp_ms": 9_500}),
        json!({"event": "tunnel_reconnect_attempt", "timestamp_ms": 9_600}),
        json!({"event": "http_cancelled", "request_id": "dropped", "timestamp_ms": 9_700,
            "terminal_reason": "client_disconnect"}),
    ];
    let correlation = classify_stream_failure(&records, 9_800, 5_000);
    assert_eq!(correlation.verdict(), "tunnel_event");
    // Every tunnel_* event inside the window is captured, including the
    // healthy start that preceded the failure.
    assert_eq!(
        correlation.tunnel_events,
        [
            "tunnel_started",
            "tunnel_failed",
            "tunnel_reconnect_waiting",
            "tunnel_reconnect_attempt"
        ]
    );
    // The transport suspect outranks the generic disconnect symptom, but the
    // full struct keeps both buckets plus the still-in-flight request.
    assert_eq!(correlation.client_cancellations, ["dropped"]);
    assert_eq!(
        correlation
            .active_at_failure
            .iter()
            .map(|active| active.request_id.as_str())
            .collect::<Vec<_>>(),
        ["in-flight"],
        "a request opened before the failure is the leading stall indicator"
    );
    // Once the supervisor reconnects and traffic completes normally, a later
    // stream failure in a quiet window has no CatDesk-side explanation.
    let quiet = vec![
        json!({"event": "tunnel_reconnected", "timestamp_ms": 12_000}),
        json!({"event": "http_started", "request_id": "done", "timestamp_ms": 20_000}),
        json!({"event": "http_finished", "request_id": "done", "timestamp_ms": 20_100,
            "status": 200, "terminal_reason": "completed"}),
    ];
    let correlation = classify_stream_failure(&quiet, 30_000, 3_000);
    assert_eq!(correlation.verdict(), "no_catdesk_failure");
}
