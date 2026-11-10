//! AI client for Kaku's built-in chat overlay.
//!
//! Reads API config from `~/.config/sumi/assistant.toml` and provides
//! synchronous streaming clients for OpenAI-compatible Chat Completions and
//! Responses APIs.
//! Supports function/tool calling for agentic workflows.
//!
//! Runs on a plain OS thread (inside overlay), so blocking I/O is fine.

mod assistant_config;
mod responses;
mod think_filter;
mod transport;

pub use assistant_config::{ApiMode, AssistantConfig};
pub(crate) use transport::build_client_with_proxy;

use responses::{
    append_tool_arguments, parse_responses_http, translate_responses_messages,
    translate_responses_tools,
};
use think_filter::{InlineThinkFilter, ThinkSegment};
use transport::{
    add_stream_bytes, add_stream_event, build_client_with_proxy_options, read_body_capped,
    read_error_response_preview, read_sse_line_capped, send_with_retry, shared_http_client,
    wait_before_request,
};

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::ai_auth;
use crate::codex_connection::{self, CodexConnection, CodexCredential};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

const DEFAULT_MODEL: &str = "gpt-5.4-mini";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODELS_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_STREAM_BYTES: usize = 16 * 1024 * 1024;
// Guards against runaway/looping streams, not against legitimate length:
// reasoning-heavy providers (DeepSeek/GLM) emit one small delta per SSE
// event, so a single long response can pass 16K events. Memory stays bounded
// by MAX_RESPONSE_STREAM_BYTES either way.
const MAX_RESPONSE_STREAM_EVENTS: usize = 65_536;
const MAX_RESPONSE_TOOL_CALLS: usize = 32;
const MAX_RESPONSE_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_OUTPUT_ITEMS: usize = 128;
const MAX_RESPONSE_CITATIONS: usize = 256;
const MAX_RESPONSE_CITATION_TITLE_CHARS: usize = 512;
const MAX_RESPONSE_CITATION_URL_CHARS: usize = 2_048;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

// ─── Message types ────────────────────────────────────────────────────────────

/// A single message in API format. Stored as a raw JSON value so it can represent
/// any role (system, user, assistant, tool) including tool_calls and tool results.
#[derive(Clone)]
pub struct ApiMessage(pub serde_json::Value);

impl ApiMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self(serde_json::json!({ "role": "system", "content": content.into() }))
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self(serde_json::json!({ "role": "user", "content": content.into() }))
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self(serde_json::json!({ "role": "assistant", "content": content.into() }))
    }
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning_content: impl AsRef<str>,
    ) -> Self {
        let mut msg = serde_json::json!({ "role": "assistant", "content": content.into() });
        let reasoning = reasoning_content.as_ref();
        if !reasoning.is_empty() {
            msg["reasoning_content"] = serde_json::Value::String(reasoning.to_string());
        }
        Self(msg)
    }
    /// Assistant turn that requested tool calls (content is null per the OpenAI spec).
    pub fn assistant_tool_calls(tool_calls: serde_json::Value) -> Self {
        Self(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": tool_calls
        }))
    }
    /// Tool result message returned after executing a function call.
    /// Includes the tool name so non-OpenAI providers (for example Gemini)
    /// can map responses back to the corresponding function declaration.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self(serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id.into(),
            "name": name.into(),
            "content": content.into()
        }))
    }

    /// A raw output item returned by the Responses API. Reasoning models can
    /// require encrypted reasoning items to be replayed unchanged before tool
    /// outputs on the next step, so reducing these to chat-completion messages
    /// loses protocol state.
    pub fn responses_output_item(item: serde_json::Value) -> Self {
        Self(serde_json::json!({ "kaku_responses_output_item": item }))
    }

    /// Approximate serialized byte size of this message. Used for history-budget
    /// accounting in the agent loop; does not need to be exact.
    pub fn byte_len(&self) -> usize {
        serde_json::to_vec(&self.0).map(|v| v.len()).unwrap_or(0)
    }
}

pub fn should_roundtrip_reasoning_content(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("deepseek")
        || model.contains("kimi")
        || model.contains("mimo")
        || model.contains("glm")
}

// ─── Tool calling ─────────────────────────────────────────────────────────────

/// A fully assembled tool call returned by the model after streaming is complete.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Complete JSON-encoded arguments string, e.g. `{"path": "~/Downloads"}`.
    pub arguments: String,
}

/// Result of one model step. `response_items` is empty for Chat Completions;
/// Responses callers must replay these raw items before function-call outputs.
pub struct ChatStepResult {
    pub tool_calls: Vec<ToolCall>,
    pub response_items: Vec<serde_json::Value>,
}

impl ChatStepResult {
    fn empty() -> Self {
        Self {
            tool_calls: Vec::new(),
            response_items: Vec::new(),
        }
    }

    fn chat_completions(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            response_items: Vec::new(),
        }
    }
}

// ─── Client ───────────────────────────────────────────────────────────────────

/// Synchronous AI client for use inside overlay threads.
/// Clone is cheap: reqwest::blocking::Client is Arc-backed internally.
#[derive(Clone)]
pub struct AiClient {
    config: AssistantConfig,
    client: reqwest::blocking::Client,
    codex_discovered_model: Arc<OnceLock<String>>,
    max_request_attempts: u32,
}

impl AiClient {
    pub fn new(config: AssistantConfig) -> Self {
        Self {
            config,
            client: shared_http_client().clone(),
            codex_discovered_model: Arc::new(OnceLock::new()),
            max_request_attempts: 3,
        }
    }

    /// Build a provider-aware one-shot client with a caller-specific timeout.
    ///
    /// Inline shell requests already have a UI-level timeout. Keep them to one
    /// normal API transport attempt so retry layers cannot multiply that budget.
    pub fn new_with_timeout(config: AssistantConfig, timeout: std::time::Duration) -> Self {
        Self {
            config,
            client: build_client_with_proxy(timeout),
            codex_discovered_model: Arc::new(OnceLock::new()),
            max_request_attempts: 1,
        }
    }

    /// Whether this client will include tools in chat requests.
    pub fn tools_enabled(&self) -> bool {
        self.config.chat_tools_enabled
    }

    /// Returns a reference to the loaded assistant configuration.
    pub fn config(&self) -> &AssistantConfig {
        &self.config
    }

    /// Single-shot (non-streaming) completion for short tasks like title generation.
    ///
    /// Internally uses `chat_step` with an empty tools list and accumulates all tokens
    /// into a String. The returned text is trimmed of leading/trailing whitespace.
    pub fn complete_once(&self, model: &str, messages: &[ApiMessage]) -> Result<String> {
        let cancelled = AtomicBool::new(false);
        let mut text = String::new();
        self.chat_step(
            model,
            messages,
            &[],
            false,
            &cancelled,
            &mut |tok| {
                text.push_str(tok);
            },
            &mut |_| {},
        )?;
        Ok(text.trim().to_string())
    }

    /// Fetch available chat models from `{base_url}/models`.
    /// Filters out non-chat models (embeddings, TTS, image, etc.).
    pub fn list_models(&self) -> Result<Vec<String>> {
        if self.config.auth_type == "codex" {
            let connection = codex_connection::load_codex_connection()
                .context("resolve user Codex connection for model discovery")?;
            return self.list_codex_models(&connection);
        }
        let url = format!("{}/models", self.config.base_url);
        let req = self.client.get(&url);
        let req = self.apply_auth_headers(req)?;
        let resp = req.send().context("GET /models failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = read_error_response_preview(resp, MAX_ERROR_BODY_BYTES);
            anyhow::bail!("models API {}: {}", status, body);
        }
        parse_models_response(resp, "models API", true)
    }

    fn list_codex_models(&self, connection: &CodexConnection) -> Result<Vec<String>> {
        let endpoint = connection.models_endpoint();
        let mut credential = connection.credential.clone();
        let mut provider_headers = HeaderMap::new();
        for (name, value) in &connection.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid Codex provider header name `{name}`"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid Codex provider header value for `{name}`"))?;
            provider_headers.insert(name, value);
        }

        let build = |credential: &CodexCredential| {
            let mut req = self
                .client
                .get(&endpoint)
                .query(&connection.query_params)
                .header("Accept", "application/json")
                .header("User-Agent", "codex_cli_rs")
                .headers(provider_headers.clone());
            match credential {
                CodexCredential::ChatGpt(auth) => {
                    req = req
                        .header("Authorization", format!("Bearer {}", auth.access_token))
                        .header("OpenAI-Beta", "responses=experimental")
                        .header("originator", "codex_cli_rs");
                    if let Some(account_id) = auth.account_id.as_deref() {
                        req = req.header("chatgpt-account-id", account_id);
                    }
                }
                CodexCredential::Bearer(token) => {
                    req = req.header("Authorization", format!("Bearer {token}"));
                }
                CodexCredential::None => {}
            }
            req
        };

        let cancelled = AtomicBool::new(false);
        let response = self.send_codex_request_with_retry(
            &mut credential,
            &self.client,
            build,
            "Codex model discovery",
            &cancelled,
            connection.request_max_attempts,
        )?;
        let models = parse_models_response(response, "Codex model discovery", false)?;
        if models.is_empty() {
            anyhow::bail!("Codex model discovery returned no chat models");
        }
        Ok(models)
    }

    fn resolve_codex_request_model(
        &self,
        connection: &CodexConnection,
        requested_model: &str,
    ) -> Result<String> {
        if requested_model != codex_connection::FOLLOW_CODEX_MODEL {
            return Ok(requested_model.to_string());
        }
        if let Some(configured) = connection
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            return Ok(configured.to_string());
        }
        if let Some(cached) = self.codex_discovered_model.get() {
            return Ok(cached.clone());
        }
        let discovered = self
            .list_codex_models(connection)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Codex provider returned no models"))?;
        let _ = self.codex_discovered_model.set(discovered.clone());
        Ok(discovered)
    }

    fn send_codex_request_with_retry<F>(
        &self,
        credential: &mut CodexCredential,
        http_client: &reqwest::blocking::Client,
        build: F,
        provider_label: &str,
        cancelled: &AtomicBool,
        max_attempts: u32,
    ) -> Result<reqwest::blocking::Response>
    where
        F: Fn(&CodexCredential) -> reqwest::blocking::RequestBuilder,
    {
        let max_attempts = max_attempts.max(1);
        let mut last_err = String::new();
        let mut refreshed_chatgpt = false;
        let mut attempt = 0;
        while attempt < max_attempts {
            let backoff = if attempt == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs(1 << attempt)
            };
            wait_before_request(cancelled, backoff)?;
            let response = match build(credential).send() {
                Ok(response) => response,
                Err(error) => {
                    last_err = error.without_url().to_string();
                    log::warn!(
                        "{} HTTP attempt {}: {}",
                        provider_label,
                        attempt + 1,
                        last_err
                    );
                    attempt += 1;
                    continue;
                }
            };
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                && matches!(credential, CodexCredential::ChatGpt(_))
                && !refreshed_chatgpt
            {
                log::debug!("{provider_label} ChatGPT token rejected; refreshing");
                *credential = CodexCredential::ChatGpt(ai_auth::refresh_codex_auth(http_client)?);
                refreshed_chatgpt = true;
                continue;
            }
            if status.is_success() {
                return Ok(response);
            }
            let code = status.as_u16();
            let body = read_error_response_preview(response, MAX_ERROR_BODY_BYTES);
            if code == 429 || code >= 500 {
                let preview: String = body.chars().take(200).collect();
                last_err = format!("{provider_label} error {code}: {preview}");
                log::warn!(
                    "{} HTTP attempt {} retryable: {}",
                    provider_label,
                    attempt + 1,
                    last_err
                );
                attempt += 1;
                continue;
            }
            anyhow::bail!("{provider_label} error {code}: {body}");
        }
        Err(anyhow::anyhow!(
            "{} request failed after {} attempts: {}",
            provider_label,
            max_attempts,
            last_err
        ))
    }

    /// Build provider-specific auth headers for the HTTP request builder.
    fn apply_auth_headers(
        &self,
        req: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let req = match self.config.auth_type.as_str() {
            "copilot" => {
                let token = ai_auth::get_copilot_token(&self.client)?;
                req.header("Authorization", format!("Bearer {token}"))
                    .header("Copilot-Integration-Id", "vscode-chat")
                    .header("Editor-Version", "vscode/1.110.1")
                    .header("Editor-Plugin-Version", "copilot-chat/0.38.2")
                    .header("Openai-Organization", "github-copilot")
                    .header("Openai-Intent", "conversation-panel")
            }
            "codex" => {
                anyhow::bail!(
                    "Codex following mode must use the resolved Codex Responses connection"
                )
            }
            _ => {
                if self.config.api_key.trim().is_empty() {
                    req
                } else {
                    req.header("Authorization", format!("Bearer {}", self.config.api_key))
                }
            }
        };
        self.apply_custom_headers(req)
    }

    fn apply_custom_headers(
        &self,
        req: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.config.custom_headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid custom header name `{name}`"))?;
            let header_value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid custom header value for `{name}`"))?;
            headers.insert(header_name, header_value);
        }
        Ok(req.headers(headers))
    }

    /// Single chat step with optional tool support.
    ///
    /// Streams text tokens via `on_token`. If the model responds by requesting
    /// tool calls instead of (or before) text, returns those calls for the
    /// caller to execute and loop. Returns an empty vec when the step is text-only.
    ///
    /// The caller must set `cancelled` to `true` to abort mid-stream.
    #[allow(clippy::too_many_arguments)]
    pub fn chat_step(
        &self,
        model: &str,
        messages: &[ApiMessage],
        tools: &[serde_json::Value],
        allow_native_web_search: bool,
        cancelled: &AtomicBool,
        on_token: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<ChatStepResult> {
        // Codex following mode resolves the user's Codex Responses provider
        // rather than using assistant.toml's Chat Completions connection.
        if self.config.auth_type == "codex" {
            return self.chat_step_codex(model, messages, tools, cancelled, on_token, on_reasoning);
        }
        if self.config.effective_api_mode() == ApiMode::Responses {
            return self.chat_step_responses(
                model,
                messages,
                tools,
                allow_native_web_search,
                cancelled,
                on_token,
                on_reasoning,
            );
        }

        let url = format!("{}/chat/completions", self.config.base_url);

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages.iter().map(|m| m.0.clone()).collect::<Vec<_>>(),
            "stream": true,
        });
        if !tools.is_empty() && self.config.chat_tools_enabled {
            body["tools"] = serde_json::Value::Array(tools.to_vec());
        }

        let req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Accept-Encoding", "identity")
            .json(&body);
        let req = self.apply_auth_headers(req)?;

        let response = send_with_retry(req, "API", cancelled, self.max_request_attempts)?;

        let mut reader = BufReader::new(response);
        // Accumulate tool call fragments by index; each index is one pending call.
        // BTreeMap keeps indices sorted so we process them in order.
        let mut tc_buf: BTreeMap<usize, ToolCallBuf> = BTreeMap::new();
        let mut finish_reason = String::new();
        let mut think_filter = InlineThinkFilter::new();
        let mut stream_bytes = 0usize;
        let mut stream_events = 0usize;

        loop {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let Some(mut line_bytes) = read_sse_line_capped(&mut reader, "API")? else {
                break;
            };
            add_stream_bytes(&mut stream_bytes, line_bytes.len(), "API")?;
            while matches!(line_bytes.last(), Some(b'\n' | b'\r')) {
                line_bytes.pop();
            }
            let line = std::str::from_utf8(&line_bytes).context("API SSE line was not UTF-8")?;
            let Some(data) = sse_data_payload(line) else {
                continue;
            };
            add_stream_event(&mut stream_events, "API")?;
            if data.trim() == "[DONE]" {
                break;
            }
            let chunk = match serde_json::from_str::<serde_json::Value>(data) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Failed to parse SSE chunk: {e}");
                    continue;
                }
            };

            let Some(choice) = chunk["choices"].get(0) else {
                continue;
            };

            // Capture finish_reason when present.
            if let Some(fr) = choice["finish_reason"].as_str() {
                if !fr.is_empty() && fr != "null" {
                    finish_reason = fr.to_string();
                }
            }

            let delta = &choice["delta"];

            // Reasoning delta (DeepSeek et al. via dedicated field).
            if let Some(reasoning) = reasoning_delta_text(choice, delta) {
                if !reasoning.is_empty() {
                    on_reasoning(reasoning);
                }
            }
            // Text delta: filter inline <think> tags (Zhipu glm-5-turbo et al.
            // embed reasoning inside content rather than a dedicated field).
            if let Some(content) = delta["content"].as_str() {
                for seg in think_filter.feed(content) {
                    match seg {
                        ThinkSegment::Token(t) => on_token(&t),
                        ThinkSegment::Reasoning(r) => on_reasoning(&r),
                    }
                }
            }

            // Tool call deltas: accumulate arguments by index.
            if let Some(tc_arr) = delta["tool_calls"].as_array() {
                for tc in tc_arr {
                    let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                    if !tc_buf.contains_key(&idx) && tc_buf.len() >= MAX_RESPONSE_TOOL_CALLS {
                        anyhow::bail!("API returned too many function calls");
                    }
                    let entry = tc_buf.entry(idx).or_default();
                    if let Some(id) = tc["id"].as_str() {
                        entry.id = id.to_string();
                    }
                    if let Some(name) = tc["function"]["name"].as_str() {
                        entry.name = name.to_string();
                    }
                    if let Some(args) = tc["function"]["arguments"].as_str() {
                        append_tool_arguments(&mut entry.arguments, args, "API")?;
                    }
                }
            }
        }

        for seg in think_filter.flush() {
            match seg {
                ThinkSegment::Token(t) => on_token(&t),
                ThinkSegment::Reasoning(r) => on_reasoning(&r),
            }
        }

        // Build ToolCall results. Some proxies (e.g. vivgrid) never set
        // finish_reason to "tool_calls" even when streaming tool call deltas,
        // so fall back to any accumulated tc_buf entries with a valid name.
        if finish_reason == "tool_calls" || !tc_buf.is_empty() {
            let calls = tc_buf
                .into_values()
                .filter(|b| !b.name.is_empty())
                .map(|b| ToolCall {
                    id: b.id,
                    name: b.name,
                    arguments: b.arguments,
                })
                .collect::<Vec<_>>();
            if calls.is_empty() {
                Ok(ChatStepResult::empty())
            } else {
                Ok(ChatStepResult::chat_completions(calls))
            }
        } else {
            Ok(ChatStepResult::empty())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn chat_step_responses(
        &self,
        model: &str,
        messages: &[ApiMessage],
        tools: &[serde_json::Value],
        allow_native_web_search: bool,
        cancelled: &AtomicBool,
        on_token: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<ChatStepResult> {
        use serde_json::{json, Value};

        let url = format!("{}/responses", self.config.base_url);
        let (instructions, input) = translate_responses_messages(messages);
        let responses_tools = translate_responses_tools(
            tools,
            self.config.chat_tools_enabled,
            allow_native_web_search && self.config.native_web_search_ready(),
        );

        let mut body = json!({
            "model": model,
            "input": input,
            "stream": true,
            "store": false,
        });
        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        if !responses_tools.is_empty() {
            body["tools"] = Value::Array(responses_tools);
            body["tool_choice"] = Value::String("auto".to_string());
        }
        if supports_encrypted_reasoning_include(&self.config.base_url) {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        let req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream, application/json")
            .header("Cache-Control", "no-cache")
            .header("Accept-Encoding", "identity")
            .json(&body);
        let req = self.apply_auth_headers(req)?;
        let response = send_with_retry(req, "Responses API", cancelled, self.max_request_attempts)?;

        parse_responses_http(response, cancelled, on_token, on_reasoning, "Responses API")
    }

    /// Codex-following chat step over the user-selected Responses provider.
    ///
    /// Translates chat-format messages and tools into the Responses request
    /// shape, streams text/reasoning, and assembles streamed `function_call`
    /// items back into `ToolCall`s for the agent loop to execute.
    fn chat_step_codex(
        &self,
        model: &str,
        messages: &[ApiMessage],
        tools: &[serde_json::Value],
        cancelled: &AtomicBool,
        on_token: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<ChatStepResult> {
        let connection =
            codex_connection::load_codex_connection().context("resolve user Codex connection")?;
        let resolved_model = self.resolve_codex_request_model(&connection, model)?;
        self.chat_step_codex_connection(
            &connection,
            &resolved_model,
            messages,
            tools,
            cancelled,
            on_token,
            on_reasoning,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn chat_step_codex_connection(
        &self,
        connection: &CodexConnection,
        model: &str,
        messages: &[ApiMessage],
        tools: &[serde_json::Value],
        cancelled: &AtomicBool,
        on_token: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<ChatStepResult> {
        use serde_json::{json, Value};

        let mut credential = connection.credential.clone();

        let (instructions, input) = translate_responses_messages(messages);
        let responses_tools =
            translate_responses_tools(tools, self.config.chat_tools_enabled, false);

        let mut reasoning = serde_json::Map::new();
        reasoning.insert(
            "effort".to_string(),
            Value::String(
                connection
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "medium".to_string()),
            ),
        );
        reasoning.insert(
            "summary".to_string(),
            Value::String(
                connection
                    .reasoning_summary
                    .clone()
                    .unwrap_or_else(|| "auto".to_string()),
            ),
        );
        let mut body = json!({
            "model": model,
            "input": input,
            "stream": true,
            "store": false,
            "reasoning": Value::Object(reasoning),
        });
        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        if !responses_tools.is_empty() {
            body["tools"] = Value::Array(responses_tools);
            body["tool_choice"] = Value::String("auto".to_string());
        }
        // Always ask for encrypted reasoning payloads, not only when tools
        // are enabled: reasoning models emit reasoning items regardless, the
        // stubs are persisted for replay, and under `store: false` a replayed
        // reasoning item without its encrypted content is rejected.
        body["include"] = json!(["reasoning.encrypted_content"]);

        let endpoint = connection.endpoint();
        let provider_client = connection.stream_idle_timeout_ms.map(|timeout_ms| {
            build_client_with_proxy_options(
                std::time::Duration::from_secs(600),
                Some(std::time::Duration::from_millis(timeout_ms.max(1))),
                true,
            )
        });
        let http_client = provider_client.as_ref().unwrap_or(&self.client);
        let mut provider_headers = HeaderMap::new();
        for (name, value) in &connection.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid Codex provider header name `{name}`"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid Codex provider header value for `{name}`"))?;
            provider_headers.insert(name, value);
        }
        let build = |credential: &CodexCredential| -> reqwest::blocking::RequestBuilder {
            let mut req = http_client
                .post(&endpoint)
                .query(&connection.query_params)
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream, application/json")
                .header("Cache-Control", "no-cache")
                .header("Accept-Encoding", "identity")
                .header("User-Agent", "codex_cli_rs")
                .headers(provider_headers.clone())
                .json(&body);
            match credential {
                CodexCredential::ChatGpt(auth) => {
                    req = req
                        .header("Authorization", format!("Bearer {}", auth.access_token))
                        .header("OpenAI-Beta", "responses=experimental")
                        .header("originator", "codex_cli_rs");
                    if let Some(account_id) = auth.account_id.as_deref() {
                        req = req.header("chatgpt-account-id", account_id);
                    }
                }
                CodexCredential::Bearer(token) => {
                    req = req.header("Authorization", format!("Bearer {token}"));
                }
                CodexCredential::None => {}
            }
            req
        };

        for stream_attempt in 0..=connection.stream_max_retries {
            let response = self.send_codex_request_with_retry(
                &mut credential,
                http_client,
                build,
                "Codex provider",
                cancelled,
                connection.request_max_attempts,
            )?;

            let emitted = std::cell::Cell::new(false);
            let parsed = parse_responses_http(
                response,
                cancelled,
                &mut |token| {
                    emitted.set(true);
                    on_token(token);
                },
                &mut |reasoning| {
                    emitted.set(true);
                    on_reasoning(reasoning);
                },
                "Codex provider",
            );
            match parsed {
                Ok(result) => return Ok(result),
                Err(err) if !emitted.get() && stream_attempt < connection.stream_max_retries => {
                    log::warn!(
                        "Codex provider stream attempt {} failed before output: {}; retrying",
                        stream_attempt + 1,
                        err
                    );
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("Codex stream loop always returns")
    }
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn parse_models_response(
    response: reqwest::blocking::Response,
    provider_label: &str,
    sort_models: bool,
) -> Result<Vec<String>> {
    let body = read_body_capped(response, MAX_MODELS_BODY_BYTES, provider_label)?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parse {provider_label} response"))?;
    parse_models_value(&value, provider_label, sort_models)
}

fn parse_models_value(
    value: &serde_json::Value,
    provider_label: &str,
    sort_models: bool,
) -> Result<Vec<String>> {
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("{provider_label} response has no `data` or `models` array")
        })?;
    let mut models: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|visibility| visibility == "list")
        })
        .filter_map(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("id").and_then(serde_json::Value::as_str))
                .or_else(|| entry.get("slug").and_then(serde_json::Value::as_str))
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(String::from)
        })
        .filter(|model| kaku_ai_utils::is_chat_model_id(model))
        .collect();
    if sort_models {
        models.sort();
        models.dedup();
    } else {
        let mut seen = HashSet::new();
        models.retain(|model| seen.insert(model.clone()));
    }
    Ok(models)
}

/// Buffer for accumulating streamed tool call fragments.
#[derive(Default)]
struct ToolCallBuf {
    id: String,
    name: String,
    arguments: String,
}

fn reasoning_delta_text<'a>(
    choice: &'a serde_json::Value,
    delta: &'a serde_json::Value,
) -> Option<&'a str> {
    delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .or_else(|| delta["reasoning"]["content"].as_str())
        .or_else(|| delta["thinking"].as_str())
        .or_else(|| delta["thinking"]["content"].as_str())
        .or_else(|| choice["reasoning_content"].as_str())
        .or_else(|| choice["reasoning"].as_str())
        .or_else(|| choice["thinking"].as_str())
        .or_else(|| choice["thinking"]["content"].as_str())
        .or_else(|| choice["message"]["reasoning_content"].as_str())
        .or_else(|| choice["message"]["reasoning"].as_str())
}

fn sse_data_payload(line: &str) -> Option<&str> {
    line.strip_prefix("data:").map(str::trim_start)
}

fn supports_encrypted_reasoning_include(base_url: &str) -> bool {
    url::Url::parse(base_url).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
    })
}

// Delegated to kaku-ai-utils crate to avoid cross-binary drift.

#[cfg(test)]
mod tests {
    use super::{
        parse_models_value, reasoning_delta_text, should_roundtrip_reasoning_content,
        sse_data_payload, supports_encrypted_reasoning_include, AiClient, ApiMessage, ApiMode,
        AssistantConfig, InlineThinkFilter, ThinkSegment, DEFAULT_BASE_URL,
    };
    use crate::codex_connection::{CodexConnection, CodexCredential};
    use reqwest::header::{AUTHORIZATION, USER_AGENT};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    #[test]
    fn retry_transports_do_not_send_cancelled_first_request() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancellation probe");
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let cancelled = AtomicBool::new(true);
        let error = super::send_with_retry(http.get(&endpoint), "test", &cancelled, 1)
            .expect_err("cancelled generic request");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        let config = AssistantConfig {
            api_key: String::new(),
            chat_model: "test".to_string(),
            chat_model_choices: Vec::new(),
            base_url: endpoint.clone(),
            custom_headers: Vec::new(),
            provider: "Codex".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "codex".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_millis(100));
        let builds = std::cell::Cell::new(0);
        let error = client
            .send_codex_request_with_retry(
                &mut CodexCredential::None,
                &http,
                |_| {
                    builds.set(builds.get() + 1);
                    http.get(&endpoint)
                },
                "test",
                &cancelled,
                1,
            )
            .expect_err("cancelled Codex request");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(builds.get(), 0);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    pub(super) fn collect_segments(segs: Vec<ThinkSegment>) -> (String, String) {
        let mut tokens = String::new();
        let mut reasoning = String::new();
        for seg in segs {
            match seg {
                ThinkSegment::Token(t) => tokens.push_str(&t),
                ThinkSegment::Reasoning(r) => reasoning.push_str(&r),
            }
        }
        (tokens, reasoning)
    }

    #[test]
    fn generic_models_parser_preserves_empty_success_response() {
        let models = parse_models_value(&serde_json::json!({ "data": [] }), "models API", true)
            .expect("empty model list remains a successful response");
        assert!(models.is_empty());
    }

    /// A provider that advertises more models than fit on screen must still be
    /// reported in full: the picker is a view concern, the parser is not the
    /// place to decide how many models a user may reach (#550).
    #[test]
    fn generic_models_parser_keeps_every_advertised_model() {
        let ids: Vec<String> = (0..170).map(|i| format!("vendor/model-{i:03}")).collect();
        let data: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();

        let models = parse_models_value(&serde_json::json!({ "data": data }), "models API", true)
            .expect("well-formed model list parses");

        assert_eq!(models.len(), ids.len());
        let mut sorted = ids;
        sorted.sort();
        assert_eq!(models, sorted);
    }

    fn route_mock_sse_lines(lines: &[&str]) -> (String, String) {
        let mut think_filter = InlineThinkFilter::new();
        let mut tokens = String::new();
        let mut reasoning = String::new();

        for line in lines {
            let Some(data) = sse_data_payload(line) else {
                continue;
            };
            if data.trim() == "[DONE]" {
                break;
            }
            // Mirror chat_step()'s production resilience: malformed JSON chunks
            // are skipped rather than panicking. Keeping the two paths in sync
            // means tests exercise the same parse error policy as live traffic.
            let chunk: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(choice) = chunk["choices"].get(0) else {
                continue;
            };
            let delta = &choice["delta"];

            if let Some(text) = reasoning_delta_text(choice, delta) {
                reasoning.push_str(text);
            }
            if let Some(content) = delta["content"].as_str() {
                let (visible, hidden) = collect_segments(think_filter.feed(content));
                tokens.push_str(&visible);
                reasoning.push_str(&hidden);
            }
        }

        let (visible, hidden) = collect_segments(think_filter.flush());
        tokens.push_str(&visible);
        reasoning.push_str(&hidden);
        (tokens, reasoning)
    }

    #[test]
    fn encrypted_reasoning_include_is_only_sent_to_openai() {
        assert!(supports_encrypted_reasoning_include(
            "https://api.openai.com/v1"
        ));
        assert!(!supports_encrypted_reasoning_include(
            "https://responses.example.com/v1"
        ));
        assert!(!supports_encrypted_reasoning_include(
            "https://api.openai.com.evil.example/v1"
        ));
        assert!(!supports_encrypted_reasoning_include(
            "http://api.openai.com/v1"
        ));
    }

    #[test]
    fn assistant_with_reasoning_keeps_reasoning_hidden_field() {
        let msg = ApiMessage::assistant_with_reasoning("visible", "hidden thought");
        assert_eq!(msg.0["role"], "assistant");
        assert_eq!(msg.0["content"], "visible");
        assert_eq!(msg.0["reasoning_content"], "hidden thought");

        let without = ApiMessage::assistant_with_reasoning("visible", "");
        assert!(without.0.get("reasoning_content").is_none());
    }

    #[test]
    fn reasoning_delta_text_accepts_common_openai_compatible_shapes() {
        let cases = [
            (
                serde_json::json!({"delta": {"reasoning_content": "a"}}),
                "a",
            ),
            (serde_json::json!({"delta": {"reasoning": "b"}}), "b"),
            (
                serde_json::json!({"delta": {"reasoning": {"content": "c"}}}),
                "c",
            ),
            (serde_json::json!({"delta": {"thinking": "d"}}), "d"),
            (
                serde_json::json!({"delta": {"thinking": {"content": "e"}}}),
                "e",
            ),
            (
                serde_json::json!({"delta": {}, "reasoning_content": "fw"}),
                "fw",
            ),
            (serde_json::json!({"delta": {}, "reasoning": "f"}), "f"),
            (
                serde_json::json!({"delta": {}, "thinking": {"content": "g"}}),
                "g",
            ),
            (
                serde_json::json!({"delta": {}, "message": {"reasoning_content": "h"}}),
                "h",
            ),
        ];

        for (choice, expected) in cases {
            assert_eq!(
                reasoning_delta_text(&choice, &choice["delta"]),
                Some(expected)
            );
        }

        let choice = serde_json::json!({"delta": {"content": "visible"}});
        assert_eq!(reasoning_delta_text(&choice, &choice["delta"]), None);
    }

    #[test]
    fn sse_data_payload_accepts_optional_space_after_colon() {
        assert_eq!(sse_data_payload("data:{\"x\":1}"), Some("{\"x\":1}"));
        assert_eq!(sse_data_payload("data: {\"x\":1}"), Some("{\"x\":1}"));
        assert_eq!(sse_data_payload("event: message"), None);
    }

    #[test]
    fn custom_responses_mode_posts_expected_wire_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock responses server");
        let address = listener.local_addr().expect("mock server address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept responses request");
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read request headers");
                assert!(count > 0, "connection closed before request headers");
                request.extend_from_slice(&chunk[..count]);
                if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("content-length header");
            while request.len() < header_end + content_length {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read request body");
                assert!(count > 0, "connection closed before request body");
                request.extend_from_slice(&chunk[..count]);
            }
            request_tx.send(request).expect("capture request");

            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write responses stream");
        });

        let config = AssistantConfig {
            api_key: "test-token".to_string(),
            chat_model: "gpt-test".to_string(),
            chat_model_choices: Vec::new(),
            base_url: format!("http://{address}"),
            custom_headers: Vec::new(),
            provider: "Custom".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "api_key".to_string(),
            chat_tools_enabled: true,
            native_web_search: true,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_secs(5));
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "pwd",
                "description": "Print cwd",
                "parameters": { "type": "object", "properties": {} }
            }
        })];
        let mut text = String::new();
        let calls = client
            .chat_step(
                "gpt-test",
                &[ApiMessage::user("hello")],
                &tools,
                true,
                &AtomicBool::new(false),
                &mut |token| text.push_str(token),
                &mut |_| {},
            )
            .expect("responses request");
        assert_eq!(text, "ok");
        assert!(calls.tool_calls.is_empty());

        server.join().expect("mock responses server");
        let request = request_rx.recv().expect("captured request");
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("request separator")
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /responses http/1.1"));
        assert!(headers.contains("authorization: bearer test-token"));
        let body: serde_json::Value =
            serde_json::from_slice(&request[header_end..]).expect("request JSON");
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert!(body["tools"]
            .as_array()
            .expect("responses tools")
            .iter()
            .any(|tool| tool == &serde_json::json!({ "type": "web_search" })));
        assert!(body["tools"]
            .as_array()
            .expect("responses tools")
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "pwd"));
        assert!(
            body.get("include").is_none(),
            "custom Responses endpoints must not receive OpenAI-only include values"
        );
    }

    #[test]
    fn codex_connection_posts_to_custom_provider_with_headers_and_query() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Codex provider");
        let address = listener.local_addr().expect("mock provider address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Codex request");
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read Codex headers");
                assert!(count > 0, "connection closed before Codex headers");
                request.extend_from_slice(&chunk[..count]);
                if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("Codex content-length header");
            while request.len() < header_end + content_length {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read Codex body");
                assert!(count > 0, "connection closed before Codex body");
                request.extend_from_slice(&chunk[..count]);
            }
            request_tx.send(request).expect("capture Codex request");

            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"followed\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write Codex response");
        });

        let config = AssistantConfig {
            api_key: String::new(),
            chat_model: "gpt-followed".to_string(),
            chat_model_choices: Vec::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            custom_headers: Vec::new(),
            provider: "Codex".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "codex".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_secs(5));
        let connection = CodexConnection {
            model: Some("gpt-followed".to_string()),
            reasoning_effort: Some("high".to_string()),
            reasoning_summary: Some("auto".to_string()),
            base_url: format!("http://{address}/v1"),
            headers: vec![("x-codex-provider".to_string(), "followed".to_string())],
            query_params: vec![("tenant".to_string(), "kaku".to_string())],
            credential: CodexCredential::None,
            request_max_attempts: 1,
            stream_max_retries: 0,
            stream_idle_timeout_ms: None,
        };
        let mut text = String::new();
        client
            .chat_step_codex_connection(
                &connection,
                "gpt-followed",
                &[ApiMessage::user("hello")],
                &[],
                &AtomicBool::new(false),
                &mut |token| text.push_str(token),
                &mut |_| {},
            )
            .expect("Codex provider request");
        assert_eq!(text, "followed");

        server.join().expect("mock Codex provider");
        let request = request_rx.recv().expect("captured Codex request");
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("Codex request separator")
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/responses?tenant=kaku http/1.1"));
        assert!(headers.contains("x-codex-provider: followed"));
        assert!(!headers.contains("authorization:"));
        let body: serde_json::Value =
            serde_json::from_slice(&request[header_end..]).expect("Codex request JSON");
        assert_eq!(body["model"], "gpt-followed");
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn codex_model_discovery_uses_provider_models_endpoint_and_connection_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Codex provider");
        let address = listener.local_addr().expect("mock provider address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept models request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read models request");
                assert!(count > 0, "connection closed before models headers");
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            request_tx.send(request).expect("capture models request");

            let body = r#"{"data":[{"id":"custom-chat"},{"id":"text-embedding-3-small"},{"id":"custom-fast"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write models response");
        });

        let config = AssistantConfig {
            api_key: String::new(),
            chat_model: "custom-chat".to_string(),
            chat_model_choices: Vec::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            custom_headers: Vec::new(),
            provider: "Codex".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "codex".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_secs(5));
        let connection = CodexConnection {
            model: None,
            reasoning_effort: None,
            reasoning_summary: None,
            base_url: format!("http://{address}/v1"),
            headers: vec![("x-codex-provider".to_string(), "followed".to_string())],
            query_params: vec![("tenant".to_string(), "kaku".to_string())],
            credential: CodexCredential::Bearer("models-key".to_string()),
            request_max_attempts: 1,
            stream_max_retries: 0,
            stream_idle_timeout_ms: None,
        };

        let model = client
            .resolve_codex_request_model(&connection, crate::codex_connection::FOLLOW_CODEX_MODEL)
            .expect("discover Codex provider model");
        assert_eq!(model, "custom-chat");

        server.join().expect("mock models provider");
        let request = request_rx.recv().expect("captured models request");
        let headers = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(headers.starts_with("get /v1/models?tenant=kaku http/1.1"));
        assert!(headers.contains("x-codex-provider: followed"));
        assert!(headers.contains("authorization: bearer models-key"));
    }

    #[test]
    fn codex_chatgpt_requests_honor_provider_retry_count() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry provider");
        let address = listener.local_addr().expect("retry provider address");
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept retry request");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).expect("read retry request");
                    assert!(count > 0, "connection closed before retry headers");
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&request).to_ascii_lowercase();
                assert!(headers.contains("authorization: bearer oauth-test"));
                let status = if attempt == 0 {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .expect("write retry response");
            }
        });

        let config = AssistantConfig {
            api_key: String::new(),
            chat_model: "custom-chat".to_string(),
            chat_model_choices: Vec::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            custom_headers: Vec::new(),
            provider: "Codex".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "codex".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_secs(5));
        let endpoint = format!("http://{address}/models");
        let build = |credential: &CodexCredential| {
            let CodexCredential::ChatGpt(auth) = credential else {
                panic!("expected ChatGPT credential");
            };
            client
                .client
                .get(&endpoint)
                .header("Authorization", format!("Bearer {}", auth.access_token))
        };
        let mut credential = CodexCredential::ChatGpt(crate::ai_auth::CodexAuth {
            access_token: "oauth-test".to_string(),
            account_id: None,
        });
        let response = client
            .send_codex_request_with_retry(
                &mut credential,
                &client.client,
                build,
                "retry provider",
                &AtomicBool::new(false),
                2,
            )
            .expect("ChatGPT request retries");
        assert!(response.status().is_success());
        server.join().expect("retry provider server");
    }

    #[test]
    fn codex_stream_idle_timeout_resets_when_data_keeps_arriving() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind streaming provider");
        let address = listener.local_addr().expect("streaming provider address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept streaming request");
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read streaming headers");
                assert!(count > 0, "connection closed before streaming headers");
                request.extend_from_slice(&chunk[..count]);
                if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("streaming content-length header");
            while request.len() < header_end + content_length {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).expect("read streaming body");
                assert!(count > 0, "connection closed before streaming body");
                request.extend_from_slice(&chunk[..count]);
            }

            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )
            .expect("write streaming headers");
            write!(
                stream,
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"still \"}}\n\n"
            )
            .expect("write first stream event");
            std::thread::sleep(std::time::Duration::from_millis(300));
            write!(
                stream,
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"alive\"}}\n\n"
            )
            .expect("write second stream event");
            std::thread::sleep(std::time::Duration::from_millis(300));
            write!(
                stream,
                "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"output\":[]}}}}\n\n"
            )
            .expect("write completed event");
        });

        let config = AssistantConfig {
            api_key: String::new(),
            chat_model: "custom-chat".to_string(),
            chat_model_choices: Vec::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            custom_headers: Vec::new(),
            provider: "Codex".to_string(),
            api_mode: ApiMode::Responses,
            auth_type: "codex".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new_with_timeout(config, std::time::Duration::from_secs(5));
        let connection = CodexConnection {
            model: Some("custom-chat".to_string()),
            reasoning_effort: None,
            reasoning_summary: None,
            base_url: format!("http://{address}"),
            headers: Vec::new(),
            query_params: Vec::new(),
            credential: CodexCredential::None,
            request_max_attempts: 1,
            stream_max_retries: 0,
            stream_idle_timeout_ms: Some(500),
        };
        let mut text = String::new();
        client
            .chat_step_codex_connection(
                &connection,
                "custom-chat",
                &[ApiMessage::user("hello")],
                &[],
                &AtomicBool::new(false),
                &mut |token| text.push_str(token),
                &mut |_| {},
            )
            .expect("active stream outlives one idle-timeout window");
        assert_eq!(text, "still alive");
        server.join().expect("streaming provider server");
    }

    #[test]
    fn mock_sse_routes_fireworks_reasoning_content_before_visible_content() {
        let (tokens, reasoning) = route_mock_sse_lines(&[
            r#"data: {"choices":[{"delta":{"reasoning_content":"hidden "},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"visible"},"finish_reason":null}]}"#,
            "data: [DONE]",
        ]);

        assert_eq!(reasoning, "hidden ");
        assert_eq!(tokens, "visible");
    }

    #[test]
    fn mock_sse_inline_think_tags_split_across_chunks_do_not_leak() {
        let (tokens, reasoning) = route_mock_sse_lines(&[
            r#"data: {"choices":[{"delta":{"content":"<THI"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"NK >one</ TH"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"INK >visible<think"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"ing>two</thinking>"},"finish_reason":null}]}"#,
            "data: [DONE]",
        ]);

        assert_eq!(reasoning, "onetwo");
        assert_eq!(tokens, "visible");
        assert!(!tokens.to_ascii_lowercase().contains("think"));
    }

    #[test]
    fn reasoning_roundtrip_is_limited_to_reasoning_models() {
        assert!(should_roundtrip_reasoning_content("deepseek-v4-pro"));
        assert!(should_roundtrip_reasoning_content("Kimi-K2.5"));
        assert!(should_roundtrip_reasoning_content("mimo-thinking"));
        assert!(!should_roundtrip_reasoning_content("gpt-5.4"));
        assert!(!should_roundtrip_reasoning_content(
            "gemini-3-flash-preview"
        ));
    }

    #[test]
    fn custom_headers_replace_existing_user_agent_without_dropping_auth() {
        let config = AssistantConfig {
            api_key: "test-token".to_string(),
            chat_model: "gpt-test".to_string(),
            chat_model_choices: Vec::new(),
            base_url: "https://example.test/v1".to_string(),
            custom_headers: vec![
                ("User-Agent".to_string(), "Sumi-Test".to_string()),
                ("X-Customer-ID".to_string(), "acme".to_string()),
            ],
            provider: "Custom".to_string(),
            api_mode: ApiMode::ChatCompletions,
            auth_type: "api_key".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };
        let client = AiClient::new(config);
        let request = reqwest::blocking::Client::new()
            .post("https://example.test/v1/chat/completions")
            .header(USER_AGENT, "reqwest-default");

        let request = client.apply_auth_headers(request).unwrap().build().unwrap();
        let headers = request.headers();
        let user_agents = headers.get_all(USER_AGENT).iter().collect::<Vec<_>>();

        assert_eq!(user_agents.len(), 1);
        assert_eq!(user_agents[0], "Sumi-Test");
        assert_eq!(
            headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(
            headers.get("X-Customer-ID").and_then(|v| v.to_str().ok()),
            Some("acme")
        );
    }

    #[test]
    fn one_shot_client_does_not_multiply_inline_timeout_with_retries() {
        let config = AssistantConfig {
            api_key: "test-token".to_string(),
            chat_model: "gpt-test".to_string(),
            chat_model_choices: Vec::new(),
            base_url: "https://example.test/v1".to_string(),
            custom_headers: Vec::new(),
            provider: "Custom".to_string(),
            api_mode: ApiMode::ChatCompletions,
            auth_type: "api_key".to_string(),
            chat_tools_enabled: true,
            native_web_search: false,
            web_search_provider: None,
            web_search_api_key: None,
            web_fetch_script: None,
            fast_model: None,
            memory_curator_model: None,
        };

        let client = AiClient::new_with_timeout(config, std::time::Duration::from_millis(100));
        assert_eq!(client.max_request_attempts, 1);
    }

    #[test]
    fn mock_sse_skips_malformed_json_chunks() {
        let lines = vec![
            "data: {not json}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "data: [DONE]",
        ];
        let (tokens, reasoning) = route_mock_sse_lines(&lines);
        assert_eq!(tokens, "hi");
        assert!(reasoning.is_empty());
    }

    #[test]
    fn mock_sse_skips_chunks_with_empty_choices() {
        // Some providers (Anthropic-compat shims, certain proxies) send
        // keep-alive chunks with empty `choices` arrays. Must not panic on
        // `choices[0]` indexing.
        let lines = vec![
            "data: {\"choices\":[]}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
            "data: [DONE]",
        ];
        let (tokens, _) = route_mock_sse_lines(&lines);
        assert_eq!(tokens, "ok");
    }

    #[test]
    fn mock_sse_ignores_html_error_page() {
        // CDN / reverse-proxy failure modes occasionally return an HTML
        // 502/504 with `data:` prefix injected by middleware. We must walk
        // off the end without crashing or fabricating output.
        let lines = vec![
            "data: <html>",
            "data: <body>502 Bad Gateway</body>",
            "data: </html>",
        ];
        let (tokens, reasoning) = route_mock_sse_lines(&lines);
        assert!(tokens.is_empty());
        assert!(reasoning.is_empty());
    }

    #[test]
    fn mock_sse_handles_interleaved_done_and_data() {
        // [DONE] must terminate the stream even if more data lines follow
        // (some providers leak trailing chunks during connection close).
        let lines = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}",
            "data: [DONE]",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ignored\"}}]}",
        ];
        let (tokens, _) = route_mock_sse_lines(&lines);
        assert_eq!(tokens, "a");
    }
}
