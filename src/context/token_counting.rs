//! Token estimation — Rust port of `src/context/token-counting.ts`.
//!
//! Two estimators exist in the original and BOTH are behavior:
//! - `estimate_message_tokens` (token-counting.ts): used by /status, /context.
//!   Structure overhead 4 tokens per message, images 1500.
//! - `estimate_message_tokens_loop` (agent.ts inline): used by the agent
//!   loop's auto-compact check. Images 2000, no per-message overhead,
//!   tool_use estimated from `name + JSON(input)`.
//!
//! See ARCHITECTURE.md §4.5 for why they differ.

use serde_json::Value as Json;

use crate::core::types::{ContentBlock, Message, SystemPromptBlock, ToolResultContent};

pub const CHARS_PER_TOKEN: usize = 4;
pub const JSON_CHARS_PER_TOKEN: usize = 2;

const LOOP_IMAGE_TOKENS: i64 = 2_000;
const EXPORT_IMAGE_TOKENS: i64 = 1_500;
const STRUCTURE_OVERHEAD: i64 = 4;

// ---------------------------------------------------------------------------
// Core estimation
// ---------------------------------------------------------------------------

/// tokens ≈ round(chars / 4), counting Unicode scalar values (closest to the
/// TS String.length behavior for non-BMP-agnostic text).
pub fn estimate_tokens(text: &str) -> i64 {
    let chars = text.chars().count() as i64;
    if chars == 0 {
        return 0;
    }
    // Math.round(n / 4) with half rounding up == (n + 2) / 4 for integers.
    (chars + 2) / 4
}

/// tokens ≈ round(json_len / 2). JSON null/undefined estimate to 0 (TS behavior).
pub fn estimate_json_tokens(value: &Json) -> i64 {
    if value.is_null() {
        return 0;
    }
    match serde_json::to_string(value) {
        Ok(s) => {
            let chars = s.chars().count() as i64;
            (chars + 1) / 2 // round(len / 2), half up
        }
        Err(_) => 0,
    }
}

fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(s) => s.clone(),
        ToolResultContent::Blocks(blocks) => serde_json::to_string(blocks).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Loop estimator (agent.ts estimateMessageTokens — inline version)
// ---------------------------------------------------------------------------

fn estimate_block_tokens_loop(block: &ContentBlock) -> i64 {
    match block {
        ContentBlock::Text { text } => estimate_tokens(text),
        ContentBlock::ToolUse { name, input, .. } => {
            let serialized = serde_json::to_string(input).unwrap_or_default();
            estimate_tokens(&format!("{name}{serialized}"))
        }
        ContentBlock::ToolResult { content, .. } => estimate_tokens(&tool_result_text(content)),
        ContentBlock::Thinking { thinking, .. } => estimate_tokens(thinking),
        ContentBlock::RedactedThinking { data } => estimate_tokens(data),
        ContentBlock::Image { .. } => LOOP_IMAGE_TOKENS,
    }
}

/// The agent-loop inline estimator (images = 2000).
pub fn estimate_message_tokens_loop(messages: &[Message]) -> i64 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(estimate_block_tokens_loop)
        .sum()
}

// ---------------------------------------------------------------------------
// Export estimator (token-counting.ts)
// ---------------------------------------------------------------------------

fn estimate_block_tokens(block: &ContentBlock) -> i64 {
    match block {
        ContentBlock::Text { text } => estimate_tokens(text),
        ContentBlock::ToolUse { id, name, input } => {
            estimate_tokens(name) + estimate_tokens(id) + estimate_json_tokens(input)
        }
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(s) => estimate_tokens(s),
            ToolResultContent::Blocks(blocks) => blocks.iter().map(estimate_block_tokens).sum(),
        },
        ContentBlock::Thinking { thinking, .. } => estimate_tokens(thinking),
        ContentBlock::RedactedThinking { data } => estimate_tokens(data),
        ContentBlock::Image { .. } => EXPORT_IMAGE_TOKENS,
    }
}

/// /status and /context estimator (structure overhead 4, images 1500).
pub fn estimate_message_tokens(messages: &[Message]) -> i64 {
    messages
        .iter()
        .map(|m| STRUCTURE_OVERHEAD + m.content.iter().map(estimate_block_tokens).sum::<i64>())
        .sum()
}

pub fn estimate_system_prompt_tokens(blocks: &[SystemPromptBlock]) -> i64 {
    blocks.iter().map(|b| estimate_tokens(&b.text)).sum()
}

// ---------------------------------------------------------------------------
// Budget utilities (token-counting.ts)
// ---------------------------------------------------------------------------

pub fn would_exceed_budget(current_tokens: i64, additional_text: &str, budget: i64) -> bool {
    current_tokens + estimate_tokens(additional_text) > budget
}

/// Truncate text to a token budget, cutting at the last word boundary when
/// that boundary is beyond 80% of the target (token-counting.ts).
pub fn truncate_to_token_budget(text: &str, max_tokens: i64) -> String {
    if estimate_tokens(text) <= max_tokens {
        return text.to_string();
    }

    let target_chars = (max_tokens * CHARS_PER_TOKEN as i64) as usize;
    let truncated: String = text.chars().take(target_chars).collect();

    let last_break = truncated
        .rfind('\n')
        .or_else(|| truncated.rfind(' '));

    match last_break {
        Some(idx) if idx as i64 > (target_chars as i64) * 8 / 10 => {
            format!("{}\n...[truncated]", &truncated[..idx])
        }
        _ => format!("{truncated}...[truncated]"),
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/context/token-counting.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Role, ToolUseBlock};

    fn msg(blocks: Vec<ContentBlock>) -> Message {
        Message { role: Role::User, content: blocks, id: None }
    }

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("Hello"), 1); // 5 chars → round(1.25) = 1
        assert_eq!(estimate_tokens("Hello, World!"), 3); // 13 → 3.25 → 3
        assert_eq!(estimate_tokens("ab"), 1); // round(0.5) = 1 (half up)
    }

    #[test]
    fn json_tokens_estimate() {
        assert_eq!(estimate_json_tokens(&Json::Null), 0);
        let v = serde_json::json!({"key": "value"});
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(estimate_json_tokens(&v), (s.chars().count() as i64 + 1) / 2);
    }

    #[test]
    fn message_estimator_structure_overhead() {
        let m = msg(vec![]);
        assert_eq!(estimate_message_tokens(&[m]), STRUCTURE_OVERHEAD);
    }

    #[test]
    fn loop_estimator_image_is_2000_export_is_1500() {
        let img = ContentBlock::Image {
            source: crate::core::types::ImageSource {
                source_type: "base64".into(),
                media_type: "image/png".into(),
                data: "xxx".into(),
            },
        };
        assert_eq!(estimate_message_tokens_loop(&[msg(vec![img.clone()])]), 2000);
        // export version adds the 4-token message structure overhead
        assert_eq!(estimate_message_tokens(&[msg(vec![img])]), 1504);
    }

    #[test]
    fn tool_use_export_uses_name_id_and_json() {
        let block = ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let expected = estimate_tokens("Bash")
            + estimate_tokens("toolu_1")
            + estimate_json_tokens(&serde_json::json!({"command": "ls"}));
        assert_eq!(estimate_message_tokens(&[msg(vec![block])]), STRUCTURE_OVERHEAD + expected);
    }

    #[test]
    fn tool_use_loop_uses_name_plus_serialized_input() {
        let block = ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let serialized = serde_json::to_string(&serde_json::json!({"command": "ls"})).unwrap();
        let expected = estimate_tokens(&format!("Bash{serialized}"));
        assert_eq!(estimate_message_tokens_loop(&[msg(vec![block])]), expected);
    }

    #[test]
    fn budget_check() {
        // 16 chars → 4 tokens → 94 ≤ 100 (not exceeding)
        assert!(!would_exceed_budget(90, "aaaaaaaaaaaaaaaa", 100));
        // 97+4=101 > 100
        assert!(would_exceed_budget(97, "aaaaaaaaaaaaaaaa", 100));
    }

    #[test]
    fn truncate_short_text_untouched() {
        assert_eq!(truncate_to_token_budget("hello", 10), "hello");
    }

    #[test]
    fn truncate_cuts_at_word_boundary() {
        let text = "word ".repeat(100); // 500 chars ≈ 125 tokens
        let out = truncate_to_token_budget(&text, 20); // 80 chars target
        assert!(out.ends_with("...[truncated]"));
        assert!(out.len() < 120);
    }

    #[test]
    fn system_prompt_tokens() {
        let blocks = vec![
            SystemPromptBlock::text("12345678"), // 2
            SystemPromptBlock::text("abcd"),     // 1
        ];
        assert_eq!(estimate_system_prompt_tokens(&blocks), 3);
    }

    // Keep ToolUseBlock referenced (used by later phases); silence unused in tests.
    #[allow(dead_code)]
    fn _touch(_: ToolUseBlock) {}
}
