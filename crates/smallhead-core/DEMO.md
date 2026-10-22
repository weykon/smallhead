# smallhead MVP Demo

> 目标：用 **真实 MiniMax API（默认）** 或 **Mock 模式（fallback）** 跑通一个最小 agent loop。

---

## 一句话介绍

`smallhead-core/examples/demo.rs` 跑的是一个完整的 agent loop：

```
[user] → [LLM: tool_call] → [execute echo] → [tool_result] → [LLM: final text]
```

**默认走 MiniMax 真实 API**（Anthropic 协议，`https://api.minimaxi.com/anthropic`）。
未设置 `MINIMAX_API_KEY` 时自动 fallback 到 `MockProvider`（0 网络、0 成本）。

---

## 运行步骤

### 1. 真实 API 模式（推荐）

```bash
cd /Users/weykon/Desktop/p/small-head
cargo run --example demo -p smallhead-core
```

demo 会自动从仓库根的 `.env` 加载 `MINIMAX_API_KEY`（手写 ~20 行 .env parser，无第三方依赖）。
也支持提前 export：

```bash
export MINIMAX_API_KEY="sk-..."   # 优先级高于 .env
cargo run --example demo -p smallhead-core
```

期望看到：
```
=== smallhead MVP demo ===

[provider] using real MiniMax API (model=MiniMax-M3, base_url=https://api.minimaxi.com/anthropic)
[provider] hint: set RUST_LOG=smallhead_core=debug to see SSE events
```

实际 LLM 响应会因模型决定而不同：
- **可能**：调 `echo` 工具 → 拿到 `[echo] hello` → 给最终文本
- **可能**：直接回答"Hello!" 之类的纯文本

两种都算成功，证明完整 SSE 流正确解析。

### 2. Mock fallback 模式

```bash
unset MINIMAX_API_KEY
cargo run --example demo -p smallhead-core
# 或显式 --mock
cargo run --example demo -p smallhead-core -- --mock "Echo hello"
```

期望看到：
```
=== smallhead MVP demo ===

[provider] MINIMAX_API_KEY not set → using MockProvider (offline mode)
```

确定性地走完整闭环：
- turn 1 → tool_call(echo)
- turn 2 → 文本 "Tool executed successfully."

### 3. CLI 参数（自由测试 prompt）

不需要改 Rust 代码就能换 prompt / system / 模型：

```bash
# 默认（演示 agent 自我介绍 + 用工具的中文 meta prompt）
cargo run --example demo -p smallhead-core

# 自定义 user prompt
cargo run --example demo -p smallhead-core -- "Please echo the word 'hello'"

# 自定义 system prompt
cargo run --example demo -p smallhead-core -- --system "You are a pirate." "Say hi"

# 强制 Mock 模式（即使设置了 MINIMAX_API_KEY）
cargo run --example demo -p smallhead-core -- --mock "Echo hello"

# 看完整帮助
cargo run --example demo -p smallhead-core -- --help
```

支持的 flag：

| Flag | 作用 |
|------|------|
| `--help` / `-h` | 打印帮助 |
| `--mock` | 强制 MockProvider（跳过 API 调用） |
| `--system <text>` | 覆盖 system prompt |
| `--model <id>` | 覆盖模型 ID |
| 位置参数（第一个）| 作为 user prompt |

位置参数优先级：CLI > hardcoded 默认值。prompt 不带 `--` 的话不会被解析成 flag。

---

## 配置项（`MiniMaxConfig`）

```rust
use smallhead_core::{MiniMaxConfig, MiniMaxProvider};

let cfg = MiniMaxConfig::new("sk-...")
    .with_model("minimax-m2")              // 默认 MiniMax-M3
    .with_base_url("https://proxy/foo/anthropic");  // 默认 https://api.minimaxi.com/anthropic

let provider = MiniMaxProvider::new(cfg);
```

也可以 `MiniMaxConfig::from_env()` 自动读 `MINIMAX_API_KEY`（未设置则返回 `Err`）。

---

## 预期输出（Mock fallback 完整版）

```text
=== smallhead MVP demo ===

INFO  smallhead_core::agent_loop: AgentLoop::run_one_turn start
INFO  smallhead_core::agent_loop: assistant: 0 content blocks, 1 tool_calls, stop=ToolUse
INFO  smallhead_core::agent_loop: assistant: 1 content blocks, 0 tool_calls, stop=EndTurn
INFO  smallhead_core::agent_loop: Run finished: 2 turns, 1 tool_calls

=== Summary ===
turns_completed: 2
tool_calls_executed: 1

=== Final messages ===
[0] [user] Please echo the word 'hello'.
[1] [assistant] (tool_calls: echo({"text":"hello"})) text=
[2] [tool_result:tc_mock_1] [echo] hello
[3] [assistant] Tool executed successfully.
```

解读：
- **turn 1**：模型调出 `echo` 工具，输入 `{"text":"hello"}`。
- **agent_loop** 找到名为 `echo` 的 tool → 调用 `EchoTool::execute()` → 返回 `"[echo] hello"`。
- **turn 2**：把 tool_result 喂回模型 → 模型返回最终文本 `"Tool executed successfully."`。
- **退出**：没有 tool_call，结束 INNER loop。

---

## 代码结构（4 个文件 + 1 个 example + 1 个 provider 子模块）

```
crates/smallhead-core/
├── Cargo.toml              # + reqwest + tokio-stream + bytes
├── src/
│   ├── lib.rs              # 重新导出（含 MiniMaxProvider）
│   ├── message.rs          # AgentMessage enum + ToLlmMessage trait
│   ├── provider/
│   │   ├── mod.rs          # Provider trait + MockProvider
│   │   └── minimax.rs      # MiniMaxProvider（Anthropic 协议真实 API + 手写 SSE 解析）
│   ├── tool.rs             # ToolKind enum + EchoTool
│   └── agent_loop.rs       # AgentLoop::run_one_turn
└── examples/
    └── demo.rs             # 自动检测 MINIMAX_API_KEY，fallback 到 Mock
```

### `src/provider/mod.rs` — Provider trait + MockProvider

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn model(&self) -> &str;
    fn context_window(&self) -> usize;
    async fn stream(&self, req: ProviderRequest)
        -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError>;
    fn cache_key(&self, req: &ProviderRequest) -> String;
}
```

### `src/provider/minimax.rs` — MiniMaxProvider（真实 API）

关键设计：
- **端点**：`https://api.minimaxi.com/anthropic/v1/messages`（兼容 Anthropic Messages API）
- **鉴权**：`x-api-key: <MINIMAX_API_KEY>` + `anthropic-version: 2023-06-01`
- **请求体**：标准 Anthropic 格式（`model` / `messages` / `tools` / `system` / `stream: true`）
- **响应**：SSE 流 → 解析为 `ProviderEvent`
- **SSE 解析器**：手写 ~50 行（避免引入 `eventsource-stream` 依赖）
  - 处理 `\r\n` 与 `\n` 两种行结束
  - 处理 chunk 跨行的情况（partial line buffer）
  - 忽略 `:` 开头的注释行
- **事件映射**：
  - `message_start` → `ProviderEvent::MessageStart`
  - `content_block_start` → `ProviderEvent::ContentBlockStart`
  - `content_block_delta` → `ProviderEvent::ContentBlockDelta`（累积）
  - `content_block_stop` → `ProviderEvent::ContentBlockStop`
  - `message_delta` → `ProviderEvent::MessageDelta`
  - `message_stop` → `ProviderEvent::MessageStop`

模型 ID：
- 默认 `MiniMax-M3`（big-head `ai_api_provider::provider.rs:229`）
- 可选 `minimax` / `minimax-m2`
- 上下文窗口 204_800（见 big-head `tokens.rs:164`）

---

## 跑测试

```bash
cargo test -p smallhead-core
```

预期 17 个测试通过：
```
running 17 tests
test message::tests::user_message_converts_to_llm ... ok
test message::tests::notification_does_not_convert_to_llm ... ok
... (12 more)
test provider::minimax::tests::parse_sse_emits_event_on_blank_line ... ok
test provider::minimax::tests::parse_sse_handles_cr_lf_and_split_chunks ... ok
test provider::minimax::tests::endpoint_appends_v1_messages ... ok
test provider::minimax::tests::endpoint_does_not_double_append ... ok
test provider::minimax::tests::request_body_has_stream_true_and_tools ... ok
test result: ok. 17 passed; 0 failed
```

---

## 下一步

参考 `ARCHITECTURE.md § 19 Roadmap`：

| v | 工作 | 关键文件 |
|---|------|---------|
| **v0.1 (现在)** | AgentMessage + Provider + ToolKind + AgentLoop + MockProvider + **MiniMaxProvider** | ✅ `crates/smallhead-core/` |
| **v0.2** | ECS integration + 11 WorldOp tools + Checkpoint + TUI + Compaction | `crates/smallhead-ecs/` + `smallhead-session/` |
| **v0.3** | OpenAI + Ollama provider + Skill loader + Tree 视图 | `provider/` 拆分 + `smallhead-skill/` |
| **v0.4** | Durable layer opt-in + Context Epoch + Web server + 沙箱 | `smallhead-server/` + Durable 实现 |

---

## 调试 tips

```bash
# 看 SSE 事件流
RUST_LOG=smallhead_core=debug cargo run --example demo -p smallhead-core

# 看 agent_loop 状态机
RUST_LOG=smallhead_core::agent_loop=trace cargo run --example demo -p smallhead-core

# 切换到 miniMax-m2 模型
MINIMAX_API_KEY=sk-... cargo run --example demo -p smallhead-core
```

（v0.2 会加入 Usage 报告 + Context Epoch cache 命中日志）

---

## 注意事项

1. **MockProvider 仅用于单元测试 / 离线开发**。生产必须用真实 provider。
2. **MiniMaxProvider 走 Anthropic 协议**。如果未来要支持 OpenAI / Gemini / Ollama，需要新建 `provider/openai.rs` / `provider/gemini.rs` 子模块（不破坏现有 Provider trait）。
3. **MVP 不包含**：permission ruleset、session 持久化、ECS world、provider cache 命中、compaction、doom_loop 检测 —— 都在后续 v 加入。
4. **MVP 包含**：完整 message 模型 + provider trait + tool dispatch + agent loop + 真实 API（MiniMax）+ Mock fallback。所有后续 v 都基于此扩展。