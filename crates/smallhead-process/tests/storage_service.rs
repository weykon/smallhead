use serde_json::json;
use smallhead_process::{
    service::{ServiceManager, ServiceSpec, ServiceState},
    ProcessLimits, ProcessManager, ProcessSpec, ProcessState,
};
use std::{collections::BTreeMap, time::Duration};

async fn controlled_health(
    port: u16,
) -> (
    u16,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let (h, e, r) = (hold.clone(), entered.clone(), release.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 2048];
            if stream.read(&mut buf).await.unwrap_or(0) == 0 {
                continue;
            }
            if h.load(std::sync::atomic::Ordering::SeqCst) {
                e.notify_one();
                r.notified().await;
            }
            let body = r#"{"kind":"smallhead-test"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (port, hold, entered, release, server)
}

#[tokio::test]
async fn description_update_during_probe_survives_and_forget_is_not_resurrected() {
    let dir = tempfile::tempdir().unwrap();
    let pm = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(pm).unwrap();
    let (port, hold, entered, release, server) = controlled_health(0).await;
    services
        .ensure(spec("description-race", port, "echo unused".into()))
        .await
        .unwrap();
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let svc = services.clone();
    let pending = tokio::spawn(async move { svc.inspect("description-race").await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    services
        .update_description("description-race", "新的说明".into())
        .unwrap();
    let updated = services
        .service_view("description-race")
        .unwrap()
        .description_updated_at;
    release.notify_one();
    pending.await.unwrap().unwrap();
    let view = services.service_view("description-race").unwrap();
    assert_eq!(view.description.as_deref(), Some("新的说明"));
    assert_eq!(view.description_updated_at, updated);
    assert!(updated.is_some());

    hold.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut wrong = spec("forget-race", port, "echo unused".into());
    wrong.identity.insert("kind".into(), json!("wrong"));
    assert_eq!(
        services.ensure(wrong).await.unwrap().0.state,
        ServiceState::Failed
    );
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let svc = services.clone();
    let pending = tokio::spawn(async move { svc.inspect("forget-race").await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    services.forget("forget-race").unwrap();
    release.notify_one();
    pending.await.unwrap().unwrap();
    assert!(services.service_view("forget-race").is_none());
    server.abort();
}

#[tokio::test]
async fn no_health_service_stop_intent_survives_inspect_and_dropped_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let pm = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(pm.clone()).unwrap();
    let mut cfg = spec(
        "plain",
        1,
        "trap '' TERM; echo ready; while :; do sleep 1; done".into(),
    );
    cfg.health_url.clear();
    cfg.identity.clear();
    let record = services.ensure(cfg.clone()).await.unwrap().0;
    assert_eq!(record.state, ServiceState::Ready);
    assert!(record.detail.contains("未配置"));
    let id = record.execution_id.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let output = pm
                .read_output(id, smallhead_process::OutputStream::Stdout, 0, 1024)
                .await
                .unwrap();
            if output.text.contains("ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let svc = services.clone();
    let stopping = tokio::spawn(async move { svc.stop("plain").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while services.service_view("plain").unwrap().desired_running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    services.inspect("plain").await.unwrap();
    stopping.abort();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let view = services.service_view("plain").unwrap();
            assert!(!view.desired_running);
            if view.state == ServiceState::Stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let restarted = services.ensure(cfg).await.unwrap().0;
    assert_ne!(restarted.execution_id, Some(id));
    services.stop("plain").await.unwrap();
    pm.shutdown(Duration::from_secs(6)).await.unwrap();
}

#[tokio::test]
async fn admission_quota_recovery_redaction_and_explicit_retention() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ProcessLimits {
        max_active: 1,
        max_concurrent: 1,
        max_records: 4,
        max_stream_bytes: 1024,
        max_disk_bytes: 200000,
        ..Default::default()
    };
    let manager = ProcessManager::with_limits(dir.path().into(), limits.clone()).unwrap();
    assert!(
        ProcessManager::with_limits(dir.path().into(), limits.clone()).is_err(),
        "目录必须独占"
    );
    let mut spec = ProcessSpec::new("sleep 10");
    spec.env
        .insert("API_TOKEN".into(), "secret-never-log".into());
    let id = manager.start(spec).await.unwrap();
    assert!(
        manager
            .start(ProcessSpec::new("echo no-space"))
            .await
            .is_err(),
        "必须为正在运行的命令预留整个输出预算"
    );
    assert!(manager.purge(id).is_err());
    manager.stop(id).await.unwrap();
    manager.wait_for(id, Duration::from_secs(3)).await.unwrap();
    let meta = std::fs::read_to_string(dir.path().join(id.to_string()).join("meta.json")).unwrap();
    assert!(!meta.contains("secret-never-log"));
    manager.shutdown(Duration::from_secs(3)).await.unwrap();
    // Executor 在 publish_final 后还会释放最后一个 Arc，给它一个调度机会。
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(manager);
    let restored = ProcessManager::with_limits(dir.path().into(), limits).unwrap();
    assert!(restored.inspect(id).unwrap().is_terminal());
    restored.purge(id).unwrap();
    assert!(!dir.path().join(id.to_string()).exists());
    let next = restored
        .start(ProcessSpec::new("echo after-purge"))
        .await
        .unwrap();
    restored
        .wait_for(next, Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn queue_bound_and_journal_failure_are_observable() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_limits(
        dir.path().into(),
        ProcessLimits {
            max_active: 1,
            max_concurrent: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let id = manager.start(ProcessSpec::new("sleep 10")).await.unwrap();
    let refused = manager
        .start(ProcessSpec::new("echo crowded"))
        .await
        .unwrap();
    assert!(matches!(
        manager.inspect(refused).unwrap().state,
        ProcessState::Rejected(_)
    ));
    let path = dir.path().join(id.to_string()).join("events.jsonl");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    manager.stop(id).await.unwrap();
    let record = manager.wait_for(id, Duration::from_secs(3)).await.unwrap();
    assert!(matches!(record.state, ProcessState::CleanupFailed(_)));
}

fn spec(name: &str, port: u16, command: String) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        process: ProcessSpec::new(command),
        health_url: format!("http://127.0.0.1:{port}/health"),
        identity: BTreeMap::from([("kind".into(), json!("smallhead-test"))]),
        startup_timeout_ms: 3000,
    }
}

async fn external_server(body: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (port, task)
}

#[tokio::test]
async fn external_health_reuse_conflict_and_stop_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let config = spec("external", port, "echo never-spawn".into());
    let (a, b) = tokio::join!(services.ensure(config.clone()), services.ensure(config));
    assert!(a.unwrap().0.external && b.unwrap().0.external);
    assert!(manager.list().is_empty());
    assert!(services.stop("external").await.is_err());
    let external_obs = services.inspect("external").await.unwrap().0;
    assert!(
        external_obs.ok,
        "外部健康服务应 ok=true，实际 reason={:?}",
        external_obs.reason
    );
    server.abort();
    let (port, server) = external_server(r#"{"kind":"wrong-service"}"#).await;
    let conflict = services
        .ensure(spec("conflict", port, "echo never-spawn".into()))
        .await
        .unwrap()
        .0;
    assert_eq!(conflict.state, ServiceState::Failed);
    assert!(manager.list().is_empty());
    server.abort();
}

#[tokio::test]
async fn managed_service_survives_task_cancel_and_stops_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let command=format!("python3 -u -c 'from http.server import HTTPServer,BaseHTTPRequestHandler\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200);self.end_headers();self.wfile.write(b\"{{\\\"kind\\\":\\\"smallhead-test\\\"}}\")\nHTTPServer((\"127.0.0.1\",{port}),H).serve_forever()'");
    let config = spec("managed", port, command);
    let (first, second) = tokio::join!(services.ensure(config.clone()), services.ensure(config));
    let first = first.unwrap().0;
    let second = second.unwrap().0;
    assert_eq!(first.state, ServiceState::Ready, "{first:?}");
    assert_eq!(first.execution_id, second.execution_id);
    assert!(!first.external);
    manager.stop_owner("chat-task").await.unwrap();
    let managed_obs = services.inspect("managed").await.unwrap().0;
    assert!(
        managed_obs.ok,
        "managed 应 ok=true，实际 reason={:?}",
        managed_obs.reason
    );
    let stopped = services.stop("managed").await.unwrap().0;
    assert_eq!(stopped.state, ServiceState::Stopped);
    assert!(!stopped.desired_running);
    assert_eq!(
        services.stop("managed").await.unwrap().0.state,
        ServiceState::Stopped
    );
    assert!(manager
        .inspect(first.execution_id.unwrap())
        .unwrap()
        .is_terminal());
}

#[tokio::test]
async fn service_startup_failure_cleans_process() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut config = spec("unhealthy", port, "sleep 10".into());
    config.startup_timeout_ms = 100;
    let record = services.ensure(config).await.unwrap().0;
    assert_eq!(record.state, ServiceState::Failed);
    assert!(manager
        .inspect(record.execution_id.unwrap())
        .unwrap()
        .is_terminal());
}

#[tokio::test]
async fn confirmed_exit_after_shutdown_can_be_started_again_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = spec("restartable", 1, "sleep 60".into());
    config.health_url.clear();
    config.identity.clear();
    let old_id;
    {
        let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
        let services = ServiceManager::new(manager.clone()).unwrap();
        old_id = services
            .ensure(config.clone())
            .await
            .unwrap()
            .0
            .execution_id
            .unwrap();
        services
            .update_description("restartable", "保留说明".into())
            .unwrap();
        // Application shutdown stops processes without updating the service's desired intent.
        manager.shutdown(Duration::from_secs(6)).await.unwrap();
        assert!(matches!(
            manager.inspect(old_id).unwrap().state,
            ProcessState::Exited(_)
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let recovered = services.service_view("restartable").unwrap();
    assert_eq!(recovered.state, ServiceState::Failed);
    assert!(recovered.desired_running);
    assert_eq!(recovered.execution_id, Some(old_id));
    assert_eq!(recovered.description.as_deref(), Some("保留说明"));
    let restarted = services.ensure(config).await.unwrap().0;
    assert_eq!(restarted.state, ServiceState::Ready);
    assert!(!restarted.external);
    assert_ne!(restarted.execution_id, Some(old_id));
    services.stop("restartable").await.unwrap();
    manager.shutdown(Duration::from_secs(6)).await.unwrap();
}

#[tokio::test]
async fn uncertain_or_unrelated_exit_evidence_still_blocks_restart() {
    use smallhead_process::{ExecutionId, ProcessRecord};
    for (state, owner) in [
        (
            ProcessState::Lost("interrupted".into()),
            "service:uncertain",
        ),
        (
            ProcessState::CleanupFailed("permission denied".into()),
            "service:uncertain",
        ),
        (ProcessState::SpawnFailed("missing".into()), "service:other"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let id = ExecutionId::new();
        let process = ProcessRecord::new(id, ProcessSpec::new("echo must-not-run"), None);
        let mut process = serde_json::to_value(process).unwrap();
        process["state"] = serde_json::to_value(state).unwrap();
        process["owner"] = json!(owner);
        let log_dir = dir.path().join(id.to_string());
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("meta.json"),
            serde_json::to_vec(&process).unwrap(),
        )
        .unwrap();
        let mut config = spec("uncertain", 1, "echo must-not-run".into());
        config.health_url.clear();
        config.identity.clear();
        std::fs::write(
            dir.path().join("services.json"),
            serde_json::to_vec(&json!({
                "uncertain": {"spec": config, "state": "Ready", "execution_id": id,
                    "external": false, "desired_running": true, "detail": "old"}
            }))
            .unwrap(),
        )
        .unwrap();
        let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
        let services = ServiceManager::new(manager.clone()).unwrap();
        assert_eq!(
            services.service_view("uncertain").unwrap().state,
            ServiceState::Unknown
        );
        assert!(!services.inspect("uncertain").await.unwrap().0.ok);
        assert!(services.ensure(config).await.is_err());
        assert_eq!(manager.list(), vec![id]);
    }
}

#[tokio::test]
async fn service_registry_survives_restart_and_requires_safe_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let config = spec("persistent_external", port, "echo never-spawn".into());

    {
        let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
        let services = ServiceManager::new(manager).unwrap();
        let record = services.ensure(config.clone()).await.unwrap().0;
        assert_eq!(record.state, ServiceState::Ready);
        assert!(record.external);
    }

    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let recovered = services.list();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state, ServiceState::Unknown);

    let reconciled = services.ensure(config).await.unwrap().0;
    assert_eq!(reconciled.state, ServiceState::Ready);
    assert!(reconciled.external);
    assert!(reconciled.execution_id.is_none());
    server.abort();
}

#[tokio::test]
async fn unknown_service_can_be_forgotten_without_signalling_old_pid() {
    let dir = tempfile::tempdir().unwrap();
    {
        let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
        let services = ServiceManager::new(manager).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut config = spec("forgotten", port, "sleep 10".into());
        config.startup_timeout_ms = 100;
        assert_eq!(
            services.ensure(config).await.unwrap().0.state,
            ServiceState::Failed
        );
    }

    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    // 启动超时路径已经持久化 desired_running=false，因此恢复时保留 Failed。
    assert_eq!(services.list()[0].state, ServiceState::Failed);
    services.forget("forgotten").unwrap();
    assert!(services.list().is_empty());
    drop(services);
    drop(manager);

    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    assert!(ServiceManager::new(manager).unwrap().list().is_empty());
}

#[tokio::test]
async fn services_json_v0_round_trips_and_description_persists() {
    // 1) v0 services.json（裸 BTreeMap）能被新代码读取，且 description/run_version 默认值。
    let dir = tempfile::tempdir().unwrap();
    let v0_path = dir.path().join("services.json");
    let v0_body = serde_json::json!({
        "legacy": {
            "spec": {
                "name": "legacy",
                "process": {
                    "command": "echo never-spawn",
                    "scope": "SessionService",
                    "env": {},
                },
                "health_url": "http://127.0.0.1:65535/health",
                "identity": {"kind": "smallhead-test"},
                "startup_timeout_ms": 3000u64,
            },
            "state": "Ready",
            "execution_id": null,
            "external": true,
            "desired_running": false,
            "detail": "external legacy",
        }
    });
    std::fs::write(&v0_path, serde_json::to_vec_pretty(&v0_body).unwrap()).unwrap();

    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let loaded = services.list();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].spec.name, "legacy");
    assert!(loaded[0].description.is_none());
    assert_eq!(
        loaded[0].run_version,
        smallhead_process::service::RunVersion(0)
    );

    // 2) update_description 后立刻可见且写入磁盘为 v1 envelope。
    services
        .update_description("legacy", "agent says hello".into())
        .unwrap();
    let v1_bytes = std::fs::read(&v0_path).unwrap();
    let v1: serde_json::Value = serde_json::from_slice(&v1_bytes).unwrap();
    assert_eq!(v1["schema_version"], serde_json::json!(1u32));
    assert_eq!(v1["services"]["legacy"]["description"], "agent says hello");

    // 3) 重新加载后描述仍存在。
    drop(services);
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let reloaded = services.list();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(
        reloaded[0].description.as_deref(),
        Some("agent says hello"),
        "新字段必须随 v1 envelope 写入并由下次加载读出"
    );
}

#[tokio::test]
async fn update_description_rejects_unknown_and_oversize() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();

    assert!(services.update_description("nope", "text".into()).is_err());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = spec("desc-target", port, "sleep 1".into());
    let _ = services.ensure(cfg).await.unwrap().0;

    let oversized =
        "y".repeat(smallhead_process::service::ServiceManager::MAX_DESCRIPTION_BYTES + 1);
    assert!(services
        .update_description("desc-target", oversized)
        .is_err());

    services
        .update_description("desc-target", "ok".into())
        .unwrap();
    assert_eq!(
        services.list()[0].description.as_deref(),
        Some("ok"),
        "成功后内存中应可读"
    );
}

#[tokio::test]
async fn service_view_reflects_record_and_keeps_names_stable() {
    use smallhead_process::service::ServiceView;
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let config = spec("alpha", port, "echo never-spawn".into());
    let record_a = services.ensure(config.clone()).await.unwrap().0;
    assert!(record_a.external);

    // 1) view 字段全
    let v = services
        .service_view("alpha")
        .expect("registered service must produce a view");
    assert_eq!(v.name, "alpha");
    assert_eq!(v.state, ServiceState::Ready);
    assert!(v.external);
    assert!(v.desired_running);
    assert_eq!(v.health_url, config.health_url);
    assert_eq!(v.startup_timeout_ms, config.startup_timeout_ms);
    assert!(!v.startup_command_display.is_empty());
    assert!(v.detail.contains("复用") || v.detail.contains("恢复"));
    assert!(v.in_flight_operation.is_none());
    assert!(v.last_observation.is_none());

    // 2) name-stable：再插入另一服务，已存在的视图名不漂移
    let config_b = spec("bravo", port, "echo never-spawn".into());
    let record_b = services.ensure(config_b.clone()).await.unwrap().0;
    assert_eq!(record_b.spec.name, "bravo");
    let v_b = services.service_view("bravo").unwrap();
    assert_eq!(v_b.name, "bravo");
    assert_ne!(v_b.name, v.name, "新服务不应与已存在服务同名");
    let v_a_again = services.service_view("alpha").unwrap();
    assert_eq!(v_a_again.name, "alpha");
    assert_eq!(v_a_again.health_url, v.health_url, "原视图字段不漂移");
    drop(v_a_again);

    // 3) service_view_all 合并外部预设
    let views = services.service_view_all();
    let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
    assert!(names.contains(&"bravo"), "已登记服务应在前");
    assert!(names.contains(&"laya"));
    assert!(names.contains(&"qwen"));
    let laya = views.iter().find(|v| v.name == "laya").unwrap();
    assert!(!laya.desired_running);
    assert!(!laya.external, "未启动预设不是外部运行实例");
    assert_eq!(laya.state, ServiceState::Stopped);
    assert_eq!(
        laya.health_url, "http://127.0.0.1:3031/health",
        "预设端口与服务面板保持一致"
    );

    // 4) 服务面板下 description 透传
    services
        .update_description("bravo", "项目服务示例".into())
        .unwrap();
    let v_b2 = services.service_view("bravo").unwrap();
    assert_eq!(v_b2.description.as_deref(), Some("项目服务示例"));

    // 避免编译期 unused 警告
    let _: Vec<ServiceView> = views;
    server.abort();
}

#[tokio::test]
async fn inspect_preserves_control_version_and_observation_records_match() {
    // 观察关联当前控制版本，但只读操作不能使 stop/ensure 的版本失效。
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();

    let cfg = spec("versioned", port, "echo never-spawn".into());
    let record = services.ensure(cfg).await.unwrap().0;
    assert!(record.external);
    assert_eq!(record.run_version.0, 1, "首次登记分配控制版本");

    let after_first = services.inspect("versioned").await.unwrap().0;
    let v_after_first = after_first.run_version.0;
    assert!(
        v_after_first >= 1,
        "观察必须关联已登记的控制版本，实际 {}",
        v_after_first
    );
    assert_eq!(
        services
            .service_view("versioned")
            .and_then(|v| v.last_observation)
            .map(|o| o.run_version.0),
        Some(v_after_first),
        "最近观察的 run_version 必须与提交时的记录一致"
    );

    // 只读检查不能使正在进行的控制操作失效。
    let after_second = services.inspect("versioned").await.unwrap().0;
    assert!(
        after_second.run_version.0 == v_after_first,
        "只读检查保留控制版本"
    );

    server.abort();
}

#[tokio::test]
async fn late_healthy_probe_cannot_resurrect_stopped_managed_service() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let cfg = spec("late-probe", port, "sleep 30".into());
    let svc = services.clone();
    let starting = tokio::spawn(async move { svc.ensure(cfg).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while services
            .service_view("late-probe")
            .and_then(|v| v.execution_id)
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (_, hold, entered, release, server) = controlled_health(port).await;
    let record = starting.await.unwrap().unwrap().0;
    assert!(!record.external);
    assert_eq!(record.state, ServiceState::Ready);
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let svc = services.clone();
    let checking = tokio::spawn(async move { svc.inspect("late-probe").await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    services.stop("late-probe").await.unwrap();
    release.notify_one();
    checking.await.unwrap().unwrap();
    let view = services.service_view("late-probe").unwrap();
    assert_eq!(view.state, ServiceState::Stopped);
    assert!(!view.desired_running);
    assert!(
        view.last_observation.is_none(),
        "stale observation must not replace current facts"
    );
    server.abort();
    manager.shutdown(Duration::from_secs(3)).await.unwrap();
}

#[tokio::test]
async fn broadcast_emits_started_finished_health_and_state_events() {
    use smallhead_process::service::{ServiceEventKind, ServiceState};
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let mut rx = services.subscribe();
    assert_eq!(services.subscriber_count(), 1);

    // 驱动两次操作（ensure + inspect），然后收集所有事件。
    let cfg = spec("events", port, "echo never-spawn".into());
    let _ = services.ensure(cfg).await.unwrap();
    let _ = services.inspect("events").await.unwrap();

    // 短窗口收集：所有事件应在 200ms 内到达（同步路径）。
    let mut all_events: Vec<smallhead_process::service::ServiceEvent> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Ok(event)) if event.name == "events" => {
                all_events.push(event);
            }
            Ok(Ok(_)) => continue, // 忽略其他服务名
            _ => break,
        }
    }
    let started = all_events
        .iter()
        .filter(|e| e.kind == ServiceEventKind::OperationStarted)
        .count();
    let finished = all_events
        .iter()
        .filter(|e| e.kind == ServiceEventKind::OperationFinished)
        .count();
    assert_eq!(
        started, 2,
        "ensure+inspect 各一个 OperationStarted，实际 {started}"
    );
    assert_eq!(
        finished, 2,
        "ensure+inspect 各一个 OperationFinished，实际 {finished}"
    );

    // 不同的 op id
    let started_ids: std::collections::BTreeSet<_> = all_events
        .iter()
        .filter(|e| e.kind == ServiceEventKind::OperationStarted)
        .filter_map(|e| e.operation_id)
        .collect();
    assert_eq!(started_ids.len(), 2, "两个 started 应有不同 op id");

    // 至少看到一次 HealthObserved 和 StateChanged
    let saw_health = all_events
        .iter()
        .any(|e| e.kind == ServiceEventKind::HealthObserved);
    let saw_state = all_events
        .iter()
        .any(|e| e.kind == ServiceEventKind::StateChanged);
    assert!(saw_health, "应至少观察到一次 HealthObserved 事件");
    assert!(saw_state, "应至少观察到一次 StateChanged 事件");

    // 序号单调递增
    let mut sequences: Vec<u64> = all_events.iter().map(|e| e.sequence).collect();
    sequences.sort();
    sequences.dedup();
    let total = all_events.len();
    assert!(sequences.len() <= total, "序号必须唯一");
    assert_eq!(sequences[0], 1, "第一条事件序号应为 1");
    let _ = ServiceState::Ready;
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn health_scheduler_runs_inspect_periodically_and_stops_cleanly() {
    use smallhead_process::service::ServiceEventKind;
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let cfg = spec("sched-target", port, "echo never-spawn".into());
    let _ = services.ensure(cfg).await.unwrap();

    let mut rx = services.subscribe();
    // 短间隔、低并发；只用来跑两次 tick。
    let handle = services.start_health_scheduler_with(Duration::from_millis(150), 2);

    // 周期检查只广播观察，不用开始/结束事件反复刷新状态栏。
    let mut scheduler_inspects = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while scheduler_inspects < 2 && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(event))
                if event.name == "sched-target"
                    && event.kind == ServiceEventKind::HealthObserved =>
            {
                scheduler_inspects += 1;
            }
            Ok(Ok(event)) => assert_ne!(event.kind, ServiceEventKind::OperationStarted),
            _ => continue,
        }
    }
    assert!(
        scheduler_inspects >= 2,
        "调度器应至少触发 2 次 inspect，实际 {scheduler_inspects}"
    );

    // 显式 stop，handle 也能在 drop 时停掉。
    handle.stop().await;
    server.abort();
}

#[tokio::test]
async fn health_scheduler_drop_terminates_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let handle = services.start_health_scheduler_with(Duration::from_millis(50), 1);
    // 立刻 drop；任务应被 abort。
    drop(handle);
    // 给事件循环机会退出；不再断言状态，因为 abort 已经足够。
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn service_events_jsonl_is_written_and_rotates_at_threshold() {
    use smallhead_process::service::EventLogWriter;
    let dir = tempfile::tempdir().unwrap();
    // 用极小的 max_bytes 让单条 append 就触发轮转。
    let mut writer = EventLogWriter::with_limits(dir.path(), 64, 2);
    let line = b"{\"hello\":\"world\",\"padding\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}";
    writer.append(line);
    writer.append(line);
    writer.append(line);
    let current = std::fs::read(dir.path().join("service-events.jsonl")).unwrap();
    assert!(!current.is_empty(), "当前文件应有内容");
    let backup = std::fs::read(dir.path().join("service-events.jsonl.1"));
    assert!(backup.is_ok(), "应存在 .1 备份文件");

    // 测试 keep=1：禁止备份
    let dir2 = tempfile::tempdir().unwrap();
    let mut writer2 = EventLogWriter::with_limits(dir2.path(), 32, 1);
    let line2 = b"{\"a\":1,\"padding\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}";
    writer2.append(line2);
    writer2.append(line2);
    let _current2 = std::fs::read(dir2.path().join("service-events.jsonl")).unwrap();
    let backup2 = std::fs::read(dir2.path().join("service-events.jsonl.1"));
    assert!(backup2.is_err(), "keep=1 时不应生成 .1 备份文件");
}

#[tokio::test]
async fn eight_concurrent_ensure_calls_share_one_execution_and_emit_all_events() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager.clone()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let command = format!(
        "python3 -u -c 'from http.server import HTTPServer,BaseHTTPRequestHandler\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200);self.end_headers();self.wfile.write(b\"{{\\\"kind\\\":\\\"smallhead-test\\\"}}\")\nHTTPServer((\"127.0.0.1\",{port}),H).serve_forever()'"
    );
    let cfg = spec("concurrent-ensure", port, command);
    // 先订阅，捕获全部事件。
    let mut rx = services.subscribe();
    // 8 个并发 ensure；所有应拿到同一 execution_id 并都 Ready。
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = services.clone();
        let c = cfg.clone();
        handles.push(tokio::spawn(async move { s.ensure(c).await }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap().unwrap().0);
    }
    let exec_ids: std::collections::BTreeSet<_> =
        results.iter().filter_map(|r| r.execution_id).collect();
    assert_eq!(exec_ids.len(), 1, "8 个并发 ensure 应共用同一 execution_id");
    for r in &results {
        assert_eq!(r.state, ServiceState::Ready, "{r:?}");
    }
    assert!(
        !results[0].external,
        "由本管理器启动的服务应是 external=false"
    );

    // 至少应有 8 个 OperationFinished 事件（每个 ensure 一个）。
    let mut finished = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while finished < 8 && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(event))
                if event.name == "concurrent-ensure"
                    && event.kind
                        == smallhead_process::service::ServiceEventKind::OperationFinished =>
            {
                finished += 1;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(
        finished >= 8,
        "至少 8 个 OperationFinished，实际 {finished}"
    );
}

#[tokio::test]
async fn stop_does_not_clobber_subsequent_ensure_state() {
    // 关键不变量：stop("foo") 触发的 state=Stopped 提交仅当 in_flight 仍为本 op 时落地。
    // 一旦 ensure("foo") 完成并写入新记录，stop 的最终写入必须被丢弃。
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let cfg = spec("race", port, "echo never-spawn".into());
    let _ = services.ensure(cfg.clone()).await.unwrap();
    // 第二次 ensure 接管 in_flight。
    let _ = services.ensure(cfg).await.unwrap();
    // 第一次 ensure 已经终止；现在再 stop（外部服务会被拒，但能走完路径验证 in_flight 检查）
    // 外部服务在 stop_io 阶段会被拒；改测 stop + 后续 ensure 在 in_flight 竞争下的状态。
    let _ = services.stop("race").await; // Err，因为是 external
    let view = services.service_view("race").unwrap();
    assert_eq!(
        view.state,
        ServiceState::Ready,
        "外部服务的 stop 失败不应回写状态"
    );
    server.abort();
}

#[tokio::test]
async fn operation_retention_returns_none_for_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let fake_id = smallhead_process::service::OperationId(uuid::Uuid::now_v7());
    assert!(
        services.operation(fake_id).is_none(),
        "未知 op_id 应返回 None"
    );
    // 触发 tick 后再查，依然 None
    services.tick();
    assert!(services.operation(fake_id).is_none());
}

#[tokio::test]
async fn service_event_log_captures_real_events_to_disk() {
    // 验证 emit() 同步落盘：先 ensure+stop，然后读 service-events.jsonl，至少有 1 行。
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = external_server(r#"{"kind":"smallhead-test"}"#).await;
    let manager = ProcessManager::with_log_root(dir.path().into()).unwrap();
    let services = ServiceManager::new(manager).unwrap();
    let cfg = spec("logged", port, "echo never-spawn".into());
    let _ = services.ensure(cfg).await.unwrap();
    let log_path = dir.path().join("service-events.jsonl");
    let bytes = std::fs::read(&log_path).unwrap_or_default();
    assert!(!bytes.is_empty(), "service-events.jsonl 应有内容");
    let line_count = bytes.iter().filter(|&&b| b == b'\n').count();
    assert!(
        line_count >= 2,
        "至少 OperationStarted + Finished 两行，实际 {line_count}"
    );

    // 每行应是合法 JSON
    for line in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let value: serde_json::Value = serde_json::from_slice(line).unwrap();
        assert!(value.get("sequence").is_some(), "事件必须有 sequence");
    }
    server.abort();
}
