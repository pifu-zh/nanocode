# nanocode (Rust)

Rust 原生重实现 —— 移植自 [Lyt060814/nanocode](https://github.com/Lyt060814/nanocode)（TypeScript, ~9K LOC）。

目标：**保持 nanocode 的用户可见行为与核心 Agent 能力**，同时以 idiomatic Rust 重建内部架构（详见任务书 `../PORT_NANOCODE_TO_RUST.md` 与 Phase 0 考古文档 `../ARCHITECTURE.md` / `../BEHAVIOR.md`）。

```
┌──────────────────────────── 终端 ────────────────────────────┐
│ cli/ (REPL · one-shot · 16 slash 命令 · 渲染 · 权限交互)      │
├──────────────────────────────────────────────────────────────┤
│ core/  agent.rs 主循环 · api.rs Anthropic SSE 客户端 ·        │
│        streaming_executor.rs 并行工具执行 · errors.rs 重试    │
│ tools/ 15 工具 + registry + bash 只读白名单校验器             │
│ context/ 压缩(3层) · 会话(JSONL) · NANOCODE.md · git 上下文   │
│ permissions/ 规则引擎(settings.json, 已接线)                  │
│ skills/ SKILL.md 发现/解析/展开/inline+fork                   │
│ mcp/ stdio JSON-RPC 客户端(启动接线, D1)                      │
│ files/ LRU 状态缓存 · 原子写 · 版本化历史(D1)                 │
└──────────────────────────────────────────────────────────────┘
```

## 快速开始

```bash
cargo build --release
```

### 智谱 BigModel（已实测验证的平台）

走智谱的 Anthropic 兼容端点（GLM-5.3 / GLM-5.3-Flash，1M 上下文）：

```bash
export ANTHROPIC_API_KEY="<你的智谱 API Key>"
export ANTHROPIC_BASE_URL="https://open.bigmodel.cn/api/anthropic"
./target/release/nanocode --model GLM-5.3-Flash        # Coding Plan (lite) 套餐
./target/release/nanocode --model GLM-5.3
```

实测（lite 套餐 GLM-5.3-Flash）：流式回答、Read/Bash 工具调用、thinking signature 回传、
prompt 缓存命中（cache read）、401 错误分类全部通过（`tests/zhipu_live.rs`，
`ZHIPU_LIVE_E2E=1 cargo test --test zhipu_live` 触发）。

### Anthropic / OpenRouter

export ANTHROPIC_API_KEY="sk-ant-..."          # 或 OPENROUTER_API_KEY
export ANTHROPIC_BASE_URL="https://openrouter.ai/api"  # 可选：OpenAI 兼容网关

./target/release/nanocode                       # 交互 REPL
./target/release/nanocode -p "fix the bug"      # one-shot
./target/release/nanocode --model opus          # 换模型
./target/release/nanocode --dangerously-skip-permissions -p "run tests"
./target/release/nanocode --resume <session-id>
```

配置目录与原版兼容：`NANOCODE.md`/`CLAUDE.md`（层级加载）、`.nanocode/settings.json`（权限规则、MCP servers）、`.nanocode/skills/*/SKILL.md`。会话文件与 TS 版双向兼容（`~/.nanocode/sessions/`）。

## 测试

```bash
cargo test        # 229 tests（226 单元/E2E + 3 智谱 live，后者需 ZHIPU_LIVE_E2E=1）
cargo clippy      # 0 warnings
```

## 与 TS 版的刻意差异（RUST_DESIGN.md §5 白名单）

1. **Dead code 做实**（PORTING_PLAN D1）：权限规则引擎、MCP 启动连接、压缩后文件回注、文件历史快照、子代理参数注入、子代理模型覆盖——这些在 TS v0.1.0 中已实现但从未接线（考古确认，含 `setAgentQueryParams` 无调用方导致 Agent 工具恒报错）。Rust 版全部生效。
2. 环境信息行 `Node version` → `Rust version`（等价替换）。
3. 工具并发限流用 Semaphore 精确实现（TS 的 Promise.race 限流有 bug，可观察行为一致：≤10 并发、结果按原序）。
4. `diff`/`turndown` 依赖未迁移（前者是死依赖；HTML→markdown 用轻量转换器，保留纯文本回落路径）。
5. **GLM/智谱适配**：thinking 块携带 `signature` 回传（GLM 的 `signature_delta` 必须随
   assistant 历史回传，否则后续请求被拒）；GLM 的 `input_tokens` 在 `message_delta` 中
   才出现（Anthropic 在 `message_start`），解码器两处都取；模型注册表含 GLM-5.3 /
   GLM-5.3-Flash（1M ctx / 128K out，套餐计费单价 0）。

其余用户可见行为（BEHAVIOR.md §1–§13）零偏差（协议层经智谱真实 API 端到端验证）。

## License

MIT（同上游）。仅用于教育与研究目的；与 Anthropic 无关联。
