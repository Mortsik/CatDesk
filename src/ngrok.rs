use crate::state::{SharedState, load_ngrok_authtoken};
use ngrok::prelude::*;
use reqwest::Url;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;

fn reconnect_delay(attempt: u32, jitter_seed: u64) -> Duration {
    let shift = attempt.min(6);
    let base_ms = RECONNECT_BASE_MS
        .saturating_mul(1_u64 << shift)
        .min(RECONNECT_MAX_MS);
    let jitter_window = (base_ms / 5).max(1);
    let jitter_ms = jitter_seed % (jitter_window + 1);
    Duration::from_millis(base_ms.saturating_add(jitter_ms).min(RECONNECT_MAX_MS))
}

fn reconnect_jitter_seed(attempt: u32) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos ^ ((std::process::id() as u64) << 32) ^ attempt as u64
}

/// Wrap one SDK connector invocation with reconnect evidence. The ngrok SDK
/// calls the session connector for the initial connect (no error) and again
/// for every reconnect attempt, passing the error that ended the previous
/// connection; with the default connector it retries reconnects indefinitely
/// until the session is canceled. Reconnect attempts record
/// `tunnel_session_reconnect_attempt`, and a re-established transport records
/// `tunnel_session_renewed` (the SDK rebinds the tunnel on top of it). The
/// initial connect stays silent — the supervisor's own `tunnel_starting`
/// already covers it. The delegate does the actual transport, so production
/// wires `ngrok::session::default_connect` and connection behavior is
/// unchanged; tests wire a recording fake.
async fn connect_with_reconnect_evidence<D, E>(
    delegate: D,
    on_evidence: E,
    host: String,
    port: u16,
    tls_config: Arc<rustls::ClientConfig>,
    error: Option<ngrok::tunnel::AcceptError>,
) -> Result<Box<dyn ngrok::session::IoStream>, ngrok::session::ConnectError>
where
    D: ngrok::session::Connector,
    E: Fn(&'static str),
{
    let was_reconnect = error.is_some();
    if was_reconnect {
        on_evidence("tunnel_session_reconnect_attempt");
    }
    let stream = delegate.connect(host, port, tls_config, error).await?;
    if was_reconnect {
        on_evidence("tunnel_session_renewed");
    }
    Ok(stream)
}

/// Start an ngrok HTTP tunnel using the embedded Rust SDK. After the first
/// connection attempt, the owned supervisor keeps retrying transient failures
/// and unexpected tunnel exits until application shutdown aborts `ngrok_task`.
pub async fn start(state: SharedState) -> Result<(), String> {
    let (port, mcp_path) = {
        let app = state.lock().await;
        if app.ngrok_running
            || app
                .ngrok_task
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        {
            return Err("ngrok supervisor is already running".into());
        }
        (app.port, app.mcp_path())
    };
    let authtoken = load_ngrok_authtoken()
        .map_err(|e| format!("Failed to read ~/.catdesk/config.toml: {e}"))?
        .ok_or_else(|| "ngrok authtoken is not configured".to_string())?;
    let forwards_to: Url = format!("http://127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("Invalid forward URL: {e}"))?;

    let (initial_tx, initial_rx) = oneshot::channel::<Result<(), String>>();
    let supervisor_state = state.clone();
    let supervisor = tokio::spawn(async move {
        let mut first_attempt = Some(initial_tx);
        let mut failures = 0_u32;

        loop {
            crate::diagnostics::event(if first_attempt.is_some() {
                "tunnel_starting"
            } else {
                "tunnel_reconnect_attempt"
            });

            let session = match ngrok::Session::builder()
                .authtoken(authtoken.clone())
                .connector(|host, port, tls_config, error| {
                    connect_with_reconnect_evidence(
                        ngrok::session::default_connect,
                        crate::diagnostics::event,
                        host,
                        port,
                        tls_config,
                        error,
                    )
                })
                .connect()
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    let message = format!("Failed to connect ngrok session: {error}");
                    crate::diagnostics::event("tunnel_connect_failed");
                    if let Some(tx) = first_attempt.take() {
                        let _ = tx.send(Err(message.clone()));
                    }
                    {
                        let mut app = supervisor_state.lock().await;
                        app.ngrok_running = false;
                        app.ngrok_url = None;
                        app.set_remote_connected(false);
                        app.log("ERROR", format!("ngrok: {message}; retrying"));
                    }
                    let delay = reconnect_delay(failures, reconnect_jitter_seed(failures));
                    failures = failures.saturating_add(1);
                    crate::diagnostics::event("tunnel_reconnect_waiting");
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };

            let mut http_endpoint = session.http_endpoint();
            if let Some(domain) = {
                let app = supervisor_state.lock().await;
                app.ngrok_domain.clone()
            } {
                if !domain.is_empty() {
                    http_endpoint.domain(domain);
                }
            }

            let mut forwarder = match http_endpoint.listen_and_forward(forwards_to.clone()).await {
                Ok(forwarder) => forwarder,
                Err(error) => {
                    let message = format!("Failed to open ngrok tunnel: {error}");
                    crate::diagnostics::event("tunnel_listen_failed");
                    if let Some(tx) = first_attempt.take() {
                        let _ = tx.send(Err(message.clone()));
                    }
                    {
                        let mut app = supervisor_state.lock().await;
                        app.ngrok_running = false;
                        app.ngrok_url = None;
                        app.set_remote_connected(false);
                        app.log("ERROR", format!("ngrok: {message}; retrying"));
                    }
                    let delay = reconnect_delay(failures, reconnect_jitter_seed(failures));
                    failures = failures.saturating_add(1);
                    crate::diagnostics::event("tunnel_reconnect_waiting");
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };

            let url = forwarder.url().to_string();
            crate::diagnostics::event(if failures == 0 && first_attempt.is_some() {
                "tunnel_started"
            } else {
                "tunnel_reconnected"
            });
            {
                let mut app = supervisor_state.lock().await;
                app.ngrok_running = true;
                app.ngrok_url = Some(url.clone());
                app.log("INFO", "ngrok SDK tunnel started".into());
                app.log("INFO", format!("ngrok URL: {url}"));
                app.log("INFO", format!("MCP Server URL: {url}{mcp_path}"));

                if app.ngrok_domain.is_none() {
                    if let Ok(parsed_url) = reqwest::Url::parse(&url) {
                        if let Some(host) = parsed_url.host_str() {
                            app.ngrok_domain = Some(host.to_string());
                            app.log("INFO", format!("Auto-saved ngrok static domain: {host}"));
                            app.persist_state_with_log();
                        }
                    }
                }
            }
            if let Some(tx) = first_attempt.take() {
                let _ = tx.send(Ok(()));
            }
            failures = 0;

            // While the forwarder lives, SDK-internal reconnects are already
            // visible: the connector hook installed on the session builder
            // reports every attempt and every re-established transport as
            // tunnel_session_* events (see connect_with_reconnect_evidence).
            let result = forwarder.join().await;
            crate::diagnostics::event(match &result {
                Ok(Ok(())) => "tunnel_stopped",
                Ok(Err(_)) => "tunnel_failed",
                Err(error) if error.is_cancelled() => "tunnel_cancelled",
                Err(_) => "tunnel_join_failed",
            });
            {
                let mut app = supervisor_state.lock().await;
                match result {
                    Ok(Ok(())) => app.log("WARN", "ngrok tunnel exited; reconnecting".into()),
                    Ok(Err(error)) => app.log(
                        "ERROR",
                        format!("ngrok tunnel failed: {error}; reconnecting"),
                    ),
                    Err(error) => app.log(
                        "ERROR",
                        format!("ngrok tunnel join failed: {error}; reconnecting"),
                    ),
                }
                app.ngrok_running = false;
                app.ngrok_url = None;
                app.set_remote_connected(false);
            }

            let delay = reconnect_delay(failures, reconnect_jitter_seed(failures));
            failures = failures.saturating_add(1);
            crate::diagnostics::event("tunnel_reconnect_waiting");
            tokio::time::sleep(delay).await;
        }
    });

    state.lock().await.ngrok_task = Some(supervisor);
    match initial_rx.await {
        Ok(result) => result,
        Err(_) => Err("ngrok supervisor stopped before its initial connection attempt".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered() {
        let first = reconnect_delay(0, 0);
        let first_jittered = reconnect_delay(0, u64::MAX);
        assert!(first >= Duration::from_millis(500));
        assert!(first_jittered >= first);
        assert!(first_jittered <= Duration::from_millis(600));
        assert!(reconnect_delay(20, u64::MAX) <= Duration::from_secs(30));
    }

    #[test]
    fn reconnect_backoff_grows_before_reaching_the_cap() {
        assert!(reconnect_delay(1, 0) > reconnect_delay(0, 0));
        assert!(reconnect_delay(2, 0) > reconnect_delay(1, 0));
    }

    fn test_tls_config() -> Arc<rustls::ClientConfig> {
        // Tests never run main(), so the process-level CryptoProvider install
        // from main.rs is absent and rustls cannot auto-detect a provider
        // (reqwest pulls in a second one). Build the config explicitly.
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("aws-lc-rs supports the safe default protocol versions")
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        )
    }

    type DelegateCalls = Arc<Mutex<Vec<(String, u16, bool)>>>;

    /// Connector double that records every SDK invocation as
    /// (host, port, was_reconnect) and answers with a duplex stream.
    fn succeeding_delegate(calls: DelegateCalls) -> impl ngrok::session::Connector {
        move |host: String,
              port: u16,
              _tls_config: Arc<rustls::ClientConfig>,
              error: Option<ngrok::tunnel::AcceptError>| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push((host, port, error.is_some()));
                let (_server, client) = tokio::io::duplex(64);
                Ok(Box::new(client) as Box<dyn ngrok::session::IoStream>)
            }
        }
    }

    /// Connector double that records the invocation and fails the transport.
    fn failing_delegate(calls: DelegateCalls) -> impl ngrok::session::Connector {
        move |host: String,
              port: u16,
              _tls_config: Arc<rustls::ClientConfig>,
              error: Option<ngrok::tunnel::AcceptError>| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push((host, port, error.is_some()));
                Err(ngrok::session::ConnectError::Tcp(std::io::Error::other(
                    "simulated transport failure",
                )))
            }
        }
    }

    /// The error the SDK passes to the connector when a live connection died
    /// and it is dialing again.
    fn reconnect_cause() -> Option<ngrok::tunnel::AcceptError> {
        Some(ngrok::tunnel::AcceptError::Reconnect(Arc::new(
            ngrok::session::ConnectError::Tcp(std::io::Error::other("simulated transport loss")),
        )))
    }

    #[tokio::test]
    async fn initial_connect_delegates_verbatim_without_reconnect_evidence() {
        let calls: DelegateCalls = Arc::new(Mutex::new(Vec::new()));
        let evidence: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let evidence = evidence.clone();
            move |name: &'static str| evidence.lock().unwrap().push(name)
        };

        let result = connect_with_reconnect_evidence(
            succeeding_delegate(calls.clone()),
            recorder,
            "connect.ngrok.com".to_string(),
            443,
            test_tls_config(),
            None,
        )
        .await;

        assert!(result.is_ok(), "the initial transport must pass through");
        assert!(
            evidence.lock().unwrap().is_empty(),
            "the initial connect is already covered by tunnel_starting"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("connect.ngrok.com".to_string(), 443, false)],
            "the delegate must receive the SDK arguments verbatim"
        );
    }

    #[tokio::test]
    async fn successful_reconnect_reports_attempt_then_renewal() {
        let calls: DelegateCalls = Arc::new(Mutex::new(Vec::new()));
        let evidence: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let evidence = evidence.clone();
            move |name: &'static str| evidence.lock().unwrap().push(name)
        };

        let result = connect_with_reconnect_evidence(
            succeeding_delegate(calls.clone()),
            recorder,
            "connect.ngrok.com".to_string(),
            443,
            test_tls_config(),
            reconnect_cause(),
        )
        .await;

        assert!(result.is_ok(), "the renewed transport must pass through");
        assert_eq!(
            *evidence.lock().unwrap(),
            vec!["tunnel_session_reconnect_attempt", "tunnel_session_renewed"],
            "a re-established transport must leave both traces"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("connect.ngrok.com".to_string(), 443, true)],
            "the delegate must see the reconnect error flag"
        );
    }

    #[tokio::test]
    async fn failed_reconnect_reports_the_attempt_without_renewal() {
        let calls: DelegateCalls = Arc::new(Mutex::new(Vec::new()));
        let evidence: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let evidence = evidence.clone();
            move |name: &'static str| evidence.lock().unwrap().push(name)
        };

        let result = connect_with_reconnect_evidence(
            failing_delegate(calls.clone()),
            recorder,
            "connect.ngrok.com".to_string(),
            443,
            test_tls_config(),
            reconnect_cause(),
        )
        .await;

        assert!(
            matches!(result, Err(ngrok::session::ConnectError::Tcp(_))),
            "the transport failure must propagate to the SDK"
        );
        assert_eq!(
            *evidence.lock().unwrap(),
            vec!["tunnel_session_reconnect_attempt"],
            "a failed dial must not claim renewal"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("connect.ngrok.com".to_string(), 443, true)],
            "the delegate must see the reconnect error flag"
        );
    }
}
