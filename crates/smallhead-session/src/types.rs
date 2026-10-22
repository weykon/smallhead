//! 公共类型：Session / SessionMeta / SessionStore trait / SessionError。

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smallhead_core::{AgentMessage, ContentBlock};

/// 会话 ID —— MVP 用 UUID v7 字符串（time-ordered，便于 list 排序时也 fallback 到 updated_at）。
pub type SessionId = String;

/// 生成新 session ID（UUID v7，time-ordered）。
pub fn new_session_id() -> SessionId {
    uuid::Uuid::now_v7().to_string()
}

/// 当前 epoch millis（`SystemTime::now()`）。失败时返回 0（极端边界）。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 完整 session 快照 —— 序列化整个对话历史 + system prompt。
///
/// MVP 只做全量快照；incremental journal（只 append 增量消息）是 v0.5 的事。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// session ID（UUID v7 字符串）。
    pub id: SessionId,
    /// 创建时间（epoch millis）。
    pub created_at: i64,
    /// 最后一次 save 时间（epoch millis）。
    pub updated_at: i64,
    /// AgentLoop 的 system prompt —— resume 时恢复。
    pub system_prompt: String,
    /// Artifact root used by projected tool results and compacted history.
    #[serde(default)]
    pub artifact_root: Option<PathBuf>,
    /// 完整 messages 历史（含 user / assistant / tool_result）。
    pub messages: Vec<AgentMessage>,
}

impl Session {
    /// 新建一个空 session（只有 id + created_at + system_prompt）。
    pub fn new(id: SessionId, system_prompt: impl Into<String>) -> Self {
        let now = now_millis();
        Self {
            id,
            created_at: now,
            updated_at: now,
            system_prompt: system_prompt.into(),
            artifact_root: None,
            messages: Vec::new(),
        }
    }
}

/// 轻量元信息 —— 用于 list（不读全 session 文件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// session ID。
    pub id: SessionId,
    /// 创建时间（epoch millis）。
    pub created_at: i64,
    /// 最后一次 save 时间（epoch millis）。
    pub updated_at: i64,
    /// messages 数量。
    pub message_count: usize,
    /// 第一条 user message 的文本（最多 60 chars），给 list 表格预览用。
    pub first_user_message: Option<String>,
}

/// 从 `Session` 派生 `SessionMeta`。
pub fn meta_from_session(session: &Session) -> SessionMeta {
    SessionMeta {
        id: session.id.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        message_count: session.messages.len(),
        first_user_message: extract_first_user_text(&session.messages).map(|s| {
            if s.chars().count() > 60 {
                let mut out: String = s.chars().take(60).collect();
                out.push('…');
                out
            } else {
                s
            }
        }),
    }
}

/// 取出第一条 user message 的 text（trim 后非空）。
fn extract_first_user_text(messages: &[AgentMessage]) -> Option<String> {
    for m in messages {
        if let AgentMessage::User { content, .. } = m {
            for c in content {
                if let ContentBlock::Text { text } = c {
                    let t = text.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Session 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// IO 错误（文件读写、目录创建等）。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化 / 反序列化错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// session 不存在。
    #[error("session not found: {0}")]
    NotFound(String),
    /// 其他错误（如 HOME 未设置等）。
    #[error("session store error: {0}")]
    Other(String),
}

/// Session 存储抽象 trait。
///
/// MVP 只提供 `JsonFileStore` 一个 impl，但留 trait 方便后续加 SQLite / S3。
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 保存（全量覆盖；同名文件已存在则替换）。
    async fn save(&self, session: &Session) -> Result<(), SessionError>;
    /// 加载整个 session。
    async fn load(&self, id: &SessionId) -> Result<Session, SessionError>;
    /// 列出所有 session 的元信息（按 updated_at 倒序）。
    async fn list(&self) -> Result<Vec<SessionMeta>, SessionError>;
    /// 删除一个 session（同时从 index.json 里移除）。
    async fn delete(&self, id: &SessionId) -> Result<(), SessionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallhead_core::AgentMessage;

    #[test]
    fn meta_from_session_picks_first_user_text() {
        let mut s = Session::new("abc".into(), "sys");
        s.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "  hi there  ".into(),
            }],
            timestamp: 0,
        });
        let meta = meta_from_session(&s);
        assert_eq!(meta.first_user_message.as_deref(), Some("hi there"));
        assert_eq!(meta.id, "abc");
        assert_eq!(meta.message_count, 1);
    }

    #[test]
    fn meta_from_session_truncates_long_text_to_60_chars() {
        let long = "x".repeat(200);
        let mut s = Session::new("abc".into(), "sys");
        s.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text { text: long }],
            timestamp: 0,
        });
        let meta = meta_from_session(&s);
        let preview = meta.first_user_message.expect("should have preview");
        // 60 chars + ellipsis
        assert_eq!(preview.chars().count(), 61);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn meta_from_session_returns_none_when_no_user_message() {
        let s = Session::new("abc".into(), "sys");
        let meta = meta_from_session(&s);
        assert!(meta.first_user_message.is_none());
        assert_eq!(meta.message_count, 0);
    }

    #[test]
    fn session_deserializes_without_artifact_root() {
        let json = r#"{
            "id": "abc",
            "created_at": 1,
            "updated_at": 2,
            "system_prompt": "sys",
            "messages": []
        }"#;

        let session: Session = serde_json::from_str(json).unwrap();

        assert!(session.artifact_root.is_none());
    }

    #[test]
    fn new_session_id_is_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        // UUID v7 length
        assert_eq!(a.len(), 36);
    }
}
