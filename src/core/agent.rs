//! Agent Loop — Rust port of `src/core/agent.ts` (THE critical file).
//!
//! while(true): auto-compact check → streaming model call → collect tool_use
//! → (none → done) → permissions → execute tools → accumulate results →
//! maxTurns check. Messages live in a shared `Arc<Mutex<Vec<Message>>>`
//! (TS mutated the array in place); the caller holds the same Arc.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::Value as Json;

use crate::core::api::{CallModelParams, ModelCaller, ToolSpec};
use crate::core::errors::NanocodeError;
use crate::core::types::{
    AgentEvent, ContentBlock, Message, ModelConfig, PermissionDecision, PermissionMode,
    Role, SystemPromptBlock, ToolResult, ToolResultContent, ToolUseBlock,
};
use crate::context::token_counting::estimate_message_tokens_loop;
use crate::tools::Tool;

// ---------------------------------------------------------------------------
// Constants (agent.ts)
// ---------------------------------------------------------------------------

const AUTOCOMPACT_BUFFER_TOKENS: i64 = 13_000;
const MAX_PTL_RETRIES: u32 = 3;
pub(crate) const DEFAULT_MAX_TURNS: u32 = 200;
const MICRO_COMPACT_THRESHOLD_CHARS: usize = 50_000;
const MICRO_KEEP_RECENT: usize = 6; // ~3 turns
const MICRO_TRUNCATE_TO: usize = 5000;

// ---------------------------------------------------------------------------
// Compactor seam (wired to context/compaction.rs in Phase 5)
// ---------------------------------------------------------------------------

pub struct CompactOutcome {
    pub compacted: Vec<Message>,
    pub old_tokens: i64,
    pub new_tokens: i64,
}

#[async_trait]
pub trait Compactor: Send + Sync {
    async fn compact(&self, messages: Vec<Message>) -> Result<CompactOutcome, String>;
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

pub struct QueryParams {
    /// Shared, mutated-in-place message history (TS: params.messages array).
    pub messages: Arc<Mutex<Vec<Message>>>,
    pub tools: Vec<Tool>,
    pub model_caller: Arc<dyn ModelCaller>,
    pub model_config: ModelConfig,
    pub system_prompt_blocks: Vec<SystemPromptBlock>,
    pub max_turns: u32,
    pub permission_mode: PermissionMode,
    pub enable_thinking: bool,
    pub thinking_budget: Option<u64>,
    pub tool_context: crate::core::types::ToolContext,
    pub compactor: Option<Arc<dyn Compactor>>,
    /// Permission rules (settings.json + session) — wired per D1.
    pub rules: crate::permissions::RuleSet,
}

// ---------------------------------------------------------------------------
// Auto-compact / micro-compact / PTL helpers (agent.ts)
// ---------------------------------------------------------------------------

pub fn should_auto_compact(messages: &[Message], context_window: u64, max_output_tokens: u64) -> bool {
    let threshold = context_window as i64 - max_output_tokens as i64 - AUTOCOMPACT_BUFFER_TOKENS;
    if threshold <= 0 {
        return false;
    }
    estimate_message_tokens_loop(messages) > threshold
}

/// Truncate oversized tool_results older than the last ~3 turns, in place.
pub fn micro_compact_messages(messages: &mut [Message]) {
    let len = messages.len();
    if len <= MICRO_KEEP_RECENT {
        return;
    }
    let recent_start = len - MICRO_KEEP_RECENT;
    for msg in &mut messages[..recent_start] {
        if msg.role != Role::User {
            continue;
        }
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                let text = match content {
                    ToolResultContent::Text(s) => s.clone(),
                    ToolResultContent::Blocks(b) => serde_json::to_string(b).unwrap_or_default(),
                };
                if text.len() > MICRO_COMPACT_THRESHOLD_CHARS {
                    let original = text.chars().count();
                    let truncated: String = text.chars().take(MICRO_TRUNCATE_TO).collect();
                    *content = ToolResultContent::Text(format!(
                        "{truncated}\n\n[Content truncated: was {original} chars. Re-read the file if needed.]"
                    ));
                }
            }
        }
    }
}

/// PTL recovery: drop the two oldest messages (TS truncateForPTL; the
/// "keep first if system" comment in TS was never implemented there either).
pub fn truncate_for_ptl(messages: &mut Vec<Message>) {
    if messages.len() > 2 {
        messages.drain(0..2);
    }
}

// ---------------------------------------------------------------------------
// Permission checking (agent.ts checkToolPermission — path A, the live path)
// ---------------------------------------------------------------------------

const ACCEPT_EDITS_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit"];

fn describe_tool_use(tool_use: &ToolUseBlock) -> String {
    let input = &tool_use.input;
    match tool_use.name.as_str() {
        "Bash" => format!("Bash: {}", str_field(input, &["command"]).unwrap_or("(no command)".into())),
        "Edit" => format!("Edit: {}", str_field(input, &["file_path", "path", "filePath"]).unwrap_or("(unknown file)".into())),
        "Write" => format!("Write: {}", str_field(input, &["file_path", "path"]).unwrap_or("(unknown file)".into())),
        "Read" => format!("Read: {}", str_field(input, &["file_path", "path"]).unwrap_or("(unknown file)".into())),
        other => {
            let json = serde_json::to_string(input).unwrap_or_default();
            let short: String = json.chars().take(200).collect();
            format!("{other}: {short}")
        }
    }
}

fn str_field(input: &Json, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
}

async fn check_tool_permission(
    tool_use: &ToolUseBlock,
    tools: &[Tool],
    params: &QueryParams,
) -> PermissionDecision {
    let Some(tool) = tools.iter().find(|t| t.name() == tool_use.name) else {
        // Unknown tools handled by executor
        return PermissionDecision::allow();
    };

    // Rules engine (deny rules → read-only → bypass → allow rules), per D1.
    let is_read_only = tool.is_read_only(&tool_use.input);
    if let Some(behavior) = params.rules.check(
        &tool_use.name,
        &tool_use.input,
        is_read_only,
        params.permission_mode,
        &params.tool_context.cwd,
    ) {
        return match behavior {
            crate::core::types::PermissionBehavior::Allow => PermissionDecision::allow(),
            crate::core::types::PermissionBehavior::Deny => PermissionDecision::deny(format!(
                "Denied by rule for tool \"{}\"",
                tool_use.name
            )),
            crate::core::types::PermissionBehavior::Ask => PermissionDecision::allow(),
        };
    }

    // Path A fallback (agent.ts): plan denies writes, acceptEdits auto-allows
    // the file tools, everything else goes to the interactive gate.
    if params.permission_mode == PermissionMode::Plan {
        return PermissionDecision::deny(format!(
            "Tool {} is not allowed in plan mode (read-only).",
            tool_use.name
        ));
    }

    if params.permission_mode == PermissionMode::AcceptEdits
        && ACCEPT_EDITS_TOOLS.contains(&tool_use.name.as_str())
    {
        return PermissionDecision::allow();
    }

    params
        .tool_context
        .permission_gate
        .decide(&tool_use.name, &tool_use.input, &describe_tool_use(tool_use))
        .await
}

// ---------------------------------------------------------------------------
// Message builders (agent.ts)
// ---------------------------------------------------------------------------

fn build_tool_result_message(results: &[(String, ToolResult)]) -> Message {
    let content = results
        .iter()
        .map(|(tool_use_id, result)| ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: ToolResultContent::Text(result.result.clone()),
            is_error: Some(result.is_error()),
        })
        .collect();
    Message { role: Role::User, content, id: Some(uuid::Uuid::new_v4().to_string()) }
}

// ---------------------------------------------------------------------------
// Main agent loop
// ---------------------------------------------------------------------------

pub struct AgentLoop {
    pub params: Arc<QueryParams>,
}

/// Bridges the Agent tool to `run_sub_agent` (completes the TS dead wiring).
struct ParamsRunner {
    params: std::sync::Weak<QueryParams>,
}

#[async_trait]
impl crate::core::types::SubAgentRunner for ParamsRunner {
    async fn run(
        &self,
        prompt: String,
        tools_allow: Option<Vec<String>>,
        tools_disallow: Option<Vec<String>>,
        max_turns: u32,
    ) -> String {
        let Some(params) = self.params.upgrade() else {
            return "(No response from sub-agent)".to_string();
        };
        run_sub_agent_with(
            &params,
            &prompt,
            SubAgentOptions {
                tools: tools_allow,
                disallowed_tools: tools_disallow,
                max_turns,
                model: None,
            },
        )
        .await
    }
}

impl AgentLoop {
    /// Run the loop, yielding events. The final message history is the shared
    /// `params.messages` array when the stream completes.
    pub fn run(self) -> impl Stream<Item = AgentEvent> {
        async_stream::stream! {
            let params = self.params;

            // Inject the sub-agent runner so the Agent tool can delegate.
            {
                let runner: Arc<dyn crate::core::types::SubAgentRunner> =
                    Arc::new(ParamsRunner { params: Arc::downgrade(&params) });
                *params.tool_context.sub_agent.lock().unwrap() = Some(runner);
            }
            let mut turn_count: u32 = 0;
            let mut ptl_retries: u32 = 0;

            loop {
                // Check abort
                if params.tool_context.cancel.is_cancelled() {
                    yield AgentEvent::Error { error: NanocodeError::Abort };
                    return;
                }

                // 1. Auto-compact check
                if let Some(compactor) = &params.compactor {
                    let snapshot: Vec<Message> = params.messages.lock().unwrap().clone();
                    if should_auto_compact(
                        &snapshot,
                        params.model_config.context_window,
                        params.model_config.max_output_tokens,
                    ) {
                        match compactor.compact(snapshot).await {
                            Ok(outcome) => {
                                let with_attachments =
                                    crate::context::compaction::with_post_compact_attachments(
                                        outcome.compacted,
                                        &params.tool_context.file_state,
                                    )
                                    .await;
                                *params.messages.lock().unwrap() = with_attachments;
                                yield AgentEvent::Compact {
                                    old_tokens: outcome.old_tokens,
                                    new_tokens: outcome.new_tokens,
                                };
                            }
                            Err(msg) => {
                                // Compact failure is non-fatal
                                yield AgentEvent::Error { error: NanocodeError::Other { message: format!("Auto-compact failed: {msg}") } };
                            }
                        }
                    }
                }

                // Micro-compact old tool results (in place)
                micro_compact_messages(&mut params.messages.lock().unwrap());

                // 2. Call the model (streaming)

                let mut tool_use_blocks: Vec<ToolUseBlock> = Vec::new();
                let mut assistant_content: Vec<ContentBlock> = Vec::new();
                let mut stop_reason = "end_turn".to_string();
                let mut stream_errored = false;
                let mut ptl_hit = false;

                {
                    let call_params = CallModelParams {
                        messages: params.messages.lock().unwrap().clone(),
                        tools: params
                            .tools
                            .iter()
                            .map(|t| ToolSpec {
                                name: t.name().to_string(),
                                description: t.description(None),
                                input_schema: t.input_schema(),
                            })
                            .collect(),
                        model_config: params.model_config.clone(),
                        system_prompt_blocks: params.system_prompt_blocks.clone(),
                        enable_thinking: params.enable_thinking,
                        thinking_budget: params.thinking_budget,
                        cancel: params.tool_context.cancel.clone(),
                    };

                    let mut events = params.model_caller.call_model(call_params);
                    while let Some(event) = events.next().await {
                        if params.tool_context.cancel.is_cancelled() {
                            stream_errored = true;
                            break;
                        }
                        match &event {
                            AgentEvent::ToolUse { tool_use } => {
                                tool_use_blocks.push(tool_use.clone());
                            }
                            AgentEvent::AssistantMessage { message } => {
                                assistant_content = message.content.clone();
                            }
                            AgentEvent::TurnComplete { stop_reason: sr } => {
                                stop_reason = sr.clone();
                            }
                            AgentEvent::Error { error } => {
                                if matches!(error, NanocodeError::PromptTooLong { .. }) {
                                    ptl_hit = true;
                                }
                                stream_errored = true;
                            }
                            _ => {}
                        }
                        if let AgentEvent::Error { error } = &event {
                            if !ptl_hit {
                                yield AgentEvent::Error { error: error.clone() };
                                return; // other errors end the loop (TS: yield + return)
                            }
                        }
                        yield event;
                    }
                }

                if ptl_hit {
                    ptl_retries += 1;
                    if ptl_retries >= MAX_PTL_RETRIES {
                        yield AgentEvent::Error { error: NanocodeError::Other { message: format!("Prompt too long after {MAX_PTL_RETRIES} truncation attempts. Try /compact.") } };
                        return;
                    }
                    truncate_for_ptl(&mut params.messages.lock().unwrap());
                    yield AgentEvent::Error { error: NanocodeError::Other { message: format!("Prompt too long — truncating old messages (attempt {ptl_retries}/{MAX_PTL_RETRIES})") } };
                    continue; // retry the loop
                }
                if stream_errored && assistant_content.is_empty() && tool_use_blocks.is_empty() {
                    // Cancelled mid-stream or terminal error already yielded
                    return;
                }

                ptl_retries = 0;

                // 3. Accumulate assistant message
                if !assistant_content.is_empty() {
                    params
                        .messages
                        .lock()
                        .unwrap()
                        .push(Message { role: Role::Assistant, content: assistant_content, id: Some(uuid::Uuid::new_v4().to_string()) });
                }

                // 4. No tool use → done
                if tool_use_blocks.is_empty() {
                    return;
                }

                // 5. Permission checks (parallel, TS Promise.all)
                let decisions: Vec<PermissionDecision> = {
                    let checks = tool_use_blocks
                        .iter()
                        .map(|tu| check_tool_permission(tu, &params.tools, &params));
                    futures::future::join_all(checks).await
                };

                let mut tool_results: Vec<(String, ToolResult)> = Vec::new();
                let mut allowed: Vec<ToolUseBlock> = Vec::new();
                for (tu, decision) in tool_use_blocks.iter().zip(decisions) {
                    if decision.behavior == crate::core::types::PermissionBehavior::Allow {
                        allowed.push(tu.clone());
                    } else {
                        let message = decision.message.unwrap_or_else(|| {
                            format!("Permission denied for {}.", tu.name)
                        });
                        tool_results.push((tu.id.clone(), ToolResult::err(message.clone())));
                        yield AgentEvent::ToolResult {
                            tool_use_id: tu.id.clone(),
                            tool_name: tu.name.clone(),
                            result: message,
                            is_error: true,
                        };
                    }
                }

                // 6. Execute allowed tools
                if !allowed.is_empty() {
                    let mut exec = Box::pin(crate::tools::streaming_executor::execute_tools(
                        allowed,
                        params.tools.clone(),
                        &params.tool_context,
                    ));
                    while let Some(event) = exec.as_mut().next().await {
                        if let AgentEvent::ToolResult { tool_use_id, result, is_error, .. } = &event {
                            tool_results.push((
                                tool_use_id.clone(),
                                ToolResult { result: result.clone(), is_error: Some(*is_error) },
                            ));
                        }
                        yield event;
                    }
                }

                // 7. Accumulate results as one user message, in tool_use order
                let ordered: Vec<(String, ToolResult)> = tool_use_blocks
                    .iter()
                    .map(|tu| {
                        tool_results
                            .iter()
                            .find(|(id, _)| *id == tu.id)
                            .cloned()
                            .unwrap_or_else(|| {
                                (
                                    tu.id.clone(),
                                    ToolResult::err("Tool execution failed (no result)"),
                                )
                            })
                    })
                    .collect();

                params.messages.lock().unwrap().push(build_tool_result_message(&ordered));

                // 8. Max turns check
                turn_count += 1;
                if turn_count >= params.max_turns {
                    yield AgentEvent::MaxTurnsReached { max_turns: params.max_turns };
                    return;
                }
                let _ = stop_reason;
            }
        }
    }
}

impl QueryParams {
    #[cfg(test)]
    pub(crate) fn clone_fields(&self) -> QueryParams {
        QueryParams {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            model_caller: self.model_caller.clone(),
            model_config: self.model_config.clone(),
            system_prompt_blocks: self.system_prompt_blocks.clone(),
            max_turns: self.max_turns,
            permission_mode: self.permission_mode,
            enable_thinking: self.enable_thinking,
            thinking_budget: self.thinking_budget,
            tool_context: self.tool_context.clone(),
            compactor: self.compactor.clone(),
            rules: self.rules.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-agent helper (agent.ts runSubAgent)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct SubAgentOptions {
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub max_turns: u32,
    pub model: Option<String>,
}

impl Default for SubAgentOptions {
    fn default() -> Self {
        SubAgentOptions { tools: None, disallowed_tools: None, max_turns: 200, model: None }
    }
}

/// Public helper matching the TS `runSubAgent` signature.
pub async fn run_sub_agent(prompt: &str, params: &Arc<QueryParams>, options: SubAgentOptions) -> String {
    run_sub_agent_with(params, prompt, options).await
}

/// Run a sub-agent with isolated context (fresh messages, cloned file-state
/// cache, shared file history). Returns the final assistant text.
async fn run_sub_agent_with(
    params: &Arc<QueryParams>,
    prompt: &str,
    options: SubAgentOptions,
) -> String {
    let mut tools = params.tools.clone();
    if let Some(allow) = &options.tools {
        tools.retain(|t| allow.contains(&t.name().to_string()));
    }
    if let Some(disallow) = &options.disallowed_tools {
        tools.retain(|t| !disallow.contains(&t.name().to_string()));
    }

    // Isolated file-state cache (merged back after the run, newer wins)
    let isolated_state = {
        let guard = params.tool_context.file_state.lock().unwrap();
        guard.deep_clone()
    };

    let isolated_state_arc: Arc<Mutex<crate::core::types::FileStateCache>> =
        Arc::new(Mutex::new(isolated_state));

    let sub_params = QueryParams {
        messages: Arc::new(Mutex::new(vec![Message::user_text(prompt)])),
        tools,
        model_caller: params.model_caller.clone(),
        model_config: params.model_config.clone(),
        system_prompt_blocks: params.system_prompt_blocks.clone(),
        max_turns: options.max_turns,
        permission_mode: params.permission_mode,
        enable_thinking: params.enable_thinking,
        thinking_budget: params.thinking_budget,
        tool_context: crate::core::types::ToolContext {
            file_state: isolated_state_arc.clone(),
            file_history: params.tool_context.file_history.clone(),
            modified_files: params.tool_context.modified_files.clone(),
            ..clone_context_fields(params)
        },
        compactor: None, // sub-agents never auto-compact (TS behavior)
        rules: params.rules.clone(),
    };

    let mut last_assistant_text = String::new();
    let stream = AgentLoop { params: Arc::new(sub_params) }.run();
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        if let AgentEvent::AssistantMessage { message } = event {
            let text = message.assistant_text();
            if !text.is_empty() {
                last_assistant_text = text;
            }
        }
    }

    // Merge the isolated file-state cache back (newer timestamps win).
    {
        let merged = isolated_state_arc.lock().unwrap().deep_clone();
        params.tool_context.file_state.lock().unwrap().merge(&merged);
    }
    last_assistant_text = if last_assistant_text.is_empty() {
        "(No response from sub-agent)".to_string()
    } else {
        last_assistant_text
    };
    last_assistant_text
}

fn clone_context_fields(params: &QueryParams) -> crate::core::types::ToolContext {
    // ToolContext is Clone; use it to keep cwd/session/gate/mode shared.
    params.tool_context.clone()
}

// ---------------------------------------------------------------------------
// Tests — mock model streams driving the full loop
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{
        FileHistoryState, FileStateCache, PermissionGate,
    };
    use std::sync::RwLock;

    struct AllowGate;
    #[async_trait]
    impl PermissionGate for AllowGate {
        async fn decide(&self, _: &str, _: &Json, _: &str) -> PermissionDecision {
            PermissionDecision::allow()
        }
    }

    struct DenyGate;
    #[async_trait]
    impl PermissionGate for DenyGate {
        async fn decide(&self, _: &str, _: &Json, _: &str) -> PermissionDecision {
            PermissionDecision::deny("User denied")
        }
    }

    /// Mock model that plays scripted event sequences, one per call.
    struct MockCaller {
        scripts: Mutex<Vec<Vec<AgentEvent>>>,
        calls: AtomicCount,
    }

    type AtomicCount = std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    #[async_trait]
    impl ModelCaller for MockCaller {
        fn call_model(&self, _params: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
            let mut scripts = self.scripts.lock().unwrap();
            let script = if scripts.is_empty() {
                vec![AgentEvent::AssistantMessage { message: Message { role: Role::Assistant, content: vec![ContentBlock::text("done")], id: None } },
                     AgentEvent::TurnComplete { stop_reason: "end_turn".into() }]
            } else {
                scripts.remove(0)
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            futures::stream::iter(script).boxed()
        }
    }

    fn text_msg_events(text: &str) -> Vec<AgentEvent> {
        vec![
            AgentEvent::AssistantText { text: text.to_string() },
            AgentEvent::AssistantMessage {
                message: Message { role: Role::Assistant, content: vec![ContentBlock::text(text)], id: None },
            },
            AgentEvent::TurnComplete { stop_reason: "end_turn".into() },
        ]
    }

    fn tool_use_events(id: &str, name: &str, input: Json) -> Vec<AgentEvent> {
        let block = ToolUseBlock { id: id.into(), name: name.into(), input };
        vec![
            AgentEvent::ToolUse { tool_use: block.clone() },
            AgentEvent::AssistantMessage {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse { id: block.id, name: block.name, input: block.input }],
                    id: None,
                },
            },
            AgentEvent::TurnComplete { stop_reason: "tool_use".into() },
        ]
    }

    struct EchoTool;
    #[async_trait]
    impl crate::tools::ToolDef for EchoTool {
        fn name(&self) -> &str {
            "Echo"
        }
        fn description(&self, _: Option<&Json>) -> String {
            "echoes".into()
        }
        fn input_schema(&self) -> Json {
            serde_json::json!({"type": "object"})
        }
        async fn call(&self, input: Json, _ctx: &crate::core::types::ToolContext) -> ToolResult {
            ToolResult::ok(format!("echo: {input}"))
        }
    }

    fn base_params(caller: Arc<dyn ModelCaller>, gate: Arc<dyn PermissionGate>) -> Arc<QueryParams> {
        let ctx = crate::core::types::ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(FileStateCache::new())),
            file_history: Arc::new(Mutex::new(FileHistoryState::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Default)),
            permission_gate: gate,
            sub_agent: Arc::new(Mutex::new(None)),
        };
        Arc::new(QueryParams {
            messages: Arc::new(Mutex::new(vec![Message::user_text("task")])),
            tools: vec![Arc::new(EchoTool)],
            model_caller: caller,
            model_config: crate::core::api::get_model_config("sonnet"),
            system_prompt_blocks: vec![],
            max_turns: 10,
            permission_mode: PermissionMode::Default,
            enable_thinking: false,
            thinking_budget: None,
            tool_context: ctx,
            compactor: None,
            rules: Default::default(),
        })
    }

    async fn collect(params: Arc<QueryParams>) -> Vec<AgentEvent> {
        AgentLoop { params }.run().collect().await
    }

    #[tokio::test]
    async fn ends_when_no_tool_use() {
        let caller = Arc::new(MockCaller { scripts: Mutex::new(vec![text_msg_events("answer")]), calls: AtomicCount::new(0) });
        let params = base_params(caller, Arc::new(AllowGate));
        let events = collect(params.clone()).await;

        assert!(events.iter().any(|e| matches!(e, AgentEvent::AssistantText { text } if text == "answer")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnComplete { .. })));
        // Final history: initial user + assistant
        let msgs = params.messages.lock().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn tool_call_round_trip_and_result_order() {
        let caller = Arc::new(MockCaller {
            scripts: Mutex::new(vec![
                tool_use_events("t1", "Echo", serde_json::json!({"n": 1})),
                text_msg_events("done"),
            ]),
            calls: AtomicCount::new(0),
        });
        let params = base_params(caller, Arc::new(AllowGate));
        let events = collect(params.clone()).await;

        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolStart { tool_name, .. } if tool_name == "Echo")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result, is_error, .. } if result.contains("echo:") && !*is_error)));

        let msgs = params.messages.lock().unwrap();
        // user, assistant(tool_use), user(tool_result), assistant(done)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].role, Role::User);
        match &msgs[2].content[0] {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                assert_eq!(tool_use_id, "t1");
                assert!(*is_error == Some(false));
                match content {
                    ToolResultContent::Text(t) => assert!(t.contains("echo: {\"n\":1}")),
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn denied_tool_gets_error_result() {
        let caller = Arc::new(MockCaller {
            scripts: Mutex::new(vec![
                tool_use_events("t1", "Echo", serde_json::json!({})),
                text_msg_events("ok"),
            ]),
            calls: AtomicCount::new(0),
        });
        let params = base_params(caller, Arc::new(DenyGate));
        let events = collect(params.clone()).await;

        let denied = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(denied.1);
        assert_eq!(denied.0, "User denied");

        // Denied result lands in history as isError tool_result
        let msgs = params.messages.lock().unwrap();
        match &msgs[2].content[0] {
            ContentBlock::ToolResult { is_error, .. } => assert_eq!(*is_error, Some(true)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_mode_denies_write_tools() {
        struct WriteTool;
        #[async_trait]
        impl crate::tools::ToolDef for WriteTool {
            fn name(&self) -> &str { "Write" }
            fn description(&self, _: Option<&Json>) -> String { "writes".into() }
            fn input_schema(&self) -> Json { serde_json::json!({}) }
            async fn call(&self, _: Json, _: &crate::core::types::ToolContext) -> ToolResult {
                ToolResult::ok("wrote")
            }
        }

        let caller = Arc::new(MockCaller {
            scripts: Mutex::new(vec![
                tool_use_events("t1", "Write", serde_json::json!({"file_path": "/x"})),
                text_msg_events("ok"),
            ]),
            calls: AtomicCount::new(0),
        });
        let params = Arc::new(QueryParams {
            tools: vec![Arc::new(WriteTool)],
            permission_mode: PermissionMode::Plan,
            ..base_params(caller, Arc::new(AllowGate)).clone_fields()
        });
        let events = collect(params).await;
        let denied = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(denied.1);
        assert!(denied.0.contains("not allowed in plan mode"));
    }

    #[tokio::test]
    async fn max_turns_stops_loop() {
        // Every call returns a tool_use → loop should stop at max_turns=3
        let _caller = Arc::new(MockCaller { scripts: Mutex::new(vec![]), calls: AtomicCount::new(0) });
        // MockCaller with empty scripts keeps returning a text message (done),
        // so instead build an infinite tool-use caller:
        struct LoopCaller;
        #[async_trait]
        impl ModelCaller for LoopCaller {
            fn call_model(&self, _: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
                futures::stream::iter(tool_use_events("t", "Echo", serde_json::json!({}))).boxed()
            }
        }
        let params = base_params(Arc::new(LoopCaller), Arc::new(AllowGate));
        let params = Arc::new(QueryParams {
            max_turns: 3,
            ..(*params).clone_fields()
        });
        let events = collect(params).await;
        assert!(events.iter().any(|e| matches!(e, AgentEvent::MaxTurnsReached { max_turns } if *max_turns == 3)));
    }

    #[tokio::test]
    async fn ptl_recovery_truncates_then_gives_up() {
        struct PtlCaller;
        #[async_trait]
        impl ModelCaller for PtlCaller {
            fn call_model(&self, _: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
                futures::stream::iter(vec![
                    AgentEvent::Error { error: NanocodeError::PromptTooLong { message: "prompt is too long".into(), token_count: None, max_tokens: None } },
                ]).boxed()
            }
        }
        let params = base_params(Arc::new(PtlCaller), Arc::new(AllowGate));
        // Start with 5 messages so truncation has something to remove
        {
            let mut msgs = params.messages.lock().unwrap();
            for i in 0..4 {
                msgs.push(Message::user_text(format!("m{i}")));
            }
        }
        let events = collect(params.clone()).await;
        let errors: Vec<String> = events.iter().filter_map(|e| match e {
            AgentEvent::Error { error } => Some(error.to_string()),
            _ => None,
        }).collect();
        assert!(errors.iter().any(|e| e.contains("truncating old messages (attempt 1/3)")));
        assert!(errors.iter().any(|e| e.contains("after 3 truncation attempts")));
        // History truncated: 5 initial - 2*3 attempts, clamped ≥ 0 — with 3
        // truncations of 2 messages each from 5, we hit the floor.
        assert!(params.messages.lock().unwrap().len() <= 2);
    }

    #[tokio::test]
    async fn other_model_errors_end_loop() {
        struct FailCaller;
        #[async_trait]
        impl ModelCaller for FailCaller {
            fn call_model(&self, _: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
                futures::stream::iter(vec![
                    AgentEvent::Error { error: NanocodeError::Authentication { status: 401, message: "bad key".into() } },
                ]).boxed()
            }
        }
        let params = base_params(Arc::new(FailCaller), Arc::new(AllowGate));
        let events = collect(params).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Error { error: NanocodeError::Authentication { .. } }));
    }

    #[tokio::test]
    async fn permission_rules_wired_into_loop() {
        struct WriteTool2;
        #[async_trait]
        impl crate::tools::ToolDef for WriteTool2 {
            fn name(&self) -> &str { "Write" }
            fn description(&self, _: Option<&Json>) -> String { "writes".into() }
            fn input_schema(&self) -> Json { serde_json::json!({}) }
            async fn call(&self, _: Json, _: &crate::core::types::ToolContext) -> ToolResult {
                ToolResult::ok("wrote")
            }
        }
        let caller = Arc::new(MockCaller {
            scripts: Mutex::new(vec![
                tool_use_events("t1", "Write", serde_json::json!({"file_path": "/secrets.txt"})),
                text_msg_events("ok"),
            ]),
            calls: AtomicCount::new(0),
        });
        let mut rules = crate::permissions::RuleSet::default();
        rules.session.push(crate::core::types::PermissionRule {
            tool: "Write".into(),
            content: Some("/secrets*".into()),
            behavior: crate::core::types::PermissionBehavior::Deny,
            source: crate::core::types::RuleSource::Session,
        });
        let params = Arc::new(QueryParams {
            tools: vec![Arc::new(WriteTool2)],
            rules,
            ..base_params(caller, Arc::new(AllowGate)).clone_fields()
        });
        let events = collect(params).await;
        let denied = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(denied.1);
        assert!(denied.0.contains("Denied by rule"));
    }

    #[tokio::test]
    async fn unknown_tool_passes_permission_and_errors_in_executor() {
        let caller = Arc::new(MockCaller {
            scripts: Mutex::new(vec![
                tool_use_events("t1", "Mystery", serde_json::json!({})),
                text_msg_events("ok"),
            ]),
            calls: AtomicCount::new(0),
        });
        let params = base_params(caller, Arc::new(AllowGate));
        let events = collect(params).await;
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(result.1);
        assert!(result.0.contains("Unknown tool: Mystery"));
    }

}

#[cfg(test)]
mod sub_agent_integration_tests {
    use super::*;
    use crate::core::types::{FileHistoryState, FileStateCache, PermissionGate};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::RwLock;

    struct AllowGate;
    #[async_trait]
    impl PermissionGate for AllowGate {
        async fn decide(&self, _: &str, _: &Json, _: &str) -> PermissionDecision {
            PermissionDecision::allow()
        }
    }

    /// Scripted caller: call 0 → Agent tool_use; call 1 → sub answer text;
    /// call 2 → final text.
    struct SeqCaller {
        call_index: AtomicU32,
    }

    fn tool_use_msg(id: &str, name: &str, input: Json) -> Vec<AgentEvent> {
        let block = ToolUseBlock { id: id.into(), name: name.into(), input };
        vec![
            AgentEvent::ToolUse { tool_use: block.clone() },
            AgentEvent::AssistantMessage {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse { id: block.id, name: block.name, input: block.input }],
                    id: None,
                },
            },
            AgentEvent::TurnComplete { stop_reason: "tool_use".into() },
        ]
    }

    fn text_events(text: &str) -> Vec<AgentEvent> {
        vec![
            AgentEvent::AssistantMessage {
                message: Message { role: Role::Assistant, content: vec![ContentBlock::text(text)], id: None },
            },
            AgentEvent::TurnComplete { stop_reason: "end_turn".into() },
        ]
    }

    #[async_trait]
    impl ModelCaller for SeqCaller {
        fn call_model(&self, _params: CallModelParams) -> futures::stream::BoxStream<'static, AgentEvent> {
            let n = self.call_index.fetch_add(1, Ordering::SeqCst);
            let script = match n {
                0 => tool_use_msg("t1", "Agent", serde_json::json!({"prompt": "sub task", "subagent_type": "Explore"})),
                1 => text_events("sub answer: 3 files"),
                _ => text_events("final done"),
            };
            futures::stream::iter(script).boxed()
        }
    }

    #[tokio::test]
    async fn agent_tool_runs_nested_sub_loop() {
        struct AgentTool;
        #[async_trait]
        impl crate::tools::ToolDef for AgentTool {
            fn name(&self) -> &str { "Agent" }
            fn description(&self, _: Option<&Json>) -> String { String::new() }
            fn input_schema(&self) -> Json { serde_json::json!({}) }
            async fn call(&self, input: Json, ctx: &crate::core::types::ToolContext) -> ToolResult {
                crate::tools::agent::AgentTool.call(input, ctx).await
            }
        }

        let ctx = crate::core::types::ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(FileStateCache::new())),
            file_history: Arc::new(Mutex::new(FileHistoryState::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Default)),
            permission_gate: Arc::new(AllowGate),
            sub_agent: Arc::new(Mutex::new(None)),
        };
        let params = Arc::new(QueryParams {
            messages: Arc::new(Mutex::new(vec![Message::user_text("main task")])),
            tools: vec![Arc::new(AgentTool)],
            model_caller: Arc::new(SeqCaller { call_index: AtomicU32::new(0) }),
            model_config: crate::core::api::get_model_config("sonnet"),
            system_prompt_blocks: vec![],
            max_turns: 10,
            permission_mode: PermissionMode::Default,
            enable_thinking: false,
            thinking_budget: None,
            tool_context: ctx,
            compactor: None,
            rules: Default::default(),
        });

        let events: Vec<AgentEvent> = AgentLoop { params: params.clone() }.run().collect().await;

        // The Agent tool result carries the sub answer
        let agent_result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, .. } => Some(result.clone()),
            _ => None,
        }).expect("tool result");
        assert!(agent_result.contains("[Sub-agent: Explore]"));
        assert!(agent_result.contains("sub answer: 3 files"));

        // Final history: user, assistant(agent_use), user(agent_result), assistant(final)
        let msgs = params.messages.lock().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3].assistant_text(), "final done");
    }
}
