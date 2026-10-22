# Agent Design Methodology — Cross-Project Synthesis

> 调研日期：2026-10-04。本文档为 **smallhead**（Rust agent）的设计提供方法论基础。
> 调研对象：`earendil-works/pi`（原 badlogic/pi-mono，2026-10 已迁至 mitsuhiko）、`anomalyco/opencode`、`openai/codex`、`big-head`（用户前作）。
> 本地仓库快照日期：pi-mono 2026-01-30 / opencode 2026-02-13。已 fetch 远程最新提交（pi → `2003871`，2026-10-04；opencode → `907b3bc`，2026-10-03；codex → `afb436d`，2026-10-04）。
> 用户已经写过 `~/Desktop/p/big-head/docs/agent-harness-source-study-20261004.md`（已读），本文是其延伸，专注于**可被 new agent 借用的方法论**，而非 big-head 的修补清单。

---

## 1. 大方向共识（Cross-Project Consensus）

三个一线项目 + big-head 在过去 9 个月里独立演进了同一套核心架构。下面 6 条是社区已收敛的"**公共底盘**"。

### 1.1 Agent loop = 嵌套两层 while + 事件流

```text
OUTER while (true):                  // 处理 follow-up 队列
    INNER while (moreToolCalls | pending):
        stream assistant response     // streaming
        if tool_calls:
            executeTools(... )        // 中间可被 steering 中断
        if not moreToolCalls:
            break INNER
        if followUps:
            pendingMessages = followUps; continue OUTER
    if no followUps: break OUTER
```

- **pi**：`packages/agent/src/agent-loop.ts` 的 `runLoop()` 是这个模式的范本（line 117–195）。
- **opencode**：`packages/opencode/src/session/prompt.ts` 的 `SessionPrompt.loop()` 演化版本，使用 stream processor 而非 for-await。
- **codex**：`codex-rs/core/src/session/turn.rs` 用 Rust 状态机表达同样结构。
- **big-head**：`crates/big-head-loop` 的 `AgentRuntime` 是同一思路，但是以 ECS 系统（system + action）表达，不是函数式循环。

**关键洞见**：outer 处理 follow-up 队列，inner 处理工具调用 + steering。Steering 和 follow-up **必须在不同边界** 接入（steering 在工具执行后立刻、follow-up 在所有工具 + steering 清空后）。

### 1.2 消息模型 = AgentMessage 联合 + 类型擦除到 LLM Message

```ts
type AgentMessage =
  | { role: "user", ... }
  | { role: "assistant", content: TextContent | ThinkingContent | ToolCall }
  | { role: "toolResult", toolCallId, content, isError }
  | { role: "notification" }   // 声明合并扩展
```

- **pi**：`packages/agent/src/types.ts` `AgentMessage` 联合；声明合并让外部扩展自定义 role。
- **opencode**：`packages/opencode/src/session/message-v2.ts` 同样的 union，但用 Effect Schema 而非 typebox。
- **转换边界**：`transformContext()` → `convertToLlm()` 严格分层（pi agent-loop.ts:204–289）。
  - `transformContext`：在 AgentMessage 层面修剪 / 注入（用户可见信息保留）。
  - `convertToLlm`：在 LLM Message 层面过滤（保留 user/assistant/toolResult）。

**Rust 启示**：用 `enum AgentMessage { User, Assistant, ToolResult, Notification, ... }` + 单独 trait `ToLlmMessage`，**不要**让 LLM 关心的字段渗透到主状态。

### 1.3 工具抽象 = `Tool<I, O>` 接口 + Schema Codec

```ts
interface Tool<I, O> {
  name: string
  label: string                  // UI 显示用
  description: string            // 模型选择依据
  input: Schema<C, I>
  output: Schema<C, O>
  execute(args, ctx): Effect<O, ToolFailure>
  toModelOutput?(args, output): Content[]    // 控制模型看到的内容
}
```

- **opencode** V2（`specs/v2/tools.md`）：强制 `execute(input, ctx: Tool.Context)` 签名，**dependencies captured at construction** → schemas 不能要求 services。
- **pi**：`AgentTool<any>` 用 typebox。`onUpdate(partialResult)` 钩子支持流式 partial output。
- **codex**：`codex-rs/core/src/tools/orchestrator.rs` 把工具编排集中（permission + sandbox + retry），工具实现只关心输入输出。
- **big-head**：`crates/big-head-ai/src/tool_definitions.rs` 直接用 JSON Schema，但有 `define_tool(rhai_script)` 让运行时脚本也能成为工具。

**Rust 启示**：
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    type Input: DeserializeOwned + JsonSchema;
    type Output: Serialize + JsonSchema;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn execute(&self, input: Self::Input, ctx: &ToolContext) -> Result<ToolResult<Self::Output>>;
}
```
但 **Trait 限制**：用 enum-dispatch（`enum ToolKind { Bash, Read, Edit, Write, Web, Custom(Box<dyn CustomTool>) }`）而不是 trait object。Big-head 的经验：`dyn Tool` 难以序列化、难以在 LLM 侧暴露 schema。

### 1.4 会话存储 = JSONL 树 + SQLite 索引

所有四个项目都收敛到这个组合。

| 项目 | 会话格式 | 元数据 / 索引 | 分支模型 |
|------|---------|--------------|---------|
| pi | JSONL tree（`id`, `parentId`） | 文件目录按 cwd 分 | 原地多分支（`/tree`） |
| opencode | JSONL tree（`id`, `parentId`） | SQLite for events/state | 原地多分支 + fork 为新 session |
| codex | JSONL（`rollout/*.jsonl`） | filesystem-only | 单链 + `--last` |
| big-head | v3 format: `checkpoint.json` + `checkpoint.blobs` + `checkpoint.journal` | ECS World snapshot + Action Log JSONL | 多 World ID（文件系统隔离） |

**关键洞见**：
- 原始 JSONL 是**不可变事件流**（append-only），永远不要原地修改。
- 派生数据（索引、聚合）放 SQLite。
- pi 的 `firstKeptEntryId` 范式：compaction 记录指向保留段起点，**原始消息保留**。这是事件溯源 + 物化视图的思路（big-head 已经识别）。

**Rust 启示**：用 `tokio::task::spawn` + 异步 writer + 周期性 fsync，不要每个事件都同步写。SQLite 用 `rusqlite` WAL 模式。big-head 的 v3 journal+blobs 二进制格式对小数据是过度设计，**smallhead 应该先用纯文本 JSONL**，等到 1MB+ 单会话时再优化。

### 1.5 权限模型 = 三态 Ruleset + 规则合并

| 项目 | 模型 | 默认行为 | 高级 |
|------|------|---------|------|
| Claude Code | 单点 ask 弹窗 | ask 每个 Bash | `--dangerously-skip-permissions` |
| opencode | `Ruleset = [{permission, pattern, action: allow|ask|deny}][]` | `*: allow` + 部分 ask | 规则叠加 + arity prefix + doom_loop 检测 |
| pi | **不弹窗** | 全开 | 走容器隔离 |
| codex | sandbox + 文件策略 | OS-level sandbox | 配置白名单 |
| big-head | OS sandbox + 显式 ECS 操作 | ECS 操作由 agent 全权 | `BIGHEAD_LAYA_SHADOW` advisory-only 限制 |

**共识**：
1. **三态**（allow/ask/deny）优于二态（allow/deny）。ask 可以由用户提升为 always。
2. **Ruleset 数组 + last-match-wins**（opencode `evaluate.ts`）比嵌套条件树好维护。
3. **pattern 支持 glob + arity prefix**：例如 `git checkout *` 这种"git 子命令级"模式远好于"每次 git 调用"。
4. **doom_loop 检测**：连续 N 次相同 (tool, args) 触发 ask。N=3 是经验值。
5. **container > popup**：Claude Code 自己也强调 `--dangerously-skip-permissions` 应在容器里用（[Anthropic 内部数据](https://www.anthropic.com/engineering/claude-code-best-practices)：sandboxing 可减少 84% 权限提示）。

**smallhead 推荐**：
- 默认走 **deny-list + ask 必要项**（不是 allow-all）。
- 提供 `--sandbox=landlock|bubblewrap|docker|none` 四档。
- doom_loop 检测保留为内置。

### 1.6 持久化执行 = Durable Harness + 原子提交

这是 2026 年最显著的架构演进。

- **pi-durable**（v1.0+）：Pico5 规范的核心。
  ```text
  Session atomically commits:
    - immutable entries
    - full task records
    - Chord-tracked documents
  Only committed state is observable.
  ```
  - 用 Chord CRDT 跟踪所有可变 JSON 状态。
  - Crash-resumable：进程死了，重开从 storage 拾起工作。
  - 8 个硬不变量（spec.md:46）包括"所有可见进展都是 durable 的"。

- **opencode V2 Session**（specs/v2/session.md）：
  ```text
  sessions.create({id?, location, ...})
  sessions.prompt({id?, sessionID, prompt, delivery?, resume?})
  ```
  - `session_input` 是**独立的 durable admission inbox**，从 Session History 解耦。
  - `PromptAdmitted` 记录可被投影、回放、复制、观察。
  - `SessionExecution.resume(sessionID)` 重新发现 Location → 缓存 SessionRunner。
  - 本地 runner **durable fails** 任何上次遗留 `running` 状态的 tool 为 "interrupted"。

**共识**：
- 进程死了不能丢工作。
- 状态要么完全提交，要么完全没发生。
- 重启 = 重放 inbox + 接着上次的 entry。
- 不要有"内存里但没落盘"的中间态作为真理来源。

**big-head 的对照落点**（来自 `agent-harness-source-study-20261004.md`）：
- 当前 big-head 已经在 ECS + Action Log 上做了类似工作，但**摘要 / 派生观察与原话的因果链**没有显式跟踪。
- "日志 parallel 标注可单独小修；不能用这个表面修正代替循环验证"。
- 验证成功只能证明结构/来源契约，不能证明摘要没有歪曲原话。

**smallhead 应该从第一天就建立可重复验证的工程测试**（不是事后补救）。

---

## 2. 项目独特的、有价值的模式

### 2.1 pi 的"Minimal Core + Packages"

> "Pi ships with powerful defaults but skips features like sub agents and plan mode. Instead, you can ask pi to build what you want or install a third party pi package that matches your workflow."

**独特价值**：
- 反"bloat-core"哲学。核心代码量小（agent core ~500 行）。
- 所有非核心功能走 package（npm `pi` keyword 标记）。
- TypeScript 扩展用 jiti 即时加载，无需构建。

**不要做的事**：把 plan mode、sub-agent、permission popup、to-do、MCP 全部塞进核心。

**smallhead 的取舍**：Rust 项目用 dynamic library 加载（`libloading`）比 TS 困难，但可以用 WASM 作为扩展 runtime，或者用配置驱动的 module 加载（参考 big-head 的 `define_system(rhai)`）。

### 2.2 opencode 的 Effect-style Pipeline

```ts
// SessionRunner issues ONE explicit llm.stream(request) per provider turn
const stream = runner.stream(request)
for await (const event of stream) {
  // ...
}
```

**独特价值**：
- 每一次 provider turn 是显式的、可记录的、可重放的。
- Local tool `running` 状态在重启时自动 fail 为 "interrupted"。
- Provider-native reasoning 只在相同模型下保留；切模型时降级为普通 text。

**smallhead 启示**：把 provider turn 抽象成显式的 `ProviderTurn { request, events, response }`，**重放性**是 first-class concern。Big-head 已经有了 `big_head_view_snapshot.json` 类似的概念，可以借鉴。

### 2.3 opencode 的 Context Epochs

```text
[Context Epoch 1]  ← immutable provider-cache baseline
  ↑ provider cache hit
[Context Epoch 2]  ← new baseline (after compaction/session move)
  ↑ provider cache hit
[Context Epoch 3]
```

**独特价值**：
- Provider cache 命中是 **$0.1 per 1M tokens** vs cache miss **$3 per 1M tokens**（Anthropic pricing）。命中 = 30 倍便宜。
- Epoch 边界 = compact 完成、session move、不兼容的 context 转换。
- 在 Epoch 内部，所有"不变"的部分都共享同一个 provider cache key。

**smallhead 启示**：在 prompt assembly 层实现 Context Epoch 跟踪。每个 assistant turn 启动时记录 `epoch_id`，如果 epoch 没变就把相同的 baseline 标记为 cached。

### 2.4 pi 的 Skills（Agent Skills Spec）

```
~/.pi/agent/skills/my-skill/SKILL.md
├── SKILL.md          # frontmatter: name, description
├── scripts/          # optional
├── references/       # optional
└── assets/           # optional
```

- `description` 是路由信号——模型看到 name+description，按需 `load_skill`。
- vague description 是反模式。
- 遵循 [agentskills.io 规范](https://agentskills.io/specification)，跨工具通用（Claude Code 也用）。

**smallhead 启示**：直接采用 agentskills.io 规范，技能目录可以放在项目根目录 `.smallhead/skills/`。

### 2.5 pi 的 Code Mode

> "Scripts call `models.generateImages()`, `image()`, `models.classify()` — model returns TS code that the harness runs against a typed tool surface."

**独特价值**：
- 模型不必每个工具调用都发起一次 LLM round-trip。
- 一个 turn 内可以组合多个工具调用，输出是一段可执行代码。
- Harness 提供 typed tool surface 给 sandboxed runtime。

**smallhead 启示**：在 Rust 里可以用 [rhai](https://github.com/rhaiscript/rhai) 或 [wasmtime](https://wasmtime.rs/) 实现类似 sandbox——big-head 已经用了 rhai。

### 2.6 pi 的 Steering vs Follow-up

```ts
agent.steer({ role: "user", content: "Stop! Do this instead." })     // 中断当前工具批
agent.followUp({ role: "user", content: "Also summarize." })        // 在所有工作结束后
```

- 两种模式："one-at-a-time"（默认，等回复）vs "all"（一次性投递）。
- Editor 里 Enter = steer，Alt+Enter = follow-up。

**smallhead 启示**：这是非常自然的 Rust 实现——用 `VecDeque<Message>` + `Notify` 唤醒。

### 2.7 pi 的 Message Type Declaration Merging

```ts
declare module "@mariozechner/pi-agent" {
  interface CustomAgentMessages {
    notification: { role: "notification"; text: string };
  }
}
```

- 用户可以扩展自己的 AgentMessage 变体而无需改核心代码。
- `convertToLlm` 决定哪些 variant 进入 LLM、哪些只留在 UI。

**Rust 启示**：在 Rust 里没有 declaration merging，但可以用 `enum CustomMessage { Notification, Spark, AmbientSignal }` + `UserExtension` trait 让 crate 用户添加自己的变体。或者更简单：用 `Box<dyn AnyMessage>` + downcast。

### 2.8 big-head 的 ECS Kernel

big-head 独一无二的差异化（在四个项目里）：

```text
ECS World
  + Action Log
  + Indexes
  + Job Queue
  + App Packages
  + Policy Engine
  + Source/Connector Graph
  + View Runtime
```

**独特价值**：
- **事实与会话分离**：facts 不是 message，是 entities。
- **动态 schema**：`define_component` 运行时定义。
- **动态 system**：`define_system` + Rhai 注册脚本作为 LLM 可见工具。
- **持久 world**：survive across restarts。

**smallhead 必须继承的核心概念**（用户明确说要"部分沿用核心关键概念"）：
1. **持久化的 "World"**：超出单次会话的事实存储。
2. **Action Log 是单独不可变流**：与 ECS 状态分离。
3. **Agent 不必内置业务模型**：通过 system + schema 注入。
4. **Tools = World 上的 operations**：与代码工具（bash, read, write）并列。

### 2.9 agent-deck-rs 的双层 ToolAdapter

```rust
// 内层：通用 capability 抽象
trait ToolAdapter {
    fn capabilities(&self) -> ToolCapabilities;   // 7 个 bool flags
    fn build_resume_command(...) -> Option<String>;
    fn parse_session_ref_from_command(...) -> ...;
    ...
}

// 外层：实际 mutate 工具配置文件
trait HookAdapter {
    fn register_hooks(&self, hook_cmd: &Path) -> Result<()>;
    fn unregister_hooks(&self) -> Result<()>;
    ...
}
```

**独特价值**：
- 新增工具 = 加一个 struct，不改 UI/runtime。
- UI/runtime **永远不 branch on tool identity**，只 branch on capability flag。

**smallhead 启示**：用 enum + match 表达工具身份，capability flag 表达能力。这是 Rust 习惯写法。

### 2.10 agent-deck-rs 的 Pure Pipeline

```rust
pub fn run_pure_pipeline(
    runtime_dir: &Path,
    packets: &[Packet],
    packet: &Packet,
    canvas_projection_enabled: bool,
) -> CoordinationPipelineOutput {  // 纯函数
    // ...
}
```

- 无 `async`，无 `self`，无 I/O。
- 调用方负责持久化。
- **测试时不需要 mock 文件系统**。

**smallhead 启示**：把"决策"和"执行"严格分离。Agent loop 的核心决策应该是 pure function，输入是 state + event，输出是新的 state + actions 列表。Actions 由 executor 异步执行。

---

## 3. 工具数量与设计的实证

| 项目 | 内置工具数 | 备注 |
|------|----------|------|
| pi | 4 (`read`, `write`, `edit`, `bash`) | 其他靠 skill + extension |
| Claude Code | ~10 | + plan mode, todo |
| opencode (build) | 3 (`bash`, `edit`, `webfetch`) + LSP fallback | + explore subagent |
| codex | ~12 | + patch apply |
| big-head | 27 ECS tools + N dynamic | 内置业务模型 |

**共识**：
- **4-8 个内置工具**是甜区。
- 工具太少的妥协：用户写 skill 包装。
- 工具太多的代价：模型选择困难 + doom_loop 风险上升。

**smallhead 推荐**：默认 5 个（`bash`, `read`, `edit`, `write`, `webfetch`），加 `ls/find/grep` 作为 bash 的常用模式包装。所有其他能力走 skill / extension / world operation。

---

## 4. Context Engineering 共识

四个项目的 compaction 策略：

| 项目 | 触发阈值 | 策略 | 备注 |
|------|---------|------|------|
| pi | `contextTokens > contextWindow - reserveTokens` (reserve 16,384) | 5 步算法：walk backwards → extract cut → summarize → append CompactionEntry → rebuild from `firstKeptEntryId` | **关闭 prompt cache writes**（一次性提示，不太可能复用） |
| opencode | 默认 80% (configurable) | 调 compaction agent 替换旧消息 | 也支持 branch summary |
| codex | 内部阈值不公开 | provider API 调用自动 compact | |
| big-head | `TokenBudget::Exceeded` 时 | 一次性警告 + early return | 还未实现自动 compact |

**共识**：
- 自动 compact 阈值 = 70-80%（codex 更激进 95%，opencode 80%）。
- 摘要不能覆盖原话——原话保留在 JSONL 中。
- "Anchored-iterative summarization"（pi 的 5 步）优于"regenerative from scratch"。
- **保留原始 token 引用**（anchor）让摘要可验证。

**Artifact preservation 是普遍弱点**：所有项目在"保留 artifact trail"上都只拿 2.2-2.5 / 5.0（社区调研分）。Smallhead 在这点上可以从设计第一天就做好。

---

## 5. 实操建议（Smallhead 落地清单）

按优先级排列——P0 是必须做的，P1 是建议的，P2 是 nice-to-have。

### P0: 必须做

1. **AgentMessage 联合 + 严格 convertToLlm 边界**。Rust 用 enum + 单独 trait。
2. **事件流 + 嵌套两 while loop**。Outer 处理 follow-up，inner 处理 tool + steering。
3. **JSONL 树 + 父 ID**。append-only，原话永不被修改或删除。
4. **三态 permission ruleset**。Deny > Ask > Allow + 规则叠加 + arity prefix。
5. **Doom loop 检测**。连续 3 次相同 (tool, args) 触发 ask。
6. **Token budget 估算 + 早期警告**。每波 LLM 调用前计算 + 发出 TokenBreakdown event。
7. **工具流式 partial output**。长任务支持 `onUpdate` 回调。
8. **Bash tool 用 arity prefix 解析**。把 `git checkout *` 这种模式作为最小"always"粒度。
9. **可重复验证的工程测试**（来自 big-head 的核心教训）。固定输入序列 + 实际循环 + 独立读取临时文件，不在测试里复制业务逻辑。

### P1: 强烈建议

10. **Durable harness layer**：进程死了不丢工作。原子提交 entries + task records + World documents。
11. **Context Epoch tracking**：在 prompt assembly 层跟踪 epoch，标记可缓存的 baseline。
12. **Output truncation**：超过 N tokens 的工具输出写到磁盘 + 返回 stub（opencode Truncate）。
13. **Agents Skills spec 兼容**。`.smallhead/skills/` 目录。
14. **Plugin/extension 钩子系统**：`before_tool_call`, `after_tool_call`, `before_prompt`, `after_response`。Rust 用 trait object。
15. **Spec-driven specs/ 目录**：参考 agent-deck-rs 的 `specs/NN-name.md` 模式。
16. **Atomic write**：sessions.json 写 .tmp → sync_all → rename。
17. **Provider-agnostic interface**：不绑定 Anthropic；至少支持 Anthropic + OpenAI + 本地（Ollama）。

### P2: Nice-to-have

18. **Code mode**：rhai 或 wasmtime sandbox，让模型输出可执行代码。
19. **Subagent = 隔离的 session + 精简 permissions**。
20. **Tree-sitter bash 解析**：精确提取 mutating commands + 外部目录。
21. **Client/server 拆分**：可选，本地优先；将来支持 mobile/web client。
22. **Pure pipeline 模式**：核心决策函数是 pure，测试无 I/O。
23. **LSP 集成**：参考 opencode 的大 registry，但只支持最常用 5 种（ts, py, rs, go, vue）。
24. **Plugin discoverable via glob**：参考 opencode 的 `tool/*.{js,ts}` → 动态 import。
25. **Web UI 作为可选 frontend**（参考 big-head 的 R3F 3D spatial UI——smallhead 应该用更轻的 HTML+CSS，不做 3D）。

---

## 6. 公开的争议与未解之谜

社区未达成共识的点：

| 议题 | 阵营 A | 阵营 B |
|------|--------|--------|
| 权限 popup | Claude Code（必弹） | Pi（不弹，全权给 agent） |
| 客户端拆分 | opencode（server/TUI/web/desktop 全分离） | pi/codex（单进程） |
| Sub-agent | opencode（task tool + 独立 session） | pi（拒绝内置，让用户用 tmux） |
| MCP | Claude Code / opencode（内置） | pi（拒绝，写 CLI 工具代替） |
| 内存模型 | pi-durable（CRDT + atomic commit） | opencode V2（inbox + history 分离） |
| TUI 还是 GUI | pi/codex/opencode（TUI） | big-head（macOS native + R3F） |

**smallhead 应该明确表态**：
- 走 **Pi 哲学的 minimal core + 严格的 ECS kernel**。
- 走 **opencode 的 V2 Session 模型**（durable admission inbox）。
- 走 **Claude Code 的 permission popup**（但只在 destructive op 时）。
- 走 **pi 的 Skills** + **agent-deck-rs 的 ToolAdapter** 抽象。
- **不**做 3D Spatial UI（big-head 已有）。
- **不**做 macOS native app（big-head 已有）。
- **做** TUI（ratatui）+ Web UI（轻量 HTML）。

---

## 7. 引用与延伸阅读

### 主要项目仓库
- pi: https://github.com/earendil-works/pi (2026-10-04)
- opencode: https://github.com/anomalyco/opencode (2026-10-03)
- codex: https://github.com/openai/codex (2026-10-04)
- big-head: 本地 ~/Desktop/p/big-head

### 关键 spec 文档
- pi-durable spec: https://github.com/earendil-works/pi/blob/main/packages/durable/docs/spec.md
- opencode V2 session: https://github.com/anomalyco/opencode/blob/dev/specs/v2/session.md
- opencode V2 tools: https://github.com/anomalyco/opencode/blob/dev/specs/v2/tools.md
- Agent Skills spec: https://agentskills.io/specification

### 关键博客
- Mario Zechner "What if you don't need MCP?": https://mariozechner.at/posts/2025-11-02-what-if-you-dont-need-mcp/
- Mario Zechner "pi-coding-agent": https://mariozechner.at/posts/2025-11-30-pi-coding-agent/
- Anthropic "Claude Code Best Practices": https://www.anthropic.com/engineering/claude-code-best-practices
- Anthropic "Building Effective Agents": https://www.anthropic.com/research/building-effective-agents

### 本地参考文档
- `~/Desktop/p/big-head/docs/agent-harness-source-study-20261004.md` — 用户已写的 Pi/Codex/OpenCode 对照
- `~/Desktop/p/big-head/CLAUDE.md` — big-head 项目说明
- `~/Desktop/p/big-head/big-plan/runtime-model.md` — big-head runtime 模型
- `~/Desktop/p/big-head/docs/checkpoint-lifecycle.md` — 持久化设计
- `~/Desktop/p/big-head/docs/spatial-ui-design.md` — UI 设计决策

---

## 8. 小结：smallhead 应该站的位置

**smallhead = 最小可扩展的 Rust agent 核心 + 可选 durable 引擎 + 严格的 ECS kernel 集成 + Pi-style minimal philosophy。**

差异化主张：

1. **ECS kernel 作为 first-class 公民**（继承自 big-head）：facts 不在 messages 里，在 entities/components 里。
2. **Durable harness + Context Epochs**（继承自 pi-durable / opencode V2）：进程死了不丢工作，provider cache 友好。
3. **Pi-style minimal core**：4 个内置工具，其余走 skill/extension/world operation。
4. **Provider-agnostic**：Anthropic, OpenAI, Google, Ollama 都平等。
5. **TUI + Web（无 3D）**：ratatui + 简单 HTML UI。

**smallhead 不做的事**（避免大爆炸）：
- ❌ 不做 3D Spatial UI（big-head 已有 R3F）
- ❌ 不做 macOS native app（big-head 已有 AppKit）
- ❌ 不做 A2A bridge 协议（big-head 已有，简单复用）
- ❌ 不做 dynamic Rhai scripting（big-head 已有，smallhead 用更受限的 wasmtime 或 rhai 子集）
- ❌ 不做 multi-mode ambient product（big-head 已有 Dwelling/Work/Chat/Notes）

**smallhead 做的事**：
- ✅ Rust crate workspace，**类似 big-head 的分层**
- ✅ TUI 用 ratatui（不是 termimad + 自写）
- ✅ Session/agent loop 直接搬 pi 的 agent-loop.ts 模式
- ✅ Permission ruleset 用 opencode 的 evaluate + arity
- ✅ Skill 目录遵循 agentskills.io
- ✅ ECS kernel 用 bevy_ecs（继承自 big-head）
- ✅ Durable layer 在最顶层，可选启用
