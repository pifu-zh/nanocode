# ARCHITECTURE.md — nanocode 仓库考古（Phase 0 产出）

> 对象：https://github.com/Lyt060814/nanocode @ master（v0.1.0，2026-09-13 检出）
> 范围：src/ 9,147 LOC（14,317 含注释）、55 文件、20 个测试文件（559 用例，5009 行）
> 本文回答任务书 §4 全部问题，并补全任务书在 §4.3 之后缺失的章节（4.4–4.14）。

---

## 0. 总览

nanocode 是 Claude Code 的极简重实现。全项目只有 6 个运行时依赖，架构为单层 TypeScript 模块：
无 DI 框架、无接口抽象层，靠**模块级单例状态 + 动态 import 打破循环依赖**组织。

```
┌────────────────────────── 终端 ──────────────────────────┐
│  cli.ts (REPL/one-shot)      headless.ts (SDK)          │
│    │  readline/completer/format/spinner (utils)          │
│    │  commands/index.ts (16 slash 命令)                  │
├────▼─────────────────────────────────────────────────────┤
│  core/agent.ts  ←  Agent Loop（async generator）          │
│    │            core/streaming-executor.ts（并行分批）     │
│    │            core/api.ts（Anthropic 流式客户端 + 重试） │
│    │            core/errors.ts（错误分类 + withRetry）     │
│    │            core/types.ts（全部共享类型）              │
├────┼─────────────────────────────────────────────────────┤
│  tools/（15 工具 + registry + bash-readonly 校验器）       │
│  permissions/（engine、rules、modes、path-validation）     │
│  context/（compaction、token-counting、session、memory、  │
│            git-context、post-compact）                    │
│  prompt/（system、compact-prompt、cache-boundary、        │
│           agent-prompts）                                │
│  skills/（loader、skill-tool）   mcp/（client、config、   │
│                                  index、types）           │
│  files/（cache LRU、history、atomic-write、utils）        │
│  utils/（cost、format、streaming、process、completer）     │
└──────────────────────────────────────────────────────────┘
```

模块间关键耦合方式：

- `core/types.ts` 是所有类型的唯一定义点（ContentBlock、Message、StreamEvent、Tool/ToolDef、QueryParams、PermissionMode 等）。
- Agent 工具与 Skill(fork) 工具通过**动态 import `core/agent.js`** 反向调用 Agent Loop（打破循环依赖）。
- Agent 工具通过模块级单例 `setAgentQueryParams()` 注入父级 QueryParams（运行时 hack）。
- Plan mode 工具通过 `(context as any).permissionMode = 'plan'` **直接修改 ToolContext** 实现模式切换（ToolContext 在 JS 里可变，Rust 移植时需要显式的模式信号通道）。

---

## 4.1 Agent Loop

**真正的主循环在 `src/core/agent.ts` 的 `agentLoop()`（304–521 行），是一个 async generator**：

```ts
export async function* agentLoop(params: QueryParams): AsyncGenerator<StreamEvent, Message[]>
```

- 返回值（generator 的 return 值）是**最终 messages 数组**，供 CLI 持久化。
- 消费方：`cli.ts runAgent()`、`headless.ts`、`runSubAgent()`。三方都是 `while(true){ gen.next() }` 手动驱动。

### 数据流

```
user input
    ↓ (cli.ts processInput: push user Message → runAgent)
prompt construction        ← QueryParams 已含 systemPromptBlocks/tools/modelConfig；循环内不重建 prompt
    ↓
LLM request (streaming)    ← core/api.ts callModel()，async generator
    ↓
StreamEvent 流             ← assistant_text / thinking / tool_use / assistant_message / usage / turn_complete
    ↓
text / tool call 收集       ← tool_use blocks 累积到 toolUseBlocks[]；assistantContent 收完整消息
    ↓
无 tool_use? → return      ← stopping condition #1
    ↓
permission check           ← checkToolPermission()（Promise.all 并行检查）
    ↓ 拒绝的直接生成 isError tool_result
tool execution             ← executeTools()（streaming-executor，分批并行/串行）
    ↓
tool result                ← 按 tool_use 原始顺序重排 → buildToolResultMessage → push 为 user Message
    ↓
context update             ← messages.push(assistant) / messages.push(tool_results)（**原地变异同一数组**）
    ↓
next LLM request           ← while(true) 回到顶部
```

### 循环状态

| 状态 | 位置 | 说明 |
|---|---|---|
| `messages` | 循环局部（`let { messages } = params`） | **唯一会话状态**，每轮原地 push；auto-compact/PTL 时整体替换 |
| `turnCount` | 循环局部 | 每完成一轮工具执行 +1 |
| `ptlRetries` | 循环局部 | PTL 恢复计数，成功一次调用后清零 |
| `toolContext` | 循环外构建一次 | cwd / readFileState / fileHistory / modifiedFiles / sessionId / abortSignal / permissionMode / onPermissionRequest，跨轮共享 |
| `maxTurns` | `params.maxTurns ?? 200` | CLI 与 headless 均传 200 |

### Stopping conditions（按触发顺序）

1. **abort**：每轮顶部检查 `abortSignal.aborted` → yield `error(AbortError)`，return messages。
2. **无 tool_use**：assistant 消息没有工具调用 → 正常结束。
3. **PTL 重试耗尽**：`PromptTooLongError` 连续 3 次 → yield error（"Try /compact"），return。
4. **其他模型错误**：classifyError 后 yield error，return。
5. **max turns**：`turnCount >= maxTurns` → yield `max_turns_reached`，return。

### 每轮细节（按代码顺序）

1. **Auto-compact 检查**：`estimateMessageTokens(messages) > contextWindow - maxOutputTokens - 13_000` 时触发。压缩失败**不致命**（yield error 后继续用原 messages）。
2. **Micro-compact**（无条件的，每轮执行）：对 `messages.length - 6` 之前的 user 消息里的 tool_result，超过 50,000 字符的截断到 5,000 + `"[Content truncated: was N chars. Re-read the file if needed.]"`。
3. **流式调用**：yield 透传所有流事件；收集 tool_use / assistant_message / turn_complete。
4. **PTL 恢复**：`truncateForPTL()` 删除最老的 2 条消息（`splice(i,1); i--` 循环），continue 重试。
5. **Push assistant 消息**（content 非空时）。
6. **权限检查**：对每个 tool_use 并行 `checkToolPermission`；bypass → 全允许；`tool.isReadOnly(input)` → 允许；plan → 拒绝写；acceptEdits → 允许 `Edit/Write/NotebookEdit`；其余 → `params.onPermissionRequest(name, input, describeToolUse)`（CLI 的 y/n/a readline 交互）。
7. **执行**：`executeTools(allowedToolUses, tools, toolContext)`，yield 透传 `tool_start`/`tool_result`，收集结果。
8. **结果重排**：按 tool_use 原始顺序对齐结果（丢失的填 `Tool execution failed (no result)`，isError），push 为单条 user 消息（每个 block 是 `tool_result`，含 `tool_use_id` 和 `is_error`）。

### Parallel tool calls

见 §4.3 Execution model。权限检查本身 Promise.all 并行；执行按 `isConcurrencySafe` 分批。

### Error handling

- 工具执行错误**永不冒泡**：`executeSingleTool` 捕获一切，转为 `{result: "Error executing X: msg", isError: true}`。
- 模型流错误：`classifyError`（见 §4.2），PTL 特殊处理，其余 yield + return。
- 压缩错误：yield error，继续。
- CLI 侧 `runAgent` 再兜底 try/catch（"Fatal: …"）。

### Cancellation

- `AbortSignal` 贯穿：loop 顶部、callModel 流、withRetry、sleep、工具执行（Bash SIGTERM→2s→SIGKILL、WebFetch/WebSearch abort、Ask readline close）。
- CLI：REPL Ctrl+C 在 processing 中调用 `currentAbort.abort()`，显示 `[interrupted]`，不退出进程。

### Retry

模型调用层：`withRetry`（5 次、1s 起步、×2 指数退避、上限 60s、±20% 抖动；429 用 Retry-After；529 连续 3 次放弃；401/403、PTL、Abort 不重试）。PTL 在 loop 层另有消息截断重试（3 次）。

### Context mutation

`messages` 是被 `agentLoop` 原地变异的共享数组（cli.ts 传入后用返回值覆盖；sub-agent 用新数组）。micro-compact 直接改写历史消息的 block 内容。**Rust 移植注意**：这是 `&mut Vec<Message>` 语义，generator 需要持有可变借用或把状态收进 struct。

### runSubAgent（agent.ts 531–597）

- 新 messages 数组（单条 user prompt），`readFileState.clone()` 隔离，共享 fileHistory。
- 工具过滤：`options.tools`（allowlist）/ `disallowedTools`（blocklist）。
- 事件不透传给用户，静默运行，只提取最后一条 assistant 文本作为返回值；空返回 `'(No response from sub-agent)'`。
- 结束后 `readFileState.merge(clone)`（时间戳新的赢）。
- **Bug 记录**：`options.model` 参数被接收但从未写入 subParams，模型覆盖实际不生效。

---

## 4.2 Model Layer

`core/api.ts` — 职责不是"API 名称列表"，而是：

1. **Provider 归一化**：通过官方 `@anthropic-ai/sdk` 单例 client（`createClient`，带模块级缓存 `_client`）。
   - `ANTHROPIC_BASE_URL` 重定向 → 支持 OpenRouter 等 OpenAI 兼容网关（仍是 Anthropic 消息格式）。
   - key 缺失时 fallback `OPENROUTER_API_KEY`。
2. **流式协议解码**：`callModel()` 把 Anthropic SSE 事件流解码为内部 `StreamEvent`：
   - `content_block_start`（tool_use → 开始累积 id/name；thinking → 开始累积）；
   - `content_block_delta`：`text_delta` → 立即 yield `assistant_text`（增量）；`input_json_delta` → 累积工具 JSON；`thinking_delta` → yield `thinking`；
   - `content_block_stop`：解析累积 JSON（失败→`{}`），yield 完整 `tool_use`；
   - `message_start`/`message_delta`：提取 usage（OpenRouter 兼容路径）；
   - 流结束后：`finalMessage()` 拿权威 usage（空则回退流内累计值）→ 组装完整 assistant 消息（顺序：thinking → text → tool_use[]）→ yield `assistant_message`、`usage`、`turn_complete(stopReason)`。
3. **Extended thinking**：`enableThinking && supportsThinking` 时走 `client.beta.messages.stream` + `betas: ['interleaved-thinking-2025-05-14']`，`thinking.budget_tokens` 默认 10000。
4. **重试**：见 §4.1 Retry；重试回调向 stderr 打黄色 `[retry N] Name: message (waiting Xs)`。
5. **请求组装**：`system` 接受 block 数组（cache_control 挂在最后 static block，见 §4.6）；tools 经 `zodToJsonSchema(target: 'openApi3')` 转 JSON Schema 并剥 `$schema`；`description` 支持函数（Bash/Agent 是动态描述）。
6. **模型注册表**：`MODEL_CONFIGS` 三模型（sonnet/opus/haiku 的 claude-4 型号），含 contextWindow 200K、maxOutputTokens 16,384、价格表、supportsThinking；别名 + 双向 includes 部分匹配 + 默认回落 sonnet。
7. **全局 lastUsage**（`getLastUsage`/`setLastUsage`）：**dead code**——agent.ts 导入但未使用；实际用量走 `usage` 事件 → CLI CostTracker。

**Rust 移植含义**：Model abstraction 的真正职责 = 「Anthropic Messages 协议客户端 + SSE 流解码器 + 重试策略 + 事件归一化」。OpenAI 兼容仅指"换 baseURL"，不涉及格式转换。

---

## 4.3 Tool System

### 工具接口（core/types.ts）

```ts
interface ToolDef<Input> {
  name; description: string | (input?) => string;
  inputSchema: z.ZodType<Input>;           // 唯一 schema 来源
  call(input, context): Promise<ToolResult>;
  prompt?(): string;                        // 注入 system prompt 的附加说明（Skill 工具用）
  isConcurrencySafe?(input): boolean;       // 默认 false（fail-closed）
  isReadOnly?(input): boolean;              // 默认 false（fail-closed）
  maxResultSizeChars?;                      // 默认 30_000
  userFacingName?(input): string;
}
```

`buildTool()`（registry.ts）应用 fail-closed 默认值。注册表是**模块级 Map 单例**，`initializeTools()` 动态 import 15 个模块并注册（Skill 带 catch 容错）。MCP 工具运行时动态注册，不在初始化列表。

### 完整工具清单（15 个）

| Tool | Input schema 要点 | 输出格式 | Side effects | Permission | 并行 | 错误模型 |
|---|---|---|---|---|---|---|
| **Bash** | command、timeout(≤600s, 默认120s)、description | `[Command timed out]`/stdout(30K 截)/`STDERR:`(10K 截)/`(exit code: N)`/`(No output)` | 执行任意 shell（spawn bash -c；禁 pager） | readOnly=命令白名单判定；非只读需询问 | 仅只读命令 | 非零退出码 → isError（timeout 除外） |
| **Read** | file_path、offset(1-based)、limit(默认2000行) | `cat -n` 风格行号 + partial 元数据 `(N more lines below)` | 写 readFileState 缓存 | 只读 | ✅ | ENOENT/EISDIR/EACCES 专属消息；二进制（前8K含\0）与图片扩展名给出建议文案（非错误） |
| **Edit** | file_path、old_string、new_string、replace_all | `Edited path (N replacements)` + 3 行上下文 diff | 改文件（原子写）、readFileState、modifiedFiles、trackedFiles | 非只读，需询问（acceptEdits 放行） | ❌ | 6 步验证链（§BEHAVIOR）；任何一步失败即中止 |
| **Write** | file_path、content | `Written: path (N lines)` | 同上 | 非只读；已存在文件必须先 Read | ❌ | read-before-write、staleness 检查 |
| **Glob** | pattern、path | 绝对路径排序列表，≤200，附 cap 提示 | 无 | 只读 | ✅ | — |
| **Grep** | pattern、path、include | `file:line:content`，≤500 行，附计数 | 无 | 只读 | ✅ | rg 退出码 1=无匹配（正常）；2=错误→fallback grep |
| **Agent** | prompt、subagent_type(Explore/Plan/default)、description、model(失效)、run_in_background(未用) | `[Sub-agent: X]\n\n<最终文本>` | 派生子循环；合并 readFileState | 非只读（尽管不改文件——fail-closed） | ❌ | 未初始化/空 prompt 报错 |
| **Ask** | question | 用户输入原文 / `(No response from user)` | 阻塞读 stdin | readOnly=true 但并行=false | ❌ | abort → `(User interaction cancelled)` |
| **Todo** | command(create/update/list)、task、id、status | `[ ]/[~]/[x] id8 | task (status)` 分组列表 + Total 行 | 改内存 taskStore（模块单例） | ❌ | 前缀匹配 id；未知命令报错 |
| **WebFetch** | url | `URL:/Status:/Content-Type:` 头 + markdown 正文（30K 截） | 无 | 只读 | ✅ | 非 2xx → isError；30s 超时；5MB 上限 |
| **WebSearch** | query | 编号结果 `N. title / url / snippet`（≤10） | 无 | 只读 | ✅ | DDG HTML 正则解析（uddg 解码）；15s 超时 |
| **Skill** | skill、args（容错 name/arguments） | inline=展开后 prompt；fork=子代理输出 | fork 时跑子循环 | readOnly=true（fork 实际可写——宽松点） | ❌ | 未找到 → 列出可用 skills |
| **EnterPlanMode** | reason? | 确认文案 | **改写 ToolContext.permissionMode='plan'** | 只读 | ❌ | 已在 plan → "Already in plan mode." |
| **ExitPlanMode** | reason? | 恢复文案 | 恢复 `_previousMode` | 只读 | ❌ | 不在 plan → "Not currently in plan mode." |
| **NotebookEdit** | notebook_path、cell_index、new_source、cell_type? | `Edited cell N in path\nOld (N chars) -> New (M chars)` | 改 .ipynb（code cell 清 outputs）、缓存 | 非只读 | ❌ | JSON/结构/索引校验 |

### 层次图

```
Agent (core/agent.ts)
  ↓ checkToolPermission()          ← 权限层（agent.ts 内简化版；engine.ts 未接线，见 §4.4）
Tool Registry (tools/registry.ts)  ← name → Tool 查找；未知工具 → createErrorTool
  ↓ executeTools() (core/streaming-executor.ts)
Tool.call(input, ToolContext)      ← 每个工具自带 zod 校验（由 schema 推断类型）
  ↓                                    Permission 已在上方裁决；Executor 只管执行与截断
Result (ToolResult) → 截断 maxResultSizeChars → tool_result 事件 → agent loop 组装 user 消息
```

### 并行执行模型（streaming-executor.ts）

1. 每个块按 `tool.isConcurrencySafe(input)` 标记 safe/unsafe。
2. **partitionToolCalls**：保留原始顺序，把**连续的** safe 块合成一个并行批；每个 unsafe 块自成串行批。即 `[Read, Read, Bash(写), Read]` → `[并行[Read,Read], 串行[Bash], 并行[Read]]`——写操作是顺序屏障。
3. 并行批：逐个 yield `tool_start` 后立刻启动 promise，`MAX_CONCURRENCY = 10`（实现用 `Promise.race` 等一个完成再放行下一个——有索引删除 bug，实际效果是近似限流）；全部 Promise.all 后**按原顺序** yield `tool_result`。
4. 结果超 `maxResultSizeChars` 截断并附 `[Output truncated: was N chars, limit M]`。
5. 未知工具 → `createErrorTool`（`Unknown tool: X`，unsafe，串行）。
6. 工具内部抛异常 → `Error executing {name}: {msg}`（isError）。

---

## 4.4 Permission System

**关键发现：存在两条并行且不一致的权限路径，运行时只走一条。**

- **路径 A（实际生效）**：`agent.ts checkToolPermission()` —— bypass → `tool.isReadOnly(input)` → plan 拒写 → acceptEdits 放行 3 个文件工具 → `onPermissionRequest` 回调（CLI readline y/n/a，`alwaysAllowed` 会话内记忆；headless 全允许）。
- **路径 B（未接线）**：`permissions/engine.ts checkPermission()` —— 8 步决策（deny 规则 → readOnly → bypass → allow 规则 → acceptEdits → plan → Bash 前缀白名单 → ask），支持 settings.json 规则（`rules.ts`：`.nanocode/settings.json` → `.claude/settings.json`，project + user 级，glob 匹配 content、`mcp__server__` 前缀匹配）+ 会话规则。**仅测试引用**。

其余组件：
- `modes.ts`：4 模式的描述与 `{allowReads, allowWrites, allowBash}` 矩阵（default: T/F/F；plan: T/F/F；acceptEdits: T/T/F；bypass: T/T/T）。
- `path-validation.ts`：危险路径（/、/etc、/usr…+ ~ 下 .ssh/.aws/.gnupg/.config）、symlink realpath 二次校验、项目边界外警告放行。**未接线（仅测试引用）**。

**行为语义（用户可感知的）**由路径 A 决定；任务书要求的 permissions 能力若要"保持行为"，需以路径 A 为基线，同时决定是否把路径 B 做实（见 PORTING_PLAN）。

---

## 4.5 Context Management（三层压缩 + 估算）

| 层 | 触发 | 机制 | 代码 |
|---|---|---|---|
| ① Auto-compact | `tokens ≥ contextWindow − maxOutputTokens − 13_000`（每轮循环顶部检查） | 找最后一个 `[CONVERSATION_COMPACTED]` 边界 → 之后的消息按「保留最近 3 个 turn（user+assistant 对）」切分 → 旧消息序列化（tool call 截 500 字符、tool result 截 1000、thinking 略去）→ 用 9 段格式 COMPACT_PROMPT 非流式调用模型（max_tokens 8000）→ 摘要包进 boundary 消息（user role）→ `[...preBoundary, summary, ...recent]` | agent.ts + compaction.ts + compact-prompt.ts |
| ② Micro-compact | 每轮无条件执行 | 除最近 6 条消息外，>50K 字符的 tool_result 截到 5K + 提示语（**原地改写历史**） | agent.ts microCompactMessages |
| ③ Post-compact 文件恢复 | （设计上）压缩后 | 按 readFileState 时间戳取最近 5 个文件，各 ≤5K tokens、总预算 50K，`<file path="...">` 包裹注入为 user 消息；跳过保留消息已引用的路径 | post-compact.ts。**未接线（仅测试引用）** |

Token 估算（不精确，刻意的）：文本 4 chars/token、JSON 2 chars/token、image 固定值、消息结构开销 4 tokens。**注意 agent.ts 内联版本（image=2000，tool_use 含 id）与 token-counting.ts 导出版本（image=1500，结构更细）不一致**；loop 判压缩用的是内联版本，`/status`、`/context` 命令用导出版本。

`/compact` 手动压缩与 auto-compact 共用 `compact()`，CLI 端自建非流式 `callModelForCompact`。

---

## 4.6 Prompt System 与 Prompt Caching

`buildSystemPromptBlocks({claudeMd, gitContext, cwd, model})` 产出有序 block 数组：

1. **STATIC_SYSTEM_PROMPT**（纯静态行为指令：IDENTITY / System Rules / Doing Tasks / Executing Actions / Using Tools / Tone and Style——全文复刻 Claude Code 风格，含"优先行动不请示""最小输出 token""不用 Bash 替代专用工具""prompt injection 警惕"等）。
2. **边界标记** `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`（哨兵字符串）。
3. Memory 段（`<nanocode-md>` 包裹的 NANOCODE.md/CLAUDE.md 合并内容，若有）。
4. Environment 段（cwd/platform/shell/model/date/Node version——Rust 版对应改为 rustc 版本或省略）。
5. Git 段（`<git-status>` 包裹，若有）。

`cache-boundary.ts applyCache()`：找到边界 → 边界前的最后一个 static block 打 `cache_control: {type:'ephemeral'}` → 移除边界块 → 拼回 dynamic。**但 cli.ts/headless.ts 构建请求时并没有调用 applyCache**（blocks 原样进 `callModel`，system 原样上送）——缓存边界做了拆分逻辑而真正挂 cache_control 的动作未接线。行为上等于：无显式 cache_control，缓存与否取决于 provider 默认。

子代理 prompt：`buildSubAgentPrompt()` 的 4–6 行短前缀（Explore/Plan/default 各一段）；`agent-prompts.ts` 的三份长 prompt **未接线（仅导出）**。

---

## 4.7 Session Persistence

- 位置 `~/.nanocode/sessions/{uuid}/`：`transcript.jsonl`（每行一个 `{type: user|assistant, message, timestamp, id}`）+ `meta.json`（id/createdAt/updatedAt/cwd/messageCount/summary?）。
- **懒初始化**：首条消息落盘时才 `initSession`（无空会话）。
- CLI 每条新消息即时 append（user 消息发出时 + agent loop 结束后从 `initialLen` 起的所有新增）；`saveMessage` 每次写 meta 都**全文件重读计数**（O(n)，行为而非 bug 修复点）。
- 加载：逐行 JSON.parse，坏行跳过；`listSessions` 按 updatedAt 降序。
- resume：`--resume <id>`（找不到→退出 1）或 `/resume <前缀>`（前缀匹配第一个）。
- compact_boundary 类型在 SessionEntry 联合类型里定义但从未写入。

---

## 4.8 Memory（项目指令加载）

`loadClaudeMd(cwd)`：
1. 从 cwd **向上走到文件系统根**（去重防环），每级按序检查：`NANOCODE.md`、`CLAUDE.md`、`.nanocode/NANOCODE.md`、`.nanocode/CLAUDE.md`、`.claude/NANOCODE.md`、`.claude/CLAUDE.md`、`NANOCODE.local.md`、`CLAUDE.local.md`、`.nanocode/rules/*.md`、`.claude/rules/*.md`（rules 目录按字母序）。
2. 再查用户级 `~/.nanocode/{NANOCODE,CLAUDE}.md` → `~/.claude/{NANOCODE,CLAUDE}.md`。
3. 全部内容经 `@include path/to/file.md` 递归展开（深 ≤5，缺失→`<!-- @include failed: ... -->`）。
4. 合并为 `# Source: <相对路径>\n\n<内容>` 段落，`\n\n---\n\n` 连接；空文件跳过。

---

## 4.9 Git Context

`getGitContext(cwd)`：5 分钟 TTL 缓存（按 cwd）。非 git → 缓存 `"Not a git repository."`。否则并行采集：当前分支（`rev-parse --abbrev-ref HEAD`）、主分支（探测 main/master）、`status --porcelain`（>100 行截断）、`log --oneline -10`。`GIT_TERMINAL_PROMPT=0`、`LC_ALL=C`。快照进 system prompt，会话内不刷新（文档明示）。

---

## 4.10 Skills

- 发现：`loadAllSkills(cwd)` 从 cwd 向上走到 home（含），每级查 `.nanocode/skills/`、`.claude/skills/`；再加 `~/.nanocode/skills/`、`~/.claude/skills/`。**同 名（大小写不敏感）先到先得**（项目遮蔽用户）。
- 格式：目录 + `SKILL.md`，`---` 围栏 YAML 子集（key: value、`- item` 数组、`[a,b]`、布尔、数字、引号；不支持嵌套）。字段：name/description/when_to_use/argument-hint/arguments/allowed-tools/model/user-invocable/context(inline|fork)/agent/paths。
- name 缺省取目录名；description 缺省 `Skill: {name}`。
- 展开：`$ARGUMENTS`（全量）、`$1..$9`（按空格切分、双引号成组）、命名参数（frontmatter arguments 列表）、`${NANOCODE_SKILL_DIR}`/`${CLAUDE_SKILL_DIR}`。
- 执行：`Skill` 工具（模型调用）——inline：展开文本直接作为 tool_result 注入对话；fork：`runSubAgent`（allowedTools 过滤、50 turns、skill.model 或硬编码 sonnet）。
- 懒加载：工具首次调用或 `/skills` 命令时 `initializeSkills(cwd)`。

---

## 4.11 MCP

- `mcp/client.ts`：stdio JSON-RPC 客户端。行分隔帧；`initialize`（协议 2024-11-05，30s 超时）→ `notifications/initialized` 通知 → `tools/list`/`tools/call`（60s 超时）；pending map 按 id 配对；服务器 stderr 转发 `[mcp:{name}]`；崩溃后 `_ensureConnected` 重连（≤3 次）；SIGTERM→2s→SIGKILL。
- `mcp/config.ts`：`loadMcpConfig` —— 用户级 `~/.nanocode/settings.json` → `~/.claude/settings.json`，项目级 `.nanocode/` → `.claude/`，**项目覆盖用户**；`mcpServers: {name: {command, args?, env?}}`。
- `mcp/index.ts`：`initializeMcpServers(cwd)` 并发连接所有服务器，逐工具包装（`mcp__{server}__{tool}`，schema 用 passthrough `z.record(z.unknown())` 接收任意对象，JSON Schema 原样保存；isConcurrencySafe=true、isReadOnly=false、maxResult 200K）；失败服务器 stderr 记录后跳过。
- **未接线**：`initializeMcpServers` 没有任何调用方；`/mcp` 命令只展示配置。即 v0.1.0 的 MCP 是「配置可读、客户端可用、启动时未连接」。`tools/mcp-wrapper.ts` 与 `mcp/index.ts` 存在**重复的 wrapMcpTool 实现**（50K vs 200K 截断差异），前者也未被运行时使用。

---

## 4.12 Files 子系统

- `files/cache.ts` FileStateCache：LRU（Map 插入序即访问序，touch 重插），上限 100 条 / 25MB；key 归一化（resolve+normalize）；`clone()` 深拷贝（子代理隔离）、`merge()` 时间戳新者胜。
- `files/history.ts` FileHistoryState：设计为版本化备份（`~/.nanocode/file-history/{sessionId}/{sha256(path)[0:16]}@v{N}`，快照 ≤100，rewind/getDiffStats）。**运行时只有 `trackedFiles.add()` 被工具调用**；trackEdit/makeSnapshot/rewind 无调用方 → 会话内 `/status` 的 `Snapshots: 0` 恒成立，undo 能力实际不存在。
- 原子写：edit.ts/write.ts/notebook-edit.ts 各自内联**同步**实现（同目录 `.nanocode-tmp-{uuid}` → rename，失败清理）；`files/atomic-write.ts` 的异步通用版（权限保留 chmod、rename 失败 fallback 直写）**未被工具使用**。

---

## 4.13 CLI / REPL / Commands / Headless

### CLI 参数（cli.ts parseArgs）
`-p/--prompt`、`-m/--model`（默认 `ANTHROPIC_MODEL` 或 sonnet）、`--api-key`（默认 `ANTHROPIC_API_KEY` → `OPENROUTER_API_KEY`）、`--max-turns`、`--permission-mode`、`--dangerously-skip-permissions`、`--resume <id>`、`--thinking`、`-h/--help`、`--version`；首个位置参数视为 prompt。无 key → stderr 报错退出 1。

### REPL
- 启动：logo（≥72 列用大版，蓝→金逐字符渐变）+ 信息框（版本/Model/CWD）+ 提示行；预加载 `initializeTools()`。
- 输入：readline + 自绘补全（completer.ts：slash 命令与文件路径建议、Tab/↑↓/Esc、Enter 接受）；多行（行尾 `\` 续行、Option/Shift+Enter 换行）；`/` 开头 prompt 变蓝；processing 期间输入进队列。
- Ctrl+C：processing 中 → abort 当前轮（`[interrupted]`）；空闲 → 提示 Ctrl+D。Ctrl+D/close：等待 in-flight 结束后 `Goodbye!` 退出 0。
- 渲染：assistant 文本按完整行缓冲 → `renderMarkdown`（代码块围栏线、标题加粗、列表圆点、inline 高亮：路径/URL/斜杠命令/camelCase 标识符等全部染品牌蓝）；thinking 灰显；工具执行 `● Tool summary` + 结果框（`┃` 前缀，20 行/每行 200 字符截断，隐藏行计数尾注）；Edit 结果 diff 上色；每次工具结果后重启 spinner（80ms braille 帧 + 52 个随机动词，如 "Pondering…"）；回合结束打印 cost 分隔线 `─── Turn N | 1.2K in / 500 out | $0.012 ───`。

### Slash 命令（commands/index.ts，单一事实源）
`/help /compact /clear /model /cost /resume /plan /memory /config /status /skills /context /init /mcp /exit`（+ `/quit` 别名）。`/init` 通过 `sendPrompt` 注入 INIT_PROMPT（生成 NANOCODE.md 的完整指令）再走 agent 循环。`/compact` 走压缩管线（spinner）。详见 BEHAVIOR.md。

### Headless（headless.ts）
`createAgent(options)` → `AgentInstance`：`query(prompt)` async generator（默认 bypassPermissions + 全允许 handler）、`getMessages/getCostSummary/getSessionId/reset`；`oneShot()` 便捷封装收集纯文本。

---

## 4.14 Dead Code / 未接线清单（考古结论）

| 模块 | 状态 | 影响 |
|---|---|---|
| permissions/engine.checkPermission + rules 加载 | 仅测试引用 | settings.json 权限规则运行时无效 |
| permissions/path-validation.validatePath | 仅测试引用 | 无路径防护（除 bash 白名单） |
| mcp/index.initializeMcpServers | 无调用方 | MCP 启动时不会连接；/mcp 只读配置 |
| tools/mcp-wrapper.ts | 与 mcp/index.ts 重复，未用 | — |
| context/post-compact.ts | 仅测试引用 | 压缩后无文件回注 |
| files/history.ts trackEdit/makeSnapshot/rewind | 仅 trackedFiles.add 被调用 | 无快照、无 undo；Snapshots 恒 0 |
| files/atomic-write.ts | 工具用各自内联同步版 | — |
| prompt/agent-prompts.ts | 仅导出 | 子代理用短前缀 |
| utils/process.ts spawnCommand/cleanup | 无调用方 | Bash 工具内联 spawn |
| api.ts getLastUsage/setLastUsage | 导入未使用 | — |
| plan-mode getPreviousPlanMode/reset | 仅导出/测试 | — |
| runSubAgent options.model | 接收后丢弃 | 子代理模型覆盖无效 |
| SessionEntry compact_boundary | 定义未写入 | — |
| Agent 工具 run_in_background | schema 有、无实现 | 恒同步 |

> 任务书要求「不要删除现有能力」。上述能力中 MCP/skills/permissions 属于任务书 Target 清单，但**原项目用户可见行为**并不包含其运行时效果。Rust 版的处理策略见 PORTING_PLAN.md §决策 D1–D3。
