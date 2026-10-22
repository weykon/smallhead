use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use smallhead_core::loop_policy::{
    LoopPolicy, PolicyAction, PolicyContext, PolicyEvent, PolicyNext,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::LensPackage;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) struct JsPolicy {
    package: Arc<LensPackage>,
    worker: Mutex<Option<Worker>>,
    failed_or_busy: AtomicBool,
}

impl JsPolicy {
    pub fn new(package: Arc<LensPackage>) -> Self {
        Self {
            package,
            worker: Mutex::new(None),
            failed_or_busy: AtomicBool::new(false),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.failed_or_busy.store(true, Ordering::SeqCst);
        if let Some(mut worker) = self.worker.lock().await.take() {
            worker.child.kill().await.context("stop Lens worker")?;
        }
        Ok(())
    }
}

#[async_trait]
impl LoopPolicy for JsPolicy {
    fn name(&self) -> &str {
        &self.package.manifest.id
    }

    async fn apply(
        &self,
        context: &PolicyContext,
        event: PolicyEvent,
        next: PolicyNext,
    ) -> Result<PolicyAction> {
        if self.failed_or_busy.swap(true, Ordering::SeqCst) {
            bail!("Lens {} is failed, closed or already in use", self.name());
        }
        // The worker is locally owned across every await. Dropping this future
        // on cancellation kills it rather than leaving a stale RPC in the slot.
        let mut worker = match self.worker.lock().await.take() {
            Some(worker) => worker,
            None => Worker::start(&self.package, &context.turn_id).await?,
        };
        if worker.turn_id != context.turn_id {
            bail!("Lens sessions cannot be shared across turns; create a fresh session");
        }
        let result = worker.evaluate(context, event, next).await;
        match result {
            Ok(action) => {
                *self.worker.lock().await = Some(worker);
                self.failed_or_busy.store(false, Ordering::SeqCst);
                Ok(action)
            }
            Err(error) => {
                let stderr = String::from_utf8_lossy(&worker.stderr_tail.lock()).into_owned();
                bail!("Lens {}: {error}; stderr: {stderr}", self.name());
            }
        }
    }
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: tokio::task::JoinHandle<()>,
    stderr_tail: Arc<parking_lot::Mutex<Vec<u8>>>,
    sequence: u64,
    turn_id: String,
}

impl Worker {
    async fn start(package: &LensPackage, turn_id: &str) -> Result<Self> {
        let mut child = Command::new("node")
            .args(["--input-type=module", "-e", include_str!("worker.mjs")])
            .current_dir(&package.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start Lens worker (Node.js required)")?;
        let stdin = child.stdin.take().context("worker stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("worker stdout")?);
        let mut stderr = child.stderr.take().context("worker stderr")?;
        let stderr_tail = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tail = stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(size) = stderr.read(&mut buffer).await {
                if size == 0 {
                    break;
                }
                let mut tail = tail.lock();
                tail.extend_from_slice(&buffer[..size]);
                let excess = tail.len().saturating_sub(4096);
                tail.drain(..excess);
            }
        });
        let mut worker = Self {
            child,
            stdin,
            stdout,
            stderr_task,
            stderr_tail,
            sequence: 0,
            turn_id: turn_id.to_owned(),
        };
        worker.send(json!({"type":"initialize","source":package.source,"config":package.manifest.config})).await?;
        let ready = worker.receive().await?;
        if ready.get("type").and_then(Value::as_str) != Some("ready") {
            bail!("Lens initialization failed: {ready}");
        }
        Ok(worker)
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        if bytes.len().saturating_add(1) > MAX_FRAME {
            bail!("Lens protocol frame exceeds 16 MiB");
        }
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Value> {
        // read_line would allocate without a bound on untrusted plugin output.
        let mut bytes = Vec::new();
        loop {
            let chunk = self.stdout.fill_buf().await?;
            if chunk.is_empty() {
                bail!("Lens worker closed stdout");
            }
            let end = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1);
            let take = end.unwrap_or(chunk.len());
            if bytes.len() + take > MAX_FRAME {
                bail!("Lens protocol frame exceeds 16 MiB");
            }
            bytes.extend_from_slice(&chunk[..take]);
            self.stdout.consume(take);
            if end.is_some() {
                return serde_json::from_slice(&bytes).context("invalid Lens protocol JSON");
            }
        }
    }

    async fn evaluate(
        &mut self,
        context: &PolicyContext,
        event: PolicyEvent,
        next: PolicyNext,
    ) -> Result<PolicyAction> {
        self.sequence += 1;
        let id = self.sequence;
        self.send(json!({"type":"evaluate","id":id,"context":context,"event":event}))
            .await?;
        let mut continued = false;
        let mut next = Some(next);
        loop {
            let message = self.receive().await?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                bail!("Lens response has stale/unknown checkpoint id");
            }
            match message.get("type").and_then(Value::as_str) {
                Some("next") if !continued => {
                    continued = true;
                    let event = serde_json::from_value(
                        message
                            .get("event")
                            .cloned()
                            .context("next missing event")?,
                    )?;
                    match next
                        .take()
                        .context("next was already consumed")?
                        .run(event)
                        .await
                    {
                        Ok(action) => {
                            self.send(json!({"type":"next_result","id":id,"action":action}))
                                .await?
                        }
                        // Failure propagates; a wrapper cannot swallow a failed
                        // inner policy and execute a different side effect.
                        Err(error) => return Err(error),
                    }
                }
                Some("result") => {
                    return serde_json::from_value(
                        message
                            .get("action")
                            .cloned()
                            .context("result missing action")?,
                    )
                    .context("invalid Lens action")
                }
                Some("error") => bail!("{}", message.get("error").unwrap_or(&Value::Null)),
                _ => bail!("unexpected Lens protocol message"),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}
