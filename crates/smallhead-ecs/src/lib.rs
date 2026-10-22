//! # `smallhead-ecs`
//!
//! The ECS kernel for the `smallhead` agent. This crate is the *only* place
//! in the workspace that depends on `bevy_ecs` (vendored from `big-head`), so
//! the rest of the agent never sees the engine's types directly.
//!
//! ## What lives here
//!
//! - [`world_op`] — `WorldOpKind` (the 11 closed enumeration), `WorldOpTool`,
//!   `WorldOpResult`. This is the only mutating surface an agent (or any
//!   caller) sees.
//! - [`checkpoint`] — `WorldCheckpoint`, `CheckpointStore` trait, and a
//!   filesystem-backed implementation that performs atomic commits
//!   (`write tmp → fsync → rename → fsync dir`).
//! - [`error`] — `EcsError` + `EcsResult<T>`, the canonical error type of the
//!   crate.
//!
//! ## What is deliberately *not* here
//!
//! - The agent loop itself. That's `smallhead-core`'s job.
//! - Permission evaluation. That's `smallhead-permission`'s job; this crate
//!   only consumes the `Permission` token.
//! - Persistence of the *action log* (the JSONL append-only stream). That
//!   lives next to `CheckpointStore` callers (usually `smallhead-session`)
//!   and only its digest is embedded in each checkpoint.
//!
//! ## Origin
//!
//! Adapted from `big-head` — specifically the `big-head-save` atomic commit
//! pattern and the `WorldOp` enumeration. The vendored `bevy_ecs` is shared
//!   with `big-head` via a path dependency (see `Cargo.toml`).
//!
//! Source: `ARCHITECTURE.md` §4 (ECS Kernel Integration).

#![warn(missing_docs)]

pub mod checkpoint;
pub mod world_op;

pub mod error;

// -------------------------------------------------------------------------
// Re-exports — keep the surface small but stable.
// -------------------------------------------------------------------------

pub use error::{EcsError, EcsResult, WorldLockKind};

pub use world_op::{OpPermission, WorldOpKind, WorldOpResult, WorldOpTool};

pub use checkpoint::{
    default_world_root, sha256 as checkpoint_sha256, CheckpointStore, CommitResult,
    FileCheckpointStore, WorldCheckpoint,
};
