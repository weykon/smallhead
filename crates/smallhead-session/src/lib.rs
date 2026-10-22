//! `smallhead-session` —— 会话持久化（v0.4 MVP）。
//!
//! ## 设计
//!
//! - **存储格式**：每个 session 一个 JSON 文件,单独一个 `index.json` 索引元数据。
//! - **存储位置**：`~/.smallhead/sessions/{uuid-v7}.json` + `index.json`。
//! - **写入策略**：turn 结束自动 save（全量快照,MVP 不做 incremental journal）。
//! - **原子写**：写 `.tmp` → `rename` 替换,避免崩溃留半截文件。
//! - **不涉及 agent 记忆**：这是会话快照（messages + system prompt）,不是 vector store / summary。
//!
//! ## 文件布局
//!
//! ```text
//! ~/.smallhead/sessions/
//! ├── index.json              # Vec<SessionMeta>（最近 save 排序）
//! └── {uuid-v7}.json          # Session 全量快照
//! ```
//!
//! ## 用法
//!
//! ```rust,no_run
//! use smallhead_session::{JsonFileStore, Session, SessionStore, new_session_id};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let store = JsonFileStore::default_location()?;
//! let session = Session::new(new_session_id(), "you are helpful");
//! store.save(&session).await?;
//! let loaded = store.load(&session.id).await?;
//! assert_eq!(loaded.id, session.id);
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod store;
mod types;

pub use store::JsonFileStore;
pub use types::{
    new_session_id, now_millis, Session, SessionError, SessionId, SessionMeta, SessionStore,
};
