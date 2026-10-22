use serde_json::{json, Value};
use smallhead_core::{
    execution::{
        smallhead_process::{ExecutionId, ExitReason, ProcessManager, ProcessState},
        ExecutionRuntime, ProcessTool,
    },
    tool::{ToolContext, ToolImpl},
    AgentLoop, AgentMessage, BashTool, ContentBlock, MockProvider, MockResponse, ToolKind,
};
use std::{sync::Arc, time::Duration};

fn setup() -> (tempfile::TempDir, Arc<ExecutionRuntime>, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        ExecutionRuntime::new(ProcessManager::with_log_root(dir.path().join("logs")).unwrap())
            .unwrap(),
    );
    let ctx = ToolContext {
        execution: Some(runtime.clone()),
        owner: Some("test-task".into()),
        cwd: dir.path().into(),
        ..Default::default()
    };
    (dir, runtime, ctx)
}
fn value(result: smallhead_core::ToolResult) -> Value {
    if let ContentBlock::Text { text } = &result.content[0] {
        serde_json::from_str(text).unwrap()
    } else {
        panic!("text result required")
    }
}

#[tokio::test]
async fn custom_service_without_health_restarts_by_name_and_accepts_description() {
    let (_dir, rt, ctx) = setup();
    let first = value(ProcessTool.execute(json!({
        "action":"service_ensure",
        "service":{"name":"preview", "process":smallhead_core::execution::smallhead_process::ProcessSpec::new("sleep 30"), "startup_timeout_ms":2000}
    }), &ctx).await.unwrap());
    assert_eq!(first["record"]["state"], "Ready");
    let first_id = first["record"]["execution_id"].clone();
    let result = value(ProcessTool.execute(json!({"action":"service_update_description", "name":"preview", "description":"本地预览；尚未检查页面"}), &ctx).await.unwrap());
    assert_eq!(result["updated"], true);
    assert!(rt
        .services
        .service_view("preview")
        .unwrap()
        .description_updated_at
        .is_some());
    ProcessTool
        .execute(json!({"action":"service_stop", "name":"preview"}), &ctx)
        .await
        .unwrap();
    let restarted = value(
        ProcessTool
            .execute(json!({"action":"service_ensure", "name":"preview"}), &ctx)
            .await
            .unwrap(),
    );
    assert_eq!(restarted["record"]["state"], "Ready");
    assert_ne!(restarted["record"]["execution_id"], first_id);
    assert_eq!(restarted["record"]["description"], "本地预览；尚未检查页面");
    rt.services.stop("preview").await.unwrap();
    rt.processes.shutdown(Duration::from_secs(6)).await.unwrap();
}

#[tokio::test]
async fn direct_args_are_not_shell_expanded_and_results_are_queryable() {
    let (_dir, rt, ctx) = setup();
    let result = BashTool
        .execute(
            json!({"program":"printf","args":["%s","a; echo unintended"]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let output = value(result);
    assert_eq!(output["stdout"]["text"], "a; echo unintended");
    let id: ExecutionId = serde_json::from_value(output["execution"]["id"].clone()).unwrap();
    assert_eq!(
        rt.processes.inspect(id).unwrap().owner.as_deref(),
        Some("test-task")
    );
    let inspected = value(
        ProcessTool
            .execute(json!({"action":"inspect","execution_id":id}), &ctx)
            .await
            .unwrap(),
    );
    assert_eq!(inspected["id"], output["execution"]["id"]);
    let list = value(
        ProcessTool
            .execute(json!({"action":"list","limit":1}), &ctx)
            .await
            .unwrap(),
    );
    assert_eq!(list["total"], 1);
}

#[tokio::test]
async fn bash_records_structured_execution_metadata_and_events() {
    let (dir, rt, ctx) = setup();
    let result = ToolKind::Bash(BashTool)
        .execute(
            "tool-call-42",
            json!({
                "program":"printf",
                "args":["%s","done"],
                "objective":"produce a sentinel string",
                "completion_policy":{"type":"exited","allowed_exit_codes":[0]}
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.tool_call_id, "tool-call-42");
    let output = value(result);
    let id: ExecutionId = serde_json::from_value(output["execution"]["id"].clone()).unwrap();
    let record = rt.processes.inspect(id).unwrap();
    assert_eq!(record.metadata.source.as_deref(), Some("agent_tool"));
    assert_eq!(
        record.metadata.tool_call_id.as_deref(),
        Some("tool-call-42")
    );
    assert_eq!(
        record.metadata.objective.as_deref(),
        Some("produce a sentinel string")
    );
    assert_eq!(
        record.metadata.completion_policy.as_ref().unwrap()["type"],
        "exited"
    );

    let exec_dir = dir.path().join("logs").join(id.to_string());
    let meta: Value =
        serde_json::from_slice(&std::fs::read(exec_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["metadata"]["tool_call_id"], "tool-call-42");
    assert_eq!(meta["metadata"]["objective"], "produce a sentinel string");
    let events = std::fs::read_to_string(exec_dir.join("events.jsonl")).unwrap();
    assert!(events.contains("\"tool_call_id\":\"tool-call-42\""));
    assert!(events.contains("produce a sentinel string"));
}

#[tokio::test]
async fn exited_completion_policy_controls_tool_success() {
    let (_dir, _rt, ctx) = setup();
    let result = BashTool
        .execute(
            json!({
                "command":"exit 7",
                "completion_policy":{"type":"exited","allowed_exit_codes":[7]}
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let output = value(result);
    assert_eq!(output["completion"]["satisfied"], true);
    assert_eq!(
        output["completion"]["policy"]["allowed_exit_codes"],
        json!([7])
    );
}

#[tokio::test]
async fn unsupported_completion_policy_is_rejected_before_spawn() {
    let (_dir, rt, ctx) = setup();
    let error = BashTool
        .execute(
            json!({
                "command":"true",
                "completion_policy":{"type":"artifact","path":"out.txt"}
            }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("completion_policy.type=exited"));
    assert!(rt.processes.list().is_empty());
}

#[tokio::test]
async fn dropped_foreground_tool_future_stops_its_managed_child() {
    let (_dir, rt, ctx) = setup();
    let worker =
        tokio::spawn(async move { BashTool.execute(json!({"command":"sleep 30"}), &ctx).await });
    let id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(id) = rt.processes.list().first().copied() {
                if rt.processes.inspect(id).unwrap().pid.is_some() {
                    break id;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    let _ = worker.await;
    let record = rt
        .processes
        .wait_for(id, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(record.state,ProcessState::Exited(info) if info.reason==ExitReason::Cancelled)
    );
}

#[tokio::test]
async fn agent_background_task_is_cleaned_after_turn_and_is_logged() {
    let (dir, rt, ctx) = setup();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall {
            name: "bash".into(),
            input: json!({"command":"sleep 30","wait":false}),
        },
        MockResponse::AssistantText {
            text: "已登记后台任务".into(),
        },
    ]));
    let mut agent = AgentLoop::new(
        provider,
        vec![
            ToolKind::Bash(BashTool),
            ToolKind::ProcessControl(ProcessTool),
        ],
        "测试受管执行",
    );
    agent.tool_ctx = ctx;
    agent
        .run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "运行任务".into(),
            }],
            timestamp: 0,
        })
        .await
        .unwrap();
    let id = rt.processes.list()[0];
    let record = rt
        .processes
        .wait_for(id, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(record.is_terminal());
    let events = std::fs::read_to_string(
        dir.path()
            .join("logs")
            .join(id.to_string())
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.contains("queued") && events.contains("completed"));
}
