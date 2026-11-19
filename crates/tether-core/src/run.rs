//! Run lifecycle and the state machine (`RunState`).
//!
//! A [`Run`] answers the two things an agent asks for: a model completion
//! ([`Run::complete`]) and a tool call ([`Run::call_tool`]). Each goes through
//! policy first and leaves its events in the trace in the order
//! `docs/TRACE_FORMAT.md` guarantees. How the agent loops is its own business.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tether_mcp::{McpClient, McpError, ToolSpec};
use tether_models::{Block, Completion, CompletionRequest, ModelAdapter, ModelError};
use tether_policy::{budget, Policy, RunCtx, Verdict};
use tether_trace::{NewEvent, TraceError, TraceWriter};
use tether_types::{
    BudgetSnapshot, Decision, Event, EventType, ModelRequest, ModelResponse, Payload, RunId,
    RunState, ToolCall, ToolResult, SCHEMA_VERSION,
};
use time::OffsetDateTime;

use crate::trace_task::TraceHandle;
use crate::Clock;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("trace: {0}")]
    Trace(#[from] TraceError),
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("tools: {0}")]
    Tools(#[from] McpError),
    #[error("run halted: {reason}")]
    Halted { reason: String },
    #[error("run is {0:?}, not running")]
    NotRunning(RunState),
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub run: RunId,
    /// Agent id written on every event. `"main"` for single-agent runs.
    pub agent: String,
    /// Path of the trace to create, `<run>.jsonl`. Must not exist.
    pub trace: PathBuf,
}

/// Everything a run talks to, behind the traits that let replay swap it.
#[derive(Clone)]
pub struct RunDeps {
    pub model: Arc<dyn ModelAdapter>,
    pub tools: Arc<dyn McpClient>,
    pub policy: Arc<dyn Policy>,
    pub clock: Arc<dyn Clock>,
}

pub struct Run {
    trace: TraceHandle,
    deps: RunDeps,
    agent: String,
    tools: Vec<ToolSpec>,
    state: RunState,
    step: u64,
    started_ms: u64,
    budget: BudgetSnapshot,
}

impl Run {
    /// Create the trace, record `run.created` and move to `Running`.
    pub async fn start(config: RunConfig, deps: RunDeps) -> Result<Self, RunError> {
        let tools = deps.tools.list_tools().await?;
        let limits = deps.policy.limits();
        let writer = TraceWriter::create(&config.trace, config.run)?;
        let mut run = Self {
            trace: TraceHandle::spawn(writer),
            started_ms: deps.clock.now_ms(),
            deps,
            agent: config.agent,
            tools,
            state: RunState::Created,
            step: 0,
            budget: limits.fresh(),
        };
        let created = json!({
            "agent": run.agent,
            "model": { "provider": run.deps.model.id() },
            "policy": run.deps.policy.snapshot(),
            "sandbox": null,
            "tools": run.tools.iter().map(|t| json!({
                "name": t.name,
                "server": t.server,
                "description": t.description,
                "input_schema": t.input_schema,
                "external": t.external,
            })).collect::<Vec<_>>(),
            "schema_version": SCHEMA_VERSION,
        });
        run.emit(None, EventType::RunCreated(created)).await?;
        run.transition(RunState::Running, "start").await?;
        Ok(run)
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    pub fn step(&self) -> u64 {
        self.step
    }

    /// The tools listed at run start, as recorded in `run.created`.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Answer a model request. Starts a new step.
    ///
    /// Trace: `model.request`, `policy.decision`, `model.response`,
    /// `budget.tick`. A denied call or a provider error halts the run, and so
    /// does a spent budget, before anything is asked of the model.
    pub async fn complete(&mut self, req: CompletionRequest) -> Result<Completion, RunError> {
        self.ensure_running()?;
        self.check_budget().await?;
        self.step += 1;

        let mut messages = Vec::with_capacity(req.messages.len());
        for message in &req.messages {
            messages.push(self.put_json(message).await?);
        }
        let request = ModelRequest {
            provider: self.deps.model.id().to_owned(),
            model: req.model.clone(),
            messages,
            tools: req.tools.iter().map(|t| t.name.clone()).collect(),
            params: req.params.clone(),
        };
        let asked = self
            .emit(None, EventType::ModelRequest(request.clone()))
            .await?;

        let verdict = self.deps.policy.decide_model(&request, &self.ctx());
        self.record_decision(asked.seq, verdict.clone()).await?;
        match verdict.decision {
            Decision::Allow => {}
            Decision::Deny { reason } => {
                return Err(self.halt(format!("model call denied: {reason}")).await);
            }
            Decision::Ask { prompt } => return Err(self.ask(asked.seq, prompt).await),
        }

        let sent_ms = self.deps.clock.now_ms();
        let completion = match self.deps.model.complete(req).await {
            Ok(completion) => completion,
            Err(e) => {
                self.halt(format!("model error: {e}")).await;
                return Err(e.into());
            }
        };
        let latency_ms = self.deps.clock.now_ms().saturating_sub(sent_ms);

        self.budget.tokens_used += completion.usage.input_tokens + completion.usage.output_tokens;
        if let Some(price) = self.deps.model.price(&completion.usage) {
            self.budget.usd_used += price.0;
        }

        let message = self.put_json(&completion.message).await?;
        let raw = match &completion.raw {
            Some(raw) => Some(self.trace.put_blob(raw.clone()).await?),
            None => None,
        };
        let tool_calls = completion
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                Block::ToolUse {
                    id,
                    name,
                    arguments,
                } => Some(self.resolve(id, name, arguments.clone()).0),
                _ => None,
            })
            .collect();
        let response = ModelResponse {
            message,
            tool_calls,
            usage: completion.usage,
            latency_ms,
            ttft_ms: completion.ttft_ms,
            stream: completion.stream,
            provider_request_id: completion.provider_request_id.clone(),
            raw,
        };
        self.emit(Some(asked.seq), EventType::ModelResponse(response))
            .await?;
        self.tick(asked.seq).await?;
        Ok(completion)
    }

    /// Answer a tool request within the current step.
    ///
    /// Trace: `tool.call`, `policy.decision`, `tool.result`, `budget.tick`. A
    /// denied call, an unknown tool and a failed dispatch all come back as an
    /// ordinary result with `is_error` set, so the agent sees a normal failed
    /// call. A spent budget halts the run before the call is recorded.
    pub async fn call_tool(
        &mut self,
        id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolResult, RunError> {
        self.ensure_running()?;
        self.check_budget().await?;
        let (call, known) = self.resolve(id, name, arguments);
        let asked = self.emit(None, EventType::ToolCall(call.clone())).await?;

        let verdict = if known {
            self.deps.policy.decide(&call, &self.ctx())
        } else {
            Decision::Deny {
                reason: format!("unknown tool {name:?}"),
            }
            .into()
        };
        self.record_decision(asked.seq, verdict.clone()).await?;

        let failed = |text: String| ToolResult {
            id: call.id.clone(),
            content: Payload::Inline(text.into()),
            is_error: true,
            duration_ms: 0,
        };
        let result = match verdict.decision {
            Decision::Allow => {
                self.budget.tool_calls_used += 1;
                let sent_ms = self.deps.clock.now_ms();
                let outcome = self.deps.tools.call(&call).await;
                let duration_ms = self.deps.clock.now_ms().saturating_sub(sent_ms);
                let mut result = outcome.unwrap_or_else(|e| failed(format!("tool error: {e}")));
                result.duration_ms = duration_ms;
                result
            }
            Decision::Deny { reason } => failed(format!("denied by policy: {reason}")),
            Decision::Ask { prompt } => return Err(self.ask(asked.seq, prompt).await),
        };

        // The trace may hold a blob reference; the agent gets the content.
        self.emit(Some(asked.seq), EventType::ToolResult(result.clone()))
            .await?;
        self.tick(asked.seq).await?;
        Ok(result)
    }

    /// The agent is done. Records `run.finished` and flushes the trace.
    pub async fn finish(mut self, outcome: serde_json::Value) -> Result<(), RunError> {
        self.ensure_running()?;
        self.transition(RunState::Finished, "agent done").await?;
        self.tick_wall();
        let finished = json!({
            "outcome": outcome,
            "steps": self.step,
            "budget": self.budget,
        });
        self.emit(None, EventType::RunFinished(finished)).await?;
        self.trace.sync().await?;
        Ok(())
    }

    fn ensure_running(&self) -> Result<(), RunError> {
        match self.state {
            RunState::Running => Ok(()),
            other => Err(RunError::NotRunning(other)),
        }
    }

    fn ctx(&self) -> RunCtx {
        RunCtx {
            step: self.step,
            agent: self.agent.clone(),
            budget: self.budget,
        }
    }

    /// Fill in the owning server from the tool list recorded at run start.
    /// The flag is false when no such tool was listed.
    fn resolve(&self, id: &str, name: &str, arguments: serde_json::Value) -> (ToolCall, bool) {
        let spec = self.tools.iter().find(|t| t.name == name);
        let call = ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            server: spec.map(|t| t.server.clone()).unwrap_or_default(),
            arguments,
            external: spec.is_some_and(|t| t.external),
        };
        (call, spec.is_some())
    }

    fn tick_wall(&mut self) {
        let elapsed_ms = self.deps.clock.now_ms().saturating_sub(self.started_ms);
        self.budget.wall_secs_used = elapsed_ms / 1000;
    }

    /// Halt if a budget is spent. Runs before every model and tool call,
    /// ahead of its first event, so a halted run ends on an event boundary
    /// and the refused call is not half recorded.
    async fn check_budget(&mut self) -> Result<(), RunError> {
        self.tick_wall();
        match budget::tripped(&self.budget) {
            Some(tripped) => Err(self.halt(format!("budget exceeded: {tripped}")).await),
            None => Ok(()),
        }
    }

    /// Record the budget after the call that `parent` asked for.
    async fn tick(&mut self, parent: u64) -> Result<(), RunError> {
        self.tick_wall();
        self.emit(Some(parent), EventType::BudgetTick(self.budget))
            .await?;
        Ok(())
    }

    async fn put_json<T: serde::Serialize>(&self, value: &T) -> Result<String, RunError> {
        let encoded = serde_json::to_vec(value).map_err(TraceError::from)?;
        Ok(self.trace.put_blob(encoded).await?)
    }

    async fn emit(&self, parent: Option<u64>, body: EventType) -> Result<Event, RunError> {
        let nanos = i128::from(self.deps.clock.now_ms()) * 1_000_000;
        let event = NewEvent {
            ts: OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            step: self.step,
            agent: self.agent.clone(),
            parent,
            body,
        };
        Ok(self.trace.append(event).await?)
    }

    async fn record_decision(&mut self, subject: u64, verdict: Verdict) -> Result<(), RunError> {
        self.tick_wall();
        let body = EventType::PolicyDecision {
            subject,
            rule: verdict.rule,
            decision: verdict.decision,
            budget: self.budget,
        };
        self.emit(Some(subject), body).await?;
        Ok(())
    }

    async fn transition(&mut self, state: RunState, reason: &str) -> Result<(), RunError> {
        let body = EventType::RunStateChanged {
            state,
            reason: reason.to_owned(),
        };
        self.emit(None, body).await?;
        self.state = state;
        Ok(())
    }

    /// Move to `Halted` and return the error to hand back. If the trace
    /// itself cannot be written, that error wins.
    async fn halt(&mut self, reason: String) -> RunError {
        match self.transition(RunState::Halted, &reason).await {
            Ok(()) => RunError::Halted { reason },
            Err(e) => e,
        }
    }

    /// Record a `policy.ask` and suspend. Nothing can answer an ask until the
    /// CLI prompt lands in v0.2, so the run then halts, as an unattended ask
    /// does on timeout.
    async fn ask(&mut self, subject: u64, prompt: String) -> RunError {
        let body = EventType::PolicyAsk { subject, prompt };
        if let Err(e) = self.emit(Some(subject), body).await {
            return e;
        }
        if let Err(e) = self.transition(RunState::Suspended, "policy.ask").await {
            return e;
        }
        self.halt("ask cannot be answered before v0.2".into()).await
    }
}
