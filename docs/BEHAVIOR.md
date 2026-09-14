# BEHAVIOR.md — nanocode 用户可见行为规格（Phase 0 产出）

> 目的：把「用户能看到/感受到的行为」与实现解耦，作为 Rust 重写的**验收基线**。
> 每条均为从源码与测试提取的可观察行为。Rust 版可以改变内部结构，但本文件中的行为不得静默改变。
> 标注 ⚠ 的条目源自 dead code 或失效路径，属于"源码如此、用户不可感知"，移植时需先做决策（见 PORTING_PLAN §D）。

---

## 1. 进程级行为

| 场景 | 行为 |
|---|---|
| `nanocode` | 进入交互 REPL |
| `nanocode "prompt"` / `-p "prompt"` | one-shot：跑完输出 + cost 分隔线后退出 |
| `nanocode --version` | 打印 `nanocode 0.1.0`，退出 0 |
| `nanocode -h` / `--help` | 打印用法 + 全部 slash 命令列表（含别名 /quit），退出 0 |
| 无 API key | stderr：`Error: ANTHROPIC_API_KEY not set.` + 提示行，退出 1 |
| `--resume <id>` 会话不存在/为空 | stderr：`Session <id> not found or empty.`，退出 1 |
| key 来源 | `--api-key` > `ANTHROPIC_API_KEY` > `OPENROUTER_API_KEY`；模型默认 `ANTHROPIC_MODEL` 环境变量，否则 `sonnet` |

模型名解析：全名/别名（sonnet/opus/haiku）/双向部分匹配；未知名称**不报错**，按 sonnet 配置跑指定模型名。

## 2. REPL 交互

- 启动画面：空行 → 渐变大 logo（终端 <72 列换小版）→ 金色圆角信息框（`nanocode v0.1.0`、`Model:`、`CWD:`（HOME 缩写为 ~））→ 灰色提示 ` /help for commands · Ctrl+C abort · Ctrl+D exit`。
- 提示符：金色 `❯ `；输入以 `/` 开头时整行变品牌蓝。
- 多行输入：行尾 `\`（非 `\\`）→ 续行提示 `… `；Option/Shift+Enter 在行中换行。
- Tab：触发命令/路径建议；↑↓ 选择、Esc 关闭、Enter 接受建议（当输入尚不是完整命令时）。
- 输入队列：agent 处理中输入的行先排队，空闲后逐条处理。
- Ctrl+C：处理中 → 中止当前轮（打印 `[interrupted]`），不退出；空闲 → 打印 `(Ctrl+D to exit)`。
- Ctrl+D：等待 in-flight 处理完成后打印 `Goodbye!`，退出 0。
- 每轮结束（无论 one-shot/REPL）：打印灰分隔线 `─── Turn N | <in> in / <out> out | $<cost> ───`（有缓存时插入 `cache: X read / Y write`）。token 缩写 1.2K/1.5M；$<0.001 显示 5 位小数、<0.01 四位、<1 三位、其余两位。

## 3. 流式响应渲染

| 事件 | 终端表现 |
|---|---|
| 等待模型 | spinner：braille 帧 ⠋⠙⠹…，80ms 刷新，随机动词（"Pondering…" 等 52 个），金色 |
| assistant 文本增量 | spinner 停；按完整行缓冲，渲染 markdown-lite：代码块用 `┌─ lang ─`/`└────` 围栏 + `│` 前缀；`#{1,3}` 标题加粗蓝；`> ` 引用灰；`-/*` 列表 → 蓝 `•`；inline 高亮（`**粗体**`、`` `代码` ``、文件路径、URL、`/命令`、@提及、CLI 命令、camelCase/PascalCase/snake_case 标识符、flags、数字+单位） |
| thinking 增量 | spinner 停；灰显逐行 |
| 工具开始 | 空行 + `  ● **ToolName**  <input 摘要>`（金点、蓝名、灰摘要；Bash 显示命令前 200 字符，文件工具显示路径，其余 JSON 前 100 字符） |
| 工具结果 | `┃ ` 前缀结果框：≤20 行、每行 ≤200 字符；超出显示 `╰─ (N lines, M hidden)`；错误整块红；Edit 结果的 diff 行 +/-/@@ 上色；之后重启 spinner |
| compact | `  [compact] <old> → <new> tokens`（黄标签灰数字） |
| 错误 | stderr 红 `\n  Error: <msg>` |
| max turns | 黄 `\n  Max turns reached (N). Use /compact or continue.` |

## 4. 权限交互

非只读工具在 default 模式下触发 readline 确认：

```
  ⚡ Allow <描述>            # 如 "Bash: rm -rf /tmp/x"、"Edit: /path/file.ts"
  [y] Yes  [n] No  [a] Always  >
```

- 空输入 = y；`n/no` = deny（工具收到 `User denied`，模型看到 isError 结果）；`a/always` = 本会话内该**工具名**全部放行。
- 描述格式：`Bash: <command>`、`Edit/Write/Read: <path>`、其他 `<Tool>: <JSON 前 200 字符>`。
- one-shot 模式（无 readline）：**全允许**。
- `--dangerously-skip-permissions`：全允许，不提示。

## 5. 四种权限模式语义（路径 A，实际生效）

| 模式 | 只读工具 | Edit/Write/NotebookEdit | 非 Bash 其他写类工具 | 非 readOnly 的 Bash |
|---|---|---|---|---|
| default | ✅ | 询问 | 询问 | 询问（除非命令判定只读） |
| plan | ✅ | ❌ 拒绝（"not allowed in plan mode (read-only)"） | 询问 | 询问 |
| acceptEdits | ✅ | ✅ | 询问 | 询问 |
| bypassPermissions | ✅ | ✅ | ✅ | ✅ |

"Bash 命令判定只读" = 白名单算法（见 §7 Bash）。`/plan` 命令在 default↔plan 间切换。

## 6. Slash 命令精确行为

| 命令 | 行为 |
|---|---|
| `/help` | 列出全部命令（蓝 `/name` 补齐 12 列 + 描述）+ 尾注 |
| `/compact` | 空历史 → `Nothing to compact.`；否则 spinner "Compacting..." 调模型摘要，成功打印 `Compacted: <old> → <new> tokens`，失败抛命令错误 |
| `/clear` | 清空消息数组 → `Conversation cleared.` |
| `/model` | 无参显示当前；有参切换（立即生效，影响下轮请求） |
| `/cost` | Model/Turns/Input/Output/Cache R/Cache W/Cost 七行（数字千分位、Cost 金色 4 位小数） |
| `/resume` | 无参：最近 10 个会话（8 位 id、时间、~cwd、消息数）；带前缀：匹配第一个并加载，`Resumed session <id8> (N messages).`；不匹配报错 |
| `/plan` | 切换 plan 模式并提示（黄 `Plan mode enabled.` / `Plan mode disabled. All tools available.`） |
| `/memory` | 显示合并后 NANOCODE.md/CLAUDE.md（>3000 字符截断 + 总长）；无 → `No NANOCODE.md or CLAUDE.md found.` |
| `/config` | Model/Context/Max output/Mode/CWD/Session(8位)/Tools 数量 |
| `/status` | Session/Model/Messages/Tokens（估算/阈值+百分比）/Mode/CWD/Files mod/Snapshots |
| `/skills` | 列出技能（懒加载）；无 → 提示放置路径 |
| `/context` | 40 字符进度条 + 百分比 + Messages/System prompt/Tool schemas(150×N)/Free 估算行 |
| `/init` | 注入内置 prompt（分析代码库 → 写 NANOCODE.md，含已存在时先问用户的要求），立即作为用户输入执行 |
| `/mcp` | 无配置：说明 + 示例 JSON；有配置：`MCP Servers (N)` 列表（name + command 行）；**仅展示，不连接** ⚠ |
| `/exit`、`/quit` | `Goodbye!` 退出 0 |
| 未知 | `Unknown command: /<name>. Try /help.` |

## 7. 工具级可见行为

### Bash
- 执行环境：`bash -c`，cwd=项目目录，`GIT_PAGER/PAGER=cat`，TERM 兜底。
- 超时默认 120s，可指定至 600s；超时输出 `[Command timed out]`，**不算 isError**；SIGTERM 后 2s 强杀。
- 输出组装：stdout（>30K 截断并注明总长）→ `STDERR:` 段（>10K 截断）→ 非零退出码追加 `(exit code: N)`；完全无输出 → `(No output)` 或 `(No output, exit code: N)`。
- 只读命令白名单（免确认、可并行）：约 60 个基础命令（cat/head/tail/ls/find/grep/rg/awk/sed/tr/cut/git/jq/xxd/...），细分：git 仅 log/diff/show/status/branch/remote/tag/rev-parse/... 子命令（stash 仅 list/show）；npm/yarn/pip 仅 list/view/search 等只读子命令；node/python 等仅 `--version`；sed 禁 `-i`；find 禁 `-exec/-delete`；awk 禁 `system()` 与输出重定向；tee 仅 `/dev/null`；xargs 仅安全子命令。
- 一票否决：`$()`、反引号、`<()`/`>()`、输出重定向（`>/>>` 允许 `/dev/null`）、函数定义、`eval/exec/source`、非白名单环境变量赋值。管道/链式命令每段都需安全。引号内不分隔。

### Read
- 行号 `cat -n` 风格（右对齐 + Tab）；默认 2000 行；partial 显示 `(showing from line N)`/`(M more lines below, T total)`。
- 错误消息精确文案：`Error: file not found: <path>`；`Error: <path> is a directory, not a file. Use ls or find to list directory contents.`；`Error: permission denied: <path>`。
- 二进制（前 8K 含 NUL）→ 建议 xxd/od/strings；图片扩展名 → 说明不支持文本查看。
- UTF-16LE BOM 自动转码；UTF-8 BOM 剥除。

### Edit（6 步验证链，每步失败消息固定）
1. `old_string === new_string` → `Error: old_string and new_string are identical. No changes needed.`
2. 文件不存在且 old_string 空 → 创建：`Created new file: <path> (N lines)`；old_string 非空且文件不存在 → 提示用空 old_string 创建。
3. 未读过 → `Error: you must Read the file before editing it. ...`；部分读过且目标不在缓存 → 提示重读相关段。
4. mtime 比缓存新 >1s → `Error: file has been modified since you last read it ... Please Read the file again before editing.`
5. 精确匹配失败（但 trim 后能匹配）→ 特别提示空白/缩进问题；否则 `Error: old_string not found in <path> ...`
6. 多处匹配且未 replace_all → `Error: found N matches for old_string. To replace all occurrences, set replace_all: true. ...`
- 成功：`Edited <path> (N replacements):` + `@@ ... @@` diff 片段（3 行上下文，+/− 前缀）。
- 写入原子（同目录临时文件 + rename），父目录自动创建。

### Write
- 已存在但未 Read → `Error: file already exists at <path>. You must Read the file before overwriting it. ...`
- staleness 同 Edit；成功 `Written: <path> (N lines)`。

### Glob / Grep
- Glob：默认忽略 node_modules/.git/dist/build/target/vendor/.venv 等 16 类；≤200 条按字母序；`No files found matching pattern: <p> in <dir>`；达上限加提示行。
- Grep：rg 优先（每文件 ≤50 匹配、行长 ≤200 截断、同上忽略目录），无 rg 回落 grep -rn；`file:line:content` ≤500 行 + `(N matches)` 或截断提示；空 pattern 报错。

### Agent（子代理）
- `Explore`：只用 Read/Glob/Grep/Bash，30 turns，prompt 加只读研究前缀；`Plan`：除 Agent 外全部工具，50 turns；default：全工具 50 turns。
- 结果包装 `[Sub-agent: <description|type>]\n\n<最终 assistant 文本>`；无输出 → `Sub-agent completed but produced no response.`
- 子代理事件**不**流式显示（静默运行）。⚠ `model` 参数被忽略。

### Ask / Todo / NotebookEdit / WebFetch / WebSearch / Skill
- Ask：stderr 青色 `? <question>` + `> `，读单行；空 → `(No response from user)`；中止 → `(User interaction cancelled)`。
- Todo：内存任务表（会话内）；`[ ]`/`[~]`/`[x]` 图标 + 8 位 id；list 按 in_progress→pending→completed + 计数行。
- NotebookEdit：0-based cell 索引；code cell 编辑后清 outputs；`Edited cell N in <path>\nOld source (N chars) -> New source (M chars)`。
- WebFetch：仅 http/https；结果带 `URL:/Status:/Content-Type:` 头；HTML→markdown；>30K 截断注记；`Error: HTTP <code> <status> fetching <url>`；30s 超时报错。
- WebSearch：DuckDuckGo 后端 ≤10 条 `N. title / url / snippet`；`No search results found for: <q>` + WebFetch 建议。
- Skill：inline 直接注入展开文本；fork 跑子代理；未找到 → `Skill "<name>" not found. Available skills:\n  - <name>: <desc>...`（或"无技能"提示 + 放置路径）。

## 8. 上下文压缩行为（用户可感知部分）

- 自动触发阈值：估算 tokens ≥ `contextWindow − maxOutputTokens − 13000`（sonnet/opus/haiku 均 200K 窗口 → 阈值 170,616）。
- 触发时：摘要调用（非流式，max_tokens 8000）→ 用户看到 `[compact] <old> → <new> tokens`；失败显示 `Auto-compact failed: <msg>` 且**继续原对话**。
- 摘要格式：`[CONVERSATION_COMPACTED]` 标记 + 9 段结构（Primary Request/Key Concepts/Files and Code/Errors and fixes/Problem Solving/All user messages/Pending Tasks/Current Work/Next Step），保留最近 3 个 turn 原文。
- 每轮 micro-compact：3 turns 前的超大 tool_result（>50K 字符）被静默截为 5K + `[Content truncated: was N chars. Re-read the file if needed.]`。
- Prompt-too-long：模型报错时自动删最老 2 条消息重试，显示 `Prompt too long — truncating old messages (attempt N/3)`；3 次后 `Prompt too long after 3 truncation attempts. Try /compact.` 结束回合。
- ⚠ 压缩后**没有**文件回注（post-compact 未接线）。

## 9. 会话持久化

- 存储位置：`~/.nanocode/sessions/<uuid>/{transcript.jsonl, meta.json}`。
- 首条消息落盘时才建会话（无空会话目录）。
- resume 后从消息历史继续；REPL 显示 `Resumed session <id8> (N messages)`。
- ⚠ compact_boundary 条目设计上有、实际从不写入。

## 10. API 错误与重试（用户可见）

- 重试提示（stderr，黄）：`[retry N] <ErrorName>: <message> (waiting <X>s)`，指数退避 1s×2^n（上限 60s，±20% 抖动），最多 5 次；429 尊重 Retry-After；连续 529 三次放弃；401/403 与 PTL 不重试。
- 401/403 → `Authentication failed (N): ...`；循环终止并显示于错误行。
- 每次模型调用成功后 usage 进 CostTracker（input/output/cache read/cache write 四项）。

## 11. 环境上下文注入（模型可见，间接影响行为）

- System prompt 静态段：身份（"You are nanocode, a CLI-based coding agent…"）、最小输出 token、专用工具优先、prompt injection 警惕、安全编码、任务纪律（先读后写、不过度工程、不主动建文档）、行动风险分级、并行工具调用鼓励、简短风格（file:line 引用、无 emoji）。
- 动态段：NANOCODE.md/CLAUDE.md 合并（含向上层级与 rules 目录）、Environment（cwd/platform/shell/model/date/Node version）、Git 快照（branch/main/status(≤100 行)/log(10 条)）。

## 12. Headless/SDK 行为

`createAgent` 默认 bypassPermissions + 全允许；`query()` 产出与 CLI 相同的 StreamEvent 流；`oneShot()` 返回拼接纯文本。事件类型与 CLI 完全一致（11 种）。

## 13. Exit codes

| 情形 | 码 |
|---|---|
| 正常退出（/exit、Ctrl+D） | 0 |
| 无 API key、resume 失败、fatal | 1 |
| SIGINT（仅 signal handler 路径） | 130 |
| SIGTERM | 143 |
| `--version`/`--help` | 0 |
