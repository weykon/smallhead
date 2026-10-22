//! Process-local, host-bound collaboration. Dropping handles never ends a child.
//! Paused children retain their runtime permit and wall-clock deadline.

use super::{MonadError, TaskId};
use crate::agent_loop::SteeringInbox;
use crate::message::ContentBlock;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Maximum retained child-attempt records; ended records are not evicted.
pub const MAX_CHILDREN: usize = 4096;
/// Total event bound, including reserved cancel/end and acknowledgement/unlocks.
pub const MAX_CHILD_EVENTS: usize = 128;
/// Maximum combined report text/request size, or steering text size, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
/// Steering inbox capacity; an outstanding request reserves one slot for reply.
pub const STEERING_CAPACITY: usize = 64;

/// Child-authored report category, never a caller-supplied sender identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// Intermediate progress, without a reply requirement.
    Progress,
    /// Evidence or an intermediate conclusion, not runtime completion.
    Finding,
    /// Execution cannot proceed until the parent replies to this request.
    Blocked,
    /// A decision must be aligned with the parent before proceeding.
    AlignmentRequest,
}

/// Collaboration state, independent of runtime success/failure terminal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationPhase {
    /// No outstanding stop condition.
    Running,
    /// Explicit pause requested but not acknowledged at a checkpoint yet.
    PauseRequested,
    /// Explicit pause acknowledged at a safe checkpoint.
    Paused,
    /// An exact request reply is required; see snapshot.paused for acknowledgement.
    WaitingAlignment,
    /// The host closed the channel after the owned worker ended.
    Ended,
}

/// Typed events retained for non-consuming parent reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationEventKind {
    /// Child report; JSON uses {"report":"progress"} and related report names.
    Report(ReportKind),
    /// Ordinary parent steering accepted by the inbox.
    Steering,
    /// Exact request reply accepted by the inbox.
    Reply,
    /// Explicit parent pause command accepted.
    PauseRequested,
    /// Safe-boundary acknowledgement of explicit pause and/or alignment wait.
    Paused,
    /// Explicit pause released; pending alignment may still block.
    Resumed,
    /// Stop requested, without implying cleanup has finished.
    Cancelled,
    /// Host-confirmed channel end; not a successful runtime result.
    Ended,
}

/// A host-authenticated event and acceptance receipt. Sequence IDs are Hub-global.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationEvent {
    /// Monotonic Hub-global cursor used by events/ready.
    pub seq: u64,
    /// Acceptance ID, identical to seq.
    pub message_id: u64,
    /// Parent identity captured by host registration.
    pub parent: TaskId,
    /// Child task identity captured by host registration.
    pub childtask: TaskId,
    /// Registered child attempt, never supplied by report text.
    pub attempt: u32,
    /// Typed report or lifecycle/control event.
    pub kind: CollaborationEventKind,
    /// Message text; system control events use empty text.
    pub text: String,
    /// Request report's seq, also attached to the matching reply.
    pub request_id: Option<u64>,
    /// Optional additional child request description, counted in the byte bound.
    pub request: Option<String>,
    /// Inbox acceptance ID; acceptance alone does not imply history commitment.
    pub steering_id: Option<u64>,
    /// Phase immediately after the event's state transition.
    pub phase: CollaborationPhase,
}

/// The single outstanding exact-reply request for an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    /// Automatically assigned request report sequence.
    pub request_id: u64,
    /// Blocked or AlignmentRequest.
    pub kind: ReportKind,
    /// Original report text.
    pub text: String,
    /// Optional additional request description.
    pub request: Option<String>,
}

/// Atomic child state plus lazily collected SteeringInbox commitment receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollaborationSnapshot {
    /// Host-bound parent identity.
    pub parent: TaskId,
    /// Host-bound child task identity.
    pub task_id: TaskId,
    /// Host-bound attempt number.
    pub attempt_id: u32,
    /// Collaboration phase, not a runtime terminal status.
    pub phase: CollaborationPhase,
    /// True only after a checkpoint acknowledged a still-active stop condition.
    pub paused: bool,
    /// Explicit pause flag, independent of a pending alignment request.
    pub pause_requested: bool,
    /// Exact request still requiring a reply, even after resume.
    pub pending_request: Option<PendingRequest>,
    /// Whether this channel's cancellation token has been cancelled.
    pub cancelled: bool,
    /// Steering accepted but not yet committed into AgentLoop history.
    pub messages_queued: usize,
    /// Steering receipts confirm history commitment, not model interpretation.
    pub messages_delivered: usize,
    /// Exact inbox IDs committed to history, not proof of model interpretation.
    /// Retained and deduplicated; bounded by this attempt's accepted event history.
    pub delivered_steering_ids: Vec<u64>,
    /// Latest visible event sequence for this child attempt.
    pub seq: u64,
}

#[derive(Debug)]
struct ChildState {
    parent: TaskId,
    task: TaskId,
    attempt: u32,
    explicit_pause: bool,
    acknowledged: bool,
    pending: Option<PendingRequest>,
    ended: bool,
    inbox: SteeringInbox,
    cancel: CancellationToken,
    notify: Arc<Notify>,
    history: Vec<CollaborationEvent>,
    // Terminal notification has its own reserved slot, even when history is full.
    terminal: Option<CollaborationEvent>,
    cancellation_event: Option<CollaborationEvent>,
    delivered_steering_ids: Vec<u64>,
    seq: u64,
}

impl ChildState {
    fn phase(&self) -> CollaborationPhase {
        if self.ended {
            CollaborationPhase::Ended
        } else if self.pending.is_some() {
            CollaborationPhase::WaitingAlignment
        } else if self.explicit_pause && self.acknowledged {
            CollaborationPhase::Paused
        } else if self.explicit_pause {
            CollaborationPhase::PauseRequested
        } else {
            CollaborationPhase::Running
        }
    }

    fn open(&self) -> Result<(), MonadError> {
        if self.ended {
            Err(error("collaboration has ended"))
        } else if self.cancel.is_cancelled() {
            Err(error("collaboration cancelled"))
        } else {
            Ok(())
        }
    }

    fn capacity_after(
        &self,
        pending: bool,
        explicit_pause: bool,
        acknowledged: bool,
    ) -> Result<(), MonadError> {
        self.open()?;
        // Retain space for the exact reply, explicit resume, and one checkpoint
        // acknowledgement. No history is evicted and unlocks cannot be crowded out.
        let reserved = usize::from(pending)
            + usize::from(explicit_pause)
            + usize::from(!acknowledged && (pending || explicit_pause));
        if self.history.len() + 1 + reserved > MAX_CHILD_EVENTS - 2 {
            Err(error("collaboration history full"))
        } else {
            Ok(())
        }
    }

    fn snapshot(&mut self) -> CollaborationSnapshot {
        for id in self.inbox.take_receipts() {
            if self
                .history
                .iter()
                .any(|event| event.steering_id == Some(id))
                && !self.delivered_steering_ids.contains(&id)
            {
                self.delivered_steering_ids.push(id);
            }
        }
        CollaborationSnapshot {
            parent: self.parent.clone(),
            task_id: self.task.clone(),
            attempt_id: self.attempt,
            phase: self.phase(),
            paused: !self.ended
                && self.acknowledged
                && (self.explicit_pause || self.pending.is_some()),
            pause_requested: self.explicit_pause,
            pending_request: self.pending.clone(),
            cancelled: self.cancel.is_cancelled(),
            messages_queued: self.inbox.pending().len(),
            messages_delivered: self.delivered_steering_ids.len(),
            delivered_steering_ids: self.delivered_steering_ids.clone(),
            seq: self.seq,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    seq: u64,
    children: HashMap<(TaskId, u32), ChildState>,
    parents: HashMap<TaskId, u64>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

/// Bounded, shared in-process collaboration registry with parent-scoped reads.
#[derive(Debug, Clone, Default)]
pub struct CollaborationHub {
    shared: Arc<Shared>,
}

/// Short alias for host integration and tests.
pub type Hub = CollaborationHub;

/// A capability obtained through host registration or parent-validated lookup.
#[derive(Debug, Clone)]
pub struct ChildChannel {
    shared: Arc<Shared>,
    task: TaskId,
    attempt: u32,
}

impl CollaborationHub {
    /// Create an independent Hub with the documented fixed capacity bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registration is silent: no report event or sequence increment is produced.
    pub fn register(
        &self,
        parent: TaskId,
        task: TaskId,
        attempt: u32,
    ) -> Result<ChildChannel, MonadError> {
        if parent.0.trim().is_empty() || task.0.trim().is_empty() || parent == task || attempt == 0
        {
            return Err(error("invalid collaboration binding"));
        }
        let mut state = self.shared.state.lock();
        let key = (task.clone(), attempt);
        if state.children.contains_key(&key) {
            return Err(error("collaboration already registered"));
        }
        if state.children.len() >= MAX_CHILDREN {
            return Err(error("collaboration record capacity exhausted"));
        }
        if state
            .children
            .values()
            .any(|child| child.task == task && child.parent != parent)
        {
            return Err(error("collaboration parent mismatch"));
        }
        if state
            .children
            .values()
            .any(|child| child.task == task && (!child.ended || child.attempt >= attempt))
        {
            return Err(error("new attempt requires ended older collaboration"));
        }
        state.parents.entry(parent.clone()).or_insert(0);
        state.children.insert(
            key,
            ChildState {
                parent,
                task: task.clone(),
                attempt,
                explicit_pause: false,
                acknowledged: false,
                pending: None,
                ended: false,
                inbox: SteeringInbox::with_capacity(STEERING_CAPACITY),
                cancel: CancellationToken::new(),
                notify: Arc::new(Notify::new()),
                history: Vec::new(),
                terminal: None,
                cancellation_event: None,
                delivered_steering_ids: Vec::new(),
                seq: 0,
            },
        );
        Ok(ChildChannel {
            shared: self.shared.clone(),
            task,
            attempt,
        })
    }

    /// Retrieve a capability only when parent, task, and attempt all match.
    pub fn channel(
        &self,
        parent: &TaskId,
        task: &TaskId,
        attempt: u32,
    ) -> Result<ChildChannel, MonadError> {
        let state = self.shared.state.lock();
        let child = state
            .children
            .get(&(task.clone(), attempt))
            .ok_or_else(|| error("unknown collaboration"))?;
        if &child.parent != parent {
            return Err(error("collaboration parent mismatch"));
        }
        Ok(ChildChannel {
            shared: self.shared.clone(),
            task: task.clone(),
            attempt,
        })
    }

    /// Snapshot all this parent's retained attempts in task/attempt order.
    pub fn children(&self, parent: &TaskId) -> Vec<CollaborationSnapshot> {
        let mut state = self.shared.state.lock();
        let mut children: Vec<_> = state
            .children
            .values_mut()
            .filter(|c| &c.parent == parent)
            .map(ChildState::snapshot)
            .collect();
        children.sort_by(|a, b| {
            a.task_id
                .0
                .cmp(&b.task_id.0)
                .then(a.attempt_id.cmp(&b.attempt_id))
        });
        children
    }

    /// Latest visible sequence for this parent, or zero before any event.
    pub fn cursor(&self, parent: &TaskId) -> u64 {
        self.shared
            .state
            .lock()
            .parents
            .get(parent)
            .copied()
            .unwrap_or(0)
    }

    /// Non-consuming, ordered history. Zero limit returns no events. `after` and
    /// event `seq` share the same global scale; registration emits no event.
    pub fn events(&self, parent: &TaskId, after: u64, limit: usize) -> Vec<CollaborationEvent> {
        let state = self.shared.state.lock();
        let mut events: Vec<_> = state
            .children
            .values()
            .filter(|c| &c.parent == parent)
            .flat_map(|c| {
                c.history
                    .iter()
                    .chain(c.cancellation_event.iter())
                    .chain(c.terminal.iter())
            })
            .filter(|event| event.seq > after)
            .cloned()
            .collect();
        events.sort_by_key(|event| event.seq);
        events.truncate(limit);
        events
    }

    /// Wait without a default timeout for an event newer than `after`. The caller
    /// cancels by dropping/selecting this future. Other parents do not advance it.
    pub async fn ready(&self, parent: &TaskId, after: u64) -> u64 {
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let cursor = self.cursor(parent);
            if cursor > after {
                return cursor;
            }
            notified.await;
        }
    }

    /// On timeout returns the latest cursor (possibly unchanged).
    pub async fn wait_changed(&self, parent: &TaskId, after: u64, timeout: Duration) -> u64 {
        match tokio::time::timeout(timeout, self.ready(parent, after)).await {
            Ok(cursor) => cursor,
            Err(_) => self.cursor(parent),
        }
    }
}

impl ChildChannel {
    fn mutate<T>(
        &self,
        action: impl FnOnce(&mut ChildState, u64) -> Result<T, MonadError>,
    ) -> Result<T, MonadError> {
        let mut state = self.shared.state.lock();
        let seq = state
            .seq
            .checked_add(1)
            .ok_or_else(|| error("collaboration sequence exhausted"))?;
        let child = state
            .children
            .get_mut(&(self.task.clone(), self.attempt))
            .expect("registered channel");
        let result = action(child, seq)?;
        child.seq = seq;
        let parent = child.parent.clone();
        let notify = child.notify.clone();
        state.seq = seq;
        state.parents.insert(parent, seq);
        drop(state);
        notify.notify_waiters();
        self.shared.changed.notify_waiters();
        Ok(result)
    }

    /// Append a bounded, host-authenticated report. Blocked/alignment generate a
    /// request ID equal to seq and require exact reply at the next checkpoint.
    pub fn report(
        &self,
        kind: ReportKind,
        text: impl Into<String>,
        request: Option<String>,
    ) -> Result<CollaborationEvent, MonadError> {
        let text = text.into();
        validate_text(&text)?;
        if let Some(request) = &request {
            validate_text(request)?;
        }
        if text.len() + request.as_ref().map_or(0, String::len) > MAX_MESSAGE_BYTES {
            return Err(error("collaboration message exceeds 8 KiB"));
        }
        self.mutate(|child, seq| {
            let needs_reply = matches!(kind, ReportKind::Blocked | ReportKind::AlignmentRequest);
            if needs_reply && child.pending.is_some() {
                return Err(error("alignment request already pending"));
            }
            child.capacity_after(
                child.pending.is_some() || needs_reply,
                child.explicit_pause,
                child.acknowledged,
            )?;
            if needs_reply && child.inbox.pending().len() >= STEERING_CAPACITY {
                return Err(error("steering inbox has no reserved reply capacity"));
            }
            let request_id = needs_reply.then_some(seq);
            if needs_reply {
                child.pending = Some(PendingRequest {
                    request_id: seq,
                    kind,
                    text: text.clone(),
                    request: request.clone(),
                });
            }
            let event = event(
                child,
                seq,
                CollaborationEventKind::Report(kind),
                text,
                request_id,
                request,
                None,
            );
            child.history.push(event.clone());
            Ok(event)
        })
    }

    /// Queue ordinary steering; never resolves an outstanding alignment request.
    pub fn send(&self, text: impl Into<String>) -> Result<CollaborationEvent, MonadError> {
        self.steer(text.into(), None, false)
    }

    /// Queue an exact reply and clear its pending request atomically. A stale ID
    /// or full inbox changes neither pending state nor the event cursor.
    pub fn reply(
        &self,
        request_id: u64,
        text: impl Into<String>,
    ) -> Result<CollaborationEvent, MonadError> {
        self.steer(text.into(), Some(request_id), false)
    }

    fn steer(
        &self,
        text: String,
        request_id: Option<u64>,
        resume: bool,
    ) -> Result<CollaborationEvent, MonadError> {
        validate_text(&text)?;
        self.mutate(|child, seq| {
            if let Some(id) = request_id {
                if child.pending.as_ref().map(|p| p.request_id) != Some(id) {
                    return Err(error("stale or unknown alignment reply"));
                }
            }
            let pending = child.pending.is_some() && request_id.is_none();
            let explicit_pause = child.explicit_pause && !resume;
            let acknowledged = child.acknowledged && (pending || explicit_pause);
            child.capacity_after(pending, explicit_pause, acknowledged)?;
            // Keep one inbox slot for a pending exact reply. Ordinary steering
            // may fill all 64 slots only when no request is outstanding.
            if pending && child.inbox.pending().len() >= STEERING_CAPACITY - 1 {
                return Err(error("steering inbox reply reserve exhausted"));
            }
            // Hold the collaboration lock across inbox acceptance: full means no
            // pending request or pause state is cleared and no receipt is issued.
            let id = child
                .inbox
                .try_push(
                    vec![ContentBlock::Text {
                        text: format!(
                            "[Monad collaboration reference {}]\n{}",
                            serde_json::json!({
                                "source": "parent", "parent": child.parent,
                                "task": child.task, "attempt": child.attempt,
                                "seq": seq, "request_id": request_id,
                            }),
                            text,
                        ),
                    }],
                    timestamp(),
                )
                .map_err(|e| error(&e.to_string()))?;
            if request_id.is_some() {
                child.pending = None;
            }
            child.explicit_pause = explicit_pause;
            child.acknowledged = acknowledged;
            let kind = if request_id.is_some() {
                CollaborationEventKind::Reply
            } else if resume {
                CollaborationEventKind::Resumed
            } else {
                CollaborationEventKind::Steering
            };
            let event = event(child, seq, kind, text, request_id, None, Some(id));
            child.history.push(event.clone());
            Ok(event)
        })
    }

    /// Request explicit pause; Paused is acknowledged only at a safe checkpoint.
    pub fn pause(&self) -> Result<CollaborationPhase, MonadError> {
        self.mutate(|child, seq| {
            child.capacity_after(child.pending.is_some(), true, child.acknowledged)?;
            child.explicit_pause = true;
            let event = event(
                child,
                seq,
                CollaborationEventKind::PauseRequested,
                String::new(),
                None,
                None,
                None,
            );
            child.history.push(event);
            Ok(child.phase())
        })
    }

    /// Release explicit pause, optionally queueing steering. Pending alignment
    /// remains blocked. With no text no inbox capacity is needed to unlock.
    pub fn resume(&self, text: Option<String>) -> Result<CollaborationPhase, MonadError> {
        if let Some(text) = text {
            return self.steer(text, None, true).map(|e| e.phase);
        }
        self.mutate(|child, seq| {
            let acknowledged = child.acknowledged && child.pending.is_some();
            child.capacity_after(child.pending.is_some(), false, acknowledged)?;
            child.explicit_pause = false;
            child.acknowledged = acknowledged;
            let event = event(
                child,
                seq,
                CollaborationEventKind::Resumed,
                String::new(),
                None,
                None,
                None,
            );
            child.history.push(event);
            Ok(child.phase())
        })
    }

    /// Child-scoped stop token. Use cancel() to also publish a parent event;
    /// cancelling this raw token directly does not publish a cursor event.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.shared.state.lock().children[&(self.task.clone(), self.attempt)]
            .cancel
            .clone()
    }

    /// Cancellation and terminal events have reserved capacity outside history.
    /// Repeated cancellation is a no-op without advancing the parent cursor.
    pub fn cancel(&self) -> Result<(), MonadError> {
        let mut state = self.shared.state.lock();
        let key = (self.task.clone(), self.attempt);
        let child = &state.children[&key];
        if child.ended {
            return Err(error("collaboration has ended"));
        }
        if child.cancel.is_cancelled() {
            return Ok(());
        }
        let seq = state
            .seq
            .checked_add(1)
            .ok_or_else(|| error("collaboration sequence exhausted"))?;
        let child = state.children.get_mut(&key).expect("registered channel");
        child.cancel.cancel();
        child.seq = seq;
        child.cancellation_event = Some(event(
            child,
            seq,
            CollaborationEventKind::Cancelled,
            String::new(),
            None,
            None,
            None,
        ));
        let parent = child.parent.clone();
        let notify = child.notify.clone();
        state.seq = seq;
        state.parents.insert(parent, seq);
        drop(state);
        notify.notify_waiters();
        self.shared.changed.notify_waiters();
        Ok(())
    }

    /// Inbox for the AgentLoop binding. Parent writes should use send/reply so
    /// request reserves and terminal checks hold; snapshots own receipt draining.
    pub fn steering_inbox(&self) -> SteeringInbox {
        self.shared.state.lock().children[&(self.task.clone(), self.attempt)]
            .inbox
            .clone()
    }

    /// Host calls only after the owned runtime worker has finished cleanup.
    /// Ended describes channel lifecycle, never runtime success or completion.
    pub fn finish(&self) {
        let mut state = self.shared.state.lock();
        let key = (self.task.clone(), self.attempt);
        if state.children[&key].ended {
            return;
        }
        let seq = state
            .seq
            .checked_add(1)
            .expect("collaboration sequence exhausted");
        let child = state.children.get_mut(&key).expect("registered channel");
        child.ended = true;
        child.seq = seq;
        child.terminal = Some(event(
            child,
            seq,
            CollaborationEventKind::Ended,
            String::new(),
            None,
            None,
            None,
        ));
        let parent = child.parent.clone();
        let notify = child.notify.clone();
        state.seq = seq;
        state.parents.insert(parent, seq);
        drop(state);
        notify.notify_waiters();
        self.shared.changed.notify_waiters();
    }

    /// Inspect state and collect new history-commitment receipts exactly once.
    pub fn snapshot(&self) -> CollaborationSnapshot {
        self.shared
            .state
            .lock()
            .children
            .get_mut(&(self.task.clone(), self.attempt))
            .expect("registered channel")
            .snapshot()
    }

    /// Call at safe AgentLoop boundaries, before starting another provider/tool
    /// action. Ordinary steering never resolves alignment; only exact reply does.
    pub async fn checkpoint(&self, cancel: &CancellationToken) -> Result<(), MonadError> {
        let notify = self.shared.state.lock().children[&(self.task.clone(), self.attempt)]
            .notify
            .clone();
        let own_cancel = self.cancellation_token();
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if cancel.is_cancelled() || own_cancel.is_cancelled() {
                return Err(error("collaboration cancelled"));
            }
            {
                let mut state = self.shared.state.lock();
                let seq = state
                    .seq
                    .checked_add(1)
                    .ok_or_else(|| error("collaboration sequence exhausted"))?;
                let child = state
                    .children
                    .get_mut(&(self.task.clone(), self.attempt))
                    .expect("registered channel");
                child.open()?;
                if !child.explicit_pause && child.pending.is_none() {
                    return Ok(());
                }
                if !child.acknowledged {
                    child.capacity_after(child.pending.is_some(), child.explicit_pause, true)?;
                    child.acknowledged = true;
                    child.seq = seq;
                    let event = event(
                        child,
                        seq,
                        CollaborationEventKind::Paused,
                        String::new(),
                        None,
                        None,
                        None,
                    );
                    child.history.push(event);
                    let parent = child.parent.clone();
                    state.seq = seq;
                    state.parents.insert(parent, seq);
                    self.shared.changed.notify_waiters();
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(error("collaboration cancelled")),
                _ = own_cancel.cancelled() => return Err(error("collaboration cancelled")),
                _ = &mut notified => {}
            }
        }
    }
}

fn event(
    child: &ChildState,
    seq: u64,
    kind: CollaborationEventKind,
    text: String,
    request_id: Option<u64>,
    request: Option<String>,
    steering_id: Option<u64>,
) -> CollaborationEvent {
    CollaborationEvent {
        seq,
        message_id: seq,
        parent: child.parent.clone(),
        childtask: child.task.clone(),
        attempt: child.attempt,
        kind,
        text,
        request_id,
        request,
        steering_id,
        phase: child.phase(),
    }
}

fn error(message: &str) -> MonadError {
    MonadError::new(message)
}

fn validate_text(text: &str) -> Result<(), MonadError> {
    if text.trim().is_empty() {
        Err(error("collaboration text must not be blank"))
    } else if text.len() > MAX_MESSAGE_BYTES {
        Err(error("collaboration message exceeds 8 KiB"))
    } else {
        Ok(())
    }
}

fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
#[path = "collaboration_tests.rs"]
mod tests;
