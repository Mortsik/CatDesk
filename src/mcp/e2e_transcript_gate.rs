//! End-to-end transcript-footprint and capability regression gate
//! (bead catdesk-ojt.9, epic "Context/output efficiency without capability
//! loss").
//!
//! Runs the five representative workflow families through the REAL tools/call
//! handler and response budget — no stubs above the tool layer, no wall-clock
//! assertions (every wait is a bounded rendezvous loop over observable state):
//!
//! (a) a large run_command whose stdout+stderr are externalized and later
//!     reconstructed byte-for-byte through read_result,
//! (b) a file workflow (write a large file, search for a needle, read the
//!     file back) with full-content reconstruction,
//! (c) a browser/DevTools workflow through a fake peer: bounded input
//!     defaults (take_snapshot non-verbose, bounded listing pages) and a
//!     large network body externalized and reconstructed,
//! (d) error diagnostics: a failing command keeps its diagnostic text
//!     visible and bounded,
//! (e) repeated polling: every poll answer stays inside the inline budget
//!     and the job's outcome is correct.
//!
//! For every workflow the gate asserts BOTH axes at once:
//!   footprint — the serialized inline result (what a transcript pays for)
//!     stays within the shared inline budget, and wherever the budget
//!     externalized, raw bytes exceed the inline bytes (reported as a
//!     reduction percentage);
//!   capability — externalized results reconstruct byte-for-byte and the
//!     task's observable outcome (found needle, job output, echoed argument
//!     defaults, diagnostic text) is correct.
//!
//! The gate therefore FAILS if an oversized payload bypasses the budget
//! (inline bound blows) or if required data becomes unrecoverable
//! (reconstruction diverges). Both bites are verified against deliberately
//! broken builds in the bead's verification log, not assumed.

use base64::Engine as _;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::mcp::jsonrpc::JsonRpcRequest;
use crate::mcp::response_budget::DEFAULT_INLINE_RESPONSE_BYTES;
use crate::mcp::{JsonRpcResponse, handle_tools_call_with_result_store};
use crate::result_store::{DEFAULT_MAX_RANGE_BYTES, LargeResultStore};
use crate::state::{Mode, ShowDetailMode, ToolMode};
use tokio::sync::Mutex;

/// One workflow's footprint row for the summary report.
struct Footprint {
    workflow: &'static str,
    detail: String,
    raw_bytes: u64,
    inline_bytes: u64,
    externalized: bool,
}

fn print_footprint_report(footprints: &[Footprint]) {
    eprintln!("== transcript gate: inline-byte reduction ==");
    for row in footprints {
        let reduction = if row.externalized && row.raw_bytes > 0 {
            format!(
                "{:3.0}%",
                100.0 - row.inline_bytes as f64 / row.raw_bytes as f64 * 100.0
            )
        } else {
            "  -".to_string()
        };
        eprintln!(
            "  {:<14} {:<34} raw {:>9} B  inline {:>7} B  reduced {}",
            row.workflow, row.detail, row.raw_bytes, row.inline_bytes, reduction
        );
    }
}

/// Assert the shared footprint invariants for one tool-result row.
fn assert_footprint(row: &Footprint) {
    assert!(
        row.inline_bytes <= DEFAULT_INLINE_RESPONSE_BYTES as u64,
        "[{}] inline result {} B exceeds the {} B budget — an oversized \
         payload bypassed the response budget",
        row.workflow,
        row.inline_bytes,
        DEFAULT_INLINE_RESPONSE_BYTES
    );
    if row.externalized {
        assert!(
            row.raw_bytes > row.inline_bytes,
            "[{}] externalized result ({}) reported raw {} B <= inline {} B — \
             the budget externalized without reducing the transcript",
            row.workflow,
            row.detail,
            row.raw_bytes,
            row.inline_bytes
        );
    }
}

fn tool_call_request(name: &str, arguments: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!("gate")),
        method: "tools/call".to_string(),
        params: json!({
            "name": name,
            "arguments": arguments,
        }),
    }
}

/// Shared harness: the real tools/call handler with one workspace, one
/// result store, and one optional DevTools bridge, exactly like production
/// wiring minus the HTTP transport (whose failure modes the soak suite owns).
struct GateHarness {
    workspace_root: String,
    store: LargeResultStore,
    jobs: CommandJobManager,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
}

impl GateHarness {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("catdesk-gate-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create gate workspace");
        Self {
            workspace_root: root.to_string_lossy().into_owned(),
            store: LargeResultStore::new_default().expect("create gate store"),
            jobs: CommandJobManager::new(),
            devtools: None,
        }
    }

    fn with_devtools(mut self, bridge: Arc<Mutex<DevtoolsBridge>>) -> Self {
        self.devtools = Some(bridge);
        self
    }

    async fn call(&self, name: &str, arguments: Value) -> JsonRpcResponse {
        let req = tool_call_request(name, arguments);
        handle_tools_call_with_result_store(
            &req,
            &self.workspace_root,
            1,
            Mode::Both,
            ToolMode::MultiTools,
            false,
            &self.jobs,
            &self.devtools,
            ShowDetailMode::Disable,
            &self.store,
            Some("gate-session"),
            None,
        )
        .await
    }

    /// Serialized size of the inline result — the transcript's real cost.
    fn inline_bytes(response: &JsonRpcResponse) -> u64 {
        response
            .result
            .as_ref()
            .and_then(|result| serde_json::to_vec(result).ok())
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(0)
    }

    fn structured<'a>(response: &'a JsonRpcResponse) -> &'a Value {
        response
            .result
            .as_ref()
            .and_then(|result| result.get("structuredContent"))
            .unwrap_or_else(|| panic!("missing structuredContent"))
    }

    fn output_ref(response: &JsonRpcResponse) -> String {
        response
            .result
            .as_ref()
            .and_then(|result| result.pointer("/responseBudget/outputRef"))
            .and_then(Value::as_str)
            .expect("expected the budget to externalize this result")
            .to_string()
    }

    fn raw_bytes(response: &JsonRpcResponse) -> u64 {
        response
            .result
            .as_ref()
            .and_then(|result| result.pointer("/responseBudget/originalBytes"))
            .and_then(Value::as_u64)
            .expect("expected responseBudget.originalBytes on an externalized result")
    }

    fn externalized(response: &JsonRpcResponse) -> bool {
        response
            .result
            .as_ref()
            .and_then(|result| result.pointer("/responseBudget/outputRef"))
            .and_then(Value::as_str)
            .is_some()
    }

    /// Capability axis: pull the externalized payload back through read_result
    /// until EOF and return the exact stored bytes.
    async fn reconstruct(&self, output_ref: &str) -> Vec<u8> {
        let mut rebuilt = Vec::new();
        let mut offset = 0_u64;
        loop {
            let response = self
                .call(
                    "read_result",
                    json!({
                        "result_id": output_ref,
                        "offset": offset,
                        "max_bytes": DEFAULT_MAX_RANGE_BYTES
                    }),
                )
                .await;
            let range = response
                .result
                .as_ref()
                .and_then(|result| result.get("structuredContent"))
                .unwrap_or_else(|| {
                    panic!(
                        "read_result range missing structuredContent: error={:?}",
                        response.error.as_ref().map(|e| e.message.clone())
                    )
                });
            let encoded = range
                .get("dataBase64")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!("missing dataBase64 in range at offset {offset}: {range:?}")
                });
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
                return rebuilt;
            }
        }
    }

    /// Footprint row for a response that MUST be externalized, including the
    /// shared invariants.
    fn require_externalized(
        &self,
        workflow: &'static str,
        detail: &str,
        response: &JsonRpcResponse,
    ) -> Footprint {
        assert!(
            Self::externalized(response),
            "[{workflow}] {detail:} was NOT externalized — the response budget \
             let a large payload through inline"
        );
        let row = Footprint {
            workflow,
            detail: detail.to_string(),
            raw_bytes: Self::raw_bytes(response),
            inline_bytes: Self::inline_bytes(response),
            externalized: true,
        };
        assert_footprint(&row);
        row
    }

    fn bounded_row(workflow: &'static str, detail: &str, response: &JsonRpcResponse) -> Footprint {
        let row = Footprint {
            workflow,
            detail: detail.to_string(),
            raw_bytes: 0,
            inline_bytes: Self::inline_bytes(response),
            externalized: false,
        };
        assert_footprint(&row);
        row
    }
}

impl Drop for GateHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(Path::new(&self.workspace_root));
    }
}

/// Deterministic filler: distinct lines so no two lines are equal and the
/// content is reproducible run to run.
fn filler_lines(count: usize, marker: &str) -> String {
    let mut out = String::with_capacity(count * 48);
    for index in 0..count {
        out.push_str(&format!("{marker} {index:06} - gate filler line\n"));
    }
    out
}

// ── (a) large run_command: externalize + byte-for-byte retrieval ──────────

async fn workflow_large_command(harness: &GateHarness, footprints: &mut Vec<Footprint>) {
    let stdout = format!(
        "GATE-STDOUT-HEAD\n{}GATE-STDOUT-TAIL\n",
        filler_lines(24_000, "S")
    );
    let stderr = format!(
        "GATE-STDERR-HEAD\n{}GATE-STDERR-TAIL\n",
        filler_lines(24_000, "E")
    );
    // Generate the large streams in-shell: a multi-megabyte argv would hit
    // E2BIG, and the expected strings above are exactly what these awk
    // loops emit (ASCII filler, %06d index).
    let command = concat!(
        "printf 'GATE-STDOUT-HEAD\\n'; ",
        "awk 'BEGIN { for (i = 0; i < 24000; i++) printf \"S %06d - gate filler line\\n\", i }'; ",
        "printf 'GATE-STDOUT-TAIL\\n'; ",
        "{ printf 'GATE-STDERR-HEAD\\n'; ",
        "awk 'BEGIN { for (i = 0; i < 24000; i++) printf \"E %06d - gate filler line\\n\", i }'; ",
        "printf 'GATE-STDERR-TAIL\\n'; } >&2"
    );
    let response = harness
        .call("run_command", json!({ "command": command }))
        .await;
    assert_ne!(
        response.result.as_ref().and_then(|r| r.get("isError")),
        Some(&json!(true)),
        "the large command must succeed"
    );
    let row = harness.require_externalized("run_command", "large stdout+stderr", &response);
    assert!(
        row.raw_bytes >= 8 * row.inline_bytes,
        "[run_command] raw {} B vs inline {} B — reduction collapsed",
        row.raw_bytes,
        row.inline_bytes
    );

    // Capability: the stored JSON reconstructs and carries both streams intact.
    let rebuilt = harness
        .reconstruct(&GateHarness::output_ref(&response))
        .await;
    let full: Value = serde_json::from_slice(&rebuilt).expect("parse reconstructed result");
    let structured = full.get("structuredContent").expect("structuredContent");
    let got_stdout = structured
        .get("stdout")
        .and_then(Value::as_str)
        .expect("stdout");
    let got_stderr = structured
        .get("stderr")
        .and_then(Value::as_str)
        .expect("stderr");
    assert_eq!(got_stdout, stdout, "stdout must reconstruct byte-for-byte");
    assert_eq!(got_stderr, stderr, "stderr must reconstruct byte-for-byte");
    footprints.push(row);
}

// ── (b) file workflow: write big, find the needle, read it back whole ─────

async fn workflow_files(harness: &GateHarness, footprints: &mut Vec<Footprint>) {
    let needle = "GATE-NEEDLE-9f3ab2";
    let content = format!(
        "GATE-FILE-HEAD\n{}{}: the needle lives here\nGATE-FILE-TAIL\n",
        filler_lines(17_000, "F"),
        needle
    );

    let write = harness
        .call(
            "write",
            json!({ "path": "gate/big.txt", "content": content, "create_dirs": true }),
        )
        .await;
    assert_ne!(
        write.result.as_ref().and_then(|r| r.get("isError")),
        Some(&json!(true)),
        "write must succeed: {:?}",
        write.result
    );
    footprints.push(GateHarness::bounded_row(
        "files",
        "write acknowledgment",
        &write,
    ));

    // Capability: search must still find the needle (a budget must never
    // erase discoverability).
    let search = harness
        .call("search", json!({ "pattern": needle, "path": "gate" }))
        .await;
    let search_text = serde_json::to_string(&search.result).expect("serialize search");
    assert!(
        search_text.contains("big.txt") && search_text.contains(needle),
        "search must locate the needle in the big file"
    );
    footprints.push(GateHarness::bounded_row(
        "files",
        "needle search hit",
        &search,
    ));

    // Big read must externalize and reconstruct byte-for-byte.
    let read = harness
        .call("read", json!({ "paths": ["gate/big.txt"] }))
        .await;
    let row = harness.require_externalized("files", "read big.txt", &read);
    let rebuilt = harness.reconstruct(&GateHarness::output_ref(&read)).await;
    let full: Value = serde_json::from_slice(&rebuilt).expect("parse reconstructed read");
    let got = full
        .pointer("/structuredContent/files/0/text")
        .and_then(Value::as_str)
        .expect("read content text");
    assert_eq!(got, content, "file content must reconstruct byte-for-byte");
    footprints.push(row);
}

// ── (c) browser/DevTools: bounded input defaults + big body retrieval ─────

/// Fake DevTools peer: echoes request arguments back as the tool text so the
/// gate can assert the exact arguments that reached the peer, and serves a
/// large deterministic body for get_network_request.
fn spawn_fake_devtools_peer() -> Arc<Mutex<DevtoolsBridge>> {
    let child = tokio::process::Command::new("python3")
        .args([
            "-c",
            r#"import json,sys
for line in sys.stdin:
    req=json.loads(line)
    name=req.get('params',{}).get('name','')
    args=req.get('params',{}).get('arguments',{})
    if name=='get_network_request':
        body='GATE-NET-HEAD\n' + ('0123456789abcdef' * 8192) + '\nGATE-NET-TAIL'
        text=body
    else:
        text=json.dumps(args,sort_keys=True)
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':{'content':[{'type':'text','text':text}]}}), flush=True)
"#,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn fake DevTools peer");
    DevtoolsBridge::from_child(child).expect("create DevTools bridge")
}

#[cfg(unix)]
async fn workflow_devtools(harness: &GateHarness, footprints: &mut Vec<Footprint>) {
    // Input-side bounding must reach the peer as exact arguments (ojt.4).
    let snapshot = harness.call("take_snapshot", json!({})).await;
    let snapshot_args: Value = serde_json::from_str(
        snapshot
            .result
            .as_ref()
            .and_then(|result| result.pointer("/content/0/text"))
            .and_then(Value::as_str)
            .expect("echoed snapshot arguments"),
    )
    .expect("decode echoed arguments");
    assert_eq!(snapshot_args["verbose"], json!(false));

    let listing = harness
        .call("list_console_messages", json!({ "pageIdx": 2 }))
        .await;
    let listing_args: Value = serde_json::from_str(
        listing
            .result
            .as_ref()
            .and_then(|result| result.pointer("/content/0/text"))
            .and_then(Value::as_str)
            .expect("echoed listing arguments"),
    )
    .expect("decode echoed arguments");
    assert_eq!(listing_args["pageSize"], json!(100));
    assert_eq!(listing_args["pageIdx"], json!(2));
    footprints.push(GateHarness::bounded_row(
        "devtools",
        "snapshot + listing (input bounds)",
        &listing,
    ));

    // Output side: a large network body must externalize and reconstruct.
    let network = harness
        .call("get_network_request", json!({ "reqid": 41 }))
        .await;
    let row = harness.require_externalized("devtools", "large network body", &network);
    let rebuilt = harness
        .reconstruct(&GateHarness::output_ref(&network))
        .await;
    let full: Value = serde_json::from_slice(&rebuilt).expect("parse reconstructed body");
    let body = full
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .expect("stored network body");
    let expected = format!(
        "GATE-NET-HEAD\n{}\nGATE-NET-TAIL",
        "0123456789abcdef".repeat(8192)
    );
    assert_eq!(
        body, expected,
        "network body must reconstruct byte-for-byte"
    );
    footprints.push(row);
}

// ── (d) error diagnostics stay visible and bounded ────────────────────────

async fn workflow_error_diagnostics(harness: &GateHarness, footprints: &mut Vec<Footprint>) {
    let response = harness
        .call(
            "run_command",
            json!({ "command": "echo gate-boom-diagnostic >&2; exit 7" }),
        )
        .await;
    let structured = GateHarness::structured(&response);
    assert_eq!(
        structured.get("exitCode").and_then(Value::as_i64),
        Some(7),
        "the failing exit code must stay observable"
    );
    let transcript = serde_json::to_string(&response.result).expect("serialize error result");
    assert!(
        transcript.contains("gate-boom-diagnostic"),
        "the diagnostic text must stay visible to the model"
    );
    footprints.push(GateHarness::bounded_row(
        "diagnostics",
        "failing command error text",
        &response,
    ));
}

// ── (e) repeated polling: every answer bounded, outcome correct ───────────

async fn workflow_repeated_polling(harness: &GateHarness, footprints: &mut Vec<Footprint>) {
    let start = harness
        .call(
            "start_command",
            json!({ "command": "printf gate-poll-done" }),
        )
        .await;
    let structured = GateHarness::structured(&start);
    let job_id = structured
        .get("jobId")
        .and_then(Value::as_str)
        .expect("jobId")
        .to_string();
    let mut cursor = structured
        .get("nextCursor")
        .and_then(Value::as_u64)
        .expect("initial cursor");

    // Bounded rendezvous: iterate over observable state, never wall clock.
    let mut polls = 0_u32;
    let mut max_poll_inline = 0_u64;
    let mut collected_output = String::new();
    loop {
        polls += 1;
        assert!(polls <= 500, "job never reached a terminal state");
        let poll = harness
            .call(
                "poll_command",
                json!({ "job_id": job_id, "after": cursor, "wait_ms": 0 }),
            )
            .await;
        let inline = GateHarness::inline_bytes(&poll);
        max_poll_inline = max_poll_inline.max(inline);
        let structured = GateHarness::structured(&poll);
        cursor = structured
            .get("nextCursor")
            .and_then(Value::as_u64)
            .expect("poll cursor");
        if let Some(events) = structured.get("events").and_then(Value::as_array) {
            for event in events {
                if event.get("stream").and_then(Value::as_str) == Some("stdout") {
                    collected_output.push_str(
                        event
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
            }
        }
        if matches!(
            structured.get("state").and_then(Value::as_str),
            Some("succeeded" | "failed")
        ) {
            break;
        }
    }
    let final_output = collected_output;
    assert_eq!(final_output, "gate-poll-done", "job output must be correct");
    assert!(
        polls >= 2,
        "the gate must exercise REPEATED polling, got {polls} poll"
    );
    assert!(
        max_poll_inline <= DEFAULT_INLINE_RESPONSE_BYTES as u64,
        "a poll answer reached {max_poll_inline} B — repeated polling must \
         stay inside the inline budget"
    );
    footprints.push(Footprint {
        workflow: "polling",
        detail: format!("{polls} polls, max answer"),
        raw_bytes: 0,
        inline_bytes: max_poll_inline,
        externalized: false,
    });
}

#[cfg(unix)]
#[tokio::test]
async fn transcript_gate_bounds_footprint_and_keeps_capability() {
    let mut footprints = Vec::new();

    let command_harness = GateHarness::new("command");
    workflow_large_command(&command_harness, &mut footprints).await;

    let file_harness = GateHarness::new("files");
    workflow_files(&file_harness, &mut footprints).await;

    let devtools_harness = GateHarness::new("devtools").with_devtools(spawn_fake_devtools_peer());
    workflow_devtools(&devtools_harness, &mut footprints).await;

    let error_harness = GateHarness::new("errors");
    workflow_error_diagnostics(&error_harness, &mut footprints).await;

    let poll_harness = GateHarness::new("polling");
    workflow_repeated_polling(&poll_harness, &mut footprints).await;

    print_footprint_report(&footprints);

    // Aggregate: the externalizing workflows must show real reduction, not a
    // rounding accident.
    let externalized_rows: Vec<&Footprint> =
        footprints.iter().filter(|row| row.externalized).collect();
    assert!(
        externalized_rows.len() >= 3,
        "expected the command, file-read, and DevTools workflows to externalize"
    );
    for row in &externalized_rows {
        assert!(
            row.raw_bytes >= 2 * row.inline_bytes,
            "[{}] reduction collapsed: raw {} B vs inline {} B",
            row.workflow,
            row.raw_bytes,
            row.inline_bytes
        );
    }
}
