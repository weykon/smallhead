use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Configuration loaded explicitly by the host. No server connects on load.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: BTreeMap<String, ServerConfig>,
}

/// One stdio command OR streamable HTTP endpoint. Debug intentionally omitted.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    /// Header name -> environment variable name (not a literal header value).
    pub http_headers: BTreeMap<String, String>,
    pub description: Option<String>,
    pub disabled: bool,
    pub connect_timeout_ms: u64,
    pub list_timeout_ms: u64,
    pub call_timeout_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            url: None,
            http_headers: BTreeMap::new(),
            description: None,
            disabled: false,
            connect_timeout_ms: 30_000,
            list_timeout_ms: 30_000,
            call_timeout_ms: 60_000,
        }
    }
}
