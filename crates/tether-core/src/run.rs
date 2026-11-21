//! Run lifecycle and the state machine (`RunState`).
//!
//! A [`Run`] answers the two things an agent asks for: a model completion
//! ([`Run::complete`]) and a tool call ([`Run::call_tool`]). Each goes through
//! policy first and leaves its events in the trace in the order
//! `docs/TRACE_FORMAT.md` guarantees. How the agent loops is its own business.

use std::mem::discriminant;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tether_mcp::{McpClient, McpError, ToolSpec};
use tether_models::{Block, Completion, CompletionRequest, ModelAdapter, ModelError};
use tether_policy::{budget, KillSwitch, OnTimeout, Policy, RunCtx, Verdict};
use tether_sandbox::{Handle, Sandbox, SandboxError};
use tether_trace::{NewEvent, TraceError, TraceWriter};
use tether_types::{
    BudgetSnapshot, Decision, Event, EventType, ModelRequest, ModelResponse, Payload, Price, RunId,
    RunState, ToolCall, ToolResult, SCHEMA_VERSION,
};
use time::OffsetDateTime;

use crate::ask::{Answerer, AskOutcome, AskRequest};
use crate::replay::Timeline;
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
    /// Write a `checkpoint` after every this many steps. 0 writes none.
    pub checkpoint_every: u64,
}

impl RunConfig {
    /// A single-agent run with a checkpoint after every step.
    pub fn new(run: RunId, trace: impl Into<PathBuf>) -> Self {
        Self {
            run,
            agent: "main".into(),
            trace: trace.into(),
            checkpoint_every: 1,
        }
    }
}

/// The sandbox a run's tools work in, for the things the run loop itself
/// needs from it: its name for `run.created` and a snapshot per checkpoint.
#[derive(Clone)]
pub struct SandboxRef {
    pub sandbox: Arc<dyn Sandbox>,
    pub handle: Handle,
}

/// Everything a run talks to, behind the traits that let replay swap it.
#[derive(Clone)]
pub struct RunDeps {
    pub model: Arc<dyn ModelAdapter>,
    pub tools: Arc<dyn McpClient>,
    pub policy: Arc<dyn Policy>,
    /// The sandbox behind `tools`, if there is one. Without it checkpoints
    /// carry no snapshot.
    pub sandbox: Option<SandboxRef>,
    /// Who answers a `policy.ask`. [`crate::Unattended`] if nobody can.
    pub answerer: Arc<dyn Answerer>,
    /// Stops the run from outside. `Run::start` also makes it watch the
    /// halt file, `<run>.halt` beside the trace.
    pub kill: KillSwitch,
    pub clock: Arc<dyn Clock>,
    /// The recording this run replays, if it is a replay. While the run
    /// keeps to it, timestamps and timings come from it and not the clock.
    pub replay: Option<Arc<Timeline>>,
}

/// How often a run waiting on an ask looks at the kill switch.
const KILL_POLL: Duration = Duration::from_millis(100);

/// How an ask came out, when the run goes on.
enum Answer {
    Allow,
    Deny { by: String },
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
    checkpoint_every: u64,
    /// The last step a checkpoint was written for.
    checkpointed: u64,
    /// In a replay: how far into the recording this run has got.
    cursor: usize,
    /// In a replay: the timestamp last taken from the recording.
    recorded_ts: Option<OffsetDateTime>,
    /// Set once a replay has left its recording and the wall clock has been
    /// moved to carry on from the recorded elapsed time.
    rebased: bool,
}

impl Run {
    /// Create the trace, record `run.created` and move to `Running`.
    pub async fn start(config: RunConfig, deps: RunDeps) -> Result<Self, RunError> {
        let tools = deps.tools.list_tools().await?;
        let limits = deps.policy.limits();
        deps.kill.watch_file(config.trace.with_extension("halt"));
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
            checkpoint_every: config.checkpoint_every,
            checkpointed: 0,
            cursor: 0,
            recorded_ts: None,
            rebased: false,
        };
        let created = json!({
            "agent": run.agent,
            "model": { "provider": run.deps.model.id() },
            "policy": run.deps.policy.snapshot(),
            "sandbox": run.deps.sandbox.as_ref().map(|s| json!({"backend": s.sandbox.backend()})),
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
    /// `budget.tick`, after a `checkpoint` for the step before if one is due.
    /// A denied call or a provider error halts the run, and so does a spent
    /// budget, before anything is asked of the model.
    pub async fn complete(&mut self, req: CompletionRequest) -> Result<Completion, RunError> {
        self.ensure_running()?;
        // The step before this request is complete. Checkpoint it first, so
        // a halt on the next two lines still leaves that step resumable.
        self.checkpoint().await?;
        self.check_kill().await?;
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
        match verdict.decision.clone() {
            Decision::Allow => {}
            Decision::Deny { reason } => {
                return Err(self.halt(format!("model call denied: {reason}")).await);
            }
            Decision::Ask { prompt } => {
                if let Answer::Deny { by } = self.ask(asked.seq, prompt, &verdict).await? {
                    return Err(self.halt(format!("model call denied by {by}")).await);
                }
            }
        }

        let asked_model = req.model.clone();
        let sent_ms = self.deps.clock.now_ms();
        let completion = match self.deps.model.complete(req).await {
            Ok(completion) => completion,
            Err(e) => {
                self.halt(format!("model error: {e}")).await;
                return Err(e.into());
            }
        };
        let latency_ms = self.deps.clock.now_ms().saturating_sub(sent_ms);

        // Cached tokens are tokens the model read or wrote too.
        let usage = completion.usage;
        self.budget.tokens_used += usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_tokens
            + usage.cache_write_tokens;
        // Priced as the model that answered, which a provider-side fallback
        // can make different from the one asked for.
        let answered_by = completion.model.as_deref().unwrap_or(&asked_model);
        let price = self.deps.model.price(answered_by, &usage);
        if let Some(price) = &price {
            self.budget.usd_used += price.usd;
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
            stop_reason: completion.stop_reason.clone(),
            latency_ms,
            ttft_ms: completion.ttft_ms,
            stream: completion.stream,
            provider_request_id: completion.provider_request_id.clone(),
            raw,
        };
        self.emit(Some(asked.seq), EventType::ModelResponse(response))
            .await?;
        self.tick(asked.seq, price).await?;
        Ok(completion)
    }

    /// Answer a tool request within the current step.
    ///
    /// Trace: `tool.call`, `policy.decision`, `tool.result`, `budget.tick`. A
    /// denied call, an unknown tool and a failed dispatch all come back as an
    /// ordinary result with `is_error` set, so the agent sees a normal failed
    /// call, and so does an `ask` that is answered no. A spent budget halts
    /// the run before the call is recorded; an `ask` nobody answers halts it
    /// after.
    pub async fn call_tool(
        &mut self,
        id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolResult, RunError> {
        self.ensure_running()?;
        self.check_kill().await?;
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
        let denied = match verdict.decision.clone() {
            Decision::Allow => None,
            Decision::Deny { reason } => Some(format!("denied by policy: {reason}")),
            Decision::Ask { prompt } => match self.ask(asked.seq, prompt, &verdict).await? {
                Answer::Allow => None,
                Answer::Deny { by } => Some(format!("denied by {by}")),
            },
        };
        let result = match denied {
            Some(why) => failed(why),
            None => {
                self.budget.tool_calls_used += 1;
                let sent_ms = self.deps.clock.now_ms();
                let outcome = self.deps.tools.call(&call).await;
                let duration_ms = self.deps.clock.now_ms().saturating_sub(sent_ms);
                let mut result = outcome.unwrap_or_else(|e| failed(format!("tool error: {e}")));
                result.duration_ms = duration_ms;
                result
            }
        };

        // The trace may hold a blob reference; the agent gets the content.
        self.emit(Some(asked.seq), EventType::ToolResult(result.clone()))
            .await?;
        self.tick(asked.seq, None).await?;
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

    /// The recording this run is still keeping to, if it is a replay that
    /// has not diverged.
    fn replaying(&self) -> Option<&Arc<Timeline>> {
        self.deps
            .replay
            .as_ref()
            .filter(|timeline| !timeline.state.diverged())
    }

    /// Bring `wall_secs_used` up to date.
    ///
    /// A replay takes it from the recording: the elapsed time on the next
    /// recorded budget, so a budget check sees what the original check saw.
    /// A recorded halt on `max_wall_secs` carries the figure in its reason.
    fn tick_wall(&mut self) {
        if let Some(timeline) = self.replaying() {
            let ahead = timeline.events.iter().skip(self.cursor).take(2);
            let recorded = ahead.filter_map(|event| recorded_wall(&event.body)).next();
            if let Some(wall) = recorded {
                self.budget.wall_secs_used = wall;
            }
            return;
        }
        let now = self.deps.clock.now_ms();
        if self.deps.replay.is_some() && !self.rebased {
            // Just diverged: carry on from the recorded elapsed time.
            self.started_ms = now.saturating_sub(self.budget.wall_secs_used * 1000);
            self.rebased = true;
        }
        self.budget.wall_secs_used = now.saturating_sub(self.started_ms) / 1000;
    }

    /// Halt the way a recording was halted from outside: by its kill switch,
    /// or by anything else a replay cannot bring about itself. `checkpoint`
    /// says whether the recording checkpointed the finished step first.
    pub async fn halt_as_recorded(
        &mut self,
        reason: &str,
        checkpoint: bool,
    ) -> Result<(), RunError> {
        self.ensure_running()?;
        if checkpoint {
            self.checkpoint().await?;
        }
        self.halt(reason.to_owned()).await;
        Ok(())
    }

    /// Write a `checkpoint` for the step just completed, if one is due.
    ///
    /// The checkpoint is the trace up to this event; `snapshot` adds the
    /// sandbox's own state when the backend can capture it. A backend that
    /// cannot, or a snapshot that fails, leaves it null: resume then rebuilds
    /// the sandbox by replaying the trace's writes.
    async fn checkpoint(&mut self) -> Result<(), RunError> {
        let due = self.checkpoint_every > 0
            && self.step > self.checkpointed
            && self.step.is_multiple_of(self.checkpoint_every);
        if !due {
            return Ok(());
        }
        let snapshot = match &self.deps.sandbox {
            Some(at) => match at.sandbox.snapshot(&at.handle).await {
                Ok(id) => Some(id.0),
                Err(SandboxError::Unsupported(_)) => None,
                Err(e) => {
                    tracing::warn!(step = self.step, "sandbox snapshot failed: {e}");
                    None
                }
            },
            None => None,
        };
        let body = EventType::Checkpoint {
            step: self.step,
            snapshot,
        };
        self.emit(None, body).await?;
        self.checkpointed = self.step;
        Ok(())
    }

    /// Halt if the kill switch is tripped. Like the budget check this runs
    /// ahead of a call's first event: a call already in flight finishes and
    /// is recorded, and the run stops at the boundary after it.
    async fn check_kill(&mut self) -> Result<(), RunError> {
        match self.deps.kill.tripped() {
            Some(why) => Err(self.halt(format!("kill switch: {why}")).await),
            None => Ok(()),
        }
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

    /// Record the budget after the call that `parent` asked for, with what
    /// the call cost if it was a priced model call.
    async fn tick(&mut self, parent: u64, price: Option<Price>) -> Result<(), RunError> {
        self.tick_wall();
        let body = EventType::BudgetTick {
            budget: self.budget,
            price,
        };
        self.emit(Some(parent), body).await?;
        Ok(())
    }

    async fn put_json<T: serde::Serialize>(&self, value: &T) -> Result<String, RunError> {
        let encoded = serde_json::to_vec(value).map_err(TraceError::from)?;
        Ok(self.trace.put_blob(encoded).await?)
    }

    async fn emit(&mut self, parent: Option<u64>, mut body: EventType) -> Result<Event, RunError> {
        // A replay writes each event with the time the recording gave its
        // counterpart, and with its recorded timings. The counterpart is the
        // next recorded event of the same kind within the same call: under a
        // changed policy a call can have more or fewer events than it had,
        // and the ones it still has line up. An event with no counterpart
        // takes the time of the one before it.
        let mut ts = None;
        if let Some(timeline) = self.replaying() {
            let ahead = timeline.events.iter().enumerate().skip(self.cursor);
            let mut found = None;
            for (at, old) in ahead {
                if discriminant(&old.body) == discriminant(&body) {
                    found = Some((at, old));
                    break;
                }
                // Do not look past the start of the next call.
                if matches!(
                    old.body,
                    EventType::ModelRequest(_) | EventType::ToolCall(_)
                ) {
                    break;
                }
            }
            if let Some((at, old)) = found {
                let (old_ts, wall) = (old.ts, restore(&mut body, &old.body));
                self.cursor = at + 1;
                self.recorded_ts = Some(old_ts);
                if let Some(wall) = wall {
                    self.budget.wall_secs_used = wall;
                }
            }
            ts = self.recorded_ts;
        }
        let ts = ts.unwrap_or_else(|| {
            let nanos = i128::from(self.deps.clock.now_ms()) * 1_000_000;
            OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
        });
        let event = NewEvent {
            ts,
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

    /// Record a `policy.ask`, suspend, and wait for the answerer.
    ///
    /// An answer is recorded as `policy.answer` and the run goes back to
    /// `Running`. The kill switch halts the run while it waits. With no
    /// answer the run halts too, unless the rule that asked
    /// set `on_timeout = "deny"`: then the timeout is recorded as a denial
    /// answered by `"timeout"` and the run goes on.
    async fn ask(
        &mut self,
        subject: u64,
        prompt: String,
        verdict: &Verdict,
    ) -> Result<Answer, RunError> {
        let request = AskRequest {
            subject,
            prompt: prompt.clone(),
            rule: verdict.rule.clone(),
            timeout_secs: self.deps.policy.ask_timeout_secs(),
        };
        self.emit(Some(subject), EventType::PolicyAsk { subject, prompt })
            .await?;
        self.transition(RunState::Suspended, "policy.ask").await?;

        // A suspended run is between events, so the kill switch can stop it
        // here without waiting for the answer.
        let answerer = self.deps.answerer.clone();
        let kill = self.deps.kill.clone();
        let outcome = {
            let answer = answerer.answer(&request);
            tokio::pin!(answer);
            loop {
                tokio::select! {
                    outcome = &mut answer => break Ok(outcome),
                    () = tokio::time::sleep(KILL_POLL) => {
                        if let Some(why) = kill.tripped() {
                            break Err(why);
                        }
                    }
                }
            }
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(why) => return Err(self.halt(format!("kill switch: {why}")).await),
        };

        let (allow, answered_by) = match outcome {
            AskOutcome::Answered { allow, answered_by } => (allow, answered_by),
            AskOutcome::Unanswered { .. } if verdict.on_timeout == OnTimeout::Deny => {
                (false, "timeout".to_owned())
            }
            AskOutcome::Unanswered { waited_secs } => {
                let reason = format!("ask unanswered after {waited_secs} s");
                return Err(self.halt(reason).await);
            }
        };
        let body = EventType::PolicyAnswer {
            subject,
            answered_by: answered_by.clone(),
            allow,
        };
        self.emit(Some(subject), body).await?;
        let reason = if allow { "ask allowed" } else { "ask denied" };
        self.transition(RunState::Running, reason).await?;
        Ok(if allow {
            Answer::Allow
        } else {
            Answer::Deny { by: answered_by }
        })
    }
}

/// The elapsed wall time a recorded event carries, if it carries one.
fn recorded_wall(body: &EventType) -> Option<u64> {
    match body {
        EventType::PolicyDecision { budget, .. } | EventType::BudgetTick { budget, .. } => {
            Some(budget.wall_secs_used)
        }
        EventType::RunFinished(data) => data["budget"]["wall_secs_used"].as_u64(),
        // "budget exceeded: max_wall_secs (301 of 300)"
        EventType::RunStateChanged {
            state: RunState::Halted,
            reason,
        } => reason
            .strip_prefix("budget exceeded: max_wall_secs (")?
            .split(' ')
            .next()?
            .parse()
            .ok(),
        _ => None,
    }
}

/// Put the recorded values of everything that came from a clock or a
/// sandbox into `body`, an event of the same kind about to be written in the
/// same place. Returns the recorded elapsed wall time if the event has one.
fn restore(body: &mut EventType, old: &EventType) -> Option<u64> {
    match (body, old) {
        (EventType::ModelResponse(new), EventType::ModelResponse(old)) => {
            new.latency_ms = old.latency_ms;
        }
        (EventType::ToolResult(new), EventType::ToolResult(old)) => {
            new.duration_ms = old.duration_ms;
        }
        (
            EventType::PolicyDecision { budget: new, .. },
            EventType::PolicyDecision { budget: old, .. },
        )
        | (EventType::BudgetTick { budget: new, .. }, EventType::BudgetTick { budget: old, .. }) => {
            new.wall_secs_used = old.wall_secs_used;
        }
        (EventType::RunFinished(new), EventType::RunFinished(old)) => {
            if let Some(wall) = old["budget"]["wall_secs_used"].as_u64() {
                new["budget"]["wall_secs_used"] = json!(wall);
            }
        }
        (
            EventType::Checkpoint { snapshot: new, .. },
            EventType::Checkpoint { snapshot: old, .. },
        ) => {
            new.clone_from(old);
        }
        (EventType::RunCreated(new), EventType::RunCreated(old)) => {
            new["sandbox"] = old["sandbox"].clone();
        }
        _ => {}
    }
    recorded_wall(old)
}
