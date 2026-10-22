# smallhead — Rust Agent 架构设计

> 版本：v0.1 — 2026-10-04
> 配套阅读：`AGENT_METHODOLOGY.md`（已写）+ `~/Desktop/p/big-head/CLAUDE.md`（big-head 既有设计）
> 目标：**小型、清晰、可扩展的 Rust agent 核心**，把 big-head 的"ECS kernel + 持久 world"差异化优势搬到一个最小化、Pi 哲学的 Rust 工作台上。

---

## 0. TL;DR（一页结论）

| 维度 | 决策 |
|------|------|
| 语言 | Rust 2024 edition（stable + MSRV 1.85） |
| 核心 crate 数 | 6 个 |
| Agent loop 模型 | pi agent-loop.ts 的 nested 2-while（Rust 端口 + futures） |
| 消息模型 | `AgentMessage` enum + 严格 `to_llm_message()` 边界 |
| 会话持久化 | JSONL append-only + SQLite 索引（pi + opencode 共识） |
| Permission | `Ruleset = [{pattern, action: allow\|ask\|deny}]` + arity prefix + doom_loop |
| LLM Provider | Anthropic / OpenAI / Google / Ollama，统一 `Provider` trait |
| 内置工具 | 5 个：`bash` / `read` / `edit` / `write` / `webfetch` |
| Skill | `agentskills.io` 兼容，`.smallhead/skills/` |
| ECS Kernel | **沿用 big-head** bevy_ecs vendored + 精简 tool surface |
| Durable Layer | opencode V2 Session 模型：inbox + history 分离 |
| TUI | ratatui（不是 big-head 的 termimad 自写） |
| Web UI | 轻量 HTML + HTMX（不是 big-head 的 R3F） |
| MCP | 通过独立 SDK 连接管理器接入，状态在服务页展示；详见 `docs/mcp-integration.md` |
| **不做** | 3D Spatial UI / macOS native / Multi-mode ambient |

---

## 1. 设计哲学

### 1.1 smallhead ≠ big-head 缩小版

| 维度 | big-head | smallhead |
|------|---------|-----------|
| 产品形态 | macOS app + 3D Spatial UI + 多 mode | CLI + TUI + 轻量 Web |
| 工具数 | 27 ECS tools + N dynamic Rhai | 5 + ECS world ops（11 类 world operation） |
| 脚本 | Rhai hot reload | **不内置**——走 skill + extension |
| UI 渲染 | AppKit + R3F | ratatui + HTMX |
| 用户画像 | "World+Scenario+Lens" 沉浸式 | TuTKI | 命令行优先，开发者场景 |
| Durable | v3 checkpoint 二进制 | **V2 Session inbox**（更成熟） |
| Diff 哲学 | "agent 可以修改 ECS" | "agent 可以修改 ECS，但任何 mutation 都需要 traceable reason" |

### 1.2 沿用 big-head 的核心关键概念

big-head 的差异化是 **ECS kernel 作为 first-class 公民**——facts 不在 message thread 里，在 entities/components 里。Smallhead 保留这一点：

1. **持久 World**：跨会话存活的事实存储。big-head 的 `World` + `Action Log` 设计是 solid 的，smallhead 复制其 schema 设计但简化 commit 协议。
2. **Action Log 独立不可变流**：与 ECS 状态严格分离。Append-only JSONL。
3. **Tools = World 上的 operations**：bash/read/write 与 ECS 系统工具并列。
4. **可重复验证的因果追踪**：从 big-head 的 `agent-harness-source-study-20261004.md` 教训中，**必须在设计第一天**就保证"摘要不能歪曲原话"的工程测试。

### 1.3 不沿用 big-head 的部分

1. ❌ **不做 3D Spatial UI**：big-head 的 R3F UI 是它的独有差异化，smallhead 不复制。
2. ❌ **不做 macOS native app**：big-head 用 AppKit，smallhead 是 CLI 优先。
3. ❌ **不做多 mode ambient product**：big-head 的 Dwelling/Work/Chat/Notes 不在 smallhead 范围内。
4. ❌ **不做 Rhai hot-reload**：big-head 允许运行时脚本作为 LLM 可见工具；smallhead 用受控 wasmtime sandbox 或直接不支持。
6. ❌ **不做 binary v3 checkpoint**：big-head 的 checkpoint.json + blobs + journal 是为大数据优化；smallhead 直接 JSONL，1MB 以下无差别。

---

## 2. Crate Workspace 布局

```
smallhead/
├── Cargo.toml                  # workspace manifest
├── rust-toolchain.toml         # MSRV pin
├── README.md
├── AGENT_METHODOLOGY.md        # 已写 ✓
├── ARCHITECTURE.md             # 本文件
├── ROADMAP.md                  # 下一步写
├── crates/
│   ├── smallhead-core/         # 核心：消息、事件、agent loop、provider
│   ├── smallhead-tools/        # 内置 5 工具 + world operations
│   ├── smallhead-permission/   # ruleset 评估 + doom_loop 检测
│   ├── smallhead-session/      # JSONL + SQLite + branch tree
│   ├── smallhead-ecs/          # bevy_ecs vendored + world API
│   ├── smallhead-skill/        # agentskills.io 兼容的 skill loader
│   ├── smallhead-cli/          # CLI + ratatui TUI
│   ├── smallhead-server/       # 可选 HTTP/WS server（web UI 用）
│   └── smallhead-web/          # 轻量 HTML + HTMX 静态资源
├── specs/                      # spec-driven docs (agent-deck-rs style)
│   ├── 01-message-model.md
│   ├── 02-agent-loop.md
│   ├── 03-tool-model.md
│   ├── 04-session-storage.md
│   ├── 05-permission-model.md
│   ├── 06-ecs-kernel.md
│   ├── 07-provider-interface.md
│   ├── 08-durable-layer.md
│   ├── 09-skills.md
│   └── 10-cli-and-tui.md
└── examples/
    └── hello-tool/              # 写一个最小 plugin example
```

### 2.1 各 crate 职责

| Crate | 职责 | 关键依赖 |
|-------|------|---------|
| `smallhead-core` | `AgentMessage` enum, `Event`, `AgentLoop` (pure pipeline), `Provider` trait | `tokio`, `serde`, `async-trait`, `futures` |
| `smallhead-tools` | 内置 5 工具 + world ops | `smallhead-core`, `smallhead-permission`, `tokio::process`, `reqwest` |
| `smallhead-permission` | `Rule`, `Ruleset`, `evaluate`, `doom_loop::Detector` | `regex`, `glob` |
| `smallhead-session` | JSONL append-only + SQLite index + branch tree | `tokio::fs`, `rusqlite`, `serde_json`, `uuid` |
| `smallhead-ecs` | 复用 big-head 的 vendored bevy_ecs；封装 `WorldOp` API | `bevy_ecs` (vendored) |
| `smallhead-skill` | agentskills.io loader | `tokio::fs`, `serde_yaml`, `walkdir` |
| `smallhead-cli` | CLI 入口 + ratatui TUI | `clap`, `ratatui`, `crossterm`, `smallhead-core` |
| `smallhead-server` | HTTP/WS server（可选） | `axum`, `tower`, `smallhead-core` |
| `smallhead-web` | 静态 HTML 资源（不进 server binary） | 无 |

### 2.2 依赖方向（强制规则）

```text
smallhead-cli / smallhead-server / smallhead-web
  ↓
smallhead-core, smallhead-session, smallhead-permission, smallhead-skill
  ↓
smallhead-tools (依赖 core, permission)
  ↓
smallhead-ecs (vendored bevy_ecs，独立)
```

**禁止**：
- `smallhead-core` 不能依赖 `smallhead-tools`（否则循环）。
- `smallhead-tools` 不能依赖 `smallhead-cli`。
- 任何 crate 不能依赖 `smallhead-server`（它是 runner）。

---

## 3. 核心抽象（AgentMessage + Provider + Tool）

### 3.1 `AgentMessage` enum（smallhead-core/src/message.rs）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User { content: Vec<ContentBlock>, timestamp: i64 },
    Assistant { content: Vec<ContentBlock>, tool_calls: Vec<ToolCall>, model: String },
    ToolResult { tool_call_id: String, content: Vec<ContentBlock>, is_error: bool },
    /// 跨 AgentMessage 边界传递的"模型不该看到"信息
    Notification { kind: String, payload: serde_json::Value },
    /// Compaction 边界标记
    Compaction { summary: String, first_kept_entry_id: String, anchor_refs: Vec<AnchorRef> },
    /// Branch / fork 标记
    BranchMarker { parent_id: String, label: String },
    /// Steering（用户中断）
    Steering { content: Vec<ContentBlock> },
    /// Follow-up（用户追加，等当前 turn 完成）
    FollowUp { content: Vec<ContentBlock> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
    Thinking { text: String, signature: Option<String> },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_call_id: String, content: Vec<Box<ContentBlock>>, is_error: bool },
}

pub trait ToLlmMessage {
    fn to_llm(&self) -> Option<LlmMessage>;
}
```

**关键不变量**：
- `convertToLlm`（pi 命名，Rust 用 `to_llm`）只保留 `User` / `Assistant` / `ToolResult`。
- `Notification` / `Steering` / `FollowUp` / `Compaction` / `BranchMarker` 是 Agent 内部状态，**不进入 LLM context**。
- `Notification` 走 JSONL 但不进 prompt。

### 3.2 `Provider` trait（smallhead-core/src/provider.rs）

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;                                    // "anthropic", "openai", ...
    fn model(&self) -> &str;
    fn context_window(&self) -> usize;
    
    /// 单 turn 流式响应
    async fn stream(
        &self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>>;
    
    /// 用于 Context Epoch 的 cache-key 计算
    fn cache_key(&self, request: &ProviderRequest) -> String;
}

pub struct ProviderRequest {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: usize,
    pub temperature: f32,
}

pub enum ProviderEvent {
    MessageStart { id: String, model: String },
    ContentBlockStart { index: usize, block: ContentBlock },
    ContentBlockDelta { index: usize, delta: String },
    ContentBlockStop { index: usize },
    MessageDelta { usage: Usage, stop_reason: StopReason },
    MessageStop,
    Error { error: ProviderError, retryable: bool },
}
```

**关键设计点**：
- 显式 `stream()` 而不是返回 `reqwest::Response`：每事件可记录、可重放（opencode V2 设计）。
- `cache_key()` 显式表达 —— 这是 Context Epoch 跟踪的基础。
- 实现：`AnthropicProvider` / `OpenAiProvider` / `GoogleProvider` / `OllamaProvider` 平级。

### 3.3 `Tool` enum-dispatch（smallhead-core/src/tool.rs）

```rust
pub enum ToolKind {
    Bash(BashTool),
    Read(ReadTool),
    Edit(EditTool),
    Write(WriteTool),
    WebFetch(WebFetchTool),
    /// World operations（见 §4）
    WorldOp(WorldOpTool),
    /// 用户注册的 dynamic tool
    Dynamic(Box<dyn DynamicTool>),
    /// Skill-loaded tool
    Skill(SkillTool),
}

pub struct ToolSpec {
    pub name: String,
    pub label: String,
    pub description: String,
    pub input_schema: serde_json::Value,    // JSON Schema
    pub output_schema: Option<serde_json::Value>,
}

impl ToolKind {
    pub fn spec(&self) -> ToolSpec { /* ... */ }
    pub async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult> { /* dispatch */ }
}
```

**关键决策**：
- **不用 trait object `Box<dyn Tool>`**（这是 big-head 的痛点：序列化、schema 注册都不友好）。
- 用 enum-dispatch + `match`，动态部分用 `Box<dyn DynamicTool>` 兜底。
- Schema 直接存 JSON Value，**不**走 Rust 反射（学 opencode V2）。

---

## 4. ECS Kernel 集成（smallhead-ecs）

这是 big-head 的核心差异化，必须沿用。

### 4.1 沿用 big-head 的 vendored bevy_ecs

```toml
# smallhead-ecs/Cargo.toml
[dependencies]
bevy_ecs = { path = "vendor/bevy_ecs-0.16" }
```

**决策**：复用 big-head 的 vendored 路径，不引入上游依赖（上游版本差异风险）。

### 4.2 `WorldOp` API（11 类）

```rust
// smallhead-ecs/src/world_op.rs
pub enum WorldOpKind {
    QueryEntities,       // SELECT
    GetComponent,        // GET
    InsertComponent,     // INSERT
    RemoveComponent,     // DELETE
    UpdateComponent,     // UPDATE (with predicate)
    SpawnEntity,         // CREATE
    DespawnEntity,       // DROP
    AddRelation,         // GRAPH EDGE
    RemoveRelation,
    DefineComponent,     // SCHEMA CREATE
    RunSystem,           // PROCEDURAL
}

pub struct WorldOpTool {
    pub kind: WorldOpKind,
    pub world: Arc<Mutex<World>>,
    pub permission: Permission,        // "ecs:mutate" / "ecs:read"
}
```

**所有 ECS mutations 都通过 `WorldOp`**，**不**让 agent 直接拿到 World 引用。
这是 big-head 已经验证的设计：LLM 看到的工具 surface 是受控的。

### 4.3 持久化契约

```rust
// smallhead-ecs/src/checkpoint.rs
pub struct WorldCheckpoint {
    pub world_id: String,
    pub version: u64,                          // monatomic
    pub world: serde_json::Value,               // ECS state snapshot
    pub action_log_digest: String,              // sha256 of action log up to version
    pub timestamp: i64,
}

pub enum CommitResult {
    Committed { version: u64, checkpoint_id: String },
    Conflict { expected_version: u64, actual_version: u64 },
}
```

**Action Log 不进 checkpoint**——它是不可变流（JSONL），checkpoint 是它的 snapshot。

**big-head 的改进点**（来自 big-head 自我反思文档）：
- 原始 Action Log 必须保留（已 OK）。
- **Checkpoint 必须包含 original token references**（`anchor_refs`）—— 这样摘要可验证。
- 测试：写一个"还原测试"，固定输入 → 实际循环 → 独立 reader 读取临时文件 → 验证 world state 一致。

---

## 5. Agent Loop（smallhead-core/src/agent_loop.rs）

这是从 pi agent-loop.ts 移植到 Rust 的版本。

### 5.1 主循环

```rust
pub struct AgentLoop {
    state: Arc<Mutex<LoopState>>,
    provider: Arc<dyn Provider>,
    tools: Vec<ToolKind>,
    ruleset: Ruleset,
    session: Arc<SessionStore>,
    world: Arc<Mutex<World>>,
    doom_loop: DoomLoopDetector,
    steering_tx: mpsc::Sender<AgentMessage>,
    steering_rx: mpsc::Receiver<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    follow_up_rx: mpsc::Receiver<AgentMessage>,
}

pub async fn run(mut self) -> Result<()> {
    // OUTER while: 处理 follow-up 队列
    loop {
        // 主对话 turn
        self.run_one_turn().await?;
        
        // 检查 follow-up 队列
        let follow_ups = self.drain_follow_ups().await;
        if follow_ups.is_empty() {
            break;
        }
        self.state.pending_messages = follow_ups;
    }
    Ok(())
}

async fn run_one_turn(&mut self) -> Result<()> {
    // INNER while: 处理 tool + steering
    loop {
        // 1. 转换 context: transformContext
        let ctx_msgs = self.transform_context().await?;
        // 2. 转换到 LLM: convertToLlm
        let llm_msgs: Vec<LlmMessage> = ctx_msgs.iter().filter_map(|m| m.to_llm()).collect();
        
        // 3. 调用 provider
        let request = ProviderRequest { messages: llm_msgs, ... };
        let stream = self.provider.stream(request, self.cancel.clone()).await?;
        
        // 4. 收集 assistant 消息 + tool calls
        let assistant = self.collect_assistant(stream).await?;
        
        // 5. 如果没有 tool calls, INNER 结束
        if assistant.tool_calls.is_empty() {
            self.commit_message(AgentMessage::Assistant { ... }).await?;
            return Ok(());
        }
        
        // 6. 执行工具（中间可被 steering 中断）
        for tc in &assistant.tool_calls {
            // 6a. 检查 steering
            if let Some(steer) = self.peek_steering().await {
                self.commit_message(steer).await?;
                self.commit_message(AgentMessage::Assistant { ... }).await?;
                return Ok(());
            }
            
            // 6b. 评估 permission
            let decision = self.ruleset.evaluate(&tc.name, &tc.input, &self.doom_loop);
            if !matches!(decision.action, Action::Allow) {
                self.handle_permission_decision(tc, decision).await?;
                continue;
            }
            
            // 6c. 执行
            let result = self.execute_tool(tc).await?;
            
            // 6d. 检查 doom_loop
            self.doom_loop.record(tc);
            if self.doom_loop.triggered() {
                self.handle_doom_loop().await?;
            }
            
            // 6e. 提交 tool result
            self.commit_message(AgentMessage::ToolResult { ... }).await?;
        }
        
        // 7. 检查 follow-up（如果有就退出 INNER，让 OUTER 处理）
        if !self.follow_up_rx.is_empty() {
            return Ok(());
        }
    }
}
```

### 5.2 Pure Pipeline 模式（来自 agent-deck-rs）

`AgentLoop::run` 的核心**决策部分**应该是 pure function：

```rust
/// 输入：当前 state + 一个 event
/// 输出：新 state + actions 列表（无 I/O）
pub fn step(state: &LoopState, event: LoopEvent) -> (LoopState, Vec<Action>);

/// actions 由 executor 异步执行
pub async fn execute_actions(actions: Vec<Action>, ctx: &Context) -> Result<Vec<Effect>>;
```

**收益**：
- `step()` 测试无需 mock 文件系统/LLM。
- 重放、调试、stop、resume 都是线性 this.

### 5.3 Steering vs Follow-up

```rust
// 从 TUI / RPC 接收
pub async fn steer(&self, msg: AgentMessage) -> Result<()> {
    self.steering_tx.send(msg).await?;
    Ok(())
}

pub async fn follow_up(&self, msg: AgentMessage) -> Result<()> {
    self.follow_up_tx.send(msg).await?;
    Ok(())
}
```

**语义**：
- `steer`：插入**当前 turn 中**——在下一个 tool call 检查点被处理（pi 行为）。
- `follow_up`：插入**下一个 turn 之前**——所有当前 turn 完成后才看到。

---

## 6. Permission Model（smallhead-permission）

来自 opencode 的 Ruleset 设计 + Claude Code 的 arity prefix。

### 6.1 Rule 结构

```rust
pub struct Rule {
    pub permission: Permission,
    pub pattern: Pattern,                       // glob + arity prefix
    pub action: Action,
}

pub enum Action { Allow, Ask, Deny }

pub enum Permission {
    Bash,
    Read,
    Write,
    Edit,
    Webfetch,
    EcsRead,
    EcsMutate,
    /// Skill / extension 工具
    Custom(String),
}

pub enum Pattern {
    /// 精确匹配: "Read"
    Exact(String),
    /// 前缀匹配: "Read .md"
    Prefix(String, String),
    /// Glob: "Bash git *"
    Glob(String, String),
    /// Tree-sitter 解析的 command shape: ["git", "checkout", "*"]
    CommandShape(Vec<String>),
}
```

### 6.2 评估

```rust
impl Ruleset {
    /// last-match-wins
    pub fn evaluate(&self, perm: &Permission, input: &serde_json::Value, ctx: &EvalContext) -> Decision {
        for rule in self.rules.iter().rev() {
            if rule.matches(perm, input, ctx) {
                return Decision { action: rule.action, reason: rule.reason() };
            }
        }
        Decision { action: Action::Ask, reason: "no matching rule" }
    }
}
```

### 6.3 Doom Loop Detector

```rust
pub struct DoomLoopDetector {
    threshold: usize,                            // 默认 3
    history: VecDeque<(String, String)>,        // (name, args_hash)
}

impl DoomLoopDetector {
    pub fn record(&mut self, name: &str, args: &serde_json::Value) {
        let hash = sha256_short(args);
        self.history.push_back((name.to_string(), hash));
        if self.history.len() > self.threshold * 2 {
            self.history.pop_front();
        }
    }
    
    pub fn triggered(&self) -> bool {
        self.history.iter().rev().take(self.threshold).all(|x| Some(x) == self.history.back())
    }
}
```

---

## 7. Session 持久化（smallhead-session）

JSONL tree + SQLite index，opencode V2 模型。

### 7.1 JSONL Layout

```
~/.local/share/smallhead/sessions/
└── {cwd-hash}/
    └── {session-id}/
        ├── entries.jsonl         # append-only
        ├── checkpoints/          # ECS world snapshots (sparse)
        │   └── {version}.json
        ├── meta.sqlite           # SQLite index
        └── attachments/            # tool outputs > N tokens
            └── {file-id}.bin
```

### 7.2 Entry Schema

```rust
pub struct Entry {
    pub id: String,                            // uuid v7
    pub parent_id: Option<String>,             // 形成树
    pub timestamp: i64,
    pub kind: EntryKind,
}

pub enum EntryKind {
    Message(AgentMessage),
    Compaction { first_kept_entry_id: String, summary: String, anchor_refs: Vec<AnchorRef> },
    BranchMarker { label: String },
    ProviderTurn { request_hash: String, events: Vec<ProviderEvent> },
    ToolCall { tool_call_id: String, name: String, input: serde_json::Value },
    ToolResult { tool_call_id: String, output_summary: String, full_output_path: Option<String> },
    Checkpoint { version: u64, world_snapshot_path: String },
    WorldMutation { entity_id: u64, component: String, before: Value, after: Value },
    DoomLoopTriggered { tool: String, count: usize },
    PermissionDecision { tool: String, action: Action, decided_by: String },
    /// 错误永远保留
    Error { kind: String, message: String, recoverable: bool },
}
```

### 7.3 SQLite Index

```sql
CREATE TABLE entries (
    id TEXT PRIMARY KEY,
    parent_id TEXT REFERENCES entries(id),
    timestamp INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload JSON NOT NULL
);

CREATE INDEX idx_parent ON entries(parent_id);
CREATE INDEX idx_timestamp ON entries(timestamp);
CREATE INDEX idx_kind ON entries(kind);

CREATE VIRTUAL TABLE entries_fts USING fts5(id, content);
```

**写入策略**：
- `entries.jsonl` 是真理来源（append-only，fsync 每 5 条或 1s）。
- SQLite 异步写入（best-effort，崩溃可重建）。
- 启动时如果 SQLite 缺，从 JSONL 重建。

### 7.4 Compaction

```rust
pub struct Compactor {
    pub context_window: usize,
    pub reserve_tokens: usize,                  // 默认 16,384
    pub summary_model: String,                  // 默认 fast model
}

impl Compactor {
    /// pi 的 5 步算法
    pub async fn compact(&self, session: &SessionStore) -> Result<CompactionEntry> {
        let entries = session.all_entries();
        let total_tokens = self.estimate_tokens(&entries);
        if total_tokens < self.context_window - self.reserve_tokens {
            return Ok(/* no-op */);
        }
        
        // 1. Walk backwards 找到 cut point
        let cut = self.find_cut_point(&entries, total_tokens - (self.context_window - self.reserve_tokens));
        
        // 2. Extract anchor refs (被引用的 entry IDs)
        let anchor_refs = self.extract_anchors(&entries[..cut]);
        
        // 3. 调用 LLM 摘要（保留 anchor refs）
        let summary = self.summarize(&entries[..cut], &anchor_refs).await?;
        
        // 4. Append CompactionEntry
        let comp = CompactionEntry {
            first_kept_entry_id: entries[cut].id.clone(),
            summary,
            anchor_refs,
        };
        session.append(EntryKind::Compaction(comp.clone())).await?;
        
        // 5. 重建 prompt (从 first_kept_entry_id 起 + summary block)
        // （这一步在 transform_context() 时按需做）
        Ok(comp)
    }
}
```

**关键不变量**（来自 big-head 的核心教训）：
- 原始 entries 永不删除。
- CompactionEntry 必须包含 `anchor_refs`（被引用 entry IDs）—— 这样**摘要可独立验证**。
- 工程测试：还原 compaction 前的 entries → 应用 summary → 验证还原后的内容 + anchor_refs 一致。

---

## 8. Durable Layer（可选启用）

opencode V2 Session 模型 + pi-durable 的原子提交原则。

### 8.1 核心抽象

```rust
pub struct DurableSession {
    inbox: PathBuf,                              // 不可变 admission inbox
    history: PathBuf,                            // 不可变 history
    checkpoints: PathBuf,                        // ECS snapshots
    pub session_id: String,
}

pub enum SessionInput {
    Prompt { id: String, content: AgentMessage, delivery: Delivery, resume: bool },
    /// 模型选择变更
    SwitchModel { model: String },
    /// 用户主动中断
    Cancel,
    /// 重启信号
    Resume,
}

pub enum Delivery {
    /// 等当前 turn 完成
    Steer,
    /// 等当前 turn + follow-up 都完成
    FollowUp,
    /// 立刻投递（打断当前 turn）
    Interrupt,
}
```

### 8.2 原子提交

```rust
impl DurableSession {
    /// 原子提交一组 entry
    pub async fn commit_atomic(&self, entries: Vec<Entry>) -> Result<CommitReceipt> {
        // 1. 写临时文件
        let tmp = self.history.with_extension("jsonl.tmp");
        let mut f = OpenOptions::new().append(true).open(&tmp).await?;
        for entry in &entries {
            f.write_all(&serde_json::to_vec(entry)?).await?;
            f.write_all(b"\n").await?;
        }
        f.sync_all().await?;
        drop(f);
        
        // 2. 原子 rename
        tokio::fs::rename(&tmp, &self.history).await?;
        
        // 3. 同步 SQLite（best-effort）
        self.update_index(&entries).await;
        
        Ok(CommitReceipt { session_id: self.session_id.clone(), last_entry_id: entries.last().unwrap().id.clone() })
    }
    
    /// 重启时拾取
    pub async fn resume(&self) -> Result<ResumedSession> {
        let entries = self.read_all_entries().await?;
        let last_running = entries.iter().rev().find(|e| matches!(e.kind, EntryKind::ToolCall { .. } | EntryKind::ProviderTurn { .. }));
        if let Some(entry) = last_running {
            // 标记为 interrupted
            self.commit_atomic(vec![Entry {
                id: uuid::Uuid::now_v7().to_string(),
                parent_id: Some(entry.id.clone()),
                timestamp: now(),
                kind: EntryKind::Error {
                    kind: "interrupted".into(),
                    message: "tool/provider was running when process died".into(),
                    recoverable: true,
                },
            }]).await?;
        }
        Ok(ResumedSession { entries, last_entry_id: entries.last().map(|e| e.id.clone()) })
    }
}
```

### 8.3 何时启用

`smallhead` 默认 **不启用** Durable Layer（PI 哲学：简单第一）。

启用条件（CLI flag `--durable`）：
- 进程死了不能丢工作（CI、无人值守）。
- 用户愿意付出 SQLite + atomic commit 的额外复杂度。

---

## 9. Provider 接口实现细节

### 9.1 AnthropicProvider（最小可用）

```rust
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,                              // "claude-sonnet-4-6" 等
    base_url: String,
}

impl Provider for AnthropicProvider {
    async fn stream(&self, req: ProviderRequest, cancel: CancellationToken) -> Result<...> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": req.system,
            "tools": req.tools.iter().map(to_anthropic_tool).collect::<Vec<_>>(),
            "messages": req.messages.iter().map(to_anthropic_msg).collect::<Vec<_>>(),
            "stream": true,
        });
        let mut response = self.client.post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send().await?;
        
        // Parse SSE stream → ProviderEvent
        let stream = async_stream::try_stream! {
            // ...
        };
        Ok(Box::pin(stream))
    }
    
    fn cache_key(&self, req: &ProviderRequest) -> String {
        // Anthropic cache_control: ephemeral → sha256 of system + tools + first user message
        sha256(serde_json::to_string(&req.messages).unwrap().as_bytes())
    }
}
```

### 9.2 Provider 配置

```toml
# smallhead.toml
[provider]
primary = "anthropic"
fallback = "anthropic"   # single provider 起步

[provider.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-6"
base_url = "https://api.anthropic.com"

[provider.anthropic.haiku]                  # fast model for compaction
model = "claude-haiku-4-5"
```

---

## 10. CLI + TUI（smallhead-cli）

### 10.1 命令

```bash
smallhead                          # 默认 TUI
smallhead run "fix the bug"         # 单次运行
smallhead run -p file.md           # 用文件作为初始 prompt
smallhead resume {session-id}       # 恢复
smallhead tree {session-id}        # 查看会话树
smallhead compact {session-id}     # 手动 compact
smallhead sessions                 # 列出所有 session
smallhead serve                    # 启动 HTTP server（web UI）
smallhead skills                   # 列出已加载 skill
smallhead permission test          # 测试 ruleset
smallhead version
```

### 10.2 TUI（ratatui）

参考 pi TUI 设计：左边对话流（折叠/展开），右边 token budget + world state。

```
┌─ smallhead ──────────────────────────────────────────┐
│ Session: abc-123  Model: claude-sonnet-4-6           │
├──────────────────────────────┬───────────────────────┤
│                              │ World State           │
│ You: Fix the login bug.      │ ┌───────────────────┐ │
│                              │ │ Users: 42         │ │
│ Assistant: I'll start by      │ │ Sessions: 17      │ │
│ examining the auth handler... │ │ Components: 8     │ │
│                              │ └───────────────────┘ │
│ [tool: read src/auth.rs]      │                       │
│ Read 240 lines (offset 0)     │ Token Budget          │
│                              │ ┌──────────────────┐  │
│ Assistant: Found it. The bug  │ │ Used: 12,400     │  │
│ is in line 47...              │ │ Window: 200,000  │  │
│                              │ │ Cache: ✓ Epoch 2 │  │
│                              └───────────────────────┘
├──────────────────────────────┴───────────────────────┤
│ > _                                       [Submit]   │
└──────────────────────────────────────────────────────┘
```

**关键设计**：
- **无 modal**：所有视图通过 layout 而非全屏 modal 实现。
- **Token budget 可视化**（来自 big-head 灵感）：用户实时看到 context 用量。
- **World State 可视化**：左边 + 右边分屏，不需要切 tab。

### 10.3 Keybindings（参考 pi）

| Key | Action |
|-----|--------|
| Enter | steer（插入当前 turn） |
| Alt+Enter | follow-up（等当前 turn 完成） |
| Tab | 折叠/展开 tool 输出 |
| Ctrl+T | 切换 tree 视图 |
| Ctrl+P | 切换 provider |
| Ctrl+K | 手动 compact |
| Ctrl+R | undo / restore checkpoint |
| Ctrl+C | interrupt（标记 interrupted） |
| Esc | 取消当前输入 |

---

## 11. Web UI（可选，smallhead-server + smallhead-web）

**目标**：轻量、能在浏览器看 TUI 内容、能在手机上控制正在跑的 agent。

### 11.1 Stack

- Server: `axum` + `tower` + `tokio-tungstenite`
- Frontend: HTML + HTMX + Pico.css（无框架、无 build step）
- Protocol: HTTP + WebSocket（per-session subscribe）

### 11.2 关键端点

```rust
// 状态：未启用
GET  /sessions                                  // 列出 session
GET  /sessions/{id}                             // 会话详情
GET  /sessions/{id}/entries                     // JSONL 流
POST /sessions/{id}/prompt                       // 提交新 turn
GET  /sessions/{id}/stream                      // SSE 流（实时事件）
GET  /sessions/{id}/world                       // 当前 world state
POST /sessions/{id}/compact                     // 触发 compact
GET  /skills                                    // 列出已加载 skill
```

**约束**：
- Web UI 只读 + 简单控制；复杂编辑走 CLI/TUI。
- **无登录**：默认监听 `127.0.0.1:8787`（不可外网）。
- 配 `--web-auth-token` 后才暴露在 `0.0.0.0`。

---

## 12. Skill 加载（smallhead-skill）

agentskills.io 兼容。

```
.smallhead/skills/my-skill/
├── SKILL.md              # frontmatter: name, description
├── scripts/              # optional
├── references/           # optional
└── assets/               # optional
```

```yaml
---
name: docker-compose
description: |
  Manage docker-compose stacks. Use when the user asks to start/stop/restart
  a docker-compose service, view logs, or describe services.
---
# ... skill content ...
```

加载规则（参考 pi）：
- 启动时扫描 `~/.smallhead/skills/` + `.smallhead/skills/`（项目级覆盖）。
- `description` 是路由信号——模型看到 name+description，按需 `load_skill`。
- SKILL.md 内容**按需注入 context**（不预先全部加载）。

---

## 13. 测试与可重复验证

### 13.1 单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    /// pure pipeline 测试：无需 I/O
    #[test]
    fn step_handles_user_message() {
        let state = LoopState::default();
        let event = LoopEvent::Message(AgentMessage::User { content: vec![/* ... */], timestamp: 0 });
        let (new_state, actions) = step(&state, event);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::CallProvider { .. }));
    }
}
```

### 13.2 可重复验证的工程测试（核心）

来自 big-head 的核心教训：

```rust
#[tokio::test]
async fn compaction_round_trip_preserves_anchors() {
    // 1. 准备固定输入序列
    let entries = fixture::sample_conversation();      // 100 entries
    let tempdir = tempdir().unwrap();
    let session = SessionStore::new(tempdir.path()).await.unwrap();
    for entry in &entries {
        session.append(entry.clone()).await.unwrap();
    }
    
    // 2. 实际循环
    let compactor = Compactor::default();
    let compaction = compactor.compact(&session).await.unwrap();
    
    // 3. 独立 reader 读取临时文件
    let reader = IndependentReader::new(tempdir.path());
    let summary_block = reader.read_entry(&compaction.entry_id).unwrap();
    
    // 4. 验证 anchor_refs 覆盖所有被引用的 entry
    let referenced_ids = entries.iter().filter_map(|e| /* extract refs */ None).collect();
    assert!(compaction.anchor_refs.iter().all(|r| referenced_ids.contains(&r.entry_id)));
    
    // 5. 验证还原：从 first_kept_entry_id + summary 重建 context，
    //    与原始 entries[..cut] 的语义等价（不是文本等价）
    let restored = compactor.restore_for_prompt(&session).await.unwrap();
    let original_semantic_hash = sha256(extract_semantic(&entries[..cut]));
    let restored_semantic_hash = sha256(extract_semantic(&restored));
    assert_eq!(original_semantic_hash, restored_semantic_hash);
}
```

### 13.3 集成测试

```rust
#[tokio::test]
async fn end_to_end_simple_task() {
    let tempdir = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("smallhead").unwrap();
    cmd.env("SHELLHEAD_HOME", tempdir.path());
    cmd.args(["run", "--model", "test-mock", "echo hello"]);
    cmd.assert().success();
}
```

### 13.4 Mock Provider（用于测试）

```rust
pub struct MockProvider {
    responses: VecDeque<ProviderEvent>,
}

impl Provider for MockProvider {
    async fn stream(&self, _: ProviderRequest, _: CancellationToken) -> Result<...> {
        Ok(Box::pin(stream::iter(self.responses.iter().cloned())))
    }
    
    fn cache_key(&self, _: &ProviderRequest) -> String { "mock-key".into() }
}
```

**关键**：Mock 模拟真实 Provider 的事件流（包括 `Error` event），不绕过任何代码路径。

---

## 14. 配置（smallhead.toml）

```toml
# ~/.config/smallhead/smallhead.toml
version = "0.1"

[provider]
primary = "anthropic"
haiku_model = "anthropic:claude-haiku-4-5"

[provider.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-6"
base_url = "https://api.anthropic.com"
max_retries = 3

[provider.openai]
api_key_env = "OPENAI_API_KEY"
model = "gpt-5"

[provider.ollama]
base_url = "http://localhost:11434"
model = "llama-3.3-70b"

[permission]
mode = "default"                                # default | permissive | strict
ask_threshold = 3                               # doom_loop

[permission.rules]
# 全部 bash 默 allow，但"bash rm *" 必问
"Bash *" = "allow"
"Bash rm *" = "ask"
"Bash sudo *" = "deny"
"Read .env" = "deny"
"Webfetch github.com/*" = "allow"
"Webfetch *" = "ask"
"EcsRead *" = "allow"
"EcsMutate *" = "ask"

[session]
auto_compact_threshold = 0.80                  # 80% of context window
reserve_tokens = 16384
storage_root = "~/.local/share/smallhead/sessions"
sqlite_index = true

[ecs]
world_dir = "~/.local/share/smallhead/worlds"
checkpoint_every_n_actions = 100

[durable]
enabled = false                                # opt-in
inbox_dir = "~/.local/share/smallhead/inbox"

[skill]
extra_dirs = ["./.smallhead/skills"]
auto_load = false                              # 按需加载

[ui]
theme = "auto"                                  # auto | dark | light
token_budget_visibility = true
world_state_visibility = true
```

---

## 15. 安全 & 沙箱

### 15.1 OS-Level Sandbox（可选启用）

```bash
smallhead run --sandbox=landlock "fix the auth bug"
smallhead run --sandbox=bubblewrap ...
smallhead run --sandbox=docker ...
smallhead run --sandbox=none "..."             # 不推荐
```

**优先级**（最严格到最宽松）：
1. `docker` -- 全 OS 级别隔离，root 重新映射
2. `bubblewrap` -- 用户级 namespace 隔离（Linux）
3. `landlock` -- Linux kernel LSM（最轻量）
4. `none` -- 不隔离，仅靠 permission ruleset

**macOS**：仅支持 `none` / `sandbox-exec`（Apple 内置 profile）。

### 15.2 Bash Tool 沙箱

即使 sandbox=none，Bash tool 也有**进程级限制**：

```rust
pub struct BashTool {
    pub allowed_paths: Vec<PathBuf>,           // 限制 cwd + 显式路径
    pub env_passthrough: Vec<String>,          // 白名单 env vars
    pub max_duration: Duration,                 // 默认 30s
    pub max_output_bytes: usize,                // 默认 1MB
}
```

**Tree-sitter bash 解析**：精确识别 mutating commands（rm, mv, >, >>, dd）→ 提升为 ask。

### 15.3 WebFetch 限制

```rust
pub struct WebFetchTool {
    pub allowed_domains: Option<Vec<String>>,   // None = all + ask
    pub max_response_bytes: usize,              // 默认 5MB
    pub timeout: Duration,                      // 默认 15s
    pub follow_redirects: bool,
}
```

---

## 16. 性能预算

| 操作 | 预算 | 备注 |
|------|------|------|
| Agent loop 启动 | < 100ms | 不预加载任何东西 |
| Permission evaluate | < 1ms | Ruleset 数组 < 100 条 |
| Tool execute（bash） | 用户配置 max_duration | 默认 30s |
| JSONL append | < 5ms | 5 条批量 fsync |
| SQLite index write | < 10ms | async best-effort |
| Context Epoch check | < 1ms | hash lookup |
| Compactor（cold） | < 5s（10MB session） | 取决于 LLM |
| TUI 渲染 | 60fps | ratatui |
| Web UI SSE event delivery | < 100ms | local |

---

## 17. 风险与对策

| 风险 | 影响 | 对策 |
|------|------|------|
| LLM Provider API 变更 | 高 | Provider trait 隔离，单 crate 实现 |
| Permission 弹窗用户错 | 中 | 默认 deny + ask；提供 dry-run 模式 |
| ECS World 状态腐化 | 高 | Checkpoint 验证 + 还原测试 |
| 会话 JSONL 文件损坏 | 高 | 定期校验 + 自动备份 .jsonl.bak |
| Doom_loop 漏报 | 中 | N=3 阈值 + 用户可调 |
| Agent 在 prompt cache 失效 | 中 | Context Epoch tracking |
| 进程死掉丢工作 | 中（默认）→ 高（CI） | Durable Layer opt-in |
| Skill 注入攻击 | 中 | description 严格审查 + SKILL.md frontmatter 签名 |
| Provider key 泄露 | 高 | api_key_env 强制；不写 config |
| 过度 ECS 操作 | 中 | "EcsMutate *" = "ask" by default |

---

## 18. 与 big-head 的关系图

```
                    ┌──────────────────────────────┐
                    │   big-head  (大头部)          │
                    │                              │
                    │   - 3D Spatial UI (R3F)       │
                    │   - macOS native app          │
                    │   - Multi-mode ambient        │
                    │   - Dynamic Rhai scripting    │
                    │   - 27 ECS tools              │
                    └──────────────┬───────────────┘
                                   │
                            ECS Kernel (复用)
                                   │
                    ┌──────────────▼───────────────┐
                    │   smallhead  (小头部)        │
                    │                              │
                    │   - TUI + Web (no 3D)         │
                    │   - CLI first                 │
                    │   - 5 builtin tools           │
                    │   - Skill-driven extensions   │
                    │   - ECS world ops (11 类)     │
                    └──────────────────────────────┘
```

**复用路径**：
- `~/Desktop/p/big-head/vendor/bevy_ecs` → `smallhead/vendor/bevy_ecs`（或 crate 直接 path =）
- `~/Desktop/p/big-head/crates/big-head-ecs/src/world_op.rs` → 借鉴（不是直接依赖）
- `~/Desktop/p/big-head/crates/big-head-script/` → **不移植**

**不沿用**：
- big-plan 产品方向（ambient dwelling）
- 3D spatial UI
- AppKit integration
- ToolSchema Mac bundle

---

## 19. Roadmap（v0.1 → v1.0）

### v0.1（MVP，~6 周）
- [ ] Crate workspace 初始化
- [ ] `smallhead-core`：AgentMessage + Provider trait
- [ ] `smallhead-tools`：5 个内置工具
- [ ] `smallhead-permission`：Ruleset + DoomLoop
- [ ] `smallhead-session`：JSONL + SQLite
- [ ] `smallhead-cli run`：单次运行
- [ ] Anthropic provider
- [ ] Mock provider + 测试

### v0.2（~4 周）
- [ ] ECS integration（smallhead-ecs）
- [ ] 11 WorldOp tools
- [ ] Checkpoint + Action log 同步
- [ ] TUI（ratatui）
- [ ] Compaction（pi 5 步算法）

### v0.3（~4 周）
- [ ] OpenAI + Ollama provider
- [ ] Skill loader
- [ ] Tree 视图 + 分支
- [ ] DoomLoop 触发对抗测试

### v0.4（~4 周）
- [ ] Durable layer opt-in
- [ ] Context Epoch tracking
- [ ] Web server (axum) + Web UI
- [ ] `--sandbox=landlock|bubblewrap|docker`

### v1.0（~4 周）
- [ ] 全部 spec 文档完整
- [ ] 可重复验证测试套件
- [ ] 用户文档 + examples
- [ ] 1.0 公告

---

## 20. 附录：参考表

### 20.1 借鉴来源对照

| 设计 | 来源 | smallhead 落地 |
|------|------|---------------|
| AgentMessage + convertToLlm | pi agent-loop.ts | `AgentMessage` enum + `ToLlmMessage` trait |
| Nested 2-while | pi | `AgentLoop::run` Rust 端口 |
| Skills | agentskills.io + pi | `smallhead-skill` crate |
| JSONL tree | pi + opencode | `entries.jsonl` + `parent_id` |
| Ruleset last-match-wins | opencode | `smallhead-permission::Ruleset::evaluate` |
| Doom loop | opencode | `DoomLoopDetector` |
| Context Epochs | opencode V2 | `Provider::cache_key()` + epoch_id 跟踪 |
| Durable inbox/history | opencode V2 + pi-durable | `DurableSession` opt-in |
| Code mode (sandbox) | pi-codemode | **v0.3+** 用 wasmtime |
| ToolAdapter 双层抽象 | agent-deck-rs | `ToolKind` enum + `ToolSpec` |
| Pure pipeline | agent-deck-rs | `step()` pure function |
| ECS kernel | **big-head** | `smallhead-ecs` (vendored bevy_ecs) |
| 11 World ops | big-head | `WorldOpKind` enum |
| Checkpoint + Action Log | big-head v3 | 简化为 JSONL + sparse checkpoint |
| TokenBreakdown 可视化 | big-head | TUI 右侧 panel |

### 20.2 Crate 依赖矩阵

```
                    core  tools  perm  session  ecs  skill  cli  server  web
core                  -     -     -      -       -     -     -     -     -
tools                 ✓     -     -      -       ✓     -     -     -     -
permission            ✓     ✓     -      -       -     -     -     -     -
session               ✓     ✓     ✓      -       -     -     -     -     -
ecs                   ✓     ✓     -      -       -     -     -     -     -
skill                 ✓     ✓     -      -       -     -     -     -     -
cli                   ✓     ✓     ✓      ✓       ✓     ✓     -     -     -
server                ✓     ✓     ✓      ✓       ✓     ✓     -     -     -
web                   -     -     -      -       -     -     -     ✓     -

✓ = 依赖
```

**禁止**：
- `core` 依赖任何其他 crate。
- `ecs` 依赖 `tools` / `cli`（避免循环）。
- `web` 依赖 `cli` / `server`（独立静态资源）。

### 20.3 文件路径约定

```
~/.config/smallhead/smallhead.toml              # 用户配置
~/.local/share/smallhead/sessions/{cwd-hash}/{session-id}/  # 会话
~/.local/share/smallhead/worlds/{world-id}/     # ECS worlds
~/.local/share/smallhead/inbox/{session-id}/    # Durable inbox
~/.smallhead/skills/                            # 项目级 skill
./.smallhead/                                   # 项目级 config
```

---

## 21. 一句话总结

**smallhead = pi 的最小核心 + opencode 的 V2 durable session + Claude Code 的 arity ruleset + agent-deck-rs 的 pure pipeline + big-head 的 ECS kernel，减去 3D UI、macOS app、multi-mode ambient、Rhai scripting。**

差异化主张：
1. ECS kernel 是 first-class（继承 big-head）。
2. Durable session 是 opt-in（继承 opencode V2）。
3. Pi-style minimal core 是哲学。
4. TUI + Web（无 3D）是产品形态。

下一步：写 `ROADMAP.md` 详细拆 v0.1 - v1.0 的任务。
