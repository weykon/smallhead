# 03 — Tool Model

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-core/src/tool.rs`, `smallhead-tools/src/`, `smallhead-ecs/src/world_op.rs`
> 上游: `ARCHITECTURE.md` §3.3, §4.2; `AGENT_METHODOLOGY.md` §1.3, §2.9

---

## 1. 概述

Tool 是 agent 与外部世界交互的接口。smallhead 的 Tool 模型有三层：

1. **内置工具**：`Bash` / `Read` / `Edit` / `Write` / `WebFetch`——5 个核心能力。
2. **World operations**：`WorldOpTool`——11 类 ECS 操作（详见 §4）。
3. **Dynamic / Skill tools**：用户或第三方扩展的工具。

关键设计选择是 **enum-dispatch 而不是 `Box<dyn Tool>` trait object**。big-head 的教训：`dyn Tool` 难以序列化、schema 注册不友好、type erasure 让 IDE 跳转失效。smallhead 用 `enum ToolKind { Bash(BashTool), Read(ReadTool), ... }` + `match` 表达，动态部分用 `Box<dyn DynamicTool>` 兜底。

---

## 2. 类型定义

```rust
// smallhead-core/src/tool.rs
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum ToolKind {
    Bash(BashTool),
    Read(ReadTool),
    Edit(EditTool),
    Write(WriteTool),
    WebFetch(WebFetchTool),
    /// World operations（见 §4）—— big-head 沿用的差异化能力
    WorldOp(WorldOpTool),
    /// 用户注册的 dynamic tool
    Dynamic(Box<dyn DynamicTool>),
    /// Skill-loaded tool
    Skill(SkillTool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,                              // "bash"
    pub label: String,                             // "Bash" (UI 显示)
    pub description: String,                       // 模型选择依据
    pub input_schema: serde_json::Value,           // JSON Schema
    pub output_schema: Option<serde_json::Value>,  // 可选
    pub kind: ToolKindDiscriminant,                // 用于 permission dispatch
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKindDiscriminant {
    Bash, Read, Edit, Write, WebFetch,
    WorldOp, Dynamic, Skill,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub on_update: Option<Box<dyn Fn(Value) + Send + Sync>>,  // 流式 partial output
    pub world: Arc<Mutex<World>>,                 // WorldOp 需要
    pub permission: Arc<Ruleset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub metadata: Value,
}

impl ToolKind {
    pub fn spec(&self) -> ToolSpec {
        match self {
            ToolKind::Bash(t) => t.spec(),
            ToolKind::Read(t) => t.spec(),
            ToolKind::Edit(t) => t.spec(),
            ToolKind::Write(t) => t.spec(),
            ToolKind::WebFetch(t) => t.spec(),
            ToolKind::WorldOp(t) => t.spec(),
            ToolKind::Dynamic(t) => t.spec(),
            ToolKind::Skill(t) => t.spec(),
        }
    }

    pub async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        match self {
            ToolKind::Bash(t) => t.execute(input, ctx).await,
            ToolKind::Read(t) => t.execute(input, ctx).await,
            ToolKind::Edit(t) => t.execute(input, ctx).await,
            ToolKind::Write(t) => t.execute(input, ctx).await,
            ToolKind::WebFetch(t) => t.execute(input, ctx).await,
            ToolKind::WorldOp(t) => t.execute(input, ctx).await,
            ToolKind::Dynamic(t) => t.execute(input, ctx).await,
            ToolKind::Skill(t) => t.execute(input, ctx).await,
        }
    }

    /// 控制模型看到的内容（opencode V2 设计）
    pub fn to_model_output(&self, args: &Value, output: &ToolResult) -> Vec<ContentBlock> {
        match self {
            ToolKind::Bash(t) => t.to_model_output(args, output),
            ToolKind::Read(t) => t.to_model_output(args, output),
            // ... 其余类似
            _ => output.content.clone(),
        }
    }
}
```

### 2.1 Dynamic / Skill Tool

```rust
/// 用户注册的 dynamic tool —— 必须的逃生通道
#[async_trait]
pub trait DynamicTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn output_schema(&self) -> Option<Value>;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult>;
    fn to_model_output(&self, args: &Value, output: &ToolResult) -> Vec<ContentBlock>;
}

#[async_trait]
pub trait SkillTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult>;
}
```

---

## 3. 内置工具（5 个）

### 3.1 Bash

```rust
#[derive(Debug, Clone)]
pub struct BashTool {
    pub allowed_paths: Vec<PathBuf>,    // cwd + 显式路径白名单
    pub env_passthrough: Vec<String>,
    pub max_duration: Duration,         // 默认 30s
    pub max_output_bytes: usize,        // 默认 1MB
}

impl BashTool {
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            label: "Bash".into(),
            description: "Run a shell command. Use for git, ls, grep, build, test.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "shell command" },
                    "timeout": { "type": "integer", "description": "timeout in seconds" }
                },
                "required": ["command"]
            }),
            output_schema: None,
            kind: ToolKindDiscriminant::Bash,
        }
    }

    pub async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        // 1. tree-sitter 解析 command，识别 mutating shape
        // 2. 检查 path 限制
        // 3. tokio::process::Command spawn
        // 4. 收集 stdout/stderr，截断到 max_output_bytes
        // 5. 通过 ctx.on_update 流式回调
        todo!()
    }

    pub fn to_model_output(&self, _args: &Value, output: &ToolResult) -> Vec<ContentBlock> {
        // 默认：完整返回。允许 truncate 到 N tokens。
        output.content.clone()
    }
}
```

### 3.2 Read / Edit / Write / WebFetch

类似结构，输入 schema 不同。WebFetch 必须有 `allowed_domains` 白名单 + 5MB 上限。

---

## 4. WorldOp（11 类）

这是 big-head 沿用的核心差异化能力——agent 不直接拿到 World 引用，所有 mutation 通过 `WorldOpTool`：

```rust
// smallhead-ecs/src/world_op.rs

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorldOpKind {
    QueryEntities,        // SELECT
    GetComponent,         // GET
    InsertComponent,      // INSERT
    RemoveComponent,      // DELETE
    UpdateComponent,      // UPDATE with predicate
    SpawnEntity,          // CREATE
    DespawnEntity,        // DROP
    AddRelation,          // GRAPH EDGE
    RemoveRelation,
    DefineComponent,      // SCHEMA CREATE
    RunSystem,            // PROCEDURAL
}

#[derive(Debug, Clone)]
pub struct WorldOpTool {
    pub kind: WorldOpKind,
    pub world: Arc<Mutex<World>>,
    pub permission_required: Permission,    // EcsRead | EcsMutate
}

impl WorldOpTool {
    pub fn spec(&self) -> ToolSpec {
        let (name, desc) = match self.kind {
            WorldOpKind::QueryEntities => ("query_entities", "SELECT entities matching a filter"),
            WorldOpKind::GetComponent => ("get_component", "GET a component value"),
            WorldOpKind::InsertComponent => ("insert_component", "INSERT a component on entity"),
            WorldOpKind::RemoveComponent => ("remove_component", "DELETE a component"),
            WorldOpKind::UpdateComponent => ("update_component", "UPDATE components by predicate"),
            WorldOpKind::SpawnEntity => ("spawn_entity", "CREATE a new entity"),
            WorldOpKind::DespawnEntity => ("despawn_entity", "DROP an entity"),
            WorldOpKind::AddRelation => ("add_relation", "GRAPH EDGE add"),
            WorldOpKind::RemoveRelation => ("remove_relation", "GRAPH EDGE remove"),
            WorldOpKind::DefineComponent => ("define_component", "SCHEMA CREATE — register new component"),
            WorldOpKind::RunSystem => ("run_system", "PROCEDURAL — invoke a registered system"),
        };
        ToolSpec {
            name: name.into(),
            label: format!("WorldOp: {}", name),
            description: desc.into(),
            input_schema: world_op_schema(self.kind),  // 按 kind 生成
            output_schema: None,
            kind: ToolKindDiscriminant::WorldOp,
        }
    }

    pub async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let mut world = self.world.lock().await;
        match self.kind {
            WorldOpKind::QueryEntities => query(&mut world, input),
            WorldOpKind::GetComponent => get(&mut world, input),
            // ...
        }
    }
}
```

**安全约束**：
- 所有 mutation 类（Insert/Remove/Update/Spawn/Despawn/Add/Remove relation/Define/RunSystem）需要 `EcsMutate` permission。
- `EcsMutate *` 在默认 ruleset 是 `ask`（详见 `specs/05-permission-model.md`）。
- `Query` 和 `Get` 是 `EcsRead`，默认 `allow`。

---

## 5. on_update 流式输出钩子

长任务（bash 长命令、WebFetch 大文件下载）支持流式 partial output：

```rust
pub struct ToolContext {
    // ...
    pub on_update: Option<Box<dyn Fn(Value) + Send + Sync>>,
}

// BashTool::execute 中：
if let Some(cb) = &ctx.on_update {
    let mut child = Command::new("bash").arg(&command).stdout(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut buf = String::new();
    while reader.read_line(&mut buf).await? > 0 {
        cb(json!({ "partial_stdout": buf }));  // 推给 TUI
        buf.clear();
    }
}
```

TUI 收到 partial 时折叠显示，用户可展开看完整流。

---

## 6. to_model_output 控制

`to_model_output(args, output)` 决定 **LLM 看到** 的内容——可以与 **用户看到** 的内容不同：

```rust
pub fn to_model_output(&self, args: &Value, output: &ToolResult) -> Vec<ContentBlock> {
    match self {
        // Bash: 模型看 stdout + stderr，但超过 2000 tokens 就 truncate
        ToolKind::Bash(t) => {
            let text = extract_text(&output.content);
            if estimate_tokens(&text) > 2000 {
                vec![ContentBlock::Text { text: truncate(&text, 2000) }]
            } else {
                output.content.clone()
            }
        }
        // Read: 模型看完整内容，但 attachment 路径在 metadata
        ToolKind::Read(t) => output.content.clone(),
        // WebFetch: 模型看 markdown 化的内容，原始 HTML 进 attachment
        ToolKind::WebFetch(t) => markdownify(&output.content),
        _ => output.content.clone(),
    }
}
```

差异点：用户界面展示可能更丰富（彩色 ANSI、原始字节），但模型只看**对决策有用的子集**。

---

## 7. 与 big-head 对照

| 维度 | big-head | smallhead |
|------|---------|-----------|
| Tool 抽象 | `Box<dyn Tool>` + Rhai `define_tool` | `enum ToolKind` + enum-dispatch |
| Schema 暴露 | JSON Value（运行时） | JSON Value（编译时 enum 也可派生） |
| 动态扩展 | Rhai 脚本 hot-reload | `Box<dyn DynamicTool>` + `Skill` (按需加载) |
| 工具数 | 27 ECS tools | 5 + 11 WorldOp + N dynamic |
| 流式输出 | 自定义回调 | `ctx.on_update: Box<dyn Fn>` |

**为什么 enum 优于 trait object**：
1. **序列化稳定**：`#[derive(Serialize)]` 直接得到确定 JSON 结构。
2. **Schema 编译期生成**：`#[derive(JsonSchema)]` → 静态 schema，避免运行时 `schemars` 反射开销。
3. **IDE 跳转**：`ToolKind::Bash(BashTool)` 点击直接到 struct 定义。
4. **二进制大小可控**：enum 是 monomorphized；trait object 是 dynamic dispatch。

**代价**：dynamic tool 仍需要 `Box<dyn DynamicTool>` 兜底；这是有意识的"逃生通道"。

---

## 8. TypeScript 类比（opencode V2 tools.md）

```ts
// opencode specs/v2/tools.md (简化)
interface Tool<I, O> {
  name: string;
  label: string;
  description: string;
  input: Schema<I>;
  output?: Schema<O>;
  execute(args: I, ctx: Tool.Context): Promise<Result<O, ToolFailure>>;
  toModelOutput?(args: I, output: O): Content[];
  // onUpdate 走 ctx callback
}

// Tool.Context:
interface ToolContext {
  sessionID: string;
  abort: AbortSignal;
  onUpdate?(partial: O): void;     // 流式 partial
  metadata(): Metadata;            // 读 metadata（只读）
  ask(args): Promise<PermissionDecision>;  // 显式 ask
}
```

smallhead 的差异：把所有内置工具用 enum-dispatch 表达，但保持相同的 `ToolContext` 抽象边界。

---

## 9. 验收测试

**用例 1: Bash 安全约束**

```rust
#[tokio::test]
async fn bash_respects_max_duration() {
    let tool = BashTool { max_duration: Duration::from_secs(2), ..Default::default() };
    let result = tool.execute(json!({"command": "sleep 5"}), &test_ctx()).await;
    assert!(result.is_err() || result.unwrap().is_error);
}
```

**用例 2: WorldOp permission**

```rust
#[tokio::test]
async fn ecs_mutate_requires_permission() {
    let tool = WorldOpTool {
        kind: WorldOpKind::InsertComponent,
        world: Arc::new(Mutex::new(World::new())),
        permission_required: Permission::EcsMutate,
    };
    // 默认 ruleset 下 InsertComponent 应触发 ask
    let decision = test_ruleset().evaluate(&Permission::EcsMutate, &json!({}), &ctx());
    assert!(matches!(decision.action, Action::Ask));
}
```

**用例 3: to_model_output truncate**

```rust
#[test]
fn bash_to_model_output_truncates_long_output() {
    let tool = BashTool::default();
    let long = "x".repeat(100_000);
    let output = ToolResult {
        content: vec![ContentBlock::Text { text: long.clone() }],
        is_error: false,
        metadata: json!({}),
    };
    let model_view = tool.to_model_output(&json!({}), &output);
    let text = extract_text(&model_view);
    assert!(text.len() < 10_000);
}
```

**用例 4: on_update 流式**

```rust
#[tokio::test]
async fn bash_on_update_streams_partial_output() {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_clone = updates.clone();
    let ctx = ToolContext {
        on_update: Some(Box::new(move |v| {
            updates_clone.lock().unwrap().push(v);
        })),
        ..test_ctx()
    };
    let tool = BashTool::default();
    tool.execute(json!({"command": "for i in 1 2 3; do echo $i; done"}), &ctx).await.unwrap();
    let updates = updates.lock().unwrap();
    assert!(updates.len() >= 3);  // 至少 3 行 partial
}
```

---

## 10. 不变量

1. **ToolKind 包含所有工具类型**——新增工具必须加 enum variant（编译期强制）。
2. **execute() 永远返回 `Result<ToolResult>`**——不允许 panic 跨过边界。
3. **to_model_output 默认返回 `output.content.clone()`**——只有需要 truncate / 转换的才 override。
4. **on_update 回调不得 panic**——错误用 Result 表达。
5. **WorldOp mutation 必须有 `EcsMutate` permission**——编译期保证。
6. **Bash 的 `max_duration` 是 hard cap**——不可绕过（防止模型自我取消 timeout）。
7. **WebFetch 的 `allowed_domains` 为 None 时默认 ask**——不留未配置即放行的缺口。
8. **Dynamic tool 必须显式注册才能被 agent 调用**——无 wildcard 加载。

---

## 11. 引用

- 上游 spec: `specs/01-message-model.md`（`ToolUse` / `ToolResult` content block）
- 引用本 spec:
  - `specs/02-agent-loop.md` — `execute_tool()` 调用点
  - `specs/04-session-storage.md` — ToolCall / ToolResult 持久化
  - `specs/05-permission-model.md` — Bash / EcsRead / EcsMutate permission
- 源码: `ARCHITECTURE.md` §3.3, §4.2
- 方法论: `AGENT_METHODOLOGY.md` §1.3, §2.9

---

## 12. 变更日志

- **2026-10-04 v0.1** — 初版。enum-dispatch + 5 内置工具 + 11 WorldOp + Dynamic / Skill 兜底。
- **未来**: 考虑给 WorldOp 引入"事务"边界（多个 WorldOp 在一个 checkpoint 提交），目前每个 WorldOp 是独立操作。
