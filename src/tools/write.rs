//! Write tool — Rust port of `src/tools/write.ts`.
//!
//! Full-file creation/overwrite with read-before-write and staleness checks,
//! atomic writes, cache/history updates.

use async_trait::async_trait;
use serde_json::Value as Json;

use crate::core::types::{FileState, ToolContext, ToolResult};
use crate::files::atomic_write::atomic_write_sync;
use crate::tools::ToolDef;
use crate::tools::read::now_ms;

pub struct WriteTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "The file to write. Absolute or relative to working directory."},
            "content": {"type": "string", "description": "The complete content to write to the file."}
        },
        "required": ["file_path", "content"]
    })
}

fn extract_path(input: &Json) -> Option<String> {
    ["file_path", "path", "filePath"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
}

#[async_trait]
impl ToolDef for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Write content to a file, creating it if it does not exist or overwriting if it does. For targeted edits to existing files, prefer the Edit tool instead."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn user_facing_name(&self, input: &Json) -> String {
        format!("Write: {}", extract_path(input).unwrap_or_default())
    }

    fn prompt(&self) -> String {
        [
            "Write complete file contents to disk.",
            "",
            "Guidelines:",
            "- Use for creating new files or complete rewrites.",
            "- For targeted edits, use the Edit tool instead.",
            "- You must Read existing files before overwriting them.",
            "- Parent directories are created automatically.",
            "- Writes are atomic (temp file → rename) to prevent corruption.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(raw_path) = extract_path(&input) else {
            return ToolResult::err("Error: file_path parameter is required.");
        };
        if raw_path.is_empty() {
            return ToolResult::err("Error: file_path parameter is required.");
        }
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let file_path = if raw_path.starts_with('/') {
            std::path::PathBuf::from(&raw_path)
        } else {
            ctx.cwd.join(&raw_path)
        };
        let path_str = file_path.to_string_lossy().to_string();

        // Read-before-write for existing files
        if file_path.exists() {
            let Some(cached) = ctx.file_state.lock().unwrap().get(&path_str) else {
                return ToolResult::err(format!(
                    "Error: file already exists at {path_str}. You must Read the file before overwriting it. Use the Read tool first, or use the Edit tool for targeted changes."
                ));
            };
            let current_mtime = std::fs::metadata(&file_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64);
            let Some(current_mtime) = current_mtime else {
                return ToolResult::err(format!("Error checking file: stat failed for {path_str}"));
            };
            if current_mtime > cached.timestamp + 1000.0 {
                return ToolResult::err(
                    "Error: file has been modified since you last read it. Please Read the file again before writing.",
                );
            }
        }

        if let Err(e) = atomic_write_sync(&file_path, content) {
            let msg = if e.kind() == std::io::ErrorKind::PermissionDenied {
                format!("Error: permission denied writing to {path_str}")
            } else {
                format!("Error writing file: {e}")
            };
            return ToolResult::err(msg);
        }

        ctx.modified_files.lock().unwrap().insert(path_str.clone());
        ctx.file_history.lock().unwrap().tracked_files.insert(path_str.clone());

        let mtime = std::fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or_else(now_ms);

        ctx.file_state.lock().unwrap().set(
            &path_str,
            FileState { content: content.to_string(), timestamp: mtime, offset: None, limit: None, is_partial_view: None },
        );

        let line_count = content.split('\n').count();
        ToolResult::ok(format!("Written: {path_str} ({line_count} lines)"))
    }
}

// ---------------------------------------------------------------------------
// Tests (write behavior; TS has no dedicated write tests — behavior derived
// from write.ts and the read-before-write contract)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    #[tokio::test]
    async fn creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("new/deep/file.txt");
        let r = WriteTool
            .call(serde_json::json!({"file_path": f.to_str().unwrap(), "content": "a\nb\nc"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.is_error(), "{}", r.result);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\nc");
        assert!(r.result.contains("Written: ") && r.result.contains("(3 lines)"));
    }

    #[tokio::test]
    async fn existing_file_requires_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "old").unwrap();
        let ctx = test_ctx(dir.path());
        let r = WriteTool
            .call(serde_json::json!({"file_path": f.to_str().unwrap(), "content": "new"}), &ctx)
            .await;
        assert!(r.is_error());
        assert!(r.result.contains("You must Read the file before overwriting"));
    }

    #[tokio::test]
    async fn overwrite_after_read_ok_and_updates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "old").unwrap();
        let ctx = test_ctx(dir.path());
        // Simulate a fresh Read
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64;
        ctx.file_state.lock().unwrap().set(
            &f.to_string_lossy(),
            FileState { content: "old".into(), timestamp: mtime, offset: None, limit: None, is_partial_view: None },
        );

        let r = WriteTool
            .call(serde_json::json!({"file_path": f.to_str().unwrap(), "content": "brand\nnew"}), &ctx)
            .await;
        assert!(!r.is_error(), "{}", r.result);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "brand\nnew");
        assert_eq!(ctx.file_state.lock().unwrap().get(f.to_str().unwrap()).unwrap().content, "brand\nnew");
    }

    #[tokio::test]
    async fn empty_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = WriteTool.call(serde_json::json!({"content": "x"}), &test_ctx(dir.path())).await;
        assert!(r.is_error());
    }
}
