//! Plan mode tools — Rust port of `src/tools/plan-mode.ts`.
//! EnterPlanMode / ExitPlanMode flip ToolContext.permission_mode (the TS
//! `(context as any).permissionMode` mutation becomes an explicit
//! Arc<RwLock<PermissionMode>> write; same next-turn-visible timing).

use async_trait::async_trait;
use serde_json::Value as Json;
use std::sync::{Arc, RwLock};

use crate::core::types::{PermissionMode, ToolContext, ToolResult};
use crate::tools::ToolDef;

pub struct EnterPlanModeTool;

#[async_trait]
impl ToolDef for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Enter plan mode. In plan mode, you can only read files and search — no edits or writes are allowed. Use this when you need to gather information and plan before making changes."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        serde_json::json!({
            "type": "object",
            "properties": {"reason": {"type": "string", "description": "Optional reason for entering plan mode."}}
        })
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn user_facing_name(&self, _input: &Json) -> String {
        "EnterPlanMode".to_string()
    }

    fn prompt(&self) -> String {
        "Enter plan mode to restrict to read-only operations.\nUseful for analysis and planning phases before making changes.".into()
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        if *ctx.permission_mode.read().unwrap() == PermissionMode::Plan {
            return ToolResult::ok("Already in plan mode.");
        }
        *ctx.permission_mode.write().unwrap() = PermissionMode::Plan;
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|r| format!("Reason: {r}"))
            .unwrap_or_default();
        ToolResult::ok(
            [
                "Plan mode activated. You can now only use read-only tools.",
                "Use ExitPlanMode when ready to implement.",
                &reason,
            ]
            .iter()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
    }
}

pub struct ExitPlanModeTool {
    /// Saved previous mode (TS module-level `_previousMode` → shared state).
    pub previous_mode: Arc<RwLock<Option<PermissionMode>>>,
}

#[async_trait]
impl ToolDef for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Exit plan mode and return to normal mode where edits and writes are allowed.".to_string()
    }

    fn input_schema(&self) -> Json {
        serde_json::json!({
            "type": "object",
            "properties": {"reason": {"type": "string", "description": "Optional reason for exiting plan mode."}}
        })
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn user_facing_name(&self, _input: &Json) -> String {
        "ExitPlanMode".to_string()
    }

    fn prompt(&self) -> String {
        "Exit plan mode and return to normal operation.\nRestores the previous permission mode.".into()
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        if *ctx.permission_mode.read().unwrap() != PermissionMode::Plan {
            return ToolResult::ok("Not currently in plan mode.");
        }

        let previous = self
            .previous_mode
            .write()
            .unwrap()
            .take()
            .unwrap_or(PermissionMode::Default);
        *ctx.permission_mode.write().unwrap() = previous;

        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|r| format!("Reason: {r}"))
            .unwrap_or_default();
        ToolResult::ok(
            [
                &format!(
                    "Plan mode deactivated. Restored to \"{}\" mode.",
                    match previous {
                        PermissionMode::Default => "default",
                        PermissionMode::Plan => "plan",
                        PermissionMode::AcceptEdits => "acceptEdits",
                        PermissionMode::BypassPermissions => "bypassPermissions",
                    }
                ),
                "All tools are now available including edits and writes.",
                &reason,
            ]
            .iter()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn exit_tool() -> (ExitPlanModeTool, Arc<RwLock<Option<PermissionMode>>>) {
        let state = Arc::new(RwLock::new(Some(PermissionMode::AcceptEdits)));
        (ExitPlanModeTool { previous_mode: state.clone() }, state)
    }

    #[tokio::test]
    async fn enter_and_exit_roundtrip() {
        let ctx = test_ctx(std::path::Path::new("/tmp"));
        let (exit_tool, prev_state) = exit_tool();

        *prev_state.write().unwrap() = Some(PermissionMode::AcceptEdits);

        let r = EnterPlanModeTool.call(serde_json::json!({}), &ctx).await;
        assert!(r.result.contains("Plan mode activated"));
        assert_eq!(*ctx.permission_mode.read().unwrap(), PermissionMode::Plan);

        // double-enter is a no-op
        let r = EnterPlanModeTool.call(serde_json::json!({}), &ctx).await;
        assert_eq!(r.result, "Already in plan mode.");

        let r = exit_tool.call(serde_json::json!({}), &ctx).await;
        assert!(r.result.contains("Restored to \"acceptEdits\" mode"));
        assert_eq!(*ctx.permission_mode.read().unwrap(), PermissionMode::AcceptEdits);
    }

    #[tokio::test]
    async fn exit_without_plan_is_noop() {
        let ctx = test_ctx(std::path::Path::new("/tmp"));
        let (exit_tool, _) = exit_tool();
        *exit_tool.previous_mode.write().unwrap() = None;
        let r = exit_tool.call(serde_json::json!({}), &ctx).await;
        assert_eq!(r.result, "Not currently in plan mode.");
    }
}
