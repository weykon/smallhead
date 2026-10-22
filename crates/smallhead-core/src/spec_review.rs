//! One bounded, revisioned specification per session. The UI owns durable
//! comments and steering; this broker never waits for a reviewer.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    diagnostics,
    message::ContentBlock,
    tool::{ToolContext, ToolError, ToolImpl, ToolResult, ToolSpec},
};

/// Maximum Markdown size in bytes retained by the broker.
pub const MAX_SPEC_BYTES: usize = 256 * 1024;

/// Complete document snapshot, suitable for durable UI storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpecDocument {
    /// Stable document identity, generated on first publication.
    pub id: String,
    /// Short display title (at most 160 bytes).
    pub title: String,
    /// Full Markdown source, preserved verbatim.
    pub markdown: String,
    /// Positive, monotonically increasing revision within this document.
    pub revision: u64,
    /// Exact revision approved by the UI; never carried across an update.
    #[serde(default)]
    pub approved_revision: Option<u64>,
}

impl SpecDocument {
    /// Whether the current contents have explicit UI approval.
    pub fn is_approved(&self) -> bool {
        self.approved_revision == Some(self.revision)
    }
}

/// Rejected operations leave the broker unchanged.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    /// Invalid document or publication arguments.
    #[error("invalid spec: {0}")]
    Invalid(String),
    /// A session already has a current spec.
    #[error("a spec already exists; update it with its id and expected_revision")]
    AlreadyExists,
    /// No current document matches the requested identity.
    #[error("spec not found: {0}")]
    NotFound(String),
    /// The caller acted on an obsolete revision.
    #[error("spec revision conflict: expected {expected}, current {actual}")]
    RevisionConflict {
        /// Revision supplied by the caller.
        expected: u64,
        /// Revision currently held by the broker.
        actual: u64,
    },
}

/// Synchronous shared state. Retains only one bounded document and no history.
#[derive(Debug, Default)]
pub struct SpecBroker {
    current: Mutex<Option<SpecDocument>>,
}

impl SpecBroker {
    /// Create an empty session broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read an atomic snapshot for rendering or persistence.
    pub fn current(&self) -> Option<SpecDocument> {
        self.current.lock().clone()
    }

    /// Start a new spec without an id, or replace the matching revision.
    /// Updates always require expected_revision, even if contents are identical.
    pub fn publish(
        &self,
        id: Option<&str>,
        title: &str,
        markdown: &str,
        expected_revision: Option<u64>,
    ) -> Result<SpecDocument, SpecError> {
        validate_contents(title, markdown)?;
        let mut current = self.current.lock();
        let (id, revision) = match (id, expected_revision) {
            (None, None) => {
                if current.is_some() {
                    return Err(SpecError::AlreadyExists);
                }
                (uuid::Uuid::now_v7().to_string(), 1)
            }
            (Some(id), Some(expected)) => {
                let doc = matching_document(&current, id, expected)?;
                let next = doc
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| SpecError::Invalid("revision exhausted".into()))?;
                (doc.id.clone(), next)
            }
            _ => {
                return Err(SpecError::Invalid(
                    "new specs omit id and expected_revision; updates require both".into(),
                ))
            }
        };
        let document = SpecDocument {
            id,
            title: title.to_owned(),
            markdown: markdown.to_owned(),
            revision,
            approved_revision: None,
        };
        *current = Some(document.clone());
        Ok(document)
    }

    /// Restore a persisted checkpoint into an empty session without renumbering.
    /// The UI must explicitly clear before switching sessions. Approval, if
    /// present, must refer to exactly the restored revision.
    pub fn restore(&self, document: SpecDocument) -> Result<SpecDocument, SpecError> {
        validate_contents(&document.title, &document.markdown)?;
        if document.id.trim().is_empty()
            || document.id.len() > 128
            || document.revision == 0
            || document
                .approved_revision
                .is_some_and(|r| r != document.revision)
        {
            return Err(SpecError::Invalid(
                "invalid checkpoint id, revision, or approval".into(),
            ));
        }
        let mut current = self.current.lock();
        if current.is_some() {
            return Err(SpecError::AlreadyExists);
        }
        *current = Some(document.clone());
        Ok(document)
    }

    /// Record UI approval only if the reviewed identity and revision still match.
    pub fn approve(&self, id: &str, revision: u64) -> Result<SpecDocument, SpecError> {
        let mut current = self.current.lock();
        matching_document(&current, id, revision)?;
        let document = current.as_mut().expect("matching document exists");
        document.approved_revision = Some(revision);
        Ok(document.clone())
    }

    /// Reopen the exact reviewed revision after the UI accepts a comment.
    /// Clears approval without changing contents or revision; repeated calls
    /// on the same current revision are harmless.
    pub fn reopen(&self, id: &str, revision: u64) -> Result<SpecDocument, SpecError> {
        let mut current = self.current.lock();
        matching_document(&current, id, revision)?;
        let document = current.as_mut().expect("matching document exists");
        document.approved_revision = None;
        Ok(document.clone())
    }

    /// Drop this session's checkpoint when the UI switches or resets sessions.
    pub fn clear(&self) {
        *self.current.lock() = None;
    }
}

fn validate_contents(title: &str, markdown: &str) -> Result<(), SpecError> {
    if title.trim().is_empty() || title.len() > 160 {
        return Err(SpecError::Invalid(
            "title must be nonempty and at most 160 bytes".into(),
        ));
    }
    if markdown.trim().is_empty() || markdown.len() > MAX_SPEC_BYTES {
        return Err(SpecError::Invalid(format!(
            "markdown must be nonempty and at most {MAX_SPEC_BYTES} bytes"
        )));
    }
    Ok(())
}

fn matching_document<'a>(
    current: &'a Option<SpecDocument>,
    id: &str,
    expected: u64,
) -> Result<&'a SpecDocument, SpecError> {
    let document = current
        .as_ref()
        .filter(|doc| doc.id == id)
        .ok_or_else(|| SpecError::NotFound(id.to_owned()))?;
    if document.revision != expected {
        return Err(SpecError::RevisionConflict {
            expected,
            actual: document.revision,
        });
    }
    Ok(document)
}

/// Publish a reviewable specification and immediately return its revision.
#[derive(Debug, Clone)]
pub struct PublishSpecTool {
    /// Session broker shared with the UI; independent of ToolContext.
    pub broker: Arc<SpecBroker>,
}

impl PublishSpecTool {
    /// Bind this tool to the session's shared broker.
    pub fn new(broker: Arc<SpecBroker>) -> Self {
        Self { broker }
    }
}

// Tool identity must remain stable as its broker's contents change.
impl PartialEq for PublishSpecTool {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.broker, &other.broker)
    }
}
impl Eq for PublishSpecTool {}
impl Hash for PublishSpecTool {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.broker).hash(state);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishInput {
    id: Option<String>,
    title: String,
    markdown: String,
    expected_revision: Option<u64>,
}

#[async_trait]
impl ToolImpl for PublishSpecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_spec".into(),
            label: "Update specification".into(),
            description: "Publish a complete Markdown specification for user review. For a new spec omit id and expected_revision; to revise supply the current id and expected_revision. Returns the submitted document immediately, without waiting for review. Publishing means awaiting review, NOT approval: do not implement until the UI explicitly approves the matching document id and revision. User comments arrive as steering; revise the full document in response. Every publication invalidates previous approval.".into(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "required": ["title", "markdown"],
                "properties": {
                    "id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "title": {"type": "string", "minLength": 1, "maxLength": 160},
                    "markdown": {"type": "string", "minLength": 1, "maxLength": MAX_SPEC_BYTES},
                    "expected_revision": {"type": "integer", "minimum": 1}
                },
                "oneOf": [
                    {"required": ["id", "expected_revision"]},
                    {"not": {"anyOf": [{"required": ["id"]}, {"required": ["expected_revision"]}]}}
                ]
            }),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        if ctx.cancel {
            return Err(ToolError::Execution("spec publication cancelled".into()));
        }
        let input: PublishInput = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let document = self
            .broker
            .publish(
                input.id.as_deref(),
                &input.title,
                &input.markdown,
                input.expected_revision,
            )
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let text = serde_json::to_string(&document)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        diagnostics::emit(
            "spec",
            "spec_updated",
            serde_json::json!({
                "owner": ctx.owner,
                "tool_call_id": ctx.tool_call_id,
                "document": document,
            }),
        );
        Ok(ToolResult {
            tool_call_id: ctx.tool_call_id.clone().unwrap_or_default(),
            is_error: false,
            content: vec![ContentBlock::Text { text }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_and_approval_invalidation() {
        let broker = SpecBroker::new();
        let first = broker.publish(None, "Design", "# Draft", None).unwrap();
        assert_eq!(first.revision, 1);
        let approved = broker.approve(&first.id, 1).unwrap();
        assert!(approved.is_approved());
        assert!(broker
            .publish(Some(&first.id), "Design", "bad", None)
            .is_err());
        assert!(broker.publish(None, "Design", "bad", None).is_err());
        assert!(broker
            .publish(Some("wrong"), "Design", "bad", Some(1))
            .is_err());
        assert_eq!(broker.current(), Some(approved));
        let second = broker
            .publish(Some(&first.id), "Design", "# Revised", Some(1))
            .unwrap();
        assert_eq!(second.revision, 2);
        assert!(!second.is_approved());
        assert!(matches!(
            broker.approve(&first.id, 1),
            Err(SpecError::RevisionConflict { .. })
        ));
        assert!(matches!(
            broker.publish(Some(&first.id), "Design", "stale", Some(1)),
            Err(SpecError::RevisionConflict { .. })
        ));
        assert_eq!(broker.current(), Some(second));
    }

    #[test]
    fn reopen_revokes_only_matching_approval_and_is_idempotent() {
        let broker = SpecBroker::new();
        assert!(matches!(
            broker.reopen("missing", 1),
            Err(SpecError::NotFound(_))
        ));
        let first = broker.publish(None, "Design", "# Draft", None).unwrap();
        let second = broker
            .publish(Some(&first.id), "Design", "# Revised", Some(1))
            .unwrap();
        let approved = broker.approve(&second.id, second.revision).unwrap();
        assert_eq!(
            broker.approve(&second.id, second.revision).unwrap(),
            approved
        );
        assert!(matches!(
            broker.reopen("wrong", second.revision),
            Err(SpecError::NotFound(_))
        ));
        assert!(matches!(
            broker.reopen(&first.id, first.revision),
            Err(SpecError::RevisionConflict {
                expected: 1,
                actual: 2
            })
        ));
        assert_eq!(broker.current(), Some(approved));
        let reopened = broker.reopen(&second.id, second.revision).unwrap();
        assert_eq!(reopened, second, "only approval changes");
        assert_eq!(broker.current(), Some(reopened.clone()));
        assert_eq!(
            broker.reopen(&second.id, second.revision).unwrap(),
            reopened
        );
        assert!(broker
            .approve(&second.id, second.revision)
            .unwrap()
            .is_approved());
    }

    #[test]
    fn restore_preserves_checkpoint_and_rejects_invalid_state() {
        let broker = SpecBroker::new();
        let checkpoint = SpecDocument {
            id: "saved".into(),
            title: "Saved".into(),
            markdown: "# Saved".into(),
            revision: 42,
            approved_revision: Some(42),
        };
        assert_eq!(broker.restore(checkpoint.clone()).unwrap(), checkpoint);
        assert!(broker.restore(checkpoint.clone()).is_err());
        let next = broker
            .publish(Some("saved"), "Saved", "# Next", Some(42))
            .unwrap();
        assert_eq!(next.revision, 43);
        assert_eq!(next.approved_revision, None);
        broker.clear();
        let mut invalid = checkpoint.clone();
        invalid.approved_revision = Some(41);
        assert!(broker.restore(invalid).is_err());
        assert!(broker.current().is_none());
        assert!(broker
            .publish(None, "Large", &"x".repeat(MAX_SPEC_BYTES + 1), None)
            .is_err());
        broker.restore(checkpoint).unwrap();
    }

    #[test]
    fn concurrent_updates_have_one_winner() {
        let broker = Arc::new(SpecBroker::new());
        let first = broker.publish(None, "Design", "Draft", None).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let broker = broker.clone();
                let barrier = barrier.clone();
                let id = first.id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    broker.publish(Some(&id), "Design", "Revised", Some(1))
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(wins, 1);
        assert_eq!(broker.current().unwrap().revision, 2);
    }

    #[tokio::test]
    async fn tool_returns_without_review_and_emits_owner() {
        let tool = PublishSpecTool::new(Arc::new(SpecBroker::new()));
        let ctx = ToolContext {
            owner: Some("task-7".into()),
            tool_call_id: Some("call-1".into()),
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        diagnostics::observe(tx, async {
            let result = tokio::time::timeout(std::time::Duration::from_secs(1),
                tool.execute(serde_json::json!({"title": "Design", "markdown": "# Draft"}), &ctx)).await.unwrap().unwrap();
            let ContentBlock::Text { text } = &result.content[0] else { panic!("expected text") };
            let document: SpecDocument = serde_json::from_str(text).unwrap();
            assert_eq!(result.tool_call_id, "call-1");
            assert!(!document.is_approved());
            let event = rx.try_recv().unwrap();
            assert_eq!(event.layer, "spec");
            assert_eq!(event.event, "spec_updated");
            assert_eq!(event.data["owner"], "task-7");
            assert_eq!(event.data["document"], serde_json::to_value(&document).unwrap());
            assert!(tool.execute(serde_json::json!({"id": document.id, "title": "Design", "markdown": "missing CAS"}), &ctx).await.is_err());
            assert!(rx.try_recv().is_err());
            assert_eq!(tool.broker.current(), Some(document));
        }).await;
    }
}
