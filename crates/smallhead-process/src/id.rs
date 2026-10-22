//! Identity types for managed process executions.
//!
//! Two distinct identities are required because the same logical execution
//! attempt can outlive an OS process (restarts get a new PID) and conversely
//! a single OS PID can be reused across completely unrelated executions.
//! See [`ExecutionId`] vs [`ProcessId`] for the rationale.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Unique identifier for one logical execution attempt.
///
/// An attempt is registered **before** validation, queueing, or spawn — see
/// [`crate::manager::ProcessManager::start`]. Refusal, spawn failure, timeout,
/// cancel and success all reuse the same `ExecutionId` so callers can `inspect`
/// the outcome regardless of where the lifecycle ended.
///
/// Backed by **UUID v7** so the id sorts lexicographically by creation time,
/// which makes registry listings and persistence indexes naturally ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecutionId(uuid::Uuid);

impl ExecutionId {
    /// Allocate a fresh [`ExecutionId`] backed by `Uuid::now_v7()`.
    ///
    /// Time-ordered so a `BTreeMap<ExecutionId, _>` index would naturally
    /// order by insertion time without a separate timestamp column.
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Borrow the underlying [`uuid::Uuid`] (useful for logging/serialization
    /// variants that take `Uuid` directly).
    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::str::FromStr for ExecutionId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(uuid::Uuid::parse_str(value)?))
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // hyphenated form — matches what every other crate in this workspace
        // does for v7 UUIDs.
        self.0.fmt(f)
    }
}

impl From<uuid::Uuid> for ExecutionId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}

/// OS-level PID of the managed process.
///
/// Distinct from [`ExecutionId`] because restarts keep the same logical
/// identity but get a brand new OS PID (and the reverse: an unrelated OS
/// PID can theoretically be reused for a new execution once the previous
/// one exited). Future `ServiceManager` work will use `ExecutionId` for
/// cross-restart correlation while `ProcessId` only describes the live OS
/// instance currently holding that logical attempt.
///
/// PID `0` is reserved by the kernel and never used as a real PID; we keep
/// the full `u32` range for now — filtering happens at the syscall boundary
/// in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessId(u32);

impl ProcessId {
    /// Construct a `ProcessId` from a raw OS PID value.
    pub fn from_pid(pid: u32) -> Self {
        Self(pid)
    }

    /// Borrow the underlying OS PID.
    pub fn as_pid(&self) -> u32 {
        self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
