//! Compact prompt — Rust port of `src/prompt/compact-prompt.ts`.
//! The 9-section format matches Claude Code's compaction prompt exactly.

use crate::core::types::{ContentBlock, Message, ToolResultContent};

pub const COMPACT_PROMPT: &str = "\
Your task is to create a detailed summary of this conversation that will \
replace the conversation history. This summary will be used as context for \
continuing the conversation, so it must preserve all important information.

The summary should be detailed enough that a reader could continue the \
conversation without losing important context.

Please organize the summary into the following sections:

1. **Primary Request and Intent**: What is the user trying to accomplish? \
What are their goals?

2. **Key Technical Concepts**: Important technical details, architecture \
decisions, algorithms discussed

3. **Files and Code Sections**: Important files referenced or modified, with \
key code snippets preserved verbatim (include file paths and line numbers)

4. **Errors and fixes**: Any errors encountered and their resolutions

5. **Problem Solving**: Approaches tried, what worked and what didn't

6. **All user messages**: preserve the exact content and intent of all user \
messages

7. **Pending Tasks**: Tasks that still need to be completed

8. **Current Work**: What is currently being worked on

9. **Optional Next Step**: If there is a clear next step, describe it \
(should align with user's latest request)

Important guidelines:
- Preserve ALL file paths, code snippets, and error messages VERBATIM
- Include specific line numbers where code was modified
- Keep exact command-line invocations and their outputs
- Maintain the chronological order of events
- Be specific - include actual values, names, and identifiers rather than \
generic descriptions";

pub const COMPACT_SYSTEM_INSTRUCTION: &str = "\
You are a conversation summarizer. Your job is to create a detailed, \
structured summary of the conversation provided to you. You must follow \
the format and guidelines specified in the user message exactly. Do NOT \
attempt to continue the conversation, answer questions, or take any \
actions. Only produce the summary.";

pub const COMPACT_BOUNDARY_MARKER: &str = "[CONVERSATION_COMPACTED]";

pub fn format_compact_summary(summary: &str) -> String {
    format!(
        "{COMPACT_BOUNDARY_MARKER}\n\nThe following is a summary of the conversation so far. Continue the \
conversation from where the summary leaves off. Do NOT repeat information \
already covered in the summary — pick up where it ends.\n\n---\n\n{summary}\n\n---\n\nThe conversation has been compacted. The above summary replaces earlier \
messages. Continue from where the summary leaves off."
    )
}

/// Serialize messages for the compaction prompt: tool calls truncated at
/// 500 chars, tool results at 1000, thinking omitted (compact-prompt.ts).
pub fn serialize_messages_for_compact(messages: &[Message]) -> String {
    let mut lines: Vec<String> = Vec::new();

    for msg in messages {
        let role = match msg.role {
            crate::core::types::Role::User => "USER",
            crate::core::types::Role::Assistant => "ASSISTANT",
        };
        lines.push(format!("--- {role} ---"));

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => lines.push(text.clone()),
                ContentBlock::ToolUse { name, input, .. } => {
                    let json = serde_json::to_string(input).unwrap_or_default();
                    let short: String = json.chars().take(500).collect();
                    lines.push(format!("[Tool call: {name}({short})]"));
                }
                ContentBlock::ToolResult { content, .. } => {
                    let text = match content {
                        ToolResultContent::Text(s) => s.clone(),
                        ToolResultContent::Blocks(b) => {
                            serde_json::to_string(b).unwrap_or_default()
                        }
                    };
                    let truncated = if text.chars().count() > 1000 {
                        let t: String = text.chars().take(1000).collect();
                        format!("{t}...[truncated]")
                    } else {
                        text
                    };
                    lines.push(format!("[Tool result: {truncated}]"));
                }
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                    // Omit thinking blocks from compact — they are internal
                }
                ContentBlock::Image { .. } => {}
            }
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Role;

    #[test]
    fn prompt_has_nine_sections() {
        for section in [
            "Primary Request and Intent",
            "Key Technical Concepts",
            "Files and Code Sections",
            "Errors and fixes",
            "Problem Solving",
            "All user messages",
            "Pending Tasks",
            "Current Work",
            "Optional Next Step",
        ] {
            assert!(COMPACT_PROMPT.contains(section));
        }
    }

    #[test]
    fn boundary_marker_wraps_summary() {
        let out = format_compact_summary("the summary");
        assert!(out.starts_with(COMPACT_BOUNDARY_MARKER));
        assert!(out.contains("the summary"));
        assert!(out.ends_with("Continue from where the summary leaves off."));
    }

    #[test]
    fn serialization_truncates_and_omits_thinking() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::text("user says hi")],
                id: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking { thinking: "internal".into(), signature: None },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Bash".into(),
                        input: serde_json::json!({"command": "x".repeat(600)}),
                    },
                ],
                id: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::Text("r".repeat(1200)),
                    is_error: None,
                }],
                id: None,
            },
        ];
        let s = serialize_messages_for_compact(&msgs);
        assert!(s.contains("--- USER ---"));
        assert!(s.contains("--- ASSISTANT ---"));
        assert!(s.contains("user says hi"));
        assert!(!s.contains("internal"), "thinking omitted");
        assert!(!s.contains(&"r".repeat(1001)), "tool result truncated");
        assert!(s.contains("...[truncated]"));
    }
}
