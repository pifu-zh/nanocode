//! Grep tool — Rust port of `src/tools/grep.ts`.
//!
//! Content search: ripgrep when available (with per-file/column caps and the
//! TS exclude list), falling back to `grep -rn`. Results in
//! `file:line:content` format, 500-line cap.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::path::PathBuf;
use tokio::process::Command;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

const MAX_RESULTS: usize = 500;
const SEARCH_TIMEOUT_MS: u64 = 30_000;

const EXCLUDE_DIRS: &[&str] = &[
    "node_modules", ".git", "dist", "build", ".next", ".nuxt", "coverage",
    "__pycache__", ".pytest_cache", "target", "vendor", ".venv", "venv", ".tox",
];

pub struct GrepTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Search pattern (regex supported with ripgrep, basic regex with grep)."},
            "path": {"type": "string", "description": "Directory or file to search in. Default: working directory."},
            "include": {"type": "string", "description": "File pattern to include (e.g., \"*.ts\", \"*.py\"). Only search matching files."}
        },
        "required": ["pattern"]
    })
}

/// Run ripgrep; returns Ok(Some(output)) on hits, Ok(Some("")) when exit 1
/// (no matches), Ok(None) when rg is unusable (fallback to grep).
async fn search_with_ripgrep(pattern: &str, search_path: &str, include: Option<&str>) -> Option<String> {
    let mut args: Vec<String> = vec![
        "--line-number".into(),
        "--no-heading".into(),
        "--color=never".into(),
        "--max-count=50".into(),
        "--max-columns=200".into(),
        "--max-columns-preview".into(),
    ];
    for dir in EXCLUDE_DIRS {
        args.push(format!("--glob=!{dir}"));
    }
    if let Some(inc) = include {
        args.push(format!("--glob={inc}"));
    }
    args.push("--".into());
    args.push(pattern.into());
    args.push(search_path.into());

    let output = Command::new("rg")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output();

    let output = tokio::time::timeout(
        std::time::Duration::from_millis(SEARCH_TIMEOUT_MS),
        output,
    )
    .await
    .ok()?
    .ok()?; // spawn failure → fallback

    match output.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&output.stdout).to_string()),
        Some(1) => Some(String::new()), // no matches
        _ => None,                      // error → fallback
    }
}

async fn search_with_grep(pattern: &str, search_path: &str, include: Option<&str>) -> String {
    let mut cmd = String::from("grep -rn --color=never");
    for dir in EXCLUDE_DIRS {
        cmd.push_str(&format!(" --exclude-dir={dir}"));
    }
    if let Some(inc) = include {
        cmd.push_str(&format!(" --include=\"{inc}\""));
    }
    let escaped = pattern.replace('"', "\\\"");
    cmd.push_str(&format!(" -- \"{escaped}\" \"{search_path}\" 2>/dev/null"));
    cmd.push_str(&format!(" | head -{MAX_RESULTS}"));

    let output = Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output();

    match tokio::time::timeout(std::time::Duration::from_millis(SEARCH_TIMEOUT_MS), output).await {
        Ok(Ok(out)) => {
            if out.status.code() == Some(1) {
                String::new() // no matches
            } else {
                String::from_utf8_lossy(&out.stdout).to_string()
            }
        }
        _ => String::new(),
    }
}

#[async_trait]
impl ToolDef for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Search file contents for a pattern. Uses ripgrep (rg) for speed, falls back to grep. Returns file:line:content format."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        true
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        format!("Grep: {pattern} {path}")
    }

    fn prompt(&self) -> String {
        [
            "Search file contents for a pattern (regex supported).",
            "",
            "Guidelines:",
            "- Uses ripgrep (rg) for fast search, falls back to grep.",
            "- Results are in file:line:content format.",
            "- Use include to filter by file type (e.g., \"*.ts\").",
            "- Regex patterns are supported.",
            "- Max 500 results returned.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: search pattern cannot be empty.");
        };
        if pattern.trim().is_empty() {
            return ToolResult::err("Error: search pattern cannot be empty.");
        }
        let include = input.get("include").and_then(|v| v.as_str());
        let search_path: PathBuf = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.cwd.join(p),
            None => ctx.cwd.as_ref().clone(),
        };

        let output = match search_with_ripgrep(pattern, &search_path.to_string_lossy(), include).await {
            Some(out) => out,
            None => search_with_grep(pattern, &search_path.to_string_lossy(), include).await,
        };

        if output.trim().is_empty() {
            let include_note = include.map(|i| format!(" in {i} files")).unwrap_or_default();
            return ToolResult::ok(format!(
                "No matches found for pattern: {pattern}{include_note}"
            ));
        }

        let lines: Vec<&str> = output.trim().split('\n').collect();
        let mut result;
        if lines.len() > MAX_RESULTS {
            result = lines[..MAX_RESULTS].join("\n");
            result.push_str(&format!(
                "\n\n({} total matches, showing first {MAX_RESULTS}. Narrow your search for more specific results.)",
                lines.len()
            ));
        } else {
            result = lines.join("\n");
            result.push_str(&format!("\n\n({} matches)", lines.len()));
        }

        ToolResult::ok(result)
    }
}

// ---------------------------------------------------------------------------
// Tests (require rg or grep in the environment — CI Linux has both)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    async fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\nsecond line\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn main() { println!(\"hello\"); }\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/c.js"), "hello from vendor\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matches_with_count() {
        let dir = make_tree().await;
        let r = GrepTool
            .call(serde_json::json!({"pattern": "hello"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.is_error(), "{}", r.result);
        assert!(r.result.contains("a.txt:1:hello world"));
        assert!(r.result.contains("(2 matches)"));
        assert!(!r.result.contains("vendor")); // excluded dir
    }

    #[tokio::test]
    async fn include_filter() {
        let dir = make_tree().await;
        let r = GrepTool
            .call(serde_json::json!({"pattern": "hello", "include": "*.rs"}), &test_ctx(dir.path()))
            .await;
        assert!(r.result.contains("b.rs"));
        assert!(!r.result.contains("a.txt"));
    }

    #[tokio::test]
    async fn no_matches_message() {
        let dir = make_tree().await;
        let r = GrepTool
            .call(serde_json::json!({"pattern": "zzzz-not-there"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.is_error());
        assert!(r.result.contains("No matches found for pattern: zzzz-not-there"));
    }

    #[tokio::test]
    async fn empty_pattern_rejected() {
        let dir = make_tree().await;
        let r = GrepTool
            .call(serde_json::json!({"pattern": "  "}), &test_ctx(dir.path()))
            .await;
        assert!(r.is_error());
        assert!(r.result.contains("cannot be empty"));
    }
}
