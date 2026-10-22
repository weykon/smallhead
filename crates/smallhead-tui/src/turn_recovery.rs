//! Durable turn snapshots scoped by their existing artifact directory.
//!
//! Callers serialize saves and repair interrupted message history through core
//! after loading. This module preserves messages and pending steering verbatim.
use serde::{Deserialize, Serialize};
use smallhead_core::{agent_loop::SteeringRecord, AgentMessage};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const CHECKPOINT_FILENAME: &str = "turn-checkpoint.json";
pub const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCheckpoint {
    pub messages: Vec<AgentMessage>,
    pub pending: Vec<SteeringRecord>,
    pub interrupted: bool,
}

impl TurnCheckpoint {
    /// Atomically replace the checkpoint, syncing both data and directory.
    /// An error before rename leaves the previous checkpoint intact. An error
    /// syncing the directory after rename means durability is unconfirmed.
    pub fn save(&self, artifact_root: &Path) -> io::Result<()> {
        fs::create_dir_all(artifact_root)?;
        let (mut file, temporary) = create_temporary(artifact_root)?;
        let mut writer = BoundedWriter {
            inner: &mut file,
            remaining: MAX_CHECKPOINT_BYTES,
        };
        serde_json::to_writer(&mut writer, self).map_err(io::Error::other)?;
        file.sync_all()?;
        fs::rename(&temporary.0, artifact_root.join(CHECKPOINT_FILENAME))?;
        File::open(artifact_root)?.sync_all()?;
        Ok(())
    }

    /// Missing means no checkpoint. Invalid or oversized files are errors, not
    /// empty history. The read limit also holds if the file grows after stat.
    pub fn load(artifact_root: &Path) -> io::Result<Option<Self>> {
        let file = match File::open(artifact_root.join(CHECKPOINT_FILENAME)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_CHECKPOINT_BYTES {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CHECKPOINT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(too_large());
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "turn checkpoint exceeds 16 MiB")
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(too_large());
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_temporary(root: &Path) -> io::Result<(File, TemporaryFile)> {
    loop {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            ".{CHECKPOINT_FILENAME}.{}.{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, TemporaryFile(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallhead_core::ContentBlock;

    fn checkpoint(interrupted: bool) -> TurnCheckpoint {
        TurnCheckpoint {
            messages: vec![
                AgentMessage::User {
                    content: vec![ContentBlock::Text {
                        text: "original request".into(),
                    }],
                    timestamp: 123,
                },
                AgentMessage::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: vec![ContentBlock::Text {
                        text: "actual result".into(),
                    }],
                    is_error: false,
                },
            ],
            pending: vec![SteeringRecord {
                id: 7,
                content: vec![ContentBlock::Text {
                    text: "change direction".into(),
                }],
                timestamp: 456,
            }],
            interrupted,
        }
    }

    #[test]
    fn round_trip_preserves_history_pending_and_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let original = checkpoint(true);
        original.save(dir.path()).unwrap();
        let loaded = TurnCheckpoint::load(dir.path()).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&original).unwrap(),
            serde_json::to_value(&loaded).unwrap()
        );
        assert_eq!(loaded.pending[0].id, 7);
        assert_eq!(loaded.pending[0].timestamp, 456);
        assert!(loaded.interrupted);
    }

    #[test]
    fn roots_are_isolated_and_replacement_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        assert!(TurnCheckpoint::load(&first).unwrap().is_none());
        checkpoint(true).save(&first).unwrap();
        assert!(TurnCheckpoint::load(&second).unwrap().is_none());
        TurnCheckpoint {
            messages: vec![],
            pending: vec![],
            interrupted: false,
        }
        .save(&first)
        .unwrap();
        let loaded = TurnCheckpoint::load(&first).unwrap().unwrap();
        assert!(loaded.messages.is_empty() && loaded.pending.is_empty() && !loaded.interrupted);
        assert_eq!(fs::read_dir(first).unwrap().count(), 1);
    }

    #[test]
    fn malformed_and_oversized_files_are_not_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CHECKPOINT_FILENAME);
        for bytes in [b"{".as_slice(), b"{}", b"null"] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                TurnCheckpoint::load(dir.path()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        File::create(path)
            .unwrap()
            .set_len(MAX_CHECKPOINT_BYTES + 1)
            .unwrap();
        assert_eq!(
            TurnCheckpoint::load(dir.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejected_save_keeps_previous_snapshot_and_removes_temporary() {
        let dir = tempfile::tempdir().unwrap();
        checkpoint(true).save(dir.path()).unwrap();
        let before = fs::read(dir.path().join(CHECKPOINT_FILENAME)).unwrap();
        let mut oversized = checkpoint(false);
        oversized.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "x".repeat(MAX_CHECKPOINT_BYTES as usize),
            }],
            timestamp: 789,
        });
        assert!(oversized.save(dir.path()).is_err());
        assert_eq!(
            fs::read(dir.path().join(CHECKPOINT_FILENAME)).unwrap(),
            before
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn readers_observe_complete_snapshots_during_replacement() {
        let dir = tempfile::tempdir().unwrap();
        checkpoint(true).save(dir.path()).unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for index in 0..20 {
                    checkpoint(index % 2 == 0).save(dir.path()).unwrap();
                }
            });
            for _ in 0..100 {
                let loaded = TurnCheckpoint::load(dir.path()).unwrap().unwrap();
                assert_eq!(loaded.messages.len(), 2);
                assert_eq!(loaded.pending.len(), 1);
                assert_eq!(loaded.pending[0].id, 7);
            }
            writer.join().unwrap();
        });
    }
}
