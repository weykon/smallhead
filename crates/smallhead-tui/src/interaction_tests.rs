//! Integration regressions for App input, completion, and durable review state.
use super::*;
use crate::spec_review::SpecStore;
use smallhead_core::{message::ToolCall, MonadId, ResourceUsage, ResultSlot};
use std::time::Duration;

fn response_event(event: &str, data: serde_json::Value) -> DiagnosticEvent {
    DiagnosticEvent {
        timestamp_ms: 0,
        layer: "response".into(),
        event: event.into(),
        data,
    }
}

#[tokio::test]
async fn steering_keeps_partial_response_before_followup_without_committing_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    app.history.clear();
    app.note("你", "First request");
    app.apply(response_event(
        "provider_request",
        serde_json::json!({"iteration":1}),
    ));
    app.apply(response_event(
        "content_block_start",
        serde_json::json!({"block":{"type":"text","text":"Checking "}}),
    ));
    app.note("你", "Second request");
    app.apply(response_event(
        "content_block_delta",
        serde_json::json!({"delta":"the repository"}),
    ));
    app.apply(response_event(
        "steering_drained",
        serde_json::json!({"id":1}),
    ));
    app.apply(response_event(
        "steering_drained",
        serde_json::json!({"id":2}),
    ));
    app.apply(response_event(
        "provider_request",
        serde_json::json!({"iteration":2}),
    ));
    app.apply(response_event("assistant_complete", serde_json::json!({"message":{"content":[{"type":"text","text":"Continuing with the new request"}]}})));
    assert_eq!(app.history.len(), 4);
    assert_eq!(app.history[1].role, "助手 · 已中断");
    assert_eq!(app.history[1].text, "Checking the repository");
    assert_eq!(app.history[2].text, "Second request");
    assert_eq!(app.history[3].text, "Continuing with the new request");
    assert!(app.stream_text.is_empty());
    assert!(app.messages.is_empty());
}

#[tokio::test]
async fn completed_response_replaces_preview_once_before_followup() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    app.history.clear();
    app.note("你", "First request");
    app.apply(response_event(
        "provider_request",
        serde_json::json!({"iteration":1}),
    ));
    app.apply(response_event(
        "content_block_start",
        serde_json::json!({"block":{"type":"text","text":"Preview"}}),
    ));
    app.note("你", "Followup");
    app.apply(response_event(
        "assistant_complete",
        serde_json::json!({"message":{"content":[{"type":"text","text":"Complete answer"}]}}),
    ));
    app.apply(response_event(
        "steering_drained",
        serde_json::json!({"id":1}),
    ));
    app.apply(response_event(
        "provider_request",
        serde_json::json!({"iteration":2}),
    ));
    assert_eq!(app.history.len(), 3);
    assert_eq!(app.history[1].role, "助手");
    assert_eq!(app.history[1].text, "Complete answer");
    assert_eq!(app.history[2].text, "Followup");
    assert!(app.stream_text.is_empty());
}

#[tokio::test]
async fn checkpoint_failure_stays_dirty_and_retries_after_directory_repair() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let root = dir.path().join("blocked-artifacts");
    std::fs::write(&root, "not a directory").unwrap();
    app.artifact_root = Some(root.clone());
    app.steering.seed_history(tool_history());
    app.steering
        .push(
            vec![ContentBlock::Text {
                text: "Keep pending".into(),
            }],
            1234,
        )
        .unwrap();
    app.recovery_interrupted = true;
    let snapshot = app.steering.latest_snapshot();
    let acknowledged = app.checkpoint_generation;
    let before_failure = Instant::now();
    app.tick().await;
    assert_eq!(app.checkpoint_generation, acknowledged);
    assert!(app.checkpoint_error.is_some());
    assert!(app
        .steering
        .snapshot_if_changed(app.checkpoint_generation)
        .is_some());
    let retry_at = app.checkpoint_retry_at;
    let errors = app
        .history
        .iter()
        .filter(|entry| entry.role == "错误")
        .count();
    // Keep the throttle deterministic even on slow CI machines.
    app.checkpoint_retry_at = Instant::now() + Duration::from_secs(60);
    app.tick().await;
    assert_eq!(app.checkpoint_generation, acknowledged);
    assert_eq!(
        app.history
            .iter()
            .filter(|entry| entry.role == "错误")
            .count(),
        errors
    );
    assert!(retry_at.duration_since(before_failure) >= Duration::from_secs(1));
    std::fs::remove_file(&root).unwrap();
    std::fs::create_dir(&root).unwrap();
    app.checkpoint_retry_at = Instant::now();
    app.tick().await;
    assert_eq!(app.checkpoint_generation, snapshot.generation);
    assert!(app.checkpoint_error.is_none());
    assert!(app
        .steering
        .snapshot_if_changed(app.checkpoint_generation)
        .is_none());
    let saved = crate::turn_recovery::TurnCheckpoint::load(&root)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(saved.messages).unwrap(),
        serde_json::to_value(snapshot.messages).unwrap()
    );
    assert_eq!(
        serde_json::to_value(saved.pending).unwrap(),
        serde_json::to_value(snapshot.pending).unwrap()
    );
    assert!(saved.interrupted);
    app.shutdown().await.unwrap();
}

async fn assert_corrupt_load_is_transactional(corrupt_spec: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut target = app_in(dir.path());
    target.messages = vec![AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: "Target history".into(),
        }],
        timestamp: 111,
    }];
    target.steering.seed_history(target.messages.clone());
    target
        .steering
        .push(
            vec![ContentBlock::Text {
                text: "Target pending".into(),
            }],
            222,
        )
        .unwrap();
    publish_spec(&mut target).await;
    target.shutdown().await.unwrap();
    let target_id = target.session_id.clone();
    let root = target.artifact_root.clone().unwrap();
    let checkpoint_path = root.join(crate::turn_recovery::CHECKPOINT_FILENAME);
    let spec_path = root.join("specs").join(format!(
        "{}.json",
        target.spec_document.as_ref().unwrap().id
    ));
    std::fs::write(
        if corrupt_spec {
            &spec_path
        } else {
            &checkpoint_path
        },
        b"{corrupt",
    )
    .unwrap();
    let protected: Vec<_> = [
        checkpoint_path,
        spec_path,
        root.join("specs/.current.json"),
        dir.path()
            .join("sessions")
            .join(format!("{target_id}.json")),
    ]
    .into_iter()
    .map(|path| {
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect();

    let mut app = app_in(dir.path());
    app.messages = tool_history();
    app.steering.seed_history(app.messages.clone());
    app.steering
        .push(
            vec![ContentBlock::Text {
                text: "Original pending".into(),
            }],
            333,
        )
        .unwrap();
    publish_spec(&mut app).await;
    let current = app.spec_broker.current().unwrap();
    app.spec_broker
        .approve(&current.id, current.revision)
        .unwrap();
    app.sync_spec().unwrap();
    app.note("你", "Visible original history");
    app.input = "Unsent original draft".into();
    let old_id = app.session_id.clone();
    let old_created_at = app.created_at;
    let old_root = app.artifact_root.clone();
    let old_messages = serde_json::to_value(&app.messages).unwrap();
    let old_history: Vec<_> = app
        .history
        .iter()
        .map(|entry| (entry.role, entry.text.clone()))
        .collect();
    let old_pending = serde_json::to_value(app.steering.pending()).unwrap();
    let old_broker = app.spec_broker.clone();
    let old_document = app.spec_document.clone();
    let old_log_path = app.diagnostic_log_path();

    assert!(app.load(&target_id).await.is_err());
    assert_eq!(app.session_id, old_id);
    assert_eq!(app.created_at, old_created_at);
    assert_eq!(app.artifact_root, old_root);
    assert_eq!(serde_json::to_value(&app.messages).unwrap(), old_messages);
    assert_eq!(
        app.history
            .iter()
            .map(|entry| (entry.role, entry.text.clone()))
            .collect::<Vec<_>>(),
        old_history
    );
    assert_eq!(
        serde_json::to_value(app.steering.pending()).unwrap(),
        old_pending
    );
    assert!(Arc::ptr_eq(&app.spec_broker, &old_broker));
    assert_eq!(app.spec_document, old_document);
    assert_eq!(
        app.spec_broker.current(),
        old_document.as_ref().map(|doc| doc.to_document())
    );
    assert_eq!(app.input, "Unsent original draft");
    assert_eq!(app.diagnostic_log_path(), old_log_path);
    app.tick().await;
    app.shutdown().await.unwrap();
    for (path, bytes) in protected {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "failed load followed by shutdown modified {}",
            path.display()
        );
    }
    let saved = crate::turn_recovery::TurnCheckpoint::load(old_root.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(saved.messages).unwrap(), old_messages);
    assert_eq!(serde_json::to_value(saved.pending).unwrap(), old_pending);
}

#[tokio::test]
async fn corrupt_checkpoint_load_keeps_original_state_and_shutdown_target_untouched() {
    assert_corrupt_load_is_transactional(false).await;
}

#[tokio::test]
async fn corrupt_spec_load_keeps_original_state_and_shutdown_target_untouched() {
    assert_corrupt_load_is_transactional(true).await;
}

fn app_in(root: &Path) -> App {
    App::new(
        Runtime::new(true, 30),
        Route::Large,
        Some(JsonFileStore::new(root.join("sessions"))),
    )
}

#[tokio::test]
async fn failed_pending_precedes_newer_idle_correction_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let tx = hold_worker(&mut app);
    app.input = "write all files".into();
    press(&mut app, KeyCode::Enter).await;
    release_worker(&app, tx, Err("previous turn failed".into())).await;
    app.tick().await;
    assert!(!app.busy());
    assert_eq!(app.steering.pending().len(), 1);
    assert_eq!(text(&app.steering.pending()[0].content), "write all files");

    app.submit("only inspect, no writes".into());
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.busy() {
            tokio::task::yield_now().await;
            app.tick().await;
        }
    })
    .await
    .expect("mock correction turn did not complete");

    let users: Vec<_> = app
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::User { content, .. } => Some(text(content)),
            _ => None,
        })
        .collect();
    let older: Vec<_> = users
        .iter()
        .enumerate()
        .filter(|(_, value)| value.as_str() == "write all files")
        .map(|(index, _)| index)
        .collect();
    let newer: Vec<_> = users
        .iter()
        .enumerate()
        .filter(|(_, value)| value.as_str() == "only inspect, no writes")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        older.len(),
        1,
        "older pending instruction must appear exactly once: {users:?}"
    );
    assert_eq!(
        newer.len(),
        1,
        "fresh correction must appear exactly once: {users:?}"
    );
    assert!(
        older[0] < newer[0],
        "older pending instruction must precede the newer correction: {users:?}"
    );
    assert!(app.steering.pending().is_empty());
    assert!(
        !app.recovery_interrupted,
        "mock correction turn should complete successfully"
    );
    app.shutdown().await.unwrap();
}

async fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE)).await;
}

#[tokio::test]
async fn cancellation_prevents_auto_resume_when_successful_completion_wins_race() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let tx = hold_worker(&mut app);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    app.cancel = Some(cancel_tx);
    app.input = "Pending instruction must wait".into();
    press(&mut app, KeyCode::Enter).await;
    let pending = serde_json::to_value(app.steering.pending()).unwrap();
    release_worker(&app, tx, completed()).await;
    app.cancel();
    assert!(
        cancel_rx.try_recv().is_ok(),
        "cancellation must reach the worker channel"
    );
    app.tick().await;
    app.tick().await;
    assert!(
        !app.busy(),
        "late successful completion must not override user cancellation"
    );
    assert_eq!(app.task_id, "controlled-worker");
    assert_eq!(
        serde_json::to_value(app.steering.pending()).unwrap(),
        pending
    );
    let saved = crate::turn_recovery::TurnCheckpoint::load(app.artifact_root.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(saved.pending).unwrap(), pending);
    app.shutdown().await.unwrap();
}

fn text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hold_worker(app: &mut App) -> oneshot::Sender<Finished> {
    assert!(!app.busy());
    let (tx, rx) = oneshot::channel();
    app.steering.seed_history(app.messages.clone());
    app.worker = Some(tokio::spawn(async move { rx.await.unwrap() }));
    app.task_id = "controlled-worker".into();
    app.recovery_interrupted = true;
    tx
}

async fn release_worker(
    app: &App,
    tx: oneshot::Sender<Finished>,
    result: Result<MonadResult, String>,
) {
    assert!(tx
        .send(Finished {
            messages: app.messages.clone(),
            result
        })
        .is_ok());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.worker.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controlled worker did not finish");
}

fn completed() -> Result<MonadResult, String> {
    Ok(MonadResult::Completed {
        task_id: TaskId::new(),
        parent_task_id: None,
        result_slot: ResultSlot("chat.reply".into()),
        monad_id: MonadId("main".into()),
        output: serde_json::json!({"turns_completed": 1}),
        evidence_refs: vec![],
        usage: ResourceUsage::default(),
        finished_at_ms: now_millis(),
    })
}

async fn stop_held_worker(app: &mut App) {
    if let Some(worker) = app.worker.take() {
        worker.abort();
        let _ = worker.await;
    }
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn busy_enter_queues_input_once_and_clears_draft() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let _tx = hold_worker(&mut app);
    app.input = "Include the timeout case".into();
    press(&mut app, KeyCode::Enter).await;
    assert!(app.input.is_empty());
    assert!(app.busy());
    assert_eq!(app.task_id, "controlled-worker");
    let pending = app.steering.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(text(&pending[0].content), "Include the timeout case");
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.steering.pending().len(), 1);
    assert_eq!(
        app.history
            .iter()
            .filter(|entry| entry.role == "你" && entry.text == "Include the timeout case")
            .count(),
        1
    );
    stop_held_worker(&mut app).await;
}

#[tokio::test]
async fn late_successful_completion_starts_continuation_and_consumes_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let tx = hold_worker(&mut app);
    // The worker is done, but App has not observed completion yet.
    release_worker(&app, tx, completed()).await;
    app.input = "Also cover recovery".into();
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.steering.pending().len(), 1);
    app.tick().await;
    assert!(
        app.busy(),
        "successful completion must start the pending continuation"
    );
    assert_ne!(app.task_id, "controlled-worker");
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.busy() {
            tokio::task::yield_now().await;
            app.tick().await;
        }
    })
    .await
    .expect("mock continuation did not complete");
    assert!(app.steering.pending().is_empty());
    assert_eq!(
        app.messages
            .iter()
            .filter(|message| matches!(message,
                AgentMessage::User { content, .. } if text(content) == "Also cover recovery"
            ))
            .count(),
        1
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_completion_retains_pending_without_automatic_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let tx = hold_worker(&mut app);
    app.input = "Preserve this instruction".into();
    press(&mut app, KeyCode::Enter).await;
    let accepted = app.steering.pending();
    release_worker(&app, tx, Err("provider failed after a side effect".into())).await;
    app.tick().await;
    app.tick().await;
    assert!(!app.busy());
    assert_eq!(app.task_id, "controlled-worker");
    assert!(app.recovery_interrupted);
    assert_eq!(
        serde_json::to_value(app.steering.pending()).unwrap(),
        serde_json::to_value(accepted).unwrap()
    );
    let saved = crate::turn_recovery::TurnCheckpoint::load(app.artifact_root.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert!(saved.interrupted);
    assert_eq!(text(&saved.pending[0].content), "Preserve this instruction");
    app.shutdown().await.unwrap();
}

fn tool_history() -> Vec<AgentMessage> {
    vec![
        AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "Run the checks".into(),
            }],
            timestamp: 1,
        },
        AgentMessage::Assistant {
            content: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "done-call".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command":"echo done"}),
                },
                ToolCall {
                    id: "unknown-call".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command":"external-write"}),
                },
            ],
            model: "test".into(),
            usage: None,
        },
        AgentMessage::ToolResult {
            tool_call_id: "done-call".into(),
            content: vec![ContentBlock::Text {
                text: "actual persisted result".into(),
            }],
            is_error: false,
        },
    ]
}

#[tokio::test]
async fn app_load_uses_checkpoint_retains_results_repairs_unknown_and_restores_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    app.save().await.unwrap();
    app.messages = tool_history();
    app.recovery_interrupted = true;
    app.steering
        .push(
            vec![ContentBlock::Text {
                text: "Verify before retry".into(),
            }],
            987,
        )
        .unwrap();
    app.persist_checkpoint(app.steering.pending())
        .await
        .unwrap();
    let id = app.session_id.clone();
    let artifact_root = app.artifact_root.clone();
    // A stale session snapshot must not replace the richer turn checkpoint.
    app.messages.clear();
    app.save().await.unwrap();
    let mut restored = app_in(dir.path());
    restored.load(&id).await.unwrap();
    assert_eq!(restored.artifact_root, artifact_root);
    assert!(restored.recovery_interrupted);
    assert!(!restored.busy());
    assert_eq!(
        restored
            .messages
            .iter()
            .filter(|message| matches!(message,
                AgentMessage::ToolResult { tool_call_id, content, is_error: false }
                if tool_call_id == "done-call" && text(content) == "actual persisted result"
            ))
            .count(),
        1
    );
    assert_eq!(restored.messages.iter().filter(|message| matches!(message,
        AgentMessage::ToolResult { tool_call_id, is_error: true, .. } if tool_call_id == "unknown-call"
    )).count(), 1);
    let pending = restored.steering.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(text(&pending[0].content), "Verify before retry");
    assert_eq!(pending[0].timestamp, 987);
    restored.tick().await;
    assert!(
        restored.recovery_interrupted,
        "idle tick must retain interrupted recovery state"
    );
    assert_recovered_tool_pairs(&restored.messages);
    let saved =
        crate::turn_recovery::TurnCheckpoint::load(restored.artifact_root.as_ref().unwrap())
            .unwrap()
            .unwrap();
    assert!(saved.interrupted);
    assert_recovered_tool_pairs(&saved.messages);
    restored.shutdown().await.unwrap();
}

fn assert_recovered_tool_pairs(messages: &[AgentMessage]) {
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message,
                AgentMessage::ToolResult { tool_call_id, content, is_error: false }
                if tool_call_id == "done-call" && text(content) == "actual persisted result"
            ))
            .count(),
        1,
        "completed result must survive recovery exactly once"
    );
    let unknown: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                tool_call_id,
                content,
                is_error: true,
            } if tool_call_id == "unknown-call" => Some(text(content)),
            _ => None,
        })
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "interrupted tool call must have exactly one error result"
    );
    let error: serde_json::Value = serde_json::from_str(&unknown[0]).unwrap();
    assert_eq!(error["error"]["kind"], "interrupted");
}

async fn assert_worker_join_failure_preserves_snapshot(panic: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let tx = hold_worker(&mut app);
    // Publish newer history than App currently holds before losing the worker.
    assert!(app.messages.is_empty());
    app.steering.seed_history(tool_history());
    app.steering
        .push(
            vec![ContentBlock::Text {
                text: "Check unknown effects first".into(),
            }],
            4321,
        )
        .unwrap();
    if panic {
        // The controlled worker unwraps the dropped completion channel.
        drop(tx);
    } else {
        app.worker.as_ref().unwrap().abort();
        drop(tx);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.worker.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    app.tick().await;
    assert!(
        !app.busy(),
        "join failure must not automatically replay a turn"
    );
    assert!(app.recovery_interrupted);
    assert_recovered_tool_pairs(&app.messages);
    // The next tick must not replace repaired history with a stale inbox snapshot.
    app.tick().await;
    assert!(app.recovery_interrupted);
    assert_recovered_tool_pairs(&app.messages);
    let saved = crate::turn_recovery::TurnCheckpoint::load(app.artifact_root.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert!(saved.interrupted);
    assert_recovered_tool_pairs(&saved.messages);
    assert_eq!(saved.pending.len(), 1);
    assert_eq!(
        text(&saved.pending[0].content),
        "Check unknown effects first"
    );
    assert_eq!(saved.pending[0].timestamp, 4321);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn panicked_worker_retains_snapshot_and_repairs_unknown_tool_pair() {
    assert_worker_join_failure_preserves_snapshot(true).await;
}

#[tokio::test]
async fn aborted_worker_retains_snapshot_and_repairs_unknown_tool_pair() {
    assert_worker_join_failure_preserves_snapshot(false).await;
}

#[tokio::test]
async fn comment_reopens_approved_spec_in_broker_disk_and_subsequent_ticks() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let _tx = hold_worker(&mut app);
    publish_spec(&mut app).await;
    press(&mut app, KeyCode::Char('a')).await;
    assert_eq!(
        app.spec_broker.current().unwrap().approved_revision,
        Some(1)
    );
    let store = SpecStore::new(app.artifact_root.as_ref().unwrap());
    assert_eq!(
        store.load_current().unwrap().unwrap().approved_revision,
        Some(1)
    );
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Char('c')).await;
    for c in "One more correction".chars() {
        press(&mut app, KeyCode::Char(c)).await;
    }
    press(&mut app, KeyCode::Enter).await;
    for _ in 0..3 {
        let broker = app.spec_broker.current().unwrap();
        assert_eq!(broker.revision, 1);
        assert_eq!(broker.approved_revision, None);
        let doc = app.spec_document.as_ref().unwrap();
        assert_eq!(doc.approved_revision, None);
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].text, "One more correction");
        assert_eq!(store.load_current().unwrap().as_ref(), Some(doc));
        app.tick().await;
    }
    assert_eq!(app.steering.pending().len(), 2);
    let id = app.session_id.clone();
    stop_held_worker(&mut app).await;
    let mut restored = app_in(dir.path());
    restored.load(&id).await.unwrap();
    restored.tick().await;
    assert_eq!(
        restored.spec_broker.current().unwrap().approved_revision,
        None
    );
    assert_eq!(
        restored.spec_document.as_ref().unwrap().approved_revision,
        None
    );
    assert_eq!(restored.spec_document.as_ref().unwrap().comments.len(), 1);
    restored.shutdown().await.unwrap();
}

async fn publish_spec(app: &mut App) {
    app.spec_broker
        .publish(
            None,
            "Implementation review",
            "# Plan\n\nFirst paragraph.\n\nSecond paragraph.",
            None,
        )
        .unwrap();
    app.tick().await;
    press(app, KeyCode::F(9)).await;
    assert_eq!(app.current, Page::Spec);
}

#[tokio::test]
async fn spec_keys_comment_revision_approval_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let _tx = hold_worker(&mut app);
    publish_spec(&mut app).await;
    press(&mut app, KeyCode::Down).await;
    assert_eq!(app.spec_view.selected, 1);
    let block = app.spec_document.as_ref().unwrap().blocks[1].id;
    press(&mut app, KeyCode::Char('c')).await;
    for c in "Explain recovery".chars() {
        press(&mut app, KeyCode::Char(c)).await;
    }
    press(&mut app, KeyCode::Enter).await;
    let doc = app.spec_document.as_ref().unwrap();
    assert_eq!(doc.comments.len(), 1);
    assert_eq!(doc.comments[0].block_id, block);
    assert_eq!(doc.comments[0].revision, 1);
    assert_eq!(doc.comments[0].text, "Explain recovery");
    assert!(app.spec_view.input.is_none());
    assert!(text(&app.steering.pending()[0].content).contains("Explain recovery"));
    press(&mut app, KeyCode::Char('a')).await;
    assert_eq!(
        app.spec_broker.current().unwrap().approved_revision,
        Some(1)
    );
    let id = app.spec_document.as_ref().unwrap().id.clone();
    app.spec_broker
        .publish(
            Some(&id),
            "Implementation review",
            "# Plan\n\nFirst paragraph.\n\nRecovery is explicit.",
            Some(1),
        )
        .unwrap();
    app.tick().await;
    assert_eq!(app.spec_document.as_ref().unwrap().revision, 2);
    assert_eq!(app.spec_document.as_ref().unwrap().approved_revision, None);
    assert_eq!(app.spec_document.as_ref().unwrap().comments[0].revision, 1);
    press(&mut app, KeyCode::Char('a')).await;
    assert_eq!(
        app.spec_broker.current().unwrap().approved_revision,
        Some(2)
    );
    assert_eq!(app.steering.pending().len(), 3);
    let disk = SpecStore::new(app.artifact_root.as_ref().unwrap())
        .load_current()
        .unwrap()
        .unwrap();
    assert_eq!(&disk, app.spec_document.as_ref().unwrap());
    assert_eq!(disk.approved_revision, Some(2));
    press(&mut app, KeyCode::Esc).await;
    assert_eq!(app.current, Page::Chat);
    let session = app.session_id.clone();
    stop_held_worker(&mut app).await;
    let mut restored = app_in(dir.path());
    restored.load(&session).await.unwrap();
    assert_eq!(restored.spec_document, Some(disk));
    assert_eq!(
        restored.spec_broker.current().unwrap().approved_revision,
        Some(2)
    );
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_comment_draft_is_not_applied_to_a_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_in(dir.path());
    let _tx = hold_worker(&mut app);
    publish_spec(&mut app).await;
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Char('c')).await;
    for c in "Old paragraph feedback".chars() {
        press(&mut app, KeyCode::Char(c)).await;
    }
    let id = app.spec_document.as_ref().unwrap().id.clone();
    app.spec_broker
        .publish(
            Some(&id),
            "Implementation review",
            "# Replaced\n\nDifferent paragraph.",
            Some(1),
        )
        .unwrap();
    app.tick().await;
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.spec_document.as_ref().unwrap().revision, 2);
    assert!(app.spec_document.as_ref().unwrap().comments.is_empty());
    assert!(app.steering.pending().is_empty());
    assert_eq!(
        app.spec_view.input.as_deref(),
        Some("Old paragraph feedback")
    );
    assert_eq!(app.spec_view.input_revision, Some(1));
    assert!(
        app.status.starts_with("方案："),
        "stale submission should report the review error: {}",
        app.status
    );
    assert!(SpecStore::new(app.artifact_root.as_ref().unwrap())
        .load_current()
        .unwrap()
        .unwrap()
        .comments
        .is_empty());
    stop_held_worker(&mut app).await;
}
