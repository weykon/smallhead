//! Host-owned registration and JSON-lines tool workers. No learning dependency.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smallhead_core::tool::ToolImpl;
use smallhead_core::{
    AgentLoop, ExternalTool, ToolContext, ToolError, ToolKind, ToolResult, ToolSpec,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{read_bounded, LensPackage};

const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Minimum host capability needed to advertise and execute a contribution.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAccess {
    /// Read local or external data, without application-state writes.
    Observe,
    /// Change application or domain state.
    Mutate,
    /// Execute additional operating-system commands.
    Execute,
}

/// Host ceiling. Package metadata can narrow but cannot grant host permissions.
#[derive(Clone, Copy, Debug)]
pub struct ExtensionAccess {
    /// Permit tools annotated observe.
    pub observe: bool,
    /// Permit tools annotated mutate.
    pub mutate: bool,
    /// Permit tools annotated execute.
    pub execute: bool,
}

impl ExtensionAccess {
    /// Locally enabled interactive host capabilities.
    pub fn all() -> Self {
        Self {
            observe: true,
            mutate: true,
            execute: true,
        }
    }

    fn allows(self, access: ToolAccess) -> bool {
        match access {
            ToolAccess::Observe => self.observe,
            ToolAccess::Mutate => self.mutate,
            ToolAccess::Execute => self.execute,
        }
    }
}

fn default_timeout() -> u64 {
    30_000
}

/// Local command and pinned files for one package's domain capabilities.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    /// Executable on PATH, or a file path relative to the package.
    pub command: String,
    /// Literal arguments; no shell expansion is performed.
    #[serde(default)]
    pub args: Vec<String>,
    /// JSON configuration, never supplied by the model or an A2A peer.
    pub config_file: PathBuf,
    /// JSON array of tool specifications including an access annotation.
    pub tools_file: PathBuf,
    /// Domain instructions read at load/reload time.
    pub system_file: PathBuf,
    /// Deadline for each worker exchange, including initialization.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contribution {
    name: String,
    label: String,
    description: String,
    input_schema: Value,
    access: ToolAccess,
}

impl Contribution {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            label: self.label.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

pub(crate) struct ExtensionPackage {
    command: String,
    args: Vec<String>,
    root: PathBuf,
    config: Value,
    tools: Vec<Contribution>,
    system: String,
    timeout: Duration,
}

fn package_file(root: &Path, path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        bail!("extension data files must be relative to the Lens package");
    }
    let path = root.join(path).canonicalize()?;
    if !path.starts_with(root) || !path.is_file() {
        bail!("extension data file escapes the Lens package or is not a file");
    }
    Ok(path)
}

impl ExtensionPackage {
    pub(crate) fn load(root: &Path, manifest: &ExtensionManifest) -> Result<Self> {
        if manifest.command.trim().is_empty()
            || manifest.timeout_ms == 0
            || manifest.timeout_ms > 300_000
        {
            bail!("invalid extension command or timeout (1..300000 ms)");
        }
        let command = if manifest.command.contains('/') || manifest.command.contains('\\') {
            let path = root.join(&manifest.command).canonicalize().context(
                "extension executable does not exist; build or install its worker first",
            )?;
            if !path.is_file() {
                bail!("extension command is not a file");
            }
            path.to_string_lossy().into_owned()
        } else {
            manifest.command.clone()
        };
        let config: Value = serde_json::from_str(&read_bounded(
            &package_file(root, &manifest.config_file)?,
            1024 * 1024,
        )?)?;
        if !config.is_object() {
            bail!("extension config must be a JSON object");
        }
        let tools: Vec<Contribution> = serde_json::from_str(&read_bounded(
            &package_file(root, &manifest.tools_file)?,
            1024 * 1024,
        )?)?;
        if tools.is_empty() || tools.len() > 128 {
            bail!("extension must contribute 1..128 tools");
        }
        let mut names = HashSet::new();
        for tool in &tools {
            if tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || !names.insert(&tool.name)
            {
                bail!("invalid or duplicate extension tool name: {}", tool.name);
            }
            if tool.input_schema.get("type").and_then(Value::as_str) != Some("object") {
                bail!("tool {} schema must describe an object", tool.name);
            }
        }
        let system = read_bounded(&package_file(root, &manifest.system_file)?, 256 * 1024)?;
        Ok(Self {
            command,
            args: manifest.args.clone(),
            root: root.into(),
            config,
            tools,
            system,
            timeout: Duration::from_millis(manifest.timeout_ms),
        })
    }
}

pub(crate) fn validate_names<'a>(
    packages: impl Iterator<Item = &'a Arc<LensPackage>>,
) -> Result<()> {
    let mut names = HashSet::new();
    for package in packages {
        if let Some(extension) = &package.extension {
            for tool in &extension.tools {
                if !names.insert(&tool.name) {
                    bail!("duplicate Lens tool: {}", tool.name);
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn attach(
    extensions: &[Arc<ExtensionClient>],
    agent: &mut AgentLoop,
    access: ExtensionAccess,
) -> Result<()> {
    let (tools, system) = contributions(extensions, &agent.tools, access)?;
    let attached: Vec<_> = tools.iter().map(|tool| tool.spec().name).collect();
    agent.tools.extend(tools);
    agent.system.push_str(&system);
    if !attached.is_empty() {
        smallhead_core::diagnostics::emit(
            "lens",
            "extension_tools_attached",
            json!({"tools":attached}),
        );
    }
    Ok(())
}

pub(crate) fn contributions(
    extensions: &[Arc<ExtensionClient>],
    host_tools: &[ToolKind],
    access: ExtensionAccess,
) -> Result<(Vec<ToolKind>, String)> {
    let mut names: HashSet<String> = host_tools.iter().map(|tool| tool.spec().name).collect();
    let mut tools = Vec::new();
    let mut system = String::new();
    for client in extensions {
        let permitted: Vec<_> = client
            .package
            .tools
            .iter()
            .filter(|tool| access.allows(tool.access))
            .collect();
        if permitted.is_empty() {
            continue;
        }
        for contribution in permitted {
            if !names.insert(contribution.name.clone()) {
                bail!(
                    "Lens {} tool conflicts with host: {}",
                    client.id,
                    contribution.name
                );
            }
            tools.push(ToolKind::External(ExternalTool::new(Arc::new(
                ExtensionTool {
                    client: client.clone(),
                    specification: contribution.spec(),
                },
            ))));
        }
        system.push_str(&format!(
            "\n# Lens domain: {}\n{}\n",
            client.id, client.package.system
        ));
    }
    Ok((tools, system))
}

pub(crate) struct ExtensionClient {
    id: String,
    package: Arc<ExtensionPackage>,
    process: Mutex<Option<Process>>,
    failed_or_busy: AtomicBool,
    closed: AtomicBool,
    shutdown_token: smallhead_core::CancellationToken,
    active: Mutex<()>,
    reapers: Reapers,
}

type ReapResult = Option<std::result::Result<(), String>>;
type Reapers = Arc<parking_lot::Mutex<Vec<tokio::sync::watch::Receiver<ReapResult>>>>;

impl ExtensionClient {
    pub(crate) fn new(id: String, package: Arc<ExtensionPackage>) -> Self {
        Self {
            id,
            package,
            process: Mutex::new(None),
            failed_or_busy: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            shutdown_token: smallhead_core::CancellationToken::new(),
            active: Mutex::new(()),
            reapers: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    async fn call(&self, name: &str, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        if self.closed.load(Ordering::SeqCst) || self.failed_or_busy.swap(true, Ordering::SeqCst) {
            bail!("extension {} is closed, failed or already in use", self.id);
        }
        let _active = self.active.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            bail!("extension {} is closed", self.id);
        }
        let operation = async {
            // Own the child across awaits. Dropped RPC futures kill it instead
            // of storing a potentially stale response for the next tool call.
            let mut process = match self.process.lock().await.take() {
                Some(process) => process,
                None => Process::start(&self.package, self.reapers.clone()).await?,
            };
            let input_context = ctx
                .accepted_input
                .as_ref()
                .context("extension tools require a host-bound input")?;
            if process.bound_input.as_deref() != Some(&input_context.id) {
                process
                    .rpc("bind_input", json!({"input":input_context}))
                    .await?;
                process.bound_input = Some(input_context.id.clone());
            }
            let id = ctx
                .tool_call_id
                .clone()
                .context("extension call missing host tool identity")?;
            let result: ToolResult = serde_json::from_value(
                process
                    .rpc("call", json!({"name":name,"input":input,"tool_call_id":id}))
                    .await?,
            )?;
            if result.tool_call_id != id {
                bail!("extension returned a mismatched tool receipt");
            }
            let mut slot = self.process.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                bail!(
                    "extension {} closed during call; effects may be unknown",
                    self.id
                );
            }
            *slot = Some(process);
            Ok(result)
        };
        let result = tokio::select! {
            biased;
            _ = self.shutdown_token.cancelled() => Err(anyhow::anyhow!("extension {} closed during call; effects may be unknown", self.id)),
            _ = ctx.cancellation.cancelled() => Err(anyhow::anyhow!("extension {} cancelled; effects may be unknown", self.id)),
            result = tokio::time::timeout(self.package.timeout, operation) => result.context("extension RPC timed out; effects may be unknown")?,
        };
        if result.is_ok() {
            self.failed_or_busy.store(false, Ordering::SeqCst);
        } else if let Err(error) = &result {
            smallhead_core::diagnostics::emit(
                "lens",
                "extension_rpc_error",
                json!({"lens":self.id,"tool":name,"error":error.to_string()}),
            );
        }
        result
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.shutdown_token.cancel();
        self.failed_or_busy.store(true, Ordering::SeqCst);
        // An interrupted call first transfers its owned child to a reaper.
        // The gate ensures shutdown cannot report completion before that handoff.
        let _active = self.active.lock().await;
        drop(self.process.lock().await.take());
        // Retain completion tracking when a shutdown is cancelled or times out.
        let reapers = self.reapers.lock().clone();
        let mut errors = Vec::new();
        for mut reaper in reapers {
            let wait = async {
                loop {
                    if let Some(result) = reaper.borrow().clone() {
                        return result;
                    }
                    if reaper.changed().await.is_err() {
                        return Err("extension reaper stopped without confirming exit".into());
                    }
                }
            };
            let result = tokio::time::timeout(Duration::from_secs(5), wait).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(_) => errors.push("extension cleanup timed out; exit not confirmed".into()),
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        Ok(())
    }
}

struct ExtensionTool {
    client: Arc<ExtensionClient>,
    specification: ToolSpec,
}

#[async_trait]
impl ToolImpl for ExtensionTool {
    fn spec(&self) -> ToolSpec {
        self.specification.clone()
    }
    async fn execute(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, ToolError> {
        self.client
            .call(&self.specification.name, input, ctx)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

struct Process {
    child: Option<Child>,
    reapers: Reapers,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: tokio::task::JoinHandle<()>,
    stderr_tail: Arc<parking_lot::Mutex<Vec<u8>>>,
    sequence: u64,
    bound_input: Option<String>,
}

impl Process {
    async fn start(package: &ExtensionPackage, reapers: Reapers) -> Result<Self> {
        let mut child = Command::new(&package.command)
            .args(&package.args)
            .current_dir(&package.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start Lens tool worker")?;
        let stdin = child.stdin.take().context("extension stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("extension stdout")?);
        let mut stderr = child.stderr.take().context("extension stderr")?;
        let stderr_tail = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tail = stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            // Drain without printing into the TUI or buffering unbounded output.
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
        let mut process = Self {
            child: Some(child),
            reapers,
            stdin,
            stdout,
            stderr_task,
            stderr_tail,
            sequence: 0,
            bound_input: None,
        };
        let initialized = process
            .rpc(
                "initialize",
                json!({"api_version":1,"config":package.config}),
            )
            .await?;
        if initialized["api_version"] != 1 {
            bail!("unsupported extension worker API version");
        }
        Ok(process)
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let result = self.exchange(method, params).await;
        result.map_err(|error| {
            let tail = self.stderr_tail.lock();
            if tail.is_empty() {
                error
            } else {
                error.context(format!("worker stderr: {}", String::from_utf8_lossy(&tail)))
            }
        })
    }

    async fn exchange(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        let mut frame = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        if frame.len() >= MAX_FRAME {
            bail!("extension request exceeds 16 MiB");
        }
        frame.push(b'\n');
        self.stdin.write_all(&frame).await?;
        self.stdin.flush().await?;
        let mut frame = Vec::new();
        loop {
            let chunk = self.stdout.fill_buf().await?;
            if chunk.is_empty() {
                bail!("extension worker closed stdout");
            }
            let end = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1);
            let take = end.unwrap_or(chunk.len());
            if frame.len() + take > MAX_FRAME {
                bail!("extension response exceeds 16 MiB");
            }
            frame.extend_from_slice(&chunk[..take]);
            self.stdout.consume(take);
            if end.is_some() {
                break;
            }
        }
        let response: Value =
            serde_json::from_slice(&frame).context("invalid extension response")?;
        if response["id"].as_u64() != Some(id) {
            bail!("stale or unknown extension response id");
        }
        if let Some(error) = response.get("error") {
            bail!("extension error: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("extension response missing result")
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let (finished, completion) = tokio::sync::watch::channel(None);
                self.reapers.lock().push(completion);
                runtime.spawn(async move {
                    let result = child
                        .wait()
                        .await
                        .map(|_| ())
                        .map_err(|error| format!("reap extension worker: {error}"));
                    let _ = finished.send(Some(result));
                });
            }
        }
        self.stderr_task.abort();
    }
}
