//! CLI 适配层：Agent/TUI 共用进程事实与服务生命周期。
use crate::{
    tool::{BashTool, ToolContext, ToolError, ToolImpl, ToolResult, ToolSpec},
    ContentBlock,
};
use serde_json::{json, Value};
pub use smallhead_process;
use smallhead_process::service::{ServiceManager, ServiceSpec};
use smallhead_process::{
    ExecutionId, ExecutionMetadata, ExitReason, OutputStream, ProcessManager, ProcessSpec,
    ProcessState,
};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

/// 会话执行依赖，由应用创建一次并注入工具上下文。
pub struct ExecutionRuntime {
    /// OS 进程的唯一管理入口。
    pub processes: ProcessManager,
    /// 共用同一进程管理器的服务管理入口。
    pub services: ServiceManager,
}
impl std::fmt::Debug for ExecutionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionRuntime")
            .field("log_root", &self.processes.log_root())
            .finish()
    }
}
impl ExecutionRuntime {
    /// 从显式管理器构造依赖，测试可注入临时日志目录。
    pub fn new(processes: ProcessManager) -> Result<Self, ToolError> {
        let services = ServiceManager::new(processes.clone()).map_err(exec_error)?;
        Ok(Self {
            processes,
            services,
        })
    }
    /// 兼容旧 demo 的进程级默认依赖；初始化失败会向调用方报告。
    pub fn shared() -> Result<Arc<Self>, ToolError> {
        static SHARED: OnceLock<Result<Arc<ExecutionRuntime>, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                ProcessManager::new()
                    .map_err(|e| e.to_string())
                    .and_then(|p| Self::new(p).map(Arc::new).map_err(|e| e.to_string()))
            })
            .clone()
            .map_err(ToolError::Execution)
    }
}
pub(crate) fn runtime(ctx: &ToolContext) -> Result<Arc<ExecutionRuntime>, ToolError> {
    ctx.execution
        .clone()
        .map(Ok)
        .unwrap_or_else(ExecutionRuntime::shared)
}

pub(crate) struct TaskExecutions {
    pub(crate) runtime: Arc<ExecutionRuntime>,
    pub(crate) owner: String,
}

/// Stop task-owned executions and wait for actual terminal records. Services
/// have separate owners and are deliberately excluded. Every stop is attempted
/// before waiting, so one failure cannot leave the remaining children running.
pub(crate) async fn stop_and_reap_owner(
    runtime: &ExecutionRuntime,
    owner: &str,
) -> Result<(), ToolError> {
    let ids: Vec<_> = runtime
        .processes
        .list()
        .into_iter()
        .filter(|id| {
            runtime.processes.inspect(*id).is_some_and(|record| {
                record.owner.as_deref() == Some(owner)
                    && (!record.is_terminal()
                        || matches!(
                            record.state,
                            ProcessState::Lost(_) | ProcessState::CleanupFailed(_)
                        ))
            })
        })
        .collect();
    let mut errors = Vec::new();
    for id in &ids {
        if let Err(error) = runtime.processes.stop(*id).await {
            errors.push(format!("stop {id}: {error}"));
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    for id in ids {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match runtime.processes.wait_for(id, remaining).await {
            Ok(record)
                if matches!(
                    record.state,
                    ProcessState::Exited(_)
                        | ProcessState::Rejected(_)
                        | ProcessState::SpawnFailed(_)
                ) => {}
            Ok(record) => errors.push(format!(
                "execution {id} cleanup unconfirmed: {:?}",
                record.state
            )),
            Err(error) => errors.push(format!("reap {id}: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ToolError::Execution(errors.join("; ")))
    }
}
impl Drop for TaskExecutions {
    fn drop(&mut self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let rt = self.runtime.clone();
            let ids: Vec<_> = rt
                .processes
                .list()
                .into_iter()
                .filter(|id| {
                    rt.processes.inspect(*id).is_some_and(|r| {
                        r.owner.as_deref() == Some(&self.owner) && !r.is_terminal()
                    })
                })
                .collect();
            handle.spawn(async move {
                for id in ids {
                    let _ = rt.processes.stop(id).await;
                }
            });
        }
    }
}
fn exec_error(e: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(e.to_string())
}
fn result(value: Value, is_error: bool) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        is_error,
        content: vec![ContentBlock::Text {
            text: value.to_string(),
        }],
    }
}

fn parse_completion_policy(input: Option<&Value>) -> Result<Option<Value>, ToolError> {
    let Some(policy) = input else {
        return Ok(None);
    };
    if policy.is_null() {
        return Ok(None);
    }
    let Value::Object(map) = policy else {
        return Err(ToolError::InvalidInput(
            "completion_policy 必须是对象".into(),
        ));
    };
    if map.get("type").and_then(Value::as_str) != Some("exited") {
        return Err(ToolError::InvalidInput(
            "当前仅支持 completion_policy.type=exited".into(),
        ));
    }
    if let Some(codes) = map.get("allowed_exit_codes") {
        let Value::Array(codes) = codes else {
            return Err(ToolError::InvalidInput(
                "allowed_exit_codes 必须是整数数组".into(),
            ));
        };
        if codes.is_empty() || codes.len() > 32 {
            return Err(ToolError::InvalidInput(
                "allowed_exit_codes 长度必须为 1..32".into(),
            ));
        }
        for code in codes {
            let Some(code) = code.as_i64().and_then(|n| i32::try_from(n).ok()) else {
                return Err(ToolError::InvalidInput(
                    "allowed_exit_codes 必须是 i32 整数数组".into(),
                ));
            };
            if code < 0 {
                return Err(ToolError::InvalidInput(
                    "allowed_exit_codes 暂不支持负数".into(),
                ));
            }
        }
    }
    Ok(Some(policy.clone()))
}

fn execution_satisfies_policy(record: &smallhead_process::ProcessRecord) -> bool {
    let ProcessState::Exited(info) = &record.state else {
        return false;
    };
    if info.reason != ExitReason::Natural {
        return false;
    }
    let Some(policy) = &record.metadata.completion_policy else {
        return info.exit_code == Some(0);
    };
    let allowed: Vec<i32> = policy
        .get("allowed_exit_codes")
        .and_then(Value::as_array)
        .map(|codes| {
            codes
                .iter()
                .filter_map(|code| code.as_i64().and_then(|n| i32::try_from(n).ok()))
                .collect()
        })
        .filter(|codes: &Vec<i32>| !codes.is_empty())
        .unwrap_or_else(|| vec![0]);
    info.exit_code.is_some_and(|code| allowed.contains(&code))
}

struct StopOnDrop {
    manager: ProcessManager,
    id: Option<ExecutionId>,
}
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let (Some(id), Ok(handle)) = (self.id.take(), tokio::runtime::Handle::try_current()) {
            let manager = self.manager.clone();
            handle.spawn(async move {
                let _ = manager.stop(id).await;
            });
        }
    }
}

#[async_trait::async_trait]
impl ToolImpl for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "bash".into(), label: "执行 CLI".into(),
            description: "执行受管 shell 命令。返回 execution_id、真实状态与有界输出。默认等待；wait=false 立即返回，可用 process_control 查询/停止。网络请求、包管理和可能等待用户输入的命令必须设置合理的 timeout_ms，避免单个子进程无期限等待；timeout_ms 是命令级时限，整轮超时仍由宿主控制。常驻服务请用 service_ensure，任务完成或取消时 task_bound 命令会停止。输出过量触发预算停止，不能把截断内容当完整输出。".into(),
            input_schema: json!({"type":"object","properties":{
                "command":{"type":"string","description":"通过 sh -c 执行；不要用尾部 & 逃离管理。"},
                "program":{"type":"string","description":"直接启动程序，与 command 二选一；参数不会经 shell 展开。"},
                "args":{"type":"array","items":{"type":"string"}},
                "cwd":{"type":"string","description":"工作目录，相对路径基于当前任务目录。"},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":600000},
                "wait":{"type":"boolean","description":"默认 true；false 返回 ID 后由调用方等待或停止。"},
                "description":{"type":"string","description":"兼容旧字段：执行目的。优先使用 objective。"},
                "objective":{"type":"string","description":"本次执行要达成的具体目标，用于审计和后续完成条件判断。"},
                "completion_policy":{"type":"object","description":"结构化完成条件；当前支持 {\"type\":\"exited\",\"allowed_exit_codes\":[0]}，用于判断工具成功与否。"}
            },"additionalProperties":false}) }
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        if ctx.cancel {
            return Err(ToolError::Execution("当前任务已取消".into()));
        }
        let rt = runtime(ctx)?;
        let mut spec = match (input.get("command"), input.get("program")) {
            (Some(Value::String(command)), None) if input.get("args").is_none() => {
                ProcessSpec::new(command)
            }
            (None, Some(Value::String(program))) => ProcessSpec::program(
                program,
                serde_json::from_value(input.get("args").cloned().unwrap_or(json!([])))
                    .map_err(|e| ToolError::InvalidInput(format!("args: {e}")))?,
            ),
            _ => {
                return Err(ToolError::InvalidInput(
                    "提供 command，或 program + args，两者不能同时提供".into(),
                ))
            }
        };
        let base = if ctx.cwd.as_os_str().is_empty() {
            std::env::current_dir().map_err(exec_error)?
        } else {
            ctx.cwd.clone()
        };
        spec.cwd = Some(match input.get("cwd") {
            Some(Value::String(path)) => base.join(path),
            None => base,
            _ => return Err(ToolError::InvalidInput("cwd 必须为字符串".into())),
        });
        spec.env = ctx.env.clone().into_iter().collect();
        if let Some(timeout) = input.get("timeout_ms") {
            spec.timeout = Some(Duration::from_millis(
                timeout
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| ToolError::InvalidInput("timeout_ms 必须是正整数".into()))?,
            ));
        }
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let objective = input
            .get("objective")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| description.clone());
        spec.description = description.or_else(|| objective.clone());
        let completion_policy = parse_completion_policy(input.get("completion_policy"))?;
        let metadata = ExecutionMetadata {
            source: Some(
                if ctx.tool_call_id.is_some() {
                    "agent_tool"
                } else {
                    "tool"
                }
                .into(),
            ),
            tool_call_id: ctx.tool_call_id.clone(),
            objective,
            completion_policy,
        };
        let wait = match input.get("wait") {
            None => true,
            Some(Value::Bool(v)) => *v,
            _ => return Err(ToolError::InvalidInput("wait 必须是布尔值".into())),
        };
        let id = rt
            .processes
            .start_with_metadata(spec, ctx.owner.clone(), metadata)
            .await
            .map_err(exec_error)?;
        crate::diagnostics::emit(
            "process",
            "execution_registered",
            json!({"execution_id":id,"owner":ctx.owner}),
        );
        if !wait {
            let record = rt.processes.inspect(id).unwrap();
            let is_error = record.is_terminal() && !execution_satisfies_policy(&record);
            return Ok(result(
                json!({"execution":record,"next_actions":["inspect","read_output","stop"]}),
                is_error,
            ));
        }
        let mut guard = StopOnDrop {
            manager: rt.processes.clone(),
            id: Some(id),
        };
        let wait_started = Instant::now();
        let mut last_wait_event = Instant::now();
        let record = loop {
            // 等待窗口不改变命令执行时限，期间调用者可取消 future。
            if let Some(record) = rt.processes.inspect(id) {
                if record.is_terminal() {
                    break record;
                }
            } else {
                return Err(ToolError::Execution("执行记录已被清理".into()));
            }
            if last_wait_event.elapsed() >= Duration::from_secs(5) {
                crate::diagnostics::emit(
                    "process",
                    "execution_waiting",
                    json!({
                        "execution_id": id,
                        "elapsed_secs": wait_started.elapsed().as_secs(),
                        "state": "running",
                    }),
                );
                last_wait_event = Instant::now();
            }
            let _ = rt.processes.wait_for(id, Duration::from_secs(1)).await;
        };
        guard.id = None;
        let mut output = json!({"execution":record,"next_actions":["read_output","inspect"]});
        for (name, stream) in [
            ("stdout", OutputStream::Stdout),
            ("stderr", OutputStream::Stderr),
        ] {
            if let Ok(page) = rt.processes.read_output(id, stream, 0, 4096).await {
                output[name] = json!({"text":page.text,"next_offset":page.next_offset,"stored_bytes":page.stored_bytes,"truncated":!page.eof,"output_limited":page.output_limited});
                if !page.eof {
                    let tail = rt
                        .processes
                        .read_output(
                            id,
                            stream,
                            page.stored_bytes.saturating_sub(2048).max(page.next_offset),
                            2048,
                        )
                        .await
                        .map_err(exec_error)?;
                    output[name]["tail"] = json!(tail.text);
                }
            }
        }
        let success = execution_satisfies_policy(&record);
        output["completion"] = json!({
            "satisfied": success,
            "policy": record.metadata.completion_policy.clone(),
        });
        output["summary"] = json!(match &record.state {
            ProcessState::Rejected(reason) => format!("BLOCKED: {reason}"),
            ProcessState::Exited(info) if info.reason == ExitReason::Timeout =>
                "timeout：已停止执行".into(),
            ProcessState::Exited(info) =>
                format!("exit code {:?}; reason {:?}", info.exit_code, info.reason),
            other => format!("{other:?}"),
        });
        Ok(result(output, !success))
    }
}

/// 查询/控制与执行共用同一状态源。
#[derive(Clone, Debug, Default, Hash, Eq, PartialEq)]
pub struct ProcessTool;
#[async_trait::async_trait]
impl ToolImpl for ProcessTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name:"process_control".into(), label:"进程与服务管理".into(),
            description:"查询或停止执行，分页读取输出。service_ensure 支持 laya/qwen 预设，也可传 service 配置；只验证就绪，服务持续运行。外部服务不允许停止，不按端口杀进程。service_forget 只清除停止、失败或身份未知的登记，不发送信号。service_update_description 写入 Agent 视角的说明（用途、身份、原因等），受 ServiceManager::MAX_DESCRIPTION_BYTES 字节上限约束。purge 显式删除已完成执行的日志；仅在用户要求清理时调用。".into(),
            input_schema:json!({"type":"object","properties":{
                "action":{"type":"string","enum":["list","inspect","stop","read_output","purge","service_list","service_ensure","service_inspect","service_stop","service_forget","service_update_description"]},
                "execution_id":{"type":"string"},"stream":{"type":"string","enum":["stdout","stderr","events"]},
                "offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":65536},
                "name":{"type":"string"},"description":{"type":"string","maxLength":4096},
                "service":{"type":"object","description":"自定义 ServiceSpec：name/process/startup_timeout_ms；health_url 和 identity 可同时省略，此时只确认进程启动，不检查可访问性或身份。若配置健康检查，仅支持本机 HTTP + 非空 identity。"}
            },"required":["action"],"additionalProperties":false}) }
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        let rt = runtime(ctx)?;
        let id = || {
            input["execution_id"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("缺 execution_id".into()))?
                .parse::<ExecutionId>()
                .map_err(|e| ToolError::InvalidInput(e.to_string()))
        };
        let name = || {
            input["name"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("缺 name".into()))
        };
        let value = match input["action"].as_str() {
            Some("list") => {
                let mut ids = rt.processes.list();
                ids.sort();
                let offset = input["offset"].as_u64().unwrap_or(0).min(ids.len() as u64) as usize;
                let limit = input["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
                json!({"executions":ids.iter().skip(offset).take(limit).filter_map(|id|rt.processes.inspect(*id)).collect::<Vec<_>>(),"total":ids.len(),"next_offset":(offset+limit).min(ids.len())})
            }
            Some("inspect") => json!(rt
                .processes
                .inspect(id()?)
                .ok_or_else(|| ToolError::InvalidInput("未知 execution_id".into()))?),
            Some("stop") => {
                let id = id()?;
                rt.processes.stop(id).await.map_err(exec_error)?;
                json!(rt
                    .processes
                    .wait_for(id, Duration::from_secs(6))
                    .await
                    .map_err(exec_error)?)
            }
            Some("purge") => {
                rt.processes.purge(id()?).map_err(exec_error)?;
                json!({"purged":true})
            }
            Some("read_output") => {
                let stream = match input["stream"].as_str().unwrap_or("stdout") {
                    "stdout" => OutputStream::Stdout,
                    "stderr" => OutputStream::Stderr,
                    "events" => OutputStream::Events,
                    _ => return Err(ToolError::InvalidInput("未知输出流".into())),
                };
                json!(rt
                    .processes
                    .read_output(
                        id()?,
                        stream,
                        input["offset"].as_u64().unwrap_or(0),
                        input["limit"].as_u64().unwrap_or(4096).min(65537) as usize
                    )
                    .await
                    .map_err(exec_error)?)
            }
            Some("service_list") => json!(rt.services.list()),
            Some("service_inspect") => {
                let (observation, op) = rt.services.inspect(name()?).await.map_err(exec_error)?;
                json!({"observation": observation, "operation_id": op})
            }
            Some("service_stop") => {
                let (record, op) = rt.services.stop(name()?).await.map_err(exec_error)?;
                json!({"record": record, "operation_id": op})
            }
            Some("service_forget") => {
                let (record, op) = rt.services.forget(name()?).map_err(exec_error)?;
                json!({"record": record, "operation_id": op})
            }
            Some("service_ensure") => {
                let spec = if let Some(spec) = input.get("service") {
                    serde_json::from_value::<ServiceSpec>(spec.clone())
                        .map_err(|e| ToolError::InvalidInput(e.to_string()))?
                } else {
                    let service_name = name()?;
                    match rt
                        .services
                        .list()
                        .into_iter()
                        .find(|r| r.spec.name == service_name)
                    {
                        Some(record) => record.spec,
                        None => service_preset(service_name, &ctx.cwd)?,
                    }
                };
                let (record, op) = rt.services.ensure(spec).await.map_err(exec_error)?;
                json!({"record": record, "operation_id": op})
            }
            Some("service_update_description") => {
                let name = name()?;
                let description = input["description"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidInput("缺 description".into()))?
                    .to_string();
                rt.services
                    .update_description(name, description)
                    .map_err(exec_error)?;
                json!({"name": name, "updated": true})
            }
            _ => return Err(ToolError::InvalidInput("未知进程管理 action".into())),
        };
        crate::diagnostics::emit(
            "process",
            "control",
            json!({"action":input["action"],"result":value}),
        );
        let failed = value["state"]
            .as_str()
            .is_some_and(|s| matches!(s, "Failed" | "Unknown" | "Degraded"))
            || value["state"].get("CleanupFailed").is_some()
            || value["state"].get("Lost").is_some();
        Ok(result(value, failed))
    }
}

/// 单个服务预设的元数据。Agent 用 `service_preset` 拿全量配置；UI 也用它做列表展示。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServicePreset {
    /// 预设名（== `ServiceSpec.name`）。
    pub name: &'static str,
    /// 启动脚本相对 `cwd` 的路径。
    pub script: &'static str,
    /// 启动后健康检查的 loopback HTTP 端口。
    pub port: u16,
    /// 透传给 `sh` 的环境变量白名单（仅在环境变量存在时注入）。
    pub env_passthrough: &'static [&'static str],
    /// 健康协议要求的 JSON 字段。
    pub identity: serde_json::Value,
    /// 启动就绪最大等待毫秒。
    pub startup_timeout_ms: u64,
}

/// 全部已知服务预设。新增在 [`all_service_presets`] 数组内追加；
/// `service_preset` 函数会自动按名字查找。
pub fn all_service_presets() -> Vec<ServicePreset> {
    vec![
        ServicePreset {
            name: "laya",
            script: "experiments/laya/start.sh",
            port: 3031,
            env_passthrough: &[
                "HOME",
                "TMPDIR",
                "LAYA_PYTHON",
                "LAYA_MODEL_DIR",
                "BIGHEAD_DIR",
            ],
            identity: json!({"status":"ready","protocol_version":1,"question_types":["choice","score","noul"]}),
            startup_timeout_ms: 120_000,
        },
        ServicePreset {
            name: "qwen",
            script: "experiments/qwen-reranker/start.sh",
            port: 3032,
            env_passthrough: &["HOME", "TMPDIR", "QWEN_PYTHON"],
            identity: json!({"status":"ready","model":"Qwen/Qwen3-Reranker-0.6B"}),
            startup_timeout_ms: 120_000,
        },
    ]
}

/// 列出所有预设名（保持插入顺序）。
pub fn service_preset_names() -> Vec<&'static str> {
    all_service_presets().iter().map(|p| p.name).collect()
}

/// 当前项目的服务预设。不会自动下载模型或安装依赖。
///
/// 输入 `name` 必须是预设名之一；`cwd` 是脚本相对基准路径。
pub fn service_preset(name: &str, cwd: &std::path::Path) -> Result<ServiceSpec, ToolError> {
    let preset = all_service_presets()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            let known: Vec<&str> = service_preset_names();
            ToolError::InvalidInput(format!(
                "预设仅支持 {}，自定义请传 service",
                known.join("/")
            ))
        })?;
    let mut process = ProcessSpec::program("sh", vec![preset.script.into()]);
    process.cwd = Some(if cwd.as_os_str().is_empty() {
        std::env::current_dir().map_err(exec_error)?
    } else {
        cwd.into()
    });
    for key in preset.env_passthrough {
        if let Ok(value) = std::env::var(key) {
            process.env.insert((*key).into(), value);
        }
    }
    Ok(ServiceSpec {
        name: preset.name.into(),
        process,
        health_url: format!("http://127.0.0.1:{}/health", preset.port),
        identity: serde_json::from_value(preset.identity.clone()).unwrap(),
        startup_timeout_ms: preset.startup_timeout_ms,
    })
}

/// 对已注入管理器、使用标准本机 endpoint 的快速后端按需启动服务。
/// 自定义 endpoint 保持由调用方管理；用户停止过的服务不会自动复活。
pub(crate) async fn ensure_fast_service(
    ctx: &ToolContext,
    name: &str,
    endpoint: &str,
) -> Result<(), ToolError> {
    let Some(rt) = &ctx.execution else {
        return Ok(());
    };
    let spec = service_preset(name, &ctx.cwd)?;
    if format!("{}/health", endpoint.trim_end_matches('/')) != spec.health_url {
        return Ok(());
    }
    if rt
        .services
        .list()
        .iter()
        .any(|r| r.spec.name == name && !r.desired_running)
    {
        return Err(ToolError::Execution(format!(
            "服务 {name} 已停止；需要显式 service_ensure 才能重新启动"
        )));
    }
    let (record, _op) = rt.services.ensure(spec).await.map_err(exec_error)?;
    crate::diagnostics::emit("process", "service_ready", json!(&record));
    if record.state != smallhead_process::service::ServiceState::Ready {
        return Err(ToolError::Execution(format!(
            "服务未就绪：{}",
            record.detail
        )));
    }
    Ok(())
}
