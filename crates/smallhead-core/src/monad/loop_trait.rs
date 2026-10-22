//! Monad 抽象层:MonadLoop trait + 错误 + 顶层 monad id + 目录条目。
//!
//! 对应 `docs/monad-orchestration-pattern.md`:
//! - §2.1: `MonadLoop` trait
//! - §2.2: 目录条目(`CatalogEntry`)
//! - §3.4: `MonadResult::Failed` 用到的 `MonadError`
//!
//! 本文件定义业务循环接口；主循环适配器见 [`crate::monad::MainMonad`]，
//! 本地示例业务的实现位于 `fixtures.rs`。

use std::sync::OnceLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::monad::protocol::{MonadHandoff, MonadId, MonadResult};
use crate::tool::ToolKind;

/// 顶层 MainMonad 的固定 id —— 用 [`main_monad_id`] 取,
/// 因 `MonadId` 是非 const 构造(`String::new()` 不是 const fn),
/// 用 `OnceLock` 静态初始化,零运行时开销、无新依赖。
fn main_monad_id_slot() -> &'static OnceLock<MonadId> {
    static ID: OnceLock<MonadId> = OnceLock::new();
    &ID
}

/// 返回顶层 MainMonad 的全局 MonadId。第一次调用时初始化。
pub fn main_monad_id() -> &'static MonadId {
    main_monad_id_slot().get_or_init(|| MonadId::new("main"))
}

/// 目录条目 —— 给主循环展示用的短摘要。
///
/// 跟 [`crate::monad::MonadHandoff`] 是不同的概念:
/// - `CatalogEntry` 是**注册表层**的展示信息(短文 + 工具列表)。
/// - `MonadHandoff` 是**调度层**的一次性派发包。
///
/// 见 `docs/monad-orchestration-pattern.md` §2.2。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// 一句话描述(给主循环 UI/日志展示用)。
    pub summary: String,
    /// 此 monad 在目录中可见的工具集合(短名,不带 spec)。
    /// 实际可见集合由 `ToolGate` 在 P1 决定。
    pub visible_tools: Vec<String>,
    /// 关键词索引,用于 [`crate::monad::MonadRegistry::search`] 的关键词匹配。
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// 顶层 Monad 错误 —— MVP 第一片是 String newtype。
///
/// Slice 4 之后才细化为带 source / retryable 提示的 enum。
/// 见 `docs/monad-orchestration-pattern.md` §3.4 MonadResult::Failed 用到的 `MonadError`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonadError(pub String);

impl std::fmt::Display for MonadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MonadError {}

impl MonadError {
    /// 字符串便捷构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Monad 抽象 —— 每个 monad 必须实现的契约。
///
/// 见 `docs/monad-orchestration-pattern.md` §2.1。
///
/// 工厂返回未绑定实例；start 绑定任务，step 执行。fixture 的一次 step 消费绑定，
/// Completed 后可接新任务；WaitingContext 后须 start 补充后的 handoff 才能再执行。
/// MainMonad 当前只桥接单 turn；不保证持久化恢复或外部副作用的 exactly-once。
///
/// 故意 **不** 给 default impl:每个 monad 自己决定生命周期语义,
/// 强迫实现者读 trait 文档。
#[async_trait]
pub trait MonadLoop: Send + Sync {
    /// Optional host-bound communication endpoint. Runtime ends it only after
    /// validation and cleanup settle, so a returned step is not a false terminal.
    fn collaboration_channel(&self) -> Option<super::ChildChannel> {
        None
    }

    /// 注册时唯一。后续 TaskGraph / ResultSlot 都引用此 id。
    fn monad_id(&self) -> &MonadId;

    /// 短摘要(供主循环目录)。不应包含任何方法细节。
    fn catalog_entry(&self) -> &CatalogEntry;

    /// 此 monad 默认可见的工具集合。
    /// 主循环不直接看到这些工具,只有被 handoff 后才在该 monad 的 frame 内可见。
    fn default_tool_set(&self) -> Vec<ToolKind>;

    /// 接收 handoff,做局部初始化(读 inputs、构造初始消息、注入约束)。
    ///
    /// 由 MonadRuntime 调用一次；模型 Monad 在这里构造独立上下文和预算，
    /// 确定性业务在这里校验输入和绑定任务。初始化失败同样会调用 cancel。
    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError>;

    /// 走一步。
    ///
    /// 未绑定任务时返回错误；具体等待和恢复语义由实现声明。
    async fn step(&mut self) -> Result<MonadResult, MonadError>;

    /// 硬性取消 hook。允许清理资源(关连接、撤回 inflight 请求)。
    ///
    /// runtime 先丢弃 start/step future，再有界等待本 hook，避免并发可变借用。
    /// 实现必须报告未能确认的资源清理；不能将发送停止请求等同进程回收。
    async fn cancel(&mut self) -> Result<(), MonadError>;
}
