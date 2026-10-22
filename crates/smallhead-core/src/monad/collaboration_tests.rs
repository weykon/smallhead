use super::*;
use std::time::Duration;

fn setup() -> (Hub, TaskId, ChildChannel) {
    let hub = Hub::default();
    let parent = TaskId::new();
    let child = hub.register(parent.clone(), TaskId::new(), 1).unwrap();
    (hub, parent, child)
}

#[test]
fn registration_and_lookup_are_bound_to_host_identity() {
    let (hub, parent, child) = setup();
    let snapshot = child.snapshot();
    assert!(hub
        .register(parent.clone(), snapshot.task_id.clone(), 1)
        .is_err());
    assert!(hub.channel(&TaskId::new(), &snapshot.task_id, 1).is_err());
    assert!(hub.channel(&parent, &snapshot.task_id, 2).is_err());
    let clone = hub.channel(&parent, &snapshot.task_id, 1).unwrap();
    clone.pause().unwrap();
    assert_eq!(child.snapshot().phase, CollaborationPhase::PauseRequested);
    drop(clone);
    assert_ne!(child.snapshot().phase, CollaborationPhase::Ended);
}

#[tokio::test]
async fn exact_reply_and_explicit_pause_are_independent() {
    let (_, _, child) = setup();
    let report = child
        .report(ReportKind::AlignmentRequest, "choose", None)
        .unwrap();
    let request = report.request_id.unwrap();
    child.pause().unwrap();
    child.resume(Some("extra context".into())).unwrap();
    assert_eq!(child.snapshot().phase, CollaborationPhase::WaitingAlignment);
    assert!(child.reply(request + 1, "wrong").is_err());
    let cancel = CancellationToken::new();
    let checkpoint = child.checkpoint(&cancel);
    tokio::pin!(checkpoint);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), &mut checkpoint)
            .await
            .is_err()
    );
    child.pause().unwrap();
    child.reply(request, "approved").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), &mut checkpoint)
            .await
            .is_err()
    );
    assert_eq!(child.snapshot().phase, CollaborationPhase::Paused);
    child.resume(None).unwrap();
    checkpoint.await.unwrap();
    let next = child
        .report(ReportKind::Blocked, "next", Some("need input".into()))
        .unwrap();
    assert!(next.request_id.unwrap() > request);
    assert!(child.reply(request, "stale").is_err());
    assert!(child.snapshot().pending_request.is_some());
}

#[test]
fn bounded_messages_and_inbox_reject_without_partial_reply() {
    let (_, _, child) = setup();
    let request = child
        .report(ReportKind::Blocked, "blocked", None)
        .unwrap()
        .request_id
        .unwrap();
    assert!(child.send("x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
    for _ in 0..STEERING_CAPACITY - 1 {
        child.send("context").unwrap();
    }
    assert!(child.send("would consume reply reserve").is_err());
    assert!(child
        .reply(request, "x".repeat(MAX_MESSAGE_BYTES + 1))
        .is_err());
    assert_eq!(
        child.snapshot().pending_request.unwrap().request_id,
        request
    );
    assert_eq!(child.snapshot().messages_queued, STEERING_CAPACITY - 1);
    assert_eq!(child.snapshot().messages_delivered, 0);
    child.reply(request, "answer").unwrap();
    assert_eq!(child.snapshot().messages_queued, STEERING_CAPACITY);
    assert!(child.snapshot().pending_request.is_none());
    let (_, _, other) = setup();
    for _ in 0..MAX_CHILD_EVENTS - 2 {
        other.report(ReportKind::Progress, "ok", None).unwrap();
    }
    assert!(other.report(ReportKind::Finding, "overflow", None).is_err());
    let cursor = other.snapshot().seq;
    other.finish();
    assert_eq!(other.snapshot().phase, CollaborationPhase::Ended);
    let hub = Hub {
        shared: other.shared.clone(),
    };
    let parent = other.snapshot().parent;
    let ended = hub.events(&parent, cursor, 10);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].kind, CollaborationEventKind::Ended);
}

#[tokio::test]
async fn cursor_reads_do_not_consume_and_wait_cannot_miss_change() {
    let (hub, parent, child) = setup();
    let cursor = hub.cursor(&parent);
    let event = child.report(ReportKind::Finding, "evidence", None).unwrap();
    assert!(
        hub.wait_changed(&parent, cursor, Duration::from_secs(1))
            .await
            > cursor
    );
    let events = hub.events(&parent, cursor, 1);
    assert_eq!(events[0].message_id, event.message_id);
    assert_eq!(hub.events(&parent, cursor, 1), events);
    let cursor = hub.cursor(&parent);
    let waiting = hub.wait_changed(&parent, cursor, Duration::from_secs(1));
    let control = async {
        tokio::task::yield_now().await;
        child.pause().unwrap();
    };
    let (next, ()) = tokio::join!(waiting, control);
    assert!(next > cursor);
    assert!(hub.events(&TaskId::new(), 0, 10).is_empty());
}

#[tokio::test]
async fn finish_and_cancel_wake_checkpoint_and_close_control() {
    let (_, _, child) = setup();
    child.pause().unwrap();
    let token = CancellationToken::new();
    let checkpoint = child.checkpoint(&token);
    let end = async {
        tokio::task::yield_now().await;
        child.finish();
    };
    let (result, ()) = tokio::join!(checkpoint, end);
    assert!(result.is_err());
    assert!(child.send("late").is_err());
    assert!(child.resume(None).is_err());
    assert!(child.report(ReportKind::Progress, "late", None).is_err());
    child.finish();
    let (_, _, child) = setup();
    child.pause().unwrap();
    child.cancel().unwrap();
    assert!(child.cancellation_token().is_cancelled());
    assert!(child.checkpoint(&token).await.is_err());
    let (_, _, child) = setup();
    token.cancel();
    assert!(child.checkpoint(&token).await.is_err());
}

#[test]
fn record_capacity_and_invalid_bindings_are_rejected() {
    let hub = Hub::default();
    let parent = TaskId::new();
    assert!(hub.register(parent.clone(), TaskId::new(), 0).is_err());
    assert!(hub.register(parent.clone(), parent.clone(), 1).is_err());
    for _ in 0..MAX_CHILDREN {
        hub.register(parent.clone(), TaskId::new(), 1).unwrap();
    }
    assert!(hub.register(parent.clone(), TaskId::new(), 1).is_err());
    assert_eq!(hub.children(&parent).len(), MAX_CHILDREN);
}

#[tokio::test]
async fn ready_wakes_all_parent_waiters_but_not_for_another_parent() {
    let (hub, parent, child) = setup();
    let other = hub.register(TaskId::new(), TaskId::new(), 1).unwrap();
    other
        .report(ReportKind::Progress, "other parent", None)
        .unwrap();
    assert_eq!(hub.cursor(&parent), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), hub.ready(&parent, 0))
            .await
            .is_err()
    );
    let first = hub.ready(&parent, 0);
    let second = hub.ready(&parent, 0);
    let produce = async {
        tokio::task::yield_now().await;
        child.finish();
    };
    let (a, b, ()) = tokio::join!(first, second, produce);
    assert_eq!(a, b);
    assert!(a > 0);
}

#[test]
fn report_serialization_and_attempt_endpoints_preserve_provenance() {
    let (hub, parent, child) = setup();
    let snapshot = child.snapshot();
    let kind: ReportKind = serde_json::from_str("\"alignment_request\"").unwrap();
    let report = child.report(kind, "need decision", None).unwrap();
    assert_eq!(report.request_id, Some(report.seq));
    assert_eq!(report.message_id, report.seq);
    assert_eq!(report.childtask, snapshot.task_id);
    assert_eq!(serde_json::to_value(&report).unwrap()["seq"], report.seq);
    assert!(hub
        .register(parent.clone(), snapshot.task_id.clone(), 2)
        .is_err());
    child.finish();
    let next = hub.register(parent.clone(), snapshot.task_id, 2).unwrap();
    let pending = next
        .report(ReportKind::Blocked, "new attempt", None)
        .unwrap();
    assert!(child
        .reply(pending.request_id.unwrap(), "old endpoint")
        .is_err());
    assert!(next
        .reply(report.request_id.unwrap(), "old request")
        .is_err());
    assert_eq!(
        next.snapshot().pending_request.unwrap().request_id,
        pending.seq
    );
}

#[tokio::test]
async fn steering_delivery_requires_actual_history_receipt() {
    let (_, _, child) = setup();
    let receipt = child.send("queued").unwrap();
    assert_eq!(child.snapshot().messages_delivered, 0);
    assert!(child.snapshot().delivered_steering_ids.is_empty());
    let provider = Arc::new(crate::provider::MockProvider::new(vec![
        crate::provider::MockResponse::AssistantText {
            text: "done".into(),
        },
    ]));
    let mut agent = crate::AgentLoop::new(provider, vec![], "test");
    agent.set_steering_inbox(child.steering_inbox());
    agent
        .run_one_turn(crate::AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "start".into(),
            }],
            timestamp: 0,
        })
        .await
        .unwrap();
    let snapshot = child.snapshot();
    assert_eq!(snapshot.messages_queued, 0);
    assert_eq!(snapshot.messages_delivered, 1);
    assert_eq!(
        snapshot.delivered_steering_ids,
        vec![receipt.steering_id.unwrap()]
    );
    assert_eq!(child.snapshot().messages_delivered, 1);
    assert_eq!(
        child.snapshot().delivered_steering_ids,
        snapshot.delivered_steering_ids
    );
}

#[test]
fn finish_serializes_with_control_and_reserves_terminal_capacity() {
    let (hub, parent, child) = setup();
    for _ in 0..MAX_CHILD_EVENTS - 2 {
        child.report(ReportKind::Progress, "full", None).unwrap();
    }
    child.cancel().unwrap();
    let cancelled_cursor = hub.cursor(&parent);
    assert_eq!(
        hub.events(&parent, cancelled_cursor - 1, 1)[0].kind,
        CollaborationEventKind::Cancelled
    );
    child.cancel().unwrap();
    assert_eq!(hub.cursor(&parent), cancelled_cursor);
    child.finish();
    let cursor = hub.cursor(&parent);
    child.finish();
    assert_eq!(hub.cursor(&parent), cursor);
    assert!(child.pause().is_err());
    assert!(child.cancel().is_err());
    assert!(child.reply(1, "late").is_err());
    let events = hub.events(&parent, 0, usize::MAX);
    assert_eq!(events.len(), MAX_CHILD_EVENTS);
    assert_eq!(events.last().unwrap().kind, CollaborationEventKind::Ended);
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[test]
fn blank_text_is_rejected_and_steering_keeps_reference_provenance() {
    let (_, _, child) = setup();
    assert!(child.send(" \n\t").is_err());
    assert!(child.report(ReportKind::Progress, "", None).is_err());
    assert!(child
        .report(ReportKind::Finding, "text", Some("  ".into()))
        .is_err());
    assert!(child.resume(Some("  ".into())).is_err());
    let receipt = child.send("reference only").unwrap();
    assert_eq!(receipt.text, "reference only");
    let inbox = child.steering_inbox();
    let pending = inbox.pending();
    let ContentBlock::Text { text } = &pending[0].content[0] else {
        panic!("expected reference text")
    };
    assert!(text.contains("Monad collaboration reference"));
    assert!(text.contains(&format!("\"seq\":{}", receipt.seq)));
    assert!(text.contains("\"source\":\"parent\""));
    assert!(text.ends_with("reference only"));
}

#[tokio::test]
async fn history_reserves_unlock_and_pause_ack_events() {
    let (hub, parent, child) = setup();
    let request = child
        .report(ReportKind::AlignmentRequest, "choose", None)
        .unwrap()
        .seq;
    child.pause().unwrap();
    assert!(!child.snapshot().paused);
    while child.report(ReportKind::Progress, "fill", None).is_ok() {}
    let before_ack = hub.cursor(&parent);
    let cancel = CancellationToken::new();
    let checkpoint = child.checkpoint(&cancel);
    tokio::pin!(checkpoint);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), &mut checkpoint)
            .await
            .is_err()
    );
    assert!(child.snapshot().paused);
    let acknowledged = hub.events(&parent, before_ack, 10);
    assert_eq!(acknowledged.len(), 1);
    assert_eq!(acknowledged[0].kind, CollaborationEventKind::Paused);
    child.reply(request, "decision").unwrap();
    assert!(child.snapshot().paused);
    child.resume(None).unwrap();
    checkpoint.await.unwrap();
    assert!(!child.snapshot().paused);
    child.cancel().unwrap();
    child.finish();
    assert_eq!(hub.events(&parent, 0, usize::MAX).len(), MAX_CHILD_EVENTS);
}
