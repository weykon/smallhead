//! Built-in tool dispatch and host-provided tool registration.
//!
//! Built-in tools use explicit enum variants. Hosts register SDK-backed
//! integrations through [`ExternalTool`] without adding SDK dependencies here.
//! Both paths share call identity, execution context, and result projection.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::message::ContentBlock;

pub use crate::plan::UpdatePlanTool;
pub use crate::spec_review::PublishSpecTool;

/// Tool 错误。
#[derive(Debug, Error)]
pub enum ToolError {
    /// 输入校验失败。
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// 执行失败。
    #[error("execution failed: {0}")]
    Execution(String),
    /// 内部协议不变量被破坏，必须终止当前运行。
    #[error("tool invariant violated: {0}")]
    Invariant(String),
    /// 工具未注册。
    #[error("tool not registered: {0}")]
    NotRegistered(String),
}

/// Tool 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// tool_call_id。
    pub tool_call_id: String,
    /// 是否错误。
    pub is_error: bool,
    /// 输出内容（多模块）。
    pub content: Vec<ContentBlock>,
}

/// Tool result projection metadata recorded when a large result is shortened
/// before it is added back to model context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultProjection {
    /// Whether the result was projected.
    pub projected: bool,
    /// Original textual payload size in bytes.
    pub original_bytes: usize,
    /// Projected payload size in bytes.
    pub visible_bytes: usize,
    /// Projection strategy name.
    pub strategy: String,
    /// Artifact reference for the full payload, when storage succeeded.
    pub artifact_ref: Option<String>,
    /// Local path for diagnostics. This is not sent to the model directly.
    pub artifact_path: Option<std::path::PathBuf>,
    /// Storage failure, if the payload could not be persisted.
    pub storage_error: Option<String>,
}

/// Host-assigned origin of initial input and steering, never inferred from history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    /// Input from the host's user ingress.
    #[default]
    User,
    /// Input from another peer.
    Peer,
    /// Instructions to a delegated agent.
    Delegate,
}

/// Latest input accepted by the driver, independent of provider/Lens projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedInput {
    /// Fresh identity for each accepted input, including repeated text.
    pub id: String,
    /// Text blocks in original order, separated by newlines; non-text is omitted.
    pub text: String,
    /// Origin assigned by the host, not by the model.
    pub source: InputSource,
}

/// Tool 执行上下文（MVP 最小化）。
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// Host-bound origin for this loop's initial input and steering.
    pub input_source: InputSource,
    /// Latest accepted input only, not a ledger of all turn inputs. A batch of
    /// steering leaves its last record here; non-user turn starts clear it.
    pub accepted_input: Option<AcceptedInput>,
    /// Shared Monad dispatch runtime for this task tree.
    pub monad_runtime: Option<std::sync::Arc<crate::monad::MonadRuntime>>,
    /// Collaboration state shared by this task tree.
    pub monad_collaboration: std::sync::Arc<crate::monad::CollaborationHub>,
    /// Host-bound collaboration endpoint for a child loop.
    pub monad_channel: Option<crate::monad::ChildChannel>,
    /// Cancellation of this task and its descendants.
    pub cancellation: tokio_util::sync::CancellationToken,
    /// 会话共享的进程及服务管理器。
    pub execution: Option<std::sync::Arc<crate::execution::ExecutionRuntime>>,
    /// 用户选择请求 broker；TUI 注入后工具可暂停等待用户点击。
    pub choices: Option<std::sync::Arc<crate::choice::ChoiceBroker>>,
    /// 当前工具调用 ID。由 [`ToolKind::execute`] 注入，直接调用工具时可为空。
    pub tool_call_id: Option<String>,
    /// 当前任务的身份，供定向取消与审计关联。
    pub owner: Option<String>,
    /// 当前 cwd。
    pub cwd: std::path::PathBuf,
    /// Artifact root for oversized tool result storage. Defaults to
    /// `~/.smallhead/artifacts` when unset.
    pub artifact_root: Option<std::path::PathBuf>,
    /// 环境变量（白名单）。
    pub env: HashMap<String, String>,
    /// 取消 token（MVP 简化版）。
    pub cancel: bool,
}

/// Tool 规格（给 LLM 看的 schema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 工具名。
    pub name: String,
    /// UI 显示用 label。
    pub label: String,
    /// 模型选择依据的描述。
    pub description: String,
    /// 输入 JSON schema。
    pub input_schema: serde_json::Value,
}

/// Tool trait（供 enum dispatch 内部使用）。
#[async_trait]
pub trait ToolImpl: Send + Sync {
    /// 工具规格。
    fn spec(&self) -> ToolSpec;
    /// 执行。
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

/// Host-owned tool implementation, registered without a core dependency on its SDK.
/// The advertised specification is frozen at registration; call-time discovery
/// belongs to the implementation (for example, an MCP connection's tool list).
#[derive(Clone)]
pub struct ExternalTool {
    implementation: Arc<dyn ToolImpl>,
    specification: Arc<ToolSpec>,
}

impl ExternalTool {
    /// Register one shared implementation and capture its specification.
    pub fn new(implementation: Arc<dyn ToolImpl>) -> Self {
        let specification = Arc::new(implementation.spec());
        Self {
            implementation,
            specification,
        }
    }
}

impl std::fmt::Debug for ExternalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalTool")
            .field("name", &self.specification.name)
            .finish()
    }
}

impl PartialEq for ExternalTool {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.implementation, &other.implementation)
    }
}

impl Eq for ExternalTool {}

impl Hash for ExternalTool {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.implementation).cast::<()>().hash(state);
    }
}

#[async_trait]
impl ToolImpl for ExternalTool {
    fn spec(&self) -> ToolSpec {
        self.specification.as_ref().clone()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.implementation.execute(input, ctx).await
    }
}

/// ToolKind enum——所有工具的统一 dispatch。
///
/// MVP demo 只包含 `Echo`。其他变体（`Bash / Read / ...`）会随后续版本添加。
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum ToolKind {
    /// Host-registered tool backed by an independently owned integration.
    External(ExternalTool),
    /// Publish a revisioned Markdown specification for UI review.
    UpdateSpec(PublishSpecTool),
    /// Replace the current task's progress plan.
    UpdatePlan(UpdatePlanTool),
    /// 请求用户在 TUI 卡片里选择一个选项。
    Choice(ChoiceTool),
    /// 查询、停止进程以及管理本机服务。
    ProcessControl(crate::execution::ProcessTool),
    /// Sequential business discovery/dispatch bridge for the Monad demo.
    MonadBridge(crate::monad_bridge::MonadBridgeTool),
    /// 快速模型的类型决策与候选排序。
    FastDecision(crate::fast_decision::FastDecisionTool),
    /// Echo 工具——把输入的 text 字段原样返回。
    Echo(EchoTool),
    /// 算子工具：加 / 减 / 乘 / 除。
    Add(AddTool),
    /// 减法算子（a - b）。
    Sub(SubTool),
    /// 乘法算子（a * b）。
    Mul(MulTool),
    /// 除法算子（a / b, b ≠ 0）。
    Div(DivTool),
    /// 计算业务统一入口（meta-dispatcher）。
    ///
    /// 接受表达式字符串（如 "1+2"），解析后返回 hint 引导 LLM
    /// 在下一轮调用具体算子（add/sub/mul/div）。这是"事务统一入口
    /// → 细粒度方法"模式的最小演示。
    Calculate(CalculateTool),
    /// Bash 工具：执行 shell 命令（YOLO 模式，无沙箱）。
    Bash(BashTool),
    /// Read 工具：分页读文件。
    Read(ReadTool),
    /// 读取由工具结果投影保存的大输出 artifact。
    ArtifactRead(ArtifactReadTool),
    /// Edit 工具：搜索-替换编辑。
    Edit(EditTool),
    /// Write 工具：写入文件（带 .bak 备份）。
    Write(WriteTool),
    /// 统一多源 web 搜索(web / wiki / code / news kind,CJK-aware chain)。
    ///
    /// 实现逻辑(health-aware fallback chain + 9 个 provider)在
    /// `smallhead-tools` 里;本类型系 marker,`execute` 调用
    /// `set_web_search_handler` 注册嘅 handler。
    WebSearch(WebSearchTool),
    /// Agent-Reach CLI channel 工具(search / webpage / rss / youtube / bili 等)。
    ///
    /// 实现逻辑在 `smallhead-tools` 里;本类型系 marker,`execute` 调用
    /// `set_agent_reach_handler` 注册嘅 handler。
    AgentReach(AgentReachTool),
}

impl ToolKind {
    /// 工具名。
    pub fn name(&self) -> &str {
        match self {
            ToolKind::External(t) => &t.specification.name,
            ToolKind::UpdateSpec(_) => "update_spec",
            ToolKind::UpdatePlan(_) => "update_plan",
            ToolKind::Choice(_) => "ask_user_choice",
            ToolKind::ProcessControl(_) => "process_control",
            ToolKind::MonadBridge(_) => "monad_bridge",
            ToolKind::FastDecision(_) => "fast_decision",
            ToolKind::Echo(_) => "echo",
            ToolKind::Add(_) => "add",
            ToolKind::Sub(_) => "sub",
            ToolKind::Mul(_) => "mul",
            ToolKind::Div(_) => "div",
            ToolKind::Calculate(_) => "calculate",
            ToolKind::Bash(_) => "bash",
            ToolKind::Read(_) => "read",
            ToolKind::ArtifactRead(_) => "artifact_read",
            ToolKind::Edit(_) => "edit",
            ToolKind::Write(_) => "write",
            ToolKind::WebSearch(_) => "web_search",
            ToolKind::AgentReach(_) => "agent_reach",
        }
    }

    /// 工具规格。
    pub fn spec(&self) -> ToolSpec {
        match self {
            ToolKind::External(t) => t.spec(),
            ToolKind::UpdateSpec(t) => t.spec(),
            ToolKind::UpdatePlan(t) => t.spec(),
            ToolKind::Choice(t) => t.spec(),
            ToolKind::ProcessControl(t) => t.spec(),
            ToolKind::MonadBridge(t) => t.spec(),
            ToolKind::FastDecision(t) => t.spec(),
            ToolKind::Echo(t) => t.spec(),
            ToolKind::Add(t) => t.spec(),
            ToolKind::Sub(t) => t.spec(),
            ToolKind::Mul(t) => t.spec(),
            ToolKind::Div(t) => t.spec(),
            ToolKind::Calculate(t) => t.spec(),
            ToolKind::Bash(t) => t.spec(),
            ToolKind::Read(t) => t.spec(),
            ToolKind::ArtifactRead(t) => t.spec(),
            ToolKind::Edit(t) => t.spec(),
            ToolKind::Write(t) => t.spec(),
            ToolKind::WebSearch(t) => t.spec(),
            ToolKind::AgentReach(t) => t.spec(),
        }
    }

    /// 执行。
    pub async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let mut call_ctx = ctx.clone();
        call_ctx.tool_call_id = Some(tool_call_id.to_string());
        let ctx = &call_ctx;
        let mut result = match self {
            ToolKind::External(t) => t.execute(input, ctx).await?,
            ToolKind::UpdateSpec(t) => t.execute(input, ctx).await?,
            ToolKind::UpdatePlan(t) => t.execute(input, ctx).await?,
            ToolKind::Choice(t) => t.execute(input, ctx).await?,
            ToolKind::ProcessControl(t) => t.execute(input, ctx).await?,
            ToolKind::MonadBridge(t) => t.execute(input, ctx).await?,
            ToolKind::FastDecision(t) => t.execute(input, ctx).await?,
            ToolKind::Echo(t) => t.execute(input, ctx).await?,
            ToolKind::Add(t) => t.execute(input, ctx).await?,
            ToolKind::Sub(t) => t.execute(input, ctx).await?,
            ToolKind::Mul(t) => t.execute(input, ctx).await?,
            ToolKind::Div(t) => t.execute(input, ctx).await?,
            ToolKind::Calculate(t) => t.execute(input, ctx).await?,
            ToolKind::Bash(t) => t.execute(input, ctx).await?,
            ToolKind::Read(t) => t.execute(input, ctx).await?,
            ToolKind::ArtifactRead(t) => t.execute(input, ctx).await?,
            ToolKind::Edit(t) => t.execute(input, ctx).await?,
            ToolKind::Write(t) => t.execute(input, ctx).await?,
            ToolKind::WebSearch(t) => t.execute(input, ctx).await?,
            ToolKind::AgentReach(t) => t.execute(input, ctx).await?,
        };
        result.tool_call_id = tool_call_id.to_string();
        Ok(result)
    }
}

// ============================ EchoTool ============================

/// Echo 工具：把 `text` 字段原样回显。
///
/// 设计目标：
/// - **指挥式 description** 引导 LLM 传 `text` 字段（不传空对象）
/// - **schema 标记 text 为 required**，让模型 schema-层面就知道必传
/// - **代码侧保持宽容**——若 LLM 还是传了 `{}`（不传 text），
///   把整个 JSON 序列化返回，不让 agent loop 因为 schema 细节崩溃
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct EchoTool;

#[async_trait]
impl ToolImpl for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            label: "Echo".into(),
            description:
                "将用户指定的原文作为工具结果回显。必须把用户提到的词语或句子填入 text，禁止用空对象 {} 调用。例如用户说“请说 你好”，调用 echo({text: \"你好\"})。保留用户原话，不要自行改写。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "用户要求回显的原词或句子。从请求中提取，必须填写，不能是空字符串。例如：hello、world、你好。"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        // 优先取 `text` 字段；缺省时把整个 JSON 序列化回去（宽容 fallback）
        let echoed = if let Some(text) = input.get("text").and_then(|v| v.as_str()) {
            text.to_string()
        } else {
            serde_json::to_string(&input).unwrap_or_else(|_| "<invalid json>".into())
        };
        Ok(ToolResult {
            tool_call_id: String::new(), // 由 ToolKind::execute 填充
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[echo] {}", echoed),
            }],
        })
    }
}

/// Choice 工具：请求用户在前端卡片里选择一个选项。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct ChoiceTool;

#[derive(Debug, Deserialize)]
struct ChoiceToolInput {
    title: String,
    body: String,
    options: Vec<crate::choice::ChoiceOption>,
    #[serde(default)]
    default_option: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl ToolImpl for ChoiceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user_choice".into(),
            label: "请求用户选择".into(),
            description: "当继续执行需要用户明确选择时调用。以结构化选项卡片暂停当前任务，等待用户选择后返回 selected_option_id 和 intent。界面也允许用户直接输入补充文本；这会返回 selected_option_id=custom_input，intent={type:custom,kind:free_text,payload:{text}}。不要用自由文本假装用户已经选择。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type":"string","description":"卡片标题，简短说明正在决定什么。"},
                    "body": {"type":"string","description":"选择背景和影响，不要写长篇说明。"},
                    "options": {
                        "type":"array",
                        "minItems":1,
                        "maxItems":4,
                        "items":{
                            "type":"object",
                            "properties":{
                                "id":{"type":"string"},
                                "label":{"type":"string"},
                                "description":{"type":"string"},
                                "intent":{"type":"object","description":"结构化含义，默认 {type:'continue'}。"}
                            },
                            "required":["id","label"],
                            "additionalProperties":false
                        }
                    },
                    "default_option":{"type":"string"},
                    "timeout_ms":{"type":"integer","minimum":100,"maximum":600000}
                },
                "required":["title","body","options"],
                "additionalProperties":false
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let input: ChoiceToolInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let broker = ctx
            .choices
            .clone()
            .ok_or_else(|| ToolError::Execution("当前界面不支持用户选择卡片".into()))?;
        let request = crate::choice::ChoiceRequest {
            id: String::new(),
            owner: ctx.owner.clone(),
            title: input.title,
            body: input.body,
            options: input.options,
            default_option: input.default_option,
            timeout_ms: input.timeout_ms,
            source: Some("agent_tool".into()),
            tool_call_id: ctx.tool_call_id.clone(),
            created_at_ms: 0,
        };
        crate::diagnostics::emit(
            "choice",
            "choice_waiting",
            serde_json::json!({
                "owner": &request.owner,
                "title": &request.title,
                "option_count": request.options.len(),
                "timeout_ms": &request.timeout_ms,
            }),
        );
        let response = match broker.ask(request).await {
            Ok(response) => {
                crate::diagnostics::emit(
                    "choice",
                    "choice_answered",
                    serde_json::json!({
                        "choice_id": &response.choice_id,
                        "selected_option_id": &response.selected_option_id,
                        "selected_label": &response.selected.label,
                    }),
                );
                response
            }
            Err(error) => {
                crate::diagnostics::emit(
                    "choice",
                    "choice_failed",
                    serde_json::json!({"error":error.to_string()}),
                );
                return Err(ToolError::Execution(error.to_string()));
            }
        };
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: serde_json::json!({
                    "choice": response,
                    "next_actions": ["continue_with_selected_option"],
                })
                .to_string(),
            }],
        })
    }
}

// ============================ 算子工具 (Add/Sub/Mul/Div) ============================
//
// 设计：4 个细粒度算子共享同一套 helper（extract_numbers），分别独立实现以保证
// 每个工具的 spec.description 是独立的强信号文案 —— LLM 看的是 spec，不是代码。
//
// 配合 `CalculateTool`（见下）：用户先调 calculate，calculate 返回 hint 告诉
// LLM 用哪一个算子 + 参数。LLM 在下一轮调具体算子拿到结果。
//
// 为什么不直接让 calculate 内部完成计算？
//   → 因为这是"业务统一入口 → 细粒度方法"的标准模式演示。现实里 calculate
//     可能是网关、参数校验层；具体算子是底层服务。LLM 的两轮 dispatch 走的是
//     真实的 production 路由。

/// 抽取 (a, b) 两个数字参数。
fn extract_ab(input: &serde_json::Value) -> Result<(f64, f64), ToolError> {
    let a = input
        .get("a")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ToolError::InvalidInput("missing or non-numeric 'a'".into()))?;
    let b = input
        .get("b")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ToolError::InvalidInput("missing or non-numeric 'b'".into()))?;
    Ok((a, b))
}

/// 把 f64 格式化为不带小数点的整数（如果是整数）或带小数点的浮点数。
fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Add 算子。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct AddTool;

#[async_trait]
impl ToolImpl for AddTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "add".into(),
            label: "Add".into(),
            description:
                "将两个数相加并返回和。calculate 提示使用 add 时，或用户直接要求加法时调用。参数 a、b 为两个加数。例如 add({a: 1, b: 2}) 返回 3。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number", "description": "第一个加数"},
                    "b": {"type": "number", "description": "第二个加数"}
                },
                "required": ["a", "b"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let (a, b) = extract_ab(&input)?;
        let sum = a + b;
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[add] {} + {} = {}", fmt_num(a), fmt_num(b), fmt_num(sum)),
            }],
        })
    }
}

/// Sub 算子。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct SubTool;

#[async_trait]
impl ToolImpl for SubTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sub".into(),
            label: "Subtract".into(),
            description:
                "从 a 中减去 b，返回差 a-b。calculate 提示使用 sub 时，或用户直接要求减法时调用。a 为被减数，b 为减数。例如 sub({a: 5, b: 3}) 返回 2。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number", "description": "被减数"},
                    "b": {"type": "number", "description": "减数"}
                },
                "required": ["a", "b"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let (a, b) = extract_ab(&input)?;
        let diff = a - b;
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[sub] {} - {} = {}", fmt_num(a), fmt_num(b), fmt_num(diff)),
            }],
        })
    }
}

/// Mul 算子。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct MulTool;

#[async_trait]
impl ToolImpl for MulTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mul".into(),
            label: "Multiply".into(),
            description:
                "将两个数相乘并返回积。calculate 提示使用 mul 时，或用户直接要求乘法时调用。a、b 为两个因数。例如 mul({a: 4, b: 3}) 返回 12。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number", "description": "第一个因数"},
                    "b": {"type": "number", "description": "第二个因数"}
                },
                "required": ["a", "b"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let (a, b) = extract_ab(&input)?;
        let prod = a * b;
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[mul] {} * {} = {}", fmt_num(a), fmt_num(b), fmt_num(prod)),
            }],
        })
    }
}

/// Div 算子。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct DivTool;

#[async_trait]
impl ToolImpl for DivTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "div".into(),
            label: "Divide".into(),
            description:
                "用 a 除以 b，返回商 a/b。calculate 提示使用 div 时，或用户直接要求除法时调用。a 为被除数，b 为除数且不能为零。例如 div({a: 10, b: 2}) 返回 5；b 为零时返回错误。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number", "description": "被除数"},
                    "b": {"type": "number", "description": "除数，不能为零"}
                },
                "required": ["a", "b"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let (a, b) = extract_ab(&input)?;
        if b == 0.0 {
            return Err(ToolError::Execution("division by zero".into()));
        }
        let quot = a / b;
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[div] {} / {} = {}", fmt_num(a), fmt_num(b), fmt_num(quot)),
            }],
        })
    }
}

// ============================ CalculateTool (统一入口) ============================
//
// 这是"业务统一入口"演示。calculate 本身不做计算，而是解析表达式，告诉 LLM
// 下一步该调哪个细粒度算子。LLM 看完 hint 后在下一轮调具体算子。
//
// 真实生产里 calculate 可能是：
//   - 网关服务（路由到正确的微服务）
//   - 参数校验层（先验证输入格式再转发）
//   - 业务编排层（决定调用顺序、是否需要多个算子）

/// 计算业务统一入口：解析表达式，返回 "调哪个算子 + 参数" 的 hint。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct CalculateTool;

#[async_trait]
impl ToolImpl for CalculateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "calculate".into(),
            label: "Calculate".into(),
            description:
                "算术计算的统一业务入口。接受只有两个操作数的表达式，例如 1+2、10-3、4*5、20/4。必须遵循：1. 用户请求算术计算时，首先调用 calculate。2. 读取返回的 hint，确定具体算子及 a、b。3. 下一轮调用对应的 add / sub / mul / div。4. 根据算子实际返回的结果回复。不要自行心算替代工具。例如第一轮 calculate({expression: \"1+2\"})，下一轮 add({a: 1, b: 2})，最终使用返回值 3。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "只有两个操作数的简单表达式，例如 1+2、10-3、4*5、20/4。"
                    }
                },
                "required": ["expression"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let expr = input
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'expression'".into()))?
            .trim();

        // 极简 parser：找第一个 + - * /（优先级 * / > + -，但 MVP 不做优先级，
        // 严格两 operand；空格容忍）。
        let ops: &[(&str, &str)] = &[("+", "add"), ("-", "sub"), ("*", "mul"), ("/", "div")];
        for (op, tool) in ops {
            if let Some((a_str, b_str)) = expr.split_once(op) {
                let a_str = a_str.trim();
                let b_str = b_str.trim();
                // 校验两边都是数字
                let a: f64 = a_str.parse().map_err(|_| {
                    ToolError::InvalidInput(format!("left operand '{a_str}' not numeric"))
                })?;
                let b: f64 = b_str.parse().map_err(|_| {
                    ToolError::InvalidInput(format!("right operand '{b_str}' not numeric"))
                })?;
                // 返回 hint：告诉 LLM 下一步调哪个算子
                let hint = format!(
                    "[calculate] 表达式 '{expr}' 已解析，运算符为 '{op}'。\n\
                     → 下一步：调用 '{tool}' 工具，参数为 {{a: {a}, b: {b}}}.\n\
                     → 然后将 '{tool}' 返回的结果作为最终答案告知用户。"
                );
                return Ok(ToolResult {
                    tool_call_id: String::new(),
                    is_error: false,
                    content: vec![ContentBlock::Text { text: hint }],
                });
            }
        }
        Err(ToolError::InvalidInput(format!(
            "cannot parse '{expr}' as a simple two-operand expression. \
             Expected formats: '1+2', '10-3', '4*5', '20/4'"
        )))
    }
}

// ============================ Bash/Read/Edit/Write (YOLO 真工具) ============================
//
// 设计要点：
// - YOLO 模式：不接 permission gate（Phase 2 才接 smallhead-permission）
// - 路径白名单：soft check，只防 escape（绝对路径 / 开头 ..）
// - 输出截断：统一 8000 chars + 末尾标记，便于 LLM 处理
// - Write/Edit 自动 .bak 备份，简单 atomic write（写 tmp + rename）
//
// 这套是 MVP，目的是让 agent loop 能动起来跑通 e2e。

/// 输出截断阈值（chars）。超过这个长度会被截断并附 `<output_truncated/>` 标记。
const OUTPUT_TRUNCATE_CHARS: usize = 8000;
/// Read 单次最多返回的行数（防止单次返回巨大文件）。
const READ_MAX_LIMIT: usize = 8000;

/// 截断字符串到指定字符数（按 char 边界切，避免切坏 UTF-8）。
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let mut out = String::with_capacity(max);
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(c);
    }
    out.push_str("<output_truncated/>");
    (out, true)
}

/// Bash 工具：执行 shell 命令并捕获 stdout+stderr。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct BashTool;

/// Read 工具：分页读文件。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct ReadTool;

/// ArtifactRead 工具：分页读取大工具结果 artifact。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct ArtifactReadTool;

#[async_trait]
impl ToolImpl for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            label: "Read".into(),
            description: "按行分页读取文件。offset 从 1 开始。limit 默认 200 行，最大 8000 行。输出超过 8000 个字符时截断并标记 <output_truncated/>。还有后续行时返回 <next_offset>N</next_offset>，N 为下一页起始行。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径。相对路径基于 Agent 当前目录解析。拒绝绝对路径和通过 .. 越界的路径；这是 YOLO 模式下的软检查。"
                    },
                    "offset": {
                        "type": "number",
                        "description": "起始行号，从 1 开始，默认 1。"
                    },
                    "limit": {
                        "type": "number",
                        "description": "最多返回的行数，默认 200，上限 8000。"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .min(READ_MAX_LIMIT as u64) as usize;

        // 路径白名单 soft check：禁止绝对路径 + 禁止 `..` 段
        if path_str.starts_with('/') || path_str.starts_with('\\') {
            return Ok(ToolResult {
                tool_call_id: String::new(),
                is_error: true,
                content: vec![ContentBlock::Text {
                    text: "path outside current dir: absolute path not allowed".into(),
                }],
            });
        }
        // 检查每一段是否是 ".."
        let segments_are_safe = std::path::Path::new(&path_str)
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir));
        if !segments_are_safe {
            return Ok(ToolResult {
                tool_call_id: String::new(),
                is_error: true,
                content: vec![ContentBlock::Text {
                    text: "path outside current dir: '..' segment not allowed".into(),
                }],
            });
        }

        // 解析成绝对路径（相对 ctx.cwd）
        let abs_path = if std::path::Path::new(&path_str).is_absolute() {
            std::path::PathBuf::from(&path_str)
        } else {
            ctx.cwd.join(&path_str)
        };

        let content = std::fs::read_to_string(&abs_path)
            .map_err(|e| ToolError::Execution(format!("read failed: {e}")))?;
        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len();

        let start = (offset as usize).saturating_sub(1).min(total);
        let end = (start + limit).min(total);
        let slice: Vec<&str> = lines[start..end].to_vec();
        let lines_returned = slice.len();
        let mut out = slice.join("\n");
        if start + lines_returned < total {
            let next_offset = offset as usize + lines_returned;
            out.push_str(&format!("\n<next_offset>{next_offset}</next_offset>"));
        }

        let (text, _) = truncate_chars(&out, OUTPUT_TRUNCATE_CHARS);
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text { text }],
        })
    }
}

#[async_trait]
impl ToolImpl for ArtifactReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "artifact_read".into(),
            label: "Read Artifact".into(),
            description: "读取投影保存的完整 artifact。用于 tool_result_projection.read_more 或 conversation_history_compaction.history_segment / parent_history_refs 指示。ref 支持 artifact://tool_result/sha256:<hash> 与 artifact://history/sha256:<hash>；offset 是 UTF-8 byte offset，默认 0；limit 默认 12000，最大 12000。返回 output、next_offset 和 has_more；不要把 preview/summary 当完整内容。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ref": {
                        "type": "string",
                        "description": "artifact://tool_result/sha256:<hash> 或 artifact://history/sha256:<hash> 引用，来自 projection/compaction 摘要。"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "UTF-8 byte offset。默认 0；继续读取时使用上一页 next_offset。"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 12000,
                        "description": "本页最多读取的字节数。默认 12000，最大 12000。"
                    },
                    "expected_sha256": {
                        "type": "string",
                        "description": "可选校验值；如果提供，必须与 ref 内 hash 一致。"
                    }
                },
                "required": ["ref"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let artifact_ref = input
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'ref'".into()))?;
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(crate::artifact::ARTIFACT_READ_DEFAULT_LIMIT);
        let expected_sha256 = input.get("expected_sha256").and_then(|v| v.as_str());

        let page = match crate::artifact::read_artifact_page(
            artifact_ref,
            ctx.artifact_root.as_deref(),
            offset,
            limit,
            expected_sha256,
        ) {
            Ok(page) => page,
            Err(error) => {
                return Ok(ToolResult {
                    tool_call_id: String::new(),
                    is_error: true,
                    content: vec![ContentBlock::Text {
                        text: serde_json::json!({
                            "error": {"kind": "artifact_read_failed", "message": error},
                            "ref": artifact_ref,
                        })
                        .to_string(),
                    }],
                });
            }
        };

        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: page.to_string(),
            }],
        })
    }
}

/// Write 工具：写文件（overwrite，带 .bak 备份）。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct WriteTool;

#[async_trait]
impl ToolImpl for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            label: "Write".into(),
            description: "写入文件并覆盖已有内容。原文件存在时先备份为 <path>.bak。注意：YOLO 模式会直接覆盖，不询问确认。必要时创建父目录。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径。相对路径基于 Agent 当前目录解析；拒绝绝对路径和通过 .. 越界的路径。"
                    },
                    "content": {
                        "type": "string",
                        "description": "完整文件内容，将覆盖整个文件。"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'content'".into()))?
            .to_string();

        // 路径白名单（与 ReadTool 同样的软检查）
        if path_str.starts_with('/') || path_str.starts_with('\\') {
            return Err(ToolError::InvalidInput(
                "path outside current dir: absolute path not allowed".into(),
            ));
        }
        let segments_are_safe = std::path::Path::new(&path_str)
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir));
        if !segments_are_safe {
            return Err(ToolError::InvalidInput(
                "path outside current dir: '..' segment not allowed".into(),
            ));
        }

        let abs_path = if std::path::Path::new(&path_str).is_absolute() {
            std::path::PathBuf::from(&path_str)
        } else {
            ctx.cwd.join(&path_str)
        };

        // 备份原文件
        let mut backed_up = false;
        if abs_path.exists() {
            let bak = abs_path.with_extension(format!(
                "{}.bak",
                abs_path.extension().and_then(|s| s.to_str()).unwrap_or("")
            ));
            // 如果 bak 已存在则覆盖
            std::fs::copy(&abs_path, &bak)
                .map_err(|e| ToolError::Execution(format!("backup failed: {e}")))?;
            backed_up = true;
        }

        // 创建父目录
        if let Some(parent) = abs_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ToolError::Execution(format!("mkdir failed: {e}")))?;
            }
        }

        // 原子写：写到 tmp，再 rename
        let tmp = abs_path.with_extension(format!(
            "{}.tmp",
            abs_path.extension().and_then(|s| s.to_str()).unwrap_or("")
        ));
        std::fs::write(&tmp, &content)
            .map_err(|e| ToolError::Execution(format!("write tmp failed: {e}")))?;
        std::fs::rename(&tmp, &abs_path)
            .map_err(|e| ToolError::Execution(format!("rename failed: {e}")))?;

        let mut msg = format!(
            "[write] wrote {} chars to {}",
            content.chars().count(),
            path_str
        );
        if backed_up {
            msg.push_str(" (backed up to .bak)");
        }
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text { text: msg }],
        })
    }
}

/// Edit 工具：搜索-替换编辑。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct EditTool;

#[async_trait]
impl ToolImpl for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            label: "Edit".into(),
            description: "在文件中查找并替换文本。search_text 必须精确匹配，包括空白字符。replace_globally 为 false 时必须恰好匹配一次，否则返回 is_error=true 和匹配次数；为 true 时替换所有匹配。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径。相对路径基于 Agent 当前目录解析；拒绝绝对路径和通过 .. 越界的路径。"
                    },
                    "search_text": {
                        "type": "string",
                        "description": "精确查找的文本，空白字符也必须匹配。"
                    },
                    "replace_text": {
                        "type": "string",
                        "description": "替换后的文本。空字符串表示删除匹配内容。"
                    },
                    "replace_globally": {
                        "type": "boolean",
                        "description": "true 表示替换所有匹配；false（默认）要求恰好匹配一次。"
                    }
                },
                "required": ["path", "search_text", "replace_text"]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let search_text = input
            .get("search_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'search_text'".into()))?
            .to_string();
        let replace_text = input
            .get("replace_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'replace_text'".into()))?
            .to_string();
        let replace_globally = input
            .get("replace_globally")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 路径白名单
        if path_str.starts_with('/') || path_str.starts_with('\\') {
            return Err(ToolError::InvalidInput(
                "path outside current dir: absolute path not allowed".into(),
            ));
        }
        let segments_are_safe = std::path::Path::new(&path_str)
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir));
        if !segments_are_safe {
            return Err(ToolError::InvalidInput(
                "path outside current dir: '..' segment not allowed".into(),
            ));
        }

        let abs_path = if std::path::Path::new(&path_str).is_absolute() {
            std::path::PathBuf::from(&path_str)
        } else {
            ctx.cwd.join(&path_str)
        };

        let original = std::fs::read_to_string(&abs_path)
            .map_err(|e| ToolError::Execution(format!("read failed: {e}")))?;

        // 数 matches（non-overlapping）
        let match_count = original.matches(&search_text).count();

        if !replace_globally {
            if match_count != 1 {
                return Ok(ToolResult {
                    tool_call_id: String::new(),
                    is_error: true,
                    content: vec![ContentBlock::Text {
                        text: format!("expected 1 match, found {match_count}"),
                    }],
                });
            }
        } else if match_count == 0 {
            return Ok(ToolResult {
                tool_call_id: String::new(),
                is_error: true,
                content: vec![ContentBlock::Text {
                    text: "expected 1 match, found 0".into(),
                }],
            });
        }

        // 备份
        let bak = abs_path.with_extension(format!(
            "{}.bak",
            abs_path.extension().and_then(|s| s.to_str()).unwrap_or("")
        ));
        std::fs::copy(&abs_path, &bak)
            .map_err(|e| ToolError::Execution(format!("backup failed: {e}")))?;

        let new_content = if replace_globally {
            original.replace(&search_text, &replace_text)
        } else {
            original.replacen(&search_text, &replace_text, 1)
        };

        // 原子写
        let tmp = abs_path.with_extension(format!(
            "{}.tmp",
            abs_path.extension().and_then(|s| s.to_str()).unwrap_or("")
        ));
        std::fs::write(&tmp, &new_content)
            .map_err(|e| ToolError::Execution(format!("write tmp failed: {e}")))?;
        std::fs::rename(&tmp, &abs_path)
            .map_err(|e| ToolError::Execution(format!("rename failed: {e}")))?;

        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: format!("[edit] replaced {match_count} occurrence(s) in {path_str}"),
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_input_source_wire_names() {
        assert_eq!(InputSource::default(), InputSource::User);
        for (source, wire) in [
            (InputSource::User, "user"),
            (InputSource::Peer, "peer"),
            (InputSource::Delegate, "delegate"),
        ] {
            let value = serde_json::json!(wire);
            assert_eq!(serde_json::to_value(source).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<InputSource>(value).unwrap(),
                source
            );
        }
        assert!(serde_json::from_value::<InputSource>(serde_json::json!("User")).is_err());
    }

    #[tokio::test]
    async fn external_tool_preserves_context_identity_and_error_result() {
        struct HostTool;
        #[async_trait]
        impl ToolImpl for HostTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "host_tool".into(),
                    label: "Host".into(),
                    description: "Host integration".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                ctx: &ToolContext,
            ) -> Result<ToolResult, ToolError> {
                assert_eq!(ctx.owner.as_deref(), Some("task-owner"));
                assert_eq!(ctx.tool_call_id.as_deref(), Some("call-1"));
                Ok(ToolResult {
                    tool_call_id: "upstream-id".into(),
                    is_error: true,
                    content: vec![ContentBlock::Text {
                        text: "upstream tool error".into(),
                    }],
                })
            }
        }
        let external = ExternalTool::new(Arc::new(HostTool));
        assert_eq!(external, external.clone());
        assert_ne!(external, ExternalTool::new(Arc::new(HostTool)));
        let tool = ToolKind::External(external);
        assert_eq!(tool.name(), "host_tool");
        assert_eq!(tool.spec().name, "host_tool");
        let result = tool
            .execute(
                "call-1",
                serde_json::json!({}),
                &ToolContext {
                    owner: Some("task-owner".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.tool_call_id, "call-1");
        assert!(result.is_error);
        assert!(
            matches!(&result.content[0], ContentBlock::Text { text } if text == "upstream tool error")
        );
    }

    #[tokio::test]
    async fn echo_tool_echoes_text() {
        let tool = EchoTool;
        let ctx = ToolContext::default();
        let result = tool
            .execute(serde_json::json!({"text": "hello"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[echo] hello"),
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn echo_tool_echoes_any_input_as_fallback() {
        // 即使 LLM 违反 schema 没传 text 字段，代码侧也不应崩 ——
        // 把整个 JSON 序列化返回。这是 A+C 修复的 C 部分：宽容执行。
        let tool = EchoTool;
        let ctx = ToolContext::default();
        let result = tool
            .execute(serde_json::json!({"foo": "bar", "n": 42}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.starts_with("[echo]"));
                assert!(text.contains("foo"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn echo_tool_handles_empty_object_gracefully() {
        // 真实场景：LLM 偶尔会传 {}（违反 schema 但实际出现过）。
        // 代码侧不应 panic 或返回 Err。
        let tool = EchoTool;
        let ctx = ToolContext::default();
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[echo] {}"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn echo_tool_spec_has_text_as_required() {
        // A+C 修复的 A 部分：schema 必须标记 text 为 required，
        // 引导 LLM 传这个字段。
        let spec = EchoTool.spec();
        assert_eq!(spec.name, "echo");
        let required = spec.input_schema["required"]
            .as_array()
            .expect("required should be array");
        assert!(
            required.contains(&serde_json::Value::String("text".into())),
            "text 字段必须标记为 required，当前: {:?}",
            required
        );
    }

    #[test]
    fn echo_tool_description_guides_llm() {
        // description 里必须有强信号让 LLM 知道怎么用
        let spec = EchoTool.spec();
        assert!(spec.description.contains("echo"), "desc 应提到 echo");
        assert!(spec.description.contains("text"), "desc 应提到 text 字段");
        // 强信号词：MUST / NEVER / literal
        let lower = spec.description.to_lowercase();
        assert!(
            lower.contains("必须") || lower.contains("禁止"),
            "desc 必须包含“必须”或“禁止”等明确要求，当前: {:?}",
            spec.description
        );
        assert!(
            spec.description.contains("hello")
                || spec.description.contains("world")
                || spec.description.contains("你好"),
            "desc 应包含具体示例值（hello/world/你好）"
        );
    }

    // ============================ 算子 + CalculateTool 测试 ============================

    #[tokio::test]
    async fn add_tool_basic() {
        let t = AddTool;
        let r = t
            .execute(serde_json::json!({"a": 1, "b": 2}), &ToolContext::default())
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[add] 1 + 2 = 3"),
            _ => panic!(),
        }
        assert!(!r.is_error);
    }

    #[tokio::test]
    async fn sub_tool_basic() {
        let t = SubTool;
        let r = t
            .execute(serde_json::json!({"a": 5, "b": 3}), &ToolContext::default())
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[sub] 5 - 3 = 2"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn mul_tool_basic() {
        let t = MulTool;
        let r = t
            .execute(serde_json::json!({"a": 4, "b": 3}), &ToolContext::default())
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[mul] 4 * 3 = 12"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn div_tool_basic() {
        let t = DivTool;
        let r = t
            .execute(
                serde_json::json!({"a": 20, "b": 4}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[div] 20 / 4 = 5"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn div_tool_rejects_zero() {
        let t = DivTool;
        let r = t
            .execute(
                serde_json::json!({"a": 10, "b": 0}),
                &ToolContext::default(),
            )
            .await;
        assert!(r.is_err(), "除以零应返回 Err");
    }

    #[tokio::test]
    async fn operators_require_a_and_b() {
        for (name, result) in [
            (
                "add",
                AddTool
                    .execute(serde_json::json!({"a": 1}), &ToolContext::default())
                    .await,
            ),
            (
                "sub",
                SubTool
                    .execute(serde_json::json!({"b": 1}), &ToolContext::default())
                    .await,
            ),
            (
                "mul",
                MulTool
                    .execute(serde_json::json!({}), &ToolContext::default())
                    .await,
            ),
        ] {
            let _ = name;
            assert!(result.is_err(), "缺 a/b 时应返回 Err");
        }
    }

    #[tokio::test]
    async fn calculate_returns_dispatch_hint_for_add() {
        let t = CalculateTool;
        let r = t
            .execute(
                serde_json::json!({"expression": "1+2"}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("'add'"), "hint 应提到 add: {text}");
                assert!(text.contains("a: 1"), "hint 应给出 a=1: {text}");
                assert!(text.contains("b: 2"), "hint 应给出 b=2: {text}");
                assert!(text.contains("下一步"), "hint 应明确说 'Next step': {text}");
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn calculate_handles_each_op() {
        let t = CalculateTool;
        for (expr, expected_tool) in [
            ("10-3", "sub"),
            ("4*5", "mul"),
            ("20/4", "div"),
            (" 1 + 2 ", "add"), // 容错：两边空格
        ] {
            let r = t
                .execute(
                    serde_json::json!({"expression": expr}),
                    &ToolContext::default(),
                )
                .await
                .unwrap();
            match &r.content[0] {
                ContentBlock::Text { text } => assert!(
                    text.contains(&format!("'{expected_tool}'")),
                    "{expr} 应路由到 {expected_tool}，实际: {text}"
                ),
                _ => panic!(),
            }
        }
    }

    #[tokio::test]
    async fn calculate_rejects_unparseable() {
        let t = CalculateTool;
        let r = t
            .execute(
                serde_json::json!({"expression": "1+2+3"}),
                &ToolContext::default(),
            )
            .await;
        assert!(r.is_err(), "三 operand 应拒绝（MVP 只支持两 operand）");
    }

    #[test]
    fn calculate_spec_explains_two_turn_dispatch() {
        // spec 必须明确告诉 LLM 两轮 dispatch 流程
        let spec = CalculateTool.spec();
        let lower = spec.description.to_lowercase();
        assert!(lower.contains("首先"), "应强调 FIRST");
        assert!(lower.contains("下一轮"), "应说明 NEXT turn 的动作");
        assert!(lower.contains("add"), "应列出 add 工具名");
        assert!(lower.contains("sub"), "应列出 sub 工具名");
        assert!(lower.contains("mul"), "应列出 mul 工具名");
        assert!(lower.contains("div"), "应列出 div 工具名");
    }

    #[test]
    fn tool_kind_dispatches_all_variants() {
        // 验证 ToolKind::name 对所有 variant 都正确
        assert_eq!(ToolKind::Choice(ChoiceTool).name(), "ask_user_choice");
        assert_eq!(ToolKind::Echo(EchoTool).name(), "echo");
        assert_eq!(ToolKind::Add(AddTool).name(), "add");
        assert_eq!(ToolKind::Sub(SubTool).name(), "sub");
        assert_eq!(ToolKind::Mul(MulTool).name(), "mul");
        assert_eq!(ToolKind::Div(DivTool).name(), "div");
        assert_eq!(ToolKind::Calculate(CalculateTool).name(), "calculate");
        assert_eq!(ToolKind::Bash(BashTool).name(), "bash");
        assert_eq!(ToolKind::Read(ReadTool).name(), "read");
        assert_eq!(
            ToolKind::ArtifactRead(ArtifactReadTool).name(),
            "artifact_read"
        );
        assert_eq!(ToolKind::Edit(EditTool).name(), "edit");
        assert_eq!(ToolKind::Write(WriteTool).name(), "write");
        assert_eq!(ToolKind::WebSearch(WebSearchTool).name(), "web_search");
        assert_eq!(ToolKind::AgentReach(AgentReachTool).name(), "agent_reach");
    }

    #[tokio::test]
    async fn choice_tool_waits_for_ui_selection() {
        let broker = std::sync::Arc::new(crate::ChoiceBroker::new());
        let ctx = ToolContext {
            choices: Some(broker.clone()),
            owner: Some("task-choice".into()),
            ..Default::default()
        };
        let worker = tokio::spawn(async move {
            ChoiceTool
                .execute(
                    serde_json::json!({
                        "title":"文件已存在",
                        "body":"请选择处理方式",
                        "options":[
                            {"id":"overwrite","label":"覆盖"},
                            {"id":"cancel","label":"取消","intent":{"type":"cancel_task"}}
                        ]
                    }),
                    &ctx,
                )
                .await
                .unwrap()
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while broker.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        broker.answer(&broker.pending()[0].id, "overwrite").unwrap();
        let result = worker.await.unwrap();
        let ContentBlock::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["choice"]["selected_option_id"], "overwrite");
        assert_eq!(value["choice"]["selected"]["label"], "覆盖");
    }

    // ============================ BashTool 测试 ============================

    fn bash_test_context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            crate::execution::smallhead_process::ProcessManager::with_log_root(dir.path().into())
                .unwrap();
        let runtime = crate::execution::ExecutionRuntime::new(manager).unwrap();
        (
            dir,
            ToolContext {
                execution: Some(std::sync::Arc::new(runtime)),
                cwd: std::env::current_dir().unwrap(),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn bash_runs_simple_command_and_captures_stdout() {
        let tool = BashTool;
        let (_dir, ctx) = bash_test_context();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("hello"),
                    "stdout 应包含 'hello'，实际: {text}"
                );
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn bash_timeout_returns_error() {
        let tool = BashTool;
        let (_dir, ctx) = bash_test_context();
        // sleep 5 + timeout 200ms → 必须超时
        let result = tool
            .execute(
                serde_json::json!({"command": "sleep 5", "timeout_ms": 200}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error, "timeout 应返回 is_error=true");
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(
                text.contains("timeout"),
                "错误内容应提到 timeout，实际: {text}"
            ),
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit_returns_is_error() {
        let tool = BashTool;
        let (_dir, ctx) = bash_test_context();
        // `false` 命令退出码 1
        let result = tool
            .execute(serde_json::json!({"command": "false"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error, "非零退出应返回 is_error=true");
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("exit code"),
                    "错误内容应提到 exit code，实际: {text}"
                );
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn bash_allows_removing_temporary_subdirectory() {
        let tool = BashTool;
        let (dir, ctx) = bash_test_context();
        let target = dir.path().join("disposable");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("sample.txt"), "test").unwrap();
        let escaped: String = target
            .display()
            .to_string()
            .chars()
            .flat_map(|c| {
                if c.is_ascii_alphanumeric() || "/._-".contains(c) {
                    vec![c]
                } else {
                    vec!['\\', c]
                }
            })
            .collect();
        let result = tool
            .execute(
                serde_json::json!({"command": format!("rm -rf {escaped}")}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn bash_blacklisted_command_returns_error_result() {
        // v0.4: 黑名单命令被 check_command 拦截，应返回 is_error=true
        // 且 content 里包含 "BLOCKED by permission" —— 而且不真正执行。
        // --help 保证检查失效时测试也不会执行删除。
        let tool = BashTool;
        let (_dir, ctx) = bash_test_context();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf / --help"}), &ctx)
            .await
            .unwrap();
        assert!(
            result.is_error,
            "黑名单命令应返回 is_error=true，实际 is_error={}",
            result.is_error
        );
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("BLOCKED"),
                    "错误内容应包含 'BLOCKED by permission'，实际: {text}"
                );
                assert!(
                    text.contains("rm -rf /"),
                    "错误内容应指明命中的 pattern，实际: {text}"
                );
            }
            _ => panic!("expected text block"),
        }
    }

    // ============================ ReadTool 测试 ============================

    #[tokio::test]
    async fn read_returns_offset_limit_slice() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let tool = ReadTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };
        // offset=2 limit=2 → 取 line2, line3
        let result = tool
            .execute(
                serde_json::json!({"path": "sample.txt", "offset": 2, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("line2"), "应包含 line2: {text}");
                assert!(text.contains("line3"), "应包含 line3: {text}");
                assert!(!text.contains("line1"), "不应包含 line1: {text}");
                assert!(!text.contains("line4"), "不应包含 line4: {text}");
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn read_appends_next_offset_when_more_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("five.txt");
        std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

        let tool = ReadTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };
        // limit=2 → 取前 2 行，后面还有 3 行，必须有 <next_offset>3</next_offset>
        let result = tool
            .execute(
                serde_json::json!({"path": "five.txt", "offset": 1, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("<next_offset>3</next_offset>"),
                    "应追加 next_offset=3，实际: {text}"
                );
            }
            _ => panic!("expected text block"),
        }
    }

    // ============================ WriteTool 测试 ============================

    #[tokio::test]
    async fn write_creates_and_overwrites_with_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.txt");
        let bak = tmp.path().join("note.txt.bak");

        let tool = WriteTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };

        // 1) 首次写：fresh，无 .bak
        let r1 = tool
            .execute(
                serde_json::json!({"path": "note.txt", "content": "hello v1"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r1.is_error);
        let v1 = std::fs::read_to_string(&file).unwrap();
        assert_eq!(v1, "hello v1");
        assert!(!bak.exists(), "首次写不应有 .bak");

        // 2) 二次写：覆盖 + .bak
        let r2 = tool
            .execute(
                serde_json::json!({"path": "note.txt", "content": "hello v2"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r2.is_error);
        let v2 = std::fs::read_to_string(&file).unwrap();
        assert_eq!(v2, "hello v2");
        assert!(bak.exists(), "二次写应生成 .bak");
        let bak_content = std::fs::read_to_string(&bak).unwrap();
        assert_eq!(
            bak_content, "hello v1",
            ".bak 内容应是原文件 v1，实际: {bak_content}"
        );
    }

    // ============================ EditTool 测试 ============================

    #[tokio::test]
    async fn edit_replaces_single_match() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("data.txt");
        std::fs::write(&file, "foo\nbar\nbaz\n").unwrap();

        let tool = EditTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };
        // 默认 replace_globally=false，正好 1 个 match → OK
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "data.txt",
                    "search_text": "bar",
                    "replace_text": "BAR"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, "foo\nBAR\nbaz\n");
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_match() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("dup.txt");
        std::fs::write(&file, "x x x\n").unwrap();

        let tool = EditTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };
        // 3 个 match，replace_globally=false → is_error
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "dup.txt",
                    "search_text": "x",
                    "replace_text": "Y",
                    "replace_globally": false
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error, "多 match 应返回 is_error");
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(
                text.contains("expected 1 match"),
                "应报告 expected 1 match，实际: {text}"
            ),
            _ => panic!("expected text block"),
        }
        // 文件不应被修改
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, "x x x\n");
    }

    #[tokio::test]
    async fn edit_replaces_globally() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("multi.txt");
        std::fs::write(&file, "a a a\n").unwrap();

        let tool = EditTool;
        let ctx = ToolContext {
            cwd: tmp.path().to_path_buf(),
            ..Default::default()
        };
        // 3 个 match，replace_globally=true → 全部替换
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "multi.txt",
                    "search_text": "a",
                    "replace_text": "Z",
                    "replace_globally": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, "Z Z Z\n");
    }
}

// ============================ WebSearchTool ============================
//
// Marker struct —— 实际执行逻辑喺 `smallhead-tools` 里。
// 用 `OnceLock<HandlerFn>` 注册,避免 `smallhead-tools` 反过来依赖
// `smallhead-core`(已经依赖)。
//
// 注册流程:
// 1. host crate 启动时调 `smallhead_tools::register()`(或者 crates 直接 use)
// 2. `smallhead_tools::register` 内部调 `set_web_search_handler(handler)`
// 3. `WebSearchTool::execute` 通过全局 handler 派发
//
// 如果 host 忘记注册,`execute` 返 `ToolError::Execution` 错误,而不是 panic。

use std::sync::OnceLock;

/// 统一 web 搜索 tool 嘅 handler 签名。
///
/// 接受与 [`ToolImpl::execute`] 相同嘅参数,返与 [`ToolImpl::execute`]
/// 相同嘅结果。`smallhead-tools` 提供呢个签名嘅函数并通过
/// [`set_web_search_handler`] 注入。
pub type WebSearchHandler = fn(
    input: serde_json::Value,
    ctx: &ToolContext,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ToolResult, ToolError>> + Send + 'static>,
>;

static WEB_SEARCH_HANDLER: OnceLock<WebSearchHandler> = OnceLock::new();

/// 注册统一 web 搜索 handler。重复注册会 panic(典型使用场景只调一次)。
pub fn set_web_search_handler(handler: WebSearchHandler) {
    if WEB_SEARCH_HANDLER.set(handler).is_err() {
        panic!("web_search handler already registered");
    }
}

/// 内部:获取 handler 引用;未注册返 None。
fn web_search_handler() -> Option<WebSearchHandler> {
    WEB_SEARCH_HANDLER.get().copied()
}

/// 统一 web 搜索 tool marker。
///
/// 实际逻辑:见 `smallhead_tools::web_search_tool`。
/// - `kind=web`   → Baidu / Bing.cn / Mojeek / DDG(或者 en: Bing.com / Mojeek / DDG)
/// - `kind=wiki` → Wikipedia(zh / en 按 CJK)
/// - `kind=code` → GitHub repos
/// - `kind=news` → HackerNews stories
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct WebSearchTool;

#[async_trait]
impl ToolImpl for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            label: "Web 搜索".into(),
            description: "统一多源 web 搜索(9 个 provider,health-aware fallback chain)。必须传 query + kind。kind=web 走 CJK-aware 通用链(Baidu/Bing.cn/Mojeek/DDG 或 Bing.com/Mojeek/DDG);kind=wiki 按 CJK 拣 zh/en Wikipedia;kind=code 走 GitHub repos;kind=news 走 HackerNews stories。CJK detection 自动,无需 explicit cjk flag。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索词。CJK 字符会自动拣 zh 链;ASCII 默认 en 链。"
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["web", "wiki", "code", "news"],
                        "description": "搜索源类型。web=通用网页;wiki=Wikipedia;code=GitHub;news=HackerNews。"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "description": "返回结果数量,默认 8,clamp 到 [1, 30]。"
                    }
                },
                "required": ["query", "kind"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let handler = web_search_handler().ok_or_else(|| {
            ToolError::Execution(
                "web_search handler not registered; ensure smallhead-tools::register() is called at startup".into(),
            )
        })?;
        handler(input, ctx).await
    }
}

// ============================ AgentReachTool ============================
//
// Marker struct —— 实际执行逻辑喺 `smallhead-tools` 里。
// Agent-Reach 依赖外部 CLI 和用户本机认证状态,所以 core 只持有 schema
// 与全局 handler 注册口。

/// Agent-Reach CLI tool 嘅 handler 签名。
pub type AgentReachHandler = fn(
    input: serde_json::Value,
    ctx: &ToolContext,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ToolResult, ToolError>> + Send + 'static>,
>;

static AGENT_REACH_HANDLER: OnceLock<AgentReachHandler> = OnceLock::new();

/// 注册 Agent-Reach CLI handler。重复注册会 panic(典型使用场景只调一次)。
pub fn set_agent_reach_handler(handler: AgentReachHandler) {
    if AGENT_REACH_HANDLER.set(handler).is_err() {
        panic!("agent_reach handler already registered");
    }
}

/// 内部:获取 Agent-Reach handler 引用;未注册返 None。
fn agent_reach_handler() -> Option<AgentReachHandler> {
    AGENT_REACH_HANDLER.get().copied()
}

/// Agent-Reach CLI tool marker。
///
/// 实际逻辑:见 `smallhead_tools::agent_reach_tool`。它 shell-out 到本机
/// Agent-Reach/upstream CLI,用于 Exa 搜索、网页读取、RSS、YouTube 和 Bilibili。
#[derive(Clone, Default, Debug, Hash, Eq, PartialEq)]
pub struct AgentReachTool;

#[async_trait]
impl ToolImpl for AgentReachTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent_reach".into(),
            label: "Agent-Reach CLI".into(),
            description: "调用本机 Agent-Reach/upstream CLI channel。先 action=list 查看 channel、CLI 与可用性。action=search 目前支持 channel=search/youtube/bili，需要 query，可选 limit。action=fetch 支持 channel=webpage/rss/youtube/bili，需要 target URL，可选 output_limit_chars。视频 fetch 当前只返回元数据，transcript_included=false 表示未读取字幕或视频内容，不能据此声称已观看或总结完整视频。本工具不提供安装动作；用户明确要求安装或配置时使用 bash 完成。CLI 缺失或未登录时如实返回错误。通用网页/wiki/code/news 优先用 web_search。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "search", "fetch"],
                        "description": "list=列出 channel; search=搜索; fetch=抓取单个 URL/feed/video。"
                    },
                    "channel": {
                        "type": "string",
                        "enum": ["search", "webpage", "rss", "youtube", "bili", "twitter", "github", "reddit", "xhs", "linkedin", "boss", "v2ex", "xueqiu", "xiaoyuzhou", "facebook", "instagram"],
                        "description": "Agent-Reach channel。search 支持 search/youtube/bili；fetch 支持 webpage/rss/youtube/bili。"
                    },
                    "query": {
                        "type": "string",
                        "description": "search action 的搜索词。"
                    },
                    "target": {
                        "type": "string",
                        "description": "fetch action 的 URL/feed/video 地址。"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "description": "search 返回数量，默认 8。"
                    },
                    "output_limit_chars": {
                        "type": "integer",
                        "minimum": 1024,
                        "maximum": 60000,
                        "description": "fetch 输出字符上限，默认 20000。"
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let handler = agent_reach_handler().ok_or_else(|| {
            ToolError::Execution(
                "agent_reach handler not registered; ensure smallhead-tools::agent_reach_tool::register() is called at startup".into(),
            )
        })?;
        handler(input, ctx).await
    }
}

#[cfg(test)]
mod web_search_tool_tests {
    use super::*;

    #[test]
    fn spec_name_and_required_fields() {
        let tool = WebSearchTool;
        let spec = tool.spec();
        assert_eq!(spec.name, "web_search");
        let schema = spec.input_schema;
        let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"query"));
        assert!(names.contains(&"kind"));
    }

    #[test]
    fn execute_without_handler_returns_error() {
        // 用一个 isolated executor 测 —— 但 OnceLock 系 process-global,
        // 如果其他 test 已经注册,呢个 test 就会失败。
        // 改为只断言:如果未注册,execute 返 Err;如果已注册,execute 返 Ok。
        // 真嘅 round-trip 行为留俾 smallhead-tools 那边嘅测试。
        let tool = WebSearchTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.execute(
            serde_json::json!({"query": "x", "kind": "web"}),
            &ToolContext::default(),
        ));
        match web_search_handler() {
            None => assert!(result.is_err()),
            Some(_) => {
                // 已注册,允许返 Ok 或者 Err(取决于 provider chain 实际 hit)
                let _ = result;
            }
        }
    }
}

#[cfg(test)]
mod agent_reach_tool_tests {
    use super::*;

    #[test]
    fn spec_name_and_action_field() {
        let tool = AgentReachTool;
        let spec = tool.spec();
        assert_eq!(spec.name, "agent_reach");
        let schema = spec.input_schema;
        let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"action"));
    }

    #[test]
    fn execute_without_handler_returns_error() {
        let tool = AgentReachTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.execute(
            serde_json::json!({"action": "list"}),
            &ToolContext::default(),
        ));
        match agent_reach_handler() {
            None => assert!(result.is_err()),
            Some(_) => {
                let _ = result;
            }
        }
    }
}
