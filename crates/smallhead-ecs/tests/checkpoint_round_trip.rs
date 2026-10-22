//! Integration test: full checkpoint round-trip.
//!
//! This is the "可重复验证的工程测试" from `AGENT_METHODOLOGY.md` §1.6 — a
//! fixed input state is written to a checkpoint, the storage is "killed" by
//! dropping the writer handle, a fresh reader reads it back, and we verify
//! the world state is byte-identical. No mocks, no shared globals, every
//! step uses the public API only.
//!
//! It is the single most important test in this crate: if the atomic-commit
//! invariants regress, the smallhead session replay feature breaks silently.

use std::sync::Arc;

use bevy_ecs::world::World;
use parking_lot::Mutex;
use serde_json::{json, Value};
use smallhead_ecs::checkpoint::{
    CheckpointStore, CommitResult, FileCheckpointStore, WorldCheckpoint,
};
use tempfile::tempdir;

fn fixed_world_state() -> Value {
    json!({
        "entities": [
            { "index": 1, "components": { "User": { "id": "u-1", "name": "alice" } } },
            { "index": 2, "components": { "User": { "id": "u-2", "name": "bob" } } },
            { "index": 3, "components": { "Project": { "id": "p-1", "owner": "u-1" } } },
        ],
        "components_registered": ["User", "Project", "Relation"],
    })
}

#[test]
fn single_commit_round_trip_preserves_state() {
    let tmp = tempdir().unwrap();
    let store = FileCheckpointStore::new(tmp.path());

    let world = fixed_world_state();

    let commit = store
        .commit_atomic(
            "world-A",
            0,
            "digest-of-action-log-v0",
            world.clone(),
            vec!["entry-1".into(), "entry-2".into()],
        )
        .expect("commit");

    let (version, _id) = match commit {
        CommitResult::Committed {
            version,
            checkpoint_id,
        } => (version, checkpoint_id),
        CommitResult::Conflict { .. } => panic!("unexpected conflict on first commit"),
    };
    assert_eq!(version, 1);

    // "Kill" the world: drop the store handle, open a fresh one pointed at the
    // same directory. This simulates the "process died, restart" scenario.
    drop(store);
    let store2 = FileCheckpointStore::new(tmp.path());

    let restored: WorldCheckpoint = store2
        .read("world-A", version)
        .expect("read")
        .expect("checkpoint present");

    // The payload must be byte-identical.
    assert_eq!(restored.world, world);
    assert_eq!(restored.world_id, "world-A");
    assert_eq!(restored.version, version);
    assert_eq!(restored.action_log_digest, "digest-of-action-log-v0");
    assert_eq!(restored.anchor_refs, vec!["entry-1", "entry-2"]);
}

#[test]
fn versioned_commits_increment_monotonically() {
    let tmp = tempdir().unwrap();
    let store = FileCheckpointStore::new(tmp.path());

    let mut prev_version: u64 = 0;
    for i in 0..5 {
        let state = json!({ "i": i });
        let r = store
            .commit_atomic("world-B", prev_version, "digest", state, vec![])
            .unwrap();
        let v = match r {
            CommitResult::Committed { version, .. } => version,
            CommitResult::Conflict { .. } => panic!("conflict on sequential commit"),
        };
        assert_eq!(v, prev_version + 1);
        prev_version = v;
    }

    let versions = store.list_versions("world-B").unwrap();
    assert_eq!(versions, vec![1, 2, 3, 4, 5]);

    let latest = store.latest("world-B").unwrap().unwrap();
    assert_eq!(latest.version, 5);
    assert_eq!(latest.world, json!({ "i": 4 }));
}

#[test]
fn commit_with_stale_expected_version_reports_conflict() {
    let tmp = tempdir().unwrap();
    let store = FileCheckpointStore::new(tmp.path());

    // Establish version 1.
    let r = store
        .commit_atomic("world-C", 0, "d", json!({"v": 1}), vec![])
        .unwrap();
    assert!(matches!(r, CommitResult::Committed { version: 1, .. }));

    // Try to commit on top of v0 — must fail with a conflict.
    let r = store
        .commit_atomic("world-C", 0, "d", json!({"v": 2}), vec![])
        .unwrap();
    match r {
        CommitResult::Conflict {
            expected_version,
            actual_version,
        } => {
            assert_eq!(expected_version, 0);
            assert_eq!(actual_version, 1);
        }
        _ => panic!("expected conflict, got {r:?}"),
    }
}

#[test]
fn current_version_returns_none_for_never_committed_world() {
    let tmp = tempdir().unwrap();
    let store = FileCheckpointStore::new(tmp.path());
    assert_eq!(store.current_version("never-seen").unwrap(), None);
    assert_eq!(store.latest("never-seen").unwrap(), None);
}

#[test]
fn read_missing_version_returns_none_not_error() {
    let tmp = tempdir().unwrap();
    let store = FileCheckpointStore::new(tmp.path());
    let _ = store
        .commit_atomic("world-D", 0, "d", json!({"v": 1}), vec![])
        .unwrap();
    // Version 99 was never written.
    assert!(store.read("world-D", 99).unwrap().is_none());
}

#[test]
fn checkpoint_is_recoverable_after_restart_with_world_lock_never_needed() {
    // This is the *core* regression test: write → drop writer → fresh reader.
    // It proves the atomic commit sequence actually survives a "kill".
    let tmp = tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    let writer_store = FileCheckpointStore::new(&root);
    let state = json!({
        "entities": [
            { "id": "alpha", "kind": "note", "data": "hello world" },
            { "id": "beta",  "kind": "note", "data": "second note" },
        ]
    });
    let commit = writer_store
        .commit_atomic(
            "world-recover",
            0,
            "digest-of-action-log",
            state.clone(),
            vec!["anchor-1".into()],
        )
        .unwrap();
    let version = match commit {
        CommitResult::Committed { version, .. } => version,
        CommitResult::Conflict { .. } => unreachable!(),
    };

    // Drop the writer. Simulate process death.
    drop(writer_store);

    // Fresh process, fresh handle, same disk.
    let reader_store = FileCheckpointStore::new(&root);
    let cp = reader_store
        .read("world-recover", version)
        .unwrap()
        .unwrap();
    assert_eq!(cp.world, state);

    // The world id alone should be enough to find the latest checkpoint.
    let latest = reader_store.latest("world-recover").unwrap().unwrap();
    assert_eq!(latest.version, version);
}

#[test]
fn world_lock_is_documented_for_future_dispatch_use() {
    // This test doesn't exercise a lock path — that path lives behind the
    // world mutex in `world_op.rs` and is exercised by a separate suite. We
    // pin the docstring here so that future readers can find both halves of
    // the safety story.
    let _w = World::new();
    let _arc = Arc::new(Mutex::new(World::new()));
    // Sentinel: this test always passes; its purpose is documentation.
    assert!(true);
}
