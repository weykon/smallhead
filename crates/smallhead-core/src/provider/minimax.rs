//! `MiniMaxProvider` —— MiniMax 真实 API provider（Anthropic 协议）。
//!
//! MiniMax 暴露的是 Anthropic Messages API 的兼容端点：
//!
//! - 端点：<https://api.minimaxi.com/anthropic/v1/messages>
//! - 鉴权：`x-api-key: <MINIMAX_API_KEY>`
//! - 请求体格式：与 Anthropic 一致（含 `system`、`messages`、`tools`、`stream: true`）
//! - 响应：SSE 流（`message_start` / `content_block_*` / `message_delta` / `message_stop`）
//!
//! 默认模型：`MiniMax-M3`（big-head `ai_api_provider::provider.rs:229`）。
//! 也可显式传 `model: "minimax"` / `"minimax-m2"` 覆盖。
//!
//! ## Demo 用法
//!
//! ```bash
//! export MINIMAX_API_KEY="sk-..."
//! cargo run --example demo -p smallhead-core
//! ```
//!
//! 未设置 `MINIMAX_API_KEY` 时 demo 自动 fallback 到 `MockProvider`（见 `demo.rs`）。

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, Usage};
use crate::provider::{LlmStopReason, Provider, ProviderError, ProviderEvent, ProviderRequest};

/// MiniMax 默认 base URL（指向兼容 Anthropic Messages API 的子路径）。
pub const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";

/// MiniMax 默认模型 ID。
pub const MINIMAX_DEFAULT_MODEL: &str = "MiniMax-M3";

/// Anthropic API 版本（参考 Anthropic 官方文档，2023-06-01 至今稳定）。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// MiniMaxProvider 配置。
#[derive(Debug, Clone)]
pub struct MiniMaxConfig {
    /// API key。
    pub api_key: String,
    /// base URL（默认 `MINIMAX_DEFAULT_BASE_URL`）。
    pub base_url: String,
    /// 模型 ID（默认 `MINIMAX_DEFAULT_MODEL`）。
    pub model: String,
    /// 请求超时（秒，默认 120）。
    pub timeout_secs: u64,
}

impl MiniMaxConfig {
    /// 从环境变量 `MINIMAX_API_KEY` 创建（未设置则返回 `Err`）。
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("MINIMAX_API_KEY")
            .map_err(|_| "MINIMAX_API_KEY environment variable not set".to_string())?;
        Ok(Self::new(api_key))
    }

    /// 用 API key 创建（用默认 base_url 与 model）。
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: MINIMAX_DEFAULT_BASE_URL.to_string(),
            model: MINIMAX_DEFAULT_MODEL.to_string(),
            timeout_secs: 120,
        }
    }

    /// 覆盖 base_url（用于 proxy 或测试）。
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// 覆盖 model。
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

/// MiniMax / Anthropic-protocol provider。
pub struct MiniMaxProvider {
    cfg: MiniMaxConfig,
    http: Client,
    /// 全局取消 token；TUI 使用独立请求 token，不取消此 token。
    abort_token: CancellationToken,
}

impl MiniMaxProvider {
    /// 从配置创建。
    pub fn new(cfg: MiniMaxConfig) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .pool_max_idle_per_host(0)
            .build()
            .expect("reqwest client should build");
        Self {
            cfg,
            http,
            abort_token: CancellationToken::new(),
        }
    }

    /// 暴露实例的全局取消 token；单轮请求请使用 `stream_with_cancel`。
    pub fn abort_token(&self) -> CancellationToken {
        self.abort_token.clone()
    }

    /// 便捷构造：从环境变量读取 API key。
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(MiniMaxConfig::from_env()?))
    }

    /// 暴露当前配置（只读）。
    pub fn config(&self) -> &MiniMaxConfig {
        &self.cfg
    }

    /// 构造 Anthropic Messages 请求体（含 stream: true）。
    fn build_request_body(&self, req: &ProviderRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| match m {
                crate::message::LlmMessage::User { content } => {
                    let blocks: Vec<Value> = content
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                            _ => json!({"type": "text", "text": ""}), // MVP 简化：只支持文本
                        })
                        .collect();
                    json!({"role": "user", "content": blocks})
                }
                crate::message::LlmMessage::Assistant {
                    content,
                    tool_calls,
                } => {
                    let mut blocks: Vec<Value> = content
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                            _ => json!({"type": "text", "text": ""}),
                        })
                        .collect();
                    for tc in tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.input,
                        }));
                    }
                    json!({"role": "assistant", "content": blocks})
                }
                crate::message::LlmMessage::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    let text = content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": text,
                            "is_error": *is_error,
                        }]
                    })
                }
            })
            .collect();

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
            "stream": true,
            "temperature": req.temperature,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if !req.system.is_empty() {
            body["system"] = Value::String(req.system.clone());
        }
        body
    }

    /// 端点 URL（自动拼 `/v1/messages`）。
    fn endpoint(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_string()
        } else {
            format!("{}/v1/messages", base)
        }
    }
}

#[async_trait]
impl Provider for MiniMaxProvider {
    fn id(&self) -> &str {
        "minimax"
    }

    fn model(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> usize {
        204_800 // 见 big-head tokens.rs:164
    }

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        self.stream_with_cancel(request, self.abort_token.child_token())
            .await
    }

    async fn stream_with_cancel(
        &self,
        request: ProviderRequest,
        stream_token: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let body = self.build_request_body(&request);
        let url = self.endpoint();
        crate::diagnostics::emit(
            "request",
            "http_body",
            serde_json::json!({"url":url,"body":&body}),
        );

        // 请求级 token 只作用于本轮，同时覆盖发送和流式读取。
        let mut attempt = 0u8;
        let resp = loop {
            attempt += 1;
            let send_fut = self
                .http
                .post(&url)
                .header("user-agent", "Claude-Code/1.0")
                .header("x-api-key", &self.cfg.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .json(&body)
                .send();
            let resp = tokio::select! {
                biased;
                _ = stream_token.cancelled() => {
                    return Err(ProviderError::Network("aborted before send".into()));
                }
                r = send_fut => r.map_err(|e| ProviderError::Network(e.to_string()))?,
            };
            let status = resp.status();
            if status.is_success() {
                break resp;
            }
            let text = resp.text().await.unwrap_or_default();
            let retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
            if retryable && attempt < 4 {
                let delay = Duration::from_millis(500u64.saturating_mul(1u64 << (attempt - 1)));
                crate::diagnostics::emit(
                    "transport",
                    "provider_retry",
                    json!({
                        "provider": "minimax",
                        "status": status.as_u16(),
                        "attempt": attempt,
                        "next_attempt_in_ms": delay.as_millis(),
                    }),
                );
                tokio::select! {
                    biased;
                    _ = stream_token.cancelled() => {
                        return Err(ProviderError::Network("request cancelled during retry backoff".into()));
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                continue;
            }
            crate::diagnostics::emit(
                "transport",
                "provider_error",
                json!({
                    "provider": "minimax",
                    "status": status.as_u16(),
                    "attempt": attempt,
                    "response": text.replace(&self.cfg.api_key, "[redacted]").chars().take(8192).collect::<String>(),
                }),
            );
            return Err(ProviderError::Api {
                status: status.as_u16(),
                message: format!("MiniMax API error: {text}"),
            });
        };

        let byte_stream = resp.bytes_stream();
        // 把 byte 流解析为 SSE 事件，再把每个事件映射为 ProviderEvent
        let event_stream = parse_sse(byte_stream);
        let provider_stream = map_sse_to_provider_events(event_stream);

        Ok(super::cancel_stream(
            Box::pin(provider_stream),
            stream_token,
        ))
    }

    fn abort_signal(&self) -> CancellationToken {
        self.abort_token.clone()
    }

    fn cache_key(&self, request: &ProviderRequest) -> String {
        // 简易 cache key：模型 + system + 最后一条消息。
        // v0.4 会升级为完整 hash（见 ARCHITECTURE.md §6）。
        let last = request.messages.last();
        let last_kind = match last {
            Some(crate::message::LlmMessage::User { .. }) => "u",
            Some(crate::message::LlmMessage::Assistant { .. }) => "a",
            Some(crate::message::LlmMessage::ToolResult { .. }) => "t",
            None => "_",
        };
        format!(
            "minimax:{}:{}:{}",
            self.cfg.model,
            request.system.len(),
            last_kind
        )
    }
}

// ============================ SSE 解析器 ============================

/// SSE event（Anthropic 格式：`event: <name>\ndata: <json>\n\n`）。
#[derive(Debug, Clone)]
struct SseEvent {
    event: String,
    data: String,
}

/// 把 reqwest byte 流解析为 `SseEvent` 流（每个 event 一条）。
///
/// 解析规则（SSE 规范）：
///
/// - `event: <name>` → 设置当前事件名
/// - `data: <text>` → 追加到当前 data（去前缀 + 单换行连接）
/// - 空行 → 提交当前 event
/// - `: comment` → 忽略
/// - 其他行 → 忽略
///
/// trace 级日志：`RUST_LOG=smallhead_core::provider::minimax=trace` 可以看到
/// 每个 SSE event 的名字 + data 前 200 字符。
fn parse_sse(
    bytes: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
) -> impl Stream<Item = SseEvent> + Send {
    async_stream::stream! {
        let mut event_name = String::new();
        let mut data = String::new();
        let mut buffer: Vec<u8> = Vec::new();

        tokio::pin!(bytes);
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    yield SseEvent {
                        event: "error".into(),
                        data: serde_json::json!({
                            "error": {
                                "message": format!("SSE byte stream error: {e}")
                            }
                        })
                        .to_string(),
                    };
                    break;
                }
            };
            buffer.extend_from_slice(&chunk);

            // 处理完整的行（以 \n 分隔；容忍 \r\n 与单独 \n）
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let mut line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                if line_bytes.last() == Some(&b'\n') {
                    line_bytes.pop();
                }
                if line_bytes.last() == Some(&b'\r') {
                    line_bytes.pop();
                }
                let line = String::from_utf8_lossy(&line_bytes).into_owned();

                if let Some(event) = parse_sse_line(&line, &mut event_name, &mut data) {
                    yield event;
                }
            }
        }
        if !buffer.is_empty() {
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            let line = String::from_utf8_lossy(&buffer).into_owned();
            if let Some(event) = parse_sse_line(&line, &mut event_name, &mut data) {
                yield event;
            }
        }
        if !data.is_empty() || !event_name.is_empty() {
            tracing::trace!(
                event = %event_name,
                data_preview = %truncate(&data, 200),
                "SSE event (eof flush)"
            );
            yield SseEvent {
                event: std::mem::take(&mut event_name),
                data: std::mem::take(&mut data),
            };
        }
    }
}

fn parse_sse_line(line: &str, event_name: &mut String, data: &mut String) -> Option<SseEvent> {
    if line.is_empty() {
        // 空行 = event 边界
        if !data.is_empty() || !event_name.is_empty() {
            tracing::trace!(
                event = %event_name,
                data_preview = %truncate(data, 200),
                "SSE event"
            );
            return Some(SseEvent {
                event: std::mem::take(event_name),
                data: std::mem::take(data),
            });
        }
    } else if let Some(rest) = line.strip_prefix("event:") {
        *event_name = rest.trim().to_string();
    } else if let Some(rest) = line.strip_prefix("data:") {
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest.trim_start());
    } else if line.starts_with(':') {
        // comment, ignore
    } else {
        // 其他字段（id / retry 等），MiniMax 不用，忽略
    }
    None
}

/// 把字符串截断到 n 字节以内（防止日志爆炸）。
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // 用 char 边界避免 panic on multi-byte UTF-8
        let mut end = n;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…(truncated, total {}B)", &s[..end], s.len())
    }
}

// Usage fields are cumulative snapshots; absent or invalid fields retain their
// previous values. Cache aliases describe the same count and must not be added.
fn merge_sse_usage(usage: &mut Usage, value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let count = |key| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let mut reported = false;
    for (target, incoming) in [
        (&mut usage.input_tokens, count("input_tokens")),
        (&mut usage.output_tokens, count("output_tokens")),
        (
            &mut usage.cache_hit_tokens,
            count("cache_hit_tokens").or_else(|| count("cache_read_input_tokens")),
        ),
    ] {
        if let Some(incoming) = incoming {
            *target = incoming;
            reported = true;
        }
    }
    reported
}

fn sse_stop_reason(value: Option<&Value>) -> Option<LlmStopReason> {
    match value.and_then(Value::as_str)? {
        "end_turn" | "stop_sequence" => Some(LlmStopReason::EndTurn),
        "tool_use" => Some(LlmStopReason::ToolUse),
        "max_tokens" => Some(LlmStopReason::MaxTokens),
        "content_filter" => Some(LlmStopReason::ContentFilter),
        _ => None,
    }
}

/// 把 SSE event 流映射为 ProviderEvent 流。
///
/// 关键 SSE event：
///
/// - `message_start`        → ProviderEvent::MessageStart
/// - `content_block_start`  → ProviderEvent::ContentBlockStart（block 暂存待累积）
/// - `content_block_delta`  → ProviderEvent::ContentBlockDelta（拼到当前累积）
/// - `content_block_stop`   → ProviderEvent::ContentBlockStop（提交累积块到结果）
/// - `message_delta`        → ProviderEvent::MessageDelta（含 stop_reason）
/// - `message_stop`         → ProviderEvent::MessageStop（流结束）
/// - `ping` / 其他           → 跳过
fn map_sse_to_provider_events(
    sse: impl Stream<Item = SseEvent> + Send + 'static,
) -> impl Stream<Item = ProviderEvent> + Send {
    async_stream::stream! {
        let mut accum_text = String::new();
        let mut accum_tool_json = String::new();
        let mut current_block: Option<String> = None; // "text" | "tool_use" | "thinking"
        let mut block_started_emitted = false;
        let mut usage = Usage::default();
        let mut stop_reason = LlmStopReason::Unknown;
        let mut pending_usage = false;

        tokio::pin!(sse);
        while let Some(ev) = sse.next().await {
            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(error) => {
                    crate::diagnostics::emit(
                        "transport",
                        "sse_parse_error",
                        serde_json::json!({
                            "event": ev.event,
                            "error": error.to_string(),
                            "data_preview": truncate(&ev.data, 500),
                            "data_bytes": ev.data.len(),
                        }),
                    );
                    continue;
                }
            };

            match ev.event.as_str() {
                "message_start" => {
                    usage = Usage::default();
                    pending_usage = merge_sse_usage(&mut usage, parsed.pointer("/message/usage"));
                    stop_reason = sse_stop_reason(parsed.pointer("/message/stop_reason"))
                        .unwrap_or(LlmStopReason::Unknown);
                    let model = parsed.get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    tracing::trace!(model = %model, "message_start");
                    yield ProviderEvent::MessageStart {
                        id: parsed.get("message")
                            .and_then(|m| m.get("id"))
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string(),
                        model,
                    };
                }
                "content_block_start" => {
                    let index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let block_type = parsed.get("content_block")
                        .and_then(|b| b.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    current_block = Some(block_type.to_string());
                    accum_text.clear();
                    accum_tool_json.clear();
                    block_started_emitted = true;

                    let block = match block_type {
                        "text" => ContentBlock::Text { text: String::new() },
                        "tool_use" => {
                            let id = parsed.get("content_block")
                                .and_then(|b| b.get("id"))
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = parsed.get("content_block")
                                .and_then(|b| b.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            tracing::trace!(
                                index,
                                tool_id = %id,
                                tool_name = %name,
                                "content_block_start (tool_use)"
                            );
                            ContentBlock::ToolUse {
                                id,
                                name,
                                input: Value::Object(serde_json::Map::new()),
                            }
                        }
                        _ => {
                            tracing::trace!(
                                index,
                                block_type = %block_type,
                                "content_block_start"
                            );
                            ContentBlock::Text { text: String::new() }
                        }
                    };
                    yield ProviderEvent::ContentBlockStart { index, block };
                }
                "content_block_delta" => {
                    let index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    let delta_type = parsed.get("delta")
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    let delta_text = match delta_type {
                        "text_delta" => parsed.get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        "input_json_delta" => {
                            let pj = parsed.get("delta")
                                .and_then(|d| d.get("partial_json"))
                                .and_then(|p| p.as_str())
                                .unwrap_or("")
                                .to_string();
                            tracing::trace!(
                                index,
                                partial_json = %truncate(&pj, 200),
                                accum_so_far = %truncate(&accum_tool_json, 200),
                                "input_json_delta"
                            );
                            accum_tool_json.push_str(&pj);
                            pj
                        }
                        "thinking_delta" => parsed.get("delta")
                            .and_then(|d| d.get("thinking"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        _ => String::new(),
                    };
                    if !delta_text.is_empty() {
                        yield ProviderEvent::ContentBlockDelta {
                            index,
                            delta: delta_text,
                        };
                    }
                }
                "content_block_stop" => {
                    let index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                    // Fragments have already been emitted once. The driver
                    // assembles and validates them when this block finishes.
                    if current_block.as_deref() == Some("tool_use") {
                        tracing::trace!(
                            index,
                            final_input_json = %truncate(&accum_tool_json, 500),
                            bytes = accum_tool_json.len(),
                            "content_block_stop (tool_use, 累积完整体)"
                        );
                        if accum_tool_json.is_empty() {
                            tracing::trace!(
                                index,
                                "content_block_stop (tool_use, **模型从未发 input_json_delta** — 工具调用 input 为空)"
                            );
                        }
                    }
                    yield ProviderEvent::ContentBlockStop { index };
                    block_started_emitted = false;
                    current_block = None;
                }
                "message_delta" => {
                    if let Some(reason) = sse_stop_reason(parsed.pointer("/delta/stop_reason")) {
                        stop_reason = reason;
                    }
                    merge_sse_usage(&mut usage, parsed.get("usage"));
                    pending_usage = false;
                    yield ProviderEvent::MessageDelta { usage, stop_reason };
                }
                "message_stop" => {
                    if pending_usage {
                        pending_usage = false;
                        yield ProviderEvent::MessageDelta { usage, stop_reason };
                    }
                    yield ProviderEvent::MessageStop;
                }
                "ping" | "error" => {
                    if ev.event == "ping" {
                        crate::diagnostics::emit(
                            "transport",
                            "sse_ping",
                            serde_json::json!({
                                "data_preview": truncate(&ev.data, 200),
                            }),
                        );
                    } else {
                        let msg = parsed.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        yield ProviderEvent::Error {
                            error: ProviderError::Api { status: 0, message: msg },
                            retryable: false,
                        };
                    }
                }
                _ => {
                    crate::diagnostics::emit(
                        "transport",
                        "sse_ignored",
                        serde_json::json!({
                            "event": ev.event,
                            "data_preview": truncate(&ev.data, 300),
                        }),
                    );
                }
            }

            // 抑制 unused warning
            let _ = block_started_emitted;
            let _ = accum_text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_from_str(s: &str) -> Bytes {
        Bytes::from(s.as_bytes().to_vec())
    }

    async fn map_test_sse(events: Vec<(&str, Value)>) -> Vec<ProviderEvent> {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = events
            .into_iter()
            .map(|(event, data)| Ok(Bytes::from(format!("event: {event}\ndata: {data}\n\n"))))
            .collect();
        map_sse_to_provider_events(parse_sse(futures::stream::iter(chunks)))
            .collect()
            .await
    }

    fn usage_snapshots(events: &[ProviderEvent]) -> Vec<(u32, u32, u32)> {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::MessageDelta { usage, .. } => Some((
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_hit_tokens,
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn tool_json_fragments_are_emitted_once_without_a_duplicate_full_payload() {
        let events = map_test_sse(vec![
            ("content_block_start", serde_json::json!({"index":0,"content_block":{"type":"tool_use","id":"call","name":"echo","input":{}}})),
            ("content_block_delta", serde_json::json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\":"}})),
            ("content_block_delta", serde_json::json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"\"你好\"}"}})),
            ("content_block_stop", serde_json::json!({"index":0})),
            ("message_stop", serde_json::json!({})),
        ]).await;
        let fragments: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ContentBlockDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments.concat(), r#"{"text":"你好"}"#);
        assert!(matches!(
            events[events.len() - 2],
            ProviderEvent::ContentBlockStop { .. }
        ));
    }

    #[tokio::test]
    async fn sse_usage_merges_start_input_and_delta_output() {
        let events = map_test_sse(vec![
            (
                "message_start",
                serde_json::json!({"message":{"usage":{"input_tokens":20,"output_tokens":0}}}),
            ),
            (
                "message_delta",
                serde_json::json!({"usage":{"output_tokens":5},"delta":{"stop_reason":"end_turn"}}),
            ),
            ("message_stop", serde_json::json!({})),
        ])
        .await;
        assert_eq!(usage_snapshots(&events), vec![(20, 5, 0)]);
        assert!(matches!(events.last(), Some(ProviderEvent::MessageStop)));
    }

    #[tokio::test]
    async fn sse_partial_usage_preserves_input_and_terminal_stop_reason() {
        let events = map_test_sse(vec![
            ("message_start", serde_json::json!({"message":{"usage":{"input_tokens":20}}})),
            ("message_delta", serde_json::json!({"usage":{"output_tokens":2},"delta":{"stop_reason":"max_tokens"}})),
            ("message_delta", serde_json::json!({"usage":{"output_tokens":5},"delta":{"stop_reason":null}})),
            ("message_delta", serde_json::json!({"usage":{"input_tokens":21}})),
            ("message_stop", serde_json::json!({})),
        ]).await;
        assert_eq!(
            usage_snapshots(&events),
            vec![(20, 2, 0), (20, 5, 0), (21, 5, 0)]
        );
        for event in &events {
            if let ProviderEvent::MessageDelta { stop_reason, .. } = event {
                assert!(matches!(stop_reason, LlmStopReason::MaxTokens));
            }
        }
    }

    #[tokio::test]
    async fn sse_usage_merges_both_cache_field_names_without_double_counting() {
        for cache_field in ["cache_read_input_tokens", "cache_hit_tokens"] {
            let events = map_test_sse(vec![
                ("message_start", serde_json::json!({"message":{"usage":{"input_tokens":20,(cache_field):7}}})),
                ("message_delta", serde_json::json!({"usage":{"output_tokens":3}})),
                ("message_delta", serde_json::json!({"usage":{(cache_field):9}})),
                ("message_delta", serde_json::json!({"usage":{"cache_hit_tokens":10,"cache_read_input_tokens":10}})),
                ("message_stop", serde_json::json!({})),
            ]).await;
            assert_eq!(
                usage_snapshots(&events),
                vec![(20, 3, 7), (20, 3, 9), (20, 3, 10)]
            );
        }
    }

    #[tokio::test]
    async fn sse_start_only_usage_is_reported_before_message_stop() {
        let events = map_test_sse(vec![
            (
                "message_start",
                serde_json::json!({"message":{
                    "usage":{"input_tokens":20,"output_tokens":4,"cache_read_input_tokens":7},
                    "stop_reason":"end_turn"
                }}),
            ),
            ("message_stop", serde_json::json!({})),
        ])
        .await;
        assert_eq!(usage_snapshots(&events), vec![(20, 4, 7)]);
        assert!(matches!(
            events.as_slice(),
            [
                ProviderEvent::MessageStart { .. },
                ProviderEvent::MessageDelta {
                    stop_reason: LlmStopReason::EndTurn,
                    ..
                },
                ProviderEvent::MessageStop,
            ]
        ));
    }

    #[tokio::test]
    async fn sse_invalid_usage_fields_do_not_erase_valid_counts() {
        let events = map_test_sse(vec![
            ("message_start", serde_json::json!({"message":{"usage":{"input_tokens":20,"output_tokens":2,"cache_hit_tokens":7}}})),
            ("message_delta", serde_json::json!({"usage":{"input_tokens":null,"output_tokens":5,"cache_hit_tokens":"invalid"}})),
            ("message_delta", serde_json::json!({"usage":{"input_tokens":-1,"output_tokens":4294967296_u64}})),
            ("message_delta", serde_json::json!({"usage":{"output_tokens":0,"cache_read_input_tokens":0}})),
            ("message_stop", serde_json::json!({})),
        ]).await;
        assert_eq!(
            usage_snapshots(&events),
            vec![(20, 5, 7), (20, 5, 7), (20, 0, 0)]
        );
    }

    #[tokio::test]
    async fn parse_sse_emits_event_on_blank_line() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(chunk_from_str("event: message_start\n")),
            Ok(chunk_from_str("data: {\"foo\":1}\n")),
            Ok(chunk_from_str("\n")),
            Ok(chunk_from_str("event: message_stop\n")),
            Ok(chunk_from_str("data: {}\n\n")),
        ];
        let s = parse_sse(futures::stream::iter(chunks));
        tokio::pin!(s);
        let events: Vec<SseEvent> = {
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "message_start");
        assert_eq!(events[0].data, "{\"foo\":1}");
        assert_eq!(events[1].event, "message_stop");
        assert_eq!(events[1].data, "{}");
    }

    #[tokio::test]
    async fn parse_sse_handles_cr_lf_and_split_chunks() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(chunk_from_str("event: ping\r\n")),
            Ok(chunk_from_str("data: ok")),
            Ok(chunk_from_str("\r\n\r\n")),
        ];
        let s = parse_sse(futures::stream::iter(chunks));
        tokio::pin!(s);
        let events: Vec<SseEvent> = {
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ping");
        assert_eq!(events[0].data, "ok");
    }

    #[tokio::test]
    async fn parse_sse_flushes_last_event_on_eof_without_blank_line() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(chunk_from_str("event: message_stop\ndata: {}"))];
        let s = parse_sse(futures::stream::iter(chunks));
        tokio::pin!(s);
        let events: Vec<SseEvent> = {
            let mut v = Vec::new();
            while let Some(ev) = s.next().await {
                v.push(ev);
            }
            v
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message_stop");
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn endpoint_appends_v1_messages() {
        let cfg = MiniMaxConfig::new("sk-test").with_base_url("https://api.minimaxi.com/anthropic");
        let p = MiniMaxProvider::new(cfg);
        assert_eq!(
            p.endpoint(),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn endpoint_does_not_double_append() {
        let cfg = MiniMaxConfig::new("sk-test")
            .with_base_url("https://example.com/anthropic/v1/messages");
        let p = MiniMaxProvider::new(cfg);
        assert_eq!(p.endpoint(), "https://example.com/anthropic/v1/messages");
    }

    #[test]
    fn request_body_has_stream_true_and_tools() {
        let cfg = MiniMaxConfig::new("sk-test");
        let p = MiniMaxProvider::new(cfg);
        let req = ProviderRequest {
            system: "你是一名乐于助人的助手。".into(),
            messages: vec![crate::message::LlmMessage::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![crate::tool::ToolSpec {
                name: "echo".into(),
                label: "Echo".into(),
                description: "echoes".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 1024,
            temperature: 0.5,
        };
        let body = p.build_request_body(&req);
        assert_eq!(body["stream"], Value::Bool(true));
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["system"], "你是一名乐于助人的助手。");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["name"], "echo");
    }
    #[tokio::test]
    async fn terminal_http_error_is_diagnosed_without_recording_the_api_key() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = MiniMaxConfig::new("private-test-key");
        cfg.base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            socket.read_exact(&mut vec![0; length]).await.unwrap();
            let body = r#"{"error":"invalid tool history; private-test-key"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        let provider = MiniMaxProvider::new(cfg);
        let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            crate::diagnostics::observe(sink, provider.stream(ProviderRequest::default())),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(ProviderError::Api { status: 400, .. })
        ));
        server.await.unwrap();
        let mut errors = Vec::new();
        while let Ok(event) = events.try_recv() {
            assert!(!serde_json::to_string(&event)
                .unwrap()
                .contains("private-test-key"));
            if event.event == "provider_error" {
                errors.push(event);
            }
        }
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].data["status"], 400);
        assert_eq!(errors[0].data["attempt"], 1);
        assert!(errors[0].data["response"]
            .as_str()
            .unwrap()
            .contains("invalid tool history; [redacted]"));
    }

    #[tokio::test]
    async fn scoped_cancel_disconnects_http_and_allows_next_request() {
        use futures::StreamExt;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let disconnected = std::sync::Arc::new(tokio::sync::Notify::new());
        let server_ready = ready.clone();
        let server_disconnected = disconnected.clone();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
                socket
                    .read_exact(&mut vec![0; content_length])
                    .await
                    .unwrap();
                if index == 1 {
                    let event = "event: message_start\ndata: {\"message\":{\"id\":\"test\",\"model\":\"test\"}}\n\n";
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n", event.len(), event);
                    socket
                        .get_mut()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap();
                }
                if index < 2 {
                    server_ready.notify_one();
                    let read = socket.read(&mut [0; 1]).await;
                    assert!(
                        matches!(read, Ok(0) | Err(_)),
                        "expected client disconnect, got {read:?}"
                    );
                    server_disconnected.notify_one();
                } else {
                    let event = "event: message_stop\ndata: {}\n\n";
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",event.len(),event);
                    socket
                        .get_mut()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap();
                }
            }
        });
        let mut cfg = MiniMaxConfig::new("local-test-key");
        cfg.base_url = url;
        let provider = MiniMaxProvider::new(cfg);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            // 发送后等待响应头时取消。
            let cancel = CancellationToken::new();
            let request = provider.stream_with_cancel(ProviderRequest::default(), cancel.clone());
            tokio::pin!(request);
            tokio::select! {
                result = &mut request => panic!("request ended before cancellation: {}", result.is_ok()),
                _ = ready.notified() => {}
            }
            cancel.cancel();
            assert!(request.await.is_err());
            disconnected.notified().await;
            // 已收到事件、继续等待 SSE 时取消。
            let cancel = CancellationToken::new();
            let mut stream = provider.stream_with_cancel(ProviderRequest::default(),cancel.clone()).await.unwrap();
            ready.notified().await;
            assert!(matches!(stream.next().await,Some(ProviderEvent::MessageStart{..})));
            cancel.cancel();
            assert!(matches!(stream.next().await,Some(ProviderEvent::Error{retryable:false,..})));
            drop(stream);
            disconnected.notified().await;
            // 复用同一个 provider，新一轮仍能完成请求。
            assert!(!provider.abort_signal().is_cancelled());
            let mut stream = provider.stream_with_cancel(ProviderRequest::default(),CancellationToken::new()).await.unwrap();
            assert!(matches!(stream.next().await,Some(ProviderEvent::MessageStop)));
            server.await.unwrap();
        }).await.expect("HTTP cancellation or recovery timed out");
    }
}
