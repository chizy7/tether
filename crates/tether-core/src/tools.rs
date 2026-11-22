//! Build the tool list a run sees: the built-in sandbox server plus every
//! `[[mcp_server]]` the policy names, connected and merged into one
//! [`Registry`]. The CLI, the daemon and `tether tools` all go through here
//! so a server is configured and connected the same way everywhere.

use std::sync::Arc;

use tether_mcp::{McpClient, McpError, Registry, ServerConfig, Transport};
use tether_policy::config::{McpServerConfig, McpTransport};

/// The policy's `[[mcp_server]]` tables as `tether-mcp` wants them. The two
/// structs have the same shape; `PolicyConfig::from_toml` already validated.
pub fn server_configs(servers: &[McpServerConfig]) -> Vec<ServerConfig> {
    servers
        .iter()
        .map(|s| ServerConfig {
            id: s.id.clone(),
            transport: match s.transport {
                McpTransport::Stdio => Transport::Stdio {
                    command: s.command.clone().unwrap_or_default(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                },
                McpTransport::Http => Transport::Http {
                    url: s.url.clone().unwrap_or_default(),
                    headers: s.headers.clone(),
                },
            },
            timeout_secs: s.timeout_secs,
        })
        .collect()
}

/// Connect every external server and merge it with the built-in one.
///
/// The built-in server goes first so its tool names stay bare on a
/// collision. A server that cannot be connected fails the whole call; the
/// ones already connected are shut down again so no child process outlives
/// the error.
pub async fn connect(
    builtin: Option<(String, Arc<dyn McpClient>)>,
    servers: &[ServerConfig],
) -> Result<Registry, McpError> {
    let mut connected: Vec<(String, Arc<dyn McpClient>)> = Vec::new();
    if let Some(builtin) = builtin {
        connected.push(builtin);
    }
    for config in servers {
        match connect_one(config.clone()).await {
            Ok(client) => connected.push((config.id.clone(), client)),
            Err(e) => {
                for (_, client) in &connected {
                    client.shutdown().await;
                }
                return Err(e);
            }
        }
    }
    // Listing tools can fail too (a server that errors on `tools/list`, a
    // name that collides twice), and the same holds: nothing is left running.
    let clients: Vec<Arc<dyn McpClient>> = connected.iter().map(|(_, c)| c.clone()).collect();
    match Registry::build(connected).await {
        Ok(registry) => Ok(registry),
        Err(e) => {
            for client in &clients {
                client.shutdown().await;
            }
            Err(e)
        }
    }
}

/// The same tool list as [`connect`] gives, connected the first time it is
/// used. This is what a replay carries for the case that it leaves the
/// recording: a replay that never does starts no server and opens no
/// connection, and one that does gets the real servers and not just the
/// sandbox.
///
/// Which servers those are is the caller's decision, and it matters: a
/// stdio server is a command, and a trace can come from anyone. Servers the
/// caller does not trust go to [`Lazy::withhold`] and are never started.
pub struct Lazy {
    builtin: Option<(String, Arc<dyn McpClient>)>,
    servers: Vec<ServerConfig>,
    registry: tokio::sync::OnceCell<Registry>,
    /// Ids of servers that are deliberately not connected, and what to tell
    /// the agent when it calls one of their tools.
    withheld: Vec<String>,
    withheld_because: String,
}

impl Lazy {
    pub fn new(builtin: Option<(String, Arc<dyn McpClient>)>, servers: Vec<ServerConfig>) -> Self {
        Self {
            builtin,
            servers,
            registry: tokio::sync::OnceCell::new(),
            withheld: Vec::new(),
            withheld_because: String::new(),
        }
    }

    /// Servers the recording had that are not to be started here. A call to
    /// one of their tools comes back as a failed call saying `because`; it
    /// is never sent anywhere.
    pub fn withhold(mut self, ids: Vec<String>, because: impl Into<String>) -> Self {
        self.withheld = ids;
        self.withheld_because = because.into();
        self
    }

    async fn registry(&self) -> Result<&Registry, McpError> {
        self.registry
            .get_or_try_init(|| connect(self.builtin.clone(), &self.servers))
            .await
    }
}

#[async_trait::async_trait]
impl McpClient for Lazy {
    async fn list_tools(&self) -> Result<Vec<tether_mcp::ToolSpec>, McpError> {
        self.registry().await?.list_tools().await
    }

    async fn call(
        &self,
        call: &tether_types::ToolCall,
    ) -> Result<tether_types::ToolResult, McpError> {
        if self.withheld.contains(&call.server) {
            return Ok(tether_types::ToolResult {
                id: call.id.clone(),
                content: tether_types::tool::Payload::Inline(
                    format!(
                        "MCP server `{}` was not started: {}",
                        call.server, self.withheld_because
                    )
                    .into(),
                ),
                is_error: true,
                duration_ms: 0,
            });
        }
        self.registry().await?.call(call).await
    }

    fn servers(&self) -> Vec<tether_mcp::ServerInfo> {
        self.registry
            .get()
            .map(McpClient::servers)
            .unwrap_or_default()
    }

    async fn shutdown(&self) {
        if let Some(registry) = self.registry.get() {
            registry.shutdown().await;
        }
    }
}

#[cfg(feature = "rmcp")]
async fn connect_one(config: ServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
    let server = tether_mcp::client::ExternalServer::connect(config).await?;
    tracing::info!(
        server = %server.info().id,
        transport = %server.info().transport,
        protocol = ?server.info().protocol_version,
        "connected MCP server"
    );
    Ok(Arc::new(server))
}

#[cfg(not(feature = "rmcp"))]
async fn connect_one(_config: ServerConfig) -> Result<Arc<dyn McpClient>, McpError> {
    Err(McpError::NoClient)
}
