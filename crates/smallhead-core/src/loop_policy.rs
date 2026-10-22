//! Composable decisions around the ReAct driver. `next` computes a decision;
//! it never executes a model or tool. The driver owns all actual effects.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{message::ToolCall, AgentMessage, ProviderRequest, ToolResult};

/// Authoritative, inexpensive snapshot of one policy checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyContext {
    /// Fresh identity per accepted user turn, including on reusable loops.
    pub turn_id: String,
    /// Current model wave (one based).
    pub wave: usize,
    /// Number of tool attempts handled by the driver this turn.
    pub tool_calls: usize,
    /// Whether a finish candidate may request another model wave.
    pub can_continue: bool,
    /// Active driver guard; policies cannot lift this guard.
    pub finalization_reason: Option<String>,
}

/// Typed checkpoints. Raw history is not mutated by request projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyEvent {
    /// Transform the next model request.
    PrepareModel {
        /// Driver-assembled request before Lens projection.
        request: ProviderRequest,
    },
    /// Adapt or reject one proposed call, before any execution.
    BeforeTool {
        /// Model-proposed invocation.
        call: ToolCall,
    },
    /// Transform a projected observation, after actual execution is recorded.
    AfterTool {
        /// Actual invocation including adapted arguments.
        call: ToolCall,
        /// Bounded observation backed by the recorded execution.
        result: ToolResult,
    },
    /// A no-tool response is a candidate, not yet a completed turn.
    BeforeFinish {
        /// Committed assistant response with no tool calls.
        candidate: AgentMessage,
    },
}

/// A decision interpreted exactly once by the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyAction {
    /// Submit a transformed request.
    ModelRequest {
        /// Request to validate and submit.
        request: ProviderRequest,
    },
    /// Execute a call through the registered tool implementation.
    ExecuteTool {
        /// Same identity and name, optionally adapted arguments.
        call: ToolCall,
    },
    /// Deliver a transformed tool observation.
    ObserveTool {
        /// Same receipt identity/status, optionally transformed content.
        result: ToolResult,
    },
    /// Refuse the call; driver pairs it with an explicit error result.
    RejectTool {
        /// Explanation included in the paired error observation.
        reason: String,
    },
    /// Accept the candidate and finish normally.
    Finish,
    /// Request another wave with explicitly Lens-sourced guidance.
    Continue {
        /// Next-wave instruction labelled as Lens-generated context.
        guidance: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Model,
    ToolCall,
    ToolObservation,
    Finish,
}

impl PolicyEvent {
    fn phase(&self) -> Phase {
        match self {
            Self::PrepareModel { .. } => Phase::Model,
            Self::BeforeTool { .. } => Phase::ToolCall,
            Self::AfterTool { .. } => Phase::ToolObservation,
            Self::BeforeFinish { .. } => Phase::Finish,
        }
    }

    fn default_action(self) -> PolicyAction {
        match self {
            Self::PrepareModel { request } => PolicyAction::ModelRequest { request },
            Self::BeforeTool { call } => PolicyAction::ExecuteTool { call },
            Self::AfterTool { result, .. } => PolicyAction::ObserveTool { result },
            Self::BeforeFinish { .. } => PolicyAction::Finish,
        }
    }
}

/// One layer of loop strategy. Instances may hold private state; host-created
/// stacks should scope stateful instances to a turn or explicitly key by turn id.
#[async_trait]
pub trait LoopPolicy: Send + Sync {
    /// Stable identity used in diagnostics.
    fn name(&self) -> &str;

    /// Wrap the inner decision. Calling `next` is optional, and at most once.
    async fn apply(
        &self,
        context: &PolicyContext,
        event: PolicyEvent,
        next: PolicyNext,
    ) -> Result<PolicyAction>;
}

/// Ordered outer-to-inner layers; an empty stack is the existing ReAct policy.
#[derive(Clone, Default)]
pub struct PolicyStack {
    layers: Arc<Vec<Arc<dyn LoopPolicy>>>,
}

impl PolicyStack {
    /// Construct `layers[0](layers[1](...default))` without executing effects.
    pub fn new(layers: Vec<Arc<dyn LoopPolicy>>) -> Self {
        Self {
            layers: Arc::new(layers),
        }
    }

    /// Ordered identities, also recorded by the driver at every checkpoint.
    pub fn names(&self) -> Vec<&str> {
        self.layers.iter().map(|layer| layer.name()).collect()
    }

    /// Whether the driver may take its zero-layer fast path.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Compute a typed decision. Driver performs cancellation, timeout and
    /// capability validation around this call before executing the decision.
    pub async fn decide(&self, context: PolicyContext, event: PolicyEvent) -> Result<PolicyAction> {
        let phase = event.phase();
        let next = PolicyNext::new(self.layers.clone(), 0, context, phase);
        next.run(event).await
    }
}

/// Owned, single-use continuation to the inner policy. Rust prevents replay by
/// consuming `next`; external runtimes enforce the same rule on their wire.
///
/// ```compile_fail
/// use smallhead_core::loop_policy::{PolicyEvent, PolicyNext};
/// async fn cannot_replay(next: PolicyNext, event: PolicyEvent) {
///     let _ = next.run(event.clone()).await;
///     let _ = next.run(event).await; // `next` was moved by the first call.
/// }
/// ```
pub struct PolicyNext {
    layers: Arc<Vec<Arc<dyn LoopPolicy>>>,
    index: usize,
    context: PolicyContext,
    phase: Phase,
    violated: Arc<AtomicBool>,
}

impl PolicyNext {
    fn new(
        layers: Arc<Vec<Arc<dyn LoopPolicy>>>,
        index: usize,
        context: PolicyContext,
        phase: Phase,
    ) -> Self {
        Self {
            layers,
            index,
            context,
            phase,
            violated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Consume this continuation to compute the inner action once. Even a
    /// swallowed inner error invalidates the enclosing decision.
    pub fn run(
        self,
        event: PolicyEvent,
    ) -> futures::future::BoxFuture<'static, Result<PolicyAction>> {
        Box::pin(async move {
            if event.phase() != self.phase {
                self.violated.store(true, Ordering::SeqCst);
                bail!("policy next cannot change checkpoint phase");
            }
            let Some(layer) = self.layers.get(self.index) else {
                return Ok(event.default_action());
            };
            let next = Self::new(
                self.layers.clone(),
                self.index + 1,
                self.context.clone(),
                self.phase,
            );
            let violation = next.violated.clone();
            let action = match layer.apply(&self.context, event, next).await {
                Ok(action) => action,
                Err(error) => {
                    self.violated.store(true, Ordering::SeqCst);
                    return Err(error);
                }
            };
            if violation.load(Ordering::SeqCst) {
                self.violated.store(true, Ordering::SeqCst);
                bail!(
                    "policy {} suppressed an invalid inner decision",
                    layer.name()
                );
            }
            let valid = matches!(
                (self.phase, &action),
                (Phase::Model, PolicyAction::ModelRequest { .. })
                    | (
                        Phase::ToolCall,
                        PolicyAction::ExecuteTool { .. } | PolicyAction::RejectTool { .. }
                    )
                    | (Phase::ToolObservation, PolicyAction::ObserveTool { .. })
                    | (
                        Phase::Finish,
                        PolicyAction::Finish | PolicyAction::Continue { .. }
                    )
            );
            if !valid {
                self.violated.store(true, Ordering::SeqCst);
                bail!(
                    "policy {} returned action for a different checkpoint",
                    layer.name()
                );
            }
            Ok(action)
        })
    }
}

/// Validate a request projection against the driver's capabilities. Tool
/// schemas/identities and protocol receipts stay authoritative; descriptions,
/// tool subsets, system, text projection and sampling may be adapted.
pub(crate) fn validate_request(
    original: &ProviderRequest,
    request: &ProviderRequest,
) -> Result<()> {
    if request.max_tokens == 0
        || request.max_tokens > original.max_tokens
        || !request.temperature.is_finite()
        || !(0.0..=2.0).contains(&request.temperature)
    {
        bail!("Lens request exceeds model parameter limits");
    }
    let mut names = std::collections::HashSet::new();
    for tool in &request.tools {
        if !names.insert(&tool.name)
            || !original
                .tools
                .iter()
                .any(|known| known.name == tool.name && known.input_schema == tool.input_schema)
        {
            bail!(
                "Lens cannot invent or duplicate tool schemas: {}",
                tool.name
            );
        }
    }
    fn receipts(messages: &[crate::LlmMessage]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|message| match message {
                crate::LlmMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    Some(serde_json::json!({"calls": tool_calls}))
                }
                crate::LlmMessage::ToolResult {
                    tool_call_id,
                    is_error,
                    ..
                } => Some(serde_json::json!({"result":tool_call_id,"is_error":is_error})),
                _ => None,
            })
            .collect()
    }
    if receipts(&original.messages) != receipts(&request.messages) {
        bail!("Lens cannot rewrite tool protocol history");
    }
    Ok(())
}
