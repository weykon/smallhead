//! Process-local admission, attempt binding and bounded lifecycle execution.
//!
//! Share one runtime across a delegation tree. Records retain the latest attempt
//! per task without eviction; full capacity rejects new identities. Slots remain
//! owned by their task, even after completion. WaitingContext ends an attempt;
//! supplying more context requires confirmed cleanup and a higher attempt with a
//! fresh child. Completed tasks cannot be retried; unknown or failed cleanup blocks
//! replay to avoid overlapping surviving side effects.
//! Cancellation is cooperative: synchronous blocking code and detached external
//! effects cannot be forcibly stopped. Cleanup has a separate bounded grace period.
//! Permits cover the entire child lifecycle. Nested delegation into this same
//! runtime can exhaust permits while ancestors wait; hosts must account for that
//! capacity or prevent recursive delegation. This is not a reentrant DAG scheduler.

use super::{MonadError, MonadHandoff, MonadLoop, MonadResult, TaskBinding, TaskId};
use async_trait::async_trait;
use futures::FutureExt;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Host limits, independent of a model's requested concurrency budget.
#[derive(Clone, Debug)]
pub struct MonadRuntimeConfig {
    /// Maximum child lifecycles, including cleanup, executing at once.
    pub max_concurrency: usize,
    /// Additional admitted children waiting for execution capacity.
    pub max_queued: usize,
    /// Maximum retained task identities; records are never silently evicted.
    pub max_records: usize,
    /// Separate grace period for the child's asynchronous cancellation hook.
    pub cleanup_timeout: Duration,
}

impl Default for MonadRuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 2,
            max_queued: 64,
            max_records: 4096,
            cleanup_timeout: Duration::from_secs(8),
        }
    }
}

/// Host envelope preserves the attempt identity without changing the wire result.
#[derive(Clone, Debug)]
pub struct MonadRun {
    /// Original host handoff, including the validated attempt number.
    pub handoff: MonadHandoff,
    /// Validated result or host-generated cancellation result.
    pub result: MonadResult,
    /// Unique host identity for this attempt.
    pub run_id: String,
}

/// Admission receipt for an owned worker; dropping it does not cancel execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonadReceipt {
    /// Task identity retained by the runtime.
    pub task_id: TaskId,
    /// Host-generated identity fencing this particular attempt.
    pub run_id: String,
    /// Attempt number supplied by the admitted handoff.
    pub attempt_id: u32,
}

struct SpawnedRun {
    receipt: MonadReceipt,
    token: CancellationToken,
    handle: tokio::task::JoinHandle<Result<MonadRun, MonadError>>,
}

/// Latest lifecycle state for one task's admitted attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    /// Admitted, waiting for a concurrency permit.
    Queued,
    /// Executing child start or step.
    Running,
    /// Checking result identity and caller acceptance.
    Validating,
    /// Awaiting the bounded asynchronous cancellation hook.
    CleaningUp,
    /// Completion passed identity and caller acceptance checks.
    Completed,
    /// Attempt ended requesting additional context; not a completed dependency.
    WaitingContext,
    /// Child returned failure or its lifecycle returned an error.
    Failed,
    /// Child or host reported cancellation.
    Cancelled,
    /// The host execution deadline elapsed.
    TimedOut,
    /// Result identity or semantic acceptance failed.
    Rejected,
    /// Child lifecycle or validator unwound with a panic.
    Panicked,
    /// Worker itself was dropped unexpectedly; async cleanup is unconfirmed.
    Abandoned,
}

impl RunState {
    /// Whether the attempt has ended. Terminal does not imply retry eligibility.
    pub fn is_terminal(self) -> bool {
        !matches!(
            self,
            Self::Queued | Self::Running | Self::Validating | Self::CleaningUp
        )
    }
}

/// `Finished` means cancel returned successfully, not proof of external rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupStatus {
    /// No cleanup was necessary for this completed attempt.
    NotNeeded,
    /// The child cancellation hook is currently being awaited.
    InProgress,
    /// The cancellation hook returned successfully.
    Finished,
    /// The cancellation hook returned an error.
    Failed(String),
    /// The cleanup grace period expired.
    TimedOut,
    /// The cancellation hook panicked.
    Panicked,
    /// Worker destruction prevented confirmation of asynchronous cleanup.
    Unknown,
}

/// Immutable copy of the runtime's current record for a task.
#[derive(Clone, Debug)]
pub struct RunSnapshot {
    /// Unique host attempt identity.
    pub run_id: String,
    /// Handoff associated with the latest admitted attempt.
    pub handoff: MonadHandoff,
    /// Current lifecycle state.
    pub state: RunState,
    /// Result retained after the attempt settles, if one was accepted.
    pub result: Option<MonadResult>,
    /// Host, lifecycle or cleanup error; can coexist with the original result.
    pub error: Option<MonadError>,
    /// Independently recorded cancellation-hook outcome.
    pub cleanup: CleanupStatus,
}

/// Business acceptance belongs to the caller; binding checks always run first.
#[async_trait]
pub trait CompletionValidator: Send + Sync {
    /// Accept or reject a result within the host's execution deadline.
    async fn validate(
        &self,
        handoff: &MonadHandoff,
        result: &MonadResult,
    ) -> Result<(), MonadError>;
}

/// Mandatory task binding checks, also usable as the default caller validator.
#[derive(Debug, Default)]
pub struct BindingValidator;

#[async_trait]
impl CompletionValidator for BindingValidator {
    async fn validate(
        &self,
        handoff: &MonadHandoff,
        result: &MonadResult,
    ) -> Result<(), MonadError> {
        if result.task_id() != &handoff.task_id {
            return Err(MonadError::new("result task identity mismatch"));
        }
        if let MonadResult::Completed {
            parent_task_id,
            result_slot,
            monad_id,
            ..
        } = result
        {
            if parent_task_id != &handoff.parent_task_id
                || result_slot != &handoff.result_slot
                || monad_id != &handoff.monad_id
            {
                return Err(MonadError::new(
                    "completion parent, slot or monad identity mismatch",
                ));
            }
        }
        Ok(())
    }
}

/// Shared process-local executor. Clones share admission, records and workers.
#[derive(Clone, Debug)]
pub struct MonadRuntime {
    config: MonadRuntimeConfig,
    records: Arc<Mutex<HashMap<TaskId, RunSnapshot>>>,
    permits: Arc<Semaphore>,
    admissions: Arc<Semaphore>,
    workers: Arc<Workers>,
}

#[derive(Debug, Default)]
struct Workers {
    active: Mutex<HashMap<String, CancellationToken>>,
    changed: Notify,
}

struct WorkerGuard {
    workers: Arc<Workers>,
    run_id: String,
    channel: Option<super::ChildChannel>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        // Claim's Drop records Abandoned first on unexpected worker destruction.
        // Normal completion reaches here only after validation and bounded cleanup.
        if let Some(channel) = &self.channel {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| channel.finish()));
        }
        self.workers.active.lock().remove(&self.run_id);
        self.workers.changed.notify_waiters();
    }
}

impl Default for MonadRuntime {
    fn default() -> Self {
        Self::new(MonadRuntimeConfig::default()).expect("valid default runtime config")
    }
}

impl MonadRuntime {
    /// Construct an independent runtime, rejecting zero or overflowing limits.
    pub fn new(config: MonadRuntimeConfig) -> Result<Self, MonadError> {
        let capacity = config
            .max_concurrency
            .checked_add(config.max_queued)
            .filter(|n| *n <= Semaphore::MAX_PERMITS)
            .ok_or_else(|| MonadError::new("invalid runtime capacity"))?;
        if config.max_concurrency == 0
            || config.max_records == 0
            || config.cleanup_timeout.is_zero()
        {
            return Err(MonadError::new(
                "concurrency, record capacity and cleanup timeout must be positive",
            ));
        }
        Ok(Self {
            permits: Arc::new(Semaphore::new(config.max_concurrency)),
            admissions: Arc::new(Semaphore::new(capacity)),
            config,
            records: Arc::new(Mutex::new(HashMap::new())),
            workers: Arc::new(Workers::default()),
        })
    }

    /// Fallback for standalone host tools; explicit host injection takes precedence.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<MonadRuntime>> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                Arc::new(
                    Self::new(MonadRuntimeConfig::default()).expect("valid default runtime config"),
                )
            })
            .clone()
    }

    /// Inspect the latest admitted attempt for a task.
    pub fn get(&self, task_id: &TaskId) -> Option<RunSnapshot> {
        self.records.lock().get(task_id).cloned()
    }

    /// Copy retained task records in stable task-ID order.
    pub fn snapshot(&self) -> Vec<RunSnapshot> {
        let mut snapshots: Vec<_> = self.records.lock().values().cloned().collect();
        snapshots.sort_by(|a, b| a.handoff.task_id.0.cmp(&b.handoff.task_id.0));
        snapshots
    }

    /// Cancel all currently registered workers without aborting their cleanup.
    /// Stop new host submissions before calling this as part of shutdown.
    pub fn cancel_all(&self) {
        for token in self.workers.active.lock().values() {
            token.cancel();
        }
    }

    /// Wait until all registered workers, including bounded cleanup, have settled.
    /// Does not cancel workers or prevent future submissions. Call after cancelling
    /// the parent or `cancel_all`, while the Tokio runtime is still alive.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.workers.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.workers.active.lock().is_empty() {
                return;
            }
            notified.await;
        }
    }

    /// Wait for a specific attempt's terminal snapshot, including cleanup status.
    /// Execution errors remain in the returned snapshot. Waiting timeout or dropping
    /// this future does not cancel the worker. A replaced attempt is an error, never
    /// silently followed. Zero timeout polls the current terminal state once.
    pub async fn wait_task(
        &self,
        task_id: &TaskId,
        run_id: &str,
        timeout: Duration,
    ) -> Result<RunSnapshot, MonadError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| MonadError::new("wait timeout is not representable"))?;
        loop {
            let notified = self.workers.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self
                .get(task_id)
                .ok_or_else(|| MonadError::new("task not found"))?;
            if snapshot.run_id != run_id {
                return Err(MonadError::new("task run identity mismatch"));
            }
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                return Err(MonadError::new(
                    "wait_task timed out; worker remains active",
                ));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(MonadError::new(
                    "wait_task timed out; worker remains active",
                ));
            }
        }
    }

    /// Cancel exactly this attempt; repeated cancellation of its terminal record
    /// succeeds. Unknown tasks and stale run IDs fail. Caller authorization belongs
    /// to the host; this method does not infer permission from parent identities.
    pub fn cancel_task(&self, task_id: &TaskId, run_id: &str) -> Result<(), MonadError> {
        let records = self.records.lock();
        let record = records
            .get(task_id)
            .ok_or_else(|| MonadError::new("task not found"))?;
        if record.run_id != run_id {
            return Err(MonadError::new("task run identity mismatch"));
        }
        if record.state.is_terminal() {
            return Ok(());
        }
        let workers = self.workers.active.lock();
        let token = workers
            .get(run_id)
            .ok_or_else(|| MonadError::new("task worker is unavailable"))?;
        token.cancel();
        Ok(())
    }

    fn claim(&self, handoff: &MonadHandoff, token: CancellationToken) -> Result<Claim, MonadError> {
        let mut records = self.records.lock();
        if let Some(previous) = records.get(&handoff.task_id) {
            if !previous.state.is_terminal() {
                return Err(MonadError::new("task already running or queued"));
            }
            if previous.state == RunState::Completed {
                return Err(MonadError::new("completed task cannot be retried"));
            }
            if previous.cleanup != CleanupStatus::Finished {
                return Err(MonadError::new(
                    "retry requires confirmed successful cleanup",
                ));
            }
            if handoff.attempt_id <= previous.handoff.attempt_id {
                return Err(MonadError::new("attempt must increase monotonically"));
            }
            if handoff.parent_task_id != previous.handoff.parent_task_id
                || handoff.monad_id != previous.handoff.monad_id
                || handoff.result_slot != previous.handoff.result_slot
            {
                return Err(MonadError::new("retry cannot change task binding"));
            }
        } else if records.len() >= self.config.max_records {
            return Err(MonadError::new("runtime record capacity exhausted"));
        }
        if records.values().any(|r| {
            r.handoff.task_id != handoff.task_id
                && r.handoff.parent_task_id == handoff.parent_task_id
                && r.handoff.result_slot == handoff.result_slot
        }) {
            return Err(MonadError::new(
                "result slot already owned by another task under this parent",
            ));
        }
        for dependency in &handoff.depends_on {
            if dependency == &handoff.task_id
                || !records
                    .get(dependency)
                    .is_some_and(|r| r.state == RunState::Completed)
            {
                return Err(MonadError::new("dependency is not completed"));
            }
        }
        let admission = self
            .admissions
            .clone()
            .try_acquire_owned()
            .map_err(|_| MonadError::new("runtime queue capacity exhausted"))?;
        let run_id = uuid::Uuid::now_v7().to_string();
        records.insert(
            handoff.task_id.clone(),
            RunSnapshot {
                run_id: run_id.clone(),
                handoff: handoff.clone(),
                state: RunState::Queued,
                result: None,
                error: None,
                cleanup: CleanupStatus::NotNeeded,
            },
        );
        // Publish record and token under the same record lock, so targeted cancel
        // cannot observe an admitted task without a corresponding worker token.
        self.workers.active.lock().insert(run_id.clone(), token);
        self.workers.changed.notify_waiters();
        Ok(Claim {
            runtime: self.clone(),
            task_id: handoff.task_id.clone(),
            _worker: WorkerGuard {
                workers: self.workers.clone(),
                run_id: run_id.clone(),
                channel: None,
            },
            run_id,
            settled: false,
            _admission: admission,
        })
    }

    /// One start/step attempt; no arbitrary iteration cap is added. The child owns
    /// its reasoning loop. Cancellation/timeout return a host-bound Cancelled result;
    /// lifecycle errors, invalid results, failed acceptance and panics return Err.
    pub async fn execute(
        &self,
        child: Box<dyn MonadLoop>,
        handoff: MonadHandoff,
        parent_cancel: CancellationToken,
        validator: Arc<dyn CompletionValidator>,
    ) -> Result<MonadRun, MonadError> {
        let spawned = self.spawn_worker(child, handoff, parent_cancel, validator)?;
        let _cancel_on_drop = spawned.token.drop_guard();
        spawned
            .handle
            .await
            .map_err(|e| MonadError::new(format!("monad worker failed: {e}")))?
    }

    /// Admit and spawn without waiting for completion. The parent token still owns
    /// the worker lifetime; receipt destruction does not cancel it. Requires a live
    /// Tokio runtime. Admission is bounded and shared with `execute`.
    pub fn submit(
        &self,
        child: Box<dyn MonadLoop>,
        handoff: MonadHandoff,
        parent_cancel: CancellationToken,
        validator: Arc<dyn CompletionValidator>,
    ) -> Result<MonadReceipt, MonadError> {
        let spawned = self.spawn_worker(child, handoff, parent_cancel, validator)?;
        // Tokio retains the detached owned worker; its claim and cancellation token
        // remain registered until cleanup settles, independently of this receipt.
        Ok(spawned.receipt)
    }

    fn spawn_worker(
        &self,
        child: Box<dyn MonadLoop>,
        handoff: MonadHandoff,
        parent_cancel: CancellationToken,
        validator: Arc<dyn CompletionValidator>,
    ) -> Result<SpawnedRun, MonadError> {
        let executor = tokio::runtime::Handle::try_current()
            .map_err(|_| MonadError::new("MonadRuntime requires an active Tokio runtime"))?;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(handoff.budget.timeout_ms))
            .filter(|_| handoff.budget.timeout_ms > 0)
            .ok_or_else(|| MonadError::new("timeout must be positive and representable"))?;
        let channel = std::panic::catch_unwind(AssertUnwindSafe(|| {
            TaskBinding::from_handoff(&handoff, child.monad_id())?;
            Ok::<_, MonadError>(child.collaboration_channel())
        }))
        .map_err(|_| MonadError::new("child identity panicked"))??;
        let token = parent_cancel.child_token();
        let mut claim = self.claim(&handoff, token.clone())?;
        claim._worker.channel = channel;
        let receipt = MonadReceipt {
            task_id: handoff.task_id.clone(),
            run_id: claim.run_id.clone(),
            attempt_id: handoff.attempt_id,
        };
        let runtime = self.clone();
        let worker_token = token.clone();
        let handle = executor.spawn(crate::diagnostics::inherit(async move {
            crate::diagnostics::in_monad(
                handoff.task_id.0.clone(),
                handoff.attempt_id,
                runtime.execute_inner(child, handoff, worker_token, validator, claim, deadline),
            )
            .await
        }));
        Ok(SpawnedRun {
            receipt,
            token,
            handle,
        })
    }

    async fn execute_inner(
        &self,
        mut child: Box<dyn MonadLoop>,
        handoff: MonadHandoff,
        parent_cancel: CancellationToken,
        validator: Arc<dyn CompletionValidator>,
        mut claim: Claim,
        deadline: Instant,
    ) -> Result<MonadRun, MonadError> {
        let mut permit = None;
        // This scoped future must be dropped before cancel borrows the child again.
        let outcome = {
            let work = async {
                permit = Some(
                    self.permits
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| (RunState::Failed, MonadError::new("runtime closed")))?,
                );
                claim.update(|r| r.state = RunState::Running);
                child
                    .start(handoff.clone())
                    .await
                    .map_err(|e| (RunState::Failed, e))?;
                let result = child.step().await.map_err(|e| (RunState::Failed, e))?;
                claim.update(|r| r.state = RunState::Validating);
                BindingValidator
                    .validate(&handoff, &result)
                    .await
                    .map_err(|e| (RunState::Rejected, e))?;
                validator
                    .validate(&handoff, &result)
                    .await
                    .map_err(|e| (RunState::Rejected, e))?;
                Ok::<_, (RunState, MonadError)>(result)
            };
            tokio::select! {
                biased;
                _ = parent_cancel.cancelled() => Err((RunState::Cancelled, MonadError::new("parent cancelled"))),
                _ = tokio::time::sleep_until(deadline) => Err((RunState::TimedOut, MonadError::new("monad timeout"))),
                result = AssertUnwindSafe(work).catch_unwind() => match result {
                    Ok(outcome) => outcome,
                    Err(_) => Err((RunState::Panicked, MonadError::new("monad execution or validator panicked"))),
                }
            }
        };
        let outcome = if parent_cancel.is_cancelled() {
            Err((RunState::Cancelled, MonadError::new("parent cancelled")))
        } else if Instant::now() >= deadline {
            Err((RunState::TimedOut, MonadError::new("monad timeout")))
        } else {
            outcome
        };
        let (state, result) = match outcome {
            Ok(result) => {
                let state = match result {
                    MonadResult::Completed { .. } => RunState::Completed,
                    MonadResult::WaitingContext { .. } => RunState::WaitingContext,
                    MonadResult::Failed { .. } => RunState::Failed,
                    MonadResult::Cancelled { .. } => RunState::Cancelled,
                };
                (state, Ok(result))
            }
            Err((state @ (RunState::Cancelled | RunState::TimedOut), error)) => (
                state,
                Ok(MonadResult::Cancelled {
                    task_id: handoff.task_id.clone(),
                    reason: error.0,
                }),
            ),
            Err((state, error)) => (state, Err(error)),
        };
        let mut cleanup_error = None;
        if state != RunState::Completed {
            claim.update(|r| {
                r.state = RunState::CleaningUp;
                r.cleanup = CleanupStatus::InProgress;
            });
            let cleanup = tokio::time::timeout(
                self.config.cleanup_timeout,
                AssertUnwindSafe(async { child.cancel().await }).catch_unwind(),
            )
            .await;
            let status = match cleanup {
                Ok(Ok(Ok(()))) => CleanupStatus::Finished,
                Ok(Ok(Err(error))) => CleanupStatus::Failed(error.0),
                Ok(Err(_)) => CleanupStatus::Panicked,
                Err(_) => CleanupStatus::TimedOut,
            };
            let detail = match &status {
                CleanupStatus::Failed(error) => Some(format!("cleanup failed: {error}")),
                CleanupStatus::TimedOut => Some(format!(
                    "cleanup timed out after {:?}",
                    self.config.cleanup_timeout
                )),
                CleanupStatus::Panicked => Some("cleanup panicked".to_string()),
                _ => None,
            };
            if let Some(detail) = detail {
                let original = match &result {
                    Err(error) => error.0.clone(),
                    Ok(
                        MonadResult::Cancelled { reason, .. }
                        | MonadResult::WaitingContext { reason, .. },
                    ) => reason.clone(),
                    Ok(MonadResult::Failed { error, .. }) => error.clone(),
                    Ok(MonadResult::Completed { .. }) => "completed".to_string(),
                };
                cleanup_error = Some(MonadError::new(format!("{state:?}: {original}; {detail}")));
            }
            claim.update(|r| r.cleanup = status);
        }
        // Keep concurrency capacity until bounded cleanup has ended.
        drop(permit);
        claim.update(|r| {
            r.state = state;
            r.result = result.as_ref().ok().cloned();
            r.error = cleanup_error
                .clone()
                .or_else(|| result.as_ref().err().cloned());
        });
        claim.settled = true;
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        result.map(|result| MonadRun {
            handoff,
            result,
            run_id: claim.run_id.clone(),
        })
    }
}

struct Claim {
    runtime: MonadRuntime,
    task_id: TaskId,
    run_id: String,
    settled: bool,
    _admission: OwnedSemaphorePermit,
    _worker: WorkerGuard,
}

impl Claim {
    fn update(&self, update: impl FnOnce(&mut RunSnapshot)) {
        if let Some(record) = self.runtime.records.lock().get_mut(&self.task_id) {
            if record.run_id == self.run_id {
                update(record);
                self.runtime.workers.changed.notify_waiters();
            }
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.settled {
            self.update(|r| {
                r.state = RunState::Abandoned;
                r.cleanup = CleanupStatus::Unknown;
                r.error = Some(MonadError::new(
                    "owned worker dropped; async cleanup not confirmed",
                ));
            });
        }
    }
}

#[cfg(test)]
mod tests;
