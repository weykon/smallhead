//! Regression tests exercise real OS children and the public manager contract.
use smallhead_process::{ExecutionScope, ExitReason, ProcessManager, ProcessSpec, ProcessState};
use std::time::Duration;

fn manager() -> (tempfile::TempDir, ProcessManager) {
    let dir = tempfile::tempdir().unwrap();
    let mgr = ProcessManager::with_log_root(dir.path().into()).unwrap();
    (dir, mgr)
}

#[tokio::test]
async fn refusals_have_ids_and_persisted_records() {
    let (dir, mgr) = manager();
    let mut scope = ProcessSpec::new("true");
    scope.scope = ExecutionScope::SessionService;
    for spec in [ProcessSpec::new(""), ProcessSpec::new("sudo true"), scope] {
        let id = mgr.start(spec).await.unwrap();
        let record = mgr.wait_for(id, Duration::from_secs(1)).await.unwrap();
        assert!(matches!(record.state, ProcessState::Rejected(_)));
        assert!(record.pid.is_none());
        let saved = tokio::fs::read(dir.path().join(id.to_string()).join("meta.json"))
            .await
            .unwrap();
        let disk: smallhead_process::ProcessRecord = serde_json::from_slice(&saved).unwrap();
        assert_eq!(disk.state, record.state);
    }
}

#[tokio::test]
async fn download_program_names_are_not_rejected() {
    let (_dir, mgr) = manager();
    let id = mgr
        .start(ProcessSpec::new("printf 'curl wget'"))
        .await
        .unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
    assert!(
        matches!(record.state, ProcessState::Exited(_)),
        "{:?}",
        record.state
    );
}

#[tokio::test]
async fn actual_spawn_failure_has_meta_before_terminal_is_visible() {
    let (dir, mgr) = manager();
    let mut spec = ProcessSpec::new("true");
    spec.cwd = Some(dir.path().join("does-not-exist"));
    let id = mgr.start(spec).await.unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
    assert!(matches!(record.state, ProcessState::SpawnFailed(_)));
    let disk: smallhead_process::ProcessRecord = serde_json::from_slice(
        &tokio::fs::read(dir.path().join(id.to_string()).join("meta.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(disk.state, record.state);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_waiters_never_miss_fast_terminal_transition() {
    let (_dir, mgr) = manager();
    for _ in 0..25 {
        let id = mgr.start(ProcessSpec::new("true")).await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let mgr = mgr.clone();
            tasks.push(tokio::spawn(async move {
                mgr.wait_for(id, Duration::from_secs(2)).await.unwrap()
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_terminal());
        }
    }
}

#[tokio::test]
async fn dropped_waiter_does_not_lose_process_ownership() {
    let (_dir, mgr) = manager();
    let id = mgr.start(ProcessSpec::new("sleep 10")).await.unwrap();
    let owner = mgr.clone();
    let waiter = tokio::spawn(async move { owner.wait_for(id, Duration::from_secs(20)).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    waiter.abort();
    let _ = waiter.await;
    assert!(!mgr.inspect(id).unwrap().is_terminal());
    mgr.stop(id).await.unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(3)).await.unwrap();
    let ProcessState::Exited(info) = record.state else {
        panic!("{:?}", record.state);
    };
    assert_eq!(info.reason, ExitReason::Cancelled);
    assert!(info.signal.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_stops_pipeline_and_term_resistant_writer() {
    let (dir, mgr) = manager();
    let mut spec = ProcessSpec::new(
        "(trap '' TERM; while :; do echo tick >> \"$HEARTBEAT\"; sleep 0.02; done) | cat",
    );
    spec.env.insert(
        "HEARTBEAT".into(),
        dir.path().join("heartbeat").to_string_lossy().into(),
    );
    spec.timeout = Some(Duration::from_millis(200));
    let id = mgr.start(spec).await.unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(4)).await.unwrap();
    let ProcessState::Exited(info) = record.state else {
        panic!("{:?}", record.state);
    };
    assert_eq!(info.reason, ExitReason::Timeout);
    let before = tokio::fs::read(dir.path().join("heartbeat")).await.unwrap();
    assert!(!before.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        before,
        tokio::fs::read(dir.path().join("heartbeat")).await.unwrap()
    );
}

#[tokio::test]
async fn meta_write_failure_is_reported_and_shutdown_reaps_active_children() {
    let (dir, mgr) = manager();
    let id = mgr.start(ProcessSpec::new("sleep 0.1")).await.unwrap();
    let meta = dir.path().join(id.to_string()).join("meta.json");
    tokio::fs::remove_file(&meta).await.unwrap();
    tokio::fs::create_dir(&meta).await.unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(3)).await.unwrap();
    assert!(matches!(record.state, ProcessState::CleanupFailed(_)));
    let active = mgr.start(ProcessSpec::new("sleep 10")).await.unwrap();
    mgr.shutdown(Duration::from_secs(3)).await.unwrap();
    assert!(mgr.inspect(active).unwrap().is_terminal());
}

#[cfg(unix)]
#[tokio::test]
async fn natural_shell_exit_cleans_remaining_group_members() {
    let (dir, mgr) = manager();
    let mut spec = ProcessSpec::new(
        "(while :; do echo tick >> \"$HEARTBEAT\"; sleep 0.02; done) & sleep 0.1; exit 7",
    );
    spec.env.insert(
        "HEARTBEAT".into(),
        dir.path().join("heartbeat").to_string_lossy().into(),
    );
    spec.timeout = Some(Duration::from_secs(3));
    let id = mgr.start(spec).await.unwrap();
    let record = mgr.wait_for(id, Duration::from_secs(4)).await.unwrap();
    let ProcessState::Exited(info) = record.state else {
        panic!("{:?}", record.state);
    };
    assert_eq!(info.reason, ExitReason::Natural);
    assert_eq!(info.exit_code, Some(7));
    let before = tokio::fs::read(dir.path().join("heartbeat")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        before,
        tokio::fs::read(dir.path().join("heartbeat")).await.unwrap()
    );
}
