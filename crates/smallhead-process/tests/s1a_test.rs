//! S1a integration tests — public API contract verification.
//!
//! Original five scenarios. Additional real spawn failure, refusal, dropped
//! caller and process-group cases are in lifecycle_regression.rs.
//!
//! These tests use the public API exclusively. They construct a
//! [`ProcessManager`] with a `tempfile::TempDir` root, drive executions through
//! the 5 required scenarios, and assert on observable state + log persistence.
//!
//! Run with: `cargo test -p smallhead-process --test s1a_test`.

use smallhead_process::{
    ExecutionScope, ExitReason, ProcessManager, ProcessRecord, ProcessSpec, ProcessState,
};
use std::time::Duration;
use tempfile::TempDir;

/// Minimal spec for tests — `ExecutionScope::TaskBound`, no cwd/env/timeout.
fn spec(command: &str) -> ProcessSpec {
    ProcessSpec::new(command)
}

/// Spec with a deadline — used by the timeout / cancel scenarios.
fn spec_with_timeout(command: &str, ms: u64) -> ProcessSpec {
    spec(command).with_timeout(Duration::from_millis(ms))
}

/// Hermetic manager rooted at a fresh temp dir. The [`TempDir`] is returned so
/// tests can independently inspect the on-disk layout.
fn mgr() -> (TempDir, ProcessManager) {
    let dir = TempDir::new().expect("tempdir");
    let m = ProcessManager::with_log_root(dir.path().to_path_buf()).expect("manager");
    (dir, m)
}

/// Convenience: extract `&ProcessExitInfo` from a terminal record, panicking
/// otherwise. Lets the scenarios assert on `exit_code` / `reason` / `duration`
/// without rebuilding the match arms in every test.
fn exit_info(rec: &ProcessRecord) -> &smallhead_process::ProcessExitInfo {
    match &rec.state {
        ProcessState::Exited(info) => info,
        other => panic!("expected Exited, got {other:?}"),
    }
}

// ============================================================================
// §9 S1 — scenario 01: success path. A trivial `echo` reaches `Exited(_, Natural)`
// with `exit_code = Some(0)`, and its stdout appears in `stdout.log`.
// ============================================================================
#[tokio::test]
async fn s1a_01_success_echo() {
    let (dir, m) = mgr();
    let id = m.start(spec("echo s1a-ok")).await.expect("start");
    let rec = m.wait_for(id, Duration::from_secs(5)).await.expect("wait");
    let info = exit_info(&rec);
    assert_eq!(info.reason, ExitReason::Natural);
    assert_eq!(info.exit_code, Some(0));
    assert_eq!(info.signal, None);

    let stdout = tokio::fs::read_to_string(dir.path().join(id.to_string()).join("stdout.log"))
        .await
        .expect("read stdout.log");
    assert!(
        stdout.contains("s1a-ok"),
        "stdout should contain marker, got: {stdout:?}"
    );
}

// ============================================================================
// §9 S1 — scenario 02: non-zero exit code is preserved verbatim. `sh -c 'exit
// 7'` reaches `Exited(_, Natural, exit_code = Some(7))`.
// ============================================================================
#[tokio::test]
async fn s1a_02_nonzero_exit() {
    let (_dir, m) = mgr();
    let id = m.start(spec("sh -c 'exit 7'")).await.expect("start");
    let rec = m.wait_for(id, Duration::from_secs(5)).await.expect("wait");
    let info = exit_info(&rec);
    assert_eq!(info.reason, ExitReason::Natural);
    assert_eq!(info.exit_code, Some(7), "expected exit 7");
    assert!(info.duration < Duration::from_secs(2));
}

// ============================================================================
// §9 S1 — scenario 03: timeout actually kills the child. `sleep 30` with a
// 300ms deadline must reach `Exited(_, Timeout)` long before sleep would
// naturally exit, proving the executor's `start_kill` + `wait` reaping fires.
// ============================================================================
#[tokio::test]
async fn s1a_03_timeout_kills_child() {
    let (_dir, m) = mgr();
    let id = m
        .start(spec_with_timeout("sleep 30", 300))
        .await
        .expect("start");
    let start = std::time::Instant::now();
    let rec = m.wait_for(id, Duration::from_secs(5)).await.expect("wait");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "should not wait full deadline, took {elapsed:?}"
    );
    let info = exit_info(&rec);
    assert_eq!(
        info.reason,
        ExitReason::Timeout,
        "expected Timeout, got {:?}",
        info.reason
    );
    assert!(
        info.duration < Duration::from_secs(2),
        "duration should be ~300ms, got {:?}",
        info.duration
    );
}

// ============================================================================
// §9 S1 — scenario 04: `stop` is idempotent and yields a terminal record. We
// require `Cancelled`: an immediate explicit stop must not wait for the
// 30-second execution deadline or report timeout instead.
// ============================================================================
#[tokio::test]
async fn s1a_04_repeat_stop_idempotent() {
    let (_dir, m) = mgr();
    // The long deadline makes a lost stop notification visible as a failure.
    let id = m
        .start(spec_with_timeout("sleep 30", 30_000))
        .await
        .expect("start");
    m.stop(id).await.expect("first stop");
    m.stop(id).await.expect("second stop (idempotent)");
    let rec = m.wait_for(id, Duration::from_secs(5)).await.expect("wait");
    assert!(
        rec.state.is_terminal(),
        "expected terminal state, got {:?}",
        rec.state
    );
    let info = exit_info(&rec);
    assert!(
        matches!(info.reason, ExitReason::Cancelled),
        "expected Cancelled, got {:?}",
        info.reason
    );
    assert!(
        info.duration < Duration::from_secs(2),
        "stop should be fast, duration = {:?}",
        info.duration
    );
}

// ============================================================================
// §9 S1 — scenario 05: log isolation per execution. Two parallel starts get
// unique ExecutionIds, each with its own subdirectory and `meta.json`, and
// both are inspectable via [`ProcessManager::inspect`].
// ============================================================================
#[tokio::test]
async fn s1a_05_log_dir_isolated_and_queryable() {
    let (dir, m) = mgr();
    let id_a = m.start(spec("echo first")).await.expect("start a");
    let id_b = m.start(spec("echo second")).await.expect("start b");
    assert_ne!(id_a, id_b, "ExecutionIds must be unique");

    let _ = m.wait_for(id_a, Duration::from_secs(5)).await.unwrap();
    let _ = m.wait_for(id_b, Duration::from_secs(5)).await.unwrap();

    // On-disk layout: each id has its own subdirectory + meta.json.
    assert!(dir.path().join(id_a.to_string()).is_dir());
    assert!(dir.path().join(id_b.to_string()).is_dir());
    assert!(dir
        .path()
        .join(id_a.to_string())
        .join("meta.json")
        .is_file());
    assert!(dir
        .path()
        .join(id_b.to_string())
        .join("meta.json")
        .is_file());

    // Both inspectable via the manager.
    let rec_a = m.inspect(id_a).expect("inspect a");
    let rec_b = m.inspect(id_b).expect("inspect b");
    assert_eq!(rec_a.id, id_a);
    assert_eq!(rec_b.id, id_b);
    assert_eq!(rec_a.spec.command, "echo first");
    assert_eq!(rec_b.spec.command, "echo second");

    // Both should be on `TaskBound` scope — that's the only scope S1a honours.
    assert_eq!(rec_a.spec.scope, ExecutionScope::TaskBound);
    assert_eq!(rec_b.spec.scope, ExecutionScope::TaskBound);

    // And `list()` exposes both ids.
    let mut listed = m.list();
    listed.sort();
    let mut expected = vec![id_a, id_b];
    expected.sort();
    assert_eq!(listed, expected);
}
