//! Anthropic Messages streaming client — Rust port of `src/core/api.ts`.
//!
//! The TS version delegates to `@anthropic-ai/sdk`; here we own the wire
//! protocol. Responsibilities (ARCHITECTURE.md §4.2):
//! 1. Request assembly (system blocks, tools as JSON Schema, thinking beta)
//! 2. SSE decoding into `AgentEvent`s (same state machine as api.ts)
//! 3. Retry on connection establishment (with_retry, 5 tries, backoff)
//! 4. Model registry (sonnet/opus/haiku + aliases + partial match)

use crate::core::errors::{classify_error, with_retry, NanocodeError, RetryOptions};
use crate::core::types::{
    AgentEvent, ContentBlock, Message, ModelConfig, SystemPromptBlock, TokenUsage, ToolUseBlock,
};
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
const THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

// ---------------------------------------------------------------------------
// Model registry (api.ts MODEL_CONFIGS)
// ---------------------------------------------------------------------------

/// name → (output, input, cache_read, cache_write prices USD/MTok, thinking,
///          context_window, max_output_tokens). GLM entries match the Zhipu
/// BigModel Anthropic-compatible endpoint (open.bigmodel.cn/api/anthropic);
/// coding-plan tiers bill by subscription, per-token prices set to 0.
fn raw_model_configs() -> Vec<RawModel> {
    vec![
        RawModel { name: "claude-sonnet-4-20250514", out: 15.0, inp: 3.0, cr: 0.3, cw: 3.75, thinking: true, ctx: 200_000, max_out: 16_384 },
        RawModel { name: "claude-opus-4-20250514", out: 75.0, inp: 15.0, cr: 1.5, cw: 18.75, thinking: true, ctx: 200_000, max_out: 16_384 },
        RawModel { name: "claude-haiku-4-5-20251001", out: 4.0, inp: 0.8, cr: 0.08, cw: 1.0, thinking: false, ctx: 200_000, max_out: 16_384 },
        RawModel { name: "GLM-5.3", out: 0.0, inp: 0.0, cr: 0.0, cw: 0.0, thinking: true, ctx: 1_000_000, max_out: 128_000 },
        RawModel { name: "GLM-5.3-Flash", out: 0.0, inp: 0.0, cr: 0.0, cw: 0.0, thinking: true, ctx: 1_000_000, max_out: 128_000 },
        // OpenAI (native provider, src/core/openai.rs): 128K ctx / 16K out;
        // newer models resolve through partial match or the default path.
        RawModel { name: "gpt-4o", out: 2.5, inp: 10.0, cr: 1.25, cw: 0.0, thinking: false, ctx: 128_000, max_out: 16_384 },
        RawModel { name: "gpt-4o-mini", out: 0.15, inp: 0.6, cr: 0.075, cw: 0.0, thinking: false, ctx: 128_000, max_out: 16_384 },
    ]
}

struct RawModel {
    name: &'static str,
    out: f64,
    inp: f64,
    cr: f64,
    cw: f64,
    thinking: bool,
    ctx: u64,
    max_out: u64,
}

fn build_model_config(raw: &RawModel, model: &str) -> ModelConfig {
    ModelConfig {
        model: model.to_string(),
        context_window: raw.ctx,
        max_output_tokens: raw.max_out,
        supports_thinking: raw.thinking,
        supports_caching: true,
        price_per_input_token: raw.inp / 1_000_000.0,
        price_per_output_token: raw.out / 1_000_000.0,
        price_per_cache_read: raw.cr / 1_000_000.0,
        price_per_cache_write: raw.cw / 1_000_000.0,
    }
}

/// Resolve a model name: exact / alias / partial match / fallback sonnet
/// with the requested name (api.ts getModelConfig).
pub fn get_model_config(model: &str) -> ModelConfig {
    let configs = raw_model_configs();
    let aliases: &[(&str, usize)] = &[("sonnet", 0), ("opus", 1), ("haiku", 2)];

    // Exact match (case-insensitive so "glm-5.3-flash" matches the registry)
    let lower = model.to_lowercase();
    if let Some(raw) = configs.iter().find(|c| c.name.to_lowercase() == lower) {
        return build_model_config(raw, model);
    }
    // Alias
    if let Some((_, idx)) = aliases.iter().find(|(a, _)| *a == model) {
        return build_model_config(&configs[*idx], model);
    }
    // Partial match (either direction contains)
    for raw in &configs {
        if raw.name.contains(model) || model.contains(raw.name) {
            return build_model_config(raw, model);
        }
    }
    for (alias, idx) in aliases {
        if model.contains(alias) {
            return build_model_config(&configs[*idx], model);
        }
    }
    // Default: sonnet config with the requested model name
    build_model_config(&configs[0], model)
}

// ---------------------------------------------------------------------------
// Call parameters
// ---------------------------------------------------------------------------

/// A tool as the API needs it. `tools::registry` converts its trait objects
/// into this flat representation (api.ts toolToAPI).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone)]
pub struct CallModelParams {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub model_config: ModelConfig,
    pub system_prompt_blocks: Vec<SystemPromptBlock>,
    pub enable_thinking: bool,
    pub thinking_budget: Option<u64>,
    pub cancel: CancellationToken,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ModelClient {
    http: reqwest::Client,
    base_url: String,
}

impl ModelClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-api-key", api_key.parse().expect("api key header"));
        headers.insert("anthropic-version", ANTHROPIC_VERSION.parse().unwrap());
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("http client");
        ModelClient {
            http,
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
        }
    }

    /// Env fallbacks mirroring api.ts createClient: `ANTHROPIC_BASE_URL` for
    /// OpenRouter-style gateways; empty key falls back to `OPENROUTER_API_KEY`.
    pub fn from_env(api_key: &str) -> Self {
        let key = if api_key.is_empty() {
            std::env::var("OPENROUTER_API_KEY").unwrap_or_default()
        } else {
            api_key.to_string()
        };
        let base = std::env::var("ANTHROPIC_BASE_URL").ok();
        Self::new(&key, base.as_deref())
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn build_body(params: &CallModelParams) -> serde_json::Value {
        let thinking = params.enable_thinking && params.model_config.supports_thinking;
        let mut body = serde_json::json!({
            "model": params.model_config.model,
            "max_tokens": params.model_config.max_output_tokens,
            "system": params.system_prompt_blocks,
            "messages": params.messages,
            "stream": true,
        });
        if !params.tools.is_empty() {
            let tools: Vec<serde_json::Value> = params
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }
        if thinking {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": params.thinking_budget.unwrap_or(10_000),
            });
        }
        body
    }

    /// POST /v1/messages and return the raw response. Connection-level errors
    /// (non-2xx, transport) are classified for the retry loop.
    async fn post_stream(
        &self,
        params: &CallModelParams,
    ) -> Result<reqwest::Response, NanocodeError> {
        let thinking = params.enable_thinking && params.model_config.supports_thinking;
        let mut req = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .json(&Self::build_body(params));
        if thinking {
            req = req.header("anthropic-beta", THINKING_BETA);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let body = resp.text().await.unwrap_or_default();
                Err(classify_error(Some(status), retry_after.as_deref(), &body))
            }
            Err(e) => Err(classify_error(None, None, &e.to_string())),
        }
    }

    /// Streaming model call — yields the same event sequence as api.ts
    /// callModel: assistant_text/thinking deltas, tool_use blocks,
    /// assistant_message, usage, turn_complete; or a terminal error event.
    pub fn call_model(&self, params: CallModelParams) -> impl Stream<Item = AgentEvent> {
        let client = self.clone();
        async_stream::stream! {
            let cancel = params.cancel.clone();

            // Connection + headers with retry (api.ts wraps the stream creation).
            let mut retry_opts = RetryOptions::defaults();
            retry_opts.on_retry = Some(Box::new(|err, attempt, delay| {
                eprintln!(
                    "\x1b[33m[retry {attempt}] {err}: {} (waiting {}s)\x1b[0m",
                    err_message(err),
                    delay / 1000
                );
            }));

            let response = match with_retry(
                || client.post_stream(&params),
                &retry_opts,
                &cancel,
            )
            .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    yield AgentEvent::Error { error: err };
                    return;
                }
            };

            // Decode the SSE body.
            let byte_stream = response.bytes_stream();
            for await event in decode_sse(byte_stream, cancel.clone()) {
                yield event;
            }
        }
    }
}

fn err_message(err: &NanocodeError) -> &str {
    match err {
        NanocodeError::RateLimit { message, .. }
        | NanocodeError::Overloaded { message }
        | NanocodeError::Network { message }
        | NanocodeError::Other { message } => message,
        NanocodeError::PromptTooLong { message, .. } => message,
        NanocodeError::Authentication { message, .. } => message,
        NanocodeError::ToolExecution { message, .. } => message,
        NanocodeError::Abort => "Operation aborted",
    }
}

// ---------------------------------------------------------------------------
// SSE decoding (api.ts stream event switch)
// ---------------------------------------------------------------------------

struct StreamAccumulator {
    tool_use_blocks: Vec<ToolUseBlock>,
    current_tool: Option<(String, String)>, // (id, name)
    current_tool_json: String,
    text_content: String,
    thinking_content: String,
    thinking_signature: String,
    usage: TokenUsage,
    stop_reason: String,
}

impl StreamAccumulator {
    fn new() -> Self {
        StreamAccumulator {
            tool_use_blocks: Vec::new(),
            current_tool: None,
            current_tool_json: String::new(),
            text_content: String::new(),
            thinking_content: String::new(),
            thinking_signature: String::new(),
            usage: TokenUsage::default(),
            stop_reason: "end_turn".to_string(),
        }
    }
}

fn u64_field(obj: &serde_json::Value, key: &str) -> u64 {
    obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Process one SSE `data:` payload against the accumulator; may emit events.
fn handle_data(acc: &mut StreamAccumulator, data: &str) -> Vec<AgentEvent> {
    let parsed: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Vec::new(), // debug output / keep-alive — ignore
    };
    let mut events = Vec::new();

    match parsed.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "content_block_start" => {
            let block = &parsed["content_block"];
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    acc.current_tool = Some((
                        block["id"].as_str().unwrap_or_default().to_string(),
                        block["name"].as_str().unwrap_or_default().to_string(),
                    ));
                    acc.current_tool_json.clear();
                }
                Some("thinking") => {
                    acc.thinking_content.clear();
                    acc.thinking_signature.clear();
                }
                _ => {}
            }
        }

        "content_block_delta" => {
            let delta = &parsed["delta"];
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    if let Some(text) = delta["text"].as_str() {
                        acc.text_content.push_str(text);
                        events.push(AgentEvent::AssistantText { text: text.to_string() });
                    }
                }
                Some("input_json_delta") => {
                    if let Some(pj) = delta["partial_json"].as_str() {
                        acc.current_tool_json.push_str(pj);
                    }
                }
                Some("thinking_delta") => {
                    if let Some(t) = delta["thinking"].as_str() {
                        acc.thinking_content.push_str(t);
                        events.push(AgentEvent::Thinking { text: t.to_string() });
                    }
                }
                Some("signature_delta") => {
                    if let Some(sig) = delta["signature"].as_str() {
                        acc.thinking_signature.push_str(sig);
                    }
                }
                _ => {}
            }
        }

        "content_block_stop" => {
            if let Some((id, name)) = acc.current_tool.take() {
                let input: serde_json::Value = if acc.current_tool_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&acc.current_tool_json).unwrap_or(serde_json::json!({}))
                };
                acc.current_tool_json.clear();
                let block = ToolUseBlock { id, name, input: input.clone() };
                events.push(AgentEvent::ToolUse { tool_use: block.clone() });
                acc.tool_use_blocks.push(block);
            }
        }

        "message_start" => {
            let usage = &parsed["message"]["usage"];
            if usage.is_object() {
                acc.usage.input_tokens = u64_field(usage, "input_tokens");
                acc.usage.cache_read_tokens = u64_field(usage, "cache_read_input_tokens");
                acc.usage.cache_creation_tokens = u64_field(usage, "cache_creation_input_tokens");
            }
        }

        "message_delta" => {
            // Final usage arrives here on Anthropic and GLM alike; GLM only
            // populates input_tokens in message_delta (message_start is 0).
            if let Some(usage) = parsed.get("usage") {
                let out = u64_field(usage, "output_tokens");
                if out > 0 {
                    acc.usage.output_tokens = out;
                }
                let inp = u64_field(usage, "input_tokens");
                if inp > 0 {
                    acc.usage.input_tokens = inp;
                }
                let cr = u64_field(usage, "cache_read_input_tokens");
                if cr > 0 {
                    acc.usage.cache_read_tokens = cr;
                }
                let cc = u64_field(usage, "cache_creation_input_tokens");
                if cc > 0 {
                    acc.usage.cache_creation_tokens = cc;
                }
            }
            if let Some(reason) = parsed["delta"]["stop_reason"].as_str() {
                acc.stop_reason = reason.to_string();
            }
        }

        "error" => {
            let msg = parsed["error"]["message"]
                .as_str()
                .unwrap_or("unknown stream error")
                .to_string();
            let classified = classify_error(None, None, &msg);
            events.push(AgentEvent::Error { error: classified });
        }

        _ => {} // message_stop, ping, …
    }

    events
}

/// Full SSE decoder: bytes → lines → frames → AgentEvents, ending with the
/// assembled assistant_message / usage / turn_complete (or an error event).
fn decode_sse(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    cancel: CancellationToken,
) -> impl Stream<Item = AgentEvent> {
    async_stream::stream! {
        let mut bytes = byte_stream;
        let mut line_buf: Vec<u8> = Vec::new();
        let mut event_name = String::new();
        let mut data_buf = String::new();
        let mut acc = StreamAccumulator::new();
        let mut stream_broken = false;

        while let Some(chunk) = tokio::select! {
            biased;
            _ = cancel.cancelled() => { stream_broken = true; None }
            chunk = bytes.next() => chunk,
        } {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    let err = classify_error(None, None, &e.to_string());
                    yield AgentEvent::Error { error: err };
                    return;
                }
            };
            line_buf.extend_from_slice(&chunk);

            while let Some(pos) = line_buf.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = line_buf.drain(..=pos).collect();
                let mut line = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                if line.ends_with('\r') {
                    line.pop();
                }

                if line.is_empty() {
                    if !data_buf.is_empty() {
                        for ev in handle_data(&mut acc, &data_buf) {
                            let terminal_error = matches!(&ev, AgentEvent::Error { .. });
                            yield ev;
                            if terminal_error {
                                return;
                            }
                        }
                        data_buf.clear();
                        event_name.clear();
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix("event:") {
                    event_name = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_buf.push_str(rest.trim_start());
                }
                // comment lines (`: ping`) ignored
            }
        }
        let _ = event_name;
        if stream_broken {
            return; // cancelled mid-stream — no final assembly
        }

        // Flush a trailing frame without newline.
        if !data_buf.is_empty() {
            for ev in handle_data(&mut acc, &data_buf) {
                let terminal_error = matches!(&ev, AgentEvent::Error { .. });
                yield ev;
                if terminal_error {
                    return;
                }
            }
        }

        // Assemble the complete assistant message: thinking → text → tool_use.
        let mut content: Vec<ContentBlock> = Vec::new();
        if !acc.thinking_content.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: acc.thinking_content.clone(),
                signature: (!acc.thinking_signature.is_empty())
                    .then(|| acc.thinking_signature.clone()),
            });
        }
        if !acc.text_content.is_empty() {
            content.push(ContentBlock::Text { text: acc.text_content.clone() });
        }
        for tu in &acc.tool_use_blocks {
            content.push(ContentBlock::ToolUse {
                id: tu.id.clone(),
                name: tu.name.clone(),
                input: tu.input.clone(),
            });
        }
        yield AgentEvent::AssistantMessage { message: Message { role: crate::core::types::Role::Assistant, content, id: None } };

        if acc.usage.input_tokens > 0 || acc.usage.output_tokens > 0 {
            yield AgentEvent::Usage { usage: acc.usage };
        }
        yield AgentEvent::TurnComplete { stop_reason: acc.stop_reason };
    }
}

// ---------------------------------------------------------------------------
// Provider selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProvider {
    Anthropic,
    OpenAI,
}

impl ModelProvider {
    pub fn parse(s: &str) -> Option<ModelProvider> {
        match s.to_lowercase().as_str() {
            "anthropic" => Some(ModelProvider::Anthropic),
            "openai" => Some(ModelProvider::OpenAI),
            _ => None,
        }
    }

    /// Build the caller for this provider. Provider-specific env fallbacks
    /// (OPENAI_API_KEY / OPENAI_BASE_URL) apply inside each client.
    pub fn build_caller(self, api_key: &str) -> std::sync::Arc<dyn ModelCaller> {
        match self {
            ModelProvider::Anthropic => std::sync::Arc::new(ModelClient::from_env(api_key)),
            ModelProvider::OpenAI => std::sync::Arc::new(crate::core::openai::OpenAIClient::from_env(api_key)),
        }
    }
}

// ---------------------------------------------------------------------------
// ModelCaller trait — injection seam for the agent loop (mock in tests)
// ---------------------------------------------------------------------------

use futures::stream::BoxStream;

#[async_trait::async_trait]
pub trait ModelCaller: Send + Sync {
    fn call_model(&self, params: CallModelParams) -> BoxStream<'static, AgentEvent>;
}

#[async_trait::async_trait]
impl ModelCaller for ModelClient {
    fn call_model(&self, params: CallModelParams) -> BoxStream<'static, AgentEvent> {
        
        Box::pin(ModelClient::call_model(self, params))
    }
}

// ---------------------------------------------------------------------------
// Tests — SSE fixtures mirroring test scenarios for api.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Role;

    fn params() -> CallModelParams {
        CallModelParams {
            messages: vec![Message { role: Role::User, content: vec![ContentBlock::text("hi")], id: None }],
            tools: vec![],
            model_config: get_model_config("sonnet"),
            system_prompt_blocks: vec![],
            enable_thinking: false,
            thinking_budget: None,
            cancel: CancellationToken::new(),
        }
    }

    fn sse_body() -> String {
        [
            "event: message_start",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2}}}"#,
            "",
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: message_delta",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n")
    }

    async fn setup_mock(body: String, status: u16) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(status)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn decodes_text_stream_with_usage() {
        let server = setup_mock(sse_body(), 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;

        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello ", "world"]);

        // Assistant message assembled: single text block
        let msg = events.iter().find_map(|e| match e {
            AgentEvent::AssistantMessage { message } => Some(message),
            _ => None,
        }).expect("assistant message");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 1);
        assert_eq!(msg.content[0].as_text(), Some("Hello world"));

        let usage = events.iter().find_map(|e| match e {
            AgentEvent::Usage { usage } => Some(*usage),
            _ => None,
        }).expect("usage");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_read_tokens, 5);
        assert_eq!(usage.cache_creation_tokens, 2);

        let stop = events.iter().find_map(|e| match e {
            AgentEvent::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        }).expect("turn complete");
        assert_eq!(stop, "end_turn");
    }

    #[tokio::test]
    async fn decodes_tool_use_with_bad_json_input() {
        let body = [
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"and\": \"ls\"}"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
        ]
        .join("\n");
        let server = setup_mock(body, 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;

        let tu = events.iter().find_map(|e| match e {
            AgentEvent::ToolUse { tool_use } => Some(tool_use.clone()),
            _ => None,
        })
        .expect("tool use");
        assert_eq!(tu.id, "toolu_1");
        assert_eq!(tu.name, "Bash");
        assert_eq!(tu.input, serde_json::json!({"command": "ls"}));
    }

    #[tokio::test]
    async fn bad_json_input_falls_back_to_empty_object() {
        let body = [
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t2","name":"Read"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"not-json{"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
        ]
        .join("\n");
        let server = setup_mock(body, 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        let tu = events.iter().find_map(|e| match e {
            AgentEvent::ToolUse { tool_use } => Some(tool_use.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(tu.input, serde_json::json!({}));
    }

    #[tokio::test]
    async fn error_event_classifies_to_terminal_error() {
        let body = [
            "event: error",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "",
        ]
        .join("\n");
        let server = setup_mock(body, 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Error { error: NanocodeError::Other { .. } }
        ));
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        // First call → 429, subsequent → SSE body.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(wiremock::ResponseTemplate::new(429).set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body()),
            )
            .mount(&server)
            .await;

        let client = ModelClient::new("k", Some(&server.uri()));
        let mut p = params();
        p.cancel = CancellationToken::new();
        let events: Vec<AgentEvent> = client.call_model(p).collect().await;
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnComplete { .. })));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn non_retryable_auth_fails_immediately() {
        let server = setup_mock("{\"error\":\"bad key\"}".to_string(), 401).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::Error { error: NanocodeError::Authentication { status: 401, .. } }
        ));
    }

    #[tokio::test]
    async fn thinking_deltas_stream_and_assemble() {
        let body = [
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm "}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ok"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "",
        ]
        .join("\n");
        let server = setup_mock(body, 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;

        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "hmm ok");

        let msg = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::AssistantMessage { message } => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        // Order: thinking → text
        assert!(matches!(&msg.content[0], ContentBlock::Thinking { .. }));
        assert_eq!(msg.content[1].as_text(), Some("answer"));
    }

    #[test]
    fn glm_models_registered_with_zhipu_limits() {
        let flash = get_model_config("GLM-5.3-Flash");
        assert_eq!(flash.model, "GLM-5.3-Flash");
        assert_eq!(flash.context_window, 1_000_000);
        assert_eq!(flash.max_output_tokens, 128_000);
        assert!(flash.supports_thinking);
        assert_eq!(flash.price_per_input_token, 0.0); // coding-plan subscription

        // Case-insensitive exact match
        let lower = get_model_config("glm-5.3-flash");
        assert_eq!(lower.context_window, 1_000_000);

        let full = get_model_config("GLM-5.3");
        assert_eq!(full.context_window, 1_000_000);
        // partial match still works
        assert_eq!(get_model_config("GLM-5.3").model, "GLM-5.3");
    }

    #[tokio::test]
    async fn signature_delta_accumulates_into_thinking_block() {
        let body = [
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "",
        ].join("\n");
        let server = setup_mock(body, 200).await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        let msg = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::AssistantMessage { message } => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        match &msg.content[0] {
            ContentBlock::Thinking { thinking, signature } => {
                assert_eq!(thinking, "hmm");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("{other:?}"),
        }
        // Round-trips through serialization with the signature preserved
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"][0]["signature"], "sig-abc");
    }

    #[test]
    fn model_registry_aliases_and_fallback() {
        assert_eq!(get_model_config("sonnet").model, "sonnet");
        assert_eq!(get_model_config("opus").model, "opus");
        assert_eq!(get_model_config("haiku").model, "haiku");
        assert!(get_model_config("claude-sonnet-4-20250514").supports_thinking);
        assert!(!get_model_config("haiku").supports_thinking);
        // Partial match
        assert_eq!(get_model_config("claude-sonnet-4").model, "claude-sonnet-4");
        // Unknown → sonnet config with requested name
        let fallback = get_model_config("gpt-99");
        assert_eq!(fallback.model, "gpt-99");
        assert_eq!(fallback.context_window, 200_000);
    }

    #[tokio::test]
    async fn request_body_shape() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body()),
            )
            .mount(&server)
            .await;
        let client = ModelClient::new("k", Some(&server.uri()));
        let mut p = params();
        p.tools = vec![ToolSpec {
            name: "Bash".into(),
            description: "run".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let _: Vec<AgentEvent> = client.call_model(p).collect().await;

        let req = &server.received_requests().await.unwrap()[0];
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "sonnet");
        assert_eq!(body["max_tokens"], 16_384);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("thinking").is_none());
        assert_eq!(
            req.headers.get("anthropic-version").unwrap().to_str().unwrap(),
            ANTHROPIC_VERSION
        );
        assert_eq!(req.headers.get("x-api-key").unwrap().to_str().unwrap(), "k");
    }
}
