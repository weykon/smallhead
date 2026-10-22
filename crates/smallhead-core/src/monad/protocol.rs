//! Monad 协议 wire format —— 对应 `docs/monad-orchestration-pattern.md` §3。
//!
//! 本文件**只**承载 `MonadHandoff` / `MonadResult` 这两类序列化协议 + 它们依赖的
//! ID newtype 与辅助结构。任何 monad 派生类型、`MainMonad` 适配器、`MonadRegistry`
//! 都不在本文件。
//!
//! ## 设计不变量
//!
//! 1. **三段 ID 不可互换** —— `TaskId` / `MonadId` / `ResultSlot` 各自独立 newtype。
//!    避免把 task_id 误填到 monad_id。
//! 2. **`MonadHandoff` / `MonadResult` 是 wire format** —— JSON 字段名定下来就锁定。
//!    `schema_version` 字段强制存在，任何破坏性改动必须 bump。
//! 3. **`inputs: serde_json::Map`** —— 故意不强类型，每个 monad 自己 validate。
//!    第一版不强类型，P1 跑稳再考虑给每个 monad 定义 Inputs: DeserializeOwned。
//! 4. **结果不靠顺序** —— `MonadResult` 必须带 `task_id + parent_task_id + result_slot`
//!    三元组，调度器按它回填，不靠到达顺序。

use serde::{Deserialize, Serialize};

// ============================================================================
// Schema 版本常量
// ============================================================================

/// 当前 handoff schema 版本。Major bump → 拒绝旧消息；minor bump → 走默认值。
pub const SCHEMA_VERSION_V1: &str = "monad-handoff/v1";

// ============================================================================
// ID newtypes —— 编译期防止 ID 串用
// ============================================================================

/// Monad 标识（注册表中的唯一名字，如 `"calculation"` / `"inventory"`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonadId(pub String);

impl MonadId {
    /// 从任意字符串构造。常量场景直接用 `MonadId("calculation".into())`。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// 任务实例标识。每次派发生成一个新 uuid-v7。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    /// 生成新 uuid-v7 作为 task_id。
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// 结果回填槽位（`"calculations.first"` 这种 dotted path）。
/// 父任务图里唯一锁定结果位置，允许乱序回填。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResultSlot(pub String);

// ============================================================================
// 上下文引用 —— MonadHandoff.relevant_context 用
// ============================================================================

/// 局部上下文的一行引用。
///
/// - `UserQuote`: 主任务原话引用，防止 monad 重复 prompt injection。
/// - `PrevResult`: 前置 task 的结果摘要。
/// - `MemoryHit`: LanceDB 检索出的记忆命中（P4 接 fastembed 后用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextRef {
    /// 主任务原话引用，防止 monad 重复 prompt injection。
    UserQuote {
        /// 引用的用户原话。
        text: String,
    },
    /// 前置 task 的结果摘要。
    PrevResult {
        /// 提供结果摘要的 task_id。
        task_id: TaskId,
        /// 结果摘要（由产出的 monad 生成）。
        summary: String,
    },
    /// LanceDB 检索出的记忆命中（P4 接 fastembed 后用）。
    MemoryHit {
        /// 来源表名（MemoryTable.dir_name()）。
        source: String,
        /// 记忆行 id。
        ref_id: String,
        /// 命中片段的摘要。
        summary: String,
    },
    // P4 扩展位: External { url, snippet } / Skill { name, version }
}

// ============================================================================
// 资源预算与追踪元数据
// ============================================================================

/// 调度与执行预算。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// 允许的最大步骤数（ProviderRequest 轮次）。
    pub max_steps: u32,
    /// 总超时毫秒。
    pub timeout_ms: u64,
    /// 总 token 上限（包括输入 prompt 与输出）。
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 并发 IO 操作上限（Phase 1 默认 1，Phase 3 放开）。
    #[serde(default = "default_concurrent_io")]
    pub concurrent_io: u32,
}

fn default_max_tokens() -> u32 {
    4000
}
fn default_concurrent_io() -> u32 {
    1
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_steps: 6,
            timeout_ms: 30_000,
            max_tokens: default_max_tokens(),
            concurrent_io: default_concurrent_io(),
        }
    }
}

/// 追踪元数据：关联到父 RequestFrame 的版本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceMeta {
    /// 父 RequestFrame 的版本 ref，用于追溯回主循环状态机。
    pub frame_ref: String,
    /// 本次请求的唯一 id（可与日志/x-request-id 对齐）。
    pub request_id: String,
}

/// 资源使用计数（由 MonadLoop 在完成时汇报）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceUsage {
    /// 已消耗的步骤数（ProviderRequest 轮次）。
    pub steps_taken: u32,
    /// 已消耗的 token 数（输入 + 输出合计）。
    pub tokens_used: u32,
    /// 已消耗的墙钟毫秒。
    pub elapsed_ms: u64,
}

// ============================================================================
// MonadHandoff —— 派发包
// ============================================================================

/// 主任务 → MonadLoop 的派发包。
///
/// wire format 与 Rust 类型一一对应。JSON 字段名锁定为 snake_case。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonadHandoff {
    /// 必须为 [`SCHEMA_VERSION_V1`]。反序列化时由调用方校验。
    pub schema_version: String,

    /// 本次 handoff 的 task_id（uuid-v7）。
    pub task_id: TaskId,
    /// 父 task 的 id（顶层任务时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    /// 目标 monad 的 id（注册表中的唯一键）。
    pub monad_id: MonadId,
    /// 重试 attempt 编号，首次 1。
    pub attempt_id: u32,

    /// 主任务原话 objective。
    pub objective: String,
    /// monad 自定义的输入参数（每个 monad 自己 validate）。
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// 注入到该 monad 的局部上下文（原话引用 + 前置结果 + 记忆命中）。
    #[serde(default)]
    pub relevant_context: Vec<ContextRef>,
    /// 硬性约束列表（模型返回违反时该步作废）。
    #[serde(default)]
    pub constraints: Vec<String>,
    /// 判定任务完成的条件描述。
    pub completion_criteria: String,
    /// 在父任务图中的结果回填槽位（`dotted.path`）。
    pub result_slot: ResultSlot,

    /// 依赖的其他 task_id，全部完成后才可派发。
    #[serde(default)]
    pub depends_on: Vec<TaskId>,

    /// 资源预算与并发上限。
    pub budget: Budget,

    /// 派发时刻的 epoch 毫秒。
    pub issued_at_ms: i64,
    /// 追踪元数据。
    pub trace: TraceMeta,
}

// ============================================================================
// MonadResult —— 回填包
// ============================================================================

/// MonadLoop → 主任务的回填包。
///
/// 由 `status` tag 区分四种状态。**不靠到达顺序** —— 三元组
/// `(task_id, parent_task_id, result_slot)` 唯一锁定回填位置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MonadResult {
    /// 任务完成。
    Completed {
        /// 本次对应 task 的 id。
        task_id: TaskId,
        /// 父 task 的 id（顶层任务时为 None）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<TaskId>,
        /// 回填到哪个槽位。
        result_slot: ResultSlot,
        /// 产出结果的 monad id。
        monad_id: MonadId,
        /// monad 产出的结构化结果（JSON，每个 monad 自己定义 schema）。
        output: serde_json::Value,
        /// 引用 execution_id 等可重放锚点。
        evidence_refs: Vec<String>,
        /// 资源使用计数。
        usage: ResourceUsage,
        /// 完成时刻的 epoch 毫秒。
        finished_at_ms: i64,
    },
    /// Monad 需要更多上下文才能完成。
    WaitingContext {
        /// 本次对应 task 的 id。
        task_id: TaskId,
        /// 解释缺什么的人话。
        reason: String,
        /// 缺哪些字段名（可让主循环提示用户）。
        missing: Vec<String>,
    },
    /// 任务失败。
    Failed {
        /// 本次对应 task 的 id。
        task_id: TaskId,
        /// 错误描述（可被人读）。
        error: String,
        /// 是否可重试。
        retryable: bool,
    },
    /// 任务被取消。
    Cancelled {
        /// 本次对应 task 的 id。
        task_id: TaskId,
        /// 取消原因（parent cancelled / timeout / 资源抢占等）。
        reason: String,
    },
}

impl MonadResult {
    /// 返回结果携带的 task_id，用于调度器路由。
    pub fn task_id(&self) -> &TaskId {
        match self {
            Self::Completed { task_id, .. }
            | Self::WaitingContext { task_id, .. }
            | Self::Failed { task_id, .. }
            | Self::Cancelled { task_id, .. } => task_id,
        }
    }
}
