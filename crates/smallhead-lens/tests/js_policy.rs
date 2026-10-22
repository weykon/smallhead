//! Actual Node worker integration, including cancellation and generation pinning.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use smallhead_core::{loop_policy::*, *};
use smallhead_lens::LensRegistry;

fn package(root: &Path, id: &str, source: &str) -> PathBuf {
    let path = root.join(id);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("lens.json"),
        serde_json::to_vec(
            &json!({"id":id,"version":"1","api_version":1,"entry":"loop.mjs","config":{"tag":id}}),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::write(path.join("loop.mjs"), source).unwrap();
    path
}
fn context() -> PolicyContext {
    PolicyContext {
        turn_id: "turn".into(),
        wave: 1,
        tool_calls: 0,
        can_continue: true,
        finalization_reason: None,
    }
}
fn event() -> PolicyEvent {
    PolicyEvent::PrepareModel {
        request: ProviderRequest::default(),
    }
}
async fn system(stack: &PolicyStack) -> String {
    let PolicyAction::ModelRequest { request } =
        tokio::time::timeout(Duration::from_secs(5), stack.decide(context(), event()))
            .await
            .unwrap()
            .unwrap()
    else {
        panic!()
    };
    request.system
}

#[tokio::test]
async fn real_js_workers_compose_both_directions_and_keep_private_state() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let source = r#"
        let calls = 0;
        console.log('console must not corrupt stdout');
        export async function apply(context, event, next) {
          if (event.type === 'prepare_model') event.request.system += context.config.tag + (++calls);
          const action = await next(event);
          if (action.type === 'model_request') action.request.system += context.config.tag;
          return action;
        }
    "#;
    registry.load(package(root.path(), "A", source)).unwrap();
    registry.load(package(root.path(), "B", source)).unwrap();
    let session = registry.session();
    assert_eq!(system(&session.policies()).await, "A1B1BA");
    assert_eq!(system(&session.policies()).await, "A2B2BA");
    let separate = registry.session();
    assert_eq!(system(&separate.policies()).await, "A1B1BA");
    let mut other_turn = context();
    other_turn.turn_id = "different-turn".into();
    assert!(separate
        .policies()
        .decide(other_turn, event())
        .await
        .is_err());
    separate.shutdown().await.unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_and_unload_affect_new_sessions_only_and_failed_reload_is_atomic() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    let source = |tag| {
        format!("export async function apply(ctx,e,next) {{ if(e.type==='prepare_model') e.request.system += '{tag}'; return next(e); }}")
    };
    let path = package(root.path(), "test", &source("old"));
    let manifest_path = path.join("custom.json");
    std::fs::rename(path.join("lens.json"), &manifest_path).unwrap();
    registry.load(&manifest_path).unwrap();
    assert!(registry.load(&manifest_path).is_err());
    let old = registry.session();
    std::fs::write(path.join("loop.mjs"), source("new")).unwrap();
    registry.reload("test").unwrap();
    let new = registry.session();
    // Old session was not even started until after the file edit.
    assert_eq!(system(&old.policies()).await, "old");
    assert_eq!(system(&new.policies()).await, "new");
    std::fs::write(&manifest_path, "{}").unwrap();
    assert!(registry.reload("test").is_err());
    assert_eq!(registry.list()[0].id, "test");
    registry.unload("test").unwrap();
    assert!(registry.list().is_empty());
    assert_eq!(system(&new.policies()).await, "new");
    assert_eq!(system(&registry.session().policies()).await, "");
    old.shutdown().await.unwrap();
    new.shutdown().await.unwrap();
}

#[tokio::test]
async fn js_finish_decision_changes_the_real_agent_loop() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    registry
        .load(package(
            root.path(),
            "review",
            r#"
        let checked = false;
        export async function apply(ctx, e, next) {
          const action = await next(e);
          if (action.type === 'finish' && !checked && ctx.can_continue) {
            checked = true;
            return {type:'continue',guidance:'Check the candidate against the original question.'};
          }
          return action;
        }
    "#,
        ))
        .unwrap();
    let session = registry.session();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::AssistantText {
            text: "candidate".into(),
        },
        MockResponse::AssistantText {
            text: "reviewed".into(),
        },
    ]));
    let mut agent = AgentLoop::new(provider.clone(), vec![], "base");
    agent.set_policy_stack(session.policies());
    agent
        .run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "question".into(),
            }],
            timestamp: 1,
        })
        .await
        .unwrap();
    assert!(provider.responses.lock().unwrap().is_empty());
    assert!(
        matches!(agent.state.messages.last(), Some(AgentMessage::Assistant { content, .. }) if matches!(&content[0], ContentBlock::Text { text } if text == "reviewed"))
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_protocol_errors_and_duplicate_next_poison_the_instance() {
    for source in [
        "export function apply(){process.stdout.write('not-json\\n'); return {type:'finish'};}",
        "export function apply(){throw new Error('broken');}",
        "export async function apply(c,e,next){let a=await next(e);try{await next(e)}catch{}return a;}",
        "export async function apply(c,e,next){return {type:'nonexistent'};}",
        "export async function apply(c,e,next){next(e);return {type:'model_request',request:e.request};}",
        "export function apply(){process.stdout.write(JSON.stringify({type:'result',id:999,action:{type:'finish'}})+'\\n');return {type:'finish'};}",
    ] {
        let root = tempfile::tempdir().unwrap();
        let registry = LensRegistry::default();
        registry.load(package(root.path(), "bad", source)).unwrap();
        let session = registry.session();
        assert!(session.policies().decide(context(), event()).await.is_err());
        assert!(session.policies().decide(context(), event()).await.unwrap_err().to_string().contains("failed, closed or already in use"));
        session.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_inflight_rpc_kills_the_worker_and_does_not_restart_it() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    registry
        .load(package(
            root.path(),
            "hang",
            r#"
        let calls = 0;
        export async function apply(ctx,e,next) {
          if (++calls > 1) await new Promise(()=>{});
          e.request.system = String(process.pid);
          return next(e);
        }
    "#,
        ))
        .unwrap();
    let session = registry.session();
    let stack = session.policies();
    let pid = system(&stack).await;
    assert!(alive(&pid));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stack.decide(context(), event()))
            .await
            .is_err()
    );
    for _ in 0..100 {
        if !alive(&pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!alive(&pid));
    assert!(stack.decide(context(), event()).await.is_err());
    session.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_reaps_idle_workers_even_if_policy_handles_remain() {
    let root = tempfile::tempdir().unwrap();
    let registry = LensRegistry::default();
    registry.load(package(root.path(), "pid", "export async function apply(c,e,next){ e.request.system=String(process.pid); return next(e); }")).unwrap();
    let session = registry.session();
    let stack = session.policies();
    let pid = system(&stack).await;
    session.shutdown().await.unwrap();
    assert!(!alive(&pid));
    assert!(stack.decide(context(), event()).await.is_err());
}

#[test]
fn manifests_reject_unsupported_versions_and_entries_outside_package() {
    let root = tempfile::tempdir().unwrap();
    let path = package(root.path(), "test", "");
    let registry = LensRegistry::default();
    for manifest in [
        json!({"id":"test","version":"1","api_version":2,"entry":"loop.mjs"}),
        json!({"id":"test","version":"1","api_version":1,"entry":"../outside.mjs"}),
    ] {
        std::fs::write(root.path().join("outside.mjs"), "").unwrap();
        std::fs::write(
            path.join("lens.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(registry.load(&path).is_err());
    }
    assert!(registry.list().is_empty());
}
