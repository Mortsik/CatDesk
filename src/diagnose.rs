//! `catdesk diagnose` — offline stream-failure correlation over the persisted
//! connection logs. This is the smallest durable caller of
//! [`request_lifecycle::classify_stream_failure`]: it reads the production
//! log directory read-only (current file, both rotations, and the
//! overlapping-restart `concurrent/` slot), never starts the TUI, and never
//! opens the diagnostics writer.

use crate::request_lifecycle::{classify_stream_failure, record_timestamp_ms};
use serde_json::Value;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
usage: catdesk diagnose [--recent <30m>] [--at <RFC3339>] [--window <30s>] [--logs-dir <PATH>]

Correlates ~/.catdesk/logs connection records around a client-reported
stream failure and prints the classifier verdict.

  --recent <DUR>   reference time is now minus DUR (e.g. 30m, 90s, 500ms, 2h)
  --at <TIMESTAMP> reference time as RFC 3339 (e.g. 2026-09-22T14:03:00Z)
  --window <DUR>   coincidence window either side of the reference (default 30s)
  --logs-dir <P>   log directory (default ~/.catdesk/logs)
";

const DEFAULT_RECENT_MS: u64 = 30 * 60 * 1000;
const DEFAULT_WINDOW_MS: u64 = 30 * 1000;

/// Rotation slots, oldest first; both under the primary root and under the
/// `concurrent/` root an overlapping restart writes to.
const ROTATION_FILES: [&str; 3] = [
    "connections.2.jsonl",
    "connections.1.jsonl",
    "connections.jsonl",
];

struct Config {
    at_ms: Option<u64>,
    recent_ms: Option<u64>,
    window_ms: u64,
    logs_dir: Option<PathBuf>,
}

/// Run the subcommand with the arguments following `diagnose`. Returns the
/// report (or the usage text for `--help`); errors are user-facing strings.
pub(crate) fn run(args: &[String]) -> Result<String, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(USAGE.trim_end().to_string());
    }
    let config = parse_args(args)?;
    let (at_ms, reference_note) = resolve_reference_time(&config)?;
    let logs_dir = config.logs_dir.unwrap_or_else(default_logs_dir);
    let (records, skipped_lines) = load_records(&logs_dir).map_err(|error| {
        format!(
            "CatDesk: cannot read diagnostics from {}: {error}",
            logs_dir.display()
        )
    })?;
    if records.is_empty() {
        return Err(format!(
            "CatDesk: no connection records found in {}",
            logs_dir.display()
        ));
    }
    let correlation = classify_stream_failure(&records, at_ms, config.window_ms);
    Ok(format_report(
        &logs_dir,
        &records,
        skipped_lines,
        at_ms,
        config.window_ms,
        &reference_note,
        &correlation,
    ))
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        at_ms: None,
        recent_ms: None,
        window_ms: DEFAULT_WINDOW_MS,
        logs_dir: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("CatDesk diagnose: {arg} requires a value\n\n{USAGE}"))?;
        match arg {
            "--recent" => {
                config.recent_ms = Some(parse_duration_ms(value)?);
                index += 2;
            }
            "--at" => {
                config.at_ms = Some(parse_rfc3339_ms(value)?);
                index += 2;
            }
            "--window" => {
                config.window_ms = parse_duration_ms(value)?;
                index += 2;
            }
            "--logs-dir" => {
                config.logs_dir = Some(PathBuf::from(value));
                index += 2;
            }
            _ => {
                return Err(format!(
                    "CatDesk diagnose: unknown argument '{arg}'\n\n{USAGE}"
                ));
            }
        }
    }
    if config.at_ms.is_some() && config.recent_ms.is_some() {
        return Err(format!(
            "CatDesk diagnose: --at and --recent are mutually exclusive\n\n{USAGE}"
        ));
    }
    Ok(config)
}

fn resolve_reference_time(config: &Config) -> Result<(u64, String), String> {
    if let Some(at_ms) = config.at_ms {
        return Ok((at_ms, format!("--at {}", format_timestamp(at_ms))));
    }
    let now_ms = current_unix_ms();
    let recent_ms = config.recent_ms.unwrap_or(DEFAULT_RECENT_MS);
    Ok((
        now_ms.saturating_sub(recent_ms),
        format!("--recent {}", format_age(recent_ms)),
    ))
}

fn default_logs_dir() -> PathBuf {
    crate::state::user_home_dir()
        .map(|home| home.join(".catdesk").join("logs"))
        .unwrap_or_else(|_| PathBuf::from(".catdesk").join("logs"))
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `500ms`, `30s`, `10m`, `2h` — a count and one unit suffix, no compound
/// forms, so a mistyped number fails loudly instead of being reinterpreted.
fn parse_duration_ms(text: &str) -> Result<u64, String> {
    let (digits, unit) = text.split_at(
        text.find(|c: char| c.is_ascii_alphabetic())
            .ok_or_else(|| {
                format!("CatDesk diagnose: duration '{text}' needs a unit (ms|s|m|h)")
            })?,
    );
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => {
            return Err(format!(
                "CatDesk diagnose: unknown duration unit '{unit}' in '{text}' (ms|s|m|h)"
            ));
        }
    };
    digits
        .parse::<u64>()
        .map(|count| count.saturating_mul(multiplier))
        .map_err(|_| format!("CatDesk diagnose: invalid duration '{text}'"))
}

fn parse_rfc3339_ms(text: &str) -> Result<u64, String> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .map_err(|error| {
            format!(
                "CatDesk diagnose: invalid --at timestamp '{text}' \
                 (expected RFC 3339, e.g. 2026-09-22T14:03:00Z): {error}"
            )
        })
        .and_then(|moment| {
            let nanos = moment.unix_timestamp_nanos();
            if nanos < 0 {
                return Err(format!(
                    "CatDesk diagnose: --at timestamp '{text}' predates the Unix epoch"
                ));
            }
            Ok((nanos / 1_000_000) as u64)
        })
}

fn format_timestamp(ms: u64) -> String {
    match time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
        Ok(moment) => moment
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| format!("{ms} ms")),
        Err(_) => format!("{ms} ms"),
    }
}

fn format_age(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if ms < 1_000 {
        format!("{ms} ms")
    } else if seconds < 60.0 {
        format!("{seconds:.1} s")
    } else if seconds < 3_600.0 {
        format!("{:.1} min", seconds / 60.0)
    } else {
        format!("{:.1} h", seconds / 3_600.0)
    }
}

/// Read every rotation slot that exists, tolerate unparsable lines, and
/// merge into one nondecreasing-timestamp stream so pid-boundary analysis
/// across rotations sees process history in order.
fn load_records(logs_dir: &Path) -> std::io::Result<(Vec<Value>, u64)> {
    let mut records = Vec::new();
    let mut skipped_lines = 0u64;
    for root in [logs_dir, &logs_dir.join("concurrent")] {
        if !root.is_dir() {
            continue;
        }
        for name in ROTATION_FILES {
            let text = match std::fs::read_to_string(root.join(name)) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str(line) {
                    Ok(record) => records.push(record),
                    Err(_) => skipped_lines += 1,
                }
            }
        }
    }
    records.sort_by_key(record_timestamp_ms);
    Ok((records, skipped_lines))
}

fn format_report(
    logs_dir: &Path,
    records: &[Value],
    skipped_lines: u64,
    at_ms: u64,
    window_ms: u64,
    reference_note: &str,
    correlation: &crate::request_lifecycle::StreamFailureCorrelation,
) -> String {
    let mut report = String::new();
    let skipped_note = if skipped_lines > 0 {
        format!(", {skipped_lines} unparsable lines skipped")
    } else {
        String::new()
    };
    report.push_str("CatDesk diagnose\n");
    report.push_str(&format!(
        "  logs             : {} ({} records{})\n",
        logs_dir.display(),
        records.len(),
        skipped_note
    ));
    report.push_str(&format!(
        "  reference time   : {} ({})\n",
        format_timestamp(at_ms),
        reference_note
    ));
    report.push_str(&format!(
        "  window           : ±{}\n",
        format_age(window_ms)
    ));
    report.push_str(&format!("  verdict          : {}\n", correlation.verdict()));

    report.push_str("\n  evidence in window:\n");
    report.push_str(&format!(
        "    deadline timeouts      : {}\n",
        summarize_ids(&correlation.deadline_timeouts)
    ));
    report.push_str(&format!(
        "    client cancellations   : {}\n",
        summarize_ids(&correlation.client_cancellations)
    ));
    report.push_str(&format!(
        "    shutdown cancellations : {}\n",
        summarize_ids(&correlation.shutdown_cancellations)
    ));
    report.push_str(&format!(
        "    worker failures        : {}\n",
        summarize_ids(&correlation.worker_failures)
    ));
    report.push_str(&format!(
        "    tunnel events          : {}\n",
        summarize_events(&correlation.tunnel_events)
    ));
    report.push_str(&format!(
        "    completed requests     : {}\n",
        correlation.completed_requests
    ));
    report.push_str(&format!(
        "    server stopping events : {}\n",
        correlation.server_stopping_events
    ));

    report.push_str(&format!(
        "\n  lost at restart  : {}\n",
        correlation.lost_at_restart.len()
    ));
    for lost in correlation.lost_at_restart.iter().take(5) {
        report.push_str(&format!(
            "    {} (in flight {} when an abrupt restart cut it)\n",
            lost.request_id,
            format_age(lost.age_ms)
        ));
    }
    report.push_str(&format!(
        "  active at failure: {}\n",
        correlation.active_at_failure.len()
    ));
    for active in correlation.active_at_failure.iter().take(5) {
        report.push_str(&format!(
            "    {} (age {})\n",
            active.request_id,
            format_age(active.age_ms)
        ));
    }
    if correlation.is_empty() {
        report.push_str("\n  No coincident CatDesk evidence; suspect the client or the network.\n");
    }
    report
}

fn summarize_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return "0".to_string();
    }
    let shown = ids.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if ids.len() > 5 {
        format!("{} [{shown}, …]", ids.len())
    } else {
        format!("{} [{shown}]", ids.len())
    }
}

fn summarize_events(events: &[String]) -> String {
    if events.is_empty() {
        return "0".to_string();
    }
    let shown = events
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if events.len() > 5 {
        format!("{} [{shown}, …]", events.len())
    } else {
        format!("{} [{shown}]", events.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 2027-01-15T13:20:00Z — a fixed, readable reference for fixtures.
    const BASE_MS: u64 = 1_800_019_200_000;
    const BASE_ISO: &str = "2027-01-15T13:20:00Z";

    fn write_fixture(dir: &Path, name: &str, records: &[Value]) {
        std::fs::create_dir_all(dir).expect("create fixture dir");
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        std::fs::write(dir.join(name), text).expect("write fixture");
    }

    fn diagnose(dir: &Path, extra_args: &[&str]) -> Result<String, String> {
        let args = vec!["--logs-dir".to_string(), dir.to_string_lossy().into_owned()]
            .into_iter()
            .chain(extra_args.iter().map(|arg| arg.to_string()))
            .collect::<Vec<_>>();
        run(&args)
    }

    fn started(id: &str, at_ms: u64, pid: u64) -> Value {
        json!({"event": "http_started", "request_id": id, "timestamp_ms": at_ms, "pid": pid})
    }

    fn finished(id: &str, at_ms: u64, pid: u64, terminal_reason: &str) -> Value {
        json!({"event": "http_finished", "request_id": id, "timestamp_ms": at_ms,
            "pid": pid, "terminal_reason": terminal_reason})
    }

    fn cancelled(id: &str, at_ms: u64, pid: u64, terminal_reason: &str) -> Value {
        json!({"event": "http_cancelled", "request_id": id, "timestamp_ms": at_ms,
            "pid": pid, "terminal_reason": terminal_reason})
    }

    #[test]
    fn duration_parser_accepts_units_and_rejects_the_rest() {
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
        assert_eq!(parse_duration_ms("30s").unwrap(), 30_000);
        assert_eq!(parse_duration_ms("10m").unwrap(), 600_000);
        assert_eq!(parse_duration_ms("2h").unwrap(), 7_200_000);
        assert!(parse_duration_ms("30").is_err());
        assert!(parse_duration_ms("30x").is_err());
        assert!(parse_duration_ms("min").is_err());
    }

    #[test]
    fn rfc3339_parser_matches_known_epoch() {
        assert_eq!(parse_rfc3339_ms(BASE_ISO).unwrap(), BASE_MS);
        assert_eq!(
            parse_rfc3339_ms("2027-01-15T13:20:30Z").unwrap(),
            BASE_MS + 30_000
        );
        // An explicit offset denotes the same instant as its UTC rendering.
        assert_eq!(
            parse_rfc3339_ms("2027-01-15T14:20:00+01:00").unwrap(),
            BASE_MS
        );
        assert!(parse_rfc3339_ms("not a timestamp").is_err());
        assert!(parse_rfc3339_ms("2026-09-22 14:03:00").is_err());
    }

    #[test]
    fn argument_parsing_rejects_conflicts_and_unknown_flags() {
        let at = BASE_ISO.to_string();
        assert!(parse_args(&["--at".into(), at.clone()]).is_ok());
        assert!(parse_args(&["--recent".into(), "5m".into()]).is_ok());
        assert!(
            parse_args(&["--at".into(), at, "--recent".into(), "5m".into()]).is_err(),
            "--at with --recent must be rejected"
        );
        assert!(parse_args(&["--wat".into(), "5m".into()]).is_err());
        assert!(parse_args(&["--window".into()]).is_err(), "missing value");
    }

    #[test]
    fn deadline_window_fixture_verdicts_catdesk_timeout() {
        let dir = std::env::temp_dir().join(format!("catdesk-diagnose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_fixture(
            &dir,
            "connections.jsonl",
            &[
                json!({"event": "process_started", "timestamp_ms": BASE_MS - 120_000,
                    "pid": 7}),
                started("stalled", BASE_MS - 40_000, 7),
                finished("stalled", BASE_MS - 3_000, 7, "deadline_timeout"),
                started("fine", BASE_MS - 2_000, 7),
                finished("fine", BASE_MS - 1_000, 7, "completed"),
            ],
        );
        let report = diagnose(&dir, &["--at", BASE_ISO]).expect("report");
        assert!(
            report.contains("verdict          : catdesk_timeout"),
            "{report}"
        );
        assert!(
            report.contains("deadline timeouts      : 1 [stalled]"),
            "{report}"
        );
        assert!(report.contains("completed requests     : 1"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_cluster_fixture_verdicts_client_cancellation() {
        let dir =
            std::env::temp_dir().join(format!("catdesk-diagnose-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_fixture(
            &dir,
            "connections.jsonl",
            &[
                json!({"event": "process_started", "timestamp_ms": BASE_MS - 120_000,
                    "pid": 7}),
                cancelled("one", BASE_MS - 5_000, 7, "client_disconnect"),
                cancelled("two", BASE_MS - 2_000, 7, "client_disconnect"),
            ],
        );
        let report = diagnose(&dir, &["--at", BASE_ISO]).expect("report");
        assert!(
            report.contains("verdict          : client_cancellation"),
            "{report}"
        );
        assert!(
            report.contains("client cancellations   : 2 [one, two]"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quiet_window_fixture_verdicts_no_catdesk_failure() {
        let dir =
            std::env::temp_dir().join(format!("catdesk-diagnose-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_fixture(
            &dir,
            "connections.jsonl",
            &[
                json!({"event": "process_started", "timestamp_ms": BASE_MS - 120_000,
                    "pid": 7}),
                started("old", BASE_MS - 90_000, 7),
                finished("old", BASE_MS - 80_000, 7, "completed"),
                json!({"event": "process_stopping", "timestamp_ms": BASE_MS - 90_000, "pid": 7}),
            ],
        );
        let report = diagnose(&dir, &["--at", BASE_ISO]).expect("report");
        assert!(
            report.contains("verdict          : no_catdesk_failure"),
            "{report}"
        );
        assert!(
            report.contains("No coincident CatDesk evidence"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ghost_request_across_a_restart_is_surfaced_as_lost() {
        let dir =
            std::env::temp_dir().join(format!("catdesk-diagnose-ghost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The rotation split matters: the ghost starts in an old rotation,
        // the new process writes to the current file.
        write_fixture(
            &dir,
            "connections.1.jsonl",
            &[
                json!({"event": "process_started", "timestamp_ms": BASE_MS - 300_000,
                    "pid": 100}),
                started("ghost", BASE_MS - 240_000, 100),
            ],
        );
        write_fixture(
            &dir,
            "connections.jsonl",
            &[
                json!({"event": "process_started", "timestamp_ms": BASE_MS - 60_000,
                    "pid": 200}),
                started("fresh", BASE_MS - 45_000, 200),
            ],
        );
        let report = diagnose(&dir, &["--at", BASE_ISO]).expect("report");
        assert!(report.contains("lost at restart  : 1"), "{report}");
        assert!(report.contains("ghost (in flight 3.0 min"), "{report}");
        assert!(report.contains("active at failure: 1"), "{report}");
        assert!(report.contains("fresh (age 45.0 s"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_slot_is_read_and_unparsable_lines_are_counted() {
        let dir =
            std::env::temp_dir().join(format!("catdesk-diagnose-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_fixture(&dir, "connections.jsonl", &[json!({"garbage": true})]);
        write_fixture(
            &dir.join("concurrent"),
            "connections.jsonl",
            &[json!({"event": "tunnel_reconnect_waiting",
                "timestamp_ms": BASE_MS - 1_000, "pid": 300})],
        );
        let report = diagnose(&dir, &["--at", BASE_ISO, "--window", "5s"]).expect("report");
        assert!(report.contains("2 records"), "{report}");
        // The garbage line lacks timestamp_ms and event, so it classifies as
        // nothing; the tunnel event from the concurrent slot still counts.
        assert!(
            report.contains("verdict          : tunnel_event"),
            "{report}"
        );
        assert!(report.contains("tunnel events          : 1"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_logs_dir_is_a_clean_error() {
        let dir =
            std::env::temp_dir().join(format!("catdesk-diagnose-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let error = diagnose(&dir, &["--at", BASE_ISO]).expect_err("no logs");
        assert!(error.contains("no connection records"), "{error}");
    }
}
