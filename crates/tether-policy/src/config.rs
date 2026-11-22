//! `tether.toml` parsing: `[budget]` and `[[rule]]` tables.
//!
//! ```toml
//! [[rule]]
//! name = "no-rm"                                  # optional; default is "rule-N"
//! match = { tool = "sandbox.exec", argv0 = "rm" } # every key given must match
//! action = "deny"                                 # allow | deny | ask
//! reason = "deleting files is off limits"
//! ```
//!
//! `tool`, `argv0`, `path` and `server` each take one glob or a list of them.
//! Unknown keys are errors, so a typo cannot silently turn a rule into a
//! catch-all.
//!
//! `[[mcp_server]]` tables name the external MCP servers a run connects to:
//!
//! ```toml
//! [[mcp_server]]
//! id = "fs"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "./workspace"]
//! env = ["HOME", "PATH"]     # the only variables the child process gets
//! timeout_secs = 60          # per call
//!
//! [[mcp_server]]
//! id = "search"
//! transport = "http"
//! url = "http://127.0.0.1:8931/mcp"
//! headers = { Authorization = "Bearer ..." }
//! ```

use serde::Deserialize;

use crate::PolicyError;

/// The `[budget]` table. Parsed here; enforced by the run loop through
/// [`crate::budget`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    pub max_tokens: Option<u64>,
    pub max_usd: Option<f64>,
    pub max_wall_secs: Option<u64>,
    pub max_tool_calls: Option<u64>,
    /// Seconds to wait for a human on `ask` before the run halts.
    pub ask_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
    Ask,
}

/// What an unanswered `ask` from this rule becomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTimeout {
    /// The run moves to `Halted` and can be resumed.
    #[default]
    Halt,
    Deny,
}

/// What a rule matches. Every key that is given must match; a key left out
/// matches anything, so an empty `match` is a catch-all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Match {
    /// Globs over the tool's qualified name, `<server>.<tool>`.
    pub tool: Vec<String>,
    /// Globs over the file name of `arguments.argv[0]`.
    pub argv0: Vec<String>,
    /// Globs over `arguments.path`.
    pub path: Vec<String>,
    /// Globs over the id of the server that owns the tool, so one rule can
    /// allow or deny a whole server.
    pub server: Vec<String>,
}

/// One `[[mcp_server]]` table, as written. `tether-mcp` has its own struct
/// of the same shape; the CLI and daemon map between them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub transport: McpTransport,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Recorded in `policy.decision.rule` when this rule decides a call.
    pub name: String,
    pub matcher: Match,
    pub action: Action,
    /// Shown to the agent on `deny` and to the human on `ask`.
    pub reason: Option<String>,
    pub on_timeout: OnTimeout,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyConfig {
    pub budget: BudgetConfig,
    /// In file order, which is evaluation order.
    pub rules: Vec<Rule>,
    /// External MCP servers to connect at run start, in file order.
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMatch {
    tool: Option<OneOrMany>,
    argv0: Option<OneOrMany>,
    path: Option<OneOrMany>,
    server: Option<OneOrMany>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: Option<String>,
    #[serde(default, rename = "match")]
    matcher: RawMatch,
    action: Action,
    reason: Option<String>,
    #[serde(default)]
    on_timeout: OnTimeout,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    budget: BudgetConfig,
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
    #[serde(default, rename = "mcp_server")]
    mcp_servers: Vec<McpServerConfig>,
}

fn invalid(rule: &str, reason: impl Into<String>) -> PolicyError {
    PolicyError::InvalidRule {
        rule: rule.to_owned(),
        reason: reason.into(),
    }
}

fn globs(rule: &str, key: &str, value: Option<OneOrMany>) -> Result<Vec<String>, PolicyError> {
    let globs = match value {
        None => return Ok(Vec::new()),
        Some(OneOrMany::One(glob)) => vec![glob],
        Some(OneOrMany::Many(globs)) => globs,
    };
    // An empty list or pattern is far more likely a mistake than a rule
    // meant to match nothing.
    if globs.is_empty() || globs.iter().any(String::is_empty) {
        return Err(invalid(rule, format!("`{key}` has an empty pattern")));
    }
    Ok(globs)
}

impl PolicyConfig {
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let raw: RawConfig = toml::from_str(text)?;
        if raw
            .budget
            .max_usd
            .is_some_and(|usd| !usd.is_finite() || usd < 0.0)
        {
            return Err(PolicyError::InvalidBudget {
                key: "max_usd",
                reason: "must be a number that is zero or more".into(),
            });
        }
        let mut rules: Vec<Rule> = Vec::with_capacity(raw.rules.len());
        for (index, rule) in raw.rules.into_iter().enumerate() {
            let name = rule.name.unwrap_or_else(|| format!("rule-{}", index + 1));
            if name.is_empty() {
                return Err(invalid(&name, "`name` is empty"));
            }
            if rules.iter().any(|earlier| earlier.name == name) {
                return Err(invalid(&name, "another rule has the same name"));
            }
            if rule.on_timeout == OnTimeout::Deny && rule.action != Action::Ask {
                return Err(invalid(
                    &name,
                    "`on_timeout` only applies to `action = \"ask\"`",
                ));
            }
            let matcher = Match {
                tool: globs(&name, "tool", rule.matcher.tool)?,
                argv0: globs(&name, "argv0", rule.matcher.argv0)?,
                path: globs(&name, "path", rule.matcher.path)?,
                server: globs(&name, "server", rule.matcher.server)?,
            };
            rules.push(Rule {
                name,
                matcher,
                action: rule.action,
                reason: rule.reason,
                on_timeout: rule.on_timeout,
            });
        }
        for (index, server) in raw.mcp_servers.iter().enumerate() {
            let at = |reason: String| PolicyError::InvalidMcpServer {
                server: if server.id.is_empty() {
                    format!("#{}", index + 1)
                } else {
                    server.id.clone()
                },
                reason,
            };
            if server.id.is_empty() {
                return Err(at("`id` is empty".into()));
            }
            if server.id == "sandbox" {
                return Err(at("`id` \"sandbox\" is the built-in server".into()));
            }
            if !server
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(at(
                    "`id` may only contain letters, digits, `_` and `-`".into()
                ));
            }
            if raw.mcp_servers[..index].iter().any(|s| s.id == server.id) {
                return Err(at("another server has the same id".into()));
            }
            match server.transport {
                McpTransport::Stdio => {
                    if server.command.as_deref().unwrap_or("").is_empty() {
                        return Err(at("stdio needs `command`".into()));
                    }
                    if server.url.is_some() || !server.headers.is_empty() {
                        return Err(at("`url` and `headers` are for transport = \"http\"".into()));
                    }
                }
                McpTransport::Http => {
                    let url = server.url.as_deref().unwrap_or("");
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err(at(
                            "http needs a `url` starting with http:// or https://".into()
                        ));
                    }
                    if server.command.is_some() || !server.args.is_empty() || !server.env.is_empty()
                    {
                        return Err(at(
                            "`command`, `args` and `env` are for transport = \"stdio\"".into(),
                        ));
                    }
                }
            }
            if server.timeout_secs == 0 {
                return Err(at("`timeout_secs` must be at least 1".into()));
            }
        }
        Ok(Self {
            budget: raw.budget,
            rules,
            mcp_servers: raw.mcp_servers,
        })
    }
}
