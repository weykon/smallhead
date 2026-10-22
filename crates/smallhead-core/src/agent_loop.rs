//! AgentLoop：nested 2-while loop 的 Rust 移植版。
//!
//! 设计参照 pi-mono `packages/agent/src/agent-loop.ts`（agentLoop 函数）。
//! 简化版本（MVP）：
//!
//! - **OUTER while**：处理 follow-up 队列（本 MVP demo 简化版：单轮运行）。
//! - **INNER while**：处理 assistant stream + tool execution。
//!
//! 当前 MVP 是**单 turn 版本**——完成一次"user message -> assistant stream ->
//! tool execution（如果需要）-> assistant final" 后返回。
//! Live steering is consumed at provider and tool boundaries within this turn.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::message::{AgentMessage, ContentBlock, LlmMessage, ToLlmMessage};
use crate::provider::{LlmStopReason, Provider, ProviderEvent, ProviderRequest};
use crate::tool::{AcceptedInput, ToolContext, ToolKind, ToolResult};

/// An accepted steering message. IDs are monotonic within an inbox.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SteeringRecord {
    /// Acceptance ID, scoped to this inbox.
    pub id: u64,
    /// User content to insert at the next safe boundary.
    pub content: Vec<ContentBlock>,
    /// User message timestamp in milliseconds.
    pub timestamp: i64,
}

/// Atomic persistence view: accepted steering is either pending or in messages.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SteeringSnapshot {
    /// Changes whenever pending records or committed history change.
    pub generation: u64,
    /// Last committed conversation history.
    pub messages: Vec<AgentMessage>,
    /// Accepted steering not yet committed to that history.
    pub pending: Vec<SteeringRecord>,
}

/// The bounded steering queue rejected this message; nothing was accepted.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("steering inbox full (capacity {capacity})")]
pub struct SteeringInboxFull {
    /// Maximum number of pending records.
    pub capacity: usize,
}

#[derive(Debug, Default)]
struct SteeringQueue {
    generation: u64,
    next_id: u64,
    pending: VecDeque<SteeringRecord>,
    receipts: Vec<u64>,
    messages: Vec<AgentMessage>,
    capacity: usize,
}

/// Shared, externally owned queue. Keep a clone after the worker finishes and
/// dispatch any remaining `pending()` records in the next worker. Receipts mean
/// committed to history, not merely received by a running future.
#[derive(Debug, Clone)]
pub struct SteeringInbox {
    queue: Arc<parking_lot::Mutex<SteeringQueue>>,
    notify: Arc<tokio::sync::Notify>,
}

impl Default for SteeringInbox {
    fn default() -> Self {
        Self::with_capacity(64)
    }
}

impl SteeringInbox {
    /// Create an inbox with a bounded number of pending messages.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: Arc::new(parking_lot::Mutex::new(SteeringQueue {
                capacity,
                ..Default::default()
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Accept user content and return its ID. Retained until committed by a loop.
    pub fn push(
        &self,
        content: Vec<ContentBlock>,
        timestamp: i64,
    ) -> Result<u64, SteeringInboxFull> {
        self.try_push(content, timestamp)
    }

    /// Nonblocking bounded acceptance. Rejection leaves the queue unchanged.
    pub fn try_push(
        &self,
        content: Vec<ContentBlock>,
        timestamp: i64,
    ) -> Result<u64, SteeringInboxFull> {
        let mut queue = self.queue.lock();
        if queue.pending.len() >= queue.capacity {
            return Err(SteeringInboxFull {
                capacity: queue.capacity,
            });
        }
        queue.next_id += 1;
        let id = queue.next_id;
        queue.pending.push_back(SteeringRecord {
            id,
            content,
            timestamp,
        });
        queue.generation += 1;
        drop(queue);
        self.notify.notify_one();
        crate::diagnostics::emit(
            "execution",
            "steering_accepted",
            serde_json::json!({"id": id}),
        );
        Ok(id)
    }

    /// Read history and pending records under the same lock for persistence.
    pub fn snapshot(&self) -> SteeringSnapshot {
        Self::snapshot_locked(&self.queue.lock())
    }

    /// Latest atomic history and pending snapshot, including its generation.
    pub fn latest_snapshot(&self) -> SteeringSnapshot {
        self.snapshot()
    }

    /// Seed persistence history while idle, preserving pending records and
    /// receipts. Do not call while a worker is attached and running. Initialize
    /// `AgentLoop::state.messages` before attaching this inbox to that loop.
    pub fn seed_history(&self, messages: Vec<AgentMessage>) {
        let mut queue = self.queue.lock();
        queue.messages = messages;
        queue.generation += 1;
    }

    /// Clone a persistence snapshot only when its generation has changed.
    pub fn snapshot_if_changed(&self, generation: u64) -> Option<SteeringSnapshot> {
        let queue = self.queue.lock();
        (queue.generation != generation).then(|| Self::snapshot_locked(&queue))
    }

    fn snapshot_locked(queue: &SteeringQueue) -> SteeringSnapshot {
        SteeringSnapshot {
            generation: queue.generation,
            messages: queue.messages.clone(),
            pending: queue.pending.iter().cloned().collect(),
        }
    }

    /// Snapshot uncommitted records without removing them.
    pub fn pending(&self) -> Vec<SteeringRecord> {
        self.queue.lock().pending.iter().cloned().collect()
    }

    /// Drain IDs whose user messages were committed to loop history.
    pub fn take_receipts(&self) -> Vec<u64> {
        std::mem::take(&mut self.queue.lock().receipts)
    }

    fn has_pending(&self) -> bool {
        !self.queue.lock().pending.is_empty()
    }

    async fn ready(&self) {
        loop {
            let notified = self.notify.notified();
            if self.has_pending() {
                return;
            }
            notified.await;
        }
    }
}

/// Complete missing protocol pairs without replaying tools. Existing results
/// remain authoritative; an interrupted call may already have had side effects.
/// Returns the number of synthetic results inserted. Safe to call repeatedly.
pub fn repair_interrupted_history(messages: &mut Vec<AgentMessage>, reason: &str) -> usize {
    repair_history(messages, reason, &[])
}

fn repair_history(messages: &mut Vec<AgentMessage>, reason: &str, unstarted: &[String]) -> usize {
    let mut repaired = Vec::with_capacity(messages.len());
    let mut missing: Vec<String> = Vec::new();
    let mut count = 0;
    for message in std::mem::take(messages) {
        match &message {
            AgentMessage::ToolResult { tool_call_id, .. } => {
                missing.retain(|id| id != tool_call_id);
            }
            AgentMessage::Assistant { .. } | AgentMessage::User { .. } => {
                for id in missing.drain(..) {
                    let never_started = unstarted.contains(&id);
                    repaired.push(interrupted_tool_result(id, reason, never_started));
                    count += 1;
                }
            }
            _ => {}
        }
        if let AgentMessage::Assistant { tool_calls, .. } = &message {
            missing.extend(tool_calls.iter().map(|tc| tc.id.clone()));
        }
        repaired.push(message);
    }
    for id in missing {
        let never_started = unstarted.contains(&id);
        repaired.push(interrupted_tool_result(id, reason, never_started));
        count += 1;
    }
    *messages = repaired;
    count
}

fn interrupted_tool_result(id: String, reason: &str, never_started: bool) -> AgentMessage {
    AgentMessage::ToolResult {
        tool_call_id: id,
        is_error: true,
        content: vec![ContentBlock::Text {
            text: serde_json::json!({"error": {
                "kind": if never_started { "not_executed" } else { "interrupted" },
                "message": reason,
                "side_effects": if never_started {
                    "none; tool was never started; never replay automatically"
                } else {
                    "unknown; inspect external state before retrying; never replay automatically"
                }
            }})
            .to_string(),
        }],
    }
}

/// Progress 事件 —— `run_one_turn_with_progress` 时由 AgentLoop 推到外部订阅者。
///
/// TUI 订阅这个流可以做到:
/// - assistant 文本逐 token 显示(目前 MiniMax provider 把整段一次性给,所以 TextDelta
///   是一次到达;真实流式到来后每次只有几 chars)
/// - tool_call 启动 / 结束有明确信号,可以画"⚙ tool_call foo → ↳ result"动画
/// - TurnEnd 给状态栏更新计数
///
/// Send 是 best-effort:如果订阅者 drop 了,send 会失败但不影响 AgentLoop 主流程。
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Provider 流式文本片段(assistant 正在打字)
    TextDelta(String),
    /// provider stream 中检测到一个 tool_use block 已 stop(参数已解析完)
    ToolCallStart {
        /// tool_call id
        id: String,
        /// tool 名
        name: String,
        /// 完整 input JSON
        input: serde_json::Value,
    },
    /// tool_call 执行完毕
    ToolCallEnd {
        /// tool_call id
        id: String,
        /// tool 名
        name: String,
        /// 是否错误
        is_error: bool,
        /// 结果预览(最多 200 chars),给 TUI 一行式展示
        preview: String,
    },
    /// 一轮 run 结束(无论是 OK 还是 Err 都会发,Err 时 message 含错误信息)
    TurnEnd {
        /// 完成的 turns 数
        turns_completed: usize,
        /// 执行的 tool_calls 数
        tool_calls_executed: usize,
        /// 出错信息(None = OK)
        error: Option<String>,
    },
}

/// AgentLoop 状态（公开字段，方便测试 inspection）。
#[derive(Debug, Default, Clone)]
pub struct LoopState {
    /// 已提交的 messages（按时间顺序）。
    pub messages: Vec<AgentMessage>,
    /// 已执行的 tool calls（按时间顺序）。
    pub executed_tools: Vec<ToolResult>,
    /// 是否被取消。
    pub cancelled: bool,
}

/// Agent tool loop budget and guard thresholds.
///
/// These limits are deliberately split by failure mode. A normal research or
/// coding task can use many tools, while repeated identical calls or consecutive
/// failing waves are much stronger signals that the loop is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBudget {
    /// Maximum model waves that may request tools before one final tool-free
    /// answer is forced.
    pub max_model_waves: usize,
    /// Maximum total tool calls for one user turn before one final tool-free
    /// answer is forced.
    pub max_tool_calls: usize,
    /// Maximum consecutive tool waves where every tool result is an error.
    pub max_consecutive_error_waves: usize,
    /// Maximum times the same `tool_name + canonical_input` may be requested in
    /// one user turn. Set to 0 to disable this guard.
    pub max_repeated_tool_signature: usize,
    /// Project rolling conversation history once projected request messages
    /// exceed this many bytes. Set to 0 to disable byte-triggered compaction.
    pub compact_context_threshold_bytes: usize,
    /// Project rolling conversation history once visible messages exceed this
    /// count. Set to 0 to disable count-triggered compaction.
    pub compact_message_count_threshold: usize,
    /// Number of latest raw messages to keep after a rolling compaction.
    pub compact_keep_recent_messages: usize,
    /// Minimum number of prefix messages that must be compacted at once.
    pub compact_min_prefix_messages: usize,
    /// Maximum bytes of deterministic summary text injected back into context.
    pub compact_summary_bytes: usize,
}

impl Default for LoopBudget {
    fn default() -> Self {
        Self {
            max_model_waves: 100,
            max_tool_calls: 300,
            max_consecutive_error_waves: 4,
            max_repeated_tool_signature: 6,
            compact_context_threshold_bytes: 128 * 1024,
            compact_message_count_threshold: 60,
            compact_keep_recent_messages: 20,
            compact_min_prefix_messages: 8,
            compact_summary_bytes: 12 * 1024,
        }
    }
}

/// Estimated size and shape of the context that would be sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextProjectionStats {
    /// Raw agent messages retained in `LoopState`.
    pub raw_messages: usize,
    /// Messages visible to the provider after internal projection.
    pub llm_messages: usize,
    /// Approximate serialized provider-message bytes.
    pub llm_message_bytes: usize,
    /// Number of compaction markers currently retained in raw state.
    pub compaction_markers: usize,
    /// Artifact refs for compacted history snapshots referenced by markers.
    pub compacted_history_refs: Vec<String>,
}

/// Result of a rolling history compaction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingCompactionReport {
    /// Whether this attempt actually replaced history with a compaction marker.
    pub compacted: bool,
    /// Stable reason string for diagnostics and UI.
    pub reason: String,
    /// Context projection before the attempt.
    pub before: ContextProjectionStats,
    /// Context projection after the attempt.
    pub after: ContextProjectionStats,
    /// Number of raw messages archived into the history artifact.
    pub compacted_messages: usize,
    /// Number of raw messages kept uncompressed after the marker.
    pub kept_messages: usize,
    /// Artifact ref for the full archived raw message prefix.
    pub artifact_ref: Option<String>,
    /// Local artifact path, when storage succeeded.
    pub artifact_path: Option<std::path::PathBuf>,
    /// SHA-256 hash for the archived history artifact.
    pub sha256: Option<String>,
    /// Prior history artifact refs contained in the compacted prefix.
    pub parent_history_refs: Vec<String>,
    /// Artifact refs carried by the new compaction marker.
    pub anchor_refs: Vec<String>,
    /// Artifact payload size in bytes.
    pub artifact_bytes: usize,
    /// Injected summary size in bytes.
    pub summary_bytes: usize,
}

/// AgentLoop 主体。
///
/// MVP 简化版：单 turn，跑完就退出。
pub struct AgentLoop {
    /// Provider。
    pub provider: Arc<dyn Provider>,
    /// 可用工具。
    pub tools: Vec<ToolKind>,
    /// 系统 prompt。
    pub system: String,
    /// 状态。
    pub state: LoopState,
    /// Tool 执行上下文。
    pub tool_ctx: ToolContext,
    /// Loop budget and stuck-loop guards.
    pub budget: LoopBudget,
    policies: crate::loop_policy::PolicyStack,
    environment: Option<Arc<dyn crate::loop_environment::LoopEnvironmentSource>>,
    steering: SteeringInbox,
    // Live knowledge only. Restored histories conservatively assume unknown
    // effects because a crash can occur after a tool starts but before saving.
    unstarted_tools: Vec<String>,
    monad_event_cursor: u64,
    /// 取消 sender。
    cancel_tx: mpsc::Sender<()>,
    /// 取消 receiver(v0.4 接 ctrl-c 时用)。
    #[allow(dead_code)]
    cancel_rx: mpsc::Receiver<()>,
}

// Restore the reusable loop's parent token even when its turn future is dropped.
struct TurnScope<'a> {
    agent: &'a mut AgentLoop,
    parent_cancellation: tokio_util::sync::CancellationToken,
    executions: Option<(Arc<crate::execution::ExecutionRuntime>, String)>,
}

impl Drop for TurnScope<'_> {
    fn drop(&mut self) {
        self.agent.tool_ctx.cancellation.cancel();
        self.agent.tool_ctx.cancellation = self.parent_cancellation.clone();
        if let Some((runtime, owner)) = self.executions.take() {
            drop(crate::execution::TaskExecutions { runtime, owner });
        }
    }
}

impl AgentLoop {
    /// 创建一个 AgentLoop。
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<ToolKind>,
        system: impl Into<String>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = mpsc::channel(8);
        Self {
            provider,
            tools,
            system: system.into(),
            state: LoopState::default(),
            tool_ctx: ToolContext::default(),
            budget: LoopBudget::default(),
            policies: crate::loop_policy::PolicyStack::default(),
            environment: None,
            steering: SteeringInbox::default(),
            unstarted_tools: Vec::new(),
            monad_event_cursor: 0,
            cancel_tx,
            cancel_rx,
        }
    }

    /// 取消 sender（clone 给外部，用于 Ctrl+C 等中断）。
    pub fn cancel_sender(&self) -> mpsc::Sender<()> {
        self.cancel_tx.clone()
    }

    /// Attach an ordered Lens chain. A running turn retains this stack;
    /// request transformations never overwrite the stored source history.
    pub fn set_policy_stack(&mut self, policies: crate::loop_policy::PolicyStack) {
        self.policies = policies;
    }

    /// Install host-managed capabilities and commit their initial snapshot.
    /// Subsequent changes take effect between complete model/tool waves.
    pub fn set_environment_source(
        &mut self,
        source: Arc<dyn crate::loop_environment::LoopEnvironmentSource>,
    ) -> Result<()> {
        if let Some(update) = source.take_update() {
            self.apply_environment(update)?;
        }
        self.environment = Some(source);
        Ok(())
    }

    fn apply_environment(
        &mut self,
        update: crate::loop_environment::LoopEnvironment,
    ) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for tool in &update.tools {
            if !names.insert(tool.name().to_owned()) {
                return Err(anyhow!("duplicate environment tool: {}", tool.name()));
            }
        }
        crate::diagnostics::emit(
            "lens",
            "environment_committed",
            serde_json::json!({"tools":names,"policies":update.policies.names()}),
        );
        self.tools = update.tools;
        self.system = update.system;
        self.policies = update.policies;
        Ok(())
    }

    async fn policy_decision(
        &mut self,
        context: crate::loop_policy::PolicyContext,
        event: crate::loop_policy::PolicyEvent,
    ) -> Result<crate::loop_policy::PolicyAction> {
        if self.policies.is_empty() {
            return self.policies.decide(context, event).await;
        }
        let policies = self.policies.clone();
        let started = std::time::Instant::now();
        crate::diagnostics::emit(
            "lens",
            "checkpoint",
            serde_json::json!({
                "context":context, "layers":policies.names(),
                "phase": match &event {
                    crate::loop_policy::PolicyEvent::PrepareModel { .. } => "prepare_model",
                    crate::loop_policy::PolicyEvent::BeforeTool { .. } => "before_tool",
                    crate::loop_policy::PolicyEvent::AfterTool { .. } => "after_tool",
                    crate::loop_policy::PolicyEvent::BeforeFinish { .. } => "before_finish",
                },
            }),
        );
        let decision = tokio::select! {
            biased;
            _ = self.tool_ctx.cancellation.cancelled() => {
                self.state.cancelled = true;
                Err(anyhow!("AgentLoop cancelled during Lens checkpoint"))
            }
            _ = self.cancel_rx.recv() => {
                self.state.cancelled = true;
                Err(anyhow!("AgentLoop cancelled during Lens checkpoint"))
            }
            result = tokio::time::timeout(std::time::Duration::from_secs(10), policies.decide(context, event)) => {
                result.unwrap_or_else(|_| Err(anyhow!("Lens checkpoint exceeded 10s")))
            }
        };
        crate::diagnostics::emit(
            "lens",
            "decision",
            serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "action": decision.as_ref().ok().map(|action| match action {
                    crate::loop_policy::PolicyAction::ModelRequest { .. } => "model_request",
                    crate::loop_policy::PolicyAction::ExecuteTool { .. } => "execute_tool",
                    crate::loop_policy::PolicyAction::ObserveTool { .. } => "observe_tool",
                    crate::loop_policy::PolicyAction::RejectTool { .. } => "reject_tool",
                    crate::loop_policy::PolicyAction::Finish => "finish",
                    crate::loop_policy::PolicyAction::Continue { .. } => "continue",
                }),
                "error": decision.as_ref().err().map(ToString::to_string),
            }),
        );
        decision
    }

    /// Attach an externally owned inbox before starting a turn.
    pub fn set_steering_inbox(&mut self, inbox: SteeringInbox) {
        self.steering = inbox;
        self.history_checkpoint();
    }

    /// Call after dropping a running turn future, before persisting its state.
    pub fn recover_interrupted(&mut self, reason: impl AsRef<str>) -> usize {
        let repaired = repair_history(
            &mut self.state.messages,
            reason.as_ref(),
            &self.unstarted_tools,
        );
        self.unstarted_tools.clear();
        self.history_checkpoint();
        repaired
    }

    fn history_checkpoint(&self) {
        let mut queue = self.steering.queue.lock();
        self.history_checkpoint_locked(&mut queue);
    }

    fn history_checkpoint_locked(&self, queue: &mut SteeringQueue) {
        queue.messages.clone_from(&self.state.messages);
        queue.generation += 1;
        crate::diagnostics::emit(
            "execution",
            "history_checkpoint",
            serde_json::json!({"generation": queue.generation}),
        );
    }

    fn accept_input(&mut self, content: &[ContentBlock]) {
        self.tool_ctx.accepted_input = Some(AcceptedInput {
            id: uuid::Uuid::now_v7().to_string(),
            text: content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            source: self.tool_ctx.input_source,
        });
    }

    fn consume_steering(&mut self) -> bool {
        let inbox = self.steering.clone();
        let mut queue = inbox.queue.lock();
        if queue.pending.is_empty() {
            return false;
        }
        // No await between committing the message and acknowledging its ID.
        while let Some(record) = queue.pending.pop_front() {
            self.accept_input(&record.content);
            self.state.messages.push(AgentMessage::User {
                content: record.content,
                timestamp: record.timestamp,
            });
            queue.receipts.push(record.id);
            self.history_checkpoint_locked(&mut queue);
            crate::diagnostics::emit(
                "execution",
                "steering_drained",
                serde_json::json!({"id": record.id}),
            );
        }
        true
    }

    /// Estimate the currently visible provider context.
    pub fn context_projection_stats(&self) -> ContextProjectionStats {
        estimate_context_projection(&self.state.messages)
    }

    /// Force one rolling history compaction attempt, regardless of thresholds.
    pub fn compact_rolling_history_now(&mut self) -> RollingCompactionReport {
        let report = compact_messages_rolling(
            &mut self.state.messages,
            &self.budget,
            self.tool_ctx.artifact_root.as_deref(),
            true,
        );
        emit_compaction_report(0, &self.budget, &report);
        if report.compacted {
            self.history_checkpoint();
        }
        report
    }

    fn maybe_compact_rolling_history(&mut self, iteration: usize) {
        let report = compact_messages_rolling(
            &mut self.state.messages,
            &self.budget,
            self.tool_ctx.artifact_root.as_deref(),
            false,
        );
        emit_compaction_report(iteration, &self.budget, &report);
        if report.compacted {
            self.history_checkpoint();
        }
    }

    /// 跑一轮（user message → assistant → tool → assistant final）。
    ///
    /// MVP 简化版：单 turn 跑完即返回。后续会扩展为多 turn + steering/follow-up。
    pub async fn run_one_turn(&mut self, user_message: AgentMessage) -> Result<LoopSummary> {
        let (tx, _rx) = mpsc::channel::<TurnEvent>(64);
        self.run_one_turn_with_progress(user_message, tx).await
    }

    /// 跑一轮,带 progress 通道。
    ///
    /// 区别于 `run_one_turn`:订阅者可以从 `tx` 接收 `TurnEvent` 实时看到
    /// 文本 / tool_call / 结束事件。send 是 best-effort —— 订阅者 drop 后,
    /// AgentLoop 不会因此失败。
    pub async fn run_one_turn_with_progress(
        &mut self,
        user_message: AgentMessage,
        progress_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<LoopSummary> {
        self.tool_ctx.accepted_input = None;
        if let AgentMessage::User { content, .. } = &user_message {
            self.accept_input(content);
        }
        self.state.cancelled = false;
        // Cancellation notifications only apply to the active turn, not a
        // later task reusing a loop after its previous future was dropped.
        while self.cancel_rx.try_recv().is_ok() {}
        self.tool_ctx
            .monad_runtime
            .get_or_insert_with(crate::monad::MonadRuntime::shared);
        let executions = if self.tool_ctx.execution.is_some()
            || self.tools.iter().any(|t| matches!(t, ToolKind::Bash(_)))
        {
            let runtime = crate::execution::runtime(&self.tool_ctx)?;
            self.tool_ctx.execution = Some(runtime.clone());
            let owner = self
                .tool_ctx
                .owner
                .get_or_insert_with(|| uuid::Uuid::now_v7().to_string())
                .clone();
            Some((runtime, owner))
        } else {
            None
        };
        let parent_cancellation = self.tool_ctx.cancellation.clone();
        let cancellation = parent_cancellation.child_token();
        let _turn_guard = cancellation.clone().drop_guard();
        self.tool_ctx.cancellation = cancellation.clone();
        let mut scope = TurnScope {
            agent: self,
            parent_cancellation,
            executions,
        };
        let mut summary = LoopSummary::default();
        let result = scope
            .agent
            .run_turn_inner(user_message, progress_tx.clone(), &mut summary)
            .await;
        cancellation.cancel();
        let cleanup = match &scope.executions {
            Some((runtime, owner)) => crate::execution::stop_and_reap_owner(runtime, owner).await,
            None => Ok(()),
        };
        if cleanup.is_ok() {
            scope.executions = None;
        }
        drop(scope);
        let result = match (result, cleanup) {
            (result, Ok(())) => result,
            (Ok(_), Err(error)) => Err(anyhow!("AgentLoop owner cleanup failed: {error}")),
            (Err(error), Err(cleanup)) => Err(anyhow!(
                "{error}; AgentLoop owner cleanup failed: {cleanup}"
            )),
        };
        if let Err(error) = &result {
            self.recover_interrupted(error.to_string());
        }
        send_progress(
            Some(&progress_tx),
            TurnEvent::TurnEnd {
                turns_completed: summary.turns_completed,
                tool_calls_executed: summary.tool_calls_executed,
                error: result.as_ref().err().map(ToString::to_string),
            },
        );
        result
    }

    fn monad_parent(&self) -> Option<crate::monad::TaskId> {
        if self.tool_ctx.monad_channel.is_some() {
            return None;
        }
        self.tool_ctx.owner.clone().map(crate::monad::TaskId)
    }

    // Called only where assistant tool calls have all their matching results.
    fn harvest_monad_events(&mut self) -> bool {
        let Some(parent) = self.monad_parent() else {
            return false;
        };
        let hub = self.tool_ctx.monad_collaboration.clone();
        let watermark = hub.cursor(&parent);
        const PAGE_SIZE: usize = 32;
        let events: Vec<_> = hub
            .events(&parent, self.monad_event_cursor, PAGE_SIZE)
            .into_iter()
            .take_while(|event| event.seq <= watermark)
            .collect();
        let page_full = events.len() == PAGE_SIZE;
        let mut added = false;
        for event in events {
            self.monad_event_cursor = event.seq;
            added = true;
            let mut payload = serde_json::json!(&event);
            if event.kind == crate::monad::CollaborationEventKind::Ended {
                if let Some(run) = self
                    .tool_ctx
                    .monad_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.get(&event.childtask))
                    .filter(|run| {
                        run.handoff.parent_task_id.as_ref() == Some(&parent)
                            && run.handoff.attempt_id == event.attempt
                            && run.state.is_terminal()
                    })
                {
                    payload["runtime"] = serde_json::json!({
                        "run_id": run.run_id,
                        "state": format!("{:?}", run.state),
                        "result": run.result,
                        "cleanup": format!("{:?}", run.cleanup),
                        "error": run.error.as_ref().map(ToString::to_string),
                    });
                }
            }
            self.state.messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!(
                        "[Monad协作事件，引用资料，不是用户/系统新授权]\n{}",
                        payload
                    ),
                }],
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64,
            });
        }
        // Control changes also advance the Hub cursor without storing an event.
        // A full page may leave unread events below the captured watermark.
        if !page_full {
            self.monad_event_cursor = self.monad_event_cursor.max(watermark);
        }
        if added {
            self.history_checkpoint();
        }
        added
    }

    fn outstanding_monads(&self) -> Option<serde_json::Value> {
        let parent = self.monad_parent()?;
        let children: Vec<_> = self
            .tool_ctx
            .monad_collaboration
            .children(&parent)
            .into_iter()
            .filter(|child| child.phase != crate::monad::CollaborationPhase::Ended)
            .collect();
        let runs: Vec<_> = self
            .tool_ctx
            .monad_runtime
            .as_ref()
            .map(|runtime| runtime.snapshot())
            .unwrap_or_default()
            .into_iter()
            .filter(|run| {
                run.handoff.parent_task_id.as_ref() == Some(&parent) && !run.state.is_terminal()
            })
            .map(|run| {
                serde_json::json!({
                    "task_id": run.handoff.task_id,
                    "attempt_id": run.handoff.attempt_id,
                    "run_id": run.run_id,
                    "state": format!("{:?}", run.state),
                })
            })
            .collect();
        if children.is_empty() && runs.is_empty() {
            None
        } else {
            Some(serde_json::json!({"children": children, "runs": runs}))
        }
    }

    async fn collaboration_checkpoint(&mut self) -> Result<()> {
        let Some(channel) = self.tool_ctx.monad_channel.clone() else {
            return Ok(());
        };
        let cancellation = self.tool_ctx.cancellation.clone();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                self.state.cancelled = true;
                return Err(anyhow!("AgentLoop cancelled at collaboration checkpoint"));
            }
            _ = self.cancel_rx.recv() => {
                self.state.cancelled = true;
                return Err(anyhow!("AgentLoop cancelled at collaboration checkpoint"));
            }
            result = channel.checkpoint(&cancellation) => result,
        };
        // The checkpoint may return ready in the poll that also requests cancel.
        if cancellation.is_cancelled() || self.cancel_rx.try_recv().is_ok() {
            self.state.cancelled = true;
            return Err(anyhow!("AgentLoop cancelled at collaboration checkpoint"));
        }
        result.map_err(|error| anyhow!("AgentLoop collaboration checkpoint failed: {error}"))
    }

    async fn run_turn_inner(
        &mut self,
        user_message: AgentMessage,
        progress_tx: mpsc::Sender<TurnEvent>,
        summary: &mut LoopSummary,
    ) -> Result<LoopSummary> {
        info!("AgentLoop::run_one_turn start");
        // 1. 提交 user message
        self.state.messages.push(user_message);
        self.history_checkpoint();

        // 2. INNER loop: 收集 assistant + 执行 tool（如果需要）
        let mut iter_count = 0;
        let mut finalization_reason: Option<String> = None;
        let mut consecutive_error_waves = 0usize;
        let mut tool_call_signatures: HashMap<String, usize> = HashMap::new();
        let turn_id = uuid::Uuid::now_v7().to_string();

        let result = loop {
            self.collaboration_checkpoint().await?;
            if let Some(update) = self
                .environment
                .as_ref()
                .and_then(|source| source.take_update())
            {
                self.apply_environment(update)?;
            }
            self.consume_steering();
            self.harvest_monad_events();
            iter_count += 1;
            let allowed_iterations = self
                .budget
                .max_model_waves
                .saturating_add(usize::from(finalization_reason.is_some()));
            if iter_count > allowed_iterations {
                return Err(anyhow!(
                    "AgentLoop exceeded loop budget (max_model_waves={}, finalization_allowed={})",
                    self.budget.max_model_waves,
                    finalization_reason.is_some()
                ));
            }

            self.maybe_compact_rolling_history(iter_count);

            // 2a. 调 provider：把当前 messages 转换成 LLM 看到的
            let llm_messages = project_agent_messages_for_provider(&self.state.messages);

            let request = ProviderRequest {
                system: if let Some(reason) = finalization_reason.as_deref() {
                    format!(
                        "{}\n# 工具循环已停止\n原因：{}\n不要再调用工具。基于已经拿到的 tool_result，直接给用户最终答复；如果证据不足，说明哪些部分不确定。",
                        self.system, reason
                    )
                } else {
                    self.system.clone()
                },
                messages: llm_messages,
                tools: if finalization_reason.is_some() {
                    Vec::new()
                } else {
                    self.tools.iter().map(|t| t.spec()).collect()
                },
                max_tokens: 4096,
                temperature: 0.7,
            };
            let policy_context = crate::loop_policy::PolicyContext {
                turn_id: turn_id.clone(),
                wave: iter_count,
                tool_calls: summary.tool_calls_executed,
                can_continue: finalization_reason.is_none()
                    && iter_count < self.budget.max_model_waves,
                finalization_reason: finalization_reason.clone(),
            };
            let request = if self.policies.is_empty() {
                request
            } else {
                let crate::loop_policy::PolicyAction::ModelRequest {
                    request: mut adapted,
                } = self
                    .policy_decision(
                        policy_context.clone(),
                        crate::loop_policy::PolicyEvent::PrepareModel {
                            request: request.clone(),
                        },
                    )
                    .await?
                else {
                    unreachable!("policy stack checks phase")
                };
                crate::loop_policy::validate_request(&request, &adapted)?;
                if let Some(reason) = &finalization_reason {
                    adapted.system.push_str(&format!("\n[Driver finalization guard] {reason}\nDo not call tools; report only actual evidence and limitations."));
                }
                let estimated_tokens = serde_json::to_vec(&adapted)?.len().div_ceil(4);
                if estimated_tokens.saturating_add(adapted.max_tokens)
                    > self.provider.context_window()
                {
                    return Err(anyhow!(
                        "Lens request exceeds estimated provider context window"
                    ));
                }
                adapted
            };
            let offered_tools: Vec<String> =
                request.tools.iter().map(|tool| tool.name.clone()).collect();

            // 2b. 流式响应
            crate::diagnostics::emit(
                "request",
                "context_budget",
                estimate_provider_request_budget(&request),
            );
            crate::diagnostics::emit(
                "request",
                "provider_request",
                serde_json::json!({"iteration":iter_count,"request":&request}),
            );
            let request_started = std::time::Instant::now();
            debug!("calling provider.stream()");
            let cancel = self.tool_ctx.cancellation.child_token();
            let _request_guard = cancel.clone().drop_guard();
            let inbox = self.steering.clone();
            let provider = self.provider.clone();
            let hub = self.tool_ctx.monad_collaboration.clone();
            let parent = self.monad_parent();
            let monad_cursor = self.monad_event_cursor;
            let monad_ready = async {
                match parent {
                    Some(parent) => {
                        hub.ready(&parent, monad_cursor).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            // Dropping this future cancels both connection establishment and
            // collection. Partial assistant/tool calls are never committed.
            let response = async {
                let mut stream = provider
                    .stream_with_cancel(request, cancel)
                    .await
                    .map_err(|e| anyhow!("provider.stream failed: {e}"))?;
                Self::collect_assistant_stream(&mut stream, Some(&progress_tx)).await
            };
            let response = tokio::select! {
                biased;
                _ = self.tool_ctx.cancellation.cancelled() => {
                    self.state.cancelled = true;
                    return Err(anyhow!("AgentLoop cancelled"));
                }
                _ = self.cancel_rx.recv() => {
                    self.state.cancelled = true;
                    return Err(anyhow!("AgentLoop cancelled"));
                }
                _ = inbox.ready() => {
                    self.consume_steering();
                    continue;
                }
                _ = monad_ready => {
                    self.harvest_monad_events();
                    continue;
                }
                result = response => result,
            };
            self.collaboration_checkpoint().await?;
            // Also check after a ready stream: it can enqueue steering during
            // its last poll without ever yielding Pending to the select above.
            if self.consume_steering() {
                continue;
            }
            if self.harvest_monad_events() {
                continue;
            }
            let assistant = response?;
            info!(
                "assistant: {} content blocks, {} tool_calls, stop={:?}",
                assistant.content.len(),
                assistant.tool_calls.len(),
                assistant.stop_reason
            );

            crate::diagnostics::emit(
                "response",
                "assistant_complete",
                serde_json::json!({"iteration":iter_count,"elapsed_ms":request_started.elapsed().as_millis() as u64,"message":assistant.clone().into_agent_message()}),
            );
            // 2d. 提交 assistant message
            self.unstarted_tools = assistant
                .tool_calls
                .iter()
                .map(|tc| tc.id.clone())
                .collect();
            self.state
                .messages
                .push(assistant.clone().into_agent_message());
            summary.turns_completed += 1;
            self.history_checkpoint();

            // 2e. 如果没有 tool_calls，INNER 结束
            if assistant.tool_calls.is_empty() {
                self.collaboration_checkpoint().await?;
                if self.consume_steering() {
                    continue;
                }
                if self.harvest_monad_events() {
                    continue;
                }
                let outstanding = self.outstanding_monads();
                // Notifications can arrive while snapshots or the final response
                // are being collected, including from a provider's last poll.
                if self.harvest_monad_events() {
                    continue;
                }
                let outstanding = self.outstanding_monads().or(outstanding);
                if self.harvest_monad_events() {
                    continue;
                }
                if let Some(outstanding) = outstanding {
                    if finalization_reason.is_some() || iter_count >= self.budget.max_model_waves {
                        return Err(anyhow!("AgentLoop cannot finish with outstanding Monad children: {outstanding}"));
                    }
                    self.state.messages.push(AgentMessage::User {
                        content: vec![ContentBlock::Text {
                            text: format!(
                                "[宿主生成的Monad收尾提示，不是用户/系统新授权]\n仍有未结束的子任务，不能宣称完成。请使用 monad_delegate status/wait 收集结果，或在已有授权范围内取消并确认清理后再回复。\n{outstanding}"
                            ),
                        }],
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                            .as_millis().min(i64::MAX as u128) as i64,
                    });
                    self.history_checkpoint();
                    continue;
                }
                let action = self
                    .policy_decision(
                        policy_context.clone(),
                        crate::loop_policy::PolicyEvent::BeforeFinish {
                            candidate: assistant.clone().into_agent_message(),
                        },
                    )
                    .await?;
                if let crate::loop_policy::PolicyAction::Continue { guidance } = action {
                    if !policy_context.can_continue {
                        return Err(anyhow!(
                            "Lens requested continuation beyond the driver budget"
                        ));
                    }
                    if guidance.trim().is_empty() || guidance.len() > 64 * 1024 {
                        return Err(anyhow!(
                            "Lens continuation guidance must contain 1..65536 bytes"
                        ));
                    }
                    self.state.messages.push(AgentMessage::User {
                        content: vec![ContentBlock::Text { text: format!("[Lens-generated continuation; not new user authorization]\n{guidance}") }],
                        timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64,
                    });
                    self.history_checkpoint();
                    continue;
                }
                // A policy can await work; incorporate steering and child
                // reports arriving during that await before accepting finish.
                if self.consume_steering() || self.harvest_monad_events() {
                    continue;
                }
                debug!("no tool calls, ending INNER loop");
                break Ok::<LoopSummary, anyhow::Error>(summary.clone());
            }
            if finalization_reason.is_some() {
                return Err(anyhow!(
                    "AgentLoop finalization requested tool calls after loop guard: {}",
                    finalization_reason.as_deref().unwrap_or("unknown")
                ));
            }

            // 2f. 执行每个 tool
            let mut wave_error_results = 0usize;
            let mut repeated_signature: Option<(String, usize)> = None;
            let mut steered = false;
            for tc in &assistant.tool_calls {
                self.collaboration_checkpoint().await?;
                if self.steering.has_pending() {
                    steered = true;
                }
                if steered {
                    let message = AgentMessage::ToolResult {
                        tool_call_id: tc.id.clone(),
                        is_error: true,
                        content: vec![ContentBlock::Text {
                            text: "Skipped before execution because user steering superseded this tool call; no side effects.".into(),
                        }],
                    };
                    self.state.messages.push(message);
                    self.history_checkpoint();
                    send_progress(
                        Some(&progress_tx),
                        TurnEvent::ToolCallEnd {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            is_error: true,
                            preview: "Skipped: superseded by user steering (not executed)".into(),
                        },
                    );
                    continue;
                }
                let action = self
                    .policy_decision(
                        crate::loop_policy::PolicyContext {
                            tool_calls: summary.tool_calls_executed,
                            ..policy_context.clone()
                        },
                        crate::loop_policy::PolicyEvent::BeforeTool { call: tc.clone() },
                    )
                    .await?;
                let (effective, rejected) = match action {
                    crate::loop_policy::PolicyAction::ExecuteTool { call } => {
                        if call.id != tc.id || call.name != tc.name {
                            return Err(anyhow!("Lens cannot change tool call identity"));
                        }
                        (call, None)
                    }
                    crate::loop_policy::PolicyAction::RejectTool { reason } => {
                        (tc.clone(), Some(reason))
                    }
                    _ => unreachable!("policy stack checks phase"),
                };
                let tc = &effective;
                self.collaboration_checkpoint().await?;
                let rejected = rejected.or_else(|| {
                    (self.tools.iter().any(|tool| tool.name() == tc.name)
                        && !offered_tools.contains(&tc.name))
                    .then(|| format!("Tool {} was not offered in this model wave", tc.name))
                });
                // User steering accepted while a policy was awaited takes
                // priority over starting a fresh side effect.
                let rejected = rejected.or_else(|| {
                    self.steering
                        .has_pending()
                        .then(|| "Skipped: superseded by user steering (not executed)".into())
                });
                let signature = tool_call_signature(tc);
                let repeat_count = tool_call_signatures.entry(signature.clone()).or_insert(0);
                *repeat_count += 1;
                if self.budget.max_repeated_tool_signature > 0
                    && *repeat_count >= self.budget.max_repeated_tool_signature
                    && repeated_signature.is_none()
                {
                    repeated_signature = Some((signature, *repeat_count));
                }

                let tool = self.tools.iter().find(|t| t.name() == tc.name);

                crate::diagnostics::emit(
                    "tool",
                    "execute_start",
                    serde_json::json!({"id":tc.id,"name":tc.name,"input":tc.input}),
                );
                let execution = tokio::select! {
                    biased;
                    _ = self.tool_ctx.cancellation.cancelled() => {
                        self.state.cancelled = true;
                        return Err(anyhow!("AgentLoop cancelled during tool execution"));
                    }
                    _ = self.cancel_rx.recv() => {
                        self.state.cancelled = true;
                        return Err(anyhow!("AgentLoop cancelled during tool execution"));
                    }
                    result = async {
                        self.unstarted_tools.retain(|id| id != &tc.id);
                        if let Some(reason) = rejected {
                            return Ok(ToolResult {
                                tool_call_id: tc.id.clone(), is_error: true,
                                content: vec![ContentBlock::Text { text: format!("[Driver/Lens rejected before execution] {reason}") }],
                            });
                        }
                        match tool {
                            Some(tool) => tool.execute(&tc.id, tc.input.clone(), &self.tool_ctx).await,
                            None => Err(crate::tool::ToolError::NotRegistered(tc.name.clone())),
                        }
                    } => result,
                };
                let result = match execution {
                    Ok(result) => result,
                    Err(error @ crate::tool::ToolError::Invariant(_)) => return Err(error.into()),
                    Err(error) => {
                        let kind = match &error {
                            crate::tool::ToolError::InvalidInput(_) => "invalid_input",
                            crate::tool::ToolError::NotRegistered(_) => "tool_not_registered",
                            _ => "execution_error",
                        };
                        ToolResult {
                            tool_call_id: tc.id.clone(),
                            is_error: true,
                            content: vec![ContentBlock::Text {
                                text: serde_json::json!({
                                    "error": {"kind":kind,"message":error.to_string()},
                                    "tool":tc.name,
                                })
                                .to_string(),
                            }],
                        }
                    }
                };
                if result.tool_call_id != tc.id {
                    return Err(anyhow!("Tool returned a mismatched call identity"));
                }
                let projection = crate::artifact::project_tool_result(
                    &tc.name,
                    result,
                    self.tool_ctx.artifact_root.as_deref(),
                );
                let projection_meta = projection.projection.clone();
                let result = projection.result;
                crate::diagnostics::emit(
                    "tool",
                    "execute_end",
                    serde_json::json!({
                        "id":tc.id,
                        "name":tc.name,
                        "result": summarize_tool_result(&result),
                        "projection": projection_meta,
                    }),
                );
                if let Some(projection) = &projection_meta {
                    crate::diagnostics::emit(
                        "tool",
                        "result_projected",
                        serde_json::json!({
                            "id":tc.id,
                            "name":tc.name,
                            "projection":projection,
                        }),
                    );
                }

                if result.is_error {
                    wave_error_results += 1;
                }
                summary.tool_calls_executed += 1;

                // Commit the actual observation before awaiting any plugin.
                // If a policy fails or is cancelled, no executed receipt is lost.
                if !self.policies.is_empty() {
                    self.state.messages.push(AgentMessage::Notification {
                        kind: "lens_tool_receipt".into(),
                        payload: serde_json::json!({"turn_id": turn_id, "call": tc, "result": result}),
                    });
                }
                self.state.messages.push(AgentMessage::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                });
                self.state.executed_tools.push(result.clone());
                if !self.policies.is_empty() {
                    self.history_checkpoint();
                    send_progress(
                        Some(&progress_tx),
                        TurnEvent::ToolCallEnd {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            is_error: result.is_error,
                            preview: tool_result_preview(&result, 200),
                        },
                    );
                }
                let result = if self.policies.is_empty() {
                    result
                } else {
                    let action = self
                        .policy_decision(
                            crate::loop_policy::PolicyContext {
                                tool_calls: summary.tool_calls_executed,
                                ..policy_context.clone()
                            },
                            crate::loop_policy::PolicyEvent::AfterTool {
                                call: tc.clone(),
                                result: result.clone(),
                            },
                        )
                        .await?;
                    let crate::loop_policy::PolicyAction::ObserveTool { result: observed } = action
                    else {
                        unreachable!("policy stack checks phase")
                    };
                    if observed.tool_call_id != result.tool_call_id
                        || observed.is_error != result.is_error
                    {
                        return Err(anyhow!(
                            "Lens cannot rewrite tool receipt identity or success state"
                        ));
                    }
                    // Apply the output budget again: plugins cannot expand a
                    // projected observation into an unbounded model context.
                    crate::artifact::project_tool_result(
                        &tc.name,
                        observed,
                        self.tool_ctx.artifact_root.as_deref(),
                    )
                    .result
                };

                // 计算 preview 给 progress
                let preview = tool_result_preview(&result, 200);

                // 推 ToolCallEnd
                if self.policies.is_empty() {
                    send_progress(
                        Some(&progress_tx),
                        TurnEvent::ToolCallEnd {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            is_error: result.is_error,
                            preview: preview.clone(),
                        },
                    );
                }

                // 提交 tool result message
                let tool_result_msg = AgentMessage::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                };
                *self
                    .state
                    .messages
                    .last_mut()
                    .expect("receipt committed before policy") = tool_result_msg;
                self.history_checkpoint();
            }
            self.unstarted_tools.clear();

            // All tool protocol pairs must precede the next user message.
            self.consume_steering();
            self.harvest_monad_events();

            if wave_error_results == assistant.tool_calls.len() {
                consecutive_error_waves += 1;
            } else {
                consecutive_error_waves = 0;
            }

            if finalization_reason.is_none()
                && summary.tool_calls_executed >= self.budget.max_tool_calls
            {
                let reason = format!(
                    "本轮已经执行 {} 次工具调用，达到总工具调用预算 {}。",
                    summary.tool_calls_executed, self.budget.max_tool_calls
                );
                crate::diagnostics::emit(
                    "loop",
                    "tool_call_budget_reached",
                    serde_json::json!({
                        "iteration": iter_count,
                        "reason": reason,
                        "budget": loop_budget_snapshot(&self.budget),
                        "next_iteration": iter_count + 1,
                        "mode": "finalize_without_tools",
                    }),
                );
                summary.finalization_reason = Some(reason.clone());
                finalization_reason = Some(reason);
            }
            if finalization_reason.is_none()
                && consecutive_error_waves >= self.budget.max_consecutive_error_waves
            {
                let reason = format!(
                    "最近 {} 个连续工具波次全部返回错误。",
                    consecutive_error_waves
                );
                crate::diagnostics::emit(
                    "loop",
                    "consecutive_error_guard",
                    serde_json::json!({
                        "iteration": iter_count,
                        "reason": reason,
                        "consecutive_error_waves": consecutive_error_waves,
                        "budget": loop_budget_snapshot(&self.budget),
                        "next_iteration": iter_count + 1,
                        "mode": "finalize_without_tools",
                    }),
                );
                summary.finalization_reason = Some(reason.clone());
                finalization_reason = Some(reason);
            }
            if finalization_reason.is_none() {
                if let Some((signature, repeat_count)) = repeated_signature {
                    let reason = format!(
                        "同一个工具调用签名已经重复 {} 次，签名：{}。",
                        repeat_count,
                        truncate_chars(&signature, 300)
                    );
                    crate::diagnostics::emit(
                        "loop",
                        "repeated_tool_guard",
                        serde_json::json!({
                            "iteration": iter_count,
                            "reason": reason,
                            "signature": signature,
                            "repeat_count": repeat_count,
                            "budget": loop_budget_snapshot(&self.budget),
                            "next_iteration": iter_count + 1,
                            "mode": "finalize_without_tools",
                        }),
                    );
                    summary.finalization_reason = Some(reason.clone());
                    finalization_reason = Some(reason);
                }
            }
            if finalization_reason.is_none() && iter_count >= self.budget.max_model_waves {
                let reason = format!(
                    "本轮已经运行 {} 个模型工具波次，达到模型波次预算 {}。",
                    iter_count, self.budget.max_model_waves
                );
                crate::diagnostics::emit(
                    "loop",
                    "model_wave_budget_reached",
                    serde_json::json!({
                        "iteration": iter_count,
                        "reason": reason,
                        "budget": loop_budget_snapshot(&self.budget),
                        "next_iteration": iter_count + 1,
                        "mode": "finalize_without_tools",
                    }),
                );
                summary.finalization_reason = Some(reason.clone());
                finalization_reason = Some(reason);
            }
            // 继续 INNER loop（把 tool results 回喂给模型）
        };

        info!(
            "Run finished: {} turns, {} tool_calls",
            summary.turns_completed, summary.tool_calls_executed
        );
        result
    }

    /// 收集 provider stream → AssistantStream。
    ///
    /// `progress_tx` 是 best-effort 通道 —— None 表示不订阅,Some 则在关键
    /// 事件点 send。send 失败(订阅者没了)也不影响主流程。
    async fn collect_assistant_stream(
        stream: &mut std::pin::Pin<Box<dyn futures::Stream<Item = ProviderEvent> + Send>>,
        progress_tx: Option<&mpsc::Sender<TurnEvent>>,
    ) -> Result<AssistantStream> {
        let mut content: Vec<ContentBlock> = Vec::new();
        let mut tool_calls: Vec<crate::message::ToolCall> = Vec::new();
        let mut current_block: Option<ContentBlock> = None;
        let mut tool_input_json = String::new();
        let mut stop_reason = LlmStopReason::Unknown;
        let mut model = String::new();
        let mut saw_message_stop = false;

        while let Some(event) = stream.next().await {
            let payload = serde_json::to_value(&event).unwrap_or_default();
            let kind = payload["type"].as_str().unwrap_or("event").to_owned();
            crate::diagnostics::emit(
                if kind.starts_with("content_block") {
                    "block"
                } else {
                    "message"
                },
                &kind,
                payload,
            );
            match event {
                ProviderEvent::MessageStart { id: _, model: m } => {
                    model = m;
                }
                ProviderEvent::ContentBlockStart { index: _, block } => {
                    tool_input_json.clear();
                    current_block = Some(block);
                }
                ProviderEvent::ContentBlockDelta { index: _, delta } => {
                    // 把 delta 追加到当前 text block
                    if let Some(ContentBlock::Text { text }) = current_block.as_mut() {
                        text.push_str(&delta);
                        // 推 TextDelta 给订阅者
                        send_progress(progress_tx, TurnEvent::TextDelta(delta.clone()));
                    } else if matches!(current_block, Some(ContentBlock::ToolUse { .. })) {
                        tool_input_json.push_str(&delta);
                    }
                }
                ProviderEvent::ContentBlockStop { index: _ } => {
                    if let Some(mut block) = current_block.take() {
                        if let ContentBlock::ToolUse { id, name, input } = &mut block {
                            if !tool_input_json.is_empty() {
                                *input =
                                    serde_json::from_str(&tool_input_json).map_err(|error| {
                                        anyhow!(
                                            "invalid tool input JSON for {name} ({id}): {error}"
                                        )
                                    })?;
                            }
                        }
                        // 如果是 ToolUse，提取到 tool_calls + 推 ToolCallStart
                        if let ContentBlock::ToolUse { id, name, input } = &block {
                            let tc = crate::message::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            };
                            send_progress(
                                progress_tx,
                                TurnEvent::ToolCallStart {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    input: tc.input.clone(),
                                },
                            );
                            tool_calls.push(tc);
                        } else {
                            content.push(block);
                        }
                    }
                }
                ProviderEvent::MessageDelta {
                    usage: _,
                    stop_reason: sr,
                } => {
                    stop_reason = sr;
                }
                ProviderEvent::MessageStop => {
                    saw_message_stop = true;
                    break;
                }
                ProviderEvent::Error {
                    error,
                    retryable: _,
                } => {
                    return Err(anyhow!("provider error: {error}"));
                }
            }
        }
        if !saw_message_stop {
            return Err(anyhow!(
                "provider stream ended before message_stop; stop_reason={:?}, content_blocks={}, tool_calls={}",
                stop_reason,
                content.len(),
                tool_calls.len()
            ));
        }

        Ok(AssistantStream {
            content,
            tool_calls,
            stop_reason,
            model,
        })
    }
}

fn send_progress(tx: Option<&mpsc::Sender<TurnEvent>>, event: TurnEvent) {
    if let Some(tx) = tx {
        // Progress is UI-only telemetry. It must never apply backpressure to the
        // agent loop because `run_one_turn()` intentionally has no subscriber.
        let _ = tx.try_send(event);
    }
}

/// Estimate the provider-visible projection for a raw message history.
pub fn estimate_context_projection(messages: &[AgentMessage]) -> ContextProjectionStats {
    let llm_messages = project_agent_messages_for_provider(messages);
    let mut compacted_history_refs = Vec::new();
    let mut compaction_markers = 0usize;
    for message in messages {
        if let AgentMessage::Compaction { anchor_refs, .. } = message {
            compaction_markers += 1;
            compacted_history_refs.extend(anchor_refs.iter().cloned());
        }
    }
    ContextProjectionStats {
        raw_messages: messages.len(),
        llm_messages: llm_messages.len(),
        llm_message_bytes: estimate_llm_messages_bytes(&llm_messages),
        compaction_markers,
        compacted_history_refs,
    }
}

/// Apply rolling history compaction to raw messages.
///
/// When `force` is false, compaction only runs after the byte or message-count
/// threshold in `budget` is reached. When true, thresholds are ignored but safe
/// protocol boundaries are still required.
pub fn compact_messages_rolling(
    messages: &mut Vec<AgentMessage>,
    budget: &LoopBudget,
    artifact_root: Option<&Path>,
    force: bool,
) -> RollingCompactionReport {
    let before = estimate_context_projection(messages);
    let should_compact_by_bytes = budget.compact_context_threshold_bytes > 0
        && before.llm_message_bytes > budget.compact_context_threshold_bytes;
    let should_compact_by_count = budget.compact_message_count_threshold > 0
        && before.llm_messages > budget.compact_message_count_threshold;
    if !force && !should_compact_by_bytes && !should_compact_by_count {
        return RollingCompactionReport::skipped("below_threshold", before);
    }

    let Some(selection) = select_rolling_compaction_boundary(
        messages,
        budget,
        should_compact_by_bytes,
        should_compact_by_count,
    ) else {
        return RollingCompactionReport::skipped("no_safe_prefix_boundary", before);
    };
    let boundary = selection.boundary;

    let compacted = messages[..boundary].to_vec();
    let kept = messages[boundary..].to_vec();
    let parent_history_refs = collect_compaction_anchor_refs(&compacted);
    let stored = match crate::artifact::store_history_compaction(&compacted, artifact_root) {
        Ok(stored) => stored,
        Err(error) => {
            return RollingCompactionReport::skipped(
                format!("artifact_storage_failed: {error}"),
                before,
            );
        }
    };

    let mut anchor_refs = vec![stored.artifact_ref.clone()];
    for parent in &parent_history_refs {
        if !anchor_refs.contains(parent) {
            anchor_refs.push(parent.clone());
        }
    }

    let summary = render_history_compaction_summary(
        &compacted,
        &stored,
        &parent_history_refs,
        &anchor_refs,
        budget.compact_summary_bytes,
    );
    let compacted_messages = compacted.len();
    let kept_messages = kept.len();
    *messages = vec![AgentMessage::Compaction {
        summary: summary.clone(),
        first_kept_entry_id: format!("message_index:{boundary}"),
        anchor_refs: anchor_refs.clone(),
    }];
    messages.extend(kept);
    let after = estimate_context_projection(messages);
    let base_reason = if force {
        "forced".to_string()
    } else if should_compact_by_bytes {
        "context_bytes_threshold".to_string()
    } else {
        "message_count_threshold".to_string()
    };
    let still_over_byte_threshold = !force
        && should_compact_by_bytes
        && budget.compact_context_threshold_bytes > 0
        && after.llm_message_bytes > budget.compact_context_threshold_bytes;
    let still_over_count_threshold = !force
        && should_compact_by_count
        && budget.compact_message_count_threshold > 0
        && after.llm_messages > budget.compact_message_count_threshold;
    let reason = if still_over_byte_threshold || still_over_count_threshold {
        format!("{base_reason}_recent_window_too_large")
    } else {
        base_reason
    };

    RollingCompactionReport {
        compacted: true,
        reason,
        before,
        after,
        compacted_messages,
        kept_messages,
        artifact_ref: Some(stored.artifact_ref),
        artifact_path: Some(stored.path),
        sha256: Some(stored.sha256),
        parent_history_refs,
        anchor_refs,
        artifact_bytes: stored.bytes,
        summary_bytes: summary.len(),
    }
}

impl RollingCompactionReport {
    fn skipped(reason: impl Into<String>, stats: ContextProjectionStats) -> Self {
        Self {
            compacted: false,
            reason: reason.into(),
            before: stats.clone(),
            after: stats,
            compacted_messages: 0,
            kept_messages: 0,
            artifact_ref: None,
            artifact_path: None,
            sha256: None,
            parent_history_refs: Vec::new(),
            anchor_refs: Vec::new(),
            artifact_bytes: 0,
            summary_bytes: 0,
        }
    }
}

fn emit_compaction_report(iteration: usize, budget: &LoopBudget, report: &RollingCompactionReport) {
    if report.reason == "below_threshold" {
        return;
    }
    if report.compacted {
        crate::diagnostics::emit(
            "context",
            "rolling_compaction",
            serde_json::json!({
                "iteration": iteration,
                "reason": report.reason,
                "compacted_messages": report.compacted_messages,
                "kept_messages": report.kept_messages,
                "before": context_stats_json(&report.before),
                "after": context_stats_json(&report.after),
                "artifact_ref": report.artifact_ref,
                "artifact_path": report.artifact_path,
                "sha256": report.sha256,
                "parent_history_refs": report.parent_history_refs,
                "anchor_refs": report.anchor_refs,
                "artifact_bytes": report.artifact_bytes,
                "summary_bytes": report.summary_bytes,
                "budget": loop_budget_snapshot(budget),
            }),
        );
    } else {
        crate::diagnostics::emit(
            "context",
            "rolling_compaction_skipped",
            serde_json::json!({
                "iteration": iteration,
                "reason": report.reason,
                "before": context_stats_json(&report.before),
                "budget": loop_budget_snapshot(budget),
            }),
        );
    }
}

fn context_stats_json(stats: &ContextProjectionStats) -> serde_json::Value {
    serde_json::json!({
        "raw_messages": stats.raw_messages,
        "llm_messages": stats.llm_messages,
        "llm_message_bytes": stats.llm_message_bytes,
        "compaction_markers": stats.compaction_markers,
        "compacted_history_refs": stats.compacted_history_refs,
    })
}

/// Project raw agent messages into provider-visible messages.
///
/// `ToLlmMessage::to_llm` deliberately keeps internal variants out of the
/// context. This projection is the AgentLoop request layer: it injects
/// compaction summaries as user-visible context while still keeping raw
/// compaction markers as internal state.
pub fn project_agent_messages_for_provider(messages: &[AgentMessage]) -> Vec<LlmMessage> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Compaction { summary, .. } => Some(LlmMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!(
                        "# conversation_history_compaction\n{}\nThe latest uncompressed messages follow.",
                        summary
                    ),
                }],
            }),
            _ => message.to_llm(),
        })
        .collect()
}

fn estimate_llm_messages_bytes(messages: &[LlmMessage]) -> usize {
    serde_json::to_string(messages)
        .map(|s| s.len())
        .unwrap_or(0)
}

fn rolling_compaction_boundary(
    messages: &[AgentMessage],
    keep_recent_messages: usize,
    min_prefix_messages: usize,
) -> Option<usize> {
    let keep_recent_messages = keep_recent_messages.max(1);
    if messages.len() <= keep_recent_messages.saturating_add(min_prefix_messages) {
        return None;
    }
    let max_boundary = messages.len().saturating_sub(keep_recent_messages);
    let min_boundary = min_prefix_messages.max(1);
    (min_boundary..=max_boundary)
        .rev()
        .find(|&index| is_safe_llm_suffix(&messages[index..]))
}

fn is_safe_llm_suffix(messages: &[AgentMessage]) -> bool {
    if messages.is_empty() {
        return false;
    }
    let mut outstanding_tool_uses: HashMap<&str, usize> = HashMap::new();
    for message in messages {
        match message {
            AgentMessage::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    *outstanding_tool_uses.entry(call.id.as_str()).or_insert(0) += 1;
                }
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                let Some(count) = outstanding_tool_uses.get_mut(tool_call_id.as_str()) else {
                    return false;
                };
                if *count == 0 {
                    return false;
                }
                *count -= 1;
            }
            _ => {}
        }
    }
    outstanding_tool_uses.values().all(|count| *count == 0)
}

#[derive(Debug, Clone, Copy)]
struct CompactionBoundarySelection {
    boundary: usize,
}

fn select_rolling_compaction_boundary(
    messages: &[AgentMessage],
    budget: &LoopBudget,
    should_compact_by_bytes: bool,
    should_compact_by_count: bool,
) -> Option<CompactionBoundarySelection> {
    let max_keep = budget.compact_keep_recent_messages.max(1);
    let byte_target = (should_compact_by_bytes && budget.compact_context_threshold_bytes > 0)
        .then_some(budget.compact_context_threshold_bytes);
    let count_target = (should_compact_by_count && budget.compact_message_count_threshold > 0)
        .then_some(budget.compact_message_count_threshold);
    let mut fallback = None;
    for keep_recent in (1..=max_keep).rev() {
        let Some(boundary) =
            rolling_compaction_boundary(messages, keep_recent, budget.compact_min_prefix_messages)
        else {
            continue;
        };
        let projected_bytes = estimate_projected_bytes_after_compaction(messages, boundary, budget);
        let projected_messages = project_agent_messages_for_provider(&messages[boundary..])
            .len()
            .saturating_add(1);
        let under_target = byte_target.is_none_or(|target| projected_bytes <= target)
            && count_target.is_none_or(|target| projected_messages <= target);
        let selection = CompactionBoundarySelection { boundary };
        if under_target {
            return Some(selection);
        }
        fallback = Some(selection);
    }
    fallback
}

fn estimate_projected_bytes_after_compaction(
    messages: &[AgentMessage],
    boundary: usize,
    budget: &LoopBudget,
) -> usize {
    let summary_budget = budget.compact_summary_bytes.max(512);
    let mut projected_messages = Vec::with_capacity(messages.len().saturating_sub(boundary) + 1);
    projected_messages.push(AgentMessage::Compaction {
        summary: "x".repeat(summary_budget),
        first_kept_entry_id: format!("message_index:{boundary}"),
        anchor_refs: vec![
            "artifact://history/sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
        ],
    });
    projected_messages.extend_from_slice(&messages[boundary..]);
    estimate_context_projection(&projected_messages).llm_message_bytes
}

fn collect_compaction_anchor_refs(messages: &[AgentMessage]) -> Vec<String> {
    let mut refs = Vec::new();
    for message in messages {
        if let AgentMessage::Compaction { anchor_refs, .. } = message {
            for anchor in anchor_refs {
                if !refs.contains(anchor) {
                    refs.push(anchor.clone());
                }
            }
        }
    }
    refs
}

fn render_history_compaction_summary(
    messages: &[AgentMessage],
    stored: &crate::artifact::HistoryCompactionArtifact,
    parent_history_refs: &[String],
    anchor_refs: &[String],
    max_bytes: usize,
) -> String {
    let mut out = String::new();
    let budget = max_bytes.max(512);
    push_summary_line(
        &mut out,
        budget,
        "schema_version: 1\nkind: conversation_history_compaction",
    );
    push_summary_line(
        &mut out,
        budget,
        &format!(
            "compacted_messages: {}\nhistory_segment: {}\nsegment_sha256: {}\nsegment_bytes: {}\nparent_history_refs: {}\nanchor_refs: {}\nread_more: call artifact_read with {{\"ref\":\"{}\",\"offset\":0,\"limit\":12000}}",
            messages.len(),
            stored.artifact_ref,
            stored.sha256,
            stored.bytes,
            if parent_history_refs.is_empty() {
                "none".into()
            } else {
                parent_history_refs.join(", ")
            },
            anchor_refs.join(", "),
            stored.artifact_ref,
        ),
    );
    push_summary_line(
        &mut out,
        budget,
        "summary_policy: deterministic lossy projection; prefer latest uncompressed messages for current state, and read history_segment plus parent_history_refs when older exact details matter.",
    );
    push_summary_line(&mut out, budget, "timeline:");
    for (index, message) in messages.iter().enumerate() {
        if !push_summary_line(
            &mut out,
            budget,
            &format!("- [{}] {}", index, compacted_message_summary(message, 360)),
        ) {
            break;
        }
    }
    out
}

fn push_summary_line(out: &mut String, max_bytes: usize, line: &str) -> bool {
    let needs_newline = !out.is_empty();
    let extra = line.len() + usize::from(needs_newline);
    if out.len().saturating_add(extra) <= max_bytes {
        if needs_newline {
            out.push('\n');
        }
        out.push_str(line);
        return true;
    }
    let marker = "\n... [history compaction summary truncated; use artifact_read(history_segment) for exact older messages] ...";
    if out.len() < max_bytes {
        let remaining = max_bytes.saturating_sub(out.len());
        let marker = truncate_chars(marker, remaining);
        out.push_str(&marker);
    }
    false
}

fn compacted_message_summary(message: &AgentMessage, max_chars: usize) -> String {
    match message {
        AgentMessage::User { content, .. } => {
            format!("user: {}", content_blocks_summary(content, max_chars))
        }
        AgentMessage::Assistant {
            content,
            tool_calls,
            model,
            ..
        } => {
            let text = content_blocks_summary(content, max_chars / 2);
            let calls = if tool_calls.is_empty() {
                "no_tool_calls".to_string()
            } else {
                tool_calls
                    .iter()
                    .map(|call| {
                        format!(
                            "{}({})",
                            call.name,
                            truncate_chars(&canonical_json(&call.input), 160)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            truncate_chars(
                &format!("assistant[{model}]: {text}; tool_calls: {calls}"),
                max_chars,
            )
        }
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => truncate_chars(
            &format!(
                "tool_result[id={}, is_error={}]: {}",
                tool_call_id,
                is_error,
                content_blocks_summary(content, max_chars / 2)
            ),
            max_chars,
        ),
        AgentMessage::Notification { kind, payload } => truncate_chars(
            &format!(
                "notification[{kind}]: {}",
                truncate_chars(&payload.to_string(), 240)
            ),
            max_chars,
        ),
        AgentMessage::Compaction {
            summary,
            anchor_refs,
            ..
        } => truncate_chars(
            &format!(
                "previous_compaction anchors={}: {}",
                anchor_refs.join(","),
                summary
            ),
            max_chars,
        ),
        AgentMessage::BranchMarker { parent_id, label } => {
            format!("branch_marker parent={parent_id} label={label}")
        }
        AgentMessage::Steering { content } => {
            format!("steering: {}", content_blocks_summary(content, max_chars))
        }
        AgentMessage::FollowUp { content } => {
            format!("follow_up: {}", content_blocks_summary(content, max_chars))
        }
    }
}

fn content_blocks_summary(content: &[ContentBlock], max_chars: usize) -> String {
    let rendered = content
        .iter()
        .map(content_block_summary)
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&rendered, max_chars)
}

fn content_block_summary(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image { media_type, data } => {
            format!("[image media_type={media_type} bytes={}]", data.len())
        }
        ContentBlock::Thinking { text, .. } => format!("[thinking] {text}"),
        ContentBlock::ToolUse { id, name, input } => {
            format!(
                "[tool_use id={id} name={name} input={}]",
                canonical_json(input)
            )
        }
        ContentBlock::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            let nested = content
                .iter()
                .map(|block| content_block_summary(block.as_ref()))
                .collect::<Vec<_>>()
                .join(" ");
            format!("[tool_result id={tool_call_id} is_error={is_error}] {nested}")
        }
    }
}

/// 流式收集到的 assistant 消息（中间表示）。
#[derive(Debug, Clone)]
struct AssistantStream {
    content: Vec<ContentBlock>,
    tool_calls: Vec<crate::message::ToolCall>,
    stop_reason: LlmStopReason,
    model: String,
}

impl AssistantStream {
    /// 转成 AgentMessage 形式。
    fn into_agent_message(self) -> AgentMessage {
        AgentMessage::Assistant {
            content: self.content,
            tool_calls: self.tool_calls,
            model: self.model,
            usage: None,
        }
    }
}

/// 一轮 run 的总结。
#[derive(Debug, Default, Clone)]
pub struct LoopSummary {
    /// 完成的 turns 数。
    pub turns_completed: usize,
    /// 执行的 tool_calls 数。
    pub tool_calls_executed: usize,
    /// If a loop guard forced the final answer turn, this records the reason.
    pub finalization_reason: Option<String>,
}

fn loop_budget_snapshot(budget: &LoopBudget) -> serde_json::Value {
    serde_json::json!({
        "max_model_waves": budget.max_model_waves,
        "max_tool_calls": budget.max_tool_calls,
        "max_consecutive_error_waves": budget.max_consecutive_error_waves,
        "max_repeated_tool_signature": budget.max_repeated_tool_signature,
        "compact_context_threshold_bytes": budget.compact_context_threshold_bytes,
        "compact_message_count_threshold": budget.compact_message_count_threshold,
        "compact_keep_recent_messages": budget.compact_keep_recent_messages,
        "compact_min_prefix_messages": budget.compact_min_prefix_messages,
        "compact_summary_bytes": budget.compact_summary_bytes,
    })
}

fn tool_call_signature(call: &crate::message::ToolCall) -> String {
    format!("{} {}", call.name, canonical_json(&call.input))
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => serde_json::to_string(value).unwrap_or_default(),
        serde_json::Value::Array(items) => {
            let rendered = items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{}]", rendered)
        }
        serde_json::Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let rendered = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{}}}", rendered)
        }
    }
}

fn estimate_provider_request_budget(request: &ProviderRequest) -> serde_json::Value {
    let system_bytes = request.system.len();
    let messages_bytes = serde_json::to_string(&request.messages)
        .map(|s| s.len())
        .unwrap_or(0);
    let tools_bytes = serde_json::to_string(&request.tools)
        .map(|s| s.len())
        .unwrap_or(0);
    let total_bytes = system_bytes
        .saturating_add(messages_bytes)
        .saturating_add(tools_bytes);
    serde_json::json!({
        "system_bytes": system_bytes,
        "messages_bytes": messages_bytes,
        "tools_bytes": tools_bytes,
        "total_bytes": total_bytes,
        "estimated_tokens": total_bytes.div_ceil(4),
        "message_count": request.messages.len(),
        "tool_count": request.tools.len(),
        "estimate": "bytes_div_4"
    })
}

fn summarize_tool_result(result: &ToolResult) -> serde_json::Value {
    let text = crate::artifact::tool_result_text(result);
    serde_json::json!({
        "tool_call_id": result.tool_call_id,
        "is_error": result.is_error,
        "block_count": result.content.len(),
        "text_bytes": text.len(),
        "preview": truncate_chars(&text, 500),
    })
}

fn tool_result_preview(result: &ToolResult, max_chars: usize) -> String {
    truncate_chars(&crate::artifact::tool_result_text(result), max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProvider, MockResponse};
    use crate::tool::EchoTool;
    use crate::AgentMessage;

    fn steering_text(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: text.into() }]
    }

    fn steering_user(text: &str) -> AgentMessage {
        AgentMessage::User {
            content: steering_text(text),
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn trusted_input_initial_reuse_and_nonuser_clear() {
        let provider = Arc::new(MockProvider::new(
            (0..3)
                .map(|_| MockResponse::AssistantText {
                    text: "provider text is not evidence".into(),
                })
                .collect(),
        ));
        let mut agent = AgentLoop::new(provider, vec![], "policy text is not evidence");
        agent
            .state
            .messages
            .push(steering_user("old history is not evidence"));
        agent
            .run_one_turn(steering_user("actual input"))
            .await
            .unwrap();
        let first = agent.tool_ctx.accepted_input.clone().unwrap();
        assert_eq!(first.source, crate::InputSource::User);
        assert_eq!(first.text, "actual input");
        uuid::Uuid::parse_str(&first.id).unwrap();
        let encoded = serde_json::to_string(&first).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::AcceptedInput>(&encoded).unwrap(),
            first
        );

        agent.tool_ctx.input_source = crate::InputSource::Peer;
        agent
            .run_one_turn(steering_user("actual input"))
            .await
            .unwrap();
        let second = agent.tool_ctx.accepted_input.clone().unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(second.source, crate::InputSource::Peer);
        agent
            .run_one_turn(AgentMessage::Notification {
                kind: "resume".into(),
                payload: serde_json::json!({"text":"not a user"}),
            })
            .await
            .unwrap();
        assert!(agent.tool_ctx.accepted_input.is_none());
    }

    #[tokio::test]
    async fn trusted_input_steering_commits_latest_with_host_source() {
        for source in [
            crate::InputSource::User,
            crate::InputSource::Peer,
            crate::InputSource::Delegate,
        ] {
            let provider = Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
                text: "done".into(),
            }]));
            let mut agent = AgentLoop::new(provider, vec![], "test");
            agent.tool_ctx.input_source = source;
            agent.run_one_turn(steering_user("initial")).await.unwrap();
            let initial_id = agent.tool_ctx.accepted_input.as_ref().unwrap().id.clone();
            agent
                .steering
                .push(steering_text("first steering"), 1)
                .unwrap();
            assert_eq!(
                agent.tool_ctx.accepted_input.as_ref().unwrap().text,
                "initial"
            );
            assert!(agent.consume_steering());
            let first = agent.tool_ctx.accepted_input.clone().unwrap();
            assert_ne!(first.id, initial_id);
            assert_eq!(first.text, "first steering");
            assert_eq!(first.source, source);
            agent
                .steering
                .push(steering_text("second steering"), 2)
                .unwrap();
            agent
                .steering
                .push(steering_text("latest steering"), 3)
                .unwrap();
            assert!(agent.consume_steering());
            let latest = agent.tool_ctx.accepted_input.clone().unwrap();
            assert_ne!(latest.id, first.id);
            assert_eq!(latest.text, "latest steering");
            assert_eq!(latest.source, source);
            assert_eq!(agent.steering.take_receipts(), vec![1, 2, 3]);
            assert!(!agent.consume_steering());
            assert_eq!(agent.tool_ctx.accepted_input, Some(latest));
        }
    }

    fn tool_wave(calls: Vec<(&str, &str, serde_json::Value)>) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        for (index, (id, name, input)) in calls.into_iter().enumerate() {
            events.push(ProviderEvent::ContentBlockStart {
                index,
                block: ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                },
            });
            events.push(ProviderEvent::ContentBlockStop { index });
        }
        events.push(ProviderEvent::MessageStop);
        events
    }

    fn choice_input() -> serde_json::Value {
        serde_json::json!({"title":"Continue?", "body":"Choose", "options":[{"id":"yes", "label":"Yes"}]})
    }

    #[tokio::test]
    async fn environment_switch_waits_for_all_receipts_in_the_current_wave() {
        use crate::loop_environment::{LoopEnvironment, LoopEnvironmentSource};
        use crate::tool::{ExternalTool, ToolError, ToolImpl, ToolSpec};
        struct Pending(std::sync::Mutex<Option<LoopEnvironment>>);
        impl LoopEnvironmentSource for Pending {
            fn take_update(&self) -> Option<LoopEnvironment> {
                self.0.lock().unwrap().take()
            }
        }
        struct Switch(Arc<Pending>);
        #[async_trait::async_trait]
        impl ToolImpl for Switch {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "switch".into(),
                    label: "switch".into(),
                    description: "test switch".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &ToolContext,
            ) -> Result<ToolResult, ToolError> {
                *self.0 .0.lock().unwrap() = Some(LoopEnvironment {
                    tools: vec![ToolKind::Add(crate::AddTool)],
                    system: "NEW_SYSTEM".into(),
                    policies: Default::default(),
                });
                Ok(ToolResult {
                    tool_call_id: String::new(),
                    is_error: false,
                    content: vec![],
                })
            }
        }
        let source = Arc::new(Pending(std::sync::Mutex::new(None)));
        let provider = Arc::new(RawEventProvider::new(vec![
            tool_wave(vec![
                ("switch", "switch", serde_json::json!({})),
                (
                    "old",
                    "echo",
                    serde_json::json!({"text":"still allowed in old wave"}),
                ),
                ("premature", "add", serde_json::json!({"a":1,"b":2})),
            ]),
            tool_wave(vec![("new", "add", serde_json::json!({"a":1,"b":2}))]),
            vec![ProviderEvent::MessageStop],
        ]));
        let mut agent = AgentLoop::new(
            provider,
            vec![
                ToolKind::External(ExternalTool::new(Arc::new(Switch(source.clone())))),
                ToolKind::Echo(EchoTool),
            ],
            "OLD_SYSTEM",
        );
        agent.set_environment_source(source).unwrap();
        agent
            .run_one_turn(steering_user("switch and continue"))
            .await
            .unwrap();
        let receipts: Vec<_> = agent
            .state
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult {
                    tool_call_id,
                    is_error,
                    ..
                } => Some((tool_call_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            receipts,
            [
                ("switch", false),
                ("old", false),
                ("premature", true),
                ("new", false)
            ]
        );
        assert_eq!(agent.system, "NEW_SYSTEM");
        assert_eq!(agent.tools.len(), 1);
        assert_eq!(
            repair_interrupted_history(&mut agent.state.messages, "verify pairs"),
            0
        );
    }

    #[tokio::test]
    async fn steering_between_tools_completes_pairs_before_user_and_skips_stale_write() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(RawEventProvider::new(vec![
            tool_wave(vec![
                ("choice", "ask_user_choice", choice_input()),
                (
                    "stale",
                    "write",
                    serde_json::json!({"path":"stale.txt","content":"bad"}),
                ),
                (
                    "also-stale",
                    "write",
                    serde_json::json!({"path":"also-stale.txt","content":"bad"}),
                ),
            ]),
            vec![ProviderEvent::MessageStop],
        ]));
        let inbox = SteeringInbox::default();
        let broker = Arc::new(crate::choice::ChoiceBroker::new());
        let mut requests = broker.subscribe();
        let mut agent = AgentLoop::new(
            provider,
            vec![
                ToolKind::Choice(crate::tool::ChoiceTool),
                ToolKind::Write(crate::tool::WriteTool),
            ],
            "test",
        );
        agent.tool_ctx.cwd = dir.path().into();
        agent.tool_ctx.choices = Some(broker.clone());
        agent.set_steering_inbox(inbox.clone());
        let control = async {
            let request = requests.recv().await.unwrap();
            let id = inbox.push(steering_text("Do something else"), 10).unwrap();
            let supplement = inbox
                .push(steering_text("Also keep the files unchanged"), 12)
                .unwrap();
            let before = inbox.latest_snapshot();
            assert_eq!(before.pending[0].id, id);
            assert!(matches!(
                before.messages.last(),
                Some(AgentMessage::Assistant { .. })
            ));
            broker.answer(&request.id, "yes").unwrap();
            (id, supplement)
        };
        let (result, id) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(agent.run_one_turn(steering_user("start")), control)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap().tool_calls_executed, 1);
        assert!(!dir.path().join("stale.txt").exists());
        assert!(!dir.path().join("also-stale.txt").exists());
        assert!(
            matches!(&agent.state.messages[2], AgentMessage::ToolResult { tool_call_id, is_error: false, .. } if tool_call_id == "choice")
        );
        assert!(
            matches!(&agent.state.messages[3], AgentMessage::ToolResult { tool_call_id, is_error: true, .. } if tool_call_id == "stale")
        );
        assert!(matches!(
            &agent.state.messages[5],
            AgentMessage::User { timestamp: 10, .. }
        ));
        assert!(matches!(
            &agent.state.messages[4],
            AgentMessage::ToolResult { is_error: true, .. }
        ));
        assert!(matches!(
            &agent.state.messages[6],
            AgentMessage::User { timestamp: 12, .. }
        ));
        assert_eq!(inbox.take_receipts(), vec![id.0, id.1]);
        let snapshot = inbox.latest_snapshot();
        assert!(snapshot.pending.is_empty());
        assert_eq!(snapshot.messages.len(), agent.state.messages.len());
        assert!(inbox.snapshot_if_changed(snapshot.generation).is_none());
        let late_id = inbox.push(steering_text("after worker"), 11).unwrap();
        drop(agent);
        let late = inbox.snapshot_if_changed(snapshot.generation).unwrap();
        assert_eq!(late.pending[0].id, late_id);
        assert_eq!(late.messages.len(), snapshot.messages.len());
    }

    struct SteeringStreamProvider {
        inner: MockProvider,
        inbox: SteeringInbox,
        first: std::sync::atomic::AtomicBool,
        mode: u8,
        requests: std::sync::Mutex<Vec<ProviderRequest>>,
    }

    #[async_trait::async_trait]
    impl Provider for SteeringStreamProvider {
        fn id(&self) -> &str {
            "steering-stream"
        }
        fn model(&self) -> &str {
            "test"
        }
        fn context_window(&self) -> usize {
            8000
        }
        fn cache_key(&self, _: &ProviderRequest) -> String {
            "test".into()
        }
        fn abort_signal(&self) -> crate::CancellationToken {
            crate::CancellationToken::new()
        }
        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = ProviderEvent> + Send>>,
            crate::provider::ProviderError,
        > {
            self.requests.lock().unwrap().push(request.clone());
            let first = self.first.swap(false, std::sync::atomic::Ordering::SeqCst);
            let mut stream = self.inner.stream(request).await?;
            let inbox = self.inbox.clone();
            let mode = self.mode;
            if first && mode == 0 {
                inbox.push(steering_text("new direction"), 9).unwrap();
                return std::future::pending().await;
            }
            Ok(Box::pin(async_stream::stream! {
                while let Some(event) = stream.next().await {
                    if first && matches!(event, ProviderEvent::MessageStop) {
                        inbox.push(steering_text("new direction"), 9).unwrap();
                        if mode == 1 {
                            std::future::pending::<()>().await;
                        }
                        if mode == 3 {
                            yield ProviderEvent::Error {
                                error: crate::provider::ProviderError::StreamInterrupted,
                                retryable: false,
                            };
                            break;
                        }
                    }
                    yield event;
                }
            }))
        }
    }

    #[tokio::test]
    async fn steering_cancels_provider_wait_partial_stream_and_catches_final_poll() {
        for mode in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let inbox = SteeringInbox::default();
            let provider = Arc::new(SteeringStreamProvider {
                inner: MockProvider::new(vec![
                    if mode < 4 {
                        MockResponse::AssistantText {
                            text: "stale".into(),
                        }
                    } else {
                        MockResponse::ToolCall {
                            name: "write".into(),
                            input: serde_json::json!({"path":"stale.txt","content":"never"}),
                        }
                    },
                    MockResponse::AssistantText {
                        text: "replanned".into(),
                    },
                ]),
                inbox: inbox.clone(),
                first: std::sync::atomic::AtomicBool::new(true),
                mode: mode % 4,
                requests: std::sync::Mutex::new(Vec::new()),
            });
            let mut agent = AgentLoop::new(
                provider.clone(),
                vec![ToolKind::Write(crate::tool::WriteTool)],
                "test",
            );
            agent.tool_ctx.cwd = dir.path().into();
            agent.set_steering_inbox(inbox.clone());
            let (diagnostics_tx, mut diagnostics_rx) = mpsc::unbounded_channel();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                crate::diagnostics::observe(
                    diagnostics_tx,
                    agent.run_one_turn(steering_user("start")),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(agent.state.messages.len(), 3, "mode {mode}");
            assert!(matches!(
                &agent.state.messages[1],
                AgentMessage::User { timestamp: 9, .. }
            ));
            let serialized = serde_json::to_string(&agent.state.messages).unwrap();
            assert!(!serialized.contains("stale"));
            assert!(serialized.contains("replanned"));
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(serde_json::to_string(&requests[1].messages)
                .unwrap()
                .contains("new direction"));
            assert_eq!(inbox.take_receipts(), vec![1]);
            assert!(!dir.path().join("stale.txt").exists());
            assert!(agent.state.executed_tools.is_empty());
            let mut cached_text = String::new();
            let mut request_count = 0;
            while let Ok(event) = diagnostics_rx.try_recv() {
                match event.event.as_str() {
                    "provider_request" => {
                        request_count += 1;
                        cached_text.clear();
                    }
                    "content_block_delta" => {
                        cached_text.push_str(event.data["delta"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            assert_eq!(request_count, 2);
            assert_eq!(cached_text, "replanned");
        }
    }

    #[tokio::test]
    async fn provider_error_retains_completed_side_effect_and_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall {
            name: "write".into(),
            input: serde_json::json!({"path":"done.txt","content":"once"}),
        }]));
        let mut agent = AgentLoop::new(
            provider,
            vec![ToolKind::Write(crate::tool::WriteTool)],
            "test",
        );
        agent.tool_ctx.cwd = dir.path().into();
        let inbox = SteeringInbox::default();
        agent.set_steering_inbox(inbox.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (progress_tx, mut progress_rx) = mpsc::channel(32);
        let error = crate::diagnostics::observe(
            tx,
            agent.run_one_turn_with_progress(steering_user("write"), progress_tx),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exhausted"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("done.txt")).unwrap(),
            "once"
        );
        assert_eq!(agent.state.executed_tools.len(), 1);
        assert!(matches!(
            agent.state.messages.last(),
            Some(AgentMessage::ToolResult {
                is_error: false,
                ..
            })
        ));
        assert_eq!(agent.recover_interrupted("again"), 0);
        assert_eq!(inbox.latest_snapshot().messages.len(), 3);
        let mut checkpoints = 0;
        while let Ok(event) = rx.try_recv() {
            if event.layer == "execution" && event.event == "history_checkpoint" {
                checkpoints += 1;
                assert!(event.data.get("generation").is_some());
                assert!(event.data.get("messages").is_none());
            }
        }
        assert_eq!(checkpoints, 4); // user, assistant, result, error recovery
        let mut endings = 0;
        while let Ok(event) = progress_rx.try_recv() {
            if let TurnEvent::TurnEnd {
                turns_completed,
                tool_calls_executed,
                error,
            } = event
            {
                endings += 1;
                assert_eq!((turns_completed, tool_calls_executed), (1, 1));
                assert!(error.unwrap().contains("exhausted"));
            }
        }
        assert_eq!(endings, 1);
    }

    #[tokio::test]
    async fn cancellation_and_dropped_future_repair_missing_results_without_replaying() {
        for drop_future in [false, true] {
            let provider = Arc::new(RawEventProvider::new(vec![tool_wave(vec![
                ("waiting", "ask_user_choice", choice_input()),
                ("later", "echo", serde_json::json!({"text":"never"})),
            ])]));
            let broker = Arc::new(crate::choice::ChoiceBroker::new());
            let mut requests = broker.subscribe();
            let mut agent = AgentLoop::new(
                provider,
                vec![
                    ToolKind::Choice(crate::tool::ChoiceTool),
                    ToolKind::Echo(EchoTool),
                ],
                "test",
            );
            agent.tool_ctx.choices = Some(broker);
            let cancel = agent.cancel_sender();
            if drop_future {
                {
                    let future = agent.run_one_turn(steering_user("start"));
                    tokio::pin!(future);
                    tokio::select! {
                        result = &mut future => panic!("unexpected result: {result:?}"),
                        request = requests.recv() => { request.unwrap(); }
                    }
                }
                let mut restored = agent.steering.latest_snapshot().messages;
                assert_eq!(repair_interrupted_history(&mut restored, "restart"), 2);
                assert!(serde_json::to_string(&restored[3])
                    .unwrap()
                    .contains("unknown; inspect external state"));
                assert_eq!(agent.recover_interrupted("worker future cancelled"), 2);
            } else {
                let (result, ()) =
                    tokio::join!(agent.run_one_turn(steering_user("start")), async {
                        requests.recv().await.unwrap();
                        cancel.send(()).await.unwrap();
                    });
                assert!(result.is_err());
                assert!(agent.state.cancelled);
            }
            assert_eq!(agent.recover_interrupted("again"), 0);
            assert!(agent.state.executed_tools.is_empty());
            assert_eq!(agent.state.messages.len(), 4);
            for result in &agent.state.messages[2..] {
                assert!(matches!(
                    result,
                    AgentMessage::ToolResult { is_error: true, .. }
                ));
            }
            assert!(serde_json::to_string(&agent.state.messages[2])
                .unwrap()
                .contains("unknown; inspect external state"));
            assert!(serde_json::to_string(&agent.state.messages[3])
                .unwrap()
                .contains("tool was never started"));
        }
    }

    #[test]
    fn bounded_inbox_rejection_and_snapshot_serde_preserve_accepted_records() {
        let inbox = SteeringInbox::with_capacity(1);
        assert_eq!(inbox.push(steering_text("first"), 1).unwrap(), 1);
        let generation = inbox.latest_snapshot().generation;
        assert!(inbox.try_push(steering_text("rejected"), 2).is_err());
        assert!(inbox.snapshot_if_changed(generation).is_none());
        assert!(inbox.take_receipts().is_empty());
        let snapshot: SteeringSnapshot =
            serde_json::from_str(&serde_json::to_string(&inbox.latest_snapshot()).unwrap())
                .unwrap();
        assert_eq!(snapshot.pending.len(), 1);
        assert_eq!(snapshot.pending[0].id, 1);
        inbox.seed_history(vec![steering_user("prior committed message")]);
        let seeded = inbox.snapshot_if_changed(snapshot.generation).unwrap();
        assert_eq!(seeded.messages.len(), 1);
        assert_eq!(seeded.pending[0].id, 1);
    }

    #[tokio::test]
    async fn loop_runs_user_message_to_assistant_text() {
        // Mock provider 返回纯文本（无 tool_call）
        let provider = Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
            text: "Hi back!".into(),
        }]));
        let tools = vec![ToolKind::Echo(EchoTool)];
        let mut loop_ = AgentLoop::new(provider, tools, "你是一名乐于助人的助手。");

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "Hello".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.turns_completed, 1);
        assert_eq!(summary.tool_calls_executed, 0);
        assert_eq!(loop_.state.messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn progress_channel_emits_text_tool_call_turn_end() {
        // 验证 run_one_turn_with_progress 在以下场景发送事件:
        // - 第一轮:tool_call → TextDelta? (无, MockProvider 直接给整段 tool_call)
        // - ToolCallStart (tool_use block stop 时)
        // - ToolCallEnd (tool.execute 后)
        // - 第二轮:TextDelta (assistant 文本)
        // - TurnEnd
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": "ping"}),
            },
            MockResponse::AssistantText {
                text: "all done".into(),
            },
        ]));
        let tools = vec![ToolKind::Echo(EchoTool)];
        let mut loop_ = AgentLoop::new(provider, tools, "test");

        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);

        let user_msg = AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "echo ping".into(),
            }],
            timestamp: 0,
        };

        // 在后台 spawn 一个 task 来 drain channel
        let drain = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(e) = rx.recv().await {
                events.push(e);
            }
            events
        });

        let _ = loop_
            .run_one_turn_with_progress(user_msg, tx)
            .await
            .unwrap();

        // drop sender 让 drain 任务自然结束
        drop(loop_);
        let events = drain.await.unwrap();

        // 期望事件序列:
        // 1. ToolCallStart (echo, {text:ping})  — 第一轮 tool_use block stop
        // 2. ToolCallEnd   (echo, is_error=false, preview "[echo] ping")
        // 3. TextDelta("all done")             — 第二轮文本
        // 4. TurnEnd { turns:2, tool_calls:1 }
        assert!(
            matches!(events[0], TurnEvent::ToolCallStart { ref name, .. } if name == "echo"),
            "第 1 个事件是 ToolCallStart(echo),实际: {:?}",
            events[0]
        );
        assert!(
            matches!(&events[1], TurnEvent::ToolCallEnd { name, is_error, .. }
                if name == "echo" && !*is_error),
            "第 2 个事件是 ToolCallEnd(echo, ok),实际: {:?}",
            events[1]
        );
        assert!(
            matches!(&events[2], TurnEvent::TextDelta(s) if s == "all done"),
            "第 3 个事件是 TextDelta('all done'),实际: {:?}",
            events[2]
        );
        assert!(
            matches!(
                events[3],
                TurnEvent::TurnEnd {
                    turns_completed: 2,
                    tool_calls_executed: 1,
                    error: None
                }
            ),
            "第 4 个事件是 TurnEnd,实际: {:?}",
            events[3]
        );
        assert_eq!(events.len(), 4, "不应有多余 event");
    }

    #[tokio::test]
    async fn loop_executes_tool_call_and_returns_final_response() {
        // 第一次返回 tool_call，第二次返回最终文本
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": "world"}),
            },
            MockResponse::AssistantText {
                text: "Done echoing.".into(),
            },
        ]));
        let tools = vec![ToolKind::Echo(EchoTool)];
        let mut loop_ = AgentLoop::new(provider, tools, "test");

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "echo world".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.turns_completed, 2); // tool_call turn + final turn
        assert_eq!(summary.tool_calls_executed, 1);
        // messages: user + assistant(tool_call) + tool_result + assistant(final)
        assert_eq!(loop_.state.messages.len(), 4);
    }

    #[tokio::test]
    async fn oversized_tool_result_is_projected_before_context_commit() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let large_text = "x".repeat(crate::artifact::TOOL_RESULT_PROJECTION_THRESHOLD_BYTES + 5000);
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": large_text}),
            },
            MockResponse::AssistantText {
                text: "projected".into(),
            },
        ]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");
        loop_.tool_ctx.artifact_root = Some(artifact_dir.path().to_path_buf());

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "echo a large value".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 1);
        let AgentMessage::ToolResult { content, .. } = &loop_.state.messages[2] else {
            panic!("expected projected tool result message");
        };
        let ContentBlock::Text { text } = &content[0] else {
            panic!("expected text projection envelope");
        };
        assert!(text.contains("tool_result_projection"));
        assert!(text.contains("artifact_read"));
        assert!(text.len() < crate::artifact::TOOL_RESULT_PROJECTION_THRESHOLD_BYTES);
    }

    #[tokio::test]
    async fn tool_budget_allows_one_final_response_without_tools() {
        let mut responses = Vec::new();
        for i in 0..LoopBudget::default().max_model_waves {
            responses.push(MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": format!("step {i}")}),
            });
        }
        responses.push(MockResponse::AssistantText {
            text: "final answer".into(),
        });

        let provider = Arc::new(MockProvider::new(responses));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "run until the tool budget".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 100);
        assert_eq!(summary.turns_completed, 101);
        assert!(summary
            .finalization_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("模型工具波次")));
        let Some(AgentMessage::Assistant {
            content,
            tool_calls,
            ..
        }) = loop_.state.messages.last()
        else {
            panic!("expected final assistant message");
        };
        assert!(tool_calls.is_empty());
        assert!(matches!(
            content.first(),
            Some(ContentBlock::Text { text }) if text == "final answer"
        ));
    }

    #[tokio::test]
    async fn total_tool_call_budget_forces_tool_free_final_response() {
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": "first"}),
            },
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": "second"}),
            },
            MockResponse::AssistantText {
                text: "final after tool budget".into(),
            },
        ]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");
        loop_.budget.max_tool_calls = 2;

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "use two tools then stop".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 2);
        assert_eq!(summary.turns_completed, 3);
        assert!(summary
            .finalization_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("总工具调用预算")));
    }

    #[tokio::test]
    async fn consecutive_error_waves_force_tool_free_final_response() {
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "not_registered".into(),
                input: serde_json::json!({}),
            },
            MockResponse::ToolCall {
                name: "not_registered".into(),
                input: serde_json::json!({"again": true}),
            },
            MockResponse::AssistantText {
                text: "final after errors".into(),
            },
        ]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");
        loop_.budget.max_consecutive_error_waves = 2;

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "trigger tool errors".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 2);
        assert_eq!(summary.turns_completed, 3);
        assert!(summary
            .finalization_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("连续工具波次全部返回错误")));
    }

    #[tokio::test]
    async fn repeated_identical_tool_call_forces_tool_free_final_response() {
        let repeated_input = serde_json::json!({"b": 2, "a": 1});
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: repeated_input.clone(),
            },
            MockResponse::ToolCall {
                name: "echo".into(),
                input: repeated_input,
            },
            MockResponse::AssistantText {
                text: "final after repeated call".into(),
            },
        ]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");
        loop_.budget.max_repeated_tool_signature = 2;

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "repeat the same tool call".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 2);
        assert_eq!(summary.turns_completed, 3);
        assert!(summary
            .finalization_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("同一个工具调用签名")));
    }

    /// Test-only Provider：每次 stream() 调用消耗一组事件。
    /// 用于测试 collect_assistant_stream 对不同事件流的处理（不经过 SSE 解析器）。
    struct RawEventProvider {
        /// 每调一次 stream() 消耗一组。
        streams: std::sync::Mutex<Vec<Vec<crate::provider::ProviderEvent>>>,
    }

    impl RawEventProvider {
        fn new(streams: Vec<Vec<crate::provider::ProviderEvent>>) -> Self {
            Self {
                streams: std::sync::Mutex::new(streams),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for RawEventProvider {
        fn id(&self) -> &str {
            "raw-event-test"
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn context_window(&self) -> usize {
            8000
        }
        async fn stream(
            &self,
            _req: crate::provider::ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = crate::provider::ProviderEvent> + Send>>,
            crate::provider::ProviderError,
        > {
            use futures::stream;
            let events = self.streams.lock().unwrap().remove(0);
            Ok(Box::pin(stream::iter(events)))
        }
        fn cache_key(&self, _req: &crate::provider::ProviderRequest) -> String {
            "raw".into()
        }
        fn abort_signal(&self) -> crate::CancellationToken {
            crate::CancellationToken::new()
        }
    }

    struct CapturingProvider {
        requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    }

    impl CapturingProvider {
        fn new(requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>) -> Self {
            Self { requests }
        }
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for CapturingProvider {
        fn id(&self) -> &str {
            "capturing-test"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn context_window(&self) -> usize {
            8000
        }

        async fn stream(
            &self,
            req: crate::provider::ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = crate::provider::ProviderEvent> + Send>>,
            crate::provider::ProviderError,
        > {
            use futures::stream;
            self.requests.lock().unwrap().push(req);
            Ok(Box::pin(stream::iter(vec![
                crate::provider::ProviderEvent::MessageStart {
                    id: "msg_capture".into(),
                    model: "test".into(),
                },
                crate::provider::ProviderEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                crate::provider::ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: "captured".into(),
                },
                crate::provider::ProviderEvent::ContentBlockStop { index: 0 },
                crate::provider::ProviderEvent::MessageDelta {
                    usage: crate::message::Usage::default(),
                    stop_reason: LlmStopReason::EndTurn,
                },
                crate::provider::ProviderEvent::MessageStop,
            ])))
        }

        fn cache_key(&self, _req: &crate::provider::ProviderRequest) -> String {
            "capturing".into()
        }

        fn abort_signal(&self) -> crate::CancellationToken {
            crate::CancellationToken::new()
        }
    }

    #[tokio::test]
    async fn rolling_history_compaction_archives_prefix_and_projects_summary() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingProvider::new(requests.clone()));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");
        loop_.tool_ctx.artifact_root = Some(artifact_dir.path().to_path_buf());
        loop_.budget.compact_context_threshold_bytes = 0;
        loop_.budget.compact_message_count_threshold = 5;
        loop_.budget.compact_keep_recent_messages = 3;
        loop_.budget.compact_min_prefix_messages = 2;

        for i in 0..6 {
            loop_.state.messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!("old user {i}"),
                }],
                timestamp: i,
            });
            loop_.state.messages.push(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: format!("old answer {i}"),
                }],
                tool_calls: vec![],
                model: "test".into(),
                usage: None,
            });
        }

        let summary = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "current user".into(),
                }],
                timestamp: 99,
            })
            .await
            .unwrap();

        assert_eq!(summary.tool_calls_executed, 0);
        assert!(matches!(
            loop_.state.messages.first(),
            Some(AgentMessage::Compaction { summary, anchor_refs, .. })
                if summary.contains("conversation_history_compaction")
                    && summary.contains("old user 0")
                    && anchor_refs.iter().any(|value| value.starts_with("artifact://history/sha256:"))
        ));

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let Some(LlmMessage::User { content }) = requests[0].messages.first() else {
            panic!("first projected request message should be compaction summary");
        };
        let ContentBlock::Text { text } = &content[0] else {
            panic!("expected text compaction summary");
        };
        assert!(text.contains("conversation_history_compaction"));
        assert!(text.contains("artifact://history/sha256:"));
        assert!(requests[0].messages.iter().any(|message| matches!(
            message,
            LlmMessage::User { content }
                if content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text == "current user"))
        )));
    }

    #[test]
    fn rolling_compaction_boundary_keeps_tool_use_and_result_together() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let mut messages = vec![
            AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "old setup".into(),
                }],
                timestamp: 0,
            },
            AgentMessage::Assistant {
                content: vec![],
                tool_calls: vec![crate::message::ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text":"x"}),
                }],
                model: "test".into(),
                usage: None,
            },
            AgentMessage::ToolResult {
                tool_call_id: "call-1".into(),
                content: vec![ContentBlock::Text { text: "x".into() }],
                is_error: false,
            },
            AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "after tool".into(),
                }],
                tool_calls: vec![],
                model: "test".into(),
                usage: None,
            },
            AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "current".into(),
                }],
                timestamp: 1,
            },
        ];
        let budget = LoopBudget {
            compact_keep_recent_messages: 3,
            compact_min_prefix_messages: 1,
            ..LoopBudget::default()
        };

        let report =
            compact_messages_rolling(&mut messages, &budget, Some(artifact_dir.path()), true);

        assert!(report.compacted);
        assert_eq!(report.compacted_messages, 1);
        assert!(matches!(
            messages.first(),
            Some(AgentMessage::Compaction { .. })
        ));
        assert!(matches!(
            messages.get(1),
            Some(AgentMessage::Assistant { tool_calls, .. }) if tool_calls.len() == 1
        ));
        assert!(matches!(
            messages.get(2),
            Some(AgentMessage::ToolResult { tool_call_id, .. }) if tool_call_id == "call-1"
        ));
    }

    #[test]
    fn safe_suffix_rejects_duplicate_tool_id_without_matching_results() {
        let suffix = vec![
            AgentMessage::Assistant {
                content: vec![],
                tool_calls: vec![
                    crate::message::ToolCall {
                        id: "duplicate".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text":"one"}),
                    },
                    crate::message::ToolCall {
                        id: "duplicate".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text":"two"}),
                    },
                ],
                model: "test".into(),
                usage: None,
            },
            AgentMessage::ToolResult {
                tool_call_id: "duplicate".into(),
                content: vec![ContentBlock::Text { text: "one".into() }],
                is_error: false,
            },
        ];

        assert!(!is_safe_llm_suffix(&suffix));
    }

    #[test]
    fn rolling_compaction_keeps_at_least_one_recent_message() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!("message {i}"),
                }],
                timestamp: i,
            });
        }
        let last_before = messages.last().cloned();
        let budget = LoopBudget {
            compact_keep_recent_messages: 0,
            compact_min_prefix_messages: 1,
            ..LoopBudget::default()
        };

        let report =
            compact_messages_rolling(&mut messages, &budget, Some(artifact_dir.path()), true);

        assert!(report.compacted);
        assert!(report.kept_messages >= 1);
        assert_eq!(
            messages.last().map(|message| format!("{message:?}")),
            last_before.map(|message| format!("{message:?}"))
        );
    }

    #[test]
    fn repeated_rolling_compaction_preserves_prior_history_ref_in_snapshot() {
        let artifact_dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        for i in 0..10 {
            messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!("first wave {i}"),
                }],
                timestamp: i,
            });
        }
        let budget = LoopBudget {
            compact_keep_recent_messages: 3,
            compact_min_prefix_messages: 2,
            ..LoopBudget::default()
        };

        let first =
            compact_messages_rolling(&mut messages, &budget, Some(artifact_dir.path()), true);
        assert!(first.compacted);
        let first_ref = first.artifact_ref.clone().unwrap();

        for i in 0..8 {
            messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!("second wave {i}"),
                }],
                timestamp: 100 + i,
            });
        }
        let second =
            compact_messages_rolling(&mut messages, &budget, Some(artifact_dir.path()), true);
        assert!(second.compacted);
        assert!(second
            .parent_history_refs
            .iter()
            .any(|value| value == &first_ref));
        assert!(second.anchor_refs.iter().any(|value| value == &first_ref));
        let second_ref = second.artifact_ref.unwrap();

        let page = crate::artifact::read_artifact_page(
            &second_ref,
            Some(artifact_dir.path()),
            0,
            12000,
            None,
        )
        .unwrap();
        assert!(
            page["output"].as_str().unwrap().contains(&first_ref),
            "second compaction snapshot should retain prior history artifact ref"
        );
    }

    #[tokio::test]
    async fn unobserved_progress_channel_does_not_block_tool_execution() {
        use crate::message::{ContentBlock, Usage};
        use crate::provider::{LlmStopReason, ProviderEvent};

        let mut first_events = vec![
            ProviderEvent::MessageStart {
                id: "msg_many_progress_events".into(),
                model: "test".into(),
            },
            ProviderEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::Text {
                    text: String::new(),
                },
            },
        ];
        for _ in 0..70 {
            first_events.push(ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: "x".into(),
            });
        }
        first_events.extend([
            ProviderEvent::ContentBlockStop { index: 0 },
            ProviderEvent::ContentBlockStart {
                index: 1,
                block: ContentBlock::ToolUse {
                    id: "call_one".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text":"one"}),
                },
            },
            ProviderEvent::ContentBlockStop { index: 1 },
            ProviderEvent::ContentBlockStart {
                index: 2,
                block: ContentBlock::ToolUse {
                    id: "call_two".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text":"two"}),
                },
            },
            ProviderEvent::ContentBlockStop { index: 2 },
            ProviderEvent::MessageDelta {
                usage: Usage::default(),
                stop_reason: LlmStopReason::ToolUse,
            },
            ProviderEvent::MessageStop,
        ]);

        let provider = Arc::new(RawEventProvider::new(vec![
            first_events,
            vec![
                ProviderEvent::MessageStart {
                    id: "msg_final".into(),
                    model: "test".into(),
                },
                ProviderEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: "done".into(),
                },
                ProviderEvent::ContentBlockStop { index: 0 },
                ProviderEvent::MessageDelta {
                    usage: Usage::default(),
                    stop_reason: LlmStopReason::EndTurn,
                },
                ProviderEvent::MessageStop,
            ],
        ]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");

        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            loop_.run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "run tools".into(),
                }],
                timestamp: 0,
            }),
        )
        .await
        .expect("progress backpressure should not hang the loop")
        .unwrap();

        assert_eq!(summary.tool_calls_executed, 2);
        assert_eq!(loop_.state.executed_tools.len(), 2);
    }

    #[tokio::test]
    async fn stream_without_message_stop_is_error() {
        use crate::message::{ContentBlock, Usage};
        use crate::provider::{LlmStopReason, ProviderEvent};

        let provider = Arc::new(RawEventProvider::new(vec![vec![
            ProviderEvent::MessageStart {
                id: "msg_partial".into(),
                model: "test".into(),
            },
            ProviderEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::Text {
                    text: String::new(),
                },
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: "partial".into(),
            },
            ProviderEvent::ContentBlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                usage: Usage::default(),
                stop_reason: LlmStopReason::EndTurn,
            },
        ]]));
        let mut loop_ = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "test");

        let error = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: 0,
            })
            .await
            .expect_err("stream ending without message_stop must be an error");

        assert!(
            error.to_string().contains("before message_stop"),
            "unexpected error: {error}"
        );
        assert_eq!(
            loop_.state.messages.len(),
            1,
            "partial assistant output should not be committed"
        );
    }

    #[tokio::test]
    async fn fragmented_tool_input_is_parsed_only_after_the_block_finishes() {
        use crate::message::{ContentBlock, Usage};
        use crate::provider::{LlmStopReason, ProviderEvent};

        let events = vec![
            ProviderEvent::MessageStart {
                id: "msg_test".into(),
                model: "test".into(),
            },
            ProviderEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::ToolUse {
                    id: "call_x".into(),
                    name: "echo".into(),
                    input: serde_json::Value::Object(serde_json::Map::new()), // 初始为空
                },
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: r#"{"te"#.into(),
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: "xt\":\"你".into(),
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: "好\"}".into(),
            },
            ProviderEvent::ContentBlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                usage: Usage::default(),
                stop_reason: LlmStopReason::ToolUse,
            },
            ProviderEvent::MessageStop,
        ];

        let provider = Arc::new(RawEventProvider::new(vec![
            events,
            // 第二次 stream() 调用：tool_result 回喂后，模型返回纯文本结束
            vec![
                ProviderEvent::MessageStart {
                    id: "msg_final".into(),
                    model: "test".into(),
                },
                ProviderEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: "echoed".into(),
                },
                ProviderEvent::ContentBlockStop { index: 0 },
                ProviderEvent::MessageDelta {
                    usage: Usage::default(),
                    stop_reason: LlmStopReason::EndTurn,
                },
                ProviderEvent::MessageStop,
            ],
        ]));
        let tools = vec![ToolKind::Echo(EchoTool)];
        let mut loop_ = AgentLoop::new(provider, tools, "test");

        let _ = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "echo 你好".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();

        // 找到 tool_call 那条 assistant 消息
        let assistant_msg = loop_
            .state
            .messages
            .iter()
            .find(|m| matches!(m, AgentMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
            .expect("应有一条带 tool_call 的 assistant 消息");

        if let AgentMessage::Assistant { tool_calls, .. } = assistant_msg {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "echo");
            assert_eq!(
                tool_calls[0].input,
                serde_json::json!({"text": "你好"}),
                "tool_use input 必须从 delta 解析出来，不能是空的 {{}}"
            );
        }

        // 也要看 tool_result 拿到了正确的回显
        let tool_result = loop_
            .state
            .messages
            .iter()
            .find_map(|m| match m {
                AgentMessage::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .expect("应有 tool_result 消息");
        match &tool_result[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[echo] 你好"),
            _ => panic!("expected text content block"),
        }
    }

    #[tokio::test]
    async fn malformed_tool_input_never_executes_with_empty_default_arguments() {
        use crate::message::{ContentBlock, Usage};
        use crate::provider::{LlmStopReason, ProviderEvent};

        let events = vec![
            ProviderEvent::MessageStart {
                id: "msg_test".into(),
                model: "test".into(),
            },
            ProviderEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::ToolUse {
                    id: "call_x".into(),
                    name: "echo".into(),
                    input: serde_json::Value::Object(serde_json::Map::new()),
                },
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: "{not json}".into(),
            },
            ProviderEvent::ContentBlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                usage: Usage::default(),
                stop_reason: LlmStopReason::ToolUse,
            },
            ProviderEvent::MessageStop,
        ];

        let provider = Arc::new(RawEventProvider::new(vec![
            events,
            // 第二轮 stream()：tool_result 回喂后纯文本结束
            vec![
                ProviderEvent::MessageStart {
                    id: "msg_final".into(),
                    model: "test".into(),
                },
                ProviderEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: "done".into(),
                },
                ProviderEvent::ContentBlockStop { index: 0 },
                ProviderEvent::MessageDelta {
                    usage: Usage::default(),
                    stop_reason: LlmStopReason::EndTurn,
                },
                ProviderEvent::MessageStop,
            ],
        ]));
        let tools = vec![ToolKind::Echo(EchoTool)];
        let mut loop_ = AgentLoop::new(provider, tools, "test");

        let error = loop_
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "echo".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("invalid tool input JSON"));
        assert_eq!(
            loop_.state.messages.len(),
            1,
            "no malformed tool call may be committed or executed"
        );
    }
    #[tokio::test]
    async fn unknown_tool_and_execution_error_are_returned_to_model() {
        use crate::provider::{MockProvider, MockResponse};
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "not_a_tool".into(),
                input: serde_json::json!({}),
            },
            MockResponse::ToolCall {
                name: "div".into(),
                input: serde_json::json!({"a":10,"b":0}),
            },
            MockResponse::ToolCall {
                name: "div".into(),
                input: serde_json::json!({"a":10,"b":2}),
            },
            MockResponse::AssistantText {
                text: "3000".into(),
            },
        ]));
        let mut agent = AgentLoop::new(provider, vec![ToolKind::Div(crate::DivTool)], "test");
        let summary = agent
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "test".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();
        assert_eq!(summary.tool_calls_executed, 3);
        let results = &agent.state.executed_tools;
        assert_eq!(
            results
                .iter()
                .map(|result| result.is_error)
                .collect::<Vec<_>>(),
            vec![true, true, false]
        );
        for (result, kind) in results
            .iter()
            .zip(["tool_not_registered", "execution_error"])
        {
            let ContentBlock::Text { text } = &result.content[0] else {
                panic!("expected text")
            };
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text).unwrap()["error"]["kind"],
                kind
            );
        }
    }

    struct CancellationProbe(mpsc::UnboundedSender<ToolContext>);

    #[async_trait::async_trait]
    impl crate::tool::ToolImpl for CancellationProbe {
        fn spec(&self) -> crate::ToolSpec {
            crate::ToolSpec {
                name: "cancellation_probe".into(),
                label: "Cancellation probe".into(),
                description: "Wait for cancellation".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }

        async fn execute(
            &self,
            _: serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ToolResult, crate::tool::ToolError> {
            self.0.send(ctx.clone()).unwrap();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn dropped_turn_cancels_tool_tree_and_restores_reusable_parent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "cancellation_probe".into(),
                input: serde_json::json!({}),
            },
            MockResponse::AssistantText {
                text: "next".into(),
            },
        ]));
        let mut agent = AgentLoop::new(
            provider,
            vec![ToolKind::External(crate::tool::ExternalTool::new(
                Arc::new(CancellationProbe(tx)),
            ))],
            "test",
        );
        let parent = agent.tool_ctx.cancellation.clone();
        let ctx = {
            let turn = agent.run_one_turn(steering_user("first"));
            tokio::pin!(turn);
            tokio::select! {
                result = &mut turn => panic!("probe must wait: {result:?}"),
                ctx = rx.recv() => ctx.unwrap(),
            }
        };
        let descendant = ctx.cancellation.child_token();
        assert!(descendant.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(!agent.tool_ctx.cancellation.is_cancelled());
        assert!(Arc::ptr_eq(
            ctx.monad_runtime.as_ref().unwrap(),
            agent.tool_ctx.monad_runtime.as_ref().unwrap(),
        ));
        agent.recover_interrupted("test dropped turn");
        agent.run_one_turn(steering_user("second")).await.unwrap();
        assert!(!parent.is_cancelled());
        assert!(!agent.tool_ctx.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn context_cancellation_interrupts_running_tool() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = AgentLoop::new(
            Arc::new(MockProvider::new(vec![MockResponse::ToolCall {
                name: "cancellation_probe".into(),
                input: serde_json::json!({}),
            }])),
            vec![ToolKind::External(crate::tool::ExternalTool::new(
                Arc::new(CancellationProbe(tx)),
            ))],
            "test",
        );
        let parent = agent.tool_ctx.cancellation.clone();
        let control = async {
            let ctx = rx.recv().await.unwrap();
            assert!(!ctx.cancellation.is_cancelled());
            parent.cancel();
            ctx
        };
        let (result, ctx) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(agent.run_one_turn(steering_user("first")), control)
        })
        .await
        .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cancelled during tool"));
        assert!(agent.state.cancelled);
        assert!(ctx.cancellation.is_cancelled());
    }

    struct CancellationWaitProvider(tokio::sync::Notify);

    #[async_trait::async_trait]
    impl Provider for CancellationWaitProvider {
        fn id(&self) -> &str {
            "cancellation-wait"
        }
        fn model(&self) -> &str {
            "test"
        }
        fn context_window(&self) -> usize {
            8000
        }
        fn cache_key(&self, _: &ProviderRequest) -> String {
            "test".into()
        }
        fn abort_signal(&self) -> crate::CancellationToken {
            crate::CancellationToken::new()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = ProviderEvent> + Send>>,
            crate::ProviderError,
        > {
            self.0.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn context_cancellation_interrupts_provider_connection() {
        let provider = Arc::new(CancellationWaitProvider(tokio::sync::Notify::new()));
        let mut agent = AgentLoop::new(provider.clone(), vec![], "test");
        let parent = agent.tool_ctx.cancellation.clone();
        let control = async {
            provider.0.notified().await;
            parent.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(agent.run_one_turn(steering_user("first")), control)
        })
        .await
        .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(agent.state.cancelled);
    }

    #[tokio::test]
    async fn explicit_owner_cleanup_excludes_shared_services_and_other_tasks() {
        use crate::execution::smallhead_process::{ProcessManager, ProcessSpec};
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            crate::execution::ExecutionRuntime::new(
                ProcessManager::with_log_root(dir.path().into()).unwrap(),
            )
            .unwrap(),
        );
        let owned = runtime
            .processes
            .start_owned(ProcessSpec::new("sleep 30"), Some("turn".into()))
            .await
            .unwrap();
        let service = runtime
            .processes
            .start_owned(ProcessSpec::new("sleep 30"), Some("service:shared".into()))
            .await
            .unwrap();
        let sibling = runtime
            .processes
            .start_owned(ProcessSpec::new("sleep 30"), Some("sibling".into()))
            .await
            .unwrap();
        let mut agent = AgentLoop::new(
            Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
                text: "done".into(),
            }])),
            vec![],
            "test",
        );
        agent.tool_ctx.execution = Some(runtime.clone());
        agent.tool_ctx.owner = Some("turn".into());
        agent.run_one_turn(steering_user("first")).await.unwrap();
        let owned = runtime.processes.inspect(owned).unwrap();
        assert!(
            matches!(
                owned.state,
                crate::execution::smallhead_process::ProcessState::Exited(_)
            ),
            "a completed turn must have reaped its owned process: {:?}",
            owned.state
        );
        for id in [service, sibling] {
            let record = runtime.processes.inspect(id).unwrap();
            assert!(!record.is_terminal());
            assert!(!matches!(
                record.state,
                crate::execution::smallhead_process::ProcessState::Stopping
            ));
        }
        runtime
            .processes
            .shutdown(std::time::Duration::from_secs(6))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn parent_final_guard_rejects_unfinished_child_at_wave_limit() {
        let parent = crate::monad::TaskId::new();
        let mut agent = AgentLoop::new(
            Arc::new(MockProvider::new(vec![
                MockResponse::AssistantText {
                    text: "premature".into(),
                },
                MockResponse::AssistantText {
                    text: "still premature".into(),
                },
            ])),
            vec![],
            "test",
        );
        agent.tool_ctx.owner = Some(parent.0.clone());
        let channel = agent
            .tool_ctx
            .monad_collaboration
            .register(parent, crate::monad::TaskId::new(), 1)
            .unwrap();
        agent.budget.max_model_waves = 2;
        let result = agent.run_one_turn(steering_user("parent")).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("outstanding Monad children"));
        assert!(agent.state.messages.iter().any(|message| matches!(message,
            AgentMessage::User { content, .. } if content.iter().any(|block|
                matches!(block, ContentBlock::Text { text } if text.contains("monad_delegate status/wait")))
        )));
        channel.finish();
    }

    struct ReportWakeProvider {
        requests: std::sync::Mutex<Vec<ProviderRequest>>,
        partial_ready: Arc<tokio::sync::Notify>,
        late_report: Option<crate::monad::ChildChannel>,
    }

    #[async_trait::async_trait]
    impl Provider for ReportWakeProvider {
        fn id(&self) -> &str {
            "report-wake"
        }
        fn model(&self) -> &str {
            "test"
        }
        fn context_window(&self) -> usize {
            8000
        }
        fn cache_key(&self, _: &ProviderRequest) -> String {
            "test".into()
        }
        fn abort_signal(&self) -> crate::CancellationToken {
            crate::CancellationToken::new()
        }

        async fn stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = ProviderEvent> + Send>>,
            crate::ProviderError,
        > {
            let first = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len() == 1
            };
            let ready = self.partial_ready.clone();
            let late_report = self.late_report.clone();
            Ok(Box::pin(async_stream::stream! {
                yield ProviderEvent::ContentBlockStart {
                    index: 0, block: ContentBlock::Text { text: String::new() },
                };
                yield ProviderEvent::ContentBlockDelta {
                    index: 0, delta: if first { "discarded partial" } else { "report received" }.into(),
                };
                if first {
                    if let Some(channel) = late_report {
                        channel.report(crate::monad::ReportKind::Finding, "last poll evidence", None).unwrap();
                        channel.finish();
                    } else {
                        ready.notify_one();
                        std::future::pending::<()>().await;
                    }
                }
                yield ProviderEvent::ContentBlockStop { index: 0 };
                yield ProviderEvent::MessageStop;
            }))
        }
    }

    #[tokio::test]
    async fn parent_report_wakes_provider_and_discards_partial_without_duplicate_events() {
        let provider = Arc::new(ReportWakeProvider {
            requests: std::sync::Mutex::new(vec![]),
            partial_ready: Arc::new(tokio::sync::Notify::new()),
            late_report: None,
        });
        let mut agent = AgentLoop::new(provider.clone(), vec![], "test");
        let parent = crate::monad::TaskId::new();
        agent.tool_ctx.owner = Some(parent.0.clone());
        let channel = agent
            .tool_ctx
            .monad_collaboration
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        let control = async {
            provider.partial_ready.notified().await;
            channel
                .report(crate::monad::ReportKind::Finding, "wake evidence", None)
                .unwrap();
            channel.finish();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(agent.run_one_turn(steering_user("parent")), control)
        })
        .await
        .unwrap();
        result.unwrap();
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let context = serde_json::to_string(&requests[1].messages).unwrap();
        assert!(context.contains("wake evidence"));
        assert!(context.contains("不是用户/系统新授权"));
        assert!(!context.contains("discarded partial"));
        let history = serde_json::to_string(&agent.state.messages).unwrap();
        assert!(!history.contains("discarded partial"));
        assert_eq!(history.matches("wake evidence").count(), 1);
        assert!(!agent.harvest_monad_events());
        assert_eq!(
            agent.monad_event_cursor,
            agent.tool_ctx.monad_collaboration.cursor(&parent)
        );
    }

    #[tokio::test]
    async fn parent_rechecks_report_generated_by_providers_final_poll() {
        let hub = Arc::new(crate::monad::CollaborationHub::default());
        let parent = crate::monad::TaskId::new();
        let channel = hub
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        let provider = Arc::new(ReportWakeProvider {
            requests: std::sync::Mutex::new(vec![]),
            partial_ready: Arc::new(tokio::sync::Notify::new()),
            late_report: Some(channel),
        });
        let mut agent = AgentLoop::new(provider.clone(), vec![], "test");
        agent.tool_ctx.owner = Some(parent.0);
        agent.tool_ctx.monad_collaboration = hub;
        agent.run_one_turn(steering_user("parent")).await.unwrap();
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains("last poll evidence"));
        assert!(!serde_json::to_string(&agent.state.messages)
            .unwrap()
            .contains("discarded partial"));
    }

    #[tokio::test]
    async fn parent_harvest_does_not_repeat_events_on_empty_read() {
        let mut agent = AgentLoop::new(Arc::new(MockProvider::new(vec![])), vec![], "test");
        let parent = crate::monad::TaskId::new();
        agent.tool_ctx.owner = Some(parent.0.clone());
        let child = agent
            .tool_ctx
            .monad_collaboration
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        child
            .report(crate::monad::ReportKind::Finding, "once", None)
            .unwrap();
        assert!(agent.harvest_monad_events());
        // A newly registered child has no report to inject into the context.
        let another = agent
            .tool_ctx
            .monad_collaboration
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        let before = agent.state.messages.len();
        assert!(!agent.harvest_monad_events());
        assert_eq!(agent.state.messages.len(), before);
        assert_eq!(
            agent.monad_event_cursor,
            agent.tool_ctx.monad_collaboration.cursor(&parent)
        );
        child.finish();
        another.finish();
    }

    #[tokio::test]
    async fn parent_harvest_pages_without_skipping_or_repeating_unread_events() {
        let mut agent = AgentLoop::new(Arc::new(MockProvider::new(vec![])), vec![], "test");
        let parent = crate::monad::TaskId::new();
        agent.tool_ctx.owner = Some(parent.0.clone());
        let channel = agent
            .tool_ctx
            .monad_collaboration
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        for index in 0..33 {
            channel
                .report(
                    crate::monad::ReportKind::Finding,
                    format!("page report {index}"),
                    None,
                )
                .unwrap();
        }
        assert!(agent.harvest_monad_events());
        assert_eq!(agent.state.messages.len(), 32);
        assert!(agent.monad_event_cursor < agent.tool_ctx.monad_collaboration.cursor(&parent));
        assert!(agent.harvest_monad_events());
        assert_eq!(agent.state.messages.len(), 33);
        assert_eq!(
            agent.monad_event_cursor,
            agent.tool_ctx.monad_collaboration.cursor(&parent)
        );
        assert!(!agent.harvest_monad_events());
        for (index, message) in agent.state.messages.iter().enumerate() {
            let AgentMessage::User { content, .. } = message else {
                panic!("expected event")
            };
            let ContentBlock::Text { text } = &content[0] else {
                panic!("expected event text")
            };
            let event: serde_json::Value =
                serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
            assert_eq!(event["text"], format!("page report {index}"));
        }
        channel.finish();
    }

    struct WaveReportTool {
        channel: crate::monad::ChildChannel,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::tool::ToolImpl for WaveReportTool {
        fn spec(&self) -> crate::ToolSpec {
            crate::ToolSpec {
                name: "wave_report".into(),
                label: "Report".into(),
                description: "Publish child evidence during a tool wave".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }

        async fn execute(
            &self,
            _: serde_json::Value,
            _: &ToolContext,
        ) -> Result<ToolResult, crate::tool::ToolError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.channel
                .report(
                    crate::monad::ReportKind::Finding,
                    format!("tool evidence {call}"),
                    None,
                )
                .unwrap();
            if call == 1 {
                self.channel.finish();
            }
            Ok(ToolResult {
                tool_call_id: String::new(),
                is_error: false,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
            })
        }
    }

    #[tokio::test]
    async fn parent_harvest_waits_for_all_tool_results_without_replaying_tools() {
        let hub = Arc::new(crate::monad::CollaborationHub::default());
        let parent = crate::monad::TaskId::new();
        let channel = hub
            .register(parent.clone(), crate::monad::TaskId::new(), 1)
            .unwrap();
        let tool = Arc::new(WaveReportTool {
            channel,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut agent = AgentLoop::new(
            Arc::new(RawEventProvider::new(vec![
                tool_wave(vec![
                    ("first", "wave_report", serde_json::json!({})),
                    ("second", "wave_report", serde_json::json!({})),
                ]),
                vec![
                    ProviderEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlock::Text {
                            text: "finished".into(),
                        },
                    },
                    ProviderEvent::ContentBlockStop { index: 0 },
                    ProviderEvent::MessageStop,
                ],
            ])),
            vec![ToolKind::External(crate::tool::ExternalTool::new(
                tool.clone(),
            ))],
            "test",
        );
        agent.tool_ctx.owner = Some(parent.0);
        agent.tool_ctx.monad_collaboration = hub;
        let summary = agent.run_one_turn(steering_user("parent")).await.unwrap();
        assert_eq!(summary.tool_calls_executed, 2);
        assert_eq!(tool.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            matches!(&agent.state.messages[1], AgentMessage::Assistant { tool_calls, .. } if tool_calls.len() == 2)
        );
        assert!(
            matches!(&agent.state.messages[2], AgentMessage::ToolResult { tool_call_id, .. } if tool_call_id == "first")
        );
        assert!(
            matches!(&agent.state.messages[3], AgentMessage::ToolResult { tool_call_id, .. } if tool_call_id == "second")
        );
        assert!(
            matches!(&agent.state.messages[4], AgentMessage::User { content, .. }
            if matches!(&content[0], ContentBlock::Text { text } if text.contains("tool evidence 0")))
        );
    }

    #[tokio::test]
    async fn parent_final_guard_includes_runtime_children_without_hub_channel() {
        use crate::monad::{AgentMonad, BindingValidator, MonadId, MonadRuntime};
        let runtime = Arc::new(MonadRuntime::default());
        let parent = crate::monad::TaskId::new();
        let mut handoff = crate::MonadHandoff {
            schema_version: crate::SCHEMA_VERSION_V1.into(),
            task_id: crate::TaskId::new(),
            parent_task_id: Some(parent.clone()),
            monad_id: MonadId::new("pending-child"),
            attempt_id: 1,
            objective: "wait".into(),
            inputs: Default::default(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "wait".into(),
            result_slot: crate::ResultSlot("child.pending".into()),
            depends_on: vec![],
            budget: crate::Budget::default(),
            issued_at_ms: 0,
            trace: crate::TraceMeta {
                frame_ref: "test".into(),
                request_id: "test".into(),
            },
        };
        handoff.budget.timeout_ms = 30_000;
        let child = AgentMonad::new(
            handoff.monad_id.clone(),
            "pending",
            Arc::new(CancellationWaitProvider(tokio::sync::Notify::new())),
            vec![],
            "test",
            ToolContext::default(),
        );
        let cancel = crate::CancellationToken::new();
        runtime
            .submit(
                Box::new(child),
                handoff,
                cancel.clone(),
                Arc::new(BindingValidator),
            )
            .unwrap();
        let mut agent = AgentLoop::new(
            Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
                text: "premature".into(),
            }])),
            vec![],
            "test",
        );
        agent.tool_ctx.owner = Some(parent.0);
        agent.tool_ctx.monad_runtime = Some(runtime.clone());
        agent.budget.max_model_waves = 1;
        let result = agent.run_one_turn(steering_user("parent")).await;
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), runtime.wait_idle())
            .await
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("outstanding Monad children"));
    }
}
