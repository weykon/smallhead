# 01 — Message Model

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-core/src/message.rs`
> 上游: `ARCHITECTURE.md` §3.1, `AGENT_METHODOLOGY.md` §1.2

---

## 1. 概述

`AgentMessage` 是 smallhead agent 内部状态机的消息格式。它承载 **Agent 完整的世界视图**——不仅包含送给 LLM 的 `User` / `Assistant` / `ToolResult`，还包含 Agent 用来做决策、但**不应**让模型看到的内部状态（`Notification` / `Steering` / `FollowUp` / `Compaction` / `BranchMarker`）。

关键的工程纪律是：**消息在 Agent 内部是"宽"的、跨过 `to_llm()` 边界后是"窄"的**。任何在内部把 `Notification` 之类的字段泄露给 LLM 的实现都是 bug，必须由 trait 边界静态阻止。

这条边界与 pi-mono 的 `convertToLlm` 一致（`packages/agent/src/agent-loop.ts:204–289`），并吸取 big-head 的教训——big-head 把 message 直接塞进 ECS entities，导致"模型看到工作流事件"。smallhead **严格分离**。

---

## 2. 类型定义

```rust
// smallhead-core/src/message.rs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    // ── 进 LLM context 的三类 ─────────────────────────────────────
    User {
        content: Vec<ContentBlock>,
        timestamp: i64,
    },
    Assistant {
        content: Vec<ContentBlock>,
        tool_calls: Vec<ToolCall>,
        model: String,
    },
    ToolResult {
        tool_call_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },

    // ── 不进 LLM context 的 Agent 内部状态 ───────────────────────
    /// 跨 AgentMessage 边界传递的"模型不该看到"信息
    /// 例：权限弹窗结果、UI 事件、Provider 重试信号
    Notification {
        kind: String,
        payload: serde_json::Value,
    },
    /// Compaction 边界标记（详见 specs/04-session-storage.md）
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        anchor_refs: Vec<AnchorRef>,
    },
    /// Branch / fork 标记
    BranchMarker {
        parent_id: String,
        label: String,
    },
    /// Steering（用户中断当前 turn，详见 specs/02-agent-loop.md §5.3）
    Steering {
        content: Vec<ContentBlock>,
    },
    /// Follow-up（用户追加，等当前 turn 完成）
    FollowUp {
        content: Vec<ContentBlock>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
    Thinking { text: String, signature: Option<String> },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_call_id: String, content: Vec<Box<ContentBlock>>, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorRef {
    pub entry_id: String,
    pub reason: String, // "explicit_citation", "tool_result_referenced", ...
}

/// AgentMessage 转换到 LLM 可见消息的 trait。
/// 这是 smallhead 最严格的一条 trait 边界——实现错误会让模型看到不该看的东西。
pub trait ToLlmMessage {
    fn to_llm(&self) -> Option<LlmMessage>;
}

/// 真正送给 provider 的消息格式（Anthropic / OpenAI 各自的 wire format 由 Provider impl 翻译）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum LlmMessage {
    User { content: Vec<ContentBlock> },
    Assistant { content: Vec<ContentBlock>, tool_calls: Vec<ToolCall> },
    ToolResult { tool_call_id: String, content: Vec<ContentBlock>, is_error: bool },
}

impl ToLlmMessage for AgentMessage {
    fn to_llm(&self) -> Option<LlmMessage> {
        match self {
            // 进 LLM
            AgentMessage::User { content, .. } => Some(LlmMessage::User { content: content.clone() }),
            AgentMessage::Assistant { content, tool_calls, .. } => Some(LlmMessage::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
            }),
            AgentMessage::ToolResult { tool_call_id, content, is_error } => Some(LlmMessage::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // 不进 LLM —— 显式列举避免漏写
            AgentMessage::Notification { .. } => None,
            AgentMessage::Compaction { .. } => None,
            AgentMessage::BranchMarker { .. } => None,
            AgentMessage::Steering { .. } => None,
            AgentMessage::FollowUp { .. } => None,
        }
    }
}
```

### 2.1 TypeScript 类比（pi-mono）

```ts
// pi: packages/agent/src/types.ts (简化)
type AgentMessage =
  | { role: "user"; content: ContentBlock[]; timestamp: number }
  | { role: "assistant"; content: ContentBlock[]; toolCalls: ToolCall[]; model: string }
  | { role: "toolResult"; toolCallId: string; content: ContentBlock[]; isError: boolean }
  | { role: "notification"; kind: string; payload: unknown }
  | { role: "compaction"; summary: string; firstKeptEntryId: string; anchorRefs: AnchorRef[] }
  | { role: "branchMarker"; parentId: string; label: string }
  | { role: "steering"; content: ContentBlock[] }
  | { role: "followUp"; content: ContentBlock[] };

// pi: packages/agent/src/agent-loop.ts convertToLlm()
function convertToLlm(msg: AgentMessage): LlmMessage | null {
  switch (msg.role) {
    case "user":        return { role: "user", content: msg.content };
    case "assistant":   return { role: "assistant", content: msg.content, toolCalls: msg.toolCalls };
    case "toolResult":  return { role: "toolResult", toolCallId: msg.toolCallId, content: msg.content, isError: msg.isError };
    case "notification":
    case "compaction":
    case "branchMarker":
    case "steering":
    case "followUp":
      return null; // 关键：返回 null
  }
}
```

Rust 版的 `to_llm() -> Option<LlmMessage>` 是类型安全的——`None` 显式表达"不该进"，编译器强制 match 分支完整。

---

## 3. 不变量

1. **`to_llm()` 是窄门**：只有 `User` / `Assistant` / `ToolResult` 三个 variant 返回 `Some`。其他五个 variant 永远返回 `None`。
2. **`Notification` / `Steering` / `FollowUp` / `Compaction` / `BranchMarker` 永不进 LLM context**——即便在 prompt assembly 阶段被错误地放入 messages 数组，`to_llm()` 也会过滤掉。
3. **`ContentBlock::ToolUse` 只出现在 `AgentMessage::Assistant` 中**——其他 variant 不应包含它。
4. **`ContentBlock::ToolResult` 只出现在 `AgentMessage::ToolResult` 中**（递归属性的子结构除外）。
5. **`ToolCall.id` 在同一会话中全局唯一**（UUID v7）。
6. **`Compaction.first_kept_entry_id` 必须指向 JSONL 中真实存在的 entry**——孤儿 ID 是数据腐化信号。
7. **`Steering` / `FollowUp` 在 session 中按 FIFO 处理**：保留发送顺序，避免用户输入乱序。
8. **类型安全的边界**：`AgentMessage` 不会 unwrap 成 LLM message；必须显式 `to_llm()` 调用。

---

## 4. 验收测试

### 4.1 GIVEN/WHEN/THEN 用例

**用例 1: Notification 不进 LLM**

```rust
#[test]
fn notification_never_appears_in_llm_context() {
    // GIVEN a mix of AgentMessages including Notification
    let msgs = vec![
        AgentMessage::User { content: vec![ContentBlock::Text { text: "hi".into() }], timestamp: 0 },
        AgentMessage::Notification { kind: "permission_asked".into(), payload: json!({"tool": "bash"}) },
        AgentMessage::Assistant { content: vec![], tool_calls: vec![], model: "test".into() },
    ];

    // WHEN transforming via to_llm
    let llm_msgs: Vec<LlmMessage> = msgs.iter().filter_map(|m| m.to_llm()).collect();

    // THEN only User and Assistant survive
    assert_eq!(llm_msgs.len(), 2);
    assert!(matches!(llm_msgs[0], LlmMessage::User { .. }));
    assert!(matches!(llm_msgs[1], LlmMessage::Assistant { .. }));
}
```

**用例 2: Compaction 不进 LLM**

```rust
#[test]
fn compaction_block_does_not_appear_in_llm_context() {
    let comp = AgentMessage::Compaction {
        summary: "User asked X, assistant did Y, tool returned Z".into(),
        first_kept_entry_id: "entry-007".into(),
        anchor_refs: vec![AnchorRef { entry_id: "entry-003".into(), reason: "explicit_citation".into() }],
    };
    assert!(comp.to_llm().is_none(), "Compaction must not enter LLM context");
}
```

**用例 3: Steering / FollowUp 不进 LLM**

```rust
#[test]
fn steering_and_followup_do_not_appear_in_llm_context() {
    assert!(AgentMessage::Steering { content: vec![] }.to_llm().is_none());
    assert!(AgentMessage::FollowUp { content: vec![] }.to_llm().is_none());
    assert!(AgentMessage::BranchMarker { parent_id: "x".into(), label: "y".into() }.to_llm().is_none());
}
```

**用例 4: ContentBlock 类型约束**

```rust
#[test]
fn content_block_round_trip_json() {
    let block = ContentBlock::Image { media_type: "image/png".into(), data: "base64...".into() };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", block), format!("{:?}", back));
}
```

### 4.2 Round-trip 序列化

所有 `AgentMessage` 和 `ContentBlock` 必须能 `serde_json::to_string` → `from_str` 还原（JSONL 持久化的前提）。

### 4.3 静态检查

启用 `#[deny(unreachable_patterns)]`：在 `to_llm()` match 中如果新增 variant 而忘记处理，编译失败。

---

## 5. 引用

- 上游 spec: 无（这是 spec 树的根）
- 引用本 spec:
  - `specs/02-agent-loop.md` — 在 `transform_context()` 中消费 `AgentMessage`
  - `specs/04-session-storage.md` — `AgentMessage` 持久化为 `EntryKind::Message`
  - `specs/05-permission-model.md` — `Notification` 携带权限决策结果
- 源码: `ARCHITECTURE.md` §3.1 (AgentMessage)
- 方法论: `AGENT_METHODOLOGY.md` §1.2

---

## 6. 变更日志

- **2026-10-04 v0.1** — 初版。从 pi-mono `agent-loop.ts` 移植，确定 8 个 variant 与 `ToLlmMessage` trait 边界。
- **未来**: 考虑把 `Notification` 进一步类型化为 enum（`PermissionAsked`, `ProviderRetrying`, `WorldMutated`），让 `payload: serde_json::Value` 不再是字符串逃生通道。
