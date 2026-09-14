//! Agent tool — Rust port of `src/tools/agent.ts`.
//!
//! Delegates to a sub-agent with isolated context. Three modes:
//! - Explore: read-only tools (Read/Glob/Grep/Bash), 30 turns
//! - Plan: all tools except Agent, 50 turns
//! - default: all tools, 50 turns
//!
//! Note: in the TS original this tool always errored at runtime because
//! `setAgentQueryParams` had no caller (see ARCHITECTURE.md §4.14); the Rust
//! port completes the wiring via `ToolContext.sub_agent` (PORTING_PLAN D1).

use async_trait::async_trait;
use serde_json::Value as Json;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];
const MAX_SUB_AGENT_TURNS: u32 = 50;
const DEFAULT_SUB_AGENT_TURNS: u32 = 30;

pub struct AgentTool;

struct SubAgentConfig {
    tools: Option<Vec<String>>,
    disallowed_tools: Option<Vec<String>>,
    max_turns: u32,
}

fn get_sub_agent_config(agent_type: Option<&str>) -> SubAgentConfig {
    match agent_type {
        Some("Explore") => SubAgentConfig {
            tools: Some(READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect()),
            disallowed_tools: None,
            max_turns: DEFAULT_SUB_AGENT_TURNS,
        },
        Some("Plan") => SubAgentConfig {
            tools: None,
            disallowed_tools: Some(vec!["Agent".to_string()]),
            max_turns: MAX_SUB_AGENT_TURNS,
        },
        _ => SubAgentConfig { tools: None, disallowed_tools: None, max_turns: MAX_SUB_AGENT_TURNS },
    }
}

/// Short persona prefix prepended to the sub-agent prompt (agent.ts
/// buildSubAgentPrompt — the actual live TS behavior).
fn build_sub_agent_prompt(prompt: &str, agent_type: Option<&str>, description: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(desc) = description {
        parts.push(format!("Task: {desc}"));
        parts.push(String::new());
    }
    match agent_type {
        Some("Explore") => parts.extend([
            "You are a research sub-agent. Your job is to explore the codebase and gather information.".into(),
            "You have read-only access. Do NOT attempt to modify any files.".into(),
            "Focus on finding relevant code, understanding structure, and reporting findings.".into(),
            String::new(),
        ]),
        Some("Plan") => parts.extend([
            "You are a planning sub-agent. Your job is to analyze the problem and create a detailed plan.".into(),
            "You have access to all tools except Agent (no further delegation).".into(),
            "Create a clear, actionable plan with specific file paths and changes needed.".into(),
            String::new(),
        ]),
        _ => parts.extend([
            "You are a sub-agent working on a specific task.".into(),
            "Complete the task thoroughly and report your results.".into(),
            String::new(),
        ]),
    }
    parts.push(prompt.to_string());
    parts.join("\n")
}

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": "The task description or question for the sub-agent. Be specific and provide context."},
            "subagent_type": {"type": "string", "enum": ["Explore", "Plan", "default"], "description": "Sub-agent mode. \"Explore\" = read-only research, \"Plan\" = all tools except Agent, default = all tools."},
            "description": {"type": "string", "description": "A brief description of the sub-agent task for logging."},
            "model": {"type": "string", "description": "Override the model for the sub-agent. Default: use parent model."},
            "run_in_background": {"type": "boolean", "description": "If true, run the sub-agent in the background (future feature). Default: false."}
        },
        "required": ["prompt"]
    })
}

#[async_trait]
impl ToolDef for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self, input: Option<&Json>) -> String {
        let agent_type = input
            .and_then(|i| i.get("subagent_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        match agent_type {
            "Explore" => "Launch a read-only research sub-agent to explore the codebase.".into(),
            "Plan" => "Launch a planning sub-agent with full tool access (except delegation).".into(),
            _ => "Launch a sub-agent to handle a specific task with isolated context.".into(),
        }
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn max_result_size_chars(&self) -> usize {
        60_000
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let agent_type = input.get("subagent_type").and_then(|v| v.as_str()).unwrap_or("default");
        let desc = input
            .get("description")
            .or_else(|| input.get("prompt"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let short: String = desc.chars().take(40).collect();
        let ellipsis = if desc.chars().count() >= 40 { "..." } else { "" };
        format!("Agent({agent_type}): {short}{ellipsis}")
    }

    fn prompt(&self) -> String {
        [
            "Delegate a task to a sub-agent with isolated context.",
            "",
            "Sub-agent types:",
            "  Explore — Read-only research (Read, Glob, Grep, Bash)",
            "  Plan    — Full tools except Agent (no further delegation)",
            "  default — All tools available",
            "",
            "Guidelines:",
            "- Use Explore for quick research tasks that do not modify files.",
            "- Use Plan for complex analysis that needs a structured plan.",
            "- Use default for tasks that need to both read and write.",
            "- Be specific in the prompt — provide context and expected output.",
            "- Sub-agents have isolated message context but share file state.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.trim().is_empty() {
            return ToolResult::err("Error: prompt cannot be empty.");
        }
        let agent_type = input.get("subagent_type").and_then(|v| v.as_str());
        let description = input.get("description").and_then(|v| v.as_str());

        let Some(runner) = ctx.sub_agent.lock().unwrap().clone() else {
            return ToolResult::err(
                "Error: Agent tool not properly initialized. Query params not available.",
            );
        };

        let config = get_sub_agent_config(agent_type);
        let enhanced = build_sub_agent_prompt(prompt, agent_type, description);

        let response = runner
            .run(enhanced, config.tools, config.disallowed_tools, config.max_turns)
            .await;

        if response.is_empty() || response == "(No response from sub-agent)" {
            return ToolResult::ok("Sub-agent completed but produced no response.");
        }

        let header = description
            .map(|d| format!("[Sub-agent: {d}]"))
            .unwrap_or_else(|| format!("[Sub-agent: {}]", agent_type.unwrap_or("default")));

        ToolResult::ok(format!("{header}\n\n{response}"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::SubAgentRunner;
    use std::sync::{Arc, Mutex, RwLock};

    type RecordedCall = (
        String,
        Option<Vec<String>>,
        Option<Vec<String>>,
        u32,
    );

    struct RecordingRunner {
        calls: Mutex<Vec<RecordedCall>>,
    }

    #[async_trait]
    impl SubAgentRunner for RecordingRunner {
        async fn run(
            &self,
            prompt: String,
            tools_allow: Option<Vec<String>>,
            tools_disallow: Option<Vec<String>>,
            max_turns: u32,
        ) -> String {
            self.calls.lock().unwrap().push((prompt, tools_allow, tools_disallow, max_turns));
            "found 3 files".to_string()
        }
    }

    fn ctx_with_runner() -> (ToolContext, Arc<RecordingRunner>) {
        let runner = Arc::new(RecordingRunner { calls: Mutex::new(Vec::new()) });
        let ctx = ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(Default::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(crate::core::types::PermissionMode::Default)),
            permission_gate: Arc::new(crate::tools::test_support::AllowGate),
            sub_agent: Arc::new(Mutex::new(Some(runner.clone() as Arc<dyn SubAgentRunner>))),
        };
        (ctx, runner)
    }

    #[tokio::test]
    async fn explore_filters_tools_and_prefixes_prompt() {
        let (ctx, runner) = ctx_with_runner();
        let r = AgentTool
            .call(
                serde_json::json!({
                    "prompt": "find auth code",
                    "subagent_type": "Explore",
                    "description": "auth research"
                }),
                &ctx,
            )
            .await;

        assert!(!r.is_error(), "{}", r.result);
        assert!(r.result.starts_with("[Sub-agent: auth research]\n\n"));
        assert!(r.result.contains("found 3 files"));

        let (prompt, allow, disallow, turns) = &runner.calls.lock().unwrap()[0];
        assert!(prompt.starts_with("Task: auth research"));
        assert!(prompt.contains("You are a research sub-agent"));
        assert!(prompt.ends_with("find auth code"));
        assert_eq!(allow.as_ref().unwrap(), &["Read".to_string(), "Glob".to_string(), "Grep".to_string(), "Bash".to_string()]);
        assert!(disallow.is_none());
        assert_eq!(*turns, 30);
    }

    #[tokio::test]
    async fn plan_disallows_agent() {
        let (ctx, runner) = ctx_with_runner();
        AgentTool
            .call(serde_json::json!({"prompt": "plan it", "subagent_type": "Plan"}), &ctx)
            .await;
        let (_, allow, disallow, turns) = &runner.calls.lock().unwrap()[0];
        assert!(allow.is_none());
        assert_eq!(disallow.as_ref().unwrap(), &["Agent".to_string()]);
        assert_eq!(*turns, 50);
    }

    #[tokio::test]
    async fn default_all_tools() {
        let (ctx, runner) = ctx_with_runner();
        AgentTool.call(serde_json::json!({"prompt": "do it"}), &ctx).await;
        let (prompt, allow, disallow, turns) = &runner.calls.lock().unwrap()[0];
        assert!(prompt.contains("You are a sub-agent working on a specific task."));
        assert!(allow.is_none() && disallow.is_none());
        assert_eq!(*turns, 50);
    }

    #[tokio::test]
    async fn empty_prompt_rejected() {
        let (ctx, _) = ctx_with_runner();
        let r = AgentTool.call(serde_json::json!({"prompt": "  "}), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("prompt cannot be empty"));
    }

    #[tokio::test]
    async fn no_runner_reports_uninitialized() {
        let ctx = ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(Default::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(crate::core::types::PermissionMode::Default)),
            permission_gate: Arc::new(crate::tools::test_support::AllowGate),
            sub_agent: Arc::new(Mutex::new(None)),
        };
        let r = AgentTool.call(serde_json::json!({"prompt": "x"}), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("not properly initialized"));
    }

    #[tokio::test]
    async fn empty_response_message() {
        struct EmptyRunner;
        #[async_trait]
        impl SubAgentRunner for EmptyRunner {
            async fn run(&self, _: String, _: Option<Vec<String>>, _: Option<Vec<String>>, _: u32) -> String {
                "(No response from sub-agent)".to_string()
            }
        }
        let ctx = ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(Default::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(crate::core::types::PermissionMode::Default)),
            permission_gate: Arc::new(crate::tools::test_support::AllowGate),
            sub_agent: Arc::new(Mutex::new(Some(Arc::new(EmptyRunner) as Arc<dyn SubAgentRunner>))),
        };
        let r = AgentTool.call(serde_json::json!({"prompt": "x"}), &ctx).await;
        assert!(!r.is_error());
        assert_eq!(r.result, "Sub-agent completed but produced no response.");
    }
}
