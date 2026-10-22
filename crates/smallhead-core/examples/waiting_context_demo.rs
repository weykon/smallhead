//! `waiting_context_demo` — 走完 WaitingContext 的真实链路。
//!
//! 分三部分:
//!
//! - **Part A — 确定性触发**:直接调 `MonadBridgeTool.execute`,
//!   先传空 `expression` 拿到 `WaitingContext`,再传合法 `expression` 拿到 `Completed`。
//!   这部分不依赖 LLM,只看 tool + monad 的代码路径。
//!
//! - **Part B — 真实 LLM 调度**:用真 `MiniMaxProvider` + `AgentLoop` +
//!   `MonadBridgeTool`,让 LLM 自己决定 discover / dispatch,看到底流几次。
//!   (默认会自然走 Completed;若 LLM 犯了迷糊或用空 expression,
//!   我们能看到 WaitingContext 出现在 messages 里)
//!
//! - **Part C — 真实 LLM 触发 WaitingContext**:换 system prompt 引导 LLM
//!   在客户没给 expression 时主动派发 `expression=""`,观察真实 LLM
//!   真的把 `WaitingContext` 走到 `result.status="waiting_context"`。
//!
//! 跑法:
//! ```bash
//! # 默认走真实 MiniMax API(需要 .env 里 MINIMAX_API_KEY)
//! cargo run --example waiting_context_demo -p smallhead-core
//!
//! # 强制 MockProvider(让 Part B / Part C 也离线;不影响 Part A)
//! cargo run --example waiting_context_demo -p smallhead-core -- --mock
//! ```

use std::sync::Arc;

use smallhead_core::monad_bridge::MonadBridgeTool;
use smallhead_core::tool::ToolImpl;
use smallhead_core::{
    AgentLoop, AgentMessage, ContentBlock, MiniMaxConfig, MiniMaxProvider, MockProvider,
    MockResponse, Provider, TaskId, ToolContext, ToolKind,
};

#[derive(Debug, Default)]
struct CliArgs {
    force_mock: bool,
}

fn parse_cli() -> CliArgs {
    let mut args = CliArgs::default();
    for a in std::env::args().skip(1) {
        if a == "--mock" {
            args.force_mock = true;
        }
    }
    args
}

fn load_dotenv() {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let path = d.join(".env");
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut n = 0usize;
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let mut v = v.trim().to_string();
                        if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                            v = v[1..v.len() - 1].to_string();
                        }
                        if std::env::var_os(k).is_none() {
                            std::env::set_var(k, v);
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    eprintln!("[dotenv] loaded {n} var(s) from {}", path.display());
                }
                return;
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
}

fn extract_json_payload(tool_result_text: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(tool_result_text).ok()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = parse_cli();
    load_dotenv();

    println!("═══════════════════════════════════════════════════════════════");
    println!("  Part A — 直接调 MonadBridgeTool 触发 WaitingContext");
    println!("═══════════════════════════════════════════════════════════════");

    let bridge = MonadBridgeTool {
        parent_task_id: TaskId::new(),
        model_preferences: Default::default(),
    };

    // A1. 故意传空 expression — tool 内部校验"必须 1 个 string"放过空串,
    //     进去 CalculationMonad::step() 触发 WaitingContext
    println!("\n[A1] dispatch(monad_id=calculation, inputs={{expression: \"\"}})",);
    let r1 = bridge
        .execute(
            serde_json::json!({
                "action": "dispatch",
                "monad_id": "calculation",
                "objective": "求值",
                "inputs": { "expression": "" },
            }),
            &ToolContext::default(),
        )
        .await
        .expect("empty expression 应被工具接受并返回 WaitingContext");

    let text1 = match &r1.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("期望 text content"),
    };
    let payload1 = extract_json_payload(&text1).expect("应可解析 JSON");
    println!(
        "  ToolResult(JSON):\n    {}",
        serde_json::to_string_pretty(&payload1).unwrap()
    );

    let status = payload1["result"]["status"].as_str().unwrap_or("?");
    println!("\n  ⇒ result.status = \"{status}\"");
    let missing: Vec<String> = payload1["result"]["missing"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    println!("  ⇒ result.missing = {missing:?}");

    let handoff_task_id = payload1["handoff"]["task_id"].as_str().unwrap_or("?");
    let result_task_id = payload1["result"]["task_id"].as_str().unwrap_or("?");
    println!(
        "  ⇒ handoff.task_id  = ...{}",
        &handoff_task_id[handoff_task_id.len().saturating_sub(12)..]
    );
    println!(
        "  ⇒ result.task_id   = ...{}",
        &result_task_id[result_task_id.len().saturating_sub(12)..]
    );
    println!(
        "  ⇒ 回填一致: {}",
        handoff_task_id != "?" && handoff_task_id == result_task_id
    );

    // A2. 同一工具,补上合法 expression — 应该 Completed
    println!("\n[A2] dispatch(monad_id=calculation, inputs={{expression: \"100+100\"}})");
    let r2 = bridge
        .execute(
            serde_json::json!({
                "action": "dispatch",
                "monad_id": "calculation",
                "objective": "求值",
                "inputs": { "expression": "100+100" },
            }),
            &ToolContext::default(),
        )
        .await
        .expect("100+100 应被接受并返回 Completed");

    let text2 = match &r2.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("期望 text content"),
    };
    let payload2 = extract_json_payload(&text2).expect("应可解析 JSON");
    println!(
        "  ToolResult(JSON):\n    {}",
        serde_json::to_string_pretty(&payload2).unwrap()
    );

    let status2 = payload2["result"]["status"].as_str().unwrap_or("?");
    println!("\n  ⇒ result.status = \"{status2}\"");
    let value = payload2["result"]["output"]["value"]
        .as_f64()
        .unwrap_or(f64::NAN);
    println!("  ⇒ result.output.value = {value}");

    let handoff2_task = payload2["handoff"]["task_id"].as_str().unwrap_or("?");
    let result2_task = payload2["result"]["task_id"].as_str().unwrap_or("?");
    println!(
        "  ⇒ handoff.task_id  = ...{}",
        &handoff2_task[handoff2_task.len().saturating_sub(12)..]
    );
    println!(
        "  ⇒ result.task_id   = ...{}",
        &result2_task[result2_task.len().saturating_sub(12)..]
    );
    println!(
        "  ⇒ 回填一致: {}",
        handoff2_task != "?" && handoff2_task == result2_task
    );

    println!("\n  Part A 总结:");
    println!("    A1 空 inputs.expression → WaitingContext (status=waiting_context)");
    println!("    A2 补 expression  →  Completed  (status=completed)");
    println!("    两次 task_id 独立(每次 handoff 自带新 task_id,符合协议)");

    println!("\n═══════════════════════════════════════════════════════════════");
    println!("  Part B — 真实 LLM 调度:AgentLoop + MonadBridgeTool");
    println!("═══════════════════════════════════════════════════════════════");

    let use_real_api = !cli.force_mock
        && std::env::var("MINIMAX_API_KEY")
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);

    let provider: Arc<dyn Provider> = if use_real_api {
        let key = std::env::var("MINIMAX_API_KEY").unwrap();
        let cfg = MiniMaxConfig::new(key);
        let url = cfg.base_url.clone();
        let model = cfg.model.clone();
        println!("\n  [provider] real MiniMax API (model={model}, base_url={url})");
        Arc::new(MiniMaxProvider::new(cfg))
    } else {
        println!("\n  [provider] MockProvider (offline mode)");
        println!("  [provider] hint: 去掉 --mock 或 export MINIMAX_API_KEY=... 启用真实 API");
        // Mock 一个真实 LLM 在此 prompt 下的反应:
        // 1. discover
        // 2. dispatch calculation 100+200
        // 3. dispatch calculation 100*100
        // 4. 文字:done
        Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({"action":"discover"}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({
                    "action":"dispatch","monad_id":"calculation",
                    "objective":"算加法","inputs":{"expression":"100+200"}
                }),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({
                    "action":"dispatch","monad_id":"calculation",
                    "objective":"算乘法","inputs":{"expression":"100*100"}
                }),
            },
            MockResponse::AssistantText {
                text: "已算完:100+200=300,100*100=10000".into(),
            },
        ]))
    };

    let mut agent = AgentLoop::new(
        provider.clone(),
        vec![ToolKind::MonadBridge(bridge)],
        "你是一名计算助手，请通过 monad_bridge 完成计算。",
    );

    println!("\n  [user] 你好帮我算一下 一百加两百 然后再算一个100乘以一百\n");

    let user_msg = AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: "你好帮我算一下 一百加两百 然后再算一个100乘以一百".into(),
        }],
        timestamp: 0,
    };

    let summary = agent.run_one_turn(user_msg).await.expect("run_one_turn ok");

    println!("  === Summary ===");
    println!("  turns_completed: {}", summary.turns_completed);
    println!("  tool_calls_executed: {}", summary.tool_calls_executed);

    println!("\n  === Final messages (只列不替代) ===");
    for (i, msg) in agent.state.messages.iter().enumerate() {
        match msg {
            AgentMessage::User { content, .. } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                println!("  [{i}] [user] {t}");
            }
            AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !tool_calls.is_empty() {
                    let names: Vec<String> = tool_calls
                        .iter()
                        .map(|tc| format!("{}({})", tc.name, tc.input))
                        .collect();
                    println!("  [{i}] [assistant] tool_calls: {names:?} | text={t}");
                } else {
                    println!("  [{i}] [assistant] {t}");
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                // 尝试解析 tool_result 的 JSON,看是不是 waiting_context
                let parsed = extract_json_payload(&t);
                let preview = if let Some(p) = parsed {
                    let status = p["result"]["status"].as_str().unwrap_or("?");
                    format!(
                        "{{result.status={status}, handoff.task_id=...{}, result.task_id=...{}, output={}}}",
                        p["handoff"]["task_id"].as_str().unwrap_or("?")
                            .chars()
                            .rev()
                            .take(12)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>(),
                        p["result"]["task_id"].as_str().unwrap_or("?")
                            .chars()
                            .rev()
                            .take(12)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>(),
                        p["result"]["output"]
                    )
                } else {
                    t.clone()
                };
                let tag = if *is_error {
                    "tool_result:ERR"
                } else {
                    "tool_result"
                };
                println!("  [{i}] [{tag}:{tool_call_id}] {preview}");
            }
            AgentMessage::Notification { kind, .. } => {
                println!("  [{i}] [notification] {kind:?}");
            }
            AgentMessage::Steering { .. } => println!("  [{i}] [steering]"),
            AgentMessage::FollowUp { .. } => println!("  [{i}] [follow_up]"),
            AgentMessage::Compaction { .. } => println!("  [{i}] [compaction]"),
            AgentMessage::BranchMarker { .. } => println!("  [{i}] [branch_marker]"),
        }
    }

    println!("\n  Part B 总结:");
    println!("    真实 LLM (或 Mock) 通过 monad_bridge 工具自动驱动 discover + dispatch。");
    println!("    每次 dispatch 内部:instantiate → start(handoff) → step() → 校验回填。");
    println!("    若 LLM 传空 expression,tool_result 会出现 result.status=\"waiting_context\"。");

    println!("\n═══════════════════════════════════════════════════════════════");
    println!("  Part C — 真实 LLM 触发 WaitingContext(故意让 LLM 漏数据)");
    println!("═══════════════════════════════════════════════════════════════");

    // 关键点:我们换个 system prompt 明确告诉 LLM
    //   "如果你不知道 expression,就传空字符串让 WaitingContext 提醒你"
    // 然后给一个真正模糊的 user prompt,逼 LLM 必须派发空 expression 才能往下走。
    // Part B 已消费完 mock 响应；Part C 使用独立脚本，离线也能走等待链路。
    let provider_c: Arc<dyn Provider> = if use_real_api {
        provider.clone()
    } else {
        Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({"action":"discover"}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({
                    "action":"dispatch", "monad_id":"calculation",
                    "objective":"确认待计算的表达式", "inputs":{"expression":""}
                }),
            },
            MockResponse::AssistantText {
                text: "计算任务缺少 expression，请提供需要计算的数字和运算符。".into(),
            },
        ]))
    };
    let mut agent_c = AgentLoop::new(
        provider_c,
        vec![ToolKind::MonadBridge(MonadBridgeTool {
            parent_task_id: TaskId::new(),
            model_preferences: Default::default(),
        })],
        "你通过 monad_bridge 调度计算业务。 \
         用户没有指定表达式时，派发时填写 expression=\"\" \
         （空字符串），桥接器会返回 WaitingContext，提示你向用户补充询问。 \
         禁止编造结果，禁止自行计算答案。",
    );

    let user_msg_c = AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: "客户让你帮他做一道算术题,但客户没说要算什么数字。请用 monad_bridge 派发 calculation 任务确认细节。".into(),
        }],
        timestamp: 0,
    };

    println!("\n  [user] 客户让你帮他做一道算术题,但客户没说要算什么数字。");
    println!("         请用 monad_bridge 派发 calculation 任务确认细节。\n");

    let summary_c = agent_c
        .run_one_turn(user_msg_c)
        .await
        .expect("run_one_turn ok");

    println!("  === Summary ===");
    println!("  turns_completed: {}", summary_c.turns_completed);
    println!("  tool_calls_executed: {}", summary_c.tool_calls_executed);

    // 关键检测:Part C 的 tool_result 里有没有 WaitingContext?
    let mut saw_waiting = false;
    let mut saw_completed = false;
    for m in &agent_c.state.messages {
        if let AgentMessage::ToolResult { content, .. } = m {
            if let Some(ContentBlock::Text { text }) = content.first() {
                if let Some(p) = extract_json_payload(text) {
                    match p["result"]["status"].as_str() {
                        Some("waiting_context") => saw_waiting = true,
                        Some("completed") => saw_completed = true,
                        _ => {}
                    }
                }
            }
        }
    }
    println!(
        "\n  检测:result.status 出现 waiting_context = {saw_waiting} ; completed = {saw_completed}"
    );
    if !use_real_api {
        assert!(
            saw_waiting && !saw_completed,
            "离线 Part C 必须触发等待，不能编造完成结果"
        );
    }

    println!("\n  === Final messages (Part C) ===");
    for (i, msg) in agent_c.state.messages.iter().enumerate() {
        match msg {
            AgentMessage::User { content, .. } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                println!("  [{i}] [user] {t}");
            }
            AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !tool_calls.is_empty() {
                    let names: Vec<String> = tool_calls
                        .iter()
                        .map(|tc| format!("{}({})", tc.name, tc.input))
                        .collect();
                    println!("  [{i}] [assistant] tool_calls: {names:?} | text={t}");
                } else {
                    println!("  [{i}] [assistant] {t}");
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let t = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let parsed = extract_json_payload(&t);
                let preview = if let Some(p) = parsed {
                    let status = p["result"]["status"].as_str().unwrap_or("?");
                    let missing = p["result"]["missing"].to_string();
                    let task_id_tail: String = p["result"]["task_id"]
                        .as_str()
                        .unwrap_or("?")
                        .chars()
                        .rev()
                        .take(12)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    format!("{{status={status}, missing={missing}, task_id=...{task_id_tail}}}")
                } else {
                    t.clone()
                };
                let tag = if *is_error {
                    "tool_result:ERR"
                } else {
                    "tool_result"
                };
                println!("  [{i}] [{tag}:{tool_call_id}] {preview}");
            }
            AgentMessage::Notification { kind, .. } => {
                println!("  [{i}] [notification] {kind:?}");
            }
            AgentMessage::Steering { .. } => println!("  [{i}] [steering]"),
            AgentMessage::FollowUp { .. } => println!("  [{i}] [follow_up]"),
            AgentMessage::Compaction { .. } => println!("  [{i}] [compaction]"),
            AgentMessage::BranchMarker { .. } => println!("  [{i}] [branch_marker]"),
        }
    }

    println!("\n  Part C 总结:");
    if saw_waiting {
        let source = if use_real_api {
            "真实 LLM"
        } else {
            "固定 Mock 脚本"
        };
        println!("    ✓ {source} 派发 expression=\"\"，实际 Monad 返回 WaitingContext。");
        println!("    ✓ AgentLoop 回喂等待结果后输出补充问题；此例不保存跨轮次等待任务。");
    } else if saw_completed {
        println!("    ⚠ LLM 没传空,自己编了一个 expression 进 Completed。");
        println!(
            "      重新跑或在 user prompt 里更明确要求\"必须先派空\",即可看到 WaitingContext。"
        );
    } else {
        println!("    ⚠ LLM 没有派发任何 calculation(可能直接文字回答了)。");
        println!("      调高 user prompt 的指令强度即可。");
    }

    println!("\n═══════════════════════════════════════════════════════════════");
    println!(
        "  ✓ WaitingContext 示例结束（Part A 直接派发；Part B/C 使用真实模型或固定 Mock 脚本）"
    );
    println!("═══════════════════════════════════════════════════════════════");
}
