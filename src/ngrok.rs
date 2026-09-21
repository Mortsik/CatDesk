use crate::state::{SharedState, load_ngrok_authtoken};
use ngrok::prelude::*;
use reqwest::Url;
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
}
