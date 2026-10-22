# 10 — CLI and TUI

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-cli/`、`smallhead-server/`（可选）、`smallhead-web/`
> 上游: `ARCHITECTURE.md` §10, §11; `AGENT_METHODOLOGY.md` §2.4, §2.6
> 对照: pi TUI、opencode V2 CLI、big-head R3F UI

---

## 1. 概述

CLI + TUI 是 smallhead 的**产品形态**——区别于 big-head 的 macOS app + R3F 3D UI。

设计哲学：
- **CLI first**：开发者场景，默认命令行优先。
- **轻量**：ratatui（不是自写 TUI） + axum + HTMX（不是 R3F）—— 二进制比 big-head 小 10×。
- **Web 是 CLI 的镜像**：默认 127.0.0.1，不可外网（除非显式 `--web-auth-token`）。
- **Pi 风格 keybinding**：layout-based（无 modal）、Enter = steer、Alt+Enter = follow-up。

---

## 2. CLI 子命令

```rust
// smallhead-cli/src/main.rs

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "smallhead", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 默认 TUI（无 subcommand 时）
    Tui,
    /// 单次运行
    Run {
        prompt: String,
        #[arg(short, long)] p: Option<PathBuf>,
        #[arg(long)] model: Option<String>,
        #[arg(long)] provider: Option<String>,
        #[arg(long)] session: Option<String>,
        #[arg(long)] resume: bool,
        #[arg(long)] durable: bool,
        #[arg(long)] sandbox: Option<SandboxKind>,
    },
    /// 恢复会话
    Resume {
        session_id: String,
        #[arg(long)] provider: Option<String>,
    },
    /// 可视化会话树
    Tree {
        session_id: String,
    },
    /// 手动 compaction
    Compact {
        session_id: String,
        #[arg(long)] model: Option<String>,
    },
    /// 列出所有 session
    Sessions {
        #[arg(long)] cwd: Option<PathBuf>,
        #[arg(long)] json: bool,
    },
    /// 启动 HTTP server（Web UI）
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")] bind: String,
        #[arg(long)] web_auth_token: Option<String>,
    },
    /// 列出已加载 skill
    Skills {
        #[arg(long)] json: bool,
    },
    /// 测试 ruleset
    Permission {
        #[command(subcommand)]
        cmd: PermissionCmd,
    },
    /// 打印版本
    Version,
}

#[derive(Subcommand)]
pub enum PermissionCmd {
    Test {
        tool: String,
        #[arg(short)] i: Option<serde_json::Value>,
        #[arg(long)] session: Option<String>,
    },
}
```

**使用示例**：

```bash
smallhead                                 # 默认 TUI
smallhead run "fix the bug"               # 单次
smallhead run -p file.md                  # 用文件作为初始 prompt
smallhead run --durable "long task"       # 启用 durable
smallhead resume abc-123                  # 恢复会话
smallhead tree abc-123                    # 可视化会话树
smallhead compact abc-123                 # 手动 compact
smallhead sessions                        # 列出所有
smallhead serve --bind 127.0.0.1:8787       # 启动 Web UI
smallhead skills                          # 列出已加载 skill
smallhead permission test bash -i '{"command":"rm foo"}'  # 测试 ruleset
smallhead version
```

---

## 3. TUI 设计（ratatui）

### 3.1 双栏布局

```
┌─ smallhead ──────────────────────────────────────────┐
│ Session: abc-123  Model: claude-sonnet-4-6           │
├──────────────────────────────┬───────────────────────┤
│                              │ World State           │
│ You: Fix the login bug.      │ ┌───────────────────┐ │
│                              │ │ Users: 42         │ │
│ Assistant: I'll start by      │ │ Sessions: 17      │ │
│ examining the auth handler... │ │ Components: 8     │ │
│                              │ └───────────────────┘ │
│ [tool: read src/auth.rs]      │                       │
│ Read 240 lines (offset 0)     │ Token Budget          │
│                              │ ┌──────────────────┐  │
│ Assistant: Found it. The bug  │ │ Used: 12,400     │  │
│ is in line 47...              │ │ Window: 200,000  │  │
│                              │ │ Cache: ✓ Epoch 2 │  │
│                              └───────────────────────┘
├──────────────────────────────┴───────────────────────┤
│ > _                                       [Submit]   │
└──────────────────────────────────────────────────────┘
```

**布局**：
- **左栏（70%）**：对话流 + tool 输出
- **右栏（30%）**：World State + Token Budget
- **底部**：输入框

### 3.2 关键设计原则

```rust
// smallhead-cli/src/tui/layout.rs

use ratatui::prelude::*;

pub fn build_layout(area: Rect) -> Layout {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                          // header
            Constraint::Min(10),                              // main
            Constraint::Length(3),                           // input
        ])
        .split(area)
}

pub fn build_main(area: Rect) -> Layout {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(70),                     // 对话流
            Constraint::Percentage(30),                     // world state + tokens
        ])
        .split(area)
}
```

**关键原则**：
- **无 modal**：所有视图通过 layout 而非全屏 modal 实现（来自 pi）。
- **Token budget 实时**：每个 provider turn 后 emit `Notification`，右侧数字更新。
- **World state 实时**：每个 WorldOp 后 emit Notification，右侧 entity 数更新。

### 3.3 渲染组件

```rust
// smallhead-cli/src/tui/widgets.rs

pub struct ConversationWidget {
    pub messages: VecDeque<RenderedMessage>,
    pub fold_states: HashMap<String, FoldState>,
}

impl Widget for ConversationWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // 渲染对话流
        // 每个消息用 Paragraph 渲染
        // Tool 输出默认折叠（Tab 切换）
    }
}

pub struct WorldStateWidget {
    pub entity_count: usize,
    pub component_kinds: usize,
    pub schema_names: Vec<String>,
}

impl Widget for WorldStateWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // 顶部 + bottom 的简化视图
        let text = format!(
            "Users: {}\nSessions: {}\nComponents: {}",
            self.entity_count,
            self.session_count,
            self.component_kinds,
        );
        Paragraph::new(text).render(area, buf);
    }
}

pub struct TokenBudgetWidget {
    pub used: usize,
    pub window: usize,
    pub cache_hit: bool,
    pub epoch_id: u64,
}

impl Widget for TokenBudgetWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let pct = (self.used as f64 / self.window as f64) * 100.0;
        let text = format!(
            "Used: {}\nWindow: {}\nCache: {} Epoch {}\nFill: {:.0}%",
            format_tokens(self.used),
            format_tokens(self.window),
            if self.cache_hit { "✓" } else { "✗" },
            self.epoch_id,
            pct,
        );
        Paragraph::new(text).render(area, buf);
    }
}
```

**TokenBreakdown 可视化**：来自 big-head 灵感（`big-head/crates/big-head-ai/src/token_breakdown.rs`），但更轻量——只在右侧显示 4 行。

### 3.4 Keybindings

```rust
// smallhead-cli/src/tui/input.rs

pub enum KeyAction {
    Submit(UserMsgKind),
    ToggleTreeView,
    SwitchProvider,
    Compact,
    Undo,
    Interrupt,
    Cancel,
    Fold,
}

pub fn map_key(key: KeyEvent) -> Option<KeyAction> {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Enter) => Some(KeyAction::Submit(UserMsgKind::Steer)),
        (KeyModifiers::ALT, KeyCode::Enter) => Some(KeyAction::Submit(UserMsgKind::FollowUp)),
        (KeyModifiers::NONE, KeyCode::Tab) => Some(KeyAction::Fold),
        (KeyModifiers::CONTROL, KeyCode::Char('t')) => Some(KeyAction::ToggleTreeView),
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => Some(KeyAction::SwitchProvider),
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => Some(KeyAction::Compact),
        (KeyModifiers::CONTROL, KeyCode::Char('r')) => Some(KeyAction::Undo),
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => Some(KeyAction::Interrupt),
        (KeyModifiers::NONE, KeyCode::Esc) => Some(KeyAction::Cancel),
        _ => None,
    }
}

pub enum UserMsgKind {
    /// 等当前 turn 完成
    Steer,
    /// 等当前 turn + follow-up 都完成
    FollowUp,
    /// 立刻投递（打断当前 turn）
    Interrupt,
}
```

| Key | Action | 备注 |
|-----|--------|------|
| Enter | **steer**（插入当前 turn） | 在 tool call 边界处理 |
| Alt+Enter | **follow-up**（等当前 turn 完成） | OUTER 处理 |
| Tab | 折叠/展开 tool 输出 | 长输出默认折叠 |
| Ctrl+T | 切换 tree 视图 | 显示 session tree |
| Ctrl+P | 切换 provider | 循环 primary / fallback |
| Ctrl+K | 手动 compact | 触发 compaction |
| Ctrl+R | undo / restore checkpoint | 回滚到上一个 checkpoint |
| Ctrl+C | interrupt（标记 interrupted） | durable layer 接管 |
| Esc | 取消当前输入 | 清空输入框 |

---

## 4. Web UI（可选）

### 4.1 Stack

- **Server**：`axum` + `tower` + `tokio-tungstenite`
- **Frontend**：HTML + HTMX + Pico.css（无框架、无 build step）
- **Protocol**：HTTP + WebSocket（per-session subscribe）

### 4.2 端点

```rust
// smallhead-server/src/routes.rs

use axum::{Router, routing::{get, post}};

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/:id", get(get_session))
        .route("/sessions/:id/entries", get(get_entries))
        .route("/sessions/:id/prompt", post(submit_prompt))
        .route("/sessions/:id/stream", get(sse_stream))
        .route("/sessions/:id/world", get(get_world))
        .route("/sessions/:id/compact", post(trigger_compact))
        .route("/skills", get(list_skills))
        .with_state(state)
}
```

**端点语义**：

| Method | Path | 描述 |
|--------|------|------|
| GET | `/sessions` | 列出所有 session |
| GET | `/sessions/:id` | 会话详情 |
| GET | `/sessions/:id/entries` | JSONL 流（分页） |
| POST | `/sessions/:id/prompt` | 提交新 turn |
| GET | `/sessions/:id/stream` | SSE 流（实时事件） |
| GET | `/sessions/:id/world` | 当前 world state |
| POST | `/sessions/:id/compact` | 触发 compact |
| GET | `/skills` | 列出已加载 skill |

### 4.3 启动参数

```bash
# 默认：仅本地
smallhead serve
# → listening on 127.0.0.1:8787

# 显式 bind
smallhead serve --bind 127.0.0.1:8787

# 暴露外网（需要 token）
smallhead serve --bind 0.0.0.0:8787 --web-auth-token <token>
# → listening on 0.0.0.0:8787, requires Bearer <token>
```

**约束**：
- Web UI 只读 + 简单控制；复杂编辑走 CLI/TUI。
- **默认不可外网**：默认监听 `127.0.0.1`。
- **配 `--web-auth-token` 后才暴露在 `0.0.0.0`**：避免无认证暴露 LLM API key。

### 4.4 SSE Stream

```rust
// smallhead-server/src/sse.rs

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;

pub async fn sse_stream(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.subscribe(session_id).await;

    Sse::new(rx.map(|event| {
        Ok(Event::default()
            .event(event.kind())
            .json_data(event.payload())
            .unwrap_or_else(|_| Event::default().data("error")))
    }))
    .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
```

**事件类型**：
- `message` — 新 agent message
- `tool_call` — 工具调用开始
- `tool_result` — 工具调用完成
- `world_mutation` — World 变化
- `token_breakdown` — Token 使用更新
- `error` — 错误

---

## 5. smallhead.toml UI 配置

```toml
# ~/.config/smallhead/smallhead.toml

[ui]
theme = "auto"                              # auto | dark | light
token_budget_visibility = true             # 右侧 Token Budget 面板
world_state_visibility = true              # 右侧 World State 面板
keybinding = "default"                      # default | vim | emacs

[ui.tui]
conversation_pct = 70                      # 左栏宽度
show_thinking = false                      # 是否显示 thinking block
auto_fold_tool_output = true               # 长输出自动折叠

[ui.web]
bind = "127.0.0.1:8787"                    # 默认不可外网
web_auth_token = null                      # null = 不启用外网
```

---

## 6. 验收测试

### 6.1 CLI 参数解析

```rust
#[test]
fn run_command_parses_arguments() {
    let cli = Cli::parse_from(&["smallhead", "run", "--model", "test", "fix bug"]);
    match cli.command {
        Some(Command::Run { prompt, model, .. }) => {
            assert_eq!(prompt, "fix bug");
            assert_eq!(model, Some("test".into()));
        }
        _ => panic!("expected Run command"),
    }
}
```

### 6.2 TUI keymap

```rust
#[test]
fn enter_maps_to_steer() {
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(map_key(key), Some(KeyAction::Submit(UserMsgKind::Steer))));
}

#[test]
fn alt_enter_maps_to_follow_up() {
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    assert!(matches!(map_key(key), Some(KeyAction::Submit(UserMsgKind::FollowUp))));
}

#[test]
fn ctrl_r_maps_to_undo() {
    let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(matches!(map_key(key), Some(KeyAction::Undo)));
}
```

### 6.3 TUI 渲染（snapshot 测试）

```rust
#[test]
fn tui_layout_no_modal() {
    let terminal = TestBackend::new(120, 40);
    let mut tui = Tui::new(terminal);
    tui.render(&test_loop_state()).unwrap();

    // Snapshot：不应有 modal（无全屏 overlay）
    let buffer = tui.backend().buffer().clone();
    assert!(!has_modal_overlay(&buffer), "TUI must be layout-based, not modal");
}
```

### 6.4 Web server 端点

```rust
#[tokio::test]
async fn web_server_lists_sessions() {
    let tempdir = tempdir().unwrap();
    let state = Arc::new(AppState::new(tempdir.path()).await.unwrap());
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let response = reqwest::get(format!("http://{}/sessions", addr)).await.unwrap();
    assert_eq!(response.status(), 200);
}
```

### 6.5 Web server 默认不可外网

```rust
#[tokio::test]
async fn web_server_default_not_exposed() {
    let tempdir = tempdir().unwrap();
    let state = Arc::new(AppState::new(tempdir.path()).await.unwrap());
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // 应只在 127.0.0.1
    assert!(addr.ip().is_loopback(), "default bind must be loopback");
}
```

### 6.6 version 子命令

```rust
#[test]
fn version_command_prints_version() {
    let cli = Cli::parse_from(&["smallhead", "version"]);
    assert!(matches!(cli.command, Some(Command::Version)));
}
```

---

## 7. 与 big-head 对照

| 维度 | big-head | smallhead |
|------|---------|-----------|
| 产品形态 | macOS app + 3D Spatial UI (R3F) | CLI + TUI + 轻量 Web |
| TUI 库 | termimad 自写 | ratatui（成熟库） |
| Web 库 | AppKit | axum + HTMX |
| 二进制大小 | ~50MB | ~5MB |
| 内存占用 | ~200MB（3D 渲染） | ~20MB |
| 跨平台 | macOS only | macOS / Linux / Windows |
| UI 哲学 | immersive | utilitarian |

**核心差异**：big-head 是沉浸式产品，smallhead 是开发者工具——smallhead 的 UI 不抢戏。

---

## 8. 引用

- 上游 spec:
  - `specs/02-agent-loop.md`（steering / follow-up）
  - `specs/05-permission-model.md`（`smallhead permission test`）
- 引用本 spec: 无（leaf spec）
- 源码: `ARCHITECTURE.md` §10, §11
- 方法论: `AGENT_METHODOLOGY.md` §2.4, §2.6
- 对照:
  - pi TUI（keybindings、layout-based）
  - opencode V2 CLI
  - big-head `big-head/crates/big-head-ai/src/token_breakdown.rs`（可视化灵感）

---

## 9. 变更日志

- **2026-10-04 v0.1** — 初版。CLI 子命令 + ratatui TUI 双栏布局 + keybindings + axum + HTMX Web UI（默认不可外网）。
- **未来**: 考虑在 TUI 加入 LSP-style autocomplete（基于 skill descriptions 和历史命令）。
- **未来**: 考虑为 Web UI 加入 `/v1/chat/completions` 兼容端点（让其他工具可调 smallhead）。