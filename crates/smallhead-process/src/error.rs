//! Error type for [`crate::manager::ProcessManager`] and friends.
//!
//! All public APIs that can fail return [`ProcessError`]. Variants carry
//! enough context (the offending `ExecutionId`, validator reason, raw IO
//! cause) that callers can render user-facing messages without re-deriving
//! the story from internal state.

use thiserror::Error;

use crate::id::ExecutionId;

/// Errors produced by [`crate::manager::ProcessManager`] operations.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The supplied spec failed validation (empty command, unsupported
    /// scope, zero timeout, etc.).
    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// No execution with the requested id exists in the registry.
    #[error("execution {0} not found")]
    NotFound(ExecutionId),

    /// Attempted to drive a state transition on an execution that has
    /// already reached a terminal state. Terminal states are immutable —
    /// once `Exited / Rejected / SpawnFailed / Lost / CleanupFailed` is
    /// recorded, no further `mark_*` calls are accepted.
    #[error("execution {0} already terminal")]
    AlreadyTerminal(ExecutionId),

    /// Spawn itself failed (binary missing, EACCES, etc.). Captured by the
    /// executor at OS-call time and surfaced as the `SpawnFailed` terminal
    /// state — this variant exists for ergonomic returns from internal
    /// helpers, not from the public `start` API directly.
    #[error("spawn failed: {0}")]
    Spawn(String),

    /// Catch-all for invariants violated by the caller that don't fit the
    /// other variants. Reserved for internal sanity checks; if you find
    /// yourself returning this from a public API, please file a bug.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<uuid::Error> for ProcessError {
    fn from(err: uuid::Error) -> Self {
        ProcessError::Internal(format!("uuid error: {err}"))
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(err: std::io::Error) -> Self {
        ProcessError::Internal(format!("io error: {err}"))
    }
}
