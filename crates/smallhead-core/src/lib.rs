//! `smallhead-core` — Agent core abstractions.
//!
//! 这是 smallhead 项目的最内层 crate。它只包含 **不依赖任何外部世界** 的类型与逻辑：
//!
//! - [`message`] — `AgentMessage` 联合（pi-style，Rust enum-dispatch）
//! - [`provider`] — `Provider` trait（opencode V2 风格）+ `MockProvider`（无 API key 跑 demo）
//! - [`provider::minimax`] — `MiniMaxProvider`（Anthropic 协议真实 API，`MINIMAX_API_KEY` 鉴权）
//! - [`tool`] — `ToolKind` enum（不依赖 big-head 的 dyn Tool）
//! - [`agent_loop`] — `AgentLoop::run_one_turn`（nested 2-while，pi 移植）
//!
//! 主循环示例见仓库中的 `crates/smallhead-core/examples/demo.rs`。
//! Monad 的完整公开接口通过 [`monad`] 导入，根级仅重新导出常用类型。
//!
//! ## 设计不变量
//!
//! 1. `AgentMessage::Notification / Steering / FollowUp / Compaction / BranchMarker`
//!    不通过 `to_llm` 直接进入 LLM context；AgentLoop 的 provider projection
//!    会把 `Compaction` 摘要作为可见上下文注入。
//! 2. `Provider` trait 是显式 stream，不是隐式 `run`-and-await-callback——每个 event 可以独立记录。
//! 3. Built-in tools use enum dispatch; host integrations use `ExternalTool`
//!    so SDK dependencies and connection ownership stay outside core.
//! 4. `AgentLoop` 的核心决策（`step`）是 pure function，输入 state + event 输出新 state + actions。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent_loop;
pub mod artifact;
pub mod choice;
pub mod diagnostics;
pub mod execution;
pub mod fast_decision;
pub mod fast_route;
pub mod loop_environment;
pub mod loop_policy;
pub mod message;
pub mod monad;
pub mod monad_bridge;
pub mod plan;
pub mod provider;
pub mod spec_review;
pub mod tool;

// 重新导出最常用类型，方便 `use smallhead_core::*;`
pub use agent_loop::{
    compact_messages_rolling, estimate_context_projection, project_agent_messages_for_provider,
    repair_interrupted_history, AgentLoop, ContextProjectionStats, LoopBudget, LoopState,
    LoopSummary, RollingCompactionReport, SteeringInbox, SteeringInboxFull, SteeringRecord,
    SteeringSnapshot, TurnEvent,
};
pub use choice::{
    ChoiceBroker, ChoiceError, ChoiceId, ChoiceIntent, ChoiceOption, ChoiceRequest, ChoiceResponse,
};
pub use message::{AgentMessage, ContentBlock, LlmMessage, ToLlmMessage, Usage};
pub use monad::{
    main_monad_id, Budget, CatalogEntry, ContextRef, MainMonad, MonadError, MonadHandoff, MonadId,
    MonadLoop, MonadResult, ResourceUsage, ResultSlot, TaskId, TraceMeta, SCHEMA_VERSION_V1,
};
pub use plan::UpdatePlanTool;
pub use provider::minimax::{
    MiniMaxConfig, MiniMaxProvider, MINIMAX_DEFAULT_BASE_URL, MINIMAX_DEFAULT_MODEL,
};
pub use provider::{
    LlmStopReason, MockProvider, MockResponse, Provider, ProviderError, ProviderEvent,
    ProviderRequest,
};
pub use spec_review::{PublishSpecTool, SpecBroker, SpecDocument, SpecError};
/// 重新导出 tokio_util::sync::CancellationToken,避免下游 crate 都得直接
/// 依赖 tokio-util 才有 abort signal。
pub use tokio_util::sync::CancellationToken;
pub use tool::{
    set_agent_reach_handler, set_web_search_handler, AcceptedInput, AddTool, AgentReachHandler,
    AgentReachTool, ArtifactReadTool, BashTool, CalculateTool, ChoiceTool, DivTool, EchoTool,
    EditTool, ExternalTool, InputSource, MulTool, ReadTool, SubTool, ToolContext, ToolError,
    ToolKind, ToolResult, ToolResultProjection, ToolSpec, WebSearchHandler, WebSearchTool,
    WriteTool,
};
