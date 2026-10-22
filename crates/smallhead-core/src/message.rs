//! AgentMessage 类型模型。
//!
//! 这是一个 enum-dispatch 的版本，对照 pi-mono 的 TypeScript union：
//!
//! ```ts
//! type AgentMessage =
//!   | { role: "user", content, ... }
//!   | { role: "assistant", content, tool_calls, ... }
//!   | { role: "toolResult", tool_call_id, content, isError }
//!   | { role: "notification", ... }   // 走 Agent 内部，不进 prompt
//! ```
//!
//! 关键不变量：`Notification / Steering / FollowUp / Compaction / BranchMarker`
//! 这 5 个变体不会通过 [`ToLlmMessage::to_llm`] 直接进入 LLM context。
//! AgentLoop 在发 provider 请求前会使用 provider projection，把 `Compaction`
//! 的摘要作为可见上下文注入。

use serde::{Deserialize, Serialize};

/// Agent 内部 message 联合。
///
/// 严格区分"模型看到"和"Agent 内部状态"。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    /// 用户消息（含文本 + 图片 + 思考等）。
    User {
        /// 多模内容块。
        content: Vec<ContentBlock>,
        /// 毫秒时间戳。
        timestamp: i64,
    },
    /// 模型消息：包含文本块 + 可能的 tool calls。
    Assistant {
        /// 内容块（可能包含 thinking + text + tool_use）。
        content: Vec<ContentBlock>,
        /// LLM 调出的工具调用。
        tool_calls: Vec<ToolCall>,
        /// 模型 id（如 "claude-sonnet-4-6"）。
        model: String,
        /// token 用量。
        usage: Option<Usage>,
    },
    /// 工具执行结果。
    ToolResult {
        /// 对应的 tool_call_id。
        tool_call_id: String,
        /// 输出内容。
        content: Vec<ContentBlock>,
        /// 是否错误。
        is_error: bool,
    },
    /// Agent 内部事件（如 tool call 启动、compaction 触发），**不进 LLM**。
    Notification {
        /// 事件类型（"tool_call_started" / "compaction_triggered" 等）。
        kind: String,
        /// 任意 JSON payload。
        payload: serde_json::Value,
    },
    /// Compaction 边界标记，不通过 `to_llm` 直接进 LLM。
    Compaction {
        /// 摘要内容（会注入 prompt）。
        summary: String,
        /// 保留段的第一个 entry id。
        first_kept_entry_id: String,
        /// 被引用的 anchor entry ids（保留可验证性）。
        anchor_refs: Vec<String>,
    },
    /// Branch / fork 标记，**不进 LLM**。
    BranchMarker {
        /// 父 entry id。
        parent_id: String,
        /// 用户标签。
        label: String,
    },
    /// Steering 中断（来自当前 turn 中的 user）。
    Steering {
        /// 内容。
        content: Vec<ContentBlock>,
    },
    /// Follow-up 追加（当前 turn 结束后处理）。
    FollowUp {
        /// 内容。
        content: Vec<ContentBlock>,
    },
}

/// 内容块（多模支持）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图片（base64）。
    Image {
        /// MIME type（"image/png" 等）。
        media_type: String,
        /// base64 编码的二进制。
        data: String,
    },
    /// 思考块（extended thinking）。
    Thinking {
        /// 思考文本。
        text: String,
        /// 可选签名（用于跨 turn 校验）。
        signature: Option<String>,
    },
    /// 工具调用（在 Assistant 里）。
    ToolUse {
        /// tool_call_id。
        id: String,
        /// 工具名。
        name: String,
        /// 输入 JSON。
        input: serde_json::Value,
    },
    /// 工具结果（在 ToolResult.content 里）。
    ToolResult {
        /// 对应 tool_call_id。
        tool_call_id: String,
        /// 嵌套内容块。
        content: Vec<Box<ContentBlock>>,
        /// 是否错误。
        is_error: bool,
    },
}

/// 工具调用声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// tool_call_id（必须唯一）。
    pub id: String,
    /// 工具名。
    pub name: String,
    /// 输入参数（已通过 schema 校验）。
    pub input: serde_json::Value,
}

/// Token 用量。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Usage {
    /// 输入 token。
    pub input_tokens: u32,
    /// 输出 token。
    pub output_tokens: u32,
    /// cache hit token（如 Anthropic prompt cache）。
    pub cache_hit_tokens: u32,
}

/// LLM 实际看到的 message 形式（不含 Agent 内部变体）。
///
/// 这是 `to_llm` 的输出。是 pi `runtime` 层"清洗后的 message"。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum LlmMessage {
    /// 用户消息。
    User {
        /// 内容。
        content: Vec<ContentBlock>,
    },
    /// 助手消息（含 tool_calls）。
    Assistant {
        /// 内容。
        content: Vec<ContentBlock>,
        /// tool_calls。
        tool_calls: Vec<ToolCall>,
    },
    /// 工具结果。
    ToolResult {
        /// tool_call_id。
        tool_call_id: String,
        /// 内容。
        content: Vec<ContentBlock>,
        /// is_error。
        is_error: bool,
    },
}

/// 把 `AgentMessage` 转成 LLM 实际看到的 message。
///
/// 不变量：只有 `User / Assistant / ToolResult` 通过这个 trait 直接进入 LLM。
/// AgentLoop 的 provider projection 会另外把 `Compaction` 摘要注入请求上下文。
pub trait ToLlmMessage {
    /// 如果此消息应该出现在 LLM context 里，返回 `Some(LlmMessage)`；否则 `None`。
    fn to_llm(&self) -> Option<LlmMessage>;
}

impl ToLlmMessage for AgentMessage {
    fn to_llm(&self) -> Option<LlmMessage> {
        match self {
            AgentMessage::User { content, .. } => Some(LlmMessage::User {
                content: content.clone(),
            }),
            AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            } => Some(LlmMessage::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
            }),
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => Some(LlmMessage::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // —— 以下变体不进 LLM ——
            AgentMessage::Notification { .. }
            | AgentMessage::Compaction { .. }
            | AgentMessage::BranchMarker { .. }
            | AgentMessage::Steering { .. }
            | AgentMessage::FollowUp { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_converts_to_llm() {
        let msg = AgentMessage::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            timestamp: 0,
        };
        assert!(matches!(msg.to_llm(), Some(LlmMessage::User { .. })));
    }

    #[test]
    fn notification_does_not_convert_to_llm() {
        let msg = AgentMessage::Notification {
            kind: "compaction_triggered".into(),
            payload: serde_json::json!({}),
        };
        assert!(msg.to_llm().is_none());
    }

    #[test]
    fn steering_does_not_convert_to_llm() {
        let msg = AgentMessage::Steering {
            content: vec![ContentBlock::Text {
                text: "stop".into(),
            }],
        };
        assert!(msg.to_llm().is_none());
    }

    #[test]
    fn compaction_does_not_convert_to_llm() {
        let msg = AgentMessage::Compaction {
            summary: "summary".into(),
            first_kept_entry_id: "e0".into(),
            anchor_refs: vec!["e1".into()],
        };
        assert!(msg.to_llm().is_none());
    }
}
