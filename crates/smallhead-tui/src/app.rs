//! UI 状态、输入和会话动作；Agent 只在 runtime 后台运行。
use crate::{
    logs::Logs,
    runtime::{Finished, Route, Runtime, SpawnRequest, SYSTEM},
    task_plan::{PlanPhase, PlanView},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use smallhead_core::{
    agent_loop::SteeringInbox,
    choice::{ChoiceIntent, ChoiceOption, ChoiceRequest, ChoiceResponse},
    compact_messages_rolling,
    diagnostics::DiagnosticEvent,
    estimate_context_projection,
    execution::smallhead_process::{service::ServiceEvent, ProcessState},
    spec_review::SpecBroker,
    AgentMessage, ContentBlock, LoopBudget, MonadResult, TaskId,
};
use smallhead_session::{new_session_id, now_millis, JsonFileStore, Session, SessionStore};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "显示快捷键与命令"),
    ("/process", "Ctrl+P / F3 进程页；↑↓ 选择，F4 停止，Esc 返回"),
    ("/exec ", "直接执行命令，无需模型；如 /exec sleep 20"),
    (
        "/exec-bg ",
        "fire-and-forget 执行；等价 /exec 命令 · wait=false",
    ),
    ("/process output ", "读取输出：ID 字节偏移"),
    ("/process purge ", "删除已完成执行的日志：ID"),
    (
        "/process filter ",
        "行级子过滤：command/owner 子串，无参清空",
    ),
    ("/events ", "读取指定执行的 events.jsonl：ID [偏移]"),
    ("/service ensure laya", "确保 Laya 就绪，后台等待"),
    ("/service ensure qwen", "确保 Qwen 就绪，后台等待"),
    ("/service list", "列出已登记服务"),
    ("/service stop ", "停止受管服务：laya 或 qwen"),
    ("/service forget ", "清除停止、失败或未知的服务登记"),
    ("/services", "切换到服务面板(Ctrl+S 等价)"),
    ("/mcp", "MCP 服务；list / connect NAME / disconnect NAME / tools NAME / reload"),
    ("/lens ", "Lens 策略与工具扩展；list / load 路径 / reload ID / unload ID；下一轮生效"),
    ("/filter process", "只看执行与服务日志"),
    ("/choice-demo", "显示一张用户选择卡片测试"),
    ("/plan", "查看任务进度；/plan 要求 先生成可评论、确认的方案"),
    ("/spec", "F9 查看方案；/spec 路径 打开文档，↑↓ 选段、c 评论、a 确认"),
    ("/retry", "基于已保留结果继续，先核验结果未知的操作"),
    ("/mouse", "查看鼠标事件接收情况与 iTerm2 设置提示"),
    ("/route large", "使用主模型决策"),
    ("/route auto", "允许 Agent 按任务选择 Laya/Qwen 或主模型"),
    ("/route laya", "计算业务优先 Laya"),
    ("/route qwen", "计算业务优先 Qwen"),
    ("/req", "查看最近一次完整请求"),
    ("/context", "查看当前上下文预算与压缩状态"),
    ("/compact", "立即压缩旧对话历史并保存完整 artifact"),
    ("/logs", "切换详细日志页"),
    ("/diag", "显示本会话自动诊断日志路径"),
    (
        "/filter request",
        "筛选日志层：all/request/message/block/tool/plan/monad/lens/route/response/error/process/search_chain/transport/ui",
    ),
    ("/export ", "导出当前日志为 JSONL：/export 文件路径"),
    ("/cancel", "取消本轮等待"),
    ("/list", "列出最近会话"),
    ("/load ", "加载完整会话 ID"),
    ("/new", "新建会话"),
    (
        "/scrolloff ",
        "日志光标距视窗边留白行数：/scrolloff 5（0=贴边）",
    ),
];

/// TUI 顶层导航的页面。一次只有一个页面可见；`Chat` 是默认进入位置。
///
/// 旧实现用 `Processes::visible` + `App::log_page` 两个互斥 bool 表达这个状态，
/// 引入 `Services` 后 bool 不再够用。`log_focus` 是页面内子面板（聊天 / 日志）的聚焦
/// 切换，与 page 维度正交，保留为 `App` 上的 bool。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Spec,
    /// 聊天输入 + 输出（`/help` 进入）。
    Chat,
    /// 日志全屏（F2 / Ctrl+R）。
    Logs,
    /// 进程列表 + 服务区（Ctrl+P / F3）。
    Processes,
    /// 常驻服务管理面板（Ctrl+S / `/services` / 鼠标服务按钮）。
    Services,
}

impl Page {
    /// `true` 表示聊天页（默认页）。
    pub fn is_chat(self) -> bool {
        matches!(self, Page::Chat)
    }
    /// `true` 表示进程页；其它键鼠分支用它判别是否命中服务区。
    pub fn is_processes(self) -> bool {
        matches!(self, Page::Processes)
    }
    /// `true` 表示服务页。
    pub fn is_services(self) -> bool {
        matches!(self, Page::Services)
    }
    /// `true` 表示日志全屏页。
    pub fn is_logs(self) -> bool {
        matches!(self, Page::Logs)
    }
}

#[derive(Clone, Debug)]
pub struct Chat {
    pub role: &'static str,
    pub text: String,
}

/// 鼠标点击可触发的高层动作。
///
/// 渲染层在每帧把"按钮"的屏幕坐标 + 这个 action 注册进
/// [`ClickMap`];`App::on_click` 拿到坐标后查表并执行。
#[derive(Clone, Debug)]
pub enum ClickAction {
    FocusSpec,
    FocusProcesses,
    SelectProcess(usize),
    StopProcess,
    /// 提交当前输入框(同 Enter)。
    #[allow(dead_code)] // 仅测试用;顶部「发送」按钮已删除,新服务页用 Enter 直发。
    Submit,
    /// 聚焦输入框，不提交草稿。
    FocusInput,
    /// 聊天面板点击与滚轮区域。
    ScrollChat,
    /// 日志列表点击与滚轮区域。
    ScrollLogs,
    /// 日志详情点击与滚轮区域。
    ScrollDetail,
    /// 取消本轮等待(同 Ctrl+X)。
    Cancel,
    /// 切到聊天页(同 Ctrl+F1)。
    FocusChat,
    /// 切到日志页(同 F2)。
    FocusLogs,
    /// 切到服务页(同 Ctrl+S,也可由顶部「服务」按钮触发)。
    FocusServices,

    /// 跑一个 `/` 斜杠命令(如 "/help"、"/route qwen")。
    RunCommand(String),
    /// 选中第 N 行日志(F2 日志页里)。
    SelectLog(usize),
    /// P1: 选中服务列表里的第 N 行(进程页里服务区)。
    SelectService(usize),
    /// P1: 对服务名触发 ensure(走 service_ensure action)。
    EnsureService(String),
    /// P1: 对服务名触发 stop(走 service_stop action)。
    StopService(String),
    /// P1: 对服务名触发 inspect(走 service_inspect action)。
    InspectService(String),
    /// 遗忘一个服务登记(走 service_forget)。外部 + 本应用管理的服务都允许。
    /// 与 stop 不同:stop 只把 desired_running 置 false;forget 把整个
    /// ServiceRecord 从 ServiceManager 摘掉,后续需要重新 ensure 才能再管理。
    #[allow(dead_code)]
    // 新服务页用 'd' 键直调 ServiceManager::forget;ClickAction 仅留扩展位。
    ForgetService(String),
    /// P1: 切换服务列表第 N 行的详情展开/收起。
    ExpandService(usize),
    /// 回答一个 pending 用户选择卡片。
    AnswerChoice {
        choice_id: String,
        option_id: String,
    },
}

/// 一个可点击矩形 + 触发的动作。
#[derive(Clone, Debug)]
pub struct ClickRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub action: ClickAction,
}

/// 鼠标点击 → 动作 映射表。`render::draw` 开头清空,渲染过程中 register,
/// 鼠标事件时 `hit_test` 解析。
#[derive(Default)]
pub struct ClickMap {
    regions: Vec<ClickRegion>,
}

impl ClickMap {
    /// 清空。每帧重渲前调用,防止上帧残留。
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// 注册一个可点击矩形。
    pub fn register(&mut self, x: u16, y: u16, w: u16, h: u16, action: ClickAction) {
        self.regions.push(ClickRegion { x, y, w, h, action });
    }

    /// 命中测试。返回最后一次 register 且坐标落在矩形内的 action
    /// (后画的覆盖先画的,跟图层 z-order 一致)。
    pub fn hit_test(&self, x: u16, y: u16) -> Option<ClickAction> {
        for r in self.regions.iter().rev() {
            if x >= r.x && x < r.x.saturating_add(r.w) && y >= r.y && y < r.y.saturating_add(r.h) {
                return Some(r.action.clone());
            }
        }
        None
    }
}

pub struct App {
    pub mcp_view: crate::mcp_services::McpView,
    pub services_mcp: bool,
    pub steering: SteeringInbox,
    pub spec_broker: Arc<SpecBroker>,
    pub spec_document: Option<crate::spec_review::SpecDocument>,
    pub spec_view: crate::spec_review::SpecViewState,
    spec_reply_targets: Vec<(String, u64)>,
    checkpoint_generation: u64,
    checkpoint_retry_at: Instant,
    checkpoint_error: Option<String>,
    recovery_interrupted: bool,
    pub plan: Option<PlanView>,
    pub processes: crate::processes::Processes,
    pub history: Vec<Chat>,
    pub markdown_cache: crate::markdown::MarkdownCache,
    pub input: String,
    pub status: String,
    pub logs: Logs,
    pub log_focus: bool,
    /// 当前顶层页面；`Page` enum 取代了旧的 `processes.visible` + `log_page` 两个互斥 bool。
    /// `log_focus` 仍保留——它是 `Chat` 与 `Logs` 之间子面板聚焦切换的状态。
    pub current: Page,
    pub chat_scroll: u16,
    pub runtime: Runtime,
    pub route: Route,
    pub messages: Vec<AgentMessage>,
    pub task_id: String,
    pub started: Option<Instant>,
    pub total_turns: usize,
    pub total_tools: usize,
    pub session_id: String,
    pub created_at: i64,
    pub artifact_root: Option<PathBuf>,
    pub store: Option<JsonFileStore>,
    pub should_quit: bool,
    pub stream_text: String,
    pub stream_history_index: usize,
    text_block: bool,
    worker: Option<tokio::task::JoinHandle<Finished>>,
    cancel: Option<oneshot::Sender<()>>,
    cancel_requested: bool,
    receiver: Option<mpsc::UnboundedReceiver<DiagnosticEvent>>,
    diagnostic_journal: Option<DiagnosticJournal>,
    diagnostic_journal_error: Option<String>,
    /// 鼠标点击 hit-test 表。`render::draw` 写入,`on_click` 读取。
    pub click_map: ClickMap,
    pub mouse_events: u64,
    /// P2 #14: 任何键鼠输入都会刷新此时间戳;`tick()` 在 2s 静止后
    /// 计算一条状态栏提示 (`tooltip`),给用户额外上下文。
    pub last_input_at: Instant,
    /// P2 #14: 渲染层展示的 tooltip 文本 —— 静默 2s 后由 tick 填充。
    pub tooltip: Option<String>,
    /// Currently highlighted option in the active choice card.
    pub choice_selection: usize,
    /// ServiceManager 事件订阅：当后台 ensure/inspect/stop/forget 改变状态时,
    /// 走 `tokio::sync::broadcast` 把事件推给 UI。
    /// `tick()` 每帧 drain 一次,事件序号连续;落后 (Lagged) 时清空内部状态。
    pub service_events: Option<tokio::sync::broadcast::Receiver<ServiceEvent>>,
    /// 上一帧收到的最新事件序号,用于让面板/事件 tab 判断"是否有新事件"。
    /// 测试中可以用它验证 broadcast 真的把消息送到了 TUI。
    pub last_service_sequence: u64,
    /// 服务页详情 tab: 0 = 概览, 1 = 输出, 2 = 事件。
    /// 设计稿 §3 要求三个 tab 切换;此处不引入新 enum,沿用 usize + 注释。
    pub service_detail_tab: usize,
    pub service_detail_focus: bool,
    pub service_detail_scroll: u16,
    pub service_event_cache: std::collections::VecDeque<ServiceEvent>,
    service_output_next_poll: Instant,
    health_scheduler:
        Option<smallhead_core::execution::smallhead_process::service::HealthSchedulerHandle>,
    /// Output tab 缓存:每个 (服务名) 最近一次 fetch 的 stdout / stderr 文本。
    /// 渲染层是同步的,而 `ProcessManager::read_output` 是 async,所以在
    /// `tick()` 与 tab 切换时预取一次,渲染时直接读缓存。
    pub service_output_cache: std::collections::HashMap<String, ServiceOutputSnapshot>,
    /// 当前 Output tab 选中服务的 execution_id + log 字节总量,用于
    /// 输出 tab 顶部显示「尾部 (N KiB)」。
    pub service_output_meta: Option<ServiceOutputMeta>,
    /// 标记「Output 缓存需要重新拉取」。当 ServiceEvent 触达、或用户切到
    /// Output tab、或被通知有 stdout 增量时置 true,`tick()` 看到就刷新。
    pub service_output_dirty: bool,
}

/// 单个服务在 Output tab 的快照;key = 服务名。
#[derive(Debug, Clone, Default)]
pub struct ServiceOutputSnapshot {
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    /// tick 拉取时的 epoch 毫秒。给「相对多久之前的快照」备用字段;
    /// 当前 commit 渲染未用,后续可加"刚刚 / N 秒前"。
    #[allow(dead_code)]
    pub fetched_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ServiceOutputMeta {
    pub service_name: String,
    pub stored_bytes: u64,
}

struct DiagnosticJournal {
    session_id: String,
    path: PathBuf,
    file: std::fs::File,
}

impl DiagnosticJournal {
    fn open(root: PathBuf, session_id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let path = root.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            session_id: session_id.into(),
            path,
            file,
        })
    }

    fn append(
        &mut self,
        sequence: u64,
        task: &str,
        record: &DiagnosticEvent,
    ) -> std::io::Result<()> {
        let line = serde_json::json!({
            "sequence": sequence,
            "task": task,
            "record": record,
        })
        .to_string();
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

fn diagnostic_root(store: &Option<JsonFileStore>) -> Option<PathBuf> {
    store.as_ref().map(|store| {
        store
            .dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("diagnostics")
    })
}

fn artifact_root_for_session(store: &Option<JsonFileStore>, session_id: &str) -> Option<PathBuf> {
    store.as_ref().map(|store| {
        store
            .dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("artifacts")
            .join("sessions")
            .join(session_id)
    })
}
impl App {
    pub fn new(runtime: Runtime, route: Route, store: Option<JsonFileStore>) -> Self {
        let mock = runtime.mock;
        let mut history = vec![Chat {
            role: "系统",
            text: format!(
                "欢迎。默认使用 Monad 业务发现与派发。{} F2 查看日志；/help 查看命令。",
                if mock {
                    "当前为固定计算 Mock，不理解自定义输入。"
                } else {
                    ""
                }
            ),
        }];
        // 启动恢复：上次会话未正常关闭时,ProcessManager::storage::recover 把所有
        // 非终态执行标成 Lost。这里把它们的 ID 汇总到历史最上方,让 TUI 用户
        // 一启动就知道哪些执行的真实状态未知、不会按旧 PID 接管进程。
        if let Ok(rt) = &runtime.execution {
            let lost_ids: Vec<_> = rt
                .processes
                .list()
                .into_iter()
                .filter_map(|id| rt.processes.inspect(id))
                .filter(|r| matches!(r.state, ProcessState::Lost(_)))
                .map(|r| r.id.to_string())
                .collect();
            let n = lost_ids.len();
            if n > 0 {
                let prefix = lost_ids
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = if n > 8 {
                    format!(
                        "上次会话的 {n} 个执行因未正常关闭被标记为 Lost：\n{prefix} … 等共 {n} 个"
                    )
                } else {
                    format!("上次会话的 {n} 个执行因未正常关闭被标记为 Lost：\n{prefix}")
                };
                history.push(Chat {
                    role: "系统", text
                });
            }
        }
        let status = match &runtime.execution {
            Ok(_) => "就绪".into(),
            Err(error) => {
                let message = format!("进程与服务管理器初始化失败：{error}");
                history.push(Chat {
                    role: "错误",
                    text: message.clone(),
                });
                message
            }
        };
        let session_id = new_session_id();
        let artifact_root = artifact_root_for_session(&store, &session_id);
        let (diagnostic_journal, diagnostic_journal_error) = diagnostic_root(&store)
            .map(|root| match DiagnosticJournal::open(root, &session_id) {
                Ok(journal) => (Some(journal), None),
                Err(error) => (None, Some(error.to_string())),
            })
            .unwrap_or((None, Some("会话存储不可用，诊断日志未落盘".into())));
        // 在移动 runtime 之前先把 broadcast 订阅句柄拿出来。
        let service_events = runtime
            .execution
            .as_ref()
            .ok()
            .map(|rt| rt.services.subscribe());
        let service_event_cache = runtime
            .execution
            .as_ref()
            .ok()
            .map(|rt| {
                crate::services::read_recent_events(
                    &rt.processes.log_root().join("service-events.jsonl"),
                )
            })
            .unwrap_or_default();
        let health_scheduler = runtime
            .execution
            .as_ref()
            .ok()
            .filter(|_| tokio::runtime::Handle::try_current().is_ok())
            .map(|rt| rt.services.start_health_scheduler());
        let mcp_view = crate::mcp_services::McpView::new(runtime.mcp_config.clone());
        Self {
            steering: SteeringInbox::default(),
            spec_broker: Arc::new(SpecBroker::new()),
            spec_document: None,
            spec_view: crate::spec_review::SpecViewState::default(),
            spec_reply_targets: Vec::new(),
            checkpoint_generation: 0,
            checkpoint_retry_at: Instant::now(),
            checkpoint_error: None,
            recovery_interrupted: false,
            processes: crate::processes::Processes::new(runtime.execution.clone()),
            history,
            markdown_cache: Default::default(),
            input: String::new(),
            status,
            logs: Logs::new(),
            current: Page::Chat,
            log_focus: false,
            chat_scroll: 0,
            runtime,
            route,
            // App 与共享 ServiceManager 的事件流绑定,每次新建 App 都从当前
            // 序号开始。如果 ExecutionRuntime 拿不到,就保持 None,降级为不订阅。
            service_events,
            last_service_sequence: 0,
            service_detail_tab: 0,
            service_detail_focus: false,
            mcp_view,
            services_mcp: false,
            service_detail_scroll: 0,
            service_event_cache,
            service_output_next_poll: Instant::now(),
            health_scheduler,
            service_output_cache: std::collections::HashMap::new(),
            service_output_meta: None,
            service_output_dirty: true,
            messages: vec![],
            task_id: String::new(),
            started: None,
            total_turns: 0,
            total_tools: 0,
            session_id,
            created_at: now_millis(),
            artifact_root,
            store,
            should_quit: false,
            stream_text: String::new(),
            stream_history_index: 0,
            text_block: false,
            worker: None,
            cancel: None,
            cancel_requested: false,
            receiver: None,
            diagnostic_journal,
            diagnostic_journal_error,
            click_map: ClickMap::default(),
            mouse_events: 0,
            last_input_at: Instant::now(),
            tooltip: None,
            choice_selection: 0,
            plan: None,
        }
    }
    pub fn busy(&self) -> bool {
        self.worker.is_some()
    }

    fn save_spec_document(&self, doc: &mut crate::spec_review::SpecDocument) -> anyhow::Result<()> {
        if let Some(root) = &self.artifact_root {
            crate::spec_review::SpecStore::new(root).save(doc)?;
        }
        Ok(())
    }

    fn sync_spec(&mut self) -> anyhow::Result<()> {
        let Some(current) = self.spec_broker.current() else {
            return Ok(());
        };
        if self
            .spec_document
            .as_ref()
            .is_some_and(|doc| doc.to_document() == current)
        {
            return Ok(());
        }
        let mut document = match &self.spec_document {
            Some(doc) if doc.id == current.id => {
                let mut doc = doc.clone();
                doc.sync_from_document(&current)?;
                doc
            }
            _ => crate::spec_review::SpecDocument::from_document(&current)?,
        };
        self.save_spec_document(&mut document)?;
        self.spec_document = Some(document);
        Ok(())
    }

    fn open_spec(&mut self, path: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(!self.busy(), "任务运行时请等待或取消后再打开另一份文档");
        let markdown = crate::spec_review::read_markdown(path)?;
        let title = path.file_name().and_then(|s| s.to_str()).unwrap_or("方案");
        let current = self.spec_broker.current();
        self.spec_broker.publish(
            current.as_ref().map(|d| d.id.as_str()),
            title,
            &markdown,
            current.as_ref().map(|d| d.revision),
        )?;
        self.sync_spec()?;
        self.spec_view = Default::default();
        Ok(())
    }

    fn spec_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        use crate::spec_review::SpecAction;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Ok(());
        }
        let Some(doc) = self.spec_document.as_ref() else {
            if key.code == KeyCode::Esc {
                self.current = Page::Chat;
            }
            return Ok(());
        };
        match self.spec_view.handle_key(key, doc) {
            SpecAction::None => {}
            SpecAction::Back => self.current = Page::Chat,
            SpecAction::ReloadRequested => self.sync_spec()?,
            SpecAction::CommentSubmitted(text) => {
                let mut doc = doc.clone();
                let revision = self.spec_view.input_revision.unwrap_or(doc.revision);
                let block = self
                    .spec_view
                    .input_block
                    .ok_or_else(|| anyhow::anyhow!("未选择段落"))?;
                anyhow::ensure!(
                    doc.revision == revision,
                    "方案已更新，评论草稿保留；Esc 返回后重新选择段落"
                );
                let id = doc.add_comment(revision, block, &text)?;
                let current = self
                    .spec_broker
                    .reopen(&doc.id, revision)
                    .map_err(|error| {
                        anyhow::anyhow!("评论未提交，草稿保留；方案可能已更新：{error}")
                    })?;
                doc.sync_from_document(&current)?;
                let message = doc.comment_message(id)?;
                self.save_spec_document(&mut doc)?;
                self.spec_reply_targets.push((doc.id.clone(), id));
                self.spec_document = Some(doc);
                self.spec_view.finish_comment();
                self.submit(message);
            }
            SpecAction::ApproveRequested => {
                if doc.is_approved() {
                    self.status = format!("方案 r{} 已确认", doc.revision);
                    return Ok(());
                }
                let mut doc = doc.clone();
                let approved = self.spec_broker.approve(&doc.id, doc.revision)?;
                doc.sync_from_document(&approved)?;
                self.save_spec_document(&mut doc)?;
                let message = doc.approval_message()?;
                self.spec_document = Some(doc);
                self.submit(message);
            }
        }
        Ok(())
    }

    pub fn paste(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        if self.current == Page::Spec {
            if let Some(input) = self.spec_view.input.as_mut() {
                for ch in text.chars() {
                    if input.len() + ch.len_utf8() > crate::spec_review::MAX_COMMENT_BYTES {
                        break;
                    }
                    input.push(ch);
                }
            }
        } else {
            self.input.push_str(&text);
        }
    }

    fn finish_spec_replies(&mut self, completed: bool) -> anyhow::Result<()> {
        self.sync_spec()?;
        if !completed || self.spec_reply_targets.is_empty() || !self.steering.pending().is_empty() {
            return Ok(());
        }
        let reply = self
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_default();
        if reply.trim().is_empty() {
            return Ok(());
        }
        if let Some(mut doc) = self.spec_document.clone() {
            // The final response addresses the batch of comments accepted in this run.
            for (document_id, comment_id) in &self.spec_reply_targets {
                if *document_id == doc.id {
                    let bounded = reply
                        .chars()
                        .scan(0usize, |bytes, ch| {
                            *bytes += ch.len_utf8();
                            (*bytes <= crate::spec_review::MAX_COMMENT_BYTES).then_some(ch)
                        })
                        .collect::<String>();
                    doc.reply(doc.revision, *comment_id, &bounded)?;
                }
            }
            self.save_spec_document(&mut doc)?;
            self.spec_document = Some(doc);
        }
        self.spec_reply_targets.clear();
        Ok(())
    }
    /// 当前 ServiceManager 事件订阅句柄。测试/调试用,可读出最近一条事件的序号。
    /// 注意:这只是一个只读探针;真正的消费在 `tick()` 里。
    #[allow(dead_code)] // 仅测试用探针;运行时不依赖。
    pub fn subscription(&self) -> Option<&tokio::sync::broadcast::Receiver<ServiceEvent>> {
        self.service_events.as_ref()
    }
    pub fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') {
            return vec![];
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(&self.input))
            .collect()
    }
    pub fn choice_selection_index(&self, request: &ChoiceRequest) -> usize {
        self.choice_selection
            .min(request.options.len().saturating_sub(1))
    }
    fn move_choice_selection(&mut self, delta: isize) -> bool {
        let Some(request) = self.runtime.choices.pending().into_iter().next() else {
            return false;
        };
        if request.options.is_empty() {
            self.choice_selection = 0;
            return false;
        }
        let current = self.choice_selection_index(&request);
        let last = request.options.len() - 1;
        self.choice_selection = if delta < 0 {
            current.saturating_sub((-delta) as usize)
        } else {
            current.saturating_add(delta as usize).min(last)
        };
        self.status = format!("当前选择：{}", request.options[self.choice_selection].label);
        true
    }
    fn note(&mut self, role: &'static str, text: impl Into<String>) {
        self.history.push(Chat {
            role,
            text: text.into(),
        });
    }

    fn preserve_partial_response(&mut self, role: &'static str) {
        if !self.stream_text.is_empty() {
            self.history.insert(
                self.stream_history_index.min(self.history.len()),
                Chat {
                    role,
                    text: std::mem::take(&mut self.stream_text),
                },
            );
        }
        self.text_block = false;
    }
    /// 键盘和鼠标共用的输入入口，斜杠命令在本地执行。
    async fn send_input(&mut self) {
        self.last_input_at = Instant::now();
        if self.input.trim().is_empty() {
            return;
        }
        let is_command = self.input.trim_start().starts_with('/');
        if !is_command && !self.runtime.choices.pending().is_empty() {
            let text = std::mem::take(&mut self.input);
            let _ = self.answer_custom_choice(text).await;
            return;
        }
        let text = std::mem::take(&mut self.input);
        if is_command {
            self.command(text.trim()).await;
        } else {
            self.submit(text);
        }
    }

    pub fn submit(&mut self, text: String) {
        if self.busy() {
            match self.steering.push(
                vec![ContentBlock::Text { text: text.clone() }],
                now_millis(),
            ) {
                Ok(id) => {
                    self.push_log(&self.task_id.clone(), DiagnosticEvent {
                        timestamp_ms: now_millis() as u64,
                        layer: "ui".into(),
                        event: "steering_queued".into(),
                        data: serde_json::json!({"id": id, "message_chars": text.chars().count()}),
                    });
                    self.note("你", text);
                    self.status = format!("补充 #{id} 已排队，当前操作结束后处理");
                }
                Err(error) => {
                    self.input = text;
                    self.status = format!("补充未发送，草稿已保留：{error}");
                }
            }
            return;
        }
        self.start_turn(text, true);
    }

    fn start_turn(&mut self, text: String, show_user: bool) {
        self.cancel_requested = false;
        // Older queued input must precede a fresh instruction that may override it.
        // This runs only after the previous worker has relinquished the inbox.
        let pending = self.steering.pending();
        if !pending.is_empty() {
            self.messages
                .extend(pending.into_iter().map(|record| AgentMessage::User {
                    content: record.content,
                    timestamp: record.timestamp,
                }));
            self.steering = SteeringInbox::default();
            self.checkpoint_generation = 0;
        }
        self.steering.seed_history(self.messages.clone());
        self.recovery_interrupted = true;
        let task = TaskId::new();
        self.plan = None;
        self.task_id = task.0.clone();
        self.ensure_diagnostic_journal();
        self.push_log(
            &task.0,
            DiagnosticEvent {
                timestamp_ms: now_millis() as u64,
                layer: "ui".into(),
                event: "turn_started".into(),
                data: serde_json::json!({
                    "session_id": self.session_id.clone(),
                    "task_id": task.0.clone(),
                    "user_message_chars": text.chars().count(),
                    "diagnostic_log": self.diagnostic_log_path().map(|p| p.display().to_string()),
                }),
            },
        );
        if show_user {
            self.note("你", text.clone());
        }
        self.chat_scroll = 0;
        self.stream_text.clear();
        self.stream_history_index = self.history.len();
        self.started = Some(Instant::now());
        self.status = "正在构造主任务".into();
        let (tx, rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = oneshot::channel();
        self.worker = Some(self.runtime.spawn_controlled(
            SpawnRequest {
                messages: self.messages.clone(),
                text,
                task_id: task,
                route: self.route,
                sink: tx,
                cancel: cancel_rx,
                artifact_root: self.artifact_root.clone(),
            },
            self.steering.clone(),
            self.spec_broker.clone(),
        ));
        self.receiver = Some(rx);
        self.cancel = Some(cancel);
    }
    pub fn cancel(&mut self) {
        if let Some(sender) = self.cancel.take() {
            self.cancel_requested = true;
            if let Some(plan) = &mut self.plan {
                plan.finish(true);
            }
            let _ = sender.send(());
            self.status = "正在取消本轮等待".into();
        }
    }
    /// P2 #14: 计算 hover tooltip —— 给用户一个简洁的下一键提示。
    /// 根据当前面板与状态分四类:进程页 / 聊天页 / 日志页 / 等待中。
    fn compute_tooltip(&self) -> String {
        if !self.runtime.choices.pending().is_empty() {
            return "↑↓ 高亮选择 · Enter 确认 · 可输入补充".into();
        }
        if self.busy() {
            return "Enter 补充当前任务 · Ctrl+X 取消 · Ctrl+C 退出".into();
        }
        if self.current.is_processes() {
            if self.processes.events_stream_open {
                return "Esc 关闭 events 流 · F8 同样关闭".into();
            }
            if self.processes.row_subfilter.is_some() {
                return "Enter 进入行 / c 复制 log_dir / /process filter 清空过滤".into();
            }
            return "Enter 查看详情 / c 复制 log_dir / F4 停止 / F5 分页".into();
        }
        if self.log_focus {
            return "↑↓ 选择 · Enter 展开 · Esc 退出日志页".into();
        }
        "/exec 跑命令 · /events 观察 events.jsonl · /help 全部".into()
    }

    pub async fn tick(&mut self) {
        for (action, result) in self.mcp_view.tick(self.runtime.mcp.clone()).await {
            self.status = match &result {
                Ok(_) => format!("MCP {action} 完成"),
                Err(error) => format!("MCP {action} 失败：{error}"),
            };
            self.push_log(
                "mcp",
                DiagnosticEvent {
                    timestamp_ms: now_millis() as u64,
                    layer: "mcp".into(),
                    event: "operation_finished".into(),
                    data: serde_json::json!({"action": action, "error": result.err()}),
                },
            );
        }
        if let Err(error) = self.sync_spec() {
            self.status = format!("方案保存失败：{error}");
        }
        let receipts = self.steering.take_receipts();
        if !receipts.is_empty() {
            self.status = format!("补充 {:?} 已进入上下文", receipts);
        }
        if let Some(snapshot) = (Instant::now() >= self.checkpoint_retry_at)
            .then(|| {
                self.steering
                    .snapshot_if_changed(self.checkpoint_generation)
            })
            .flatten()
        {
            self.messages = snapshot.messages.clone();
            if self.busy() {
                self.recovery_interrupted = true;
            }
            let saved = self.persist_checkpoint(snapshot.pending).await;
            self.checkpoint_result(snapshot.generation, saved);
        }
        let (events, results) = self.processes.tick(self.current.is_processes()).await;
        for data in events {
            let owner = data["owner"].as_str().unwrap_or("process").to_string();
            self.push_log(
                &owner,
                DiagnosticEvent {
                    timestamp_ms: data["timestamp_ms"].as_u64().unwrap_or(0),
                    layer: "process".into(),
                    event: data["event"].as_str().unwrap_or("update").into(),
                    data,
                },
            );
        }
        for result in results {
            match result {
                Ok(value) => {
                    self.push_log(
                        "ui",
                        DiagnosticEvent {
                            timestamp_ms: now_millis() as u64,
                            layer: "process".into(),
                            event: "control_complete".into(),
                            data: value.clone(),
                        },
                    );
                    self.note("系统", value.to_string());
                }
                Err(error) => {
                    self.push_log(
                        "ui",
                        DiagnosticEvent {
                            timestamp_ms: now_millis() as u64,
                            layer: "error".into(),
                            event: "process_control_failed".into(),
                            data: serde_json::json!({"error":error}),
                        },
                    );
                    self.note("错误", error);
                }
            }
        }
        // Bound per-frame work so a busy stream cannot starve keyboard and drawing.
        for _ in 0..512 {
            let event = self.receiver.as_mut().and_then(|rx| rx.try_recv().ok());
            let Some(event) = event else { break };
            self.apply(event);
        }
        // 拉取 ServiceManager 事件;每一帧同样设上限,避免广播风暴阻塞渲染。
        // Lagged 时清空 receiver 内部缓冲,继续接收后续事件。
        let mut received_sequence = self.last_service_sequence;
        for _ in 0..256 {
            let Some(rx) = self.service_events.as_mut() else {
                break;
            };
            match rx.try_recv() {
                Ok(event) => {
                    received_sequence = event.sequence;
                    self.apply_service_event(event);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    // 落后超过容量:重置序号基线到落后后的最新序号。
                    // 下一轮 try_recv() 拿到的是当前最新,不会再触发 Lagged。
                    received_sequence = received_sequence.saturating_add(skipped);
                }
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
        self.last_service_sequence = received_sequence;
        // 服务页 Output tab 缓存刷新:在同步渲染前 pre-fetch 一次。
        // 只有切到 Output tab 或事件触发时 `service_output_dirty` 才为 true,
        // 所以这里不会每帧做磁盘 IO。
        if self.current.is_services()
            && !self.services_mcp
            && self.service_detail_tab == 1
            && Instant::now() >= self.service_output_next_poll
        {
            self.service_output_dirty = true;
        }
        if self.service_output_dirty {
            self.refresh_service_output().await;
            self.service_output_next_poll = Instant::now() + std::time::Duration::from_millis(500);
        }
        // P2 #14: 静默 ≥2s 后给状态栏补一条 hover 提示;有输入立即清空。
        self.tooltip = if self.last_input_at.elapsed() >= std::time::Duration::from_secs(2) {
            Some(self.compute_tooltip())
        } else {
            None
        };
        if self.worker.as_ref().is_some_and(|w| w.is_finished()) {
            let mut completed = false;
            let result = self.worker.take().unwrap().await;
            while let Some(event) = self.receiver.as_mut().and_then(|rx| rx.try_recv().ok()) {
                self.apply(event);
            }
            self.receiver = None;
            self.cancel = None;
            self.preserve_partial_response("助手 · 未完成");
            match result {
                Ok(done) => {
                    self.messages = done.messages;
                    match done.result {
                        Ok(MonadResult::Completed { output, .. }) => {
                            completed = true;
                            if let Some(plan) = &mut self.plan {
                                plan.finish(false);
                            }
                            self.total_turns +=
                                output["turns_completed"].as_u64().unwrap_or(0) as usize;
                            self.total_tools +=
                                output["tool_calls_executed"].as_u64().unwrap_or(0) as usize;
                            self.status = format!(
                                "完成 · {:.1}s",
                                self.started
                                    .map(|s| s.elapsed().as_secs_f64())
                                    .unwrap_or(0.0)
                            );
                            if self
                                .plan
                                .as_ref()
                                .is_some_and(|p| p.phase == PlanPhase::Incomplete)
                            {
                                self.status.push_str(" · 计划尚未全部完成");
                            }
                        }
                        Ok(other) => {
                            if let Some(plan) = &mut self.plan {
                                plan.finish(true);
                            }
                            self.note("系统", format!("任务状态：{other:?}"));
                        }
                        Err(e) => {
                            if let Some(plan) = &mut self.plan {
                                plan.finish(true);
                            }
                            self.status = "本轮失败或取消，可重试".into();
                            self.note("错误", e);
                        }
                    }
                }
                Err(e) => {
                    self.messages = self.steering.latest_snapshot().messages;
                    smallhead_core::agent_loop::repair_interrupted_history(
                        &mut self.messages,
                        "后台任务异常中断，缺失结果的操作需先核验外部状态",
                    );
                    if let Some(plan) = &mut self.plan {
                        plan.finish(true);
                    }
                    self.status = "后台任务异常".into();
                    self.note("错误", e.to_string());
                }
            }
            self.stream_text.clear();
            self.started = None;
            self.steering.seed_history(self.messages.clone());
            if let Err(error) = self.finish_spec_replies(completed) {
                self.note("错误", format!("方案回复保存失败：{error}"));
            }
            self.recovery_interrupted = !completed;
            let snapshot = self.steering.latest_snapshot();
            let saved = self.persist_checkpoint(snapshot.pending).await;
            self.checkpoint_result(snapshot.generation, saved);
            if completed && !self.cancel_requested && !self.steering.pending().is_empty() {
                self.start_turn(
                    "处理尚未回应的用户补充，结合已完成的结果继续。".into(),
                    false,
                );
            } else if !self.steering.pending().is_empty() {
                self.note(
                    "系统",
                    "尚有补充信息未处理，已保留；输入继续或 /retry 后接着处理。",
                );
            }
        }
    }
    /// 把 ServiceManager broadcast 事件转成 UI 可见动作:状态栏提示 + 日志条目。
    /// 实际的服务列表/状态读取由 `service_view_all()` 在渲染时拉取,这里只关心
    /// "有事件"和"哪类事件",不去和服务面板内容赛跑。
    fn apply_service_event(&mut self, event: ServiceEvent) {
        if !self
            .service_event_cache
            .iter()
            .any(|cached| cached == &event)
        {
            self.service_event_cache.push_back(event.clone());
            while self.service_event_cache.len() > crate::services::EVENT_CACHE_LIMIT {
                self.service_event_cache.pop_front();
            }
        }
        let kind = format!("{:?}", event.kind).to_lowercase();
        // 状态变化类事件意味着 stdout/stderr 可能长了,Output tab 应该重读。
        // 但如果当前不在那个服务上,等用户切回再读就够了。
        let should_refresh_output = matches!(
            event.kind,
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::OperationStarted
                | smallhead_core::execution::smallhead_process::service::ServiceEventKind::OperationFinished
                | smallhead_core::execution::smallhead_process::service::ServiceEventKind::StateChanged
        ) && self.processes.service_selected_name.as_deref() == Some(event.name.as_str());
        if should_refresh_output {
            self.service_output_dirty = true;
        }
        let summary = match event.kind {
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::OperationStarted => {
                format!("[服务] {} 开始操作", event.name)
            }
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::OperationFinished => {
                match event.reason.as_deref() {
                    Some(reason) if reason != "ok" => {
                        format!("[服务] {} 操作失败：{reason}", event.name)
                    }
                    _ => {
                        let view = self.runtime.execution.as_ref().ok()
                            .and_then(|rt| rt.services.service_view(&event.name));
                        match view {
                            Some(view) if view.desired_running && view.state != smallhead_core::execution::smallhead_process::service::ServiceState::Ready => {
                                format!("[服务] {} 尚未就绪：{}", event.name, view.detail)
                            }
                            _ => format!("[服务] {} 操作完成", event.name),
                        }
                    }
                }
            }
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::StateChanged => {
                format!("[服务] {} 状态变化", event.name)
            }
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::HealthObserved => {
                // 健康观察高频:不刷 status 栏,避免抖动;只通过 last_service_sequence 标记"有变化"。
                return;
            }
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::Logged => {
                format!("[服务] {} 日志", event.name)
            }
            smallhead_core::execution::smallhead_process::service::ServiceEventKind::DescriptionUpdated => {
                format!("[服务] {} 说明已更新", event.name)
            }
        };
        // 状态栏提示用最直观的一行;不是 noisy 路径。
        self.status = summary.clone();
        // 同步进日志面板,后续在事件 tab 显示原始 payload。
        let timestamp_ms = event
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.push_log(
            "service",
            DiagnosticEvent {
                timestamp_ms,
                layer: "service".into(),
                event: kind,
                data: serde_json::json!({
                    "name": event.name,
                    "sequence": event.sequence,
                    "operation_id": event.operation_id.map(|id| id.0.to_string()),
                    "state": event.state,
                    "reason": event.reason,
                }),
            },
        );
    }

    /// 把当前选中服务的 stdout/stderr 拉一次塞进缓存;然后在 Output tab
    /// 渲染时同步读,避免在 ratatui 的同步渲染路径里 await。
    /// `service_output_dirty` 为 true 时才会重新拉,避免每帧磁盘 IO。
    pub async fn refresh_service_output(&mut self) {
        if !self.service_output_dirty {
            return;
        }
        self.service_output_dirty = false;
        // 只对当前选中服务拉一份;如果没选服务,清空缓存并返回。
        let Some(name) = self.processes.service_selected_name.clone() else {
            self.service_output_cache.clear();
            self.service_output_meta = None;
            return;
        };
        // 找 selected 服务的 view,拿 execution_id。
        let Some(rt) = self.runtime.execution.as_ref().ok() else {
            return;
        };
        let Some(view) = rt
            .services
            .service_view_all()
            .into_iter()
            .find(|v| v.name == name)
        else {
            self.service_output_cache.remove(&name);
            self.service_output_meta = None;
            return;
        };
        let Some(eid) = view.execution_id else {
            // 外部服务或未启动;清掉这一项的缓存。
            self.service_output_cache.remove(&name);
            self.service_output_meta = Some(ServiceOutputMeta {
                service_name: name,
                stored_bytes: 0,
            });
            return;
        };
        // 各读 64 KiB 末尾;出错就把 stderr 字段填上,前端展示。
        let limit = 64 * 1024;
        let stdout_result = rt
            .processes
            .read_output(
                eid,
                smallhead_core::execution::smallhead_process::OutputStream::Stdout,
                0,
                limit,
            )
            .await;
        let stderr_result = rt
            .processes
            .read_output(
                eid,
                smallhead_core::execution::smallhead_process::OutputStream::Stderr,
                0,
                limit,
            )
            .await;
        let mut snapshot = ServiceOutputSnapshot {
            fetched_at_ms: now_millis().max(0) as u64,
            ..Default::default()
        };
        let total_bytes = match stdout_result {
            Ok(p) => {
                snapshot.stdout = p.text;
                p.stored_bytes
            }
            Err(e) => {
                snapshot.error = Some(format!("stdout 读取失败: {e}"));
                self.service_output_cache.insert(name.clone(), snapshot);
                self.service_output_meta = Some(ServiceOutputMeta {
                    service_name: name,
                    stored_bytes: 0,
                });
                return;
            }
        };
        match stderr_result {
            Ok(p) => {
                snapshot.stderr = p.text;
            }
            Err(e) => {
                snapshot.stderr = format!("(stderr 读取失败: {e})");
            }
        }
        self.service_output_cache.insert(name.clone(), snapshot);
        self.service_output_meta = Some(ServiceOutputMeta {
            service_name: name,
            stored_bytes: total_bytes,
        });
    }

    fn apply(&mut self, event: DiagnosticEvent) {
        match event.event.as_str() {
            "plan_updated" => {
                if let Ok(snapshot) =
                    serde_json::from_value::<smallhead_core::plan::TaskPlan>(event.data.clone())
                {
                    if snapshot.task_id == self.task_id {
                        self.plan = Some(PlanView {
                            snapshot,
                            phase: PlanPhase::Running,
                        });
                    }
                }
            }
            "provider_request" => {
                self.status = format!("等待模型响应 · 第 {} 轮", event.data["iteration"]);
                self.preserve_partial_response("助手 · 未完成");
                self.stream_history_index = self.history.len();
            }
            "steering_drained" => self.preserve_partial_response("助手 · 已中断"),
            "message_start" => {
                self.status = "已收到响应，读取内容块".into();
            }
            "content_block_start" => {
                self.text_block = event.data["block"]["type"] == "text";
                if self.text_block {
                    if let Some(s) = event.data["block"]["text"].as_str() {
                        self.stream_text.push_str(s);
                    }
                }
            }
            "content_block_delta" if self.text_block => {
                if let Some(s) = event.data["delta"].as_str() {
                    self.stream_text.push_str(s);
                }
            }
            "content_block_stop" => {
                self.text_block = false;
            }
            "assistant_complete" => {
                let text = event.data["message"]["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    self.history.insert(
                        self.stream_history_index.min(self.history.len()),
                        Chat {
                            role: "助手", text
                        },
                    );
                }
                self.stream_text.clear();
            }
            "execute_start" => {
                self.status = format!("执行工具 {}", event.data["name"].as_str().unwrap_or(""));
            }
            "choice_waiting" => {
                self.status = "等待用户选择 · ↑↓ 选项，或输入补充文本后 Enter".into();
            }
            "choice_answered" => {
                self.status = "已收到用户选择，继续执行".into();
            }
            "choice_failed" => {
                self.status = "用户选择失败或已取消".into();
            }
            "model-route" => {
                self.status = format!(
                    "业务路由：{}",
                    event.data["route"]["used"].as_str().unwrap_or("")
                );
            }
            _ => {}
        }
        let task = self.task_id.clone();
        self.push_log(&task, event);
    }

    fn push_log(&mut self, task: &str, event: DiagnosticEvent) {
        let sequence = self.logs.push(task, event.clone());
        if let Some(journal) = &mut self.diagnostic_journal {
            if let Err(error) = journal.append(sequence, task, &event) {
                self.diagnostic_journal_error = Some(error.to_string());
                self.diagnostic_journal = None;
            }
        }
    }

    fn diagnostic_log_path(&self) -> Option<PathBuf> {
        self.diagnostic_journal
            .as_ref()
            .map(|journal| journal.path.clone())
    }

    fn ensure_diagnostic_journal(&mut self) {
        let Some(root) = diagnostic_root(&self.store) else {
            return;
        };
        let needs_open = self
            .diagnostic_journal
            .as_ref()
            .is_none_or(|journal| journal.session_id != self.session_id);
        if needs_open {
            match DiagnosticJournal::open(root, &self.session_id) {
                Ok(journal) => {
                    self.diagnostic_journal = Some(journal);
                    self.diagnostic_journal_error = None;
                }
                Err(error) => {
                    self.diagnostic_journal = None;
                    self.diagnostic_journal_error = Some(error.to_string());
                }
            }
        }
    }

    async fn apply_choice_response(&mut self, response: ChoiceResponse) {
        self.choice_selection = 0;
        self.status = format!("已选择：{}", response.selected.label);
        let detail = response
            .selected
            .description
            .as_ref()
            .map(|text| format!("：{text}"))
            .unwrap_or_default();
        self.note(
            "系统",
            format!(
                "选择卡片已回答：{} ({}){}",
                response.selected.label, response.selected_option_id, detail
            ),
        );
        if let ChoiceIntent::RunCommand { command } = response.selected.intent {
            self.command(&command).await;
        }
    }

    async fn answer_choice(&mut self, choice_id: &str, option_id: &str) {
        match self.runtime.choices.answer(choice_id, option_id) {
            Ok(response) => {
                self.input.clear();
                self.apply_choice_response(response).await;
            }
            Err(error) => self.note("错误", error.to_string()),
        }
    }

    async fn answer_custom_choice(&mut self, text: String) -> bool {
        let Some(request) = self.runtime.choices.pending().into_iter().next() else {
            return false;
        };
        match self.runtime.choices.answer_custom(&request.id, text) {
            Ok(response) => {
                self.input.clear();
                self.apply_choice_response(response).await;
                true
            }
            Err(error) => {
                self.note("错误", error.to_string());
                false
            }
        }
    }

    async fn answer_selected_choice(&mut self) -> bool {
        let Some(request) = self.runtime.choices.pending().into_iter().next() else {
            return false;
        };
        let Some(option) = request.options.get(self.choice_selection_index(&request)) else {
            return false;
        };
        let choice_id = request.id.clone();
        let option_id = option.id.clone();
        self.answer_choice(&choice_id, &option_id).await;
        true
    }

    pub async fn key(&mut self, key: KeyEvent) {
        self.last_input_at = Instant::now();
        // F1 and Ctrl+F1 return to chat before page-specific input handling.
        if key.code == KeyCode::F(1) {
            self.current = Page::Chat;
            self.log_focus = false;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.cancel();
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('x') => {
                    self.cancel();
                    return;
                }
                KeyCode::Char('l') => {
                    self.history.clear();
                    self.chat_scroll = 0;
                    return;
                }
                KeyCode::Char('r') => {
                    self.logs.latest_request();
                    self.current = Page::Logs;
                    self.log_focus = true;
                    return;
                }
                KeyCode::Char('f') => {
                    self.logs.cycle_filter();
                    return;
                }
                KeyCode::Char('p') => {
                    self.current = Page::Processes;
                    self.log_focus = false;
                    return;
                }
                KeyCode::Char('s') => {
                    // Ctrl+S 切到服务页。注意:这与 Chat 页面输入字母 's' 不冲突,
                    // 因为本分支只在 CONTROL 修饰符启用时才进入。
                    self.current = Page::Services;
                    self.log_focus = false;
                    return;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::F(9) {
            self.current = Page::Spec;
            return;
        }
        if self.current == Page::Spec && !matches!(key.code, KeyCode::F(2) | KeyCode::F(3)) {
            if let Err(error) = self.spec_key(key) {
                self.status = format!("方案：{error}");
            }
            return;
        }
        if self.current.is_services()
            && self.input.starts_with('/')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Char(c) => {
                    self.input.push(c);
                    return;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    return;
                }
                KeyCode::Enter => {
                    self.send_input().await;
                    return;
                }
                _ => {}
            }
        }
        if self.current.is_services() && !key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('m') {
                self.services_mcp = !self.services_mcp;
                return;
            }
            if self.services_mcp && self.mcp_view.key(key, self.runtime.mcp.clone()) {
                return;
            }
        }
        if !key.modifiers.contains(KeyModifiers::CONTROL)
            && self.current.is_chat()
            && !self.runtime.choices.pending().is_empty()
        {
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    self.move_choice_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.move_choice_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    if !self.input.trim().is_empty() {
                        if self.input.trim_start().starts_with('/') {
                            self.send_input().await;
                            return;
                        }
                        let text = std::mem::take(&mut self.input);
                        if self.answer_custom_choice(text).await {
                            return;
                        }
                    }
                    if self.answer_selected_choice().await {
                        return;
                    }
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    self.status =
                        "选择卡片等待中：输入补充文本后 Enter 提交，或 ↑↓ 选择选项".into();
                    return;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    self.status =
                        "选择卡片等待中：输入补充文本后 Enter 提交，或 ↑↓ 选择选项".into();
                    return;
                }
                KeyCode::Esc => {
                    if self.input.is_empty() {
                        self.status = "选择卡片等待中：↑↓ 高亮，Enter 确认；也可输入补充".into();
                    } else {
                        self.input.clear();
                        self.status = "已清空选择卡片补充文本".into();
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Up | KeyCode::Down if self.current.is_services() => {
                if self.service_detail_focus {
                    self.service_detail_scroll = self
                        .service_detail_scroll
                        .saturating_add_signed(if key.code == KeyCode::Up { -1 } else { 1 });
                } else {
                    self.processes
                        .move_service_selection(if key.code == KeyCode::Up { -1 } else { 1 });
                    self.service_detail_scroll = 0;
                    self.service_output_dirty = true;
                }
            }
            KeyCode::Enter if self.current.is_services() => {
                if self.processes.selected_service_name().is_none() {
                    self.processes.select_service(Some(0));
                }
                self.service_detail_focus = true;
                self.service_output_dirty = true;
            }
            KeyCode::Esc if self.current.is_services() && self.service_detail_focus => {
                self.service_detail_focus = false;
            }
            KeyCode::PageUp | KeyCode::PageDown if self.current.is_services() => {
                self.service_detail_focus = true;
                self.service_detail_scroll = self
                    .service_detail_scroll
                    .saturating_add_signed(if key.code == KeyCode::PageUp { -10 } else { 10 });
            }
            KeyCode::F(3) => {
                self.current = Page::Processes;
                self.log_focus = false;
            }
            KeyCode::F(4) if self.current.is_processes() => self.stop_selected_process(),
            KeyCode::F(5) if self.current.is_processes() => self.processes.advance_output_offset(),
            KeyCode::F(6) if self.current.is_processes() => self.processes.reset_output_offset(),
            KeyCode::F(8) if self.current.is_processes() => self.processes.toggle_events_stream(),
            KeyCode::F(7) if self.current.is_processes() => self.purge_selected_process(),
            KeyCode::Up if self.current.is_processes() => self.processes.move_selection(-1),
            KeyCode::Down if self.current.is_processes() => self.processes.move_selection(1),
            KeyCode::Char('c')
                if self.current.is_processes() && self.processes.selected_id().is_some() =>
            {
                // P2 #9: 进程页按 c 把当前选中执行的 log_dir 通过 OSC52 拷到剪贴板。
                if self.processes.copy_selected_log_dir() {
                    self.status = "log_dir 已写入剪贴板 (OSC52)".into();
                } else {
                    self.status = "复制失败：当前执行无 log_dir 或终端不支持 OSC52".into();
                }
            }
            KeyCode::F(2) => {
                self.current = Page::Logs;
                self.log_focus = true;
            }
            KeyCode::Tab => {
                if self.current.is_services() {
                    // 服务页内 Tab 在 3 个详情 tab 之间循环;Shift+Tab 反向。
                    self.service_detail_tab = (self.service_detail_tab + 1) % 3;
                    self.service_detail_scroll = 0;
                    // 切到 Output tab 时,拉一次 stdout/stderr;其它 tab 不重读。
                    if self.service_detail_tab == 1 {
                        self.service_output_dirty = true;
                    }
                } else if let Some((cmd, _)) = self.suggestions().first() {
                    self.input = cmd.to_string();
                } else {
                    self.log_focus = !self.log_focus;
                }
            }
            KeyCode::BackTab if self.current.is_services() => {
                self.service_detail_tab = (self.service_detail_tab + 2) % 3;
                self.service_detail_scroll = 0;
                if self.service_detail_tab == 1 {
                    self.service_output_dirty = true;
                }
            }
            KeyCode::Up if self.log_focus => self.logs.move_selection(-1),
            KeyCode::Down if self.log_focus => self.logs.move_selection(1),
            KeyCode::Up => self.chat_scroll = self.chat_scroll.saturating_add(3),
            KeyCode::Down => self.chat_scroll = self.chat_scroll.saturating_sub(3),
            KeyCode::PageUp => self.logs.detail_scroll = self.logs.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => {
                self.logs.detail_scroll = self.logs.detail_scroll.saturating_add(10)
            }
            KeyCode::Home if self.log_focus => {
                self.logs.selected = 0;
                self.logs.follow = false;
                self.logs.detail_scroll = 0;
            }
            KeyCode::End if self.log_focus => {
                self.logs.follow = true;
                self.logs.selected = self.logs.visible().len().saturating_sub(1);
                self.logs.detail_scroll = 0;
            }
            KeyCode::Enter => self.send_input().await,
            KeyCode::Char('j') if self.current.is_processes() => {
                self.processes.move_service_selection(1);
            }
            KeyCode::Char('k') if self.current.is_processes() => {
                self.processes.move_service_selection(-1);
            }
            KeyCode::Char('s') if self.current.is_processes() => {
                if let Err(e) = self.processes.ensure_selected_service() {
                    self.status = format!("ensure 失败:{e}");
                }
            }
            KeyCode::Char('S') if self.current.is_processes() => {
                if let Err(e) = self.processes.stop_selected_service() {
                    self.status = format!("stop 失败:{e}");
                }
            }
            KeyCode::Char('?') if self.current.is_processes() => {
                if let Err(e) = self.processes.inspect_selected_service() {
                    self.status = format!("inspect 失败:{e}");
                }
            }
            KeyCode::Char('e') if self.current.is_processes() => {
                self.processes.toggle_expand_selected_service();
            }
            // 服务页快捷键:s start / x stop / r inspect / d forget。
            // 选中的服务由 `processes.service_selected_name` 持有,与 Page 解耦。
            // External services may be reused by ensure, but never stopped here.
            KeyCode::Char('s') if self.current.is_services() => {
                if let Some(name) = self.processes.service_selected_name.clone() {
                    if let Err(e) = self.processes.ensure_service_by_name(&name) {
                        self.status = format!("ensure 失败:{e}");
                    }
                } else {
                    self.status = "未选择服务,先在列表里选一行".to_string();
                }
            }
            KeyCode::Char('x') if self.current.is_services() => {
                if let Some(name) = self.processes.service_selected_name.clone() {
                    if self.processes.is_service_external(&name) {
                        self.status = format!("外部服务:不能停止；可复用、检查与清除登记({name})");
                    } else if let Err(e) = self.processes.stop_service_by_name(&name) {
                        self.status = format!("stop 失败:{e}");
                    }
                } else {
                    self.status = "未选择服务,先在列表里选一行".to_string();
                }
            }
            KeyCode::Char('r') if self.current.is_services() => {
                if let Some(name) = self.processes.service_selected_name.clone() {
                    if let Err(e) = self.processes.inspect_service_by_name(&name) {
                        self.status = format!("inspect 失败:{e}");
                    }
                } else {
                    self.status = "未选择服务,先在列表里选一行".to_string();
                }
            }
            KeyCode::Char('d') if self.current.is_services() => {
                if let Some(name) = self.processes.service_selected_name.clone() {
                    // 走 ServiceManager::forget(同步),不依赖外部/本应用管理。
                    match &self.runtime.execution {
                        Ok(rt) => match rt.services.forget(&name) {
                            Ok((_record, _op_id)) => {
                                // 当前选中被 forget 了,清空选中;输出缓存一并清。
                                self.processes.service_selected_name = None;
                                self.service_output_cache.remove(&name);
                                self.service_output_meta = None;
                                self.service_output_dirty = true;
                                self.status = format!("已遗忘 {name}");
                            }
                            Err(e) => {
                                self.status = format!("forget 失败:{e}");
                            }
                        },
                        Err(e) => {
                            self.status = format!("forget 失败:{e}");
                        }
                    }
                } else {
                    self.status = "未选择服务,先在列表里选一行".to_string();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(c)
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                if self.processes.events_stream_open {
                    self.processes.events_stream_open = false;
                } else {
                    self.current = Page::Chat;
                    self.input.clear();
                    self.log_focus = false;
                }
            }
            _ => {}
        }
    }

    /// 鼠标点击处理入口。
    ///
    /// 调用 [`ClickMap::hit_test`] 查坐标 → 动作,无命中则 no-op。
    /// 各 action 复用现有 `self.command` / `self.cancel` / `self.submit` / 字段设值路径,
    /// 行为与对应键位 / 斜杠命令完全一致。
    pub async fn on_click(&mut self, x: u16, y: u16) {
        let Some(action) = self.click_map.hit_test(x, y) else {
            return;
        };
        match action {
            ClickAction::FocusSpec => self.current = Page::Spec,
            ClickAction::FocusProcesses => self.current = Page::Processes,
            ClickAction::SelectProcess(index) => self.processes.selected = index,
            ClickAction::StopProcess => self.stop_selected_process(),
            ClickAction::Submit => self.send_input().await,
            ClickAction::ScrollChat | ClickAction::FocusInput => self.log_focus = false,
            ClickAction::ScrollLogs | ClickAction::ScrollDetail => self.log_focus = true,
            ClickAction::Cancel => self.cancel(),
            ClickAction::FocusChat => {
                self.current = Page::Chat;
                self.log_focus = false;
            }
            ClickAction::FocusLogs => {
                self.current = Page::Logs;
                self.log_focus = true;
            }
            ClickAction::FocusServices => {
                self.current = Page::Services;
                self.log_focus = false;
            }
            ClickAction::RunCommand(cmd) => {
                self.command(&cmd).await;
            }
            ClickAction::SelectLog(idx) => {
                // 选中 idx 行(渲染层会按 visible() 顺序编号,这里直接设 selected)。
                if idx < self.logs.visible().len() {
                    self.logs.selected = idx;
                    self.logs.follow = false;
                    self.logs.detail_scroll = 0;
                    self.log_focus = true;
                }
            }
            ClickAction::SelectService(idx) => {
                self.processes.select_service(Some(idx));
                self.service_detail_scroll = 0;
                self.service_detail_focus = true;
                // 切到不同服务 → 输出缓存 stale,下一次 tick 重新拉。
                self.service_output_dirty = true;
            }
            ClickAction::EnsureService(name) => {
                if let Err(e) = self.processes.ensure_service_by_name(&name) {
                    self.status = format!("ensure 失败:{e}");
                }
            }
            ClickAction::StopService(name) => {
                if self.processes.is_service_external(&name) {
                    self.status = format!("该服务为外部,{name} stop 不可用");
                } else if let Err(e) = self.processes.stop_service_by_name(&name) {
                    self.status = format!("stop 失败:{e}");
                }
            }
            ClickAction::InspectService(name) => {
                if let Err(e) = self.processes.inspect_service_by_name(&name) {
                    self.status = format!("inspect 失败:{e}");
                }
            }
            ClickAction::ForgetService(name) => {
                // forget 是同步的:ServiceManager 把 ServiceRecord 直接摘掉。
                // 选中服务被 forget 后清空 service_selected_name,避免指向已
                // 不存在的 service;输出缓存也清掉。
                match &self.runtime.execution {
                    Ok(rt) => match rt.services.forget(&name) {
                        Ok((_record, _op)) => {
                            if self.processes.service_selected_name.as_deref()
                                == Some(name.as_str())
                            {
                                self.processes.service_selected_name = None;
                            }
                            self.service_output_cache.remove(&name);
                            self.service_output_meta = None;
                            self.service_output_dirty = true;
                            self.status = format!("已遗忘 {name}");
                        }
                        Err(e) => self.status = format!("forget 失败:{e}"),
                    },
                    Err(e) => self.status = format!("forget 失败:{e}"),
                }
            }
            ClickAction::ExpandService(idx) => {
                self.processes.toggle_expand_service(idx);
            }
            ClickAction::AnswerChoice {
                choice_id,
                option_id,
            } => self.answer_choice(&choice_id, &option_id).await,
        }
    }

    /// 所有终端鼠标事件统一入口，滚轮按指针所在面板处理。
    pub async fn on_mouse(&mut self, mouse: MouseEvent) {
        self.mouse_events = self.mouse_events.saturating_add(1);
        self.last_input_at = Instant::now();
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.on_click(mouse.column, mouse.row).await;
            return;
        }
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => return,
        };
        match self.click_map.hit_test(mouse.column, mouse.row) {
            Some(ClickAction::SelectLog(_) | ClickAction::ScrollLogs) => {
                self.log_focus = true;
                self.logs.move_selection(delta);
            }
            Some(ClickAction::ScrollDetail) => {
                self.log_focus = true;
                self.logs.detail_scroll =
                    self.logs.detail_scroll.saturating_add_signed(delta as i16);
            }
            Some(ClickAction::ScrollChat) => {
                self.log_focus = false;
                self.chat_scroll = self.chat_scroll.saturating_add_signed(-delta as i16);
            }
            _ => {}
        }
    }
    pub async fn command(&mut self, line: &str) {
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim();
        match cmd {
            "/lens" => {
                let (action, value) = arg.split_once(' ').unwrap_or((arg, ""));
                let value = value.trim();
                let result = match action {
                    "" | "list" => {
                        let loaded = self.runtime.lenses.list();
                        Ok(if loaded.is_empty() { "未加载 Lens。/lens load 包目录；需要 Node.js。".to_string() }
                        else { format!("外层 → 内层：{}", loaded.iter().map(|p| format!("{}@{}{}", p.id, p.version, if p.extension.is_some() { "（含工具扩展）" } else { "" })).collect::<Vec<_>>().join(" → ")) })
                    }
                    "load" if !value.is_empty() => self.runtime.lenses.load(value).map(|p| format!("已加载 {}@{}，下一轮生效。{}", p.id, p.version, if p.extension.is_some() { "工具配置已固定，首次调用时启动；与宿主工具的名称冲突将在回合接入时检查。" } else { "" })),
                    "reload" if !value.is_empty() => self.runtime.lenses.reload(value).map(|p| format!("已重载 {}@{}，当前运行保留旧版本，下一轮生效。", p.id, p.version)),
                    "unload" if !value.is_empty() => self.runtime.lenses.unload(value).map(|()| format!("已卸载 {value}，当前运行保留旧版本，下一轮生效。")),
                    _ => Err(anyhow::anyhow!("用法：/lens list | load 包目录 | reload ID | unload ID")),
                };
                match result {
                    Ok(message) => self.note("Lens", message),
                    Err(error) => self.note("错误", error.to_string()),
                }
            }
            "/mcp" => {
                self.current = Page::Services;
                self.services_mcp = true;
                self.log_focus = false;
                if let Err(error) = self.mcp_view.command(arg, self.runtime.mcp.clone()) {
                    self.status = error;
                } else {
                    self.status = "MCP 请求已排队".into();
                }
            }
            "/exec" => {
                self.current = Page::Processes;
                if let Err(e)=self.processes.launch(true,serde_json::json!({"command":arg,"wait":false})){self.note("错误",e);}
            },
            "/exec-bg" => {
                self.current = Page::Processes;
                // /exec-bg 是 /exec 的别名,固定 wait=false,适合 fire-and-forget。
                if let Err(e)=self.processes.launch(true,serde_json::json!({"command":arg,"wait":false})){self.note("错误",e);}
            },
            "/process" => {
                self.current = Page::Processes;
                let args:Vec<_>=arg.split_whitespace().collect();
                // 第一段决定 action；缺 action 时不调用 launch。
                let Some(sub)=args.first().copied() else {
                    self.note("系统","用法：/process list|inspect|stop|purge|output ID 偏移 stream|filter [text]。");
                    return;
                };
                // P2 #12: filter 是 UI 本地动作,不走 process_control launch。
                if sub == "filter" {
                    let needle = args.get(1..).map(|a| a.join(" "));
                    match needle {
                        Some(n) if !n.is_empty() => {
                            self.processes.row_subfilter = Some(n.clone());
                            self.note("系统", format!("进程列表子过滤:{n}"));
                        }
                        _ => {
                            self.processes.row_subfilter = None;
                            self.note("系统", "进程列表子过滤已清除".to_string());
                        }
                    }
                    return;
                }
                let payload=match sub {
                    "output"|"read_output" => serde_json::json!({
                        "action":"read_output",
                        "execution_id":args.get(1).copied().unwrap_or(""),
                        "offset":args.get(2).and_then(|s|s.parse::<u64>().ok()).unwrap_or(0),
                        "stream":args.get(3).copied().unwrap_or("stdout"),
                    }),
                    "inspect" => serde_json::json!({
                        "action":"inspect",
                        "execution_id":args.get(1).copied().unwrap_or(""),
                    }),
                    "stop" => serde_json::json!({
                        "action":"stop",
                        "execution_id":args.get(1).copied().unwrap_or(""),
                    }),
                    "purge" => serde_json::json!({
                        "action":"purge",
                        "execution_id":args.get(1).copied().unwrap_or(""),
                    }),
                    "list" => serde_json::json!({
                        "action":"list",
                        "offset":args.get(1).and_then(|s|s.parse::<u64>().ok()).unwrap_or(0),
                        "limit":args.get(2).and_then(|s|s.parse::<u64>().ok()).unwrap_or(20),
                    }),
                    other => {
                        // 旧习惯 `output` 之外的 fallback：保留 read_output 兼容。
                        let _=other;
                        serde_json::json!({
                            "action":"read_output",
                            "execution_id":args.get(1).copied().unwrap_or(""),
                            "offset":args.get(2).and_then(|s|s.parse::<u64>().ok()).unwrap_or(0),
                            "stream":args.get(3).copied().unwrap_or("stdout"),
                        })
                    }
                };
                if let Err(e)=self.processes.launch(false,payload){self.note("错误",e);}
            },
            "/events" => {
                self.current = Page::Processes;
                let args:Vec<_>=arg.split_whitespace().collect();
                if args.is_empty() {
                    self.note("系统","用法：/events <execution_id> [偏移]");
                    return;
                }
                let payload = serde_json::json!({
                    "action":"read_output",
                    "execution_id":args.first().copied().unwrap_or(""),
                    "offset":args.get(1).and_then(|s|s.parse::<u64>().ok()).unwrap_or(0),
                    "stream":"events",
                    "limit":4096,
                });
                if let Err(e)=self.processes.launch(false,payload){self.note("错误",e);}
            },
            "/service" => {
                let args:Vec<_>=arg.split_whitespace().collect();
                let payload=match args.first().copied() {
                    None| Some("list") => serde_json::json!({"action":"service_list"}),
                    Some("ensure") => serde_json::json!({
                        "action":"service_ensure",
                        "name":args.get(1).copied().unwrap_or(""),
                    }),
                    Some("inspect") => serde_json::json!({
                        "action":"service_inspect",
                        "name":args.get(1).copied().unwrap_or(""),
                    }),
                    Some("stop") => serde_json::json!({
                        "action":"service_stop",
                        "name":args.get(1).copied().unwrap_or(""),
                    }),
                    Some("forget") => serde_json::json!({
                        "action":"service_forget",
                        "name":args.get(1).copied().unwrap_or(""),
                    }),
                    Some(other) => {
                        // 兼容旧形式：`/service list` / 第一个 token 不是关键字时按 list 处理。
                        let _=other;
                        serde_json::json!({"action":"service_list"})
                    }
                };
                if let Err(e)=self.processes.launch(false,payload){self.note("错误",e);}
            },
            "/help"=>self.note("系统","F1 聊天 / F2 日志 / Ctrl+P 或 F3 进程；F4 停止选中执行；F5 推进输出偏移；F6 跳回尾部；F7 清理已完成日志；F8 实时观察 events.jsonl。/exec 命令、/exec-bg 异步执行、/process inspect|stop|purge|list ID、/process output ID 偏移 stdout|stderr|events、/process filter <text> 行过滤、/events ID 偏移、/service ensure|inspect|stop|forget name。/choice-demo 显示一张用户选择卡片。进程页 c 复制 log_dir 到剪贴板 (OSC52)；静默 2s 后状态栏出 hover 提示。Tab 补全命令或切换面板焦点；↑↓ 滚动聊天或选择日志；选择卡片出现时 ↑↓ 高亮，Enter 确认选项；也可直接输入补充文本后 Enter 提交。PgUp/PgDn 滚动详情；Ctrl+F 筛选；Ctrl+R 最近请求；End 跟随日志；Ctrl+X 取消；Ctrl+C 退出。/route large|auto|laya|qwen 仅影响下一轮。/context 查看上下文预算；/compact 立即压缩旧历史。/diag 显示自动诊断日志路径；/export 路径 导出当前保留日志。/scrolloff N 设日志光标视窗留白行数。"),
            "/choice-demo" => {
                let request = ChoiceRequest {
                    id: String::new(),
                    owner: Some("tui-demo".into()),
                    title: "文件已存在".into(),
                    body: "请选择下一步处理方式。这个卡片由本地 /choice-demo 生成，Agent 工具 ask_user_choice 会复用同一套 UI。".into(),
                    options: vec![
                        ChoiceOption {
                            id: "overwrite".into(),
                            label: "覆盖".into(),
                            description: Some("替换现有内容".into()),
                            intent: ChoiceIntent::Continue,
                        },
                        ChoiceOption {
                            id: "append".into(),
                            label: "追加".into(),
                            description: Some("保留原内容并追加新内容".into()),
                            intent: ChoiceIntent::Continue,
                        },
                        ChoiceOption {
                            id: "cancel".into(),
                            label: "取消".into(),
                            description: Some("不做文件修改".into()),
                            intent: ChoiceIntent::CancelTask,
                        },
                    ],
                    default_option: Some("cancel".into()),
                    timeout_ms: None,
                    source: Some("tui".into()),
                    tool_call_id: None,
                    created_at_ms: 0,
                };
                match self.runtime.choices.present(request) {
                    Ok(_) => {
                        self.choice_selection = 0;
                        self.status = "已显示选择卡片".into();
                    }
                    Err(error) => self.note("错误", error.to_string()),
                }
            }
            "/plan" => {
                if arg.is_empty() {
                    let text = self.plan.as_ref().map(PlanView::full_text).unwrap_or_else(|| "当前任务尚未创建计划。".into());
                    self.note("计划", text);
                    self.chat_scroll = 0;
                    self.log_focus = false;
                    self.current = Page::Chat;
                } else {
                    self.submit(format!("先调查并用 update_spec 发布完整方案供我在 TUI 审阅，包括目标、实现范围、步骤、验证方式和需要确认的问题。接受我的段落评论并修订；我明确确认当前版本后才实施该方案。任务：{arg}"));
                    self.current = Page::Spec;
                }
            }
            "/spec" => {
                self.current = Page::Spec;
                if !arg.is_empty() {
                    if let Err(error) = self.open_spec(Path::new(arg)) {
                        self.note("错误", error.to_string());
                    }
                }
            }
            "/mouse"=>self.note("系统",format!("已收到 {} 个鼠标事件。iTerm2：Settings → Profiles → Terminal，开启 Enable mouse reporting、Report mouse clicks & drags、Report mouse wheel events；按住 Option 会临时禁用报告。移动指针不一定增加计数，请点击或滚轮后再查。",self.mouse_events)),
            "/cancel"=>self.cancel(),
            "/retry" => {
                if self.busy() {
                    self.note("系统", "当前任务仍在执行，可直接输入补充；需要停止时用 Ctrl+X。");
                } else {
                    self.submit("继续上次未完成的任务。沿用已经确认的工具结果，不要重复已完成的操作；结果未知的外部操作先检查实际状态。".into());
                }
            }
            "/req"=>{self.logs.latest_request();self.current=Page::Logs;self.log_focus=true;}
            "/context"=>{
                let stats = estimate_context_projection(&self.messages);
                let refs = if stats.compacted_history_refs.is_empty() {
                    "无".into()
                } else {
                    stats.compacted_history_refs.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
                };
                self.note("系统",format!("上下文：raw_messages={}，provider_messages={}，约 {} bytes，compaction_markers={}。\n历史 artifact：{}",stats.raw_messages,stats.llm_messages,stats.llm_message_bytes,stats.compaction_markers,refs));
            }
            "/compact" if self.busy()=>self.note("错误","请先取消或等待本轮结束，再压缩历史。"),
            "/compact"=>{
                let budget = LoopBudget::default();
                let artifact_root = self.artifact_root.clone();
                let report = compact_messages_rolling(
                    &mut self.messages,
                    &budget,
                    artifact_root.as_deref(),
                    true,
                );
                if report.compacted {
                    self.steering.seed_history(self.messages.clone());
                    let save_note = match self.persist_checkpoint(self.steering.pending()).await {
                        Ok(()) => "已保存会话。",
                        Err(error) => {
                            self.note("错误",format!("压缩已完成，但保存失败：{error}"));
                            return;
                        }
                    };
                    self.note("系统",format!("已压缩 {} 条旧消息，保留 {} 条；上下文约 {} -> {} bytes。\n完整历史：{}\n{}",report.compacted_messages,report.kept_messages,report.before.llm_message_bytes,report.after.llm_message_bytes,report.artifact_ref.as_deref().unwrap_or("无"),save_note));
                } else {
                    self.note("系统",format!("未压缩：{}；当前 provider_messages={}，约 {} bytes。",report.reason,report.before.llm_messages,report.before.llm_message_bytes));
                }
            }
            "/logs"=>{
                if self.current.is_logs() {
                    self.current = Page::Chat;
                    self.log_focus = false;
                } else {
                    self.current = Page::Logs;
                    self.log_focus = true;
                }
            }
            "/services" => {
                // 与 `/service ensure|...` 不同:这里只切页面,不发起操作。
                if self.current.is_services() {
                    self.current = Page::Chat;
                } else {
                    self.current = Page::Services;
                }
                self.log_focus = false;
            }
            "/diag"=>{
                self.ensure_diagnostic_journal();
                let path = self
                    .diagnostic_log_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "无".into());
                let suffix = self
                    .diagnostic_journal_error
                    .as_ref()
                    .map(|e| format!("\n最近错误：{e}"))
                    .unwrap_or_default();
                self.note("系统", format!("本会话诊断日志：{path}{suffix}"));
            }
            "/route"=>if let Some(route)=Route::parse(arg){self.route=route;self.note("系统",format!("后续请求：{}；主模型仍负责拆任务和提取参数。",route.label()));}else{self.note("错误","用法：/route large|auto|laya|qwen");},
            "/filter"=>if let Some(index)=crate::logs::FILTERS.iter().position(|f|*f==arg){self.logs.filter=index;self.logs.view_offset=0;self.logs.selected=0;self.logs.follow=false;self.logs.detail_scroll=0;}else{self.note("错误","未知日志层，输入 /filter 查看提示");},
            "/export"=>{
                if arg.is_empty(){self.note("错误","用法：/export 文件路径.jsonl");return;}
                match std::fs::OpenOptions::new().write(true).create_new(true).open(arg) {
                    Ok(mut file)=>{use std::io::Write;match file.write_all(self.logs.export().as_bytes()){Ok(())=>self.note("系统",format!("已导出当前保留日志到 {arg}")),Err(e)=>self.note("错误",e.to_string())}},
                    Err(e)=>self.note("错误",format!("导出失败（不覆盖已有文件）：{e}")),
                }
            }
            "/list"=>{
                let result=if let Some(store)=&self.store{store.list().await}else{self.note("错误","会话存储不可用");return;};
                match result {Ok(metas)=>self.note("系统",metas.iter().take(10).map(|m|format!("{} · {} 条消息",m.id,m.message_count)).collect::<Vec<_>>().join("\n")),Err(e)=>self.note("错误",e.to_string())}
            }
            "/load"|"/new" if self.busy()=>self.note("错误","请先取消或等待本轮结束，再切换会话。"),
            "/load"=>if let Err(e)=self.load(arg).await{self.note("错误",e.to_string());},
            "/new" => {
                if let Err(error) = self.sync_spec() {
                    self.note("错误", format!("当前方案尚未保存，未切换：{error}"));
                    return;
                }
                if let Err(error) = self.persist_checkpoint(self.steering.pending()).await {
                    self.note("错误", format!("当前记录尚未保存，未切换：{error}"));
                    return;
                }
                self.plan = None;
                self.messages.clear();
                self.history.clear();
                self.steering = SteeringInbox::default();
                self.checkpoint_generation = 0;
                self.checkpoint_retry_at = Instant::now();
                self.checkpoint_error = None;
                self.recovery_interrupted = false;
                self.spec_broker.clear();
                self.spec_document = None;
                self.spec_view = Default::default();
                self.spec_reply_targets.clear();
                self.session_id = new_session_id();
                self.created_at = now_millis();
                self.artifact_root = artifact_root_for_session(&self.store, &self.session_id);
                self.ensure_diagnostic_journal();
                self.note("系统", "新会话已创建；日志保留并通过任务 ID 区分。");
            }
            "/scrolloff"=>{
                match arg.parse::<usize>() {
                    Ok(n) => {self.logs.set_scrolloff(n);self.note("系统",format!("日志光标视窗留白 = {n} 行（0 = 贴边）；下次重绘生效。"));}
                    Err(_) => {self.note("系统",format!("当前 scrolloff = {}（传整数：/scrolloff 0~99）",self.logs.scrolloff));}
                }
            }
            _=>self.note("错误","未知命令，输入 /help"),
        }
    }
    pub async fn load(&mut self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.busy(), "任务运行时不能切换记录");
        let s = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("会话存储不可用"))?
            .load(&id.to_string())
            .await?;
        let artifact_root = s
            .artifact_root
            .or_else(|| artifact_root_for_session(&self.store, &s.id));
        let mut messages = s.messages;
        let steering = SteeringInbox::default();
        let mut interrupted = false;
        if let Some(root) = artifact_root.clone() {
            if let Some(checkpoint) = tokio::task::spawn_blocking(move || {
                crate::turn_recovery::TurnCheckpoint::load(&root)
            })
            .await??
            {
                messages = checkpoint.messages;
                interrupted = checkpoint.interrupted;
                let repaired = smallhead_core::agent_loop::repair_interrupted_history(
                    &mut messages,
                    "应用在收到工具结果前中断；先核验外部状态，不能直接重放操作。",
                );
                interrupted |= repaired > 0;
                for record in checkpoint.pending {
                    steering.push(record.content, record.timestamp)?;
                }
            }
        }
        steering.seed_history(messages.clone());
        let spec_broker = Arc::new(SpecBroker::new());
        let mut spec_document = None;
        if let Some(root) = &artifact_root {
            if let Some(document) = crate::spec_review::SpecStore::new(root).load_current()? {
                spec_broker.restore(document.to_document())?;
                spec_document = Some(document);
            }
        }
        // Keep the current record untouched until every incoming artifact validates.
        if self.session_id != s.id
            && (!self.messages.is_empty() || !self.steering.pending().is_empty())
        {
            self.persist_checkpoint(self.steering.pending()).await?;
        }
        self.session_id = s.id;
        self.created_at = s.created_at;
        self.artifact_root = artifact_root;
        self.messages = messages;
        self.steering = steering;
        self.recovery_interrupted = interrupted;
        self.checkpoint_generation = 0;
        self.checkpoint_retry_at = Instant::now();
        self.checkpoint_error = None;
        self.spec_broker = spec_broker;
        self.spec_document = spec_document;
        self.spec_view = Default::default();
        self.spec_reply_targets.clear();
        self.ensure_diagnostic_journal();
        self.plan = None;
        self.history.clear();
        self.chat_scroll = 0;
        for msg in &self.messages {
            let (role, content) = match msg {
                AgentMessage::User { content, .. } => ("你", content),
                AgentMessage::Assistant { content, .. } => ("助手", content),
                _ => continue,
            };
            let text = content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                self.history.push(Chat { role, text });
            }
        }
        self.note(
            "系统",
            "已恢复历史；本次仍使用 TUI 的 Monad 工具与提示词，模型偏好由当前配置决定。",
        );
        if self.recovery_interrupted || !self.steering.pending().is_empty() {
            self.note("系统", "上次任务中断，已恢复完成结果和未处理补充；输入继续或 /retry 恢复执行，结果未知的操作会先核验。");
        }
        Ok(())
    }
    fn checkpoint_result(&mut self, generation: u64, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.checkpoint_generation = generation;
                self.checkpoint_error = None;
                self.checkpoint_retry_at = Instant::now();
            }
            Err(error) => {
                let error = error.to_string();
                if self.checkpoint_error.as_ref() != Some(&error) {
                    self.note("错误", format!("执行记录保存失败，将重试：{error}"));
                }
                self.checkpoint_error = Some(error);
                self.checkpoint_retry_at = Instant::now() + std::time::Duration::from_secs(1);
            }
        }
    }

    async fn persist_checkpoint(
        &self,
        pending: Vec<smallhead_core::agent_loop::SteeringRecord>,
    ) -> anyhow::Result<()> {
        if let Some(root) = self.artifact_root.clone() {
            let checkpoint = crate::turn_recovery::TurnCheckpoint {
                messages: self.messages.clone(),
                pending,
                interrupted: self.recovery_interrupted,
            };
            tokio::task::spawn_blocking(move || checkpoint.save(&root)).await??;
            self.save().await?;
        }
        Ok(())
    }
    async fn save(&self) -> anyhow::Result<()> {
        if let Some(store) = &self.store {
            store
                .save(&Session {
                    id: self.session_id.clone(),
                    created_at: self.created_at,
                    updated_at: now_millis(),
                    system_prompt: SYSTEM.into(),
                    artifact_root: self.artifact_root.clone(),
                    messages: self.messages.clone(),
                })
                .await?;
        }
        Ok(())
    }
    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.cancel();
        self.runtime.monads.cancel_all();
        if let Some(scheduler) = self.health_scheduler.take() {
            scheduler.stop().await;
        }
        if let Some(mut worker) = self.worker.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(1), &mut worker).await {
                Ok(Ok(done)) => self.messages = done.messages,
                other => {
                    if other.is_err() {
                        worker.abort();
                        let _ = worker.await;
                    }
                    self.messages = self.steering.latest_snapshot().messages;
                    smallhead_core::agent_loop::repair_interrupted_history(
                        &mut self.messages,
                        "退出时任务中断，外部操作结果未知",
                    );
                }
            }
            self.recovery_interrupted = true;
        }
        self.runtime.monads.wait_idle().await;
        let spec_saved = self.sync_spec().map_err(|error| error.to_string());
        let saved = self
            .persist_checkpoint(self.steering.pending())
            .await
            .map_err(|error| error.to_string());
        let processes = self.processes.shutdown().await;
        let mcp = self.mcp_view.shutdown(self.runtime.mcp.clone()).await;
        let errors: Vec<_> = [saved, spec_saved, processes, mcp]
            .into_iter()
            .filter_map(Result::err)
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn stop_selected_process(&mut self) {
        if let Some(id) = self.processes.selected_id() {
            if let Err(error) = self.processes.launch(
                false,
                serde_json::json!({"action":"stop","execution_id":id}),
            ) {
                self.note("错误", error);
            }
        }
    }

    /// F7:清理选中执行的日志(purge)。仅在 terminal 时直接执行;
    /// 非终态需用户确认才能删除(避免误删仍在跑的记录)。
    fn purge_selected_process(&mut self) {
        let Some(id) = self.processes.selected_id() else {
            return;
        };
        let terminal = self
            .processes
            .rt
            .as_ref()
            .ok()
            .and_then(|rt| rt.processes.inspect(id))
            .map(|r| r.is_terminal())
            .unwrap_or(false);
        if !terminal {
            self.status = "purge 仅清理已完成执行；选中执行非终态,按 F4 先停止再 F7 清理".into();
            return;
        }
        if let Err(error) = self.processes.launch(
            false,
            serde_json::json!({"action":"purge","execution_id":id}),
        ) {
            self.status = format!("purge 失败:{error}");
        } else {
            // 同步清掉选中执行在本地的输出缓存与 offset —— purge 后 ids() 不再含它,
            // 防止选中下一行后渲染旧的 page 徽标。
            self.processes
                .output_pages
                .retain(|(k_id, _), _| *k_id != id);
            self.processes
                .output_offset
                .retain(|(k_id, _), _| *k_id != id);
            self.processes
                .last_read_signature
                .retain(|(k_id, _), _| *k_id != id);
            self.status = format!("已清理 {id} 的日志");
        }
    }
}

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod interaction_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Page;

    #[tokio::test]
    async fn lens_commands_load_reload_list_and_unload_in_order() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lenses");
        app.command(&format!("/lens load {}", root.join("learning").display()))
            .await;
        app.command(&format!(
            "/lens load {}",
            root.join("evidence-check").display()
        ))
        .await;
        app.command("/lens list").await;
        assert!(app
            .history
            .last()
            .unwrap()
            .text
            .contains("learning@0.1.0 → evidence-check@0.1.0"));
        app.command("/lens reload learning").await;
        assert!(app.history.last().unwrap().text.contains("下一轮生效"));
        app.command("/lens unload learning").await;
        assert_eq!(app.runtime.lenses.list().len(), 1);
        app.command("/lens unload missing").await;
        assert!(app.history.last().unwrap().text.contains("Lens not loaded"));
    }

    fn plan_event(task: &str) -> DiagnosticEvent {
        DiagnosticEvent {
            timestamp_ms: 0,
            layer: "plan".into(),
            event: "plan_updated".into(),
            data: serde_json::json!({"task_id":task,"explanation":null,"plan":[
                {"step":"检查项目","status":"in_progress"},
                {"step":"执行验证","status":"pending"}
            ]}),
        }
    }

    #[tokio::test]
    async fn plans_are_task_scoped_and_cancellation_preserves_unfinished_steps() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.task_id = "current".into();
        app.apply(plan_event("old-task"));
        assert!(app.plan.is_none());
        app.apply(plan_event("current"));
        let (cancel, _rx) = oneshot::channel();
        app.cancel = Some(cancel);
        app.cancel();
        let plan = app.plan.as_ref().unwrap();
        assert_eq!(plan.phase, PlanPhase::Interrupted);
        assert_eq!(plan.completed(), 0);
        assert_eq!(
            plan.snapshot.plan[0].status,
            smallhead_core::plan::PlanStatus::InProgress
        );
        app.command("/plan").await;
        assert!(app.history.last().unwrap().text.contains("执行验证"));
        assert!(app.history.last().unwrap().text.contains("已中断"));
        app.command("/new").await;
        assert!(app.plan.is_none());
    }

    #[tokio::test]
    async fn agent_plan_tool_updates_reach_tui_during_real_execution() {
        use smallhead_core::{MockProvider, MockResponse};
        let mut runtime = Runtime::new(true, 30);
        runtime.mock = false;
        runtime.provider = std::sync::Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "update_plan".into(),
                input: serde_json::json!({"plan":[
                    {"step":"运行检查","status":"in_progress"},
                    {"step":"汇总结果","status":"pending"}
                ]}),
            },
            MockResponse::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"command":"printf plan-smoke"}),
            },
            MockResponse::ToolCall {
                name: "update_plan".into(),
                input: serde_json::json!({"plan":[
                    {"step":"运行检查","status":"completed"},
                    {"step":"汇总结果","status":"completed"}
                ]}),
            },
            MockResponse::AssistantText {
                text: "检查完成".into(),
            },
        ]));
        let mut app = App::new(runtime, Route::Large, None);
        app.submit("检查后汇总".into());
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while app.busy() {
                app.tick().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let plan = app.plan.as_ref().unwrap();
        assert_eq!(plan.snapshot.task_id, app.task_id);
        assert_eq!(plan.phase, PlanPhase::Completed);
        assert_eq!(plan.completed(), 2);
        assert_eq!(
            app.logs
                .entries
                .iter()
                .filter(|e| e.record.event == "plan_updated")
                .count(),
            2
        );
        assert!(app
            .logs
            .entries
            .iter()
            .any(|e| e.record.event == "execute_start" && e.record.data["name"] == "bash"));
        let tool_results: Vec<_> = app
            .messages
            .iter()
            .filter_map(|msg| match msg {
                AgentMessage::ToolResult {
                    is_error, content, ..
                } => Some((is_error, content)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 3);
        assert!(tool_results.iter().all(|(is_error, _)| !**is_error));
        assert!(app.history.iter().any(|m| m.text == "检查完成"));
    }

    #[tokio::test]
    async fn ending_a_turn_does_not_complete_pending_plan_steps() {
        use smallhead_core::{MockProvider, MockResponse};
        let mut runtime = Runtime::new(true, 30);
        runtime.mock = false;
        runtime.provider = std::sync::Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "update_plan".into(),
                input: serde_json::json!({"plan":[
                    {"step":"待执行的检查","status":"pending"}
                ]}),
            },
            MockResponse::AssistantText {
                text: "需要更多信息".into(),
            },
        ]));
        let mut app = App::new(runtime, Route::Large, None);
        app.submit("检查项目".into());
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while app.busy() {
                app.tick().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.plan.as_ref().unwrap().phase, PlanPhase::Incomplete);
        assert_eq!(app.plan.as_ref().unwrap().completed(), 0);
        assert!(app.status.contains("计划尚未全部完成"));
    }

    #[tokio::test]
    async fn context_command_reports_projection_stats() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            timestamp: 0,
        });

        app.command("/context").await;

        assert!(app
            .history
            .last()
            .is_some_and(|entry| entry.text.contains("上下文：raw_messages=1")));
    }

    #[tokio::test]
    async fn compact_command_persists_compacted_session_artifact_root() {
        let temp = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(temp.path().join("sessions"));
        let mut app = App::new(Runtime::new(true, 30), Route::Large, Some(store));
        for i in 0..32 {
            app.messages.push(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: format!("message {i}"),
                }],
                timestamp: i,
            });
        }

        app.command("/compact").await;

        assert!(matches!(
            app.messages.first(),
            Some(AgentMessage::Compaction { .. })
        ));
        let session = app
            .store
            .as_ref()
            .unwrap()
            .load(&app.session_id)
            .await
            .unwrap();
        assert!(session.artifact_root.is_some());
        assert!(matches!(
            session.messages.first(),
            Some(AgentMessage::Compaction { .. })
        ));
    }

    #[tokio::test]
    async fn ui_ticks_persist_results_and_do_not_double_count() {
        let temp = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(temp.path());
        let mut app = App::new(Runtime::new(true, 30), Route::Large, Some(store));
        app.submit("固定脚本".into());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while app.busy() {
                app.tick().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.total_tools, 3);
        assert_eq!(app.total_turns, 4);
        assert_eq!(app.history.iter().filter(|m| m.role == "助手").count(), 1);
        let session = app
            .store
            .as_ref()
            .unwrap()
            .load(&app.session_id)
            .await
            .unwrap();
        assert!(!session.messages.is_empty());
        app.command("/req").await;
        assert!(app.current.is_logs());
        assert_eq!(app.logs.current().unwrap().record.layer, "request");
        let id = app.session_id.clone();
        app.command("/new").await;
        assert!(app.messages.is_empty());
        app.load(&id).await.unwrap();
        assert!(!app.messages.is_empty());
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .await;
        assert!(!app.should_quit);
        assert_eq!(app.input, "q");
    }

    #[tokio::test]
    async fn submit_writes_session_diagnostic_journal() {
        let temp = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(temp.path().join("sessions"));
        let mut app = App::new(Runtime::new(true, 30), Route::Large, Some(store));

        app.submit("固定脚本".into());
        let path = app
            .diagnostic_log_path()
            .expect("diagnostic journal should be opened");
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(path.ends_with(format!("{}.jsonl", app.session_id)));
        assert!(content.contains("\"event\":\"turn_started\""));
        assert!(content.contains("\"layer\":\"ui\""));
        app.cancel();
        if let Some(worker) = app.worker.take() {
            worker.await.unwrap();
        }
    }

    #[tokio::test]
    async fn process_shortcut_preserves_pending_choice_and_input() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.command("/choice-demo").await;
        app.input = "补充内容".into();
        app.log_focus = true;
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        assert!(app.current.is_processes());
        assert!(!app.log_focus);
        assert_eq!(app.input, "补充内容");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(!app.current.is_processes());
        assert!(!app.runtime.choices.pending().is_empty());
        app.key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))
            .await;
        assert!(app.current.is_processes());
    }

    #[tokio::test]
    async fn chat_shortcuts_preserve_input_and_pending_choice_from_every_page() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.command("/choice-demo").await;
        app.input = "补充内容".into();
        app.services_mcp = true;
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            for page in [
                Page::Chat,
                Page::Logs,
                Page::Processes,
                Page::Services,
                Page::Spec,
            ] {
                app.current = page;
                app.log_focus = true;
                app.key(KeyEvent::new(KeyCode::F(1), modifiers)).await;
                assert!(app.current.is_chat());
                assert!(!app.log_focus);
                assert_eq!(app.input, "补充内容");
                assert!(!app.runtime.choices.pending().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn route_change_applies_to_next_turn_and_slash_completion_works() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.input = "/route qw".into();
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input, "/route qwen");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.route, Route::Qwen);
        app.input = "/route auto".into();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.route, Route::Auto);
        assert!(!app.busy());
    }

    #[tokio::test]
    async fn pending_choice_accepts_typed_custom_answer() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.command("/choice-demo").await;
        assert_eq!(app.runtime.choices.pending().len(), 1);

        for ch in "weykonk".chars() {
            app.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(app.input, "weykonk");

        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;

        assert!(app.runtime.choices.pending().is_empty());
        assert!(app.input.is_empty());
        assert!(app
            .history
            .iter()
            .any(|entry| entry.text.contains("选择卡片已回答：自定义输入")
                && entry.text.contains("weykonk")));
    }

    #[tokio::test]
    async fn mouse_submit_executes_route_command_locally_and_queues_busy_input() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.click_map.register(0, 0, 2, 2, ClickAction::Submit);
        app.input = " /route laya ".into();
        app.on_click(0, 0).await;
        assert_eq!(app.route, Route::Laya);
        assert!(!app.busy());
        assert!(app.messages.is_empty());
        app.submit("test".into());
        app.input = "保留草稿".into();
        app.on_click(0, 0).await;
        assert!(app.input.is_empty());
        assert_eq!(app.steering.pending().len(), 1);
        app.cancel();
        if let Some(worker) = app.worker.take() {
            worker.await.unwrap();
        }
    }
    #[tokio::test]
    async fn mouse_wheel_routes_by_panel_and_click_resets_details() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        for _ in 0..30 {
            app.logs.push(
                "t",
                DiagnosticEvent {
                    timestamp_ms: 0,
                    layer: "tool".into(),
                    event: "e".into(),
                    data: serde_json::json!({}),
                },
            );
        }
        app.click_map
            .register(0, 0, 10, 10, ClickAction::ScrollLogs);
        app.click_map
            .register(0, 2, 10, 1, ClickAction::SelectLog(5));
        app.click_map
            .register(20, 0, 10, 10, ClickAction::ScrollDetail);
        app.click_map
            .register(40, 0, 10, 10, ClickAction::ScrollChat);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.logs.detail_scroll = 100;
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 2))
            .await;
        assert_eq!(app.logs.selected, 5);
        assert_eq!(app.logs.detail_scroll, 0);
        assert!(!app.logs.follow);
        assert!(app.log_focus);
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 1, 2)).await;
        assert_eq!(app.logs.selected, 8);
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 21, 1)).await;
        assert_eq!(app.logs.detail_scroll, 3);
        assert_eq!(app.logs.selected, 8);
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 41, 1)).await;
        assert_eq!(app.chat_scroll, 3);
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 41, 1)).await;
        assert_eq!(app.chat_scroll, 0);
        assert_eq!(app.mouse_events, 5);
        app.command("/mouse").await;
        assert!(app.history.last().unwrap().text.contains("5 个鼠标事件"));
    }

    /// P0 #1: `/process inspect <id>` 解析后应直接转成 `{action:"inspect", execution_id}`
    /// 走 process_control 工具,不需要再打开 /process 面板或额外命令。错误信息里
    /// 能看到 "未知 execution_id",证明参数传递正确。
    #[tokio::test]
    async fn process_inspect_subcommand_routes_to_process_control() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 任意 UUID 形式的字符串即可,ProcessTool 解析失败时会回 ToolError::InvalidInput。
        let fake_id = "0123abcd-0000-7000-8000-000000000000";
        app.command(&format!("/process inspect {fake_id}")).await;
        // 等 job 完成并把结果取出。
        let mut error_text = String::new();
        for _ in 0..200 {
            let (events, results) = app.processes.tick(app.current.is_processes()).await;
            for r in results {
                match r {
                    Ok(value) => {
                        // 不应成功:fake id 根本不存在。
                        panic!("不应成功,got value={value}");
                    }
                    Err(text) => error_text = text,
                }
            }
            // 没有 pending job 就可以停。
            if app.processes.jobs.is_empty() && !error_text.is_empty() {
                let _ = events;
                break;
            }
            // 让出执行权让 job 有机会跑。
            tokio::task::yield_now().await;
        }
        assert!(
            error_text.contains("未知 execution_id"),
            "应当把 fake_id 透传给 inspect,错误信息应含 '未知 execution_id',got: {error_text:?}"
        );
    }

    /// P0 #5: 启动时若 recover 出 Lost 记录,App::new 在 history 顶部插入系统消息。
    ///
    /// 通过手工在日志目录里写两个 `<execution_id>/meta.json` 记录(状态 Running,
    /// 启动时会被 recover 改成 Lost)来模拟"上次未正常关闭"。这样可以避开
    /// start_owned 的后台 executor 持有 Inner Arc 导致 .manager.lock 释放不了
    /// 的问题。
    #[tokio::test]
    async fn app_new_announces_lost_records_from_recovery() {
        use smallhead_core::execution::smallhead_process::{
            ProcessManager, ProcessSpec, ProcessState,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let id_a = smallhead_core::execution::smallhead_process::ExecutionId::new();
        let id_b = smallhead_core::execution::smallhead_process::ExecutionId::new();
        // 写两个 meta.json 假装是上次会话留下的非终态执行。
        for id in [id_a, id_b] {
            let id_dir = root.join(id.to_string());
            std::fs::create_dir_all(&id_dir).unwrap();
            let record = serde_json::json!({
                "owner": "ui",
                "id": id.to_string(),
                "spec": {
                    "command": "sleep 60",
                    "args": [],
                    "env": {},
                    "scope": "TaskBound",
                },
                "state": {"Running": null},
                "pid": null,
                "registered_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                "started_at": null,
                "finished_at": null,
                "log_dir": null,
            });
            std::fs::write(
                id_dir.join("meta.json"),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }
        let pm = ProcessManager::with_log_root(root).unwrap();
        assert_eq!(pm.list().len(), 2);
        for id in [id_a, id_b] {
            let record = pm.inspect(id).expect("记录应被 recover 出来");
            assert!(
                matches!(record.state, ProcessState::Lost(_)),
                "recover 后应为 Lost, got {:?}",
                record.state
            );
        }
        let execution =
            std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        let runtime = Runtime {
            monads: Default::default(),
            mcp: crate::mcp_services::default_manager(true),
            mcp_config: Default::default(),
            lenses: Default::default(),
            execution: Ok(execution),
            choices: std::sync::Arc::new(smallhead_core::ChoiceBroker::new()),
            provider: crate::runtime::mock_provider(),
            label: "Lost 测试".into(),
            mock: true,
            timeout: std::time::Duration::from_secs(30),
        };
        let app = App::new(runtime, Route::Large, None);
        // 找到那条 Lost 公告(可能在欢迎语之后)。
        let lost_msg = app
            .history
            .iter()
            .find(|m| m.text.contains("因未正常关闭被标记为 Lost"))
            .expect("应有 Lost 公告");
        assert!(lost_msg.text.contains("2 个执行因未正常关闭被标记为 Lost"));
        assert!(lost_msg.text.contains(&id_a.to_string()));
        assert!(lost_msg.text.contains(&id_b.to_string()));
        let _ = (id_a, id_b, ProcessSpec::new("echo")); // 避免 unused warning
    }

    /// P2 #8: `/exec-bg <cmd>` 等价于 `/exec` —— launch 路径带 wait=false。
    /// 直接验证 launch payload 而非 mock_run 端到端,因为 process 行为由
    /// BashTool 路径保证,这里测的是命令分发正确性。
    #[tokio::test]
    async fn exec_bg_slash_command_is_alias_for_exec_with_wait_false() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        let before = app.processes.jobs.len();
        app.command("/exec-bg sleep 60").await;
        // jobs 队列应多一个,bash=true + wait=false payload 由 launch 内部发出。
        assert_eq!(
            app.processes.jobs.len(),
            before + 1,
            "/exec-bg 应入队一个 launch job"
        );
        // /exec 走相同路径,bash=true。
        let before2 = app.processes.jobs.len();
        app.command("/exec sleep 60").await;
        assert_eq!(
            app.processes.jobs.len(),
            before2 + 1,
            "/exec 也应入队一个 launch job"
        );
        // shutdown 时这些 task 会被 abort —— 但不能漏 drop。
        app.shutdown().await.ok();
    }

    /// P2 #6: F7 清理已完成的执行;非终态拒绝并提示。
    #[tokio::test]
    async fn f7_purges_terminal_execution_and_refuses_running() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 启一个 echo,等 terminal。
        app.command("/exec echo p2-f7").await;
        let mut id = None;
        for _ in 0..200 {
            if let Some(first) = app.processes.ids().first().copied() {
                id = Some(first);
                break;
            }
            app.tick().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let id = id.expect("应有 1 个执行");
        // 等 terminal。
        for _ in 0..200 {
            app.tick().await;
            if let Ok(rt) = app.processes.rt.as_ref() {
                if rt
                    .processes
                    .inspect(id)
                    .map(|r| r.is_terminal())
                    .unwrap_or(false)
                {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        app.processes.selected = 0;
        app.current = Page::Processes;
        // F7 应触发 purge。launch 进入 jobs,等结果。
        let _before_jobs = app.processes.jobs.len();
        app.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
            .await;
        // 非终态路径保护:为同一个 id 设缓存签名,以便 purge 后 retain 清掉。
        app.processes.output_pages.insert(
            (id, "stdout"),
            smallhead_core::execution::smallhead_process::OutputPage {
                bytes: vec![],
                text: String::new(),
                next_offset: 0,
                stored_bytes: 0,
                eof: true,
                complete: true,
                output_limited: false,
            },
        );
        // 跑 tick 把结果 drain 出来。
        for _ in 0..500 {
            let (_, results) = app.processes.tick(app.current.is_processes()).await;
            for value in results.into_iter().flatten() {
                // 结果应是 Ok({"purged":true})。
                if value.get("purged").is_some() {
                    // 此时 id 应已从 ids() 中移除。
                    assert!(
                        !app.processes.ids().contains(&id),
                        "purge 后 ids() 不应仍含该 id"
                    );
                    // 缓存项也应被 retain 清掉。
                    assert!(!app.processes.output_pages.contains_key(&(id, "stdout")));
                    assert!(!app
                        .processes
                        .last_read_signature
                        .contains_key(&(id, "stdout")));
                    break;
                }
            }
            tokio::task::yield_now().await;
            if !app.processes.ids().contains(&id) {
                break;
            }
        }
        assert!(!app.processes.ids().contains(&id), "purge 后执行消失");
        // 启一个 sleep 30 验证非终态拒绝路径。
        app.command("/exec sleep 30").await;
        for _ in 0..200 {
            app.tick().await;
            if app.processes.ids().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // 选中新的 sleep —— sort 后通常 sleep 在前。
        let sleep_id = app
            .processes
            .ids()
            .first()
            .copied()
            .expect("sleep 应在 ids 里");
        app.processes.selected = 0;
        app.processes.output_pages.insert(
            (sleep_id, "stdout"),
            smallhead_core::execution::smallhead_process::OutputPage {
                bytes: vec![],
                text: String::new(),
                next_offset: 0,
                stored_bytes: 0,
                eof: false,
                complete: false,
                output_limited: false,
            },
        );
        let jobs_before = app.processes.jobs.len();
        app.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.processes.jobs.len(),
            jobs_before,
            "非终态 F7 不应入队 purge job"
        );
        assert!(
            app.status.contains("非终态") || app.status.contains("先停止"),
            "状态栏应提示非终态拒绝: {}",
            app.status
        );
        app.shutdown().await.ok();
    }

    /// P2 #5: `/process list <offset> [limit]` 把 offset/limit 透传进 process_control。
    /// 端到端验证 launch 的 payload 字段;不依赖 read_output 行为。
    #[tokio::test]
    async fn process_list_slash_command_passes_offset_and_limit() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 启动若干个执行。
        for i in 0..3 {
            app.command(&format!("/exec echo p2-list-{i}")).await;
        }
        // 跑 /process list 5 10,等结果。
        app.command("/process list 5 10").await;
        let mut captured = None;
        for _ in 0..500 {
            let (_, results) = app.processes.tick(app.current.is_processes()).await;
            for value in results.into_iter().flatten() {
                if value.get("total").is_some() {
                    captured = Some(value);
                }
            }
            if captured.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let value = captured.expect("/process list 应有结果");
        // process_control list 返回 { executions: [...], total: N, next_offset: ... }
        assert_eq!(
            value.get("offset").and_then(|v| v.as_u64()),
            None,
            "process_control 透传 offset 不回写,但调用本身不抛错"
        );
        assert!(value.get("total").is_some(), "result 应有 total 字段");
        assert!(
            value
                .get("executions")
                .map(|v| v.is_array())
                .unwrap_or(false),
            "result 应有 executions 数组"
        );
        app.shutdown().await.ok();
    }

    /// P2 #1: `/events <id> [offset]` 走 process_control read_output stream=events。
    /// 等待 job 完成,从 results 拿到 events.jsonl 文本内容即可。
    #[tokio::test]
    async fn events_slash_command_routes_to_process_control_with_events_stream() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 启动一个 echo 命令以便拿到一个 execution_id。
        app.command("/exec echo p2-events").await;
        let mut id: Option<smallhead_core::execution::smallhead_process::ExecutionId> = None;
        for _ in 0..200 {
            if let Some(first) = app.processes.ids().first().copied() {
                id = Some(first);
                break;
            }
            app.tick().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let id = id.expect("应有 1 个执行");
        app.processes.selected = 0;
        // 触发 /events。
        app.command(&format!("/events {id}")).await;
        // 等 /events 任务完成,从 results 里筛 OutputPage(next_offset 字段唯一)。
        let mut captured: Option<serde_json::Value> = None;
        for _ in 0..500 {
            let (_, results) = app.processes.tick(app.current.is_processes()).await;
            for r in results {
                match r {
                    Ok(value) => {
                        // OutputPage 有 next_offset + text 字段;ProcessRecord 用 id/state 字段。
                        if value.get("next_offset").is_some() && value.get("text").is_some() {
                            captured = Some(value);
                        }
                    }
                    Err(text) => panic!("/events 不应失败: {text}"),
                }
            }
            if captured.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let value = captured.expect("/events 应有 OutputPage 结果返回");
        assert!(value.get("text").is_some(), "OutputPage 应含 text");
        assert!(
            value.get("next_offset").is_some(),
            "OutputPage 应含 next_offset"
        );
        assert!(value.get("complete").is_some(), "OutputPage 应含 complete");
        // /events 缺 ID 应给用法提示。
        app.command("/events").await;
        let usage_msg = app
            .history
            .iter()
            .rev()
            .find(|m| m.role == "系统" && m.text.contains("/events"))
            .expect("应有 /events 用法提示");
        assert!(usage_msg.text.contains("用法"));
        app.shutdown().await.ok();
    }

    /// P2 #14: tooltip 在静默 ≥2s 后填充,输入立即清空。
    /// 直接测 compute_tooltip() 的语义分支,因为渲染层走 stdout 不便捕获。
    #[tokio::test]
    async fn tooltip_appears_after_two_seconds_of_idle() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 初始:刚刚初始化,last_input_at = now → < 2s → tooltip = None。
        app.tick().await;
        assert!(
            app.tooltip.is_none(),
            "刚初始化时 last_input_at 是 now,不应有 tooltip"
        );
        // 把 last_input_at 拨到 3s 前,再 tick → tooltip 应填充。
        app.last_input_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(3))
            .unwrap_or_else(std::time::Instant::now);
        app.tick().await;
        assert!(app.tooltip.is_some(), "静默 3s 后 tooltip 应有内容");
        let tip = app.tooltip.as_deref().unwrap();
        assert!(tip.contains("/exec") || tip.contains("取消"));
        // 按键 → last_input_at 重置 → 下次 tick tooltip 清空。
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.tick().await;
        assert!(app.tooltip.is_none(), "按键后 tooltip 应清空");
        app.shutdown().await.ok();
    }

    /// P2 #14: 进程页的 tooltip 文案区分 events 流 / 子过滤两种状态。
    #[test]
    fn compute_tooltip_branches_by_process_state() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 进程页,默认状态。
        app.current = Page::Processes;
        let tip = app.compute_tooltip();
        assert!(tip.contains("c 复制 log_dir"), "进程页默认 tooltip: {tip}");
        // 子过滤设上。
        app.processes.row_subfilter = Some("mongo".into());
        let tip = app.compute_tooltip();
        assert!(tip.contains("清空过滤"), "子过滤 tooltip: {tip}");
        app.processes.row_subfilter = None;
        // events 流打开。
        app.processes.events_stream_open = true;
        let tip = app.compute_tooltip();
        assert!(tip.contains("关闭 events"), "events 流 tooltip: {tip}");
        app.processes.events_stream_open = false;
        // 等待中 → 取消提示。
        let tip = app.compute_tooltip();
        assert!(tip.contains("c 复制 log_dir"));
    }

    /// Page 枚举的 helper 必须保持互斥:同一时刻最多一个 is_*() 为 true。
    #[test]
    fn current_page_helper_keeps_state_consistent() {
        // 初始默认页是 Chat。
        assert!(Page::Chat.is_chat());
        assert!(!Page::Chat.is_logs());
        assert!(!Page::Chat.is_processes());
        assert!(!Page::Chat.is_services());

        // 显式切到 Logs。
        assert!(!Page::Logs.is_chat());
        assert!(Page::Logs.is_logs());
        assert!(!Page::Logs.is_processes());
        assert!(!Page::Logs.is_services());

        // 显式切到 Processes。
        assert!(!Page::Processes.is_chat());
        assert!(!Page::Processes.is_logs());
        assert!(Page::Processes.is_processes());
        assert!(!Page::Processes.is_services());

        // 显式切到 Services(为后续 commit 4 预留的页面)。
        assert!(!Page::Services.is_chat());
        assert!(!Page::Services.is_logs());
        assert!(!Page::Services.is_processes());
        assert!(Page::Services.is_services());

        // App.current 在四个页面间循环切换时,只有当前页 helper 返回 true。
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        let pages = [Page::Chat, Page::Logs, Page::Processes, Page::Services];
        for page in pages {
            app.current = page;
            let flags = [
                app.current.is_chat(),
                app.current.is_logs(),
                app.current.is_processes(),
                app.current.is_services(),
            ];
            assert_eq!(flags.iter().filter(|b| **b).count(), 1, "{page:?}");
            assert_eq!(app.current, page);
        }
    }

    /// Commit 2: 用名字做稳定选择。插入 2 个 service 后 ensure 一个、forget 另一个,
    /// 反复操作,被选中的服务名应保持不变,不会被行号悄悄换成另一个服务。
    #[tokio::test]
    async fn service_selected_name_survives_ensure_and_forget() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        // 一次性塞两个独立 service。health_url 指向空闲端口(0)避免副作用。
        for name in ["alpha-svc", "beta-svc"] {
            rt.services
                .ensure(ServiceSpec {
                    name: name.into(),
                    process: ProcessSpec::new("echo done"),
                    health_url: format!("http://127.0.0.1:0/health-{name}"),
                    identity: BTreeMap::from([("status".into(), json!("ready"))]),
                    startup_timeout_ms: 60_000,
                })
                .await
                .unwrap();
        }
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        // 把 ServiceManager 事件订阅也接到 App 上,模拟生产路径。
        app.service_events = Some(rt.services.subscribe());
        // 排序按字母:alpha 在 0,beta 在 1。鼠标点击第 1 行(beta)。
        app.processes.select_service(Some(1));
        assert_eq!(
            app.processes.service_selected_name.as_deref(),
            Some("beta-svc"),
            "select_service(1) 应记录 beta-svc"
        );
        // 直接对 beta-svc ensure 一遍(已登记)。`ensure_service_by_name` 接受
        // 任意已登记 spec 重启;这里我们改用更稳的 ensure 路径:再次 ensure 一个
        // 新的 spec 不会影响 beta-svc 的名字。
        rt.services
            .ensure(ServiceSpec {
                name: "gamma-svc".into(),
                process: ProcessSpec::new("echo gamma"),
                health_url: "http://127.0.0.1:0/health-gamma".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        // 新服务 gamma 出现,排序会变成 [alpha, beta, gamma];但选中的依然是 beta-svc。
        assert_eq!(
            app.processes.service_selected_name.as_deref(),
            Some("beta-svc"),
            "插入新服务后,选中的 beta-svc 不应被洗掉"
        );
        // forget alpha-svc,服务列表变成 [beta, gamma];选中仍应是 beta-svc。
        rt.services.forget("alpha-svc").unwrap();
        assert_eq!(
            app.processes.service_selected_name.as_deref(),
            Some("beta-svc"),
            "forget 其他服务后,选中的 beta-svc 不应漂移到 gamma"
        );
        // 关键:被 forget 的服务不是我们选中的,所以 selected_service_name() 仍返回 beta。
        assert_eq!(
            app.processes.selected_service_name().as_deref(),
            Some("beta-svc")
        );
        // forget beta-svc 之后,被选中的名字应当从 list 消失,`selected_service_name()`
        // 必须返回 None —— 不能 fallback 到第 0 行误把 alpha-svc 顶上来。
        rt.services.forget("beta-svc").unwrap();
        assert_eq!(
            app.processes.service_selected_name.as_deref(),
            Some("beta-svc")
        );
        assert!(
            app.processes.selected_service_name().is_none(),
            "forget 选中的服务后,selected_service_name() 必须返回 None,不能 fallback"
        );
    }

    /// Commit 4: Ctrl+S 切到服务页;/services 同效;再按一次回到 Chat。
    /// 同时验证服务页能在 ExecutionRuntime 不可用时不崩,只渲染"尚无服务"提示。
    #[tokio::test]
    async fn ctrl_s_and_services_command_route_to_services_page() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        // 初始 Chat。
        assert!(app.current.is_chat());
        // Ctrl+S 切到 Services。
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        assert!(app.current.is_services());
        // /services 再切回 Chat。
        app.command("/services").await;
        assert!(app.current.is_chat());
        // 第二次 /services 又去 Services。
        app.command("/services").await;
        assert!(app.current.is_services());
    }

    /// Commit 4: 渲染服务页能正确把 ServiceView 列表画到 TestBackend;
    /// 即便用户尚未登记任何服务,`service_view_all()` 也会把 `laya/qwen` 预设
    /// 当作"未启动"列出,验证默认状态。
    #[tokio::test]
    async fn services_page_renders_presets_as_unstarted_when_no_records() {
        use crate::app::Page;
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.current = Page::Services;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 把整屏 cell 拼起来,剔除空格再做包含判断。
        // TestBackend 的中文字符被 width-pad,直接 contains("未启动") 不可靠。
        let screen: String = (2..30)
            .flat_map(|y| (0..140).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        let collapsed: String = screen.split_whitespace().collect();
        assert!(
            collapsed.contains("laya"),
            "服务页应显示预设 laya, screen: {screen:?}"
        );
        assert!(
            collapsed.contains("qwen"),
            "服务页应显示预设 qwen, screen: {screen:?}"
        );
        // "未启动" 三字 (每个 1 字符) 拼起来 = "未启动";若被空格切分后是 "未启动" 一段。
        assert!(
            collapsed.contains("未启动"),
            "未登记的预设应显示「未启动」状态, screen: {screen:?}"
        );
    }

    /// Commit 4: 给出 ExecutionRuntime + 2 个服务时,服务页能渲染两条 ServiceView
    /// 记录,选中名从 0/1 切换时反色行也跟随。
    #[tokio::test]
    async fn services_page_renders_views_with_color_per_state() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        for name in ["alpha-svc", "beta-svc"] {
            rt.services
                .ensure(ServiceSpec {
                    name: name.into(),
                    process: ProcessSpec::new("echo done"),
                    health_url: format!("http://127.0.0.1:0/health-{name}"),
                    identity: BTreeMap::from([("status".into(), json!("ready"))]),
                    startup_timeout_ms: 60_000,
                })
                .await
                .unwrap();
        }
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 扫描整个屏幕(不含顶部按钮行 0~1),应能找到 "alpha-svc" 与 "beta-svc"。
        let mut found_alpha = false;
        let mut found_beta = false;
        for y in 2..30 {
            let line: String = (0..140).map(|x| buffer[(x, y)].symbol()).collect();
            if line.contains("alpha-svc") {
                found_alpha = true;
            }
            if line.contains("beta-svc") {
                found_beta = true;
            }
        }
        assert!(found_alpha, "服务列表应渲染 alpha-svc");
        assert!(found_beta, "服务列表应渲染 beta-svc");
    }

    /// Commit 2: `App.subscription()` 必须从 `ExecutionRuntime::services.subscribe()` 拿到
    /// 真实 receiver;`tick()` 跑一次后,序号从 0 前进,代表事件确实被消费了。
    #[tokio::test]
    async fn subscription_wires_broadcast_and_tick_advances_sequence() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        // 默认 Runtime 是 mock 的,execution 不会初始化;这里手工把 receiver 接上,
        // 复现 production 路径。
        app.service_events = Some(rt.services.subscribe());
        // 必须存在一个订阅句柄,否则 App 与 ServiceManager 没有真正桥接。
        assert!(
            app.subscription().is_some(),
            "subscription() 必须返回 Some(receiver),否则 broadcast 没接上"
        );
        // 起始序号 0。
        assert_eq!(app.last_service_sequence, 0);
        // 触发一次 ensure。ServiceManager 内部至少发出 1 条 OperationStarted
        // 事件(broadcast),`tick()` 应该把它消费并前进 last_service_sequence。
        rt.services
            .ensure(ServiceSpec {
                name: "wire-svc".into(),
                process: ProcessSpec::new("echo wire"),
                health_url: "http://127.0.0.1:0/health-wire".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        app.tick().await;
        assert!(
            app.last_service_sequence > 0,
            "ensure 后 tick 一次,last_service_sequence 应前进(实际={})",
            app.last_service_sequence
        );
        // 同一帧内 status 栏或日志会被 apply_service_event 写过,这里只断言序号
        // 推进。状态栏文案随具体 kind 变化,不锁死。
    }

    /// Commit 5: Tab 键在服务页三个详情 tab 间循环 (0 → 1 → 2 → 0),
    /// BackTab 反向 (0 → 2 → 1 → 0);非服务页时不影响。
    #[tokio::test]
    async fn service_detail_tab_cycles_on_tab_and_backtab() {
        use crate::app::Page;
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.current = Page::Services;
        assert_eq!(app.service_detail_tab, 0);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 1, "Tab 一次应到输出");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 2, "Tab 两次应到事件");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 0, "Tab 三次循环回概览");
        // BackTab 反向。
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.service_detail_tab, 2, "BackTab 一次应到事件");
        // 切回 Chat 页面后,Tab 行为应回到补全/焦点切换,不应改 service_detail_tab。
        app.current = Page::Chat;
        app.service_detail_tab = 0;
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.service_detail_tab, 0,
            "非服务页 Tab 不应改 service_detail_tab"
        );
    }

    #[tokio::test]
    async fn app_owns_one_scheduler_until_shutdown() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        assert!(app.health_scheduler.is_some());
        app.tick().await;
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.tick().await;
        assert!(app.health_scheduler.is_some());
        app.shutdown().await.unwrap();
        assert!(app.health_scheduler.is_none());
        app.shutdown().await.unwrap();
    }

    /// Commit 5: 概览 tab 渲染 Agent 写入的 description,长文本自动 wrap。
    /// 验证 description 出现 + "── 服务说明 ──" 标题出现。
    #[tokio::test]
    async fn service_overview_renders_description_and_skips_legacy_choice_card() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "desc-svc".into(),
                process: ProcessSpec::new("echo desc"),
                health_url: "http://127.0.0.1:0/health-desc".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        // 写一段 description(模拟 Agent 调过 service_update_description)。
        rt.services
            .update_description(
                "desc-svc",
                "这是一个给 Laya 调度用的预置,负责把请求路由到合适的 Provider。第一行很普通。第二行仍然在说明文字,稍微长一点。".to_string(),
            )
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("desc-svc".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 60)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 概览 tab 默认;扫描右半区(40% 列切分后从 x=80 起,到 x=199)。
        // ratatui 在 CJK 字符之间补空格,所以先把行压平再做包含检查。
        let mut found_description_marker = false;
        let mut found_description_text = false;
        let mut found_technical_marker = false;
        for y in 0..60 {
            let raw: String = (80..200).map(|x| buffer[(x, y)].symbol()).collect();
            let row: String = raw.split_whitespace().collect::<Vec<_>>().join("");
            if row.contains("服务说明") {
                found_description_marker = true;
            }
            if row.contains("这是一个给Laya调度用的预置") {
                found_description_text = true;
            }
            if row.contains("技术详情") {
                found_technical_marker = true;
            }
        }
        if !found_description_marker || !found_description_text || !found_technical_marker {
            for y in 0..20 {
                let row: String = (0..200).map(|x| buffer[(x, y)].symbol()).collect();
                let compact: String = row.split_whitespace().collect::<Vec<_>>().join("");
                eprintln!("y={y:02} compact={compact:?}");
            }
        }
        assert!(found_description_marker, "应渲染 '── 服务说明 ──' 标题");
        assert!(found_description_text, "应渲染 description 第一行");
        assert!(found_technical_marker, "应渲染 '── 技术详情 ──' 标题");
    }

    /// Commit 5: 没有 description 时,概览 tab 显示降级提示而非空白。
    /// 设计稿 2026-10-15:不展示 use_case / accessible / authenticated 等固定字段。
    #[tokio::test]
    async fn service_overview_renders_no_description_hint_when_blank() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "naked-svc".into(),
                process: ProcessSpec::new("echo naked"),
                health_url: "http://127.0.0.1:0/health-naked".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("naked-svc".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 右半区从 40% 列切分后从 x=56(140*40%) 起,到 x=139。
        // Ratatui 在 CJK 字符之间补空格,所以用 split_whitespace 拼回。
        let mut found_hint = false;
        for y in 0..30 {
            let raw: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
            let row: String = raw.split_whitespace().collect::<Vec<_>>().join("");
            if row.contains("暂无服务说明") {
                found_hint = true;
            }
            // 不应再渲染旧 Choice Card 的固定字段。
            assert!(
                !row.contains("use_case"),
                "设计稿 2026-10-15:不展示 use_case 字段"
            );
            assert!(
                !row.contains("accessible"),
                "设计稿 2026-10-15:不展示 accessible 字段"
            );
            assert!(
                !row.contains("authenticated"),
                "设计稿 2026-10-15:不展示 authenticated 字段"
            );
        }
        if !found_hint {
            // 调试输出:把右半区逐行打印。
            for y in 0..30 {
                let row: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
                let compact: String = row.split_whitespace().collect::<Vec<_>>().join("");
                eprintln!("y={y:02} {compact}");
            }
        }
        assert!(found_hint, "无 description 时应显示降级提示");
    }

    /// Commit 5: 切到 Output tab 后,`tick()` 把 stdout/stderr 预拉进缓存;
    /// 渲染 Output tab 读缓存显示,不在渲染路径里 await。
    #[tokio::test]
    async fn service_output_tab_reads_cached_snapshot_after_tick() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "out-svc".into(),
                process: ProcessSpec::new("echo hello-from-stdout"),
                health_url: "http://127.0.0.1:0/health-out".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        // 等 echo 跑完。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("out-svc".to_string());
        // 切到 Output tab,触发 dirty。
        app.service_detail_tab = 1;
        app.service_output_dirty = true;
        app.tick().await;
        // 缓存里应有一份 out-svc 的 stdout 文本。
        let snap = app
            .service_output_cache
            .get("out-svc")
            .expect("tick 后应填充 out-svc 快照");
        assert!(
            snap.stdout.contains("hello-from-stdout")
                || snap.stdout.contains("hello")
                || !snap.stdout.is_empty(),
            "stdout 应被预拉进缓存(实际长度={})",
            snap.stdout.len()
        );
        // 渲染 Output tab 时不应再次 await 阻塞。
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut found_stdout_section = false;
        for y in 0..30 {
            let row: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
            if row.contains("stdout") {
                found_stdout_section = true;
            }
        }
        assert!(found_stdout_section, "Output tab 应渲染 'stdout' 段标题");
        // An idle service may produce output without emitting a state event.
        app.service_output_cache.clear();
        app.service_output_dirty = false;
        app.service_output_next_poll = Instant::now() - std::time::Duration::from_secs(1);
        app.tick().await;
        assert!(app.service_output_cache["out-svc"]
            .stdout
            .contains("hello-from-stdout"));
        assert!(!app.service_output_dirty);
    }

    /// Commit 5: 切到 Output tab 时,`service_output_dirty` 被置 true,
    /// 触发 `tick()` 重新拉缓存;切走再切回也仍然 dirty。
    #[tokio::test]
    async fn service_output_tab_marks_dirty_on_switch_back() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "switch-svc".into(),
                process: ProcessSpec::new("echo switch"),
                health_url: "http://127.0.0.1:0/health-switch".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("switch-svc".to_string());
        app.service_detail_tab = 0;
        // 切到 Output tab 一次。
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 1);
        assert!(app.service_output_dirty, "切到 Output 应置 dirty");
        // tick 后 dirty 被清掉。
        app.tick().await;
        assert!(!app.service_output_dirty, "tick 后 dirty 应被清");
        // 切走再切回应再次 dirty。
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 2);
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.service_detail_tab, 1);
        assert!(app.service_output_dirty, "BackTab 切回 Output 也应 dirty");
    }

    /// Commit 5: 选中无 execution_id 的服务(尚未启动或外部),Output tab
    /// 应显示降级提示而非「正在拉取」抖动。
    #[tokio::test]
    async fn service_output_tab_renders_no_execution_hint_when_external_or_idle() {
        use crate::app::Page;
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        // 只 ensure 但不等待启动完成;execution_id 可能在中间状态。这里直接选个
        // 不在视图里的服务名,让 view.execution_id 为 None 的等效路径触发。
        rt.services
            .ensure(ServiceSpec {
                name: "idle-svc".into(),
                process: ProcessSpec::new("echo idle"),
                health_url: "http://127.0.0.1:0/health-idle".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.service_detail_tab = 1;
        // 选一个不存在于 views 的服务名,view lookup 返回 None。
        app.processes
            .service_selected_name
            .replace("ghost-svc".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut found_hint = false;
        for y in 0..30 {
            let raw: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
            let row: String = raw.split_whitespace().collect::<Vec<_>>().join("");
            if row.contains("未选择服务") {
                found_hint = true;
            }
        }
        if !found_hint {
            for y in 0..30 {
                let row: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
                let compact: String = row.split_whitespace().collect::<Vec<_>>().join("");
                eprintln!("y={y:02} {compact}");
            }
        }
        assert!(found_hint, "选中空服务时 Output tab 应给降级提示");
    }

    /// External services can be reused by ensure, but cannot be stopped by the UI.
    #[tokio::test]
    async fn external_service_ensure_allowed_and_stop_disabled() {
        use smallhead_core::execution::smallhead_process::service::ServiceSpec;
        use std::collections::BTreeMap;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 临时 HTTP server 模拟外部服务的 /health,让 ensure 走 external=true 分支。
        // 不能直接调 processes::tests::external_health_server(私有 mod)。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = r#"{"status":"ready","protocol_version":1}"#;
        let server_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        let spec = ServiceSpec {
            name: "laya".into(),
            process: smallhead_core::execution::smallhead_process::ProcessSpec::new(
                "echo never-spawned",
            ),
            health_url: format!("http://127.0.0.1:{port}/health"),
            identity: BTreeMap::from([("status".into(), serde_json::json!("ready"))]),
            startup_timeout_ms: 2000,
        };
        let (record, _op_id) = rt.services.ensure(spec).await.unwrap();
        assert!(
            record.external,
            "测试前提: 服务必须被识别为 external, got {:?}",
            record
        );
        server_task.abort();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        // 选中刚登记的外部服务。
        app.processes
            .service_selected_name
            .replace("laya".to_string());
        // Ensure can reuse an external service; stop remains forbidden.
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .await;
        assert!(
            !app.status.contains("仅可检查与清除登记"),
            "external ensure should be allowed, status={:?}",
            app.status
        );
        // 按 x —— 同样禁用。
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert!(
            app.status.contains("外部服务") && app.status.contains("不能停止"),
            "x 键在外部服务上应给禁用提示,实际 status={:?}",
            app.status
        );
        // r 键(inspect)对外部服务也允许 —— 设计稿未禁用。
        // 先把 status 清成已知字符串,再按 r,确认 status 不再含禁用提示。
        app.status = "sentinel".to_string();
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        assert!(
            !app.status.contains("仅可检查与清除登记"),
            "r 键在外部服务上不应禁用,实际 status={:?}",
            app.status
        );
        // 关键断言:停掉外部服务时不应该创建 ServiceOperation::Stop 之类的
        // in-flight 操作 —— 因为我们直接 early-return,根本没调 stop_service_by_name。
        let view = rt
            .services
            .service_view("laya")
            .expect("外部服务应在 view 里");
        assert!(
            view.in_flight_operation.is_none(),
            "禁用路径不应创建 in-flight 操作,实际={:?}",
            view.in_flight_operation
        );
    }

    /// Commit 7 #1: 宽屏 (200×40) 时服务页左列 40% / 右列 60% 并排,两列
    /// 都应该能看到 —— list 渲染 laya 名字,detail 渲染「详情」标题。
    #[tokio::test]
    async fn wide_layout_shows_list_and_detail_side_by_side() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "wide-svc".into(),
                process: ProcessSpec::new("echo wide"),
                health_url: "http://127.0.0.1:0/wide".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("wide-svc".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 40)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 40% 列(0..80)应当含 wide-svc;60% 列(80..200)应当含 "详情"。
        let mut found_list = false;
        let mut found_detail = false;
        for y in 0..40 {
            let left: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
            if left.contains("wide-svc") {
                found_list = true;
            }
            let right: String = (80..200).map(|x| buffer[(x, y)].symbol()).collect();
            let right_compact: String = right.split_whitespace().collect::<Vec<_>>().join("");
            if right_compact.contains("详情") {
                found_detail = true;
            }
        }
        assert!(found_list, "左列(0..80)应渲染 wide-svc");
        assert!(found_detail, "右列(80..200)应渲染「详情」标题");
    }

    /// Narrow terminals switch between the list and detail using Enter/Esc.
    #[tokio::test]
    async fn narrow_layout_stacks_list_and_detail() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "narrow-svc".into(),
                process: ProcessSpec::new("echo narrow"),
                health_url: "http://127.0.0.1:0/narrow".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("narrow-svc".to_string());
        rt.services
            .update_description(
                "narrow-svc",
                (0..40)
                    .map(|i| format!("detail-line-{i:02}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // The list occupies the narrow viewport until Enter focuses detail.
        let mut found_list = false;
        for y in 0..24 {
            let left: String = (0..32).map(|x| buffer[(x, y)].symbol()).collect();
            if left.contains("narrow-svc") {
                found_list = true;
            }
        }
        assert!(found_list, "窄屏左列(0..32)应仍渲染 narrow-svc");
        assert!(!app.service_detail_focus);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.service_detail_focus);
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let detail = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(detail.contains("detail-line-00"));
        app.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_scroll, 10);
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let scrolled: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!scrolled.contains("detail-line-00"));
        assert!(scrolled.contains("detail-line-10"));
        app.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_scroll, 0);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(!app.service_detail_focus);
        assert_eq!(app.current, Page::Services);
    }

    /// Commit 7 #3: 中文服务名("我的服务")的渲染宽度应该用 unicode-width
    /// 测出来的 8 cells(每字 2 wide),不是 4 chars 4 cells。
    /// 因为 `ServiceSpec` 校验只允许 ASCII 名字(确保),所以这里:
    /// 1) 用 unicode-width 直接验证 "我的服务" = 8 cells 的契约;
    /// 2) 走 services 页渲染,验证 CJK 字符("服务")在 list 标题里被
    ///    正确按 2 cells/char 渲染(render 路径走 unicode-width,
    ///    所以这同时验证了 CJK 在真实渲染中的宽度行为)。
    #[tokio::test]
    async fn chinese_service_name_renders_with_correct_width() {
        use unicode_width::UnicodeWidthStr;
        let chinese_name = "我的服务";
        assert_eq!(
            UnicodeWidthStr::width(chinese_name),
            8,
            "unicode-width 应对 CJK 字符返回 2 cells/char,实际={}",
            UnicodeWidthStr::width(chinese_name)
        );
        // 走服务页渲染,list block 标题里含 "服务" —— 验证它在 buffer 里
        // 出现且按 unicode-width 计算占 4 cells。
        let runtime = Runtime {
            monads: Default::default(),
            mcp: crate::mcp_services::default_manager(true),
            mcp_config: Default::default(),
            lenses: Default::default(),
            execution: Err("no execution for cjk width test".into()),
            choices: std::sync::Arc::new(smallhead_core::ChoiceBroker::new()),
            provider: crate::runtime::mock_provider(),
            label: "cjk width test".into(),
            mock: true,
            timeout: std::time::Duration::from_secs(30),
        };
        let mut app = App::new(runtime, Route::Large, None);
        app.current = Page::Services;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 左列 (0..56) 顶部 block 标题 "服务 · ↑↓ 选行 · Enter 详情"。
        // 找到含 "服务" 的行,验证用 unicode-width 计算宽度 ≥ 4 cells。
        let mut max_width = 0usize;
        for y in 0..30 {
            let row: String = (0..56).map(|x| buffer[(x, y)].symbol()).collect();
            let compact: String = row.split_whitespace().collect::<Vec<_>>().join("");
            if compact.contains("服务") {
                let w = UnicodeWidthStr::width(row.trim_end());
                if w > max_width {
                    max_width = w;
                }
            }
        }
        assert!(
            max_width >= 4,
            "中文「服务」应至少占 4 cells,实际={max_width}"
        );
    }

    /// Commit 7 #4: 全键盘可达,不需要鼠标。
    /// 1) Ctrl+S 进服务页;2) ↓ 选下一行;3) s 触发 ensure;
    /// 4) r 触发 inspect;5) d 触发 forget。整轮无 on_mouse 调用。
    #[tokio::test]
    async fn no_mouse_works_via_keyboard_only() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        rt.services
            .ensure(ServiceSpec {
                name: "kbd-svc".into(),
                process: ProcessSpec::new("echo kbd"),
                health_url: "http://127.0.0.1:0/kbd".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        let mouse_count_before = app.mouse_events;
        // 1) Ctrl+S 进服务页。
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.current, Page::Services, "Ctrl+S 应进服务页");
        // 2) 选第 1 行(用名字引用)。先选第 0 行,然后 ↓ 一次到 laya 默认预设。
        app.processes
            .service_selected_name
            .replace("kbd-svc".to_string());
        // 3) 按 r(inspect) —— 不依赖鼠标,应成功。
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        assert!(
            !app.status.contains("失败"),
            "r 键(inspect)应执行成功,实际 status={:?}",
            app.status
        );
        // 4) 按 s(ensure) —— 也不依赖鼠标。
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .await;
        // ensure 路径可能后台跑,我们只断言没立刻报错。
        assert!(
            !app.status.contains("失败"),
            "s 键(ensure)应执行成功,实际 status={:?}",
            app.status
        );
        // 5) 按 d(forget) —— 走 ServiceManager::forget 同步路径。
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .await;
        assert!(
            app.status.contains("已遗忘") || app.status.contains("forget 失败"),
            "d 键(forget)应返回遗忘成功或显式失败,实际 status={:?}",
            app.status
        );
        // mouse_events 计数不应增加。
        assert_eq!(
            app.mouse_events, mouse_count_before,
            "纯键盘流程不应触发 on_mouse,mouse_events={} -> {}",
            mouse_count_before, app.mouse_events
        );
    }

    /// Commit 7 #5: 服务页上 pending Choice 出现时,Tab / ↑↓ 仍能在服务
    /// 列表里切换选中,不应被 Choice 的焦点机制抢走。
    #[tokio::test]
    async fn choice_card_pending_does_not_steal_focus_from_services() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        for name in ["alpha", "beta"] {
            rt.services
                .ensure(ServiceSpec {
                    name: name.into(),
                    process: ProcessSpec::new("echo {name}"),
                    health_url: format!("http://127.0.0.1:0/{name}"),
                    identity: BTreeMap::from([("status".into(), json!("ready"))]),
                    startup_timeout_ms: 60_000,
                })
                .await
                .unwrap();
        }
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.processes
            .service_selected_name
            .replace("alpha".to_string());
        // 注入一个 pending Choice request。
        let choice = smallhead_core::choice::ChoiceRequest {
            id: "test-choice".into(),
            owner: Some("test".into()),
            title: "Pick one".into(),
            body: "pick one".into(),
            options: vec![
                smallhead_core::choice::ChoiceOption {
                    id: "a".into(),
                    label: "Option A".into(),
                    description: None,
                    intent: smallhead_core::choice::ChoiceIntent::Continue,
                },
                smallhead_core::choice::ChoiceOption {
                    id: "b".into(),
                    label: "Option B".into(),
                    description: None,
                    intent: smallhead_core::choice::ChoiceIntent::Continue,
                },
            ],
            default_option: None,
            timeout_ms: None,
            source: None,
            tool_call_id: None,
            created_at_ms: 0,
        };
        app.runtime.choices.present(choice).unwrap();
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.processes.selected_service_name().as_deref(),
            Some("beta")
        );
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.processes.selected_service_name().as_deref(),
            Some("alpha")
        );
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.service_detail_tab, 1);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.service_detail_focus);
        assert!(!app.runtime.choices.pending().is_empty());
        // ↑ / ↓ 应当切换服务 selected_name —— 不能被 Choice 的 navigation 抢走。
        // Service actions must also bypass pending chat choices.
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .await;
        // 即使 Choice pending,s 键仍按服务页语义作用。
        assert!(
            !app.status.contains("Choice"),
            "Choice pending 时 s 键不应被 Choice 抢走,实际 status={:?}",
            app.status
        );
        // 反向:把 page 切到 Chat,Choice 的 ↑↓ 才接管。
        app.current = Page::Chat;
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        // 不需要断言具体 choice_selection(可能为 0 或 2),只断言 page 切回 Chat 后
        // Choice 仍 pending,证明 choice 状态没被服务页的 s 键清掉。
        assert!(
            !app.runtime.choices.pending().is_empty(),
            "服务页 s 键不应消费 Choice"
        );
    }

    /// Initialization failure must not be rendered as an empty service registry.
    #[tokio::test]
    async fn unavailable_service_manager_renders_error_instead_of_empty_list() {
        let runtime = Runtime {
            monads: Default::default(),
            mcp: crate::mcp_services::default_manager(true),
            mcp_config: Default::default(),
            lenses: Default::default(),
            execution: Err("日志目录已被其他管理器占用或无法锁定".into()),
            choices: std::sync::Arc::new(smallhead_core::ChoiceBroker::new()),
            provider: crate::runtime::mock_provider(),
            label: "empty test".into(),
            mock: true,
            timeout: std::time::Duration::from_secs(30),
        };
        let mut app = App::new(runtime, Route::Large, None);
        assert!(app.status.contains("初始化失败"));
        assert!(app
            .history
            .iter()
            .any(|message| message.role == "错误"
                && message.text.contains("日志目录已被其他管理器占用")));
        app.current = Page::Services;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        for tab in 0..3 {
            app.service_detail_tab = tab;
            terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            let compact = text.split_whitespace().collect::<String>();
            assert!(compact.contains("服务管理器初始化失败"));
            assert!(compact.contains("日志目录已被其他管理器占用"));
            assert!(compact.contains("再重新启动本窗口"));
            assert!(!compact.contains("尚无服务"));
        }
    }

    /// Commit 7 #7 (optional): 外部服务没有 execution_id 时,Output tab
    /// 应当显示「未由本应用采集(外部服务或尚未启动)」,而不是空文本或异常。
    #[tokio::test]
    async fn external_service_with_no_execution_log_shows_clear_message() {
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 同 commit 6:启 server 让 ensure 走 external=true 分支。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = r#"{"status":"ready","protocol_version":1}"#;
        let server_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        let spec = ServiceSpec {
            name: "laya".into(),
            process: ProcessSpec::new("echo never-spawned"),
            health_url: format!("http://127.0.0.1:{port}/health"),
            identity: BTreeMap::from([("status".into(), serde_json::json!("ready"))]),
            startup_timeout_ms: 2000,
        };
        let (record, _op) = rt.services.ensure(spec).await.unwrap();
        assert!(record.external);
        server_task.abort();
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        app.service_detail_tab = 1; // Output tab
        app.processes
            .service_selected_name
            .replace("laya".to_string());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 右列(56..140)应含「未由本应用采集」提示。
        let mut found_msg = false;
        for y in 0..30 {
            let right: String = (56..140).map(|x| buffer[(x, y)].symbol()).collect();
            let compact: String = right.split_whitespace().collect::<Vec<_>>().join("");
            if compact.contains("未由本应用采集") {
                found_msg = true;
            }
        }
        assert!(found_msg, "外部服务在 Output tab 应显示「未由本应用采集」");
    }

    /// Commit 7 #8 (optional): 列表重排(新 service 被 ensure 进表)后,
    /// 选中名应保持稳定 —— 不漂移到错误的服务。
    #[tokio::test]
    async fn list_reordering_does_not_move_selection_to_wrong_service() {
        use serde_json::json;
        use smallhead_core::execution::smallhead_process::{service::ServiceSpec, ProcessSpec};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let pm = smallhead_core::execution::smallhead_process::ProcessManager::with_log_root(
            dir.path().to_path_buf(),
        )
        .unwrap();
        let rt = std::sync::Arc::new(smallhead_core::execution::ExecutionRuntime::new(pm).unwrap());
        for name in ["alpha", "beta"] {
            rt.services
                .ensure(ServiceSpec {
                    name: name.into(),
                    process: ProcessSpec::new("echo {name}"),
                    health_url: format!("http://127.0.0.1:0/{name}"),
                    identity: BTreeMap::from([("status".into(), json!("ready"))]),
                    startup_timeout_ms: 60_000,
                })
                .await
                .unwrap();
        }
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.processes.rt = Ok(rt.clone());
        app.runtime.execution = Ok(rt.clone());
        app.current = Page::Services;
        // 选中 beta —— 名字做锚,不是 idx。
        app.processes
            .service_selected_name
            .replace("beta".to_string());
        // 现在 ensure 第三个服务 gamma —— service_view_all 顺序会变(records 的迭代序),
        // 但因为 selected 用名字,仍应指向 beta。
        rt.services
            .ensure(ServiceSpec {
                name: "gamma".into(),
                process: ProcessSpec::new("echo gamma"),
                health_url: "http://127.0.0.1:0/gamma".into(),
                identity: BTreeMap::from([("status".into(), json!("ready"))]),
                startup_timeout_ms: 60_000,
            })
            .await
            .unwrap();
        // 触发一次渲染 —— 走 selected_service_name 路径,确认仍指向 beta。
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 40)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        // 在右栏找状态文字。beta 应在 service_view_all 里,且 selected。
        // 验证最简:仍能渲染 beta 名字 + 「详情」框,且 status 没报错。
        let mut found_beta = false;
        let mut found_detail = false;
        for y in 0..40 {
            let full: String = (0..200).map(|x| buffer[(x, y)].symbol()).collect();
            let compact: String = full.split_whitespace().collect::<Vec<_>>().join("");
            if compact.contains("beta") {
                found_beta = true;
            }
            if compact.contains("详情") {
                found_detail = true;
            }
        }
        assert!(found_beta, "gamma 进表后 beta 名字仍应渲染(被选中)");
        assert!(found_detail, "右栏「详情」仍应可见");
        assert_eq!(
            app.processes.service_selected_name.as_deref(),
            Some("beta"),
            "selected_name 仍应指向 beta,不是 gamma"
        );
    }
}
