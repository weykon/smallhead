//! Configuration types for `smallhead-memory`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

/// Top-level configuration object.
///
/// Default values live in [`MemoryConfig::default_for_home`]; the same struct
/// round-trips through [`MemoryConfig::from_yaml_str`] / [`MemoryConfig::to_yaml_string`]
/// so users can keep a checked-in config and override individual fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Embedded vector store backend. Only [`EmbeddingProvider::LanceDB`] is
    /// wired in v0.5; [`EmbeddingProvider::Minimax`] is reserved for later.
    pub store: EmbeddingProvider,
    /// Root directory on disk where LanceDB files live.
    pub path: PathBuf,
    /// Embedding model selection.
    pub embedding: EmbeddingConfig,
    /// Ingestion rules.
    pub ingest: IngestConfig,
    /// Search-time behavior.
    pub search: SearchConfig,
}

/// Which embedded vector store backend the crate talks to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum EmbeddingProvider {
    /// LanceDB — Rust-native embedded vector store, no server.
    LanceDB,
    /// Reserved for a future provider.
    Minimax,
}

/// Embedding-model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Which library actually produces the vectors.
    pub provider: EmbeddingBackend,
    /// Model identifier (e.g. `"bge-small-zh-v1.5"` for fastembed, or the
    /// MiniMax embedding model id).
    pub model: String,
    /// Vector dimensionality produced by `model`. Used to allocate LanceDB
    /// columns and validate the embedding backend's output.
    pub dim: u32,
}

/// Embedding backend library.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum EmbeddingBackend {
    /// `fastembed-rs` — runs ONNX models locally.
    Fastembed,
    /// MiniMax-hosted embedding API.
    Minimax,
}

/// Rules for turning raw input (e.g. session messages) into stored chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestConfig {
    /// Tokens per chunk.
    pub chunk_size: usize,
    /// Token overlap between consecutive chunks.
    pub chunk_overlap: usize,
    /// Which input feeds to ingest.
    pub sources: Vec<IngestSource>,
    /// Retention window in days; `0` means "never forget".
    pub retain_days: u32,
}

/// A single source of memory to ingest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestSource {
    /// Chunks of session messages.
    SessionMessages,
}

/// Search-time knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Hybrid-search blend ratio: `0.0` = pure BM25, `1.0` = pure vector.
    pub hybrid_ratio: f32,
    /// Top-K hits to return from a search.
    pub top_k: usize,
    /// Half-life in days for recency decay; `0` disables decay entirely.
    pub recency_decay_days: u32,
}

impl MemoryConfig {
    /// Default config rooted at `~/.smallhead/memory/lance`.
    ///
    /// Returns [`MemoryError::Env`] if `$HOME` is not set.
    pub fn default_for_home() -> Result<Self, MemoryError> {
        let home = std::env::var("HOME")?;
        Ok(Self::for_root(
            PathBuf::from(home).join(".smallhead/memory/lance"),
        ))
    }

    /// Build a default config rooted at an arbitrary directory.
    pub fn for_root(path: PathBuf) -> Self {
        Self {
            store: EmbeddingProvider::LanceDB,
            path,
            embedding: EmbeddingConfig {
                provider: EmbeddingBackend::Fastembed,
                model: "bge-small-zh-v1.5".into(),
                dim: 512,
            },
            ingest: IngestConfig {
                chunk_size: 512,
                chunk_overlap: 64,
                sources: vec![IngestSource::SessionMessages],
                retain_days: 0,
            },
            search: SearchConfig {
                hybrid_ratio: 0.7,
                top_k: 5,
                recency_decay_days: 0,
            },
        }
    }

    /// Parse a YAML string into a config. Missing fields fall back to
    /// [`MemoryConfig::for_root`] defaults at `~/.smallhead/memory/lance` —
    /// callers can supply a complete YAML or just override individual fields.
    ///
    /// Implementation: serialize the defaults to a `serde_yaml::Value`,
    /// deep-merge the user input on top, then deserialize the merged result.
    /// This way every field is optional from the caller's perspective.
    pub fn from_yaml_str(s: &str) -> Result<Self, MemoryError> {
        let mut cfg = Self::for_root(
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".smallhead/memory/lance"),
        );
        let defaults_value = serde_yaml::to_value(&cfg)
            .map_err(|e| MemoryError::InvalidConfig(format!("default serialize: {e}")))?;
        let user_value: serde_yaml::Value = serde_yaml::from_str(s)?;
        let merged = deep_merge(defaults_value, user_value);
        cfg = serde_yaml::from_value(merged)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Serialize the config to YAML for round-trip / backup.
    pub fn to_yaml_string(&self) -> Result<String, MemoryError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Check that numeric ranges and required fields are sane. Called by
    /// [`MemoryConfig::from_yaml_str`]; callers using [`MemoryConfig::for_root`]
    /// or [`MemoryConfig::default_for_home`] can rely on the defaults being
    /// valid, but should still call this if they mutate fields directly.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if !(0.0..=1.0).contains(&self.search.hybrid_ratio) {
            return Err(MemoryError::InvalidConfig(format!(
                "search.hybrid_ratio must be in [0.0, 1.0], got {}",
                self.search.hybrid_ratio
            )));
        }
        if self.search.top_k == 0 {
            return Err(MemoryError::InvalidConfig(
                "search.top_k must be > 0".into(),
            ));
        }
        if self.embedding.dim == 0 {
            return Err(MemoryError::InvalidConfig(
                "embedding.dim must be > 0".into(),
            ));
        }
        if self.ingest.chunk_size == 0 {
            return Err(MemoryError::InvalidConfig(
                "ingest.chunk_size must be > 0".into(),
            ));
        }
        if self.ingest.chunk_overlap >= self.ingest.chunk_size {
            return Err(MemoryError::InvalidConfig(format!(
                "ingest.chunk_overlap ({}) must be < chunk_size ({})",
                self.ingest.chunk_overlap, self.ingest.chunk_size
            )));
        }
        Ok(())
    }

    /// Root directory of this config (alias for `self.path`).
    pub fn root(&self) -> &Path {
        &self.path
    }
}

/// Recursively merge `user` onto `defaults`. Maps are merged key-by-key
/// (user wins on leaf scalars); sequences and scalars in `user` replace
/// `defaults`.
fn deep_merge(defaults: serde_yaml::Value, user: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (defaults, user) {
        (Value::Mapping(mut d), Value::Mapping(u)) => {
            for (k, v) in u {
                let entry = d.remove(&k).unwrap_or(Value::Null);
                d.insert(k, deep_merge(entry, v));
            }
            Value::Mapping(d)
        }
        (_, u) => u,
    }
}
