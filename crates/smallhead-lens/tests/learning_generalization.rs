//! Real JS policy and Rust worker through the host's ordinary AgentLoop.
use std::{path::Path, sync::Arc, time::Duration};

use serde_json::{json, Value};
use smallhead_core::{AgentLoop, AgentMessage, ContentBlock, MockProvider, MockResponse};
use smallhead_lens::{ExtensionAccess, LensRegistry};

fn call(name: &str, input: Value) -> MockResponse {
    MockResponse::ToolCall {
        name: name.into(),
        input,
    }
}

fn package(root: &Path, data: &Path) -> std::path::PathBuf {
    let source = std::path::PathBuf::from(
        std::env::var("SMALLHEAD_LEARNING_LENS")
            .expect("set SMALLHEAD_LEARNING_LENS to the complete Learning Lens package"),
    );
    let target = root.join("learning");
    std::fs::create_dir_all(&target).unwrap();
    for file in ["loop.mjs", "system.md", "tools.json"] {
        std::fs::copy(source.join(file), target.join(file)).unwrap();
    }
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(source.join("lens.json")).unwrap()).unwrap();
    manifest["extension"]["command"] = json!(std::env::var("SMALLHEAD_LEARNING_WORKER")
        .expect("set SMALLHEAD_LEARNING_WORKER to the built Rust worker"));
    std::fs::write(target.join("lens.json"), manifest.to_string()).unwrap();
    std::fs::write(
        target.join("binding.json"),
        json!({"data_dir":data,"user":"learner"}).to_string(),
    )
    .unwrap();
    target
}

async fn turn(registry: &LensRegistry, responses: Vec<MockResponse>, text: &str) -> AgentLoop {
    let mut host = AgentLoop::new(
        Arc::new(MockProvider::new(responses)),
        vec![],
        "Host instructions",
    );
    let session = registry.session();
    session.attach(&mut host, ExtensionAccess::all()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        host.run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: 1,
        }),
    )
    .await
    .expect("host turn deadline")
    .unwrap();
    drop(session);
    host
}

fn receipts(host: &AgentLoop) -> Vec<Value> {
    host.state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error, "{content:?}");
                content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => serde_json::from_str(text).ok(),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect()
}

fn done() -> MockResponse {
    MockResponse::AssistantText {
        text: "现在沿着这个例子继续。".into(),
    }
}

#[tokio::test]
#[ignore = "requires the external Learning Lens and its built worker; see the verification document"]
async fn learning_lens_loads_native_management_and_recalls_real_feedback_on_next_task() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let notebook = temp.path().join("example.ipynb");
    std::fs::write(
        &notebook,
        json!({"nbformat":4,"cells":[
            {"cell_type":"markdown","source":"# Learning rate"},
            {"cell_type":"code","source":"alpha = 0.1\nweight -= alpha * derivative"}
        ]})
        .to_string(),
    )
    .unwrap();
    let registry = LensRegistry::default();
    registry.load(package(temp.path(), &data)).unwrap();
    let first = turn(&registry, vec![
        call("learning_material",json!({"action":"import","location":notebook})),
        call("learning_record",json!({"action":"save","kind":"self_report","topic":"学习率 alpha",
            "note":"用户觉得先看具体数字更清楚，尚未观察独立作答","evidence_quote":"先改一个数更清楚"})),
        call("learning_course",json!({"action":"create","title":"Rust ownership"})),
        done(),
    ], "学习 alpha，先改一个数更清楚").await;
    assert!(first
        .tools
        .iter()
        .any(|tool| tool.name() == "learning_material"));
    assert!(first
        .tools
        .iter()
        .any(|tool| tool.name() == "learning_course"));
    let first = receipts(&first);
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["coverage"]["executed"], false);
    let source = first[0]["source_id"].clone();
    let second = turn(
        &registry,
        vec![
            call("learning_context", json!({"query":"学习率 alpha"})),
            call(
                "learning_material",
                json!({"action":"bind","source_id":source,"role":"primary","expected_revision":0}),
            ),
            call("search_materials", json!({"query":"alpha"})),
            done(),
        ],
        "继续学习率 alpha",
    )
    .await;
    let second = receipts(&second);
    assert_eq!(
        second[0]["course"], "自由学习",
        "creating another goal must not switch the next task"
    );
    assert_eq!(
        second[0]["recent_learning_records"][0]["kind"],
        "self_report"
    );
    assert!(!second[0]["relevant_learning_records"]["records"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(second[2]["matches"][0]["source_id"], source);
}

#[tokio::test]
#[ignore = "uses configured MiniMax API and a downloaded notebook; see verification document"]
async fn live_model_uses_native_material_tools_and_explains_the_notebook() {
    use smallhead_core::execution::{smallhead_process::ProcessManager, ExecutionRuntime};
    use smallhead_core::{BashTool, MiniMaxConfig, MiniMaxProvider, ToolKind};
    let env_file = std::env::var("SMALLHEAD_ENV_FILE").expect("set SMALLHEAD_ENV_FILE");
    let env = std::fs::read_to_string(env_file).unwrap();
    let configured = |name: &str| {
        env.lines().find_map(|line| {
            let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| value.trim().trim_matches(['\'', '"']).to_owned())
        })
    };
    let mut config =
        MiniMaxConfig::new(configured("MINIMAX_API_KEY").expect("MiniMax key required"));
    if let Some(model) = configured("MINIMAX_MODEL") {
        config.model = model;
    }
    if let Some(base) = configured("MINIMAX_BASE_URL") {
        config.base_url = base
            .trim_end_matches('/')
            .trim_end_matches("/v1/messages")
            .into();
    }
    let notebook =
        std::env::var("SMALLHEAD_LEARNING_NOTEBOOK").expect("set SMALLHEAD_LEARNING_NOTEBOOK");
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let registry = LensRegistry::default();
    registry.load(package(temp.path(), &data)).unwrap();
    registry
        .load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lenses/teaching-feedback"))
        .unwrap();
    let mut host = AgentLoop::new(Arc::new(MiniMaxProvider::new(config)), vec![],
        "你在临时学习库中帮助模拟学习者。回应当前问题，遵守工具真实回执。不要访问其他资料或用户的真实学习库。");
    host.budget.max_model_waves = 10;
    host.budget.max_tool_calls = 24;
    host.tools.push(ToolKind::Bash(BashTool));
    host.tool_ctx.cwd = temp.path().to_path_buf();
    host.tool_ctx.execution = Some(Arc::new(
        ExecutionRuntime::new(ProcessManager::with_log_root(temp.path().join("runs")).unwrap())
            .unwrap(),
    ));
    let session = registry.session();
    session.attach(&mut host, ExtensionAccess::all()).unwrap();
    let prompt = format!("请把本地材料 {notebook} 导入并关联当前学习目标。这是 Trask 第4章的官方配套 Notebook。我看到 alpha 加进了权重更新，想继续学这一步。请结合材料里的一个实际算例解释 alpha 的作用，并演示同一起点换两个 alpha 的第一步变化。直接讲解，不需要先测验我，也不要去网上找其他书。");
    let summary = tokio::time::timeout(
        Duration::from_secs(180),
        host.run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text { text: prompt }],
            timestamp: 1,
        }),
    )
    .await
    .expect("live model deadline")
    .unwrap();
    let mut errors = Vec::new();
    let output: Vec<Value> = host
        .state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                content, is_error, ..
            } => {
                let data = content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => serde_json::from_str::<Value>(text).ok(),
                    _ => None,
                });
                if *is_error {
                    errors.push(data.clone());
                    None
                } else {
                    data
                }
            }
            _ => None,
        })
        .collect();
    if let Ok(path) = std::env::var("SMALLHEAD_LEARNING_SMOKE_REPORT") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(
                &json!({"messages":host.state.messages,"recoverable_tool_errors":errors}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    assert!(
        output
            .iter()
            .any(|r| r["kind"] == "notebook" && r["coverage"]["executed"] == false),
        "must import real material"
    );
    assert!(
        output.iter().any(|r| r["bound"] == true),
        "must persist a material binding"
    );
    let answer = host
        .state
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            } if tool_calls.is_empty() => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .expect("plain language answer required");
    assert!(!answer.trim().is_empty());
    assert!(
        host.state.messages.iter().any(|message| matches!(message,
        AgentMessage::Assistant {tool_calls,..} if tool_calls.iter().any(|call|call.name=="bash"))),
        "numerical comparisons must use the available verification tool"
    );
    println!(
        "live tool calls: {}, recoverable errors: {}\n{answer}",
        summary.tool_calls_executed,
        errors.len()
    );
}
