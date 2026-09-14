//! OpenAI Chat Completions client — native provider support.
//!
//! The internal message representation stays Anthropic-shaped (RUST_DESIGN
//! §4.6); this module converts at the boundary:
//! - outbound: internal messages → OpenAI `messages` array (system role,
//!   tool / tool_calls messages, thinking dropped)
//! - inbound: OpenAI SSE chunks → the same `AgentEvent` stream the Anthropic
//!   client produces (content deltas, tool_call aggregation, [DONE] assembly)
//!
//! Auth: `Authorization: Bearer`. Works with api.openai.com and any
//! OpenAI-compatible endpoint (DeepSeek, vLLM, llama.cpp, GLM paas/v4, …).

use async_trait::async_trait;
use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use crate::core::api::{CallModelParams, ModelCaller, ToolSpec};
use crate::core::errors::{classify_error, with_retry, NanocodeError, RetryOptions};
use crate::core::types::{
    AgentEvent, ContentBlock, Message, Role, ToolResultContent, ToolUseBlock,
};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

// ---------------------------------------------------------------------------
// Model registry additions (OpenAI)
// ---------------------------------------------------------------------------

/// Known OpenAI models; unknown names fall back via `get_model_config`'s
/// partial-match/default path with a sane 128K window.
pub fn is_openai_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.starts_with("gpt-") || lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("o4")
}

// ---------------------------------------------------------------------------
// Outbound conversion: internal (Anthropic-shaped) → OpenAI messages
// ---------------------------------------------------------------------------

/// Convert the internal history into OpenAI chat messages. The first message
/// is the system prompt when system blocks exist.
pub fn convert_messages(system_blocks: &[crate::core::types::SystemPromptBlock], messages: &[Message]) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();

    // System prompt: concatenated block texts (cache_control has no OpenAI
    // equivalent; the boundary marker block is dropped).
    let system_text: String = system_blocks
        .iter()
        .map(|b| b.text.trim())
        .filter(|t| !t.is_empty() && *t != crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system_text.is_empty() {
        out.push(serde_json::json!({"role": "system", "content": system_text}));
    }

    for msg in messages {
        match msg.role {
            Role::User => {
                // tool_result blocks become standalone tool messages (they
                // must follow the assistant tool_calls message); text blocks
                // after them become a normal user message.
                let mut text_parts: Vec<String> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                            let text = match content {
                                ToolResultContent::Text(s) => s.clone(),
                                ToolResultContent::Blocks(blocks) => {
                                    serde_json::to_string(blocks).unwrap_or_default()
                                }
                            };
                            let body = if *is_error == Some(true) {
                                format!("ERROR: {text}")
                            } else {
                                text
                            };
                            out.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": body,
                            }));
                        }
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        _ => {}
                    }
                }
                if !text_parts.is_empty() {
                    out.push(serde_json::json!({"role": "user", "content": text_parts.join("\n")}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls: Vec<Json> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                                }
                            }));
                        }
                        // Thinking blocks have no OpenAI round-trip form.
                        _ => {}
                    }
                }
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut entry = serde_json::json!({"role": "assistant"});
                if !text.is_empty() {
                    entry["content"] = Json::String(text);
                } else {
                    entry["content"] = Json::Null;
                }
                if !tool_calls.is_empty() {
                    entry["tool_calls"] = Json::Array(tool_calls);
                }
                out.push(entry);
            }
        }
    }
    out
}

/// Internal tools → OpenAI function-tool definitions.
fn convert_tools(tools: &[ToolSpec]) -> Vec<Json> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Inbound: OpenAI SSE → AgentEvent
// ---------------------------------------------------------------------------

/// Aggregated tool call being streamed (index-keyed).
#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct OpenAiAccumulator {
    text: String,
    reasoning: String,
    tool_calls: Vec<Option<PartialToolCall>>, // index-addressed
    finish_reason: String,
    usage: crate::core::types::TokenUsage,
    saw_first_chunk: bool,
}

impl OpenAiAccumulator {
    fn new() -> Self {
        OpenAiAccumulator {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            finish_reason: String::new(),
            usage: Default::default(),
            saw_first_chunk: false,
        }
    }
}

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" | "function_call" => "tool_use",
        "length" => "max_tokens",
        "stop" => "end_turn",
        _ => "end_turn",
    }
}

/// Process one SSE `data:` payload; returns events to emit and whether the
/// stream is finished ([DONE] sentinel handled by the decoder loop).
fn handle_chunk(acc: &mut OpenAiAccumulator, data: &str) -> (Vec<AgentEvent>, bool) {
    let mut events = Vec::new();
    let parsed: Json = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return (events, false),
    };

    // Some providers put the error object mid-stream
    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown OpenAI stream error");
        events.push(AgentEvent::Error { error: classify_error(None, None, msg) });
        return (events, true);
    }

    if let Some(usage) = parsed.get("usage").filter(|u| u.is_object()) {
        acc.usage.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        acc.usage.output_tokens =
            usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        // OpenAI-compatible caches: prompt_tokens_details.(cached_tokens|input_cached_tokens)
        if let Some(details) = usage.get("prompt_tokens_details").and_then(|d| d.as_object()) {
            let cached = details
                .get("cached_tokens")
                .or_else(|| details.get("input_cached_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            acc.usage.cache_read_tokens = cached;
        }
    }

    let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) else {
        return (events, false);
    };
    let Some(choice) = choices.first() else {
        return (events, false);
    };

    if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
        acc.finish_reason = reason.to_string();
    }

    let Some(delta) = choice.get("delta") else {
        return (events, false);
    };

    if !acc.saw_first_chunk {
        acc.saw_first_chunk = true;
    }

    // Reasoning content (DeepSeek / GLM style) → thinking events
    if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        if !r.is_empty() {
            acc.reasoning.push_str(r);
            events.push(AgentEvent::Thinking { text: r.to_string() });
        }
    } else if let Some(r) = delta.get("reasoning").and_then(|v| v.as_str()) {
        if !r.is_empty() {
            acc.reasoning.push_str(r);
            events.push(AgentEvent::Thinking { text: r.to_string() });
        }
    }

    // Content deltas
    if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
        if !c.is_empty() {
            acc.text.push_str(c);
            events.push(AgentEvent::AssistantText { text: c.to_string() });
        }
    }

    // Tool call deltas: index-addressed fragments of name/arguments
    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if acc.tool_calls.len() <= index {
                acc.tool_calls.resize(index + 1, None);
            }
            let slot = acc.tool_calls[index].get_or_insert_with(PartialToolCall::default);
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    slot.id = id.to_string();
                }
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    if !name.is_empty() {
                        slot.name.push_str(name);
                    }
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    slot.arguments.push_str(args);
                }
            }
        }
    }

    (events, false)
}

/// Final assembly on [DONE]: complete ToolUse events, the assistant message
/// (text → tool_use order), usage, turn_complete.
fn assemble(acc: &mut OpenAiAccumulator) -> Vec<AgentEvent> {
    let mut events = Vec::new();

    let mut completed: Vec<ToolUseBlock> = Vec::new();
    for slot in acc.tool_calls.iter().flatten() {
        let input: Json = if slot.arguments.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&slot.arguments).unwrap_or(serde_json::json!({}))
        };
        let id = if slot.id.is_empty() {
            format!("call_{}", uuid::Uuid::new_v4().simple())
        } else {
            slot.id.clone()
        };
        let block = ToolUseBlock { id, name: slot.name.clone(), input };
        events.push(AgentEvent::ToolUse { tool_use: block.clone() });
        completed.push(block);
    }

    let mut content: Vec<ContentBlock> = Vec::new();
    if !acc.text.is_empty() {
        content.push(ContentBlock::Text { text: acc.text.clone() });
    }
    for tu in &completed {
        content.push(ContentBlock::ToolUse {
            id: tu.id.clone(),
            name: tu.name.clone(),
            input: tu.input.clone(),
        });
    }

    events.push(AgentEvent::AssistantMessage {
        message: Message { role: Role::Assistant, content, id: None },
    });

    if acc.usage.input_tokens > 0 || acc.usage.output_tokens > 0 {
        events.push(AgentEvent::Usage { usage: acc.usage });
    }

    let stop = map_finish_reason(if acc.finish_reason.is_empty() {
        if completed.is_empty() { "stop" } else { "tool_calls" }
    } else {
        &acc.finish_reason
    });
    events.push(AgentEvent::TurnComplete { stop_reason: stop.to_string() });

    events
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OpenAIClient {
    http: reqwest::Client,
    base_url: String,
}

impl OpenAIClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {api_key}").parse().expect("bearer header"),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("http client");
        OpenAIClient {
            http,
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string(),
        }
    }

    /// Env-based construction: OPENAI_API_KEY / OPENAI_BASE_URL.
    pub fn from_env(api_key: &str) -> Self {
        let key = if api_key.is_empty() {
            std::env::var("OPENAI_API_KEY").unwrap_or_default()
        } else {
            api_key.to_string()
        };
        let base = std::env::var("OPENAI_BASE_URL").ok();
        Self::new(&key, base.as_deref())
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn build_body(params: &CallModelParams) -> Json {
        let mut body = serde_json::json!({
            "model": params.model_config.model,
            "messages": convert_messages(&params.system_prompt_blocks, &params.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": params.model_config.max_output_tokens,
        });
        let tools = convert_tools(&params.tools);
        if !tools.is_empty() {
            body["tools"] = Json::Array(tools);
        }
        body
    }

    async fn post_stream(&self, params: &CallModelParams) -> Result<reqwest::Response, NanocodeError> {
        let url = format!("{}/chat/completions", self.base_url);
        match self.http.post(url).json(&Self::build_body(params)).send().await {
            Ok(resp) if resp.status().is_success() => Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let body = resp.text().await.unwrap_or_default();
                // OpenAI error body: {"error":{"message":...}} — surface the
                // inner message for classification.
                let display = extract_error_message(&body).unwrap_or(body);
                Err(classify_error(Some(status), retry_after.as_deref(), &display))
            }
            Err(e) => Err(classify_error(None, None, &e.to_string())),
        }
    }
}

fn extract_error_message(body: &str) -> Option<String> {
    let parsed: Json = serde_json::from_str(body).ok()?;
    parsed
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|m| {
            let code = e_code(&parsed);
            if code.is_empty() {
                m.to_string()
            } else {
                format!("{m} (code {code})")
            }
        })
}

fn e_code(parsed: &Json) -> String {
    parsed
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| if c.is_string() { c.as_str().map(String::from) } else { c.as_u64().map(|n| n.to_string()) })
        .unwrap_or_default()
}

#[async_trait]
impl ModelCaller for OpenAIClient {
    fn call_model(&self, params: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
        use futures::StreamExt;
        let client = self.clone();
        Box::pin(async_stream::stream! {
            let cancel: CancellationToken = params.cancel.clone();

            let mut retry_opts = RetryOptions::defaults();
            retry_opts.on_retry = Some(Box::new(|err, attempt, delay| {
                eprintln!(
                    "\x1b[33m[retry {attempt}] {err}: {} (waiting {}s)\x1b[0m",
                    match err {
                        NanocodeError::RateLimit { message, .. }
                        | NanocodeError::Overloaded { message }
                        | NanocodeError::Network { message }
                        | NanocodeError::Other { message } => message.clone(),
                        _ => String::new(),
                    },
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

            // SSE decode: `data: {...}` lines, `data: [DONE]` terminates.
            let mut acc = OpenAiAccumulator::new();
            let mut bytes = response.bytes_stream();
            let mut line_buf: Vec<u8> = Vec::new();
            let mut cancelled = false;

            'stream: while let Some(chunk) = tokio::select! {
                biased;
                _ = cancel.cancelled() => { cancelled = true; None }
                c = bytes.next() => c,
            } {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield AgentEvent::Error { error: classify_error(None, None, &e.to_string()) };
                        return;
                    }
                };
                line_buf.extend_from_slice(&chunk);

                while let Some(pos) = line_buf.iter().position(|&c| c == b'\n') {
                    let raw: Vec<u8> = line_buf.drain(..=pos).collect();
                    let mut line = String::from_utf8_lossy(&raw[..raw.len() - 1]).to_string();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
                        continue; // event:/comments/SSE meta lines
                    };
                    if data == "[DONE]" {
                        break 'stream;
                    }
                    let (events, terminal) = handle_chunk(&mut acc, data);
                    for ev in events {
                        let is_error = matches!(&ev, AgentEvent::Error { .. });
                        yield ev;
                        if is_error || terminal {
                            return;
                        }
                    }
                }
            }

            if cancelled {
                return;
            }

            for ev in assemble(&mut acc) {
                yield ev;
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests — wiremock fixtures for the full conversion + decode pipeline
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    // --- outbound conversion ---

    #[test]
    fn converts_system_and_plain_messages() {
        let system = vec![crate::core::types::SystemPromptBlock::text("be terse")];
        let messages = vec![Message::user_text("hi")];
        let out = convert_messages(&system, &messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "be terse");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hi");
    }

    #[test]
    fn converts_tool_use_and_results_roundtrip() {
        let messages = vec![
            Message::user_text("read it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text { text: "checking".into() },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "a.txt"}),
                    },
                ],
                id: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: ToolResultContent::Text("file body".into()),
                    is_error: Some(false),
                }],
                id: None,
            },
        ];
        let out = convert_messages(&[], &messages);
        assert_eq!(out.len(), 3); // user, assistant(text+tool_calls), tool
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "Read");
        assert!(
            out[1]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .contains("a.txt")
        );
        assert_eq!(out[1]["content"], "checking");
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_1");
        assert_eq!(out[2]["content"], "file body");
    }

    #[test]
    fn error_tool_results_prefixed() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c".into(),
                content: ToolResultContent::Text("boom".into()),
                is_error: Some(true),
            }],
            id: None,
        }];
        let out = convert_messages(&[], &messages);
        assert_eq!(out[0]["content"], "ERROR: boom");
    }

    #[test]
    fn thinking_blocks_dropped() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking { thinking: "internal".into(), signature: None },
                ContentBlock::Text { text: "answer".into() },
            ],
            id: None,
        }];
        let out = convert_messages(&[], &messages);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "answer");
        assert!(out[0].get("tool_calls").is_none());
    }

    #[test]
    fn assistant_with_only_tool_use_has_null_content() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            id: None,
        }];
        let out = convert_messages(&[], &messages);
        assert!(out[0]["content"].is_null());
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "Bash");
    }

    // --- inbound decode ---

    fn chunk(content: Option<&str>, finish: Option<&str>) -> String {
        let mut delta = serde_json::json!({});
        if let Some(c) = content {
            delta["content"] = Json::String(c.to_string());
        }
        let mut obj = serde_json::json!({"choices": [{"delta": delta}]});
        if let Some(f) = finish {
            obj["choices"][0]["finish_reason"] = Json::String(f.to_string());
        }
        obj.to_string()
    }

    #[test]
    fn decode_text_stream_and_assemble() {
        let mut acc = OpenAiAccumulator::new();
        let (ev1, _) = handle_chunk(&mut acc, &chunk(Some("Hello "), None));
        let (ev2, _) = handle_chunk(&mut acc, &chunk(Some("world"), None));
        let (_, _) = handle_chunk(&mut acc, &chunk(None, Some("stop")));

        assert!(matches!(&ev1[0], AgentEvent::AssistantText { text } if text == "Hello "));
        assert!(matches!(&ev2[0], AgentEvent::AssistantText { text } if text == "world"));

        let final_events = assemble(&mut acc);
        let msg = final_events.iter().find_map(|e| match e {
            AgentEvent::AssistantMessage { message } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(msg.content.len(), 1);
        assert_eq!(msg.content[0].as_text(), Some("Hello world"));
        let stop = final_events.iter().find_map(|e| match e {
            AgentEvent::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(stop, "end_turn");
    }

    #[test]
    fn decode_tool_call_fragments_aggregate() {
        let mut acc = OpenAiAccumulator::new();
        // First fragment carries id + name start
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"Bash","arguments":""}}]}}]}"#,
        );
        // Argument fragments stream in
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]}}]}"#,
        );
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}"#,
        );
        handle_chunk(&mut acc, &chunk(None, Some("tool_calls")));

        let final_events = assemble(&mut acc);
        let tu = final_events.iter().find_map(|e| match e {
            AgentEvent::ToolUse { tool_use } => Some(tool_use.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(tu.id, "call_9");
        assert_eq!(tu.name, "Bash");
        assert_eq!(tu.input, serde_json::json!({"command": "ls"}));

        let stop = final_events.iter().find_map(|e| match e {
            AgentEvent::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(stop, "tool_use");
    }

    #[test]
    fn multiple_parallel_tool_calls_by_index() {
        let mut acc = OpenAiAccumulator::new();
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"Read","arguments":"{}"}}]}}]}"#,
        );
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"Glob","arguments":"{}"}}]}}]}"#,
        );
        let events = assemble(&mut acc);
        let tool_uses: Vec<&ToolUseBlock> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolUse { tool_use } => Some(tool_use),
                _ => None,
            })
            .collect();
        assert_eq!(tool_uses.len(), 2);
        assert_eq!(tool_uses[0].id, "a");
        assert_eq!(tool_uses[1].id, "b");
    }

    #[test]
    fn reasoning_content_maps_to_thinking() {
        let mut acc = OpenAiAccumulator::new();
        let (events, _) = handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"reasoning_content":"pondering"}}]}"#,
        );
        assert!(matches!(&events[0], AgentEvent::Thinking { text } if text == "pondering"));
    }

    #[test]
    fn usage_chunk_parsed() {
        let mut acc = OpenAiAccumulator::new();
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":120,"completion_tokens":34,"prompt_tokens_details":{"cached_tokens":50}}}"#,
        );
        let events = assemble(&mut acc);
        let usage = events.iter().find_map(|e| match e {
            AgentEvent::Usage { usage } => Some(*usage),
            _ => None,
        })
        .unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 34);
        assert_eq!(usage.cache_read_tokens, 50);
    }

    #[test]
    fn bad_arguments_fall_back_to_empty_object() {
        let mut acc = OpenAiAccumulator::new();
        handle_chunk(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"Y","arguments":"not-json{"}}]}}]}"#,
        );
        let events = assemble(&mut acc);
        let tu = events.iter().find_map(|e| match e {
            AgentEvent::ToolUse { tool_use } => Some(tool_use.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(tu.input, serde_json::json!({}));
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("content_filter"), "end_turn");
    }

    #[test]
    fn error_message_extraction() {
        let body = r#"{"error":{"code":"1113","message":"余额不足或无可用资源包"}}"#;
        assert_eq!(
            extract_error_message(body).unwrap(),
            "余额不足或无可用资源包 (code 1113)"
        );
        assert!(extract_error_message("not json").is_none());
    }

    // --- full HTTP round-trip via wiremock ---

    async fn mock_server(body: String, status: u16) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(status)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        server
    }

    fn params() -> CallModelParams {
        CallModelParams {
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            model_config: crate::core::api::get_model_config("gpt-4o-mini"),
            system_prompt_blocks: vec![],
            enable_thinking: false,
            thinking_budget: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn http_stream_end_to_end() {
        let body = [
            "data: ".to_owned() + r#"{"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}"#,
            String::new(),
            "data: ".to_owned() + r#"{"choices":[{"delta":{"content":" there"},"finish_reason":null}]}"#,
            String::new(),
            "data: ".to_owned() + r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
            String::new(),
            "data: [DONE]".to_owned(),
            String::new(),
        ]
        .join("\n");
        let server = mock_server(body, 200).await;
        let client = OpenAIClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;

        assert!(events.iter().any(|e| matches!(e, AgentEvent::AssistantText { text } if text == "Hi")));
        let usage = events.iter().find_map(|e| match e {
            AgentEvent::Usage { usage } => Some(*usage),
            _ => None,
        })
        .unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnComplete { stop_reason } if stop_reason == "end_turn")));
    }

    #[tokio::test]
    async fn http_error_classified_from_body() {
        let server = mock_server(
            r#"{"error":{"code":"1113","message":"insufficient balance"}}"#.into(),
            402,
        )
        .await;
        let client = OpenAIClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Error { .. }));
    }

    #[tokio::test]
    async fn http_auth_error_not_retried() {
        let server = mock_server(r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#.into(), 401).await;
        let client = OpenAIClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        assert!(matches!(
            &events[0],
            AgentEvent::Error { error: NanocodeError::Authentication { status: 401, .. } }
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn http_retries_rate_limit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(429).set_body_string("slow"))
            .up_to_n_times(1)
            .expect(1..)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n"),
            )
            .mount(&server)
            .await;
        let client = OpenAIClient::new("k", Some(&server.uri()));
        let events: Vec<AgentEvent> = client.call_model(params()).collect().await;
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnComplete { .. })));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn request_body_shape() {
        let server = mock_server("data: [DONE]\n".into(), 200).await;
        let client = OpenAIClient::new("secret-key", Some(&server.uri()));
        let mut p = params();
        p.tools = vec![ToolSpec {
            name: "Bash".into(),
            description: "run".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        p.system_prompt_blocks = vec![crate::core::types::SystemPromptBlock::text("sys prompt")];
        let _: Vec<AgentEvent> = client.call_model(p).collect().await;

        let req = &server.received_requests().await.unwrap()[0];
        let body: Json = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys prompt");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "Bash");
        assert_eq!(
            req.headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer secret-key"
        );
    }
}
