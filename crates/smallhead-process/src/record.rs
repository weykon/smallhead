//! Per-execution record stored in [`crate::manager::ProcessManager`].
//!
//! One [`ProcessRecord`] per `ExecutionId`. The record is the source of
//! truth — neither the executor nor the TUI caches its own copy.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::id::{ExecutionId, ProcessId};
use crate::spec::ProcessSpec;
use crate::state::ProcessState;

/// Serde helper for [`SystemTime`] fields — encodes as seconds since the
/// UNIX epoch (truncating sub-second precision; the runtime never needs
/// nanosecond granularity on disk). `SystemTime` does not implement
/// `Serialize` / `Deserialize` out of the box, hence this shim.
mod system_time_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map_err(serde::ser::Error::custom)?
            .as_secs();
        s.serialize_u64(secs)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

/// Serde helper for `Option<SystemTime>` — null when unset.
mod system_time_opt_serde {
    use super::system_time_serde;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => system_time_serde::serialize(t, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        Option::<u64>::deserialize(d)
            .map(|opt| opt.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)))
    }
}

/// Serde helper for `PathBuf` encoded as a string. `PathBuf` would
/// serialize via its own impl, but tagging the encoding explicitly as
/// "UTF-8 string" keeps the on-disk JSON stable across platforms.
mod path_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(p: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => s.serialize_some(&p.to_string_lossy().into_owned()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
        Option::<String>::deserialize(d).map(|opt| opt.map(PathBuf::from))
    }
}

/// Structured intent and policy attached to an execution.
///
/// This is separate from [`ProcessSpec`]: the spec tells the OS what to run,
/// while metadata records why the Agent or user started it and which
/// higher-level policy should interpret the result. The manager persists it
/// but does not evaluate completion policy yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionMetadata {
    /// Stable source label such as `agent_tool`, `tui`, `service`, or
    /// `checker`. Free-form for now so harness layers can add sources without
    /// a storage migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Tool call id from the Agent protocol, when this execution came from a
    /// model tool invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Human intent for this execution. Prefer this over parsing command text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,

    /// Future-facing structured completion contract. Stored as JSON so the
    /// first runtime step can preserve policy before implementing evaluators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_policy: Option<serde_json::Value>,
}

/// Mutable per-execution record held in the manager's registry.
///
/// `Clone` is implemented deliberately: callers that want a stable
/// snapshot (e.g. for TUI rendering) can clone out without holding the
/// registry lock. In-place mutation happens only via
/// internal `ProcessManager::mark_*` helpers so the state machine is
/// enforced in one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// 所属任务或服务，用于定向取消和日志关联。
    #[serde(default)]
    pub owner: Option<String>,

    /// Structured intent, source and completion policy supplied by the caller.
    #[serde(default)]
    pub metadata: ExecutionMetadata,

    /// Logical identity of this execution attempt. Never changes after
    /// registration.
    pub id: ExecutionId,

    /// Spec the caller supplied. Kept around so that `inspect()` can
    /// render intent without re-deriving it from logs.
    pub spec: ProcessSpec,

    /// Current lifecycle state. Transitions are driven exclusively by
    /// `ProcessManager::mark_*`.
    pub state: ProcessState,

    /// OS PID once spawn succeeds. `None` while in `Queued` /
    /// `Rejected` / `SpawnFailed` and during the brief window between
    /// `spawn()` returning and the executor capturing the child.
    pub pid: Option<ProcessId>,

    /// Wall-clock registration time — the moment the `ExecutionId` was
    /// allocated, before validation. Always set.
    #[serde(with = "system_time_serde")]
    pub registered_at: SystemTime,

    /// First moment we observed `Running` (or `Stopping` if a stop raced
    /// the running state). `None` until then.
    #[serde(with = "system_time_opt_serde", default)]
    pub started_at: Option<SystemTime>,

    /// First terminal transition time. `None` while still alive.
    #[serde(with = "system_time_opt_serde", default)]
    pub finished_at: Option<SystemTime>,

    /// Directory the executor (Phase 2) will write bounded stdout/stderr
    /// logs into. Allocated by the manager at registration; `None` only
    /// for legacy records loaded before Track B landed.
    #[serde(with = "path_serde", default)]
    pub log_dir: Option<PathBuf>,
}

impl ProcessRecord {
    /// Allocate a fresh record in [`ProcessState::Queued`] state.
    ///
    /// `registered_at` is `SystemTime::now()` at call time; the caller
    /// already has the `ExecutionId` from `ProcessManager::start`.
    pub fn new(id: ExecutionId, spec: ProcessSpec, log_dir: Option<PathBuf>) -> Self {
        Self {
            owner: None,
            metadata: ExecutionMetadata::default(),
            id,
            spec,
            state: ProcessState::Queued,
            pid: None,
            registered_at: SystemTime::now(),
            started_at: None,
            finished_at: None,
            log_dir,
        }
    }

    /// Convenience: has this record reached a terminal state already?
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}
