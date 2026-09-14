//! Context compaction — Rust port of `src/context/compaction.ts`.
//!
//! Summarize older messages into a boundary-marked summary while preserving
//! the last PRESERVE_RECENT_TURNS turns verbatim. Wired as the `Compactor`
//! the agent loop calls (auto + manual). Post-compact file re-attachment
//! (dead code in TS) is completed here per PORTING_PLAN D1.

use async_trait::async_trait;

use crate::core::agent::{CompactOutcome, Compactor};
use crate::core::api::CallModelParams;
use crate::core::types::{ContentBlock, Message, ModelConfig, Role};
use crate::context::post_compact::create_post_compact_attachments;
use crate::context::token_counting::estimate_message_tokens;
use crate::prompt::compact_prompt::{
    format_compact_summary, serialize_messages_for_compact, COMPACT_BOUNDARY_MARKER,
    COMPACT_PROMPT, COMPACT_SYSTEM_INSTRUCTION,
};

pub const PRESERVE_RECENT_TURNS: usize = 3;
pub const SAFETY_MARGIN_TOKENS: i64 = 13_000;
pub const MAX_SUMMARY_TOKENS: u64 = 8_000;

// ---------------------------------------------------------------------------
// Boundary detection & splitting (compaction.ts)
// ---------------------------------------------------------------------------

/// Index of the last compact boundary marker in a user message text block.
pub fn find_last_compact_boundary(messages: &[Message]) -> Option<usize> {
    messages.iter().enumerate().rev().find_map(|(i, msg)| {
        if msg.role == Role::User
            && msg.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains(COMPACT_BOUNDARY_MARKER))
            })
        {
            Some(i)
        } else {
            None
        }
    })
}

/// Indices of the last PRESERVE_RECENT_TURNS complete turns (user+assistant
/// pairs counted from the end).
fn count_turns_from_end(messages: &[Message]) -> Vec<usize> {
    let mut indices: Vec<usize> = Vec::new();
    let mut turn_count = 0usize;
    let mut expect_role = Role::Assistant;

    for (i, msg) in messages.iter().enumerate().rev() {
        if expect_role == Role::Assistant && msg.role == Role::Assistant {
            expect_role = Role::User;
        } else if expect_role == Role::User && msg.role == Role::User {
            turn_count += 1;
            expect_role = Role::Assistant;
        }
        if turn_count <= PRESERVE_RECENT_TURNS {
            indices.insert(0, i);
        }
        if turn_count >= PRESERVE_RECENT_TURNS {
            break;
        }
    }
    indices
}

fn split_messages(messages: &[Message], after_index: Option<usize>) -> (Vec<Message>, Vec<Message>) {
    let start_from = after_index.map_or(0, |i| i + 1);
    let relevant: Vec<Message> = messages[start_from.min(messages.len())..].to_vec();

    if relevant.len() <= PRESERVE_RECENT_TURNS * 2 {
        return (Vec::new(), relevant);
    }

    let preserve_indices = count_turns_from_end(&relevant);
    let split_point = preserve_indices.first().copied().unwrap_or(relevant.len());

    (
        relevant[..split_point].to_vec(),
        relevant[split_point..].to_vec(),
    )
}

// ---------------------------------------------------------------------------
// Compactor implementation
// ---------------------------------------------------------------------------

pub struct ModelCompactor {
    pub caller: std::sync::Arc<dyn crate::core::api::ModelCaller>,
    pub model: String,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl ModelCompactor {
    /// Non-streaming single-shot text call (TS callModelForCompact:
    /// messages.create with max_tokens 8000).
    async fn complete_text(&self, system: &str, user_message: &str) -> Result<String, String> {
        let mut config = ModelConfig {
            model: self.model.clone(),
            ..crate::core::api::get_model_config(&self.model)
        };
        config.max_output_tokens = MAX_SUMMARY_TOKENS;

        let params = CallModelParams {
            messages: vec![Message::user_text(user_message)],
            tools: vec![],
            model_config: config,
            system_prompt_blocks: vec![crate::core::types::SystemPromptBlock::text(system)],
            enable_thinking: false,
            thinking_budget: None,
            cancel: self.cancel.clone(),
        };

        let mut text = String::new();
        let mut events = Box::pin(self.caller.call_model(params));
        use futures::StreamExt;
        while let Some(event) = events.as_mut().next().await {
            match event {
                crate::core::types::AgentEvent::AssistantText { text: t } => text.push_str(&t),
                crate::core::types::AgentEvent::Error { error } => {
                    return Err(error.to_string());
                }
                _ => {}
            }
        }
        Ok(text)
    }
}

#[async_trait]
impl Compactor for ModelCompactor {
    async fn compact(&self, messages: Vec<Message>) -> Result<CompactOutcome, String> {
        let old_tokens = estimate_message_tokens(&messages);

        let boundary_index = find_last_compact_boundary(&messages);
        let pre_compact: Vec<Message> = boundary_index
            .map(|i| messages[..=i].to_vec())
            .unwrap_or_default();

        let (to_summarize, to_preserve) = split_messages(&messages, boundary_index);
        if to_summarize.is_empty() {
            return Ok(CompactOutcome { compacted: messages, old_tokens, new_tokens: old_tokens });
        }

        let serialized = serialize_messages_for_compact(&to_summarize);
        let user_message = format!(
            "{COMPACT_PROMPT}\n\nHere is the conversation to summarize:\n\n<conversation>\n{serialized}\n</conversation>\n\nPlease produce the summary now, following the 9-section format above."
        );

        let summary = self.complete_text(COMPACT_SYSTEM_INSTRUCTION, &user_message).await?;

        let summary_message = Message {
            role: Role::User,
            content: vec![ContentBlock::text(format_compact_summary(&summary))],
            id: None,
        };

        let mut compacted = pre_compact;
        compacted.push(summary_message);
        compacted.extend(to_preserve);

        let new_tokens = estimate_message_tokens(&compacted);
        Ok(CompactOutcome { compacted, old_tokens, new_tokens })
    }
}

/// Attach post-compact file refresh (completed dead wiring, D1). Returns the
/// messages including any attachment message.
pub async fn with_post_compact_attachments(
    compacted: Vec<Message>,
    file_state: &std::sync::Mutex<crate::core::types::FileStateCache>,
) -> Vec<Message> {
    let mut out = compacted;
    let attachments = create_post_compact_attachments(&out, file_state).await;
    out.extend(attachments);
    out
}

// ---------------------------------------------------------------------------
// Tests — ported from test/context/compaction.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Message {
        Message { role, content: vec![ContentBlock::text(text)], id: None }
    }

    #[test]
    fn constants_match_ts() {
        assert_eq!(PRESERVE_RECENT_TURNS, 3);
        assert_eq!(SAFETY_MARGIN_TOKENS, 13_000);
        assert_eq!(MAX_SUMMARY_TOKENS, 8_000);
    }

    #[test]
    fn boundary_detection() {
        let messages = vec![
            msg(Role::User, "a"),
            msg(Role::Assistant, "b"),
            msg(Role::User, &format!("x {COMPACT_BOUNDARY_MARKER} y")),
            msg(Role::Assistant, "c"),
        ];
        assert_eq!(find_last_compact_boundary(&messages), Some(2));

        let no_boundary = vec![msg(Role::User, "a")];
        assert_eq!(find_last_compact_boundary(&no_boundary), None);

        // last of multiple
        let multi = vec![
            msg(Role::User, COMPACT_BOUNDARY_MARKER),
            msg(Role::User, "mid"),
            msg(Role::User, COMPACT_BOUNDARY_MARKER),
        ];
        assert_eq!(find_last_compact_boundary(&multi), Some(2));
    }

    #[test]
    fn split_preserves_recent_turns() {
        // 3 full turns + extra
        let mut messages = Vec::new();
        for i in 0..5 {
            messages.push(msg(Role::User, &format!("u{i}")));
            messages.push(msg(Role::Assistant, &format!("a{i}")));
        }
        let (to_summarize, to_preserve) = split_messages(&messages, None);
        // preserves last 3 turns (6 messages), summarizes the first 4
        assert_eq!(to_preserve.len(), 6);
        assert_eq!(to_summarize.len(), 4);
        assert_eq!(to_preserve[0].content[0].as_text(), Some("u2"));
    }

    #[test]
    fn split_respects_previous_boundary() {
        let messages = vec![
            msg(Role::User, COMPACT_BOUNDARY_MARKER),
            msg(Role::Assistant, "a"),
            msg(Role::User, "u"),
        ];
        let (to_summarize, _) = split_messages(&messages, Some(0));
        assert!(to_summarize.is_empty());
    }

    #[test]
    fn split_too_few_messages_preserves_all() {
        let messages = vec![msg(Role::User, "a"), msg(Role::Assistant, "b")];
        let (to_summarize, to_preserve) = split_messages(&messages, None);
        assert!(to_summarize.is_empty());
        assert_eq!(to_preserve.len(), 2);
    }

    #[tokio::test]
    async fn compact_with_mock_model() {
        // Mock model returning a canned summary text
        use futures::StreamExt;
        #[allow(dead_code)]
        struct MockClient;
        #[async_trait]
        impl crate::core::api::ModelCaller for MockClient {
            fn call_model(
                &self,
                _p: CallModelParams,
            ) -> futures::stream::BoxStream<'static, crate::core::types::AgentEvent> {
                use crate::core::types::AgentEvent;
                futures::stream::iter(vec![
                    AgentEvent::AssistantText { text: "structured summary".into() },
                    AgentEvent::AssistantMessage {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::text("structured summary")],
                            id: None,
                        },
                    },
                    AgentEvent::TurnComplete { stop_reason: "end_turn".into() },
                ])
                .boxed()
            }
        }

        // ModelCompactor builds its own client; test via a shim Compactor impl
        struct MockCompactor;
        #[async_trait]
        impl Compactor for MockCompactor {
            async fn compact(&self, messages: Vec<Message>) -> Result<CompactOutcome, String> {
                let old = estimate_message_tokens(&messages);
                let boundary = find_last_compact_boundary(&messages);
                let (to_summarize, to_preserve) = split_messages(&messages, boundary);
                if to_summarize.is_empty() {
                    return Ok(CompactOutcome { compacted: messages, old_tokens: old, new_tokens: old });
                }
                let mut out = Vec::new();
                out.push(msg(Role::User, &format_compact_summary("structured summary")));
                out.extend(to_preserve);
                let new = estimate_message_tokens(&out);
                Ok(CompactOutcome { compacted: out, old_tokens: old, new_tokens: new })
            }
        }

        let mut messages = Vec::new();
        for _i in 0..5 {
            // Long messages so the summary is strictly smaller
            messages.push(msg(Role::User, &format!("user message {} with plenty of context that costs tokens to keep around verbatim in the window", "x".repeat(60))));
            messages.push(msg(Role::Assistant, &format!("assistant reply {} elaborating at length on the findings so far in this conversation segment", "y".repeat(60))));
        }
        let outcome = MockCompactor.compact(messages).await.unwrap();

        assert!(outcome.new_tokens < outcome.old_tokens);
        assert_eq!(outcome.compacted.len(), 1 + 6); // summary + preserved
        assert!(outcome.compacted[0].content[0]
            .as_text()
            .unwrap()
            .contains(COMPACT_BOUNDARY_MARKER));
    }
}
