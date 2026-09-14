//! Ask tool — Rust port of `src/tools/ask.ts`. Reads one line from stdin
//! (question written to stderr so stdout stays clean).

use async_trait::async_trait;
use serde_json::Value as Json;
use tokio::io::AsyncBufReadExt;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

pub struct AskTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {"question": {"type": "string", "description": "The question to ask the user. Be clear and specific about what information you need."}},
        "required": ["question"]
    })
}

#[async_trait]
impl ToolDef for AskTool {
    fn name(&self) -> &str {
        "Ask"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Ask the user a question and wait for their response. Use when you need clarification or additional information to proceed.".into()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        false // cannot run multiple user prompts concurrently
    }

    fn max_result_size_chars(&self) -> usize {
        10_000
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let q = input.get("question").and_then(|v| v.as_str()).unwrap_or("");
        let short: String = q.chars().take(50).collect();
        let dots = if q.chars().count() > 50 { "..." } else { "" };
        format!("Ask: {short}{dots}")
    }

    fn prompt(&self) -> String {
        [
            "Ask the user a question when you need clarification.",
            "",
            "Guidelines:",
            "- Only ask when you truly need information you cannot determine yourself.",
            "- Be specific about what you need to know.",
            "- Avoid asking multiple questions at once — one at a time.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(question) = input.get("question").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: question is required.");
        };
        if question.trim().is_empty() {
            return ToolResult::err("Error: question cannot be empty.");
        }

        eprintln!("\n\x1b[36m? {question}\x1b[0m\n> ");

        let stdin = tokio::io::stdin();
        let mut lines = tokio::io::BufReader::new(stdin).lines();

        let answer = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => l.trim().to_string(),
                Ok(None) => String::new(), // stdin closed
                Err(e) => return ToolResult::err(format!("Error reading user input: {e}")),
            },
            _ = ctx.cancel.cancelled() => {
                return ToolResult::ok("(User interaction cancelled)");
            }
        };

        if answer.is_empty() {
            ToolResult::ok("(No response from user)")
        } else {
            ToolResult::ok(answer)
        }
    }
}
