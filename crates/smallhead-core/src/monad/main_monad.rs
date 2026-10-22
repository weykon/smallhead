//! 顶层 `MainMonad` 适配器 —— 把现有 [`crate::AgentLoop`] 包成 [`MonadLoop`] 实现。
//!
//! 对应 `docs/monad-orchestration-pattern.md` §6.1 P1 阶段改造路径。
//!
//! 持有:
//! - `inner`:现成的 [`crate::AgentLoop`](提供 `run_one_turn_with_progress`)
//! - `frame`:MainMonad 自己的消息 buffer(从 `handoff.objective` 初始化)
//! - `current_task`:`step()` 把 [`crate::agent_loop::LoopSummary`] 翻译成 [`MonadResult::Completed`] 时,
//!   需要回填的 `task_id` / `parent_task_id` / `result_slot`(由 `start()` 注入)
//!
//! `Option<AgentLoop>` 是为了让测试能构造空实例 —— 真实使用时 `start()` 接管 handoff
//! 后由 AgentLoop 内部 state 主导 message 流。
//!
//! 本文件负责主循环适配；本地示例业务位于 `fixtures.rs`。

use crate::message::LlmMessage;
use crate::monad::loop_trait::{main_monad_id, CatalogEntry, MonadError, MonadLoop};
use crate::monad::protocol::{MonadHandoff, MonadId, MonadResult};
use crate::monad::task::TaskSlot;
use crate::tool::ToolKind;
use async_trait::async_trait;
use std::time::{SystemTime, UNIX_EPOCH};

/// 顶层 MainMonad —— 把现有 [`crate::AgentLoop`] 包成 MonadLoop 实现。
///
/// 见 `docs/monad-orchestration-pattern.md` §6.1 P1 阶段改造路径。
///
/// 持有:
/// - `inner`:现成的 [`crate::AgentLoop`](提供 `run_one_turn_with_progress`)
/// - `frame`:MainMonad 自己的消息 buffer(从 `handoff.objective` 初始化)
/// - `current_task`:`step()` 把 [`crate::agent_loop::LoopSummary`] 翻译成 [`MonadResult::Completed`] 时,
///   需要回填的 `task_id` / `parent_task_id` / `result_slot`(由 `start()` 注入)
///
/// `Option<AgentLoop>` 是为了让测试能构造空实例 —— 真实使用时 `start()` 接管 handoff
/// 后由 AgentLoop 内部 state 主导 message 流。
pub struct MainMonad {
    /// 底层 AgentLoop。`None` 表示未初始化(`MainMonad::empty()` 用于测试 / 未接入场景)。
    pub inner: Option<crate::AgentLoop>,
    /// MainMonad 自己的消息 buffer —— 从 handoff.objective 初始化。
    pub frame: Vec<LlmMessage>,
    /// 是否已 start 过(避免重复 push objective)。
    pub started: bool,
    /// 当前 task 绑定(由 [`TaskSlot`] 守门)。
    /// `is_bound()` == false 时表示没有可执行任务。
    current_task: TaskSlot,
}

impl MainMonad {
    /// 构造一个空的 MainMonad(未持有 AgentLoop)。
    ///
    /// 真实场景由 Scheduler 通过 `MainMonad::wrap(agent_loop)` 包装已构造的 AgentLoop。
    pub fn empty() -> Self {
        Self {
            inner: None,
            frame: Vec::new(),
            started: false,
            current_task: TaskSlot::new(),
        }
    }

    /// 用一个已构造的 AgentLoop 包成 MainMonad。
    pub fn wrap(agent_loop: crate::AgentLoop) -> Self {
        Self {
            inner: Some(agent_loop),
            frame: Vec::new(),
            started: false,
            current_task: TaskSlot::new(),
        }
    }
}

impl Default for MainMonad {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for MainMonad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainMonad")
            .field("inner", &self.inner.is_some())
            .field("frame_len", &self.frame.len())
            .field("started", &self.started)
            .field("current_task", &self.current_task)
            .finish()
    }
}

#[async_trait]
impl MonadLoop for MainMonad {
    fn monad_id(&self) -> &MonadId {
        main_monad_id()
    }

    fn catalog_entry(&self) -> &CatalogEntry {
        // 顶层对话循环:接收用户原话并产出最终回复。
        // 短摘要包含"对话"以匹配测试断言。
        static ENTRY: std::sync::OnceLock<CatalogEntry> = std::sync::OnceLock::new();
        ENTRY.get_or_init(|| CatalogEntry {
            summary: "顶层对话循环,接收用户原话并产出最终回复".to_string(),
            visible_tools: vec![
                "echo".into(),
                "add".into(),
                "sub".into(),
                "mul".into(),
                "div".into(),
                "calculate".into(),
                "bash".into(),
                "process_control".into(),
                "read".into(),
                "artifact_read".into(),
                "edit".into(),
                "write".into(),
            ],
            keywords: vec!["对话".into(), "顶层".into(), "chat".into(), "main".into()],
        })
    }

    fn default_tool_set(&self) -> Vec<ToolKind> {
        // 顶层对话默认看全部工具。
        // 故意 hardcode 完整列表,等 P2 ToolGate 出现后再改成"全量 + Gate 过滤"。
        vec![
            ToolKind::Echo(crate::EchoTool),
            ToolKind::Add(crate::AddTool),
            ToolKind::Sub(crate::SubTool),
            ToolKind::Mul(crate::MulTool),
            ToolKind::Div(crate::DivTool),
            ToolKind::Calculate(crate::CalculateTool),
            ToolKind::Bash(crate::BashTool),
            ToolKind::ProcessControl(crate::execution::ProcessTool),
            ToolKind::Read(crate::ReadTool),
            ToolKind::ArtifactRead(crate::ArtifactReadTool),
            ToolKind::Edit(crate::EditTool),
            ToolKind::Write(crate::WriteTool),
        ]
    }

    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        // 业务输入准备放在 try_bind 之前(局部变量);只有 try_bind 成功后才写回 self。
        // 这样 handoff 校验失败时实例保持完全未绑状态。
        let seed = LlmMessage::User {
            content: vec![crate::message::ContentBlock::Text {
                text: handoff.objective.clone(),
            }],
        };
        // expected 提到 self 借用外,避免 self.task 与 self.monad_id() 借用冲突。
        let expected = self.monad_id().clone();
        self.current_task.try_bind(&handoff, &expected)?;
        // 从这里开始,binding 已写入;中途 panic 只会留下"绑了但 frame 仍空"的状态,
        // 下次 cancel() 会清理 —— 比"半写"更安全。
        self.frame.clear();
        self.frame.push(seed);
        self.started = true;
        Ok(())
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        // 真桥接到 AgentLoop:把 frame 顶部的 user 消息喂给 inner.run_one_turn,
        // 把 LoopSummary + 最终 assistant 文本翻译成 MonadResult::Completed。
        //
        // 见 docs/monad-orchestration-pattern.md §6.1。

        // 0. 必须有 wrap 过的 AgentLoop
        let loop_ = self.inner.as_mut().ok_or_else(|| {
            MonadError::new(
                "MainMonad::step() requires inner AgentLoop (use MainMonad::wrap(agent_loop))",
            )
        })?;

        // 1. 必须已 start 过(才有 user 消息可推)
        if !self.started {
            return Err(MonadError::new(
                "MainMonad::step() called before start(handoff)",
            ));
        }

        // 用 take() 单次消费 binding —— 跟 fixture 的 `self.task.take()` 对称,
        // 保证 step() 不会用同一个 task_id 出两次 MonadResult::Completed。
        let binding = self.current_task.take()?;

        // 2. 取 frame[0] 作为本轮 user 消息 —— 单 turn MVP,只取首条。
        let user_llm = match self.frame.first() {
            Some(LlmMessage::User { content }) => content.clone(),
            Some(_) => {
                // 状态已消费但业务上拿不到 user 消息 —— 清掉本轮残留再返回。
                self.frame.clear();
                self.started = false;
                return Err(MonadError::new(
                    "MainMonad::step() expects frame[0] to be LlmMessage::User",
                ));
            }
            None => {
                self.frame.clear();
                self.started = false;
                return Err(MonadError::new(
                    "MainMonad::step() called with empty frame (did start() run?)",
                ));
            }
        };

        // 3. 转 LlmMessage::User → AgentMessage::User(补 timestamp)
        let user_msg = crate::message::AgentMessage::User {
            content: user_llm,
            timestamp: now_ms(),
        };

        // 4. 委托 AgentLoop 跑一轮;失败时也要清空本轮 frame + started,
        //    这样调用方下次 start() 不会被旧消息或旧 started 标记卡住。
        let summary = match loop_.run_one_turn(user_msg).await {
            Ok(s) => s,
            Err(e) => {
                let err = MonadError::new(format!("AgentLoop::run_one_turn failed: {e}"));
                self.frame.clear();
                self.started = false;
                return Err(err);
            }
        };

        // 5. 抽最终 assistant 文本(MVP:取 messages 中最后一条 Assistant 的 text 拼接)
        let final_text = extract_final_assistant_text(&loop_.state.messages);

        // 6. 构造 output JSON —— 同时含计数与最终回复,parent 父任务不用再追原始流。
        let output = serde_json::json!({
            "turns_completed": summary.turns_completed,
            "tool_calls_executed": summary.tool_calls_executed,
            "final_text": final_text,
        });

        // 7. 回填 handoff 标识 + 资源用量 + 时间戳。
        let usage = TaskSlot::usage_after(&binding, summary.turns_completed as u32, 0);
        let finished_at_ms = now_ms();
        let (task_id, parent_task_id, result_slot) = binding.into_parts();

        // 8. 单 turn 完成 —— 清空 frame 和 started 标记。
        // current_task 已经在上面 take() 时置 None,这里只重置 frame & started。
        self.frame.clear();
        self.started = false;

        Ok(MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            monad_id: main_monad_id().clone(),
            output,
            evidence_refs: Vec::new(),
            usage,
            finished_at_ms,
        })
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        let had_task = self.current_task.is_bound();
        self.current_task.clear();
        self.frame.clear();
        self.started = false;
        // 无任务或重复取消不发送通知。通知是 best-effort，不能等待满队列。
        if !had_task {
            return Ok(());
        }
        // AgentLoop MVP 暂未消费取消通道；Full 表示已有通知待处理。
        // 这里只清理本地状态，不承诺中断已经提交的外部工作。
        if let Some(loop_) = self.inner.as_ref() {
            match loop_.cancel_sender().try_send(()) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Err(MonadError::new("MainMonad::cancel: cancel channel closed"))
                }
            }
        } else {
            // 没有 inner 的空实例 —— 取消是 no-op,直接 Ok。
            Ok(())
        }
    }
}

/// 从 AgentLoop 的消息历史中抽出"最后一条 Assistant 消息的纯文本"。
///
/// 输入:`&[AgentMessage]`(通常来自 `loop.state.messages`),只关心 `Assistant` variant。
/// 输出:把所有 `ContentBlock::Text` 按出现顺序拼接;非 text block 跳过。
///
/// MVP:不解析 thinking / tool_use;若需要 reasoning trace 留给 P1 单独暴露。
fn extract_final_assistant_text(messages: &[crate::message::AgentMessage]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            crate::message::AgentMessage::Assistant { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        crate::message::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// 取得 epoch 毫秒，仅供 monad 模块内的主循环和本地业务共享。
pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
