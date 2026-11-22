//! Merge tools from every server into one list; route calls back to the owner.
//!
//! The built-in `sandbox` server goes first, so its tool names (`exec`,
//! `read_file`, ...) are the ones that stay bare when an external server
//! offers the same name; the external one becomes `<server>__<tool>`.
//!
//! The separator is two underscores and not a dot because the name goes to
//! the model as it is, and model APIs take only letters, digits, `_` and `-`
//! in a tool name. Policy rules still see `<server>.<tool>`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tether_types::{ToolCall, ToolResult};

use crate::{McpClient, McpError, ServerInfo, ToolSpec};

/// Between the server id and the tool name where a name had to be prefixed.
pub const NAME_SEPARATOR: &str = "__";

struct Entry {
    /// Index into `servers`.
    server: usize,
    /// The name the server itself knows the tool by.
    original: String,
    spec: ToolSpec,
}

pub struct Registry {
    ids: Vec<String>,
    servers: Vec<Arc<dyn McpClient>>,
    /// Keyed by the exposed name.
    tools: HashMap<String, Entry>,
    order: Vec<String>,
}

impl Registry {
    /// Connects nothing itself: every server is already connected. Lists each
    /// server's tools once and merges them, so a server that fails
    /// `tools/list` fails the run before it starts.
    pub async fn build(servers: Vec<(String, Arc<dyn McpClient>)>) -> Result<Self, McpError> {
        let mut ids = Vec::with_capacity(servers.len());
        let mut clients = Vec::with_capacity(servers.len());
        for (id, client) in servers {
            if ids.contains(&id) {
                return Err(McpError::DuplicateServer(id));
            }
            ids.push(id);
            clients.push(client);
        }

        let mut tools: HashMap<String, Entry> = HashMap::new();
        let mut order = Vec::new();
        for (index, client) in clients.iter().enumerate() {
            let server_id = &ids[index];
            for mut spec in client.list_tools().await? {
                let original = spec.name.clone();
                // Rules know a tool as `<server>.<tool>`, and the prefix a
                // collision gets is taken off to arrive at that. A server
                // that names a tool with its own id in front would have two
                // tools a rule cannot tell apart.
                if original.starts_with(&format!("{server_id}{NAME_SEPARATOR}"))
                    || original.starts_with(&format!("{server_id}."))
                {
                    return Err(McpError::Server {
                        server: server_id.clone(),
                        reason: format!(
                            "tool {original:?} starts with the server's own id; a rule could not tell it from the tool without it"
                        ),
                    });
                }
                let exposed = if tools.contains_key(&original) {
                    format!("{server_id}{NAME_SEPARATOR}{original}")
                } else {
                    original.clone()
                };
                if tools.contains_key(&exposed) {
                    return Err(McpError::Server {
                        server: server_id.clone(),
                        reason: format!("tool {original:?} collides even after prefixing"),
                    });
                }
                spec.name = exposed.clone();
                spec.server = server_id.clone();
                order.push(exposed.clone());
                tools.insert(
                    exposed,
                    Entry {
                        server: index,
                        original,
                        spec,
                    },
                );
            }
        }
        Ok(Self {
            ids,
            servers: clients,
            tools,
            order,
        })
    }

    pub fn server_ids(&self) -> &[String] {
        &self.ids
    }
}

#[async_trait]
impl McpClient for Registry {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        Ok(self
            .order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|e| e.spec.clone())
            .collect())
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        let entry = self
            .tools
            .get(&call.name)
            .ok_or_else(|| McpError::UnknownTool(call.name.clone()))?;
        // The server sees its own name for the tool, not the prefixed one.
        let routed = ToolCall {
            name: entry.original.clone(),
            server: self.ids[entry.server].clone(),
            ..call.clone()
        };
        self.servers[entry.server].call(&routed).await
    }

    fn servers(&self) -> Vec<ServerInfo> {
        self.servers.iter().flat_map(|s| s.servers()).collect()
    }

    async fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown().await;
        }
    }
}
