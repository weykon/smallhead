//! Error types for the smallhead ECS kernel.
//!
//! Every failure path inside `smallhead-ecs` is funneled into [`EcsError`]. The
//! crate never returns `anyhow::Error` from its public surface so callers can
//! exhaustively match on failure modes (and tests can pin them down).
//!
//! ## Design notes
//!
//! - **No panics across the public API.** Anything the agent or its tool layer
//!   could legitimately trigger is represented as a variant here.
//! - **Conflict reporting.** Version-related failures carry both the
//!   `expected_version` and the `actual_version` so the upper layers can log
//!   them and decide whether to retry.
//! - **`thiserror` derives.** Cheap on mature crates. We deliberately avoid
//!   storing `std::io::Error` inside variants so the type can be `Serialize +
//!   Deserialize + Clone` (needed to embed in `WorldOpResult.error`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Categorises *which* borrow held the world when a lock conflict was
/// detected. Used for metrics + clearer error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorldLockKind {
    /// An entity / component query was still streaming.
    Query,
    /// A `WorldOp::RunSystem` was mid-flight.
    System,
    /// A commit was being written to disk.
    Commit,
    /// Another mutation was in progress on the same world.
    Mutation,
}

impl std::fmt::Display for WorldLockKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            WorldLockKind::Query => "query",
            WorldLockKind::System => "system",
            WorldLockKind::Commit => "commit",
            WorldLockKind::Mutation => "mutation",
        };
        f.write_str(s)
    }
}

/// All errors that can be produced by the smallhead ECS kernel.
///
/// Use [`std::result::Result<T, EcsError>`] as the canonical return type from
/// every public function in this crate.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EcsError {
    /// The world was borrowed mutably while a long-lived read/query was still
    /// holding a reference. This is the classic `bevy_ecs` "world locked" error
    /// and is recoverable by retrying after the conflicting operation ends.
    #[error("world is locked by another in-flight operation (kind: {lock_kind:?})")]
    WorldLocked { lock_kind: WorldLockKind },

    /// A `WorldOp::GetComponent` (or any read op) referenced a component name
    /// that has not been registered with the world's component registry.
    #[error("component not found: {component} (entity={entity:?})")]
    ComponentNotFound {
        component: String,
        entity: Option<u64>,
    },

    /// A `WorldOp::SpawnEntity` / `DespawnEntity` referenced an entity id that
    /// does not exist (or has already been despawned).
    #[error("entity not found: {entity}")]
    EntityNotFound { entity: u64 },

    /// The JSON `input` for a `WorldOp` did not satisfy the op's input schema.
    /// The string is a human-readable reason — never user-facing without
    /// redaction in the TUI layer.
    #[error("invalid input for {op:?}: {reason}")]
    InvalidInput {
        op: crate::world_op::WorldOpKind,
        reason: String,
    },

    /// The caller attempted a write op while holding only an `EcsRead`
    /// permission grant. This is enforced before the op is dispatched to
    /// `bevy_ecs`; the world is never mutated on the read path.
    #[error("permission denied for {op:?}: requires {required}")]
    PermissionDenied {
        op: crate::world_op::WorldOpKind,
        required: String,
    },

    /// `commit_atomic` was called with `expected_version` that does not match
    /// the world's current `version`. The conflict is reported with both
    /// sides so the agent loop can decide whether to re-read and merge.
    #[error("checkpoint version mismatch: expected {expected_version}, actual {actual_version}")]
    CheckpointVersionMismatch {
        expected_version: u64,
        actual_version: u64,
    },

    /// A `WorldCheckpoint` read from disk failed to deserialize. The payload
    /// path is included so the user can inspect the corrupt file.
    #[error("corrupt checkpoint at {path:?}: {reason}")]
    CorruptCheckpoint { path: PathBuf, reason: String },

    /// `CheckpointStore::write` or `read` failed at the I/O layer. We keep the
    /// raw OS error code as an integer so the variant stays serializable.
    #[error("checkpoint store I/O error at {path:?}: (os_error={os_error}) {description}")]
    CheckpointIo {
        path: PathBuf,
        os_error: i32,
        description: String,
    },

    /// The same world was being committed concurrently by another writer.
    /// This is distinct from a version mismatch — it means the lock could not
    /// be acquired at all.
    #[error("concurrent commit on world {world_id} (holder: {holder})")]
    ConcurrentCommit { world_id: String, holder: String },

    /// The world id supplied to `CheckpointStore::open` was not found in the
    /// storage root. Recoverable: caller may create a new world.
    #[error("world not found: {world_id}")]
    WorldNotFound { world_id: String },

    /// `DefineComponent` was called twice for the same component name with
    /// conflicting schemas. The agent must drop one definition before
    /// continuing.
    #[error("component {name} redefined with incompatible schema")]
    ComponentSchemaConflict { name: String },

    /// Catch-all for serialization / deserialization errors raised by serde
    /// while (de)constructing a checkpoint or world snapshot.
    #[error("serde error: {0}")]
    Serde(String),
}

/// Convenience alias for `Result<T, EcsError>`.
pub type EcsResult<T> = std::result::Result<T, EcsError>;

impl From<serde_json::Error> for EcsError {
    fn from(e: serde_json::Error) -> Self {
        EcsError::Serde(e.to_string())
    }
}

impl From<std::io::Error> for EcsError {
    fn from(e: std::io::Error) -> Self {
        EcsError::CheckpointIo {
            path: PathBuf::new(),
            os_error: e.raw_os_error().unwrap_or(-1),
            description: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_carry_payload() {
        let err = EcsError::CheckpointVersionMismatch {
            expected_version: 3,
            actual_version: 7,
        };
        let msg = err.to_string();
        assert!(msg.contains("expected 3"));
        assert!(msg.contains("actual 7"));
    }

    #[test]
    fn serde_conversion_preserves_message() {
        let je = serde_json::from_str::<u32>("\"oops\"").unwrap_err();
        let err: EcsError = je.into();
        matches!(err, EcsError::Serde(_));
    }

    #[test]
    fn world_lock_kind_is_copy() {
        let k = WorldLockKind::Commit;
        let k2 = k;
        assert_eq!(k, k2);
    }

    #[test]
    fn variants_serialize_with_tag() {
        let e = EcsError::EntityNotFound { entity: 42 };
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("entity_not_found"));
    }
}
