//! Parent-owned controls. All targets are checked against the bound parent and
//! exact run/attempt before exposing state or mutating an endpoint.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{MonadRuntime, RunSnapshot, TaskId};
use crate::{ContentBlock, ToolContext, ToolError, ToolResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    action: String,
    task_id: Option<TaskId>,
    run_id: Option<String>,
    attempt_id: Option<u32>,
    request_id: Option<u64>,
    message: Option<String>,
    #[serde(default)]
    after: u64,
    timeout_ms: Option<u64>,
}

pub(super) fn record_json(record: &RunSnapshot) -> Value {
    json!({"handoff":record.handoff,"result":record.result,"run_id":record.run_id,
        "task_id":record.handoff.task_id,"attempt_id":record.handoff.attempt_id,
        "state":format!("{:?}",record.state),"terminal":record.state.is_terminal(),
        "cleanup":format!("{:?}",record.cleanup),"error":record.error.as_ref().map(ToString::to_string)})
}

pub(super) async fn execute(input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let command: Control = serde_json::from_value(input)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if !matches!(
        command.action.as_str(),
        "status" | "wait" | "send" | "reply" | "pause" | "resume" | "cancel"
    ) {
        return Err(ToolError::InvalidInput(
            "unknown Monad control action; discover first".into(),
        ));
    }
    if (command.task_id.is_none() && (command.run_id.is_some() || command.attempt_id.is_some()))
        || (command.request_id.is_some() && command.action != "reply")
        || (command.message.is_some()
            && !matches!(command.action.as_str(), "send" | "reply" | "resume"))
        || (command.timeout_ms.is_some() && command.action != "wait")
    {
        return Err(ToolError::InvalidInput(
            "fields do not match the selected action; discover first".into(),
        ));
    }
    if ctx.monad_channel.is_some() {
        return Err(ToolError::Execution(
            "parent controls cannot be used from a delegated child".into(),
        ));
    }
    let parent = ctx
        .owner
        .as_ref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| TaskId(id.clone()))
        .ok_or_else(|| ToolError::Execution("control requires a host-bound parent".into()))?;
    let runtime = ctx
        .monad_runtime
        .clone()
        .unwrap_or_else(MonadRuntime::shared);
    let hub = &ctx.monad_collaboration;
    if command.after > hub.cursor(&parent) {
        return Err(ToolError::InvalidInput(
            "after exceeds the current parent event cursor".into(),
        ));
    }
    let target = match &command.task_id {
        Some(task_id) => {
            let record = runtime
                .get(task_id)
                .filter(|record| record.handoff.parent_task_id.as_ref() == Some(&parent))
                .ok_or_else(|| ToolError::Execution("task is not owned by this parent".into()))?;
            if command.run_id.is_none() || command.attempt_id.is_none() {
                let missing: Vec<_> = [
                    command.run_id.is_none().then_some("run_id"),
                    command.attempt_id.is_none().then_some("attempt_id"),
                ]
                .into_iter()
                .flatten()
                .collect();
                return Err(ToolError::InvalidInput(format!(
                    "missing required target fields: {}. The action '{}' IS supported; this is an input error, not an unavailable capability. Retry the same action with task_id={}, run_id={}, attempt_id={} copied from the receipt. Preserve the intended message/request_id. Do not restart or ask the user to choose a workaround for missing fields.",
                    missing.join(", "), command.action, record.handoff.task_id.0,
                    record.run_id, record.handoff.attempt_id,
                )));
            }
            if command.run_id.as_deref() != Some(record.run_id.as_str())
                || command.attempt_id != Some(record.handoff.attempt_id)
            {
                return Err(ToolError::InvalidInput(format!(
                    "target identity mismatch; '{}' is supported but no action was applied. Refresh via status or retry with this task's exact receipt: {}. Copy IDs literally; do not change a character or combine different tasks.",
                    command.action, json!({"task_id":record.handoff.task_id,"run_id":record.run_id,"attempt_id":record.handoff.attempt_id}),
                )));
            }
            let channel = hub
                .channel(&parent, task_id, record.handoff.attempt_id)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            Some((record, channel))
        }
        None if matches!(command.action.as_str(), "status" | "wait") => None,
        None => {
            return Err(ToolError::InvalidInput(
                "this action requires task_id, run_id and attempt_id".into(),
            ))
        }
    };
    let mut acknowledgement = Value::Null;
    if let Some((record, channel)) = &target {
        let message = || {
            command
                .message
                .clone()
                .filter(|message| !message.trim().is_empty())
                .ok_or_else(|| ToolError::InvalidInput("nonempty message required".into()))
        };
        match command.action.as_str() {
            "send" => acknowledgement = json!(channel.send(message()?).map_err(control_error)?),
            "reply" => {
                acknowledgement = json!(channel
                    .reply(
                        command.request_id.ok_or_else(|| ToolError::InvalidInput(
                            "reply requires request_id".into()
                        ))?,
                        message()?
                    )
                    .map_err(control_error)?)
            }
            "pause" => {
                channel.pause().map_err(control_error)?;
            }
            "resume" => {
                channel
                    .resume(command.message.clone())
                    .map_err(control_error)?;
            }
            "cancel" if !record.state.is_terminal() => {
                runtime
                    .cancel_task(&record.handoff.task_id, &record.run_id)
                    .map_err(control_error)?;
                // Completion may race the request; an already-ended endpoint
                // is a successful no-op after the runtime accepted cancellation.
                if let Err(error) = channel.cancel() {
                    if channel.snapshot().phase != super::CollaborationPhase::Ended {
                        return Err(control_error(error));
                    }
                }
            }
            _ => {}
        }
    }
    if command.action == "wait" {
        let timeout = command.timeout_ms.unwrap_or(10_000);
        if !(1..=30_000).contains(&timeout) {
            return Err(ToolError::InvalidInput(
                "wait timeout_ms must be 1..30000; timeout does not cancel a worker".into(),
            ));
        }
        let active = runtime.snapshot().iter().any(|record| {
            record.handoff.parent_task_id.as_ref() == Some(&parent)
                && !record.state.is_terminal()
                && target
                    .as_ref()
                    .is_none_or(|(target, _)| target.run_id == record.run_id)
        });
        if active {
            tokio::select! {
                biased;
                _ = ctx.cancellation.cancelled() => return Err(ToolError::Execution("parent cancelled while waiting".into())),
                _ = hub.wait_changed(&parent, command.after, Duration::from_millis(timeout)) => {}
            }
        }
    }
    let results: Vec<_> = runtime
        .snapshot()
        .iter()
        .filter(|record| record.handoff.parent_task_id.as_ref() == Some(&parent))
        .filter(|record| {
            target
                .as_ref()
                .is_none_or(|(target, _)| target.run_id == record.run_id)
        })
        .map(record_json)
        .collect();
    let observed = hub.cursor(&parent);
    let events = hub.events(&parent, command.after, 64);
    let cursor = events.last().map_or(observed, |event| event.message_id);
    let channels = hub.children(&parent);
    Ok(ToolResult { tool_call_id: String::new(), is_error: false,
        content: vec![ContentBlock::Text { text: json!({"results":results,"channels":channels,
            "events":events,"cursor":cursor,"acknowledgement":acknowledgement,
            "delivery":"accepted instructions are applied at safe boundaries; inspect delivered receipts",
            "waiting_timeout_cancels_task":false}).to_string() }] })
}

fn control_error(error: super::MonadError) -> ToolError {
    ToolError::Execution(error.to_string())
}
