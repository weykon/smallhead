# 07 — Provider Interface

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-core/src/provider.rs`、`smallhead-core/src/providers/`
> 上游: `ARCHITECTURE.md` §3.2, §9; `AGENT_METHODOLOGY.md` §1.3, §2.2, §2.3
> 对照: opencode V2 (`specs/v2/session.md`)、pi opencode

---

## 1. 概述

`Provider` trait 是 smallhead 隔离 LLM API 差异的唯一接口。Anthropic、OpenAI、Google、Ollama 等 provider 通过实现同一 trait 与 agent loop 解耦。

设计原则（来自 opencode V2 + agent-deck-rs）：
- **显式 `stream()` 而不是返回 `reqwest::Response`**：每事件可记录、可重放（ProviderTurn 是 session JSONL 的 entry）。
- **`cache_key()` 显式表达**：Context Epoch 跟踪的基础（opencode V2 Context Epochs 借鉴）。
- **统一的错误类型 `ProviderError`**：跨 provider 一致处理 retry / rate limit。
- **`async-trait` + `Pin<Box<dyn Stream>>`**：Rust 表达流式响应的标准做法。

---

## 2. 类型定义

```rust
// smallhead-core/src/provider.rs

use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use serde::{Deserialize, Serialize};

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;                                  // "anthropic" | "openai" | "google" | "ollama"
    fn model(&self) -> &str;
    fn context_window(&self) -> usize;

    /// 单 turn 流式响应
    async fn stream(
        &self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError>;

    /// 用于 Context Epoch 的 cache-key 计算
    fn cache_key(&self, request: &ProviderRequest) -> String;

    /// 估算 token 数（用于 token budget 检查）
    fn estimate_tokens(&self, request: &ProviderRequest) -> usize;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: usize,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    MessageStart { id: String, model: String },
    ContentBlockStart { index: usize, block: ContentBlock },
    ContentBlockDelta { index: usize, delta: String },
    ContentBlockStop { index: usize },
    MessageDelta {
        usage: Usage,
        stop_reason: StopReason,
    },
    MessageStop,
    /// Provider 错误（在 stream 中间）
    Error {
        error: ProviderError,
        retryable: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_read_tokens: usize,    // prompt cache 命中
    pub cache_write_tokens: usize,   // prompt cache 写入
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContentFilter,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    #[error("authentication failed")]
    Auth,
    #[error("rate limited (retry after {retry_after_ms}ms)")]
    RateLimit { retry_after_ms: u64 },
    #[error("server error: {message}")]
    Server { message: String, status: u16 },
    #[error("context length exceeded: used {used}, window {window}")]
    ContextLength { used: usize, window: usize },
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },
    #[error("network error: {message}")]
    Network { message: String },
    #[error("stream cancelled")]
    Cancelled,
    #[error("unknown error: {message}")]
    Unknown { message: String },
}

impl ProviderError {
    pub fn retryable(&self) -> bool {
        matches!(self,
            ProviderError::RateLimit { .. }
            | ProviderError::Server { status, .. } if *status >= 500
            | ProviderError::Network { .. }
        )
    }
}
```

---

## 3. AnthropicProvider 实现

```rust
// smallhead-core/src/providers/anthropic.rs

use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use serde_json::json;
use sha2::{Digest, Sha256};

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
    max_retries: u32,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            base_url: "https://api.anthropic.com".into(),
            max_retries: 3,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str { "anthropic" }
    fn model(&self) -> &str { &self.model }
    fn context_window(&self) -> usize {
        match self.model.as_str() {
            m if m.contains("haiku") => 200_000,
            m if m.contains("opus") => 200_000,
            _ => 200_000,                    // 默认 Claude 全 200K
        }
    }

    async fn stream(
        &self,
        req: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": req.system,
            "tools": req.tools.iter().map(to_anthropic_tool).collect::<Vec<_>>(),
            "messages": req.messages.iter().map(to_anthropic_msg).collect::<Vec<_>>(),
            "stream": true,
            "temperature": req.temperature,
        });

        // 1. retry loop
        let mut attempt = 0;
        let response = loop {
            let req = self.client.post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .build()
                .map_err(|e| ProviderError::Network { message: e.to_string() })?;

            let res = self.client.execute(req).await;
            match res {
                Ok(r) if r.status().is_success() => break r,
                Err(e) => {
                    attempt += 1;
                    if attempt > self.max_retries {
                        return Err(ProviderError::Network { message: e.to_string() });
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Ok(r) => {
                    let status = r.status().as_u16();
                    let body = r.text().await.unwrap_or_default();
                    if status == 429 || status == 529 {
                        attempt += 1;
                        if attempt > self.max_retries {
                            return Err(ProviderError::RateLimit { retry_after_ms: 5000 });
                        }
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(ProviderError::Server { message: body, status });
                }
            }
        };

        // 2. SSE → ProviderEvent stream
        let stream = async_stream::try_stream! {
            let mut event_source = EventSource::new(response);
            let mut current_block: Option<usize> = None;

            while let Some(event) = event_source.next().await {
                let event = event.map_err(|e| ProviderError::Network {
                    message: e.to_string()
                })?;

                match event {
                    Event::Open => continue,
                    Event::Message(msg) => {
                        let parsed: serde_json::Value = serde_json::from_str(&msg.data)
                            .map_err(|e| ProviderError::Unknown { message: e.to_string() })?;

                        match parsed["type"].as_str() {
                            Some("message_start") => {
                                yield ProviderEvent::MessageStart {
                                    id: parsed["message"]["id"].as_str().unwrap_or_default().to_string(),
                                    model: parsed["message"]["model"].as_str().unwrap_or_default().to_string(),
                                };
                            }
                            Some("content_block_start") => {
                                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                                let block = parse_anthropic_block(&parsed["content_block"])?;
                                yield ProviderEvent::ContentBlockStart { index, block };
                                current_block = Some(index);
                            }
                            Some("content_block_delta") => {
                                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                                let delta = parsed["delta"]["text"].as_str().unwrap_or_default();
                                yield ProviderEvent::ContentBlockDelta {
                                    index,
                                    delta: delta.to_string(),
                                };
                            }
                            Some("content_block_stop") => {
                                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                                yield ProviderEvent::ContentBlockStop { index };
                                current_block = None;
                            }
                            Some("message_delta") => {
                                let usage = parse_usage(&parsed["usage"])?;
                                let stop_reason = parse_stop_reason(&parsed["delta"]["stop_reason"])?;
                                yield ProviderEvent::MessageDelta { usage, stop_reason };
                            }
                            Some("message_stop") => {
                                yield ProviderEvent::MessageStop;
                            }
                            Some("error") => {
                                yield ProviderEvent::Error {
                                    error: parse_anthropic_error(&parsed["error"])?,
                                    retryable: false,
                                };
                            }
                            _ => continue,
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn cache_key(&self, req: &ProviderRequest) -> String {
        // Anthropic prompt cache：sha256 of system + tools + first user message
        // 简化：sha256 of 整个 request（命中粒度 = epoch）
        let canonical = serde_json::to_string(&json!({
            "system": req.system,
            "tools": req.tools,
            "messages": &req.messages[..1.min(req.messages.len())],
        })).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(&hasher.finalize()[..16])
    }

    fn estimate_tokens(&self, req: &ProviderRequest) -> usize {
        // 粗估：4 chars ≈ 1 token
        let chars: usize = req.system.len()
            + req.messages.iter().map(|m| serialize_msg_len(m)).sum::<usize>();
        chars / 4
    }
}

fn backoff(attempt: u32) -> Duration {
    // exponential backoff with jitter
    let base = 1000u64 * 2u64.pow(attempt);
    let jitter = rand::random::<u64>() % 500;
    Duration::from_millis(base + jitter)
}
```

---

## 4. OpenAIProvider 实现

模式与 Anthropic 完全一致——wire format 不同，但 trait 表达相同：

```rust
// smallhead-core/src/providers/openai.rs

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str { "openai" }
    fn model(&self) -> &str { &self.model }
    fn context_window(&self) -> usize {
        match self.model.as_str() {
            m if m.contains("gpt-5") => 400_000,
            m if m.contains("gpt-4") => 128_000,
            _ => 128_000,
        }
    }

    async fn stream(
        &self,
        req: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": [
                { "role": "system", "content": req.system },
                ...req.messages.iter().map(to_openai_msg),
            ],
            "tools": req.tools.iter().map(to_openai_tool).collect::<Vec<_>>(),
            "stream": true,
            "temperature": req.temperature,
        });

        let response = self.client.post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network { message: e.to_string() })?;

        // SSE → ProviderEvent（OpenAI 格式：data: {json}\n\n）
        let stream = async_stream::try_stream! {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| ProviderError::Network { message: e.to_string() })?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find("\n\n") {
                    let event_str: String = buffer.drain(..pos + 2).collect();
                    for line in event_str.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            if data == "[DONE]" {
                                yield ProviderEvent::MessageStop;
                                return;
                            }
                            let parsed: serde_json::Value = serde_json::from_str(data)
                                .map_err(|e| ProviderError::Unknown { message: e.to_string() })?;

                            // 解析 OpenAI delta 格式 → ProviderEvent
                            for choice in parsed["choices"].as_array().unwrap_or(&vec![]).iter() {
                                let delta = &choice["delta"];
                                if let Some(content) = delta["content"].as_str() {
                                    yield ProviderEvent::ContentBlockDelta {
                                        index: 0,
                                        delta: content.to_string(),
                                    };
                                }
                                if let Some(tool_calls) = delta["tool_calls"].as_array() {
                                    for tc in tool_calls {
                                        yield ProviderEvent::ContentBlockStart {
                                            index: 1,
                                            block: ContentBlock::ToolUse {
                                                id: tc["id"].as_str().unwrap_or_default().to_string(),
                                                name: tc["function"]["name"].as_str().unwrap_or_default().to_string(),
                                                input: serde_json::from_str(
                                                    tc["function"]["arguments"].as_str().unwrap_or("{}")
                                                ).unwrap_or(json!({})),
                                            },
                                        };
                                    }
                                }
                                if let Some(finish) = choice["finish_reason"].as_str() {
                                    yield ProviderEvent::MessageDelta {
                                        usage: parse_openai_usage(&parsed["usage"]),
                                        stop_reason: parse_openai_stop_reason(finish),
                                    };
                                }
                            }
                        }
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn cache_key(&self, req: &ProviderRequest) -> String {
        // OpenAI automatic caching：粗略 sha256 of first user message
        let canonical = serde_json::to_string(&json!({
            "system": req.system,
            "tools": req.tools,
            "messages": &req.messages[..1.min(req.messages.len())],
        })).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(&hasher.finalize()[..16])
    }

    fn estimate_tokens(&self, req: &ProviderRequest) -> usize {
        // OpenAI 提供 tokenizer；简化用字符估算
        let chars: usize = req.system.len()
            + req.messages.iter().map(|m| serialize_msg_len(m)).sum::<usize>();
        chars / 4
    }
}
```

---

## 5. OllamaProvider（本地优先）

```rust
// smallhead-core/src/providers/ollama.rs

pub struct OllamaProvider {
    client: Client,
    model: String,
    base_url: String,    // 默认 http://localhost:11434
}

impl OllamaProvider {
    pub fn new(model: String) -> Self {
        Self {
            client: Client::new(),
            model,
            base_url: "http://localhost:11434".into(),
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str { "ollama" }
    fn model(&self) -> &str { &self.model }
    fn context_window(&self) -> usize { 32_768 }  // 默认；具体模型可查

    async fn stream(
        &self,
        req: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        // Ollama API: POST /api/chat with { "stream": true }
        // 格式与 OpenAI 类似但消息格式略有差异
        // ... 类似 OpenAIProvider 的 stream 实现
        todo!()
    }

    fn cache_key(&self, req: &ProviderRequest) -> String {
        // Ollama 无 prompt cache —— cache_key 永远返回唯一值
        let mut hasher = Sha256::new();
        hasher.update(self.model.as_bytes());
        hasher.update(serde_json::to_string(&req.messages).unwrap().as_bytes());
        hex::encode(&hasher.finalize()[..16])
    }

    fn estimate_tokens(&self, req: &ProviderRequest) -> usize {
        // Ollama 不返回 usage —— 客户端估
        let chars: usize = req.system.len()
            + req.messages.iter().map(|m| serialize_msg_len(m)).sum::<usize>();
        chars / 4
    }
}
```

---

## 6. Provider 配置

```toml
# ~/.config/smallhead/smallhead.toml
[provider]
primary = "anthropic"
fallback = "google"          # primary 失败 → fallback
haiku_model = "anthropic:claude-haiku-4-5"    # 用于 compaction 摘要

[provider.anthropic]
api_key_env = "ANTHROPIC_API_KEY"     # 强制从 env 读，不写 config
model = "claude-sonnet-4-6"
base_url = "https://api.anthropic.com"
max_retries = 3

[provider.anthropic.haiku]            # fast model for compaction
api_key_env = "ANTHROPIC_API_KEY"     # 复用 primary 的 env
max_retries = 3

[provider.openai]
api_key_env = "OPENAI_API_KEY"
model = "gpt-5"
base_url = "https://api.openai.com"
max_retries = 3

[provider.google]
api_key_env = "GOOGLE_API_KEY"
model = "gemini-2.0-pro"
max_retries = 3

[provider.ollama]
base_url = "http://localhost:11434"
model = "llama-3.3-70b"
```

**关键不变量**：
- **API key 永远从 env 读**（`api_key_env`），不写 `smallhead.toml` 也不进 session JSONL。
- `max_retries` 默认 3 —— exponential backoff with jitter。
- `fallback` 是 opt-in —— 默认不启用（避免双 provider 复杂度）。

---

## 7. Context Epoch 与 cache_key

`cache_key()` 是 Context Epoch 跟踪的基础。每次 prompt assembly 时计算 cache_key，如果变了 → epoch_id++：

```rust
// 在 AgentLoop::begin_epoch() 中（参见 specs/02 §5）
let cache_key = self.provider.cache_key(&request);
let new_epoch = if cache_key == self.state.last_cache_key {
    self.state.epoch_id
} else {
    self.state.epoch_id + 1
};
```

**Epoch 边界**：
- Compaction 完成（summary 变了，cache_key 必变）
- Session move / fork
- Provider 切换（cache_key schema 不同）
- 不兼容的 context 转换

**收益**：epoch 内所有"不变"的部分共享 provider prompt cache（Anthropic: $0.1 per 1M vs $3 per 1M = 30× 便宜）。

---

## 8. Retry 策略

所有 provider 实现都遵守同一 retry policy：

```rust
// smallhead-core/src/retry.rs

pub async fn retry<F, T, E>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut() -> BoxFuture<Result<T, E>>,
    E: HasRetryable,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if e.retryable() && attempt < policy.max_retries => {
                attempt += 1;
                let delay = policy.delay_for(attempt);
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub struct RetryPolicy {
    pub max_retries: u32,          // 默认 3
    pub base_delay: Duration,      // 默认 1s
    pub max_delay: Duration,       // 默认 30s
}

impl RetryPolicy {
    fn delay_for(&self, attempt: u32) -> Duration {
        let exp = self.base_delay * 2u32.pow(attempt - 1);
        let jitter = rand::random::<u64>() % 1000; // 1s jitter
        std::cmp::min(exp + Duration::from_millis(jitter), self.max_delay)
    }
}

pub trait HasRetryable {
    fn retryable(&self) -> bool;
}

impl HasRetryable for ProviderError {
    fn retryable(&self) -> bool {
        self.retryable()
    }
}
```

**Retryable 错误**：
- `RateLimit { .. }` — 尊重 `retry_after_ms`，如果 server 给的话
- `Server { status >= 500 }` — server error
- `Network { .. }` — connection issues

**Non-retryable**：
- `Auth` — 用户必须重新配置
- `ContextLength` — 减小输入或升级 model
- `InvalidRequest` — 代码 bug，应该 fail fast
- `Cancelled` — 用户主动取消

---

## 9. Rate Limit Handling

Anthropic 与 OpenAI 都有 rate limit。除了 retry 外，agent loop 也要尊重 `retry_after_ms`：

```rust
// 在 ProviderEvent::Error 路径中
ProviderError::RateLimit { retry_after_ms } => {
    // 1. 等待 server 给的时间
    tokio::time::sleep(Duration::from_millis(retry_after_ms)).await;
    // 2. 重试（RetryPolicy 处理）
}
```

**TUI 反馈**：rate limit 期间 emit `AgentMessage::Notification { kind: "rate_limited", payload }` 让 UI 显示 "Waiting 30s for rate limit..."。

---

## 10. Mock Provider（测试用）

```rust
// smallhead-core/src/providers/mock.rs

use std::collections::VecDeque;

pub struct MockProvider {
    responses: VecDeque<ProviderEvent>,
    pub call_count: AtomicUsize,
    pub cache_key_override: Option<String>,
}

impl MockProvider {
    pub fn new(responses: Vec<ProviderEvent>) -> Self {
        Self {
            responses: responses.into(),
            call_count: AtomicUsize::new(0),
            cache_key_override: None,
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str { "mock" }
    fn model(&self) -> &str { "mock-model" }
    fn context_window(&self) -> usize { 200_000 }

    async fn stream(
        &self,
        _: ProviderRequest,
        _: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let responses = self.responses.clone();
        let stream = async_stream::try_stream! {
            for event in responses {
                yield event;
            }
        };
        Ok(Box::pin(stream))
    }

    fn cache_key(&self, _: &ProviderRequest) -> String {
        self.cache_key_override.clone().unwrap_or_else(|| "mock-cache-key".into())
    }

    fn estimate_tokens(&self, req: &ProviderRequest) -> usize {
        let chars: usize = req.messages.iter().map(|m| serialize_msg_len(m)).sum::<usize>()
            + req.system.len();
        chars / 4
    }
}
```

**Mock 设计哲学**（来自 big-head 教训）：
- Mock 模拟真实 Provider 的事件流（包括 `Error` event）。
- 不绕过任何代码路径（permission、cache_key、token budget）。
- 测试只 swap provider，不 swap 整个 agent loop。

---

## 11. 不变量

1. **Provider trait 隔离差异**——`AnthropicProvider` / `OpenAiProvider` / `OllamaProvider` 平级。
2. **`stream()` 总是 async + `Pin<Box<dyn Stream>>`**——可重放、可中断、可重试。
3. **`cache_key()` 显式表达**——Context Epoch 跟踪的合约。
4. **API key 永远从 env 读**——不写 config、不进 session JSONL。
5. **Retryable vs Non-retryable 显式分类**——避免盲目重试 auth / context length。
6. **Rate limit 尊重 `retry_after_ms`**——不雪崩。
8. **Mock provider 走真实事件路径**——测试不绕过权限、token budget、cache_key。
9. **Provider id 稳定**——`"anthropic" | "openai" | "google" | "ollama" | "mock"`，session JSONL 引用。
10. **ProviderEvent::Error 是显式 variant**——不 panic，agent loop 可恢复。

---

## 12. 验收测试

### 12.1 Mock provider + agent loop 集成

```rust
#[tokio::test]
async fn mock_provider_drives_agent_loop() {
    let mock = MockProvider::new(vec![
        ProviderEvent::MessageStart { id: "msg-1".into(), model: "mock".into() },
        ProviderEvent::ContentBlockStart { index: 0, block: ContentBlock::Text { text: "".into() } },
        ProviderEvent::ContentBlockDelta { index: 0, delta: "Hi!".into() },
        ProviderEvent::ContentBlockStop { index: 0 },
        ProviderEvent::MessageDelta {
            usage: Usage { input_tokens: 100, output_tokens: 5, cache_read_tokens: 0, cache_write_tokens: 100 },
            stop_reason: StopReason::EndTurn,
        },
        ProviderEvent::MessageStop,
    ]);

    let loop = AgentLoop::new_test(Arc::new(mock));
    loop.run().await.unwrap();

    assert_eq!(mock.call_count.load(Ordering::SeqCst), 1);
}
```

### 12.2 Cache key 稳定

```rust
#[test]
fn cache_key_is_stable_for_same_request() {
    let p = AnthropicProvider::new("test-key".into(), "claude-sonnet-4-6".into());
    let req = ProviderRequest {
        system: "you are a helper".into(),
        messages: vec![LlmMessage::User { content: vec![ContentBlock::Text { text: "hi".into() }] }],
        tools: vec![],
        max_tokens: 1000,
        temperature: 0.0,
    };

    let k1 = p.cache_key(&req);
    let k2 = p.cache_key(&req);
    assert_eq!(k1, k2, "cache_key must be stable for identical requests");
}
```

### 12.3 Retry policy

```rust
#[tokio::test]
async fn retry_policy_does_not_retry_non_retryable() {
    let policy = RetryPolicy::default();
    let mut attempts = 0;

    let result = retry(&policy, || {
        attempts += 1;
        async move {
                Err(ProviderError::Auth)
            }.boxed()
    }).await;

    assert!(matches!(result, Err(ProviderError::Auth)));
    assert_eq!(attempts, 1, "non-retryable errors must not retry");
}
```

### 12.4 真实 provider 集成测试（CI 可选）

```rust
#[tokio::test]
#[ignore]   // 默认不跑，需要 ANTHROPIC_API_KEY
async fn anthropic_real_provider_smoke_test() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY not set");
    let provider = AnthropicProvider::new(api_key, "claude-haiku-4-5".into());

    let request = ProviderRequest {
        system: "you are a helper".into(),
        messages: vec![LlmMessage::User {
            content: vec![ContentBlock::Text { text: "say hi".into() }]
        }],
        tools: vec![],
        max_tokens: 100,
        temperature: 0.0,
    };

    let mut stream = provider.stream(request, CancellationToken::new()).await.unwrap();
    let mut got_message = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ProviderEvent::MessageStop => { got_message = true; break; }
            _ => continue,
        }
    }
    assert!(got_message);
}
```

### 12.5 ProviderEvent::Error 是 stream 的一部分

```rust
#[tokio::test]
async fn stream_error_event_does_not_panic() {
    let mock = MockProvider::new(vec![
        ProviderEvent::MessageStart { id: "msg-1".into(), model: "mock".into() },
        ProviderEvent::Error {
            error: ProviderError::RateLimit { retry_after_ms: 100 },
            retryable: true,
        },
    ]);

    let mut stream = mock.stream(test_request(), CancellationToken::new()).await.unwrap();
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ProviderEvent::Error { error, retryable } => {
                assert!(matches!(error, ProviderError::RateLimit { .. }));
                assert!(retryable);
            }
            _ => continue,
        }
    }
}
```

---

## 13. 与 opencode V2 / pi 对照

| 维度 | opencode V2 | pi | smallhead |
|------|------------|-----|-----------|
| Provider 抽象 | 显式 `stream()` | 隐式 `runLoop` 调用 | `Provider` trait + 显式 `stream()` |
| Cache key | `cache_key()` | 无（pi 不优化 cache） | `cache_key()` + Context Epoch |
| Retry | 内部 retry | 简单 try-catch | `RetryPolicy` + 显式分类 |
| Rate limit | retry with backoff | 无 | `retry_after_ms` 尊重 |
| Multi-provider | 平级（anthropic/openai/google/ollama） | 单 provider | 平级 |

---

## 14. 引用

- 上游 spec: 无
- 引用本 spec:
  - `specs/02-agent-loop.md` — `Provider::stream()` 调用点
  - `specs/04-session-storage.md` — `ProviderTurn` entry 持久化 stream 事件
- 源码: `ARCHITECTURE.md` §3.2, §9
- 方法论: `AGENT_METHODOLOGY.md` §1.3, §2.2, §2.3
- 对照: opencode V2 session.md, pi agent-loop.ts

---

## 15. 变更日志

- **2026-10-04 v0.1** — 初版。Provider trait + Anthropic / OpenAI / Ollama 实现 + cache_key + retry + rate limit。
- **未来**: 考虑为 provider 实现 `ProviderTurn::to_markdown()` —— 把 stream 事件渲染成可读 markdown（用于 `smallhead run` 单次模式的 verbose 输出）。
- **未来**: 考虑加入 GoogleProvider（Gemini API）。