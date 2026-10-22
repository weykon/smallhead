//! Executor tasks own children independently of callers waiting for results.
//! Unix process groups use safe Tokio/rustix APIs; escaped sessions are outside
//! this guarantee. Other platforms currently clean up the direct child only.

use crate::persist::{LogFinalizer, LogHandle};
use crate::{
    ExecutionId, ExitReason, ProcessExitInfo, ProcessId, ProcessManager, ProcessSpec, ProcessState,
};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

const STOP_GRACE: Duration = Duration::from_millis(150);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct Executor {
    id: ExecutionId,
    spec: ProcessSpec,
    manager: ProcessManager,
    log: LogHandle,
}

impl Executor {
    pub(crate) fn new(
        id: ExecutionId,
        spec: ProcessSpec,
        log: LogHandle,
        manager: ProcessManager,
    ) -> Self {
        Self {
            id,
            spec,
            log,
            manager,
        }
    }

    pub(crate) async fn run(self) {
        let Self {
            id,
            spec,
            log,
            manager,
        } = self;
        let stream_limit = manager.limits().max_stream_bytes;
        let started = Instant::now();
        let (out_writer, err_writer, finalizer) = log.split();
        // 排队期间不 spawn；显式 stop 不需要等待前面的命令释放名额。
        let permit = tokio::select! {
            biased;
            _ = manager.subscribe_change(id) => None,
            permit = manager.acquire_slot() => permit.ok(),
        };
        let Some(_permit) = permit else {
            finish(
                &manager,
                id,
                &finalizer,
                ProcessState::Exited(ProcessExitInfo {
                    exit_code: None,
                    signal: None,
                    reason: ExitReason::Cancelled,
                    duration: started.elapsed(),
                }),
            )
            .await;
            return;
        };
        if manager.inspect(id).is_some_and(|r| r.state.is_terminal()) {
            return;
        }
        if manager
            .inspect(id)
            .is_some_and(|r| matches!(r.state, ProcessState::Stopping))
        {
            finish(
                &manager,
                id,
                &finalizer,
                ProcessState::Exited(ProcessExitInfo {
                    exit_code: None,
                    signal: None,
                    reason: ExitReason::Cancelled,
                    duration: started.elapsed(),
                }),
            )
            .await;
            return;
        }
        let mut cmd = if let Some(program) = &spec.program {
            let mut cmd = Command::new(program);
            cmd.args(&spec.args);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&spec.command);
            cmd
        };
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        cmd.envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Defensive direct-child fallback; normal stop uses group cleanup.
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                finish(
                    &manager,
                    id,
                    &finalizer,
                    ProcessState::SpawnFailed(e.to_string()),
                )
                .await;
                return;
            }
        };
        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            finish(
                &manager,
                id,
                &finalizer,
                ProcessState::Lost("spawn returned no PID".into()),
            )
            .await;
            return;
        };
        let _ = manager.mark_starting(id, ProcessId::from_pid(pid));
        let _ = manager.mark_running(id);
        manager.notify_waiters(id);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (failure_tx, mut failure_rx) = tokio::sync::mpsc::channel(2);
        let out_failure = failure_tx.clone();
        let mut out_task = tokio::spawn(async move {
            let result = drain(stdout, out_writer, stream_limit).await;
            if let Err(e) = &result {
                let _ = out_failure.send((e.kind(), e.to_string())).await;
            }
            result
        });
        let mut err_task = tokio::spawn(async move {
            let result = drain(stderr, err_writer, stream_limit).await;
            if let Err(e) = &result {
                let _ = failure_tx.send((e.kind(), e.to_string())).await;
            }
            result
        });
        let deadline = async {
            match spec.timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        let (result, reason) = tokio::select! {
            biased;
            _ = manager.subscribe_change(id) => {
                (terminate(&mut child, pid).await, ExitReason::Cancelled)
            }
            Some((kind, error)) = failure_rx.recv() => {
                let _ = manager.mark_stopping(id);
                let cleanup = terminate(&mut child, pid).await;
                if kind == std::io::ErrorKind::FileTooLarge {
                    (cleanup, ExitReason::OutputLimit)
                } else {
                    let detail = match cleanup { Ok(_) => error, Err(e) => format!("{error}; cleanup: {e}") };
                    (Err(std::io::Error::other(detail)), ExitReason::Lost)
                }
            }
            result = natural_exit(&mut child, pid) => (result, ExitReason::Natural),
            _ = deadline => {
                let _ = manager.mark_stopping(id);
                manager.notify_waiters(id);
                (terminate(&mut child, pid).await, ExitReason::Timeout)
            }
        };
        let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
            let (out, err) = tokio::join!(&mut out_task, &mut err_task);
            let out = reader_result(out);
            let err = reader_result(err);
            // 实际 I/O 失败优先于正常预算截断，不能被另一条流的超限掩盖。
            for result in [&out, &err] {
                if let Err(error) = result {
                    if error.kind() != std::io::ErrorKind::FileTooLarge {
                        return Err(std::io::Error::new(error.kind(), error.to_string()));
                    }
                }
            }
            out.and(err)
        })
        .await;
        let drain_result = match drained {
            Ok(result) => result,
            Err(_) => {
                out_task.abort();
                err_task.abort();
                let _ = tokio::join!(out_task, err_task);
                Err(std::io::Error::other("output drain deadline exceeded"))
            }
        };
        let state = match (result, drain_result) {
            // 子进程可能先退出，reader 才发现超限；仍记录真实预算原因。
            (Ok(status), Err(e)) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                ProcessState::Exited(ProcessExitInfo {
                    exit_code: status.code(),
                    signal: exit_signal(&status),
                    reason: ExitReason::OutputLimit,
                    duration: started.elapsed(),
                })
            }
            (Ok(status), Ok(())) => ProcessState::Exited(ProcessExitInfo {
                exit_code: status.code(),
                signal: exit_signal(&status),
                reason,
                duration: started.elapsed(),
            }),
            (Err(e), _) | (_, Err(e)) => ProcessState::CleanupFailed(e.to_string()),
        };
        finish(&manager, id, &finalizer, state).await;
    }
}

#[cfg(unix)]
async fn signal_group(pid: u32, signal: rustix::process::Signal) -> std::io::Result<()> {
    use rustix::{
        io::Errno,
        process::{kill_process_group, Pid},
    };
    let pgid = i32::try_from(pid).map_err(|_| std::io::Error::other("invalid PGID"))?;
    if pgid <= 1 {
        return Err(std::io::Error::other("invalid PGID"));
    }
    match kill_process_group(Pid::from_raw(pgid).expect("positive PID"), signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        #[cfg(target_os = "macos")]
        Err(Errno::PERM) if group_has_only_zombies(pid).await? => Ok(()),
        Err(e) => Err(std::io::Error::other(format!(
            "group signal {signal:?} to {pid}: {e}"
        ))),
    }
}

async fn terminate(child: &mut Child, pid: u32) -> std::io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        signal_group(pid, rustix::process::Signal::TERM).await?;
        // Keep the leader unreaped during grace, preserving PGID identity.
        tokio::time::sleep(STOP_GRACE).await;
        signal_group(pid, rustix::process::Signal::KILL).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        child.start_kill()?;
    }
    tokio::time::timeout(REAP_TIMEOUT, child.wait())
        .await
        .map_err(|_| std::io::Error::other("child reap deadline exceeded"))?
}

async fn natural_exit(child: &mut Child, pid: u32) -> std::io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
        let pid_value = Pid::from_raw(i32::try_from(pid).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("invalid PID"))?;
        loop {
            // Observe without reaping: group identity remains pinned until
            // cleanup completes. Never signal a recycled PID after child.wait.
            if waitid(
                WaitId::Pid(pid_value),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map_err(|e| std::io::Error::other(format!("waitid: {e}")))?
            .is_some()
            {
                signal_group(pid, rustix::process::Signal::KILL).await?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    child.wait().await
}

#[cfg(target_os = "macos")]
async fn group_has_only_zombies(pid: u32) -> std::io::Result<bool> {
    // Darwin can return EPERM for a group containing only zombies. Verify
    // the exact group through its OS status tool; never swallow EPERM for a
    // live member. This is an internal platform query, not a business command.
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-g", &pid.to_string(), "-o", "stat="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("piped ps stdout").take(16_385);
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        let status = child.wait().await?;
        if bytes.len() > 16_384 || (!status.success() && status.code() != Some(1)) {
            return Err(std::io::Error::other("cannot verify process group members"));
        }
        let text = std::str::from_utf8(&bytes).map_err(std::io::Error::other)?;
        Ok(text.split_whitespace().all(|state| state.starts_with('Z')))
    })
    .await;
    result.map_err(|_| std::io::Error::other("process group verification timed out"))?
}

fn reader_result(
    result: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> std::io::Result<()> {
    result.map_err(std::io::Error::other)?
}

async fn drain<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    pipe: Option<R>,
    mut writer: W,
    limit: u64,
) -> std::io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0u8; 4096];
    let mut written = 0u64;
    loop {
        let n = pipe.read(&mut buffer).await?;
        if n == 0 {
            return writer.flush().await;
        }
        let allowed = (limit - written).min(n as u64) as usize;
        writer.write_all(&buffer[..allowed]).await?;
        written += allowed as u64;
        writer.flush().await?;
        if allowed < n {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "输出超过管理器保存预算，已截断",
            ));
        }
    }
}

async fn finish(
    manager: &ProcessManager,
    id: ExecutionId,
    finalizer: &LogFinalizer,
    state: ProcessState,
) {
    let Some(mut record) = manager.inspect(id) else {
        return;
    };
    record.state = state;
    if let Some(error) = manager.audit_error(id) {
        record.state = ProcessState::CleanupFailed(error);
    }
    record.finished_at = Some(SystemTime::now());
    if let Err(e) = finalizer.finalize(&record).await {
        record.state = ProcessState::CleanupFailed(format!("meta.json persistence failed: {e}"));
        tracing::error!(execution = %id, error = %e, "final record persistence failed");
    }
    manager.publish_final(record);
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_: &ExitStatus) -> Option<i32> {
    None
}
