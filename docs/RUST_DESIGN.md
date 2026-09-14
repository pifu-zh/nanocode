# RUST_DESIGN.md — Rust Agent Runtime 目标设计（Phase 0 产出）

> 原则：idiomatic Rust ≠ 把 TS 类型机械映射为 struct（任务书非目标 #7）。
> 本设计回答：TS 的动态性在 Rust 中落在哪个语言设施上，行为规格（BEHAVIOR.md）如何不被结构牺牲。

---

## 1. 核心设计决策

| # | 决策 | 理由 |
|---|---|---|
| 1 | **单 crate `nanocode`**，模块树 = 分层（`core/ tools/ context/ prompt/ skills/ mcp/ files/ permissions/ cli/`），用 `pub(crate)` 纪律约束边界 | 9K LOC 规模；module 树即 ARCHITECTURE 分层；未来拆 workspace 的接缝已在模块边界 |
| 2 | **异步运行时 tokio（full features）**；终端 I/O 同步即可的部分用 std，不强行 async | 子进程/MCP/SSE 全是 async 天然场景；REPL 渲染是同步串行 |
| 3 | **Agent Loop = struct 状态机 + `Stream`**，不是 trait object 生成器 | TS async generator 的 `yield` 语义由 `async_stream::stream!` 宏承载（闭包内可变借用 messages）；`runSubAgent` 复用同一实现 |
| 4 | **工具输入不建 zod 镜像**：每个工具入参是 `serde_json::Value`，工具内部 `serde_path_to_error` 反序列化为强类型 struct；`input_schema()` 返回该 struct 的 schemars JSON Schema（openApi3 兼容结构） | 校验错误转为 TS 同风格的工具 isError 消息，而不是请求前拒绝——保持「schema 由 provider 端约束、运行端容错」的行为 |
| 5 | **Abort 用 `tokio_util::sync::CancellationToken`** 贯穿（对应 AbortSignal） | 与 tokio select!/timeout 天然组合；child process kill（SIGTERM→2s→SIGKILL）包装成 helper |
| 6 | **错误体系 `thiserror` 枚举** + `classify()` 保地位，不引入 anyhow 到库层 | errors.ts 的分类与重试语义是行为；CLI 边界才允许 source 链打印 |
| 7 | **模块级单例一律消灭**：Todo taskStore、plan-mode `_previousMode`、Agent 工具 `_queryParamsRef`、sessionRules、git cache、MCP activeClients → 全部收进 `Session`/`AppContext` 持有的具名状态 | Rust 全局可变状态即竞争源；这些单例在 TS 里都是"每进程一份"，Rust 版等价物是"每 Session 一份"，语义更严格但对外行为不变 |
| 8 | **序列化字段名手写 `#[serde(rename)]` 锁定**（`tool_use_id`、`is_error`、`content` 等） | transcript.jsonl 必须与 TS 版双向兼容（resume 旧会话） |
| 9 | **终端渲染手写 ANSI**（品牌色/框/markdown-lite 逐函数移植），不引 ratatui/crossterm 大件 | 渲染输出是行为规格（BEHAVIOR §3）；TTY 检测用 `std::io::IsTerminal` |
| 10 | **REPL 用 `rustyline`**（validator 定制多行、Completer trait、 hinter 关闭） | 覆盖 node:readline 用到的 90% 能力；蓝 prompt 切换用 hook 有限实现，降级预案见 PORTING_PLAN 风险表 |

## 2. Cargo 清单

```toml
[package]
name = "nanocode"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"                 # CancellationToken
reqwest = { version = "0.12", features = ["stream", "json", "gzip"] }
futures = "0.3"
async-stream = "0.3"               # Stream! 宏
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "0.8"                   # 工具 JSON Schema
thiserror = "2"
uuid = { version = "1", features = ["v4"] }
sha2 = "0.10"
rand = "0.8"                       # 抖动/随机动词
glob = "0.3"                       # Glob 工具（ignore 表自实现，保持 DEFAULT_IGNORE 行为）
dirs = "5"                         # ~ 展开
rustyline = "14"                   # REPL
clap = { version = "4", features = ["derive"] }
tracing = "0.1"                    # 日志（原项目 stderr 直写 → 保持 stderr，tracing 仅库内诊断）

[dev-dependencies]
tempfile = "3"
wiremock = "0.6"                   # SSE 夹具回放
```

明确**不用**：tokio Semaphore 之外的并发框架、ORM、config 大件、openrouter 专用 SDK。

## 3. 关键类型草案

### 3.1 core::types（对齐 core/types.ts）

```rust
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: ToolResultContent, #[serde(rename = "is_error")] is_error: Option<bool> },
    Thinking { thinking: String },
    RedactedThinking { data: String },
    Image { source: ImageSource },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Message { pub role: Role, pub content: Vec<ContentBlock>, #[serde(skip_serializing_if="Option::is_none")] pub id: Option<String> }

#[derive(Clone)] // 不序列化，事件是运行时通道
pub enum AgentEvent {                       // == StreamEvent
    AssistantText { text: String }, Thinking { text: String },
    ToolUse { tool_use: ToolUseBlock }, ToolStart { tool_use_id: String, tool_name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, tool_name: String, result: String, is_error: bool },
    AssistantMessage { message: Message }, TurnComplete { stop_reason: String },
    Usage { usage: TokenUsage }, Compact { old_tokens: u64, new_tokens: u64 },
    Error { error: NanocodeError }, MaxTurnsReached { max_turns: u32 },
}
```

### 3.2 Tool trait（fail-closed 默认经 RequiredTool 包装）

```rust
#[async_trait]
pub trait ToolDef: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self, input: Option<&serde_json::Value>) -> String;
    fn input_schema(&self) -> serde_json::Value;                 // schemars 预生成或常量
    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
    // 可选语义，默认 fail-closed：
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool { false }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool { false }
    fn max_result_size_chars(&self) -> usize { 30_000 }
    fn user_facing_name(&self, input: &serde_json::Value) -> String { self.name().to_string() }
    fn prompt(&self) -> String { String::new() }
}
pub type Tool = Arc<dyn ToolDef>;   // registry: Vec<Tool> + name 查找
```

### 3.3 ToolContext / 模式信号

```rust
pub struct ToolContext {
    pub cwd: PathBuf,
    pub file_state: Arc<RwLock<FileStateCache>>,     // LRU，clone 隔离/merge 保留
    pub file_history: Arc<RwLock<FileHistoryState>>,
    pub modified_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub session_id: String,
    pub cancel: CancellationToken,
    pub permission_mode: Arc<AtomicPermissionMode>,  // RwLock<PermissionMode>；EnterPlanMode 写、loop 读——保持 TS mutable-context 时序（下一轮生效）
    pub permission_gate: Arc<dyn PermissionGate>,    // onPermissionRequest
    pub query_params: OnceLock<Arc<QueryParams>>,    // 替代 Agent 工具的 _queryParamsRef 单例
}
#[async_trait]
pub trait PermissionGate: Send + Sync {
    async fn decide(&self, tool: &str, input: &serde_json::Value, message: &str) -> PermissionDecision;
}
```

### 3.4 Agent Loop 形态

```rust
pub struct AgentLoop<'a> { params: QueryParams, messages: &'a mut Vec<Message>, /* turn_count, ptl_retries … */ }
impl AgentLoop<'_> {
    pub async fn run(self, events: impl Sink<AgentEvent>) -> Vec<Message>;   // 供 CLI
    // headless/sub-agent 用: pub fn stream(self) -> impl Stream<Item=AgentEvent> + 返回值经共享槽
}
```
`async_stream::stream!` 内 `self.messages` 可变借用即可复刻「原地变异 + yield 透传」；循环体八步与 TS 逐行对应（ARCHITECTURE §4.1 有序清单）。

### 3.5 Session / 序列化契约

- `~/.nanocode/sessions/{id}/transcript.jsonl`：每行 `SessionEntry { type, message, timestamp, id }`；字段名与 TS 完全一致（snake_case 已对齐）；**Phase 1 固化 fixture 测试**（TS 生成的真实文件入库 `tests/fixtures/`）。
- `meta.json`：同字段；读取端宽容（unknown fields 忽略）。

## 4. 测试策略

| 层 | 手段 |
|---|---|
| 纯逻辑（bash-readonly、token 估算、errors/classify、executor 分批、memory 合并、yaml 子集、markdown-lite） | 单元测试直接移植 TS 用例断言（总计约 250+） |
| SSE/HTTP | wiremock 夹具：标准流、OpenRouter usage、坏 JSON 工具入参、PTL 错误帧、断流 |
| 工具 | tempfile 目录 + 真 rg/rg 缺失两种环境；Bash 用真 bash（CI Linux） |
| Agent Loop | mock ModelClient（trait 注入）驱动七场景（PORTING_PLAN Phase 4 验收） |
| 会话契约 | TS transcript fixture 往返 |
| E2E | mock 模型 one-shot：断言 stdout 结构（logo/框/分隔线经 ANSI 剥离后文本对拍） |

## 4.6 Model Provider 抽象（OpenAI 支持）

内部消息表示保持 Anthropic 风格（`Message`/`ContentBlock`）为规范格式，
provider 差异全部收敛在 Model 边界：

```
ModelCaller (trait) ← agent loop 唯一依赖
  ├─ ModelClient   (Anthropic Messages SSE, x-api-key)
  └─ OpenAIClient  (Chat Completions SSE, Bearer)   ← src/core/openai.rs
```

出向转换（内部→OpenAI）：system blocks → 首条 system 消息；user 的
tool_result 块 → 独立 tool 消息（先于同消息内 text）；assistant 的
tool_use → tool_calls（arguments 序列化为字符串）；thinking 丢弃
（协议无对应，签名不可跨协议回传）。cache_control 不存在则忽略。

入向归一化（OpenAI SSE→AgentEvent）：delta.content→AssistantText；
delta.reasoning_content（DeepSeek/GLM 风格）→Thinking；delta.tool_calls
按 index 聚合、[DONE] 时 yield 完整 ToolUse；finish_reason 映射
stop→end_turn / tool_calls→tool_use / length→max_tokens；
stream_options.include_usage 取 prompt/completion tokens。

选择：CLI `--provider anthropic|openai`（env NANOCODE_PROVIDER 兜底）；
openai 用 OPENAI_API_KEY / OPENAI_BASE_URL（默认 api.openai.com/v1）。
错误分类复用 classify_error（OpenAI 错误体 {"error":{message,code,type}}
走 HTTP status + message 前缀路径）。

## 5. 与原实现的刻意差异清单（唯一允许的偏差面）

1. 死依赖不移植（diff、turndown 可选路径→用 Rust HTML→markdown crate，保留纯文本回落）。
2. `Node version` 环境行 → `Rust version`（等价替换，BEHAVIOR §1 取舍规则 2）。
3. TS race-限流 bug → Semaphore 正确限流（可观察行为不变）。
4. D1 清单的「做实」项（rules、path-validation、MCP 接线、post-compact、model 覆盖修复）。
5. **智谱 GLM 平台适配**（实测验证）：thinking 块回传需携带 `signature`（GLM 发 `signature_delta`，缺省回传会被拒）；GLM 的 `input_tokens` 只在 `message_delta` 出现（Anthropic 在 `message_start`），SSE 解码两处都取；模型注册表含 GLM-5.3/GLM-5.3-Flash（1M ctx / 128K out，套餐计费单价记 0，/cost 显示 $0.00000 属预期）。
6. 其余一切用户可见行为零偏差；新增偏差必须先改本文件再动代码。
