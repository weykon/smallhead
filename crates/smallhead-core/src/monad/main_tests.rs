//! MainMonad 的适配、任务清理与取消测试；作为其子模块可读取私有状态。

use super::*;
use crate::monad::test_support::{assert_completion_binding, lifecycle_handoff};
use crate::monad::*;

// ========================================================================
// Slice 4 测试:MainMonad + MonadLoop trait
// ========================================================================

/// `main_monad_id()` 全局返回 `"main"`。
#[test]
fn main_monad_id_is_main() {
    assert_eq!(main_monad_id().0, "main");
    // 多次调用返回同一个 'static 引用
    let a = main_monad_id();
    let b = main_monad_id();
    assert!(std::ptr::eq(a, b), "OnceLock 应返回同一指针");
}

/// catalog 摘要包含"对话",标识这是顶层对话循环。
#[test]
fn main_monad_catalog_entry_says_top_level() {
    let m = MainMonad::empty();
    let entry = m.catalog_entry();
    assert!(
        entry.summary.contains("对话"),
        "catalog summary 应包含 '对话',实际: {}",
        entry.summary
    );
    // CLI 执行与查询控制配对暴露。
    assert_eq!(entry.visible_tools.len(), 12);
    assert!(entry
        .visible_tools
        .iter()
        .any(|name| name == "process_control"));
    assert!(entry
        .visible_tools
        .iter()
        .any(|name| name == "artifact_read"));
}

/// `default_tool_set()` 包含 Bash(也包含其他工具)。
#[test]
fn main_monad_default_tool_set_includes_bash() {
    let m = MainMonad::empty();
    let tools = m.default_tool_set();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&"bash"),
        "default tool set 应包含 bash,实际: {:?}",
        names
    );
    // CLI 执行与查询控制配对注册。
    assert_eq!(names.len(), 12);
    assert!(names.contains(&"process_control"));
    assert!(names.contains(&"artifact_read"));
}

/// `step()` on 空实例(无 `AgentLoop`) 必须返回 Err,指明"未 wrap"。
#[tokio::test]
async fn main_monad_step_without_wrap_returns_err() {
    let mut m = MainMonad::empty();
    let result = m.step().await;
    assert!(result.is_err(), "step() on empty MainMonad 必须返回 Err");
    let err = result.unwrap_err();
    assert!(
        err.0.contains("MainMonad::wrap"),
        "错误信息应提示调用 wrap(),实际: {}",
        err.0
    );
}

/// `step()` on 未 start 的 wrap 实例也要返回 Err —— 没有 user 消息可喂。
#[tokio::test]
async fn main_monad_step_without_start_returns_err() {
    let provider = std::sync::Arc::new(crate::provider::MockProvider::new(vec![
        crate::provider::MockResponse::AssistantText {
            text: "unused".into(),
        },
    ]));
    let mut m = MainMonad::wrap(crate::AgentLoop::new(
        provider,
        vec![crate::ToolKind::Echo(crate::EchoTool)],
        "test",
    ));
    let result = m.step().await;
    assert!(result.is_err(), "step() 在 start() 之前调用必须返回 Err");
    let err = result.unwrap_err();
    assert!(
        err.0.contains("start"),
        "错误信息应提示 start(),实际: {}",
        err.0
    );
}

/// `step()` 真桥接到 AgentLoop:MockProvider 返回纯文本,完成单 turn,
/// 翻译成 `MonadResult::Completed` 并回填 handoff 标识。
#[tokio::test]
async fn main_monad_step_invokes_agent_loop_and_returns_completed() {
    use crate::provider::{MockProvider, MockResponse};
    use crate::tool::EchoTool;
    use crate::ToolKind;

    let provider = std::sync::Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
        text: "你好!".into(),
    }]));
    let mut m = MainMonad::wrap(crate::AgentLoop::new(
        provider,
        vec![ToolKind::Echo(EchoTool)],
        "你是一名乐于助人的助手。",
    ));

    // start() 注入 handoff 标识
    let expected_task_id = TaskId::new();
    let expected_result_slot = ResultSlot("main.first".into());
    let expected_parent_task_id = TaskId::new();
    m.start(MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: expected_task_id.clone(),
        parent_task_id: Some(expected_parent_task_id.clone()),
        monad_id: MonadId::new("main"),
        attempt_id: 1,
        objective: "打个招呼".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "回复一句话".into(),
        result_slot: expected_result_slot.clone(),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    })
    .await
    .expect("start 应成功");

    let result = m.step().await.expect("step 应成功");
    let (
        task_id,
        parent_task_id,
        result_slot,
        monad_id,
        output,
        evidence_refs,
        usage,
        finished_at_ms,
    ) = match result {
        MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            monad_id,
            output,
            evidence_refs,
            usage,
            finished_at_ms,
        } => (
            task_id,
            parent_task_id,
            result_slot,
            monad_id,
            output,
            evidence_refs,
            usage,
            finished_at_ms,
        ),
        other => panic!("MockProvider 单 turn 完成,应返回 Completed;实际: {other:?}"),
    };

    // task 标识回填
    assert_eq!(task_id, expected_task_id);
    assert_eq!(parent_task_id, Some(expected_parent_task_id));
    assert_eq!(result_slot, expected_result_slot);
    assert_eq!(monad_id.0, "main");

    // output JSON 含 turns/tool_calls/final_text
    assert_eq!(output["turns_completed"], serde_json::json!(1));
    assert_eq!(output["tool_calls_executed"], serde_json::json!(0));
    assert_eq!(output["final_text"], serde_json::json!("你好!"));

    // 单 turn MVP:无 tool_calls,evidence_refs 空
    assert!(evidence_refs.is_empty());
    assert_eq!(usage.steps_taken, 1);

    // finished_at_ms 应 > 0(epoch 之后)
    assert!(finished_at_ms > 0);

    // step() 消费后,frame 与 current_* 已清空,下次 start() 才能再走.
    // debug 不爆炸说明字段都齐
    let _ = format!("{:?}", m);
}

/// `cancel()` on 空 MainMonad 是 no-op(返回 Ok())。
#[tokio::test]
async fn main_monad_cancel_on_empty_is_noop() {
    let mut m = MainMonad::empty();
    m.cancel().await.expect("cancel on empty MainMonad 应是 Ok");
}

/// `start()` 把 handoff.objective push 到 frame。
#[tokio::test]
async fn main_monad_start_pushes_objective() {
    let mut m = MainMonad::empty();
    assert!(m.frame.is_empty(), "初始 frame 应为空");

    let handoff = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: None,
        monad_id: MonadId::new("main"),
        attempt_id: 1,
        objective: "帮我算 1+1".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "返回结果".into(),
        result_slot: ResultSlot("main.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    };

    m.start(handoff.clone()).await.expect("start 应成功");
    assert_eq!(m.frame.len(), 1, "frame 应含 1 条 user message");
    assert!(m.started, "started flag 应被设置");

    // frame[0] 是 User message,文本内容 == objective
    match &m.frame[0] {
        LlmMessage::User { content } => {
            assert_eq!(content.len(), 1);
            match &content[0] {
                crate::message::ContentBlock::Text { text } => {
                    assert_eq!(text, "帮我算 1+1");
                }
                _ => panic!("frame[0] 应是 Text content block"),
            }
        }
        _ => panic!("frame[0] 应是 User message"),
    }
}

/// `monad_id()` 返回与全局一致的 MonadId。
#[test]
fn main_monad_trait_returns_same_id_as_global() {
    let m = MainMonad::empty();
    assert_eq!(m.monad_id(), main_monad_id());
    assert_eq!(m.monad_id().0, "main");
}

#[tokio::test]
async fn main_rejects_overwrite_and_resets_after_completion_and_cancel() {
    use crate::provider::{MockProvider, MockResponse};
    let provider = std::sync::Arc::new(MockProvider::new(vec![
        MockResponse::AssistantText {
            text: "first".into(),
        },
        MockResponse::AssistantText {
            text: "second".into(),
        },
    ]));
    let mut m = MainMonad::wrap(crate::AgentLoop::new(provider, vec![], "test"));
    let first = lifecycle_handoff("main");
    m.start(first.clone()).await.unwrap();
    assert!(m.start(lifecycle_handoff("main")).await.is_err());
    assert_eq!(m.frame.len(), 1);
    assert_completion_binding(m.step().await.unwrap(), &first);
    assert!(!m.started);
    assert!(m.step().await.is_err());
    let second = lifecycle_handoff("main");
    m.start(second.clone()).await.unwrap();
    assert_completion_binding(m.step().await.unwrap(), &second);
    m.start(lifecycle_handoff("main")).await.unwrap();
    m.cancel().await.unwrap();
    assert!(!m.started);
    assert!(m.frame.is_empty());
    assert!(!m.current_task.is_bound());
    assert!(m.step().await.is_err());
}

/// 重复取消及满通知队列都不阻塞，取消后能执行新任务。
#[tokio::test]
async fn main_repeated_cancel_with_full_queue_allows_new_task() {
    use crate::provider::{MockProvider, MockResponse};
    let provider = std::sync::Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
        text: "fresh result".into(),
    }]));
    let mut m = MainMonad::wrap(crate::AgentLoop::new(provider, vec![], "test"));
    // 多次绑定后取消，超过无人消费的取消队列容量，覆盖 Full 分支。
    for _ in 0..16 {
        m.start(lifecycle_handoff("main")).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(200), m.cancel())
            .await
            .expect("cancel must not block")
            .unwrap();
        assert!(!m.current_task.is_bound());
        assert!(!m.started);
        assert!(m.frame.is_empty());
    }
    for _ in 0..16 {
        tokio::time::timeout(std::time::Duration::from_millis(200), m.cancel())
            .await
            .expect("repeated cancel must not block")
            .unwrap();
    }
    let fresh = lifecycle_handoff("main");
    m.start(fresh.clone()).await.unwrap();
    let result = m.step().await.unwrap();
    assert_completion_binding(result.clone(), &fresh);
    match result {
        MonadResult::Completed { output, .. } => {
            assert_eq!(output["final_text"], "fresh result");
        }
        other => panic!("expected completion, got {other:?}"),
    }
}

/// §14.2 行为测试:`run_one_turn` 失败时,MainMonad 须清空 frame / started /
/// current_task,然后允许 fresh `start()` —— 不要把用户卡在"半完成"状态。
/// 用空 MockProvider 队列触发 "MockProvider exhausted" 错误。
#[tokio::test]
async fn main_step_failure_cleans_state_and_allows_restart() {
    use crate::provider::MockProvider;
    // 空响应队列 → `stream` 立即返回 ProviderError::Other("exhausted"),
    // 走到 MainMonad::step 里的 Err 分支,触发 frame/started/current_task 清理。
    let provider = std::sync::Arc::new(MockProvider::new(vec![]));
    let mut m = MainMonad::wrap(crate::AgentLoop::new(provider, vec![], "test"));
    let first = lifecycle_handoff("main");
    m.start(first.clone()).await.unwrap();
    assert!(m.started, "start sets started flag");
    assert_eq!(m.frame.len(), 1);
    assert!(m.current_task.is_bound());

    // 第一次 step 必失败(空 provider 队列)
    let err = m
        .step()
        .await
        .expect_err("step must fail with empty provider");
    assert!(
        err.0.contains("AgentLoop::run_one_turn failed"),
        "err msg: {}",
        err.0
    );

    // 失败后状态必须完整清理
    assert!(!m.started, "started must reset after failed step");
    assert!(m.frame.is_empty(), "frame must clear after failed step");
    assert!(
        !m.current_task.is_bound(),
        "current_task must release after failed step"
    );

    // fresh start() 应通过;若半完成状态留下,会再被 "monad already has an active task" 卡住
    let second = lifecycle_handoff("main");
    m.start(second.clone())
        .await
        .expect("fresh start after failure must succeed");
    assert!(m.started);
    assert_eq!(m.frame.len(), 1);
    assert!(m.current_task.is_bound());
}
