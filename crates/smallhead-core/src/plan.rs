//! Task plan snapshots and the tool that publishes complete replacements.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};

use crate::diagnostics;
use crate::message::ContentBlock;
use crate::tool::{ToolContext, ToolError, ToolImpl, ToolResult, ToolSpec};

/// Progress reported for a single plan step.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Work has not started.
    Pending,
    /// Work is currently underway.
    InProgress,
    /// Actual evidence supports completion.
    Completed,
}

/// One named step in a task plan.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// Trimmed, nonempty step title.
    pub step: String,
    /// Reported progress of this step.
    pub status: PlanStatus,
}

/// Complete replacement snapshot associated with a task owner.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskPlan {
    /// Task owner, or an empty string when no owner is available.
    pub task_id: String,
    /// Optional reason for updating or replanning.
    pub explanation: Option<String>,
    /// All steps in their intended order.
    pub plan: Vec<PlanStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanInput {
    plan: Vec<PlanStep>,
    #[serde(default, deserialize_with = "deserialize_explanation")]
    explanation: Option<String>,
}

fn deserialize_explanation<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

impl UpdatePlanInput {
    fn validate(&mut self) -> Result<(), ToolError> {
        let invalid = |message: &str| ToolError::InvalidInput(message.into());
        if !(1..=32).contains(&self.plan.len()) {
            return Err(invalid("plan must contain 1..32 steps"));
        }
        if self
            .explanation
            .as_ref()
            .is_some_and(|s| s.chars().count() > 2000)
        {
            return Err(invalid("explanation must be at most 2000 characters"));
        }
        let mut titles = HashSet::new();
        let mut in_progress = 0;
        for step in &mut self.plan {
            step.step = step.step.trim().to_owned();
            if step.step.is_empty() || step.step.chars().count() > 300 {
                return Err(invalid(
                    "step must have a nonempty trimmed title of at most 300 characters",
                ));
            }
            if !titles.insert(step.step.clone()) {
                return Err(invalid("duplicate plan steps are not allowed"));
            }
            if step.status == PlanStatus::InProgress {
                in_progress += 1;
                if in_progress > 1 {
                    return Err(invalid("at most one step may be in_progress"));
                }
            }
        }
        Ok(())
    }
}

/// Publishes the current task's full plan without performing its steps.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct UpdatePlanTool;

#[async_trait]
impl ToolImpl for UpdatePlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            label: "更新任务计划".into(),
            description: "为复杂的多步骤任务维护计划；简单任务无需计划。每次调用用完整 plan 替换当前任务的计划。步骤状态为 pending、in_progress 或 completed，最多一个步骤 in_progress。只有真实执行或验证结果支持时才能标记 completed；计划快照仅记录进度声明，不是工作已经执行的证据。创建计划后继续实际执行，在完成步骤或调整方案时更新；重新规划时用 explanation 说明变化与原因。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["plan"],
                "properties": {
                    "plan": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 32,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["step", "status"],
                            "properties": {
                                "step": {"type": "string", "minLength": 1, "maxLength": 300},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            }
                        }
                    },
                    "explanation": {"type": "string", "maxLength": 2000}
                }
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        if ctx.cancel {
            return Err(ToolError::Execution("plan update cancelled".into()));
        }
        let mut input: UpdatePlanInput = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        input.validate()?;
        let snapshot = TaskPlan {
            task_id: ctx.owner.clone().unwrap_or_default(),
            explanation: input.explanation,
            plan: input.plan,
        };
        let data = serde_json::to_value(snapshot)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let text = data.to_string();
        diagnostics::emit("plan", "plan_updated", data);
        Ok(ToolResult {
            tool_call_id: ctx.tool_call_id.clone().unwrap_or_default(),
            is_error: false,
            content: vec![ContentBlock::Text { text }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolKind;
    use serde_json::{json, Value};

    fn pending(title: &str) -> Value {
        json!({"step": title, "status": "pending"})
    }

    #[tokio::test]
    async fn update_plan_emits_the_returned_snapshot_and_replaces_all_steps() {
        let tool = ToolKind::UpdatePlan(UpdatePlanTool);
        assert_eq!(tool.name(), "update_plan");
        assert_eq!(tool.spec().name, tool.name());
        let ctx = ToolContext {
            owner: Some("task-plan".into()),
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        diagnostics::observe(tx, async {
            let inputs = [
                json!({"plan": [{"step": "  Investigate  ", "status": "in_progress"}, pending("Verify")]}),
                json!({"plan": [{"step": "Revised work", "status": "completed"}],
                    "explanation": "Verification supports the revised scope"}),
            ];
            for input in inputs {
                let result = tool.execute("call-plan", input, &ctx).await.unwrap();
                assert_eq!(result.tool_call_id, "call-plan");
                assert!(!result.is_error);
                assert_eq!(result.content.len(), 1);
                let ContentBlock::Text { text } = &result.content[0] else {
                    panic!("expected a JSON text snapshot");
                };
                let event = rx.try_recv().unwrap();
                assert_eq!(event.layer, "plan");
                assert_eq!(event.event, "plan_updated");
                assert_eq!(serde_json::from_str::<Value>(text).unwrap(), event.data);
                let snapshot: TaskPlan = serde_json::from_value(event.data).unwrap();
                assert_eq!(snapshot.task_id, "task-plan");
                if snapshot.explanation.is_none() {
                    assert_eq!(snapshot.plan[0].step, "Investigate");
                    assert_eq!(snapshot.plan[0].status, PlanStatus::InProgress);
                    assert_eq!(snapshot.plan.len(), 2);
                } else {
                    assert_eq!(snapshot.plan.len(), 1);
                    assert_eq!(snapshot.plan[0].step, "Revised work");
                    assert_eq!(snapshot.plan[0].status, PlanStatus::Completed);
                }
            }
            assert!(rx.try_recv().is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn update_plan_rejects_invalid_input_without_events() {
        let oversized: Vec<_> = (0..33).map(|i| pending(&format!("Step {i}"))).collect();
        let cases = vec![
            json!({}),
            json!({"plan": []}),
            json!({"plan": oversized}),
            json!({"plan": [pending(" \n\t ")]}),
            json!({"plan": [pending(&"x".repeat(301))]}),
            json!({"plan": [pending("Repeat"), pending(" Repeat ")]}),
            json!({"plan": [{"step": "A", "status": "in_progress"},
                            {"step": "B", "status": "in_progress"}]}),
            json!({"plan": [pending("A")], "explanation": "x".repeat(2001)}),
            json!({"plan": [pending("A")], "task_id": "spoofed"}),
            json!({"plan": [{"step": "A", "status": "pending", "extra": true}]}),
            json!({"plan": [{"step": "A", "status": "InProgress"}]}),
            json!({"plan": [{"step": "A", "status": "done"}]}),
            json!({"plan": [{"step": "A"}]}),
            json!({"plan": [{"step": 1, "status": "pending"}]}),
            json!({"plan": [pending("A")], "explanation": null}),
            json!({"plan": [pending("A")], "explanation": 1}),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        diagnostics::observe(tx, async {
            for input in cases {
                let result = UpdatePlanTool
                    .execute(input.clone(), &ToolContext::default())
                    .await;
                assert!(
                    matches!(result, Err(ToolError::InvalidInput(_))),
                    "accepted {input}"
                );
            }
            assert!(rx.try_recv().is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn update_plan_accepts_unicode_limits_and_missing_owner_without_observer() {
        let title = "\u{6b65}".repeat(300);
        let steps: Vec<_> = (0..32)
            .map(|i| {
                if i == 0 {
                    pending(&title)
                } else {
                    pending(&format!("Step {i}"))
                }
            })
            .collect();
        let result = UpdatePlanTool
            .execute(
                json!({"plan": steps, "explanation": "\u{754c}".repeat(2000)}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        let ContentBlock::Text { text } = &result.content[0] else {
            panic!("expected a JSON text snapshot");
        };
        let snapshot: TaskPlan = serde_json::from_str(text).unwrap();
        assert_eq!(snapshot.task_id, "");
        assert_eq!(snapshot.plan.len(), 32);
        assert_eq!(snapshot.plan[0].step, title);
        assert_eq!(snapshot.explanation.unwrap().chars().count(), 2000);
    }

    #[tokio::test]
    async fn update_plan_cancellation_emits_no_update() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolContext {
            cancel: true,
            ..Default::default()
        };
        let result = diagnostics::observe(
            tx,
            UpdatePlanTool.execute(json!({"plan": [pending("Work")]}), &ctx),
        )
        .await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
        assert!(rx.try_recv().is_err());
    }
}
