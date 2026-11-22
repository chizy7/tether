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
    match Registry::build(connected).await {
        Ok(registry) => Ok(registry),
        Err(e) => Err(e),
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
