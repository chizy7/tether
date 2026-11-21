//! Policy decisions and budget snapshots.

use serde::{Deserialize, Serialize};

/// The outcome of `Policy::decide`. Recorded as a `policy.decision` event
/// whether the call was allowed, denied or escalated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Decision {
    Allow,
    Deny {
        reason: String,
    },
    /// Suspend the run and ask a human. On timeout the run moves to `Halted`
    /// unless the matching rule sets `on_timeout = "deny"`.
    Ask {
        prompt: String,
    },
}

/// Budget used against limits at a point in time. Written on every
/// `budget.tick` and alongside every `policy.decision`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub tokens_used: u64,
    pub tokens_limit: Option<u64>,
    pub usd_used: f64,
    pub usd_limit: Option<f64>,
    pub wall_secs_used: u64,
    pub wall_secs_limit: Option<u64>,
    pub tool_calls_used: u64,
    pub tool_calls_limit: Option<u64>,
}

/// What one model call cost, recorded on the `budget.tick` that follows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub usd: f64,
    /// Version of the pricing table the cost was computed from.
    pub table: String,
}
