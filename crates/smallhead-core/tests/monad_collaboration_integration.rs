//! Real parent/child loops with history-driven, cancellation-safe provider routing.

use std::{collections::HashSet, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};
use smallhead_core::{
    monad::{
        CollaborationEvent, CollaborationEventKind, CollaborationHub, CollaborationPhase,
        DelegateTool, MonadResult, MonadRuntime, MonadRuntimeConfig, ReportKind, RunState, TaskId,
    },
    AgentLoop, AgentMessage, CancellationToken, ContentBlock, ExternalTool, LlmMessage,
    LlmStopReason, Provider, ProviderError, ProviderEvent, ProviderRequest, ReadTool, ToolKind,
    Usage,
};

const PRIVATE: &str = "parent-private-context-not-delegated";
const CONTEXT: &str = "Read evidence.txt after alignment; fixture-public-context";
const QUESTION: &str = "Should the research cover the stable API only?";
const REPLY: &str = "Confirmed: cover the stable API only.";
const FINDING: &str = "evidence.txt:1 confirms the stable API is version 7.";
const FINAL: &str = "Collected the completed research: stable API version 7.";

fn text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn result(request: &ProviderRequest, id: &str) -> Option<(bool, String)> {
    request.messages.iter().find_map(|message| match message {
        LlmMessage::ToolResult {
            tool_call_id,
            content,
            is_error,
        } if tool_call_id == id => Some((*is_error, text(content))),
        _ => None,
    })
}

fn successful_json(request: &ProviderRequest, id: &str) -> Option<Value> {
    result(request, id).map(|(error, output)| {
        assert!(!error, "{id} failed: {output}");
        serde_json::from_str(&output).expect("delegate result must be JSON")
    })
}

fn user_text(request: &ProviderRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            LlmMessage::User { content } => Some(text(content)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn alignment_event(request: &ProviderRequest) -> Option<CollaborationEvent> {
    // Read the actual event injected by the parent loop, not an out-of-band hub copy.
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            LlmMessage::User { content } => Some(text(content)),
            _ => None,
        })
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<CollaborationEvent>(line).ok())
                .collect::<Vec<_>>()
        })
        .find(|event| event.kind == CollaborationEventKind::Report(ReportKind::AlignmentRequest))
}

enum Response {
    Tool(&'static str, &'static str, Value),
    Final(&'static str),
}

fn response_stream(response: Response) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send>> {
    let (block, stop_reason) = match response {
        Response::Tool(id, name, input) => (
            ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
            LlmStopReason::ToolUse,
        ),
        Response::Final(text) => (
            ContentBlock::Text { text: text.into() },
            LlmStopReason::EndTurn,
        ),
    };
    Box::pin(futures::stream::iter(vec![
        ProviderEvent::MessageStart {
            id: "fixture".into(),
            model: "routing-fixture".into(),
        },
        ProviderEvent::ContentBlockStart { index: 0, block },
        ProviderEvent::ContentBlockStop { index: 0 },
        ProviderEvent::MessageDelta {
            usage: Usage::default(),
            stop_reason,
        },
        ProviderEvent::MessageStop,
    ]))
}

struct RoutingProvider {
    runtime: Arc<MonadRuntime>,
    hub: Arc<CollaborationHub>,
    parent: TaskId,
    reject_wrong_reply: bool,
}

impl RoutingProvider {
    fn child(&self, request: &ProviderRequest) -> Response {
        let names: HashSet<_> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, HashSet::from(["read", "monad_report"]));
        let users = user_text(request);
        assert!(users.contains(CONTEXT));
        assert!(
            !users.contains(PRIVATE),
            "parent conversation leaked into child"
        );
        assert!(!request.system.contains(PRIVATE));
        if result(request, "alignment").is_none() {
            return Response::Tool(
                "alignment",
                "monad_report",
                json!({
                    "kind":"alignment_request", "message":QUESTION,
                }),
            );
        }
        let report = successful_json(request, "alignment").unwrap();
        assert!(report["accepted"]["request_id"].as_u64().is_some());
        assert!(
            users.contains(REPLY),
            "child crossed alignment checkpoint without exact reply"
        );
        if result(request, "nested").is_none() {
            // An invented call must also fail at dispatch, not merely disappear from schemas.
            return Response::Tool(
                "nested",
                "monad_delegate",
                json!({"action":"start","tasks":[{
                    "role":"research", "objective":"must not run", "context":"none",
                    "completion_criteria":"must not run",
                }]}),
            );
        }
        assert!(
            result(request, "nested").unwrap().0,
            "nested delegation was permitted"
        );
        if result(request, "read-evidence").is_none() {
            return Response::Tool("read-evidence", "read", json!({"path":"evidence.txt"}));
        }
        let (error, evidence) = result(request, "read-evidence").unwrap();
        assert!(!error, "read failed: {evidence}");
        assert!(evidence.contains("stable API is version 7"));
        Response::Final(FINDING)
    }

    async fn parent(&self, request: &ProviderRequest) -> Response {
        let Some(start) = successful_json(request, "start") else {
            return Response::Tool(
                "start",
                "monad_delegate",
                json!({"action":"start","tasks":[{
                    "role":"research", "objective":"Identify the stable API version",
                    "context":CONTEXT, "completion_criteria":"Read evidence.txt and cite its first line",
                }]}),
            );
        };
        assert_eq!(start["results"].as_array().unwrap().len(), 1);
        let receipt = &start["results"][0];
        let control = |action: &str| {
            json!({
                "action":action, "task_id":receipt["task_id"],
                "run_id":receipt["run_id"], "attempt_id":receipt["attempt_id"],
            })
        };
        let Some(event) = alignment_event(request) else {
            // The real loop must cancel this wait and reissue with harvested events.
            return std::future::pending().await;
        };
        assert_eq!(event.parent, self.parent);
        assert_eq!(json!(event.childtask), receipt["task_id"]);
        assert_eq!(json!(event.attempt), receipt["attempt_id"]);
        assert_eq!(event.text, QUESTION);
        let request_id = event.request_id.expect("alignment must be correlated");
        if self.reject_wrong_reply && result(request, "wrong-reply").is_none() {
            let mut input = control("reply");
            input["request_id"] = json!(request_id + 1);
            input["message"] = json!("Wrong correlation must not unblock the child");
            return Response::Tool("wrong-reply", "monad_delegate", input);
        }
        if result(request, "reply").is_none() {
            if self.reject_wrong_reply {
                assert!(result(request, "wrong-reply").unwrap().0);
                let children = self.hub.children(&self.parent);
                assert_eq!(children.len(), 1);
                assert_eq!(
                    children[0].pending_request.as_ref().unwrap().request_id,
                    request_id
                );
            }
            let mut input = control("reply");
            input["request_id"] = json!(request_id);
            input["message"] = json!(REPLY);
            return Response::Tool("reply", "monad_delegate", input);
        }
        let reply = successful_json(request, "reply").unwrap();
        assert_eq!(reply["acknowledgement"]["request_id"], json!(request_id));
        if successful_json(request, "collect").is_none() {
            // Synchronize on worker completion, never on wall-clock sleeps. Reissuing this
            // provider future after Reply/Ended events is harmless: no fixture state advances.
            self.runtime.wait_idle().await;
            return Response::Tool("collect", "monad_delegate", control("status"));
        }
        let collected = successful_json(request, "collect").unwrap();
        let completed = &collected["results"][0];
        for field in ["task_id", "run_id", "attempt_id"] {
            assert_eq!(completed[field], receipt[field]);
        }
        assert_eq!(completed["state"], "Completed");
        assert_eq!(completed["terminal"], true);
        assert!(completed["error"].is_null());
        let result: MonadResult = serde_json::from_value(completed["result"].clone()).unwrap();
        match result {
            MonadResult::Completed {
                output,
                evidence_refs,
                ..
            } => {
                assert_eq!(output["findings"][0]["text"], FINDING);
                assert!(!evidence_refs.is_empty());
            }
            other => panic!("unexpected child result: {other:?}"),
        }
        Response::Final(FINAL)
    }
}

#[async_trait]
impl Provider for RoutingProvider {
    fn id(&self) -> &str {
        "monad-routing-fixture"
    }
    fn model(&self) -> &str {
        "routing-fixture"
    }
    fn context_window(&self) -> usize {
        200_000
    }
    fn cache_key(&self, _: &ProviderRequest) -> String {
        "fixture".into()
    }
    fn abort_signal(&self) -> CancellationToken {
        CancellationToken::new()
    }

    async fn stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>, ProviderError> {
        let response = if request.tools.iter().any(|tool| tool.name == "monad_report") {
            self.child(&request)
        } else {
            self.parent(&request).await
        };
        Ok(response_stream(response))
    }
}

async fn exercise_collaboration(reject_wrong_reply: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("evidence.txt"),
        "stable API is version 7\n",
    )
    .unwrap();
    let runtime = Arc::new(MonadRuntime::new(MonadRuntimeConfig::default()).unwrap());
    let hub = Arc::new(CollaborationHub::default());
    let parent = TaskId::new();
    let provider = Arc::new(RoutingProvider {
        runtime: runtime.clone(),
        hub: hub.clone(),
        parent: parent.clone(),
        reject_wrong_reply,
    });
    let delegate = DelegateTool::new(provider.clone(), vec![ToolKind::Read(ReadTool)]);
    let mut agent = AgentLoop::new(
        provider,
        vec![ToolKind::External(ExternalTool::new(Arc::new(delegate)))],
        "Coordinate research",
    );
    agent.tool_ctx.owner = Some(parent.0.clone());
    agent.tool_ctx.monad_runtime = Some(runtime.clone());
    agent.tool_ctx.monad_collaboration = hub.clone();
    agent.tool_ctx.cwd = directory.path().to_path_buf();
    agent.tool_ctx.artifact_root = Some(directory.path().join("artifacts"));
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: PRIVATE.into(),
            }],
            timestamp: 0,
        }),
    )
    .await;
    // Always settle workers before inspecting a timeout/error or dropping artifact storage.
    runtime.cancel_all();
    tokio::time::timeout(Duration::from_secs(10), runtime.wait_idle())
        .await
        .expect("worker cleanup timed out");
    let summary = outcome
        .expect("cross-loop collaboration deadlocked")
        .expect("parent turn failed");
    assert!(summary.finalization_reason.is_none());
    assert_eq!(
        summary.tool_calls_executed,
        if reject_wrong_reply { 4 } else { 3 }
    );
    let runs = runtime.snapshot();
    assert_eq!(
        runs.len(),
        1,
        "nested delegation must not create another run"
    );
    assert_eq!(runs[0].state, RunState::Completed);
    let children = hub.children(&parent);
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].phase, CollaborationPhase::Ended);
    assert!(children[0].pending_request.is_none());
    let events = hub.events(&parent, 0, 128);
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event.kind == CollaborationEventKind::Report(ReportKind::AlignmentRequest)
            )
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == CollaborationEventKind::Reply)
            .count(),
        1
    );

    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    let mut final_text = String::new();
    for message in &agent.state.messages {
        match message {
            AgentMessage::Assistant {
                tool_calls,
                content,
                ..
            } => {
                assert!(
                    pending.is_empty(),
                    "assistant preceded outstanding tool results"
                );
                for call in tool_calls {
                    assert!(
                        seen.insert(call.id.clone()),
                        "a committed control was replayed"
                    );
                    pending.insert(call.id.clone());
                }
                if tool_calls.is_empty() {
                    final_text = text(content);
                }
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                assert!(
                    pending.remove(tool_call_id),
                    "unmatched or duplicate result"
                );
            }
            AgentMessage::User { .. } => {
                assert!(pending.is_empty(), "event split a tool protocol pair")
            }
            _ => {}
        }
    }
    assert!(pending.is_empty());
    assert_eq!(final_text, FINAL);
    let terminal = agent
        .state
        .messages
        .iter()
        .find_map(|message| {
            let AgentMessage::User { content, .. } = message else {
                return None;
            };
            let body = text(content);
            let (_, payload) = body.split_once('\n')?;
            let value: Value = serde_json::from_str(payload).ok()?;
            (value["kind"] == "ended").then_some(value)
        })
        .expect("parent must receive the child's terminal report");
    assert_eq!(terminal["runtime"]["run_id"], runs[0].run_id);
    assert_eq!(terminal["runtime"]["state"], "Completed");
    assert_eq!(
        terminal["runtime"]["result"]["output"]["findings"][0]["text"],
        FINDING
    );
}

#[tokio::test]
async fn parent_starts_aligns_and_collects_read_only_child() {
    exercise_collaboration(true).await;
}
