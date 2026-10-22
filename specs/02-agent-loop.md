# 02 — Agent Loop

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-core/src/agent_loop.rs`
> 上游: `ARCHITECTURE.md` §5, `AGENT_METHODOLOGY.md` §1.1

---

## 1. 概述

`AgentLoop` 是 smallhead 的核心——一个 **nested 2-while loop**，把 LLM 响应、tool 执行、用户 steering、follow-up 队列串成一个收敛的状态机。

模型来源是 pi-mono 的 `agent-loop.ts`（`packages/agent/src/agent-loop.ts:117–195`），关键洞察是：

- **OUTER while** 处理 **follow-up 队列**：等当前 turn 完成、工具全部执行完后，再处理用户排队的下一条消息。
- **INNER while** 处理 **tool + steering**：在每个 tool 调用前检查 steering channel，让用户能在 agent 跑一半时打断。

smallhead 把这套结构用 Rust 的 `async` + `tokio::sync::mpsc` 表达，并借鉴 agent-deck-rs 的 **Pure Pipeline 模式**：核心决策 `step(state, event)` 是纯函数，I/O 由 executor 异步完成。这样 `step()` 测试不需要 mock 文件系统或 LLM。

---

## 2. 主流程伪代码

```rust
// smallhead-core/src/agent_loop.rs
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;

pub struct AgentLoop {
    state: Arc<Mutex<LoopState>>,
    provider: Arc<dyn Provider>,
    tools: Vec<ToolKind>,
    ruleset: Ruleset,
    session: Arc<SessionStore>,
    world: Arc<Mutex<World>>,
    doom_loop: DoomLoopDetector,
    cancel: CancellationToken,
    steering_tx: mpsc::Sender<AgentMessage>,
    steering_rx: mpsc::Receiver<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    follow_up_rx: mpsc::Receiver<AgentMessage>,
}

impl AgentLoop {
    /// 顶层入口：运行直到所有 follow-up 处理完或被取消
    pub async fn run(mut self) -> Result<()> {
        // OUTER while: 处理 follow-up 队列
        loop {
            self.run_one_turn().await?;

            // 检查 follow-up 队列
            let follow_ups = self.drain_follow_ups().await;
            if follow_ups.is_empty() {
                break;
            }
            for msg in follow_ups {
                self.commit_message(msg).await?;
            }
        }
        Ok(())
    }

    async fn run_one_turn(&mut self) -> Result<()> {
        // INNER while: 处理 tool + steering
        loop {
            self.cancel.check()?;

            // 1. 转换 context: transformContext (见 specs/04 §compaction)
            let ctx_msgs = self.transform_context().await?;

            // 2. 转换到 LLM: convertToLlm —— 严格过滤
            let llm_msgs: Vec<LlmMessage> =
                ctx_msgs.iter().filter_map(|m| m.to_llm()).collect();

            // 3. Token budget check (见 §4)
            self.check_token_budget(&llm_msgs).await?;

            // 4. Context Epoch tracking (见 §5)
            let epoch_id = self.begin_epoch(&llm_msgs).await?;

            // 5. 调用 provider
            let request = ProviderRequest {
                messages: llm_msgs,
                tools: self.tools.iter().map(|t| t.spec()).collect(),
                ..self.default_request()
            };
            let stream = self.provider.stream(request, self.cancel.clone()).await?;

            // 6. 收集 assistant 消息 + tool calls
            let assistant = self.collect_assistant(stream).await?;
            self.commit_message(AgentMessage::Assistant {
                content: assistant.content.clone(),
                tool_calls: assistant.tool_calls.clone(),
                model: self.provider.model().to_string(),
            }).await?;

            // 7. 如果没有 tool calls → INNER 结束
            if assistant.tool_calls.is_empty() {
                return Ok(());
            }

            // 8. 执行工具（中间可被 steering 中断）
            for tc in &assistant.tool_calls {
                // 8a. 检查 steering channel
                if let Some(steer) = self.try_recv_steering().await {
                    self.commit_message(steer).await?;
                    return Ok(());  // INNER 结束 → OUTER 看 follow-up
                }

                // 8b. DoomLoop 检查（先于 permission，避免误判）
                if self.doom_loop.would_trigger(tc) {
                    self.handle_doom_loop(tc).await?;
                }

                // 8c. Permission 决策
                let decision = self.ruleset.evaluate(
                    &tool_name_to_permission(&tc.name),
                    &tc.input,
                    &self.eval_context(),
                );
                if !matches!(decision.action, Action::Allow) {
                    self.handle_permission_decision(tc, &decision).await?;
                    if matches!(decision.action, Action::Deny) {
                        continue;  // 跳过这个 tool call
                    }
                }

                // 8d. 执行
                let result = self.execute_tool(tc).await?;

                // 8e. DoomLoop 记录
                self.doom_loop.record(tc);

                // 8f. 提交 tool result
                self.commit_message(AgentMessage::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: result.content,
                    is_error: result.is_error,
                }).await?;
            }

            // 9. 检查 follow-up（如果有就退出 INNER，让 OUTER 处理）
            if !self.follow_up_rx.is_empty() {
                return Ok(());
            }
        }
    }

    // ── Steering / Follow-up 入口 ──────────────────────────────

    pub async fn steer(&self, msg: AgentMessage) -> Result<()> {
        self.steering_tx.send(msg).await?;
        Ok(())
    }

    pub async fn follow_up(&self, msg: AgentMessage) -> Result<()> {
        self.follow_up_tx.send(msg).await?;
        Ok(())
    }

    async fn try_recv_steering(&mut self) -> Option<AgentMessage> {
        self.steering_rx.try_recv().ok()
    }

    async fn drain_follow_ups(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = self.follow_up_rx.try_recv() {
            out.push(msg);
        }
        out
    }
}
```

---

## 3. Pure Pipeline 模式（来自 agent-deck-rs）

`AgentLoop::run` 的决策核心应该是 **pure function**——便于测试、重放、调试：

```rust
// smallhead-core/src/loop_state.rs

#[derive(Debug, Clone)]
pub struct LoopState {
    pub messages: Vec<AgentMessage>,
    pub pending_tools: Vec<ToolCall>,
    pub follow_ups: VecDeque<AgentMessage>,
    pub steerings: VecDeque<AgentMessage>,
    pub epoch_id: u64,
    pub token_used: usize,
    pub doom_loop_history: VecDeque<(String, String)>,
    pub turn_count: usize,
}

#[derive(Debug, Clone)]
pub enum LoopEvent {
    Message(AgentMessage),
    Steering(AgentMessage),
    FollowUp(AgentMessage),
    ToolCompleted { tool_call_id: String, result: ToolResult },
    PermissionDecision { tool_call_id: String, decision: Decision },
    ProviderEvent(ProviderEvent),
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum Action {
    CallProvider { request: ProviderRequest },
    ExecuteTool { tool_call_id: String, name: String, input: serde_json::Value },
    CommitMessage(AgentMessage),
    EmitNotification { kind: String, payload: serde_json::Value },
    TriggerDoomLoop { tool: String, count: usize },
    EndInnerTurn,
    EndOuterLoop,
}

/// 纯函数：state + event → new state + actions
pub fn step(state: &LoopState, event: LoopEvent) -> (LoopState, Vec<Action>) {
    // 无 I/O。调用方负责实际执行 actions 并把 effect 喂回来。
    // 测试不需要 mock 文件系统、Provider、tool runner。
    todo!("implemented in smallhead-core")
}
```

**收益**：
- `step()` 测试无需 mock 文件系统/LLM → 决策路径可独立验证。
- 重放、调试、stop、resume 都是 this-linear：state 序列 = 事件序列。
- "为什么 agent 这么决定" 永远可以从前一 state 推出。

---

## 4. Token Budget Tracking

每 turn 启动时检查预算：

```rust
async fn check_token_budget(&self, llm_msgs: &[LlmMessage]) -> Result<()> {
    let estimated = self.estimate_tokens(llm_msgs);
    let window = self.provider.context_window();
    let reserve = self.config.session.reserve_tokens; // 默认 16,384

    if estimated > window - reserve {
        // 触发 compaction（见 specs/04 §Compaction）
        self.session.compact().await?;
        // 重新计算
        let new_llm_msgs = self.transform_context().await?;
        let new_estimated = self.estimate_tokens(&new_llm_msgs);
        if new_estimated > window - reserve {
            return Err(Error::ContextWindowExceeded { used: new_estimated, window, reserve });
        }
    }

    // 发出 TokenBreakdown event 用于 TUI 可视化
    self.emit_notification("token_breakdown", json!({
        "used": estimated,
        "window": window,
        "reserve": reserve,
        "epoch_id": self.state.epoch_id,
    })).await;

    Ok(())
}
```

---

## 5. Context Epoch 跟踪

```rust
async fn begin_epoch(&mut self, llm_msgs: &[LlmMessage]) -> Result<u64> {
    let cache_key = self.provider.cache_key(&ProviderRequest {
        messages: llm_msgs.to_vec(),
        tools: self.tools.iter().map(|t| t.spec()).collect(),
        ..self.default_request()
    });

    let new_epoch = if cache_key == self.state.last_cache_key {
        self.state.epoch_id
    } else {
        self.state.epoch_id + 1
    };

    self.state.last_cache_key = Some(cache_key);
    self.state.epoch_id = new_epoch;
    Ok(new_epoch)
}
```

**Epoch 边界**：compaction 完成、session move、不兼容的 context 转换。每个 epoch 内的"不变"部分共享 provider cache（Anthropic prompt cache 命中 = 30× 便宜）。

---

## 6. DoomLoop 检测触发

```rust
async fn handle_doom_loop(&mut self, tc: &ToolCall) -> Result<()> {
    self.commit_message(AgentMessage::Notification {
        kind: "doom_loop_triggered".into(),
        payload: json!({
            "tool": tc.name,
            "count": self.doom_loop.threshold(),
        }),
    }).await?;
    // 弹 permission prompt 给用户（即使规则是 allow，doom loop 也会 ask）
    let decision = self.ask_user_permission(tc).await?;
    self.commit_message(AgentMessage::Notification {
        kind: "permission_decision".into(),
        payload: json!({
            "tool_call_id": tc.id,
            "action": format!("{:?}", decision.action),
            "decided_by": "user",
        }),
    }).await?;
    Ok(())
}
```

---

## 7. Permission 决策流程

```rust
async fn handle_permission_decision(&mut self, tc: &ToolCall, decision: &Decision) -> Result<()> {
    match decision.action {
        Action::Allow => {
            // ruleset 已经允许 → 不操作
        }
        Action::Ask => {
            let user_decision = self.ask_user_permission(tc).await?;
            if matches!(user_decision.action, Action::Deny) {
                self.commit_message(AgentMessage::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: vec![ContentBlock::Text { text: "Denied by user".into() }],
                    is_error: true,
                }).await?;
            }
        }
        Action::Deny => {
            self.commit_message(AgentMessage::ToolResult {
                tool_call_id: tc.id.clone(),
                content: vec![ContentBlock::Text { text: "Denied by ruleset".into() }],
                is_error: true,
            }).await?;
        }
    }
    Ok(())
}
```

详见 `specs/05-permission-model.md`。

---

## 8. Cancellation 语义

```rust
pub struct CancellationToken { /* tokio_util */ }

impl AgentLoop {
    pub async fn run(mut self) -> Result<()> {
        tokio::select! {
            result = self.run_until_done() => result,
            _ = self.cancel.cancelled() => {
                self.commit_message(AgentMessage::Notification {
                    kind: "cancelled".into(),
                    payload: json!({"at_turn": self.state.turn_count}),
                }).await?;
                Ok(())
            }
        }
    }

    fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
```

取消点：`run_one_turn` 每轮开始 + tool 执行完成后。中断的 tool 在 session JSONL 中标记为 `interrupted`（durability layer 接管）。

---

## 9. 与 pi agent-loop.ts 的对照

| pi (TypeScript) | smallhead (Rust) | 备注 |
|-----------------|-------------------|------|
| `agent-loop.ts:117 runLoop()` | `AgentLoop::run` | OUTER while |
| `runLoop()` 内 `while(true)` | `run_one_turn()` 内 `loop` | INNER while |
| `transformContext()` | `transform_context()` | 同语义 |
| `convertToLlm()` | `to_llm()` trait + `filter_map` | Rust 类型安全 |
| `steer(msg)` | `steer(msg)` (mpsc::Sender) | 同 API |
| `followUp(msg)` | `follow_up(msg)` (mpsc::Sender) | 同 API |
| `cancellationToken` | `tokio_util::CancellationToken` | 一致 |
| 隐式 async/Promise | 显式 `async fn` | Rust 表达 |

---

## 10. 验收测试

**用例 1: Pure step**

```rust
#[test]
fn step_handles_user_message() {
    let state = LoopState::default();
    let event = LoopEvent::Message(AgentMessage::User {
        content: vec![ContentBlock::Text { text: "hello".into() }],
        timestamp: 0,
    });
    let (new_state, actions) = step(&state, event);
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], Action::CommitMessage(_)));
}
```

**用例 2: Fixed input → real loop → verify outputs**

```rust
#[tokio::test]
async fn end_to_end_simple_conversation() {
    let tempdir = tempdir().unwrap();
    let session = Arc::new(SessionStore::new(tempdir.path()).await.unwrap());
    let mock = MockProvider::new(vec![
        ProviderEvent::MessageStart { id: "msg-1".into(), model: "test".into() },
        ProviderEvent::ContentBlockStart { index: 0, block: ContentBlock::Text { text: "Hi!".into() } },
        ProviderEvent::ContentBlockStop { index: 0 },
        ProviderEvent::MessageDelta { usage: Usage::default(), stop_reason: StopReason::EndTurn },
        ProviderEvent::MessageStop,
    ]);

    let mut loop = AgentLoop::new_test(session.clone(), Arc::new(mock));
    loop.run().await.unwrap();

    let entries = session.read_all_entries().await.unwrap();
    assert_eq!(entries.len(), 2); // User + Assistant
    assert!(matches!(entries[1].kind, EntryKind::Message(AgentMessage::Assistant { .. })));
}
```

**用例 3: Steering 中断当前 turn**

```rust
#[tokio::test]
async fn steering_interrupts_inner_turn() {
    // GIVEN: agent 准备执行第二个 tool call
    // WHEN: steering channel 收到 "stop"
    // THEN: INNER 立即返回，OUTER 处理 follow-up 队列（如果有）
}
```

**用例 4: Doom loop 触发 ask**

```rust
#[tokio::test]
async fn doom_loop_triggers_ask_after_3_repeats() {
    // GIVEN: 同 (tool, args) 连续 3 次
    // WHEN: 第 4 次 attempt
    // THEN: 即使 ruleset 是 allow，也强制 ask
}
```

---

## 11. 不变量

1. **OUTER 处理 follow-up，INNER 处理 tool+steering**：两个边界物理分离（两个 mpsc channel）。
2. **`to_llm()` 在每轮 `transform_context()` 之后立即调用**——任何时候送给 provider 的消息都经过这条窄门。
3. **`epoch_id` 在每轮 prompt assembly 启动时记录**——如果 cache_key 没变，epoch_id 不递增。
4. **doom_loop 检测在 permission evaluate 之前**——避免"rule allow 但 doom 触发 ask"的歧义。
5. **steering 只在 tool call 边界插入**——不在 LLM 响应中途打断。
6. **follow-up 只在 INNER 结束时由 OUTER 处理**——不在 tool 中途插入。
7. **cancellation 是 cooperative**——tool 不知道 cancel，只有下一个 `cancel.check()` 知道。
8. **pure `step()` 不调用任何 I/O**——测试时无副作用。

---

## 12. 引用

- 上游 spec: `specs/01-message-model.md`（`AgentMessage` + `to_llm()`）
- 引用本 spec:
  - `specs/03-tool-model.md` — `execute_tool()` 实现
  - `specs/04-session-storage.md` — `transform_context()` 含 compaction
  - `specs/05-permission-model.md` — `evaluate()` + doom loop
- 源码: `ARCHITECTURE.md` §5
- 方法论: `AGENT_METHODOLOGY.md` §1.1, §2.6

---

## 13. 变更日志

- **2026-10-04 v0.1** — 初版。从 pi agent-loop.ts 移植 + agent-deck-rs Pure Pipeline 增强。
- **未来**: 考虑把 `LoopEvent` 拆为多 stream（steering / follow-up / cancel 各自一 channel），让 scheduler 更细。
