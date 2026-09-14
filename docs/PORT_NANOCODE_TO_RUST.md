# Port NanoCode to Rust

## 0. Mission

将当前 NanoCode 项目：

https://github.com/Lyt060814/nanocode

重新实现为一个 Rust 原生 Agent Runtime。

目标不是把 TypeScript 代码逐文件翻译成 Rust，而是：

> 保持 NanoCode 的用户可见行为与核心 Agent 能力，同时重新设计内部架构，使其成为一个 idiomatic、可测试、可扩展、长期可演进的 Rust Agent。

最终产物应当是一个可以独立运行的 Rust CLI Agent，例如：

```bash
nanocode
```

而不是一个依赖原 TypeScript runtime 的 Rust wrapper。

---

# 1. Non-goals

以下事情第一阶段禁止做：

1. 不要机械地将 `.ts` 文件逐个转换成 `.rs`。
2. 不要为了“Rust 化”而改变 NanoCode 已经验证过的 Agent 行为。
3. 不要删除现有能力，只因为实现起来麻烦。
4. 不要一开始就加入大量新功能。
5. 不要为了追求代码量少而牺牲架构边界。
6. 不要把所有逻辑塞进一个 `main.rs`。
7. 不要把 TypeScript 的类型系统机械映射为 Rust struct。
8. 不要假设“能编译”就是迁移完成。
9. 不要在没有理解原项目行为之前开始大规模重写。

第一原则：

> Understand first. Design second. Implement third. Optimize last.

---

# 2. Target

目标 Rust Agent 至少应该覆盖 NanoCode 的核心能力：

- Agent Loop
- LLM provider abstraction
- streaming response
- tool calling
- tool registry
- Bash / shell execution
- file reading
- file writing
- file editing
- glob / search
- grep / content search
- permissions
- session
- context management
- context compaction
- skills
- project instructions
- MCP
- plan mode
- sub-agent
- configuration
- CLI
- error handling
- logging
- tests

如果原项目存在其他已经形成稳定行为的核心能力，也应纳入迁移范围。

---

# 3. Phase 0 — Repository Archaeology

## IMPORTANT

这一阶段：

> **禁止修改原项目代码。**

首先完整阅读仓库。

执行：

```bash
git clone https://github.com/Lyt060814/nanocode
cd nanocode
```

分析：

```text
package.json
tsconfig*
src/**
tests/**
README*
docs/**
configuration files
```

建立以下文档：

```text
ARCHITECTURE.md
BEHAVIOR.md
DEPENDENCY_GRAPH.md
PORTING_PLAN.md
RUST_DESIGN.md
```

---

# 4. ARCHITECTURE.md

必须回答：

## 4.1 Agent Loop

找到真正的 Agent Loop。

明确：

```text
user input
    ↓
prompt construction
    ↓
LLM request
    ↓
streaming
    ↓
text / tool call
    ↓
tool execution
    ↓
tool result
    ↓
context update
    ↓
next LLM request
```

记录：

- loop 的入口
- loop 的状态
- stopping conditions
- tool call handling
- parallel tool calls
- error handling
- cancellation
- streaming
- retry
- context mutation

---

## 4.2 Model Layer

确认：

- 支持哪些模型/provider
- OpenAI-compatible API 是否存在
- Anthropic API 是否存在
- streaming 如何处理
- tool calling 如何表示
- token usage 如何获得
- model error 如何处理
- retry 如何处理

不要只记录 API 名称。

必须描述：

> Runtime 中 Model abstraction 的真正职责是什么。

---

## 4.3 Tool System

建立完整工具清单：

```text
Tool name
Input schema
Output schema
Side effects
Permission requirements
Execution model
Error model
```

画出：

```text
Agent
  ↓
Tool Registry
  ↓
Tool
  ↓
Permission
  ↓
Executor
  ↓
Result
```

明确哪些工具可以并行执行。

---

## 4