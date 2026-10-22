//! Monad 生命周期测试共用的交接参数与结果断言。

use super::*;

pub(super) fn lifecycle_handoff(id: &str) -> MonadHandoff {
    MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.into(),
        task_id: TaskId::new(),
        parent_task_id: Some(TaskId::new()),
        monad_id: MonadId::new(id),
        attempt_id: 1,
        objective: "first task".into(),
        inputs: match id {
            "calculation" => serde_json::json!({"expression":"5*6"}),
            "echo" => serde_json::json!({"echo_text":"first"}),
            "greeting" => serde_json::json!({"name":"Ada"}),
            _ => serde_json::json!({}),
        }
        .as_object()
        .unwrap()
        .clone(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "return result".into(),
        result_slot: ResultSlot("parent.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: "frame".into(),
            request_id: "request".into(),
        },
    }
}

pub(super) fn assert_completion_binding(result: MonadResult, handoff: &MonadHandoff) {
    match result {
        MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            monad_id,
            ..
        } => {
            assert_eq!(task_id, handoff.task_id);
            assert_eq!(parent_task_id, handoff.parent_task_id);
            assert_eq!(result_slot, handoff.result_slot);
            assert_eq!(monad_id, handoff.monad_id);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}
