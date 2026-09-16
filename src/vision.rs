//! Server-side vision analysis for the read_image MCP tool.
//!
//! DLACZEGO: ChatGPT's connector surface never feeds image blocks from tool
//! results to its vision model (confirmed by OpenAI support, 2026-04), so a
//! remote agent can request the image but cannot "see" it. The workaround is
//! to analyze the image server-side and return the description as text —
//! structuredContent text does reach ChatGPT models. The backend is env-driven
//! so it stays swappable (Gemini today; e.g. a local Ollama model later)
//! without touching the MCP layer.

use serde_json::{Value, json};

// gemini-3-flash ma na free tierze zaledwie 20 requestów/dzień (429 po chwilę);
// 3.5-flash działa i ma hojniejszy limit. Nadpisywalne przez CATDESK_VISION_MODEL.
const DEFAULT_GEMINI_MODEL: &str = "gemini-3.5-flash";
const DEFAULT_ANALYSIS_PROMPT: &str = "Describe what this image shows in detail. \
Include the subject, layout, colors, any visible text verbatim, and anything \
notable about the composition.";

/// Parsed vision-backend configuration. `from_parts` keeps the pure mapping
/// testable without touching process env (tests run in parallel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisionConfig {
    pub backend: VisionBackend,
    pub model: String,
    pub api_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisionBackend {
    Gemini,
}

impl VisionBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
        }
    }
}

pub fn vision_config_from_parts(
    backend: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
) -> Result<VisionConfig, String> {
    let backend = backend.unwrap_or("gemini").trim().to_lowercase();
    match backend.as_str() {
        // Default backend when CATDESK_VISION_BACKEND is unset: gemini, but
        // only meaningful together with an API key (checked by the caller).
        "gemini" => {}
        other => {
            return Err(format!(
                "Unsupported CATDESK_VISION_BACKEND '{other}'. Supported backends: gemini."
            ));
        }
    }
    let api_key = match api_key {
        Some(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ => {
            return Err(
                "Vision analysis is not configured: set GEMINI_API_KEY to enable it \
(add it to Doppler and the catdesk launcher injects it)."
                    .to_string(),
            );
        }
    };
    let model = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(DEFAULT_GEMINI_MODEL)
        .to_string();
    Ok(VisionConfig {
        backend: VisionBackend::Gemini,
        model,
        api_key,
    })
}

pub fn vision_config_from_env() -> Result<VisionConfig, String> {
    let read = |name: &str| {
        std::env::var(name).ok().or_else(|| {
            // CATDESK_-prefixed variants win so a host can keep vendor names clean.
            std::env::var(format!("CATDESK_{name}")).ok()
        })
    };
    vision_config_from_parts(
        read("VISION_BACKEND").as_deref(),
        read("GEMINI_API_KEY").as_deref(),
        read("VISION_MODEL").as_deref(),
    )
}

pub fn default_analysis_prompt() -> &'static str {
    DEFAULT_ANALYSIS_PROMPT
}

/// Ask the configured backend to describe `image_base64` (standard base64,
/// no line wrapping) and return the plain-text answer.
pub async fn analyze_image(
    config: &VisionConfig,
    prompt: &str,
    image_base64: &str,
    mime_type: &str,
) -> Result<String, String> {
    match config.backend {
        VisionBackend::Gemini => gemini_analyze(config, prompt, image_base64, mime_type).await,
    }
}

const GEMINI_INTERACTIONS_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/interactions";
const GEMINI_HTTP_TIMEOUT_SECS: u64 = 55;

async fn gemini_analyze(
    config: &VisionConfig,
    prompt: &str,
    image_base64: &str,
    mime_type: &str,
) -> Result<String, String> {
    let body = json!({
        "model": config.model,
        "input": [
            { "type": "text", "text": prompt },
            { "type": "image", "data": image_base64, "mime_type": mime_type },
        ],
    });
    let raw = gemini_http_post(GEMINI_INTERACTIONS_URL, &config.api_key, &body).await?;
    let (http_status, payload) = split_status_marker(&raw)?;
    let payload: Value = serde_json::from_str(&payload).map_err(|e| {
        format!("Vision backend returned a non-JSON body (HTTP {http_status}): {e}")
    })?;
    if !(200..300).contains(&http_status) {
        let message = payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!(
            "Vision backend error (HTTP {http_status}): {message}"
        ));
    }
    if payload.get("status").and_then(Value::as_str) != Some("completed") {
        let interaction_status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!(
            "Vision backend did not complete the analysis (status: {interaction_status})"
        ));
    }
    extract_gemini_text(&payload)
}

/// Gemini Interactions API przez subprocess curl, NIE reqwest.
/// DLACZEGO: z wnętrza procesu catdesk (WSL2, mirrored networking) żądanie
/// HTTPS do generativelanguage.googleapis.com przez reqwest po zestawieniu TCP
/// wisi bez odpowiedzi aż do timeoutu — podczas gdy ten sam stack z osobnego
/// procesu odpowiada w <0.3 s (examples/probe_gemini). Systemowy curl omija
/// ten quirk niezawodnie, więc jest deliberate zależnością runtime tego modułu.
async fn gemini_http_post(url: &str, api_key: &str, body: &Value) -> Result<String, String> {
    let status_marker = "__CATDESK_HTTP_STATUS__";
    let body_str = serde_json::to_string(body)
        .map_err(|e| format!("Failed to serialize vision request: {e}"))?;
    let mut command = tokio::process::Command::new("curl");
    command
        .args([
            "-sS",
            "--max-time",
            &GEMINI_HTTP_TIMEOUT_SECS.to_string(),
            "-X",
            "POST",
            url,
            "-H",
            &format!("x-goog-api-key: {api_key}"),
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "-w",
            &format!("\n{status_marker}%{{http_code}}"),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn curl for vision backend: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(body_str.as_bytes())
            .await
            .map_err(|e| format!("Failed to send vision request body to curl: {e}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| format!("Failed to close curl stdin: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("Vision backend curl failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Vision backend curl failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.into_owned())
}

/// Splits the `-w` status marker appended by gemini_http_post into
/// (http_status, raw JSON body).
fn split_status_marker(raw: &str) -> Result<(u16, String), String> {
    const STATUS_MARKER: &str = "__CATDESK_HTTP_STATUS__";
    let (payload, status) = raw
        .rsplit_once(&format!("\n{STATUS_MARKER}"))
        .ok_or_else(|| "Vision backend response is missing the HTTP status marker".to_string())?;
    let http_status: u16 = status
        .trim()
        .parse()
        .map_err(|e| format!("Vision backend returned an invalid HTTP status: {e}"))?;
    Ok((http_status, payload.to_string()))
}

/// Answer path per the Interactions API schema: steps[type=model_output]
/// .content[type=text].text, joined across blocks/steps in order.
fn extract_gemini_text(payload: &Value) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(steps) = payload.get("steps").and_then(Value::as_array) {
        for step in steps {
            if step.get("type").and_then(Value::as_str) != Some("model_output") {
                continue;
            }
            if let Some(content) = step.get("content").and_then(Value::as_array) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                parts.push(text.trim().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        return Err("Vision backend returned no text description".to_string());
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_api_key_and_defaults_to_gemini() {
        let err = vision_config_from_parts(None, None, None).expect_err("must require a key");
        assert!(err.contains("GEMINI_API_KEY"), "unexpected error: {err}");

        let config = vision_config_from_parts(None, Some(" k "), None).expect("valid config");
        assert_eq!(config.backend, VisionBackend::Gemini);
        assert_eq!(config.api_key, "k");
        assert_eq!(config.model, DEFAULT_GEMINI_MODEL);

        let config =
            vision_config_from_parts(Some("gemini"), Some("k"), Some("m1 ")).expect("valid config");
        assert_eq!(config.model, "m1");
    }

    #[test]
    fn config_rejects_unknown_backend() {
        let err = vision_config_from_parts(Some("ollama"), Some("k"), None)
            .expect_err("unknown backend must be rejected");
        assert!(
            err.contains("Unsupported CATDESK_VISION_BACKEND"),
            "err: {err}"
        );
    }

    #[test]
    fn gemini_response_text_is_extracted_from_model_output_steps() {
        let payload = json!({
            "status": "completed",
            "steps": [
                { "type": "thought", "content": [ { "type": "text", "text": "internal" } ] },
                {
                    "type": "model_output",
                    "content": [
                        { "type": "text", "text": "A mural on a bedroom wall." },
                        { "type": "text", "text": "  " },
                    ],
                },
                {
                    "type": "model_output",
                    "content": [ { "type": "text", "text": "Blue tones dominate." } ],
                },
            ],
        });
        assert_eq!(
            extract_gemini_text(&payload).expect("text"),
            "A mural on a bedroom wall.\nBlue tones dominate."
        );
    }

    #[test]
    fn gemini_response_without_text_fails_clearly() {
        let payload = json!({ "status": "completed", "steps": [] });
        let err = extract_gemini_text(&payload).expect_err("no text");
        assert!(err.contains("no text description"), "err: {err}");
    }
}
