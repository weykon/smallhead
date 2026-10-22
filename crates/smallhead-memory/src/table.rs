//! The three logical tables backed by LanceDB.

use serde::{Deserialize, Serialize};

/// Logical name for a table of stored memory.
///
/// One [`LanceStore`] owns one LanceDB root. Each variant maps to a subdirectory
/// under it (e.g. `agent_memory/`, `personal_rag/`, `ecs_checkpoints/`) and will
/// eventually correspond to a separate LanceDB table inside that directory.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MemoryTable {
    /// Per-session conversation chunks (the primary "agent self-memory" feed).
    AgentMemory,
    /// User-authored documents for personal RAG.
    PersonalRag,
    /// big-head ECS component snapshots.
    EcsCheckpoints,
}

impl MemoryTable {
    /// Canonical directory name under the [`LanceStore`] root for this table.
    pub fn dir_name(&self) -> &'static str {
        match self {
            Self::AgentMemory => "agent_memory",
            Self::PersonalRag => "personal_rag",
            Self::EcsCheckpoints => "ecs_checkpoints",
        }
    }

    /// Reverse of [`MemoryTable::dir_name`]: parse a directory name back into a
    /// table, or return `None` if it isn't a known table name.
    pub fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "agent_memory" => Some(Self::AgentMemory),
            "personal_rag" => Some(Self::PersonalRag),
            "ecs_checkpoints" => Some(Self::EcsCheckpoints),
            _ => None,
        }
    }

    /// All known tables, in stable declaration order.
    pub fn all() -> &'static [MemoryTable] {
        &[Self::AgentMemory, Self::PersonalRag, Self::EcsCheckpoints]
    }
}
