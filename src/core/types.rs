//! Core shared types — Rust port of nanocode `src/core/types.ts`.
//!
//! Serialization field names are locked with `#[serde(rename)]` so that
//! session transcripts written by the TypeScript implementation remain
//! readable (BEHAVIOR.md §9, RUST_DESIGN.md §3.5).

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::core::errors::NanocodeError;

// ---------------------------------------------------------------------------
// Content blocks (API-compatible)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String,
    pub data: String,
}

/// `tool_result.content` is `string | ContentBlock[]` in the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Json,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        /// Signed thinking blocks must round-trip this field on the next
        /// request (Anthropic protocol; GLM emits `signature_delta`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking { data: String },
    Image { source: ImageSource },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    /// Extract text if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
            id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    /// Concatenated text of all text blocks (sub-agent final answer extraction).
    pub fn assistant_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---------------------------------------------------------------------------
// Stream events (yielded by the agent loop)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Json,
}

/// Mirrors TS `StreamEvent`. Runtime channel payload, never serialized.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AssistantText { text: String },
    AssistantMessage { message: Message },
    ToolUse { tool_use: ToolUseBlock },
    ToolResult {
        tool_use_id: String,
        tool_name: String,
        result: String,
        is_error: bool,
    },
    ToolStart {
        tool_use_id: String,
        tool_name: String,
        input: Json,
    },
    TurnComplete { stop_reason: String },
    Usage { usage: TokenUsage },
    Compact { old_tokens: i64, new_tokens: i64 },
    Error { error: NanocodeError },
    MaxTurnsReached { max_turns: u32 },
    Thinking { text: String },
}

// ---------------------------------------------------------------------------
// Tool system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ToolResult {
    pub fn ok(result: impl Into<String>) -> Self {
        ToolResult { result: result.into(), is_error: None }
    }
    pub fn err(result: impl Into<String>) -> Self {
        ToolResult { result: result.into(), is_error: Some(true) }
    }
    pub fn is_error(&self) -> bool {
        self.is_error.unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Permission system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
pub struct PermissionDecision {
    pub behavior: PermissionBehavior,
    pub message: Option<String>,
    pub updated_input: Option<Json>,
}

impl PermissionDecision {
    pub fn allow() -> Self {
        PermissionDecision { behavior: PermissionBehavior::Allow, message: None, updated_input: None }
    }
    pub fn deny(message: impl Into<String>) -> Self {
        PermissionDecision { behavior: PermissionBehavior::Deny, message: Some(message.into()), updated_input: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleSource {
    Session,
    Project,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub behavior: PermissionBehavior, // only Allow / Deny in rules
    pub source: RuleSource,
}

// ---------------------------------------------------------------------------
// File system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub content: String,
    pub timestamp: f64, // mtime ms
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(rename = "isPartialView", default, skip_serializing_if = "Option::is_none")]
    pub is_partial_view: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryBackup {
    #[serde(rename = "backupFileName")]
    pub backup_file_name: Option<String>,
    pub version: u64,
    #[serde(rename = "backupTime")]
    pub backup_time: u64, // epoch ms
}

#[derive(Debug, Clone)]
pub struct FileHistorySnapshot {
    pub message_id: String,
    pub tracked_file_backups: std::collections::HashMap<String, FileHistoryBackup>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FileHistoryState {
    pub snapshots: Vec<FileHistorySnapshot>,
    pub tracked_files: std::collections::HashSet<String>,
    pub snapshot_sequence: u64,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEntryType {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "compact_boundary")]
    CompactBoundary,
}

/// Transcript JSONL line — field names locked to the TS format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    #[serde(rename = "type")]
    pub entry_type: SessionEntryType,
    pub message: Message,
    pub timestamp: u64,
    pub id: String,
}

// ---------------------------------------------------------------------------
// Query params / model config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_thinking: bool,
    pub supports_caching: bool,
    pub price_per_input_token: f64,
    pub price_per_output_token: f64,
    pub price_per_cache_read: f64,
    pub price_per_cache_write: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptBlock {
    #[serde(rename = "type")]
    pub block_type: String, // "text"
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CacheControl {
    Ephemeral,
}

impl SystemPromptBlock {
    pub fn text(text: impl Into<String>) -> Self {
        SystemPromptBlock { block_type: "text".into(), text: text.into(), cache_control: None }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

// ---------------------------------------------------------------------------
// Tool context & permission gate
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

/// The permission callback (`onPermissionRequest` in TS) — headless allows
/// all; the REPL implements y/n/a interaction.
#[async_trait::async_trait]
pub trait PermissionGate: Send + Sync {
    async fn decide(&self, tool: &str, input: &Json, message: &str) -> PermissionDecision;
}

/// Sub-agent runner seam: the Agent tool spawns isolated sub-loops through
/// this. In the TS original the equivalent singleton was never wired
/// (`setAgentQueryParams` had no caller), so sub-agents always errored there;
/// per PORTING_PLAN D1 this implementation completes the wiring.
#[async_trait::async_trait]
pub trait SubAgentRunner: Send + Sync {
    async fn run(
        &self,
        prompt: String,
        tools_allow: Option<Vec<String>>,
        tools_disallow: Option<Vec<String>>,
        max_turns: u32,
    ) -> String;
}

/// LRU file state cache — struct declared here (as in TS core/types.ts),
/// implemented in `files/cache.rs`.
#[derive(Default)]
pub struct FileStateCache {
    pub(crate) entries: Vec<(String, FileState)>, // insertion order == LRU order (tail = MRU)
    pub(crate) total_bytes: usize,
}


impl FileStateCache {
    /// Read without touching LRU order.
    pub fn peek(&self, path: &str) -> Option<FileState> {
        let key = crate::files::utils::normalize_path(path);
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, s)| s.clone())
    }
}

/// Shared per-session tool context (TS ToolContext). Module-level singletons
/// from the TS implementation become explicit fields here (RUST_DESIGN §1.7).
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: Arc<PathBuf>,
    pub file_state: Arc<Mutex<FileStateCache>>,
    pub file_history: Arc<Mutex<FileHistoryState>>,
    pub modified_files: Arc<Mutex<std::collections::HashSet<String>>>,
    pub session_id: String,
    pub cancel: CancellationToken,
    pub permission_mode: Arc<RwLock<PermissionMode>>,
    pub permission_gate: Arc<dyn PermissionGate>,
    /// Injected by AgentLoop::run so the Agent tool can spawn sub-agents.
    pub sub_agent: Arc<Mutex<Option<Arc<dyn SubAgentRunner>>>>,
}

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

/// Command context is filled by the CLI layer (cli/commands.rs); declared here
/// to keep the type ownership identical to the TS layout.
pub struct CommandContext {
    pub messages: Vec<Message>,
    pub model_config: ModelConfig,
    pub cwd: String,
    pub session_id: String,
    pub permission_mode: PermissionMode,
    // setPermissionMode / setModel / clearMessages / compact / resumeSession /
    // sendPrompt are methods on the CLI session, not data.
}
