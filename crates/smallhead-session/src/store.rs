//! `JsonFileStore`：文件系统 JSON snapshot 实现。
//!
//! ## 文件布局
//!
//! ```text
//! ~/.smallhead/sessions/
//! ├── index.json              # Vec<SessionMeta>（按 updated_at 倒序）
//! └── {uuid-v7}.json          # Session 全量快照（pretty-printed）
//! ```
//!
//! ## 写入策略
//!
//! - 原子写：先写 `<id>.json.tmp` → `rename` 到 `<id>.json`，避免崩溃留半截。
//! - 串行化：所有 IO 走 `tokio::sync::Mutex<()>` 串行化,避免并发 save 抢 index.json。
//! - index 总是 derived from session（每次 save 后重写 index.json）。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::types::{
    meta_from_session, Session, SessionError, SessionId, SessionMeta, SessionStore,
};

/// JSON 文件存储。
///
/// 同一个 store 内部的 save / load / list 全部走内部 Mutex 串行化。
/// 跨进程的并发不在保护范围内（典型是「单进程 turn → internal API」）。
pub struct JsonFileStore {
    /// session 文件所在目录。
    pub dir: PathBuf,
    /// 进程内串行化锁。
    lock: Arc<Mutex<()>>,
}

impl JsonFileStore {
    /// 创建一个 store（dir 必须存在 or 可创建）。
    pub fn new<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            dir: dir.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// 默认存储位置：`$HOME/.smallhead/sessions/`。
    ///
    /// HOME 未设置时返回 `SessionError::Other`。
    pub fn default_location() -> Result<Self, SessionError> {
        let home = std::env::var("HOME")
            .map_err(|_| SessionError::Other("HOME env var not set".into()))?;
        Ok(Self::new(
            PathBuf::from(home).join(".smallhead").join("sessions"),
        ))
    }

    /// 单个 session 文件路径。
    fn session_path(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// 索引文件路径。
    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    /// 确保目录存在（幂等）。
    async fn ensure_dir(&self) -> Result<(), SessionError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        Ok(())
    }

    /// 读取 index.json（不存在返回空 vec）。
    async fn read_index(&self) -> Result<Vec<SessionMeta>, SessionError> {
        let p = self.index_path();
        match tokio::fs::read_to_string(&p).await {
            Ok(content) => {
                if content.trim().is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(serde_json::from_str(&content)?)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// 重写 index.json（按 updated_at 倒序）。
    async fn write_index(&self, metas: &[SessionMeta]) -> Result<(), SessionError> {
        let mut sorted = metas.to_vec();
        sorted.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        let content = serde_json::to_string_pretty(&sorted)?;
        tokio::fs::write(self.index_path(), content).await?;
        Ok(())
    }
}

#[async_trait]
impl SessionStore for JsonFileStore {
    async fn save(&self, session: &Session) -> Result<(), SessionError> {
        let _guard = self.lock.lock().await;
        self.ensure_dir().await?;

        // 1. 原子写 session 文件
        let path = self.session_path(&session.id);
        let tmp_path = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(session)?;
        tokio::fs::write(&tmp_path, content).await?;
        tokio::fs::rename(&tmp_path, &path).await?;

        // 2. 更新 index.json
        let mut metas = self.read_index().await.unwrap_or_default();
        let new_meta = meta_from_session(session);
        if let Some(existing) = metas.iter_mut().find(|m| m.id == session.id) {
            *existing = new_meta;
        } else {
            metas.push(new_meta);
        }
        self.write_index(&metas).await?;

        Ok(())
    }

    async fn load(&self, id: &SessionId) -> Result<Session, SessionError> {
        let _guard = self.lock.lock().await;
        let path = self.session_path(id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(serde_json::from_str(&content)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(SessionError::NotFound(id.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, SessionError> {
        let _guard = self.lock.lock().await;
        let mut metas = self.read_index().await?;
        // 保险：index.json 可能因外部删除而含过期项。
        // 过滤掉实际文件不存在的 entry；如果有过滤，rewrite 一次 index 清理 stale。
        let before = metas.len();
        metas.retain(|m| self.session_path(&m.id).exists());
        if metas.len() != before {
            self.write_index(&metas).await?;
        }
        metas.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        Ok(metas)
    }

    async fn delete(&self, id: &SessionId) -> Result<(), SessionError> {
        let _guard = self.lock.lock().await;
        let path = self.session_path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // 从 index 移除
        let mut metas = self.read_index().await?;
        metas.retain(|m| &m.id != id);
        self.write_index(&metas).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallhead_core::{AgentMessage, ContentBlock};

    /// 构造一个有两个 user message 的 session。
    fn sample_session(id: &str) -> Session {
        let mut s = Session::new(id.into(), "you are helpful");
        s.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "first user".into(),
            }],
            timestamp: 1,
        });
        s.messages.push(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "first reply".into(),
            }],
            tool_calls: vec![],
            model: "test".into(),
            usage: None,
        });
        s.messages.push(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "second user".into(),
            }],
            timestamp: 2,
        });
        s
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());

        let mut s = sample_session("session-a");
        s.updated_at = 12345;
        store.save(&s).await.unwrap();

        let loaded = store.load(&"session-a".into()).await.unwrap();
        assert_eq!(loaded.id, "session-a");
        assert_eq!(loaded.system_prompt, "you are helpful");
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.updated_at, 12345);
        assert_eq!(loaded.created_at, s.created_at);
    }

    #[tokio::test]
    async fn load_missing_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());
        let err = store.load(&"ghost".into()).await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_returns_meta_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());

        let mut a = sample_session("s-a");
        a.updated_at = 100;
        let mut b = sample_session("s-b");
        b.updated_at = 200;
        let mut c = sample_session("s-c");
        c.updated_at = 150;

        store.save(&a).await.unwrap();
        store.save(&b).await.unwrap();
        store.save(&c).await.unwrap();

        let metas = store.list().await.unwrap();
        assert_eq!(metas.len(), 3);
        // 按 updated_at 倒序
        assert_eq!(metas[0].id, "s-b");
        assert_eq!(metas[1].id, "s-c");
        assert_eq!(metas[2].id, "s-a");
        // first_user_message
        assert_eq!(metas[0].first_user_message.as_deref(), Some("first user"));
        assert_eq!(metas[0].message_count, 3);
    }

    #[tokio::test]
    async fn list_returns_empty_when_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());
        let metas = store.list().await.unwrap();
        assert!(metas.is_empty());
    }

    #[tokio::test]
    async fn save_overwrites_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());

        let mut s = sample_session("dup");
        store.save(&s).await.unwrap();

        s.messages.push(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "extra".into(),
            }],
            tool_calls: vec![],
            model: "test".into(),
            usage: None,
        });
        s.updated_at = 999;
        store.save(&s).await.unwrap();

        let loaded = store.load(&"dup".into()).await.unwrap();
        assert_eq!(loaded.messages.len(), 4);
        assert_eq!(loaded.updated_at, 999);

        let metas = store.list().await.unwrap();
        assert_eq!(metas.len(), 1, "不应有重复 entry");
        assert_eq!(metas[0].id, "dup");
        assert_eq!(metas[0].message_count, 4);
    }

    #[tokio::test]
    async fn delete_removes_session_and_index_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());

        store.save(&sample_session("to-delete")).await.unwrap();
        store.save(&sample_session("to-keep")).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 2);

        store.delete(&"to-delete".into()).await.unwrap();

        let metas = store.list().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, "to-keep");
        // 确认文件确实被删
        assert!(!store.session_path(&"to-delete".into()).exists());
    }

    #[tokio::test]
    async fn delete_missing_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path());
        store.delete(&"ghost".into()).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_dir_creates_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        let store = JsonFileStore::new(&nested);
        // dir doesn't exist yet
        assert!(!nested.exists());
        store.save(&sample_session("x")).await.unwrap();
        assert!(nested.exists());
    }
}
