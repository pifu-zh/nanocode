# DEPENDENCY_GRAPH.md — 依赖图与模块耦合（Phase 0 产出）

---

## 1. 外部依赖（npm，运行时 6 个）

| 包 | 版本 | nanocode 中的用途 | 使用者 | Rust 等价建议 |
|---|---|---|---|---|
| `@anthropic-ai/sdk` | ^0.39.0 | Anthropic Messages API 客户端 + SSE 流式 + beta thinking 端点；OpenRouter 走 baseURL | core/api.ts、cli.ts/headless.ts 的压缩调用 | 自写：`reqwest`（stream + eventsource 解码）或 `async-openai` 风格薄封装。**必须自控 SSE 解码**以复刻事件语义 |
| `zod` + `zod-to-json-schema` | ^3.24 | 工具输入 schema（运行时校验 + 转 JSON Schema 给 API）；MCP 工具用 passthrough record | 全部 15 工具、core/types.ts、mcp | `serde_json::Value` + `schemars`（或手写 JSON Schema 常量）。工具入参用强类型 struct + `Deserialize`，校验由 serde 承担 |
| `fast-glob` | ^3.3.3 | Glob 工具主路径（含 ignore、absolute、dot 开关） | tools/glob.ts | `glob` crate 或 `ignore` crate（后者自带 .gitignore 风格过滤，性能好） |
| `diff` ^7 / `turndown` ^7.2 | — | package.json 声明但源码零 import（`diff` 未使用；HTML→markdown 用 turndown） | turndown: tools/web-fetch.ts（动态 import，可缺省回落 stripHtml）；diff: **死依赖** | Rust: `html2md`/`htmd` 或自写简化版（原实现本就有 stripHtml 回落）；diff 无需 |
| dev: `vitest`、`typescript`、`@types/*` | — | 测试与编译 | test/ | `cargo test` + 内建 |

结论：真正的运行时硬依赖是 **Anthropic SDK、zod、fast-glob、turndown** 四个；Rust 版对应为 **HTTP/SSE 客户端、schema 体系、glob 实现、HTML→markdown**，其余皆可用标准库/已有 crate 覆盖。

## 2. Node 内置 API 依赖面（移植时逐项对位）

| 能力 | API | Rust 对位 |
|---|---|---|
| 子进程（Bash/MCP/git） | `child_process.spawn/execFile` + readline 行读取 | `tokio::process::Command` + `BufReader::lines` |
| 原子写 | `writeFileSync` + `renameSync` | `tempfile` 同目录 + `tokio::fs::rename`（或 std::fs 同步） |
| crypto | `randomUUID`、`sha256`、`randomBytes` | `uuid` crate、`sha2` |
| readline REPL | `node:readline`（prompt/completer/keypress） | `rustyline`（补全/多行/hook 齐备）或手写 termios |
| ANSI 输出 | 手写 `\x1b[...m` | 保持手写（格式是行为规格，不引 crossterm 也可；需 TTY 检测） |
| fetch（WebFetch/WebSearch） | 全局 `fetch` + AbortController | `reqwest` + `tokio::time::timeout` + `CancellationToken` |
| 路径/环境 | `node:path`、`node:os`、`process.env` | `std::path`、`dirs`/`home` crate、`std::env` |
| hash | `sha256`（file history 命名） | `sha2` |

## 3. 内部模块依赖图（运行时真实引用）

```mermaid
graph TD
    cli["cli.ts (bin)"] --> commands
    cli --> core
    cli --> tools
    cli --> context
    cli --> prompt
    cli --> files
    cli --> utils

    headless["headless.ts (lib)"] --> core
    headless --> tools
    headless --> context
    headless --> prompt
    headless --> files

    commands --> utils
    commands -.动态 import.-> context
    commands -.动态 import.-> skills
    commands -.动态 import.-> mcp

    subgraph core
      agent[agent.ts] --> api[api.ts]
      agent --> xexec[streaming-executor.ts]
      agent --> errors[errors.ts]
      api --> errors
      types[types.ts]
      agent & api & xexec --> types
    subgraph _
    end

    xexec --> toolsreg[tools/registry.ts 仅类型]
    xexec --> types

    toolsreg -.动态 import 15 模块.-> toolsAll[tools/*]
    toolsAll --> types
    toolsAgent["tools/agent.ts"] -.动态 import.-> agent
    toolsSkill["tools/skill-tool.ts (skills/)"] -.动态 import.-> agent
    toolsSkill --> skillsLoader[skills/loader.ts]
    toolsBash["tools/bash.ts"] --> bashro[tools/bash-readonly.ts]
    toolsPlan["tools/plan-mode.ts"] --> types

    context --> prompt
    compaction[context/compaction.ts] -.callModel 回调注入.-> api

    skillsLoader --> types2[skills/types.ts]
    mcpindex[mcp/index.ts] --> mcpclient[mcp/client.ts] --> mcpconfig[mcp/config.ts]
    mcpwrapper[tools/mcp-wrapper.ts] --> toolsreg

    files --> types
    utils --> types
```

要点：

1. **循环依赖全部靠动态 `import()` 切断**：`tools/agent.ts → core/agent.ts`、`skills/skill-tool.ts → core/agent.ts`、`context/compaction.ts ← core/agent.ts`（回调注入）。Rust 中这三处分别用 trait 对象/闭包自然解决，无需保留动态性。
2. `core/types.ts` 被所有层引用（类型汇聚点）→ Rust 中即核心类型模块。
3. `permissions/*` 与 `prompt/agent-prompts.ts` 在运行时图中**孤立**（仅测试边）。
4. `mcp/index.ts` 链完整但无上游调用方；`tools/mcp-wrapper.ts` 是平行孤立实现。

## 4. 依赖强度分层（移植顺序依据）

| 层 | 模块 | 被依赖数 | 说明 |
|---|---|---|---|
| L0 叶子 | core/types.ts | 全部 | 无依赖 |
| L1 | core/errors.ts、utils/{cost,format,streaming}、tools/bash-readonly.ts、context/token-counting.ts | 高 | 纯函数，最先移植、最先测 |
| L2 | core/api.ts、files/{cache,history}、context/{memory,session,git-context,post-compact}、permissions/*、mcp/*、skills/loader.ts、prompt/* | 中 | IO 边界 |
| L3 | tools/*（15 工具）、core/streaming-executor.ts | — | 依赖 L0-L2 + ToolContext |
| L4 | core/agent.ts | 顶 | 组装一切 |
| L5 | cli.ts、headless.ts、commands/index.ts、utils/completer.ts | — | 用户界面 |
