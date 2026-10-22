//! Explicit, session-shared MCP clients. Construction never starts a server.
#![forbid(unsafe_code)]

mod config;
mod manager;
mod tool;

pub use config::{McpConfig, ServerConfig};
pub use manager::{
    ConnectionSnapshot, ConnectionStatus, McpError, McpManager, McpToolInfo, TransportKind,
};
pub use tool::McpTool;
