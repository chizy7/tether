//! The rule engine: the `[[rule]]` tables of a `tether.toml` as a [`Policy`].
//!
//! Rules are tried top to bottom and the first match decides. A call no rule
//! matches is an `ask` with no rule name.
//!
//! The engine matches on what the call says, not on what it will do: a rule
//! on `argv0 = "rm"` does not see `sh -c "rm -rf ."`. Deny by default and
//! allow narrowly if that matters; the sandbox is what enforces.

use tether_types::{Decision, ToolCall};

use crate::config::{Action, PolicyConfig, Rule};
use crate::glob::{glob_match, name_match};
use crate::{Limits, Policy, PolicyError, RunCtx, Verdict, DEFAULT_ASK_TIMEOUT_SECS};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RulePolicy {
    config: PolicyConfig,
    /// The file the config was parsed from, byte for byte.
    source: String,
}

impl RulePolicy {
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        Ok(Self {
            config: PolicyConfig::from_toml(text)?,
            source: text.to_owned(),
        })
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// The first rule that matches `call`, if any.
    pub fn matching(&self, call: &ToolCall) -> Option<&Rule> {
        let tool = qualified_name(call);
        self.config
            .rules
            .iter()
            .find(|rule| rule_matches(rule, &tool, call))
    }
}

impl Policy for RulePolicy {
    fn decide(&self, call: &ToolCall, _ctx: &RunCtx) -> Verdict {
        let Some(rule) = self.matching(call) else {
            return Decision::Ask {
                prompt: describe(call),
            }
            .into();
        };
        let decision = match rule.action {
            Action::Allow => Decision::Allow,
            Action::Deny => Decision::Deny {
                reason: rule
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("denied by rule {}", rule.name)),
            },
            Action::Ask => Decision::Ask {
                prompt: match &rule.reason {
                    Some(reason) => format!("{} ({reason})", describe(call)),
                    None => describe(call),
                },
            },
        };
        Verdict {
            decision,
            rule: Some(rule.name.clone()),
            on_timeout: rule.on_timeout,
        }
    }

    fn limits(&self) -> Limits {
        Limits::from(&self.config.budget)
    }

    fn ask_timeout_secs(&self) -> u64 {
        self.config
            .budget
            .ask_timeout_secs
            .unwrap_or(DEFAULT_ASK_TIMEOUT_SECS)
    }

    /// The file as it was read, comments included. An empty policy, where
    /// every call asks, snapshots as the empty string.
    fn snapshot(&self) -> Option<String> {
        Some(self.source.clone())
    }
}

/// `<server>.<tool>`, the name a rule matches on. Where two servers offered
/// the same tool the agent calls the later one as `<server>__<tool>` (the
/// registry's prefix, in `tether-mcp`); that prefix is taken off first, so a
/// rule sees `<server>.<tool>` whichever name the agent used. A name that
/// already starts with `<server>.` is left as it is.
pub fn qualified_name(call: &ToolCall) -> String {
    if call.server.is_empty() {
        return call.name.clone();
    }
    let after_server = call.name.strip_prefix(call.server.as_str());
    if after_server.is_some_and(|rest| rest.starts_with('.')) {
        return call.name.clone();
    }
    let tool = after_server
        .and_then(|rest| rest.strip_prefix("__"))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(&call.name);
    format!("{}.{tool}", call.server)
}

/// Whether `rule` covers `call`.
///
/// Tool and server names are matched as names: `*` covers a `/` too, since
/// a server may call a tool anything. `argv0` and `path` are matched as
/// written for an `allow`, and more widely for a `deny` or an `ask`, so
/// that the two ways of being wrong both fail safe:
///
/// - Letter case is ignored. On a filesystem that ignores it too, `SECRETS/`
///   is `secrets/` and `RM` is `rm`.
/// - A path that climbs out of where it started (`../x`) matches any path
///   pattern. Nobody can say what it names, so no deny may be skipped by it.
///
/// An `allow` gets neither, so it never lets through more than it says.
fn rule_matches(rule: &Rule, tool: &str, call: &ToolCall) -> bool {
    let m = &rule.matcher;
    let wide = rule.action != Action::Allow;
    let any = |globs: &[String], text: &str| {
        if wide {
            let text = text.to_lowercase();
            globs
                .iter()
                .any(|glob| glob_match(&glob.to_lowercase(), &text))
        } else {
            globs.iter().any(|glob| glob_match(glob, text))
        }
    };
    let names = |globs: &[String], text: &str| globs.iter().any(|glob| name_match(glob, text));

    (m.tool.is_empty() || names(&m.tool, tool))
        && (m.server.is_empty() || names(&m.server, &call.server))
        && (m.argv0.is_empty() || argv0(call).is_some_and(|name| any(&m.argv0, name)))
        && (m.path.is_empty()
            || match path(call) {
                PathArg::Absent => false,
                PathArg::Climbs => wide,
                PathArg::At(path) => any(&m.path, &path),
            })
}

/// The file name of the command, so `rm`, `/bin/rm` and `./rm` are all `rm`.
fn argv0(call: &ToolCall) -> Option<&str> {
    let program = call.arguments.get("argv")?.get(0)?.as_str()?;
    program.rsplit('/').next()
}

enum PathArg {
    /// The call has no `path` argument.
    Absent,
    /// `..` took it above where it started.
    Climbs,
    /// With `.` and `..` resolved.
    At(String),
}

/// `arguments.path` with `.` and `..` resolved, so `src/../secret` cannot
/// pass as something under `src/`.
fn path(call: &ToolCall) -> PathArg {
    let Some(raw) = call.arguments.get("path").and_then(|path| path.as_str()) else {
        return PathArg::Absent;
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return PathArg::Climbs;
                }
            }
            name => parts.push(name),
        }
    }
    let root = if raw.starts_with('/') { "/" } else { "" };
    PathArg::At(format!("{root}{}", parts.join("/")))
}

/// One line saying what the call is, for a human deciding an `ask`.
///
/// The arguments are shown as JSON with the ones a person decides on first:
/// `path`, `argv`, `cwd`, then the rest. That way a long `content` cannot
/// push the path of a write off the end. If the line is cut, it says by how
/// much, so nobody approves what they could not see without knowing it.
fn describe(call: &ToolCall) -> String {
    const MAX: usize = 400;
    const FIRST: [&str; 3] = ["path", "argv", "cwd"];
    let arguments = match call.arguments.as_object() {
        Some(fields) => {
            let ordered = FIRST
                .iter()
                .filter_map(|key| fields.get_key_value(*key))
                .chain(
                    fields
                        .iter()
                        .filter(|(key, _)| !FIRST.contains(&key.as_str())),
                )
                .map(|(key, value)| format!("{}:{value}", serde_json::Value::from(key.as_str())))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{ordered}}}")
        }
        None => call.arguments.to_string(),
    };
    let length = arguments.chars().count();
    let shown = if length > MAX {
        let head: String = arguments.chars().take(MAX).collect();
        format!("{head}... ({} more characters not shown)", length - MAX)
    } else {
        arguments
    };
    format!("Allow {} {shown}?", qualified_name(call))
}
