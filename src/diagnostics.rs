//! Bounded, metadata-only connection diagnostics. Never persist MCP payloads.

use crate::request_lifecycle::{RequestStage, TerminalReason};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn process_started_record_carries_version_and_identity_line() {
        let record = process_started_record();
        assert_eq!(
            record.get("event").and_then(Value::as_str),
            Some("process_started")
        );
        assert_eq!(
            record.get("version").and_then(Value::as_str),
            Some(crate::build_info::VERSION)
        );
        assert_eq!(
            record.get("build").and_then(Value::as_str),
            Some(
                crate::build_info::identity_line(
                    crate::build_info::VERSION,
                    crate::build_info::GIT_SHA,
                    crate::build_info::GIT_BRANCH,
                    crate::build_info::BUILD_TIMESTAMP,
                )
                .as_str()
            )
        );
        // Identity fields only here; per-request records keep their shape.
        assert!(!record.to_string().contains("rpc_method"));
    }

    #[test]
    fn request_metadata_does_not_persist_client_secrets() {
        let value = request_metadata(&json!({
            "id": "secret-client-id",
            "method": "tools/call",
            "params": {"name": "secret-tool-name", "arguments": {
                "command": "secret-command", "token": "secret-token"
            }}
        }));
        assert_eq!(value["rpc_method"], "tools/call");
        assert_eq!(value["rpc_tool"], "other");
        assert_eq!(
            request_metadata(&json!({"method":"tools/call", "params":{"name":"start_command"}}))["rpc_tool"],
            "start_command"
        );
        assert_eq!(
            request_metadata(&json!({"method":"tools/call", "params":{"name":"read_result"}}))["rpc_tool"],
            "read_result"
        );
        assert!(!value.to_string().contains("secret"));
        let unknown = request_metadata(&json!({"method": "secret-method"}));
        assert_eq!(unknown["rpc_method"], "other");
        assert!(!unknown.to_string().contains("secret"));
        assert_eq!(
            request_metadata(&json!({"method": "initialize"}))["rpc_method"],
            "initialize"
        );
    }

    #[test]
    fn request_metadata_whitelist_stays_in_sync_with_perf_metrics_tool_slots() {
        // Direction 1: every perf-metrics tool slot (except the trailing
        // "other") must survive the diagnostics whitelist; otherwise the tool
        // is silently counted as "other" in the connection log.
        for name in &crate::perf_metrics::TOOLS[..crate::perf_metrics::TOOL_OTHER] {
            let metadata = request_metadata(&json!({
                "method": "tools/call",
                "params": {"name": name}
            }));
            assert_eq!(
                metadata["rpc_tool"],
                json!(name),
                "perf-metrics tool slot {name:?} is redacted to \"other\" by the \
                 request_metadata whitelist; add it to RPC_TOOL_WHITELIST in \
                 src/diagnostics.rs so diagnostics and perf metrics agree"
            );
        }

        // Direction 2: every whitelisted name must own a real perf-metrics
        // slot; otherwise diagnostics records a name perf metrics counts as
        // "other".
        for name in RPC_TOOL_WHITELIST {
            let slot = crate::perf_metrics::tool_index(Some(name));
            assert_ne!(
                slot,
                crate::perf_metrics::TOOL_OTHER,
                "request_metadata whitelists {name:?} but perf_metrics::TOOLS has \
                 no slot for it; add it to TOOLS in src/perf_metrics.rs"
            );
            assert_eq!(
                crate::perf_metrics::tool_name(slot),
                name,
                "whitelisted tool {name:?} resolves to a perf-metrics slot whose \
                 canonical name differs"
            );
        }

        // Load-bearing slot indices other code and exported metrics rely on.
        assert_eq!(crate::perf_metrics::TOOLS[5], "read");
        assert_eq!(crate::perf_metrics::TOOLS[11], "create_handoff");
        assert_eq!(
            crate::perf_metrics::TOOLS[crate::perf_metrics::TOOL_OTHER],
            "other"
        );
    }

    #[test]
    fn request_metadata_records_only_safe_requested_timing_values() {
        let poll = request_metadata(&json!({
            "method": "tools/call",
            "params": {
                "name": "poll_command",
                "arguments": {
                    "job_id": "secret-job-id",
                    "wait_ms": 15_000,
                    "command": "secret-command"
                }
            }
        }));
        assert_eq!(poll["rpc_tool"], "poll_command");
        assert_eq!(poll["requested_wait_ms"], 15_000);
        assert!(poll.get("requested_timeout_ms").is_none());
        assert!(!poll.to_string().contains("secret"));

        let run = request_metadata(&json!({
            "method": "tools/call",
            "params": {
                "name": "run_command",
                "arguments": {
                    "command": "secret-command",
                    "timeout": 20_000,
                    "token": "secret-token"
                }
            }
        }));
        assert_eq!(run["rpc_tool"], "run_command");
        assert_eq!(run["requested_timeout_ms"], 20_000);
        assert!(run.get("requested_wait_ms").is_none());
        assert!(!run.to_string().contains("secret"));

        let invalid = request_metadata(&json!({
            "method": "tools/call",
            "params": {
                "name": "poll_command",
                "arguments": {"wait_ms": "secret-not-a-number"}
            }
        }));
        assert!(invalid.get("requested_wait_ms").is_none());
        assert!(!invalid.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn tool_result_bytes_records_numbers_only_aggregates() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let log = Diagnostics {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            write_failures: Arc::new(AtomicU64::new(0)),
            write_dropped: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicU64::new(0)),
            active_requests: Arc::new(StdMutex::new(HashMap::new())),
            stopping_since: Arc::new(StdMutex::new(None)),
        };
        let trace = RequestLog {
            log,
            id: "request-under-test".to_string(),
            started: Instant::now(),
            complete: AtomicBool::new(false),
            tool: AtomicU8::new(0),
        };
        REQUEST
            .scope(trace, async {
                tool_result_bytes(
                    crate::perf_metrics::tool_index(Some("run_command")),
                    "externalized",
                    123_456,
                    4_096,
                    123_456,
                    false,
                );
                // A slot beyond the whitelist degrades to "other": numeric slots
                // can never carry caller-controlled text into the log.
                tool_result_bytes(usize::MAX, "small", 1, 1, 0, true);
            })
            .await;

        let record = receiver.recv().unwrap().unwrap();
        assert_eq!(record["event"], "tool_result_bytes");
        assert_eq!(record["request_id"], "request-under-test");
        assert_eq!(record["rpc_tool"], "run_command");
        assert_eq!(record["class"], "externalized");
        assert_eq!(record["raw_bytes"], 123_456);
        assert_eq!(record["inline_bytes"], 4_096);
        assert_eq!(record["externalized_bytes"], 123_456);
        assert_eq!(record["is_error"], false);
        let fallback = receiver.recv().unwrap().unwrap();
        assert_eq!(fallback["rpc_tool"], "other");
        assert_eq!(fallback["is_error"], true);
        let expected_keys = [
            "event",
            "timestamp_ms",
            "pid",
            "dropped_records",
            "diagnostic_write_failures",
            "diagnostic_write_dropped",
            "request_id",
            "rpc_tool",
            "class",
            "raw_bytes",
            "inline_bytes",
            "externalized_bytes",
            "is_error",
        ];
        assert_eq!(
            record.as_object().map(|object| object.len()),
            Some(expected_keys.len()),
            "the record must carry exactly the aggregate fields, never payload keys"
        );
    }

    #[test]
    fn writer_persists_records_and_rotates_with_private_permissions() {
        let root =
            std::env::temp_dir().join(format!("catdesk-diagnostics-{}", uuid::Uuid::new_v4()));
        let mut writer = LogWriter::open(&root, 128).unwrap();
        for n in 0..20 {
            writer
                .write(&json!({"sequence": n, "event": "test"}))
                .unwrap();
        }
        drop(writer);
        let current = std::fs::read_to_string(root.join("connections.jsonl")).unwrap();
        assert!(current.contains("\"sequence\":19"));
        for name in [
            "connections.jsonl",
            "connections.1.jsonl",
            "connections.2.jsonl",
        ] {
            let path = root.join(name);
            assert!(std::fs::metadata(&path).unwrap().len() <= 128);
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_failure_reopens_writer_and_does_not_disable_later_records() {
        let root = std::env::temp_dir().join(format!(
            "catdesk-diagnostics-recovery-{}",
            uuid::Uuid::new_v4()
        ));
        let mut writer = Some(LogWriter::open(&root, 128).unwrap());
        let failures = AtomicU64::new(0);
        let oversized = json!({"event": "too-large", "payload": "x".repeat(512)});
        assert!(!write_record_resilient(
            &mut writer,
            &root,
            128,
            &oversized,
            &failures,
        ));
        assert!(failures.load(Ordering::Relaxed) >= 1);

        assert!(write_record_resilient(
            &mut writer,
            &root,
            128,
            &json!({"event": "after-failure"}),
            &failures,
        ));
        drop(writer);
        let current = std::fs::read_to_string(root.join("connections.jsonl")).unwrap();
        assert!(current.contains("after-failure"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn busy_writer_drops_records_without_blocking_and_reports_loss() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let log = Diagnostics {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            write_failures: Arc::new(AtomicU64::new(0)),
            write_dropped: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicU64::new(0)),
            active_requests: Arc::new(StdMutex::new(HashMap::new())),
            stopping_since: Arc::new(StdMutex::new(None)),
        };
        log.record(json!({"event": "first"}));
        log.record(json!({"event": "dropped"}));
        assert_eq!(receiver.recv().unwrap().unwrap()["event"], "first");
        log.record(json!({"event": "next"}));
        assert_eq!(receiver.recv().unwrap().unwrap()["dropped_records"], 1);
    }

    #[test]
    fn lifecycle_stages_are_visible_in_the_active_registry() {
        let root =
            std::env::temp_dir().join(format!("catdesk-stage-registry-{}", uuid::Uuid::new_v4()));
        let (log, guard) = Diagnostics::start(&root).unwrap();

        log.begin_request("slow", Instant::now() - Duration::from_millis(900));
        log.begin_request("fresh", Instant::now());
        assert_eq!(log.active_requests_view()[0].request_id, "slow");
        assert_eq!(log.active_requests_view()[0].stage, RequestStage::Queued);

        // Stage transitions are visible live, including on the oldest request.
        log.update_stage("slow", RequestStage::Dispatch);
        log.update_stage("fresh", RequestStage::Dispatch);
        log.update_stage("fresh", RequestStage::Executing);
        let view = log.active_requests_view();
        assert_eq!(view[0].stage, RequestStage::Dispatch);
        assert_eq!(view[1].request_id, "fresh");
        assert_eq!(view[1].stage, RequestStage::Executing);

        let snapshot = log.finish_request("fresh");
        assert_eq!(snapshot.oldest_active_stage, Some(RequestStage::Dispatch));
        assert_eq!(log.finish_request("slow").active_requests, 0);
        assert!(log.active_requests_view().is_empty());

        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellations_after_server_stopping_are_attributed_to_shutdown() {
        let root =
            std::env::temp_dir().join(format!("catdesk-cancel-reason-{}", uuid::Uuid::new_v4()));
        let (log, guard) = Diagnostics::start(&root).unwrap();

        let make_trace = |id: &str| RequestLog {
            log: log.clone(),
            id: id.to_string(),
            started: Instant::now(),
            complete: AtomicBool::new(false),
            tool: AtomicU8::new(0),
        };
        drop(make_trace("client-drop"));
        log.mark_server_stopping();
        drop(make_trace("shutdown-drop"));

        drop(guard); // waits for every accepted record to reach disk
        let records: Vec<Value> = std::fs::read_to_string(root.join("connections.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let cancelled: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "http_cancelled")
            .collect();
        assert_eq!(cancelled.len(), 2);
        assert_eq!(cancelled[0]["terminal_reason"], "client_disconnect");
        assert_eq!(cancelled[0]["stage"], "cancelled");
        assert_eq!(cancelled[1]["terminal_reason"], "server_shutdown");
        assert_eq!(cancelled[1]["stage"], "cancelled");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn response_terminal_reason_prefers_scheduler_failure_then_deadline_stage() {
        let deadline_stage = |stage: Option<&'static str>| SchedulerTiming {
            class: "control",
            queue_wait_ms: 0,
            execution_ms: 1,
            deadline_stage: stage,
        };
        assert_eq!(
            response_terminal_reason(None, None),
            TerminalReason::Completed
        );
        assert_eq!(
            response_terminal_reason(Some(deadline_stage(None)), None),
            TerminalReason::Completed
        );
        // A deadline stage alone (legacy records and older responses) is a
        // timeout, while an explicit scheduler failure outranks it.
        assert_eq!(
            response_terminal_reason(Some(deadline_stage(Some("execution"))), None),
            TerminalReason::DeadlineTimeout
        );
        assert_eq!(
            response_terminal_reason(
                Some(deadline_stage(None)),
                Some(TerminalReason::DeadlineTimeout)
            ),
            TerminalReason::DeadlineTimeout
        );
        assert_eq!(
            response_terminal_reason(None, Some(TerminalReason::WorkerFailed)),
            TerminalReason::WorkerFailed
        );
    }

    #[test]
    fn active_request_snapshot_tracks_oldest_remaining_request() {
        let root = std::env::temp_dir().join(format!(
            "catdesk-active-request-age-{}",
            uuid::Uuid::new_v4()
        ));
        let (log, guard) = Diagnostics::start(&root).unwrap();
        let now = Instant::now();

        let first = log.begin_request("first", now - Duration::from_millis(100));
        assert_eq!(first.active_requests, 1);
        assert!(first.oldest_active_request_ms >= 50);
        assert_eq!(first.oldest_active_stage, Some(RequestStage::Queued));

        let second = log.begin_request("second", now - Duration::from_millis(10));
        assert_eq!(second.active_requests, 2);
        assert!(second.oldest_active_request_ms >= 50);

        let remaining = log.finish_request("first");
        assert_eq!(remaining.active_requests, 1);
        assert!(remaining.oldest_active_request_ms >= 5);
        assert!(remaining.oldest_active_request_ms < 1_000);

        let empty = log.finish_request("second");
        assert_eq!(empty.active_requests, 0);
        assert_eq!(empty.oldest_active_request_ms, 0);
        assert_eq!(empty.oldest_active_stage, None);

        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlapping_processes_keep_separate_bounded_logs() {
        let root =
            std::env::temp_dir().join(format!("catdesk-concurrent-logs-{}", uuid::Uuid::new_v4()));
        let (first, first_guard) = Diagnostics::start(&root).unwrap();
        let second = Diagnostics::start(&root);
        first.record(json!({"event":"first"}));
        drop(first_guard);
        let (second, second_guard) = second.expect("a second process must retain diagnostics");
        second.record(json!({"event":"second"}));
        drop(second_guard);
        assert!(
            std::fs::read_to_string(root.join("connections.jsonl"))
                .unwrap()
                .contains("first")
        );
        assert!(
            std::fs::read_to_string(root.join("concurrent/connections.jsonl"))
                .unwrap()
                .contains("second")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_one_writer_can_rotate_the_same_log() {
        let root =
            std::env::temp_dir().join(format!("catdesk-diagnostics-lock-{}", uuid::Uuid::new_v4()));
        let writer = LogWriter::open(&root, 1024).unwrap();
        assert!(LogWriter::open(&root, 1024).is_err());
        drop(writer);
        drop(LogWriter::open(&root, 1024).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A holder that releases inside the retry budget must be acquired — the
    /// flake resistance this helper exists for. The synchronization is fully
    /// structural; no wall-clock assumption anywhere:
    /// 1. the holder thread owns the lock file through a blocking rendezvous
    ///    before the retry starts, so the first attempt provably runs against
    ///    a held lock;
    /// 2. the injectable wait callback fires only after a blocked attempt, so
    ///    its single invocation proves the first attempt failed with
    ///    `WouldBlock`;
    /// 3. that invocation requests the release and returns only after the
    ///    holder's zero-capacity ack, sent after the lock file was dropped —
    ///    so the next attempt provably runs against a released lock, and with
    ///    no other contender it must succeed there. Hence exactly one blocked
    ///    attempt.
    #[test]
    fn held_log_lock_is_acquired_once_the_holder_releases() {
        let root =
            std::env::temp_dir().join(format!("catdesk-lock-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (handover, handover_rx) = mpsc::sync_channel::<File>(0);
        let (release_request, release_request_rx) = mpsc::sync_channel::<()>(0);
        let (released_ack, released_ack_rx) = mpsc::sync_channel::<()>(0);
        let holder = std::thread::spawn(move || {
            let held = handover_rx
                .recv()
                .expect("handover must deliver the lock file");
            // Release only on request: the lock stays held until the retry
            // loop has provably observed its first blocked attempt.
            release_request_rx
                .recv()
                .expect("the retry loop must request the release");
            drop(held);
            released_ack
                .send(())
                .expect("the retry loop must await the release ack");
        });
        let lock = private_file(&root.join("connections.lock")).unwrap();
        lock.try_lock()
            .expect("the test must take the lock before the handover");
        // Rendezvous channel of capacity zero: when send returns, the holder
        // thread has received — and therefore owns and still holds — the lock.
        handover.send(lock).unwrap();

        let mut blocked_attempts = 0_u32;
        {
            let retry_lock = private_file(&root.join("connections.lock")).unwrap();
            let acquired = try_lock_bounded_with(&retry_lock, LOCK_RETRY_ATTEMPTS, || {
                blocked_attempts += 1;
                release_request
                    .send(())
                    .expect("the holder must still be waiting for the release request");
                // Returns only after the holder dropped the lock file.
                released_ack_rx
                    .recv()
                    .expect("the holder must confirm the release");
            });
            acquired.expect("a lock released inside the retry budget must be acquired");
        }
        assert_eq!(
            blocked_attempts, 1,
            "exactly the first attempt may observe the held lock; the next one \
             runs after the acknowledged release and must acquire"
        );
        holder.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A holder that outlasts the retry budget still fails the open, keeping
    /// the `WouldBlock` failure shape (and `Diagnostics::start`'s concurrent
    /// slot fallback) exactly as before the retry existed. The test thread
    /// itself holds the lock for the whole call, so the exhausted budget is
    /// deterministic regardless of scheduler timing; the lower elapsed bound
    /// is the protocol's own retry delay between the two attempts.
    #[test]
    fn held_log_lock_still_fails_after_the_retry_budget() {
        let root =
            std::env::temp_dir().join(format!("catdesk-lock-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let held = private_file(&root.join("connections.lock")).unwrap();
        held.try_lock().expect("the test thread must take the lock");

        let started = Instant::now();
        let retry_lock = private_file(&root.join("connections.lock")).unwrap();
        let error = try_lock_bounded(&retry_lock, 2, Duration::from_millis(10))
            .expect_err("an outlasted budget must fail");
        assert!(
            started.elapsed() >= Duration::from_millis(10),
            "the protocol's delay between both attempts must have run"
        );
        assert!(
            format!("{error:?}").contains("WouldBlock"),
            "the exhausted budget must keep the WouldBlock failure shape: {error:?}"
        );
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn real_http_requests_keep_status_and_correlation_without_payloads() {
        use crate::{command_jobs::CommandJobManager, state::AppState};
        use tokio::sync::{Mutex, mpsc::channel};
        let root =
            std::env::temp_dir().join(format!("catdesk-diagnostics-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (log, guard) = Diagnostics::start(&root.join("logs")).unwrap();
        let state = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let (events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let app = crate::server::router(
            Arc::new(Mutex::new(state)),
            None,
            CommandJobManager::new(),
            "/secret-slug/mcp".into(),
            events,
        )
        .route_layer(axum::middleware::from_fn_with_state(
            Some(log.clone()),
            http_request,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let call = |method: &'static str| {
            client
                .post(format!("{base}/secret-slug/mcp"))
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", method)
                .json(
                    &json!({"jsonrpc": "2.0", "id": "secret-client-id", "method": method,
                    "params": {"secret": "secret-argument", "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}}}}),
                )
                .send()
        };
        let (ping, initialize) = tokio::join!(call("ping"), call("initialize"));
        assert_eq!(ping.unwrap().status(), 200);
        let initialize = initialize.unwrap();
        assert_eq!(initialize.status(), 404);
        assert_eq!(
            initialize.json::<Value>().await.unwrap()["error"]["code"],
            -32601
        );
        assert_eq!(
            client
                .get(format!("{base}/secret-slug/mcp"))
                .send()
                .await
                .unwrap()
                .status(),
            405
        );
        assert_eq!(
            client
                .get(format!("{base}/secret-wrong-slug/mcp"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            client
                .post(format!("{base}/secret-slug/mcp"))
                .body("secret-invalid-json")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        for tool in ["catdesk_instruction", "secret-unknown-tool"] {
            let response = client
                .post(format!("{base}/secret-slug/mcp"))
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", tool)
                .json(
                    &json!({"jsonrpc": "2.0", "id": "secret-tool-id", "method": "tools/call",
                    "params": {"name": tool, "arguments": {}, "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}}}}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.json::<Value>().await.unwrap();
            assert_eq!(
                body["result"]["isError"].as_bool().unwrap_or(false),
                tool == "secret-unknown-tool"
            );
        }
        server.abort();
        let _ = server.await;
        drop(guard); // waits for every accepted record to reach disk
        let text = std::fs::read_to_string(root.join("logs/connections.jsonl")).unwrap();
        assert!(!text.contains("secret"));
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let starts: Vec<_> = records
            .iter()
            .filter(|r| r["event"] == "http_started")
            .collect();
        assert_eq!(starts.len(), 6);
        for start in &starts {
            assert!(start["active_requests"].is_number());
            assert!(start["oldest_active_request_ms"].is_number());
            // Every request enters the registry queued; the snapshot always
            // names a live stage because the request itself is active.
            assert_eq!(start["stage"], "queued");
            assert!(start["oldest_active_stage"].is_string());
            let finishes: Vec<_> = records
                .iter()
                .filter(|r| r["event"] == "http_finished" && r["request_id"] == start["request_id"])
                .collect();
            assert_eq!(finishes.len(), 1);
            assert!(finishes[0]["elapsed_ms"].is_number());
            assert!(finishes[0]["active_requests"].is_number());
            assert!(finishes[0]["oldest_active_request_ms"].is_number());
            // A produced response completes the lifecycle, whatever the status.
            assert_eq!(finishes[0]["stage"], "completed");
            assert_eq!(finishes[0]["terminal_reason"], "completed");
        }
        let init = records
            .iter()
            .find(|r| r["rpc_method"] == "initialize")
            .unwrap();
        let finish = records
            .iter()
            .find(|r| r["event"] == "http_finished" && r["request_id"] == init["request_id"])
            .unwrap();
        assert_eq!(finish["status"], 404);
        assert_eq!(finish["rpc_error_code"], -32601);
        assert!(
            records
                .iter()
                .any(|r| r["status"] == 400 && r["rpc_error_code"] == -32700)
        );
        // Tool errors carry exactly one model-readable text content item
        // (see mcp::jsonrpc::tool_response) alongside the structured payload.
        assert!(
            records
                .iter()
                .any(|r| r["status"] == 200 && r["tool_error"] == true && r["content_items"] == 1)
        );
        assert!(
            records
                .iter()
                .any(|r| r["status"] == 405 && r["rpc_error_code"] == -32601)
        );
        assert!(
            records.iter().any(|r| {
                r["event"] == "http_finished"
                    && r["scheduler_class"] == "control"
                    && r["scheduler_queue_wait_ms"].is_number()
                    && r["scheduler_execution_ms"].is_number()
                    && r["scheduler_deadline_stage"].is_null()
            }),
            "scheduled MCP calls must persist queue/execution timing without payloads"
        );
        assert!(
            records
                .iter()
                .any(|r| r["event"] == "http_started" && r["route_matched"] == true)
        );
        assert!(
            records
                .iter()
                .any(|r| r["event"] == "mcp_request" && r["rpc_method"] == "tools/call")
        );
        assert_eq!(log.active.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn perf_metrics_observe_http_requests_end_to_end() {
        use crate::{command_jobs::CommandJobManager, perf_metrics, state::AppState};
        use tokio::sync::{Mutex, mpsc::channel};
        let root = std::env::temp_dir().join(format!("catdesk-perf-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (log, guard) = Diagnostics::start(&root.join("logs")).unwrap();
        let state = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let (events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let app = crate::server::router(
            Arc::new(Mutex::new(state)),
            None,
            CommandJobManager::new(),
            "/secret-slug/mcp".into(),
            events,
        )
        .route_layer(axum::middleware::from_fn_with_state(
            Some(log.clone()),
            http_request,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let post = |method: &'static str, tool: Option<&'static str>| {
            client
                .post(format!("{base}/secret-slug/mcp"))
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", method)
                .header("Mcp-Name", tool.unwrap_or(""))
                .json(&json!({"jsonrpc": "2.0", "id": "perf-id", "method": method,
                    "params": {"name": tool, "arguments": {}, "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}}}}))
                .send()
        };
        let before = perf_metrics::snapshot();

        // ping answers early (no scheduler timing) -> plain "http" class.
        let ping = post("ping", None).await.unwrap();
        assert_eq!(ping.status(), 200);
        // tools/call is classified "control" via SchedulerTiming and keyed to
        // the whitelisted tool counter through rpc_request.
        let instruction = post("tools/call", Some("catdesk_instruction"))
            .await
            .unwrap();
        assert_eq!(instruction.status(), 200);

        server.abort();
        let _ = server.await;
        drop(guard);

        let after = perf_metrics::snapshot();
        assert!(
            after.aggregate.count >= before.aggregate.count + 2,
            "both requests must land in the latency aggregate"
        );
        assert!(
            after.aggregate.bytes > before.aggregate.bytes,
            "buffered JSON bodies must report their response bytes"
        );
        // In-flight depth/max themselves are proven by perf_metrics unit
        // tests; here a raised max only confirms the middleware participates
        // (a strict increase would race with parallel tests' requests).
        assert!(
            after.in_flight_max >= 1,
            "middleware must track in-flight depth"
        );
        let control_before = before.classes[perf_metrics::CLASS_CONTROL].count;
        assert!(
            after.classes[perf_metrics::CLASS_CONTROL].count > control_before,
            "MCP tools/call must observe the control class"
        );
        let tool = perf_metrics::tool_index(Some("catdesk_instruction"));
        assert!(
            after.tools[tool].count > before.tools[tool].count,
            "rpc_request must key the tool counter from the whitelist"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn spawn_diagnostic_server(
        root: &std::path::Path,
    ) -> (String, Diagnostics, Guard, tokio::task::JoinHandle<()>) {
        use crate::{command_jobs::CommandJobManager, state::AppState};
        use tokio::sync::{Mutex, mpsc::channel};
        let (log, guard) = Diagnostics::start(&root.join("logs")).unwrap();
        let state = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let (events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let app = crate::server::router(
            Arc::new(Mutex::new(state)),
            None,
            CommandJobManager::new(),
            "/secret-slug/mcp".into(),
            events,
        )
        .route_layer(axum::middleware::from_fn_with_state(
            Some(log.clone()),
            http_request,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, log, guard, server)
    }

    /// The same harness as [`spawn_diagnostic_server`], plus the soak-only
    /// response-deadline override layer (`post_mcp_http` reads the injected
    /// extension): requests on this router expire at `deadline` instead of
    /// the production policy. Production routers never install the layer.
    async fn spawn_deadline_diagnostic_server(
        root: &std::path::Path,
        deadline: Duration,
    ) -> (String, Diagnostics, Guard, tokio::task::JoinHandle<()>) {
        use crate::{command_jobs::CommandJobManager, state::AppState};
        use tokio::sync::{Mutex, mpsc::channel};
        let (log, guard) = Diagnostics::start(&root.join("logs")).unwrap();
        let state = AppState::new_for_test(
            0,
            root.to_string_lossy().into_owned(),
            root.join("config.toml"),
        )
        .unwrap();
        let (events, _receiver) = channel(crate::state::UI_EVENT_CAPACITY);
        let app = crate::server::router(
            Arc::new(Mutex::new(state)),
            None,
            CommandJobManager::new(),
            "/secret-slug/mcp".into(),
            events,
        )
        .layer(axum::middleware::from_fn(
            move |mut request: Request, next: Next| async move {
                request.extensions_mut().insert(deadline);
                next.run(request).await
            },
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            Some(log.clone()),
            http_request,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, log, guard, server)
    }

    async fn post_tools_call(
        client: &reqwest::Client,
        base: &str,
        tool: &'static str,
        arguments: Value,
    ) -> Value {
        client
            .post(format!("{base}/secret-slug/mcp"))
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", tool)
            .json(
                &json!({"jsonrpc": "2.0", "id": "lifecycle-id", "method": "tools/call",
                "params": {"name": tool, "arguments": arguments, "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}}}}),
            )
            .send()
            .await
            .expect("tool call request must complete")
            .json::<Value>()
            .await
            .expect("tool call response must be JSON")
    }

    #[tokio::test]
    async fn client_disconnection_is_recorded_as_cancelled_with_a_reason() {
        let root =
            std::env::temp_dir().join(format!("catdesk-cancel-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (base, log, guard, server) = spawn_diagnostic_server(&root).await;
        let client = reqwest::Client::new();

        // The instruction gate must be opened for the (anonymous) session
        // before any other tool is accepted.
        let instruction = post_tools_call(&client, &base, "catdesk_instruction", json!({})).await;
        // A denied call would carry the gate's structured errorCode.
        assert!(
            instruction["result"]["structuredContent"]["errorCode"].is_null(),
            "catdesk_instruction must open the session gate: {instruction}"
        );

        // A job that outlives the poll keeps the scheduled request blocked.
        let start = post_tools_call(
            &client,
            &base,
            "start_command",
            json!({"command": "sleep 3"}),
        )
        .await;
        let job_id = start["result"]["structuredContent"]["jobId"]
            .as_str()
            .expect("start_command must return a jobId")
            .to_string();

        // Poll with a wait, then drop the connection mid-request.
        let poll = tokio::spawn({
            let client = client.clone();
            let base = base.clone();
            async move {
                post_tools_call(
                    &client,
                    &base,
                    "poll_command",
                    json!({"job_id": job_id, "wait_ms": 1_500}),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        poll.abort();

        // The lifecycle registry is updated synchronously on drop, so it
        // proves the server saw the disconnect without waiting for disk.
        let mut drained = false;
        for _ in 0..50 {
            if log.active_requests_view().is_empty() {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            drained,
            "the cancelled request never left the lifecycle registry"
        );

        server.abort();
        let _ = server.await;
        drop(guard); // waits for every accepted record to reach disk
        let records: Vec<Value> = std::fs::read_to_string(root.join("logs/connections.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let cancelled: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "http_cancelled")
            .collect();
        assert_eq!(cancelled.len(), 1, "only the dropped poll is cancelled");
        assert_eq!(
            cancelled[0]["terminal_reason"], "client_disconnect",
            "a mid-request disconnect must be attributed to the client"
        );
        assert_eq!(cancelled[0]["stage"], "cancelled");
        let poll_start = records
            .iter()
            .find(|r| r["event"] == "http_started" && r["request_id"] == cancelled[0]["request_id"])
            .expect("the cancelled request must have started");
        assert_eq!(poll_start["stage"], "queued");
        assert_eq!(log.active.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_scheduled_requests_keep_correlated_lifecycle_records() {
        let root =
            std::env::temp_dir().join(format!("catdesk-concurrent-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (base, log, guard, server) = spawn_diagnostic_server(&root).await;
        let client = reqwest::Client::new();

        // The instruction gate must be opened for the (anonymous) session
        // before any other tool is accepted.
        let instruction = post_tools_call(&client, &base, "catdesk_instruction", json!({})).await;
        // A denied call would carry the gate's structured errorCode.
        assert!(
            instruction["result"]["structuredContent"]["errorCode"].is_null(),
            "catdesk_instruction must open the session gate: {instruction}"
        );

        let start = post_tools_call(
            &client,
            &base,
            "start_command",
            json!({"command": "sleep 3"}),
        )
        .await;
        let job_id = start["result"]["structuredContent"]["jobId"]
            .as_str()
            .expect("start_command must return a jobId")
            .to_string();

        // One deliberately slow poll plus fast ones running alongside it:
        // concurrency must not lose or merge any lifecycle record.
        let slow = tokio::spawn({
            let client = client.clone();
            let base = base.clone();
            let job_id = job_id.clone();
            async move {
                post_tools_call(
                    &client,
                    &base,
                    "poll_command",
                    json!({"job_id": job_id, "wait_ms": 1_000}),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut fast = Vec::new();
        for _ in 0..4 {
            fast.push(post_tools_call(
                &client,
                &base,
                "poll_command",
                json!({"job_id": "missing-job", "wait_ms": 0}),
            ));
        }
        for (index, body) in fast.into_iter().enumerate() {
            let body = body.await;
            assert_eq!(body["result"]["isError"], true, "fast poll {index} errored");
        }
        let slow_body = slow.await.expect("slow poll task panicked");
        assert!(
            slow_body["result"]["structuredContent"]["state"].is_string(),
            "the slow poll must return a job snapshot: {slow_body}"
        );

        server.abort();
        let _ = server.await;
        drop(guard);
        let records: Vec<Value> = std::fs::read_to_string(root.join("logs/connections.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let starts: Vec<_> = records
            .iter()
            .filter(|r| r["event"] == "http_started")
            .collect();
        assert_eq!(
            starts.len(),
            7,
            "one instruction, one start_command and five polls"
        );
        for start in &starts {
            let finishes: Vec<_> = records
                .iter()
                .filter(|r| r["event"] == "http_finished" && r["request_id"] == start["request_id"])
                .collect();
            assert_eq!(finishes.len(), 1, "exactly one terminal per request id");
            assert_eq!(finishes[0]["stage"], "completed");
            assert_eq!(finishes[0]["terminal_reason"], "completed");
            // polls and catdesk_instruction are control; start_command is process.
            assert!(
                ["control", "process"]
                    .contains(&finishes[0]["scheduler_class"].as_str().unwrap_or("")),
                "unexpected scheduler class: {}",
                finishes[0]
            );
            assert!(finishes[0]["scheduler_deadline_stage"].is_null());
        }
        assert!(
            starts
                .iter()
                .any(|start| start["active_requests"].as_u64() >= Some(2)),
            "the concurrent polls must overlap in the lifecycle registry"
        );
        assert_eq!(log.active.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Like [`post_tools_call`], but returns the HTTP response so a deadline
    /// scenario can assert the 504 status before decoding the JSON-RPC body.
    async fn post_tools_call_raw(
        client: &reqwest::Client,
        base: &str,
        tool: &'static str,
        arguments: Value,
    ) -> reqwest::Response {
        client
            .post(format!("{base}/secret-slug/mcp"))
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", tool)
            .json(
                &json!({"jsonrpc": "2.0", "id": "lifecycle-id", "method": "tools/call",
                "params": {"name": tool, "arguments": arguments, "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}}}}),
            )
            .send()
            .await
            .expect("tool call request must complete")
    }

    /// A real HTTP `tools/call` that runs past its response deadline must
    /// surface as a 504 with a JSON-RPC error and exactly one `http_finished`
    /// record whose `terminal_reason` is `deadline_timeout`, paired with one
    /// `request_worker_timeout` event. This test exercises the deadline
    /// BRANCH with the soak-only shortened override (the suite must stay
    /// fast); the production 120-second deadline value itself is pinned
    /// separately by `request_deadlines_never_exceed_mcp_http_ceiling` in
    /// `src/server.rs` — a literal near-ceiling probe would also race
    /// `run_command`'s own 30 s default / 120 s max command timeout, so the
    /// branch is deliberately tested short. The pipeline under test is the
    /// production one end to end.
    #[tokio::test]
    async fn deadline_timeout_is_recorded_through_real_http() {
        let root =
            std::env::temp_dir().join(format!("catdesk-deadline-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        // Pay the process-global catdesk_instruction warm-up (tokenizer, first
        // widget build) against a production-deadline router: on the shortened
        // router below, a cold gate call could exceed the deadline and pollute
        // the failure budget with its own deadline_timeout record.
        let (warmup_base, _warmup_guard, _warmup_log, warmup_server) =
            spawn_diagnostic_server(&root.join("warmup")).await;
        let warmup_client = reqwest::Client::new();
        let warmup = post_tools_call(
            &warmup_client,
            &warmup_base,
            "catdesk_instruction",
            json!({}),
        )
        .await;
        assert!(
            warmup["result"]["structuredContent"]["errorCode"].is_null(),
            "the warm-up gate call must succeed: {warmup}"
        );
        warmup_server.abort();
        let _ = warmup_server.await;

        // Deadline far below any production policy value, but wide enough for
        // the command to reach `Executing` first (soak-proven margin).
        let (base, log, guard, server) =
            spawn_deadline_diagnostic_server(&root, Duration::from_millis(1_500)).await;
        let client = reqwest::Client::new();

        let instruction = post_tools_call(&client, &base, "catdesk_instruction", json!({})).await;
        assert!(
            instruction["result"]["structuredContent"]["errorCode"].is_null(),
            "the gate must open for the anonymous session: {instruction}"
        );

        let probe =
            post_tools_call_raw(&client, &base, "run_command", json!({"command": "sleep 2"})).await;
        assert_eq!(
            probe.status().as_u16(),
            504,
            "work outliving the deadline must answer 504"
        );
        let body = probe.json::<Value>().await.expect("deadline body is JSON");
        assert_eq!(body["error"]["code"], -32000);

        // The lifecycle registry is updated synchronously on completion.
        assert_eq!(log.active.load(Ordering::Relaxed), 0);
        server.abort();
        let _ = server.await;
        drop(guard); // waits for every accepted record to reach disk

        let records: Vec<Value> = std::fs::read_to_string(root.join("logs/connections.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let timeouts: Vec<_> = records
            .iter()
            .filter(|record| {
                record["event"] == "http_finished"
                    && record["terminal_reason"] == "deadline_timeout"
            })
            .collect();
        assert_eq!(
            timeouts.len(),
            1,
            "exactly the run_command probe may time out (saw {} finished records)",
            records.len()
        );
        assert_eq!(timeouts[0]["status"], 504);
        assert_eq!(timeouts[0]["rpc_error_code"], -32000);
        assert_eq!(timeouts[0]["stage"], "completed");
        assert_eq!(timeouts[0]["scheduler_class"], "process");
        assert_eq!(
            timeouts[0]["scheduler_deadline_stage"], "execution",
            "the command must have started before the deadline expired"
        );
        assert!(
            timeouts[0]["elapsed_ms"].as_u64().unwrap() >= 1_400,
            "the finish must come from the shortened deadline, not a fast failure"
        );
        // The tool identity lives on the correlated request record, so the
        // timed-out finish is proven to belong to the run_command probe.
        let probe_request = records
            .iter()
            .find(|record| {
                record["event"] == "mcp_request"
                    && record["request_id"] == timeouts[0]["request_id"]
            })
            .expect("the timed-out request must have an mcp_request record");
        assert_eq!(probe_request["rpc_tool"], "run_command");

        // The paired `request_worker_timeout` failure event is emitted through
        // the process-global diagnostics feed (`diagnostics::event`), which
        // test harnesses deliberately never install (see [`Diagnostics::start`]
        // and the soak deadline scenario, which hits the same boundary). The
        // 504 + `deadline_timeout` classification above proves the same
        // `RequestFailure::Deadline` that drives that event fired on a real
        // connection.

        std::fs::remove_dir_all(root).unwrap();
    }
}

use axum::{
    body::HttpBody,
    extract::{MatchedPath, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOG_LIMIT: u64 = 5 * 1024 * 1024;
const WRITE_RETRY_ATTEMPTS: usize = 4;
const WRITE_RETRY_BASE_MS: u64 = 25;
static GLOBAL: OnceLock<Diagnostics> = OnceLock::new();
tokio::task_local! { static REQUEST: RequestLog; }

#[derive(Clone)]
pub(crate) struct Diagnostics {
    sender: mpsc::SyncSender<Option<Value>>,
    dropped: Arc<AtomicU64>,
    write_failures: Arc<AtomicU64>,
    write_dropped: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    /// Live lifecycle registry: request id -> start and current stage.
    active_requests: Arc<StdMutex<HashMap<String, ActiveRequest>>>,
    /// Set when the server begins stopping so cancellations after it are
    /// attributed to the shutdown rather than to client disconnects.
    stopping_since: Arc<StdMutex<Option<Instant>>>,
}

/// Drain accepted records on ordinary exit. A crash may lose queued records.
pub(crate) struct Guard {
    log: Diagnostics,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Blocking send is intentional: queue is bounded (1024) and we must drain
        // accepted records; blocks at most until worker consumes one slot.
        let _ = self.log.sender.send(None);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One in-flight request in the lifecycle registry.
#[derive(Clone, Copy, Debug)]
struct ActiveRequest {
    started: Instant,
    stage: RequestStage,
}

/// A correlated live view of one in-flight request.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveRequestView {
    pub(crate) request_id: String,
    pub(crate) stage: RequestStage,
    pub(crate) age_ms: u64,
}

/// The scheduler-level terminal reason of a response, carried in extensions so
/// the HTTP middleware can record why the request lifecycle ended.
#[derive(Clone, Copy)]
pub(crate) struct TerminalReasonExt(pub(crate) TerminalReason);

fn ms_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Why a produced response's lifecycle ended. The scheduler failure wins; a
/// recorded scheduler deadline stage is the legacy fallback for responses
/// built before terminal reasons existed; everything else completed.
fn response_terminal_reason(
    scheduler: Option<SchedulerTiming>,
    scheduled_failure: Option<TerminalReason>,
) -> TerminalReason {
    scheduled_failure
        .or_else(|| {
            scheduler
                .filter(|timing| timing.deadline_stage.is_some())
                .map(|_| TerminalReason::DeadlineTimeout)
        })
        .unwrap_or(TerminalReason::Completed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveRequestSnapshot {
    active_requests: u64,
    oldest_active_request_ms: u64,
    oldest_active_stage: Option<RequestStage>,
}

impl Diagnostics {
    fn active_request_snapshot(
        active_requests: &HashMap<String, ActiveRequest>,
    ) -> ActiveRequestSnapshot {
        // The oldest request is the one that started first; ties resolve to
        // any one of them, which is fine for a diagnostic snapshot.
        let oldest = active_requests
            .values()
            .min_by_key(|request| request.started);
        ActiveRequestSnapshot {
            active_requests: u64::try_from(active_requests.len()).unwrap_or(u64::MAX),
            oldest_active_request_ms: oldest.map_or(0, |request| ms_since(request.started)),
            oldest_active_stage: oldest.map(|request| request.stage),
        }
    }

    fn begin_request(&self, id: &str, started: Instant) -> ActiveRequestSnapshot {
        let mut active_requests = self
            .active_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active_requests.insert(
            id.to_string(),
            ActiveRequest {
                started,
                stage: RequestStage::Queued,
            },
        );
        let snapshot = Self::active_request_snapshot(&active_requests);
        self.active
            .store(snapshot.active_requests, Ordering::Relaxed);
        snapshot
    }

    fn update_stage(&self, id: &str, stage: RequestStage) {
        let mut active_requests = self
            .active_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(request) = active_requests.get_mut(id) {
            request.stage = stage;
        }
    }

    fn finish_request(&self, id: &str) -> ActiveRequestSnapshot {
        let mut active_requests = self
            .active_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active_requests.remove(id);
        let snapshot = Self::active_request_snapshot(&active_requests);
        self.active
            .store(snapshot.active_requests, Ordering::Relaxed);
        snapshot
    }

    /// Correlated live view of every in-flight request, oldest first.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn active_requests_view(&self) -> Vec<ActiveRequestView> {
        let active_requests = self
            .active_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut view: Vec<_> = active_requests
            .iter()
            .map(|(request_id, request)| ActiveRequestView {
                request_id: request_id.clone(),
                stage: request.stage,
                age_ms: ms_since(request.started),
            })
            .collect();
        view.sort_by(|a, b| b.age_ms.cmp(&a.age_ms));
        view
    }

    fn mark_server_stopping(&self) {
        let mut stopping_since = self
            .stopping_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stopping_since.is_none() {
            *stopping_since = Some(Instant::now());
        }
    }

    fn cancellation_reason(&self) -> TerminalReason {
        let stopping_since = self
            .stopping_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stopping_since.is_some() {
            TerminalReason::ServerShutdown
        } else {
            TerminalReason::ClientDisconnect
        }
    }

    /// Open the log writer and spawn its drain thread. Besides the module's
    /// own tests this is the harness entry point for the in-suite soak
    /// scenarios (src/soak.rs), which need a Diagnostics instance that is not
    /// installed as the process global.
    pub(crate) fn start(root: &Path) -> io::Result<(Self, Guard)> {
        // An overlapping restart must not silently lose all diagnostics while
        // the old process still holds the primary log. Two fixed slots retain
        // rotation bounds; they do not accumulate per-PID files indefinitely.
        let writer = LogWriter::open(root, LOG_LIMIT)
            .or_else(|_| LogWriter::open(&root.join("concurrent"), LOG_LIMIT))?;
        let (sender, receiver) = mpsc::sync_channel(1024);
        let write_failures = Arc::new(AtomicU64::new(0));
        let write_dropped = Arc::new(AtomicU64::new(0));
        let log = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            write_failures: write_failures.clone(),
            write_dropped: write_dropped.clone(),
            active: Arc::new(AtomicU64::new(0)),
            active_requests: Arc::new(StdMutex::new(HashMap::new())),
            stopping_since: Arc::new(StdMutex::new(None)),
        };
        let writer_root = writer.root.clone();
        let writer_limit = writer.limit;
        let worker = std::thread::Builder::new()
            .name("catdesk-diagnostics".into())
            .spawn(move || {
                let mut writer = Some(writer);
                while let Ok(Some(record)) = receiver.recv() {
                    if !write_record_resilient(
                        &mut writer,
                        &writer_root,
                        writer_limit,
                        &record,
                        &write_failures,
                    ) {
                        write_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok((
            log.clone(),
            Guard {
                log,
                worker: Some(worker),
            },
        ))
    }

    fn record(&self, mut value: Value) {
        value["timestamp_ms"] = json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        value["pid"] = json!(std::process::id());
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let write_failures = self.write_failures.swap(0, Ordering::Relaxed);
        let write_dropped = self.write_dropped.swap(0, Ordering::Relaxed);
        value["dropped_records"] = json!(dropped);
        value["diagnostic_write_failures"] = json!(write_failures);
        value["diagnostic_write_dropped"] = json!(write_dropped);
        if self.sender.try_send(Some(value)).is_err() {
            self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
            self.write_failures
                .fetch_add(write_failures, Ordering::Relaxed);
            self.write_dropped
                .fetch_add(write_dropped, Ordering::Relaxed);
        }
    }
}

pub(crate) fn init(root: &Path) -> io::Result<Guard> {
    let (log, guard) = Diagnostics::start(root)?;
    GLOBAL
        .set(log.clone())
        .map_err(|_| io::Error::other("diagnostics already initialized"))?;
    log.record(process_started_record());
    Ok(guard)
}

pub(crate) fn global() -> Option<Diagnostics> {
    GLOBAL.get().cloned()
}

/// Only call with fixed event names, never error strings, URLs or payloads.
pub(crate) fn event(event: &'static str) {
    if let Some(log) = GLOBAL.get() {
        log.record(json!({"event": event}));
    }
}

/// Record the server-stop event and mark the moment, so request futures
/// dropped afterwards are attributed to the shutdown instead of client
/// disconnects.
pub(crate) fn server_stopping() {
    if let Some(log) = GLOBAL.get() {
        log.mark_server_stopping();
    }
    event("server_stopping");
}

/// Advance the live lifecycle stage of the request running on this task.
/// No-op outside request scope, so stage transitions are free for paths
/// that never entered the middleware.
pub(crate) fn set_current_request_stage(stage: RequestStage) {
    let _ = REQUEST.try_with(|request| request.log.update_stage(&request.id, stage));
}

/// A callback for `run_timed` that advances the creating request's stage to
/// `Executing` from the blocking-pool thread, where task-locals do not exist.
pub(crate) fn current_execute_started() -> Option<crate::request_workers::ExecuteStarted> {
    let reporter = REQUEST
        .try_with(|request| StageReporter {
            log: request.log.clone(),
            id: request.id.clone(),
        })
        .ok()?;
    Some(Arc::new(move || {
        reporter
            .log
            .update_stage(&reporter.id, RequestStage::Executing)
    }) as crate::request_workers::ExecuteStarted)
}

/// Owned handle to one request's live stage, usable from any thread.
struct StageReporter {
    log: Diagnostics,
    id: String,
}

/// Identity appears exactly once per process, in this record; per-request
/// diagnostics keep their shape without build metadata.
pub(crate) fn process_started_record() -> Value {
    json!({
        "event": "process_started",
        "version": crate::build_info::VERSION,
        "build": crate::build_info::identity_line(
            crate::build_info::VERSION,
            crate::build_info::GIT_SHA,
            crate::build_info::GIT_BRANCH,
            crate::build_info::BUILD_TIMESTAMP,
        ),
    })
}

/// Tool names `request_metadata` may persist; only known local tool names
/// are safe because browser/custom names are client input. Must stay in sync
/// with `perf_metrics::TOOLS` (minus its trailing "other" slot) — enforced by
/// `request_metadata_whitelist_stays_in_sync_with_perf_metrics_tool_slots`,
/// not by this comment.
const RPC_TOOL_WHITELIST: [&str; 14] = [
    "catdesk_instruction",
    "run_command",
    "start_command",
    "poll_command",
    "cancel_command",
    "read",
    "read_image",
    "search",
    "write",
    "edit",
    "delete",
    "create_handoff",
    "read_result",
    "search_result",
];

pub(crate) fn request_metadata(body: &Value) -> Value {
||||||| parent of f18b298 (feat(tui): warn on consecutive identical 504s per tool)
pub(crate) fn request_metadata(body: &Value) -> Value {
    let method = match body.get("method").and_then(Value::as_str) {
        Some(
            method @ ("initialize"
            | "server/discover"
            | "ping"
            | "tools/list"
            | "tools/call"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
            | "prompts/list"
            | "prompts/get"
            | "notifications/initialized"
            | "notifications/cancelled"),
        ) => method,
        Some(_) => "other",
        None => "missing",
    };
    let mut metadata = json!({"rpc_method": method});
    if method == "tools/call" {
        let tool = match body
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
        {
            Some(name) if RPC_TOOL_WHITELIST.contains(&name) => name,
            _ => "other",
        };
        metadata["rpc_tool"] = json!(tool);

        // Persist only bounded numeric timing hints that help correlate a
        // client-visible stream stall with an intentionally blocking tool
        // call. Never persist command text, job ids, paths, or other payloads.
        let arguments = body
            .get("params")
            .and_then(|params| params.get("arguments"));
        match tool {
            "poll_command" => {
                if let Some(wait_ms) = arguments
                    .and_then(|arguments| arguments.get("wait_ms"))
                    .and_then(Value::as_u64)
                {
                    metadata["requested_wait_ms"] = json!(wait_ms);
                }
            }
            "run_command" => {
                if let Some(timeout_ms) = arguments
                    .and_then(|arguments| arguments.get("timeout"))
                    .and_then(Value::as_u64)
                {
                    metadata["requested_timeout_ms"] = json!(timeout_ms);
                }
            }
            _ => {}
        }
    }
    metadata
}

pub(crate) fn rpc_request(body: &Value) {
    let _ = REQUEST.try_with(|request| {
        let mut metadata = request_metadata(body);
        metadata["event"] = json!("mcp_request");
        metadata["request_id"] = json!(request.id);
        request.log.record(metadata);
        // Key the perf tool counter from the same whitelist; stored as
        // index + 1 so 0 means "not a tools/call request".
        if body.get("method").and_then(Value::as_str) == Some("tools/call") {
            let tool = body
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str);
            let index = crate::perf_metrics::tool_index(tool);
            request.tool.store(index as u8 + 1, Ordering::Relaxed);
        }
    });
}

/// Numeric-only per-call byte accounting for one tool result (see
/// `tool_result_metrics`). The tool name resolves from the fixed perf-metrics
/// whitelist by slot, so caller-controlled text can never reach the log.
pub(crate) fn tool_result_bytes(
    slot: usize,
    class: &'static str,
    raw: u64,
    inline: u64,
    externalized: u64,
    is_error: bool,
) {
    let _ = REQUEST.try_with(|request| {
        request.log.record(json!({
            "event": "tool_result_bytes",
            "request_id": request.id,
            "rpc_tool": crate::perf_metrics::tool_name(slot),
            "class": class,
            "raw_bytes": raw,
            "inline_bytes": inline,
            "externalized_bytes": externalized,
            "is_error": is_error,
        }));
    });
}

/// Response extensions stay local; they do not alter HTTP headers or bodies.
#[derive(Clone, Copy)]
pub(crate) struct RpcError(pub i64);

#[derive(Clone, Copy)]
pub(crate) struct ToolResult {
    pub is_error: Option<bool>,
    pub content_items: Option<usize>,
}

#[derive(Clone, Copy)]
pub(crate) struct SchedulerTiming {
    pub class: &'static str,
    pub queue_wait_ms: u64,
    pub execution_ms: u64,
    pub deadline_stage: Option<&'static str>,
}

struct RequestLog {
    log: Diagnostics,
    id: String,
    started: Instant,
    complete: AtomicBool,
    /// Perf tool-counter key (`tool_index + 1`); 0 until rpc_request runs.
    tool: AtomicU8,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        if !self.complete.load(Ordering::Relaxed) {
            let reason = self.log.cancellation_reason();
            let active = self.log.finish_request(&self.id);
            self.log
                .record(json!({"event": "http_cancelled", "request_id": self.id,
                "stage": reason.stage_str(), "terminal_reason": reason.as_str(),
                "elapsed_ms": self.started.elapsed().as_millis(),
                "active_requests": active.active_requests,
                "oldest_active_request_ms": active.oldest_active_request_ms,
                "oldest_active_stage": active.oldest_active_stage.map(RequestStage::as_str)}));
        }
    }
}

pub(crate) async fn http_request(
    State(log): State<Option<Diagnostics>>,
    request: Request,
    next: Next,
) -> Response {
    // Depth accounting survives cancellations: the guard decrements on drop.
    let _in_flight = crate::perf_metrics::InFlightGuard::new();
    let started = Instant::now();
    let Some(log) = log else {
        let response = next.run(request).await;
        crate::perf_metrics::observe(perf_observation(
            &response,
            started.elapsed().as_millis(),
            None,
        ));
        return response;
    };
    let method = match request.method().as_str() {
        m @ ("GET" | "POST" | "DELETE" | "OPTIONS" | "HEAD" | "PUT" | "PATCH") => m,
        _ => "other",
    };
    let trace = RequestLog {
        log,
        id: uuid::Uuid::new_v4().to_string(),
        started,
        complete: AtomicBool::new(false),
        tool: AtomicU8::new(0),
    };
    let active = trace.log.begin_request(&trace.id, trace.started);
    trace.log.record(
        json!({"event": "http_started", "request_id": trace.id, "http_method": method,
        "stage": RequestStage::Queued.as_str(),
        "route_matched": request.extensions().get::<MatchedPath>().is_some(),
        "active_requests": active.active_requests,
        "oldest_active_request_ms": active.oldest_active_request_ms,
        "oldest_active_stage": active.oldest_active_stage.map(RequestStage::as_str)}),
    );
    REQUEST.scope(trace, async {
        let response = next.run(request).await;
        REQUEST.with(|trace| {
            let scheduler = response.extensions().get::<SchedulerTiming>().copied();
            let terminal_reason = response_terminal_reason(
                scheduler,
                response.extensions().get::<TerminalReasonExt>().map(|e| e.0),
            );
            let active = trace.log.finish_request(&trace.id);
            trace.complete.store(true, Ordering::Relaxed);
            let elapsed_ms = trace.started.elapsed().as_millis();
            let observation = perf_observation(&response, elapsed_ms, scheduler);
            crate::perf_metrics::observe(observation);
            let tool = trace.tool.load(Ordering::Relaxed);
            if tool != 0 {
                crate::perf_metrics::observe_tool(
                    usize::from(tool - 1),
                    observation.bytes,
                    observation.deadline,
                    observation.failed,
                );
            }
            trace.log.record(json!({"event": "http_finished", "request_id": trace.id,
                "stage": terminal_reason.stage_str(), "terminal_reason": terminal_reason.as_str(),
                "status": response.status().as_u16(), "rpc_error_code": response.extensions().get::<RpcError>().map(|e| e.0),
                "tool_error": response.extensions().get::<ToolResult>().and_then(|r| r.is_error),
                "content_items": response.extensions().get::<ToolResult>().and_then(|r| r.content_items),
                "scheduler_class": scheduler.map(|timing| timing.class),
                "scheduler_queue_wait_ms": scheduler.map(|timing| timing.queue_wait_ms),
                "scheduler_execution_ms": scheduler.map(|timing| timing.execution_ms),
                "scheduler_deadline_stage": scheduler.and_then(|timing| timing.deadline_stage),
                "elapsed_ms": elapsed_ms,
                "active_requests": active.active_requests,
                "oldest_active_request_ms": active.oldest_active_request_ms,
                "oldest_active_stage": active.oldest_active_stage.map(RequestStage::as_str)}));
        });
        response
    }).await
}

/// Map a finished response onto one bounded perf observation. MCP calls carry
/// their request class via `SchedulerTiming`; everything else is plain "http".
fn perf_observation(
    response: &Response,
    elapsed_ms: u128,
    scheduler: Option<SchedulerTiming>,
) -> crate::perf_metrics::Observation {
    let status = response.status();
    crate::perf_metrics::Observation {
        class: scheduler
            .map(|timing| timing.class)
            .unwrap_or(crate::perf_metrics::HTTP_CLASS_NAME),
        elapsed_ms: u64::try_from(elapsed_ms).unwrap_or(u64::MAX),
        dispatch_ms: scheduler.map(|timing| timing.queue_wait_ms),
        execution_ms: scheduler.map(|timing| timing.execution_ms),
        deadline: scheduler.is_some_and(|timing| timing.deadline_stage.is_some())
            || status == StatusCode::GATEWAY_TIMEOUT,
        failed: status.is_client_error() || status.is_server_error(),
        // Buffered JSON bodies know their size; unknown/streaming sizes stay 0.
        bytes: response.body().size_hint().exact().unwrap_or(0),
    }
}

struct LogWriter {
    root: PathBuf,
    file: Option<File>,
    bytes: u64,
    limit: u64,
    _lock: File,
}

/// How long `LogWriter::open` keeps re-attempting a held `connections.lock`.
/// `WouldBlock` is the lock's normal "held by another writer" answer, and a
/// holder often releases within milliseconds (a draining overlapping process,
/// a racing writer reopen), so a bounded wait absorbs that contention instead
/// of failing on the first try. After the budget the error surfaces unchanged
/// and `Diagnostics::start` keeps its designed fallback to the `concurrent/`
/// slot, so long-lived holders (another live process) behave exactly as
/// before.
const LOCK_RETRY_ATTEMPTS: u32 = 5;
const LOCK_RETRY_DELAY_MS: u64 = 25;

/// Try to take the log's advisory lock, briefly re-attempting while it is
/// still held. Other errors surface immediately; an exhausted budget maps the
/// final `WouldBlock` through `io::Error::other` exactly as before, so the
/// failure shape in logs and tests stays the same.
fn try_lock_bounded(lock: &File, attempts: u32, retry_delay: Duration) -> Result<(), io::Error> {
    try_lock_bounded_with(lock, attempts, || std::thread::sleep(retry_delay))
}

/// The retry loop behind `try_lock_bounded`, parameterized over the
/// inter-attempt wait. Production passes the fixed sleep; tests pass a
/// synchronizing callback instead, which makes the loop's ordering — blocked
/// attempt, then wait, then attempt again — provable without any wall-clock
/// assumption.
fn try_lock_bounded_with(
    lock: &File,
    attempts: u32,
    mut wait_between_attempts: impl FnMut(),
) -> Result<(), io::Error> {
    for attempt in 0..attempts {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
            Err(std::fs::TryLockError::WouldBlock) if attempt + 1 < attempts => {
                wait_between_attempts();
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::other(std::fs::TryLockError::WouldBlock));
            }
        }
    }
    unreachable!("the final attempt returns inside the loop")
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    // Windows File::try_lock requires read or write access, not append-only.
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn write_record_resilient(
    writer: &mut Option<LogWriter>,
    root: &Path,
    limit: u64,
    record: &Value,
    write_failures: &AtomicU64,
) -> bool {
    for attempt in 0..WRITE_RETRY_ATTEMPTS {
        if writer.is_none() {
            match LogWriter::open(root, limit) {
                Ok(opened) => *writer = Some(opened),
                Err(_) => {
                    write_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if let Some(current) = writer.as_mut() {
            match current.write(record) {
                Ok(()) => return true,
                Err(_) => {
                    write_failures.fetch_add(1, Ordering::Relaxed);
                    writer.take();
                }
            }
        }

        if attempt + 1 < WRITE_RETRY_ATTEMPTS {
            let shift = (attempt as u32).min(3);
            std::thread::sleep(Duration::from_millis(
                WRITE_RETRY_BASE_MS.saturating_mul(1u64 << shift),
            ));
        }
    }

    eprintln!("CatDesk: connection diagnostics degraded; one record dropped after retry budget");
    false
}

impl LogWriter {
    fn open(root: &Path, limit: u64) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let lock = private_file(&root.join("connections.lock"))?;
        try_lock_bounded(
            &lock,
            LOCK_RETRY_ATTEMPTS,
            Duration::from_millis(LOCK_RETRY_DELAY_MS),
        )?;
        let file = private_file(&root.join("connections.jsonl"))?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            root: root.to_path_buf(),
            file: Some(file),
            bytes,
            limit,
            _lock: lock,
        })
    }

    fn write(&mut self, record: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if line.len() as u64 > self.limit {
            return Err(io::Error::other("diagnostic record too large"));
        }
        if self.bytes + line.len() as u64 > self.limit {
            self.file.take();
            let backup = self.root.join("connections.2.jsonl");
            match std::fs::remove_file(&backup) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let previous = self.root.join("connections.1.jsonl");
            match std::fs::rename(&previous, &backup) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            std::fs::rename(self.root.join("connections.jsonl"), previous)?;
            self.file = Some(private_file(&self.root.join("connections.jsonl"))?);
            self.bytes = 0;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file unavailable"))?
            .write_all(&line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}
