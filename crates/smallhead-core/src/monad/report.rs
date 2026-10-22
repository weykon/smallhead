//! Child-facing actions for communicating with the owning Monad.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ReportKind;
use crate::{tool::ToolImpl, ContentBlock, ToolContext, ToolError, ToolResult, ToolSpec};

/// Shared child instructions. Control requests are explicit tools, not phrases
/// guessed from model output. Scope and authority remain host-owned.
pub const COLLABORATION_GUIDANCE: &str = "\n# 与主 Monad 协作\n你只处理交接给你的目标，主 Monad 负责统筹和最终验收。使用 monad_report(kind=progress) 汇报重要阶段进展；kind=finding 汇报影响其他工作的已核实发现，注明来源和不确定性。无需汇报每次读文件。遇到缺少关键输入、目标冲突、需扩大范围或影响结论的歧义，使用 kind=alignment_request，写清已知事实、需要主 Monad 决定的问题与建议；存在阻碍用 kind=blocked，说明阻碍和可行的替代路线。这两种动作会创建 request_id 并在安全边界等待主 Monad 对应回复，不能自行猜测批准，也不能继续执行同一响应中与该决定有关的工具。主 Monad 的回复和补充会作为有编号的协作指示进入本任务上下文；结合原目标调整工作，不忽略补充。暂停在安全边界生效，不能撤回在途调用，也不会回滚已完成操作。resume 只解除显式暂停，未回答的对齐请求仍须 reply；取消将终止任务并清理资源，不能用新任务盲目重放未知副作用。收到暂停/取消/变更请求后，不声称已经生效，依据程序回执。完成时提交带证据的简短最终结论、覆盖范围、未解决问题及限制，不能用一条 progress/finding 冒充最终完成。报告失败（队列满/任务已结束）必须如实处理，不能声称已送达。\n";

/// Available only inside a host-bound delegated Monad. The model cannot choose
/// a sender, parent, attempt or destination.
#[derive(Clone, Debug, Default)]
pub struct MonadReportTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    kind: ReportKind,
    message: String,
}

#[async_trait]
impl ToolImpl for MonadReportTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "monad_report".into(), label: "Report to parent Monad".into(),
            description: "Report substantive progress or findings to the owning Monad. alignment_request or blocked opens a correlated request and suspends at the next safe boundary until the parent replies to that request_id. Reports are not final completion. Sender/parent/attempt are host-bound; do not repeat an accepted report. State facts, sources, uncertainty, and exactly what decision is needed.".into(),
            input_schema: json!({"type":"object","properties":{
                "kind":{"type":"string","enum":["progress","finding","blocked","alignment_request"]},
                "message":{"type":"string","minLength":1,"maxLength":8192}
            },"required":["kind","message"],"additionalProperties":false}),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        if ctx.cancel || ctx.cancellation.is_cancelled() {
            return Err(ToolError::Execution("task cancelled".into()));
        }
        let report: Report = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let channel = ctx.monad_channel.as_ref().ok_or_else(|| {
            ToolError::Execution("monad_report requires a host-bound child channel".into())
        })?;
        let receipt = channel
            .report(report.kind, report.message, None)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: json!({"accepted":receipt,
                "delivery":"recorded for parent; not proof parent has read or accepted findings"})
                .to_string(),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::{CollaborationHub, TaskId};
    use std::sync::Arc;

    #[tokio::test]
    async fn report_requires_host_binding_and_rejects_forged_destination() {
        let hub = Arc::new(CollaborationHub::default());
        let parent = TaskId::new();
        let channel = hub.register(parent.clone(), TaskId::new(), 1).unwrap();
        let mut ctx = ToolContext {
            monad_collaboration: hub.clone(),
            ..Default::default()
        };
        let input = json!({"kind":"finding","message":"Verified evidence"});
        assert!(MonadReportTool.execute(input.clone(), &ctx).await.is_err());
        ctx.monad_channel = Some(channel);
        let mut forged = input.clone();
        forged["parent_task_id"] = json!("another-parent");
        assert!(MonadReportTool.execute(forged, &ctx).await.is_err());
        assert_eq!(hub.cursor(&parent), 0);
        assert!(MonadReportTool
            .execute(json!({"kind":"progress","message":"  "}), &ctx)
            .await
            .is_err());
        MonadReportTool.execute(input, &ctx).await.unwrap();
        let events = hub.events(&parent, 0, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].parent, parent);
        assert_eq!(
            events[0].kind,
            crate::monad::CollaborationEventKind::Report(ReportKind::Finding)
        );
    }
}
