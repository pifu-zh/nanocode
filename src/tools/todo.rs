//! Todo tool — Rust port of `src/tools/todo.ts`. In-memory task list.
//! The TS module-level `taskStore` becomes a shared store handle (RUST_DESIGN
//! §1.7); sessions own one store.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::sync::{Arc, Mutex};

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn icon(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "[ ]",
            TaskStatus::InProgress => "[~]",
            TaskStatus::Completed => "[x]",
        }
    }
    fn name(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
        }
    }
    fn order(&self) -> u8 {
        match self {
            TaskStatus::InProgress => 0,
            TaskStatus::Pending => 1,
            TaskStatus::Completed => 2,
        }
    }
    fn parse(s: &str) -> Option<TaskStatus> {
        match s {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub task: String,
    pub status: TaskStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Default)]
pub struct TodoStore {
    tasks: Mutex<Vec<Task>>,
}

impl TodoStore {
    pub fn new() -> Arc<TodoStore> {
        Arc::new(TodoStore { tasks: Mutex::new(Vec::new()) })
    }
}

pub struct TodoTool {
    pub store: Arc<TodoStore>,
}

fn format_task(task: &Task) -> String {
    format!(
        "{} {} | {} ({})",
        task.status.icon(),
        &task.id[..8.min(task.id.len())],
        task.task,
        task.status.name()
    )
}

fn format_task_list(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "No tasks.".to_string();
    }
    let mut sections: Vec<String> = Vec::new();
    for (label, filter) in [
        ("In Progress:", TaskStatus::InProgress),
        ("Pending:", TaskStatus::Pending),
        ("Completed:", TaskStatus::Completed),
    ] {
        let group: Vec<&Task> = tasks.iter().filter(|t| t.status == filter).collect();
        if !group.is_empty() {
            sections.push(label.to_string());
            sections.extend(group.iter().map(|t| format!("  {}", format_task(t))));
        }
    }
    let pending = tasks.iter().filter(|t| t.status == TaskStatus::Pending).count();
    let in_prog = tasks.iter().filter(|t| t.status == TaskStatus::InProgress).count();
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
    sections.push(String::new());
    sections.push(format!(
        "Total: {} ({} pending, {} in progress, {} completed)",
        tasks.len(),
        pending,
        in_prog,
        done
    ));
    sections.join("\n")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "enum": ["create", "update", "list"], "description": "The operation to perform: create a new task, update an existing task, or list all tasks."},
            "task": {"type": "string", "description": "Task description (required for \"create\")."},
            "id": {"type": "string", "description": "Task ID (required for \"update\")."},
            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "New status for the task (used with \"update\"). Default: in_progress."}
        },
        "required": ["command"]
    })
}

#[async_trait]
impl ToolDef for TodoTool {
    fn name(&self) -> &str {
        "Todo"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Manage a task list for tracking multi-step work. Create tasks, update their status, and list all tasks."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn user_facing_name(&self, input: &Json) -> String {
        format!("Todo: {}", input.get("command").and_then(|v| v.as_str()).unwrap_or(""))
    }

    fn prompt(&self) -> String {
        [
            "Track multi-step tasks during the session.",
            "",
            "Commands:",
            "  create — Create a new task (requires task description)",
            "  update — Update task status (requires id, optional status)",
            "  list   — List all tasks",
            "",
            "Statuses: pending, in_progress, completed",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, _ctx: &ToolContext) -> ToolResult {
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        match command {
            "create" => {
                let Some(task_desc) = input.get("task").and_then(|v| v.as_str()).map(str::trim) else {
                    return ToolResult::err("Error: task description is required for \"create\".");
                };
                if task_desc.is_empty() {
                    return ToolResult::err("Error: task description is required for \"create\".");
                }
                let task = Task {
                    id: uuid::Uuid::new_v4().to_string(),
                    task: task_desc.to_string(),
                    status: TaskStatus::Pending,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                };
                let formatted = format_task(&task);
                self.store.tasks.lock().unwrap().push(task);
                ToolResult::ok(format!("Created task: {formatted}"))
            }

            "update" => {
                let Some(id) = input.get("id").and_then(|v| v.as_str()) else {
                    return ToolResult::err("Error: task ID is required for \"update\".");
                };
                let mut tasks = self.store.tasks.lock().unwrap();
                let pos = tasks
                    .iter()
                    .position(|t| t.id == id)
                    .or_else(|| tasks.iter().position(|t| t.id.starts_with(id)));
                let Some(idx) = pos else {
                    return ToolResult::err(format!("Error: task not found with ID: {id}"));
                };
                let found = &mut tasks[idx];
                let task = found;
                task.status = input
                    .get("status")
                    .and_then(|v| v.as_str())
                    .and_then(TaskStatus::parse)
                    .unwrap_or(TaskStatus::InProgress);
                task.updated_at = now_ms();
                if let Some(desc) = input.get("task").and_then(|v| v.as_str()).map(str::trim) {
                    if !desc.is_empty() {
                        task.task = desc.to_string();
                    }
                }
                ToolResult::ok(format!("Updated task: {}", format_task(task)))
            }

            "list" => {
                let mut tasks = self.store.tasks.lock().unwrap().clone();
                tasks.sort_by(|a, b| {
                    a.status
                        .order()
                        .cmp(&b.status.order())
                        .then(a.created_at.cmp(&b.created_at))
                });
                ToolResult::ok(format_task_list(&tasks))
            }

            other => ToolResult::err(format!(
                "Error: unknown command \"{other}\". Use \"create\", \"update\", or \"list\"."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn tool() -> TodoTool {
        TodoTool { store: TodoStore::new() }
    }

    #[tokio::test]
    async fn create_update_list_flow() {
        let t = tool();
        let ctx = test_ctx(std::path::Path::new("/tmp"));

        let r = t
            .call(serde_json::json!({"command": "create", "task": "write tests"}), &ctx)
            .await;
        assert!(r.result.contains("[ ]") && r.result.contains("write tests"));

        let r = t.call(serde_json::json!({"command": "list"}), &ctx).await;
        assert!(r.result.contains("Pending:"));
        assert!(r.result.contains("Total: 1 (1 pending, 0 in progress, 0 completed)"));

        // update by prefix id
        let id: String = {
            let tasks = t.store.tasks.lock().unwrap();
            tasks[0].id[..8].to_string()
        };
        let r = t
            .call(serde_json::json!({"command": "update", "id": id, "status": "completed"}), &ctx)
            .await;
        assert!(r.result.contains("[x]"));

        let r = t.call(serde_json::json!({"command": "list"}), &ctx).await;
        assert!(r.result.contains("Completed:"));
    }

    #[tokio::test]
    async fn validation_errors() {
        let t = tool();
        let ctx = test_ctx(std::path::Path::new("/tmp"));
        let r = t.call(serde_json::json!({"command": "create"}), &ctx).await;
        assert!(r.is_error());
        let r = t.call(serde_json::json!({"command": "update"}), &ctx).await;
        assert!(r.is_error());
        let r = t
            .call(serde_json::json!({"command": "update", "id": "nope"}), &ctx)
            .await;
        assert!(r.result.contains("task not found"));
        let r = t.call(serde_json::json!({"command": "delete"}), &ctx).await;
        assert!(r.result.contains("unknown command"));
        let r = t.call(serde_json::json!({"command": "list"}), &ctx).await;
        assert_eq!(r.result, "No tasks.");
    }
}
