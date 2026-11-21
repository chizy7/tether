//! The trace event envelope.
//!
//! One run produces one JSONL file; every line is one [`Event`]. Events are
//! append-only, densely numbered by `seq`, and chained by `hash` so a
//! tampered or truncated trace fails verification.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Trace schema version written into every event. Frozen from v0.3 onward.
pub const SCHEMA_VERSION: u8 = 0;

/// Identifier of a run. Opaque to everything but `tether-core`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

/// The states a run can be in. Every transition is a `run.state` event.
///
/// See the state machine in `docs/ARCHITECTURE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Running,
    /// Waiting on a human answer to a `policy.ask`.
    Suspended,
    /// Stopped by budget, kill switch, error or ask timeout. Resumable.
    Halted,
    /// The agent returned. Terminal.
    Finished,
}

/// Event types. Anything not listed here is an `ext.*` event, carried as
/// [`EventType::Ext`] and ignored by replay and diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub enum EventType {
    #[serde(rename = "run.created")]
    RunCreated(serde_json::Value),
    #[serde(rename = "run.state")]
    RunStateChanged { state: RunState, reason: String },
    #[serde(rename = "run.child")]
    RunChild { child: RunId },
    #[serde(rename = "run.finished")]
    RunFinished(serde_json::Value),

    #[serde(rename = "model.request")]
    ModelRequest(crate::model::ModelRequest),
    #[serde(rename = "model.response")]
    ModelResponse(crate::model::ModelResponse),

    #[serde(rename = "tool.call")]
    ToolCall(crate::tool::ToolCall),
    #[serde(rename = "tool.result")]
    ToolResult(crate::tool::ToolResult),

    #[serde(rename = "policy.decision")]
    PolicyDecision {
        /// `seq` of the event this decision is about.
        subject: u64,
        rule: Option<String>,
        decision: crate::policy::Decision,
        budget: crate::policy::BudgetSnapshot,
    },
    #[serde(rename = "policy.ask")]
    PolicyAsk { subject: u64, prompt: String },
    #[serde(rename = "policy.answer")]
    PolicyAnswer {
        subject: u64,
        answered_by: String,
        allow: bool,
    },

    #[serde(rename = "sandbox.exec")]
    SandboxExec(serde_json::Value),
    #[serde(rename = "sandbox.exit")]
    SandboxExit(serde_json::Value),

    #[serde(rename = "budget.tick")]
    BudgetTick {
        #[serde(flatten)]
        budget: crate::policy::BudgetSnapshot,
        /// Cost of the model call this tick follows. Absent after a tool
        /// call, and when the adapter cannot price the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        price: Option<crate::policy::Price>,
    },

    #[serde(rename = "checkpoint")]
    Checkpoint { step: u64, snapshot: Option<String> },

    /// Clock reads, random seeds and environment lookups captured for replay.
    #[serde(rename = "nondeterminism")]
    Nondeterminism(serde_json::Value),

    /// Namespaced extension event. Replay and diff ignore it.
    #[serde(untagged)]
    Ext {
        r#type: String,
        data: serde_json::Value,
    },
}

/// One line of a trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Schema version. Always [`SCHEMA_VERSION`] for events written by this build.
    pub v: u8,
    pub run: RunId,
    /// Dense, starts at 0, increments by exactly 1.
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    /// Agent turn this event belongs to.
    pub step: u64,
    /// Which agent emitted this, for multi-agent runs. Single-agent runs use `"main"`.
    pub agent: String,
    /// `seq` of the request this event answers, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    /// `blake3:<hex>` over the canonical encoding of this event with `hash`
    /// set to the previous event's hash. The first event hashes over an empty
    /// previous hash.
    pub hash: String,
    #[serde(flatten)]
    pub body: EventType,
}
