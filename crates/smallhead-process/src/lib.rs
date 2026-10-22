//! `smallhead-process` — managed OS process lifecycle for small-head.
//!
//! [`ProcessManager`] owns the registry and state machine plus an internal
//! executor driving spawn → wait / timeout / cancel → terminal. Execution
//! streams, events and final metadata are persisted with explicit budgets;
//! [`service::ServiceManager`] builds verified session services on top.
//! See `docs/cli-process-service-design.md` for the full design.
//!
//! ## Identity
//!
//! Two distinct identities are exposed:
//!
//! - [`ExecutionId`] — one logical execution attempt. UUID v7 so it sorts
//!   by creation time. Allocated **before** validation, queueing, or
//!   spawn — even refusal / spawn-failure records carry one.
//! - [`ProcessId`] — the OS PID of a successfully spawned child.
//!   Distinct from `ExecutionId` because restarts reuse the logical id
//!   but get a brand new OS PID.
//!
//! ## State machine
//!
//! ```text
//! Queued ──► Starting ──► Running ──► Stopping ──► Exited
//!    │          │
//!    ├──► Rejected
//!    └──► SpawnFailed
//!   (any non-terminal) ──► Lost
//! ```
//!
//! Transitions are internal to the manager/executor. External callers use
//! start, stop, inspect, wait_for and shutdown; they cannot declare a live
//! child exited by editing the state machine.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub(crate) mod executor;
pub mod id;
pub mod limits;
pub mod manager;
pub mod output;
pub(crate) mod persist;
pub mod record;
pub mod service;
pub mod spec;
pub mod state;
mod storage;

pub use error::ProcessError;
pub use id::{ExecutionId, ProcessId};
pub use limits::ProcessLimits;
pub use manager::ProcessManager;
pub use output::{OutputPage, OutputStream, MAX_OUTPUT_PAGE_BYTES};
pub use record::{ExecutionMetadata, ProcessRecord};
pub use spec::{ExecutionScope, ProcessSpec};
pub use state::{ExitReason, ProcessExitInfo, ProcessState};
