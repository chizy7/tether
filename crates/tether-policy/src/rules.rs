//! The rule engine: the `[[rule]]` tables of a `tether.toml` as a [`Policy`].
//!
//! Rules are tried top to bottom and the first match decides. A call no rule
//! matches is an `ask` with no rule name.
//!
//! The engine matches on what the call says, not on what it will do: a rule
//! on `argv0 = "rm"` does not see `sh -c "rm -rf ."`. Deny by default and
//! allow narrowly if that matters; the sandbox is what enforces.

use tether_types::{Decision, ToolCall};

use crate::config::{Action, Match, PolicyConfig, Rule};
use crate::glob::glob_match;
use crate::{Policy, PolicyError, RunCtx, Verdict};

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
            .find(|rule| rule_matches(&rule.matcher, &tool, call))
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
        }
    }

    /// The file as it was read, comments included. An empty policy, where
    /// every call asks, snapshots as the empty string.
    fn snapshot(&self) -> Option<String> {
        Some(self.source.clone())
    }
}

/// `<server>.<tool>`. A name the registry already prefixed with its server
/// on a collision is left as it is.
pub fn qualified_name(call: &ToolCall) -> String {
    let prefixed = call
        .name
        .strip_prefix(call.server.as_str())
        .is_some_and(|rest| rest.starts_with('.'));
    if call.server.is_empty() || prefixed {
        call.name.clone()
    } else {
        format!("{}.{}", call.server, call.name)
    }
}

fn any_glob(globs: &[String], text: &str) -> bool {
    globs.iter().any(|glob| glob_match(glob, text))
}

fn rule_matches(m: &Match, tool: &str, call: &ToolCall) -> bool {
    (m.tool.is_empty() || any_glob(&m.tool, tool))
        && (m.argv0.is_empty() || argv0(call).is_some_and(|name| any_glob(&m.argv0, name)))
        && (m.path.is_empty() || path(call).is_some_and(|path| any_glob(&m.path, &path)))
}

/// The file name of the command, so `rm`, `/bin/rm` and `./rm` are all `rm`.
fn argv0(call: &ToolCall) -> Option<&str> {
    let program = call.arguments.get("argv")?.get(0)?.as_str()?;
    program.rsplit('/').next()
}

/// `arguments.path` with `.` and `..` resolved, so `src/../secret` cannot
/// pass as something under `src/`. A path that climbs out of where it started
/// matches no pattern.
fn path(call: &ToolCall) -> Option<String> {
    let raw = call.arguments.get("path")?.as_str()?;
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    let root = if raw.starts_with('/') { "/" } else { "" };
    Some(format!("{root}{}", parts.join("/")))
}

/// One line saying what the call is, for a human deciding an `ask`.
fn describe(call: &ToolCall) -> String {
    const MAX: usize = 200;
    let mut arguments = call.arguments.to_string();
    if arguments.chars().count() > MAX {
        arguments = arguments.chars().take(MAX).collect::<String>() + "...";
    }
    format!("Allow {} {arguments}?", qualified_name(call))
}
