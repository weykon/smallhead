//! 会话服务：按名称复用、验证健康协议、区分外部服务、显式停止。
//! 不自动重启；用户停止后仅后续显式 ensure 才能重新启动。
use crate::{
    ExecutionId, ExecutionMetadata, ExecutionScope, ProcessError, ProcessManager, ProcessSpec,
    ProcessState,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

/// services.json 当前 schema 版本（v1 引入 description 字段、RunVersion 与新事件类型）。
pub const SERVICES_SCHEMA_VERSION: u32 = 1;

/// 单次健康/身份探测的快照。
///
/// `run_version` 用于在并发争用中判断结果是否过期：异步 I/O 完成后读回 `ServiceRecord.run_version`，
/// 与此处记录不一致即视为 stale。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ServiceObservation {
    /// 被观察的服务名。
    pub name: String,
    /// 探测完成时间。
    pub observed_at: SystemTime,
    /// 探测耗时毫秒。
    pub elapsed_ms: u64,
    /// 健康协议是否通过。
    pub ok: bool,
    /// 简短原因（成功/失败摘要，长度受 manager 限制）。
    pub reason: String,
    /// 探测发起时的 run_version。
    pub run_version: RunVersion,
}

/// 运行代次（每次 ensure/inspect 启动时 +1），用于版本化提交。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct RunVersion(pub u64);

/// 单个管理操作的 ID（manager-owned UUID）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub uuid::Uuid);

/// `stop` 操作 I/O 阶段的结果，用于在 commit 阶段决定最终 state/detail。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// 进程已确认结束。
    Stopped,
    /// `wait_for` 返回非终止状态或失败——状态会变成 Unknown。
    Unconfirmed(String),
}

/// 管理操作的来源：用户、Agent、调度器、终止热路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationSource {
    /// 用户/键盘/UI。
    User,
    /// Agent 工具调用。
    Agent,
    /// 周期健康调度。
    Scheduler,
    /// 失败恢复路径。
    Recover,
}

/// 管理操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationKind {
    /// 启动并验证就绪。
    Ensure,
    /// 单次健康/身份探测。
    Inspect,
    /// 显式停止。
    Stop,
    /// 清除登记。
    Forget,
}

/// 操作终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperationStatus {
    /// 已入队，尚未启动。
    Pending,
    /// 正在执行。
    Running,
    /// 成功完成。
    Done,
    /// 失败。
    Failed,
    /// 被取消。
    Cancelled,
}

/// 一次管理操作的可观察状态。
///
/// 终态后保留 60s，供 TUI 查询。终态字段：`status` ∈ {Done, Failed, Cancelled} 且 `finished_at` 已填。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ServiceOperation {
    /// 唯一 ID。
    pub id: OperationId,
    /// 服务名。
    pub name: String,
    /// 操作类型。
    pub action: ServiceOperationKind,
    /// 操作来源。
    pub source: ServiceOperationSource,
    /// 当前状态。
    pub status: ServiceOperationStatus,
    /// 开始时间。
    pub started_at: Option<SystemTime>,
    /// 终态时间（Done/Failed/Cancelled 后填）。
    pub finished_at: Option<SystemTime>,
    /// 发起操作时记录的 run_version（用于诊断）。
    pub target_run_version: RunVersion,
    /// 失败时的简短错误描述。
    pub error: Option<String>,
}

/// 服务事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceEventKind {
    /// 操作开始。
    OperationStarted,
    /// 操作结束（Done/Failed/Cancelled）。
    OperationFinished,
    /// 状态机变化（Ready↔Degraded↔Stopped...）。
    StateChanged,
    /// 周期或手工 inspect 的健康观察。
    HealthObserved,
    /// 用户可见的诊断日志（受 reason 长度限制）。
    Logged,
    /// 服务说明被 Agent 改写。
    DescriptionUpdated,
}

/// 服务事件：通过 `tokio::sync::broadcast` 暴露给订阅者。
///
/// schema_version 固定为 [`SERVICES_SCHEMA_VERSION`]；sequence 是 manager 单调递增序号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ServiceEvent {
    /// 当前 schema 版本（让消费者识别字段）。
    pub schema_version: u32,
    /// 单调递增序号。
    pub sequence: u64,
    /// 服务名。
    pub name: String,
    /// 事件时间戳。
    pub timestamp: SystemTime,
    /// 事件类型。
    pub kind: ServiceEventKind,
    /// 关联操作 ID（仅 OperationStarted/Finished）。
    pub operation_id: Option<OperationId>,
    /// 关联执行 ID（仅本管理器启动时存在）。
    pub execution_id: Option<ExecutionId>,
    /// 涉及的状态（StateChanged 时）。
    pub state: Option<ServiceState>,
    /// 简要描述（健康失败原因、日志内容、说明预览等，长度受限）。
    pub reason: Option<String>,
}

/// 常驻服务的配置。名称同时作为稳定 ServiceId。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// 稳定名称，例如 laya 或 qwen。
    pub name: String,
    /// 启动命令及环境。仅允许会话服务。
    pub process: ProcessSpec,
    /// 本机 HTTP 健康检查地址。
    #[serde(default)]
    pub health_url: String,
    /// 健康 JSON 必须包含的字段及值，用于区分错误服务。
    #[serde(default)]
    pub identity: BTreeMap<String, serde_json::Value>,
    /// 等待就绪的毫秒数（1..120000）。
    pub startup_timeout_ms: u64,
}

/// 服务状态不等同于 OS 进程状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceState {
    /// 正在启动或探测。
    Starting,
    /// 健康检查通过。
    Ready,
    /// 当前健康检查失败，后续 inspect 会再次探测。
    Degraded,
    /// 用户停止或会话结束。
    Stopped,
    /// 启动失败或进程提前退出。
    Failed,
    /// 无法确认历史进程身份或清理结果。
    Unknown,
}

/// 可供 Agent 和 TUI 展示的服务快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// 服务配置。
    pub spec: ServiceSpec,
    /// 当前服务状态。
    pub state: ServiceState,
    /// 仅本管理器启动的进程有执行 ID。
    pub execution_id: Option<ExecutionId>,
    /// 外部服务可连接但不能由本管理器停止。
    pub external: bool,
    /// 用户期望该服务运行；显式 stop 将其设为 false。
    pub desired_running: bool,
    /// 最近的探测或停止原因。
    pub detail: String,
    /// Agent 写入的自然语言说明（用途、身份验证、最近原因等）。
    #[serde(default)]
    pub description: Option<String>,
    /// 最近一次说明更新的时间；旧记录可能没有。
    #[serde(default)]
    pub description_updated_at: Option<SystemTime>,
    /// 当前运行/控制代次；只读检查不改变它。
    #[serde(default)]
    pub run_version: RunVersion,
}

/// 仅供 TUI 展示的服务视图快照。
///
/// 合并 [`ServiceRecord`] 与最近一次 [`ServiceObservation`]，并暴露给前端作为唯一事实源。
/// TUI 不建模 use_case / accessible / authenticated 固定字段；这些由 Agent 写入
/// `description` 自由文本，本视图原样透传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceView {
    /// 服务名。
    pub name: String,
    /// 当前状态。
    pub state: ServiceState,
    /// 是否外部服务（无法停止）。
    pub external: bool,
    /// 用户期望运行。
    pub desired_running: bool,
    /// 最近一次健康观察（探测成功/失败 + 原因）。
    pub last_observation: Option<ServiceObservation>,
    /// 正在运行的管理操作 ID（保证 TUI 能显示「正在停止」）。
    pub in_flight_operation: Option<OperationId>,
    /// 健康检查 URL。
    pub health_url: String,
    /// 工作目录。
    pub workdir: Option<std::path::PathBuf>,
    /// 启动就绪等待毫秒。
    pub startup_timeout_ms: u64,
    /// 启动命令的人读展示。
    pub startup_command_display: String,
    /// 最近的探测或停止原因（与 ServiceRecord.detail 同源）。
    pub detail: String,
    /// 当前执行 ID（仅本管理器启动时存在）。
    pub execution_id: Option<ExecutionId>,
    /// Agent 写入的自由文本说明。
    pub description: Option<String>,
    /// 最近一次说明更新时间。
    pub description_updated_at: Option<SystemTime>,
}

/// 共用 ProcessManager 的服务管理器。
#[derive(Clone)]
pub struct ServiceManager {
    processes: ProcessManager,
    records: Arc<parking_lot::RwLock<BTreeMap<String, ServiceRecord>>>,
    /// 最近一次成功 / 失败的健康观察（per-service），用于 `service_view.last_observation`。
    last_observations: Arc<parking_lot::RwLock<BTreeMap<String, ServiceObservation>>>,
    /// 当前正在执行的 per-service 操作。
    in_flight_ops: Arc<parking_lot::RwLock<BTreeMap<String, OperationId>>>,
    /// 所有已发出操作（含终态）的可查询表。终态保留 60s 后由 `tick()` 清扫。
    operations: Arc<parking_lot::Mutex<BTreeMap<OperationId, ServiceOperation>>>,
    persist_lock: Arc<parking_lot::Mutex<()>>,
    gates: Arc<parking_lot::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    probes: Arc<parking_lot::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    probe_permits: Arc<tokio::sync::Semaphore>,
    next_version: Arc<std::sync::atomic::AtomicU64>,
    client: reqwest::Client,
    /// 事件广播器：管理操作、状态变化、健康观察都从这里发出。
    /// 缓冲 256 条——足够覆盖 TUI 短暂阻塞；溢出仅丢事件不阻塞 emit。
    event_tx: tokio::sync::broadcast::Sender<ServiceEvent>,
    /// 事件序号，单调递增。
    event_sequence: Arc<std::sync::atomic::AtomicU64>,
    /// service-events.jsonl 写入器（带轮转）。
    event_log: Arc<parking_lot::Mutex<EventLogWriter>>,
}

/// 已发出操作的默认保留时长。TUI 用这个值去轮询 `operation()` 是否仍可达。
pub const OPERATION_RETENTION: Duration = Duration::from_secs(60);

/// broadcast 通道容量。足够让 TUI 在 256 个事件内追上；溢出仅丢事件。
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// 默认健康调度间隔。
pub const HEALTH_SCHEDULER_INTERVAL: Duration = Duration::from_secs(5);

/// 默认健康调度并发数（同一时刻最多 2 个服务并行 inspect）。
pub const HEALTH_SCHEDULER_CONCURRENCY: usize = 2;

/// service-events.jsonl 单文件最大字节数（5 MiB）。
pub const SERVICE_EVENTS_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// service-events.jsonl 保留的旋转文件数（除当前外最多 1 个 .1 备份）。
pub const SERVICE_EVENTS_KEEP: usize = 2;

/// 周期健康调度的句柄。Drop 也会停掉任务。
pub struct HealthSchedulerHandle {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

struct OperationGuard {
    manager: ServiceManager,
    id: OperationId,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let unfinished = self
            .manager
            .operations
            .lock()
            .get(&self.id)
            .is_some_and(|op| op.status == ServiceOperationStatus::Running);
        if unfinished {
            self.manager.finish_op(
                self.id,
                ServiceOperationStatus::Cancelled,
                Some("操作等待已取消".into()),
            );
        }
    }
}

impl HealthSchedulerHandle {
    /// 显式停止并等待任务结束。
    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for HealthSchedulerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// service-events.jsonl 写入器：固定阈值 + 最多 N 份轮转。
///
/// `append` 把一行 JSON 追加到当前文件，超过 `max_bytes` 时把当前文件改名为 `.1`
/// （先删旧 `.1`），再创建新文件。落盘失败不阻塞事件流——记录 warn 即可。
pub struct EventLogWriter {
    path: std::path::PathBuf,
    current_bytes: u64,
    max_bytes: u64,
    keep: usize,
}

impl EventLogWriter {
    /// 使用默认阈值（5 MiB / 保留 2 份）。
    pub fn new(root: &std::path::Path) -> Self {
        Self {
            path: root.join("service-events.jsonl"),
            current_bytes: 0,
            max_bytes: SERVICE_EVENTS_MAX_BYTES,
            keep: SERVICE_EVENTS_KEEP,
        }
    }

    /// 测试用：覆盖默认阈值。
    pub fn with_limits(root: &std::path::Path, max_bytes: u64, keep: usize) -> Self {
        Self {
            path: root.join("service-events.jsonl"),
            current_bytes: 0,
            max_bytes,
            keep,
        }
    }

    /// 追加一行 JSON。`payload` 应是单条事件序列化结果。
    /// 返回 Ok(()) 即便写盘失败——记录 warn 即可，绝不阻塞 emit 路径。
    pub fn append(&mut self, line: &[u8]) {
        if self.keep == 0 {
            return; // 测试或上层关闭日志
        }
        if let Err(e) = self.try_append(line) {
            tracing::warn!(
                target: "smallhead.process.events",
                error = %e,
                path = %self.path.display(),
                "service-events.jsonl 写入失败，已丢弃该事件"
            );
        }
    }

    fn try_append(&mut self, line: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let needs_rotate =
            self.current_bytes > 0 && self.current_bytes + line.len() as u64 + 1 > self.max_bytes;
        if needs_rotate {
            self.rotate()?;
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        self.current_bytes += line.len() as u64 + 1;
        Ok(())
    }

    /// 当前 → .1，删旧 .1；当 `keep <= 1` 时直接删除当前文件（无备份）。
    fn rotate(&mut self) -> std::io::Result<()> {
        if self.keep <= 1 {
            // 不保留任何备份：直接删旧文件。
            if self.path.exists() {
                std::fs::remove_file(&self.path)?;
            }
            self.current_bytes = 0;
            return Ok(());
        }
        // 删最旧的备份（.1）
        let backup = self.path.with_extension("jsonl.1");
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        if self.path.exists() {
            std::fs::rename(&self.path, &backup)?;
        }
        self.current_bytes = 0;
        Ok(())
    }

    /// 当前文件字节数（测试用）。
    pub fn current_bytes(&self) -> u64 {
        self.current_bytes
    }

    /// 当前 JSONL 文件路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl ServiceManager {
    /// 构造管理器。调用方应共享此实例，服务停止由 ProcessManager 执行。
    pub fn new(processes: ProcessManager) -> Result<Self, ProcessError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(800))
            .build()
            .map_err(|e| ProcessError::Internal(e.to_string()))?;
        let mut records = crate::storage::load_services(processes.log_root())?;
        for record in records.values_mut() {
            if record.desired_running {
                if let Some(detail) = confirmed_exit_detail(&processes, record) {
                    record.state = ServiceState::Failed;
                    record.detail = detail;
                } else {
                    record.state = ServiceState::Unknown;
                    record.detail = unknown_recovery_detail(&processes, record);
                }
            }
        }
        crate::storage::save_services(processes.log_root(), &records)?;
        let (event_tx, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let log_root = processes.log_root().to_path_buf();
        let next_version = records.values().map(|r| r.run_version.0).max().unwrap_or(0);
        Ok(Self {
            processes,
            records: Arc::new(parking_lot::RwLock::new(records)),
            last_observations: Default::default(),
            in_flight_ops: Default::default(),
            operations: Default::default(),
            persist_lock: Default::default(),
            gates: Default::default(),
            probes: Default::default(),
            probe_permits: Arc::new(tokio::sync::Semaphore::new(HEALTH_SCHEDULER_CONCURRENCY)),
            next_version: Arc::new(std::sync::atomic::AtomicU64::new(next_version)),
            client,
            event_tx,
            event_sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_log: Arc::new(parking_lot::Mutex::new(EventLogWriter::new(&log_root))),
        })
    }

    /// 分配新的 `OperationId` 与对应的初始 `ServiceOperation`。
    ///
    /// 同时把它登记到 `in_flight_ops`（按服务名）和 `operations` 表（按 ID 查询），
    /// 并发出 `OperationStarted` 事件。
    fn start_op(
        &self,
        name: &str,
        action: ServiceOperationKind,
        source: ServiceOperationSource,
    ) -> OperationId {
        let id = OperationId(uuid::Uuid::now_v7());
        let run_version = self
            .records
            .read()
            .get(name)
            .map(|r| r.run_version)
            .unwrap_or_default();
        let op = ServiceOperation {
            id,
            name: name.into(),
            action,
            source,
            status: ServiceOperationStatus::Running,
            started_at: Some(SystemTime::now()),
            finished_at: None,
            target_run_version: run_version,
            error: None,
        };
        self.operations.lock().insert(id, op);
        if action != ServiceOperationKind::Inspect {
            self.in_flight_ops.write().insert(name.into(), id);
        }
        if source != ServiceOperationSource::Scheduler {
            self.emit(ServiceEvent {
                schema_version: SERVICES_SCHEMA_VERSION,
                sequence: 0,
                name: name.into(),
                timestamp: SystemTime::now(),
                kind: ServiceEventKind::OperationStarted,
                operation_id: Some(id),
                execution_id: None,
                state: None,
                reason: None,
            });
        }
        id
    }

    /// 标记操作为终态（Done/Failed/Cancelled）。从 in_flight 表移除，发出 `OperationFinished`。
    fn finish_op(&self, id: OperationId, status: ServiceOperationStatus, error: Option<String>) {
        let (name, source) = {
            let mut ops = self.operations.lock();
            let Some(op) = ops.get_mut(&id) else {
                return;
            };
            op.status = status;
            op.finished_at = Some(SystemTime::now());
            op.error = error.clone();
            (op.name.clone(), op.source)
        };
        let mut in_flight = self.in_flight_ops.write();
        // 仅当当前 in_flight 与本 op 匹配时才移除——避免覆盖后来的 op。
        let mut remove = Vec::new();
        for (entry_name, op_id) in in_flight.iter() {
            if *op_id == id {
                remove.push(entry_name.clone());
            }
        }
        for entry_name in remove {
            in_flight.remove(&entry_name);
        }
        drop(in_flight);
        if source != ServiceOperationSource::Scheduler {
            self.emit(ServiceEvent {
                schema_version: SERVICES_SCHEMA_VERSION,
                sequence: 0,
                name,
                timestamp: SystemTime::now(),
                kind: ServiceEventKind::OperationFinished,
                operation_id: Some(id),
                execution_id: None,
                state: None,
                reason: error.or_else(|| {
                    matches!(status, ServiceOperationStatus::Done).then(|| "ok".to_string())
                }),
            });
        }
    }

    /// 清扫已超过 [`OPERATION_RETENTION`] 的终态操作。
    ///
    /// 在每次 `operation()` 查询时懒清理；保留期内查询稳定返回。
    pub fn tick(&self) {
        let now = SystemTime::now();
        let mut ops = self.operations.lock();
        ops.retain(|_, op| match op.finished_at {
            Some(finished) => now
                .duration_since(finished)
                .map(|d| d < OPERATION_RETENTION)
                .unwrap_or(false),
            None => true,
        });
    }

    /// 查询操作当前快照。已超过保留期的返回 None。
    pub fn operation(&self, op: OperationId) -> Option<ServiceOperation> {
        self.tick();
        self.operations.lock().get(&op).cloned()
    }

    /// 当前会话已知服务快照；健康状态通过 inspect 主动刷新。
    pub fn list(&self) -> Vec<ServiceRecord> {
        self.records.read().values().cloned().collect()
    }

    /// 查询单个服务的展示快照（合并 `ServiceRecord` 与最近观察/操作状态）。
    ///
    /// 名称不在登记中时返回 `None`。`service_view_all` 会再补上未登记的 laya/qwen 预设。
    pub fn service_view(&self, name: &str) -> Option<ServiceView> {
        let records = self.records.read();
        let record = records.get(name).cloned()?;
        let observations = self.last_observations.read();
        let observation = observations
            .get(name)
            .filter(|o| o.run_version == record.run_version)
            .cloned();
        let in_flight = self.in_flight_ops.read();
        let in_flight_operation = in_flight.get(name).copied();
        drop(in_flight);
        drop(observations);
        drop(records);
        Some(view_from_record(record, observation, in_flight_operation))
    }

    /// 所有已知服务的展示快照——已登记 + 未登记的 laya/qwen 预设。
    ///
    /// 未登记预设以 `external: false, desired_running: false, state: Stopped` 标记，
    /// 让 UI 能区分「可发起 ensure」和「已被遗忘」。
    /// 顺序：`records` 的迭代序 + 固定追加 `laya, qwen`。
    pub fn service_view_all(&self) -> Vec<ServiceView> {
        let mut views: Vec<ServiceView> = {
            let records = self.records.read();
            let observations = self.last_observations.read();
            let in_flight = self.in_flight_ops.read();
            records
                .values()
                .map(|record| {
                    let observation = observations
                        .get(&record.spec.name)
                        .filter(|o| o.run_version == record.run_version)
                        .cloned();
                    let in_flight_operation = in_flight.get(&record.spec.name).copied();
                    view_from_record(record.clone(), observation, in_flight_operation)
                })
                .collect()
        };
        let mut present: std::collections::BTreeSet<String> =
            views.iter().map(|v| v.name.clone()).collect();
        for preset in ["laya", "qwen"] {
            if present.insert(preset.into()) {
                views.push(ServiceView {
                    name: preset.into(),
                    state: ServiceState::Stopped,
                    external: false,
                    desired_running: false,
                    last_observation: None,
                    in_flight_operation: None,
                    health_url: format!("http://127.0.0.1:{}/health", preset_port(preset)),
                    workdir: None,
                    startup_timeout_ms: 120000,
                    startup_command_display: format!("sh experiments/{preset}/start.sh"),
                    detail: "未登记的预设；按 s 启动".into(),
                    execution_id: None,
                    description: None,
                    description_updated_at: None,
                });
            }
        }
        views
    }

    fn persist(&self) -> Result<(), ProcessError> {
        let _guard = self.persist_lock.lock();
        crate::storage::save_services(self.processes.log_root(), &self.records.read())
    }

    /// 确保就绪。工作由管理器后台任务持有，丢弃等待者不会丢失服务所有权。
    ///
    /// 返回最新的 [`ServiceRecord`] 与本次操作的 [`OperationId`]。
    pub async fn ensure(
        &self,
        spec: ServiceSpec,
    ) -> Result<(ServiceRecord, OperationId), ProcessError> {
        let op_id = self.start_op(
            &spec.name,
            ServiceOperationKind::Ensure,
            ServiceOperationSource::User,
        );
        let this = self.clone();
        tokio::spawn(async move {
            let _operation = OperationGuard {
                manager: this.clone(),
                id: op_id,
            };
            let result = this.ensure_inner(spec).await;
            match &result {
                Ok(_) => this.finish_op(op_id, ServiceOperationStatus::Done, None),
                Err(e) => {
                    this.finish_op(op_id, ServiceOperationStatus::Failed, Some(e.to_string()))
                }
            }
            result.map(|record| (record, op_id))
        })
        .await
        .map_err(|e| ProcessError::Internal(e.to_string()))?
    }

    async fn ensure_inner(&self, spec: ServiceSpec) -> Result<ServiceRecord, ProcessError> {
        let url = if spec.health_url.is_empty() && spec.identity.is_empty() {
            None
        } else {
            Some(
                reqwest::Url::parse(&spec.health_url)
                    .map_err(|e| ProcessError::InvalidSpec(e.to_string()))?,
            )
        };
        if spec.name.is_empty()
            || spec.name.len() > 64
            || !spec
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || url.as_ref().is_some_and(|url| {
                url.scheme() != "http"
                    || !matches!(
                        url.host_str(),
                        Some("127.0.0.1") | Some("[::1]") | Some("::1")
                    )
                    || spec.identity.is_empty()
            })
            || !(1..=120000).contains(&spec.startup_timeout_ms)
        {
            return Err(ProcessError::InvalidSpec(
                "服务名称、loopback HTTP 地址、身份检查及启动时限必须合法".into(),
            ));
        }
        let gate = {
            let mut gates = self.gates.lock();
            if gates.len() >= 16 && !gates.contains_key(&spec.name) {
                return Err(ProcessError::InvalidSpec("服务数量上限为 16".into()));
            }
            gates.entry(spec.name.clone()).or_default().clone()
        };
        let _guard = gate.lock().await;
        let old = self.records.read().get(&spec.name).cloned();
        let expected_version = old.as_ref().map(|r| r.run_version);
        let old_run_version = old.as_ref().map(|o| o.run_version).unwrap_or_default();
        let old_description = old.as_ref().and_then(|o| o.description.clone());
        let old_description_updated_at = old.as_ref().and_then(|o| o.description_updated_at);
        if let Some(old) = old {
            if old.spec != spec {
                return Err(ProcessError::InvalidSpec(
                    "同名服务配置冲突；请使用新名称或原配置".into(),
                ));
            }
            if old.state == ServiceState::Unknown {
                if url.is_some() && self.health(&spec).await {
                    let mut records = self.records.write();
                    let current = records
                        .get_mut(&spec.name)
                        .ok_or_else(|| ProcessError::InvalidSpec("服务已被遗忘".into()))?;
                    if current.run_version != old_run_version || !current.desired_running {
                        return Ok(current.clone());
                    }
                    let recovered = ServiceRecord {
                        spec: spec.clone(),
                        state: ServiceState::Ready,
                        execution_id: None,
                        external: true,
                        desired_running: true,
                        detail: "恢复后通过身份健康检查；按外部服务复用，无权停止".into(),
                        description: current.description.clone(),
                        description_updated_at: current.description_updated_at,
                        run_version: old_run_version,
                    };
                    *current = recovered.clone();
                    drop(records);
                    self.persist()?;
                    self.emit_state(
                        &spec.name,
                        &ServiceState::Unknown,
                        &recovered.state,
                        Some(recovered.detail.clone()),
                    );
                    return Ok(recovered);
                }
                return Err(ProcessError::Internal(
                    "服务身份或清理结果未知；请人工确认后 service_forget，再显式 ensure".into(),
                ));
            }
            if old.desired_running
                && matches!(old.state, ServiceState::Ready | ServiceState::Degraded)
            {
                // 已经 Ready/Degraded：只跑一次 inspect 刷新健康观察，行为记录在 last_observations。
                let _ = self.inspect(&spec.name).await?;
                return Ok(self.records.read().get(&spec.name).cloned().unwrap_or(old));
            }
        }
        // 旧进程未确认结束时不重复启动，也不使用旧 PID 控制。
        for id in self.processes.list() {
            if let Some(record) = self.processes.inspect(id) {
                if record.owner.as_deref() == Some(&format!("service:{}", spec.name))
                    && (!record.is_terminal()
                        || matches!(
                            record.state,
                            ProcessState::Lost(_) | ProcessState::CleanupFailed(_)
                        ))
                {
                    return Err(ProcessError::Internal(
                        "同名服务仍有活跃或身份未知的执行".into(),
                    ));
                }
            }
        }
        let run_version = RunVersion(self.next_version.fetch_add(1, Ordering::Relaxed) + 1);
        {
            let mut records = self.records.write();
            // A stop/forget can invalidate the snapshot while ensure is preparing.
            if records.get(&spec.name).map(|r| r.run_version) != expected_version {
                return Err(ProcessError::Internal("服务在启动准备期间发生变化".into()));
            }
            let description = records
                .get(&spec.name)
                .and_then(|r| r.description.clone())
                .or(old_description);
            let description_updated_at = records
                .get(&spec.name)
                .and_then(|r| r.description_updated_at)
                .or(old_description_updated_at);
            records.insert(
                spec.name.clone(),
                ServiceRecord {
                    spec: spec.clone(),
                    state: ServiceState::Starting,
                    execution_id: None,
                    external: false,
                    desired_running: true,
                    detail: "等待启动与协议健康检查".into(),
                    description,
                    description_updated_at,
                    run_version,
                },
            );
            drop(records);
        }
        self.persist()?;
        let occupied = if let Some(url) = &url {
            let host = url.host_str().unwrap().trim_matches(['[', ']']);
            let port = url.port_or_known_default().unwrap();
            tokio::time::timeout(
                Duration::from_millis(300),
                tokio::net::TcpStream::connect((host, port)),
            )
            .await
            .is_ok_and(|r| r.is_ok())
        } else {
            false
        };
        if occupied {
            let healthy = self.health(&spec).await;
            let mut records = self.records.write();
            let record = records
                .get_mut(&spec.name)
                .ok_or_else(|| ProcessError::InvalidSpec("服务已被遗忘".into()))?;
            if !record.desired_running || record.run_version != run_version {
                return Ok(record.clone());
            }
            let old_state = record.state.clone();
            record.external = true;
            record.state = if healthy {
                ServiceState::Ready
            } else {
                ServiceState::Failed
            };
            record.detail = if healthy {
                "复用健康外部服务；无权停止"
            } else {
                "端口已被其他或不健康服务占用，未启动、未接管"
            }
            .into();
            let result = record.clone();
            drop(records);
            self.persist()?;
            self.emit_state(
                &spec.name,
                &old_state,
                &result.state,
                Some(result.detail.clone()),
            );
            return Ok(result);
        }
        {
            let mut records = self.records.write();
            let record = records
                .get_mut(&spec.name)
                .ok_or_else(|| ProcessError::InvalidSpec("服务已被遗忘".into()))?;
            if !record.desired_running || record.run_version != run_version {
                return Ok(record.clone());
            }
            // 启动新执行：bump run_version，让任何飞行中的 inspect 结果在 commit 时被丢弃。
            let mut process = spec.process.clone();
            process.scope = ExecutionScope::SessionService;
            match self.processes.start_internal(
                process,
                Some(format!("service:{}", spec.name)),
                ExecutionMetadata {
                    source: Some("service".into()),
                    objective: Some(format!("ensure service {}", spec.name)),
                    ..ExecutionMetadata::default()
                },
                true,
            ) {
                Ok(id) => record.execution_id = Some(id),
                Err(e) => {
                    let old_state = record.state.clone();
                    record.state = ServiceState::Failed;
                    record.detail = e.to_string();
                    let result = record.clone();
                    drop(records);
                    self.persist()?;
                    self.emit_state(
                        &spec.name,
                        &old_state,
                        &result.state,
                        Some(result.detail.clone()),
                    );
                    return Ok(result);
                }
            }
        }
        self.persist()?;
        let until = tokio::time::Instant::now() + Duration::from_millis(spec.startup_timeout_ms);
        loop {
            let _ = self.inspect(&spec.name).await?;
            let record = self
                .records
                .read()
                .get(&spec.name)
                .cloned()
                .ok_or_else(|| ProcessError::Internal("服务记录丢失".into()))?;
            if !record.desired_running
                || record.run_version != run_version
                || matches!(
                    record.state,
                    ServiceState::Ready | ServiceState::Failed | ServiceState::Unknown
                )
            {
                return Ok(record);
            }
            if tokio::time::Instant::now() >= until {
                let (mut record, _op) = self.stop(&spec.name).await?;
                let old_state = record.state.clone();
                if record.state == ServiceState::Stopped {
                    record.state = ServiceState::Failed;
                }
                record.detail = "启动就绪超时，已请求清理".into();
                let mut records = self.records.write();
                if let Some(current) = records.get_mut(&spec.name) {
                    if current.run_version == record.run_version {
                        current.state = record.state.clone();
                        current.detail = record.detail.clone();
                    }
                }
                drop(records);
                self.persist()?;
                self.emit_state(
                    &spec.name,
                    &old_state,
                    &record.state,
                    Some(record.detail.clone()),
                );
                return Ok(record);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn health(&self, spec: &ServiceSpec) -> bool {
        let Ok(mut response) = self.client.get(&spec.health_url).send().await else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= 16384 => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => return false,
            }
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        spec.identity
            .iter()
            .all(|(key, expected)| value.get(key) == Some(expected))
    }

    /// 刷新进程及健康状态；不会自动重启。
    ///
    /// 返回本次观察的 [`ServiceObservation`] 与 [`OperationId`]。
    /// 调用方需要最新 [`ServiceRecord`] 时用 [`ServiceManager::service_view`]。
    pub async fn inspect(
        &self,
        name: &str,
    ) -> Result<(ServiceObservation, OperationId), ProcessError> {
        self.inspect_with_source(name, ServiceOperationSource::User)
            .await
    }

    /// 与 `inspect` 相同，但操作来源（影响事件流）可由调用方指定。
    /// 调度器用 `Scheduler`；故障恢复用 `Recover`。
    pub async fn inspect_with_source(
        &self,
        name: &str,
        source: ServiceOperationSource,
    ) -> Result<(ServiceObservation, OperationId), ProcessError> {
        let op_id = self.start_op(name, ServiceOperationKind::Inspect, source);
        let _operation = OperationGuard {
            manager: self.clone(),
            id: op_id,
        };
        let result = self.inspect_inner(name).await;
        match &result {
            Ok(_) => self.finish_op(op_id, ServiceOperationStatus::Done, None),
            Err(e) => self.finish_op(op_id, ServiceOperationStatus::Failed, Some(e.to_string())),
        }
        result.map(|obs| (obs, op_id))
    }

    async fn inspect_inner(&self, name: &str) -> Result<ServiceObservation, ProcessError> {
        if !self.records.read().contains_key(name) {
            return Err(ProcessError::InvalidSpec("未知服务".into()));
        }
        let gate = self.probes.lock().entry(name.into()).or_default().clone();
        let _probe = gate.lock().await;
        let _permit = self
            .probe_permits
            .acquire()
            .await
            .map_err(|e| ProcessError::Internal(e.to_string()))?;
        let mut record = self
            .records
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| ProcessError::InvalidSpec("未知服务".into()))?;
        let run_version = record.run_version;
        let execution_id = record.execution_id;
        let started = std::time::Instant::now();
        if record.desired_running {
            if record.state == ServiceState::Unknown {
                if !record.spec.health_url.is_empty() && self.health(&record.spec).await {
                    record.state = ServiceState::Ready;
                    record.external = true;
                    record.execution_id = None;
                    record.detail = "恢复后通过身份健康检查；按外部服务复用，无权停止".into();
                } else if let Some(detail) = confirmed_exit_detail(&self.processes, &record) {
                    record.state = ServiceState::Failed;
                    record.detail = detail;
                } else {
                    record.detail = unknown_recovery_detail(&self.processes, &record);
                }
            } else if let Some(id) = execution_id {
                if !record.spec.health_url.is_empty() && self.health(&record.spec).await {
                    record.state = ServiceState::Ready;
                    record.detail = "健康协议验证通过".into();
                } else if !record.spec.health_url.is_empty()
                    && record.state != ServiceState::Starting
                {
                    record.state = ServiceState::Degraded;
                    record.detail = "健康或身份检查失败".into();
                }
                // Re-read after the HTTP await: the process may have exited meanwhile.
                match self.processes.inspect(id) {
                    Some(process) if process.is_terminal() => {
                        record.state = if matches!(
                            process.state,
                            ProcessState::Lost(_) | ProcessState::CleanupFailed(_)
                        ) {
                            ServiceState::Unknown
                        } else {
                            ServiceState::Failed
                        };
                        record.detail = format!("服务进程结束：{:?}", process.state);
                    }
                    Some(process)
                        if record.spec.health_url.is_empty()
                            && process.state == ProcessState::Running =>
                    {
                        record.state = ServiceState::Ready;
                        record.detail = "进程已启动；未配置健康/身份检查".into();
                    }
                    None => {
                        record.state = ServiceState::Unknown;
                        record.detail = "受管执行记录已不可用".into();
                    }
                    _ => {}
                }
            } else if record.external {
                let healthy = self.health(&record.spec).await;
                record.state = if healthy {
                    ServiceState::Ready
                } else {
                    ServiceState::Degraded
                };
                record.detail = if healthy {
                    "健康协议验证通过"
                } else {
                    "健康或身份检查失败"
                }
                .into();
            }
        }
        let observation = ServiceObservation {
            name: name.into(),
            observed_at: SystemTime::now(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            ok: matches!(record.state, ServiceState::Ready) && record.desired_running,
            reason: record.detail.clone(),
            run_version,
        };
        let transition = {
            let mut records = self.records.write();
            let Some(current) = records.get_mut(name) else {
                return Ok(observation);
            };
            if current.run_version != run_version
                || current.execution_id != execution_id
                || current.desired_running != record.desired_running
            {
                return Ok(observation);
            }
            let old = current.state.clone();
            let changed = current.state != record.state
                || current.detail != record.detail
                || current.external != record.external
                || current.execution_id != record.execution_id;
            // Preserve description and any unrelated metadata updated during the probe.
            current.state = record.state.clone();
            current.detail = record.detail.clone();
            current.external = record.external;
            current.execution_id = record.execution_id;
            self.last_observations
                .write()
                .insert(name.into(), observation.clone());
            (old, changed)
        };
        if transition.1 {
            self.persist()?;
        }
        if transition.0 != record.state {
            self.emit_state(name, &transition.0, &record.state, Some(record.detail));
        }
        self.emit_health(&observation);
        Ok(observation)
    }

    /// 发出 `HealthObserved` 事件（含 ok 状态和 reason 预览）。
    fn emit_health(&self, observation: &ServiceObservation) {
        let preview: String = observation.reason.chars().take(120).collect();
        self.emit(ServiceEvent {
            schema_version: SERVICES_SCHEMA_VERSION,
            sequence: 0,
            name: observation.name.clone(),
            timestamp: observation.observed_at,
            kind: ServiceEventKind::HealthObserved,
            operation_id: None,
            execution_id: None,
            state: None,
            reason: Some(preview),
        });
    }

    /// 发出 `StateChanged` 事件（仅在 state 实际变化时）。
    fn emit_state(
        &self,
        name: &str,
        from: &ServiceState,
        to: &ServiceState,
        reason: Option<String>,
    ) {
        if from == to {
            return;
        }
        self.emit(ServiceEvent {
            schema_version: SERVICES_SCHEMA_VERSION,
            sequence: 0,
            name: name.into(),
            timestamp: SystemTime::now(),
            kind: ServiceEventKind::StateChanged,
            operation_id: None,
            execution_id: None,
            state: Some(to.clone()),
            reason,
        });
    }

    /// 停止受管服务。外部服务不会被发送信号。
    ///
    /// 返回停止后的 [`ServiceRecord`] 与本次操作的 [`OperationId`]。
    /// 状态变更（包括 `desired_running = false` 与 `state = Stopped`）只在
    /// 该 op 仍是当前 in_flight op 时才会提交——并发 ensure / forget 不会
    /// 被覆盖。
    pub async fn stop(&self, name: &str) -> Result<(ServiceRecord, OperationId), ProcessError> {
        let this = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let op_id = this.start_op(
                &name,
                ServiceOperationKind::Stop,
                ServiceOperationSource::User,
            );
            let _operation = OperationGuard {
                manager: this.clone(),
                id: op_id,
            };
            let result = this.stop_inner(&name).await;
            match &result {
                Ok(_) => this.finish_op(op_id, ServiceOperationStatus::Done, None),
                Err(e) => {
                    this.finish_op(op_id, ServiceOperationStatus::Failed, Some(e.to_string()))
                }
            }
            result.map(|record| (record, op_id))
        })
        .await
        .map_err(|e| ProcessError::Internal(e.to_string()))?
    }

    async fn stop_inner(&self, name: &str) -> Result<ServiceRecord, ProcessError> {
        let (execution_id, version) = {
            let mut records = self.records.write();
            let record = records
                .get_mut(name)
                .ok_or_else(|| ProcessError::InvalidSpec("未知服务".into()))?;
            if record.external {
                return Err(ProcessError::InvalidSpec(
                    "外部服务不属于本管理器，无权停止".into(),
                ));
            }
            if record.state == ServiceState::Unknown {
                return Err(ProcessError::InvalidSpec(
                    "服务身份未知，不能确认停止权".into(),
                ));
            }
            record.desired_running = false;
            record.run_version = RunVersion(self.next_version.fetch_add(1, Ordering::Relaxed) + 1);
            record.detail = "正在停止".into();
            (record.execution_id, record.run_version)
        };
        // Persist intent before waiting; read-only probes cannot cancel it.
        self.persist()?;
        let outcome = if let Some(id) = execution_id {
            self.processes.stop(id).await?;
            match self.processes.wait_for(id, Duration::from_secs(6)).await {
                Ok(process)
                    if matches!(
                        process.state,
                        ProcessState::Exited(_)
                            | ProcessState::Rejected(_)
                            | ProcessState::SpawnFailed(_)
                    ) =>
                {
                    StopOutcome::Stopped
                }
                result => StopOutcome::Unconfirmed(format!("{result:?}")),
            }
        } else {
            StopOutcome::Stopped
        };
        let (record, old_state) = {
            let mut records = self.records.write();
            let record = records
                .get_mut(name)
                .ok_or_else(|| ProcessError::InvalidSpec("服务已被遗忘".into()))?;
            if record.run_version != version {
                return Ok(record.clone());
            }
            let old_state = record.state.clone();
            match outcome {
                StopOutcome::Stopped => {
                    record.state = ServiceState::Stopped;
                    record.detail = "已停止".into();
                }
                StopOutcome::Unconfirmed(reason) => {
                    record.state = ServiceState::Unknown;
                    record.detail = format!("停止未确认：{reason}");
                }
            }
            (record.clone(), old_state)
        };
        self.persist()?;
        self.emit_state(name, &old_state, &record.state, Some(record.detail.clone()));
        Ok(record)
    }

    /// 清除已经停止、失败或身份未知的服务登记，不会向任何 PID 发送信号。
    ///
    /// 返回被遗忘的 [`ServiceRecord`] 与本次操作的 [`OperationId`]。
    pub fn forget(&self, name: &str) -> Result<(ServiceRecord, OperationId), ProcessError> {
        let op_id = self.start_op(
            name,
            ServiceOperationKind::Forget,
            ServiceOperationSource::User,
        );
        let result = self.forget_inner(name);
        match &result {
            Ok(_) => self.finish_op(op_id, ServiceOperationStatus::Done, None),
            Err(e) => self.finish_op(op_id, ServiceOperationStatus::Failed, Some(e.to_string())),
        }
        result.map(|record| (record, op_id))
    }

    fn forget_inner(&self, name: &str) -> Result<ServiceRecord, ProcessError> {
        let mut records = self.records.write();
        let record = records
            .get(name)
            .cloned()
            .ok_or_else(|| ProcessError::InvalidSpec("未知服务".into()))?;
        if !matches!(
            record.state,
            ServiceState::Stopped | ServiceState::Failed | ServiceState::Unknown
        ) {
            return Err(ProcessError::InvalidSpec(
                "只允许遗忘已停止、失败或身份未知的服务".into(),
            ));
        }
        let owner = format!("service:{name}");
        if self.processes.list().into_iter().any(|id| {
            self.processes
                .inspect(id)
                .is_some_and(|p| p.owner.as_deref() == Some(&owner) && !p.is_terminal())
        }) {
            return Err(ProcessError::InvalidSpec(
                "服务仍有受管执行，不能遗忘".into(),
            ));
        }
        records.remove(name);
        self.last_observations.write().remove(name);
        drop(records);
        self.persist()?;
        Ok(record)
    }

    /// 更新服务的用户可见说明。
    ///
    /// 仅修改 `description` 与时间戳；不动 `desired_running`、执行 ID 或状态机。
    /// 服务未登记时返回 `InvalidSpec`；长度受 [`Self::MAX_DESCRIPTION_BYTES`] 限制。
    pub fn update_description(&self, name: &str, description: String) -> Result<(), ProcessError> {
        if description.len() > Self::MAX_DESCRIPTION_BYTES {
            return Err(ProcessError::InvalidSpec(format!(
                "description 超过 {} 字节上限",
                Self::MAX_DESCRIPTION_BYTES
            )));
        }
        let mut records = self.records.write();
        let record = records
            .get_mut(name)
            .ok_or_else(|| ProcessError::InvalidSpec("未知服务".into()))?;
        record.description = Some(description.clone());
        record.description_updated_at = Some(SystemTime::now());
        let preview: String = description.chars().take(120).collect();
        drop(records);
        self.persist()?;
        self.emit(ServiceEvent {
            schema_version: SERVICES_SCHEMA_VERSION,
            sequence: 0, // emit 会重新分配序号
            name: name.into(),
            timestamp: SystemTime::now(),
            kind: ServiceEventKind::DescriptionUpdated,
            operation_id: None,
            execution_id: None,
            state: None,
            reason: Some(preview),
        });
        Ok(())
    }

    /// 服务说明长度上限（含 UTF-8 多字节字符，按字节计）。
    pub const MAX_DESCRIPTION_BYTES: usize = 4096;

    /// 订阅事件流。返回的 receiver 在 broadcast 通道满时收到 `RecvError::Lagged`。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ServiceEvent> {
        self.event_tx.subscribe()
    }

    /// 当前订阅者数量（测试用）。
    pub fn subscriber_count(&self) -> usize {
        self.event_tx.receiver_count()
    }

    /// 启动周期健康调度：每 [`HEALTH_SCHEDULER_INTERVAL`] 跑一次 `inspect`，
    /// 同时最多 [`HEALTH_SCHEDULER_CONCURRENCY`] 个服务并行。
    ///
    /// 调度来源是 `ServiceOperationSource::Scheduler`，事件流可区分。
    /// 多次调用会生成多个独立 handle；不需要时用 `HealthSchedulerHandle::stop`
    /// 或 drop 句柄停掉。
    pub fn start_health_scheduler(&self) -> HealthSchedulerHandle {
        self.start_health_scheduler_with(HEALTH_SCHEDULER_INTERVAL, HEALTH_SCHEDULER_CONCURRENCY)
    }

    /// 自定义参数版本（测试可调短间隔）。
    pub fn start_health_scheduler_with(
        &self,
        interval: Duration,
        concurrency: usize,
    ) -> HealthSchedulerHandle {
        assert!(concurrency > 0, "调度器并发数必须 > 0");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = stop.clone();
        let this = self.clone();
        let task = tokio::spawn(async move {
            let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // 第一次 tick 立即触发；之后按 interval 周期。
            loop {
                ticker.tick().await;
                if stop_signal.load(Ordering::SeqCst) {
                    break;
                }
                let names: Vec<String> = {
                    let records = this.records.read();
                    records
                        .values()
                        .filter(|r| r.desired_running && r.state != ServiceState::Starting)
                        .map(|r| r.spec.name.clone())
                        .collect()
                };
                let mut tasks = tokio::task::JoinSet::new();
                for name in names {
                    let permit = semaphore.clone().acquire_owned().await;
                    let Ok(permit) = permit else {
                        break;
                    };
                    let this = this.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _ = this
                            .inspect_with_source(&name, ServiceOperationSource::Scheduler)
                            .await;
                    });
                }
                while tasks.join_next().await.is_some() {}
                this.tick();
            }
        });
        HealthSchedulerHandle { stop, task }
    }

    /// 发出事件：分配序号、写入 broadcast + service-events.jsonl，溢出仅丢事件。
    fn emit(&self, mut event: ServiceEvent) {
        event.sequence = self
            .event_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        // 优先写盘（落盘不阻塞 broadcast 路径），但失败仅记录 warn。
        if let Ok(bytes) = serde_json::to_vec(&event) {
            self.event_log.lock().append(&bytes);
        }
        // 忽略 send 错误：没有订阅者时是常态；溢出仅丢事件，不阻塞 emit 路径。
        let _ = self.event_tx.send(event);
    }
}

// Reuse durable exit evidence without trusting or signalling a persisted PID.
fn confirmed_exit_detail(processes: &ProcessManager, record: &ServiceRecord) -> Option<String> {
    let process = processes.inspect(record.execution_id?)?;
    if process.owner.as_deref() != Some(format!("service:{}", record.spec.name).as_str()) {
        return None;
    }
    match &process.state {
        ProcessState::Exited(exit) if exit.reason != crate::ExitReason::Lost => {}
        ProcessState::Rejected(_) | ProcessState::SpawnFailed(_) => {}
        _ => return None,
    }
    Some(format!(
        "历史执行已确认结束：{:?}；可显式启动",
        process.state
    ))
}

fn unknown_recovery_detail(processes: &ProcessManager, record: &ServiceRecord) -> String {
    let evidence = record
        .execution_id
        .and_then(|id| processes.inspect(id))
        .map(|process| format!("历史执行：{:?}", process.state))
        .unwrap_or_else(|| "历史执行证据不可用".into());
    format!("服务身份尚未确认；{evidence}。不会按旧 PID 操作；需确认旧实例已结束后再处理登记")
}

/// 把 `ServiceRecord` 折叠成 `ServiceView`。
fn view_from_record(
    record: ServiceRecord,
    last_observation: Option<ServiceObservation>,
    in_flight_operation: Option<OperationId>,
) -> ServiceView {
    let startup_command_display = if !record.spec.process.command.is_empty() {
        format!("sh -c {}", record.spec.process.command)
    } else if let Some(program) = record.spec.process.program.as_deref() {
        format!("{} {}", program, record.spec.process.args.join(" "))
    } else {
        String::new()
    };
    ServiceView {
        name: record.spec.name.clone(),
        state: record.state.clone(),
        external: record.external,
        desired_running: record.desired_running,
        last_observation,
        in_flight_operation,
        health_url: record.spec.health_url.clone(),
        workdir: record.spec.process.cwd.clone(),
        startup_timeout_ms: record.spec.startup_timeout_ms,
        startup_command_display,
        detail: record.detail.clone(),
        execution_id: record.execution_id,
        description: record.description,
        description_updated_at: record.description_updated_at,
    }
}

/// 预设服务的固定端口（与 `service_preset` 保持一致）。
fn preset_port(name: &str) -> u16 {
    match name {
        "laya" => 3031,
        "qwen" => 3032,
        _ => 0,
    }
}
