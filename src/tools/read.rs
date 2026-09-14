//! Read tool — Rust port of `src/tools/read.ts`.
//!
//! Reads files with `cat -n` style line numbers, offset/limit, binary and
//! image detection, UTF-16LE BOM handling; caches file state for the
//! read-before-edit chain.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::path::{Path, PathBuf};

use crate::core::types::{FileState, ToolContext, ToolResult};
use crate::tools::ToolDef;

const DEFAULT_LIMIT: u64 = 2000;
const MAX_RESULT_SIZE_CHARS: usize = 60_000;
const BINARY_CHECK_SIZE: usize = 8192;

const IMAGE_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".svg", ".webp", ".tiff",
    ".tif", ".psd", ".raw", ".heif", ".heic",
];

pub struct ReadTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "Absolute path to the file to read. Relative paths will be resolved against the working directory."
            },
            "offset": {
                "type": "number",
                "description": "Line number to start reading from (1-based). Default: 1."
            },
            "limit": {
                "type": "number",
                "description": "Maximum number of lines to read. Default: 2000."
            }
        },
        "required": ["file_path"]
    })
}

/// Resolve the file path from `file_path`/`path`/`filePath` (TS宽容别名).
fn extract_path(input: &Json) -> Option<String> {
    ["file_path", "path", "filePath"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
}

fn is_binary(buffer: &[u8]) -> bool {
    let check = buffer.len().min(BINARY_CHECK_SIZE);
    buffer[..check].contains(&0)
}

fn decode(buffer: &[u8]) -> String {
    // UTF-16LE BOM → decode; UTF-8 BOM → skip; else assume UTF-8.
    if buffer.len() >= 2 && buffer[0] == 0xFF && buffer[1] == 0xFE {
        let (decoded, _, _) = encoding_rs::UTF_16LE.decode(&buffer[2..]);
        return decoded.into_owned();
    }
    let start = if buffer.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    String::from_utf8_lossy(&buffer[start..]).into_owned()
}

fn format_with_line_numbers(lines: &[&str], start_line: u64) -> String {
    let max_line_num = start_line + lines.len() as u64 - 1;
    let width = max_line_num.to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}\t{line}", start_line + i as u64, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn mtime_ms(path: &Path) -> f64 {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or_else(now_ms)
}

pub(crate) fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[async_trait]
impl ToolDef for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Read a file and display its contents with line numbers. Supports text files with various encodings."
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

    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }

    fn user_facing_name(&self, input: &Json) -> String {
        format!("Read: {}", extract_path(input).unwrap_or_default())
    }

    fn prompt(&self) -> String {
        [
            "Read files to understand code, configurations, and data.",
            "",
            "Guidelines:",
            "- Always read a file before editing it.",
            "- Use offset and limit for large files to read specific sections.",
            "- The default limit is 2000 lines.",
            "- Line numbers are shown in cat -n format.",
            "- For binary files, use Bash with xxd or strings instead.",
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

        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_LIMIT);

        let file_path: PathBuf = if raw_path.starts_with('/') {
            PathBuf::from(&raw_path)
        } else {
            ctx.cwd.join(&raw_path)
        };

        // Image files — not supported in text mode
        let ext = file_path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            return ToolResult::ok(format!(
                "This is an image file ({ext}). Image viewing is not supported in text mode. Use a separate image viewer or the Bash tool with an appropriate command."
            ));
        }

        // Read file with TS-matched error messages
        let buffer = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(e) => {
                return ToolResult::err(match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        format!("Error: file not found: {}", file_path.display())
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Error: permission denied: {}", file_path.display())
                    }
                    _ if e.to_string().contains("Is a directory") => format!(
                        "Error: {} is a directory, not a file. Use ls or find to list directory contents.",
                        file_path.display()
                    ),
                    _ => format!("Error reading file: {e}"),
                });
            }
        };

        if is_binary(&buffer) {
            let size = buffer.len();
            return ToolResult::ok(format!(
                "This is a binary file ({size} bytes). Use xxd, od, or strings to inspect binary content."
            ));
        }

        let content = decode(&buffer);
        let all_lines: Vec<&str> = content.split('\n').collect();
        let total_lines = all_lines.len() as u64;

        let start_idx = (offset - 1) as usize;
        let end_idx = ((start_idx as u64) + limit).min(total_lines) as usize;
        let selected: Vec<&str> = all_lines
            .get(start_idx..end_idx)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let is_partial_view = start_idx > 0 || (end_idx as u64) < total_lines;

        let mut output = format_with_line_numbers(&selected, offset);

        if is_partial_view {
            let mut meta: Vec<String> = Vec::new();
            if start_idx > 0 {
                meta.push(format!("(showing from line {offset})"));
            }
            if (end_idx as u64) < total_lines {
                meta.push(format!(
                    "({} more lines below, {} total)",
                    total_lines - end_idx as u64,
                    total_lines
                ));
            }
            if !meta.is_empty() {
                output.push('\n');
                output.push_str(&meta.join(" "));
            }
        }

        // Update readFileState cache for the edit chain
        let timestamp = mtime_ms(&file_path).await;
        ctx.file_state.lock().unwrap().set(
            &file_path.to_string_lossy(),
            FileState {
                content,
                timestamp,
                offset: Some(offset),
                limit: Some(limit),
                is_partial_view: Some(is_partial_view),
            },
        );

        ToolResult::ok(output)
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/tools/read.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn read_input(path: &str) -> Json {
        serde_json::json!({ "file_path": path })
    }

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "line1\nline2\nline3\n").unwrap();
        let result = ReadTool.call(read_input(f.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(!result.is_error());
        assert!(result.result.contains("1\tline1"));
        assert!(result.result.contains("3\tline3"));
    }

    #[tokio::test]
    async fn relative_path_resolved_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "hi").unwrap();
        let result = ReadTool.call(read_input("x.txt"), &test_ctx(dir.path())).await;
        assert!(!result.is_error());
        assert!(result.result.contains("1\thi"));
    }

    #[tokio::test]
    async fn error_cases_have_ts_messages() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");

        let r = ReadTool.call(read_input(missing.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        assert!(r.result.starts_with("Error: file not found:"));

        let d = dir.path().join("subdir");
        std::fs::create_dir(&d).unwrap();
        let r = ReadTool.call(read_input(d.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        assert!(r.result.contains("is a directory, not a file"));
    }

    #[tokio::test]
    async fn empty_file_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = ReadTool.call(serde_json::json!({"file_path": ""}), &test_ctx(dir.path())).await;
        assert!(r.is_error());
        let r = ReadTool.call(serde_json::json!({}), &test_ctx(dir.path())).await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("n.txt");
        let content: String = (1..=10).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&f, &content).unwrap();

        let r = ReadTool.call(
            serde_json::json!({"file_path": f.to_str().unwrap(), "offset": 3, "limit": 2}),
            &test_ctx(dir.path()),
        )
        .await;
        assert!(r.result.contains("3\tL3"));
        assert!(r.result.contains("4\tL4"));
        assert!(!r.result.contains("5\tL5"));
        assert!(r.result.contains("7 more lines below, 11 total"));
    }

    #[tokio::test]
    async fn binary_detection() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bin.dat");
        std::fs::write(&f, b"ok\x00binary").unwrap();
        let r = ReadTool.call(read_input(f.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(!r.is_error());
        assert!(r.result.contains("This is a binary file ("));
    }

    #[tokio::test]
    async fn image_extension_hint() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("img.png");
        std::fs::write(&f, b"pretend").unwrap();
        let r = ReadTool.call(read_input(f.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(!r.is_error());
        assert!(r.result.contains("This is an image file (.png)"));
    }

    #[tokio::test]
    async fn updates_file_state_cache() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.txt");
        std::fs::write(&f, "abc").unwrap();
        let ctx = test_ctx(dir.path());
        ReadTool.call(read_input(f.to_str().unwrap()), &ctx).await;
        assert!(ctx.file_state.lock().unwrap().has(f.to_str().unwrap()));
        // partial view flag
        let state = ctx
            .file_state
            .lock()
            .unwrap()
            .get(f.to_str().unwrap())
            .unwrap();
        assert_eq!(state.is_partial_view, Some(false));
    }

    #[tokio::test]
    async fn utf16_with_nul_bytes_treated_as_binary() {
        // TS behavior: the null-byte binary check runs BEFORE decoding, so
        // UTF-16 text (ASCII half-bytes are NUL) reports as binary.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("u.txt");
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
        for unit in "héllo\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&f, &bytes).unwrap();
        let r = ReadTool.call(read_input(f.to_str().unwrap()), &test_ctx(dir.path())).await;
        assert!(r.result.contains("This is a binary file ("), "{}", r.result);
    }
}
