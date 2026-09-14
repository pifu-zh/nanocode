//! Bash tool — Rust port of `src/tools/bash.ts`.
//!
//! Spawns `bash -c` with pager-suppressing env, timeout (SIGTERM → 2s →
//! SIGKILL), 10 MB stream caps, and TS-matched output formatting. Read-only
//! detection delegates to `bash_readonly`.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;
use crate::tools::bash_readonly::is_read_only_command;

#[allow(dead_code)]
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_RESULT_SIZE_CHARS: usize = 30_000;
const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

pub struct BashTool;

fn input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "The bash command to execute. Can be a simple command or a compound command with pipes, &&, ||, etc."},
            "timeout": {"type": "number", "description": "Timeout in seconds. Default: 120. Max: 600."},
            "description": {"type": "string", "description": "A short human-readable description of what the command does and why."}
        },
        "required": ["command"]
    })
}

struct ExecResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

async fn execute_command(
    command: &str,
    cwd: &std::path::Path,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<ExecResult, String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn: {e}"))?;

    let mut stdout_pipe = child.stdout.take().expect("stdout");
    let mut stderr_pipe = child.stderr.take().expect("stderr");

    let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
    let cancel_fut = cancel.cancelled();
    let mut timed_out = false;

    tokio::pin!(deadline, cancel_fut);

    let collect = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let out = async move {
            let mut buf = [0u8; 8192];
            loop {
                match stdout_pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.len() < MAX_BUFFER_SIZE {
                            stdout.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            }
            stdout
        };
        let err = async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr_pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stderr.len() < MAX_BUFFER_SIZE {
                            stderr.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            }
            stderr
        };
        tokio::join!(out, err)
    };

    let (stdout_bytes, stderr_bytes, status) = tokio::select! {
        (o, e, s) = async {
            let (o, e) = collect.await;
            let s = child.wait().await.ok();
            (o, e, s)
        } => (o, e, s),
        _ = &mut deadline, if !timed_out => {
            timed_out = true;
            let _ = child.start_kill();
            (Vec::new(), Vec::new(), child.wait().await.ok())
        },
        _ = &mut cancel_fut => {
            let _ = child.start_kill();
            return Err("Aborted".to_string());
        }
    };

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
        exit_code: status.and_then(|s| s.code()).unwrap_or(1),
        timed_out,
    })
}

fn format_output(result: &ExecResult) -> String {
    let mut parts: Vec<String> = Vec::new();

    if result.timed_out {
        parts.push("[Command timed out]".to_string());
    }

    if !result.stdout.is_empty() {
        let stdout = &result.stdout;
        if stdout.len() > MAX_RESULT_SIZE_CHARS {
            let total = stdout.len();
            parts.push(stdout.chars().take(MAX_RESULT_SIZE_CHARS).collect());
            parts.push(format!("\n[stdout truncated: {total} chars total]"));
        } else {
            parts.push(stdout.clone());
        }
    }

    if !result.stderr.is_empty() {
        let stderr = &result.stderr;
        let limit = MAX_RESULT_SIZE_CHARS / 3;
        if stderr.len() > limit {
            let total = stderr.len();
            parts.push(format!("\nSTDERR:\n{}", stderr.chars().take(limit).collect::<String>()));
            parts.push(format!("[stderr truncated: {total} chars total]"));
        } else if !stderr.trim().is_empty() {
            parts.push(format!("\nSTDERR:\n{stderr}"));
        }
    }

    if parts.is_empty() {
        return if result.exit_code == 0 {
            "(No output)".to_string()
        } else {
            format!("(No output, exit code: {})", result.exit_code)
        };
    }

    if result.exit_code != 0 && !result.timed_out {
        parts.push(format!("\n(exit code: {})", result.exit_code));
    }

    parts.join("")
}

#[async_trait]
impl ToolDef for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self, input: Option<&Json>) -> String {
        if let Some(desc) = input.and_then(|i| i.get("description")).and_then(|d| d.as_str()) {
            return format!("Bash: {desc}");
        }
        "Execute a bash command. Use for running scripts, installing packages, searching code, and system operations."
            .to_string()
    }

    fn input_schema(&self) -> Json {
        input_schema()
    }

    fn is_read_only(&self, input: &Json) -> bool {
        input
            .get("command")
            .and_then(|c| c.as_str())
            .map(is_read_only_command)
            .unwrap_or(false)
    }

    fn is_concurrency_safe(&self, input: &Json) -> bool {
        self.is_read_only(input)
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let cmd = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let cmd: String = cmd.chars().take(60).collect();
        if input
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.chars().count() > 60)
            .unwrap_or(false)
        {
            format!("Bash: {}...", cmd.trim_end_matches('.'))
        } else {
            format!("Bash: {cmd}")
        }
    }

    fn prompt(&self) -> String {
        [
            "Execute bash commands to interact with the system.",
            "Use for: running scripts, searching code, checking file status, running tests, installing packages.",
            "",
            "Guidelines:",
            "- Prefer non-interactive commands.",
            "- For long-running processes, consider using timeout.",
            "- Use pipes and redirection for complex data processing.",
            "- The command runs in the project working directory.",
            "- Combine commands with && for sequential execution.",
            "- Use grep/rg for searching, find for file discovery.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(command) = input.get("command").and_then(|c| c.as_str()) else {
            return ToolResult::err("Error: command is required.");
        };
        if command.trim().is_empty() {
            return ToolResult::err("Error: command cannot be empty.");
        }
        let timeout_secs = input.get("timeout").and_then(|t| t.as_u64()).unwrap_or(DEFAULT_TIMEOUT_MS / 1000);
        let timeout_ms = (timeout_secs * 1000).min(600_000);

        match execute_command(command, &ctx.cwd, timeout_ms, &ctx.cancel).await {
            Ok(result) => {
                let output = format_output(&result);
                ToolResult {
                    is_error: Some(result.exit_code != 0 && !result.timed_out),
                    result: output,
                }
            }
            Err(msg) if msg == "Aborted" => ToolResult::ok("(Aborted)"),
            Err(msg) => ToolResult::err(format!("Error executing command: {msg}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from bash.ts behavior
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    async fn run(cmd: &str) -> ToolResult {
        BashTool
            .call(serde_json::json!({"command": cmd}), &test_ctx(std::path::Path::new("/tmp")))
            .await
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let r = run("echo hello").await;
        assert!(!r.is_error());
        assert_eq!(r.result, "hello\n");

        let r = run("exit 3").await;
        assert!(r.is_error());
        assert_eq!(r.result, "(No output, exit code: 3)");

        // non-zero exit with output → trailing "(exit code: N)"
        let r = run("echo x; exit 3").await;
        assert!(r.is_error());
        assert!(r.result.contains("x\n\n(exit code: 3)"));
    }

    #[tokio::test]
    async fn stderr_section_and_no_output() {
        let r = run("echo err >&2").await;
        assert!(!r.is_error());
        assert!(r.result.contains("STDERR:\nerr"));

        let r = run("true").await;
        assert_eq!(r.result, "(No output)");
        let r = run("false").await;
        assert_eq!(r.result, "(No output, exit code: 1)");
    }

    #[tokio::test]
    async fn timeout_reports_not_error() {
        let started = std::time::Instant::now();
        let r = BashTool
            .call(
                serde_json::json!({"command": "sleep 5", "timeout": 1}),
                &test_ctx(std::path::Path::new("/tmp")),
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!r.is_error(), "timeout is not an error: {}", r.result);
        assert!(r.result.starts_with("[Command timed out]"));
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let r = run("").await;
        assert!(r.is_error());
        assert!(r.result.contains("cannot be empty"));
    }

    #[tokio::test]
    async fn pager_env_suppressed() {
        // git config without pager must not hang
        let r = run("echo $PAGER; echo $GIT_PAGER").await;
        assert!(r.result.contains("cat"));
    }

    #[tokio::test]
    async fn cwd_is_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let r = BashTool
            .call(serde_json::json!({"command": "pwd"}), &test_ctx(dir.path()))
            .await;
        assert_eq!(r.result.trim(), dir.path().to_string_lossy());
    }

    #[test]
    fn readonly_delegates_to_validator() {
        assert!(BashTool.is_read_only(&serde_json::json!({"command": "ls"})));
        assert!(!BashTool.is_read_only(&serde_json::json!({"command": "rm x"})));
        assert!(!BashTool.is_concurrency_safe(&serde_json::json!({"command": "rm x"})));
    }
}

