//! Bounded, metadata-only connection diagnostics. Never persist MCP payloads.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        };
        log.record(json!({"event": "first"}));
        log.record(json!({"event": "dropped"}));
        assert_eq!(receiver.recv().unwrap().unwrap()["event"], "first");
        log.record(json!({"event": "next"}));
        assert_eq!(receiver.recv().unwrap().unwrap()["dropped_records"], 1);
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
            let finishes: Vec<_> = records
                .iter()
                .filter(|r| r["event"] == "http_finished" && r["request_id"] == start["request_id"])
                .collect();
            assert_eq!(finishes.len(), 1);
            assert!(finishes[0]["elapsed_ms"].is_number());
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
        assert!(
            records
                .iter()
                .any(|r| r["status"] == 200 && r["tool_error"] == true && r["content_items"] == 0)
        );
        assert!(
            records
                .iter()
                .any(|r| r["status"] == 405 && r["rpc_error_code"] == -32601)
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
}

use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

impl Diagnostics {
    fn start(root: &Path) -> io::Result<(Self, Guard)> {
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
            self.write_dropped.fetch_add(write_dropped, Ordering::Relaxed);
        }
    }
}

pub(crate) fn init(root: &Path) -> io::Result<Guard> {
    let (log, guard) = Diagnostics::start(root)?;
    GLOBAL
        .set(log)
        .map_err(|_| io::Error::other("diagnostics already initialized"))?;
    event("process_started");
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

fn request_metadata(body: &Value) -> Value {
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
        // Only known local tool names are safe: browser/custom names are input.
        let tool = match body
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
        {
            Some(
                name @ ("catdesk_instruction"
                | "run_command"
                | "start_command"
                | "poll_command"
                | "cancel_command"
                | "read"
                | "read_image"
                | "search"
                | "write"
                | "edit"
                | "delete"
                | "create_handoff"),
            ) => name,
            _ => "other",
        };
        metadata["rpc_tool"] = json!(tool);
    }
    metadata
}

pub(crate) fn rpc_request(body: &Value) {
    let _ = REQUEST.try_with(|request| {
        let mut metadata = request_metadata(body);
        metadata["event"] = json!("mcp_request");
        metadata["request_id"] = json!(request.id);
        request.log.record(metadata);
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

struct RequestLog {
    log: Diagnostics,
    id: String,
    started: Instant,
    complete: AtomicBool,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        self.log.active.fetch_sub(1, Ordering::Relaxed);
        if !self.complete.load(Ordering::Relaxed) {
            self.log
                .record(json!({"event": "http_cancelled", "request_id": self.id,
                "elapsed_ms": self.started.elapsed().as_millis()}));
        }
    }
}

pub(crate) async fn http_request(
    State(log): State<Option<Diagnostics>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(log) = log else {
        return next.run(request).await;
    };
    let method = match request.method().as_str() {
        m @ ("GET" | "POST" | "DELETE" | "OPTIONS" | "HEAD" | "PUT" | "PATCH") => m,
        _ => "other",
    };
    let trace = RequestLog {
        log,
        id: uuid::Uuid::new_v4().to_string(),
        started: Instant::now(),
        complete: AtomicBool::new(false),
    };
    let active = trace.log.active.fetch_add(1, Ordering::Relaxed) + 1;
    trace.log.record(json!({"event": "http_started", "request_id": trace.id, "http_method": method,
        "route_matched": request.extensions().get::<MatchedPath>().is_some(), "active_requests": active}));
    REQUEST.scope(trace, async {
        let response = next.run(request).await;
        REQUEST.with(|trace| {
            trace.log.record(json!({"event": "http_finished", "request_id": trace.id,
                "status": response.status().as_u16(), "rpc_error_code": response.extensions().get::<RpcError>().map(|e| e.0),
                "tool_error": response.extensions().get::<ToolResult>().and_then(|r| r.is_error),
                "content_items": response.extensions().get::<ToolResult>().and_then(|r| r.content_items),
                "elapsed_ms": trace.started.elapsed().as_millis()}));
            trace.complete.store(true, Ordering::Relaxed);
        });
        response
    }).await
}

struct LogWriter {
    root: PathBuf,
    file: Option<File>,
    bytes: u64,
    limit: u64,
    _lock: File,
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
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
        lock.try_lock().map_err(io::Error::other)?;
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
