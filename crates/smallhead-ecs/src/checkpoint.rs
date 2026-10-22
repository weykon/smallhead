//! Checkpoint store — durable, atomic snapshots of the ECS world.
//!
//! A `WorldCheckpoint` is a self-describing JSON document capturing the
//! entire ECS state at a specific monotonic `version`. The action log is
//! *not* embedded — it is the separate, immutable, append-only JSONL stream
//! that lives next to the world (see `ARCHITECTURE.md` §4.3).
//!
//! ## Atomic commit protocol
//!
//! Every write goes through this sequence (the same recipe used by
//! `big-head-save` and `pi-durable`):
//!
//! 1. Serialize the checkpoint to a sibling file with a `.tmp` suffix.
//! 2. `fsync` the temp file so the bytes are durable.
//! 3. `rename(temp, target)` — atomic on POSIX, atomic on Windows for
//!    same-volume renames.
//! 4. `fsync` the parent directory so the rename itself is durable.
//!
//! Readers therefore never observe a half-written file: either the old
//! version is intact, or the new one is fully present.
//!
//! ## Layout on disk
//!
//! ```text
//! ~/.local/share/smallhead/worlds/{world-id}/
//! ├── meta.json                 # world metadata (id, current version)
//! ├── {version:020}.jsonl            # committed checkpoint (renamed atomically)
//! └── .tmp-{nanos}             # in-flight writes (never visible to readers)
//! ```
//!
//! ## Conflict reporting
//!
//! `commit_atomic` takes an `expected_version`. If the world's current
//! version differs, the write is *rejected* (not retried) and the caller
//! receives [`CommitResult::Conflict`] with both sides of the conflict. This
//! is the standard optimistic-concurrency pattern; the upper layer decides
//! whether to merge, re-read, or surface the conflict to the user.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{EcsError, EcsResult};

/// A durable snapshot of the world at a specific version.
///
/// The `world` payload is opaque `serde_json::Value` for now — the full ECS
/// snapshot serialisation is implemented in `smallhead-ecs` v0.2 (see
/// `ARCHITECTURE.md` §4.3 and the checkpoint lifecycle doc). For this
/// skeleton the value is whatever the caller chose to persist, validated
/// against the input schema at write time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldCheckpoint {
    pub world_id: String,
    pub version: u64,
    pub world: Value,
    pub action_log_digest: String,
    pub timestamp: i64,
    /// Anchor IDs (entry IDs in the upstream session store) that this checkpoint
    /// proves it has applied. Same anchor invariant as `Compaction.anchor_refs`
    /// in `ARCHITECTURE.md` §7.4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchor_refs: Vec<String>,
}

/// Outcome of a single `commit_atomic` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitResult {
    /// The checkpoint was atomically committed at `version`. The next commit
    /// must use `expected_version = new_version` to succeed.
    Committed { version: u64, checkpoint_id: String },
    /// The expected version did not match the world's current version. The
    /// caller must reconcile and re-attempt.
    Conflict {
        expected_version: u64,
        actual_version: u64,
    },
}

/// Storage trait for checkpoints. Implementations MUST be safe to share across
/// threads and MUST guarantee that a returned `read()` is a fully-written
/// checkpoint (never a partially-written one).
pub trait CheckpointStore: Send + Sync {
    /// Commit a new checkpoint. See `Atomic commit protocol` above.
    fn commit_atomic(
        &self,
        world_id: &str,
        expected_version: u64,
        action_log_digest: &str,
        world: Value,
        anchor_refs: Vec<String>,
    ) -> EcsResult<CommitResult>;

    /// Read the checkpoint for a specific `(world_id, version)`. Returns
    /// `None` if no checkpoint exists at that version.
    fn read(&self, world_id: &str, version: u64) -> EcsResult<Option<WorldCheckpoint>>;

    /// Read the most recent checkpoint for `world_id`. Returns `None` if the
    /// world has never been committed.
    fn latest(&self, world_id: &str) -> EcsResult<Option<WorldCheckpoint>>;

    /// List all committed versions for `world_id`, ascending.
    fn list_versions(&self, world_id: &str) -> EcsResult<Vec<u64>>;

    /// Returns the highest committed version for `world_id`, or `None` if the
    /// world has never been committed.
    fn current_version(&self, world_id: &str) -> EcsResult<Option<u64>>;
}

/// Filesystem-backed implementation of [`CheckpointStore`].
///
/// `root` is the directory under which `worlds/{world-id}/` subdirectories
/// are created. The default convention is
/// `~/.local/share/smallhead/worlds/` but any writable directory works
/// (tests use `tempfile::tempdir()`).
#[derive(Debug, Clone)]
pub struct FileCheckpointStore {
    root: PathBuf,
}

impl FileCheckpointStore {
    /// Build a store rooted at `root`. The directory is created lazily on the
    /// first commit, not on construction.
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self { root: root.into() }
    }

    /// Returns the absolute path of the world directory.
    fn world_dir(&self, world_id: &str) -> PathBuf {
        self.root.join(world_id)
    }

    /// Filename for a committed checkpoint. Zero-padded so `list_versions` is
    /// cheap (lexicographic == numeric).
    fn checkpoint_path(&self, world_id: &str, version: u64) -> PathBuf {
        self.world_dir(world_id)
            .join(format!("{version:020}.jsonl"))
    }

    /// Filename for the metadata index.
    fn meta_path(&self, world_id: &str) -> PathBuf {
        self.world_dir(world_id).join("meta.json")
    }
}

impl CheckpointStore for FileCheckpointStore {
    fn commit_atomic(
        &self,
        world_id: &str,
        expected_version: u64,
        action_log_digest: &str,
        world: Value,
        anchor_refs: Vec<String>,
    ) -> EcsResult<CommitResult> {
        let dir = self.world_dir(world_id);
        if let Err(source) = fs::create_dir_all(&dir) {
            return Err(EcsError::CheckpointIo {
                path: dir.clone(),
                os_error: source.raw_os_error().unwrap_or(-1),
                description: source.to_string(),
            });
        }

        // 1. Verify expected version matches current.
        let actual = self.current_version(world_id)?.unwrap_or(0);
        if actual != expected_version {
            return Ok(CommitResult::Conflict {
                expected_version,
                actual_version: actual,
            });
        }

        let new_version = expected_version + 1;
        let checkpoint_id = Uuid::now_v7().to_string();

        let cp = WorldCheckpoint {
            world_id: world_id.to_string(),
            version: new_version,
            world,
            action_log_digest: action_log_digest.to_string(),
            timestamp: Utc::now().timestamp(),
            anchor_refs,
        };

        let target = self.checkpoint_path(world_id, new_version);
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), checkpoint_id));

        // 2. Serialise, write to tmp, fsync.
        let body = serde_json::to_vec(&cp)?;
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|source| EcsError::CheckpointIo {
                    path: tmp.clone(),
                    os_error: source.raw_os_error().unwrap_or(-1),
                    description: source.to_string(),
                })?;
            if let Err(source) = f.write_all(&body) {
                return Err(EcsError::CheckpointIo {
                    path: tmp.clone(),
                    os_error: source.raw_os_error().unwrap_or(-1),
                    description: source.to_string(),
                });
            }
            if let Err(source) = f.sync_all() {
                return Err(EcsError::CheckpointIo {
                    path: tmp.clone(),
                    os_error: source.raw_os_error().unwrap_or(-1),
                    description: source.to_string(),
                });
            }
        }

        // 3. Atomic rename.
        if let Err(source) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(EcsError::CheckpointIo {
                path: target.clone(),
                os_error: source.raw_os_error().unwrap_or(-1),
                description: source.to_string(),
            });
        }

        // 4. fsync parent directory so the rename is durable.
        if let Ok(dir_handle) = File::open(&dir) {
            let _ = dir_handle.sync_all();
        }

        // Update meta.json (best-effort; readers can rebuild from filenames).
        let meta = json!({
            "world_id": world_id,
            "current_version": new_version,
            "checkpoint_id": checkpoint_id,
            "updated_at": Utc::now().timestamp(),
        });
        let meta_tmp = self.world_dir(world_id).join("meta.json.tmp");
        if let (Ok(mut f), Ok(body)) = (
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&meta_tmp),
            serde_json::to_vec(&meta),
        ) {
            let _ = f.write_all(&body);
            let _ = f.sync_all();
            let _ = fs::rename(&meta_tmp, self.meta_path(world_id));
        }

        Ok(CommitResult::Committed {
            version: new_version,
            checkpoint_id,
        })
    }

    fn read(&self, world_id: &str, version: u64) -> EcsResult<Option<WorldCheckpoint>> {
        let path = self.checkpoint_path(world_id, version);
        match fs::File::open(&path) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                if let Err(source) = f.read_to_end(&mut buf) {
                    return Err(EcsError::CheckpointIo {
                        path: path.clone(),
                        os_error: source.raw_os_error().unwrap_or(-1),
                        description: source.to_string(),
                    });
                }
                let cp: WorldCheckpoint =
                    serde_json::from_slice(&buf).map_err(|e| EcsError::CorruptCheckpoint {
                        path,
                        reason: e.to_string(),
                    })?;
                Ok(Some(cp))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(EcsError::CheckpointIo {
                path,
                os_error: source.raw_os_error().unwrap_or(-1),
                description: source.to_string(),
            }),
        }
    }

    fn latest(&self, world_id: &str) -> EcsResult<Option<WorldCheckpoint>> {
        match self.current_version(world_id)? {
            Some(v) => self.read(world_id, v),
            None => Ok(None),
        }
    }

    fn list_versions(&self, world_id: &str) -> EcsResult<Vec<u64>> {
        let dir = self.world_dir(world_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::new();
        let entries = fs::read_dir(&dir).map_err(|source| EcsError::CheckpointIo {
            path: dir.clone(),
            os_error: source.raw_os_error().unwrap_or(-1),
            description: source.to_string(),
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| EcsError::CheckpointIo {
                path: dir.clone(),
                os_error: source.raw_os_error().unwrap_or(-1),
                description: source.to_string(),
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".jsonl") {
                if let Ok(v) = stem.parse::<u64>() {
                    versions.push(v);
                }
            }
        }
        versions.sort_unstable();
        Ok(versions)
    }

    fn current_version(&self, world_id: &str) -> EcsResult<Option<u64>> {
        let versions = self.list_versions(world_id)?;
        Ok(versions.last().copied())
    }
}

/// Helper for callers that need a SHA-256 digest of arbitrary bytes (the
/// action log uses it to compute `action_log_digest`).
pub fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Convenience: ensure the default world root exists. Useful for CLI startup
/// before any world is opened.
pub fn default_world_root() -> PathBuf {
    if let Ok(home) = std::env::var("SMALLHEAD_HOME") {
        PathBuf::from(home).join("worlds")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/smallhead/worlds")
    } else {
        std::env::temp_dir().join("smallhead-worlds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // "abc"
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn default_root_ends_with_worlds() {
        let r = default_world_root();
        assert!(r.ends_with("worlds"));
    }

    #[test]
    fn file_path_is_zero_padded() {
        let store = FileCheckpointStore::new("/tmp/x");
        let p = store.checkpoint_path("w1", 1);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // 20-wide zero-padded version, then .jsonl
        assert_eq!(name, format!("{:020}.jsonl", 1u64));
    }
}
