//! Error types for `smallhead-memory`.

use serde_yaml;

/// Errors produced by `smallhead-memory`.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// Filesystem I/O failure (missing directory, permission, etc).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// YAML parsing / serialization failure.
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// A configuration value was structurally valid YAML but semantically
    /// invalid (e.g. `hybrid_ratio` outside `[0.0, 1.0]`).
    #[error("config invalid: {0}")]
    InvalidConfig(String),

    /// Missing or unreadable environment variable.
    #[error("env var: {0}")]
    Env(#[from] std::env::VarError),

    /// LanceDB operation error. Currently a placeholder until the next PR
    /// wires the real `lancedb` connection in.
    #[error("lancedb: {0}")]
    LanceDb(String),

    /// Local ONNX embedding failure (model load, ONNX runtime init, or inference).
    #[error("embedding: {0}")]
    Embedding(String),

    /// Marker for behavior that is intentionally not implemented in this scaffold.
    #[error("not yet implemented")]
    Todo,
}
