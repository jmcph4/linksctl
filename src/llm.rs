//! LLM access in the two modes the spec requires (links.md "Components"):
//! *API mode* talks to the Anthropic Messages API over HTTP, and *headless
//! agent mode* runs a local harness, `claude -p` by default. Both return a
//! JSON value matching a caller-supplied schema.

use std::{path::PathBuf, process::Stdio, time::Duration};

use eyre::{Context, Result, bail, eyre};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command};
use tracing::{debug, warn};

/// The Messages API endpoint.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The API version every request is pinned to.
const API_VERSION: &str = "2026-10-21";
/// Beta that lets the API re-run a declined request on Anthropic's
/// recommended fallback model instead of returning a refusal.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The model used in API mode when none is configured.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Output ceiling for one categorisation response.
const MAX_TOKENS: u32 = 16_000;
/// Attempts made for one API request before giving up.
const ATTEMPTS: u32 = 5;

/// Which way the LLM is reached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The Anthropic Messages API.
    #[default]
    Api,
    /// A local headless harness (`claude -p`).
    Agent,
}

/// How to reach the model. Built by the command line and config file.
#[derive(Clone, Debug)]
pub struct Settings {
    /// API or headless agent.
    pub mode: Mode,
    /// Model name; `None` takes the mode's default.
    pub model: Option<String>,
    /// API key, used in API mode.
    pub api_key: Option<String>,
    /// File holding the API key, read when `api_key` is unset.
    pub api_key_file: Option<PathBuf>,
    /// The harness binary run in agent mode.
    pub agent_command: String,
    /// Reasoning effort passed as `output_config.effort` in API mode.
    pub effort: Option<String>,
    /// HTTP timeout for one API request.
    pub timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Api,
            model: None,
            api_key: None,
            api_key_file: None,
            agent_command: "claude".to_owned(),
            effort: None,
            timeout: Duration::from_secs(600),
        }
    }
}

/// A ready-to-use LLM client.
pub struct Llm {
    settings: Settings,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl Llm {
    /// Resolve the settings into a client: in API mode the key is read now
    /// so a missing one fails before any work is done.
    pub fn new(settings: Settings) -> Result<Self> {
        let api_key = match settings.mode {
            Mode::Api => Some(resolve_key(&settings)?),
            Mode::Agent => None,
        };
        let client = reqwest::Client::builder()
            .timeout(settings.timeout)
            .gzip(true)
            .build()?;
        Ok(Self {
            settings,
            api_key,
            client,
        })
    }

    /// Ask for a JSON value matching `schema`, given a system prompt and a
    /// user message.
    pub async fn complete_json(&self, system: &str, user: &str, schema: &Value) -> Result<Value> {
        match self.settings.mode {
            Mode::Api => self.api(system, user, schema).await,
            Mode::Agent => self.agent(system, user, schema).await,
        }
    }

    /// One Messages API call with structured output, retried on rate limits
    /// and server errors.
    async fn api(&self, system: &str, user: &str, schema: &Value) -> Result<Value> {
        let model = self.settings.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let mut output_config = json!({ "format": { "type": "json_schema", "schema": schema } });
        if let Some(effort) = &self.settings.effort {
            output_config["effort"] = json!(effort);
        }
        let body = json!({
            "model": model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
            "output_config": output_config,
            "fallbacks": "default",
        });
        let key = self.api_key.as_deref().ok_or_else(|| eyre!("no API key"))?;
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=ATTEMPTS {
            let resp = self
                .client
                .post(MESSAGES_URL)
                .header("content-type", "application/json")
                .header("x-api-key", key)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", FALLBACK_BETA)
                .json(&body)
                .send()
                .await;
            let resp = match resp {
                Ok(resp) => resp,
                Err(e) if attempt < ATTEMPTS => {
                    warn!(attempt, "request failed, retrying: {e}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(e) => return Err(e).wrap_err("sending request"),
            };
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            let text = resp.text().await.wrap_err("reading response")?;
            let retryable = status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
                || status.as_u16() == 529;
            if retryable && attempt < ATTEMPTS {
                let wait = retry_after.unwrap_or(delay);
                warn!(%status, attempt, ?wait, "retrying after API error");
                tokio::time::sleep(wait).await;
                delay *= 2;
                continue;
            }
            if !status.is_success() {
                bail!("messages API returned {status}: {}", text.trim());
            }
            let message: Value = serde_json::from_str(&text).wrap_err("parsing API response")?;
            return message_json(&message);
        }
        unreachable!("loop returns on the last attempt")
    }

    /// Run the headless harness with the prompt on stdin and parse the JSON
    /// it prints.
    async fn agent(&self, system: &str, user: &str, schema: &Value) -> Result<Value> {
        let mut cmd = Command::new(&self.settings.agent_command);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(schema.to_string())
            .arg("--tools")
            .arg("")
            .arg("--no-session-persistence")
            .arg("--system-prompt")
            .arg(system);
        if let Some(model) = &self.settings.model {
            cmd.arg("--model").arg(model);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("running {}", self.settings.agent_command))?;
        let mut stdin = child.stdin.take().ok_or_else(|| eyre!("no stdin"))?;
        let prompt = user.to_owned();
        let feed = tokio::spawn(async move {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await
        });
        let out = child
            .wait_with_output()
            .await
            .wrap_err("waiting for harness")?;
        feed.await?.wrap_err("writing prompt to harness")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            bail!(
                "{} exited with {}: {}",
                self.settings.agent_command,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        agent_json(&stdout)
    }
}

/// The API key from the settings, or the file they name.
fn resolve_key(settings: &Settings) -> Result<String> {
    if let Some(key) = &settings.api_key {
        return Ok(key.trim().to_owned());
    }
    if let Some(path) = &settings.api_key_file {
        let key = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading API key from {}", path.display()))?;
        let key = key.trim();
        if key.is_empty() {
            bail!("{} is empty", path.display());
        }
        return Ok(key.to_owned());
    }
    bail!(
        "API mode needs a key: set `api_key` or `api_key_file` under [llm] in the config file, \
         or pass --api-key-file (or use --llm agent)"
    )
}

/// Pull the structured JSON out of a Messages API response.
fn message_json(message: &Value) -> Result<Value> {
    let stop = message["stop_reason"].as_str().unwrap_or("");
    if stop == "refusal" {
        bail!(
            "model declined the request ({})",
            message["stop_details"]["category"]
                .as_str()
                .unwrap_or("no category")
        );
    }
    if stop == "max_tokens" {
        bail!("response truncated at max_tokens");
    }
    let text: String = message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    debug!(
        stop,
        model = message["model"].as_str().unwrap_or(""),
        "API response"
    );
    parse_json_text(&text)
}

/// Pull the structured JSON out of what `claude -p --output-format json`
/// prints: the `structured_output` field when the harness filled it, else
/// the `result` text, else the raw output.
fn agent_json(stdout: &str) -> Result<Value> {
    let envelope: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => return parse_json_text(stdout),
    };
    if envelope["is_error"] == true {
        bail!(
            "harness reported an error: {}",
            envelope["result"].as_str().unwrap_or("(no detail)")
        );
    }
    if let Some(v) = envelope.get("structured_output")
        && !v.is_null()
    {
        return Ok(v.clone());
    }
    if let Some(result) = envelope["result"].as_str() {
        return parse_json_text(result);
    }
    // No envelope fields we know: the output may already be the value.
    Ok(envelope)
}

/// Parse JSON from model text, tolerating a Markdown code fence around it.
fn parse_json_text(text: &str) -> Result<Value> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end().strip_suffix("```").unwrap_or(s))
        .unwrap_or(t)
        .trim();
    serde_json::from_str(t).wrap_err_with(|| format!("model output is not JSON: {t:.200}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_json_reads_text_blocks() {
        let m = json!({"stop_reason": "end_turn", "content": [
            {"type": "thinking", "thinking": ""},
            {"type": "text", "text": "{\"labels\": []}"}
        ]});
        assert_eq!(message_json(&m).unwrap(), json!({"labels": []}));
    }

    #[test]
    fn message_json_rejects_refusal_and_truncation() {
        let m =
            json!({"stop_reason": "refusal", "stop_details": {"category": "cyber"}, "content": []});
        assert!(message_json(&m).unwrap_err().to_string().contains("cyber"));
        let m = json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "{"}]});
        assert!(message_json(&m).is_err());
    }

    #[test]
    fn agent_json_prefers_structured_output() {
        let s = r#"{"type":"result","is_error":false,"result":"ignored","structured_output":{"labels":[1]}}"#;
        assert_eq!(agent_json(s).unwrap(), json!({"labels": [1]}));
        let s = r#"{"type":"result","is_error":false,"result":"```json\n{\"labels\":[2]}\n```"}"#;
        assert_eq!(agent_json(s).unwrap(), json!({"labels": [2]}));
        let s = r#"{"labels":[3]}"#;
        assert_eq!(agent_json(s).unwrap(), json!({"labels": [3]}));
        let s = r#"{"type":"result","is_error":true,"result":"boom"}"#;
        assert!(agent_json(s).unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn api_mode_needs_a_key() {
        let err = Llm::new(Settings::default())
            .err()
            .map(|e| e.to_string())
            .unwrap();
        assert!(err.contains("api_key"));
        assert!(
            Llm::new(Settings {
                mode: Mode::Agent,
                ..Settings::default()
            })
            .is_ok()
        );
    }
}
