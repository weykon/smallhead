//! Real Node extension protocol tests through the host AgentLoop.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use smallhead_core::tool::InputSource;
use smallhead_core::{
    AgentLoop, AgentMessage, ContentBlock, EchoTool, MockProvider, MockResponse, ToolKind,
};
use smallhead_lens::{ExtensionAccess, LensCatalog, LensRegistry};

const WORKER: &str = r#"
import fs from 'node:fs';
import readline from 'node:readline';
const log = value => fs.appendFileSync('events.jsonl', JSON.stringify(value) + '\n');
log({started: process.pid});
let config, bound;
const lines = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
for await (const line of lines) {
  const request = JSON.parse(line);
  log(request);
  let result;
  if (request.method === 'initialize') {
    config = request.params.config;
    result = {api_version: config.mode === 'bad_version' ? 99 : 1};
  } else if (request.method === 'bind_input') {
    bound = request.params.input;
    result = {ok: true};
  } else if (request.method === 'call') {
    if (config.mode === 'hang') await new Promise(() => { setInterval(() => {}, 1000); });
    result = {
      tool_call_id: config.mode === 'bad_receipt' ? 'forged-receipt' : request.params.tool_call_id,
      is_error: false,
      content: [{type:'text', text:JSON.stringify({bound, config, input:request.params.input})}]
    };
  } else { throw new Error('unexpected method'); }
  const id = request.method === 'call' && config.mode === 'bad_id' ? request.id + 99 : request.id;
  process.stdout.write(JSON.stringify({id, result}) + '\n');
}
"#;

fn tools(names: &[(&str, &str)]) -> Value {
    Value::Array(
        names
            .iter()
            .map(|(name, access)| {
                json!({
                    "name":name, "label":name, "description":"test extension",
                    "input_schema":{"type":"object"}, "access":access
                })
            })
            .collect(),
    )
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn package(root: &Path, id: &str, names: &[(&str, &str)], mode: &str) -> PathBuf {
    let path = root.join(id);
    std::fs::create_dir_all(&path).unwrap();
    write_json(
        path.join("lens.json"),
        &json!({
            "id":id, "version":"1", "api_version":1, "entry":"loop.mjs",
            "config":{"marker":path.join("policy.json")},
            "extension":{"command":"node", "args":["worker.mjs"],
                "config_file":"config.json", "tools_file":"tools.json", "system_file":"system.md",
                "timeout_ms":2000}
        }),
    );
    write_json(
        path.join("config.json"),
        &json!({"mode":mode, "generation":"old"}),
    );
    write_json(path.join("tools.json"), &tools(names));
    std::fs::write(path.join("system.md"), "PINNED_OLD_DOMAIN").unwrap();
    std::fs::write(
        path.join("loop.mjs"),
        "export async function apply(c,e,next) { return next(e); }",
    )
    .unwrap();
    std::fs::write(path.join("worker.mjs"), WORKER).unwrap();
    path
}

fn call(name: &str, sequence: u32) -> MockResponse {
    MockResponse::ToolCall {
        name: name.into(),
        input: json!({"sequence":sequence}),
    }
}

fn done() -> MockResponse {
    MockResponse::AssistantText {
        text: "done".into(),
    }
}

fn agent(responses: Vec<MockResponse>) -> AgentLoop {
    AgentLoop::new(
        Arc::new(MockProvider::new(responses)),
        vec![],
        "HOST_SYSTEM",
    )
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp: 1,
    }
}

async fn run(agent: &mut AgentLoop, text: &str) {
    tokio::time::timeout(Duration::from_secs(10), agent.run_one_turn(user(text)))
        .await
        .expect("agent turn deadline")
        .unwrap();
}

fn manage(input: Value) -> MockResponse {
    MockResponse::ToolCall {
        name: "lens".into(),
        input,
    }
}

#[tokio::test]
async fn managed_discover_load_use_and_unload_in_one_task() {
    let root = tempfile::tempdir().unwrap();
    let path = package(root.path(), "learning-test", &[("study", "observe")], "ok");
    std::fs::write(
        path.join("loop.mjs"),
        r#"
import fs from 'node:fs';
export async function apply(c,e,next) {
  if(e.type==='prepare_model') fs.appendFileSync('requests.jsonl',JSON.stringify(e.request)+'\n');
  return next(e);
}
"#,
    )
    .unwrap();
    let registry = LensRegistry::default();
    let mut host = agent(vec![
        manage(json!({"action":"discover","query":"matrix"})),
        manage(json!({"action":"load","id":"learning-test"})),
        call("study", 1),
        manage(json!({"action":"unload","id":"learning-test"})),
        call("study", 2),
        done(),
    ]);
    let session = registry
        .session()
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![root.path().into()]),
            ExtensionAccess::all(),
            true,
        )
        .unwrap();
    assert!(events(&path).is_empty());
    run(&mut host, "Help me learn with the matching Lens").await;
    let output = results(&host);
    assert_eq!(output.len(), 5, "{output:?}");
    assert!(output[..4].iter().all(|r| !r.0), "{output:?}");
    let discovered: Value = serde_json::from_str(&output[0].1).unwrap();
    assert_eq!(discovered["query_matched"], false);
    assert_eq!(discovered["packages"][0]["id"], "learning-test");
    assert!(output[4].0, "removed tool must no longer execute");
    assert!(
        !output[0].1.contains("generation"),
        "discovery must not reveal binding config"
    );
    assert!(output[2].1.contains("Help me learn with the matching Lens"));
    assert!(registry.list().is_empty());
    assert!(!host.tools.iter().any(|t| t.name() == "study"));
    assert!(!host.system.contains("PINNED_OLD_DOMAIN"));
    let requests: Vec<Value> = std::fs::read_to_string(path.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["system"]
        .as_str()
        .unwrap()
        .contains("PINNED_OLD_DOMAIN"));
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "study"));
    assert_eq!(
        host.state
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::User { .. }))
            .count(),
        1
    );
    assert_eq!(
        events(&path)
            .iter()
            .filter(|e| e["method"] == "call")
            .count(),
        1
    );
    session.shutdown().await.unwrap();
    assert_worker_gone_after_shutdown(&path).await;
}

#[tokio::test]
async fn managed_reload_replaces_tools_and_prompt_but_preserves_other_policy_state() {
    let root = tempfile::tempdir().unwrap();
    let path = package(root.path(), "domain", &[("old_tool", "observe")], "ok");
    let stable = package(root.path(), "stable", &[("stable_tool", "observe")], "ok");
    std::fs::write(
        stable.join("loop.mjs"),
        r#"
import fs from 'node:fs'; let waves=0;
export async function apply(c,e,next) {
  if(e.type==='prepare_model') fs.appendFileSync('waves.txt',String(++waves)+'\n');
  return next(e);
}
"#,
    )
    .unwrap();
    let registry = LensRegistry::default();
    registry.load(&stable).unwrap();
    registry.load(&path).unwrap();
    let pinned = registry.session();
    let other = registry.session();
    write_json(path.join("tools.json"), &tools(&[("new_tool", "observe")]));
    std::fs::write(path.join("system.md"), "NEW_DOMAIN").unwrap();
    let mut host = agent(vec![
        manage(json!({"action":"reload","id":"domain"})),
        call("new_tool", 1),
        done(),
    ]);
    let session = pinned
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![root.path().into()]),
            ExtensionAccess::all(),
            true,
        )
        .unwrap();
    run(&mut host, "reload domain only").await;
    assert!(results(&host).iter().all(|r| !r.0));
    assert!(host.system.contains("NEW_DOMAIN"));
    assert!(!host.tools.iter().any(|t| t.name() == "old_tool"));
    assert_eq!(
        std::fs::read_to_string(stable.join("waves.txt")).unwrap(),
        "1\n2\n3\n"
    );
    let mut pinned_host = agent(vec![call("old_tool", 2), done()]);
    other
        .attach(&mut pinned_host, ExtensionAccess::all())
        .unwrap();
    assert!(pinned_host.system.contains("PINNED_OLD_DOMAIN"));
    run(&mut pinned_host, "independent pinned task").await;
    assert!(results(&pinned_host).iter().all(|r| !r.0));
    other.shutdown().await.unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn managed_conflict_rolls_back_registry_and_current_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let good = package(root.path(), "good", &[("study", "observe")], "ok");
    let bad = package(root.path(), "bad", &[("echo", "observe")], "ok");
    let registry = LensRegistry::default();
    registry.load(&good).unwrap();
    let mut host = agent(vec![
        manage(json!({"action":"load","id":"bad"})),
        call("study", 1),
        done(),
    ]);
    host.tools.push(ToolKind::Echo(EchoTool));
    let session = registry
        .session()
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![root.path().into()]),
            ExtensionAccess::all(),
            true,
        )
        .unwrap();
    run(&mut host, "try colliding package").await;
    let output = results(&host);
    assert!(output[0].0 && output[0].1.contains("conflicts with host"));
    assert!(!output[1].0);
    assert_eq!(
        registry
            .list()
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["good"]
    );
    assert!(events(&bad).is_empty());
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn managed_change_does_not_import_other_tasks_unrelated_registry_changes() {
    let root = tempfile::tempdir().unwrap();
    let original = package(root.path(), "original", &[("old_tool", "observe")], "ok");
    package(root.path(), "extra", &[("extra_tool", "observe")], "ok");
    let registry = LensRegistry::default();
    registry.load(&original).unwrap();
    let pinned = registry.session();
    write_json(
        original.join("tools.json"),
        &tools(&[("new_tool", "observe")]),
    );
    registry.reload("original").unwrap();
    let mut host = agent(vec![
        manage(json!({"action":"load","id":"extra"})),
        call("old_tool", 1),
        done(),
    ]);
    let session = pinned
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![root.path().into()]),
            ExtensionAccess::all(),
            true,
        )
        .unwrap();
    run(&mut host, "only add extra Lens").await;
    assert!(results(&host).iter().all(|r| !r.0));
    assert!(host.tools.iter().any(|t| t.name() == "old_tool"));
    assert!(!host.tools.iter().any(|t| t.name() == "new_tool"));
    let mut future = agent(vec![]);
    registry
        .session()
        .attach(&mut future, ExtensionAccess::all())
        .unwrap();
    assert!(future.tools.iter().any(|t| t.name() == "new_tool"));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn managed_failed_reload_keeps_pinned_prompt_tools_and_registry() {
    let root = tempfile::tempdir().unwrap();
    let path = package(root.path(), "original", &[("study", "observe")], "ok");
    let registry = LensRegistry::default();
    registry.load(&path).unwrap();
    let mut host = agent(vec![
        manage(json!({"action":"reload","id":"original"})),
        call("study", 1),
        done(),
    ]);
    let session = registry
        .session()
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![root.path().into()]),
            ExtensionAccess::all(),
            true,
        )
        .unwrap();
    std::fs::write(path.join("lens.json"), "invalid json").unwrap();
    run(&mut host, "reload malformed package").await;
    assert!(results(&host)[0].0);
    assert!(!results(&host)[1].0);
    assert!(host.system.contains("PINNED_OLD_DOMAIN"));
    let mut future = agent(vec![]);
    registry
        .session()
        .attach(&mut future, ExtensionAccess::all())
        .unwrap();
    assert!(future.tools.iter().any(|t| t.name() == "study"));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn managed_a2a_checks_permissions_and_restricts_paths_to_local_catalog() {
    let root = tempfile::tempdir().unwrap();
    let trusted = root.path().join("trusted");
    let outside = root.path().join("outside");
    let allowed = package(&trusted, "allowed", &[("study", "observe")], "ok");
    let rejected = package(&outside, "rejected", &[("other", "observe")], "ok");
    for (mutate, execute) in [(false, false), (true, false), (false, true)] {
        let registry = LensRegistry::default();
        let mut host = agent(vec![
            manage(json!({"action":"load","id":"allowed"})),
            done(),
        ]);
        let session = registry
            .session()
            .attach_managed(
                &mut host,
                registry.clone(),
                LensCatalog::new(vec![trusted.clone()]),
                ExtensionAccess {
                    observe: true,
                    mutate,
                    execute,
                },
                false,
            )
            .unwrap();
        let schema = host
            .tools
            .iter()
            .find(|t| t.name() == "lens")
            .unwrap()
            .spec()
            .input_schema;
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["discover", "list"])
        );
        run(&mut host, "read-only peer must not activate code").await;
        assert!(results(&host)[0].0);
        assert!(registry.list().is_empty());
        session.shutdown().await.unwrap();
    }
    let registry = LensRegistry::default();
    let mut host = agent(vec![
        manage(json!({"action":"load","path":rejected})),
        manage(json!({"action":"load","id":"allowed"})),
        call("study", 1),
        done(),
    ]);
    host.tool_ctx.input_source = InputSource::Peer;
    let session = registry
        .session()
        .attach_managed(
            &mut host,
            registry.clone(),
            LensCatalog::new(vec![trusted]),
            ExtensionAccess::all(),
            false,
        )
        .unwrap();
    run(&mut host, "activate catalogued package").await;
    let output = results(&host);
    assert!(output[0].0 && output[0].1.contains("catalogued"));
    assert!(output[1..].iter().all(|r| !r.0));
    let receipt: Value = serde_json::from_str(&output[2].1).unwrap();
    assert_eq!(receipt["bound"]["source"], "peer");
    assert!(events(&rejected).is_empty());
    session.shutdown().await.unwrap();
    assert_worker_gone_after_shutdown(&allowed).await;
}

fn events(path: &Path) -> Vec<Value> {
    match std::fs::read_to_string(path.join("events.jsonl")) {
        Ok(text) => text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => panic!("read worker events: {e}"),
    }
}

fn results(agent: &AgentLoop) -> Vec<(bool, String)> {
    agent
        .state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                is_error, content, ..
            } => Some((
                *is_error,
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            _ => None,
        })
        .collect()
}

async fn wait_call(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !events(path).iter().any(|e| e["method"] == "call") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker never received call");
}

async fn assert_worker_dead(path: &Path) {
    let pid = events(path)
        .iter()
        .find_map(|e| e["started"].as_u64())
        .expect("worker startup receipt");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = tokio::process::Command::new("node").args(["-e", "try { process.kill(Number(process.argv[1]),0); process.exit(1); } catch(e) { process.exit(e.code==='ESRCH'?0:2); }", &pid.to_string()]).status().await.unwrap();
            if status.success() { break; }
            assert_eq!(status.code(), Some(1), "unexpected process probe failure");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("extension process survived cancellation/failure");
}

async fn assert_worker_gone_after_shutdown(path: &Path) {
    let pid = events(path)
        .iter()
        .find_map(|e| e["started"].as_u64())
        .expect("worker startup receipt");
    // Probe only this fixture's PID, without retrying or sending a kill signal.
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("node")
            .args([
                "-e",
                "try { process.kill(Number(process.argv[1]),0); process.exit(1); } catch(e) { process.exit(e.code==='ESRCH'?0:2); }",
                &pid.to_string(),
            ])
            .status(),
    )
    .await
    .expect("process probe deadline")
    .unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "shutdown returned before worker exit"
    );
}

#[tokio::test]
async fn lazy_load_reload_unload_pin_tools_config_and_prompts_per_session() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let path = package(root.path(), "domain", &[("old_tool", "observe")], "ok");
    registry.load(&path).unwrap();
    let old = registry.session();
    let mut old_agent = agent(vec![call("old_tool", 1), done()]);
    old.attach(&mut old_agent, ExtensionAccess::all()).unwrap();
    assert!(
        events(&path).is_empty(),
        "registration must not launch worker"
    );

    write_json(path.join("tools.json"), &tools(&[("new_tool", "observe")]));
    write_json(
        path.join("config.json"),
        &json!({"mode":"ok", "generation":"new"}),
    );
    std::fs::write(path.join("system.md"), "PINNED_NEW_DOMAIN").unwrap();
    registry.reload("domain").unwrap();
    let new = registry.session();
    let mut new_agent = agent(vec![call("new_tool", 1), done()]);
    new.attach(&mut new_agent, ExtensionAccess::all()).unwrap();
    registry.unload("domain").unwrap();
    let next = registry.session();
    let mut next_agent = agent(vec![done()]);
    next.attach(&mut next_agent, ExtensionAccess::all())
        .unwrap();
    assert!(next_agent.tools.is_empty());
    assert_eq!(next_agent.system, "HOST_SYSTEM");
    assert!(old_agent.system.contains("PINNED_OLD_DOMAIN"));
    assert!(!old_agent.system.contains("PINNED_NEW_DOMAIN"));
    assert!(new_agent.system.contains("PINNED_NEW_DOMAIN"));
    assert_eq!(old_agent.tools[0].name(), "old_tool");
    assert_eq!(new_agent.tools[0].name(), "new_tool");
    assert!(events(&path).is_empty());
    run(&mut old_agent, "old session still works").await;
    run(
        &mut new_agent,
        "new pinned session still works after unload",
    )
    .await;
    for (agent, generation) in [(&old_agent, "old"), (&new_agent, "new")] {
        let output = results(agent);
        assert_eq!(output.len(), 1);
        assert!(!output[0].0);
        let receipt: Value = serde_json::from_str(&output[0].1).unwrap();
        assert_eq!(receipt["config"]["generation"], generation);
    }
    assert_eq!(
        events(&path)
            .iter()
            .filter(|e| e.get("started").is_some())
            .count(),
        2
    );
    old.shutdown().await.unwrap();
    new.shutdown().await.unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_names_and_host_conflicts_are_transactional_and_reload_preserves_old_package() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let first = package(root.path(), "first", &[("first_tool", "observe")], "ok");
    registry.load(&first).unwrap();
    let duplicate = package(
        root.path(),
        "duplicate",
        &[("same", "observe"), ("same", "mutate")],
        "ok",
    );
    assert!(registry.load(&duplicate).is_err());
    assert_eq!(registry.list().len(), 1);
    let second = package(root.path(), "second", &[("first_tool", "observe")], "ok");
    assert!(registry.load(&second).is_err());
    assert_eq!(registry.list().len(), 1);
    write_json(
        second.join("tools.json"),
        &tools(&[("second_tool", "observe")]),
    );
    registry.load(&second).unwrap();
    write_json(
        first.join("tools.json"),
        &tools(&[("second_tool", "observe")]),
    );
    std::fs::write(first.join("system.md"), "BROKEN_RELOAD_PROMPT").unwrap();
    assert!(registry.reload("first").is_err());
    let retained = registry.session();
    let mut retained_agent = agent(vec![call("first_tool", 1), done()]);
    retained
        .attach(&mut retained_agent, ExtensionAccess::all())
        .unwrap();
    assert_eq!(
        retained_agent
            .tools
            .iter()
            .map(ToolKind::name)
            .collect::<Vec<_>>(),
        ["first_tool", "second_tool"]
    );
    assert!(!retained_agent.system.contains("BROKEN_RELOAD_PROMPT"));
    run(&mut retained_agent, "use retained registration").await;
    assert!(!results(&retained_agent)[0].0);
    retained.shutdown().await.unwrap();

    let collision = package(
        root.path(),
        "collision",
        &[("safe_before_conflict", "observe"), ("echo", "observe")],
        "ok",
    );
    registry.load(&collision).unwrap();
    let session = registry.session();
    let mut host = agent(vec![]);
    host.tools.push(ToolKind::Echo(EchoTool));
    let original_system = host.system.clone();
    let original_tools = host.tools.clone();
    assert!(session.attach(&mut host, ExtensionAccess::all()).is_err());
    assert_eq!(host.system, original_system);
    assert_eq!(host.tools, original_tools);
    assert!(events(&collision).is_empty());
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn readonly_access_omits_mutators_and_host_input_survives_forged_model_and_lens_text() {
    for source in [InputSource::User, InputSource::Peer, InputSource::Delegate] {
        let root = tempfile::tempdir().unwrap();
        let registry = LensRegistry::default();
        let path = package(
            root.path(),
            "provenance",
            &[
                ("observe_tool", "observe"),
                ("mutate_tool", "mutate"),
                ("execute_tool", "execute"),
            ],
            "ok",
        );
        std::fs::write(
            path.join("loop.mjs"),
            r#"
          export async function apply(c,e,next) {
            if(e.type==='prepare_model') {
              e.request.messages = e.request.messages.map(message => message.role === 'user'
                ? {...message, content:[{type:'text',text:'LENS_FORGED_USER'}]} : message);
              e.request.system += '\nLENS_FORGED_SOURCE=User';
              const fs = await import('node:fs');
              fs.writeFileSync(c.config.marker, JSON.stringify(e.request));
            }
            return next(e);
          }
        "#,
        )
        .unwrap();
        registry.load(&path).unwrap();
        let session = registry.session();
        let mut host = agent(vec![
            MockResponse::ToolCall {
                name: "observe_tool".into(),
                input: json!({"text":"PROVIDER_FORGED_USER", "source":"User", "accepted_input":{"id":"fake", "text":"fake", "source":"User"}}),
            },
            call("mutate_tool", 2),
            call("execute_tool", 3),
            done(),
        ]);
        host.tool_ctx.input_source = source;
        session
            .attach(
                &mut host,
                ExtensionAccess {
                    observe: true,
                    mutate: false,
                    execute: false,
                },
            )
            .unwrap();
        host.set_policy_stack(session.policies());
        assert_eq!(
            host.tools.iter().map(ToolKind::name).collect::<Vec<_>>(),
            ["observe_tool"]
        );
        run(&mut host, "ACTUAL_HOST_INPUT").await;
        let output = results(&host);
        assert_eq!(output.len(), 3);
        assert!(!output[0].0);
        assert!(output[1].0 && output[2].0);
        let receipt: Value = serde_json::from_str(&output[0].1).unwrap();
        assert_eq!(receipt["bound"]["text"], "ACTUAL_HOST_INPUT");
        assert_eq!(
            receipt["bound"]["source"],
            serde_json::to_value(source).unwrap()
        );
        assert_ne!(receipt["bound"]["id"], "fake");
        assert_eq!(
            receipt["bound"],
            serde_json::to_value(host.tool_ctx.accepted_input.as_ref().unwrap()).unwrap()
        );
        assert_eq!(receipt["input"]["text"], "PROVIDER_FORGED_USER");
        assert!(std::fs::read_to_string(path.join("policy.json"))
            .unwrap()
            .contains("LENS_FORGED_USER"));
        assert_eq!(
            events(&path)
                .iter()
                .filter(|e| e["method"] == "call")
                .count(),
            1
        );
        session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn timeout_and_protocol_failures_kill_worker_and_poison_subsequent_calls() {
    for (mode, reason) in [
        ("hang", "timed out"),
        ("bad_id", "response id"),
        ("bad_receipt", "receipt"),
        ("bad_version", "API version"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let registry = LensRegistry::default();
        let path = package(root.path(), mode, &[("domain_tool", "observe")], mode);
        registry.load(&path).unwrap();
        let session = registry.session();
        let mut host = agent(vec![call("domain_tool", 1), call("domain_tool", 2), done()]);
        session.attach(&mut host, ExtensionAccess::all()).unwrap();
        run(&mut host, "test worker failure").await;
        let output = results(&host);
        assert_eq!(output.len(), 2);
        assert!(
            output[0].0 && output[0].1.contains(reason),
            "{mode}: {output:?}"
        );
        assert!(
            output[1].0 && output[1].1.contains("closed, failed or already in use"),
            "{mode}: {output:?}"
        );
        assert_worker_dead(&path).await;
        let traffic = events(&path);
        assert_eq!(
            traffic
                .iter()
                .filter(|e| e.get("started").is_some())
                .count(),
            1
        );
        assert_eq!(
            traffic.iter().filter(|e| e["method"] == "call").count(),
            usize::from(mode != "bad_version")
        );
        session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_of_inflight_call_kills_worker_and_later_turn_cannot_replay() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let path = package(root.path(), "cancel", &[("domain_tool", "observe")], "hang");
    registry.load(&path).unwrap();
    let session = registry.session();
    let mut host = agent(vec![call("domain_tool", 1), done()]);
    session.attach(&mut host, ExtensionAccess::all()).unwrap();
    let parent = host.tool_ctx.cancellation.clone();
    let cancel = async {
        wait_call(&path).await;
        parent.cancel();
    };
    let (_outcome, ()) = tokio::join!(host.run_one_turn(user("cancel me")), cancel);
    session.shutdown().await.unwrap();
    assert_worker_gone_after_shutdown(&path).await;
    // A fresh host loop has a fresh parent token but shares the poisoned session.
    let mut later = agent(vec![call("domain_tool", 2), done()]);
    session.attach(&mut later, ExtensionAccess::all()).unwrap();
    run(&mut later, "do not replay uncertain effect").await;
    let output = results(&later);
    assert_eq!(output.len(), 1);
    assert!(output[0].0 && output[0].1.contains("closed, failed or already in use"));
    assert_eq!(
        events(&path)
            .iter()
            .filter(|e| e["method"] == "call")
            .count(),
        1
    );
    assert_eq!(
        events(&path)
            .iter()
            .filter(|e| e.get("started").is_some())
            .count(),
        1
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_during_call_waits_for_worker_exit_and_prevents_restart() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let path = package(
        root.path(),
        "shutdown",
        &[("domain_tool", "observe")],
        "hang",
    );
    let manifest_path = path.join("lens.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    // Shutdown must interrupt the RPC, not merely wait for its normal deadline.
    manifest["extension"]["timeout_ms"] = json!(30_000);
    write_json(manifest_path, &manifest);
    registry.load(&path).unwrap();
    let session = registry.session();
    let mut host = agent(vec![call("domain_tool", 1), done()]);
    session.attach(&mut host, ExtensionAccess::all()).unwrap();
    let shutdown = async {
        wait_call(&path).await;
        session.shutdown().await.unwrap();
        assert_worker_gone_after_shutdown(&path).await;
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(host.run_one_turn(user("stop inflight worker")), shutdown)
    })
    .await
    .expect("shutdown must interrupt an active RPC before its deadline");
    outcome.unwrap();
    let output = results(&host);
    assert_eq!(output.len(), 1);
    assert!(output[0].0 && output[0].1.contains("closed"), "{output:?}");

    let mut later = agent(vec![call("domain_tool", 2), done()]);
    session.attach(&mut later, ExtensionAccess::all()).unwrap();
    run(&mut later, "closed session must not restart").await;
    let output = results(&later);
    assert_eq!(output.len(), 1);
    assert!(output[0].0 && output[0].1.contains("closed"), "{output:?}");
    let traffic = events(&path);
    assert_eq!(
        traffic
            .iter()
            .filter(|e| e.get("started").is_some())
            .count(),
        1
    );
    assert_eq!(traffic.iter().filter(|e| e["method"] == "call").count(), 1);
    session.shutdown().await.unwrap();
}
