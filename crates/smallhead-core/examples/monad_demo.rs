//! `monad_demo` — 亲手把这一轮加的 monad 协议跑一遍。
//!
//! 覆盖 5 件事:
//! 1. 构造 `MonadHandoff` → 序列化成 JSON (wire format)
//! 2. `StaticMonadRegistry::with_fixtures()` 列 3 个 fixture
//! 3. `CalculationMonad`:缺 expression → WaitingContext;补上 → Completed
//! 4. `MainMonad::wrap(provider)` 真桥接 AgentLoop → MonadResult::Completed
//! 5. 主模型发现业务 → 自动构造参数 → 派发子 Monad → 回填 → 最终回复
//!
//! 跑法:
//!
//! ```bash
//! # 默认走真实 MiniMax API（需要 .env 里有 MINIMAX_API_KEY）
//! cargo run --example monad_demo -p smallhead-core
//!
//! # 强制 MockProvider（无 API key 或显式想离线）
//! cargo run --example monad_demo -p smallhead-core -- --mock
//! ```
//!
//! ## 两种 provider 模式
//!
//! 1. **真实 API**(默认):自动从 `.env` 读 `MINIMAX_API_KEY`,调真实 MiniMax 模型。
//! 2. **Mock 模式**:`--mock` 标志,或 `.env` 里没设 `MINIMAX_API_KEY` 时自动 fallback。

use std::sync::Arc;

use smallhead_core::monad::{CalculationMonad, MonadRegistry, RegistryError, StaticMonadRegistry};
use smallhead_core::{
    main_monad_id, AddTool, AgentLoop, AgentMessage, ArtifactReadTool, BashTool, Budget,
    CalculateTool, ContentBlock, ContextRef, DivTool, EchoTool, EditTool, MainMonad, MiniMaxConfig,
    MiniMaxProvider, MockProvider, MockResponse, MonadHandoff, MonadId, MonadLoop, MonadResult,
    MulTool, Provider, ReadTool, ResultSlot, SubTool, TaskId, ToolKind, TraceMeta, WriteTool,
    SCHEMA_VERSION_V1,
};

#[derive(Debug, Default)]
struct CliArgs {
    force_mock: bool,
    fast_backend: Option<smallhead_core::fast_route::FastBackend>,
}

fn parse_cli() -> CliArgs {
    let mut args = CliArgs::default();
    for a in std::env::args().skip(1) {
        if a == "--mock" {
            args.force_mock = true;
        } else if a == "--prefer-fast=laya" {
            args.fast_backend = Some(smallhead_core::fast_route::FastBackend::Laya);
        } else if a == "--prefer-fast=qwen" {
            args.fast_backend = Some(smallhead_core::fast_route::FastBackend::Qwen);
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = parse_cli();
    load_dotenv();

    println!("═══════════════════════════════════════════════════════════════");
    println!("  1. MonadHandoff → JSON  (wire format)");
    println!("═══════════════════════════════════════════════════════════════");
    let handoff = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: Some(TaskId::new()),
        monad_id: MonadId::new("calculation"),
        attempt_id: 1,
        objective: "算 100 加 100".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![ContextRef::UserQuote {
            text: "先算 100 加 100".into(),
        }],
        constraints: vec!["expression 必须合法".into()],
        completion_criteria: "返回数值结果".into(),
        result_slot: ResultSlot("calculations.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 1_759_939_200_000,
        trace: TraceMeta {
            frame_ref: "frame-rev-001".into(),
            request_id: "req-001".into(),
        },
    };
    let json = serde_json::to_string_pretty(&handoff).unwrap();
    println!("{json}");

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  2. StaticMonadRegistry::with_fixtures()");
    println!("═══════════════════════════════════════════════════════════════");
    let reg = StaticMonadRegistry::with_fixtures();
    for entry in reg.list() {
        println!(
            "  • summary: {}\n    visible_tools: {:?}\n    keywords: {:?}\n",
            entry.summary, entry.visible_tools, entry.keywords
        );
    }
    println!("  search(\"算术\", 10) →");
    for hit in reg.search("算术", 10) {
        println!("    → {}", hit.summary);
    }
    println!("  search(\"cal\", 10) →");
    for hit in reg.search("cal", 10) {
        println!("    → {}", hit.summary);
    }
    println!("  instantiate(\"calculation\") →");
    match reg.instantiate(&MonadId::new("calculation")) {
        Ok(_) => println!("    Ok(实例已构造)"),
        Err(RegistryError::UnknownMonad(id)) => println!("    Err(UnknownMonad({id:?}))"),
        Err(e) => println!("    Err({e:?})"),
    }
    println!("  instantiate(\"nonexistent\") →");
    match reg.instantiate(&MonadId::new("nonexistent")) {
        Ok(_) => println!("    Ok(???)"),
        Err(RegistryError::UnknownMonad(id)) => println!("    Err(UnknownMonad({:?}))", id.0),
        Err(e) => println!("    Err({e:?})"),
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  3. CalculationMonad lifecycle");
    println!("═══════════════════════════════════════════════════════════════");

    // 3a. 缺 expression → WaitingContext
    let bad_handoff = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: None,
        monad_id: MonadId::new("calculation"),
        attempt_id: 1,
        objective: "求值".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "返回数值".into(),
        result_slot: ResultSlot("calculations.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    };

    let mut calc = CalculationMonad::from_handoff(&bad_handoff).expect("构造 ok");
    let r1 = calc.step().await.expect("step ok");
    match r1 {
        MonadResult::WaitingContext {
            task_id,
            reason,
            missing,
        } => {
            println!(
                "  ① 缺 expression → WaitingContext\n    task_id: ...{}\n    reason: {reason}\n    missing: {missing:?}\n",
                &task_id.0[24..]
            );
        }
        other => panic!("应该是 WaitingContext,实际: {other:?}"),
    }

    // 3b. 补上 expression 再 start 一次
    let mut inputs = serde_json::Map::new();
    inputs.insert(
        "expression".into(),
        serde_json::Value::String("100+100".into()),
    );
    let good_handoff = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: None,
        monad_id: MonadId::new("calculation"),
        attempt_id: 1,
        objective: "求值".into(),
        inputs,
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "返回数值".into(),
        result_slot: ResultSlot("calculations.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    };
    calc.start(good_handoff).await.expect("start ok");
    let r2 = calc.step().await.expect("step ok");
    match r2 {
        MonadResult::Completed {
            output,
            monad_id,
            usage,
            finished_at_ms,
            ..
        } => {
            println!(
                "  ② expression=\"100+100\" → Completed\n    output: {}\n    monad_id: {}\n    steps_taken: {}\n    finished_at_ms: {finished_at_ms}\n",
                serde_json::to_string_pretty(&output).unwrap(),
                monad_id.0,
                usage.steps_taken,
            );
        }
        other => panic!("应该是 Completed,实际: {other:?}"),
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  4. MainMonad::wrap(provider) → step() → Completed");
    println!("═══════════════════════════════════════════════════════════════");

    // 4a. 选 provider:有 MINIMAX_API_KEY 就走真,否则 fallback Mock(可 --mock 强制 mock)
    let use_real_api = !cli.force_mock
        && std::env::var("MINIMAX_API_KEY")
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);

    let provider: Arc<dyn Provider> = if use_real_api {
        let key = std::env::var("MINIMAX_API_KEY").unwrap();
        let cfg = MiniMaxConfig::new(key);
        let url = cfg.base_url.clone();
        let model = cfg.model.clone();
        println!("  [provider] real MiniMax API (model={model}, base_url={url})");
        Arc::new(MiniMaxProvider::new(cfg))
    } else {
        let reason = if cli.force_mock {
            "--mock 标志"
        } else {
            "MINIMAX_API_KEY 未设置"
        };
        println!("  [provider] MockProvider (offline mode, reason: {reason})");
        println!("  [provider] hint: 去掉 --mock 或 export MINIMAX_API_KEY=... 启用真实 API");
        Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "echo".into(),
                input: serde_json::json!({"text": "hello"}),
            },
            MockResponse::AssistantText {
                text: "我算给你看,100 加 100 等于 200。".into(),
            },
        ]))
    };

    // 4b. 工具列表(同 demo.rs)
    let tools = vec![
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
    ];

    // 4c. MainMonad 包装 AgentLoop
    let system_prompt = "你是一名乐于助人的助手。";
    let mut main_m = MainMonad::wrap(AgentLoop::new(provider.clone(), tools, system_prompt));

    let task_id = TaskId::new();
    let parent_task_id = Some(TaskId::new());
    let result_slot = ResultSlot("main.first".into());

    main_m
        .start(MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.to_string(),
            task_id: task_id.clone(),
            parent_task_id: parent_task_id.clone(),
            monad_id: main_monad_id().clone(),
            attempt_id: 1,
            objective: "你好帮我算一下 一百加一百 然后再算一个100乘以一百".into(),
            inputs: serde_json::Map::new(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "返回结果".into(),
            result_slot: result_slot.clone(),
            depends_on: vec![],
            budget: Budget::default(),
            issued_at_ms: 0,
            trace: TraceMeta {
                frame_ref: String::new(),
                request_id: String::new(),
            },
        })
        .await
        .expect("start ok");

    println!("\n  start() 完成 → handoff 标识已绑定");
    println!("    task_id:        ...{}", &task_id.0[24..]);
    println!(
        "    parent_task_id: ...{}",
        &parent_task_id.as_ref().unwrap().0[24..]
    );
    println!("    result_slot:    {result_slot:?}");

    let result = main_m.step().await.expect("step ok");
    match result {
        MonadResult::Completed {
            task_id: rid,
            parent_task_id: rpt,
            result_slot: rrs,
            monad_id: rmid,
            output,
            usage,
            finished_at_ms,
            ..
        } => {
            println!("\n  step() 完成 → MonadResult::Completed");
            println!(
                "    task_id:        ...{} (回填成功: {})",
                &rid.0[24..],
                rid == task_id
            );
            println!(
                "    parent_task_id: ...{} (回填成功: {})",
                &rpt.as_ref().unwrap().0[24..],
                rpt == parent_task_id
            );
            println!(
                "    result_slot:    {rrs:?} (回填成功: {})",
                rrs == result_slot
            );
            println!("    monad_id:       {rmid:?}");
            println!(
                "    output:         {}",
                serde_json::to_string_pretty(&output).unwrap()
            );
            println!(
                "    usage:          steps_taken={}, tokens_used={}, elapsed_ms={}",
                usage.steps_taken, usage.tokens_used, usage.elapsed_ms
            );
            println!("    finished_at_ms: {finished_at_ms}");

            // 顺手把回填结果也用作 user_msg 看一眼（真实模式下 log 会闪 token）
            println!("\n  送给 LLM 的 user_msg (objective):");
            let user_msg = AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "你好帮我算一下 一百加一百 然后再算一个100乘以一百".into(),
                }],
                timestamp: 0,
            };
            println!("    {user_msg:?}");
        }
        other => panic!("应该 Completed,实际: {other:?}"),
    }

    // 再 start 一次应该 ok(上一轮 task 已 take 走)
    println!("\n  再 start 一次(消费完)→ 应该 OK");
    let task_id2 = TaskId::new();
    let r3 = main_m
        .start(MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.to_string(),
            task_id: task_id2.clone(),
            parent_task_id: None,
            monad_id: main_monad_id().clone(),
            attempt_id: 1,
            objective: "再试".into(),
            inputs: serde_json::Map::new(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "回复".into(),
            result_slot: ResultSlot("main.second".into()),
            depends_on: vec![],
            budget: Budget::default(),
            issued_at_ms: 0,
            trace: TraceMeta {
                frame_ref: String::new(),
                request_id: String::new(),
            },
        })
        .await;
    match r3 {
        Ok(()) => println!("    Ok → 接受新 task_id ...{}", &task_id2.0[24..]),
        Err(e) => panic!("新 task 应该能 start,实际 Err: {e}"),
    }

    println!("\n═══════════════════════════════════════════════════════════════");
    println!("  5. 主 Agent → 发现业务 → 自动派发子 Monad → 回填 → 回复");
    let parent_id = TaskId::new();
    let mut model_preferences = std::collections::BTreeMap::new();
    if let Some(backend) = cli.fast_backend {
        let endpoint = match backend {
            smallhead_core::fast_route::FastBackend::Laya => std::env::var("SMALLHEAD_LAYA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3031".into()),
            smallhead_core::fast_route::FastBackend::Qwen => std::env::var("SMALLHEAD_QWEN_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3032".into()),
        };
        model_preferences.insert(
            "calculation".into(),
            smallhead_core::fast_route::ModelPreference::PreferFast {
                backend,
                endpoint,
                timeout_ms: 5000,
            },
        );
    }
    let bridge = smallhead_core::monad_bridge::MonadBridgeTool {
        parent_task_id: parent_id.clone(),
        model_preferences,
    };
    let section_provider: Arc<dyn Provider> = if use_real_api {
        provider.clone()
    } else {
        println!("  [mock] 选择和参数为脚本响应；子 Monad 的执行和回填是真实代码路径。");
        Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({"action":"discover"}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({"action":"dispatch","monad_id":"calculation","objective":"算一百加一百","inputs":{"expression":"100+100"}}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: serde_json::json!({"action":"dispatch","monad_id":"calculation","objective":"算一百乘以一百","inputs":{"expression":"100*100"}}),
            },
            MockResponse::AssistantText {
                text: "100 + 100 = 200；100 × 100 = 10000。".into(),
            },
        ]))
    };
    let mut agent = MainMonad::wrap(AgentLoop::new(section_provider, vec![ToolKind::MonadBridge(bridge)],
        "你负责调度业务 Monad。必须先通过 monad_bridge 发现目录，再为每个操作填写正确的输入并派发。遵守用户要求的先后顺序。根据返回结果回答，禁止自行计算答案。WaitingContext 表示任务尚未完成。你唯一可用的能力是业务桥接入口。"));
    println!(
        "  [parent] {}；模型仅可见 monad_bridge，不提供 add/mul 等算子",
        parent_id.0
    );
    agent
        .start(MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.into(),
            task_id: parent_id,
            parent_task_id: None,
            monad_id: main_monad_id().clone(),
            attempt_id: 1,
            objective: "你好帮我算一下 一百加一百 然后再算一个100乘以一百".into(),
            inputs: serde_json::Map::new(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "两个子计算均执行并汇总结果".into(),
            result_slot: ResultSlot("main.section5".into()),
            depends_on: vec![],
            budget: Budget::default(),
            issued_at_ms: 0,
            trace: TraceMeta {
                frame_ref: "section5".into(),
                request_id: "section5".into(),
            },
        })
        .await
        .expect("section 5 start");
    let result = agent.step().await.expect("section 5 execution");
    println!(
        "  [parent result] {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    println!("  ✓ 5 段都跑完了");
    println!("═══════════════════════════════════════════════════════════════");
}
