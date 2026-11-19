//! Token, dollar, wall-clock and tool-call budgets.
//!
//! Limits come from the `[budget]` table. The run loop copies them into its
//! [`BudgetSnapshot`] and asks [`tripped`] before every model and tool call.
//! A budget is spent when what has been used reaches the limit, so
//! `max_tool_calls = 400` allows exactly 400 calls. Tokens, dollars and wall
//! time are only known after a call returns, so the call that crosses one of
//! those limits completes and the next one is refused.

use std::fmt;

use tether_types::BudgetSnapshot;

use crate::config::BudgetConfig;

/// The limits of a run. `None` means unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub max_tokens: Option<u64>,
    pub max_usd: Option<f64>,
    pub max_wall_secs: Option<u64>,
    pub max_tool_calls: Option<u64>,
}

impl From<&BudgetConfig> for Limits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tokens: config.max_tokens,
            max_usd: config.max_usd,
            max_wall_secs: config.max_wall_secs,
            max_tool_calls: config.max_tool_calls,
        }
    }
}

impl Limits {
    /// A snapshot with nothing used yet and these limits.
    pub fn fresh(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            tokens_limit: self.max_tokens,
            usd_limit: self.max_usd,
            wall_secs_limit: self.max_wall_secs,
            tool_calls_limit: self.max_tool_calls,
            ..BudgetSnapshot::default()
        }
    }
}

/// A spent budget, named by its `tether.toml` key.
#[derive(Debug, Clone, PartialEq)]
pub struct Tripped {
    pub budget: &'static str,
    pub used: f64,
    pub limit: f64,
}

impl fmt::Display for Tripped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} of {})", self.budget, self.used, self.limit)
    }
}

/// The first spent budget in `snapshot`, if any.
pub fn tripped(snapshot: &BudgetSnapshot) -> Option<Tripped> {
    let checks = [
        (
            "max_tokens",
            snapshot.tokens_used as f64,
            snapshot.tokens_limit.map(|limit| limit as f64),
        ),
        ("max_usd", snapshot.usd_used, snapshot.usd_limit),
        (
            "max_wall_secs",
            snapshot.wall_secs_used as f64,
            snapshot.wall_secs_limit.map(|limit| limit as f64),
        ),
        (
            "max_tool_calls",
            snapshot.tool_calls_used as f64,
            snapshot.tool_calls_limit.map(|limit| limit as f64),
        ),
    ];
    checks.into_iter().find_map(|(budget, used, limit)| {
        let limit = limit?;
        (used >= limit).then_some(Tripped {
            budget,
            used,
            limit,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{tripped, Limits};

    fn limits() -> Limits {
        Limits {
            max_tokens: Some(100),
            max_usd: Some(1.5),
            max_wall_secs: Some(60),
            max_tool_calls: Some(3),
        }
    }

    #[test]
    fn nothing_trips_below_the_limits_or_without_them() {
        let mut snapshot = limits().fresh();
        snapshot.tokens_used = 99;
        snapshot.usd_used = 1.49;
        snapshot.wall_secs_used = 59;
        snapshot.tool_calls_used = 2;
        assert_eq!(tripped(&snapshot), None);

        let mut unlimited = Limits::default().fresh();
        unlimited.tokens_used = u64::MAX;
        unlimited.usd_used = 1e9;
        assert_eq!(tripped(&unlimited), None);
    }

    #[test]
    fn a_budget_is_spent_when_used_reaches_the_limit() {
        type Spend = fn(&mut tether_types::BudgetSnapshot);
        let spend: [(&str, Spend); 4] = [
            ("max_tokens (100 of 100)", |s| s.tokens_used = 100),
            ("max_usd (1.75 of 1.5)", |s| s.usd_used = 1.75),
            ("max_wall_secs (61 of 60)", |s| s.wall_secs_used = 61),
            ("max_tool_calls (3 of 3)", |s| s.tool_calls_used = 3),
        ];
        for (expected, spend) in spend {
            let mut snapshot = limits().fresh();
            spend(&mut snapshot);
            let tripped = tripped(&snapshot).map(|t| t.to_string());
            assert_eq!(tripped.as_deref(), Some(expected));
        }
    }

    #[test]
    fn the_first_spent_budget_is_the_one_named() {
        let mut snapshot = limits().fresh();
        snapshot.tool_calls_used = 3;
        snapshot.tokens_used = 500;
        assert_eq!(tripped(&snapshot).map(|t| t.budget), Some("max_tokens"));
    }
}
