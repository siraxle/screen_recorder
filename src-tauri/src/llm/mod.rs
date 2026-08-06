//! LLM provider abstraction (F3/F4). Both "local" and "cloud" providers speak
//! the same OpenAI-compatible chat-completions HTTP shape (Ollama, LM
//! Studio, llama.cpp server, vLLM locally; OpenAI/OpenRouter/Together/Groq-
//! style gateways in the cloud) — see CLAUDE.md's "never hardcode a single
//! provider into business logic" rule. The only difference is whether a
//! bearer token is attached, so callers never need a second code path.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db;

const KEYRING_SERVICE: &str = "screen-recorder";
const KEYRING_USER: &str = "cloud-llm-token";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Local,
    Cloud,
}

/// Provider configuration minus the cloud token, which never lives in the
/// settings table or in memory longer than a single request — see
/// `read_cloud_token`/`write_cloud_token` (OS keychain, CLAUDE.md's "never
/// in plain-text config, never logged").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmSettings {
    pub provider: ProviderKind,
    pub local_base_url: String,
    pub local_model: String,
    pub cloud_base_url: String,
    pub cloud_model: String,
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Local,
            local_base_url: "http://localhost:11434/v1".to_string(),
            local_model: "llama3.1".to_string(),
            cloud_base_url: "https://api.openai.com/v1".to_string(),
            cloud_model: "gpt-4o-mini".to_string(),
        }
    }
}

/// The error returned when a provider is configured but unusable —
/// distinguished from a generic network failure so callers (F3's "clear
/// error, no data sent" criterion) can surface it without having attempted a
/// call.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("cloud provider selected but no API token is configured")]
    MissingCloudToken,
    #[error("local provider base URL is not configured")]
    MissingLocalBaseUrl,
    #[error("provider returned an authentication error — check the API token")]
    AuthFailed,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

const KEY_PROVIDER: &str = "llm_provider";
const KEY_LOCAL_BASE_URL: &str = "llm_local_base_url";
const KEY_LOCAL_MODEL: &str = "llm_local_model";
const KEY_CLOUD_BASE_URL: &str = "llm_cloud_base_url";
const KEY_CLOUD_MODEL: &str = "llm_cloud_model";

/// Loads provider settings from the generic `settings` key/value table
/// (`db::get_setting`), falling back to `LlmSettings::default()` for any key
/// that hasn't been saved yet.
pub fn load_settings(conn: &Connection) -> Result<LlmSettings> {
    let defaults = LlmSettings::default();
    let provider = match db::get_setting(conn, KEY_PROVIDER)?.as_deref() {
        Some("cloud") => ProviderKind::Cloud,
        _ => ProviderKind::Local,
    };
    Ok(LlmSettings {
        provider,
        local_base_url: db::get_setting(conn, KEY_LOCAL_BASE_URL)?
            .unwrap_or(defaults.local_base_url),
        local_model: db::get_setting(conn, KEY_LOCAL_MODEL)?.unwrap_or(defaults.local_model),
        cloud_base_url: db::get_setting(conn, KEY_CLOUD_BASE_URL)?
            .unwrap_or(defaults.cloud_base_url),
        cloud_model: db::get_setting(conn, KEY_CLOUD_MODEL)?.unwrap_or(defaults.cloud_model),
    })
}

pub fn save_settings(conn: &Connection, settings: &LlmSettings) -> Result<()> {
    let provider_str = match settings.provider {
        ProviderKind::Local => "local",
        ProviderKind::Cloud => "cloud",
    };
    db::set_setting(conn, KEY_PROVIDER, provider_str)?;
    db::set_setting(conn, KEY_LOCAL_BASE_URL, &settings.local_base_url)?;
    db::set_setting(conn, KEY_LOCAL_MODEL, &settings.local_model)?;
    db::set_setting(conn, KEY_CLOUD_BASE_URL, &settings.cloud_base_url)?;
    db::set_setting(conn, KEY_CLOUD_MODEL, &settings.cloud_model)?;
    Ok(())
}

pub fn read_cloud_token() -> Result<Option<String>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .context("failed to open keychain entry")?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("failed to read cloud API token from keychain"),
    }
}

pub fn write_cloud_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .context("failed to open keychain entry")?;
    entry
        .set_password(token)
        .context("failed to save cloud API token to keychain")
}

pub fn clear_cloud_token() -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .context("failed to open keychain entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("failed to clear cloud API token from keychain"),
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

fn base_url_and_model(settings: &LlmSettings) -> Result<(&str, &str), ProviderError> {
    match settings.provider {
        ProviderKind::Local => {
            if settings.local_base_url.trim().is_empty() {
                return Err(ProviderError::MissingLocalBaseUrl);
            }
            Ok((&settings.local_base_url, &settings.local_model))
        }
        ProviderKind::Cloud => Ok((&settings.cloud_base_url, &settings.cloud_model)),
    }
}

/// Sends a single chat-completion request and returns the assistant's reply
/// text. `cloud_token` is only attached as a bearer token when the provider
/// is `Cloud`; callers must resolve it via `read_cloud_token` beforehand and
/// reject the call themselves if `Cloud` is selected with no token (see
/// `commands::generate_summary`), so this function never has to guess intent
/// from an absent token.
pub async fn complete(
    settings: &LlmSettings,
    cloud_token: Option<&str>,
    system: &str,
    user: &str,
) -> Result<String, ProviderError> {
    let (base_url, model) = base_url_and_model(settings)?;
    if settings.provider == ProviderKind::Cloud && cloud_token.is_none() {
        return Err(ProviderError::MissingCloudToken);
    }

    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
        .json(&ChatRequest {
            model,
            messages: vec![
                ChatMessage { role: "system", content: system },
                ChatMessage { role: "user", content: user },
            ],
        });
    if let Some(token) = cloud_token {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .await
        .context("failed to reach the LLM provider")?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Err(ProviderError::AuthFailed);
    }
    let response = response
        .error_for_status()
        .context("LLM provider returned an error status")?;

    let parsed: ChatResponse = response
        .json()
        .await
        .context("failed to parse LLM provider response")?;
    let content = parsed
        .choices
        .into_iter()
        .next()
        .context("LLM provider response contained no choices")?
        .message
        .content;
    Ok(content)
}

/// F4's "check the endpoint's availability" criterion — local provider only,
/// since a cloud probe has no meaningful "no token" mode.
pub async fn check_connection(base_url: &str) -> Result<()> {
    if base_url.trim().is_empty() {
        return Err(anyhow::anyhow!("base URL is empty"));
    }
    let client = reqwest::Client::new();
    client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .context("failed to reach the local LLM endpoint")?
        .error_for_status()
        .context("local LLM endpoint returned an error status")?;
    Ok(())
}

/// The structured shape `run_summarization` asks the model for.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct SummaryContent {
    pub overview: String,
    #[serde(default)]
    pub key_points: Vec<String>,
}

/// Parses the model's reply as `{"overview": ..., "key_points": [...]}`,
/// tolerating a markdown code fence around the JSON (a common model habit).
/// Falls back to treating the whole reply as the overview with no key
/// points, rather than failing the summarize job outright, since a
/// non-conforming-but-useful reply is still better than none.
pub fn parse_summary_reply(reply: &str) -> SummaryContent {
    let trimmed = reply.trim();
    let json_slice = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(trimmed);

    serde_json::from_str(json_slice).unwrap_or_else(|_| SummaryContent {
        overview: trimmed.to_string(),
        key_points: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings_for(base_url: String) -> LlmSettings {
        LlmSettings {
            provider: ProviderKind::Local,
            local_base_url: base_url,
            local_model: "test-model".to_string(),
            ..LlmSettings::default()
        }
    }

    #[tokio::test]
    async fn complete_returns_assistant_reply_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hello from the model"}}]
            })))
            .mount(&server)
            .await;

        let settings = settings_for(server.uri());
        let reply = complete(&settings, None, "sys", "user")
            .await
            .expect("should succeed");
        assert_eq!(reply, "hello from the model");
    }

    #[tokio::test]
    async fn complete_maps_401_to_auth_failed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let settings = settings_for(server.uri());
        let err = complete(&settings, None, "sys", "user")
            .await
            .expect_err("should fail");
        assert!(matches!(err, ProviderError::AuthFailed));
    }

    #[tokio::test]
    async fn complete_fails_fast_on_missing_cloud_token() {
        let mut settings = settings_for("http://unused.invalid".to_string());
        settings.provider = ProviderKind::Cloud;
        let err = complete(&settings, None, "sys", "user")
            .await
            .expect_err("should fail without attempting a request");
        assert!(matches!(err, ProviderError::MissingCloudToken));
    }

    #[test]
    fn parse_summary_reply_parses_plain_json() {
        let parsed = parse_summary_reply(
            r#"{"overview": "a meeting about X", "key_points": ["decided Y", "action: Z"]}"#,
        );
        assert_eq!(parsed.overview, "a meeting about X");
        assert_eq!(parsed.key_points, vec!["decided Y", "action: Z"]);
    }

    #[test]
    fn parse_summary_reply_strips_markdown_fence() {
        let parsed = parse_summary_reply(
            "```json\n{\"overview\": \"fenced\", \"key_points\": []}\n```",
        );
        assert_eq!(parsed.overview, "fenced");
    }

    #[test]
    fn parse_summary_reply_falls_back_to_raw_text() {
        let parsed = parse_summary_reply("not json at all");
        assert_eq!(parsed.overview, "not json at all");
        assert!(parsed.key_points.is_empty());
    }
}
