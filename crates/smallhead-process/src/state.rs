//! Process state machine and exit bookkeeping.
//!
//! See `docs/cli-process-service-design.md` §5 for the full lifecycle
//! rationale. The short version:
//!
//! ```text
//! Queued ──► Starting ──► Running ──► Stopping ──► Exited
//!    │          │
//!    ├──► Rejected
//!    └──► SpawnFailed
//!   (any non-terminal) ──► Lost
//! ```
//!
//! `Exited` is the **only** state carrying full [`ProcessExitInfo`]; the
//! other terminal variants (`Rejected / SpawnFailed / Lost / CleanupFailed`)
//! carry just the reason string so the state machine stays shallow and the
//! persistence record stays compact.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a managed execution.
///
/// Variants are intentionally split between "still alive" states (no
/// payload) and "terminal" states (each carries the reason in its payload).
/// Once a record reaches any terminal state, [`crate::manager::ProcessManager`]
/// refuses further transitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessState {
    /// Registered but not yet started. Waiter may exist; no `ProcessId` yet.
    Queued,

    /// `tokio::process::Command::spawn()` accepted and we are about to
    /// call into the child. The `ProcessId` is known at this point.
    Starting,

    /// Child is running. We may still be in the middle of capturing its
    /// first lines of stdout/stderr; "Running" means the OS process exists.
    Running,

    /// A graceful stop has been requested (timeout, user cancel, budget).
    /// The child has been signalled but not yet reaped; we still hold the
    /// `ProcessId` and may have to escalate to KILL.
    Stopping,

    /// Child has been reaped. Carries the real exit info — never silently
    /// filled with `0` for "unknown" outcomes (per design §5).
    Exited(ProcessExitInfo),

    /// Validator refused the spec **after** registration. Distinct from
    /// "validator rejected before registration" (the latter is just
    /// `ProcessError::InvalidSpec` from `start`).
    Rejected(String),

    /// `spawn()` failed (binary missing, EACCES, ENOENT, etc.). The OS
    /// process never came up.
    SpawnFailed(String),

    /// Manager lost the handle to a still-theoretically-alive process —
    /// e.g. parent future dropped before Phase 2's executor wires cleanup.
    /// State of the OS process is unknown.
    Lost(String),

    /// Reserved for Phase 2+. Marks a stop attempt whose outcome we cannot
    /// confirm (reaped vs. zombie vs. detached). Carried as a terminal
    /// state so callers always see a definitive result.
    CleanupFailed(String),
}

impl ProcessState {
    /// True if the state is terminal — no further `mark_*` transitions
    /// will be accepted.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ProcessState::Exited(_)
                | ProcessState::Rejected(_)
                | ProcessState::SpawnFailed(_)
                | ProcessState::Lost(_)
                | ProcessState::CleanupFailed(_)
        )
    }

    /// Stable string label — used by logs and persistence. Not the same
    /// as `Debug` because persistence format is part of the contract.
    pub fn as_label(&self) -> &'static str {
        match self {
            ProcessState::Queued => "queued",
            ProcessState::Starting => "starting",
            ProcessState::Running => "running",
            ProcessState::Stopping => "stopping",
            ProcessState::Exited(_) => "exited",
            ProcessState::Rejected(_) => "rejected",
            ProcessState::SpawnFailed(_) => "spawn_failed",
            ProcessState::Lost(_) => "lost",
            ProcessState::CleanupFailed(_) => "cleanup_failed",
        }
    }
}

/// Concrete exit information recorded when [`ProcessState::Exited`] is set.
///
/// `exit_code` and `signal` are kept as `Option` because Unix and Windows
/// report exit differently:
/// - Natural exit: `exit_code = Some(N)`, `signal = None`.
/// - Killed by signal: `exit_code = None`, `signal = Some(N)`.
/// - Spawn error: never reaches `Exited` — it becomes `SpawnFailed`.
///
/// `duration` is wall-clock from registration to reaping; used by callers
/// that need to surface "took 3.2s" in TUI rows without re-deriving from
/// timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessExitInfo {
    /// Process exit code if it exited naturally. `None` if killed by signal.
    pub exit_code: Option<i32>,
    /// Signal number if killed. Unix-only meaningful; Windows always leaves
    /// this `None` (Phase 2 will convert Windows termination codes into
    /// `exit_code` instead).
    pub signal: Option<i32>,
    /// Why the execution ended — separate from the raw code/signal so
    /// callers don't have to guess from a `0` whether it was a natural
    /// exit or a synthetic "I don't know" placeholder.
    pub reason: ExitReason,
    /// Wall-clock duration from registration to reaping.
    pub duration: Duration,
}

/// Why the execution transitioned into [`ProcessState::Exited`].
///
/// Kept separate from raw exit codes because "process returned 0 because it
/// finished" and "process returned 0 because the manager killed it after a
/// deadline we couldn't observe" must remain distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// Process exited on its own with whatever code/signal the OS reported.
    Natural,
    /// Deadline elapsed and the manager escalated to terminate the child.
    Timeout,
    /// Explicit user/task cancel propagated into the executor.
    Cancelled,
    /// 输出超过管理器预算，已执行停止与清理。
    OutputLimit,
    /// Could not spawn in the first place. Should never reach `Exited` —
    /// `SpawnFailed` is the right terminal state for this case. Kept for
    /// forward compatibility if a future executor decides to surface
    /// spawn-after-attach failures as `Exited` instead.
    SpawnError,
    /// Manager lost the handle; what we recorded as `Exited` is best-effort.
    Lost,
}
