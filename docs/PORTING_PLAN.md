# PORTING_PLAN.md — Rust 重写分阶段计划（Phase 0 产出，补全任务书 §4.3 之后缺失的章节）

> 上游：ARCHITECTURE.md（内部架构）、BEHAVIOR.md（验收基线）、DEPENDENCY_GRAPH.md（依赖强度分层）、RUST_DESIGN.md（目标设计）。
> 第一原则（承任务书）：Understand first. Design second. Implement third. Optimize last.
> 每阶段结束必须：`cargo build` 零警告目标 + `cargo test` 全绿 + 该阶段验收标准逐条可演示。

---

## 0. 三项前置决策

### D1 — Dead code 的处理原则：「做实，而非翻译」

考古发现原项目 8 处已实现但未接线的能力（ARCHITECTURE.md §4.14）。任务书 Target 清单包含 MCP、permissions、sub-agent，因此策略是：

| 能力 | 原项目状态 | Rust 版决策 |
|---|---|---|
| Permission rules（settings.json allow/deny） | 已实现未接线 | **做实**（Phase 5 接入权限管线，行为向 BEHAVIOR.md §5 兼容：rules 优先级插在"只读放行"之后、"模式放行"之前，不改变默认模式下的可见交互） |
| Path validation | 已实现未接线 | **做实**（Phase 5：Edit/Write/NotebookEdit 调用前校验；项目外警告放行保持） |
| MCP 启动连接 | 客户端完备未接线 | **做实**（Phase 7：启动时按配置连接、注册 `mcp__server__tool` 工具；失败跳过不致命） |
| Post-compact 文件回注 | 已实现未接线 | **做实**（Phase 6：auto/manual compact 后回注，符合原设计预算 5 文件/50K/5K） |
| File history 快照/undo | 骨架在、从不快照 | **降级保留**：Phase 6 实现 trackEdit/makeSnapshot 真实落盘（工具已有 trackedFiles 钩子）；rewind/undo 命令不在第一阶段范围（原项目无 /undo 用户行为，NANOCODE.md 提到属 roadmap 漂移） |
| agent-prompts 长 prompt | 未用（子代理实际用 buildSubAgentPrompt 短前缀） | **保持短前缀**（实现考古后修正：长 prompt 在 TS 无注入通道，runSubAgent 不接收 system prompt；短前缀才是实际行为，BEHAVIOR §7 以此为准） |
| atomic-write 通用版 | 工具内联同步版生效 | 统一为一个原子写模块（tempfile + rename + 权限保留），替代三份内联 |
| runSubAgent model 覆盖失效 | Bug | **修复**：模型覆盖生效 |
| Agent 工具 setAgentQueryParams 无调用方 | Bug（工具恒报 not initialized，README 宣传的 sub-agent 能力实际不可用） | **做实**：AgentLoop::run 注入 SubAgentRunner，子代理完整可用 |

### D2 — 行为基线优先级

BEHAVIOR.md 是验收基线。冲突时的取舍顺序：
1. **协议正确性**（消息结构、tool_result 语义、流事件顺序）——零容忍偏差；
2. **终端可见格式**（logo/框/spinner/颜色/文案）——1:1 复刻为默认目标；确因 Rust 生态差异做不到逐字节一致时（如 Node version 行），允许**等价替换**（改为 rustc 版本），但结构、颜色、措辞风格必须一致；
3. 性能特征（并行限流 10、超时数值、缓存 TTL）——数值保持一致。

### D3 — 单 crate 起步，模块边界按 crate 预留

9K LOC 规模下 workspace 多 crate 是过度设计（任务书反目标 #5：不为架构牺牲边界，也不为少代码牺牲边界）。采用**单 crate + 禁越层访问的模块纪律**（module 树即 ARCHITECTURE 分层），待 MCP/多 provider 扩展压力出现再拆 workspace。详见 RUST_DESIGN.md。

---

## 1. 阶段总览

| Phase | 主题 | 交付 | 依赖 |
|---|---|---|---|
| 1 | 骨架与类型层 | crate 骨架、core types、errors、token 估算、CI 测试底座 | — |
| 2 | Model 层 | Anthropic 客户端 + SSE 流解码 + 重试 + 事件流 | 1 |
| 3 | 工具系统 | Tool trait、registry、streaming executor、6 个基础工具 | 1 |
| 4 | Agent Loop | 主循环 + 权限路径 A + 子代理 | 2,3 |
| 5 | 上下文与持久化 | 压缩三层、session、memory、git-context、rules、path-validation | 4 |
| 6 | Skills / Plan / File history / post-compact | 技能系统、plan mode、历史快照、回注 | 5 |
| 7 | 终端层 | REPL（rustyline）、补全、渲染、spinner、16 命令、MCP 启动接线 | 4+ |
| 8 | 验收与打磨 | BEHAVIOR.md 逐条对拍、one-shot E2E、性能核对 | 全部 |

Phase 7 依赖 4（可用 headless 驱动先行），可与 5/6 并行推进。

---

## Phase 1 — 骨架与类型层

**目标**：`cargo new nanocode`（或指定目录）、模块树、核心类型 1:1、错误体系、token 估算，全部纯逻辑带测试。

任务：
1. `Cargo.toml`：tokio(full)、reqwest(stream/json)、serde/serde_json、schemars、uuid(v4)、sha2、thiserror、async-trait、futures、async-stream、tracing、dirs、glob 或 ignore；dev: tempfile、tokio-test。
2. `src/core/types.rs`：`ContentBlock`（六变体）、`Message{role,content,id:Option}`、`StreamEvent`（11 变体）、`ToolResult{result,is_error}`、`ToolContext`、`PermissionMode/PermissionDecision/PermissionRule`、`ModelConfig`、`QueryParams`、`SystemPromptBlock`、`TokenUsage`、`SessionEntry`、`FileState` 等，字段与序列化格式对齐 TS（`tool_use_id`、`is_error` 蛇形保持，消息 JSON 与原 transcript 兼容——resume 旧会话文件必须能读）。
3. `src/core/errors.rs`：`NanocodeError` 枚举（PromptTooLong/RateLimit{retry_after}/Overloaded{consecutive}/Auth/Network/ToolExecution/Abort/Other(cause)）+ `classify()`（status 401/403/429/529、消息含 "prompt is too long" 等、网络错误串）+ `with_retry`（5 次、1s×2^n、60s cap、±20% 抖动、529 连续 3 次熔断、Retry-After 优先）——行为对齐 errors.ts 并移植其 43 个测试用例的核心断言。
4. `src/context/token_counting.rs`：4/2 chars/token、image 常量（**两套**：loop 内联版 image=2000 与导出版 image=1500，分别命名保留，ARCHITECTURE §4.5 的差异是行为的一部分）。
5. 测试底座：`cargo test` + 每模块 `#[cfg(test)]`。

**验收**：类型可序列化往返；with_retry 单测覆盖退避序列/熔断/不重试集合；token 估算与 TS 测试用例数值一致（43 用例）。

## Phase 2 — Model 层

**目标**：不依赖官方 SDK，自实现 Anthropic Messages 流式客户端。

任务：
1. `src/core/api.rs`：`ModelClient`（baseURL 可注入；`ANTHROPIC_BASE_URL`/`OPENROUTER_API_KEY` 语义保留）。
2. 请求组装：system blocks（含 cache_control 字段透传）、tools→JSON Schema（schemars 或工具自带 schema 常量）、thinking beta（`interleaved-thinking-2025-05-14`、budget 默认 10000）。
3. SSE 解码：reqwest `bytes_stream` → 逐行 → `event:`/`data:` 帧 → 复刻 api.ts 状态机（content_block_start/delta/stop、message_start/delta 的 usage 提取、input_json_delta 累积解析、stop 后组装 assistant message：thinking→text→tool_use 顺序）。
4. `call_model() -> impl Stream<Item=StreamEvent>`（async-stream），事件序列与 TS 逐类对拍（assistant_text 增量、thinking 增量、tool_use 完整块、assistant_message、usage、turn_complete）。
5. `MODEL_CONFIGS`：三模型 + 别名 + 部分匹配 + 默认回落；价格表。
6. 用 mocked HTTP（wiremock）做流解码测试：构造 SSE 夹具（含 OpenRouter usage 路径、坏 JSON 工具入参 → `{}`、finalMessage 缺失回退流内 usage）。

**验收**：对 SSE 夹具的输出事件序列与 TS 实现一致；重试在 429/529/网络错误下按 BEHAVIOR §10 表现。

## Phase 3 — 工具系统

**目标**：Tool trait + registry + 并行执行器 + 6 个核心工具，headless 可跑。

任务：
1. `Tool` trait（RUST_DESIGN §3.2）：`name/description(input)/input_schema()/call(input,ctx)->ToolResult/is_concurrency_safe/is_read_only/max_result_size/user_facing_name`；**fail-closed 默认**用 blanket impl 或宏达成（默认 unsafe & 非 readonly）。
2. `registry.rs`：构造期注册（替代 TS 运行时动态 import；`initialize_tools()` 返回 `Vec<Arc<dyn Tool>>`，保留 unknown-tool 的 createErrorTool 语义）。
3. `streaming_executor.rs`：partition（连续 safe 合批、unsafe 单批）+ `MAX_CONCURRENCY=10`（Semaphore，修复 TS 的 race 索引 bug，行为语义不变：start 事件即发、result 按原序）+ 截断 + 错误包裹。
4. 工具：Read、Edit（6 步验证链逐条 + diff 片段算法）、Write、Glob（ignore 默认表）、Grep（rg 子进程 + grep 回落）、Bash（spawn、超时 SIGTERM→2s→SIGKILL、输出组装、禁 pager env）。
5. `bash_readonly.rs`：白名单/子命令表/危险正则/引号感知解析器/env 校验——**全部 58 个 TS 测试用例移植**（此文件是安全边界，测试密度最高）。
6. `FileStateCache`（LRU 100/25MB、clone/merge 语义）+ 工具内联原子写统一为 `files::atomic_write`。

**验收**：Bash/Edit/Read 的全部 TS 测试断言在 Rust 测试中复现绿；executor 分批用例（安全/不安全混合序列的批结构）一致。

## Phase 4 — Agent Loop

**目标**：主循环 + 权限 + 子代理，headless 端到端可用。

任务：
1. `agent.rs`：循环八步（§ARCHITECTURE 4.1）逐一对齐——auto-compact 阈值判定（用内联估算版）、micro-compact（>6 条前、>50K 截 5K）、PTL 恢复（删最老 2 条 ×3）、结果按 tool_use 序重排、maxTurns 200、abort 检查点。
2. 循环实现为 `AgentLoop` struct 持 `&mut Vec<Message>` + `async fn next_event()` 或 `async_stream` 生成器（见 RUST_DESIGN §3.4 的抉择），return 语义 = 最终 messages。
3. 权限路径 A：`check_tool_permission`（bypass→readOnly→plan→acceptEdits→callback）；`onPermissionRequest` 为注入 trait `PermissionGate`（headless 全允许实现先行）。
4. `run_sub_agent`：隔离消息/克隆缓存/合并、工具过滤、模型覆盖修复（D1）、静默提取末条 assistant 文本。
5. Agent 工具注册（含子代理类型配置与短 prompt——采用长 prompt 见 D1）。
6. 集成测试：mock 模型流驱动完整回合（无工具终止、单工具、并行批、权限拒绝、PTL、maxTurns、abort 七场景）。

**验收**：七场景事件序列与消息终态与 TS 逻辑推演一致；`oneShot` 等价 API 跑通 mock E2E。

## Phase 5 — 上下文与持久化

**目标**：三层压缩 + 会话 + 项目记忆 + git 上下文 + 做实 rules/path-validation。

任务：
1. `compaction.rs`：boundary 检测、3-turn 保留切分、序列化（500/1000 截断、略 thinking）、9 段 prompt、非流式摘要调用（复用 ModelClient 非流式路径，max_tokens 8000）、boundary 包装。
2. `post_compact.rs`：做实回注（5 文件/50K/5K 预算、按时间戳、跳过已引用），接入 auto 与 manual 压缩路径。
3. `session.rs`：`~/.nanocode/sessions/{id}/transcript.jsonl + meta.json`；懒初始化；append 即时落盘；列表按 updatedAt；**读取兼容旧 TS 会话文件**（字段蛇形一致）；写侧优化 meta 计数（不再全文件重读——内部优化，不改外部行为）。
4. `memory.rs`：向上层级加载顺序（8 文件 + 2 rules 目录）、@include（深度 5、失败注释）、合并格式 `# Source: ...`。
5. `git_context.rs`：并行采集四命令、5 分钟 TTL、porcelain 100 行截断、非 git 文案。
6. `permissions/{rules,engine,path_validation}.rs`：做实（D1）——settings.json 加载（.nanocode→.claude、project+user）、glob 匹配、mcp 前缀；engine 决策链 8 步接管 agent 循环权限入口，决策顺序保证 default 模式下用户可见交互不变；path-validation 接入写工具前置校验。
7. compact-prompt/system-prompt 常量全文移植（`prompt/` 模块，含 cache boundary 逻辑与 applyCache 接线补全——发送前真正挂 cache_control）。

**验收**：compaction 21 测试 + token 43 + memory 24 + git 20 用例移植绿；旧 TS transcript 可 resume。

## Phase 6 — Skills / Plan / File history

任务：
1. `skills/loader.rs`：目录发现（向上至 home + 用户级、同名先到先得）、YAML 子集解析器（不复刻 TS 正则实现，但解析结果超集兼容）、$ARGUMENTS/$1..$9/命名参数/SKILL_DIR 展开、fork 执行（run_sub_agent + allowedTools + model）。
2. plan-mode：EnterPlanMode/ExitPlanMode 工具 + 模式信号通道（ToolContext 不可变 → 用 `Arc<Mutex<PermissionMode>>` 或 mode 变更事件，见 RUST_DESIGN §3.3）。
3. file history：trackEdit 真实备份（sha256@vN、~/.nanocode/file-history/）、makeSnapshot 在消息边界调用；rewind 保留为库 API（不做 /undo 命令）。
4. Todo/Ask/WebFetch/WebSearch/NotebookEdit 工具补齐（Ask 用 rustyline 独立实例或 termios 直读）。

**验收**：skills 29 测试移植绿；Todo/Notebook 行为对拍；plan 切换在循环下一轮生效（与 TS 的 mutable context 时序一致）。

## Phase 7 — 终端层

任务：
1. `cli.rs`：参数解析（clap，旗标语义 1:1）、one-shot、REPL。
2. 渲染：`format.rs` 全套（品牌色/工具框/markdown-lite/渐变 logo/cost 分隔线）逐函数移植 + TS format 测试（10 用例）移植；`streaming.rs` spinner（braille 80ms、52 动词）。
3. REPL：rustyline（补全 hook 复刻 completer 行为、多行 `\` 续行、蓝 prompt 切换、输入队列、Ctrl+C abort 语义、退出码表）。
4. `commands.rs`：16 命令逐个移植（含 /init 的 sendPrompt 通道、/resume 前缀匹配、/context 进度条）。
5. MCP 启动接线（D1）：启动时 loadMcpConfig → 并发连接 → 包装注册；失败 stderr 提示继续；/mcp 命令从"仅展示"升级为展示活动连接。
6. headless.rs 等价 SDK（createAgent/query/oneShot）。
7. Ask 工具的终端实现落位。

**验收**：REPL 手工冒烟清单（BEHAVIOR §2/§3/§4/§6 全条目）逐条通过；one-shot 输出与 TS 版对拍。

## Phase 8 — 验收与打磨

任务：
1. **BEHAVIOR.md 对拍**：逐条对照（§1–§13），输出对拍清单表格。
2. E2E：mock 模型 + 临时目录 + 真实子进程的全链路测试（REPL 脚本驱动可选）。
3. 性能核对：并行批限流、spinner 帧率、Glob/Grep 结果上限。
4. `cargo clippy -- -D warnings`、`cargo fmt`、无 unwrap 泛滥审计（工具边界允许 expect + 消息）。
5. README：安装、配置（ANTHROPIC_API_KEY/OPENROUTER、ANTHROPIC_BASE_URL）、与原版行为差异说明（Node version 行等）。

**验收（总闸）**：一个纯 Rust 二进制 `nanocode`，无 Node 依赖，在 mock 与真实 API 两种环境下通过 BEHAVIOR.md 全部可观察行为；测试数 ≥ 300 且全绿。

**实测记录（智谱 lite 套餐）**：真实平台为智谱 BigModel Anthropic 兼容端点
（`https://open.bigmodel.cn/api/anthropic`，GLM-5.3-Flash）。已验证：流式回答、
Read 工具真实调用与多轮回传（thinking+signature）、prompt 缓存命中、401 分类、
CLI one-shot 终端渲染（thinking 灰显/工具框/cost 分隔线）。
测试 226 单元+E2E + 3 live（`ZHIPU_LIVE_E2E=1` 触发）。

---

## 2. 风险清单

| 风险 | 缓解 |
|---|---|
| SSE 解码细节偏差（partial JSON、usage 路径） | Phase 2 用 TS 实现逐行对照写夹具，wiremock 回放 |
| rustyline 与 node:readline 行为差异（蓝 prompt 刷新、Enter 接受建议） | Phase 7 预留自绘 fallback；补全行为可降级为"Tab 展开列表"但命令接受逻辑必须一致 |
| serde 序列化字段名不齐导致旧会话不可读 | Phase 1 即固化 transcript 契约测试（用真实 TS 生成文件做 fixture） |
| 权限 rules 做实改变默认交互 | D1 明确插入位置；Phase 5 对拍 BEHAVIOR §5 |
| async 生成器借用问题 | RUST_DESIGN §3.4 预案（struct 状态机） |
| 范围蔓延 | 每阶段验收闸门；不做原项目 roadmap 里的新功能（git 工具、vim 模式等） |
