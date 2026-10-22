//! [`ProcessManager`] — the single source of truth for managed OS processes.
//!
//! This manager provides:
//! - registry of [`ProcessRecord`] keyed by [`ExecutionId`]
//! - validation + log-dir allocation + registration, then `tokio::spawn`
//!   of the internal `Executor`
//! - enforced state machine via `mark_*` helpers
//! - read-only `inspect` / `list` / `list_by_state`
//! - `wait_for` + `stop` public APIs backed by a Notify registry
//! - per-execution `LogStore` that hands a `LogHandle`
//!   to the executor and writes stdout/stderr logs + an atomic `meta.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::Notify;

use crate::error::ProcessError;
use crate::executor::Executor;
use crate::id::{ExecutionId, ProcessId};
use crate::persist::{LogHandle, LogStore};
use crate::record::{ExecutionMetadata, ProcessRecord};
use crate::spec::ProcessSpec;
#[cfg(test)]
use crate::state::ProcessExitInfo;
use crate::state::ProcessState;

/// Default root for execution logs (one subdirectory per `ExecutionId`).
///
/// Resolved once at construction time. The directory is created eagerly so
/// `log_root()` never observes a non-existent path; tests inject a
/// `tempfile::TempDir` path via [`ProcessManager::with_log_root`] instead.
const DEFAULT_LOG_ROOT: &str = ".small-head/runs";

/// In-memory managed-process registry + state-machine driver.
///
/// Cheap to clone (`Arc<Inner>`); share one instance across the Agent
/// tool layer, the TUI, and the ServiceManager. The internal
/// `parking_lot::RwLock` is **synchronous** — any `await` happens after
/// the lock is dropped, so a slow cancel future can't stall readers.
#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Inner>,
}

struct Inner {
    _root_lock: std::fs::File,
    admission: parking_lot::Mutex<()>,
    events: tokio::sync::broadcast::Sender<serde_json::Value>,
    audit_errors: RwLock<HashMap<ExecutionId, String>>,
    limits: crate::ProcessLimits,
    slots: Arc<tokio::sync::Semaphore>,
    closing: std::sync::atomic::AtomicBool,
    /// Registry of retained executions. Admission is bounded by max_records;
    /// explicit purge removes completed records and their logs.
    registry: RwLock<HashMap<ExecutionId, ProcessRecord>>,
    /// Per-execution `Notify` so multiple parties can `wait_for` the same
    /// id. Entries are removed together with records by explicit purge.
    waiters: RwLock<HashMap<ExecutionId, Arc<Notify>>>,
    /// Per-execution log persistence layer. Owns the root directory and
    /// mints per-id `LogHandle`s that the executor streams stdout/stderr
    /// into. The root is created eagerly by `with_log_root` so a failed
    /// directory creation surfaces here, not in the executor hot path.
    log_store: LogStore,
}

impl ProcessManager {
    /// Construct a manager rooted at `$HOME/.small-head/runs` (creating
    /// it if necessary). Falls back to `.small-head/runs` (cwd-relative)
    /// when `HOME` is unset so the constructor doesn't panic in stripped
    /// containers.
    pub fn new() -> Result<Self, ProcessError> {
        let root = match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(DEFAULT_LOG_ROOT),
            _ => PathBuf::from(DEFAULT_LOG_ROOT),
        };
        Self::with_log_root(root)
    }

    /// Construct a manager with a caller-chosen log root. Used by tests
    /// (pass a `tempfile::TempDir` path) and by applications that want
    /// to relocate the runs directory.
    pub fn with_log_root(path: PathBuf) -> Result<Self, ProcessError> {
        Self::with_limits(path, crate::ProcessLimits::default())
    }

    /// 使用显式预算创建管理器。所有克隆共享并发名额与关闭状态。
    pub fn with_limits(path: PathBuf, limits: crate::ProcessLimits) -> Result<Self, ProcessError> {
        limits.validate()?;
        let root_lock = crate::storage::lock_root(&path)?;
        let recovered = crate::storage::recover(&path, &limits)?;
        let log_store = LogStore::new(path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                _root_lock: root_lock,
                admission: parking_lot::Mutex::new(()),
                events: tokio::sync::broadcast::channel(256).0,
                audit_errors: RwLock::new(HashMap::new()),
                slots: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent)),
                limits,
                closing: std::sync::atomic::AtomicBool::new(false),
                registry: RwLock::new(recovered),
                waiters: RwLock::new(HashMap::new()),
                log_store,
            }),
        })
    }

    /// Allocate a fresh [`ExecutionId`], register, validate, open the
    /// per-id log directory via internal `LogStore::open`,
    /// register a record in [`ProcessState::Queued`], spawn
    /// internal `Executor::run`, and return the id.
    ///
    /// Refusals return an ID whose record is `Rejected`; unavailable storage
    /// prevents spawn and leaves an in-memory refusal. Inspect/wait for the
    /// observed result rather than assuming an accepted ID means success.
    ///
    /// The executor runs **independently** of the caller — dropping the
    /// future returned by `start()` does **not** cancel the execution,
    /// because the child is owned by `ProcessManager` (design §5). Use
    /// [`ProcessManager::stop`] for an explicit cancel.
    pub async fn start(&self, spec: ProcessSpec) -> Result<ExecutionId, ProcessError> {
        self.start_owned(spec, None).await
    }

    /// 启动并绑定所属 Task/Service；环境变量只用于执行，不写入日志。
    pub async fn start_owned(
        &self,
        spec: ProcessSpec,
        owner: Option<String>,
    ) -> Result<ExecutionId, ProcessError> {
        self.start_with_metadata(spec, owner, ExecutionMetadata::default())
            .await
    }

    /// Start with structured intent and completion metadata.
    ///
    /// The manager persists this metadata with the execution record and events,
    /// but lifecycle transitions still depend only on the OS process and
    /// manager policy.
    pub async fn start_with_metadata(
        &self,
        spec: ProcessSpec,
        owner: Option<String>,
        metadata: ExecutionMetadata,
    ) -> Result<ExecutionId, ProcessError> {
        self.start_internal(spec, owner, metadata, false)
    }

    pub(crate) fn start_internal(
        &self,
        mut spec: ProcessSpec,
        owner: Option<String>,
        metadata: ExecutionMetadata,
        service: bool,
    ) -> Result<ExecutionId, ProcessError> {
        let _admission = self.inner.admission.lock();
        if spec.command_display().len()
            + spec
                .env
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
            + spec.description.as_ref().map_or(0, String::len)
            + owner.as_ref().map_or(0, String::len)
            > 16384
        {
            return Err(ProcessError::InvalidSpec(
                "命令及参数超过 16 KiB 审计上限，未执行".into(),
            ));
        }
        if self.inner.registry.read().len() >= self.inner.limits.max_records {
            return Err(ProcessError::Internal(
                "历史记录已满；清理已完成记录后重试，未执行".into(),
            ));
        }
        crate::storage::available(
            self.log_root(),
            &self.inner.registry.read(),
            &self.inner.limits,
        )?;
        spec.timeout
            .get_or_insert(self.inner.limits.default_timeout);
        if service {
            spec.timeout = None;
        }
        let id = ExecutionId::new();
        // Open log dir FIRST (design §6: log storage unavailable => reject).
        let mut record = ProcessRecord::new(id, spec.clone(), None);
        record.owner = owner;
        record.metadata = metadata;
        if serde_json::to_vec(&record)
            .map_err(|e| ProcessError::InvalidSpec(e.to_string()))?
            .len()
            > 24576
        {
            return Err(ProcessError::InvalidSpec(
                "编码后的执行记录超过 24 KiB，未执行".into(),
            ));
        }
        {
            let mut reg = self.inner.registry.write();
            reg.insert(id, record);
        }
        let log = match self.open_log(id) {
            Ok(log) => log,
            Err(error) => {
                let mut record = self.inspect(id).expect("registered execution");
                record.state = ProcessState::Rejected(format!("log storage unavailable: {error}"));
                record.finished_at = Some(std::time::SystemTime::now());
                self.publish_final(record);
                return Ok(id);
            }
        };
        self.inner
            .registry
            .write()
            .get_mut(&id)
            .expect("registered execution")
            .log_dir = Some(log.dir().to_path_buf());
        let active = self
            .inner
            .registry
            .read()
            .values()
            .filter(|r| !r.is_terminal())
            .count();
        let refusal = if active > self.inner.limits.max_active {
            Some("运行与排队任务已达上限，未启动".into())
        } else if self
            .inner
            .closing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Some("进程管理器正在关闭，拒绝启动新命令".into())
        } else if spec
            .timeout
            .is_some_and(|t| t > self.inner.limits.max_timeout)
        {
            Some("命令执行时限超过管理器上限".into())
        } else {
            let mut validation = spec.clone();
            if service {
                validation.scope = crate::ExecutionScope::TaskBound;
            }
            validation
                .validate()
                .err()
                .map(|e| e.to_string())
                .or_else(|| {
                    smallhead_permission::check_command(&spec.command_display())
                        .err()
                        .map(|e| e.to_string())
                })
        };
        if let Some(reason) = refusal {
            let mut record = self.inspect(id).expect("registered execution");
            record.state = ProcessState::Rejected(reason);
            record.finished_at = Some(std::time::SystemTime::now());
            if let Err(error) = self.inner.log_store.write_initial(&record) {
                record.state = ProcessState::CleanupFailed(format!(
                    "refusal audit persistence failed: {error}"
                ));
            }
            self.publish_final(record);
            return Ok(id);
        }
        // Persist the accepted request before spawning. A dropped caller after
        // this point must not leave a process without a queryable owner.
        if let Err(error) = self
            .inner
            .log_store
            .write_initial(&self.inspect(id).expect("registered execution"))
        {
            let mut record = self.inspect(id).expect("registered execution");
            record.state = ProcessState::CleanupFailed(error.to_string());
            record.finished_at = Some(std::time::SystemTime::now());
            self.publish_final(record);
            return Ok(id);
        }
        // Spawn the executor task on the current tokio runtime.
        // `tokio::spawn` requires a runtime; callers obtain one via the
        // shared runtime the application composes at startup (BigHead
        // pattern, design §10).
        let executor = Executor::new(id, spec, log, self.clone());
        self.event(id, "queued");
        tokio::spawn(executor.run());
        Ok(id)
    }

    /// Open logs after registering the attempt. Currently synchronous; an
    /// asynchronous storage backend must retain ownership across cancellation.
    pub(crate) fn open_log(&self, id: ExecutionId) -> Result<LogHandle, ProcessError> {
        self.inner
            .log_store
            .open(id)
            .map_err(|e| ProcessError::Internal(format!("open log: {e}")))
    }

    /// Wait for an execution to reach a terminal state, or for `deadline`
    /// to elapse.
    ///
    /// Returns:
    /// - `Ok(record)` when the record becomes terminal — the snapshot
    ///   reflects the final observed state. Already-terminal records
    ///   return immediately with their current snapshot.
    /// - `Err(ProcessError::NotFound(_))` if no execution with that id
    ///   was ever registered.
    /// - `Err(ProcessError::Internal(_))` when `deadline` elapses before
    ///   the record finalizes.
    ///
    /// The wait is driven by `tokio::sync::Notify` — `executor::run`
    /// notifies on every state transition, so a typical run wakes once
    /// for `Starting`, once for `Running`, and once for the terminal
    /// transition. The `deadline` is the outer cap; exceeding it does
    /// not mutate the record.
    pub async fn wait_for(
        &self,
        id: ExecutionId,
        deadline: Duration,
    ) -> Result<ProcessRecord, ProcessError> {
        match tokio::time::timeout(deadline, self.wait_for_inner(id)).await {
            Ok(inner) => inner,
            Err(_) => Err(ProcessError::Internal(format!(
                "wait_for deadline exceeded for {id}"
            ))),
        }
    }

    /// Inner loop shared by [`ProcessManager::wait_for`] and tests that
    /// want to drive without a `Deadline`.
    async fn wait_for_inner(&self, id: ExecutionId) -> Result<ProcessRecord, ProcessError> {
        loop {
            let notify = self.waiter_for(id);
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self.inspect(id).ok_or(ProcessError::NotFound(id))?;
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            notified.await;
        }
    }

    /// Idempotent stop.
    ///
    /// - Already terminal: returns `Ok(())` immediately (idempotent contract).
    /// - Otherwise: best-effort `mark_stopping` followed by `notify_waiters`
    ///   so the running executor sees the cancel branch and reaps the child.
    /// - Unknown id: returns `Err(ProcessError::NotFound(_))`.
    ///
    /// `mark_stopping` may fail (e.g. the executor just finalized, or the
    /// record was still in `Starting` and the executor's own `mark_running`
    /// hadn't landed yet). Both races are harmless — the executor's cancel
    /// branch will retry `mark_stopping` and then `mark_exited`.
    pub async fn stop(&self, id: ExecutionId) -> Result<(), ProcessError> {
        // Idempotency check up front, under the read lock — terminal records
        // short-circuit without touching the state machine.
        {
            let reg = self.inner.registry.read();
            let Some(record) = reg.get(&id) else {
                return Err(ProcessError::NotFound(id));
            };
            if record.state.is_terminal() {
                return Ok(());
            }
        }

        // Non-terminal: best-effort transition + wakeup. The notify reaches
        // both the executor's `subscribe_change` future and any concurrent
        // `wait_for` callers, so they all observe the cancel.
        let _ = self.mark_stopping(id);
        self.notify_waiters(id);
        Ok(())
    }

    /// Snapshot a single record by id. `None` if the id was never
    /// registered (or, in a future build, was evicted).
    pub fn inspect(&self, id: ExecutionId) -> Option<ProcessRecord> {
        self.inner.registry.read().get(&id).cloned()
    }

    /// All registered execution ids, in arbitrary order. Callers that need
    /// time ordering should sort by `ExecutionId` (UUID v7 sorts
    /// lexicographically by creation time).
    pub fn list(&self) -> Vec<ExecutionId> {
        self.inner.registry.read().keys().copied().collect()
    }

    /// All execution ids whose state matches `predicate`. Used by TUI
    /// panels that want, e.g., "everything still running".
    pub fn list_by_state(&self, predicate: impl Fn(&ProcessState) -> bool) -> Vec<ExecutionId> {
        self.inner
            .registry
            .read()
            .iter()
            .filter_map(|(id, record)| predicate(&record.state).then_some(*id))
            .collect()
    }

    /// Root directory under which per-id log dirs live.
    pub fn log_root(&self) -> &Path {
        self.inner.log_store.root()
    }

    /// 订阅有界通知；lagged 时重新查询快照及 events.jsonl。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.inner.events.subscribe()
    }

    pub(crate) fn event(&self, id: ExecutionId, kind: &str) {
        if let Some(record) = self.inspect(id) {
            match Self::append_event(&record, kind) {
                Ok(event) => {
                    let _ = self.inner.events.send(event);
                }
                Err(error) => {
                    self.inner
                        .audit_errors
                        .write()
                        .insert(id, error.to_string());
                    if let Some(record) = self.inner.registry.write().get_mut(&id) {
                        if !record.is_terminal() {
                            record.state = ProcessState::Stopping;
                        }
                    }
                    self.notify_waiters(id);
                }
            }
        }
    }

    pub(crate) fn audit_error(&self, id: ExecutionId) -> Option<String> {
        self.inner.audit_errors.read().get(&id).cloned()
    }

    fn append_event(record: &ProcessRecord, kind: &str) -> Result<serde_json::Value, ProcessError> {
        use std::io::Write;
        let id = record.id;
        let event = serde_json::json!({"execution_id":id,"owner":record.owner,"metadata":record.metadata,"event":kind,
                "state":record.state,"pid":record.pid,"command":record.spec.command_display(),"program":record.spec.program,"args":record.spec.args,
                "timestamp_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()});
        if let Some(dir) = &record.log_dir {
            let result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("events.jsonl"))?;
                if file.metadata()?.len() + event.to_string().len() as u64 + 1 > 65536 {
                    return Err(std::io::Error::other("事件日志预算已满"));
                }
                writeln!(file, "{event}")?;
                file.flush()
            })();
            if let Err(error) = result {
                return Err(ProcessError::Internal(format!("执行事件落盘失败：{error}")));
            }
        }
        Ok(event)
    }

    /// 显式清理已确认终止的执行及日志；未知清理结果需要人工确认。
    pub fn purge(&self, id: ExecutionId) -> Result<(), ProcessError> {
        let _admission = self.inner.admission.lock();
        let record = self.inspect(id).ok_or(ProcessError::NotFound(id))?;
        if !matches!(
            record.state,
            ProcessState::Exited(_) | ProcessState::Rejected(_) | ProcessState::SpawnFailed(_)
        ) {
            return Err(ProcessError::InvalidSpec(
                "只允许清理已确认完成的执行".into(),
            ));
        }
        if let Some(dir) = record.log_dir {
            std::fs::remove_dir_all(dir)?;
        }
        self.inner.registry.write().remove(&id);
        self.notify_waiters(id);
        self.inner.waiters.write().remove(&id);
        self.inner.audit_errors.write().remove(&id);
        Ok(())
    }

    /// 定向取消所属任务的执行。
    pub async fn stop_owner(&self, owner: &str) -> Result<(), ProcessError> {
        let ids: Vec<_> = self
            .inner
            .registry
            .read()
            .values()
            .filter(|r| r.owner.as_deref() == Some(owner) && !r.is_terminal())
            .map(|r| r.id)
            .collect();
        for id in ids {
            self.stop(id).await?;
        }
        Ok(())
    }

    /// 当前共享预算的只读视图。
    pub fn limits(&self) -> &crate::ProcessLimits {
        &self.inner.limits
    }

    pub(crate) async fn acquire_slot(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ProcessError> {
        self.inner
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ProcessError::Internal("进程启动队列已关闭".into()))
    }

    /// 按执行 ID 读取有界输出；offset/next_offset 都是字节偏移。
    /// EOF 仅指当前快照结尾，complete 才表示进程已收尾。
    pub async fn read_output(
        &self,
        id: ExecutionId,
        stream: crate::OutputStream,
        offset: u64,
        limit: usize,
    ) -> Result<crate::OutputPage, ProcessError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        if limit == 0 || limit > crate::MAX_OUTPUT_PAGE_BYTES {
            return Err(ProcessError::InvalidSpec(
                "输出读取量须为 1..65536 字节".into(),
            ));
        }
        let record = self.inspect(id).ok_or(ProcessError::NotFound(id))?;
        let dir = record
            .log_dir
            .ok_or_else(|| ProcessError::Internal("此执行没有可用输出目录".into()))?;
        let mut file = tokio::fs::File::open(dir.join(stream.filename())).await?;
        let stored_bytes = file.metadata().await?.len();
        let offset = offset.min(stored_bytes);
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = Vec::with_capacity(limit);
        file.take((stored_bytes - offset).min(limit as u64))
            .read_to_end(&mut bytes)
            .await?;
        let next_offset = offset + bytes.len() as u64;
        Ok(crate::OutputPage {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
            next_offset,
            stored_bytes,
            eof: next_offset >= stored_bytes,
            complete: record.state.is_terminal(),
            output_limited: matches!(record.state, ProcessState::Exited(ref info) if info.reason == crate::ExitReason::OutputLimit),
        })
    }

    /// Stop all active executions and await cleanup before shutting down Tokio.
    /// The application owner must call this; dropping a clone is not shutdown.
    pub async fn shutdown(&self, deadline: Duration) -> Result<(), ProcessError> {
        // Register/start has no suspension after allocation, so shutdown
        // observes requests admitted before this flag and rejects later ones.
        self.inner
            .closing
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.slots.close();
        let ids = self.list_by_state(|s| !s.is_terminal());
        for id in &ids {
            self.stop(*id).await?;
        }
        tokio::time::timeout(deadline, async {
            for id in ids {
                let record = self.wait_for_inner(id).await?;
                if matches!(
                    record.state,
                    ProcessState::CleanupFailed(_) | ProcessState::Lost(_)
                ) {
                    return Err(ProcessError::Internal(format!(
                        "shutdown cleanup failed for {id}: {:?}",
                        record.state
                    )));
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| ProcessError::Internal("shutdown deadline exceeded".into()))?
    }

    // -----------------------------------------------------------------
    // Waiter registry (Notify per ExecutionId)
    // -----------------------------------------------------------------
    //
    // Both `wait_for` (TUI / tools / ServiceManager) and the executor's
    // cancel branch need to wake on state transitions. Sharing one
    // `tokio::sync::Notify` per id keeps that trivial — `notify_waiters`
    // fires on every transition; receivers either see terminal state on
    // their next inspect or the notify wakes them first.
    //
    // Entries live as long as their execution record and are removed by purge.

    /// Look up (or lazily create) the `Notify` for `id`. Grows the map
    /// on demand; purge removes the matching entry.
    fn waiter_for(&self, id: ExecutionId) -> Arc<Notify> {
        // Fast path: already registered.
        if let Some(n) = self.inner.waiters.read().get(&id).cloned() {
            return n;
        }
        // Slow path: insert under the write lock. Two racing callers may
        // both end up creating two `Notify`s but only one is kept — the
        // second insert wins; the unused `Arc` is dropped. Functionally
        // harmless.
        let mut waiters = self.inner.waiters.write();
        waiters
            .entry(id)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Wake everyone currently waiting on `id`. Called after every state
    /// transition (mark_* and notify-aware spawn failure path).
    ///
    /// `pub(crate)` so the internal executor can wake waiters from
    /// its own task without going through a `mark_*` helper.
    pub(crate) fn notify_waiters(&self, id: ExecutionId) {
        if let Some(n) = self.inner.waiters.read().get(&id) {
            // `notify_waiters` (plural) wakes *every* registered Notified,
            // which is what we want: both the executor's cancel future and
            // any concurrent `wait_for` callers need the transition.
            n.notify_waiters();
        }
    }

    /// Crate-internal: future that resolves on the next state-transition
    /// notification for `id`. Used by the executor's cancel branch inside
    /// its `select!`. Each call returns a fresh future so a stale
    /// `Notified` (e.g. from a previous iteration) cannot silently fire.
    pub(crate) fn subscribe_change(
        &self,
        id: ExecutionId,
    ) -> impl std::future::Future<Output = ()> + '_ {
        let notify = self.waiter_for(id);
        async move {
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self
                    .inspect(id)
                    .is_some_and(|r| matches!(r.state, ProcessState::Stopping))
                {
                    return;
                }
                notified.await;
            }
        }
    }

    /// Publish the exact persisted terminal snapshot and wake all observers.
    pub(crate) fn publish_final(&self, mut record: ProcessRecord) {
        let id = record.id;
        match Self::append_event(&record, "completed") {
            Ok(event) => {
                let _ = self.inner.events.send(event);
            }
            Err(error) => {
                record.state = ProcessState::CleanupFailed(error.to_string());
                if let Err(error) = self.inner.log_store.write_initial(&record) {
                    tracing::error!(%id,%error,"failed to persist audit failure");
                }
                let _=self.inner.events.send(serde_json::json!({"execution_id":id,"event":"audit_failed","state":record.state}));
            }
        }
        self.inner.registry.write().insert(id, record);
        self.notify_waiters(id);
    }

    // -----------------------------------------------------------------
    // State machine
    // -----------------------------------------------------------------
    //
    // Every `mark_*` enforces a single transition. Terminal states are
    // immutable: any further call returns `ProcessError::AlreadyTerminal`.
    // Invalid from-states return `ProcessError::Internal` — those indicate
    // a logic bug in the caller (Phase 2's executor) rather than a runtime
    // condition we want to paper over.

    /// `Queued → Starting`. Records the freshly-observed `ProcessId`.
    pub(crate) fn mark_starting(
        &self,
        id: ExecutionId,
        pid: ProcessId,
    ) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        if matches!(record.state, ProcessState::Stopping) {
            record.pid = Some(pid);
            record.started_at = Some(std::time::SystemTime::now());
            return Ok(());
        }
        if !matches!(record.state, ProcessState::Queued) {
            return Err(ProcessError::Internal(format!(
                "execution {id} cannot transition {} -> starting",
                record.state.as_label()
            )));
        }
        record.state = ProcessState::Starting;
        record.pid = Some(pid);
        Ok(())
    }

    /// `Starting → Running`. Captures `started_at` for duration reporting.
    pub(crate) fn mark_running(&self, id: ExecutionId) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        if !matches!(record.state, ProcessState::Starting) {
            return Err(ProcessError::Internal(format!(
                "execution {id} cannot transition {} -> running",
                record.state.as_label()
            )));
        }
        record.state = ProcessState::Running;
        record.started_at = Some(std::time::SystemTime::now());
        drop(registry);
        self.event(id, "running");
        Ok(())
    }

    /// Any non-terminal → `Stopping`. A graceful-stop signal has been sent;
    /// we still hold the `ProcessId` and may escalate to KILL after grace.
    ///
    /// Phase 2 widens the from-state set to all non-terminal states so
    /// [`ProcessManager::stop`] can be called before the executor has even
    /// started: a `Queued → Stopping` transition lets a cancel that
    /// races the executor's `mark_starting` still drive a terminal
    /// transition (the executor detects `Stopping` between every state
    /// change and routes to its cancel branch). Mirrors the
    /// "any non-terminal → Lost" rule of [`Self::mark_lost`].
    pub(crate) fn mark_stopping(&self, id: ExecutionId) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        // is_terminal() gate above guarantees we reach here from a
        // non-terminal state; the match documents the expectation.
        debug_assert!(!record.state.is_terminal());
        let changed = record.state != ProcessState::Stopping;
        record.state = ProcessState::Stopping;
        drop(registry);
        if changed {
            self.event(id, "stopping");
        }
        Ok(())
    }

    /// `Running | Stopping → Exited`. `info` carries the real exit code / signal
    /// and reason — never silently `0` (design §5).
    ///
    /// Phase 2 widens the from-state set beyond `Stopping` to cover natural
    /// exits: a child that exits on its own moves straight `Running →
    /// Exited`, with no intermediate `Stopping` (we never issued a stop
    /// signal). The timeout / cancel paths still take the documented
    /// `Running → Stopping → Exited` path so callers can observe the
    /// cancel-in-progress state on TUI rows.
    #[cfg(test)]
    pub(crate) fn mark_exited(
        &self,
        id: ExecutionId,
        info: ProcessExitInfo,
    ) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        if !matches!(record.state, ProcessState::Running | ProcessState::Stopping) {
            return Err(ProcessError::Internal(format!(
                "execution {id} cannot transition {} -> exited",
                record.state.as_label()
            )));
        }
        record.state = ProcessState::Exited(info);
        record.finished_at = Some(std::time::SystemTime::now());
        Ok(())
    }

    /// `Queued | Starting → Rejected`. Post-registration validator refusal
    /// (e.g. permission denial that happened after `start()` already
    /// returned — kept distinct from `start()`'s pre-registration refusal,
    /// which surfaces as `ProcessError::InvalidSpec`).
    #[cfg(test)]
    pub(crate) fn mark_rejected(
        &self,
        id: ExecutionId,
        reason: String,
    ) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        if !matches!(record.state, ProcessState::Queued | ProcessState::Starting) {
            return Err(ProcessError::Internal(format!(
                "execution {id} cannot transition {} -> rejected",
                record.state.as_label()
            )));
        }
        record.state = ProcessState::Rejected(reason);
        record.finished_at = Some(std::time::SystemTime::now());
        Ok(())
    }

    /// `Queued | Starting → SpawnFailed`. `spawn()` itself failed (ENOENT,
    /// EACCES, …) — the OS process never came up.
    #[cfg(test)]
    pub(crate) fn mark_spawn_failed(
        &self,
        id: ExecutionId,
        reason: String,
    ) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        if !matches!(record.state, ProcessState::Queued | ProcessState::Starting) {
            return Err(ProcessError::Internal(format!(
                "execution {id} cannot transition {} -> spawn_failed",
                record.state.as_label()
            )));
        }
        record.state = ProcessState::SpawnFailed(reason);
        record.finished_at = Some(std::time::SystemTime::now());
        Ok(())
    }

    /// Any non-terminal → `Lost`. Used when the manager can no longer
    /// vouch for the OS process (e.g. parent future dropped before
    /// Phase 2's cleanup wiring lands).
    #[cfg(test)]
    pub(crate) fn mark_lost(&self, id: ExecutionId, reason: String) -> Result<(), ProcessError> {
        let mut registry = self.inner.registry.write();
        let record = registry.get_mut(&id).ok_or(ProcessError::NotFound(id))?;
        if record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminal(id));
        }
        // Spec: "Any non-terminal → Lost". is_terminal is checked above,
        // so any state reaching here is by construction non-terminal.
        record.state = ProcessState::Lost(reason);
        record.finished_at = Some(std::time::SystemTime::now());
        Ok(())
    }
}

// ============================ Tests ============================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ExecutionScope;
    use crate::state::{ExitReason, ProcessExitInfo};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Build a manager rooted at a fresh temp dir so each test is hermetic.
    fn manager() -> (TempDir, ProcessManager) {
        let dir = TempDir::new().expect("tempdir");
        let mgr =
            ProcessManager::with_log_root(dir.path().to_path_buf()).expect("manager with log root");
        (dir, mgr)
    }

    fn trivial_spec(cmd: &str) -> ProcessSpec {
        ProcessSpec::new(cmd)
    }

    #[tokio::test]
    async fn start_registers_in_queued_state() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("echo hello")).await.expect("start");
        let record = mgr.inspect(id).expect("record present");
        assert_eq!(record.id, id);
        assert_eq!(record.state, ProcessState::Queued);
        assert!(record.pid.is_none());
        assert!(record.started_at.is_none());
        assert!(record.finished_at.is_none());
        assert!(record.spec.command == "echo hello");
    }

    #[tokio::test]
    async fn start_rejects_persistent_scope() {
        let (_dir, mgr) = manager();
        let mut spec = trivial_spec("sleep 1");
        spec.scope = ExecutionScope::PersistentService;
        let id = mgr.start(spec).await.expect("audited refusal");
        assert!(matches!(
            mgr.inspect(id).unwrap().state,
            ProcessState::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn start_rejects_empty_command() {
        let (_dir, mgr) = manager();
        // Use a whitespace-only command so we exercise the trim path.
        let id = mgr
            .start(trivial_spec("   \t\n  "))
            .await
            .expect("audited refusal");
        assert!(matches!(
            mgr.inspect(id).unwrap().state,
            ProcessState::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn start_rejects_zero_timeout() {
        let (_dir, mgr) = manager();
        let mut spec = trivial_spec("sleep 1");
        spec.timeout = Some(Duration::ZERO);
        let id = mgr.start(spec).await.expect("audited refusal");
        assert!(matches!(
            mgr.inspect(id).unwrap().state,
            ProcessState::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn inspect_returns_record_by_id() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("ls")).await.unwrap();
        let record = mgr.inspect(id).unwrap();
        assert_eq!(record.id, id);
        assert!(!record.is_terminal());
    }

    #[tokio::test]
    async fn inspect_returns_none_for_unknown_id() {
        let (_dir, mgr) = manager();
        let bogus = ExecutionId::new();
        assert!(mgr.inspect(bogus).is_none());
    }

    #[tokio::test]
    async fn list_includes_all_registered() {
        let (_dir, mgr) = manager();
        let a = mgr.start(trivial_spec("a")).await.unwrap();
        let b = mgr.start(trivial_spec("b")).await.unwrap();
        let c = mgr.start(trivial_spec("c")).await.unwrap();
        let mut listed = mgr.list();
        listed.sort();
        let mut expected = vec![a, b, c];
        expected.sort();
        assert_eq!(listed, expected);
    }

    #[tokio::test]
    async fn list_by_state_filters_running_subset() {
        let (_dir, mgr) = manager();
        let a = mgr.start(trivial_spec("a")).await.unwrap();
        let b = mgr.start(trivial_spec("b")).await.unwrap();
        mgr.mark_starting(b, ProcessId::from_pid(2)).unwrap();
        mgr.mark_running(b).unwrap();

        let running = mgr.list_by_state(|s| matches!(s, ProcessState::Running));
        assert_eq!(running, vec![b]);

        let queued = mgr.list_by_state(|s| matches!(s, ProcessState::Queued));
        assert_eq!(queued, vec![a]);
    }

    #[tokio::test]
    async fn state_transition_queued_to_starting_to_running() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("echo")).await.unwrap();
        mgr.mark_starting(id, ProcessId::from_pid(7)).unwrap();
        let rec = mgr.inspect(id).unwrap();
        assert_eq!(rec.state, ProcessState::Starting);
        assert_eq!(rec.pid, Some(ProcessId::from_pid(7)));

        mgr.mark_running(id).unwrap();
        let rec = mgr.inspect(id).unwrap();
        assert_eq!(rec.state, ProcessState::Running);
        assert!(rec.started_at.is_some());
    }

    #[tokio::test]
    async fn state_transition_running_to_stopping_to_exited() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("sleep 1")).await.unwrap();
        mgr.mark_starting(id, ProcessId::from_pid(11)).unwrap();
        mgr.mark_running(id).unwrap();
        mgr.mark_stopping(id).unwrap();
        assert_eq!(mgr.inspect(id).unwrap().state, ProcessState::Stopping);

        let info = ProcessExitInfo {
            exit_code: Some(0),
            signal: None,
            reason: ExitReason::Natural,
            duration: Duration::from_millis(100),
        };
        mgr.mark_exited(id, info.clone()).unwrap();
        let rec = mgr.inspect(id).unwrap();
        assert_eq!(rec.state, ProcessState::Exited(info));
        assert!(rec.finished_at.is_some());
    }

    #[tokio::test]
    async fn state_transition_invalid_returns_already_terminal_or_internal() {
        let (_dir, mgr) = manager();

        // 1) Running -> Starting must fail (wrong from-state).
        let id = mgr.start(trivial_spec("x")).await.unwrap();
        mgr.mark_starting(id, ProcessId::from_pid(1)).unwrap();
        mgr.mark_running(id).unwrap();
        let err = mgr
            .mark_starting(id, ProcessId::from_pid(2))
            .expect_err("Running -> Starting must fail");
        assert!(
            matches!(err, ProcessError::Internal(_)),
            "expected Internal for wrong from-state, got {err:?}"
        );

        // 2) After Exited, every further mark_* must return AlreadyTerminal.
        mgr.mark_stopping(id).unwrap();
        mgr.mark_exited(
            id,
            ProcessExitInfo {
                exit_code: Some(0),
                signal: None,
                reason: ExitReason::Natural,
                duration: Duration::from_millis(1),
            },
        )
        .unwrap();
        let err = mgr
            .mark_running(id)
            .expect_err("Exited -> Running must fail");
        assert!(matches!(err, ProcessError::AlreadyTerminal(_)));
        let err = mgr
            .mark_lost(id, "should be terminal".into())
            .expect_err("Exited -> Lost must fail");
        assert!(matches!(err, ProcessError::AlreadyTerminal(_)));
    }

    #[tokio::test]
    async fn mark_rejected_only_from_queued_or_starting() {
        let (_dir, mgr) = manager();

        // Queued -> Rejected works.
        let q = mgr.start(trivial_spec("q")).await.unwrap();
        mgr.mark_rejected(q, "post-registration denial".into())
            .unwrap();
        assert_eq!(
            mgr.inspect(q).unwrap().state,
            ProcessState::Rejected("post-registration denial".into())
        );

        // Starting -> Rejected works.
        let s = mgr.start(trivial_spec("s")).await.unwrap();
        mgr.mark_starting(s, ProcessId::from_pid(3)).unwrap();
        mgr.mark_rejected(s, "starting rejection".into()).unwrap();
        assert!(matches!(
            mgr.inspect(s).unwrap().state,
            ProcessState::Rejected(_)
        ));

        // Running -> Rejected must fail.
        let r = mgr.start(trivial_spec("r")).await.unwrap();
        mgr.mark_starting(r, ProcessId::from_pid(4)).unwrap();
        mgr.mark_running(r).unwrap();
        let err = mgr
            .mark_rejected(r, "too late".into())
            .expect_err("Running -> Rejected must fail");
        assert!(matches!(err, ProcessError::Internal(_)));
    }

    #[tokio::test]
    async fn mark_spawn_failed_only_from_queued_or_starting() {
        let (_dir, mgr) = manager();
        let q = mgr.start(trivial_spec("q")).await.unwrap();
        mgr.mark_spawn_failed(q, "ENOENT".into()).unwrap();
        assert!(matches!(
            mgr.inspect(q).unwrap().state,
            ProcessState::SpawnFailed(_)
        ));

        let r = mgr.start(trivial_spec("r")).await.unwrap();
        mgr.mark_starting(r, ProcessId::from_pid(5)).unwrap();
        mgr.mark_running(r).unwrap();
        let err = mgr
            .mark_spawn_failed(r, "late".into())
            .expect_err("Running -> SpawnFailed must fail");
        assert!(matches!(err, ProcessError::Internal(_)));
    }

    #[tokio::test]
    async fn mark_lost_from_any_non_terminal() {
        let (_dir, mgr) = manager();

        // Queued -> Lost.
        let a = mgr.start(trivial_spec("a")).await.unwrap();
        mgr.mark_lost(a, "dropped".into()).unwrap();
        assert!(matches!(
            mgr.inspect(a).unwrap().state,
            ProcessState::Lost(_)
        ));

        // Starting -> Lost.
        let b = mgr.start(trivial_spec("b")).await.unwrap();
        mgr.mark_starting(b, ProcessId::from_pid(6)).unwrap();
        mgr.mark_lost(b, "dropped".into()).unwrap();
        assert!(matches!(
            mgr.inspect(b).unwrap().state,
            ProcessState::Lost(_)
        ));

        // Running -> Lost.
        let c = mgr.start(trivial_spec("c")).await.unwrap();
        mgr.mark_starting(c, ProcessId::from_pid(7)).unwrap();
        mgr.mark_running(c).unwrap();
        mgr.mark_lost(c, "dropped".into()).unwrap();
        assert!(matches!(
            mgr.inspect(c).unwrap().state,
            ProcessState::Lost(_)
        ));

        // Stopping -> Lost.
        let d = mgr.start(trivial_spec("d")).await.unwrap();
        mgr.mark_starting(d, ProcessId::from_pid(8)).unwrap();
        mgr.mark_running(d).unwrap();
        mgr.mark_stopping(d).unwrap();
        mgr.mark_lost(d, "dropped".into()).unwrap();
        assert!(matches!(
            mgr.inspect(d).unwrap().state,
            ProcessState::Lost(_)
        ));

        // Exited -> Lost must fail.
        let e = mgr.start(trivial_spec("e")).await.unwrap();
        mgr.mark_starting(e, ProcessId::from_pid(9)).unwrap();
        mgr.mark_running(e).unwrap();
        mgr.mark_stopping(e).unwrap();
        mgr.mark_exited(
            e,
            ProcessExitInfo {
                exit_code: Some(0),
                signal: None,
                reason: ExitReason::Natural,
                duration: Duration::from_millis(0),
            },
        )
        .unwrap();
        let err = mgr
            .mark_lost(e, "too late".into())
            .expect_err("Exited -> Lost must fail");
        assert!(matches!(err, ProcessError::AlreadyTerminal(_)));
    }

    #[tokio::test]
    async fn mark_not_found_returns_error() {
        let (_dir, mgr) = manager();
        let bogus = ExecutionId::new();
        let err = mgr
            .mark_starting(bogus, ProcessId::from_pid(1))
            .expect_err("must reject unknown id");
        assert!(matches!(err, ProcessError::NotFound(_)));
    }

    #[tokio::test]
    async fn id_uniqueness() {
        let (_dir, mgr) = manager();
        let mut ids = Vec::with_capacity(100);
        for i in 0..100 {
            let id = mgr.start(trivial_spec(&format!("echo {i}"))).await.unwrap();
            ids.push(id);
        }
        // UUID v7 — unique with overwhelming probability; check explicitly.
        let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "ExecutionIds must be unique");
    }

    #[tokio::test]
    async fn log_root_is_created_and_returned() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested/runs");
        assert!(!nested.exists());
        let mgr = ProcessManager::with_log_root(nested.clone()).unwrap();
        assert!(nested.exists());
        assert_eq!(mgr.log_root(), nested);

        // Drop the tempdir to keep things tidy; mgr still holds the path
        // but that's fine — it's only referenced as a string.
        drop(mgr);
    }

    // -----------------------------------------------------------------
    // Phase 2 tests — exercise the executor + wait_for + stop
    // -----------------------------------------------------------------
    //
    // These all do real `sh -c` spawns. Tokio's multi-thread runtime is not
    // required, but `start_paused = false` ensures wall-clock time actually
    // advances for the timeout test.

    /// `start` should drive the executor through `Queued → Starting →
    /// Running → Exited(_, Natural)` for a trivial `echo`.
    #[tokio::test]
    async fn start_actually_spawns_and_reaches_exited() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("echo hello")).await.expect("start");
        let record = mgr
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("terminal");
        match record.state {
            ProcessState::Exited(info) => {
                assert_eq!(info.reason, ExitReason::Natural);
                assert_eq!(info.exit_code, Some(0));
                assert_eq!(info.signal, None);
            }
            other => panic!("expected Exited, got {other:?}"),
        }
        assert_eq!(record.id, id);
        assert!(record.finished_at.is_some());
    }

    /// Non-zero exit codes must surface verbatim (`false` exits with 1).
    #[tokio::test]
    async fn start_records_nonzero_exit_code() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("false")).await.expect("start");
        let record = mgr
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("terminal");
        match record.state {
            ProcessState::Exited(info) => {
                assert_eq!(info.reason, ExitReason::Natural);
                assert_eq!(info.exit_code, Some(1));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// `sh -c /nonexistent-binary-xyz` does NOT fail at the OS layer — `sh`
    /// itself spawns successfully, then prints "command not found" to
    /// stderr and exits with status 127. This test exercises the
    /// `wait_for` plumbing for a non-zero, natural-exit case (it asserts
    /// the executor reaches a terminal `Exited(_, Natural)` with code 127)
    /// and leaves the pure `SpawnFailed` outcome — which would require
    /// bypassing the `sh -c` wrapper — to Phase 2B's dedicated executor
    /// unit tests. Keeping this test confirms neither `start` nor
    /// `wait_for` hangs on a missing binary inside the shell.
    #[tokio::test]
    async fn start_records_spawn_failure() {
        let (_dir, mgr) = manager();
        let id = mgr
            .start(trivial_spec("/nonexistent-binary-xyz"))
            .await
            .expect("start returns id even for spec that won't spawn");
        let record = mgr
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("terminal");
        match record.state {
            ProcessState::Exited(info) => {
                assert_eq!(info.reason, ExitReason::Natural);
                assert_eq!(info.exit_code, Some(127));
            }
            other => panic!("expected Exited(_, Natural, exit_code=127), got {other:?}"),
        }
        assert!(record.finished_at.is_some());
    }

    /// `start_spec.with_timeout(200ms)` + `sleep 5` must be killed by the
    /// executor at 200ms and recorded as `Exited(_, Timeout)`. We do not
    /// try to verify the OS-level orphan check here (would require
    /// `libc::kill` / `unsafe`); we trust the executor's
    /// `start_kill` + `wait` reaping pair, which Phase 2B's persistence
    /// tests will tighten.
    #[tokio::test]
    async fn start_timeout_kills_child() {
        let (_dir, mgr) = manager();
        let id = mgr
            .start(trivial_spec("sleep 5").with_timeout(Duration::from_millis(200)))
            .await
            .expect("start");
        let record = mgr
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("terminal");
        match record.state {
            ProcessState::Exited(info) => {
                assert_eq!(info.reason, ExitReason::Timeout);
            }
            other => panic!("expected Exited with Timeout, got {other:?}"),
        }
    }

    /// `stop` is idempotent: calling it twice on the same id returns `Ok`
    /// both times, and the executor records `Exited(_, Cancelled)` (not
    /// `Natural`, not `Timeout`) once the cancel branch fires.
    #[tokio::test]
    async fn stop_is_idempotent_and_yields_cancelled() {
        let (_dir, mgr) = manager();
        let id = mgr.start(trivial_spec("sleep 5")).await.expect("start");

        // First stop moves Queued|Starting|Running → Stopping and notifies.
        mgr.stop(id).await.expect("first stop");
        // Second stop must also succeed (idempotent contract).
        mgr.stop(id).await.expect("second stop");

        let record = mgr
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("terminal");
        match record.state {
            ProcessState::Exited(info) => {
                assert_eq!(info.reason, ExitReason::Cancelled);
            }
            other => panic!("expected Exited with Cancelled, got {other:?}"),
        }
    }
}
