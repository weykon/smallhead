//! 资源限制与排队控制的真实执行验收。
use smallhead_process::{
    ExitReason, OutputStream, ProcessLimits, ProcessManager, ProcessSpec, ProcessState,
};
use std::time::Duration;

fn manager(limits: ProcessLimits) -> (tempfile::TempDir, ProcessManager) {
    let dir = tempfile::tempdir().unwrap();
    let manager = ProcessManager::with_limits(dir.path().into(), limits).unwrap();
    (dir, manager)
}

#[tokio::test]
async fn queued_stop_does_not_spawn_and_slot_is_reused() {
    let (dir, manager) = manager(ProcessLimits {
        max_concurrent: 1,
        ..Default::default()
    });
    let first = manager.start(ProcessSpec::new("sleep 10")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.inspect(first).unwrap().pid.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let second = manager
        .start(ProcessSpec::new("echo should-not-run"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(manager.inspect(second).unwrap().state, ProcessState::Queued);
    manager.stop(second).await.unwrap();
    let record = manager
        .wait_for(second, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(record.pid.is_none());
    assert!(
        matches!(record.state, ProcessState::Exited(ref info) if info.reason == ExitReason::Cancelled)
    );
    assert_eq!(
        std::fs::metadata(dir.path().join(second.to_string()).join("stdout.log"))
            .unwrap()
            .len(),
        0
    );
    manager.stop(first).await.unwrap();
    manager
        .wait_for(first, Duration::from_secs(2))
        .await
        .unwrap();
    let third = manager.start(ProcessSpec::new("echo next")).await.unwrap();
    assert!(
        matches!(manager.wait_for(third, Duration::from_secs(2)).await.unwrap().state,
        ProcessState::Exited(ref info) if info.exit_code == Some(0))
    );
}

#[tokio::test]
async fn output_budget_stops_both_streams_and_preserves_bounded_bytes() {
    let (dir, manager) = manager(ProcessLimits {
        max_stream_bytes: 1024,
        ..Default::default()
    });
    let id = manager
        .start(ProcessSpec::new(
            "while :; do echo abcdefghijklmnopqrstuvwxyz; echo error >&2; done",
        ))
        .await
        .unwrap();
    let record = manager.wait_for(id, Duration::from_secs(3)).await.unwrap();
    assert!(
        matches!(record.state, ProcessState::Exited(ref info) if info.reason == ExitReason::OutputLimit),
        "{:?}",
        record.state
    );
    for stream in ["stdout.log", "stderr.log"] {
        assert!(
            std::fs::metadata(dir.path().join(id.to_string()).join(stream))
                .unwrap()
                .len()
                <= 1024
        );
    }
    let page = manager
        .read_output(id, OutputStream::Stdout, 0, 17)
        .await
        .unwrap();
    assert!(page.bytes.len() <= 17);
    assert!(page.complete && page.output_limited);
    let next = manager
        .read_output(id, OutputStream::Stdout, page.next_offset, 1024)
        .await
        .unwrap();
    assert!(next.eof);
    assert_eq!(next.next_offset, next.stored_bytes);
}

#[tokio::test]
async fn invalid_utf8_is_preserved_and_read_size_is_validated() {
    let (_dir, manager) = manager(ProcessLimits::default());
    let id = manager
        .start(ProcessSpec::new("printf '\\377abc'"))
        .await
        .unwrap();
    manager.wait_for(id, Duration::from_secs(2)).await.unwrap();
    let page = manager
        .read_output(id, OutputStream::Stdout, 0, 64)
        .await
        .unwrap();
    assert_eq!(page.bytes, b"\xffabc");
    assert_eq!(page.text, "\u{fffd}abc");
    assert!(manager
        .read_output(id, OutputStream::Stdout, 0, 0)
        .await
        .is_err());
    assert!(manager
        .read_output(id, OutputStream::Stdout, 0, 65537)
        .await
        .is_err());
}

#[tokio::test]
async fn default_deadline_and_shutdown_admission_are_enforced() {
    let (_dir, manager) = manager(ProcessLimits {
        default_timeout: Duration::from_millis(50),
        ..Default::default()
    });
    let id = manager.start(ProcessSpec::new("sleep 10")).await.unwrap();
    let record = manager.wait_for(id, Duration::from_secs(2)).await.unwrap();
    assert!(
        matches!(record.state, ProcessState::Exited(ref info) if info.reason == ExitReason::Timeout)
    );
    let over = manager
        .start(ProcessSpec::new("true").with_timeout(Duration::from_secs(601)))
        .await
        .unwrap();
    assert!(matches!(
        manager.inspect(over).unwrap().state,
        ProcessState::Rejected(_)
    ));
    manager.shutdown(Duration::from_secs(2)).await.unwrap();
    let late = manager.start(ProcessSpec::new("echo late")).await.unwrap();
    let record = manager
        .wait_for(late, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(record.state, ProcessState::Rejected(_)));
    assert!(record.pid.is_none());
}

#[tokio::test]
async fn shutdown_cleans_running_and_queued_executions() {
    let (_dir, manager) = manager(ProcessLimits {
        max_concurrent: 1,
        ..Default::default()
    });
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(manager.start(ProcessSpec::new("sleep 10")).await.unwrap());
    }
    manager.shutdown(Duration::from_secs(3)).await.unwrap();
    for id in ids {
        assert!(manager.inspect(id).unwrap().is_terminal());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn natural_exit_racing_stop_keeps_consistent_terminal_audit() {
    let (dir, manager) = manager(ProcessLimits::default());
    for index in 0..30 {
        let id = manager
            .start(ProcessSpec::new("sleep 0.01; exit 7"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(index % 20)).await;
        manager.stop(id).await.unwrap();
        let record = manager.wait_for(id, Duration::from_secs(2)).await.unwrap();
        match &record.state {
            ProcessState::Exited(info) => {
                assert!(matches!(
                    info.reason,
                    ExitReason::Natural | ExitReason::Cancelled
                ));
                if info.reason == ExitReason::Natural {
                    assert_eq!(info.exit_code, Some(7));
                }
            }
            state => panic!("退出/停止竞态不应破坏清理：{state:?}"),
        }
        let persisted: smallhead_process::ProcessRecord = serde_json::from_slice(
            &std::fs::read(dir.path().join(id.to_string()).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.state, record.state);
        manager.stop(id).await.unwrap();
        assert_eq!(manager.inspect(id).unwrap().state, record.state);
    }
}
