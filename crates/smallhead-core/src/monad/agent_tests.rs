use super::*;
use crate::{monad::test_support::lifecycle_handoff, MockProvider, MockResponse};

fn child(provider: Arc<dyn Provider>, root: &std::path::Path) -> AgentMonad {
    AgentMonad::new(
        MonadId::new("research"),
        "research",
        provider,
        vec![],
        "research",
        ToolContext {
            artifact_root: Some(root.into()),
            owner: Some("parent".into()),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn reused_adapter_gets_fresh_history_owner_and_actual_evidence() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::AssistantText {
            text: "first findings".into(),
        },
        MockResponse::AssistantText {
            text: "second findings".into(),
        },
    ]));
    let mut child = child(provider, root.path());
    for expected in ["first findings", "second findings"] {
        let handoff = lifecycle_handoff("research");
        child.start(handoff.clone()).await.unwrap();
        assert_ne!(
            child.agent.as_ref().unwrap().tool_ctx.owner.as_deref(),
            Some("parent")
        );
        let result = child.step().await.unwrap();
        let MonadResult::Completed {
            output,
            evidence_refs,
            usage,
            ..
        } = result
        else {
            panic!("{result:?}")
        };
        assert_eq!(output["findings"][0]["text"], expected);
        assert_eq!(usage.steps_taken, 1);
        assert_eq!(child.agent.as_ref().unwrap().state.messages.len(), 2);
        let payload = crate::artifact::read_artifact_page(
            &evidence_refs[0],
            Some(root.path()),
            0,
            1024,
            None,
        );
        // The transcript ref must be backed by a file; it is not a fabricated URI.
        assert!(payload.is_ok(), "{payload:?}");
        assert!(child.step().await.is_err(), "binding is consumed once");
    }
}

#[tokio::test]
async fn empty_findings_cannot_reuse_a_previous_answer() {
    let root = tempfile::tempdir().unwrap();
    let mut child = child(
        Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
            text: "".into(),
        }])),
        root.path(),
    );
    child.start(lifecycle_handoff("research")).await.unwrap();
    assert!(matches!(
        child.step().await.unwrap(),
        MonadResult::Failed { .. }
    ));
}

#[tokio::test]
async fn wave_budget_is_enforced_before_another_provider_request() {
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall {
            name: "echo".into(),
            input: json!({"message":"hello"}),
        },
        MockResponse::AssistantText {
            text: "should not be consumed".into(),
        },
    ]));
    let root = tempfile::tempdir().unwrap();
    let mut child = child(provider.clone(), root.path());
    child.tools = vec![ToolKind::Echo(crate::EchoTool)];
    let mut handoff = lifecycle_handoff("research");
    handoff.budget.max_steps = 1;
    child.start(handoff).await.unwrap();
    assert!(matches!(
        child.step().await.unwrap(),
        MonadResult::Failed { .. }
    ));
    assert_eq!(provider.responses.lock().unwrap().len(), 1);
}

struct ReportedUsage(LlmStopReason);
#[async_trait]
impl Provider for ReportedUsage {
    fn id(&self) -> &str {
        "usage"
    }
    fn model(&self) -> &str {
        "usage"
    }
    fn context_window(&self) -> usize {
        1000
    }
    fn cache_key(&self, _: &ProviderRequest) -> String {
        String::new()
    }
    fn abort_signal(&self) -> CancellationToken {
        CancellationToken::new()
    }
    async fn stream(
        &self,
        _: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        Ok(Box::pin(futures::stream::iter(vec![
            ProviderEvent::MessageDelta {
                usage: crate::Usage {
                    input_tokens: 20,
                    output_tokens: 5,
                    cache_hit_tokens: 0,
                },
                stop_reason: self.0,
            },
            ProviderEvent::MessageStop,
        ])))
    }
}

#[tokio::test]
async fn usage_over_budget_rejects_response_before_it_can_execute_tools() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    let provider = BudgetProvider {
        inner: Arc::new(ReportedUsage(LlmStopReason::EndTurn)),
        meter: meter.clone(),
        max_steps: 3,
        max_tokens: 10,
        cancel: CancellationToken::new(),
    };
    let mut stream = provider.stream(ProviderRequest::default()).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Some(ProviderEvent::Error {
            retryable: false,
            ..
        })
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(meter.lock().tokens, 25);
}

#[tokio::test]
async fn rejected_responses_distinguish_budget_truncation_and_filtering() {
    for (reason, limit, expected) in [
        (LlmStopReason::EndTurn, 10, "token budget exceeded"),
        (LlmStopReason::MaxTokens, 100, "response truncated"),
        (LlmStopReason::ContentFilter, 100, "response filtered"),
    ] {
        let provider = BudgetProvider {
            inner: Arc::new(ReportedUsage(reason)),
            meter: Arc::new(Mutex::new(Meter::default())),
            max_steps: 3,
            max_tokens: limit,
            cancel: CancellationToken::new(),
        };
        let (sink, mut diagnostics) = tokio::sync::mpsc::unbounded_channel();
        let event = crate::diagnostics::observe(sink, async {
            provider
                .stream(ProviderRequest::default())
                .await
                .unwrap()
                .next()
                .await
        })
        .await
        .unwrap();
        let ProviderEvent::Error { error, retryable } = event else {
            panic!("response must be rejected")
        };
        assert!(!retryable);
        assert!(error.to_string().contains(expected));
        let diagnostic = diagnostics.try_recv().unwrap();
        assert_eq!(diagnostic.layer, "monad_budget");
        assert_eq!(diagnostic.data["used"], 25);
        assert_eq!(diagnostic.data["limit"], limit);
    }
}

#[tokio::test]
async fn child_tokens_are_scoped_in_diagnostics_instead_of_parent_output() {
    let root = tempfile::tempdir().unwrap();
    let mut child = child(
        Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
            text: "worker text".into(),
        }])),
        root.path(),
    );
    let handoff = lifecycle_handoff("research");
    let id = handoff.task_id.0.clone();
    child.start(handoff).await.unwrap();
    let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
    crate::diagnostics::observe(sink, child.step())
        .await
        .unwrap();
    let mut seen_text = false;
    while let Ok(event) = events.try_recv() {
        assert_eq!(event.layer, "monad");
        assert_eq!(event.event, "child_event");
        assert_eq!(event.data["task_id"], id);
        seen_text |= event.data.to_string().contains("worker text");
    }
    assert!(seen_text);
}
