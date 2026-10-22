//! Host-owned capability changes committed between model waves.

use crate::{loop_policy::PolicyStack, ToolKind};

/// One complete, validated host environment. History and budgets are untouched.
pub struct LoopEnvironment {
    /// Tools advertised to the next model wave.
    pub tools: Vec<ToolKind>,
    /// Host prompt including the active domain contributions.
    pub system: String,
    /// Policies matching the same capability generation.
    pub policies: PolicyStack,
}

/// A host stages changes during tool execution; the driver commits them only
/// after all receipts from that wave have been paired with their calls.
pub trait LoopEnvironmentSource: Send + Sync {
    /// Take the latest prepared environment, if a change was requested.
    fn take_update(&self) -> Option<LoopEnvironment>;
}
