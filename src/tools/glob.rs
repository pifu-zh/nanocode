//! Glob tool — Rust port of `src/tools/glob.ts`.
//!
//! Fast file pattern matching with the same default ignore list, absolute
//! paths, alphabetical sort, and 200-result cap.

use async_trait::async_trait;
use serde_json::Value as Json;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

const MAX_RESULTS: usize = 200;

const DEFAULT_IGNORE: &[&str] = &[
    "node_modules", ".git", "dist", "build", ".next", ".nuxt", "coverage",
    "__pycache__", ".pytest_cache", "target", "vendor", ".venv", "venv",
    ".tox", ".DS_Store",
];

pub struct GlobTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Glob pattern to match files against (e.g., \"**/*.ts\", \"src/**/*.test.js\")."},
            "path": {"type": "string", "description": "Directory to search in. Default: working directory."}
        },
        "required": ["pattern"]
    })
}

fn is_ignored(relative: &std::path::Path) -> bool {
    for comp in relative.components() {
        let name = comp.as_os_str().to_string_lossy();
        if DEFAULT_IGNORE.contains(&name.as_ref()) {
            return true;
        }
    }
    false
}

/// Walk with the `glob` crate semantics; fast-glob `dot: false` ≈ skip hidden
/// segments unless the pattern asks for them explicitly.
fn glob_files(pattern: &str, root: &std::path::Path) -> Vec<PathBufSafe> {
    let full = root.join(pattern);
    let mut out = Vec::new();
    for entry in glob::glob(&full.to_string_lossy()).into_iter().flatten() {
        let Ok(path) = entry else { continue };
        // Only files
        if !path.is_file() {
            continue;
        }
        // Hidden-segment filter (dot: false). Keep if any pattern segment
        // explicitly starts with '.'.
        let pattern_hidden = pattern.split('/').any(|seg| seg.starts_with('.') && seg != "." && seg != "..");
        if !pattern_hidden {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let hidden = rel.components().any(|c| {
                let n = c.as_os_str().to_string_lossy();
                n.starts_with('.') && n != "." && n != ".."
            });
            if hidden {
                continue;
            }
        }
        if is_ignored(path.strip_prefix(root).unwrap_or(&path)) {
            continue;
        }
        out.push(PathBufSafe(path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.truncate(MAX_RESULTS);
    out
}

struct PathBufSafe(std::path::PathBuf);

#[async_trait]
impl ToolDef for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Find files by glob pattern. Returns matching file paths sorted alphabetically. Ignores node_modules, .git, dist, build by default."
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
        format!("Glob: {}", input.get("pattern").and_then(|v| v.as_str()).unwrap_or(""))
    }

    fn prompt(&self) -> String {
        [
            "Find files matching a glob pattern.",
            "",
            "Common patterns:",
            "  **/*.ts          All TypeScript files",
            "  src/**/*.test.js All test files in src/",
            "  **/package.json  All package.json files",
            "  *.{js,ts}        JS and TS files in current dir",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: pattern is required.");
        };
        let search_dir = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.cwd.join(p),
            None => ctx.cwd.as_ref().clone(),
        };

        let files = glob_files(pattern, &search_dir);

        if files.is_empty() {
            return ToolResult::ok(format!(
                "No files found matching pattern: {pattern} in {}",
                search_dir.display()
            ));
        }

        let mut output = files
            .iter()
            .map(|PathBufSafe(p)| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        if files.len() >= MAX_RESULTS {
            output.push_str(&format!(
                "\n\n(Results capped at {MAX_RESULTS}. Narrow your pattern for more specific results.)"
            ));
        }

        ToolResult::ok(output)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    fn make_tree(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("src/deep/b.rs"), "").unwrap();
        std::fs::write(dir.join("top.rs"), "").unwrap();
        std::fs::write(dir.join("node_modules/pkg/x.rs"), "").unwrap();
        std::fs::write(dir.join(".git/inner.rs"), "").unwrap();
        std::fs::write(dir.join(".hidden.rs"), "").unwrap();
    }

    #[tokio::test]
    async fn finds_files_sorted_absolute() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let r = GlobTool
            .call(serde_json::json!({"pattern": "**/*.rs"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.is_error());
        let lines: Vec<&str> = r.result.lines().collect();
        assert!(lines[0].ends_with("src/a.rs"), "{}", r.result);
        assert!(lines.iter().any(|l| l.ends_with("src/deep/b.rs")));
        assert!(lines.iter().any(|l| l.ends_with("top.rs")));
        // sorted
        let mut sorted = lines.to_vec();
        sorted.sort();
        assert_eq!(lines, sorted.as_slice());
    }

    #[tokio::test]
    async fn ignores_default_dirs_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let r = GlobTool
            .call(serde_json::json!({"pattern": "**/*.rs"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.result.contains("node_modules"));
        assert!(!r.result.contains(".git/"));
        assert!(!r.result.contains(".hidden.rs"));
    }

    #[tokio::test]
    async fn no_matches_message() {
        let dir = tempfile::tempdir().unwrap();
        let r = GlobTool
            .call(serde_json::json!({"pattern": "**/*.zzz"}), &test_ctx(dir.path()))
            .await;
        assert!(!r.is_error());
        assert!(r.result.contains("No files found matching pattern: **/*.zzz"));
    }

    #[tokio::test]
    async fn subdirectory_search() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let r = GlobTool
            .call(serde_json::json!({"pattern": "**/*.rs", "path": "src"}), &test_ctx(dir.path()))
            .await;
        assert!(r.result.contains("a.rs"));
        assert!(!r.result.contains("top.rs"));
    }
}
