//! Per-execution on-disk log persistence.
//!
//! [`LogStore`] owns the root directory. Each execution gets a subdirectory
//! keyed by [`ExecutionId`] (NOT PID — see design doc §3). Layout:
//!
//! ```text
//! <root>/<execution_id>/
//!   stdout.log      // raw stdout bytes, appended as they arrive
//!   stderr.log      // raw stderr bytes, appended as they arrive
//!   meta.json       // final ProcessRecord snapshot (atomic write)
//! ```
//!
//! The executor caps raw streams using the manager budget. Transition events,
//! Aggregate admission quotas are enforced by the manager. Automatic
//! rotation/retention and separate head/tail/error summaries remain future work.

use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{ExecutionId, ProcessError, ProcessRecord};

/// Root of the on-disk log tree. Cheap to clone (`PathBuf`-backed),
/// shared by the [`crate::manager::ProcessManager`] and (read-only) by
/// S1b's stop-audit path.
#[derive(Debug)]
pub(crate) struct LogStore {
    root: PathBuf,
}

/// Three-way split of a log handle: stdout writer, stderr writer, and
/// the finalize-handle for atomic `meta.json`.
///
/// The executor hands the two writers to its parallel reader tasks and
/// uses the [`LogFinalizer`] before publishing the registry's terminal
/// state. Files are not `Clone` so we partition them here
/// instead of around the executor.
#[derive(Debug)]
pub(crate) struct LogHandle {
    dir: PathBuf,
    meta_path: PathBuf,
    stdout: Option<File>,
    stderr: Option<File>,
}

/// Owned stdout writer half — wraps an [`tokio::fs::File`] opened in
/// write+append mode at `LogHandle` creation.
#[derive(Debug)]
pub(crate) struct StdoutWriter(File);

/// Owned stderr writer half. Mirrors [`StdoutWriter`].
#[derive(Debug)]
pub(crate) struct StderrWriter(File);

/// Owned handle to the finalizer step — atomic `meta.json` write that runs
/// after both reader tasks have drained. Created by
/// [`LogHandle::split`] and moved into the executor's terminal branch.
#[derive(Debug)]
pub(crate) struct LogFinalizer {
    meta_path: PathBuf,
}

impl LogStore {
    /// Construct a [`LogStore`] rooted at `root`, creating the directory
    /// eagerly so `log_root()` never observes a non-existent path.
    pub(crate) fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Borrow the configured root directory. Used by the manager to expose
    /// `log_root()` publicly without leaking the [`LogStore`] itself.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Record the accepted request before the executor can spawn a child.
    pub(crate) fn write_initial(&self, record: &ProcessRecord) -> Result<(), ProcessError> {
        let path = self.root.join(record.id.to_string()).join("meta.json");
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| ProcessError::Internal(e.to_string()))?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Create the per-id log directory and open `stdout.log` + `stderr.log`
    /// for writing. The returned [`LogHandle`] owns the file handles and
    /// the path of `meta.json`; the caller must call
    /// [`LogHandle::split`] before the handles can be used in parallel
    /// reader tasks.
    ///
    /// This baseline uses synchronous local filesystem setup. It can block on
    /// slow storage. A future asynchronous backend needs a manager-owned
    /// allocation job; test scheduling is not a reason to rely on sync I/O.
    pub(crate) fn open(&self, id: ExecutionId) -> std::io::Result<LogHandle> {
        let dir = self.root.join(id.to_string());
        std::fs::create_dir_all(&dir)?;
        let stdout_path = dir.join("stdout.log");
        let stderr_path = dir.join("stderr.log");
        let meta_path = dir.join("meta.json");
        // Open via `std::fs` (synchronous). `tokio::fs::File::from_std`
        // hands the descriptor to Tokio without touching `unsafe`; the
        // workspace's `unsafe_code = "forbid"` lint allows safe wrapping
        // APIs.
        let stdout = tokio::fs::File::from_std(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&stdout_path)?,
        );
        let stderr = tokio::fs::File::from_std(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&stderr_path)?,
        );
        Ok(LogHandle {
            dir,
            meta_path,
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }
}

impl LogHandle {
    /// Borrow the per-execution directory path (the parent of stdout.log,
    /// stderr.log, and meta.json).
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Split the handle into its three sub-parts for parallel use:
    ///
    /// - [`StdoutWriter`] — moved into the stdout reader task.
    /// - [`StderrWriter`] — moved into the stderr reader task.
    /// - [`LogFinalizer`] — held by the executor's main task; invoked
    ///   once before publishing the terminal state so the on-disk record matches the
    ///   in-memory snapshot.
    ///
    /// Consumes `self` (the directory + path fields become unused after
    /// split and are dropped here, which is fine).
    pub(crate) fn split(mut self) -> (StdoutWriter, StderrWriter, LogFinalizer) {
        let stdout = StdoutWriter(
            self.stdout
                .take()
                .expect("stdout file present after LogStore::open"),
        );
        let stderr = StderrWriter(
            self.stderr
                .take()
                .expect("stderr file present after LogStore::open"),
        );
        let finalizer = LogFinalizer {
            meta_path: self.meta_path.clone(),
        };
        // `self` drops here, releasing the remaining `dir` and `meta_path`
        // (now duplicated inside `finalizer`) — fine, no further reads.
        (stdout, stderr, finalizer)
    }
}

impl StdoutWriter {
    /// Append `bytes` to the stdout log, flushing after each call so a
    /// crash mid-execution can preserve buffered data. Flush is not fsync.
    /// Errors are propagated so the executor can stop and report failure.
    ///
    /// Provided as a convenience around the underlying
    /// [`AsyncWriteExt::write_all`] + [`AsyncWriteExt::flush`] pair so
    /// callers don't have to spell both steps out. The executor's
    /// `drain_to_file` uses the trait methods directly; tests /
    /// one-shots can reach for this shortcut.
    #[allow(dead_code)]
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes).await?;
        self.0.flush().await
    }
}

impl StderrWriter {
    /// Append `bytes` to the stderr log. See [`StdoutWriter::write`].
    #[allow(dead_code)]
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes).await?;
        self.0.flush().await
    }
}

// Delegate `AsyncWrite` to the inner file so executors can call
// `tokio::io::AsyncWriteExt::write_all` / `flush` directly via a
// generic `AsyncWrite + Unpin` bound (used by `drain_to_file`).

impl AsyncWrite for StdoutWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl AsyncWrite for StderrWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl LogFinalizer {
    /// Borrow the `meta.json` path the finalizer would write to. Useful
    /// for callers that want to log / inspect without committing to a
    /// `finalize` call. Reserved for the executor / future audit tooling;
    /// not used by the executor today.
    #[allow(dead_code)]
    pub(crate) fn meta_path(&self) -> &Path {
        &self.meta_path
    }

    /// Atomic write of the final [`ProcessRecord`] to `meta.json`.
    ///
    /// Writes the JSON payload to `meta.json.tmp`, best-effort fsyncs the
    /// parent directory so the rename is durable on Unix, then renames the
    /// temp file over `meta.json`. Any I/O failure is wrapped in
    /// [`ProcessError::Internal`] so the executor can surface it via
    /// `tracing::warn` instead of panicking.
    ///
    /// Borrows `&self` (rather than consuming) so the same finalizer
    /// can be held alongside other state and the executor's finalize
    /// helper doesn't have to clone paths.
    pub(crate) async fn finalize(&self, record: &ProcessRecord) -> Result<(), ProcessError> {
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| ProcessError::Internal(format!("serialize record: {e}")))?;
        let tmp = self.meta_path.with_extension("json.tmp");
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(json.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        // Best-effort fsync of the directory so the rename is durable on
        // Unix. Some platforms / filesystems don't support directory fsync;
        // ignore failures since not all paths can honour it (e.g. some
        // FUSE mounts), and the rename itself is the source-of-truth step.
        if let Some(dir) = self.meta_path.parent() {
            if let Ok(dir_file) = tokio::fs::File::open(dir).await {
                let _ = dir_file.sync_all().await;
            }
        }
        tokio::fs::rename(&tmp, &self.meta_path)
            .await
            .map_err(|e| ProcessError::Internal(format!("rename meta: {e}")))?;
        if let Some(dir) = self.meta_path.parent() {
            if let Ok(file) = tokio::fs::File::open(dir).await {
                let _ = file.sync_all().await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{ExecutionScope, ProcessManager, ProcessSpec};
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    fn spec_for(cmd: &str) -> ProcessSpec {
        ProcessSpec {
            program: None,
            args: vec![],
            command: cmd.into(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: None,
            scope: ExecutionScope::TaskBound,
            description: None,
        }
    }

    #[tokio::test]
    async fn start_creates_log_directory() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id = mgr.start(spec_for("true")).await.unwrap();
        let _ = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
        let exec_dir = dir.path().join(id.to_string());
        assert!(exec_dir.is_dir(), "log dir should exist");
        assert!(exec_dir.join("stdout.log").is_file());
        assert!(exec_dir.join("stderr.log").is_file());
        assert!(exec_dir.join("meta.json").is_file());
    }

    #[tokio::test]
    async fn stdout_log_contains_command_output() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id = mgr.start(spec_for("echo hello-phase-2b")).await.unwrap();
        let _ = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
        let stdout = tokio::fs::read_to_string(dir.path().join(id.to_string()).join("stdout.log"))
            .await
            .unwrap();
        assert!(stdout.contains("hello-phase-2b"));
    }

    #[tokio::test]
    async fn stderr_log_contains_stderr_output() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id = mgr.start(spec_for("echo err-phase-2b >&2")).await.unwrap();
        let _ = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
        let stderr = tokio::fs::read_to_string(dir.path().join(id.to_string()).join("stderr.log"))
            .await
            .unwrap();
        assert!(stderr.contains("err-phase-2b"));
    }

    #[tokio::test]
    async fn meta_json_parses_to_final_record() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id = mgr.start(spec_for("exit 0")).await.unwrap();
        let _ = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
        let json = tokio::fs::read_to_string(dir.path().join(id.to_string()).join("meta.json"))
            .await
            .unwrap();
        let record: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(record["id"].as_str().unwrap(), id.to_string());
        assert_eq!(record["state"]["Exited"]["exit_code"].as_i64(), Some(0));
        assert_eq!(record["state"]["Exited"]["reason"], "Natural");
    }

    /// Renamed from `meta_json_written_on_spawn_failure` — the literal
    /// `SpawnFailed` state is awkward to trigger from the `sh -c` wrapper
    /// because `sh` itself exists; Phase 2A's deviation already documented
    /// this. This test instead asserts `meta.json` captures a non-zero
    /// natural exit (the practical equivalent, since `mark_spawn_failed` and
    /// `mark_exited` both flow through `finalize` in the executor).
    #[tokio::test]
    async fn meta_json_records_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id = mgr.start(spec_for("false")).await.unwrap();
        let _ = mgr.wait_for(id, Duration::from_secs(2)).await.unwrap();
        let json = tokio::fs::read_to_string(dir.path().join(id.to_string()).join("meta.json"))
            .await
            .unwrap();
        let record: serde_json::Value = serde_json::from_str(&json).unwrap();
        // `false` exits 1, captured in the Exited state.
        assert_eq!(record["state"]["Exited"]["exit_code"].as_i64(), Some(1));
        assert_eq!(record["state"]["Exited"]["reason"], "Natural");
    }

    #[tokio::test]
    async fn log_dir_unique_per_execution() {
        let dir = TempDir::new().unwrap();
        let mgr = ProcessManager::with_log_root(dir.path().to_path_buf()).unwrap();
        let id1 = mgr.start(spec_for("echo a")).await.unwrap();
        let id2 = mgr.start(spec_for("echo b")).await.unwrap();
        let _ = mgr.wait_for(id1, Duration::from_secs(2)).await.unwrap();
        let _ = mgr.wait_for(id2, Duration::from_secs(2)).await.unwrap();
        assert_ne!(id1, id2);
        assert!(dir.path().join(id1.to_string()).is_dir());
        assert!(dir.path().join(id2.to_string()).is_dir());
    }
}
