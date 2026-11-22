//! One external server as the policy file names it.
//!
//! ```toml
//! [[mcp_server]]
//! id = "fs"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "./workspace"]
//! env = ["HOME", "PATH"]          # names passed through from Tether's own env
//! timeout_secs = 60               # per call; default 60
//!
//! [[mcp_server]]
//! id = "search"
//! transport = "http"
//! url = "http://127.0.0.1:8931/mcp"
//! headers = { Authorization = "Bearer ..." }
//! ```
//!
//! `tether-policy` parses this table into its own struct and the CLI and
//! daemon map it to [`ServerConfig`]; the two crates do not import each other.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub transport: Transport,
    /// Per-call timeout. A server that takes longer fails that one call.
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Names of environment variables passed through to the child. Nothing
        /// else from Tether's environment reaches it.
        #[serde(default)]
        env: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl Transport {
    pub fn kind(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }
}

impl ServerConfig {
    pub fn stdio(id: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            id: id.into(),
            transport: Transport::Stdio {
                command: command.into(),
                args,
                env: Vec::new(),
            },
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    pub fn http(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: Transport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Ids are used as a name prefix and as a rule target, so they are kept
    /// to `[A-Za-z0-9_-]` and may not be the built-in server's name.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("`id` is empty".into());
        }
        if self.id == crate::sandbox_server::SERVER {
            return Err(format!("`id` {:?} is the built-in server", self.id));
        }
        if !self
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(format!(
                "`id` {:?} may only contain letters, digits, `_` and `-`",
                self.id
            ));
        }
        match &self.transport {
            Transport::Stdio { command, .. } if command.is_empty() => {
                Err("`command` is empty".into())
            }
            Transport::Http { url, .. }
                if !(url.starts_with("http://") || url.starts_with("https://")) =>
            {
                Err(format!("`url` {url:?} must start with http:// or https://"))
            }
            _ => Ok(()),
        }
    }
}
