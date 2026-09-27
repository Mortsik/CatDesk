//! Pure request-lifecycle vocabulary and stream-failure correlation.
//!
//! This module owns the stage/terminal-reason vocabulary shared by the
//! scheduler (`request_workers`), the connection diagnostics registry, and the
//! HTTP middleware. It deliberately imports nothing from those modules: the
//! classifier runs over persisted diagnostic records, so it must stay a pure
//! function of JSON values (metadata-only, never MCP payloads).

use serde_json::Value;
use std::collections::BTreeSet;

/// Live stage of an in-flight request.
///
/// - `Queued`: accepted by the HTTP middleware, not yet handed to the
///   scheduler.
/// - `Dispatch`: handed to the scheduler (`run_timed`), waiting for a
///   blocking-pool thread.
/// - `Executing`: the blocking task started and the work future is running.
/// - `Responding`: the work returned and the response is being assembled.
///
/// Terminal stages (`completed`/`cancelled`) are not live stages; they are
/// derived from [`TerminalReason::stage_str`] on the finish records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestStage {
    Queued,
    Dispatch,
    Executing,
    Responding,
}

impl RequestStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatch => "dispatch",
            Self::Executing => "executing",
            Self::Responding => "responding",
        }
    }
}

/// Why a request lifecycle ended. Every terminal reason maps to a terminal
/// stage: a response was produced (`completed`, including error and timeout
/// responses) or the request future ended without one (`cancelled`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalReason {
    /// A response was produced and handed to the HTTP layer.
    Completed,
    /// The scheduler's response deadline expired; CatDesk answered 504.
    DeadlineTimeout,
    /// The request worker failed (for example the blocking task panicked).
    WorkerFailed,
    /// The client disconnected before a response was produced.
    ClientDisconnect,
    /// The request future was dropped after the server began stopping.
    ServerShutdown,
}

impl TerminalReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::DeadlineTimeout => "deadline_timeout",
            Self::WorkerFailed => "worker_failed",
            Self::ClientDisconnect => "client_disconnect",
            Self::ServerShutdown => "server_shutdown",
        }
    }

    /// Terminal stage name recorded on finish records.
    pub(crate) fn stage_str(self) -> &'static str {
        match self {
            Self::ClientDisconnect | Self::ServerShutdown => "cancelled",
            _ => "completed",
        }
    }
}

/// One request that had started but produced no terminal record by the
/// failure time: its ID and how long it had been in flight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveAtFailure {
    pub(crate) request_id: String,
    pub(crate) age_ms: u64,
}

/// CatDesk-side evidence found in a window of connection-diagnostic records
/// around a client-reported stream failure (for example "Resume stream
/// unavailable" or "Stream cache expired").
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StreamFailureCorrelation {
    /// Requests whose lifecycle ended in a CatDesk response-deadline timeout.
    pub(crate) deadline_timeouts: Vec<String>,
    /// Requests dropped without a response because the client went away.
    pub(crate) client_cancellations: Vec<String>,
    /// Requests dropped without a response during server shutdown.
    pub(crate) shutdown_cancellations: Vec<String>,
    /// Requests whose request worker failed.
    pub(crate) worker_failures: Vec<String>,
    /// Tunnel lifecycle event names observed in the window.
    pub(crate) tunnel_events: Vec<String>,
    /// `server_stopping`/`server_stopped` events observed in the window.
    pub(crate) server_stopping_events: u64,
    /// Requests that completed normally in the window (context, not failure).
    pub(crate) completed_requests: u64,
    /// Requests still in flight at the failure time, oldest first.
    pub(crate) active_at_failure: Vec<ActiveAtFailure>,
    /// Requests cut by an abrupt restart: their owning pid died before any
    /// terminal record, and a `process_started` from a different pid proved
    /// they can no longer finish. Their age is measured from start to the
    /// restart boundary, not to the failure time.
    pub(crate) lost_at_restart: Vec<ActiveAtFailure>,
}

impl StreamFailureCorrelation {
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Ranked single-label verdict. CatDesk-attributable causes outrank
    /// transport suspects, which outrank the generic disconnect symptom:
    /// server shutdown, then deadline timeout, worker failure, tunnel event,
    /// client cancellation, and only then "no CatDesk-side failure". The full
    /// struct remains the complete answer when several coincide.
    pub(crate) fn verdict(&self) -> &'static str {
        if !self.shutdown_cancellations.is_empty() || self.server_stopping_events > 0 {
            "server_shutdown"
        } else if !self.deadline_timeouts.is_empty() {
            "catdesk_timeout"
        } else if !self.worker_failures.is_empty() {
            "worker_failure"
        } else if !self.tunnel_events.is_empty() {
            "tunnel_event"
        } else if !self.client_cancellations.is_empty() {
            "client_cancellation"
        } else {
            "no_catdesk_failure"
        }
    }
}

pub(crate) fn record_timestamp_ms(record: &Value) -> Option<u64> {
    record.get("timestamp_ms").and_then(Value::as_u64)
}

fn record_request_id(record: &Value) -> Option<String> {
    record
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn record_pid(record: &Value) -> Option<u64> {
    record.get("pid").and_then(Value::as_u64)
}

/// Terminal reason of an `http_finished` record. New records carry
/// `terminal_reason`; older rotated logs predate it, so the reason is
/// reconstructed from the scheduler fields (a deadline stage proves a
/// timeout) and, failing that, defaults to a normal completion.
fn finished_terminal_reason(record: &Value) -> TerminalReason {
    if let Some(reason) = record.get("terminal_reason").and_then(Value::as_str) {
        return match reason {
            "deadline_timeout" => TerminalReason::DeadlineTimeout,
            "worker_failed" => TerminalReason::WorkerFailed,
            _ => TerminalReason::Completed,
        };
    }
    if record
        .get("scheduler_deadline_stage")
        .is_some_and(|stage| !stage.is_null())
    {
        return TerminalReason::DeadlineTimeout;
    }
    TerminalReason::Completed
}

/// Terminal reason of an `http_cancelled` record; cancellations without a
/// reason come from older logs and are attributed to the client.
fn cancelled_terminal_reason(record: &Value) -> TerminalReason {
    match record.get("terminal_reason").and_then(Value::as_str) {
        Some("server_shutdown") => TerminalReason::ServerShutdown,
        _ => TerminalReason::ClientDisconnect,
    }
}

/// Correlate one window of connection-diagnostic records with a stream
/// failure reported at `at_ms` (Unix milliseconds). Records outside
/// `[at_ms - window_ms, at_ms + window_ms]` are ignored so client clock skew
/// is absorbed by the window rather than by exact matching. The classifier
/// reads only metadata fields (`event`, `request_id`, `terminal_reason`,
/// `scheduler_deadline_stage`, `timestamp_ms`, `pid`), never payloads, and
/// does not require the records to be pre-sorted: the pid-restart boundary is
/// resolved against `process_started` timestamps, not file order.
pub(crate) fn classify_stream_failure(
    records: &[Value],
    at_ms: u64,
    window_ms: u64,
) -> StreamFailureCorrelation {
    let lower = at_ms.saturating_sub(window_ms);
    let upper = at_ms.saturating_add(window_ms);
    let in_window =
        |record: &Value| matches!(record_timestamp_ms(record), Some(t) if lower <= t && t <= upper);

    let mut correlation = StreamFailureCorrelation::default();
    let mut deadline_timeouts = BTreeSet::new();
    let mut client_cancellations = BTreeSet::new();
    let mut shutdown_cancellations = BTreeSet::new();
    let mut worker_failures = BTreeSet::new();
    let mut started: Vec<(String, u64, Option<u64>)> = Vec::new();
    let mut finished_by_id: BTreeSet<String> = BTreeSet::new();

    // "What was in flight at the failure time" needs the full history of the
    // log, not just the window: a request that started long before the window
    // and never finished is exactly the stall the window must still see.
    // Only the three lifecycle events count here; `mcp_request` and friends
    // also carry a request_id but never terminate a lifecycle. `process_started`
    // is collected in the same sweep because its pid is the restart boundary
    // below.
    let mut restarts: Vec<(u64, u64)> = Vec::new();
    for record in records {
        let event = record.get("event").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            event,
            "http_started" | "http_finished" | "http_cancelled" | "process_started"
        ) {
            continue;
        }
        let Some(timestamp) = record_timestamp_ms(record) else {
            continue;
        };
        if timestamp > at_ms {
            continue;
        }
        match event {
            "http_started" => {
                if let Some(id) = record_request_id(record) {
                    started.push((id, timestamp, record_pid(record)));
                }
            }
            "process_started" => {
                if let Some(pid) = record_pid(record) {
                    restarts.push((timestamp, pid));
                }
            }
            _ => {
                if let Some(id) = record_request_id(record) {
                    finished_by_id.insert(id);
                }
            }
        }
    }
    restarts.sort_unstable();

    // Coincident evidence, on the other hand, is window-bounded so an
    // unrelated old failure never explains a fresh stream error. It never
    // touches the in-flight maps: those belong to the full-history sweep.
    for record in records.iter().filter(|record| in_window(record)) {
        let event = record.get("event").and_then(Value::as_str).unwrap_or("");
        match event {
            "http_finished" => match finished_terminal_reason(record) {
                TerminalReason::DeadlineTimeout => {
                    if let Some(id) = record_request_id(record) {
                        deadline_timeouts.insert(id);
                    }
                }
                TerminalReason::WorkerFailed => {
                    if let Some(id) = record_request_id(record) {
                        worker_failures.insert(id);
                    }
                }
                TerminalReason::Completed => correlation.completed_requests += 1,
                _ => {}
            },
            "http_cancelled" => {
                let Some(id) = record_request_id(record) else {
                    continue;
                };
                match cancelled_terminal_reason(record) {
                    TerminalReason::ServerShutdown => shutdown_cancellations.insert(id),
                    _ => client_cancellations.insert(id),
                };
            }
            "server_stopping" | "server_stopped" => {
                correlation.server_stopping_events += 1;
            }
            _ => {
                if event.starts_with("tunnel_") {
                    correlation.tunnel_events.push(event.to_string());
                }
            }
        }
    }

    // Requests started before the failure time with no terminal record by
    // then were still in flight; their age is the leading indicator. One
    // exception: an unfinished request whose pid was followed by a
    // `process_started` from a different pid cannot finish anymore — the
    // owning process died abruptly (no `server_stopping` on that path), so
    // the request was lost at the restart boundary, not still active hours
    // later. A strict `>` keeps a request alive when its start shares a
    // millisecond with the boundary, where record order is ambiguous.
    // Legacy records without a pid carry no boundary signal and stay active.
    for (id, started_at, pid) in started {
        if finished_by_id.contains(&id) {
            continue;
        }
        let boundary = pid.and_then(|pid| {
            restarts
                .iter()
                .find(|(ts, restart_pid)| *ts > started_at && *restart_pid != pid)
        });
        if let Some((boundary_ts, _)) = boundary {
            correlation.lost_at_restart.push(ActiveAtFailure {
                request_id: id,
                age_ms: boundary_ts - started_at,
            });
        } else {
            correlation.active_at_failure.push(ActiveAtFailure {
                request_id: id,
                age_ms: at_ms - started_at,
            });
        }
    }
    correlation
        .active_at_failure
        .sort_by(|a, b| b.age_ms.cmp(&a.age_ms));
    correlation
        .lost_at_restart
        .sort_by(|a, b| b.age_ms.cmp(&a.age_ms));

    correlation.deadline_timeouts = deadline_timeouts.into_iter().collect();
    correlation.client_cancellations = client_cancellations.into_iter().collect();
    correlation.shutdown_cancellations = shutdown_cancellations.into_iter().collect();
    correlation.worker_failures = worker_failures.into_iter().collect();
    correlation
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FAILURE_AT_MS: u64 = 10_000;
    const WINDOW_MS: u64 = 5_000;

    fn started(id: &str, at_ms: u64) -> Value {
        json!({"event": "http_started", "request_id": id, "timestamp_ms": at_ms})
    }

    fn finished(id: &str, at_ms: u64, extra: Value) -> Value {
        let mut record = json!({"event": "http_finished", "request_id": id,
            "timestamp_ms": at_ms});
        if let (Some(object), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.to_string(), value.clone());
            }
        }
        record
    }

    fn cancelled(id: &str, at_ms: u64, reason: &str) -> Value {
        json!({"event": "http_cancelled", "request_id": id, "timestamp_ms": at_ms,
            "terminal_reason": reason})
    }

    #[test]
    fn quiet_window_classifies_as_no_catdesk_failure() {
        let records = vec![
            started("far-past", 1_000),
            finished("far-past", 1_050, json!({})),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "no_catdesk_failure");
        assert!(correlation.is_empty(), "{correlation:?}");
    }

    #[test]
    fn deadline_timeout_in_window_correlates() {
        let records = vec![
            started("slow", 8_000),
            finished(
                "slow",
                8_900,
                json!({"status": 504, "terminal_reason": "deadline_timeout",
                "scheduler_deadline_stage": "execution"}),
            ),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "catdesk_timeout");
        assert_eq!(correlation.deadline_timeouts, ["slow"]);
        assert_eq!(correlation.completed_requests, 0);
    }

    #[test]
    fn legacy_records_without_terminal_reason_still_classify() {
        let records = vec![
            started("legacy-timeout", 7_000),
            finished(
                "legacy-timeout",
                7_100,
                json!({"status": 504,
                "scheduler_deadline_stage": "queue"}),
            ),
            started("legacy-done", 7_500),
            finished("legacy-done", 7_600, json!({"status": 200})),
            cancelled("legacy-cancel", 7_700, "client_disconnect"),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "catdesk_timeout");
        assert_eq!(correlation.deadline_timeouts, ["legacy-timeout"]);
        assert_eq!(correlation.completed_requests, 1);
        assert_eq!(correlation.client_cancellations, ["legacy-cancel"]);
        // A legacy cancellation without a reason defaults to the client.
        let legacy = vec![json!({"event": "http_cancelled", "request_id": "old",
            "timestamp_ms": 9_000})];
        assert_eq!(
            classify_stream_failure(&legacy, FAILURE_AT_MS, WINDOW_MS).client_cancellations,
            ["old"]
        );
    }

    #[test]
    fn client_cancellation_and_tunnel_events_correlate() {
        let records = vec![
            cancelled("dropped", 9_800, "client_disconnect"),
            json!({"event": "tunnel_reconnect_waiting", "timestamp_ms": 9_900}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        // A tunnel suspect outranks the generic disconnect symptom, but both
        // buckets stay populated; the struct is the complete answer.
        assert_eq!(correlation.verdict(), "tunnel_event");
        assert_eq!(correlation.client_cancellations, ["dropped"]);
        assert_eq!(correlation.tunnel_events, ["tunnel_reconnect_waiting"]);

        let cancelled_only = vec![cancelled("dropped", 9_800, "client_disconnect")];
        assert_eq!(
            classify_stream_failure(&cancelled_only, FAILURE_AT_MS, WINDOW_MS).verdict(),
            "client_cancellation"
        );
        let tunnel_only = vec![json!({"event": "tunnel_failed", "timestamp_ms": 9_950})];
        assert_eq!(
            classify_stream_failure(&tunnel_only, FAILURE_AT_MS, WINDOW_MS).verdict(),
            "tunnel_event"
        );
    }

    #[test]
    fn server_shutdown_outranks_every_other_coincidence() {
        let records = vec![
            finished(
                "timed-out",
                9_000,
                json!({"terminal_reason": "deadline_timeout"}),
            ),
            cancelled("dropped", 9_500, "client_disconnect"),
            cancelled("aborted", 9_600, "server_shutdown"),
            json!({"event": "tunnel_failed", "timestamp_ms": 9_700}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "server_shutdown");
        assert_eq!(correlation.shutdown_cancellations, ["aborted"]);
        assert_eq!(correlation.deadline_timeouts, ["timed-out"]);
        assert_eq!(correlation.client_cancellations, ["dropped"]);
        assert_eq!(correlation.tunnel_events, ["tunnel_failed"]);
        // The stopping event alone is enough evidence.
        let stopping = vec![json!({"event": "server_stopping", "timestamp_ms": 9_800})];
        assert_eq!(
            classify_stream_failure(&stopping, FAILURE_AT_MS, WINDOW_MS).verdict(),
            "server_shutdown"
        );
    }

    #[test]
    fn worker_failure_ranks_between_timeout_and_tunnel() {
        let records = vec![
            finished(
                "panicked",
                9_000,
                json!({"status": 500,
                "terminal_reason": "worker_failed"}),
            ),
            json!({"event": "tunnel_failed", "timestamp_ms": 9_900}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "worker_failure");
        assert_eq!(correlation.worker_failures, ["panicked"]);
    }

    #[test]
    fn requests_in_flight_at_failure_are_reported_oldest_first() {
        let records = vec![
            started("old-still-running", 4_000),
            started("young-still-running", 9_500),
            started("finished-later", 8_000),
            // Still in flight AT the failure time; the terminal lands after.
            finished("finished-later", 11_000, json!({})),
            started("after-failure", 10_500),
            started("long-done", 3_000),
            finished("long-done", 4_500, json!({})),
            // Middle-of-lifecycle records carry the same request_id but must
            // not terminate it, and never appear twice in the active list.
            json!({"event": "mcp_request", "request_id": "young-still-running",
                "timestamp_ms": 9_600, "rpc_method": "tools/call"}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(
            correlation
                .active_at_failure
                .iter()
                .map(|active| active.request_id.as_str())
                .collect::<Vec<_>>(),
            ["old-still-running", "finished-later", "young-still-running"]
        );
        assert_eq!(correlation.active_at_failure[0].age_ms, 6_000);
        assert_eq!(correlation.active_at_failure[1].age_ms, 2_000);
        assert_eq!(correlation.active_at_failure[2].age_ms, 500);
    }

    #[test]
    fn restart_under_a_new_pid_cuts_unfinished_requests() {
        // The ghost-request shape from the abrupt-restart investigation: the
        // old process started a request, died without any terminal record,
        // and a new pid took over. Reading that request as "active" hours
        // after the restart is wrong — it was cut at the pid boundary.
        let records = vec![
            json!({"event": "http_started", "request_id": "ghost", "timestamp_ms": 5_800,
                "pid": 100}),
            json!({"event": "process_started", "timestamp_ms": 6_000, "pid": 200,
                "version": "0.9.2"}),
            json!({"event": "http_started", "request_id": "fresh", "timestamp_ms": 6_200,
                "pid": 200}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(
            correlation
                .lost_at_restart
                .iter()
                .map(|lost| lost.request_id.as_str())
                .collect::<Vec<_>>(),
            ["ghost"]
        );
        // Age runs from start to the restart boundary, not to the failure.
        assert_eq!(correlation.lost_at_restart[0].age_ms, 200);
        // The new process's request is genuinely still in flight.
        assert_eq!(
            correlation
                .active_at_failure
                .iter()
                .map(|active| active.request_id.as_str())
                .collect::<Vec<_>>(),
            ["fresh"]
        );
        assert_eq!(correlation.active_at_failure[0].age_ms, 3_800);
    }

    #[test]
    fn same_pid_process_started_does_not_cut_in_flight_requests() {
        // A re-opened log slot (or the concurrent directory) can re-record
        // `process_started` for the very same pid; that is not a restart.
        let records = vec![
            json!({"event": "http_started", "request_id": "live", "timestamp_ms": 9_000,
                "pid": 7}),
            json!({"event": "process_started", "timestamp_ms": 9_500, "pid": 7}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert!(correlation.lost_at_restart.is_empty(), "{correlation:?}");
        assert_eq!(
            correlation
                .active_at_failure
                .iter()
                .map(|active| active.request_id.as_str())
                .collect::<Vec<_>>(),
            ["live"]
        );
    }

    #[test]
    fn finished_after_the_boundary_is_not_lost_at_restart() {
        // Overlapping processes: the old pid finished its request after the
        // new pid had already started. A terminal record absolves the request.
        let records = vec![
            json!({"event": "http_started", "request_id": "slow", "timestamp_ms": 5_000,
                "pid": 100}),
            json!({"event": "process_started", "timestamp_ms": 6_000, "pid": 200}),
            json!({"event": "http_finished", "request_id": "slow", "timestamp_ms": 6_500,
                "pid": 100, "status": 200}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert!(correlation.lost_at_restart.is_empty(), "{correlation:?}");
        assert!(correlation.active_at_failure.is_empty(), "{correlation:?}");
    }

    #[test]
    fn records_outside_the_window_are_ignored() {
        let records = vec![
            finished(
                "too-early",
                4_900,
                json!({"terminal_reason": "deadline_timeout"}),
            ),
            finished(
                "too-late",
                15_100,
                json!({"terminal_reason": "deadline_timeout"}),
            ),
            json!({"event": "tunnel_failed", "timestamp_ms": 4_000}),
        ];
        let correlation = classify_stream_failure(&records, FAILURE_AT_MS, WINDOW_MS);
        assert_eq!(correlation.verdict(), "no_catdesk_failure");
        assert!(correlation.is_empty(), "{correlation:?}");
    }

    #[test]
    fn terminal_reason_vocabulary_maps_to_terminal_stages() {
        assert_eq!(TerminalReason::Completed.stage_str(), "completed");
        assert_eq!(TerminalReason::DeadlineTimeout.stage_str(), "completed");
        assert_eq!(TerminalReason::WorkerFailed.stage_str(), "completed");
        assert_eq!(TerminalReason::ClientDisconnect.stage_str(), "cancelled");
        assert_eq!(TerminalReason::ServerShutdown.stage_str(), "cancelled");
        assert_eq!(TerminalReason::DeadlineTimeout.as_str(), "deadline_timeout");
    }

    #[test]
    fn stage_vocabulary_is_stable() {
        assert_eq!(RequestStage::Queued.as_str(), "queued");
        assert_eq!(RequestStage::Dispatch.as_str(), "dispatch");
        assert_eq!(RequestStage::Executing.as_str(), "executing");
        assert_eq!(RequestStage::Responding.as_str(), "responding");
    }
}
