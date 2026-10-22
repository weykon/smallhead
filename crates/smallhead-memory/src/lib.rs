//! `smallhead-memory` —— agent 自记忆 + 个人 RAG 基础设施(v0.5 scaffold)。
//!
//! ## 设计
//! - LanceDB 嵌入式存储,Rust-native,无需 server。
//! - 三类 table: agent_memory / personal_rag / ecs_checkpoints(分别装对话、文档、ECS 状态)。
//! - 配 6 项 + 2 vector:store / path / embedding / ingest / search / table。
//!
//! ## 当前范围(v0.5 scaffold)
//! - ✅ MemoryConfig 全部定义 + YAML 解析
//! - ✅ MemoryTable 枚举
//! - ✅ LanceStore 路径管理 + table dir 创建
//! - ❌ ingest / search / 真实 lance 连接(留给 v0.5 后续 PR)

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod chunker;
mod config;
mod embedder;
mod embedding;
mod error;
mod ingest;
mod store;
mod table;

#[cfg(test)]
mod tests;

pub use config::{
    EmbeddingBackend, EmbeddingConfig, EmbeddingProvider, IngestConfig, IngestSource, MemoryConfig,
    SearchConfig,
};
pub use embedder::Embedder;
pub use embedding::{LocalEmbedder, BGE_SMALL_ZH_DIM};
pub use error::MemoryError;
pub use ingest::{Hit, IngestPipeline};
pub use store::{memory_table_schema, LanceStore, MemoryHandle, EMBEDDING_DIM};
pub use table::MemoryTable;
