//! Provider trait + MockProvider + MiniMaxProvider。
//!
//! 设计参照 opencode V2：每个 provider turn 是**显式的 stream**（不是 callback 风格）。
//! 每事件可独立记录 → 重放性是 first-class concern。
//!
//! ## 子模块
//!
//! - 本文件中的 [`MockProvider`] —— 不依赖任何 API key，单元测试/demo 用。
//! - [`minimax`]：[`minimax::MiniMaxProvider`] —— MiniMax 真实 API（Anthropic 协议）。
//!
//! 真实 provider（Anthropic 原生 / OpenAI / Ollama）在后续 v 加入。

use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, LlmMessage, ToolCall, Usage};

// ── 子模块 ────────────────────────────────────────────────────────────────

/// MiniMax (Anthropic 协议) 真实 API provider。
///
/// 默认指向 `https://api.minimaxi.com/anthropic`，环境变量 `MINIMAX_API_KEY`
/// 提供鉴权。Demo 设置环境变量后即切换到真实 LLM。
pub mod minimax;

/// Provider 错误。
#[derive(Debug, Error, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    /// 网络错误。
    #[error("network error: {0}")]
    Network(String),
    /// API 返回 4xx / 5xx。
    #[error("api error: {status} {message}")]
    Api {
        /// HTTP status。
        status: u16,
        /// 错误信息。
        message: String,
    },
    /// 流中断。
    #[error("stream interrupted")]
    StreamInterrupted,
    /// 其他。
    #[error("{0}")]
    Other(String),
}

/// LLM stop reason。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmStopReason {
    /// 正常结束。
    EndTurn,
    /// 调出工具。
    ToolUse,
    /// 达到 max_tokens。
    MaxTokens,
    /// 内容过滤。
    ContentFilter,
    /// 未知。
    Unknown,
}

/// Provider 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// System prompt。
    pub system: String,
    /// LLM 看到的 messages（已经过 [`crate::message::ToLlmMessage`] 过滤）。
    pub messages: Vec<LlmMessage>,
    /// 可用工具规格。
    pub tools: Vec<crate::tool::ToolSpec>,
    /// 最大输出 token。
    pub max_tokens: usize,
    /// 采样温度。
    pub temperature: f32,
}

impl Default for ProviderRequest {
    fn default() -> Self {
        Self {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: 0.7,
        }
    }
}

/// Provider event stream 的单事件。
///
/// 对照 pi / opencode V2 的事件流：每个 event 独立可记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// 消息开始。
    MessageStart {
        /// 消息 id。
        id: String,
        /// 模型 id。
        model: String,
    },
    /// 内容块开始。
    ContentBlockStart {
        /// 块索引（0-based）。
        index: usize,
        /// 块类型。
        block: ContentBlock,
    },
    /// 内容块增量（文本，或工具参数的 JSON 片段；结束前不保证可解析）。
    ContentBlockDelta {
        /// 块索引。
        index: usize,
        /// 增量文本。
        delta: String,
    },
    /// 内容块结束。
    ContentBlockStop {
        /// 块索引。
        index: usize,
    },
    /// 消息增量（含 usage + stop_reason）。
    MessageDelta {
        /// token 用量。
        usage: Usage,
        /// stop reason。
        stop_reason: LlmStopReason,
    },
    /// 消息结束。
    MessageStop,
    /// 错误。
    Error {
        /// 错误。
        error: ProviderError,
        /// 是否可重试。
        retryable: bool,
    },
}

/// Provider trait。
///
/// 每次 stream 是**显式**的——这是 opencode V2 的设计：可独立记录、可重放。
/// `cache_key` 用于 Context Epoch tracking（同一 epoch 内可命中 provider cache）。
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider id（如 "anthropic", "openai", "mock"）。
    fn id(&self) -> &str;

    /// 模型 id。
    fn model(&self) -> &str;

    /// 上下文窗口大小。
    fn context_window(&self) -> usize;

    /// 流式响应。
    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError>;

    /// 使用本轮独立的取消信号请求模型；取消会结束请求及响应流。
    /// 不改变 provider 的全局取消状态，可安全用于连续或并发轮次。
    async fn stream_with_cancel(
        &self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Network("request cancelled".into())),
            result = self.stream(request) => result?,
        };
        Ok(cancel_stream(stream, cancel))
    }

    /// 计算 cache key（用于 Context Epoch tracking）。
    fn cache_key(&self, request: &ProviderRequest) -> String;

    /// Provider 实例的全局取消信号。取消后该实例不再接受请求。
    /// 单轮取消请使用 [`Self::stream_with_cancel`]，避免影响后续轮次。
    fn abort_signal(&self) -> CancellationToken;
}

/// 为响应流添加取消监听；流式读取阶段也能主动结束，释放底层请求。
pub(super) fn cancel_stream(
    mut stream: Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>,
    cancel: CancellationToken,
) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send>> {
    Box::pin(async_stream::stream! {
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    yield ProviderEvent::Error {
                        error: ProviderError::Network("request cancelled".into()),
                        retryable: false,
                    };
                    break;
                },
                event = stream.next() => event,
            };
            match event {
                Some(event) => yield event,
                None => break,
            }
        }
    })
}

// ============================ MockProvider ============================

/// Mock provider 的预设响应。
///
/// Demo 用：每调用 `stream()` 一次，弹出一个 `MockResponse` 并转换成 `ProviderEvent` 流。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockResponse {
    /// Assistant 输出纯文本。
    AssistantText {
        /// 文本内容。
        text: String,
    },
    /// Assistant 调出工具。
    ToolCall {
        /// 工具名。
        name: String,
        /// 输入 JSON。
        input: serde_json::Value,
    },
    /// Assistant 同时输出文本 + 调出工具。
    TextThenToolCall {
        /// 文本内容。
        text: String,
        /// 工具名。
        name: String,
        /// 输入。
        input: serde_json::Value,
    },
}

/// Mock provider：弹出 `responses` 队列里的预设响应。
///
/// 不依赖任何外部 API。测试 / demo 必备。
pub struct MockProvider {
    /// 响应队列（每调用 `stream` 一次弹出一个）。
    pub responses: std::sync::Mutex<Vec<MockResponse>>,
    /// 模型 id。
    pub model_id: String,
}

impl MockProvider {
    /// 创建一个 mock provider，附带一组预设响应。
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            model_id: "mock-model-v1".into(),
        }
    }

    /// 追加一个响应（线程安全）。
    pub fn push(&self, response: MockResponse) {
        self.responses.lock().unwrap().push(response);
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        &self.model_id
    }

    fn context_window(&self) -> usize {
        200_000
    }

    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        // 弹出一个预设响应（按 FIFO 顺序，从队首取）
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(ProviderError::Other(
                "MockProvider exhausted: no more responses".into(),
            ));
        }
        let resp = responses.remove(0);

        // 把它转成 ProviderEvent 流
        let events: Vec<ProviderEvent> = match resp {
            MockResponse::AssistantText { text } => vec![
                ProviderEvent::MessageStart {
                    id: "msg_mock_001".into(),
                    model: self.model_id.clone(),
                },
                ProviderEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: text,
                },
                ProviderEvent::ContentBlockStop { index: 0 },
                ProviderEvent::MessageDelta {
                    usage: Usage::default(),
                    stop_reason: LlmStopReason::EndTurn,
                },
                ProviderEvent::MessageStop,
            ],
            MockResponse::ToolCall { name, input } => {
                let tool_call = ToolCall {
                    id: "tc_mock_1".into(),
                    name,
                    input,
                };
                vec![
                    ProviderEvent::MessageStart {
                        id: "msg_mock_002".into(),
                        model: self.model_id.clone(),
                    },
                    ProviderEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlock::ToolUse {
                            id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            input: tool_call.input.clone(),
                        },
                    },
                    ProviderEvent::ContentBlockStop { index: 0 },
                    ProviderEvent::MessageDelta {
                        usage: Usage::default(),
                        stop_reason: LlmStopReason::ToolUse,
                    },
                    ProviderEvent::MessageStop,
                ]
            }
            MockResponse::TextThenToolCall { text, name, input } => {
                let tool_call = ToolCall {
                    id: "tc_mock_2".into(),
                    name,
                    input,
                };
                vec![
                    ProviderEvent::MessageStart {
                        id: "msg_mock_003".into(),
                        model: self.model_id.clone(),
                    },
                    ProviderEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlock::Text { text: text.clone() },
                    },
                    ProviderEvent::ContentBlockDelta {
                        index: 0,
                        delta: text,
                    },
                    ProviderEvent::ContentBlockStop { index: 0 },
                    ProviderEvent::ContentBlockStart {
                        index: 1,
                        block: ContentBlock::ToolUse {
                            id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            input: tool_call.input.clone(),
                        },
                    },
                    ProviderEvent::ContentBlockStop { index: 1 },
                    ProviderEvent::MessageDelta {
                        usage: Usage::default(),
                        stop_reason: LlmStopReason::ToolUse,
                    },
                    ProviderEvent::MessageStop,
                ]
            }
        };

        Ok(Box::pin(stream::iter(events)))
    }

    fn cache_key(&self, _request: &ProviderRequest) -> String {
        "mock-static-key".into()
    }

    fn abort_signal(&self) -> CancellationToken {
        // mock 没有真实的网络请求,abort 信号对 MockProvider 是 no-op。
        // 给一个 fresh token,AgentLoop 拿到后即使 cancel() 也不会产生任何副作用。
        CancellationToken::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::LlmMessage;

    #[tokio::test]
    async fn mock_provider_returns_text_response() {
        let provider = MockProvider::new(vec![MockResponse::AssistantText {
            text: "hello".into(),
        }]);
        let req = ProviderRequest {
            system: "sys".into(),
            messages: vec![LlmMessage::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            max_tokens: 100,
            temperature: 0.5,
        };
        let mut stream = provider.stream(req).await.unwrap();
        let first = stream.next().await;
        assert!(matches!(first, Some(ProviderEvent::MessageStart { .. })));
    }

    #[tokio::test]
    async fn mock_provider_returns_tool_call() {
        let provider = MockProvider::new(vec![MockResponse::ToolCall {
            name: "echo".into(),
            input: serde_json::json!({"text": "x"}),
        }]);
        let req = ProviderRequest::default();
        let mut stream = provider.stream(req).await.unwrap();
        let mut found_tool_use = false;
        while let Some(ev) = stream.next().await {
            if let ProviderEvent::ContentBlockStart {
                block: ContentBlock::ToolUse { name, .. },
                ..
            } = ev
            {
                assert_eq!(name, "echo");
                found_tool_use = true;
            }
        }
        assert!(found_tool_use);
    }

    #[tokio::test]
    async fn mock_provider_exhausts() {
        let provider = MockProvider::new(vec![]);
        let req = ProviderRequest::default();
        let result = provider.stream(req).await;
        assert!(matches!(result, Err(ProviderError::Other(_))));
    }
}
