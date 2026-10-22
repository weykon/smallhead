//! Monad 业务编排：协议、任务绑定、循环接口、注册表与工具权限。
//!
//! 协议和编排设计见仓库中的 `docs/monad-orchestration-pattern.md`。
//!
//! - `protocol.rs`：交接、结果、预算与互不混用的 ID 类型。
//! - `task.rs`：单个任务的绑定、消费与清理。
//! - `loop_trait.rs`：业务循环接口与目录条目。
//! - `main_monad.rs`：将主 AgentLoop 适配为业务循环。
//! - `fixtures.rs`：不调用 LLM 的本地示例业务。
//! - `registry.rs`：业务发现与创建独立实例的工厂。
//! - `tool_gate.rs`：运行时工具权限检查。
//! - `runtime/`：独立的任务准入、并发、验收、取消与收尾。
//! - `agent.rs`：带独立上下文和预算的模型 Monad。
//! - `delegate.rs`：宿主委派入口，不复制模型循环或调度逻辑。
//!
//! 本模块的公开类型通过 `smallhead_core::monad::…` 导入；
//! crate 根级仅重新导出其中的常用类型，完整清单见 `lib.rs`。
//!
//! ## 设计不变量
//!
//! 1. [`TaskId`]、[`MonadId`]、[`ResultSlot`] 使用独立类型，避免串用。
//! 2. [`MonadHandoff`] / [`MonadResult`] 的 JSON 字段遵循稳定的协议。
//! 3. 业务自行校验 `inputs`，任务绑定由 [`TaskSlot`] 校验和管理。
//! 4. 结果按任务与回填槽位识别，不依赖到达顺序。
//! 5. [`MainMonad`] 桥接主循环；[`MonadRuntime`] 管理受委派实例的生命周期。
//! 6. 运行记录限于当前进程，不承诺持久化调度或外部副作用 exactly-once。

mod protocol;
pub use protocol::{
    Budget, ContextRef, MonadHandoff, MonadId, MonadResult, ResourceUsage, ResultSlot, TaskId,
    TraceMeta, SCHEMA_VERSION_V1,
};

mod loop_trait;
pub use loop_trait::{main_monad_id, CatalogEntry, MonadError, MonadLoop};

mod task;
pub use task::{TaskBinding, TaskSlot};

mod main_monad;
pub use main_monad::MainMonad;

mod fixtures;
pub use fixtures::{CalculationMonad, EchoMonad, GreetingMonad};

mod registry;
pub use registry::{MonadRegistry, RegistryError, StaticMonadRegistry};

mod tool_gate;
pub use tool_gate::{GateError, ToolCall, ToolGate, ToolPermissions};

pub mod runtime;
pub use runtime::{
    BindingValidator, CleanupStatus, CompletionValidator, MonadReceipt, MonadRun, MonadRuntime,
    MonadRuntimeConfig, RunSnapshot, RunState,
};

mod agent;
pub use agent::AgentMonad;
mod collaboration;
pub use collaboration::*;
mod report;
pub use report::{MonadReportTool, COLLABORATION_GUIDANCE};
mod delegate;
mod delegate_control;
pub use delegate::{DelegateTool, DELEGATION_GUIDANCE};

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
