//! Edit tool — Rust port of `src/tools/edit.ts`.
//!
//! Find-and-replace with a strict 6-step validation chain:
//! 1. no-op check  2. existence (empty old_string creates)  3.
//!    read-before-edit  4. staleness (>1s mtime delta)  5. exact match
//!    6. uniqueness
//!
//! Writes are atomic; caches are updated on success only.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::path::PathBuf;

use crate::core::types::{FileState, ToolContext, ToolResult};
use crate::files::atomic_write::atomic_write_sync;
use crate::tools::ToolDef;
use crate::tools::read::now_ms;

pub struct EditTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "The file to edit. Absolute or relative to working directory."},
            "old_string": {"type": "string", "description": "The exact text to find and replace. Empty string to create a new file with new_string as content."},
            "new_string": {"type": "string", "description": "The replacement text. Empty string to delete the old_string."},
            "replace_all": {"type": "boolean", "description": "If true, replace all occurrences. Default: false (requires unique match)."}
        },
        "required": ["file_path", "old_string", "new_string"]
    })
}

fn extract_path(input: &Json) -> Option<String> {
    ["file_path", "path", "filePath"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
}

fn count_occurrences(content: &str, search: &str) -> usize {
    if search.is_empty() {
        return 0;
    }
    content.matches(search).count()
}

/// Diff snippet with 3 context lines and an `@@` hunk header (edit.ts
/// generateDiffSnippet).
fn generate_diff_snippet(old_content: &str, new_content: &str, old_string: &str, new_string: &str) -> String {
    const CONTEXT: usize = 3;
    let Some(byte_idx) = old_content.find(old_string) else { return String::new() };

    // Line index of the change (count newlines before the match)
    let lines_before = old_content[..byte_idx].matches('\n').count();
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let old_string_lines = old_string.split('\n').count();
    let new_string_lines = new_string.split('\n').count();

    let start_line = lines_before.saturating_sub(CONTEXT);
    let end_line_old = (lines_before + old_string_lines + CONTEXT).min(old_lines.len());
    let end_line_new = (lines_before + new_string_lines + CONTEXT).min(new_lines.len());

    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "@@ -{},{} +{},{} @@",
        start_line + 1,
        end_line_old - start_line,
        start_line + 1,
        end_line_new - start_line
    ));

    // Context before
    for i in start_line..lines_before {
        parts.push(format!(" {}", old_lines.get(i).unwrap_or(&"")));
    }
    // Removed lines
    for i in lines_before..(lines_before + old_string_lines) {
        parts.push(format!("-{}", old_lines.get(i).unwrap_or(&"")));
    }
    // Added lines
    for i in lines_before..(lines_before + new_string_lines) {
        parts.push(format!("+{}", new_lines.get(i).unwrap_or(&"")));
    }
    // Context after
    for i in (lines_before + new_string_lines)..end_line_new {
        parts.push(format!(" {}", new_lines.get(i).unwrap_or(&"")));
    }

    parts.join("\n")
}

fn resolve(cwd: &std::path::Path, raw: &str) -> PathBuf {
    if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    }
}

#[async_trait]
impl ToolDef for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Make a targeted edit to a file by specifying the exact text to find and replace. For creating new files, use empty old_string."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn user_facing_name(&self, input: &Json) -> String {
        format!("Edit: {}", extract_path(input).unwrap_or_default())
    }

    fn prompt(&self) -> String {
        [
            "Make targeted edits to files using find-and-replace.",
            "",
            "Guidelines:",
            "- ALWAYS Read the file before editing.",
            "- old_string must match EXACTLY (whitespace matters).",
            "- Include enough context in old_string for a unique match.",
            "- To create a new file: use empty old_string with content as new_string.",
            "- To delete text: use empty new_string.",
            "- Prefer Edit over Write for modifying existing files.",
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
        let old_string = input.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let new_string = input.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        let file_path = resolve(&ctx.cwd, &raw_path);
        let path_str = file_path.to_string_lossy().to_string();

        // 1. No-op check
        if old_string == new_string {
            return ToolResult::err(
                "Error: old_string and new_string are identical. No changes needed.",
            );
        }

        let file_exists = file_path.exists();

        // 2. Empty old_string + missing file → create
        if old_string.is_empty() && !file_exists {
            return match atomic_write_sync(&file_path, new_string) {
                Ok(()) => {
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
                        FileState { content: new_string.to_string(), timestamp: mtime, offset: None, limit: None, is_partial_view: None },
                    );
                    let line_count = new_string.split('\n').count();
                    ToolResult::ok(format!("Created new file: {path_str} ({line_count} lines)"))
                }
                Err(e) => ToolResult::err(format!("Error creating file: {e}")),
            };
        }

        if !file_exists {
            return ToolResult::err(format!(
                "Error: file not found: {path_str}. To create a new file, use empty old_string with the file content as new_string."
            ));
        }

        // 3. Read-before-edit
        let cached_state = ctx.file_state.lock().unwrap().get(&path_str);
        let Some(cached) = cached_state else {
            return ToolResult::err(format!(
                "Error: you must Read the file before editing it. Use the Read tool first to view {path_str}."
            ));
        };

        if cached.is_partial_view.unwrap_or(false) && !cached.content.contains(old_string) {
            let offset = cached.offset.unwrap_or(1);
            let limit = cached.limit.unwrap_or(2000);
            return ToolResult::err(format!(
                "Error: the file was only partially read (lines {offset}-{}). Read the full file or the relevant section before editing.",
                offset + limit - 1
            ));
        }

        // 4. Staleness check (>1s delta means external modification)
        let current_mtime = std::fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64);
        let Some(current_mtime) = current_mtime else {
            return ToolResult::err(format!("Error checking file: stat failed for {path_str}"));
        };
        if current_mtime > cached.timestamp + 1000.0 {
            return ToolResult::err(format!(
                "Error: file has been modified since you last read it (cached: {}, current: {}). Please Read the file again before editing.",
                iso_from_ms(cached.timestamp),
                iso_from_ms(current_mtime)
            ));
        }

        // 5. Read current content, exact match
        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("Error reading file: {e}")),
        };

        if !content.contains(old_string) {
            let trimmed = old_string.trim();
            if !trimmed.is_empty() && content.contains(trimmed) {
                return ToolResult::err(
                    "Error: exact match not found, but a match was found ignoring leading/trailing whitespace. Ensure old_string matches exactly, including whitespace and indentation.",
                );
            }
            return ToolResult::err(format!(
                "Error: old_string not found in {path_str}. Make sure the text matches exactly, including whitespace and line breaks."
            ));
        }

        // 6. Uniqueness
        let match_count = count_occurrences(&content, old_string);
        if match_count > 1 && !replace_all {
            return ToolResult::err(format!(
                "Error: found {match_count} matches for old_string. To replace all occurrences, set replace_all: true. Otherwise, provide more context in old_string to uniquely identify the target."
            ));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        if let Err(e) = atomic_write_sync(&file_path, &new_content) {
            return ToolResult::err(format!("Error writing file: {e}"));
        }

        ctx.modified_files.lock().unwrap().insert(path_str.clone());
        ctx.file_history.lock().unwrap().tracked_files.insert(path_str.clone());

        let new_mtime = std::fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or_else(now_ms);
        ctx.file_state.lock().unwrap().set(
            &path_str,
            FileState { content: new_content.clone(), timestamp: new_mtime, offset: None, limit: None, is_partial_view: None },
        );

        let diff = generate_diff_snippet(&content, &new_content, old_string, new_string);
        let replacements = if replace_all { match_count } else { 1 };
        let plural = if replacements > 1 { "s" } else { "" };
        ToolResult::ok(format!(
            "Edited {path_str} ({replacements} replacement{plural}):\n\n{diff}"
        ))
    }
}

/// ISO timestamp helper matching the TS error text format.
fn iso_from_ms(ms: f64) -> String {
    // Minimal ISO-8601 (UTC) without pulling chrono: seconds precision.
    let secs = (ms / 1000.0) as i64;
    let days = secs / 86400;
    let tod = secs.rem_euclid(86400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // Civil-from-days algorithm (Howard Hinnant)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}.000Z")
}

// ---------------------------------------------------------------------------
// Tests — ported from test/tools/edit.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn edit_input(file: &str, old: &str, new: &str) -> Json {
        serde_json::json!({"file_path": file, "old_string": old, "new_string": new})
    }

    async fn prime_read(dir: &std::path::Path, ctx: &ToolContext, file: &std::path::Path) {
        // Simulate a Read (populates the cache with fresh mtime)
        let content = tokio::fs::read_to_string(file).await.unwrap();
        let mtime = std::fs::metadata(file)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        ctx.file_state.lock().unwrap().set(
            &file.to_string_lossy(),
            FileState { content, timestamp: mtime, offset: None, limit: None, is_partial_view: None },
        );
        let _ = dir;
    }

    #[tokio::test]
    async fn basic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "hello world\n").unwrap();
        let ctx = test_ctx(dir.path());
        prime_read(dir.path(), &ctx, &f).await;

        let r = EditTool.call(edit_input(f.to_str().unwrap(), "world", "rust"), &ctx).await;
        assert!(!r.is_error(), "{}", r.result);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello rust\n");
        assert!(r.result.contains("Edited"));
        assert!(r.result.contains("@@"));
        assert!(r.result.contains("-hello world"));
        assert!(r.result.contains("+hello rust"));
    }

    #[tokio::test]
    async fn no_op_check() {
        let dir = tempfile::tempdir().unwrap();
        let r = EditTool.call(edit_input("f.txt", "same", "same"), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        assert!(r.result.contains("identical"));
    }

    #[tokio::test]
    async fn read_before_edit_required() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "content").unwrap();
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "content", "new"), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        assert!(r.result.contains("you must Read the file before editing"));
    }

    #[tokio::test]
    async fn staleness_check() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "content").unwrap();
        let ctx = test_ctx(dir.path());
        // Cache with an old timestamp (2+ seconds stale)
        ctx.file_state.lock().unwrap().set(
            &f.to_string_lossy(),
            FileState { content: "content".into(), timestamp: now_ms() - 5000.0, offset: None, limit: None, is_partial_view: None },
        );
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "content", "new"), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("modified since you last read it"));
    }

    #[tokio::test]
    async fn multiple_matches_require_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x\nx\nx\n").unwrap();
        let ctx = test_ctx(dir.path());
        prime_read(dir.path(), &ctx, &f).await;

        let r = EditTool.call(edit_input(f.to_str().unwrap(), "x", "y"), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("found 3 matches"));

        let mut input = edit_input(f.to_str().unwrap(), "x", "y");
        input["replace_all"] = Json::Bool(true);
        let r = EditTool.call(input, &ctx).await;
        assert!(!r.is_error(), "{}", r.result);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "y\ny\ny\n");
        assert!(r.result.contains("3 replacements"));
    }

    #[tokio::test]
    async fn create_new_file_with_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/new.txt");
        let ctx = test_ctx(dir.path());
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "", "line1\nline2"), &ctx).await;
        assert!(!r.is_error(), "{}", r.result);
        assert!(r.result.contains("Created new file"));
        assert!(r.result.contains("(2 lines)"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "line1\nline2");
    }

    #[tokio::test]
    async fn nonexistent_file_without_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let r = EditTool.call(edit_input("nope.txt", "a", "b"), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        assert!(r.result.contains("file not found"));
        assert!(r.result.contains("use empty old_string"));
    }

    #[tokio::test]
    async fn match_not_found_hint() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "  indented line\n").unwrap();
        let ctx = test_ctx(dir.path());
        prime_read(dir.path(), &ctx, &f).await;

        // trim-matching hint: exact match fails, trimmed match would succeed
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "  indented line \t", "x"), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("ignoring leading/trailing whitespace"));

        // total miss
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "zzz-not-here", "x"), &ctx).await;
        assert!(r.result.contains("old_string not found"));
    }

    #[tokio::test]
    async fn partial_view_restriction() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "l1\nl2\nl3\n").unwrap();
        let ctx = test_ctx(dir.path());
        ctx.file_state.lock().unwrap().set(
            &f.to_string_lossy(),
            FileState { content: "l1\n".into(), timestamp: now_ms(), offset: Some(1), limit: Some(1), is_partial_view: Some(true) },
        );
        let r = EditTool.call(edit_input(f.to_str().unwrap(), "l3", "x"), &ctx).await;
        assert!(r.is_error());
        assert!(r.result.contains("only partially read"));
    }

    #[tokio::test]
    async fn updates_cache_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "before\n").unwrap();
        let ctx = test_ctx(dir.path());
        prime_read(dir.path(), &ctx, &f).await;

        EditTool.call(edit_input(f.to_str().unwrap(), "before", "after"), &ctx).await;
        let state = ctx.file_state.lock().unwrap().get(f.to_str().unwrap()).unwrap();
        assert_eq!(state.content, "after\n");
        assert!(ctx.modified_files.lock().unwrap().contains(f.to_str().unwrap()));
        assert!(ctx.file_history.lock().unwrap().tracked_files.contains(f.to_str().unwrap()));
    }
}
