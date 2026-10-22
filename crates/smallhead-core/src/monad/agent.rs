//! An independently contextualized model loop implementing the Monad contract.

use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{CatalogEntry, MonadError, MonadHandoff, MonadId, MonadLoop, MonadResult, TaskSlot};
use crate::{
    AgentLoop, AgentMessage, ContentBlock, LlmStopReason, Provider, ProviderError, ProviderEvent,
    ProviderRequest, ToolContext, ToolKind,
};

#[derive(Default)]
struct Meter {
    steps: u32,
    tokens: u32,
    usage_reported: bool,
}

/// Host-configured Monad with fresh history and tools for each accepted handoff.
/// Sharing the provider shares transport configuration, never conversation state.
pub struct AgentMonad {
    id: MonadId,
    catalog: CatalogEntry,
    provider: Arc<dyn Provider>,
    tools: Vec<ToolKind>,
    system: String,
    context: ToolContext,
    task: TaskSlot,
    agent: Option<AgentLoop>,
    prompt: Option<String>,
    attempt_id: u32,
    meter: Arc<Mutex<Meter>>,
}

impl AgentMonad {
    /// Tools are selected by the host, not by untrusted delegation parameters.
    pub fn new(
        id: MonadId,
        summary: impl Into<String>,
        provider: Arc<dyn Provider>,
        tools: Vec<ToolKind>,
        system: impl Into<String>,
        context: ToolContext,
    ) -> Self {
        Self {
            id,
            catalog: CatalogEntry {
                summary: summary.into(),
                visible_tools: tools.iter().map(|tool| tool.name().to_owned()).collect(),
                keywords: vec![],
            },
            provider,
            tools,
            system: system.into(),
            context,
            task: TaskSlot::new(),
            agent: None,
            prompt: None,
            attempt_id: 0,
            meter: Arc::new(Mutex::new(Meter::default())),
        }
    }
}

#[async_trait]
impl MonadLoop for AgentMonad {
    fn collaboration_channel(&self) -> Option<super::ChildChannel> {
        self.context.monad_channel.clone()
    }
    fn monad_id(&self) -> &MonadId {
        &self.id
    }
    fn catalog_entry(&self) -> &CatalogEntry {
        &self.catalog
    }
    fn default_tool_set(&self) -> Vec<ToolKind> {
        self.tools.clone()
    }

    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        if handoff.objective.trim().is_empty()
            || handoff.budget.max_steps == 0
            || handoff.budget.max_tokens == 0
        {
            return Err(MonadError::new(
                "agent Monad requires objective and positive step/token budgets",
            ));
        }
        self.task.try_bind(&handoff, &self.id)?;
        self.attempt_id = handoff.attempt_id;
        self.meter = Arc::new(Mutex::new(Meter::default()));
        let provider = Arc::new(BudgetProvider {
            inner: self.provider.clone(),
            meter: self.meter.clone(),
            max_steps: handoff.budget.max_steps,
            max_tokens: handoff.budget.max_tokens,
            cancel: self.context.cancellation.child_token(),
        });
        let mut tools = self.tools.clone();
        if self.context.monad_channel.is_some() {
            tools.push(ToolKind::External(crate::tool::ExternalTool::new(
                Arc::new(super::MonadReportTool),
            )));
        }
        let mut agent = AgentLoop::new(
            provider,
            tools,
            format!("{}{}", self.system, super::COLLABORATION_GUIDANCE),
        );
        agent.tool_ctx = self.context.clone();
        agent.tool_ctx.input_source = crate::tool::InputSource::Delegate;
        agent.tool_ctx.accepted_input = None;
        agent.tool_ctx.owner = Some(format!(
            "monad:{}:{}",
            handoff.task_id.0, handoff.attempt_id
        ));
        agent.tool_ctx.tool_call_id = None;
        agent.tool_ctx.cancellation = self.context.cancellation.child_token();
        if let Some(channel) = &agent.tool_ctx.monad_channel {
            agent.set_steering_inbox(channel.steering_inbox());
        }
        agent.budget.max_model_waves = handoff.budget.max_steps as usize;
        self.prompt = Some(
            json!({
                "objective": handoff.objective,
                "inputs": handoff.inputs,
                "relevant_context": handoff.relevant_context,
                "constraints": handoff.constraints,
                "completion_criteria": handoff.completion_criteria,
            })
            .to_string(),
        );
        self.agent = Some(agent);
        Ok(())
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let binding = self.task.take()?;
        let prompt = self
            .prompt
            .take()
            .ok_or_else(|| MonadError::new("missing delegated prompt"))?;
        let agent = self
            .agent
            .as_mut()
            .ok_or_else(|| MonadError::new("missing child loop"))?;
        let outcome = crate::diagnostics::in_monad(
            binding.task_id().0.clone(),
            self.attempt_id,
            agent.run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text { text: prompt }],
                timestamp: super::main_monad::now_ms(),
            }),
        )
        .await;
        let failure = match outcome {
            Err(error) => Some(error.to_string()),
            Ok(summary) => summary.finalization_reason,
        };
        if let Some(error) = failure {
            return Ok(MonadResult::Failed {
                task_id: binding.task_id().clone(),
                error,
                retryable: false,
            });
        }
        let text = match agent.state.messages.last() {
            Some(AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            }) if tool_calls.is_empty() => content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if text.trim().is_empty() {
            return Ok(MonadResult::Failed {
                task_id: binding.task_id().clone(),
                error: "child returned no final findings".into(),
                retryable: false,
            });
        }
        let transcript = crate::artifact::store_history_compaction(
            &agent.state.messages,
            agent.tool_ctx.artifact_root.as_deref(),
        )
        .map_err(|error| MonadError::new(format!("cannot persist child evidence: {error}")))?;
        let mut findings = text;
        let truncated = findings.len() > 6_000;
        if truncated {
            let mut boundary = 6_000;
            while !findings.is_char_boundary(boundary) {
                boundary -= 1;
            }
            findings.truncate(boundary);
            findings.push_str("\n[Full findings are in the transcript artifact.]");
        }
        let meter = self.meter.lock();
        let usage = TaskSlot::usage_after(&binding, meter.steps, meter.tokens);
        let output = json!({
            "findings": [ContentBlock::Text { text: findings }],
            "findings_truncated": truncated,
            "usage_reported": meter.usage_reported,
            "acceptance": "host binding checked; domain criteria require a domain validator",
        });
        Ok(MonadResult::Completed {
            task_id: binding.task_id().clone(),
            parent_task_id: binding.parent_task_id().cloned(),
            result_slot: binding.result_slot().clone(),
            monad_id: self.id.clone(),
            output,
            evidence_refs: vec![transcript.artifact_ref],
            usage,
            finished_at_ms: super::main_monad::now_ms(),
        })
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task.clear();
        self.prompt = None;
        if let Some(agent) = self.agent.as_mut() {
            agent.tool_ctx.cancellation.cancel();
            agent.recover_interrupted("delegated Monad cancelled");
            if let (Some(runtime), Some(owner)) = (&agent.tool_ctx.execution, &agent.tool_ctx.owner)
            {
                crate::execution::stop_and_reap_owner(runtime, owner)
                    .await
                    .map_err(|error| MonadError::new(format!("child cleanup failed: {error}")))?;
            }
        }
        Ok(())
    }
}

// Admission happens before a provider request. Reported usage is checked before
// a response can execute tools; missing provider usage is not an exact token cap.
struct BudgetProvider {
    inner: Arc<dyn Provider>,
    meter: Arc<Mutex<Meter>>,
    max_steps: u32,
    max_tokens: u32,
    cancel: CancellationToken,
}

#[async_trait]
impl Provider for BudgetProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn context_window(&self) -> usize {
        self.inner.context_window()
    }
    fn cache_key(&self, request: &ProviderRequest) -> String {
        self.inner.cache_key(request)
    }
    fn abort_signal(&self) -> CancellationToken {
        self.cancel.clone()
    }
    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        self.stream_with_cancel(request, self.cancel.child_token())
            .await
    }
    async fn stream_with_cancel(
        &self,
        mut request: ProviderRequest,
        cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let previous = {
            let mut meter = self.meter.lock();
            if meter.steps >= self.max_steps || meter.tokens >= self.max_tokens {
                return Err(ProviderError::Other(
                    "delegated Monad exhausted provider budget".into(),
                ));
            }
            meter.steps += 1;
            request.max_tokens = request
                .max_tokens
                .min((self.max_tokens - meter.tokens) as usize);
            meter.tokens
        };
        let mut stream = self.inner.stream_with_cancel(request, cancel).await?;
        let meter = self.meter.clone();
        let limit = self.max_tokens;
        Ok(Box::pin(async_stream::stream! {
            let mut wave_tokens = 0;
            while let Some(event) = stream.next().await {
                if let ProviderEvent::MessageDelta { usage, stop_reason } = &event {
                    wave_tokens = wave_tokens.max(usage.input_tokens.saturating_add(usage.output_tokens));
                    {
                        let mut meter = meter.lock();
                        meter.usage_reported |= usage.input_tokens > 0 || usage.output_tokens > 0;
                        meter.tokens = previous.saturating_add(wave_tokens);
                    }
                    let used = previous.saturating_add(wave_tokens);
                    let failure = if used > limit {
                        Some(format!("delegated token budget exceeded: used={used}, limit={limit}, previous={previous}, wave={wave_tokens}"))
                    } else {
                        match stop_reason {
                            LlmStopReason::MaxTokens => Some("delegated response truncated: provider stop_reason=max_tokens".into()),
                            LlmStopReason::ContentFilter => Some("delegated response filtered: provider stop_reason=content_filter".into()),
                            _ => None,
                        }
                    };
                    if let Some(error) = failure {
                        crate::diagnostics::emit("monad_budget", "rejected_response", json!({
                            "used":used,"limit":limit,"previous":previous,"wave_tokens":wave_tokens,
                            "stop_reason":format!("{stop_reason:?}"),"error":error,
                        }));
                        yield ProviderEvent::Error {
                            error: ProviderError::Other(error),
                            retryable: false,
                        };
                        break;
                    }
                }
                yield event;
            }
        }))
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;

#[cfg(test)]
mod input_provenance_tests {
    use super::*;

    #[tokio::test]
    async fn trusted_input_child_never_inherits_user_evidence() {
        let provider = Arc::new(crate::MockProvider::new(vec![
            crate::MockResponse::AssistantText {
                text: "done".into(),
            },
        ]));
        let root = tempfile::tempdir().unwrap();
        let mut child = AgentMonad::new(
            MonadId::new("research"),
            "research",
            provider,
            vec![],
            "test",
            ToolContext {
                input_source: crate::InputSource::User,
                accepted_input: Some(crate::AcceptedInput {
                    id: "parent-input".into(),
                    text: "private user evidence".into(),
                    source: crate::InputSource::User,
                }),
                artifact_root: Some(root.path().into()),
                ..Default::default()
            },
        );
        child
            .start(crate::monad::test_support::lifecycle_handoff("research"))
            .await
            .unwrap();
        let context = &child.agent.as_ref().unwrap().tool_ctx;
        assert_eq!(context.input_source, crate::InputSource::Delegate);
        assert!(context.accepted_input.is_none());
        child.step().await.unwrap();
        let accepted = child
            .agent
            .as_ref()
            .unwrap()
            .tool_ctx
            .accepted_input
            .as_ref()
            .unwrap();
        assert_eq!(accepted.source, crate::InputSource::Delegate);
        assert_ne!(accepted.id, "parent-input");
        assert!(!accepted.text.contains("private user evidence"));
    }
}
