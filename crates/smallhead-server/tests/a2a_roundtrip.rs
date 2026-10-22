//! End-to-end A2A round trip over real HTTP: bind an OS-assigned port, serve the
//! real router, and drive it with the real client.
//!
//! Providers are explicitly injected: ambient credentials cannot turn tests
//! into paid model calls. Real file/process tools and Node workers still run.

use smallhead_core::{AgentMessage, MockProvider, MockResponse};
use smallhead_server::a2a::{A2aClient, A2aMessage, A2aPermissions, A2aServer, A2aServerConfig};
use smallhead_server::driver::HeadlessRuntime;
use std::sync::Arc;

fn message(text: &str, to: &str) -> A2aMessage {
    A2aMessage {
        message_id: None,
        conversation_id: None,
        from: "dsh".into(),
        to: Some(to.into()),
        kind: "request".into(),
        intent: Some("research".into()),
        text: text.into(),
        reply_to: None,
        permissions: A2aPermissions::default(),
        refs: Vec::new(),
        ttl: 3,
    }
}

fn server(sessions_dir: Option<std::path::PathBuf>) -> (A2aServer, A2aServerConfig) {
    let config = A2aServerConfig {
        name: "smallhead".into(),
        port: 0,
        sessions_dir,
    };
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::AssistantText {
            text: "first".into(),
        },
        MockResponse::AssistantText {
            text: "second".into(),
        },
    ]));
    let runtime = HeadlessRuntime::new(
        provider,
        Default::default(),
        std::env::current_dir().unwrap(),
        None,
    )
    .unwrap();
    (
        A2aServer::new(config.clone()).with_runtime(Arc::new(runtime)),
        config,
    )
}

fn development_runtime(
    root: &std::path::Path,
    responses: Vec<MockResponse>,
    lenses: smallhead_lens::LensRegistry,
) -> Arc<HeadlessRuntime> {
    let processes = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
        root.join("runs"),
    )
    .unwrap();
    let execution = smallhead_core::execution::ExecutionRuntime::new(processes).unwrap();
    Arc::new(
        HeadlessRuntime::new(
            Arc::new(MockProvider::new(responses)),
            lenses,
            root.into(),
            Some(Arc::new(execution)),
        )
        .unwrap(),
    )
}

fn development_message(text: &str) -> A2aMessage {
    let mut request = message(text, "smallhead");
    request.permissions.can_mutate = true;
    request.permissions.can_execute = true;
    request
}

fn extension_fixture(root: &std::path::Path, collision: bool) -> smallhead_lens::LensRegistry {
    std::fs::write(
        root.join("lens.json"),
        serde_json::json!({
            "id":"peer-extension","version":"1","api_version":1,"entry":"loop.mjs",
            "extension":{"command":"node","args":["worker.mjs"],"config_file":"binding.json",
                "tools_file":"tools.json","system_file":"system.md","timeout_ms":5000}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("loop.mjs"),
        "export async function apply(c,e,next){return next(e);}",
    )
    .unwrap();
    std::fs::write(
        root.join("binding.json"),
        serde_json::json!({"marker":root.join("calls.jsonl"),"pinned":"host-only"}).to_string(),
    )
    .unwrap();
    std::fs::write(root.join("system.md"), "Use only host-bound fixture tools.").unwrap();
    let names = if collision {
        ["read", "fixture_mutate", "fixture_execute"]
    } else {
        ["fixture_observe", "fixture_mutate", "fixture_execute"]
    };
    let tools:Vec<_> = names.iter().zip(["observe","mutate","execute"]).map(|(name,access)| serde_json::json!({
        "name":name,"label":"Fixture","description":"Local fixture","input_schema":{"type":"object"},"access":access
    })).collect();
    std::fs::write(
        root.join("tools.json"),
        serde_json::to_string(&tools).unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("worker.mjs"), r#"
import readline from 'node:readline';
import fs from 'node:fs';
let config, bound;
for await(const line of readline.createInterface({input:process.stdin})) {
  const r=JSON.parse(line);let result;
  if(r.method==='initialize'){config=r.params.config;result={api_version:1};}
  else if(r.method==='bind_input'){bound=r.params.input;result={};}
  else if(r.method==='call'){
    const data={name:r.params.name,bound,pinned:config.pinned};
    fs.appendFileSync(config.marker,JSON.stringify(data)+'\n');
    result={tool_call_id:r.params.tool_call_id,is_error:false,content:[{type:'text',text:JSON.stringify(data)}]};
  } else throw new Error('unexpected method');
  process.stdout.write(JSON.stringify({id:r.id,result})+'\n');
}
"#).unwrap();
    let lenses = smallhead_lens::LensRegistry::default();
    lenses.load(root).unwrap();
    lenses
}

#[tokio::test]
async fn development_peer_discovers_loads_and_calls_catalogued_lens_in_one_request() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("lenses/peer-extension");
    std::fs::create_dir_all(&package).unwrap();
    let lenses = extension_fixture(&package, false);
    lenses.unload("peer-extension").unwrap();
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "lens".into(),
                input: serde_json::json!({"action":"discover","query":"peer-extension"}),
            },
            MockResponse::ToolCall {
                name: "lens".into(),
                input: serde_json::json!({"action":"load","id":"peer-extension"}),
            },
            MockResponse::ToolCall {
                name: "fixture_observe".into(),
                input: serde_json::json!({}),
            },
            MockResponse::AssistantText {
                text: "loaded and observed".into(),
            },
        ],
        lenses,
    );
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime),
    )
    .await;
    let reply = A2aClient::new(base)
        .send(&development_message("continue with the appropriate Lens"))
        .await
        .unwrap();
    assert_eq!(reply.status, "done", "{:?}", reply.errors);
    assert_eq!(reply.text, "loaded and observed");
    let calls = std::fs::read_to_string(package.join("calls.jsonl")).unwrap();
    let call: serde_json::Value = serde_json::from_str(calls.trim()).unwrap();
    assert_eq!(call["bound"]["source"], "peer");
    let saved = saved_session(&sessions).await;
    assert_eq!(
        saved
            .messages
            .iter()
            .filter(|m| matches!(
                m,
                AgentMessage::ToolResult {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        3
    );
}

#[tokio::test]
async fn extension_read_only_peer_filters_mutation_execution_and_binds_peer_input() {
    let root = tempfile::tempdir().unwrap();
    let lenses = extension_fixture(root.path(), false);
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "fixture_mutate".into(),
                input: serde_json::json!({}),
            },
            MockResponse::ToolCall {
                name: "fixture_execute".into(),
                input: serde_json::json!({}),
            },
            MockResponse::ToolCall {
                name: "fixture_observe".into(),
                input: serde_json::json!({"config_file":"peer-cannot-select.json"}),
            },
            MockResponse::AssistantText {
                text: "observed".into(),
            },
        ],
        lenses,
    );
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime),
    )
    .await;
    let reply = A2aClient::new(base)
        .send(&message(
            "peer text must not become user learning",
            "smallhead",
        ))
        .await
        .unwrap();
    assert_eq!(reply.status, "done", "{:?}", reply.errors);
    let calls = std::fs::read_to_string(root.path().join("calls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(), 1);
    let call: serde_json::Value = serde_json::from_str(calls.trim()).unwrap();
    assert_eq!(call["name"], "fixture_observe");
    assert_eq!(call["pinned"], "host-only");
    assert_eq!(call["bound"]["source"], "peer");
    assert_eq!(
        call["bound"]["text"],
        "peer text must not become user learning"
    );
    let session = saved_session(&sessions).await;
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::ToolResult { is_error: true, .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn extension_development_peer_keeps_peer_source_with_write_permission() {
    let root = tempfile::tempdir().unwrap();
    let lenses = extension_fixture(root.path(), false);
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "fixture_mutate".into(),
                input: serde_json::json!({}),
            },
            MockResponse::ToolCall {
                name: "fixture_execute".into(),
                input: serde_json::json!({}),
            },
            MockResponse::AssistantText {
                text: "done".into(),
            },
        ],
        lenses,
    );
    let outcome = runtime
        .run_turn(
            vec![],
            "authorized peer task".into(),
            development_message("").permissions,
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, "done", "{}", outcome.reply);
    let calls = std::fs::read_to_string(root.path().join("calls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(), 2);
    for line in calls.lines() {
        let call: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(call["bound"]["source"], "peer");
    }
}

#[tokio::test]
async fn extension_attach_failure_is_persisted_as_failed_turn_with_original_input() {
    let root = tempfile::tempdir().unwrap();
    let lenses = extension_fixture(root.path(), true);
    let provider = Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
        text: "must not run".into(),
    }]));
    let runtime =
        Arc::new(HeadlessRuntime::new(provider.clone(), lenses, root.path().into(), None).unwrap());
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime),
    )
    .await;
    let reply = A2aClient::new(base)
        .send(&message("retain this accepted peer input", "smallhead"))
        .await
        .unwrap();
    assert_eq!(reply.status, "failed");
    assert!(reply
        .errors
        .iter()
        .any(|e| e.contains("Lens attach failed")));
    assert_eq!(provider.responses.lock().unwrap().len(), 1);
    assert!(!root.path().join("calls.jsonl").exists());
    let session = saved_session(&sessions).await;
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User { .. }))
            .count(),
        1
    );
    assert!(session
        .messages
        .iter()
        .any(|m| matches!(m,AgentMessage::User{content,..}
        if serde_json::to_string(content).unwrap().contains("retain this accepted peer input"))));
}

async fn saved_session(dir: &std::path::Path) -> smallhead_session::Session {
    use smallhead_session::{JsonFileStore, SessionStore};
    let store = JsonFileStore::new(dir);
    let metas = store.list().await.unwrap();
    assert_eq!(metas.len(), 1);
    store.load(&metas[0].id).await.unwrap()
}

#[tokio::test]
async fn development_roundtrip_runs_real_file_and_shell_tools_through_lenses() {
    let root = tempfile::tempdir().unwrap();
    let lenses = smallhead_lens::LensRegistry::default();
    let packages = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lenses");
    lenses.load(packages.join("learning")).unwrap();
    lenses.load(packages.join("evidence-check")).unwrap();
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "write".into(),
                input: serde_json::json!({"path":"note.txt","content":"draft"}),
            },
            MockResponse::ToolCall {
                name: "read".into(),
                input: serde_json::json!({"path":"note.txt"}),
            },
            MockResponse::ToolCall {
                name: "edit".into(),
                input: serde_json::json!({"path":"note.txt","search_text":"draft","replace_text":"verified"}),
            },
            MockResponse::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"command":"test \"$(cat note.txt)\" = verified && echo checked","timeout_ms":5000}),
            },
            MockResponse::AssistantText {
                text: "candidate".into(),
            },
            MockResponse::AssistantText {
                text: "complete deliverable".into(),
            },
        ],
        lenses,
    );
    let sessions = root.path().join("sessions");
    let journal = root.path().join("diagnostics");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime)
        .with_diagnostics_dir(Some(journal.clone())),
    )
    .await;
    let client = A2aClient::new(base);
    let card = client.card().await.unwrap();
    assert_eq!(card["limits"]["execute"], true);
    assert_eq!(card["limits"]["mutate"], true);
    let reply = client
        .send(&development_message("Write, edit, and verify note.txt"))
        .await
        .unwrap();
    assert_eq!(reply.status, "done", "{:?}", reply.errors);
    assert_eq!(reply.text, "complete deliverable");
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
        "verified"
    );
    let session = saved_session(&sessions).await;
    let receipts = session
        .messages
        .iter()
        .filter(|m| {
            matches!(m,
        AgentMessage::Notification {kind,..} if kind=="lens_tool_receipt")
        })
        .count();
    assert_eq!(receipts, 4);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| matches!(
                m,
                AgentMessage::ToolResult {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        4
    );
    let trace = std::fs::read_to_string(journal.join(format!("{}.jsonl", reply.task_id))).unwrap();
    let events: Vec<serde_json::Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["layer"] == "lens" && e["data"]["action"] == "continue")
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|e| e["layer"] == "lens" && e["data"]["action"] == "finish"));
    assert!(events
        .iter()
        .any(|e| e["data"]["layers"] == serde_json::json!(["learning", "evidence-check"])));
    assert_eq!(events.last().unwrap()["omitted_events"], 0);
}

#[tokio::test]
async fn read_only_request_cannot_use_development_hosts_write_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "write".into(),
                input: serde_json::json!({"path":"forbidden","content":"bad"}),
            },
            MockResponse::AssistantText {
                text: "write unavailable".into(),
            },
        ],
        Default::default(),
    );
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime),
    )
    .await;
    let reply = A2aClient::new(base)
        .send(&message("Try a write", "smallhead"))
        .await
        .unwrap();
    assert_eq!(reply.status, "done");
    assert!(!root.path().join("forbidden").exists());
    let session = saved_session(&sessions).await;
    assert!(session
        .messages
        .iter()
        .any(|m| matches!(m, AgentMessage::ToolResult { is_error: true, .. })));
}

#[tokio::test]
async fn failed_finish_preserves_candidate_and_actual_failure_for_polling_and_history() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("lens.json"),
        r#"{"id":"fail-finish","version":"1","api_version":1,"entry":"loop.mjs"}"#,
    )
    .unwrap();
    std::fs::write(root.path().join("loop.mjs"),"export async function apply(c,e,next){if(e.type==='before_finish')throw new Error('finish rejected');return next(e);}").unwrap();
    let lenses = smallhead_lens::LensRegistry::default();
    lenses.load(root.path()).unwrap();
    let runtime = HeadlessRuntime::new(
        Arc::new(MockProvider::new(vec![MockResponse::AssistantText {
            text: "unaccepted candidate".into(),
        }])),
        lenses,
        root.path().into(),
        None,
    )
    .unwrap();
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            name: "custom-agent".into(),
            sessions_dir: Some(sessions.clone()),
        })
        .with_runtime(Arc::new(runtime)),
    )
    .await;
    let client = A2aClient::new(base);
    let reply = client
        .send(&message("Fail at finish", "custom-agent"))
        .await
        .unwrap();
    assert_eq!(reply.status, "failed");
    assert_eq!(reply.from, "custom-agent");
    assert!(reply.errors.iter().any(|e| e.contains("finish rejected")));
    assert_eq!(
        client.task(&reply.task_id).await.unwrap().errors,
        reply.errors
    );
    let session = saved_session(&sessions).await;
    assert!(session.messages.iter().any(|m|matches!(m,
        AgentMessage::Notification {kind,payload} if kind=="a2a_turn_outcome" && payload["status"]=="failed")));
    assert!(
        matches!(session.messages.last(),Some(AgentMessage::Assistant{content,..})
        if serde_json::to_string(content).unwrap().contains("finish rejected"))
    );
    assert!(session
        .messages
        .iter()
        .any(|m| matches!(m,AgentMessage::Assistant{content,..}
        if serde_json::to_string(content).unwrap().contains("unaccepted candidate"))));
}

#[tokio::test]
async fn overlapping_requests_in_one_conversation_keep_both_turns() {
    let root = tempfile::tempdir().unwrap();
    let runtime = development_runtime(
        root.path(),
        vec![
            MockResponse::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"command":"touch started; sleep 0.3","timeout_ms":5000}),
            },
            MockResponse::AssistantText {
                text: "first settled".into(),
            },
            MockResponse::AssistantText {
                text: "second settled".into(),
            },
        ],
        Default::default(),
    );
    let sessions = root.path().join("sessions");
    let base = spawn_server(
        A2aServer::new(A2aServerConfig {
            port: 0,
            sessions_dir: Some(sessions.clone()),
            ..Default::default()
        })
        .with_runtime(runtime),
    )
    .await;
    let client = A2aClient::new(base);
    let mut first = development_message("first request");
    first.conversation_id = Some("overlap".into());
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.send(&first).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !root.path().join("started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut second = development_message("second request");
    second.conversation_id = Some("overlap".into());
    let second = client.send(&second).await.unwrap();
    assert_eq!(first.await.unwrap().text, "first settled");
    assert_eq!(second.text, "second settled");
    let session = saved_session(&sessions).await;
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User { .. }))
            .count(),
        2
    );
    assert_eq!(session.messages.len(), 6);
}

#[tokio::test]
async fn services_survive_request_boundaries_and_are_reaped_on_host_shutdown() {
    use smallhead_core::execution::{
        smallhead_process::{ProcessManager, ProcessSpec},
        ExecutionRuntime,
    };
    let root = tempfile::tempdir().unwrap();
    let processes = ProcessManager::with_log_root(root.path().join("runs")).unwrap();
    let execution = Arc::new(ExecutionRuntime::new(processes.clone()).unwrap());
    let spec = smallhead_core::execution::smallhead_process::service::ServiceSpec {
        name: "smoke-service".into(),
        process: ProcessSpec::new("sleep 60"),
        health_url: String::new(),
        identity: Default::default(),
        startup_timeout_ms: 5000,
    };
    let runtime = Arc::new(
        HeadlessRuntime::new(
            Arc::new(MockProvider::new(vec![
                MockResponse::ToolCall {
                    name: "process_control".into(),
                    input: serde_json::json!({"action":"service_ensure","service":spec}),
                },
                MockResponse::AssistantText {
                    text: "service started".into(),
                },
                MockResponse::ToolCall {
                    name: "process_control".into(),
                    input: serde_json::json!({"action":"service_inspect","name":"smoke-service"}),
                },
                MockResponse::AssistantText {
                    text: "service still available".into(),
                },
            ])),
            Default::default(),
            root.path().into(),
            Some(execution.clone()),
        )
        .unwrap(),
    );
    let server = A2aServer::new(A2aServerConfig {
        port: 0,
        ..Default::default()
    })
    .with_runtime(runtime);
    let (listener, address) = server.bind().await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_until(listener, async {
        let _ = stopped.await;
    }));
    let client = A2aClient::new(format!("http://{address}"));
    let first = client
        .send(&development_message("Start service"))
        .await
        .unwrap();
    assert_eq!(first.status, "done", "{:?}", first.errors);
    let services = execution.services.list();
    assert_eq!(services.len(), 1);
    let id = services[0].execution_id.unwrap();
    assert!(!processes.inspect(id).unwrap().is_terminal());
    let second = client
        .send(&development_message("Inspect service"))
        .await
        .unwrap();
    assert_eq!(second.status, "done");
    assert!(!processes.inspect(id).unwrap().is_terminal());
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(processes.inspect(id).unwrap().is_terminal());
}

async fn spawn_server(server: A2aServer) -> String {
    let (listener, address) = server.bind().await.expect("bind");
    tokio::spawn(server.serve(listener));
    format!("http://{address}")
}

#[tokio::test]
async fn agent_card_health_and_message_round_trip() {
    let (server, _) = server(None);
    let base = spawn_server(server).await;
    let client = A2aClient::new(base);

    // The card advertises this agent and the read-only limits.
    let card = client.card().await.expect("agent card");
    assert_eq!(card["name"], "smallhead");
    assert_eq!(card["protocol"], "smallhead-a2a-v1");
    assert_eq!(card["limits"]["mutate"], false);

    // A bounded request settles with a reply from the headless driver.
    let reply = client
        .send(&message("闭环测试：请回一句", "smallhead"))
        .await
        .expect("send");
    assert_eq!(reply.to, "dsh");
    assert_eq!(reply.from, "smallhead");
    assert_eq!(reply.status, "done");
    assert!(!reply.text.is_empty(), "reply text must not be empty");
    assert!(reply.errors.is_empty());

    // The same task is pollable afterwards, by id.
    let polled = client.task(&reply.task_id).await.expect("poll");
    assert_eq!(polled.task_id, reply.task_id);
    assert_eq!(polled.status, "done");
}

#[tokio::test]
async fn rejects_mutation_permission_and_unknown_target() {
    let (server, _) = server(None);
    let base = spawn_server(server).await;

    // A message asking to mutate is refused at the entrance (403 → client error).
    let mut mutating = message("请删掉这个文件", "smallhead");
    mutating.permissions.can_mutate = true;
    let refused = A2aClient::new(base.clone()).send(&mutating).await;
    assert!(refused.is_err(), "mutating permission must be rejected");

    // A target that is not this agent is refused too (404 → client error).
    let wrong_target = A2aClient::new(base)
        .send(&message("喂", "other-agent"))
        .await;
    assert!(wrong_target.is_err(), "unknown target must be rejected");
}

#[tokio::test]
async fn persists_one_session_per_conversation_with_accumulated_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (server, _) = server(Some(dir.path().to_path_buf()));
    let base = spawn_server(server).await;
    let client = A2aClient::new(base);

    // Two turns in one conversation accumulate into one session.
    let mut first = message("第一句", "smallhead");
    first.conversation_id = Some("conv-persist-test".into());
    let mut second = message("第二句", "smallhead");
    second.conversation_id = Some("conv-persist-test".into());

    client.send(&first).await.expect("first");
    client.send(&second).await.expect("second");

    use smallhead_session::{JsonFileStore, SessionStore};
    let store = JsonFileStore::new(dir.path().to_path_buf());
    let metas = store.list().await.expect("list");
    assert_eq!(metas.len(), 1, "one session per conversation");
    let session = store.load(&metas[0].id).await.expect("load");
    assert_eq!(
        session.messages.len(),
        4,
        "two user + two assistant messages"
    );
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| matches!(m, smallhead_core::AgentMessage::User { .. }))
            .count(),
        2,
        "both user turns are preserved"
    );
}
