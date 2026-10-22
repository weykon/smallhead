//! Structured user choice requests for Agent/TUI coordination.
//!
//! The Agent does not render UI directly. It emits a typed request, the
//! frontend displays it as cards/buttons, and the selected option is returned
//! as structured data.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};

/// Stable identifier for a pending choice request.
pub type ChoiceId = String;

/// Request shown to the user as a choice card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceRequest {
    /// Unique request id.
    pub id: ChoiceId,
    /// Optional task/owner id for audit grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Short title shown on the card.
    pub title: String,
    /// Body text with the context needed to decide.
    pub body: String,
    /// Selectable options. Keep this short; the TUI is optimized for 2-4.
    pub options: Vec<ChoiceOption>,
    /// Option id to use when the UI times out or caller wants a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_option: Option<String>,
    /// Optional caller-side wait timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Request source such as `agent_tool`, `tui`, or `runtime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Agent tool call id that created this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Milliseconds since Unix epoch.
    pub created_at_ms: u64,
}

/// One selectable option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// Stable id returned on selection.
    pub id: String,
    /// Short button label.
    pub label: String,
    /// Optional explanatory copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Structured meaning for the option.
    #[serde(default)]
    pub intent: ChoiceIntent,
}

/// Structured option meaning.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChoiceIntent {
    /// Return the choice to the Agent and continue.
    #[default]
    Continue,
    /// User wants the current task cancelled.
    CancelTask,
    /// Execute a slash command in the TUI after selection.
    RunCommand {
        /// Slash command text.
        command: String,
    },
    /// Tool-specific intent payload.
    Custom {
        /// Caller-defined intent kind.
        kind: String,
        /// Caller-defined JSON payload.
        #[serde(default)]
        payload: serde_json::Value,
    },
}

/// User response to a choice request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceResponse {
    /// Choice request id.
    pub choice_id: ChoiceId,
    /// Selected option id.
    pub selected_option_id: String,
    /// Full selected option snapshot.
    pub selected: ChoiceOption,
    /// Milliseconds since Unix epoch.
    pub answered_at_ms: u64,
}

/// Choice broker failure.
#[derive(Debug, Error)]
pub enum ChoiceError {
    /// Request was invalid.
    #[error("invalid choice request: {0}")]
    Invalid(String),
    /// Choice id is unknown or already answered.
    #[error("choice not found: {0}")]
    NotFound(String),
    /// Option id does not belong to the request.
    #[error("choice option not found: {0}")]
    OptionNotFound(String),
    /// Waiting for a response timed out.
    #[error("choice timed out: {0}")]
    Timeout(String),
    /// The UI side was dropped.
    #[error("choice response channel closed: {0}")]
    Closed(String),
}

#[derive(Clone)]
struct PendingChoice {
    request: ChoiceRequest,
    responder: Option<std::sync::Arc<parking_lot::Mutex<Option<oneshot::Sender<ChoiceResponse>>>>>,
}

struct PendingChoiceGuard {
    broker: ChoiceBroker,
    choice_id: ChoiceId,
}

impl Drop for PendingChoiceGuard {
    fn drop(&mut self) {
        let _ = self.broker.remove_pending(&self.choice_id);
    }
}

/// In-process broker shared by Agent tools and UI.
#[derive(Clone)]
pub struct ChoiceBroker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ChoiceBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChoiceBroker")
            .field("pending", &self.inner.pending.read().len())
            .finish()
    }
}

struct Inner {
    pending: parking_lot::RwLock<BTreeMap<ChoiceId, PendingChoice>>,
    events: broadcast::Sender<ChoiceRequest>,
}

impl Default for ChoiceBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ChoiceBroker {
    /// Create an empty broker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                pending: parking_lot::RwLock::new(BTreeMap::new()),
                events: broadcast::channel(64).0,
            }),
        }
    }

    /// Subscribe to newly-created choice requests.
    pub fn subscribe(&self) -> broadcast::Receiver<ChoiceRequest> {
        self.inner.events.subscribe()
    }

    /// Current pending requests.
    pub fn pending(&self) -> Vec<ChoiceRequest> {
        self.inner
            .pending
            .read()
            .values()
            .map(|pending| pending.request.clone())
            .collect()
    }

    /// Present a choice without waiting for a response.
    pub fn present(&self, mut request: ChoiceRequest) -> Result<ChoiceId, ChoiceError> {
        self.prepare_request(&mut request)?;
        let id = request.id.clone();
        let mut pending = self.inner.pending.write();
        if pending.contains_key(&id) {
            return Err(ChoiceError::Invalid("choice id already exists".into()));
        }
        pending.insert(
            id.clone(),
            PendingChoice {
                request: request.clone(),
                responder: None,
            },
        );
        drop(pending);
        let _ = self.inner.events.send(request);
        Ok(id)
    }

    /// Ask the user and await the selected option.
    pub async fn ask(&self, mut request: ChoiceRequest) -> Result<ChoiceResponse, ChoiceError> {
        self.prepare_request(&mut request)?;
        let id = request.id.clone();
        let (tx, rx) = oneshot::channel();
        let responder = std::sync::Arc::new(parking_lot::Mutex::new(Some(tx)));
        {
            let mut pending = self.inner.pending.write();
            if pending.contains_key(&id) {
                return Err(ChoiceError::Invalid("choice id already exists".into()));
            }
            pending.insert(
                id.clone(),
                PendingChoice {
                    request: request.clone(),
                    responder: Some(responder),
                },
            );
        }
        let _ = self.inner.events.send(request.clone());
        let _guard = PendingChoiceGuard {
            broker: self.clone(),
            choice_id: id.clone(),
        };

        let wait = async {
            rx.await
                .map_err(|_| ChoiceError::Closed(format!("choice {id} response dropped")))
        };
        match request.timeout_ms {
            Some(ms) => tokio::time::timeout(Duration::from_millis(ms), wait)
                .await
                .map_err(|_| {
                    let _ = self.remove_pending(&id);
                    ChoiceError::Timeout(id.clone())
                })?,
            None => wait.await,
        }
    }

    /// Answer a pending choice.
    pub fn answer(&self, choice_id: &str, option_id: &str) -> Result<ChoiceResponse, ChoiceError> {
        let (pending, selected) = {
            let mut pending_map = self.inner.pending.write();
            let pending = pending_map
                .get(choice_id)
                .cloned()
                .ok_or_else(|| ChoiceError::NotFound(choice_id.into()))?;
            let selected = pending
                .request
                .options
                .iter()
                .find(|option| option.id == option_id)
                .cloned()
                .ok_or_else(|| ChoiceError::OptionNotFound(option_id.into()))?;
            let pending = pending_map
                .remove(choice_id)
                .expect("pending choice existed before removal");
            (pending, selected)
        };
        let response = ChoiceResponse {
            choice_id: choice_id.into(),
            selected_option_id: option_id.into(),
            selected,
            answered_at_ms: now_millis(),
        };
        if let Some(sender) = pending.responder.and_then(|sender| sender.lock().take()) {
            let _ = sender.send(response.clone());
        }
        Ok(response)
    }

    /// Answer a pending choice with free-form user text.
    pub fn answer_custom(
        &self,
        choice_id: &str,
        text: impl Into<String>,
    ) -> Result<ChoiceResponse, ChoiceError> {
        let text = text.into().trim().to_string();
        if text.is_empty() || text.len() > 2000 {
            return Err(ChoiceError::Invalid(
                "custom choice text must be 1..2000 bytes".into(),
            ));
        }
        let pending = self
            .inner
            .pending
            .write()
            .remove(choice_id)
            .ok_or_else(|| ChoiceError::NotFound(choice_id.into()))?;
        let response = ChoiceResponse {
            choice_id: choice_id.into(),
            selected_option_id: "custom_input".into(),
            selected: ChoiceOption {
                id: "custom_input".into(),
                label: "自定义输入".into(),
                description: Some(text.clone()),
                intent: ChoiceIntent::Custom {
                    kind: "free_text".into(),
                    payload: serde_json::json!({ "text": text }),
                },
            },
            answered_at_ms: now_millis(),
        };
        if let Some(sender) = pending.responder.and_then(|sender| sender.lock().take()) {
            let _ = sender.send(response.clone());
        }
        Ok(response)
    }

    fn remove_pending(&self, choice_id: &str) -> Option<PendingChoice> {
        self.inner.pending.write().remove(choice_id)
    }

    fn prepare_request(&self, request: &mut ChoiceRequest) -> Result<(), ChoiceError> {
        if request.id.trim().is_empty() {
            request.id = uuid::Uuid::now_v7().to_string();
        }
        if request.title.trim().is_empty()
            || request.title.len() > 160
            || request.body.len() > 2000
            || request.options.is_empty()
            || request.options.len() > 4
        {
            return Err(ChoiceError::Invalid(
                "title/body/options must fit choice card limits".into(),
            ));
        }
        for option in &request.options {
            if option.id.trim().is_empty()
                || option.id.len() > 64
                || option.label.trim().is_empty()
                || option.label.len() > 80
                || option.description.as_ref().is_some_and(|d| d.len() > 240)
            {
                return Err(ChoiceError::Invalid(
                    "option id/label/description must fit limits".into(),
                ));
            }
        }
        if let Some(default) = &request.default_option {
            if !request.options.iter().any(|option| &option.id == default) {
                return Err(ChoiceError::Invalid(
                    "default_option must reference an option id".into(),
                ));
            }
        }
        if request
            .timeout_ms
            .is_some_and(|ms| !(100..=600000).contains(&ms))
        {
            return Err(ChoiceError::Invalid(
                "timeout_ms must be 100..600000".into(),
            ));
        }
        if request.created_at_ms == 0 {
            request.created_at_ms = now_millis();
        }
        Ok(())
    }
}

/// Current Unix epoch milliseconds.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChoiceRequest {
        ChoiceRequest {
            id: "choice-1".into(),
            owner: Some("task".into()),
            title: "文件已存在".into(),
            body: "请选择处理方式".into(),
            options: vec![
                ChoiceOption {
                    id: "overwrite".into(),
                    label: "覆盖".into(),
                    description: Some("替换现有内容".into()),
                    intent: ChoiceIntent::Continue,
                },
                ChoiceOption {
                    id: "cancel".into(),
                    label: "取消".into(),
                    description: None,
                    intent: ChoiceIntent::CancelTask,
                },
            ],
            default_option: None,
            timeout_ms: None,
            source: Some("test".into()),
            tool_call_id: None,
            created_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn ask_returns_selected_option() {
        let broker = ChoiceBroker::new();
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.ask(request()).await.unwrap() })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while broker.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let response = broker.answer("choice-1", "overwrite").unwrap();
        assert_eq!(response.selected_option_id, "overwrite");
        assert_eq!(waiter.await.unwrap().selected.label, "覆盖");
        assert!(broker.pending().is_empty());
    }

    #[tokio::test]
    async fn ask_drop_removes_pending_choice() {
        let broker = ChoiceBroker::new();
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move {
                let _ = broker.ask(request()).await;
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while broker.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        waiter.abort();
        let _ = waiter.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            while !broker.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ask_accepts_custom_text_answer() {
        let broker = ChoiceBroker::new();
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.ask(request()).await.unwrap() })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while broker.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let response = broker.answer_custom("choice-1", "weykonk").unwrap();
        assert_eq!(response.selected_option_id, "custom_input");
        assert_eq!(response.selected.description.as_deref(), Some("weykonk"));
        let ChoiceIntent::Custom { kind, payload } = &response.selected.intent else {
            panic!("expected custom intent")
        };
        assert_eq!(kind, "free_text");
        assert_eq!(payload["text"], "weykonk");
        assert_eq!(
            waiter.await.unwrap().selected.description.as_deref(),
            Some("weykonk")
        );
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn invalid_option_does_not_remove_pending_choice() {
        let broker = ChoiceBroker::new();
        broker.present(request()).unwrap();
        assert!(matches!(
            broker.answer("choice-1", "missing"),
            Err(ChoiceError::OptionNotFound(_))
        ));
        assert_eq!(broker.pending().len(), 1);
        assert_eq!(
            broker
                .answer("choice-1", "cancel")
                .unwrap()
                .selected_option_id,
            "cancel"
        );
        assert!(broker.pending().is_empty());
    }
}
