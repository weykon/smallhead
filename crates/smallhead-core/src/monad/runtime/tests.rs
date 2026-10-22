use super::*;
use crate::monad::{CatalogEntry, ResourceUsage};
use crate::tool::ToolKind;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Mode {
    Complete,
    WaitStart,
    WaitStep,
    PanicStart,
    PanicStep,
    FailStart,
    FailStep,
    Forge(u8),
    Waiting,
}

struct Child {
    id: super::super::MonadId,
    catalog: CatalogEntry,
    mode: Mode,
    handoff: Option<MonadHandoff>,
    cancelled: Arc<AtomicUsize>,
    cleanup: u8,
    cleanup_release: Option<Arc<Notify>>,
}

fn child(mode: Mode) -> (Box<dyn MonadLoop>, Arc<AtomicUsize>) {
    let cancelled = Arc::new(AtomicUsize::new(0));
    (
        Box::new(Child {
            id: super::super::MonadId::new("runtime-test"),
            catalog: CatalogEntry {
                summary: String::new(),
                visible_tools: vec![],
                keywords: vec![],
            },
            mode,
            handoff: None,
            cancelled: cancelled.clone(),
            cleanup: 0,
            cleanup_release: None,
        }),
        cancelled,
    )
}

#[async_trait]
impl MonadLoop for Child {
    fn monad_id(&self) -> &super::super::MonadId {
        &self.id
    }
    fn catalog_entry(&self) -> &CatalogEntry {
        &self.catalog
    }
    fn default_tool_set(&self) -> Vec<ToolKind> {
        vec![]
    }
    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.handoff = Some(handoff);
        match self.mode {
            Mode::WaitStart => std::future::pending().await,
            Mode::PanicStart => panic!("start panic"),
            Mode::FailStart => Err(MonadError::new("start failed")),
            _ => Ok(()),
        }
    }
    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        match self.mode {
            Mode::WaitStep => return std::future::pending().await,
            Mode::PanicStep => panic!("step panic"),
            Mode::FailStep => return Err(MonadError::new("step failed")),
            _ => {}
        }
        let mut h = self.handoff.clone().unwrap();
        if matches!(self.mode, Mode::Waiting) {
            return Ok(MonadResult::WaitingContext {
                task_id: h.task_id,
                reason: "more context".into(),
                missing: vec![],
            });
        }
        match self.mode {
            Mode::Forge(0) => h.task_id = TaskId::new(),
            Mode::Forge(1) => h.parent_task_id = Some(TaskId::new()),
            Mode::Forge(2) => h.result_slot.0 = "forged".into(),
            Mode::Forge(_) => h.monad_id.0 = "forged".into(),
            _ => {}
        }
        Ok(MonadResult::Completed {
            task_id: h.task_id,
            parent_task_id: h.parent_task_id,
            monad_id: h.monad_id,
            result_slot: h.result_slot,
            output: serde_json::json!({"ok":true}),
            evidence_refs: vec![],
            usage: ResourceUsage::default(),
            finished_at_ms: 0,
        })
    }
    async fn cancel(&mut self) -> Result<(), MonadError> {
        crate::diagnostics::emit(
            "execution",
            "cleanup_probe",
            serde_json::json!({"cleanup":true}),
        );
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        if let Some(release) = &self.cleanup_release {
            release.notified().await;
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
        match self.cleanup {
            1 => std::future::pending().await,
            2 => panic!("cleanup panic"),
            3 => Err(MonadError::new("cleanup failed")),
            _ => Ok(()),
        }
    }
}

fn handoff() -> MonadHandoff {
    super::super::test_support::lifecycle_handoff("runtime-test")
}

#[tokio::test]
async fn cleanup_diagnostics_preserve_child_scope_across_owned_worker() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    let id = h.task_id.0.clone();
    let (sink, mut records) = tokio::sync::mpsc::unbounded_channel();
    assert!(
        crate::diagnostics::observe(sink, execute(&runtime, h, Mode::FailStep))
            .await
            .is_err()
    );
    let event = records.recv().await.unwrap();
    assert_eq!(event.layer, "monad");
    assert_eq!(event.event, "child_event");
    assert_eq!(event.data["task_id"], id);
    assert_eq!(event.data["event"], "cleanup_probe");
}

async fn execute(
    runtime: &MonadRuntime,
    h: MonadHandoff,
    mode: Mode,
) -> Result<MonadRun, MonadError> {
    runtime
        .execute(
            child(mode).0,
            h,
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .await
}

async fn wait_state(runtime: &MonadRuntime, id: &TaskId, state: RunState) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime.get(id).is_some_and(|s| s.state == state) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("state transition timed out");
}

fn spawn(
    runtime: Arc<MonadRuntime>,
    h: MonadHandoff,
    mode: Mode,
    token: CancellationToken,
) -> tokio::task::JoinHandle<Result<MonadRun, MonadError>> {
    tokio::spawn(async move {
        runtime
            .execute(child(mode).0, h, token, Arc::new(BindingValidator))
            .await
    })
}

#[test]
fn config_and_shared_identity() {
    assert_eq!(MonadRuntimeConfig::default().max_concurrency, 2);
    assert_eq!(
        MonadRuntimeConfig::default().cleanup_timeout,
        Duration::from_secs(8)
    );
    assert!(Arc::ptr_eq(
        &MonadRuntime::shared(),
        &MonadRuntime::shared()
    ));
    assert!(MonadRuntime::new(MonadRuntimeConfig {
        max_concurrency: 0,
        ..Default::default()
    })
    .is_err());
    assert!(MonadRuntime::new(MonadRuntimeConfig {
        max_queued: usize::MAX,
        ..Default::default()
    })
    .is_err());
    assert!(MonadRuntime::new(MonadRuntimeConfig {
        max_records: 0,
        ..Default::default()
    })
    .is_err());
    assert!(MonadRuntime::new(MonadRuntimeConfig {
        cleanup_timeout: Duration::ZERO,
        ..Default::default()
    })
    .is_err());
}

#[tokio::test]
async fn attempts_slots_dependencies_and_bounded_retention() {
    let runtime = MonadRuntime::new(MonadRuntimeConfig {
        max_records: 2,
        ..Default::default()
    })
    .unwrap();
    let first = handoff();
    let mut dependent = handoff();
    dependent.parent_task_id = first.parent_task_id.clone();
    dependent.result_slot.0 = "dependent".into();
    dependent.depends_on = vec![first.task_id.clone()];
    assert!(execute(&runtime, dependent.clone(), Mode::Complete)
        .await
        .is_err());
    let initial = execute(&runtime, first.clone(), Mode::Waiting)
        .await
        .unwrap();
    assert_eq!(initial.handoff, first);
    assert!(execute(&runtime, first.clone(), Mode::Complete)
        .await
        .is_err());
    let mut retry = first.clone();
    retry.attempt_id = 2;
    let mut rebound = retry.clone();
    rebound.result_slot.0 = "elsewhere".into();
    assert!(execute(&runtime, rebound, Mode::Complete).await.is_err());
    let next = execute(&runtime, retry.clone(), Mode::Complete)
        .await
        .unwrap();
    assert_ne!(initial.run_id, next.run_id);
    retry.attempt_id += 1;
    assert!(execute(&runtime, retry, Mode::Complete)
        .await
        .unwrap_err()
        .0
        .contains("completed task"));
    let mut collision = first.clone();
    collision.task_id = TaskId::new();
    assert!(execute(&runtime, collision, Mode::Complete).await.is_err());
    execute(&runtime, dependent, Mode::Complete).await.unwrap();
    assert!(execute(&runtime, handoff(), Mode::Complete).await.is_err());
    assert_eq!(runtime.snapshot().len(), 2);
}

#[tokio::test]
async fn duplicate_claim_rejected_while_running_and_retry_after_terminal() {
    let runtime = Arc::new(MonadRuntime::default());
    let h = handoff();
    let cancel = CancellationToken::new();
    let task = spawn(runtime.clone(), h.clone(), Mode::WaitStep, cancel.clone());
    wait_state(&runtime, &h.task_id, RunState::Running).await;
    let mut retry = h.clone();
    retry.attempt_id += 1;
    assert!(execute(&runtime, retry.clone(), Mode::Complete)
        .await
        .is_err());
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap().unwrap().result,
        MonadResult::Cancelled { .. }
    ));
    execute(&runtime, retry, Mode::Complete).await.unwrap();
}

#[tokio::test]
async fn concurrency_queue_bound_and_waiting_cancellation() {
    let runtime = Arc::new(
        MonadRuntime::new(MonadRuntimeConfig {
            max_concurrency: 2,
            max_queued: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let token = CancellationToken::new();
    let first = handoff();
    let second = handoff();
    let a = spawn(
        runtime.clone(),
        first.clone(),
        Mode::WaitStep,
        token.clone(),
    );
    let b = spawn(
        runtime.clone(),
        second.clone(),
        Mode::WaitStep,
        token.clone(),
    );
    wait_state(&runtime, &first.task_id, RunState::Running).await;
    wait_state(&runtime, &second.task_id, RunState::Running).await;
    let queued = handoff();
    let queue_cancel = CancellationToken::new();
    let c = spawn(
        runtime.clone(),
        queued.clone(),
        Mode::Complete,
        queue_cancel.clone(),
    );
    wait_state(&runtime, &queued.task_id, RunState::Queued).await;
    assert!(execute(&runtime, handoff(), Mode::Complete).await.is_err());
    queue_cancel.cancel();
    c.await.unwrap().unwrap();
    assert_eq!(
        runtime.get(&queued.task_id).unwrap().state,
        RunState::Cancelled
    );
    token.cancel();
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    execute(&runtime, handoff(), Mode::Complete).await.unwrap();
}

#[tokio::test]
async fn parent_cancellation_covers_start_and_step_and_calls_cleanup() {
    for mode in [Mode::WaitStart, Mode::WaitStep] {
        let runtime = Arc::new(MonadRuntime::default());
        let h = handoff();
        let parent = CancellationToken::new();
        let token = parent.child_token();
        let (child, cancelled) = child(mode);
        let r = runtime.clone();
        let input = h.clone();
        let task = tokio::spawn(async move {
            r.execute(child, input, token, Arc::new(BindingValidator))
                .await
        });
        wait_state(&runtime, &h.task_id, RunState::Running).await;
        parent.cancel();
        task.await.unwrap().unwrap();
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.get(&h.task_id).unwrap().cleanup,
            CleanupStatus::Finished
        );
    }
}

struct Validator(u8, Arc<Notify>);
#[async_trait]
impl CompletionValidator for Validator {
    async fn validate(&self, _: &MonadHandoff, _: &MonadResult) -> Result<(), MonadError> {
        self.1.notify_one();
        match self.0 {
            0 => Err(MonadError::new("semantic acceptance failed")),
            1 => std::future::pending().await,
            _ => panic!("validator panic"),
        }
    }
}

#[tokio::test]
async fn identity_cannot_be_forged_and_semantic_acceptance_can_fail() {
    let runtime = MonadRuntime::default();
    for index in 0..4 {
        let h = handoff();
        assert!(execute(&runtime, h.clone(), Mode::Forge(index))
            .await
            .is_err());
        assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Rejected);
    }
    let h = handoff();
    assert!(runtime
        .execute(
            child(Mode::Complete).0,
            h.clone(),
            CancellationToken::new(),
            Arc::new(Validator(0, Arc::new(Notify::new())))
        )
        .await
        .is_err());
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Rejected);
    let mut dependent = handoff();
    dependent.depends_on = vec![h.task_id];
    assert!(execute(&runtime, dependent, Mode::Complete).await.is_err());
}

#[tokio::test]
async fn cancellation_covers_validation() {
    let runtime = Arc::new(MonadRuntime::default());
    let h = handoff();
    let cancel = CancellationToken::new();
    let entered = Arc::new(Notify::new());
    let validator = Arc::new(Validator(1, entered.clone()));
    let r = runtime.clone();
    let input = h.clone();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        r.execute(child(Mode::Complete).0, input, token, validator)
            .await
    });
    entered.notified().await;
    cancel.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn timeout_covers_start_step_validation_and_waiting() {
    for mode in [Mode::WaitStart, Mode::WaitStep] {
        let runtime = MonadRuntime::default();
        let mut h = handoff();
        h.budget.timeout_ms = 10;
        execute(&runtime, h.clone(), mode).await.unwrap();
        assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::TimedOut);
    }
    let runtime = MonadRuntime::default();
    let mut h = handoff();
    h.budget.timeout_ms = 10;
    runtime
        .execute(
            child(Mode::Complete).0,
            h.clone(),
            CancellationToken::new(),
            Arc::new(Validator(1, Arc::new(Notify::new()))),
        )
        .await
        .unwrap();
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::TimedOut);
    let runtime = Arc::new(
        MonadRuntime::new(MonadRuntimeConfig {
            max_concurrency: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let first = handoff();
    let cancel = CancellationToken::new();
    let active = spawn(
        runtime.clone(),
        first.clone(),
        Mode::WaitStep,
        cancel.clone(),
    );
    wait_state(&runtime, &first.task_id, RunState::Running).await;
    let mut queued = handoff();
    queued.budget.timeout_ms = 10;
    execute(&runtime, queued.clone(), Mode::Complete)
        .await
        .unwrap();
    assert_eq!(
        runtime.get(&queued.task_id).unwrap().state,
        RunState::TimedOut
    );
    cancel.cancel();
    active.await.unwrap().unwrap();
    let mut zero = handoff();
    zero.budget.timeout_ms = 0;
    assert!(execute(&runtime, zero.clone(), Mode::Complete)
        .await
        .is_err());
    assert!(runtime.get(&zero.task_id).is_none());
}

#[tokio::test]
async fn panics_and_lifecycle_errors_do_not_leave_running_claims() {
    let runtime = MonadRuntime::default();
    for (mode, expected) in [
        (Mode::PanicStart, RunState::Panicked),
        (Mode::PanicStep, RunState::Panicked),
        (Mode::FailStart, RunState::Failed),
        (Mode::FailStep, RunState::Failed),
    ] {
        let h = handoff();
        assert!(execute(&runtime, h.clone(), mode).await.is_err());
        let snapshot = runtime.get(&h.task_id).unwrap();
        assert_eq!(snapshot.state, expected);
        assert_eq!(snapshot.cleanup, CleanupStatus::Finished);
        let mut retry = h;
        retry.attempt_id += 1;
        execute(&runtime, retry, Mode::Complete).await.unwrap();
    }
    let h = handoff();
    assert!(runtime
        .execute(
            child(Mode::Complete).0,
            h.clone(),
            CancellationToken::new(),
            Arc::new(Validator(2, Arc::new(Notify::new())))
        )
        .await
        .is_err());
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Panicked);
}

#[tokio::test(start_paused = true)]
async fn cleanup_is_bounded_and_reports_failure_honestly() {
    for (cleanup, expected) in [
        (1, CleanupStatus::TimedOut),
        (2, CleanupStatus::Panicked),
        (3, CleanupStatus::Failed("cleanup failed".into())),
    ] {
        for (mode, state, original_reason, pre_cancel) in [
            (Mode::FailStep, RunState::Failed, "step failed", false),
            (
                Mode::Complete,
                RunState::Cancelled,
                "parent cancelled",
                true,
            ),
            (Mode::WaitStep, RunState::TimedOut, "monad timeout", false),
            (
                Mode::Waiting,
                RunState::WaitingContext,
                "more context",
                false,
            ),
        ] {
            let runtime = MonadRuntime::default();
            let mut h = handoff();
            h.budget.timeout_ms = 10;
            let actor = Child {
                id: h.monad_id.clone(),
                catalog: CatalogEntry {
                    summary: String::new(),
                    visible_tools: vec![],
                    keywords: vec![],
                },
                mode,
                handoff: None,
                cancelled: Arc::new(AtomicUsize::new(0)),
                cleanup,
                cleanup_release: None,
            };
            let token = CancellationToken::new();
            if pre_cancel {
                token.cancel();
            }
            let error = runtime
                .execute(
                    Box::new(actor),
                    h.clone(),
                    token,
                    Arc::new(BindingValidator),
                )
                .await
                .unwrap_err();
            assert!(error.0.contains(original_reason), "{}", error.0);
            assert!(error.0.contains("cleanup"), "{}", error.0);
            assert_eq!(runtime.get(&h.task_id).unwrap().cleanup, expected);
            let before = runtime.get(&h.task_id).unwrap();
            assert_eq!(before.state, state);
            assert_eq!(before.error.as_ref(), Some(&error));
            if state != RunState::Failed {
                assert!(before.result.is_some());
            }
            let mut retry = h.clone();
            retry.attempt_id += 1;
            assert!(execute(&runtime, retry, Mode::Complete)
                .await
                .unwrap_err()
                .0
                .contains("cleanup"));
            assert_eq!(runtime.get(&h.task_id).unwrap().run_id, before.run_id);
        }
    }
}

#[tokio::test]
async fn abandoned_claim_with_unknown_cleanup_cannot_be_replayed() {
    let runtime = MonadRuntime::default();
    let mut h = handoff();
    drop(runtime.claim(&h, CancellationToken::new()).unwrap());
    let snapshot = runtime.get(&h.task_id).unwrap();
    assert_eq!(snapshot.state, RunState::Abandoned);
    assert_eq!(snapshot.cleanup, CleanupStatus::Unknown);
    h.attempt_id += 1;
    assert!(execute(&runtime, h, Mode::Complete)
        .await
        .unwrap_err()
        .0
        .contains("cleanup"));
}

#[tokio::test]
async fn dropped_execute_cancels_owned_worker_and_wait_idle_awaits_cleanup() {
    let runtime = Arc::new(MonadRuntime::default());
    let mut h = handoff();
    let cancelled = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let actor = Child {
        id: h.monad_id.clone(),
        catalog: CatalogEntry {
            summary: String::new(),
            visible_tools: vec![],
            keywords: vec![],
        },
        mode: Mode::WaitStep,
        handoff: None,
        cancelled: cancelled.clone(),
        cleanup: 0,
        cleanup_release: Some(release.clone()),
    };
    let r = runtime.clone();
    let input = h.clone();
    let task = tokio::spawn(async move {
        r.execute(
            Box::new(actor),
            input,
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .await
    });
    wait_state(&runtime, &h.task_id, RunState::Running).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_state(&runtime, &h.task_id, RunState::CleaningUp).await;
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    let idle = runtime.wait_idle();
    tokio::pin!(idle);
    assert!(idle.as_mut().now_or_never().is_none());
    let mut retry = h.clone();
    retry.attempt_id += 1;
    assert!(execute(&runtime, retry, Mode::Complete).await.is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), idle)
        .await
        .unwrap();
    let snapshot = runtime.get(&h.task_id).unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 2);
    assert_eq!(snapshot.state, RunState::Cancelled);
    assert_eq!(snapshot.cleanup, CleanupStatus::Finished);
    h.attempt_id += 1;
    execute(&runtime, h, Mode::Complete).await.unwrap();
}

#[tokio::test]
async fn cancel_all_and_multiple_idle_waiters_include_detached_queued_workers() {
    let runtime = Arc::new(
        MonadRuntime::new(MonadRuntimeConfig {
            max_concurrency: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    let active = handoff();
    let queued = handoff();
    let running = spawn(
        runtime.clone(),
        active.clone(),
        Mode::WaitStep,
        CancellationToken::new(),
    );
    wait_state(&runtime, &active.task_id, RunState::Running).await;
    let waiting = spawn(
        runtime.clone(),
        queued.clone(),
        Mode::Complete,
        CancellationToken::new(),
    );
    wait_state(&runtime, &queued.task_id, RunState::Queued).await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let a = runtime.wait_idle();
    let b = runtime.wait_idle();
    tokio::pin!(a, b);
    assert!(a.as_mut().now_or_never().is_none());
    assert!(b.as_mut().now_or_never().is_none());
    runtime.cancel_all();
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(a, b);
    })
    .await
    .unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(
        runtime.get(&active.task_id).unwrap().state,
        RunState::Cancelled
    );
    assert_eq!(
        runtime.get(&queued.task_id).unwrap().state,
        RunState::Cancelled
    );
    runtime.wait_idle().await;
}

#[tokio::test]
async fn waiting_context_is_not_a_completed_dependency() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    execute(&runtime, h.clone(), Mode::Waiting).await.unwrap();
    let mut dependent = handoff();
    dependent.depends_on = vec![h.task_id.clone()];
    assert!(execute(&runtime, dependent.clone(), Mode::Complete)
        .await
        .is_err());
    let mut retry = h;
    retry.attempt_id += 1;
    execute(&runtime, retry, Mode::Complete).await.unwrap();
    execute(&runtime, dependent, Mode::Complete).await.unwrap();
}

#[tokio::test]
async fn submit_receipt_drop_keeps_worker_alive_until_parent_cancel() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    let parent = CancellationToken::new();
    let (actor, cleanup) = child(Mode::WaitStep);
    let receipt = runtime
        .submit(actor, h.clone(), parent.clone(), Arc::new(BindingValidator))
        .unwrap();
    assert_eq!(receipt.task_id, h.task_id);
    assert_eq!(receipt.attempt_id, h.attempt_id);
    let run_id = receipt.run_id.clone();
    drop(receipt);
    wait_state(&runtime, &h.task_id, RunState::Running).await;
    assert!(runtime
        .wait_task(&h.task_id, &run_id, Duration::ZERO)
        .await
        .is_err());
    assert_eq!(cleanup.load(Ordering::SeqCst), 0);
    parent.cancel();
    let result = runtime
        .wait_task(&h.task_id, &run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(result.state, RunState::Cancelled);
    assert_eq!(result.cleanup, CleanupStatus::Finished);
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
    runtime.wait_idle().await;
}

#[tokio::test(start_paused = true)]
async fn wait_timeout_and_wait_drop_never_cancel_worker() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    let receipt = runtime
        .submit(
            child(Mode::WaitStep).0,
            h.clone(),
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    assert!(runtime
        .wait_task(&h.task_id, &receipt.run_id, Duration::from_millis(5))
        .await
        .unwrap_err()
        .0
        .contains("wait_task timed out"));
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Running);
    {
        let wait = runtime.wait_task(&h.task_id, &receipt.run_id, Duration::from_secs(1));
        tokio::pin!(wait);
        assert!(wait.as_mut().now_or_never().is_none());
    }
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Running);
    runtime.cancel_task(&h.task_id, &receipt.run_id).unwrap();
    runtime
        .wait_task(&h.task_id, &receipt.run_id, Duration::from_secs(1))
        .await
        .unwrap();
    runtime.cancel_task(&h.task_id, &receipt.run_id).unwrap();
    runtime.wait_idle().await;
}

#[tokio::test]
async fn submit_and_execute_share_admission_and_targeted_cancel_is_isolated() {
    let runtime = MonadRuntime::new(MonadRuntimeConfig {
        max_concurrency: 1,
        max_queued: 1,
        ..Default::default()
    })
    .unwrap();
    let a = handoff();
    let b = handoff();
    let first = runtime
        .submit(
            child(Mode::WaitStep).0,
            a.clone(),
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    let second = runtime
        .submit(
            child(Mode::Complete).0,
            b.clone(),
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    assert!(runtime
        .submit(
            child(Mode::Complete).0,
            handoff(),
            CancellationToken::new(),
            Arc::new(BindingValidator)
        )
        .is_err());
    assert!(execute(&runtime, handoff(), Mode::Complete).await.is_err());
    assert!(runtime.cancel_task(&a.task_id, &second.run_id).is_err());
    assert!(runtime.cancel_task(&TaskId::new(), &first.run_id).is_err());
    runtime.cancel_task(&b.task_id, &second.run_id).unwrap();
    let stopped = runtime
        .wait_task(&b.task_id, &second.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(stopped.state, RunState::Cancelled);
    assert!(!runtime.get(&a.task_id).unwrap().state.is_terminal());
    runtime.cancel_task(&a.task_id, &first.run_id).unwrap();
    runtime.wait_idle().await;
}

#[tokio::test]
async fn waiters_are_notified_and_old_attempt_cannot_observe_or_cancel_retry() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    let first = runtime
        .submit(
            child(Mode::WaitStep).0,
            h.clone(),
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    let wait_a = runtime.wait_task(&h.task_id, &first.run_id, Duration::from_secs(2));
    let wait_b = runtime.wait_task(&h.task_id, &first.run_id, Duration::from_secs(2));
    tokio::pin!(wait_a, wait_b);
    assert!(wait_a.as_mut().now_or_never().is_none());
    assert!(wait_b.as_mut().now_or_never().is_none());
    runtime.cancel_task(&h.task_id, &first.run_id).unwrap();
    runtime.wait_idle().await;
    assert_eq!(wait_a.await.unwrap().state, RunState::Cancelled);
    // Leave wait_b suspended across admission of a replacement attempt.
    let mut retry = h.clone();
    retry.attempt_id += 1;
    let second = runtime
        .submit(
            child(Mode::WaitStep).0,
            retry,
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    assert!(wait_b.await.unwrap_err().0.contains("identity"));
    assert!(runtime
        .cancel_task(&h.task_id, &first.run_id)
        .unwrap_err()
        .0
        .contains("identity"));
    assert!(runtime
        .wait_task(&h.task_id, &first.run_id, Duration::ZERO)
        .await
        .is_err());
    assert!(runtime
        .wait_task(&TaskId::new(), &first.run_id, Duration::ZERO)
        .await
        .is_err());
    runtime.cancel_task(&h.task_id, &second.run_id).unwrap();
    runtime.wait_idle().await;
    let mut completed_handoff = h.clone();
    completed_handoff.attempt_id = 3;
    let done = runtime
        .submit(
            child(Mode::Complete).0,
            completed_handoff,
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    let terminal = runtime
        .wait_task(&h.task_id, &done.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(terminal.state, RunState::Completed);
    assert_eq!(
        runtime
            .wait_task(&h.task_id, &done.run_id, Duration::ZERO)
            .await
            .unwrap()
            .run_id,
        done.run_id
    );
}

#[tokio::test]
async fn submitted_panic_and_cleanup_error_remain_queryable() {
    let runtime = MonadRuntime::default();
    let h = handoff();
    let actor = Child {
        id: h.monad_id.clone(),
        catalog: CatalogEntry {
            summary: String::new(),
            visible_tools: vec![],
            keywords: vec![],
        },
        mode: Mode::PanicStep,
        handoff: None,
        cancelled: Arc::new(AtomicUsize::new(0)),
        cleanup: 3,
        cleanup_release: None,
    };
    let receipt = runtime
        .submit(
            Box::new(actor),
            h.clone(),
            CancellationToken::new(),
            Arc::new(BindingValidator),
        )
        .unwrap();
    let snapshot = runtime
        .wait_task(&h.task_id, &receipt.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(snapshot.state, RunState::Panicked);
    assert!(matches!(snapshot.cleanup, CleanupStatus::Failed(_)));
    assert!(snapshot.error.unwrap().0.contains("cleanup failed"));
    runtime.wait_idle().await;
}

struct WithChannel(Box<dyn MonadLoop>, super::super::ChildChannel);

#[async_trait]
impl MonadLoop for WithChannel {
    fn monad_id(&self) -> &super::super::MonadId {
        self.0.monad_id()
    }
    fn catalog_entry(&self) -> &CatalogEntry {
        self.0.catalog_entry()
    }
    fn default_tool_set(&self) -> Vec<ToolKind> {
        self.0.default_tool_set()
    }
    fn collaboration_channel(&self) -> Option<super::super::ChildChannel> {
        Some(self.1.clone())
    }
    async fn start(&mut self, h: MonadHandoff) -> Result<(), MonadError> {
        self.0.start(h).await
    }
    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        self.0.step().await
    }
    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.0.cancel().await
    }
}

#[tokio::test]
async fn collaboration_finishes_only_after_validation_and_cleanup() {
    use super::super::{CollaborationPhase, Hub};
    let runtime = MonadRuntime::default();
    let h = handoff();
    let hub = Hub::default();
    let channel = hub
        .register(
            h.parent_task_id.clone().unwrap(),
            h.task_id.clone(),
            h.attempt_id,
        )
        .unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let actor = Child {
        id: h.monad_id.clone(),
        catalog: CatalogEntry {
            summary: String::new(),
            visible_tools: vec![],
            keywords: vec![],
        },
        mode: Mode::Complete,
        handoff: None,
        cancelled: Arc::new(AtomicUsize::new(0)),
        cleanup: 0,
        cleanup_release: Some(release.clone()),
    };
    let receipt = runtime
        .submit(
            Box::new(WithChannel(Box::new(actor), channel.clone())),
            h.clone(),
            CancellationToken::new(),
            Arc::new(Validator(1, entered.clone())),
        )
        .unwrap();
    entered.notified().await;
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Validating);
    assert_ne!(channel.snapshot().phase, CollaborationPhase::Ended);
    runtime.cancel_task(&h.task_id, &receipt.run_id).unwrap();
    wait_state(&runtime, &h.task_id, RunState::CleaningUp).await;
    assert_ne!(channel.snapshot().phase, CollaborationPhase::Ended);
    let parent = h.parent_task_id.as_ref().unwrap();
    let cursor = hub.cursor(parent);
    let end_event = hub.wait_changed(parent, cursor, Duration::from_secs(2));
    tokio::pin!(end_event);
    assert!(end_event.as_mut().now_or_never().is_none());
    release.notify_one();
    assert!(end_event.await > cursor);
    // Parent must be able to consume the result directly on notification, without
    // first joining the worker or waiting for all unrelated tasks to become idle.
    assert_eq!(channel.snapshot().phase, CollaborationPhase::Ended);
    let settled = runtime.get(&h.task_id).unwrap();
    assert_eq!(settled.run_id, receipt.run_id);
    assert_eq!(settled.state, RunState::Cancelled);
    assert_eq!(settled.cleanup, CleanupStatus::Finished);
    assert!(settled.error.is_none());
    assert!(
        matches!(settled.result, Some(MonadResult::Cancelled { task_id, .. }) if task_id == h.task_id)
    );
    runtime.wait_idle().await;

    let h = handoff();
    let channel = hub
        .register(h.parent_task_id.clone().unwrap(), h.task_id.clone(), 1)
        .unwrap();
    let mut claim = runtime.claim(&h, CancellationToken::new()).unwrap();
    claim._worker.channel = Some(channel.clone());
    drop(claim);
    assert_eq!(runtime.get(&h.task_id).unwrap().state, RunState::Abandoned);
    assert_eq!(channel.snapshot().phase, CollaborationPhase::Ended);
}
