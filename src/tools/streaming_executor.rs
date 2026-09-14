//! Streaming tool executor — Rust port of `src/core/streaming-executor.ts`.
//!
//! Partitions consecutive tool calls: consecutive concurrency-safe tools run
//! in parallel (max 10 in flight, results re-yielded in original order);
//! each unsafe tool runs serially as its own batch. Oversized results are
//! truncated; tool errors become isError results (never propagated).

use futures::Stream;

use crate::core::types::{AgentEvent, ToolContext, ToolResult, ToolUseBlock};
use std::sync::Arc;
use crate::tools::ToolDef;

const MAX_CONCURRENCY: usize = 10;

struct QueuedTool {
    block: ToolUseBlock,
    tool: crate::tools::Tool, // always resolvable (ErrorTool fallback)
    is_safe: bool,
}

struct ToolBatch {
    is_concurrency_safe: bool,
    items: Vec<QueuedTool>,
}

/// Partition consecutive safe tools into parallel batches; each unsafe tool
/// becomes its own serial batch (streaming-executor.ts partitionToolCalls).
fn partition_tool_calls(items: Vec<QueuedTool>) -> Vec<ToolBatch> {
    let mut batches: Vec<ToolBatch> = Vec::new();
    let mut current_safe: Vec<QueuedTool> = Vec::new();

    for item in items {
        if item.is_safe {
            current_safe.push(item);
        } else {
            if !current_safe.is_empty() {
                batches.push(ToolBatch { is_concurrency_safe: true, items: std::mem::take(&mut current_safe) });
            }
            batches.push(ToolBatch { is_concurrency_safe: false, items: vec![item] });
        }
    }
    if !current_safe.is_empty() {
        batches.push(ToolBatch { is_concurrency_safe: true, items: current_safe });
    }
    batches
}

fn truncate_result(tool: &dyn ToolDef, mut result: ToolResult) -> ToolResult {
    let limit = tool.max_result_size_chars();
    if result.result.len() > limit {
        let total = result.result.len();
        result.result.truncate(limit);
        result.result.push_str(&format!(
            "\n\n[Output truncated: was {total} chars, limit {limit}]"
        ));
    }
    result
}

async fn execute_single(tool: &crate::tools::Tool, block: &ToolUseBlock, ctx: &ToolContext) -> ToolResult {
    // Tool.call returns ToolResult directly (TS parity: tools never throw,
    // they return isError results). A panic here would abort the batch task —
    // tools are required to be panic-free per the trait contract.
    let result = tool.call(block.input.clone(), ctx).await;
    truncate_result(tool.as_ref(), result)
}

/// Execute tool-use blocks with concurrent/serial partitioning. Yields
/// tool_start then tool_result events (results in original order per batch).
pub fn execute_tools(
    tool_use_blocks: Vec<ToolUseBlock>,
    tools: Vec<crate::tools::Tool>,
    ctx: &ToolContext,
) -> impl Stream<Item = AgentEvent> {
    let ctx = ctx.clone();
    async_stream::stream! {
        // Build queue; unknown tools map to ErrorTool (serial, unsafe).
        let queue: Vec<QueuedTool> = tool_use_blocks
            .into_iter()
            .map(|block| {
                let tool = tools
                    .iter()
                    .find(|t| t.name() == block.name)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(crate::tools::ErrorTool { tool_name: block.name.clone() }));
                let is_safe = tool.is_concurrency_safe(&block.input);
                QueuedTool { block, tool, is_safe }
            })
            .collect();

        let batches = partition_tool_calls(queue);

        for batch in batches {
            if batch.is_concurrency_safe {
                // Parallel batch: yield all starts, then run with a concurrency
                // cap of 10, collecting results in original order.
                let items: Vec<(crate::tools::Tool, ToolUseBlock)> = batch
                    .items
                    .iter()
                    .map(|i| (i.tool.clone(), i.block.clone()))
                    .collect();

                for (_, block) in &items {
                    yield AgentEvent::ToolStart {
                        tool_use_id: block.id.clone(),
                        tool_name: block.name.clone(),
                        input: block.input.clone(),
                    };
                }

                // Semaphore cap (TS approximated this with a buggy
                // Promise.race drain; observable behavior — max 10 in flight,
                // results in original order — is preserved exactly).
                let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
                let futs: Vec<_> = items
                    .into_iter()
                    .map(|(tool, block)| {
                        let ctx = ctx.clone();
                        let sem = sem.clone();
                        async move {
                            let _permit = sem.acquire().await;
                            let result = execute_single(&tool, &block, &ctx).await;
                            (block, result)
                        }
                    })
                    .collect();
                let completed = futures::future::join_all(futs).await;

                for (item, (block, result)) in batch.items.iter().zip(completed) {
                    let _ = block;
                    let is_error = result.is_error();
                    yield AgentEvent::ToolResult {
                        tool_use_id: item.block.id.clone(),
                        tool_name: item.block.name.clone(),
                        result: result.result,
                        is_error,
                    };
                }
            } else {
                // Serial batch (single item).
                let item = &batch.items[0];
                yield AgentEvent::ToolStart {
                    tool_use_id: item.block.id.clone(),
                    tool_name: item.block.name.clone(),
                    input: item.block.input.clone(),
                };
                let result = execute_single(&item.tool, &item.block, &ctx).await;
                let is_error = result.is_error();
                yield AgentEvent::ToolResult {
                    tool_use_id: item.block.id.clone(),
                    tool_name: item.block.name.clone(),
                    result: result.result,
                    is_error,
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/core/streaming-executor.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::tools::Tool;
    use futures::StreamExt;
    use serde_json::Value as Json;
    use super::*;
    use crate::core::types::PermissionMode;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct AllowGate;
    #[async_trait::async_trait]
    impl crate::core::types::PermissionGate for AllowGate {
        async fn decide(
            &self,
            _t: &str,
            _i: &Json,
            _m: &str,
        ) -> crate::core::types::PermissionDecision {
            crate::core::types::PermissionDecision::allow()
        }
    }

    struct TestTool {
        name: String,
        safe: bool,
        delay_ms: u64,
        result: String,
        is_error: bool,
        calls: Option<Arc<AtomicU32>>,
        max: usize,
    }

    #[async_trait::async_trait]
    impl ToolDef for TestTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self, _: Option<&Json>) -> String {
            "test".into()
        }
        fn input_schema(&self) -> Json {
            serde_json::json!({"type": "object"})
        }
        async fn call(&self, _i: Json, _c: &ToolContext) -> ToolResult {
            if let Some(c) = &self.calls {
                c.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            if self.is_error {
                // Simulate a tool that returns an error result (not a panic)
                ToolResult::err(self.result.clone())
            } else {
                ToolResult::ok(self.result.clone())
            }
        }
        fn is_concurrency_safe(&self, _i: &Json) -> bool {
            self.safe
        }
        fn is_read_only(&self, _i: &Json) -> bool {
            self.safe
        }
        fn max_result_size_chars(&self) -> usize {
            self.max
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: Arc::new(std::path::PathBuf::from("/tmp")),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(Default::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: "s".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            permission_mode: Arc::new(std::sync::RwLock::new(PermissionMode::Default)),
            permission_gate: Arc::new(AllowGate),
            sub_agent: Arc::new(Mutex::new(None)),
        }
    }

    fn block(id: &str, name: &str) -> ToolUseBlock {
        ToolUseBlock { id: id.into(), name: name.into(), input: serde_json::json!({}) }
    }

    async fn run(tools: Vec<Tool>, blocks: Vec<ToolUseBlock>) -> Vec<AgentEvent> {
        let c = ctx();
        execute_tools(blocks, tools, &c).collect().await
    }

    #[tokio::test]
    async fn consecutive_safe_tools_run_in_one_parallel_batch() {
        // Fast tool + slow tool: total wall time ≈ slow, not slow + fast.
        let tools: Vec<Tool> = vec![
            Arc::new(TestTool { name: "A".into(), safe: true, delay_ms: 150, result: "a".into(), is_error: false, calls: None, max: 30_000 }),
            Arc::new(TestTool { name: "B".into(), safe: true, delay_ms: 150, result: "b".into(), is_error: false, calls: None, max: 30_000 }),
        ];
        let started = std::time::Instant::now();
        let events = run(tools, vec![block("1", "A"), block("2", "B")]).await;
        assert!(started.elapsed() < Duration::from_millis(280), "should run concurrently");

        let starts: Vec<&str> = events.iter().filter_map(|e| match e {
            AgentEvent::ToolStart { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        }).collect();
        assert_eq!(starts, vec!["A", "B"]);

        let results: Vec<&str> = events.iter().filter_map(|e| match e {
            AgentEvent::ToolResult { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        }).collect();
        assert_eq!(results, vec!["A", "B"]); // original order preserved
    }

    #[tokio::test]
    async fn unsafe_tool_splits_batches() {
        // [safe A, unsafe W, safe C]: A runs, then W serial, then C.
        let tools: Vec<Tool> = vec![
            Arc::new(TestTool { name: "A".into(), safe: true, delay_ms: 50, result: "a".into(), is_error: false, calls: None, max: 30_000 }),
            Arc::new(TestTool { name: "W".into(), safe: false, delay_ms: 50, result: "w".into(), is_error: false, calls: None, max: 30_000 }),
            Arc::new(TestTool { name: "C".into(), safe: true, delay_ms: 50, result: "c".into(), is_error: false, calls: None, max: 30_000 }),
        ];
        let events = run(tools, vec![block("1", "A"), block("2", "W"), block("3", "C")]).await;
        let order: Vec<&str> = events.iter().filter_map(|e| match e {
            AgentEvent::ToolStart { tool_name, .. } => Some(tool_name.as_str()),
            AgentEvent::ToolResult { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        }).collect();
        // start A, start W after A's batch, etc. — serial interleaving guaranteed:
        // A.start, A.result, W.start, W.result, C.start, C.result
        assert_eq!(order, vec!["A", "A", "W", "W", "C", "C"]);
    }

    #[tokio::test]
    async fn error_result_flagged() {
        let tools: Vec<Tool> = vec![
            Arc::new(TestTool { name: "E".into(), safe: true, delay_ms: 1, result: "bad thing".into(), is_error: true, calls: None, max: 30_000 }),
        ];
        let events = run(tools, vec![block("1", "E")]).await;
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(result.1);
        assert_eq!(result.0, "bad thing");
    }

    #[tokio::test]
    async fn unknown_tool_gets_error_result() {
        let events = run(vec![], vec![block("1", "Mystery")]).await;
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, is_error, .. } => Some((result.clone(), *is_error)),
            _ => None,
        }).unwrap();
        assert!(result.1);
        assert!(result.0.contains("Unknown tool: Mystery"));
    }

    #[tokio::test]
    async fn oversized_result_truncated() {
        let tools: Vec<Tool> = vec![
            Arc::new(TestTool { name: "Big".into(), safe: true, delay_ms: 1, result: "x".repeat(5000), is_error: false, calls: None, max: 100 }),
        ];
        let events = run(tools, vec![block("1", "Big")]).await;
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { result, .. } => Some(result.clone()),
            _ => None,
        }).unwrap();
        assert!(result.contains("[Output truncated: was 5000 chars, limit 100]"));
        assert!(result.len() < 200);
    }

    #[tokio::test]
    async fn concurrency_capped_at_10() {
        // 20 tools each counting concurrent entries; peak must be ≤ 10.
        struct Counting { active: Arc<Mutex<usize>>, peak: Arc<AtomicU32> }
        #[async_trait::async_trait]
        impl ToolDef for Counting {
            fn name(&self) -> &str { "C" }
            fn description(&self, _: Option<&Json>) -> String { String::new() }
            fn input_schema(&self) -> Json { serde_json::json!({}) }
            async fn call(&self, _: Json, _: &ToolContext) -> ToolResult {
                let now = {
                    let mut a = self.active.lock().unwrap();
                    *a += 1;
                    let n = *a;
                    if n as u32 > self.peak.load(Ordering::SeqCst) {
                        self.peak.store(n as u32, Ordering::SeqCst);
                    }
                    n
                };
                let _ = now;
                tokio::time::sleep(Duration::from_millis(30)).await;
                let mut a = self.active.lock().unwrap();
                *a -= 1;
                ToolResult::ok("done")
            }
            fn is_concurrency_safe(&self, _i: &Json) -> bool { true }
        }
        let active = Arc::new(Mutex::new(0));
        let peak = Arc::new(AtomicU32::new(0));
        let tools: Vec<Tool> = vec![Arc::new(Counting { active: active.clone(), peak: peak.clone() })];
        let blocks: Vec<ToolUseBlock> = (0..20).map(|i| block(&i.to_string(), "C")).collect();
        let _ = run(tools, blocks).await;
        assert!(peak.load(Ordering::SeqCst) <= 10, "peak was {}", peak.load(Ordering::SeqCst));
        assert!(peak.load(Ordering::SeqCst) > 1, "should actually parallelize");
    }
}
