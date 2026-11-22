//! An external MCP server behind [`McpClient`], spoken to with `rmcp`.
//!
//! `connect` runs `initialize`, checks the negotiated protocol version
//! against [`SUPPORTED_PROTOCOL_VERSIONS`] and keeps the connection for the
//! run. Stdio servers are child processes that live as long as the run and
//! get `PATH` and the environment variables the policy names, nothing else.
//! `timeout_secs` bounds `initialize` and every call: a server that does not
//! answer `initialize` fails the connect, and a call past the limit fails as
//! a normal tool error.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tether_types::tool::Payload;
use tether_types::{ToolCall, ToolResult};
use tokio::sync::Mutex;

use crate::{
    McpClient, McpError, ServerConfig, ServerInfo, ToolSpec, Transport, SUPPORTED_PROTOCOL_VERSIONS,
};

pub struct ExternalServer {
    config: ServerConfig,
    info: ServerInfo,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
}

impl ExternalServer {
    pub async fn connect(config: ServerConfig) -> Result<Self, McpError> {
        config.validate().map_err(|reason| McpError::Connect {
            server: config.id.clone(),
            reason,
        })?;
        let connect_err = |e: &dyn std::fmt::Display| McpError::Connect {
            server: config.id.clone(),
            reason: e.to_string(),
        };

        // A server that never answers `initialize` must not hang the run.
        // Dropping the pending handshake drops the transport, which stops a
        // stdio child.
        let limit = Duration::from_secs(config.timeout_secs);
        let no_answer = || McpError::Connect {
            server: config.id.clone(),
            reason: format!("no answer to `initialize` within {} s", config.timeout_secs),
        };

        let service = match &config.transport {
            Transport::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                cmd.env_clear();
                // `PATH` always: without it the server cannot find its own
                // interpreter, and it is not a secret. Everything else only
                // if the policy names it.
                for name in std::iter::once("PATH").chain(env.iter().map(String::as_str)) {
                    if let Ok(value) = std::env::var(name) {
                        cmd.env(name, value);
                    }
                }
                let transport = TokioChildProcess::new(cmd).map_err(|e| connect_err(&e))?;
                tokio::time::timeout(limit, ().serve(transport))
                    .await
                    .map_err(|_| no_answer())?
                    .map_err(|e| connect_err(&e))?
            }
            Transport::Http { url, headers } => {
                // No redirects: the headers may hold a token, and it goes to
                // the URL the policy names and nowhere else.
                let mut builder =
                    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
                if !headers.is_empty() {
                    let mut map = reqwest::header::HeaderMap::new();
                    for (k, v) in headers {
                        // `${NAME}` is filled in from the environment here, so
                        // a token never has to be written in the policy file,
                        // which every trace records in full.
                        let v = crate::config::expand_env(v, |name| std::env::var(name).ok())
                            .map_err(|e| connect_err(&format!("header {k:?}: {e}")))?;
                        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                            .map_err(|e| connect_err(&format!("header {k:?}: {e}")))?;
                        let mut value = reqwest::header::HeaderValue::from_str(&v)
                            .map_err(|e| connect_err(&format!("header {k:?}: {e}")))?;
                        value.set_sensitive(true);
                        map.insert(name, value);
                    }
                    builder = builder.default_headers(map);
                }
                let client = builder.build().map_err(|e| connect_err(&e))?;
                let transport = StreamableHttpClientTransport::with_client(
                    client,
                    StreamableHttpClientTransportConfig::with_uri(url.as_str()),
                );
                tokio::time::timeout(limit, ().serve(transport))
                    .await
                    .map_err(|_| no_answer())?
                    .map_err(|e| connect_err(&e))?
            }
        };

        let peer = service.peer_info();
        let version = peer
            .as_ref()
            .map(|p| p.protocol_version.to_string())
            .unwrap_or_default();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str()) {
            service.cancel().await.ok();
            return Err(McpError::ProtocolMismatch {
                server: config.id.clone(),
                version,
            });
        }
        let info = ServerInfo {
            id: config.id.clone(),
            transport: config.transport.kind().into(),
            protocol_version: Some(version),
            server_name: peer
                .as_ref()
                .and_then(|p| p.server_info.as_ref())
                .map(|s| s.name.clone()),
            server_version: peer
                .as_ref()
                .and_then(|p| p.server_info.as_ref())
                .map(|s| s.version.clone()),
        };
        Ok(Self {
            config,
            info,
            service: Mutex::new(Some(service)),
        })
    }

    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    fn server_err(&self, reason: impl std::fmt::Display) -> McpError {
        McpError::Server {
            server: self.config.id.clone(),
            reason: reason.to_string(),
        }
    }
}

/// MCP content blocks as one JSON value: a single text block becomes its
/// string, anything else the array of blocks as the server sent them.
fn content_to_value(content: &[ContentBlock]) -> Value {
    if let [ContentBlock::Text(t)] = content {
        return Value::String(t.text.clone());
    }
    serde_json::to_value(content).unwrap_or(Value::Null)
}

#[async_trait]
impl McpClient for ExternalServer {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        let guard = self.service.lock().await;
        let service = guard
            .as_ref()
            .ok_or_else(|| self.server_err("connection closed"))?;
        let tools = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            service.list_all_tools(),
        )
        .await
        .map_err(|_| McpError::Timeout {
            server: self.config.id.clone(),
            tool: "tools/list".into(),
            secs: self.config.timeout_secs,
        })?
        .map_err(|e| self.server_err(e))?;
        Ok(tools
            .into_iter()
            .map(|t| ToolSpec {
                name: t.name.to_string(),
                server: self.config.id.clone(),
                description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
                external: true,
            })
            .collect())
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        let guard = self.service.lock().await;
        let service = guard
            .as_ref()
            .ok_or_else(|| self.server_err("connection closed"))?;
        let mut params = CallToolRequestParams::new(call.name.clone());
        if let Value::Object(args) = &call.arguments {
            params = params.with_arguments(args.clone());
        } else if !call.arguments.is_null() {
            return Ok(ToolResult {
                id: call.id.clone(),
                content: Payload::Inline("arguments must be a JSON object".into()),
                is_error: true,
                duration_ms: 0,
            });
        }
        let outcome = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            service.call_tool(params),
        )
        .await;
        let (content, is_error) = match outcome {
            Err(_) => {
                let e = McpError::Timeout {
                    server: self.config.id.clone(),
                    tool: call.name.clone(),
                    secs: self.config.timeout_secs,
                };
                (Value::String(e.to_string()), true)
            }
            // A JSON-RPC error from the server is still a tool result the
            // agent should see, not a run failure.
            Ok(Err(e)) => (Value::String(self.server_err(e).to_string()), true),
            Ok(Ok(result)) => (
                content_to_value(&result.content),
                result.is_error.unwrap_or(false),
            ),
        };
        Ok(ToolResult {
            id: call.id.clone(),
            content: Payload::Inline(content),
            is_error,
            // Timed by the core through its clock.
            duration_ms: 0,
        })
    }

    fn servers(&self) -> Vec<ServerInfo> {
        vec![self.info.clone()]
    }

    async fn shutdown(&self) {
        if let Some(service) = self.service.lock().await.take() {
            service.cancel().await.ok();
        }
    }
}
