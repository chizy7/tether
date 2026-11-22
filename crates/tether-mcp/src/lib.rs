//! Tool layer: the [`McpClient`] trait, the built-in `sandbox` server, the
//! [`registry::Registry`] that merges servers into one tool list, and (with
//! the `rmcp` feature, on by default) the [`client::ExternalServer`] that
//! speaks the MCP wire protocol to a server over stdio or streamable HTTP.
//!
//! The agent sees one flat tool list. At run start every `[[mcp_server]]`
//! named in the policy is connected, `tools/list` is called, and the merged
//! list is recorded in `run.created.tools` with the owning server on each
//! entry. Name collisions are prefixed by server id. Every call then takes
//! the same path: `tool.call` event, policy decision, dispatch to the owning
//! server, `tool.result`.
//!
//! Tools from external servers are `external = true`: replay returns the
//! recorded result and never reconnects.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tether_types::{ToolCall, ToolResult};

pub mod config;
pub mod registry;
pub mod sandbox_server;

#[cfg(feature = "rmcp")]
pub mod client;

pub use config::{ServerConfig, Transport};
pub use registry::Registry;

/// MCP protocol versions this build will talk to. The negotiated version is
/// recorded per server in `run.created.mcp_servers`.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error(
        "server {server}: negotiated protocol version {version}, which this build does not support"
    )]
    ProtocolMismatch { server: String, version: String },
    #[error("server {server}: could not connect: {reason}")]
    Connect { server: String, reason: String },
    #[error("server {server}: {reason}")]
    Server { server: String, reason: String },
    #[error("server {server}: call to {tool} took longer than {secs}s")]
    Timeout {
        server: String,
        tool: String,
        secs: u64,
    },
    #[error("no tool named {0:?}")]
    UnknownTool(String),
    #[error("server id {0:?} is used twice")]
    DuplicateServer(String),
    #[error("this build has no MCP client; rebuild tether-mcp with the `rmcp` feature to use [[mcp_server]]")]
    NoClient,
    #[error("{0}")]
    Other(String),
}

/// One tool as the agent sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// The name the agent calls. Prefixed `<server>.<tool>` only on collision.
    pub name: String,
    pub server: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// True for every tool of an external server: replay never re-invokes it.
    pub external: bool,
}

/// What `run.created.mcp_servers` records about one connected server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub id: String,
    /// `builtin`, `stdio` or `http`.
    pub transport: String,
    /// Negotiated MCP protocol version; `null` for the built-in server.
    pub protocol_version: Option<String>,
    /// The server's own name and version from `initialize`, if it gave one.
    pub server_name: Option<String>,
    pub server_version: Option<String>,
}

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError>;
    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError>;
    /// The servers behind this client, for `run.created`. A single server
    /// returns itself; the registry returns all of them.
    fn servers(&self) -> Vec<ServerInfo> {
        Vec::new()
    }
    /// Close connections and stop child processes. Idempotent.
    async fn shutdown(&self) {}
}
