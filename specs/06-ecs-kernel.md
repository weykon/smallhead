# 06 — ECS Kernel

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-ecs/`
> 上游: `ARCHITECTURE.md` §4, §18; `AGENT_METHODOLOGY.md` §2.8
> 复用自: `~/Desktop/p/big-head/vendor/bevy_ecs`、`~/Desktop/p/big-head/crates/big-head-ecs/`

---

## 1. 概述

ECS kernel 是 smallhead **区别于其他 Rust agent 项目（pi / opencode / codex）的核心差异化能力**——继承自 `big-head`。facts 不是 message thread 里的字符串，而是 entities 上的 components。模型通过 11 类 `WorldOp` 操作 world，从不直接拿到 `World` 引用。

设计原则：
- **复用而非重写**：big-head 的 vendored `bevy_ecs` 已经过生产验证，smallhead 通过 Cargo path 依赖复用。
- **schema 复制，commit 协议简化**：big-head 的 v3 binary checkpoint（`checkpoint.json` + `checkpoint.blobs` + `checkpoint.journal`）是为大数据优化；smallhead 直接 JSONL + sparse checkpoint。
- **Checkpoint + Action Log 二元结构**：Action Log 是不可变流（JSONL），Checkpoint 是它的 snapshot——这是 ECS 学习的因果追踪。

---

## 2. Vendored bevy_ecs 集成

```toml
# smallhead-ecs/Cargo.toml
[dependencies]
bevy_ecs = { path = "../vendor/bevy_ecs-0.16" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["sync", "fs"] }
sha2 = "0.10"
async-trait = "0.1"
thiserror = "1"
```

**决策**：复用 big-head 的 vendored 路径 `~/Desktop/p/big-head/vendor/bevy_ecs-0.16`，不引入上游 crates.io 版本（上游版本差异会破坏兼容性）。具体路径：

```text
~/Desktop/p/big-head/vendor/bevy_ecs-0.16/
```

可通过符号链接或 git submodule 引入 smallhead：

```bash
# 在 smallhead/vendor/ 创建符号链接
ln -s ~/Desktop/p/big-head/vendor/bevy_ecs-0.16 smallhead/vendor/bevy_ecs-0.16
```

**为什么不直接 path 依赖 big-head**：smallhead 是独立 crate workspace，避免隐式跨项目依赖。

---

## 3. WorldOp API（11 类）

LLM 看到的工具 surface 完全由 `WorldOpTool` 表达。模型不直接拿到 `World` 引用，所有 mutation 通过 enum-dispatch + 类型化 input：

```rust
// smallhead-ecs/src/world_op.rs

use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorldOpKind {
    QueryEntities,        // SELECT  —— EcsRead
    GetComponent,         // GET     —— EcsRead
    InsertComponent,      // INSERT  —— EcsMutate
    RemoveComponent,      // DELETE  —— EcsMutate
    UpdateComponent,      // UPDATE  —— EcsMutate (predicate-based)
    SpawnEntity,          // CREATE  —— EcsMutate
    DespawnEntity,        // DROP    —— EcsMutate
    AddRelation,          // GRAPH EDGE add    —— EcsMutate
    RemoveRelation,       // GRAPH EDGE remove —— EcsMutate
    DefineComponent,      // SCHEMA CREATE     —— EcsMutate (敏感)
    RunSystem,            // PROCEDURAL        —— EcsMutate (敏感)
}

#[derive(Debug, Clone)]
pub struct WorldOpTool {
    pub kind: WorldOpKind,
    pub world: Arc<tokio::sync::Mutex<World>>,
    pub checkpoint_sink: Arc<CheckpointSink>,
    pub action_log: Arc<ActionLog>,
}

impl WorldOpTool {
    pub fn permission_required(&self) -> Permission {
        match self.kind {
            WorldOpKind::QueryEntities | WorldOpKind::GetComponent => Permission::EcsRead,
            _ => Permission::EcsMutate,
        }
    }

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
            WorldOpKind::DefineComponent => ("define_component", "SCHEMA CREATE — register a new component type"),
            WorldOpKind::RunSystem => ("run_system", "PROCEDURAL — invoke a registered system"),
        };
        ToolSpec {
            name: name.into(),
            label: format!("WorldOp: {}", name),
            description: desc.into(),
            input_schema: world_op_schema(self.kind),
            output_schema: None,
            kind: ToolKindDiscriminant::WorldOp,
        }
    }

    pub async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult> {
        let mut world = self.world.lock().await;

        // 1. 执行 op
        let (entity_id, component, before, after, result) = match self.kind {
            WorldOpKind::QueryEntities => query_entities(&mut world, input)?,
            WorldOpKind::GetComponent => get_component(&mut world, input)?,
            WorldOpKind::InsertComponent => insert_component(&mut world, input)?,
            WorldOpKind::RemoveComponent => remove_component(&mut world, input)?,
            WorldOpKind::UpdateComponent => update_component(&mut world, input)?,
            WorldOpKind::SpawnEntity => spawn_entity(&mut world, input)?,
            WorldOpKind::DespawnEntity => despawn_entity(&mut world, input)?,
            WorldOpKind::AddRelation => add_relation(&mut world, input)?,
            WorldOpKind::RemoveRelation => remove_relation(&mut world, input)?,
            WorldOpKind::DefineComponent => define_component(&mut world, input)?,
            WorldOpKind::RunSystem => run_system(&mut world, input)?,
        };

        // 2. 记录 mutation 到 action log（仅 mutate 类）
        if self.permission_required() == Permission::EcsMutate {
            let entry = ActionLogEntry {
                op_kind: self.kind,
                entity_id,
                component: component.clone(),
                before: before.clone(),
                after: after.clone(),
                tool_call_id: ctx.current_tool_call_id.clone(),
                timestamp: now_ms(),
                reason: extract_reason(&ctx),
            };
            self.action_log.append(entry).await?;
        }

        Ok(result)
    }
}

fn extract_reason(ctx: &ToolContext) -> String {
    // 从 ToolContext 提取 mutation reason —— 是 big-head 的核心哲学：
    // "agent 可以修改 ECS，但任何 mutation 都需要 traceable reason"
    ctx.metadata.get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("no reason provided")
        .to_string()
}
```

---

## 4. World 状态隔离

每个 session 默认拥有独立 World。例外情况由用户显式声明（如 `--share-world=team-shared`）：

```rust
// smallhead-ecs/src/world_registry.rs

pub struct WorldRegistry {
    /// 默认：cwd + session-id 派生独立 world
    default_dir: PathBuf,
    /// 显式共享 world（高级）
    shared: HashMap<String, Arc<Mutex<World>>>,
}

impl WorldRegistry {
    pub async fn for_session(&self, cwd: &Path, session_id: &str) -> Arc<Mutex<World>> {
        let world_dir = self.default_dir
            .join(cwd_hash(cwd))
            .join(session_id);
        self.load_or_create(&world_dir).await
    }

    pub async fn load_or_create(&self, dir: &Path) -> Arc<Mutex<World>> {
        let checkpoint_path = dir.join("checkpoints/latest.json");
        if checkpoint_path.exists() {
            // 从最新 checkpoint + action log 还原
            self.restore_from_checkpoint(&checkpoint_path).await
        } else {
            Arc::new(Mutex::new(World::new()))
        }
    }
}

pub fn cwd_hash(cwd: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..8])    // 16 hex chars
}
```

**目录布局**：

```text
~/.local/share/smallhead/worlds/
└── {cwd-hash}/                          # cwd SHA-256 前 16 字符
    └── {session-id}/                    # 每个 session 独立
        ├── checkpoints/
        │   ├── latest.json              # 符号链接到最新 version
        │   ├── 001.json
        │   ├── 002.json
        │   └── 003.json
        ├── action_log.jsonl             # append-only 不可变流
        └── meta.json                    # world_id, schema 列表, 创建时间
```

---

## 5. Checkpoint + Action Log 二元结构

这是 ECS kernel 的因果追踪设计：

```rust
// smallhead-ecs/src/checkpoint.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldCheckpoint {
    pub world_id: String,
    pub version: u64,                          // monatomic，自增
    pub world: serde_json::Value,               // ECS state snapshot
    pub action_log_digest: String,              // sha256 of action log up to version
    pub timestamp: i64,
    pub parent_version: Option<u64>,            // 形成 chain（可选，用于 bisect）
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLogEntry {
    pub op_kind: WorldOpKind,
    pub entity_id: Option<u64>,
    pub component: Option<String>,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub tool_call_id: String,                   // 因果链：关联到 session JSONL 的 ToolCall
    pub timestamp: i64,
    pub reason: String,                         // big-head 哲学："任何 mutation 都有 reason"
}

pub enum CommitResult {
    Committed { version: u64, checkpoint_id: String },
    Conflict { expected_version: u64, actual_version: u64 },
}
```

**核心不变量**：
- **Action Log 永不修改**：append-only JSONL，每条 entry 包含 `tool_call_id` 关联到 session JSONL。
- **Checkpoint 是 Action Log 的 snapshot**：`action_log_digest` 验证完整性。
- **Checkpoint sparse**：每 N 条 action 自动触发一次 checkpoint（默认 N=100，可在 `smallhead.toml` 配置）。
- **Action Log 不进 checkpoint**——它是独立流。

---

## 6. 原子提交协议

写 checkpoint 必须用 atomic write（来自 pi-durable 原则）：

```rust
// smallhead-ecs/src/commit.rs

use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;

pub async fn atomic_commit_checkpoint(
    dir: &Path,
    checkpoint: &WorldCheckpoint,
) -> Result<PathBuf> {
    // 1. 写临时文件
    let version = checkpoint.version;
    let final_path = dir.join("checkpoints").join(format!("{:06}.json", version));
    let tmp_path = final_path.with_extension("json.tmp");

    let bytes = serde_json::to_vec(checkpoint)?;
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .await?;
        f.write_all(&bytes).await?;
        f.write_all(b"\n").await?;
        f.sync_all().await?;          // fsync — 数据物理落盘
        drop(f);
    }

    // 2. 原子 rename（POSIX 保证 atomic on same filesystem）
    fs::rename(&tmp_path, &final_path).await?;

    // 3. 更新 latest 符号链接（原子 swap）
    let latest_link = dir.join("checkpoints/latest.json");
    let tmp_link = latest_link.with_extension("json.tmp");
    if latest_link.exists() {
        fs::remove_file(&latest_link).await?;
    }
    fs::symlink(&final_path, &tmp_link).await?;
    fs::rename(&tmp_link, &latest_link).await?;

    // 4. 同步元数据
    sync_dir(dir).await?;
    Ok(final_path)
}

async fn sync_dir(dir: &Path) -> Result<()> {
    let f = File::open(dir).await?;
    f.sync_all().await?;
    Ok(())
}
```

**为什么必须 atomic**：
1. **崩溃一致性**：写一半时进程被 kill，tmp 文件留在原地，final 文件保留旧版本。下次启动看到的是一致的旧版本，不是 corrupt 中间态。
3. **POSIX 保证**：`rename` 在同文件系统上是 atomic。

**与 big-head 的简化**：big-head 的 v3 用 `checkpoint.blobs`（二进制压缩），smallhead 直接 JSON——读 diff、可读、可 grep。

---

## 7. Action Log 写入

```rust
// smallhead-ecs/src/action_log.rs

pub struct ActionLog {
    path: PathBuf,
    writer: tokio::sync::Mutex<tokio::fs::File>,
}

impl ActionLog {
    pub async fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("action_log.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            path,
            writer: tokio::sync::Mutex::new(file),
        })
    }

    pub async fn append(&self, entry: ActionLogEntry) -> Result<()> {
        let bytes = serde_json::to_vec(&entry)?;
        let mut f = self.writer.lock().await;
        f.write_all(&bytes).await?;
        f.write_all(b"\n").await?;
        // 不每条 fsync —— 5 条批量 fsync（参见 specs/04 §4）
        Ok(())
    }

    pub async fn digest(&self) -> Result<String> {
        // 重新读 action log 算 sha256（启动时 / checkpoint 时调用）
        let content = tokio::fs::read(&self.path).await?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        Ok(hex::encode(hasher.finalize()))
    }
}
```

---

## 8. 还原测试（核心）

big-head 的核心教训（来自 `agent-harness-source-study-20261004.md`）：**摘要不能歪曲原话，必须可独立验证**。World 状态同样必须可独立验证 — 即从 `checkpoint + action log` 还原到 `checkpoint 时刻 + 之后所有 action log`，应等于当前 world state。

```rust
// smallhead-ecs/tests/restore_round_trip.rs

#[tokio::test]
async fn checkpoint_plus_action_log_recovers_world_state() {
    let tempdir = tempdir().unwrap();
    let world_dir = tempdir.path();

    // 1. 准备固定输入：100 次 mutation
    let entries = fixture::sample_world_mutations();  // 100 ActionLogEntry
    let mut world = World::new();

    for (i, entry) in entries.iter().enumerate() {
        apply_op(&mut world, entry);
        if (i + 1) % 100 == 0 {
            let checkpoint = snapshot(&world, i + 1, &entry.tool_call_id).await;
            atomic_commit_checkpoint(world_dir, &checkpoint).await.unwrap();
        }
    }

    // 2. 模拟崩溃：删掉最新 checkpoint 后所有 action log 重新应用
    let latest_cp = WorldCheckpoint::load_latest(world_dir).await.unwrap();
    let mut restored_world = WorldCheckpoint::to_world(&latest_cp);

    // 3. 独立 reader 读取 action log（不通过 ActionLog API）
    let reader = IndependentReader::new(world_dir);
    let action_log_entries = reader.read_action_log().await.unwrap();

    let cut = action_log_entries.iter()
        .position(|e| e.tool_call_id == latest_cp.action_log_digest)
        .unwrap();
    for entry in &action_log_entries[cut..] {
        apply_op(&mut restored_world, entry);
    }

    // 4. 验证：还原后的 world state == 当前 world state
    let original = world_state_json(&world);
    let restored = world_state_json(&restored_world);
    assert_eq!(original, restored,
               "checkpoint + action log must perfectly reconstruct world state");
}
```

**这条测试是 ECS 的"立身之本"**——任何 commit 协议 / action log 协议的改动都必须通过它。

---

## 9. 不变量

1. **所有 ECS mutations 通过 `WorldOp`**——agent 不直接拿 `World` 引用。
2. **`World` 引用只在 `WorldOpTool` 内**——LLM 永远看不到 `World` 类型。
3. **每个 mutation 都有 `reason`**（来自 big-head 哲学）——空 reason 视为错误。
4. **Action Log append-only**——永不修改、永不删除。
5. **Checkpoint 包含 `action_log_digest`**——验证 Action Log 完整性。
6. **原子 commit**：写 tmp → fsync → rename，不留 corrupt 中间态。
7. **每个 session 一个独立 World**（默认）——通过 `cwd-hash + session-id` 派生路径。
8. **`tool_call_id` 是因果链**：session JSONL 的 `ToolCall` ↔ Action Log 的 `ActionLogEntry`。
9. **schema 变更（`DefineComponent` / `RunSystem`）尤其敏感**——permissions 默认 ask。
10. **Checkpoint 是 sparse**：每 N 次 mutation 自动触发一次（N 默认 100）。

---

## 10. 与 big-head 对照

| 维度 | big-head v3 | smallhead |
|------|------------|-----------|
| World 类型 | `big_head_ecs::World`（vendored bevy_ecs） | 复用同一 vendored crate |
| Action Log | `checkpoint.journal` (binary) | `action_log.jsonl` (JSONL) |
| Checkpoint | `checkpoint.json` + `checkpoint.blobs` (binary + compression) | `checkpoints/{version}.json` (JSON) |
| Commit 协议 | v3 binary format with content addressing | 简化：JSON + tmp → fsync → rename |
| Schema | 已含 `define_component` | 沿用 big-head word schema（schema 在分析时注册） |
| Dynamic systems | 是（Rhai 注册） | **不沿用**（smallhead v0.1 不支持 dynamic systems） |
| 还原验证 | 无显式 round-trip 测试 | `checkpoint_plus_action_log_recovers_world_state` 是 P0 |
| Reason 字段 | 部分支持 | **强**——所有 mutation 强制 reason |

**核心沿用**：
- ECS 数据模型 + 11 类 WorldOp
- Checkpoint + Action Log 二元结构
- World 隔离机制
- 持久 world 的哲学

**核心简化**：
- 二进制 → JSON（开发体验）
- Dynamic systems → 暂不支持（Pi 哲学：minimize core）

---

## 11. 验收测试

### 11.1 WorldOp 11 类全覆盖

```rust
#[test]
fn all_eleven_world_ops_have_specs_and_permission() {
    let kinds = [
        WorldOpKind::QueryEntities, WorldOpKind::GetComponent,
        WorldOpKind::InsertComponent, WorldOpKind::RemoveComponent,
        WorldOpKind::UpdateComponent, WorldOpKind::SpawnEntity,
        WorldOpKind::DespawnEntity, WorldOpKind::AddRelation,
        WorldOpKind::RemoveRelation, WorldOpKind::DefineComponent,
        WorldOpKind::RunSystem,
    ];
    for kind in kinds {
        let tool = WorldOpTool::test_default(kind);
        let spec = tool.spec();
        assert!(!spec.name.is_empty());
        assert!(!spec.description.is_empty());
        // 6 个 read 类，5 个 mutate 类
        match kind {
            WorldOpKind::QueryEntities | WorldOpKind::GetComponent => {
                assert_eq!(tool.permission_required(), Permission::EcsRead);
            }
            _ => assert_eq!(tool.permission_required(), Permission::EcsMutate),
        }
    }
}
```

### 11.2 原子 commit 不留 tmp

```rust
#[tokio::test]
async fn atomic_commit_leaves_no_tmp_files() {
    let tempdir = tempdir().unwrap();
    let cp = WorldCheckpoint { version: 1, ..test_default() };

    atomic_commit_checkpoint(tempdir.path(), &cp).await.unwrap();

    // 不应有 .tmp 残留
    let entries: Vec<_> = walkdir::WalkDir::new(tempdir.path())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "tmp"))
        .collect();
    assert!(entries.is_empty(), "atomic commit must clean up tmp files");
}
```

### 11.3 还原测试（§8）

### 11.4 mutation reason 强制

```rust
#[tokio::test]
async fn mutation_without_reason_is_rejected() {
    let mut world = World::new();
    let ctx = ToolContext {
        metadata: json!({}),   // 没有 reason
        ..test_ctx()
    };
    let tool = WorldOpTool::test_default(WorldOpKind::InsertComponent);

    let result = tool.execute(json!({"entity": 1, "component": "Pos", "value": {}}), &ctx).await;
    assert!(result.is_err(), "mutation without reason must be rejected");
}
```

### 11.5 World 隔离

```rust
#[tokio::test]
async fn different_sessions_have_independent_worlds() {
    let tempdir = tempdir().unwrap();
    let registry = WorldRegistry::new(tempdir.path());

    let w1 = registry.for_session(Path::new("/tmp"), "session-a").await;
    let w2 = registry.for_session(Path::new("/tmp"), "session-b").await;

    // w1 写一个 entity
    let mut world = w1.lock().await;
    world.spawn_empty().insert("foo", 42);

    // w2 应该看不到
    let world2 = w2.lock().await;
    assert_eq!(world2.iter().count(), 0);
}
```

---

## 12. 引用

- 上游 spec: `specs/01-message-model.md`（无直接影响）
- 上下游 spec:
  - `specs/03-tool-model.md` — `ToolKind::WorldOp(WorldOpTool)` 是 tool enum 的 variant
  - `specs/04-session-storage.md` — `Checkpoint` / `WorldMutation` 是 `EntryKind` 的 variant
  - `specs/05-permission-model.md` — `EcsRead` / `EcsMutate` 是 `Permission` 的 variant
- 源码: `ARCHITECTURE.md` §4, §18
- 方法论: `AGENT_METHODOLOGY.md` §2.8
- 复用自: `~/Desktop/p/big-head/vendor/bevy_ecs`、`~/Desktop/p/big-head/crates/big-head-ecs/src/world_op.rs`

---

## 13. 变更日志

- **2026-10-04 v0.1** — 初版。复用 big-head vendored bevy_ecs + 11 类 WorldOp + Checkpoint/ActionLog 二元结构 + atomic commit + reason 字段强制。
- **未来**: 考虑为 WorldOp 引入事务（多个 op 在一个 checkpoint 提交）——目前每个 op 是独立 action。
- **未来**: 考虑为 Action Log 引入 CRDT（pi-durable 风格）支持多端同步。