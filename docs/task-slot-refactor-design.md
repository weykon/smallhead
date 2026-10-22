# TaskSlot 抽离设计 —— Monad task 生命周期管理

> 状态: 已审核，方向认可；当前实现方案需修订，暂勿按第 9 节直接施工。见第 13 节。
> 目标读者: 熟悉本仓 monad 协议(`docs/monad-orchestration-pattern.md`)的 reviewer
> 配套: `crates/smallhead-core/src/monad.rs` 当前实现
> 本轮开发范围: 只实施第 14 节的局部绑定重构；第 1–12 节保留为原始草案，第 13 节为审核，第 14 节为本轮实施依据。

## 0. 背景与动机

当前 `MonadLoop` 协议下,每个 monad 实现都重复手写同一套 task 状态管理代码:

- `CalculationMonad`、`EchoMonad`、`GreetingMonad`、`MainMonad` 各自持有 `Option<TaskBinding>` 字段。
- 每个 monad 在 `bind()` 入口都写一遍:
  ```rust
  if self.task.is_some() {
      return Err(MonadError::new("monad already has an active task"));
  }
  let binding = TaskBinding::from_handoff(handoff, self.monad_id())?;
  self.task = Some(binding);
  ```
- 每个 monad 在 `step()` 入口都写一遍:
  ```rust
  let task = self.task.take().ok_or_else(|| MonadError::new("..."))?;
  ```
- 每个 monad 在 `cancel()` 都写一遍:
  ```rust
  self.task = None;
  ```
- `started_ms: i64` 时间戳散装在 4 个 monad struct 里,出口又 `now_ms() - self.started_ms` 算 elapsed。

更重要的是,**上层(bridge / main loop)看不到 task 的状态**——`task` 是 monad 私有字段,没有任何观测点。加超时、重试、metrics、test spy 都要改 4 处。

本设计要把这层"task 生命周期"**从各 monad 里抽出来**,形成单独模块,并提供可选的上层管理者。

## 1. 设计目标

| 目标 | 验证方式 |
|---|---|
| 收掉 4 处重复的 bind/take/clear 5 行代码 | 改后 `bind()` 入口只剩 1 行 `try_bind` 调用 |
| 提供跨 monad 实例的 task 记账 + 观察点 | 新增 `TaskManager` 持有 `HashMap<TaskId, TaskRecord>`,通过 `TaskObserver` trait 暴露事件 |
| 不改 `MonadLoop` 公共 trait 接口 | trait 仍只暴露 `start/step/cancel/monad_id/catalog_entry/default_tool_set` |
| 不破坏现有 demo / 测试 | 改后 `monad_demo`、`waiting_context_demo`、`cargo test` 全绿 |
| 为超时/重试/持久化预留接入点(但不实现) | 通过 `next_attempt: HashMap` + `ClearReason` enum 留位 |

## 2. 故意不做(YAGNI)

| 想法 | 为什么不做 |
|---|---|
| Typestate `TaskSlot<Unbound>` / `TaskSlot<Bound>` | 当前 monad 都是 `from_handoff` 一步构造,typestate 会让所有调用点参数化,迁移面大。**等真的有"未绑 step" bug 报告再升级** |
| 强制 `Budget.timeout_ms` / `Budget.max_steps` | `MonadLoop` trait 文档明确"不保证 timeout",改这里等于改契约 |
| `TaskManager` 内部 `tokio::sync::RwLock` | 当前 monad 实例不共享(bridge dispatch 现场 `instantiate` 出新实例),无并发场景 |
| 持久化 / 跨进程恢复 | trait 文档明说"不保证持久化" |
| 把三个 fixture monad 抽公共 trait | 业务逻辑不同(算术/回显/问候),抽 trait 反而降低清晰度 |

## 3. 架构

### 3.1 三层结构

```
crates/smallhead-core/src/
├── monad.rs                    # 改:把 TaskBinding 挪走,改成 mod monad; pub use
└── monad/
    ├── mod.rs                  # 新:monad 子模块入口
    └── task.rs                 # 新:TaskSlot + TaskBinding + TaskManager + TaskObserver
```

### 3.2 抽象层次

```
┌──────────────────────────────────────────────────────────┐
│  Layer 3: TaskManager (上层 — 可选)                       │
│  ┌────────────────────────────────────────────────────┐  │
│  │ HashMap<TaskId, TaskRecord>                        │  │
│  │ + Vec<Arc<dyn TaskObserver>>                       │  │
│  │ + next_attempt: HashMap<String, u32>  (预留)        │  │
│  └────────────────────────────────────────────────────┘  │
│                            ▲ report()                    │
│  ┌────────────────────────┴──────────────────────────┐  │
│  │  Layer 2: TaskSlot (per-monad 实例)                │  │
│  │  ┌──────────────────────────────────────────────┐  │  │
│  │  │ Option<TaskBinding>                          │  │  │
│  │  │ + try_bind (判重 + 校验)                      │  │  │
│  │  │ + take (单次消费)                             │  │  │
│  │  │ + clear (取消)                                │  │  │
│  │  │ + peek / is_bound                             │  │  │
│  │  │ + usage_after (算 elapsed)                    │  │  │
│  │  └──────────────────────────────────────────────┘  │  │
│  └────────────────────────────────────────────────────┘  │
│                            ▲ 写                           │
│  ┌────────────────────────┴──────────────────────────┐  │
│  │  Layer 1: monad 实例                                │  │
│  │  task: TaskSlot   (替代 task: Option<TaskBinding>)  │  │
│  │  CalculationMonad / MainMonad / Echo / Greeting     │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

### 3.3 `MainMonad` 特殊点

`MainMonad` 除 `current_task` 外还有 `frame: Vec<LlmMessage>` 和 `started: bool`,
这俩是 MainMonad 特有的"消息驱动 LLM"状态,**不属于 task 抽象**,
**不抽到 TaskSlot**。强行抽会污染抽象。

迁移时只换 `current_task: Option<TaskBinding>` → `current_task: TaskSlot`,
`frame` / `started` 仍归 MainMonad 私有。

## 4. 详细 API

### 4.1 `TaskBinding` —— 纯数据

```rust
#[derive(Debug, Clone)]
pub struct TaskBinding {
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub result_slot: ResultSlot,
    pub monad_id: MonadId,        // 新增:原 monad 自持,现集中到这里
    pub attempt_id: u32,          // 新增:原 handoff 校验有但 binding 不存
    pub bound_at: Instant,        // 新增:替代散装 started_ms: i64
}

impl TaskBinding {
    pub fn from_handoff(handoff: &MonadHandoff, expected: &MonadId) -> Result<Self, MonadError>;
}
```

`from_handoff` 的 5 项校验**逐字照搬**自 `monad.rs:554-578`(monad_id 匹配 /
schema_version / task_id 非空 / result_slot 非空 / attempt_id > 0)。

### 4.2 `TaskSlot` —— per-monad 守门

```rust
#[derive(Debug, Default)]
pub struct TaskSlot { inner: Option<TaskBinding> }

impl TaskSlot {
    pub fn new() -> Self;
    pub fn is_bound(&self) -> bool;
    pub fn peek(&self) -> Option<&TaskBinding>;          // 只读,给观察者
    pub fn try_bind(&mut self, handoff: &MonadHandoff, expected: &MonadId)
        -> Result<&TaskBinding, MonadError>;             // 已绑 → 拒
    pub fn take(&mut self) -> Result<TaskBinding, MonadError>;  // 物理消费
    pub fn clear(&mut self);                              // 取消
    pub fn usage_after(binding: &TaskBinding, steps_taken: u32, tokens_used: u32)
        -> ResourceUsage;                                // 用 binding.bound_at 算 elapsed
}
```

3 个动作的状态机:

```
        ┌──────────┐  try_bind(h)   ┌──────────┐
        │ Unbound  │ ──────────────▶│  Bound   │
        │ (None)   │                │ (Some b) │
        └──────────┘                └──────┬───┘
              ▲                            │
              │  take() / clear()          │
              └────────────────────────────┘
```

### 4.3 `TaskManager` —— 上层可选管理

```rust
#[derive(Debug, Default)]
pub struct TaskManager {
    active: HashMap<TaskId, TaskRecord>,
    observers: Vec<Arc<dyn TaskObserver>>,
    next_attempt: HashMap<String, u32>,  // 预留重试 attempt 自增
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub binding: TaskBinding,
    pub state: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState { Bound, Completed, Failed(String), Cancelled }

impl TaskManager {
    pub fn new() -> Self;
    pub fn attach(&mut self, obs: Arc<dyn TaskObserver>);
    pub fn register(&mut self, binding: TaskBinding);          // 绑完上报
    pub fn mark_completed(&mut self, task_id: &TaskId);
    pub fn mark_failed(&mut self, task_id: &TaskId, err: String);
    pub fn mark_cancelled(&mut self, task_id: &TaskId);
    pub fn forget(&mut self, task_id: &TaskId) -> Option<TaskRecord>;
    pub fn get(&self, task_id: &TaskId) -> Option<&TaskRecord>;
    pub fn list_active(&self) -> impl Iterator<Item = &TaskRecord>;
}
```

### 4.4 `TaskObserver` —— 观察者 trait

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearReason {
    Completed,
    Failed(String),
    Cancelled,
    Replaced,    // 预留:重试时新 attempt 替换旧 attempt
}

pub trait TaskObserver: Send + Sync {
    fn on_bind(&self, _binding: &TaskBinding) {}
    fn on_take(&self, _binding: &TaskBinding) {}
    fn on_clear(&self, _task_id: &TaskId, _reason: ClearReason) {}
}
```

默认全空实现,使用者按需 override。
**测试场景里直接实现一个 spy,把事件存 Vec 然后 assert。**

## 5. 完整 `task.rs` 代码

```rust
//! Monad task 生命周期管理 —— 把"单次消费 / 重绑检测 / 完成清理"统一收口。
//!
//! 分三层:
//! - [`TaskBinding`]:已通过校验的最小标识集合(纯数据,无行为)。
//! - [`TaskSlot`]:每个 monad 实例持有一个,守 bind/take/clear 三个门。
//! - [`TaskManager`]:上层(bridge / main loop)可选持有,做跨实例记账 + 观察者分发。
//!
//! 故意**不**包含:超时强制、重试、持久化、调度 —— 这些是 P1 计划,
//! 现在只预留接入点(`TaskObserver` / `next_attempt`)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::{
    MonadError, MonadHandoff, MonadId, MonadResult, ResourceUsage, ResultSlot, TaskId,
    SCHEMA_VERSION_V1,
};

// ────────────────────────────────────────────────────────────────
// Layer 1: TaskBinding —— 纯数据
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TaskBinding {
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub result_slot: ResultSlot,
    pub monad_id: MonadId,
    pub attempt_id: u32,
    pub bound_at: Instant,
}

impl TaskBinding {
    /// 5 项校验同原 monad.rs:554-578(逐字照搬)。
    pub fn from_handoff(handoff: &MonadHandoff, expected: &MonadId) -> Result<Self, MonadError> {
        if &handoff.monad_id != expected {
            return Err(MonadError::new("handoff targets a different monad"));
        }
        if handoff.schema_version != SCHEMA_VERSION_V1 {
            return Err(MonadError::new("unsupported handoff schema_version"));
        }
        if handoff.task_id.0.trim().is_empty()
            || handoff.result_slot.0.trim().is_empty()
            || handoff
                .parent_task_id
                .as_ref()
                .is_some_and(|id| id.0.trim().is_empty())
            || handoff.attempt_id == 0
        {
            return Err(MonadError::new(
                "handoff requires nonempty IDs/result_slot and a positive attempt_id",
            ));
        }
        Ok(Self {
            task_id: handoff.task_id.clone(),
            parent_task_id: handoff.parent_task_id.clone(),
            result_slot: handoff.result_slot.clone(),
            monad_id: handoff.monad_id.clone(),
            attempt_id: handoff.attempt_id,
            bound_at: Instant::now(),
        })
    }
}

// ────────────────────────────────────────────────────────────────
// Layer 2: TaskSlot —— per-monad 实例守门
// ────────────────────────────────────────────────────────────────

/// 一个 monad 实例的 task 状态守门。
///
/// 替代散装的 `task: Option<TaskBinding>` + `started_ms: i64` + 5 行 `is_some()` 判重。
#[derive(Debug, Default)]
pub struct TaskSlot {
    inner: Option<TaskBinding>,
}

impl TaskSlot {
    pub fn new() -> Self { Self::default() }
    pub fn is_bound(&self) -> bool { self.inner.is_some() }
    pub fn peek(&self) -> Option<&TaskBinding> { self.inner.as_ref() }

    pub fn try_bind(
        &mut self,
        handoff: &MonadHandoff,
        expected: &MonadId,
    ) -> Result<&TaskBinding, MonadError> {
        if self.inner.is_some() {
            return Err(MonadError::new("monad already has an active task"));
        }
        let binding = TaskBinding::from_handoff(handoff, expected)?;
        self.inner = Some(binding);
        Ok(self.inner.as_ref().expect("just inserted"))
    }

    pub fn take(&mut self) -> Result<TaskBinding, MonadError> {
        self.inner
            .take()
            .ok_or_else(|| MonadError::new("no active task; call start(handoff) before step()"))
    }

    pub fn clear(&mut self) { self.inner = None; }

    pub fn usage_after(
        binding: &TaskBinding,
        steps_taken: u32,
        tokens_used: u32,
    ) -> ResourceUsage {
        ResourceUsage {
            steps_taken,
            tokens_used,
            elapsed_ms: binding.bound_at.elapsed().as_millis() as u64,
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Layer 3: TaskManager —— 上层可选管理者
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct TaskManager {
    active: HashMap<TaskId, TaskRecord>,
    observers: Vec<Arc<dyn TaskObserver>>,
    next_attempt: HashMap<String, u32>,
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub binding: TaskBinding,
    pub state: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState { Bound, Completed, Failed(String), Cancelled }

impl TaskManager {
    pub fn new() -> Self { Self::default() }
    pub fn attach(&mut self, obs: Arc<dyn TaskObserver>) { self.observers.push(obs); }

    pub fn register(&mut self, binding: TaskBinding) {
        let id = binding.task_id.clone();
        for o in &self.observers { o.on_bind(&binding); }
        self.active.insert(id, TaskRecord {
            binding,
            state: TaskState::Bound,
        });
    }

    pub fn mark_completed(&mut self, task_id: &TaskId) {
        if let Some(rec) = self.active.get_mut(task_id) {
            rec.state = TaskState::Completed;
            let b = rec.binding.clone();
            for o in &self.observers { o.on_take(&b); }
        }
    }

    pub fn mark_failed(&mut self, task_id: &TaskId, err: String) {
        if let Some(rec) = self.active.get_mut(task_id) {
            rec.state = TaskState::Failed(err.clone());
            let b = rec.binding.clone();
            for o in &self.observers {
                o.on_clear(&b.task_id, ClearReason::Failed(err));
            }
        }
    }

    pub fn mark_cancelled(&mut self, task_id: &TaskId) {
        if let Some(rec) = self.active.get_mut(task_id) {
            rec.state = TaskState::Cancelled;
            let b = rec.binding.clone();
            for o in &self.observers {
                o.on_clear(&b.task_id, ClearReason::Cancelled);
            }
        }
    }

    pub fn forget(&mut self, task_id: &TaskId) -> Option<TaskRecord> {
        self.active.remove(task_id)
    }

    pub fn get(&self, task_id: &TaskId) -> Option<&TaskRecord> { self.active.get(task_id) }
    pub fn list_active(&self) -> impl Iterator<Item = &TaskRecord> { self.active.values() }
}

// ────────────────────────────────────────────────────────────────
// Observer —— 日志 / 指标 / 测试 spy 接入点
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearReason {
    Completed,
    Failed(String),
    Cancelled,
    Replaced,
}

pub trait TaskObserver: Send + Sync {
    fn on_bind(&self, _binding: &TaskBinding) {}
    fn on_take(&self, _binding: &TaskBinding) {}
    fn on_clear(&self, _task_id: &TaskId, _reason: ClearReason) {}
}
```

## 6. 迁移前后对比 —— `CalculationMonad` 完整对位

### 改前(monad.rs:1018-1150 现行)

```rust
pub struct CalculationMonad {
    task: Option<TaskBinding>,
    expression: String,
    started_ms: i64,
}

impl CalculationMonad {
    fn bind(&mut self, handoff: &MonadHandoff) -> Result<(), MonadError> {
        if self.task.is_some() {
            return Err(MonadError::new("monad already has an active task"));
        }
        let binding = TaskBinding::from_handoff(handoff, self.monad_id())?;
        self.expression = handoff
            .inputs
            .get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.started_ms = now_ms();
        self.task = Some(binding);
        Ok(())
    }
    // ... eval 不变
}

#[async_trait]
impl MonadLoop for CalculationMonad {
    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.bind(&handoff)
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let task = self.task.take()
            .ok_or_else(|| MonadError::new("no active task; call start(handoff) before step()"))?;
        match Self::eval(&self.expression) {
            Ok(value) => Ok(MonadResult::Completed {
                task_id: task.task_id,
                parent_task_id: task.parent_task_id,
                result_slot: task.result_slot,
                monad_id: MonadId::new("calculation"),
                output: serde_json::json!({
                    "expression": self.expression.clone(),
                    "value": value,
                }),
                evidence_refs: vec![],
                usage: ResourceUsage {
                    steps_taken: 1,
                    tokens_used: 0,
                    elapsed_ms: (now_ms() - self.started_ms).max(0) as u64,
                },
                finished_at_ms: now_ms(),
            }),
            Err(reason) => Ok(MonadResult::WaitingContext {
                task_id: task.task_id,
                reason,
                missing: vec!["expression".into()],
            }),
        }
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task = None;
        Ok(())
    }
}
```

### 改后

```rust
use crate::monad::task::TaskSlot;

pub struct CalculationMonad {
    task: TaskSlot,            // 替代 Option<TaskBinding>
    expression: String,        // 业务派生,bind 时写
    // started_ms 删了 —— TaskBinding.bound_at 接管
}

impl Default for CalculationMonad {
    fn default() -> Self {
        Self { task: TaskSlot::new(), expression: String::new() }
    }
}

impl CalculationMonad {
    fn bind(&mut self, handoff: &MonadHandoff) -> Result<(), MonadError> {
        let _binding = self.task.try_bind(handoff, self.monad_id())?;  // 一行守门
        self.expression = handoff
            .inputs
            .get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(())
    }
}

#[async_trait]
impl MonadLoop for CalculationMonad {
    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.bind(&handoff)
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let task = self.task.take()?;  // 一行消费
        match Self::eval(&self.expression) {
            Ok(value) => Ok(MonadResult::Completed {
                task_id: task.task_id,
                parent_task_id: task.parent_task_id,
                result_slot: task.result_slot,
                monad_id: task.monad_id.clone(),
                output: serde_json::json!({
                    "expression": self.expression.clone(),
                    "value": value,
                }),
                evidence_refs: vec![],
                usage: TaskSlot::usage_after(&task, 1, 0),  // 静态算 elapsed
                finished_at_ms: now_ms(),
            }),
            Err(reason) => Ok(MonadResult::WaitingContext {
                task_id: task.task_id,
                reason,
                missing: vec!["expression".into()],
            }),
        }
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task.clear();
        Ok(())
    }
}
```

**净删除**:
- 字段 `task: Option<TaskBinding>` → `task: TaskSlot`(净 -1 字段)
- 字段 `started_ms: i64` 删除(净 -1 字段)
- 5 行判重 → 1 行 `try_bind`(净 -4 行)
- `now_ms() - self.started_ms` 散装计算 → `TaskSlot::usage_after(&task, 1, 0)`

## 7. 迁移前后对比 —— `MainMonad`

```rust
pub struct MainMonad {
    inner: Option<Box<dyn AgentLoopImpl>>,
    current_task: TaskSlot,         // ← 从 Option<TaskBinding> 改
    frame: Vec<LlmMessage>,         // ← 保留(MainMonad 私有)
    started: bool,                  // ← 保留(MainMonad 私有)
    manager: Option<Arc<TaskManager>>,  // ← 新增(可选注入)
}

impl MainMonad {
    pub fn with_manager(mut self, m: Arc<TaskManager>) -> Self {
        self.manager = Some(m);
        self
    }
}

async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
    self.current_task.try_bind(&handoff, self.monad_id())?;
    self.frame.clear();
    self.frame.push(LlmMessage::User {
        content: vec![crate::message::ContentBlock::Text {
            text: handoff.objective.clone(),
        }],
    });
    self.started = true;
    if let (Some(mgr), Some(b)) = (self.manager.as_ref(), self.current_task.peek()) {
        mgr.register(b.clone());
    }
    Ok(())
}

async fn step(&mut self) -> Result<MonadResult, MonadError> {
    let binding = self.current_task.take()?;
    // ... 跑 AgentLoop::run_one_turn ...
    let result = MonadResult::Completed {
        task_id: binding.task_id.clone(),
        parent_task_id: binding.parent_task_id.clone(),
        result_slot: binding.result_slot.clone(),
        monad_id: binding.monad_id.clone(),
        output: serde_json::json!({ /* turns_completed / tool_calls_executed / final_text */ }),
        evidence_refs: Vec::new(),
        usage: TaskSlot::usage_after(&binding, summary.turns_completed as u32, 0),
        finished_at_ms: now_ms(),
    };
    if let Some(mgr) = &self.manager {
        mgr.mark_completed(&binding.task_id);
    }
    self.frame.clear();
    self.started = false;
    Ok(result)
}
```

`frame` / `started` 维持原样,不抽到 TaskSlot。

## 8. 文件改动清单

| 文件 | 改动 |
|---|---|
| `crates/smallhead-core/src/monad/task.rs` | **新增**:本设计全部代码 |
| `crates/smallhead-core/src/monad/mod.rs` | **新增**:`pub mod task;` + `pub use task::*;` |
| `crates/smallhead-core/src/monad.rs` | 改:顶部 `mod monad; pub use monad::*;`,把原 `TaskBinding` struct 删掉(改由 `monad/task.rs` 提供) |
| `crates/smallhead-core/src/monad.rs` | 改:`CalculationMonad` / `EchoMonad` / `GreetingMonad` 三家把 `task: Option<TaskBinding>` + `started_ms: i64` 换成 `task: TaskSlot` |
| `crates/smallhead-core/src/monad.rs` | 改:`MainMonad` 把 `current_task: Option<TaskBinding>` 换成 `current_task: TaskSlot`,加 `manager: Option<Arc<TaskManager>>` 字段 |
| `examples/monad_demo.rs` | **不动** |
| `examples/waiting_context_demo.rs` | **不动** |

`MonadLoop` trait 公共 API **零变化**。

## 9. 落地步骤(每步独立可测)

| 步 | 改动 | 验证 |
|---|---|---|
| 1 | 加 `monad/task.rs` + `monad/mod.rs`,`monad.rs` 顶部加 `mod monad; pub use monad::*;` | `cargo check -p smallhead-core` 绿(原 TaskBinding 仍在 monad.rs 里,跟 task.rs 并存) |
| 2 | 删 `monad.rs` 里的 `TaskBinding` struct(迁到 task.rs) | `cargo test -p smallhead-core` 全绿 |
| 3 | 改 `CalculationMonad` 字段为 `task: TaskSlot` + `bind/step/cancel` 调新 API | `cargo test -p smallhead-core` 全绿 |
| 4 | 同样改 `EchoMonad` / `GreetingMonad` | `cargo test` 全绿 |
| 5 | 改 `MainMonad`:`current_task: TaskSlot` + `manager: Option<Arc<TaskManager>>` 字段 + `with_manager` 构造器 | `cargo test` 全绿 + 跑 `monad_demo` / `waiting_context_demo` 看到 5 段 / 3 段输出和原来一致 |
| 6 | (可选)在 `monad_bridge.rs` 给 `MonadBridgeTool` 加 `Arc<TaskManager>` 字段 + 实现一个 spy observer 写单测 | 验证 on_bind / on_take 触发 |

## 10. 风险与回退

| 风险 | 缓解 |
|---|---|
| 改 `TaskBinding` 字段(加 `monad_id` / `attempt_id` / `bound_at`)破坏序列化兼容性 | 当前没有持久化,也不 wire 给外部消费者;改字段零影响 |
| `monad.rs` 拆 `monad/` 子目录影响 import 路径 | 用 `pub use monad::*;` 透传,`use smallhead_core::monad::*` 路径不变 |
| 4 处 monad 改动不彻底漏一个 | 第 3-5 步每步独立跑 `cargo test`,漏一处必现 |
| `TaskManager` 没被任何 monad 默认注入,等于没加 | 步骤 6 通过测试 spy 验证;或者把 `register / mark_completed` 接到 demo 主动展示一次 |

回退方案: 步骤 1-2 是纯加,随时可丢弃;步骤 3-5 改字段,回退靠 git revert 即可。

## 11. 开放问题(供 reviewer 拍板)

1. **`TaskBinding.bound_at` 用 `Instant` 还是 `i64`**:当前设计用 `Instant`(更准),但 `finished_at_ms: i64` 仍用 `now_ms()`。要不要统一改成 `Instant`?
2. **`TaskManager` 是不是默认就该有,而不是 `Option<Arc<>>`**:更激进做法是 monad struct 必带 manager,fallback 到一个无 observer 的 default。但这样改 4 处 monad,迁移面更大。
3. **是否把 `MainMonad.frame` / `started` 也想个类似抽象**:`Frame<S>` 状态机?目前**不建议**,会增加认知成本且没真实 bug 驱动。
4. **`TaskManager` 锁策略**:目前 `TaskManager` 字段都在 `&mut self` 上,要求调用方独占。后续若 monad 实例跨线程共享,需要 `Arc<Mutex<TaskManager>>` 还是其他?
5. **`TaskObserver` 是否异步**:`fn on_bind(&self, ...)` 同步。当前没异步需求;如果未来 observer 要写日志/上报 metrics,要不要加 `async fn`?

## 12. 与现有协议/文档的关系

- `MonadLoop` trait(monad.rs:592-620)及 trait 文档(monad.rs:581-590):公共 API **零变化**,文档中的"单次消费"语义**被 TaskSlot 显式收口**。
- `MonadHandoff`(monad.rs:200-220):**不动**,`TaskBinding::from_handoff` 仍消费 `&MonadHandoff`。
- `MonadResult`(monad.rs:229-275):**不动**,构造逻辑迁移后形态不变。
- `MonadBridgeTool`(monad_bridge.rs):**最小改动**(仅步骤 6 可选),不强制。
- `monad-orchestration-pattern.md`、`monad-rust-lifecycle-review.md`:本设计是它们的实现层落地,不修改协议层。

---

**审核重点**(给 reviewer):

1. 三层抽象(TaskBinding / TaskSlot / TaskManager)的边界是否合理
2. `MainMonad.frame` / `started` **不抽** 是否合理(第 3.3 节)
3. 故意不做的 5 件事(第 2 节)是否同意保留 YAGNI
4. 开放问题(第 11 节)5 项的设计选择
5. 迁移步骤(第 9 节)是否风险可控

## 13. 审核意见（2026-10-08）

**结论：认可把单个 Monad 的绑定约束收进 `TaskSlot`；暂不认可把本文的 `TaskManager` 原样接入主循环。** 本节是对现有设计稿的审核，前文代码是待修订草案，不是可直接复制的实现。任务管理确有长期价值：并发子任务的依赖、等待补充上下文、取消传播、重试与结果回填，最终都需要一处可靠的状态所有者。但当前演示中的 `MonadBridgeTool` 是顺序执行一个子 Monad 并立刻返回；只为消除几个 `Option` 字段的重复，不必提前引入一套与未来调度器平行的管理系统。

### 13.1 必须修正的落地问题

1. **模块布局会编译失败。** `lib.rs` 已有 `pub mod monad;`。不能同时用 `src/monad.rs` 和 `src/monad/mod.rs` 定义同一个模块，更不能在 `monad.rs` 内写 `mod monad;`。保留 `src/monad.rs`，只新增 `src/monad/task.rs`，在 `monad.rs` 内写 `mod task;`，必要时 `pub use task::{TaskBinding, TaskSlot};`。迁移第一步也不能在旧 `TaskBinding` 仍存在时把新同名类型重导出；应先私有引入新模块，再在同一变更中完成类型替换与导出。
2. **共享管理器的示例不能编译。** 第 7 节用 `Arc<TaskManager>` 调 `register` / `mark_completed`，但这些方法要求 `&mut self`。第 5 节的 `#[derive(Debug)]` 也不能直接用于包含 `Arc<dyn TaskObserver>` 的结构，除非 observer trait 约束 `Debug` 或手写实现。`MainMonad.inner` 当前是 `Option<crate::AgentLoop>`，并不存在示例里的 `AgentLoopImpl`。这些都要在迁移计划中改正，不能标为每步 `cargo check` 可通过。
3. **现状描述有偏差。** `started_ms` 只存在于三个 fixture Monad，`MainMonad` 没有该字段；第 0 节和第 5 节的“4 个”应改为“三个 fixture”。`TaskBinding` 目前是内部结构；若将其公开并加入 `Instant`，它就不是适合持久化的“纯数据”对象。
4. **状态与事件不够支撑未来任务管理。** `TaskSlot::take()` 发生在 `step()` 开始，而 `TaskManager::on_take()` 被安排在完成后，二者含义相冲突。`register` 会覆盖已有任务，`mark_*` 对不存在的任务静默成功，`list_active` 包含 Completed/Failed/Cancelled。`TaskState` 也缺 `Running`、`WaitingContext`，与现有 `MonadResult` 和 `monad-orchestration-pattern.md` 的调度状态不一致。`MainMonad.step()` 在执行前取走 binding；若 Provider 返回错误，当前示例不会把管理器更新为失败，`started` 仍为 true。取消与执行错误路径都要完整回报。若做管理器，应定义合法状态转换并让重复或过期结果显式报错。
5. **没有跨实例的真正接入点。** 仅把 `TaskManager` 注入 `MainMonad`，看不到 `MonadBridgeTool` 临时创建的子 Monad。即便第 9 节可选步骤把管理器也放进 bridge，未来 `TaskGraph` / scheduler 仍会与它同时拥有任务状态。应让调度器持有唯一权威状态，TUI/日志通过只读事件或快照观察，不再维护第二份可变真相。现有 `diagnostics` 已提供 task-local 事件通道，先复用它的展示链路。

### 13.2 建议的边界和顺序

- **第一阶段：局部绑定。** 只抽出 `TaskSlot` 和必要的 `TaskBinding`，保持现有 `MonadLoop` wire format 和行为。`try_bind` 校验成功后再写入；`take` 表示“执行权已移交”，不能被解释为“任务已完成”。计时用运行时私有的 `Instant`；任务身份与回填字段保持可复制/可序列化的普通数据。`frame` / `started` 仍留在 `MainMonad`，这点同意原稿。验证重复 `start`、无绑定 `step`、等待后补上下文、取消及再次接收新任务。
- **第二阶段：真实调度。** 当并发、依赖或跨 turn 等待实际接入时，在 scheduler / `TaskGraph` 内建立唯一的 `TaskRecord` 与合法转换：`Pending/Ready/Running/WaitingContext/Completed/Failed/Cancelled`（具体命名可与现有调度设计统一）。记录父任务、结果槽、依赖、当前 attempt 和最终结果；`register` 要拒绝冲突，完成/失败/取消要检查当前状态。发往 TUI 的事件在状态提交后产生，观察者不应阻塞调度器。
- **第三阶段：重试与恢复。** 先定语义：`TaskId` 标识逻辑任务，`attempt_id` 标识一次执行；旧 attempt 的迟到结果不能覆盖新 attempt。当前 `MonadResult` 除 `Completed` 外只带 `task_id`，无法单独辨别 attempt；可在调度器内部用 `(handoff, result)` 信封关联，跨进程恢复时再决定是否升级 wire 协议。`next_attempt: HashMap<String, u32>` 这种预留字段在此之前不应加入。持久化使用 epoch 时间和可序列化记录；`Instant` 只用于本进程耗时。

**对原稿开放问题的答复：** `TaskManager` 不应默认注入每个 Monad；应由未来调度器统一持有。此时不需要决定 `Arc<Mutex<_>>` 或异步 observer；先用调度器单任务所有权和事件通道即可。`TaskSlot` 可以先做，但需要先修第 8–9 节的模块路径和迁移步骤，并删去“每步可编译”的错误承诺。

## 14. 本轮实施契约：局部任务绑定重构

本节优先于第 1–12 节的草案。目标是将 Echo、Calculation、Greeting 和 Main 四个 Monad 中一致的任务绑定规则集中起来，同时保留各自的业务输入、计算逻辑和消息处理。局部 slot 空闲不等于全局任务完成：等待上下文的任务仍可由上层保留，随后重新 `start` 补充后的 handoff。

### 14.1 类型与模块边界

- 保留 `src/monad.rs`，新增 `src/monad/task.rs`，用 `mod task;` 引入。不新增 `src/monad/mod.rs`，不在 `monad.rs` 内声明 `mod monad;`。
- `TaskSlot` 内部使用 `Option<...>` 表达完整绑定或未绑定。`TaskBinding` 保存原有回填标识；只有真实调用需要时才增加 `monad_id` / `attempt_id`。字段私有或 `pub(super)`，通过校验构造，避免任意拼装非法绑定。此轮不承诺持久化，也不必新增 Serialize/Deserialize。
- 单次执行的 `Instant` 可以放在 slot 内部的运行时包装结构中，消费绑定时随运行时数据移交，以便执行完成后统计耗时。不要把 `Instant` 混进未来的可持久化任务记录；`finished_at_ms` 继续表示 epoch 时间。
- API 至少覆盖绑定、单次消费、清理；只在调用方需要时添加查询。不添加 `TaskManager`、`TaskObserver`、锁、重试计数占位字段或泛型 typestate。

### 14.2 必须保持的生命周期规则

| 场景 | 要求 |
|---|---|
| 未绑定时执行 | 返回明确错误，不执行业务 |
| 已绑定时再次 start | 拒绝，不覆盖原任务身份、输入或 frame |
| handoff 校验失败 | slot 及业务状态保持原样；沿用现有 ID、schema、目标 Monad、attempt 校验 |
| 业务输入准备失败 | 先在局部变量中准备，成功后再提交绑定与业务字段；不能留下半初始化实例 |
| 正常完成 | 回填 task_id、parent_task_id、result_slot 正确；重复 step 不产生第二个结果 |
| WaitingContext | 保持现有语义：本次绑定已消费，补充上下文后需再次 start；不能暗中复用旧输入 |
| 执行返回 Err | 明确本次绑定已消费，清理 Main 的本轮 frame/started，允许重新 start；不自动重放可能产生副作用的业务 |
| cancel | 清除本地绑定及必要的本轮状态，可重复调用；遵循现有取消信号语义，不宣称可以中断所有外部工作 |
| 完成、等待或取消后接新任务 | 新任务标识和输入完整替换，不能混入上一任务结果 |

`take` 只表示绑定数据移交给当前执行代码。未来跨实例状态管理仍由调度器负责。耗时使用单调时钟；token 用量若当前未采集，应继续明确其限制，不能把占位零当成已完成计量。

### 14.3 实施和验收

1. 阅读实际代码与现有生命周期测试，确认当前行为；修改类型与调用点时保留公共导入路径、`MonadLoop` trait 和 handoff/result JSON 协议。
2. 抽出模块并迁移三个 fixture，再迁移 Main。保留 `MainMonad.inner: Option<crate::AgentLoop>`；所有业务解析、模型选择和工具执行仍留在原业务层。
3. 优先复用已有测试，只补上述契约中缺失的行为测试。用本地 mock provider 验证 Main 的成功/错误和重新 start，测试不依赖真实 MiniMax、Laya 或 Qwen 服务。
4. 运行 `cargo check -p smallhead-core`、`cargo test -p smallhead-core`，以及 `cargo check -p smallhead-tui`；对修改的 Rust 文件检查格式。example 至少编译验证，真实 API 演示只作为可选体验，不作为验收必要条件。
5. 更新本文实施状态和实际 API 说明，附关键文件位置、验证结果与遗留限制。尊重工作区已有修改；不回退或提交其他 agent 的工作。

未来 TaskManager 的工作另开阶段，先与 `monad-orchestration-pattern.md` 的 TaskGraph 状态、取消记录和重试关联统一，再写实现；不能因本轮抽出 slot 就宣布全局管理已经完成。

## 15. 实施状态（2026-10-08）

`TaskBinding` 与 `TaskSlot` 已从 `monad.rs` 抽离到独立模块；三个 fixture 与 `MainMonad` 全部迁移到新抽象。初次实现的审核见第 16 节，修复及当前验收结果见第 17 节；本节代码行号是初次实施时的位置。

### 15.1 实际 API

公共类型与函数（`crates/smallhead-core/src/monad/task.rs`）：

| 名称 | 位置 | 角色 |
|---|---|---|
| `pub struct TaskBinding` | `monad/task.rs` | 已校验绑定及本地计时载体，字段私有，仅 `TaskBinding::from_handoff` 可构造。用 `task_id()` / `parent_task_id()` / `result_slot()` 只读查询，父模块用 `into_parts()` 消费标识。不是持久化 DTO。 |
| `TaskBinding::from_handoff` | `monad/task.rs:68-94` | 6 项校验：monad_id 匹配、schema_version、task_id 非空白、result_slot 非空白、parent_task_id 若有则非空白、attempt_id > 0。任一不过返回 `MonadError`。 |
| `pub struct TaskSlot` | `monad/task.rs:101-104` | 持有 `Option<TaskBinding>`，状态机守门。 |
| `TaskSlot::new / is_bound / peek` | `monad/task.rs:108-121` | 构造 / 状态查询 / 不可变读取。 |
| `TaskSlot::try_bind` | `monad/task.rs:133-144` | 校验通过后写入，返回不可变引用；已绑定时返回 `Err("monad already has an active task")`，**不**覆盖原 binding。 |
| `TaskSlot::take` | `monad/task.rs:150-154` | 单次消费；未绑定时返回 `Err("no active task; call start(handoff) before step()")`。 |
| `TaskSlot::clear` | `monad/task.rs:157-159` | 幂等清理。 |
| `TaskSlot::usage_after` | `monad/task.rs:165-171` | 由 `bound_at: Instant` 计算 `elapsed_ms`；只用于本进程耗时。 |

模块边界（`crates/smallhead-core/src/monad.rs`）：

- `mod task; pub use task::{TaskBinding, TaskSlot};` —— 类型通过 `smallhead_core::monad::{TaskBinding, TaskSlot}` 公开；根模块没有重导出这两个类型，原 TaskBinding 也不是公共 API。
- 三个 fixture（`EchoMonad` / `CalculationMonad` / `GreetingMonad`）与 `MainMonad` 全部持 `task: TaskSlot` 字段（`MainMonad.current_task: TaskSlot` 见 `monad.rs:615`）。
- `MainMonad::start`（`monad.rs:704-721`）遵循"先准备业务、再 try_bind、最后写回 self"的三段写法：业务输入（`frame` 种子消息）先在局部变量准备好，`try_bind` 成功后才写入实例字段。

### 15.2 关键代码位置

| 行为 | 文件:行 |
|---|---|
| 抽离的 `task` 模块入口 | `crates/smallhead-core/src/monad.rs:550-551` |
| 6 项校验函数 | `crates/smallhead-core/src/monad/task.rs:68-94` |
| 状态机守门（`try_bind` / `take` / `clear`） | `crates/smallhead-core/src/monad/task.rs:133-159` |
| `MainMonad.current_task: TaskSlot` | `crates/smallhead-core/src/monad.rs:615` |
| `MainMonad::start` 三段写法 | `crates/smallhead-core/src/monad.rs:704-721` |
| `MainMonad::step` 失败时清理 frame / started / current_task | `crates/smallhead-core/src/monad.rs:775-783` |
| `MainMonad::cancel` 同时清 binding / frame / started 并转发 cancel 信号 | `crates/smallhead-core/src/monad.rs:819-836` |
| `EchoMonad` / `CalculationMonad` / `GreetingMonad` `bind()` 与 `step()` | `crates/smallhead-core/src/monad.rs:914-979` / `1000-1110` / `1130-1192` |
| 借用冲突的规避：先 `let expected = self.monad_id().clone();` 再 `try_bind` | `monad.rs:713-714, 924-925, 1008-1009, 1140-1141` |
| 局部 move 的规避：先 `let usage = TaskSlot::usage_after(&task, ...);` 再 struct literal | `monad.rs:962-963, 1086-1087, 1184-1185` |

### 15.3 验证结果

| 命令 | 结果 |
|---|---|
| `cargo check -p smallhead-core` | 通过（无 warning 引入） |
| `cargo check -p smallhead-tui` | 通过（未引入新 warning） |
| `cargo test -p smallhead-core` | **93 passed; 0 failed**（基线 83 个，新增 10 个） |
| `cargo build -p smallhead-core --example demo` | 通过 |
| `cargo fmt -p smallhead-core` 涉及 `monad.rs` / `monad/task.rs` | 无 diff（已格式化） |

新增的 10 个测试：

1. `monad::task::tests::new_slot_is_empty`（`monad/task.rs:213`）
2. `monad::task::tests::try_bind_then_double_bind_keeps_original`（`monad/task.rs:220`）—— 第 14.2 表 "已绑定时再次 start" 行。
3. `monad::task::tests::failed_try_bind_leaves_slot_untouched`（`monad/task.rs:244`）—— 第 14.2 表 "handoff 校验失败" 行。
4. `monad::task::tests::take_consumes_and_releases`（`monad/task.rs:266`）
5. `monad::task::tests::take_on_empty_returns_err`（`monad/task.rs:283`）—— "未绑定时执行" 行。
6. `monad::task::tests::clear_is_idempotent`（`monad/task.rs:291`）
7. `monad::task::tests::peek_does_not_consume`（`monad/task.rs:304`）
8. `monad::task::tests::from_handoff_rejects_each_validation_path`（`monad/task.rs:315`）—— 6 项校验逐项独立验证。
9. `monad::task::tests::usage_after_measures_elapsed`（`monad/task.rs:343`）
10. `monad::tests::main_step_failure_cleans_state_and_allows_restart`（`monad.rs:2405`）—— 第 14.2 表 "执行返回 Err" 行。**用空 `MockProvider` 队列触发 `MockProvider exhausted` 错误**，验证 `MainMonad::step` 失败后 frame / started / current_task 全部清空，fresh `start()` 不被卡住。

保留并跑通的旧测试（共 83 个，含 §14.3 提到的 "重复绑定" / "无绑定执行" / "等待后补充" / "取消" / "接收新任务" 全部分支）：

- `monad::tests::fixtures_preserve_binding_and_reject_invalid_lifecycle` —— 重复绑定、取消、接收新任务。
- `monad::tests::rejected_handoffs_leave_instances_unbound` —— 6 项校验组合拒绝。
- `monad::tests::calculation_waiting_context_can_be_resubmitted` —— WaitingContext 后重新 start。
- `monad::tests::main_rejects_overwrite_and_resets_after_completion_and_cancel` —— Main 的 overwrite / completion / cancel 清理路径。
- `monad::tests::main_monad_step_invokes_agent_loop_and_returns_completed` —— Main 成功路径。
- `monad_bridge::tests::*` —— 业务分发与 handoff/result 协议（wire format 未受影响）。

### 15.4 遗留限制

1. **第 4.3 节的 `TaskManager` 仍未引入**。当前 `TaskSlot` 只服务单实例生命周期；跨实例状态（取消记录、重试计数、TaskGraph 关联）仍由调度器负责。第 14.3 强调"未来 TaskManager 的工作另开阶段"。
2. **`TaskObserver` 仍未引入**。第 4.4 节描述的观察者 trait 留给后续轮次。
3. **持久化未承诺**。`TaskBinding::bound_at: Instant` 不适合跨进程持久化；`MonadResult::finished_at_ms` 继续使用 epoch 毫秒以保证跨进程可比。TaskManager 阶段会重新设计可持久化的任务记录结构。
4. **MainMonad 单 turn 桥接不变**。`MainMonad::step` 仍只跑单 turn（`run_one_turn`），多 turn / 执行中取消 / 持久化由后续 runtime 接入。`step` 失败时清理本轮 frame / started，binding 已消费；内层 AgentLoop 的对话历史保留。
5. **未引入 typestate / 锁 / 重试占位字段**。与第 2 节 YAGNI 一致。
6. **不自动重放可能产生副作用的业务**。`step` 失败后只清状态，不会重发同 task_id 的 handoff；新 `start` 必须由调用方显式发起。这与第 14.2 表 "执行返回 Err" 行要求一致。
7. **没有跨进程的失败回放**。如果调度器在 `step` 失败后崩溃，`TaskSlot` 的状态会随进程退出丢失；重连后需要新 `start`。TaskManager 阶段会补 replay log。
8. **`cargo fmt` 在 `monad.rs` / `monad/task.rs` 上无 diff**；其他工作区文件（`smallhead-session`、`smallhead-tools` 等）有先于本轮的 fmt 差异，按要求"不回退或提交其他 agent 的工作"，**未触碰**。

### 15.5 兼容性确认

- 公共导入路径：`use smallhead_core::monad::{TaskBinding, TaskSlot}`；不是根级导入。
- `MonadLoop` trait surface（`monad_id` / `catalog_entry` / `default_tool_set` / `start` / `step` / `cancel`）未变；fixture 与 Main 的 `impl MonadLoop` 同步更新。
- handoff / result JSON wire format 未变（`SCHEMA_VERSION_V1` 仍为 `"monad-handoff/v1"`）；既有 round-trip 测试（`handoff_minimal_roundtrip` / `result_completed_roundtrip` 等）继续通过。
- `monad_bridge.rs` 未改：业务分发仍走 `StaticMonadRegistry::with_fixtures()` → `child.start(handoff)` → `child.step()`，返回的 `{handoff, result, model_route}` JSON 不变。

## 16. 实现审核（2026-10-08）

实际复跑 `cargo test -p smallhead-core`：93 passed；`cargo check -p smallhead-core -p smallhead-tui --all-targets` 通过（包括 examples）。局部绑定抽离、三个 fixture 的事务式输入提交、Main 的 Provider 错误后本轮状态清理方向正确。尚不能认定第 14 节契约全部满足，以下问题需修订。

1. **P1：Main 重复取消会阻塞。** `monad.rs:819-831` 每次 cancel 都向 AgentLoop 的容量为 8 的通道 `send(()).await`，接收端当前没有消费。用同一个 MainMonad 连续调用 cancel，本地复现前八次返回，第九次在 100ms timeout 下超时；没有任务时也如此。这是沿用的旧问题，但本轮明确要求可重复取消，仍需补齐。应明确无任务/重复取消语义，避免 best-effort 通知等待一个无人消费的满队列；同时验证取消后重新 start/step。只测 `TaskSlot.clear()` 幂等不够。
2. **P2：公开字段绕过已校验类型的构造约束。** `monad/task.rs:45-54` 的四个字段全是 pub，外部代码可以直接构造空 TaskId/ResultSlot 的 TaskBinding，也可以修改已校验绑定。临时 example 已编译验证。当前 TaskSlot 的 inner 私有且 try_bind 仍会校验，所以尚未发现现有 slot 路径接受非法绑定；问题在于新增公共类型不符合第 14.1 节的封装契约，且“仅 from_handoff 可构造”的文档声明不成立。用私有字段配合 getter/消费方法，或限制字段和类型的可见性；内部是否使用 pub(super) 应按实际模块边界决定。
3. **文档与 API 有差异。** `lib.rs` 根级导出列表不含 TaskBinding/TaskSlot，实际公开路径是 `smallhead_core::monad::{TaskBinding, TaskSlot}`。第 15.1/15.5 节声称根级导入兼容不正确，原 TaskBinding 也不是公共 API。bound_at 仍放在 TaskBinding 中，未按第 14.1 节分离运行时计时，应记录为偏离或调整实现；Instant 是单调时钟，不是 wall-clock。第 15.4 节所称“start 失败清理”实际新增在 step 错误路径。

临时复现 example 已删除，审核未修改业务实现。第 15 节“生命周期契约已逐项覆盖”应改为“主要路径已覆盖，以上边界待补”。保留内层 AgentLoop 的对话历史不等于清理本轮 Main.frame；后续多任务/跨会话管理时还应明确对话历史的归属，这不由 TaskSlot 自动解决。

## 17. 审核修复与当前验收（2026-10-08）

- Main.cancel 先清本轮状态；没有绑定任务时重复取消直接成功。有任务时通过 try_send 发出 best-effort 通知，Full 表示已有通知待处理，不等待队列空位；Closed 返回明确错误。AgentLoop 当前仍未消费取消通知，外部副作用的中断保证留给后续 runtime。
- TaskBinding 字段全部私有，只读 getter 不暴露可变引用；into_parts 仅父模块可用。四个 Monad 在计算 usage 后消费身份构造结果。编译失败文档测试防止重新开放非法的外部字面量构造。
- 为控制本轮抽象规模，TaskBinding 保留私有的本进程单调时钟计时数据，明确是运行时载体，而不是纯身份 DTO；未来可持久化 TaskRecord 应独立定义，不序列化该类型。修正根级导入、纯数据和 start/step 清理说明。
- 新测试 main_repeated_cancel_with_full_queue_allows_new_task 在同一实例上执行 16 次 start/cancel，覆盖满队列；随后重复 cancel 16 次，最后新任务经 mock provider 成功完成并验证回填身份与最终文字。
- 验证：cargo test -p smallhead-core 通过，94 个单元测试与 1 个 compile_fail 文档测试；cargo check -p smallhead-core -p smallhead-tui --all-targets 通过（含 examples）；修改的两个 Rust 文件通过 rustfmt --check，monad.rs 的 git diff --check 通过。
