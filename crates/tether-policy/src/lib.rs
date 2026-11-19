//! Policy engine: rules evaluated top to bottom, first match wins, default `ask`.
//!
//! Policies are TOML (`tether.toml`), loaded once per run and snapshotted into
//! the `run.created` event. This crate decides; it does not enforce. The
//! sandbox enforces.

use tether_types::{Decision, ModelRequest, ToolCall};

pub mod config;
pub mod glob;
pub mod rules;

pub use config::PolicyConfig;
pub use rules::RulePolicy;

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("rule {rule:?}: {reason}")]
    InvalidRule { rule: String, reason: String },
}

/// Per-run context a rule may consult: budget so far, step, agent id.
#[derive(Debug, Clone, Default)]
pub struct RunCtx {
    pub step: u64,
    pub agent: String,
    pub budget: tether_types::BudgetSnapshot,
}

/// A decision and the rule that made it. Both go into `policy.decision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub decision: Decision,
    /// Name of the matched rule; `None` when no rule matched.
    pub rule: Option<String>,
}

impl From<Decision> for Verdict {
    fn from(decision: Decision) -> Self {
        Self {
            decision,
            rule: None,
        }
    }
}

/// Every tool call and model call passes through exactly one of these.
pub trait Policy: Send + Sync {
    fn decide(&self, call: &ToolCall, ctx: &RunCtx) -> Verdict;

    /// Decide a model call. Rules in `tether.toml` match tools, so the default
    /// allows; budgets are what normally stop a model call.
    fn decide_model(&self, _req: &ModelRequest, _ctx: &RunCtx) -> Verdict {
        Decision::Allow.into()
    }

    /// The policy as the full text of its TOML file, recorded in
    /// `run.created` so a trace says what rules the run was held to. `None`
    /// for a policy that did not come from a file.
    fn snapshot(&self) -> Option<String> {
        None
    }
}

/// Allows everything, for tests and examples. Decisions are still recorded
/// in the trace.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Policy for AllowAll {
    fn decide(&self, _call: &ToolCall, _ctx: &RunCtx) -> Verdict {
        Decision::Allow.into()
    }
}

/// Default when nobody answers a `policy.ask`, in seconds.
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 600;

pub mod budget {
    //! Token, dollar, wall-clock and tool-call budgets.
}

pub mod kill {
    //! File-watch and Unix-signal kill switch; halts at the next event boundary.
}
