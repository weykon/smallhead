use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use parking_lot::RwLock;
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    TokioChildProcess,
};
use rmcp::{
    model::{CallToolRequestParams, PaginatedRequestParams},
    service::{RoleClient, RunningService},
    ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{McpConfig, ServerConfig};

const MAX_CONFIG: usize = 1024 * 1024;
const MAX_RESULT: usize = 4 * 1024 * 1024;
const MAX_TOOLS: usize = 128;
const MAX_PAGES: usize = 8;
type Client = RunningService<RoleClient, ()>;

/// Errors intentionally exclude transport/configuration payloads and credentials.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    Config(&'static str),
    #[error("MCP configuration could not be read")]
    ConfigRead,
    #[error("MCP executable or working directory not found")]
    ExecutableNotFound,
    #[error("MCP executable or working directory permission denied")]
    PermissionDenied,
    #[error("unknown MCP server")]
    UnknownServer,
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP server is busy; no request was queued")]
    Busy,
    #[error("MCP server is not connected; connect explicitly")]
    NotConnected,
    #[error("MCP {0} failed; remote details withheld")]
    Protocol(&'static str),
    #[error(
        "MCP {0} timed out; a submitted operation may have executed; never replay automatically"
    )]
    Timeout(&'static str),
    #[error("MCP response exceeds bounded discovery or result limits")]
    Limit,
    #[error("MCP tool arguments must be an object of at most 256 KiB and the tool must have been discovered")]
    Arguments,
    #[error("MCP call result unavailable ({0}); submitted execution may have occurred; never replay automatically")]
    CallResultUnavailable(&'static str),
    #[error(
        "MCP request was interrupted; side effects may have occurred; never replay automatically"
    )]
    Interrupted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Stdio,
    Http,
}

/// Ready means the MCP handshake and bounded tool discovery succeeded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Ready,
    Failed,
    Disabled,
}

/// Credential-free local UI metadata; never contains the command, URL or headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSnapshot {
    pub name: String,
    pub transport: TransportKind,
    pub status: ConnectionStatus,
    pub tool_count: usize,
    pub description: Option<String>,
    pub last_error: Option<String>,
}

/// Untrusted remote metadata, discovered only on explicit request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

struct Connection {
    client: Client,
    tools: Vec<McpToolInfo>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(task) = self.stderr.take() {
            task.abort();
        }
        // RunningService's drop guard cancels and closes its owned transport.
    }
}

struct Server {
    config: ServerConfig,
    state: RwLock<ConnectionSnapshot>,
    connection: Mutex<Option<Connection>>,
}

/// Share an Arc across UI/tasks. One nonqueued operation per server, at most 64
/// configured servers; no global lock is held across transport awaits.
pub struct McpManager {
    servers: RwLock<BTreeMap<String, Arc<Server>>>,
    lifecycle: tokio::sync::RwLock<()>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("server_count", &self.servers.read().len())
            .finish()
    }
}

impl McpManager {
    pub fn new(config: McpConfig) -> Result<Self, McpError> {
        if config.mcp_servers.len() > 64 {
            return Err(McpError::Config("at most 64 servers"));
        }
        let mut servers = BTreeMap::new();
        for (name, config) in config.mcp_servers {
            validate(&name, &config)?;
            let snapshot = ConnectionSnapshot {
                name: name.clone(),
                transport: if config.command.is_some() {
                    TransportKind::Stdio
                } else {
                    TransportKind::Http
                },
                status: if config.disabled {
                    ConnectionStatus::Disabled
                } else {
                    ConnectionStatus::Disconnected
                },
                tool_count: 0,
                description: config.description.clone(),
                last_error: None,
            };
            servers.insert(
                name,
                Arc::new(Server {
                    config,
                    state: RwLock::new(snapshot),
                    connection: Mutex::new(None),
                }),
            );
        }
        Ok(Self {
            servers: RwLock::new(servers),
            lifecycle: tokio::sync::RwLock::new(()),
        })
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, McpError> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| McpError::ConfigRead)?
            .take((MAX_CONFIG + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| McpError::ConfigRead)?;
        if bytes.len() > MAX_CONFIG {
            return Err(McpError::Config("file exceeds 1 MiB"));
        }
        Self::new(
            serde_json::from_slice(&bytes)
                .map_err(|_| McpError::Config("invalid JSON or unknown fields"))?,
        )
    }

    pub async fn list(&self) -> Vec<ConnectionSnapshot> {
        self.snapshots()
    }

    pub fn snapshots(&self) -> Vec<ConnectionSnapshot> {
        self.servers
            .read()
            .values()
            .map(|s| {
                let mut snapshot = s.state.read().clone();
                if let Ok(connection) = s.connection.try_lock() {
                    if connection.as_ref().is_some_and(|c| c.client.is_closed()) {
                        snapshot.status = ConnectionStatus::Failed;
                        snapshot.last_error =
                            Some("MCP transport closed; reconnect explicitly".into());
                    }
                }
                snapshot
            })
            .collect()
    }

    /// Validate first, then disconnect existing clients and atomically replace
    /// configuration. Busy operations reject reload without changing configuration.
    pub async fn reload(&self, path: impl AsRef<Path>) -> Result<(), McpError> {
        let replacement = Self::load(path)?;
        let _exclusive = self.lifecycle.try_write().map_err(|_| McpError::Busy)?;
        self.close_all().await?;
        *self.servers.write() = replacement.servers.into_inner();
        Ok(())
    }

    /// Close all owned transports; rejects while any operation is active.
    pub async fn disconnect_all(&self) -> Result<(), McpError> {
        let _exclusive = self.lifecycle.try_write().map_err(|_| McpError::Busy)?;
        self.close_all().await
    }

    async fn close_all(&self) -> Result<(), McpError> {
        let servers: Vec<_> = self.servers.read().values().cloned().collect();
        let mut first_error = None;
        for server in servers {
            if let Err(error) = disconnect_server(&server).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn connect(&self, name: &str) -> Result<ConnectionSnapshot, McpError> {
        self.connect_named(name, false).await
    }

    /// Explicitly close and reconnect under one per-server gate; never replay calls.
    pub async fn reconnect(&self, name: &str) -> Result<ConnectionSnapshot, McpError> {
        self.connect_named(name, true).await
    }

    async fn connect_named(&self, name: &str, force: bool) -> Result<ConnectionSnapshot, McpError> {
        let _lifecycle = self.lifecycle.try_read().map_err(|_| McpError::Busy)?;
        let server = self.server(name)?;
        if server.config.disabled {
            return Err(McpError::Disabled);
        }
        let mut slot = server.connection.try_lock().map_err(|_| McpError::Busy)?;
        if !force
            && slot.as_ref().is_some_and(|c| !c.client.is_closed())
            && server.state.read().status == ConnectionStatus::Ready
        {
            return Ok(server.state.read().clone());
        }
        let mut operation = Operation::new(&server);
        if let Some(mut old) = slot.take() {
            shutdown(&mut old).await?;
        }
        server.state.write().status = ConnectionStatus::Connecting;
        let result = open(&server.config).await;
        match result {
            Ok(mut connection) => {
                match discover(&connection.client, server.config.list_timeout_ms).await {
                    Ok(tools) => {
                        connection.tools = tools;
                        let mut state = server.state.write();
                        state.status = ConnectionStatus::Ready;
                        state.tool_count = connection.tools.len();
                        state.last_error = None;
                        *slot = Some(connection);
                        operation.done = true;
                        Ok(state.clone())
                    }
                    Err(error) => {
                        let _ = shutdown(&mut connection).await;
                        operation.fail(&error);
                        Err(error)
                    }
                }
            }
            Err(error) => {
                operation.fail(&error);
                Err(error)
            }
        }
    }

    pub async fn disconnect(&self, name: &str) -> Result<ConnectionSnapshot, McpError> {
        let _lifecycle = self.lifecycle.try_read().map_err(|_| McpError::Busy)?;
        let server = self.server(name)?;
        disconnect_server(&server).await
    }

    pub async fn tools(&self, name: &str) -> Result<Vec<McpToolInfo>, McpError> {
        let _lifecycle = self.lifecycle.try_read().map_err(|_| McpError::Busy)?;
        let server = self.server(name)?;
        let mut slot = server.connection.try_lock().map_err(|_| McpError::Busy)?;
        let connection = ready(&server, &mut slot)?;
        let mut operation = Operation::new(&server);
        operation.cancel = Some(connection.client.cancellation_token());
        match discover(&connection.client, server.config.list_timeout_ms).await {
            Ok(tools) => {
                connection.tools = tools.clone();
                server.state.write().tool_count = tools.len();
                operation.done = true;
                Ok(tools)
            }
            Err(error) => {
                operation.fail(&error);
                Err(error)
            }
        }
    }

    pub async fn call(&self, name: &str, tool: &str, arguments: Value) -> Result<Value, McpError> {
        let _lifecycle = self.lifecycle.try_read().map_err(|_| McpError::Busy)?;
        let args = arguments.as_object().ok_or(McpError::Arguments)?;
        if serde_json::to_vec(&arguments)
            .map_err(|_| McpError::Arguments)?
            .len()
            > 256 * 1024
        {
            return Err(McpError::Arguments);
        }
        let server = self.server(name)?;
        let mut slot = server.connection.try_lock().map_err(|_| McpError::Busy)?;
        let connection = ready(&server, &mut slot)?;
        if !connection.tools.iter().any(|t| t.name == tool) {
            return Err(McpError::Arguments);
        }
        let mut params = CallToolRequestParams::new(tool.to_owned());
        params.arguments = Some(args.clone());
        let mut operation = Operation::new(&server);
        operation.cancel = Some(connection.client.cancellation_token());
        // Peer::call_tool sends exactly one request (no MRTR retries).
        let result = tokio::time::timeout(
            Duration::from_millis(server.config.call_timeout_ms),
            connection.client.peer().call_tool(params),
        )
        .await
        .map_err(|_| McpError::Timeout("call"))
        .and_then(|r| {
            r.map_err(|_| McpError::CallResultUnavailable("protocol or transport failure"))
        })
        .and_then(|r| {
            serde_json::to_value(r)
                .map_err(|_| McpError::CallResultUnavailable("serialization failed"))
        })
        .and_then(|v| {
            bounded(v).map_err(|_| McpError::CallResultUnavailable("response exceeds 4 MiB"))
        });
        match result {
            Ok(result) => {
                operation.done = true;
                Ok(result)
            }
            Err(error) => {
                operation.fail(&error);
                Err(error)
            }
        }
    }

    fn server(&self, name: &str) -> Result<Arc<Server>, McpError> {
        self.servers
            .read()
            .get(name)
            .cloned()
            .ok_or(McpError::UnknownServer)
    }
}

async fn disconnect_server(server: &Server) -> Result<ConnectionSnapshot, McpError> {
    let mut slot = server.connection.try_lock().map_err(|_| McpError::Busy)?;
    let mut operation = Operation::new(server);
    if let Some(mut connection) = slot.take() {
        if let Err(error) = shutdown(&mut connection).await {
            operation.fail(&error);
            return Err(error);
        }
    }
    let mut state = server.state.write();
    state.status = if server.config.disabled {
        ConnectionStatus::Disabled
    } else {
        ConnectionStatus::Disconnected
    };
    state.tool_count = 0;
    state.last_error = None;
    operation.done = true;
    Ok(state.clone())
}

struct DrainGuard(Option<tokio::task::JoinHandle<()>>);
impl Drop for DrainGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

fn ready<'a>(
    server: &Server,
    slot: &'a mut Option<Connection>,
) -> Result<&'a mut Connection, McpError> {
    if server.config.disabled {
        return Err(McpError::Disabled);
    }
    slot.as_mut()
        .filter(|c| !c.client.is_closed() && server.state.read().status == ConnectionStatus::Ready)
        .ok_or(McpError::NotConnected)
}

// Drop covers externally cancelled futures without claiming that a write failed.
struct Operation<'a> {
    server: &'a Server,
    done: bool,
    cancel: Option<rmcp::service::RunningServiceCancellationToken>,
}
impl<'a> Operation<'a> {
    fn new(server: &'a Server) -> Self {
        Self {
            server,
            done: false,
            cancel: None,
        }
    }
    fn fail(&mut self, error: &McpError) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        let mut state = self.server.state.write();
        state.status = ConnectionStatus::Failed;
        state.last_error = Some(error.to_string());
        self.done = true;
    }
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.fail(&McpError::Interrupted);
        }
    }
}

async fn open(config: &ServerConfig) -> Result<Connection, McpError> {
    let timeout = Duration::from_millis(config.connect_timeout_ms);
    if let Some(command) = &config.command {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args).envs(&config.env).kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        let mut command = process_wrap::tokio::CommandWrap::from(cmd);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        let (transport, stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => McpError::ExecutableNotFound,
                std::io::ErrorKind::PermissionDenied => McpError::PermissionDenied,
                _ => McpError::Protocol("spawn"),
            })?;
        let mut drain = DrainGuard(stderr.map(|mut stderr| {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            })
        }));
        let result = tokio::time::timeout(timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::Timeout("connect"))
            .and_then(|r| r.map_err(|_| McpError::Protocol("initialize")));
        match result {
            Ok(client) => Ok(Connection {
                client,
                tools: Vec::new(),
                stderr: drain.0.take(),
            }),
            Err(error) => Err(error),
        }
    } else {
        let mut headers = std::collections::HashMap::new();
        for (name, variable) in &config.http_headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpError::Config("invalid header name"))?;
            let value = std::env::var(variable)
                .map_err(|_| McpError::Config("header environment variable unavailable"))?;
            let mut value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| McpError::Config("invalid header environment value"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(timeout)
            .build()
            .map_err(|_| McpError::Protocol("HTTP client"))?;
        let config = StreamableHttpClientTransportConfig::with_uri(
            config.url.clone().expect("validated URL"),
        )
        .custom_headers(headers)
        .max_concurrent_requests(1)
        .max_sse_event_size(MAX_RESULT)
        .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(client, config);
        let client = tokio::time::timeout(timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::Timeout("connect"))?
            .map_err(|_| McpError::Protocol("initialize"))?;
        Ok(Connection {
            client,
            tools: Vec::new(),
            stderr: None,
        })
    }
}

async fn shutdown(connection: &mut Connection) -> Result<(), McpError> {
    // SDK stdio close waits up to 3s, then kills and reaps its owned child.
    match connection
        .client
        .close_with_timeout(Duration::from_secs(5))
        .await
    {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(McpError::Timeout("disconnect")),
        Err(_) => Err(McpError::Protocol("disconnect")),
    }
}

async fn discover(client: &Client, timeout_ms: u64) -> Result<Vec<McpToolInfo>, McpError> {
    if client
        .peer_info()
        .is_some_and(|info| info.capabilities.tools.is_none())
    {
        return Ok(Vec::new());
    }
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut names = HashSet::new();
        let mut bytes = 0;
        for _ in 0..MAX_PAGES {
            let mut params = PaginatedRequestParams::default();
            params.cursor = cursor;
            let page = client
                .list_tools(Some(params))
                .await
                .map_err(|_| McpError::Protocol("list tools"))?;
            for tool in page.tools {
                bytes += serde_json::to_vec(&tool)
                    .map_err(|_| McpError::Protocol("tool schema"))?
                    .len();
                if tools.len() >= MAX_TOOLS
                    || bytes > MAX_RESULT
                    || !names.insert(tool.name.to_string())
                {
                    return Err(McpError::Limit);
                }
                tools.push(McpToolInfo {
                    name: tool.name.to_string(),
                    description: tool.description.map(|s| s.to_string()),
                    input_schema: Value::Object((*tool.input_schema).clone()),
                });
            }
            cursor = page.next_cursor;
            match &cursor {
                None => return Ok(tools),
                Some(cursor) if !cursors.insert(cursor.clone()) => return Err(McpError::Limit),
                _ => {}
            }
        }
        Err(McpError::Limit)
    })
    .await
    .map_err(|_| McpError::Timeout("list tools"))?
}

fn bounded(value: Value) -> Result<Value, McpError> {
    if serde_json::to_vec(&value)
        .map_err(|_| McpError::Protocol("serialization"))?
        .len()
        > MAX_RESULT
    {
        Err(McpError::Limit)
    } else {
        Ok(value)
    }
}

fn validate(name: &str, config: &ServerConfig) -> Result<(), McpError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))
    {
        return Err(McpError::Config("invalid server name"));
    }
    if config.command.is_some() == config.url.is_some() {
        return Err(McpError::Config("exactly one of command or url required"));
    }
    if [
        config.connect_timeout_ms,
        config.list_timeout_ms,
        config.call_timeout_ms,
    ]
    .iter()
    .any(|ms| !(1..=300_000).contains(ms))
    {
        return Err(McpError::Config("timeouts must be 1..300000 milliseconds"));
    }
    if serde_json::to_vec(config)
        .map_err(|_| McpError::Config("invalid server"))?
        .len()
        > 64 * 1024
    {
        return Err(McpError::Config("server configuration exceeds 64 KiB"));
    }
    if config
        .description
        .as_ref()
        .is_some_and(|s| s.len() > 1000 || s.chars().any(char::is_control))
    {
        return Err(McpError::Config(
            "description must be display-safe and at most 1000 bytes",
        ));
    }
    if let Some(command) = &config.command {
        if command.trim().is_empty() || command.contains('\0') || !config.http_headers.is_empty() {
            return Err(McpError::Config("invalid stdio configuration"));
        }
    }
    if let Some(url) = &config.url {
        let url = reqwest::Url::parse(url).map_err(|_| McpError::Config("invalid HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(McpError::Config(
                "HTTP URL must not contain userinfo or fragment",
            ));
        }
        if !config.args.is_empty() || config.cwd.is_some() || !config.env.is_empty() {
            return Err(McpError::Config(
                "HTTP configuration cannot include command fields",
            ));
        }
        for (name, variable) in &config.http_headers {
            if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
                || variable.is_empty()
                || !variable
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(McpError::Config(
                    "headers must map valid names to environment variable names",
                ));
            }
        }
    }
    Ok(())
}
