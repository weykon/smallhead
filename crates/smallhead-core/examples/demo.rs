//! `smallhead MVP demo` — 跑通一个最小 agent loop。
//!
//! ## 运行
//!
//! ```bash
//! # 默认：主 Monad 自动发现、派发业务并汇总结果
//! cargo run --example demo -p smallhead-core
//!
//! # 旧版普通工具模式：添加 --direct-tools
//! # 计算业务优先快速模型：添加 --prefer-fast=laya 或 --prefer-fast=qwen
//! # 自定义 user prompt
//! cargo run --example demo -p smallhead-core -- "请回显单词 'hello'"
//!
//! # 自定义 system prompt
//! cargo run --example demo -p smallhead-core -- --system "请扮演一名海盗。"
//!
//! # 切换模型
//! cargo run --example demo -p smallhead-core -- --model minimax-m2 "请回显 hello"
//!
//! # 强制 Mock（即使有 MINIMAX_API_KEY）
//! cargo run --example demo -p smallhead-core -- --mock "请回显 hello"
//!
//! # v0.4 session 持久化
//! cargo run --example demo -p smallhead-core -- --list-sessions
//! cargo run --example demo -p smallhead-core -- --resume <session-id> "继续"
//! cargo run --example demo -p smallhead-core -- --no-save "不要写入"
//!
//! # 看帮助
//! cargo run --example demo -p smallhead-core -- --help
//! ```
//!
//! ## 两种 provider 模式
//!
//! 1. **真实 API**（默认）：设置 `MINIMAX_API_KEY` 环境变量后，跑 MiniMax-M3 真实模型。
//! 2. **Mock 模式**：`--mock` 标志，或未设置 `MINIMAX_API_KEY` 时自动 fallback。
//!
//! ## 预期输出（真实模式）
//!
//! 真实 LLM 不一定会调出 echo 工具；它可能直接给一段文字回复。
//! Mock 模式则会强制走 "调工具 → 给最终文本" 的完整闭环（见 `MockProvider`）。

use std::sync::Arc;

use smallhead_core::{
    AddTool, AgentLoop, AgentMessage, ArtifactReadTool, BashTool, CalculateTool, ContentBlock,
    DivTool, EchoTool, EditTool, MiniMaxProvider, MockProvider, MockResponse, MulTool, ReadTool,
    SubTool, ToolKind, WriteTool,
};
use smallhead_session::{new_session_id, now_millis, JsonFileStore, Session, SessionStore};

/// CLI 参数。
#[derive(Debug, Default)]
struct CliArgs {
    /// 第一个非 flag 参数作为 user prompt
    user_prompt: Option<String>,
    /// --system <text>
    system: Option<String>,
    /// --model <id>
    model: Option<String>,
    /// --mock 强制 MockProvider
    force_mock: bool,
    /// --help 打印帮助
    help: bool,
    /// --resume <id> 恢复已有 session
    resume: Option<String>,
    /// --list-sessions 列出所有 session 后退出
    list_sessions: bool,
    /// --no-save 跑完后不写 session 文件
    no_save: bool,
    direct_tools: bool,
    fast_backend: Option<smallhead_core::fast_route::FastBackend>,
}

/// 默认 user prompt：中文 meta 问题（演示 agent 自我介绍 + 用工具）。
const DEFAULT_USER_PROMPT: &str = "你好\
 帮我算一下 一百加一百\
 然后再算一个100乘以一百
";

/// 默认 system prompt。
const DEFAULT_SYSTEM_PROMPT: &str = "你是一名乐于助人的助手，正在演示 smallhead 的 Agent 核心。\n\
         可用工具：\n\
         - echo(text)            : 回显一段文本\n\
         - bash(command, timeout_ms?) : 执行 shell 命令（YOLO 模式，无沙箱）。\n         - read(path, offset?, limit?) : 分页读取文件（行号从 1 开始）。\n         - edit(path, search, replace, replace_globally?) : 查找并替换文本。\n         - write(path, content)  : 备份后覆盖写入文件。\n         - calculate(expression) : 算术业务统一入口。**必须先调它**，再在下一轮调具体算子。\n\
         - artifact_read(ref, offset?, limit?) : 读取大工具输出的完整 artifact 分页。\n\
         - add/sub/mul/div(a, b) : 细粒度算子（calculate 之后调用）\n\
         \n\
         执行流程：\n\
         1. 算术请求 → 第一轮调 calculate({expression: '1+2'})\n\
         2. 拿到 hint 后 → 第二轮调 add({a: 1, b: 2}) 等具体算子\n\
         3. 用算子的结果作为最终回答。\n\
         \n\
         调用 echo 时，将用户原词填写为 {\"text\": \"<word>\"}；禁止用空对象 {} 调用。";

/// 极简 CLI parser（避免引入 clap 等依赖）。
///
/// 支持：
/// - 位置参数：第一个非 flag 作为 user prompt
/// - `--system <text>`：覆盖 system prompt
/// - `--model <id>`：覆盖模型 ID（仅 MockProvider 可用；MiniMax 模型切换需代码侧）
/// - `--mock`：强制 MockProvider
/// - `--resume <id>`：恢复 session
/// - `--list-sessions`：列出会话后退出
/// - `--no-save`：跑完后不写 session
/// - `--help` / `-h`：打印帮助
fn parse_cli() -> anyhow::Result<CliArgs> {
    parse_cli_from(std::env::args().skip(1))
}

fn parse_cli_from(values: impl IntoIterator<Item = String>) -> anyhow::Result<CliArgs> {
    let mut args = CliArgs::default();
    let mut iter = values.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--help" | "-h" => {
                args.help = true;
            }
            "--direct-tools" => {
                args.direct_tools = true;
            }
            "--prefer-fast=laya" => {
                args.fast_backend = Some(smallhead_core::fast_route::FastBackend::Laya);
            }
            "--prefer-fast=qwen" => {
                args.fast_backend = Some(smallhead_core::fast_route::FastBackend::Qwen);
            }
            "--mock" => {
                args.force_mock = true;
            }
            "--no-save" => {
                args.no_save = true;
            }
            "--list-sessions" => {
                args.list_sessions = true;
            }
            "--system" => {
                args.system = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--system 需要一个参数"))?,
                );
            }
            "--model" => {
                args.model = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--model 需要一个参数"))?,
                );
            }
            "--resume" => {
                args.resume = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--resume 需要一个 session id 参数"))?,
                );
            }
            other if other.starts_with("--") || other.starts_with('-') => {
                anyhow::bail!("未知 flag: {other} （用 --help 看用法）");
            }
            _ => {
                if args.user_prompt.is_none() {
                    args.user_prompt = Some(a);
                } else {
                    // 多余参数透传给模型（一些模型要 multimodal 多 part）
                }
            }
        }
    }
    if args.direct_tools && args.fast_backend.is_some() {
        anyhow::bail!("--prefer-fast 仅用于默认 Monad 模式，不能与 --direct-tools 一起使用");
    }
    Ok(args)
}

fn print_help() {
    println!(
            "smallhead MVP demo

USAGE:
    cargo run --example demo -p smallhead-core -- [USER_PROMPT] [OPTIONS]

ARGS:
    [USER_PROMPT]      第一个位置参数作为 user prompt

OPTIONS:
    --direct-tools     使用旧版普通工具模式（默认使用 Monad 业务派发）
    --prefer-fast=laya  计算业务优先使用本机 Laya
    --prefer-fast=qwen  计算业务优先使用本机 Qwen
    --system <text>    覆盖 system prompt
    --model <id>       覆盖模型 ID（仅 MockProvider 生效；MiniMax 改用 .env 配 MINI\\u0041MAX_MODEL）
    --mock             强制 MockProvider（跳过 API 调用）
    --resume <id>      恢复已有 session（id 可从 --list-sessions 拿到）
    --list-sessions    列出所有 session 后退出（不跑 agent）
    --no-save          跑完后不写 session 文件
    -h, --help         打印本帮助

ENV:
    MINIMAX_API_KEY    真实 API 模式（自动检测）
    RUST_LOG           tracing 日志级别（默认 info）

SESSION 存储位置：
    ~/.smallhead/sessions/{{uuid-v7}}.json + index.json

示例：
    cargo run --example demo -p smallhead-core -- \"请回显 hello\"
    cargo run --example demo -p smallhead-core -- --system \"请扮演一名海盗\" \"打个招呼\"
    cargo run --example demo -p smallhead-core -- --mock \"请回显 world\"
    cargo run --example demo -p smallhead-core -- --list-sessions
    cargo run --example demo -p smallhead-core -- --resume 0192f... \"继续这个对话\"
"
        );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 0a. 解析 CLI
    let cli = parse_cli()?;
    if cli.help {
        print_help();
        return Ok(());
    }

    // 0b. 自动加载 .env（从 CWD 向上找）—— 用户不用手动 export
    load_dotenv();

    // 0c. tracing 初始化
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!("=== smallhead MVP demo ===\n");

    let parent_task_id = smallhead_core::TaskId::new();
    println!(
        "[mode] {}；当前主任务 {}",
        if cli.direct_tools {
            "普通工具"
        } else {
            "Monad 业务派发"
        },
        parent_task_id.0
    );
    // 1. 选择 provider
    let use_real_api = !cli.force_mock
        && std::env::var("MINIMAX_API_KEY")
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);

    let provider: Arc<dyn smallhead_core::Provider> = if use_real_api {
        let key = std::env::var("MINIMAX_API_KEY").unwrap();
        let cfg = if let Some(model) = &cli.model {
            smallhead_core::MiniMaxConfig::new(key).with_model(model.clone())
        } else {
            smallhead_core::MiniMaxConfig::new(key)
        };
        let url = cfg.base_url.clone();
        let model = cfg.model.clone();
        println!("[provider] using real MiniMax API (model={model}, base_url={url})");
        println!("[provider] hint: set RUST_LOG=smallhead_core=debug to see SSE events\n");
        Arc::new(MiniMaxProvider::new(cfg))
    } else {
        let reason = if cli.force_mock {
            "--mock 标志"
        } else {
            "MINIMAX_API_KEY 未设置"
        };
        println!("[provider] using MockProvider (offline mode, reason: {reason})");
        println!("[provider] hint: 去掉 --mock 或 export MINIMAX_API_KEY=... 启用真实 API\n");
        if !cli.direct_tools {
            println!("[mock] 固定演示两项计算；不会理解自定义请求。快速模型选项仍会访问本机服务。");
            Arc::new(MockProvider::new(vec![
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: serde_json::json!({"action":"discover"}),
                },
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: serde_json::json!({"action":"dispatch","monad_id":"calculation","objective":"一百加一百","inputs":{"expression":"100+100"}}),
                },
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: serde_json::json!({"action":"dispatch","monad_id":"calculation","objective":"一百乘以一百","inputs":{"expression":"100*100"}}),
                },
                MockResponse::AssistantText {
                    text: "100 + 100 = 200；100 × 100 = 10000。".into(),
                },
            ]))
        } else {
            Arc::new(MockProvider::new(vec![
                MockResponse::ToolCall {
                    name: "echo".into(),
                    input: serde_json::json!({"text": "hello"}),
                },
                MockResponse::AssistantText {
                    text: "工具执行成功。".into(),
                },
            ]))
        }
    };

    // 2. 注册工具:echo + bash/read/edit/write/artifact_read + 算子/计算业务
    let tools = if !cli.direct_tools {
        let mut model_preferences = std::collections::BTreeMap::new();
        if let Some(backend) = cli.fast_backend {
            use smallhead_core::fast_route::{FastBackend, ModelPreference};
            let (env_key, default_url) = match backend {
                FastBackend::Laya => ("SMALLHEAD_LAYA_URL", "http://127.0.0.1:3031"),
                FastBackend::Qwen => ("SMALLHEAD_QWEN_URL", "http://127.0.0.1:3032"),
            };
            model_preferences.insert(
                "calculation".into(),
                ModelPreference::PreferFast {
                    backend,
                    endpoint: std::env::var(env_key).unwrap_or_else(|_| default_url.into()),
                    timeout_ms: 5000,
                },
            );
        }
        vec![ToolKind::MonadBridge(
            smallhead_core::monad_bridge::MonadBridgeTool {
                parent_task_id: parent_task_id.clone(),
                model_preferences,
            },
        )]
    } else {
        vec![
            ToolKind::Echo(EchoTool),
            ToolKind::Bash(BashTool),
            ToolKind::ProcessControl(smallhead_core::execution::ProcessTool),
            ToolKind::Read(ReadTool),
            ToolKind::ArtifactRead(ArtifactReadTool),
            ToolKind::Edit(EditTool),
            ToolKind::Write(WriteTool),
            ToolKind::Add(AddTool),
            ToolKind::Sub(SubTool),
            ToolKind::Mul(MulTool),
            ToolKind::Div(DivTool),
            ToolKind::Calculate(CalculateTool),
        ]
    };

    // 3. session 存储（v0.4）：默认 ~/.smallhead/sessions
    let store = match JsonFileStore::default_location() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[session] 无法创建默认 store（{e}），跳过 save");
            None
        }
    };

    // --list-sessions 模式：列出来就退出
    if cli.list_sessions {
        match &store {
            Some(s) => match s.list().await {
                Ok(metas) => {
                    if metas.is_empty() {
                        println!("[sessions] 暂无 session");
                    } else {
                        println!(
                            "[sessions] 共 {} 个 session（按 updated_at 倒序）:",
                            metas.len()
                        );
                        for m in metas {
                            let preview =
                                m.first_user_message.as_deref().unwrap_or("(no user msg)");
                            println!(
                                "  {} | msgs={} | {} | {}",
                                m.id,
                                m.message_count,
                                preview,
                                chrono_iso(m.updated_at)
                            );
                        }
                    }
                }
                Err(e) => eprintln!("[sessions] list failed: {e}"),
            },
            None => println!("[sessions] 无可用 store"),
        }
        return Ok(());
    }

    // 4. 解析 prompt：CLI > 默认
    let monad_system = "你负责调度业务 Monad。先通过 monad_bridge 的 discover 读取目录，再根据用户请求选择业务并填写 dispatch 参数。多个操作分别派发，遵守顺序和数据依赖。只根据实际业务结果回复，不自行计算或编造结果。WaitingContext 表示尚未完成，应补充信息或询问用户。目录外能力不可假装执行。";
    let system_prompt = cli.system.as_deref().unwrap_or(if cli.direct_tools {
        DEFAULT_SYSTEM_PROMPT
    } else {
        monad_system
    });
    let user_prompt = cli.user_prompt.as_deref().unwrap_or(DEFAULT_USER_PROMPT);

    println!("[system] {}\n", system_prompt);
    println!("[user] {}\n", user_prompt);

    let mut loop_ = AgentLoop::new(provider, tools, system_prompt);

    // 5. session resume（--resume <id>）—— 把 messages 恢复进 AgentLoop
    let mut session_created_at: i64 = now_millis();
    let session_id: String = if let Some(id) = &cli.resume {
        if let Some(store) = &store {
            match store.load(id).await {
                Ok(s) => {
                    println!(
                        "[session] resuming {} ({} messages, created {})",
                        s.id,
                        s.messages.len(),
                        chrono_iso(s.created_at)
                    );
                    session_created_at = s.created_at;
                    // 把 messages 注入 AgentLoop.state
                    loop_.state.messages = s.messages.clone();
                    s.id
                }
                Err(e) => {
                    eprintln!("[session] resume 失败: {e}");
                    anyhow::bail!("resume failed: {}", e);
                }
            }
        } else {
            anyhow::bail!("--resume 需要 session store（HOME 未设置？）");
        }
    } else {
        // 新 session
        let id = new_session_id();
        if let Some(store) = store.as_ref().filter(|_| !cli.no_save) {
            let s = Session::new(id.clone(), system_prompt);
            if let Err(e) = store.save(&s).await {
                eprintln!("[session] 初始化 save 失败: {e}");
            } else {
                println!("[session] new session: {id}");
            }
            session_created_at = s.created_at;
        }
        id
    };

    // 6. 投递 user message
    let user_msg = AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: user_prompt.to_string(),
        }],
        timestamp: chrono_now(),
    };

    // 7. 运行一轮
    let (turns_completed, tool_calls_executed) = if cli.direct_tools {
        let summary = loop_.run_one_turn(user_msg).await?;
        (summary.turns_completed, summary.tool_calls_executed)
    } else {
        use smallhead_core::{
            main_monad_id, Budget, MainMonad, MonadHandoff, MonadLoop, MonadResult, ResultSlot,
            TraceMeta, SCHEMA_VERSION_V1,
        };
        let mut main = MainMonad::wrap(loop_);
        main.start(MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.into(),
            task_id: parent_task_id,
            parent_task_id: None,
            monad_id: main_monad_id().clone(),
            attempt_id: 1,
            objective: user_prompt.into(),
            inputs: serde_json::Map::new(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "根据实际业务结果完成用户请求，信息不足时明确说明".into(),
            result_slot: ResultSlot("main.reply".into()),
            depends_on: vec![],
            budget: Budget::default(),
            issued_at_ms: now_millis(),
            trace: TraceMeta {
                frame_ref: session_id.clone(),
                request_id: smallhead_core::TaskId::new().0,
            },
        })
        .await?;
        println!("[main] 开始模型与业务循环，等待 API 响应……");
        let result = main.step().await?;
        println!("[parent result] {}", serde_json::to_string_pretty(&result)?);
        let counts = match &result {
            MonadResult::Completed { output, .. } => (
                output["turns_completed"].as_u64().unwrap_or(0) as usize,
                output["tool_calls_executed"].as_u64().unwrap_or(0) as usize,
            ),
            _ => anyhow::bail!("主任务未完成：{result:?}"),
        };
        loop_ = main
            .inner
            .take()
            .ok_or_else(|| anyhow::anyhow!("主 Monad 缺少 AgentLoop"))?;
        counts
    };

    // 8. 自动 save（除非 --no-save 或 store 不可用）
    if !cli.no_save {
        if let Some(store) = &store {
            let now = now_millis();
            let session = Session {
                id: session_id.clone(),
                created_at: session_created_at,
                updated_at: now,
                system_prompt: system_prompt.to_string(),
                artifact_root: None,
                messages: loop_.state.messages.clone(),
            };
            if let Err(e) = store.save(&session).await {
                eprintln!("[session] save 失败: {e}");
            } else {
                println!("[session] saved as {}", session_id);
            }
        }
    }

    // 9. 打印结果
    println!("\n=== Summary ===");
    println!("turns_completed: {}", turns_completed);
    println!("tool_calls_executed: {}", tool_calls_executed);

    println!("\n=== Final messages ===");
    for (i, msg) in loop_.state.messages.iter().enumerate() {
        println!("[{}] {}", i, format_message(msg));
    }

    Ok(())
}

/// 简单格式化 message（MVP demo 用）。
fn format_message(msg: &AgentMessage) -> String {
    match msg {
        AgentMessage::User { content, .. } => {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            format!("[user] {}", text)
        }
        AgentMessage::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !tool_calls.is_empty() {
                let calls: Vec<String> = tool_calls
                    .iter()
                    .map(|tc| format!("{}({})", tc.name, tc.input))
                    .collect();
                format!(
                    "[assistant] (tool_calls: {}) text={}",
                    calls.join(", "),
                    text
                )
            } else {
                format!("[assistant] {}", text)
            }
        }
        AgentMessage::ToolResult {
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
            if *is_error {
                format!("[tool_result:{}] ERROR: {}", tool_call_id, text)
            } else {
                format!("[tool_result:{}] {}", tool_call_id, text)
            }
        }
        AgentMessage::Notification { kind, .. } => format!("[notification] {}", kind),
        AgentMessage::Steering { .. } => "[steering]".to_string(),
        AgentMessage::FollowUp { .. } => "[follow_up]".to_string(),
        AgentMessage::Compaction { .. } => "[compaction]".to_string(),
        AgentMessage::BranchMarker { .. } => "[branch_marker]".to_string(),
    }
}

/// 简单的当前毫秒时间戳（避免引入 chrono 依赖）。
fn chrono_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// epoch millis → "YYYY-MM-DD HH:MM:SS UTC"（手写，避免引入 chrono）。
/// 用于 session list / resume 信息展示。
fn chrono_iso(epoch_millis: i64) -> String {
    let secs = epoch_millis / 1000;
    let days_since_epoch = secs / 86400;
    let time_of_day_secs = (secs % 86400).max(0) as u32;

    // 计算 UTC 日期（Gregorian,从 1970-01-01 起算）
    let (year, month, day) = days_to_ymd(days_since_epoch);
    let h = time_of_day_secs / 3600;
    let m = (time_of_day_secs % 3600) / 60;
    let s = time_of_day_secs % 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        year, month, day, h, m, s
    )
}

/// 自 1970-01-01 起算的天数 → (年, 月, 日)。
/// 用 Howard Hinnant `days_from_civil` 算法的反向 —— 简单、可移植。
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// 从 CWD 向上找 `.env` 文件，加载到进程环境（不覆盖已存在的）。
///
/// 手写 ~20 行实现，避免引入 `dotenvy` 依赖。
///
/// 支持：
/// - 空行 / `#` 开头注释
/// - `KEY=value` 形式
/// - 值不带引号或带双引号
fn load_dotenv() {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let path = d.join(".env");
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut loaded = 0usize;
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        let key = key.trim();
                        let mut value = value.trim().to_string();
                        // 去双引号
                        if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                            value = value[1..value.len() - 1].to_string();
                        }
                        // 不覆盖已有环境变量
                        if std::env::var_os(key).is_none() {
                            std::env::set_var(key, value);
                            loaded += 1;
                        }
                    }
                }
                if loaded > 0 {
                    eprintln!("[dotenv] loaded {} var(s) from {}", loaded, path.display());
                }
                return;
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn monad_is_default_and_fast_preference_is_explicit() {
        let defaults = parse_cli_from(Vec::<String>::new()).unwrap();
        assert!(!defaults.direct_tools);
        assert!(defaults.fast_backend.is_none());
        let args =
            parse_cli_from(["--prefer-fast=qwen", "计算 12 加 7", "--no-save"].map(String::from))
                .unwrap();
        assert_eq!(
            args.fast_backend,
            Some(smallhead_core::fast_route::FastBackend::Qwen)
        );
        assert_eq!(args.user_prompt.as_deref(), Some("计算 12 加 7"));
        assert!(args.no_save);
    }
    #[test]
    fn incompatible_mode_is_rejected_and_resume_is_preserved() {
        assert!(
            parse_cli_from(["--direct-tools", "--prefer-fast=laya"].map(String::from)).is_err()
        );
        let args =
            parse_cli_from(["--direct-tools", "--resume", "session-1", "--mock"].map(String::from))
                .unwrap();
        assert!(args.direct_tools && args.force_mock);
        assert_eq!(args.resume.as_deref(), Some("session-1"));
    }
}
