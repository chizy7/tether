//! Shared types for Tether.
//!
//! This crate holds the data shapes every other crate agrees on: the trace
//! event envelope, tool calls and results, model requests and responses, and
//! policy decisions. It contains no logic and depends only on `serde`,
//! `serde_json` and `time`, so any crate can use it without pulling in the
//! rest of the workspace.
//!
//! The on-disk format these types serialize to is specified in
//! `docs/TRACE_FORMAT.md`. Changing a type here is a schema change; update the
//! spec in the same PR.

pub mod event;
pub mod model;
pub mod policy;
pub mod tool;

pub use event::{Event, EventType, RunId, RunState, SCHEMA_VERSION};
pub use model::{ModelRequest, ModelResponse, Usage};
pub use policy::{BudgetSnapshot, Decision, Price};
pub use tool::{Payload, ToolCall, ToolResult};
