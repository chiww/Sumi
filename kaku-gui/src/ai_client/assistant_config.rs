//! Assistant configuration loaded from `assistant.toml`.

use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use std::path::{Path, PathBuf};

use super::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::codex_connection;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiMode {
    ChatCompletions,
    Responses,
}

impl ApiMode {
    fn from_config(value: Option<&str>) -> Self {
        match value.unwrap_or("chat_completions") {
            "chat_completions" => Self::ChatCompletions,
            "responses" => Self::Responses,
            other => {
                // Same tolerant policy as the `sumi ai` TUI, which coerces
                // unknown values to chat_completions: a typo in assistant.toml
                // must degrade to the default, not disable AI entirely.
                log::warn!("unknown api_mode `{other}` in assistant.toml; using chat_completions");
                Self::ChatCompletions
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

/// Configuration loaded from `assistant.toml`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AssistantConfig {
    pub api_key: String,
    /// Deep chat model. Falls back to the Simple Model from assistant.toml when omitted.
    pub chat_model: String,
    /// Optional user-curated model list for the chat overlay. When set, the chat
    /// overlay cycles only through these via Shift+Tab and skips the auto-fetch step.
    pub chat_model_choices: Vec<String>,
    pub base_url: String,
    /// Optional extra headers for enterprise proxies / API gateways.
    pub custom_headers: Vec<(String, String)>,
    /// Provider name derived from base_url and auth_type (e.g. "OpenAI", "Copilot").
    pub provider: String,
    /// API wire format for API-key/custom endpoints. Chat Completions remains
    /// the compatibility default; Codex following mode resolves its own Responses provider.
    pub api_mode: ApiMode,
    /// Auth mechanism: "api_key" (default), "copilot", or "codex".
    /// Legacy "gemini_key" values are recognized only to surface a friendly
    /// error at load time; the Gemini provider was removed in V0.10.0.
    pub auth_type: String,
    /// When false, the `tools` field is omitted from chat requests.
    /// Set `chat_tools_enabled = false` in assistant.toml for providers that do not
    /// support function calling (e.g. some Kimi or local-model variants).
    pub chat_tools_enabled: bool,
    /// Enable the provider-hosted Responses `web_search` tool. This does not
    /// require `web_search_provider` or `web_search_api_key`.
    pub native_web_search: bool,
    /// Web search provider: "brave", "pipellm", or "tavily". None = disabled.
    pub web_search_provider: Option<String>,
    /// API key for web_search_provider. None = search tool not registered.
    pub web_search_api_key: Option<String>,
    /// Hidden escape hatch: path to a custom fetch script (not in TUI or template).
    /// Script receives the URL as $1 and must print Markdown to stdout.
    pub web_fetch_script: Option<String>,
    /// Simple Model for quick command generation and lightweight chat. When it
    /// differs from chat_model, the overlay offers it via Shift+Tab.
    pub fast_model: Option<String>,
    /// Optional dedicated model for background memory curation. Falls back to
    /// `chat_model` when unset. Point at a cheaper/faster model to reduce cost.
    pub memory_curator_model: Option<String>,
}

impl AssistantConfig {
    /// Whether the assistant configuration file exists at the active config path.
    pub fn file_exists() -> bool {
        assistant_toml_path()
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    pub fn load() -> Result<Self> {
        let path = assistant_toml_path()?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        Self::parse(&raw, &path)
    }

    fn parse(raw: &str, path: &std::path::Path) -> Result<Self> {
        let parsed: toml::Value = raw.parse().context("Invalid assistant.toml")?;

        let auth_type = parsed
            .get("auth_type")
            .and_then(|v| v.as_str())
            .unwrap_or("api_key")
            .to_string();

        let api_mode = ApiMode::from_config(parsed.get("api_mode").and_then(|v| v.as_str()));

        // The Gemini provider was removed in V0.10.0. Surface a clear migration
        // path instead of letting the OpenAI-compatible code path silently
        // mangle Gemini requests.
        if auth_type == "gemini_key" {
            anyhow::bail!(
                "Gemini provider was removed in V0.10.0. Open `sumi ai` and \
                 switch to a different provider (OpenAI, Copilot, Codex, or a \
                 custom OpenAI-compatible endpoint), then update {}.",
                path.display()
            );
        }

        let api_key = parsed
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let model = parsed
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_MODEL)
            .to_string();

        let legacy_fast_model = parsed
            .get("fast_model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let mut simple_model = legacy_fast_model.clone().unwrap_or_else(|| model.clone());

        // If an old config had both model and fast_model but no chat_model,
        // preserve model as the deep slot and fold fast_model into Simple Model.
        let mut chat_model = parsed
            .get("chat_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if legacy_fast_model.is_some() {
                    model.clone()
                } else {
                    simple_model.clone()
                }
            });

        if auth_type == "codex"
            && (simple_model == codex_connection::FOLLOW_CODEX_MODEL
                || chat_model == codex_connection::FOLLOW_CODEX_MODEL)
        {
            if let Some(codex_model) = codex_connection::load_configured_codex_model()
                .context("resolve configured Follow Codex model")?
            {
                if simple_model == codex_connection::FOLLOW_CODEX_MODEL {
                    simple_model = codex_model.clone();
                }
                if chat_model == codex_connection::FOLLOW_CODEX_MODEL {
                    chat_model = codex_model;
                }
            }
        }

        let chat_model_choices = parsed
            .get("chat_model_choices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let base_url = parsed
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();

        let custom_headers = parse_custom_headers(parsed.get("custom_headers"))?;

        let provider = detect_provider_with_auth(&base_url, &auth_type).to_string();

        let chat_tools_enabled = parsed
            .get("chat_tools_enabled")
            .and_then(|v| v.as_bool())
            // OpenAI-compatible tool calling is supported by all providers we
            // ship presets for; per-provider opt-out is still possible by
            // setting `chat_tools_enabled = false` in assistant.toml.
            .unwrap_or(true);

        let native_web_search = parsed
            .get("native_web_search")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let web_search_provider = parsed
            .get("web_search_provider")
            .and_then(|v| v.as_str())
            .filter(|s| matches!(*s, "brave" | "pipellm" | "tavily"))
            .map(String::from);

        let web_search_api_key = parsed
            .get("web_search_api_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let web_fetch_script = parsed
            .get("web_fetch_script")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| expand_tilde(s));

        let fast_model = (simple_model != chat_model).then_some(simple_model);

        let memory_curator_model = parsed
            .get("memory_curator_model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        Ok(Self {
            api_key,
            chat_model,
            chat_model_choices,
            base_url,
            custom_headers,
            provider,
            api_mode,
            auth_type,
            chat_tools_enabled,
            native_web_search,
            web_search_provider,
            web_search_api_key,
            web_fetch_script,
            fast_model,
            memory_curator_model,
        })
    }

    /// Returns true when the third-party web_search function is configured and
    /// native Responses web search is not taking its place.
    pub fn web_search_ready(&self) -> bool {
        !self.native_web_search_ready()
            && self.web_search_provider.is_some()
            && self.web_search_api_key.is_some()
    }

    pub fn native_web_search_ready(&self) -> bool {
        self.api_mode == ApiMode::Responses && self.native_web_search && self.chat_tools_enabled
    }

    /// Codex authentication always uses the Responses transport regardless of
    /// the compatibility value stored in `api_mode`.
    pub fn effective_api_mode(&self) -> ApiMode {
        if self.auth_type == "codex" {
            ApiMode::Responses
        } else {
            self.api_mode
        }
    }
}

fn parse_custom_headers(value: Option<&toml::Value>) -> Result<Vec<(String, String)>> {
    let raw_headers: Vec<String> = match value {
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::trim))
            .filter(|item| !item.is_empty())
            .map(String::from)
            .collect(),
        Some(toml::Value::String(raw)) => raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(String::from)
            .collect(),
        Some(_) | None => Vec::new(),
    };

    let mut headers = Vec::new();
    for raw in raw_headers {
        let (name, value) = raw
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid custom_headers entry `{raw}`"))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            anyhow::bail!("invalid custom_headers entry `{raw}`");
        }
        if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("content-type") {
            anyhow::bail!("custom_headers cannot override `{name}`");
        }
        HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid custom header name `{name}`"))?;
        HeaderValue::from_str(value)
            .with_context(|| format!("invalid custom header value for `{name}`"))?;
        headers.push((name.to_string(), value.to_string()));
    }
    Ok(headers)
}

fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    s.to_string()
}

fn assistant_toml_path() -> Result<PathBuf> {
    let user_config_path = config::user_config_path();
    let config_dir = user_config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid user config path"))?;
    Ok(config_dir.join("assistant.toml"))
}

/// Maps (base_url, auth_type) to a display provider name.
///
/// Single source of truth for provider naming. The `sumi` binary used to
/// carry a parallel `#[allow(dead_code)]` table; that copy was removed in
/// V0.10.0 because it never matched the GUI version under maintenance.
fn detect_provider_with_auth(base_url: &str, auth_type: &str) -> &'static str {
    let normalized = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    match (normalized.as_str(), auth_type) {
        ("https://api.githubcopilot.com", _) => "Copilot",
        ("https://api.openai.com/v1", "codex") => "Codex",
        _ => "Custom",
    }
}
#[cfg(test)]
mod tests {
    use super::{detect_provider_with_auth, parse_custom_headers, AssistantConfig};

    /// `sumi ai` writes assistant.toml and the GUI reads it; both sides assert
    /// the same fixture so neither can reinterpret a key on its own.
    #[test]
    fn assistant_config_matches_shared_cli_contract() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/assistant-config.json"
        )))
        .expect("shared assistant config fixtures");
        for case in cases.as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let cfg = AssistantConfig::parse(
                case["toml"].as_str().unwrap(),
                std::path::Path::new("assistant.toml"),
            )
            .unwrap_or_else(|err| panic!("{}: {:#}", name, err));
            let simple = cfg.fast_model.as_deref().unwrap_or(&cfg.chat_model);
            assert_eq!(simple, case["simple_model"].as_str().unwrap(), "{}", name);
            assert_eq!(
                cfg.chat_model,
                case["deep_model"].as_str().unwrap(),
                "{}",
                name
            );
            assert_eq!(cfg.base_url, case["base_url"].as_str().unwrap(), "{}", name);
            assert_eq!(
                cfg.api_mode.as_str(),
                case["api_mode"].as_str().unwrap(),
                "{}",
                name
            );
            assert_eq!(
                cfg.auth_type,
                case["auth_type"].as_str().unwrap(),
                "{}",
                name
            );
            let choices: Vec<&str> = case["chat_model_choices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(cfg.chat_model_choices, choices, "{}", name);
        }
    }

    #[test]
    fn detects_copilot_and_codex_and_falls_back_to_custom() {
        assert_eq!(
            detect_provider_with_auth("https://api.githubcopilot.com", "copilot"),
            "Copilot"
        );
        assert_eq!(
            detect_provider_with_auth("https://api.openai.com/v1", "codex"),
            "Codex"
        );
        // Same OpenAI URL with the default api_key auth is treated as a generic
        // OpenAI-compatible endpoint, so we surface it as Custom.
        assert_eq!(
            detect_provider_with_auth("https://api.openai.com/v1", "api_key"),
            "Custom"
        );
        // Unknown / removed providers (Gemini was dropped in V0.10.0) fall
        // through to Custom rather than crashing detection.
        assert_eq!(
            detect_provider_with_auth("https://generativelanguage.googleapis.com", "gemini_key"),
            "Custom"
        );
        assert_eq!(detect_provider_with_auth("", "api_key"), "Custom");
    }

    #[test]
    fn trailing_slash_does_not_break_match() {
        assert_eq!(
            detect_provider_with_auth("https://api.githubcopilot.com/", "copilot"),
            "Copilot"
        );
        assert_eq!(
            detect_provider_with_auth("https://api.openai.com/v1/", "codex"),
            "Codex"
        );
    }

    #[test]
    fn parses_custom_headers_from_array_and_rejects_bad_entries() {
        let value = toml::Value::Array(vec![
            toml::Value::String("X-Customer-ID: acme".to_string()),
            toml::Value::String("X-Trace: abc:123".to_string()),
        ]);
        let headers = parse_custom_headers(Some(&value)).unwrap();
        assert_eq!(
            headers,
            vec![
                ("X-Customer-ID".to_string(), "acme".to_string()),
                ("X-Trace".to_string(), "abc:123".to_string())
            ]
        );

        let bad = toml::Value::Array(vec![toml::Value::String("missing-colon".to_string())]);
        assert!(parse_custom_headers(Some(&bad)).is_err());

        let reserved =
            toml::Value::Array(vec![toml::Value::String("Authorization: nope".to_string())]);
        assert!(parse_custom_headers(Some(&reserved)).is_err());
    }
}
