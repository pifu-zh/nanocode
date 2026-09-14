//! Tool system — Rust port of `src/tools/registry.ts` + `core/types.ts` ToolDef.
//!
//! Fail-closed defaults (registry.ts buildTool): is_concurrency_safe → false,
//! is_read_only → false, max_result_size_chars → 30_000.

pub mod agent;
pub mod ask;
pub mod bash;
pub mod bash_readonly;
pub mod notebook_edit;
pub mod plan_mode;
pub mod todo;
pub mod web_fetch;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod streaming_executor;
pub mod write;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::Value as Json;

use crate::core::types::{ToolContext, ToolResult};

// ---------------------------------------------------------------------------
// Tool trait (buildTool defaults built in)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ToolDef: Send + Sync {
    fn name(&self) -> &str;

    /// Description; dynamic tools (Bash, Agent) may vary by input.
    fn description(&self, _input: Option<&Json>) -> String;

    /// JSON Schema for the `input_schema` API field.
    fn input_schema(&self) -> Json;

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult;

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        false // fail closed
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        false // fail closed
    }

    fn max_result_size_chars(&self) -> usize {
        30_000
    }

    fn user_facing_name(&self, _input: &Json) -> String {
        self.name().to_string()
    }

    fn prompt(&self) -> String {
        String::new()
    }
}

pub type Tool = Arc<dyn ToolDef + 'static>;

// ---------------------------------------------------------------------------
// Unknown-tool placeholder (registry.ts createErrorTool)
// ---------------------------------------------------------------------------

pub struct ErrorTool {
    pub tool_name: String,
}

#[async_trait]
impl ToolDef for ErrorTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self, _input: Option<&Json>) -> String {
        String::new()
    }

    fn input_schema(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _input: Json, _ctx: &ToolContext) -> ToolResult {
        ToolResult::err(format!(
            "Unknown tool: {}. Available tools will be listed in the system prompt.",
            self.tool_name
        ))
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        false
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Registry (registry.ts — module-level Map becomes an owned struct)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Tool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; a same-named tool overwrites the existing entry.
    pub fn register(&mut self, tool: Tool) {
        if let Some(pos) = self.tools.iter().position(|t| t.name() == tool.name()) {
            self.tools[pos] = tool;
        } else {
            self.tools.push(tool);
        }
    }

    pub fn get(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name() == name)
    }

    pub fn all(&self) -> &[Tool] {
        &self.tools
    }

    pub fn count(&self) -> usize {
        self.tools.len()
    }

    /// Filter by name allowlist / blocklist (registry.ts helpers).
    pub fn filter_by_names(&self, names: &[&str]) -> Vec<Tool> {
        self.tools.iter().filter(|t| names.contains(&t.name())).cloned().collect()
    }

    pub fn exclude_by_names(&self, names: &[&str]) -> Vec<Tool> {
        self.tools.iter().filter(|t| !names.contains(&t.name())).cloned().collect()
    }

    /// Tool summary for system prompts (registry.ts generateToolSummary).
    pub fn generate_summary(&self) -> String {
        if self.tools.is_empty() {
            return "(No tools registered)".to_string();
        }
        let mut out = String::from("Available tools:");
        for t in &self.tools {
            out.push_str(&format!("\n  - {}: {}", t.name(), t.description(None)));
        }
        out
    }
}

/// The 6 core tools of Phase 3 (registry.ts initializeTools registers more;
/// later phases append theirs here).
/// Phase-3 core tools only (used by focused tests).
pub fn initialize_core_tools() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(bash::BashTool));
    reg.register(Arc::new(read::ReadTool));
    reg.register(Arc::new(edit::EditTool));
    reg.register(Arc::new(write::WriteTool));
    reg.register(Arc::new(glob::GlobTool));
    reg.register(Arc::new(grep::GrepTool));
    reg.register(Arc::new(agent::AgentTool));
    reg
}

/// Full tool set (registry.ts initializeTools): 15 tools, with the Skill
/// tool wired to a shared skill list + sub-agent runner.
pub fn initialize_all_tools(
    todo_store: Arc<crate::tools::todo::TodoStore>,
    skills: Arc<RwLock<Vec<crate::skills::SkillDefinition>>>,
    sub_agent: Arc<RwLock<Option<Arc<dyn crate::core::types::SubAgentRunner>>>>,
    exit_plan_state: Arc<RwLock<Option<crate::core::types::PermissionMode>>>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(bash::BashTool));
    reg.register(Arc::new(read::ReadTool));
    reg.register(Arc::new(edit::EditTool));
    reg.register(Arc::new(write::WriteTool));
    reg.register(Arc::new(glob::GlobTool));
    reg.register(Arc::new(grep::GrepTool));
    reg.register(Arc::new(agent::AgentTool));
    reg.register(Arc::new(ask::AskTool));
    reg.register(Arc::new(todo::TodoTool { store: todo_store }));
    reg.register(Arc::new(web_fetch::WebFetchTool));
    reg.register(Arc::new(web_fetch::WebSearchTool));
    reg.register(Arc::new(plan_mode::EnterPlanModeTool));
    reg.register(Arc::new(plan_mode::ExitPlanModeTool { previous_mode: exit_plan_state }));
    reg.register(Arc::new(notebook_edit::NotebookEditTool));
    reg.register(Arc::new(crate::skills::SkillTool { skills, sub_agent }));
    reg
}

// ---------------------------------------------------------------------------
// Shared test support (tool tests construct contexts everywhere)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use crate::core::types::{
        FileHistoryState, FileStateCache, PermissionDecision, PermissionGate, PermissionMode,
        ToolContext,
    };
    use serde_json::Value as Json;
    use std::path::Path;
    use std::sync::{Arc, Mutex, RwLock};

    pub struct AllowGate;
    #[async_trait::async_trait]
    impl PermissionGate for AllowGate {
        async fn decide(&self, _: &str, _: &Json, _: &str) -> PermissionDecision {
            PermissionDecision::allow()
        }
    }

    pub fn test_ctx(cwd: &Path) -> ToolContext {
        ToolContext {
            cwd: Arc::new(cwd.to_path_buf()),
            file_state: Arc::new(Mutex::new(FileStateCache::new())),
            file_history: Arc::new(Mutex::new(FileHistoryState::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "test".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Default)),
            permission_gate: Arc::new(AllowGate),
            sub_agent: Arc::new(Mutex::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/tools/registry.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::PermissionMode;
    use std::sync::{Mutex, RwLock};

    struct DummyTool {
        safe: bool,
        readonly: bool,
        max: usize,
    }

    #[async_trait]
    impl ToolDef for DummyTool {
        fn name(&self) -> &str {
            "Dummy"
        }
        fn description(&self, _i: Option<&Json>) -> String {
            "dummy tool".into()
        }
        fn input_schema(&self) -> Json {
            serde_json::json!({"type": "object"})
        }
        async fn call(&self, _i: Json, _c: &ToolContext) -> ToolResult {
            ToolResult::ok("ran")
        }
        fn is_concurrency_safe(&self, _i: &Json) -> bool {
            self.safe
        }
        fn is_read_only(&self, _i: &Json) -> bool {
            self.readonly
        }
        fn max_result_size_chars(&self) -> usize {
            self.max
        }
    }

    fn dummy_ctx() -> ToolContext {
        ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(Default::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Default)),
            permission_gate: Arc::new(AllowGate),
            sub_agent: Arc::new(Mutex::new(None)),
        }
    }

    pub struct AllowGate;
    #[async_trait]
    impl crate::core::types::PermissionGate for AllowGate {
        async fn decide(&self, _t: &str, _i: &Json, _m: &str) -> crate::core::types::PermissionDecision {
            crate::core::types::PermissionDecision::allow()
        }
    }

    #[test]
    fn fail_closed_defaults() {
        let t: Tool = Arc::new(DummyTool { safe: false, readonly: false, max: 30_000 });
        assert!(!t.is_concurrency_safe(&serde_json::json!({})));
        assert!(!t.is_read_only(&serde_json::json!({})));
        assert_eq!(t.max_result_size_chars(), 30_000);
        assert_eq!(t.user_facing_name(&serde_json::json!({})), "Dummy");
        assert_eq!(t.prompt(), "");
    }

    #[test]
    fn explicit_overrides_preserved() {
        let t: Tool = Arc::new(DummyTool { safe: true, readonly: true, max: 100 });
        assert!(t.is_concurrency_safe(&serde_json::json!({})));
        assert!(t.is_read_only(&serde_json::json!({})));
        assert_eq!(t.max_result_size_chars(), 100);
    }

    #[test]
    fn register_overwrites_same_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool { safe: false, readonly: false, max: 30_000 }));
        reg.register(Arc::new(DummyTool { safe: true, readonly: true, max: 30_000 }));
        assert_eq!(reg.count(), 1);
        assert!(reg.get("Dummy").unwrap().is_read_only(&serde_json::json!({})));
    }

    #[test]
    fn unknown_lookup_none() {
        let reg = ToolRegistry::new();
        assert!(reg.get("Nope").is_none());
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn filter_and_exclude() {
        let reg = initialize_core_tools();
        let read_only = reg.filter_by_names(&["Read", "Glob"]);
        assert_eq!(read_only.len(), 2);
        let rest = reg.exclude_by_names(&["Read", "Glob"]);
        assert_eq!(rest.len(), reg.count() - 2);
    }

    #[test]
    fn full_registry_has_fifteen_tools() {
        let reg = initialize_all_tools(
            crate::tools::todo::TodoStore::new(),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
        );
        for name in [
            "Bash", "Read", "Edit", "Write", "Glob", "Grep", "Agent", "Ask",
            "Todo", "WebFetch", "WebSearch", "EnterPlanMode", "ExitPlanMode",
            "NotebookEdit", "Skill",
        ] {
            assert!(reg.has(name), "missing {name}");
        }
        assert_eq!(reg.count(), 15);
    }

    #[test]
    fn core_registry_has_seven_tools() {
        let reg = initialize_core_tools();
        for name in ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "Agent"] {
            assert!(reg.has(name), "missing {name}");
        }
        assert_eq!(reg.count(), 7);
    }

    #[tokio::test]
    async fn error_tool_reports_unknown() {
        let t = ErrorTool { tool_name: "Mystery".into() };
        let result = t.call(serde_json::json!({}), &dummy_ctx()).await;
        assert!(result.is_error());
        assert!(result.result.contains("Unknown tool: Mystery"));
        assert!(!t.is_concurrency_safe(&serde_json::json!({})));
    }
}
