//! Local A2A bridge for smallhead — ported from big-head's `src/a2a.rs`.
//!
//! The HTTP layer only validates, queues and polls. The actual turn runs in
//! [`crate::driver::run_turn`] (a headless `MainMonad` + `AgentLoop`), never in
//! the request path, so a slow model turn can time out into `queued` and finish
//! later without blocking the server.
//!
//! Both roles live here because the protocol is symmetric:
//! - [`A2aServer`] — receive messages from a peer and answer tasks.
//! - [`A2aClient`] — send messages to a peer and poll its task store.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smallhead_session::{new_session_id, JsonFileStore, Session, SessionStore};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_REFS: usize = 32;
const MAX_TTL: u8 = 8;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_PORT: u16 = 3999;

/// One inbound/outbound message. `from` is a claim, not an identity: the bridge
/// is loopback-local and carries no authentication.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct A2aMessage {
    pub message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub from: String,
    pub to: Option<String>,
    pub kind: String,
    pub intent: Option<String>,
    pub text: String,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub permissions: A2aPermissions,
    #[serde(default)]
    pub refs: Vec<Value>,
    #[serde(default = "default_ttl")]
    pub ttl: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct A2aPermissions {
    #[serde(default = "default_true")]
    pub can_observe: bool,
    #[serde(default = "default_true")]
    pub can_plan: bool,
    #[serde(default)]
    pub can_mutate: bool,
    #[serde(default)]
    pub can_execute: bool,
    #[serde(default)]
    pub can_forward: bool,
}

impl Default for A2aPermissions {
    fn default() -> Self {
        Self {
            can_observe: true,
            can_plan: true,
            can_mutate: false,
            can_execute: false,
            can_forward: false,
        }
    }
}

impl A2aPermissions {
    /// Reject unavailable capabilities before constructing the request's tools.
    pub fn validate_against(&self, allowed: &Self) -> Result<(), String> {
        for (name, requested, available) in [
            ("observe", self.can_observe, allowed.can_observe),
            ("plan", self.can_plan, allowed.can_plan),
            ("mutate", self.can_mutate, allowed.can_mutate),
            ("execute", self.can_execute, allowed.can_execute),
            ("forward", self.can_forward, allowed.can_forward),
        ] {
            if requested && !available {
                return Err(format!("A2A host does not allow {name}"));
            }
        }
        // A shell can write files; execute-only cannot be implemented by
        // labelling arbitrary shell commands as read-only.
        if self.can_execute && !self.can_mutate {
            return Err("A2A execute requires can_mutate=true".into());
        }
        Ok(())
    }
}

/// One task's settled answer, polled through `GET /a2a/tasks/{task_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aTaskReply {
    pub task_id: String,
    pub message_id: String,
    pub conversation_id: String,
    pub status: String,
    pub from: String,
    pub to: String,
    pub text: String,
    pub errors: Vec<String>,
}

/// Shared task store, keyed by task id.
pub type A2aTaskStore = Arc<Mutex<BTreeMap<String, A2aTaskReply>>>;

/// Server config: the peer name this process advertises and the port it binds.
#[derive(Clone)]
pub struct A2aServerConfig {
    pub name: String,
    pub port: u16,
    /// Directory of a smallhead `JsonFileStore`. When set, every settled A2A
    /// conversation is persisted as one smallhead Session, so the same traffic
    /// shows up in the DSH observer panel. `None` keeps the bridge in-memory,
    /// which is what tests use.
    pub sessions_dir: Option<std::path::PathBuf>,
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            name: "smallhead".into(),
            port: DEFAULT_PORT,
            sessions_dir: None,
        }
    }
}

impl A2aServerConfig {
    /// The default session store location (`~/.smallhead/sessions`) when `HOME`
    /// resolves; `None` otherwise.
    pub fn default_sessions_dir() -> Option<std::path::PathBuf> {
        smallhead_session::JsonFileStore::default_location()
            .ok()
            .map(|store| store.dir.clone())
    }
}

/// Receive side: an axum router serving the four A2A endpoints.
#[derive(Clone)]
struct A2aState {
    config: A2aServerConfig,
    tasks: A2aTaskStore,
    /// Per-conversation message history, so a reply keeps the peer's context.
    conversations: Arc<Mutex<BTreeMap<String, Vec<smallhead_core::AgentMessage>>>>,
    /// Optional smallhead session store, plus the conversation→session mapping
    /// that keeps one A2A conversation on one session file.
    store: Option<Arc<JsonFileStore>>,
    sessions: Arc<Mutex<BTreeMap<String, String>>>,
    runtime: Option<Arc<crate::driver::HeadlessRuntime>>,
    conversation_locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    diagnostics_dir: Option<std::path::PathBuf>,
    active: tokio_util::task::TaskTracker,
}

/// An inbound message handed to the driver task, with a oneshot back to the HTTP layer.
struct InboundA2a {
    task_id: String,
    server_name: String,
    message: A2aMessage,
    reply: oneshot::Sender<A2aTaskReply>,
    tasks: A2aTaskStore,
    /// Conversation history map, written back after each turn so a later message
    /// in the same conversation keeps context.
    conversations: Arc<Mutex<BTreeMap<String, Vec<smallhead_core::AgentMessage>>>>,
    /// Session store to mirror the settled conversation into, when configured.
    store: Option<Arc<JsonFileStore>>,
    sessions: Arc<Mutex<BTreeMap<String, String>>>,
    runtime: Option<Arc<crate::driver::HeadlessRuntime>>,
    conversation_lock: Arc<Mutex<()>>,
    diagnostics_dir: Option<std::path::PathBuf>,
}

pub struct A2aServer {
    config: A2aServerConfig,
    runtime: Option<Arc<crate::driver::HeadlessRuntime>>,
    diagnostics_dir: Option<std::path::PathBuf>,
}

impl A2aServer {
    pub fn new(config: A2aServerConfig) -> Self {
        Self {
            config,
            runtime: None,
            diagnostics_dir: None,
        }
    }

    /// Attach locally configured capabilities and shared execution dependencies.
    pub fn with_runtime(mut self, runtime: Arc<crate::driver::HeadlessRuntime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Record task-local diagnostics without logging credentials or HTTP headers.
    pub fn with_diagnostics_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.diagnostics_dir = dir;
        self
    }

    /// The shared server state (task store + per-conversation history + the
    /// optional session store that mirrors settled conversations to disk).
    fn state(&self) -> A2aState {
        A2aState {
            config: self.config.clone(),
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            conversations: Arc::new(Mutex::new(BTreeMap::new())),
            store: self
                .config
                .sessions_dir
                .clone()
                .map(|dir| Arc::new(JsonFileStore::new(dir))),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            runtime: self.runtime.clone(),
            conversation_locks: Arc::new(Mutex::new(BTreeMap::new())),
            diagnostics_dir: self.diagnostics_dir.clone(),
            active: tokio_util::task::TaskTracker::new(),
        }
    }

    /// The four-endpoint router; tests drive the server over real HTTP instead,
    /// so this stays private.
    fn app(state: A2aState) -> Router {
        Router::new()
            .route("/.well-known/agent-card.json", get(agent_card))
            .route("/health", get(health))
            .route("/a2a/messages", post(send_message))
            .route("/a2a/tasks/:task_id", get(get_task))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .with_state(state)
    }

    /// Bind loopback and report the listener plus the port actually bound, so a
    /// caller (or a test) can serve on an OS-assigned port.
    pub async fn bind(&self) -> Result<(tokio::net::TcpListener, std::net::SocketAddr), String> {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], self.config.port));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|e| format!("A2A bind {address}: {e}"))?;
        let local = listener
            .local_addr()
            .map_err(|e| format!("A2A local_addr: {e}"))?;
        Ok((listener, local))
    }

    /// Serve the router on an already-bound listener until the process stops.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> Result<(), String> {
        self.serve_until(listener, std::future::pending()).await
    }

    /// Stop admission, cancel active loops, persist their outcomes, then reap
    /// this server's managed processes (including its long-running services).
    pub async fn serve_until(
        self,
        listener: tokio::net::TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), String> {
        let state = self.state();
        let active = state.active.clone();
        let stop_active = active.clone();
        let runtime = self.runtime.clone();
        if let Some(runtime) = &runtime {
            runtime.start_services();
        }
        let stop_runtime = runtime.clone();
        let result = axum::serve(listener, Self::app(state))
            .with_graceful_shutdown(async move {
                shutdown.await;
                stop_active.close();
                if let Some(runtime) = stop_runtime {
                    runtime.cancel();
                }
            })
            .await
            .map_err(|e| format!("A2A server: {e}"));
        active.close();
        if let Some(runtime) = &runtime {
            runtime.cancel();
        }
        let settled = tokio::time::timeout(Duration::from_secs(20), active.wait()).await;
        let cleanup = match runtime {
            Some(runtime) => runtime.shutdown().await,
            None => Ok(()),
        };
        result?;
        cleanup?;
        settled.map_err(|_| "A2A task shutdown exceeded 20s".to_string())
    }

    /// Bind loopback and serve until the process stops.
    pub async fn run(self) -> Result<(), String> {
        self.run_until(std::future::pending()).await
    }

    /// Bind loopback and run until the host's shutdown signal arrives.
    pub async fn run_until(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), String> {
        let (listener, address) = self.bind().await?;
        tracing::info!("smallhead A2A bridge listening on http://{address}");
        self.serve_until(listener, shutdown).await
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn agent_card(State(state): State<A2aState>) -> Json<Value> {
    let permissions = state
        .runtime
        .as_ref()
        .map(|r| r.permissions())
        .unwrap_or_default();
    let mut capabilities = vec!["observe", "research", "plan", "reply"];
    if permissions.can_mutate {
        capabilities.push("mutate");
    }
    if permissions.can_execute {
        capabilities.push("execute");
    }
    Json(json!({
        "name": state.config.name,
        "description": "Bounded local A2A bridge for the smallhead agent",
        "protocol": "smallhead-a2a-v1",
        "transport": "http-local",
        "capabilities": capabilities,
        "endpoints": {
            "messages": "/a2a/messages",
            "tasks": "/a2a/tasks/{task_id}",
            "health": "/health"
        },
        "limits": {
            "max_message_bytes": MAX_TEXT_BYTES,
            "max_ttl": MAX_TTL,
            "max_refs": MAX_REFS,
            "mutate": permissions.can_mutate,
            "execute": permissions.can_execute,
            "forward": permissions.can_forward
        }
    }))
}

async fn get_task(
    State(state): State<A2aState>,
    Path(task_id): Path<String>,
) -> Result<Json<A2aTaskReply>, (StatusCode, String)> {
    let tasks = state.tasks.lock().await;
    tasks.get(&task_id).cloned().map(Json).ok_or((
        StatusCode::NOT_FOUND,
        format!("unknown A2A task: {task_id}"),
    ))
}

async fn send_message(
    State(state): State<A2aState>,
    Json(message): Json<A2aMessage>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if state.active.is_closed() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "A2A host is shutting down".into(),
        ));
    }
    let allowed = state
        .runtime
        .as_ref()
        .map(|r| r.permissions())
        .unwrap_or_default();
    validate(&message, &state.config.name, &allowed)?;
    let message_id = message
        .message_id
        .clone()
        .unwrap_or_else(|| format!("msg_{}", Uuid::now_v7()));
    let conversation_id = message
        .conversation_id
        .clone()
        .unwrap_or_else(|| format!("conv_{}", Uuid::now_v7()));
    let mut message = message;
    message.message_id = Some(message_id.clone());
    message.conversation_id = Some(conversation_id.clone());
    let task_id = format!("task_{}", Uuid::now_v7());

    let queued = A2aTaskReply {
        task_id: task_id.clone(),
        message_id: message_id.clone(),
        conversation_id: conversation_id.clone(),
        status: "queued".into(),
        from: state.config.name.clone(),
        to: message.from.clone(),
        text: String::new(),
        errors: Vec::new(),
    };
    state
        .tasks
        .lock()
        .await
        .insert(task_id.clone(), queued.clone());

    let conversation_lock = state
        .conversation_locks
        .lock()
        .await
        .entry(conversation_id.clone())
        .or_default()
        .clone();

    let (reply_tx, reply_rx) = oneshot::channel();
    let inbound = InboundA2a {
        task_id: task_id.clone(),
        server_name: state.config.name.clone(),
        message: message.clone(),
        reply: reply_tx,
        tasks: state.tasks.clone(),
        conversations: state.conversations.clone(),
        store: state.store.clone(),
        sessions: state.sessions.clone(),
        runtime: state.runtime.clone(),
        conversation_lock,
        diagnostics_dir: state.diagnostics_dir.clone(),
    };
    // The turn runs on its own task; the handler only waits bounded time.
    state.active.spawn(execute_turn(inbound));

    match tokio::time::timeout(RESPONSE_TIMEOUT, reply_rx).await {
        Ok(Ok(reply)) => {
            state.tasks.lock().await.insert(task_id, reply.clone());
            Ok((StatusCode::OK, Json(reply)))
        }
        Ok(Err(_)) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "runtime dropped A2A task".into(),
        )),
        Err(_) => Ok((StatusCode::ACCEPTED, Json(queued))),
    }
}

async fn execute_turn(inbound: InboundA2a) {
    let conversation_id = inbound.message.conversation_id.clone().unwrap_or_default();
    let _guard = inbound.conversation_lock.lock().await;
    // Read only after the preceding turn has settled. Fetching in the HTTP
    // handler would let overlapping turns overwrite each other's history.
    let conversation = inbound
        .conversations
        .lock()
        .await
        .get(&conversation_id)
        .cloned()
        .unwrap_or_default();
    // MainMonad pushes the new objective exactly once; do not pre-append it.
    let turn = async {
        match &inbound.runtime {
            Some(runtime) => {
                runtime
                    .run_turn(
                        conversation,
                        inbound.message.text.clone(),
                        inbound.message.permissions.clone(),
                    )
                    .await
            }
            None => {
                crate::driver::run_turn_with_permissions(
                    conversation,
                    inbound.message.text.clone(),
                    inbound.message.permissions.clone(),
                )
                .await
            }
        }
    };
    let outcome = match &inbound.diagnostics_dir {
        Some(dir) => crate::journal::record(dir, &inbound.task_id, turn).await,
        None => turn.await,
    };
    let reply = match outcome {
        Ok(outcome) => {
            let mut messages = outcome.messages;
            if outcome.status != "done" {
                messages.push(smallhead_core::AgentMessage::Notification {
                        kind: "a2a_turn_outcome".into(),
                        payload: json!({"task_id":inbound.task_id,"status":outcome.status,"text":outcome.reply}),
                    });
            }
            if !matches!(
                messages.last(),
                Some(smallhead_core::AgentMessage::Assistant { .. })
            ) {
                messages.push(smallhead_core::AgentMessage::Assistant {
                    content: vec![smallhead_core::ContentBlock::Text {
                        text: outcome.reply.clone(),
                    }],
                    tool_calls: Vec::new(),
                    model: "a2a".into(),
                    usage: None,
                });
            }

            // Keep the settled history for the next message in this conversation.
            if !conversation_id.is_empty() {
                inbound
                    .conversations
                    .lock()
                    .await
                    .insert(conversation_id.clone(), messages.clone());
            }

            // Mirror to the smallhead session store when configured, so A2A
            // traffic appears in the DSH observer panel (one session per conversation).
            if let Some(store) = inbound.store.as_ref() {
                let session_id = {
                    let mut map = inbound.sessions.lock().await;
                    map.get(&conversation_id).cloned().unwrap_or_else(|| {
                        let id = new_session_id();
                        map.insert(conversation_id.clone(), id.clone());
                        id
                    })
                };
                let mut session = Session::new(session_id, crate::driver::SYSTEM.to_string());
                session.messages = messages;
                let _ = SessionStore::save(store.as_ref(), &session).await;
            }

            A2aTaskReply {
                task_id: inbound.task_id.clone(),
                message_id: inbound.message.message_id.clone().unwrap_or_default(),
                conversation_id: conversation_id.clone(),
                errors: if outcome.status == "failed" {
                    vec![outcome.reply.clone()]
                } else {
                    Vec::new()
                },
                status: outcome.status,
                from: inbound.server_name.clone(),
                to: inbound.message.from.clone(),
                text: outcome.reply,
            }
        }
        Err(error) => A2aTaskReply {
            task_id: inbound.task_id.clone(),
            message_id: inbound.message.message_id.clone().unwrap_or_default(),
            conversation_id: conversation_id.clone(),
            status: "failed".into(),
            from: inbound.server_name.clone(),
            to: inbound.message.from.clone(),
            text: String::new(),
            errors: vec![error],
        },
    };

    inbound
        .tasks
        .lock()
        .await
        .insert(inbound.task_id, reply.clone());
    let _ = inbound.reply.send(reply);
}

fn validate(
    message: &A2aMessage,
    name: &str,
    allowed: &A2aPermissions,
) -> Result<(), (StatusCode, String)> {
    for (field, value) in [
        ("from", message.from.as_str()),
        ("kind", message.kind.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_ID_BYTES {
            return Err((StatusCode::BAD_REQUEST, format!("invalid A2A {field}")));
        }
    }
    if let Some(to) = message.to.as_deref() {
        if to != name && to != "*" {
            return Err((
                StatusCode::NOT_FOUND,
                format!("A2A target is not this agent: {to}"),
            ));
        }
    }
    if message.text.trim().is_empty() || message.text.len() > MAX_TEXT_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            "A2A text must be 1..16384 bytes".into(),
        ));
    }
    if message.ttl == 0 || message.ttl > MAX_TTL {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("A2A ttl must be 1..{MAX_TTL}"),
        ));
    }
    message
        .permissions
        .validate_against(allowed)
        .map_err(|error| (StatusCode::FORBIDDEN, error))?;
    if message.refs.len() > MAX_REFS {
        return Err((StatusCode::BAD_REQUEST, "A2A refs limit is 32".into()));
    }
    Ok(())
}

/// Send side: submit a message to a peer and poll its task store.
#[derive(Clone)]
pub struct A2aClient {
    pub base_url: String,
}

impl A2aClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    fn http(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    pub async fn card(&self) -> Result<Value, String> {
        let client = self.http();
        let url = format!("{}/.well-known/agent-card.json", self.base_url);
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn send(&self, message: &A2aMessage) -> Result<A2aTaskReply, String> {
        let client = self.http();
        let url = format!("{}/a2a/messages", self.base_url);
        let mut reply: A2aTaskReply = client
            .post(&url)
            .json(message)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        // A slow peer answers `queued` and finishes in the background; poll its
        // task store up to the bridge timeout so `send` still delivers the reply.
        let deadline = std::time::Instant::now() + RESPONSE_TIMEOUT;
        while reply.status == "queued" && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let poll_url = format!("{}/a2a/tasks/{}", self.base_url, reply.task_id);
            reply = client
                .get(&poll_url)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(reply)
    }

    pub async fn task(&self, task_id: &str) -> Result<A2aTaskReply, String> {
        let client = self.http();
        let url = format!("{}/a2a/tasks/{task_id}", self.base_url);
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())
    }
}

fn default_true() -> bool {
    true
}

fn default_ttl() -> u8 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> A2aMessage {
        A2aMessage {
            message_id: None,
            conversation_id: None,
            from: "dsh".into(),
            to: Some("smallhead".into()),
            kind: "request".into(),
            intent: Some("research".into()),
            text: "检查 smallhead 能力".into(),
            reply_to: None,
            permissions: A2aPermissions::default(),
            refs: Vec::new(),
            ttl: 3,
        }
    }

    #[test]
    fn accepts_bounded_local_message() {
        assert!(validate(&message(), "smallhead", &A2aPermissions::default()).is_ok());
    }

    #[test]
    fn rejects_mutation_and_wrong_target() {
        let mut message = message();
        message.permissions.can_mutate = true;
        assert_eq!(
            validate(&message, "smallhead", &A2aPermissions::default())
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        message.permissions.can_mutate = false;
        message.to = Some("other-agent".into());
        assert_eq!(
            validate(&message, "smallhead", &A2aPermissions::default())
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn development_permissions_still_require_explicit_requests_and_valid_combinations() {
        let allowed = A2aPermissions {
            can_mutate: true,
            can_execute: true,
            ..Default::default()
        };
        assert!(A2aPermissions::default().validate_against(&allowed).is_ok());
        let mut request = message();
        request.permissions = allowed.clone();
        assert!(validate(&request, "smallhead", &allowed).is_ok());
        request.permissions.can_mutate = false;
        assert_eq!(
            validate(&request, "smallhead", &allowed).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        request.permissions.can_mutate = true;
        request.permissions.can_forward = true;
        assert_eq!(
            validate(&request, "smallhead", &allowed).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
    }
}
