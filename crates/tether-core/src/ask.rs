//! Answering a `policy.ask`.
//!
//! When policy asks, the run suspends and hands the question to an
//! [`Answerer`]: the CLI prompt, later the SDK `Answer` RPC. The answerer owns
//! the waiting, including the timeout, so the core never touches a timer and
//! replay can substitute the recorded `policy.answer`.

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    /// `seq` of the `tool.call` or `model.request` being asked about.
    pub subject: u64,
    pub prompt: String,
    /// The rule that asked; `None` when no rule matched and the default did.
    pub rule: Option<String>,
    /// How long to wait for an answer, from `ask_timeout_secs`.
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    Answered {
        allow: bool,
        /// Who answered, recorded in `policy.answer`. `"cli"` for the prompt.
        answered_by: String,
    },
    /// Nobody answered. The run halts, or the call is denied if the rule
    /// that asked set `on_timeout = "deny"`.
    Unanswered { waited_secs: u64 },
}

#[async_trait]
pub trait Answerer: Send + Sync {
    async fn answer(&self, ask: &AskRequest) -> AskOutcome;
}

/// Nobody is attached to answer, so every ask goes unanswered at once.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unattended;

#[async_trait]
impl Answerer for Unattended {
    async fn answer(&self, _ask: &AskRequest) -> AskOutcome {
        AskOutcome::Unanswered { waited_secs: 0 }
    }
}
