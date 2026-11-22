//! One external server as the policy file names it.
//!
//! ```toml
//! [[mcp_server]]
//! id = "fs"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "./workspace"]
//! env = ["HOME"]                  # names passed through from Tether's own env,
//!                                 # beside `PATH`, which always is
//! timeout_secs = 60               # for `initialize` and per call; default 60
//!
//! [[mcp_server]]
//! id = "search"
//! transport = "http"
//! url = "http://127.0.0.1:8931/mcp"
//! headers = { Authorization = "Bearer ${SEARCH_TOKEN}" }   # from the environment
//! ```
//!
//! The policy file is recorded whole in every trace, so a secret must never
//! be written in it. A header value takes `${NAME}` instead, filled in from
//! Tether's environment when the server is connected.
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
        /// Names of environment variables passed through to the child, beside
        /// `PATH`. Nothing else from Tether's environment reaches it.
        #[serde(default)]
        env: Vec<String>,
    },
    Http {
        url: String,
        /// Sent with every request. `${NAME}` in a value is replaced from the
        /// environment at connect time; see [`expand_env`].
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// Replace each `${NAME}` in `value` with what `lookup` gives for `NAME`.
/// A name that is not set is an error, as is a `${` with no closing brace:
/// sending a request with a hole where the token should be helps nobody.
pub fn expand_env(value: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "`${` without a closing `}`".to_owned())?;
        let name = &after[..end];
        if name.is_empty() {
            return Err("`${}` names no variable".into());
        }
        let found =
            lookup(name).ok_or_else(|| format!("environment variable {name} is not set"))?;
        out.push_str(&found);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::expand_env;

    fn lookup(name: &str) -> Option<String> {
        (name == "TOKEN").then(|| "abc".to_owned())
    }

    #[test]
    fn names_in_braces_are_filled_in() {
        assert_eq!(
            expand_env("Bearer ${TOKEN}", lookup).as_deref(),
            Ok("Bearer abc")
        );
        assert_eq!(
            expand_env("${TOKEN}-${TOKEN}", lookup).as_deref(),
            Ok("abc-abc")
        );
    }

    #[test]
    fn a_value_with_nothing_to_fill_in_is_unchanged() {
        assert_eq!(
            expand_env("plain $5 {x}", lookup).as_deref(),
            Ok("plain $5 {x}")
        );
    }

    #[test]
    fn an_unset_name_and_a_missing_brace_are_errors() {
        assert!(expand_env("${MISSING}", lookup).is_err_and(|e| e.contains("MISSING")));
        assert!(expand_env("${TOKEN", lookup).is_err());
        assert!(expand_env("${}", lookup).is_err());
    }
}
