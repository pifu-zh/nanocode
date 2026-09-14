//! NotebookEdit tool — Rust port of `src/tools/notebook-edit.ts`.
//! Edits a Jupyter `.ipynb` cell (0-based index); code cells clear outputs.
//! The notebook JSON structure is preserved as a `serde_json` object.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::path::PathBuf;

use crate::core::types::{FileState, ToolContext, ToolResult};
use crate::files::atomic_write::atomic_write_sync;
use crate::tools::ToolDef;
use crate::tools::read::now_ms;

pub struct NotebookEditTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "notebook_path": {"type": "string", "description": "Path to the .ipynb file. Absolute or relative to working directory."},
            "cell_index": {"type": "number", "description": "0-based cell index to edit."},
            "new_source": {"type": "string", "description": "New source code/content for the cell."},
            "cell_type": {"type": "string", "enum": ["code", "markdown", "raw"], "description": "Optionally change the cell type."}
        },
        "required": ["notebook_path", "cell_index", "new_source"]
    })
}

#[async_trait]
impl ToolDef for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Edit a Jupyter notebook (.ipynb) cell. Specify the notebook path, cell index (0-based), and the new cell source code.".into()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn user_facing_name(&self, input: &Json) -> String {
        format!(
            "NotebookEdit: {} cell {}",
            input.get("notebook_path").and_then(|v| v.as_str()).unwrap_or(""),
            input.get("cell_index").and_then(|v| v.as_i64()).unwrap_or(-1)
        )
    }

    fn prompt(&self) -> String {
        [
            "Edit Jupyter notebook (.ipynb) cells.",
            "",
            "Guidelines:",
            "- Cell index is 0-based.",
            "- Code cell outputs are cleared after editing.",
            "- You can optionally change the cell type (code, markdown, raw).",
            "- Read the notebook first to see current cell contents.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(path) = input.get("notebook_path").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: notebook_path is required.");
        };
        let Some(cell_index) = input.get("cell_index").and_then(|v| v.as_u64()) else {
            return ToolResult::err("Error: cell_index is required (0-based).");
        };
        let Some(new_source) = input.get("new_source").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: new_source is required.");
        };
        let cell_type_change = input.get("cell_type").and_then(|v| v.as_str());

        let file_path: PathBuf = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            ctx.cwd.join(path)
        };
        let path_str = file_path.to_string_lossy().to_string();

        let raw = match tokio::fs::read_to_string(&file_path).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::err(format!("Error: notebook not found: {path_str}"));
            }
            Err(e) => return ToolResult::err(format!("Error reading notebook: {e}")),
        };

        let mut notebook: Json = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                return ToolResult::err("Error: file is not valid JSON (not a Jupyter notebook).");
            }
        };

        let Some(cells) = notebook.get_mut("cells").and_then(|c| c.as_array_mut()) else {
            return ToolResult::err("Error: not a valid Jupyter notebook (no cells array).");
        };

        if cell_index as usize >= cells.len() {
            return ToolResult::err(format!(
                "Error: cell index {cell_index} out of range (notebook has {} cells, indices 0-{}).",
                cells.len(),
                cells.len().saturating_sub(1)
            ));
        }

        let cell = &mut cells[cell_index as usize];
        let old_source_len = match &cell["source"] {
            Json::Array(lines) => lines
                .iter()
                .filter_map(|l| l.as_str())
                .collect::<String>()
                .len(),
            Json::String(s) => s.len(),
            _ => 0,
        };

        // Notebooks store source as an array of lines, each ending with \n
        // except the last.
        let mut lines: Vec<String> = new_source.split('\n').map(String::from).collect();
        let len = lines.len();
        if len > 1 {
            for line in lines.iter_mut().take(len - 1) {
                line.push('\n');
            }
        }
        let lines_json: Vec<Json> = lines.into_iter().map(Json::String).collect();
        cell["source"] = Json::Array(lines_json);

        if let Some(ct) = cell_type_change {
            cell["cell_type"] = Json::String(ct.to_string());
        }

        // Code cells: clear stale outputs
        if cell["cell_type"].as_str() == Some("code") {
            cell["outputs"] = Json::Array(Vec::new());
            cell["execution_count"] = Json::Null;
        }

        let new_content = format!(
            "{}\n",
            serde_json::to_string_pretty(&notebook).unwrap_or_default()
        );

        if let Err(e) = atomic_write_sync(&file_path, &new_content) {
            return ToolResult::err(format!("Error writing notebook: {e}"));
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
            FileState { content: new_content, timestamp: mtime, offset: None, limit: None, is_partial_view: None },
        );

        let type_note = cell_type_change
            .map(|ct| format!("\nCell type changed to: {ct}"))
            .unwrap_or_default();
        ToolResult::ok(format!(
            "Edited cell {cell_index} in {path}\nOld source ({old_source_len} chars) -> New source ({} chars){type_note}",
            new_source.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn make_notebook(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("nb.ipynb");
        let nb = serde_json::json!({
            "cells": [
                {"cell_type": "code", "source": ["print(1)\n"], "metadata": {}, "outputs": [{"o": 1}], "execution_count": 3},
                {"cell_type": "markdown", "source": ["# Title"], "metadata": {}}
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        });
        std::fs::write(&path, serde_json::to_string_pretty(&nb).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn edits_cell_and_clears_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let nb = make_notebook(dir.path());
        let ctx = test_ctx(dir.path());

        let r = NotebookEditTool
            .call(
                serde_json::json!({
                    "notebook_path": nb.to_str().unwrap(),
                    "cell_index": 0,
                    "new_source": "print(42)"
                }),
                &ctx,
            )
            .await;

        assert!(!r.is_error(), "{}", r.result);
        assert!(r.result.contains("Edited cell 0"));
        assert!(r.result.contains("Old source (9 chars) -> New source (9 chars)"));

        let out: Json = serde_json::from_str(&std::fs::read_to_string(&nb).unwrap()).unwrap();
        let cell = &out["cells"][0];
        assert_eq!(cell["source"][0].as_str().unwrap(), "print(42)");
        assert_eq!(cell["outputs"].as_array().unwrap().len(), 0, "outputs cleared");
        assert!(cell["execution_count"].is_null());
    }

    #[tokio::test]
    async fn cell_type_change_and_range_check() {
        let dir = tempfile::tempdir().unwrap();
        let nb = make_notebook(dir.path());
        let ctx = test_ctx(dir.path());

        let r = NotebookEditTool
            .call(
                serde_json::json!({
                    "notebook_path": nb.to_str().unwrap(),
                    "cell_index": 1,
                    "new_source": "now code",
                    "cell_type": "code"
                }),
                &ctx,
            )
            .await;
        assert!(r.result.contains("Cell type changed to: code"));

        let r = NotebookEditTool
            .call(
                serde_json::json!({
                    "notebook_path": nb.to_str().unwrap(),
                    "cell_index": 5,
                    "new_source": "x"
                }),
                &ctx,
            )
            .await;
        assert!(r.is_error());
        assert!(r.result.contains("out of range"));
    }

    #[tokio::test]
    async fn missing_notebook_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let r = NotebookEditTool
            .call(
                serde_json::json!({"notebook_path": "nope.ipynb", "cell_index": 0, "new_source": "x"}),
                &ctx,
            )
            .await;
        assert!(r.result.contains("notebook not found"));
    }
}
