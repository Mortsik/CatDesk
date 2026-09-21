use serde_json::{Value, json};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::browser::DetectedBrowser;

const DEVTOOLS_PROTOCOL_VERSION: &str = "2025-03-26";
const DEVTOOLS_CLIENT_NAME: &str = "catdesk-bridge";
const DEVTOOLS_CLIENT_VERSION: &str = "4.0.0";
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const RESTART_BACKOFF_BASE_MS: u64 = 250;
const RESTART_BACKOFF_MAX_MS: u64 = 15_000;
const RESTART_JITTER_MAX_MS: u64 = 250;

#[derive(Clone)]
struct LaunchSpec {
    program: String,
    args: Vec<String>,
}

impl LaunchSpec {
    fn devtools(selected_browser: Option<&DetectedBrowser>) -> Self {
        let mut args = vec!["-y".to_string(), "chrome-devtools-mcp@latest".to_string()];
        if let Some(browser) = selected_browser {
            if browser.remote_debug_active {
                if let Some(target) = browser.remote_debug_target.as_deref() {
                    if target == "pipe" {
                        args.extend(["--executablePath".to_string(), browser.path.clone()]);
                    } else {
                        args.extend(["--browserUrl".to_string(), format!("http://{target}")]);
                    }
                } else {
                    args.extend(["--executablePath".to_string(), browser.path.clone()]);
                }
            } else {
                args.extend(["--executablePath".to_string(), browser.path.clone()]);
            }
        }
        Self {
            program: "npx".to_string(),
            args,
        }
    }

    fn spawn(&self) -> Result<Child, String> {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn DevTools bridge: {e}"))
    }
}

fn restart_backoff(failures: u32) -> StdDuration {
    let factor = 1u64 << failures.saturating_sub(1).min(6);
    let base = RESTART_BACKOFF_BASE_MS
        .saturating_mul(factor)
        .min(RESTART_BACKOFF_MAX_MS);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % (RESTART_JITTER_MAX_MS + 1))
        .unwrap_or(0);
    StdDuration::from_millis(base.saturating_add(jitter).min(RESTART_BACKOFF_MAX_MS))
}

fn stderr_event(bytes: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if text.contains("out of memory") || text.contains("enomem") || text.contains("heap limit") {
        Some("devtools_stderr_memory_error")
    } else if text.contains("econnreset") || text.contains("econnrefused") || text.contains("epipe")
    {
        Some("devtools_stderr_connection_error")
    } else if text.contains("error") {
        Some("devtools_stderr_error")
    } else {
        None
    }
}

#[derive(Default)]
struct Pending {
    senders: std::collections::HashMap<Value, tokio::sync::oneshot::Sender<Value>>,
    closed: bool,
}

struct PendingRequest {
    pending: Arc<StdMutex<Pending>>,
    id: Value,
}

struct PendingWrite {
    pending: Arc<StdMutex<Pending>>,
    complete: bool,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        if !self.complete {
            let mut pending = self.pending.lock().unwrap();
            pending.closed = true;
            pending.senders.clear();
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        // No await in Drop: cancellation must clean up even if the caller stops
        // polling this future. The mutex never spans I/O or an await.
        self.pending.lock().unwrap().senders.remove(&self.id);
    }
}

/// A running chrome-devtools-mcp child process with stdin/stdout JSON-RPC bridge.
pub struct DevtoolsBridge {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::io::BufWriter<tokio::process::ChildStdin>,
    pending: Arc<StdMutex<Pending>>,
    reader: tokio::task::JoinHandle<()>,
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    launch: Option<LaunchSpec>,
    restart_failures: u32,
    next_restart_at: Option<Instant>,
}

impl DevtoolsBridge {
    /// The browser protocol itself is serialized. Temporary saturation waits in
    /// Tokio's mutex queue instead of surfacing a misleading two-second Busy error.
    pub async fn call(bridge: &Arc<Mutex<Self>>, req: &Value) -> Result<Value, String> {
        let mut guard = bridge.lock().await;
        if !guard.is_connected() {
            guard.restart().await?;
        }
        guard.request(req).await
    }

    /// Spawn `npx chrome-devtools-mcp@latest` and set up stdio bridge.
    pub async fn start(
        selected_browser: Option<&DetectedBrowser>,
    ) -> Result<Arc<Mutex<Self>>, String> {
        let launch = LaunchSpec::devtools(selected_browser);
        let child = launch.spawn()?;
        let bridge = Arc::new(Mutex::new(Self::from_child_inner(child, Some(launch))?));
        {
            let mut bridge_guard = bridge.lock().await;
            bridge_guard.initialize_protocol().await?;
        }
        Ok(bridge)
    }

    fn from_child(child: Child) -> Result<Arc<Mutex<Self>>, String> {
        Ok(Arc::new(Mutex::new(Self::from_child_inner(child, None)?)))
    }

    fn from_child_inner(mut child: Child, launch: Option<LaunchSpec>) -> Result<Self, String> {
        let child_stdin = child.stdin.take().ok_or("No stdin")?;
        let child_stdout = child.stdout.take().ok_or("No stdout")?;
        let stderr_reader = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut chunk = Vec::new();
                loop {
                    chunk.clear();
                    match (&mut reader).take(4096).read_until(b'\n', &mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Some(event) = stderr_event(&chunk) {
                                // Classify only; stderr can include credential-bearing URLs.
                                crate::diagnostics::event(event);
                            }
                        }
                    }
                }
            })
        });

        let stdin = tokio::io::BufWriter::new(child_stdin);
        let pending = Arc::new(StdMutex::new(Pending::default()));

        // Spawn stdout reader task
        let pending_clone = pending.clone();
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(child_stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match (&mut reader)
                    .take(MAX_RESPONSE_BYTES + 1)
                    .read_until(b'\n', &mut line)
                    .await
                {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if line.len() as u64 > MAX_RESPONSE_BYTES {
                            crate::diagnostics::event("devtools_response_too_large");
                            break;
                        }
                        if let Ok(msg) = serde_json::from_slice::<Value>(&line) {
                            // Match response to pending request by id
                            if let Some(id) = msg.get("id").cloned() {
                                let mut map = pending_clone.lock().unwrap();
                                if let Some(tx) = map.senders.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                            }
                            // Notifications from devtools (no id) are ignored for now
                        }
                    }
                    Err(_) => break,
                }
            }
            let mut pending = pending_clone.lock().unwrap();
            pending.closed = true;
            pending.senders.clear();
            crate::diagnostics::event("devtools_stdout_closed");
        });

        Ok(Self {
            child,
            stdin,
            pending,
            reader,
            stderr_reader,
            launch,
            restart_failures: 0,
            next_restart_at: None,
        })
    }

    async fn initialize_protocol(&mut self) -> Result<(), String> {
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": "dt-init",
            "method": "initialize",
            "params": {
                "protocolVersion": DEVTOOLS_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": DEVTOOLS_CLIENT_NAME,
                    "version": DEVTOOLS_CLIENT_VERSION
                }
            }
        });
        self.request(&init_req).await?;
        self.notify(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await
    }

    pub fn is_connected(&self) -> bool {
        !self.pending.lock().unwrap().closed
    }

    fn record_restart_failure(&mut self) {
        self.restart_failures = self.restart_failures.saturating_add(1);
        self.next_restart_at = Some(Instant::now() + restart_backoff(self.restart_failures));
        crate::diagnostics::event("devtools_restart_failed");
    }

    async fn restart(&mut self) -> Result<(), String> {
        let Some(launch) = self.launch.clone() else {
            return Err("DevTools process disconnected; this bridge cannot be restarted automatically.".into());
        };
        if let Some(next) = self.next_restart_at {
            if let Some(remaining) = next.checked_duration_since(Instant::now()) {
                return Err(format!(
                    "DevTools is reconnecting; retry after {} ms",
                    remaining.as_millis()
                ));
            }
        }

        let child = match launch.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.record_restart_failure();
                return Err(error);
            }
        };
        let mut replacement = match Self::from_child_inner(child, Some(launch)) {
            Ok(replacement) => replacement,
            Err(error) => {
                self.record_restart_failure();
                return Err(error);
            }
        };
        if let Err(error) = replacement.initialize_protocol().await {
            self.record_restart_failure();
            return Err(format!("Failed to reinitialize DevTools bridge: {error}"));
        }

        let old = std::mem::replace(self, replacement);
        drop(old);
        crate::diagnostics::event("devtools_restarted");
        Ok(())
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn request(&mut self, req: &Value) -> Result<Value, String> {
        if let Some(original_id) = req.get("id").cloned() {
            // A late response must not be assigned to a new request reusing the
            // same client id (tools/list used to always use "dt-tools-list").
            let id = json!(uuid::Uuid::new_v4().to_string());
            let mut outgoing = req.clone();
            outgoing["id"] = id.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            {
                let mut map = self.pending.lock().unwrap();
                if map.closed {
                    return Err(
                        "DevTools process disconnected; restart the browser service.".into(),
                    );
                }
                // Register BEFORE writing: a local child can answer immediately.
                map.senders.insert(id.clone(), tx);
            }
            let _pending = PendingRequest {
                pending: self.pending.clone(),
                id,
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(120), async {
                self.notify(&outgoing).await?;
                rx.await
                    .map_err(|_| "DevTools process disconnected before responding.".to_string())
            })
            .await;
            match result {
                Ok(Ok(mut resp)) => {
                    resp["id"] = original_id;
                    Ok(resp)
                }
                Ok(Err(error)) => Err(error),
                Err(_) => {
                    crate::diagnostics::event("devtools_request_timeout");
                    Err("DevTools request timed out (120s)".into())
                }
            }
        } else {
            self.notify(req).await?;
            Ok(Value::Null)
        }
    }

    /// Send a notification (no id, no response expected).
    pub async fn notify(&mut self, req: &Value) -> Result<(), String> {
        if self.pending.lock().unwrap().closed {
            return Err("DevTools process disconnected".into());
        }
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let mut writing = PendingWrite {
            pending: self.pending.clone(),
            complete: false,
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await
        })
        .await;
        match result {
            Ok(Ok(())) => {
                writing.complete = true;
                Ok(())
            }
            _ => {
                // A partially written JSON line cannot safely be retried.
                let mut pending = self.pending.lock().unwrap();
                pending.closed = true;
                pending.senders.clear();
                let _ = self.child.start_kill();
                crate::diagnostics::event("devtools_stdin_failed");
                Err("DevTools stdin failed or timed out (10s)".into())
            }
        }
    }

    /// Kill the child process.
    #[allow(dead_code)]
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

impl Drop for DevtoolsBridge {
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(reader) = &self.stderr_reader {
            reader.abort();
        }
        let _ = self.child.start_kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stderr_classification_does_not_persist_raw_messages() {
        assert_eq!(
            stderr_event(b"FATAL ERROR: heap out of memory secret-url"),
            Some("devtools_stderr_memory_error")
        );
        assert_eq!(
            stderr_event(b"Error: ECONNRESET secret-url"),
            Some("devtools_stderr_connection_error")
        );
        assert_eq!(
            stderr_event(b"Error: secret-token"),
            Some("devtools_stderr_error")
        );
        assert_eq!(stderr_event(b"secret-token"), None);
    }

    fn peer(script: &str) -> Arc<Mutex<DevtoolsBridge>> {
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        DevtoolsBridge::from_child(child).unwrap()
    }

    fn restartable_peer(script: &str) -> Arc<Mutex<DevtoolsBridge>> {
        let launch = LaunchSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
        };
        let child = launch.spawn().unwrap();
        Arc::new(Mutex::new(
            DevtoolsBridge::from_child_inner(child, Some(launch)).unwrap(),
        ))
    }

    #[tokio::test]
    async fn peer_exit_fails_pending_request_promptly() {
        let bridge = peer("read line; exit 0");
        let mut bridge = bridge.lock().await;
        let req = json!({"id": "exit", "method": "tools/list"});
        let result = tokio::time::timeout(Duration::from_secs(1), bridge.request(&req)).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "EOF must fail pending calls immediately"
        );
    }

    #[tokio::test]
    async fn cancelled_request_does_not_leave_pending_sender() {
        let bridge = peer("while read line; do :; done");
        let mut bridge = bridge.lock().await;
        let req = json!({"id": "cancel", "method": "tools/list"});
        assert!(
            tokio::time::timeout(Duration::from_millis(30), bridge.request(&req))
                .await
                .is_err()
        );
        assert!(
            bridge.pending.lock().unwrap().senders.is_empty(),
            "cancelled request leaked its pending sender"
        );
    }

    #[tokio::test]
    async fn immediate_responses_keep_the_callers_id() {
        let bridge = peer("while IFS= read -r line; do printf '%s\\n' \"$line\"; done");
        for _ in 0..50 {
            let request = json!({"id": "reused-client-id", "method": "ping"});
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                DevtoolsBridge::call(&bridge, &request),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response["id"], "reused-client-id");
        }
        assert!(
            bridge
                .lock()
                .await
                .pending
                .lock()
                .unwrap()
                .senders
                .is_empty()
        );
    }

    #[tokio::test]
    async fn next_call_restarts_a_disconnected_bridge_without_replaying_failed_work() {
        let marker = std::env::temp_dir().join(format!("catdesk-devtools-restart-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "if [ ! -f '{marker}' ]; then touch '{marker}'; read line; exit 0; else while IFS= read -r line; do printf '%s\\n' \"$line\"; done; fi",
            marker = marker.display()
        );
        let bridge = restartable_peer(&script);

        let first = json!({"id": "first", "method": "side-effectful"});
        assert!(DevtoolsBridge::call(&bridge, &first).await.is_err());
        assert!(!bridge.lock().await.is_connected());

        let second = json!({"id": "second", "method": "ping"});
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            DevtoolsBridge::call(&bridge, &second),
        )
        .await
        .expect("automatic restart timed out")
        .expect("automatic restart failed");
        assert_eq!(response["id"], "second");
        assert!(bridge.lock().await.is_connected());
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn restart_backoff_is_bounded() {
        assert!(restart_backoff(1) >= Duration::from_millis(RESTART_BACKOFF_BASE_MS));
        assert!(restart_backoff(100) <= Duration::from_millis(RESTART_BACKOFF_MAX_MS));
    }

    #[tokio::test]
    async fn browser_calls_queue_behind_the_serial_bridge_instead_of_failing_busy() {
        let bridge = peer("while IFS= read -r line; do printf '%s\\n' \"$line\"; done");
        let busy = bridge.lock().await;
        let request = json!({"id": 1, "method": "ping"});
        let queued_bridge = bridge.clone();
        let queued = tokio::spawn(async move { DevtoolsBridge::call(&queued_bridge, &request).await });

        tokio::time::sleep(Duration::from_millis(2_200)).await;
        assert!(
            !queued.is_finished(),
            "temporary DevTools serialization must queue instead of returning Busy"
        );
        drop(busy);

        let response = tokio::time::timeout(Duration::from_secs(2), queued)
            .await
            .expect("queued DevTools call did not resume")
            .expect("queued task panicked")
            .expect("queued DevTools call failed");
        assert_eq!(response["id"], 1);
    }
}
