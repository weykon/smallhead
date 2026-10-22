use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{extract::State, http::Method, routing::any, Json, Router};
use serde_json::{json, Value};
use smallhead_core::{
    execution::ExecutionRuntime, monad::MonadLoop, tool::ToolImpl, Budget, MonadHandoff, MonadId,
    MonadResult, ResultSlot, TaskId, ToolContext, TraceMeta, SCHEMA_VERSION_V1,
};
use smallhead_process::{ExecutionId, ProcessManager, ProcessSpec};
use tempfile::TempDir;

use crate::{
    backend::{self, project_scope, Request},
    ProjectResearchMonad, ProjectResearchTool, XerjConfig,
};

#[derive(Clone)]
struct ServerState {
    identity: Value,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

struct Fixture {
    _dir: TempDir,
    config: XerjConfig,
    ctx: ToolContext,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(identity: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let requests = Arc::new(Mutex::new(vec![]));
        let state = ServerState {
            identity,
            requests: requests.clone(),
        };
        let app = Router::new().fallback(any(|State(state): State<ServerState>, method: Method, uri: axum::http::Uri, body: axum::body::Bytes| async move {
            let input = serde_json::from_slice(&body).unwrap_or(Value::Null);
            state.requests.lock().unwrap().push((uri.path().into(), input));
            Json(if uri.path() == "/" { state.identity }
                else if method == Method::GET { json!({"project-code":{"mappings":{"properties":{
                    "body":{"type":"text"},"defs":{"type":"text"},"ax_path":{"type":"keyword"},"line":{"type":"integer"}
                }}}}) }
                else { json!({"hits":{"hits":[{"_source":{"ax_path":"src/main.rs"},"fields":{"_passage":["fn answer() -> u32 { 42 }"]}}]}}) })
        })).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let runtime = ExecutionRuntime::new(
            ProcessManager::with_log_root(dir.path().join("processes")).unwrap(),
        )
        .unwrap();
        let config = XerjConfig {
            binary: dir.path().join("xerj"),
            data_dir: dir.path().join("data"),
            port,
        };
        let ctx = ToolContext {
            execution: Some(Arc::new(runtime)),
            cwd: project,
            owner: Some("parent-task".into()),
            ..Default::default()
        };
        Self {
            _dir: dir,
            config,
            ctx,
            requests,
            server,
        }
    }

    #[cfg(unix)]
    fn executable(&self, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&self.config.binary, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&self.config.binary, std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

fn handoff(inputs: Value) -> MonadHandoff {
    MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.into(),
        task_id: TaskId::new(),
        parent_task_id: Some(TaskId("parent-task".into())),
        monad_id: MonadId::new("project_research"),
        attempt_id: 1,
        objective: "Find project evidence".into(),
        inputs: inputs.as_object().unwrap().clone(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "Return passages".into(),
        result_slot: ResultSlot("children.research".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: "test".into(),
            request_id: "request".into(),
        },
    }
}

#[test]
fn canonical_project_namespaces_are_stable_and_distinct() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("b")).unwrap();
    let a = project_scope(Some(std::path::Path::new("a")), dir.path()).unwrap();
    let alias = project_scope(Some(&dir.path().join("a/../a")), dir.path()).unwrap();
    let b = project_scope(Some(std::path::Path::new("b")), dir.path()).unwrap();
    assert_eq!(a, alias);
    assert_ne!(a.1, b.1);
    assert_eq!(a.1.len(), 27);
}

#[tokio::test]
async fn search_uses_scoped_mappings_and_bounded_passages() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    let result = backend::run(
        &f.config,
        &f.ctx,
        Request::Search {
            project_path: None,
            query: "answer".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        result["search"]["hits"]["hits"][0]["fields"]["_passage"][0],
        "fn answer() -> u32 { 42 }"
    );
    let requests = f.requests.lock().unwrap();
    let (_, prefix) = project_scope(None, &f.ctx.cwd).unwrap();
    assert_eq!(requests[1].0, format!("/{prefix}-*/_mapping"));
    assert_eq!(requests[2].0, format!("/{prefix}-*/_search"));
    let body = &requests[2].1;
    assert_eq!(body["size"], 5);
    assert_eq!(body["fields"], json!(["_passage"]));
    assert_eq!(
        body["query"]["bool"]["should"][0]["multi_match"]["fields"],
        json!(["body", "defs"])
    );
    assert!(body["_source"].as_array().unwrap().contains(&json!("body")));
}

#[test]
fn lexical_excerpts_are_bounded_unicode_safe_and_separate_from_server_passages() {
    let text = format!("{}needle{}", "前".repeat(1500), "后".repeat(1500));
    let mut response = json!({"hits":{"hits":[{"_source":{"ax_path":"notes.txt","body":text}}]}});
    backend::project_hits(&mut response, "needle");
    let hit = &response["hits"]["hits"][0];
    assert!(hit["_source"].get("body").is_none());
    assert_eq!(hit["_source"]["ax_path"], "notes.txt");
    let excerpt = &hit["source_excerpts"][0];
    assert_eq!(excerpt["start_char"], 1300);
    assert_eq!(excerpt["end_char"], 2500);
    assert_eq!(excerpt["text"].as_str().unwrap().chars().count(), 1200);
    assert!(excerpt["text"].as_str().unwrap().contains("needle"));
    assert!(
        hit.get("fields").is_none(),
        "Host excerpts must not pretend to be server passages"
    );
}

#[tokio::test]
async fn wrong_server_identity_prevents_project_requests() {
    let f = Fixture::new(json!({"cluster_name":"elasticsearch"})).await;
    let error = backend::run(&f.config, &f.ctx, Request::Map { project_path: None })
        .await
        .unwrap_err();
    assert!(error.contains("does not identify"));
    assert_eq!(f.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ensure_reuses_external_node_and_registers_service_description() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    let result = backend::run(&f.config, &f.ctx, Request::Ensure)
        .await
        .unwrap();
    assert!(result.get("operation_id").is_some());
    let rt = f.ctx.execution.as_ref().unwrap();
    assert!(rt.services.service_view("xerj").is_some());
    assert!(
        rt.processes.list().is_empty(),
        "External nodes must not spawn another binary"
    );
    rt.services
        .update_description("xerj", "User-maintained project index".into())
        .unwrap();
    backend::run(&f.config, &f.ctx, Request::Ensure)
        .await
        .unwrap();
    assert_eq!(
        rt.services
            .service_view("xerj")
            .unwrap()
            .description
            .as_deref(),
        Some("User-maintained project index")
    );
}

#[tokio::test]
async fn indexing_rejects_data_inside_project_before_spawning() {
    let mut f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    f.config.data_dir = f.ctx.cwd.join("index-data");
    let error = backend::run(
        &f.config,
        &f.ctx,
        Request::Index {
            project_path: None,
            approval: None,
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("must not contain"));
    assert!(f
        .ctx
        .execution
        .as_ref()
        .unwrap()
        .processes
        .list()
        .is_empty());
}

#[tokio::test]
async fn monad_preserves_binding_and_cancellation_clears_pending_operation() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    let mut child = ProjectResearchMonad::new(f.config.clone(), f.ctx.clone());
    let task = handoff(json!({"operation":"map"}));
    child.start(task.clone()).await.unwrap();
    assert!(child
        .start(handoff(json!({"operation":"unknown"})))
        .await
        .is_err());
    match child.step().await.unwrap() {
        MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            ..
        } => {
            assert_eq!(task_id, task.task_id);
            assert_eq!(parent_task_id, task.parent_task_id);
            assert_eq!(result_slot, task.result_slot);
        }
        result => panic!("Unexpected result {result:?}"),
    }
    child.start(task).await.unwrap();
    child.cancel().await.unwrap();
    assert!(child.step().await.is_err());
}

#[tokio::test]
async fn invalid_dispatch_is_rejected_without_starting_services() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    assert!(ProjectResearchTool
        .execute(
            json!({"action":"dispatch","objective":"test","inputs":{"operation":"search"}}),
            &f.ctx
        )
        .await
        .is_err());
    assert!(f
        .ctx
        .execution
        .as_ref()
        .unwrap()
        .services
        .service_view("xerj")
        .is_none());
    assert!(f.requests.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn index_exit_codes_and_stderr_are_preserved() {
    for (code, phase) in [
        (0, "completed"),
        (3, "completed_with_junk"),
        (4, "needs_decision"),
        (1, "failed"),
    ] {
        let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
        f.executable(&format!(
            "printf '%s\\n' \"$@\"\nprintf 'diagnostic' >&2\nexit {code}"
        ));
        let output = backend::run(
            &f.config,
            &f.ctx,
            Request::Index {
                project_path: None,
                approval: None,
            },
        )
        .await
        .unwrap();
        let id: ExecutionId = serde_json::from_value(output["execution_id"].clone()).unwrap();
        let rt = f.ctx.execution.as_ref().unwrap();
        let record = rt
            .processes
            .wait_for(id, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(record.is_terminal());
        assert_eq!(record.owner.as_deref(), Some("parent-task"));
        let result = backend::run(
            &f.config,
            &f.ctx,
            Request::Poll {
                execution_id: id,
                offset: 0,
                stderr: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(result["phase"], phase);
        assert!(result["output"]["text"]
            .as_str()
            .unwrap()
            .contains("--no-graph"));
        let stderr = backend::run(
            &f.config,
            &f.ctx,
            Request::Poll {
                execution_id: id,
                offset: 0,
                stderr: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(stderr["output"]["text"], "diagnostic");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn poll_reports_disk_backpressure_while_job_is_still_running() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    f.executable("printf 'read_only_allow_delete (disk usage 98%% exceeded flood-stage watermark [95%%])\\n' >&2\nsleep 30");
    let job = backend::run(
        &f.config,
        &f.ctx,
        Request::Index {
            project_path: None,
            approval: None,
        },
    )
    .await
    .unwrap();
    let id = serde_json::from_value(job["execution_id"].clone()).unwrap();
    let poll = async {
        loop {
            let result = backend::run(
                &f.config,
                &f.ctx,
                Request::Poll {
                    execution_id: id,
                    offset: 0,
                    stderr: false,
                },
            )
            .await
            .unwrap();
            if result["blocker"]["kind"] == "disk_flood_stage" {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(5), poll).await;
    let rt = f.ctx.execution.as_ref().unwrap();
    rt.processes.stop(id).await.unwrap();
    rt.processes
        .wait_for(id, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.unwrap()["phase"], "running");
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_submissions_share_job_and_parent_can_stop_it() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    f.executable("sleep 30");
    let request = Request::Index {
        project_path: None,
        approval: None,
    };
    let (a, b) = tokio::join!(
        backend::run(&f.config, &f.ctx, request.clone()),
        backend::run(&f.config, &f.ctx, request)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a["execution_id"], b["execution_id"]);
    assert!(a["already_running"] == true || b["already_running"] == true);
    let id: ExecutionId = serde_json::from_value(a["execution_id"].clone()).unwrap();
    let rt = f.ctx.execution.as_ref().unwrap();
    rt.processes.stop_owner("parent-task").await.unwrap();
    assert!(rt
        .processes
        .wait_for(id, Duration::from_secs(5))
        .await
        .unwrap()
        .is_terminal());
}

#[tokio::test]
async fn poll_refuses_unrelated_process_records() {
    let f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    let rt = f.ctx.execution.as_ref().unwrap();
    let id = rt
        .processes
        .start(ProcessSpec::program("echo", vec!["hello".into()]))
        .await
        .unwrap();
    rt.processes
        .wait_for(id, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(backend::run(
        &f.config,
        &f.ctx,
        Request::Poll {
            execution_id: id,
            offset: 0,
            stderr: false
        }
    )
    .await
    .unwrap_err()
    .contains("not an XERJ"));
}

/// Opt-in smoke test: SMALLHEAD_XERJ_BIN points to the verified upstream binary.
#[tokio::test]
#[ignore = "requires an installed XERJ binary"]
async fn live_index_and_search() {
    let mut f = Fixture::new(json!({"cluster_name":"xerj"})).await;
    f.server.abort();
    let _ = (&mut f.server).await;
    let binary = std::env::var_os("SMALLHEAD_XERJ_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/bin/xerj")
        });
    f.config.binary = binary;
    std::fs::write(
        f.ctx.cwd.join("README.md"),
        "# Orchard\nThe orchard scheduler coordinates harvest workers.\n",
    )
    .unwrap();
    std::fs::write(
        f.ctx.cwd.join("main.rs"),
        "fn orchard_schedule() -> u32 { 42 }\n",
    )
    .unwrap();
    let config = f.config.clone();
    let ctx = f.ctx.clone();
    let result = async move {
        backend::run(&config, &ctx, Request::Ensure).await.unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        if std::env::var_os("SMALLHEAD_XERJ_SMOKE_RELAX_DISK_GUARD").is_some() {
            // Only this test's freshly-created disposable node; production keeps the guard.
            client.put(format!("{}/_cluster/settings", config.url())).json(&json!({"persistent":{"cluster.routing.allocation.disk.watermark.flood_stage":"100%"}})).send().await.unwrap().error_for_status().unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let job = backend::run(
            &config,
            &ctx,
            Request::Index {
                project_path: None,
                approval: None,
            },
        )
        .await
        .unwrap();
        let id = serde_json::from_value(job["execution_id"].clone()).unwrap();
        ctx.execution
            .as_ref()
            .unwrap()
            .processes
            .wait_for(id, Duration::from_secs(120))
            .await
            .unwrap();
        let status = backend::run(
            &config,
            &ctx,
            Request::Poll {
                execution_id: id,
                offset: 0,
                stderr: false,
            },
        )
        .await
        .unwrap();
        assert!(
            status["phase"] == "completed" || status["phase"] == "completed_with_junk",
            "{status}"
        );
        let result = backend::run(
            &config,
            &ctx,
            Request::Search {
                project_path: None,
                query: "orchard".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            !result["search"]["hits"]["hits"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{result}"
        );
        assert!(
            result.to_string().contains("_passage")
                || result.to_string().contains("source_excerpts"),
            "No retrieved passages: {result}"
        );
    };
    // Always clean up a managed node, including when an assertion panics.
    let outcome = tokio::spawn(result).await;
    if outcome.is_err() {
        let processes = &f.ctx.execution.as_ref().unwrap().processes;
        for id in processes.list() {
            for stream in [
                smallhead_process::OutputStream::Stdout,
                smallhead_process::OutputStream::Stderr,
            ] {
                if let Ok(page) = processes.read_output(id, stream, 0, 12000).await {
                    eprintln!("{id} {stream:?}: {}", page.text);
                }
            }
        }
    }
    f.ctx
        .execution
        .as_ref()
        .unwrap()
        .processes
        .shutdown(Duration::from_secs(5))
        .await
        .unwrap();
    outcome.unwrap();
}
#[tokio::test]
async fn delegated_lookup_rejects_indexing_before_environment_or_side_effects() {
    use smallhead_core::tool::ToolImpl;
    for operation in ["ensure", "index", "poll"] {
        let result = crate::ProjectLookupTool
            .execute(
                serde_json::json!({"operation":operation}),
                &smallhead_core::ToolContext::default(),
            )
            .await;
        assert!(matches!(
            result,
            Err(smallhead_core::ToolError::InvalidInput(_))
        ));
    }
}
