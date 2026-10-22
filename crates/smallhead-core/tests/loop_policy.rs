//! Driver integration for composable loop decisions and receipt invariants.
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use anyhow::Result;
use async_trait::async_trait;
use futures::{future::BoxFuture, Stream};
use serde_json::json;
use smallhead_core::{loop_policy::*, message::ToolCall, provider::ProviderError, *};

type Handler = dyn Fn(PolicyContext, PolicyEvent, PolicyNext) -> BoxFuture<'static, Result<PolicyAction>>
    + Send
    + Sync;
struct Layer {
    name: &'static str,
    handler: Box<Handler>,
}
#[async_trait]
impl LoopPolicy for Layer {
    fn name(&self) -> &str {
        self.name
    }
    async fn apply(
        &self,
        ctx: &PolicyContext,
        event: PolicyEvent,
        next: PolicyNext,
    ) -> Result<PolicyAction> {
        (self.handler)(ctx.clone(), event, next).await
    }
}
fn layer(
    name: &'static str,
    handler: impl Fn(PolicyContext, PolicyEvent, PolicyNext) -> BoxFuture<'static, Result<PolicyAction>>
        + Send
        + Sync
        + 'static,
) -> Arc<dyn LoopPolicy> {
    Arc::new(Layer {
        name,
        handler: Box::new(handler),
    })
}
fn user() -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: "actual user input".into(),
        }],
        timestamp: 1,
    }
}
fn context() -> PolicyContext {
    PolicyContext {
        turn_id: "test".into(),
        wave: 1,
        tool_calls: 0,
        can_continue: true,
        finalization_reason: None,
    }
}
fn finish_event() -> PolicyEvent {
    PolicyEvent::BeforeFinish { candidate: user() }
}
fn text(value: &str) -> MockResponse {
    MockResponse::AssistantText { text: value.into() }
}

struct RecordingProvider {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}
#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }
    fn model(&self) -> &str {
        "recording"
    }
    fn context_window(&self) -> usize {
        self.inner.context_window()
    }
    fn cache_key(&self, request: &ProviderRequest) -> String {
        self.inner.cache_key(request)
    }
    fn abort_signal(&self) -> CancellationToken {
        self.inner.abort_signal()
    }
    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> std::result::Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request).await
    }
}
fn agent(responses: Vec<MockResponse>) -> (AgentLoop, Arc<Mutex<Vec<ProviderRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider {
        inner: MockProvider::new(responses),
        requests: requests.clone(),
    });
    (
        AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "base"),
        requests,
    )
}

#[tokio::test]
async fn nested_layers_enter_outer_first_and_return_inner_first() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let layers = ["A", "B"]
        .into_iter()
        .map(|name| {
            let trace = trace.clone();
            layer(name, move |_, mut event, next| {
                let trace = trace.clone();
                Box::pin(async move {
                    trace.lock().unwrap().push(format!("{name}:enter"));
                    if let PolicyEvent::PrepareModel { request } = &mut event {
                        request.system.push_str(name);
                    }
                    let mut action = next.run(event).await?;
                    trace.lock().unwrap().push(format!("{name}:exit"));
                    if let PolicyAction::ModelRequest { request } = &mut action {
                        request.system.push_str(name);
                    }
                    Ok(action)
                })
            })
        })
        .collect();
    let PolicyAction::ModelRequest { request } = PolicyStack::new(layers)
        .decide(
            context(),
            PolicyEvent::PrepareModel {
                request: ProviderRequest::default(),
            },
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(request.system, "ABBA");
    assert_eq!(
        *trace.lock().unwrap(),
        ["A:enter", "B:enter", "B:exit", "A:exit"]
    );
    assert!(matches!(
        PolicyStack::default()
            .decide(context(), finish_event())
            .await
            .unwrap(),
        PolicyAction::Finish
    ));
}

#[tokio::test]
async fn wrong_next_phase_cannot_be_hidden_by_catching_the_error() {
    let policy = layer("wrong phase", |_, _, next| {
        Box::pin(async move {
            let _ = next
                .run(PolicyEvent::PrepareModel {
                    request: ProviderRequest::default(),
                })
                .await;
            Ok(PolicyAction::Finish)
        })
    });
    assert!(PolicyStack::new(vec![policy])
        .decide(context(), finish_event())
        .await
        .unwrap_err()
        .to_string()
        .contains("invalid inner decision"));
}

#[tokio::test]
async fn wrong_phase_action_fails_before_any_effect() {
    let (mut agent, requests) = agent(vec![text("unused")]);
    agent.set_policy_stack(PolicyStack::new(vec![layer("bad", |_, _, _| {
        Box::pin(async { Ok(PolicyAction::Finish) })
    })]));
    assert!(agent
        .run_one_turn(user())
        .await
        .unwrap_err()
        .to_string()
        .contains("different checkpoint"));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn finish_policy_really_runs_another_wave_and_preserves_source_history() {
    let (mut agent, requests) = agent(vec![text("candidate"), text("verified")]);
    agent.set_policy_stack(PolicyStack::new(vec![layer(
        "verify",
        |ctx, mut event, next| {
            Box::pin(async move {
                if let PolicyEvent::PrepareModel { request } = &mut event {
                    request.system.push_str(&format!("/wave={}", ctx.wave));
                    request.tools[0].description.push_str("/lens description");
                }
                let finish = matches!(event, PolicyEvent::BeforeFinish { .. });
                let action = next.run(event).await?;
                if finish && ctx.wave == 1 {
                    Ok(PolicyAction::Continue {
                        guidance: "verify the evidence".into(),
                    })
                } else {
                    Ok(action)
                }
            })
        },
    )]));
    let summary = agent.run_one_turn(user()).await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(summary.turns_completed, 2);
    assert_eq!(requests[1].system, "base/wave=2");
    assert!(requests[0].tools[0]
        .description
        .ends_with("/lens description"));
    assert_eq!(agent.system, "base");
    assert!(serde_json::to_string(&requests[1].messages)
        .unwrap()
        .contains("not new user authorization"));
    assert!(serde_json::to_string(&agent.state.messages)
        .unwrap()
        .contains("actual user input"));
}

#[tokio::test]
async fn endless_continuation_is_stopped_by_model_budget() {
    let (mut agent, requests) = agent(vec![text("one"), text("two")]);
    agent.budget.max_model_waves = 2;
    agent.set_policy_stack(PolicyStack::new(vec![layer(
        "endless",
        |_, event, next| {
            Box::pin(async move {
                if matches!(event, PolicyEvent::BeforeFinish { .. }) {
                    Ok(PolicyAction::Continue {
                        guidance: "more".into(),
                    })
                } else {
                    next.run(event).await
                }
            })
        },
    )]));
    assert!(agent
        .run_one_turn(user())
        .await
        .unwrap_err()
        .to_string()
        .contains("beyond the driver budget"));
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn tool_adaptation_executes_once_and_preserves_untransformed_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (mut agent, _) = agent(vec![
        MockResponse::ToolCall {
            name: "echo".into(),
            input: json!({"text":"proposed"}),
        },
        text("done"),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    agent.tool_ctx.artifact_root = Some(root.path().to_owned());
    let counter = calls.clone();
    agent.set_policy_stack(PolicyStack::new(vec![layer(
        "adapt",
        move |_, mut event, next| {
            let counter = counter.clone();
            Box::pin(async move {
                if let PolicyEvent::BeforeTool { call } = &mut event {
                    call.input = json!({"text":"adapted"});
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                let mut action = next.run(event).await?;
                if let PolicyAction::ObserveTool { result } = &mut action {
                    result.content = vec![ContentBlock::Text {
                        text: "model observation".repeat(2000),
                    }];
                }
                Ok(action)
            })
        },
    )]));
    agent.run_one_turn(user()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(serde_json::to_string(&agent.state.executed_tools)
        .unwrap()
        .contains("adapted"));
    assert!(!serde_json::to_string(&agent.state.executed_tools)
        .unwrap()
        .contains("model observation"));
    assert!(serde_json::to_string(&agent.state.messages)
        .unwrap()
        .contains("model observation"));
    let observation = agent
        .state
        .messages
        .iter()
        .find(|message| matches!(message, AgentMessage::ToolResult { .. }))
        .unwrap();
    let observation = serde_json::to_string(observation).unwrap();
    assert!(observation.contains("tool_result_projection"));
    assert!(observation.len() < 20 * 1024);
    let persisted = serde_json::to_string(&agent.state.messages).unwrap();
    let restored: Vec<AgentMessage> = serde_json::from_str(&persisted).unwrap();
    assert!(restored.iter().any(
        |message| matches!(message, AgentMessage::Notification {kind, payload}
        if kind == "lens_tool_receipt" && payload["call"]["input"]["text"] == "adapted"
        && payload["result"]["content"][0]["text"].as_str().unwrap().contains("adapted"))
    ));
}

#[tokio::test]
async fn rejecting_a_tool_keeps_protocol_pairs_without_executing() {
    let (mut agent, _) = agent(vec![
        MockResponse::ToolCall {
            name: "echo".into(),
            input: json!({"text":"must not execute"}),
        },
        text("done"),
    ]);
    agent.set_policy_stack(PolicyStack::new(vec![layer("reject", |_, event, next| {
        Box::pin(async move {
            if matches!(event, PolicyEvent::BeforeTool { .. }) {
                Ok(PolicyAction::RejectTool {
                    reason: "not needed".into(),
                })
            } else {
                next.run(event).await
            }
        })
    })]));
    agent.run_one_turn(user()).await.unwrap();
    assert!(agent.state.executed_tools[0].is_error);
    assert!(serde_json::to_string(&agent.state.executed_tools[0])
        .unwrap()
        .contains("rejected before execution"));
    assert_eq!(
        repair_interrupted_history(&mut agent.state.messages, "check"),
        0
    );
}

#[tokio::test]
async fn hidden_tools_are_not_executed_even_if_provider_calls_them() {
    let (mut agent, _) = agent(vec![
        MockResponse::ToolCall {
            name: "echo".into(),
            input: json!({"text":"must not execute"}),
        },
        text("done"),
    ]);
    agent.set_policy_stack(PolicyStack::new(vec![layer(
        "hide",
        |_, mut event, next| {
            Box::pin(async move {
                if let PolicyEvent::PrepareModel { request } = &mut event {
                    request.tools.clear();
                }
                next.run(event).await
            })
        },
    )]));
    agent.run_one_turn(user()).await.unwrap();
    assert!(agent.state.executed_tools[0].is_error);
    assert!(serde_json::to_string(&agent.state.executed_tools[0])
        .unwrap()
        .contains("not offered"));
}

#[tokio::test]
async fn failed_observation_policy_retains_real_receipt_and_status() {
    let (mut agent, _) = agent(vec![MockResponse::ToolCall {
        name: "echo".into(),
        input: json!({"text":"real effect"}),
    }]);
    agent.set_policy_stack(PolicyStack::new(vec![layer("forge", |_, event, next| {
        Box::pin(async move {
            let mut action = next.run(event).await?;
            if let PolicyAction::ObserveTool { result } = &mut action {
                result.is_error = true;
            }
            Ok(action)
        })
    })]));
    assert!(agent
        .run_one_turn(user())
        .await
        .unwrap_err()
        .to_string()
        .contains("receipt identity or success"));
    assert_eq!(agent.state.executed_tools.len(), 1);
    assert!(!agent.state.executed_tools[0].is_error);
    assert_eq!(
        repair_interrupted_history(&mut agent.state.messages, "check"),
        0
    );
}

#[tokio::test]
async fn invented_schema_and_rewritten_receipts_are_rejected() {
    for rewrite_history in [false, true] {
        let (mut agent, requests) = agent(vec![text("unused")]);
        if rewrite_history {
            agent.state.messages = vec![
                AgentMessage::Assistant {
                    content: vec![],
                    tool_calls: vec![ToolCall {
                        id: "a".into(),
                        name: "echo".into(),
                        input: json!({}),
                    }],
                    model: "mock".into(),
                    usage: None,
                },
                AgentMessage::ToolResult {
                    tool_call_id: "a".into(),
                    content: vec![],
                    is_error: true,
                },
            ];
        }
        agent.set_policy_stack(PolicyStack::new(vec![layer(
            "invalid request",
            move |_, mut event, next| {
                Box::pin(async move {
                    if let PolicyEvent::PrepareModel { request } = &mut event {
                        if rewrite_history {
                            request.messages.clear();
                        } else {
                            request.tools[0].input_schema = json!({});
                        }
                    }
                    next.run(event).await
                })
            },
        )]));
        assert!(agent.run_one_turn(user()).await.is_err());
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_blocked_policy() {
    let (mut agent, requests) = agent(vec![text("unused")]);
    let cancel = agent.tool_ctx.cancellation.clone();
    agent.set_policy_stack(PolicyStack::new(vec![layer("blocked", move |_, _, _| {
        let cancel = cancel.clone();
        Box::pin(async move {
            cancel.cancel();
            std::future::pending().await
        })
    })]));
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        agent.run_one_turn(user()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("cancelled during Lens"));
    assert!(agent.state.cancelled);
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn blocked_policy_times_out_before_calling_provider() {
    let (mut agent, requests) = agent(vec![text("unused")]);
    agent.set_policy_stack(PolicyStack::new(vec![layer("blocked", |_, _, _| {
        Box::pin(std::future::pending())
    })]));
    assert!(agent
        .run_one_turn(user())
        .await
        .unwrap_err()
        .to_string()
        .contains("exceeded 10s"));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_identity_cannot_be_changed_by_a_policy() {
    let (mut agent, _) = agent(vec![MockResponse::ToolCall {
        name: "echo".into(),
        input: json!({"text":"unused"}),
    }]);
    agent.set_policy_stack(PolicyStack::new(vec![layer(
        "rename",
        |_, mut event, next| {
            Box::pin(async move {
                if let PolicyEvent::BeforeTool { call } = &mut event {
                    call.id = "forged".into();
                }
                next.run(event).await
            })
        },
    )]));
    assert!(agent
        .run_one_turn(user())
        .await
        .unwrap_err()
        .to_string()
        .contains("tool call identity"));
    assert!(agent.state.executed_tools.is_empty());
    assert_eq!(
        repair_interrupted_history(&mut agent.state.messages, "check"),
        0
    );
}

#[tokio::test]
async fn outer_policy_cannot_swallow_inner_failure() {
    let outer = layer("outer", |_, event, next| {
        Box::pin(async move {
            let _ = next.run(event).await;
            Ok(PolicyAction::Finish)
        })
    });
    let inner = layer("inner", |_, _, _| {
        Box::pin(async { anyhow::bail!("broken inner layer") })
    });
    assert!(PolicyStack::new(vec![outer, inner])
        .decide(context(), finish_event())
        .await
        .is_err());
}
