//! Replay: answer a run from a recorded trace instead of the world.
//!
//! A [`Recording`] is a trace loaded for replay. From it come stand-ins for
//! everything a run asks of the outside: a model that returns the recorded
//! responses, tools that return the recorded results, an answerer that gives
//! the recorded answers, and the recorded timeline the run loop restores
//! timestamps and timings from. Policy is not replayed. It is evaluated again,
//! which is how a changed policy is tested against an old run.
//!
//! This module answers; it does not ask. Whatever drives the run, the original
//! agent or something re-issuing the recorded requests, is outside the core.
//!
//! A replay *diverges* the first time it is asked something the recording has
//! no answer for. From then on every stand-in hands over to its live
//! counterpart if it was given one, and fails if not.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::Value;
use tether_mcp::{McpClient, McpError, ToolSpec};
use tether_models::{
    Block, Completion, CompletionRequest, Message, ModelAdapter, ModelError, ToolDef,
};
use tether_policy::{KillSwitch, Limits, OnTimeout, Policy, RunCtx, Verdict};
use tether_trace::blob::hash_bytes;
use tether_trace::{BlobStore, TraceError, TraceReader};
use tether_types::{
    BudgetSnapshot, Decision, Event, EventType, ModelRequest, ModelResponse, Payload, Price,
    RunState, ToolCall, ToolResult, Usage,
};

use crate::ask::{Answerer, AskOutcome, AskRequest};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("trace: {0}")]
    Trace(#[from] TraceError),
    #[error("this trace cannot be replayed: {0}")]
    Invalid(String),
}

fn invalid(why: impl Into<String>) -> ReplayError {
    ReplayError::Invalid(why.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a replay has left its recording, and why. Shared by every
/// stand-in and the run loop, so the first of them to find a mismatch moves
/// all of them to live together.
#[derive(Debug, Default)]
pub struct ReplayState {
    diverged: AtomicBool,
    why: Mutex<Option<String>>,
}

impl ReplayState {
    pub fn diverged(&self) -> bool {
        self.diverged.load(Ordering::SeqCst)
    }

    /// Record a divergence. The first reason is the one kept.
    pub fn diverge(&self, why: impl Into<String>) {
        let mut slot = lock(&self.why);
        if slot.is_none() {
            *slot = Some(why.into());
        }
        self.diverged.store(true, Ordering::SeqCst);
    }

    /// Why the replay diverged, if it has.
    pub fn why(&self) -> Option<String> {
        lock(&self.why).clone()
    }
}

/// The recorded events a run restores its timeline from, in `RunDeps`.
pub struct Timeline {
    pub(crate) events: Vec<Event>,
    pub(crate) state: Arc<ReplayState>,
}

/// What stands in for the world when nothing is recorded: the live adapter,
/// tools and answerer a replay hands over to after it diverges. Leave a field
/// `None` and that kind of request fails after divergence instead.
#[derive(Default)]
pub struct Live {
    pub model: Option<Arc<dyn ModelAdapter>>,
    pub tools: Option<Arc<dyn McpClient>>,
    pub answerer: Option<Arc<dyn Answerer>>,
}

/// Everything a replayed run is built from. `policy` is the recorded
/// decisions played back; use it only when the trace has no policy file to
/// evaluate again.
pub struct ReplayParts {
    pub model: Arc<dyn ModelAdapter>,
    pub tools: Arc<dyn McpClient>,
    pub answerer: Arc<dyn Answerer>,
    pub recorded_policy: Arc<dyn Policy>,
    /// Trips where the recording was stopped by its kill switch.
    pub kill: KillSwitch,
    pub timeline: Arc<Timeline>,
    pub state: Arc<ReplayState>,
}

/// A trace loaded for replay.
pub struct Recording {
    events: Vec<Event>,
    blobs: BlobStore,
}

/// How the recorded run was halted, when it was.
fn halt_reason(events: &[Event]) -> Option<&str> {
    events.iter().rev().find_map(|event| match &event.body {
        EventType::RunStateChanged {
            state: RunState::Halted,
            reason,
        } => Some(reason.as_str()),
        _ => None,
    })
}

impl Recording {
    /// Read and verify a trace.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ReplayError> {
        let path = path.as_ref();
        let events: Vec<Event> = TraceReader::open(path)?.collect::<Result<_, _>>()?;
        if !matches!(
            events.first().map(|e| &e.body),
            Some(EventType::RunCreated(_))
        ) {
            return Err(invalid("it does not start with run.created"));
        }
        Ok(Self {
            events,
            blobs: BlobStore::for_trace(path),
        })
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    fn created(&self) -> &Value {
        match &self.events[0].body {
            EventType::RunCreated(data) => data,
            _ => &Value::Null,
        }
    }

    /// The policy file the run was held to, if it had one.
    pub fn policy_toml(&self) -> Option<&str> {
        self.created()["policy"].as_str()
    }

    /// The tool list recorded at run start.
    pub fn tools(&self) -> Vec<ToolSpec> {
        let tools = self.created()["tools"].as_array();
        let text = |tool: &Value, name: &str| tool[name].as_str().unwrap_or_default().to_owned();
        tools
            .into_iter()
            .flatten()
            .map(|tool| ToolSpec {
                name: text(tool, "name"),
                server: text(tool, "server"),
                description: text(tool, "description"),
                input_schema: tool["input_schema"].clone(),
                external: tool["external"].as_bool().unwrap_or(false),
            })
            .collect()
    }

    /// The servers recorded at run start, replayed as they were so the
    /// replayed `run.created` matches without connecting to anything.
    pub fn servers(&self) -> Vec<tether_mcp::ServerInfo> {
        self.created()["mcp_servers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect()
    }

    /// How often the run wrote checkpoints: the step of its first one, or 0
    /// if it wrote none.
    pub fn checkpoint_every(&self) -> u64 {
        self.events
            .iter()
            .find_map(|event| match event.body {
                EventType::Checkpoint { step, .. } => Some(step),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// The reason on the run's final halt, if it ended halted.
    pub fn halted(&self) -> Option<&str> {
        match self.events.iter().rev().find_map(|e| match &e.body {
            EventType::RunStateChanged { state, .. } => Some(*state),
            _ => None,
        }) {
            Some(RunState::Halted) => halt_reason(&self.events),
            _ => None,
        }
    }

    fn message(&self, hash: &str) -> Result<Message, ReplayError> {
        let bytes = self.blobs.get(hash)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("blob {hash} is not a message: {e}")))
    }

    /// A recorded `model.request` as the request the agent made, with its
    /// messages read back from the blob store.
    pub fn completion_request(
        &self,
        request: &ModelRequest,
    ) -> Result<CompletionRequest, ReplayError> {
        let specs = self.tools();
        let tools = request
            .tools
            .iter()
            .map(|name| {
                let spec = specs.iter().find(|spec| spec.name == *name);
                ToolDef {
                    name: name.clone(),
                    description: spec.map(|s| s.description.clone()).unwrap_or_default(),
                    input_schema: spec.map_or(Value::Null, |s| s.input_schema.clone()),
                }
            })
            .collect();
        Ok(CompletionRequest {
            model: request.model.clone(),
            messages: request
                .messages
                .iter()
                .map(|hash| self.message(hash))
                .collect::<Result<_, _>>()?,
            tools,
            params: request.params.clone(),
        })
    }

    /// The event that answers `seq`, picked out by `pick`.
    fn answer_to<'a, T>(&'a self, seq: u64, pick: impl Fn(&'a Event) -> Option<T>) -> Option<T> {
        self.events[seq as usize + 1..].iter().find_map(pick)
    }

    /// A recorded `model.response` as the completion the agent got.
    fn completion(&self, response: &ModelResponse) -> Result<Completion, ReplayError> {
        Ok(Completion {
            message: self.message(&response.message)?,
            model: None,
            stop_reason: response.stop_reason.clone(),
            usage: response.usage,
            ttft_ms: response.ttft_ms,
            stream: response.stream,
            provider_request_id: response.provider_request_id.clone(),
            raw: match &response.raw {
                Some(hash) => Some(self.blobs.get(hash)?),
                None => None,
            },
        })
    }

    fn model_calls(&self) -> Result<Vec<ModelCall>, ReplayError> {
        let mut calls = Vec::new();
        for event in &self.events {
            let EventType::ModelRequest(request) = &event.body else {
                continue;
            };
            let response = self.answer_to(event.seq, |e| match &e.body {
                EventType::ModelResponse(r) if e.parent == Some(event.seq) => Some(r),
                _ => None,
            });
            let outcome = match response {
                Some(response) => Ok(self.completion(response)?),
                // The provider failed and the run halted with its error.
                None => Err(halt_reason(&self.events)
                    .and_then(|reason| reason.strip_prefix("model error: "))
                    .unwrap_or("the recording has no response to this request")
                    .to_owned()),
            };
            let price = self.answer_to(event.seq, |e| match &e.body {
                EventType::BudgetTick { price, .. } if e.parent == Some(event.seq) => {
                    Some(price.clone())
                }
                _ => None,
            });
            calls.push(ModelCall {
                messages: request.messages.clone(),
                outcome,
                price: price.flatten(),
                used: false,
            });
        }
        Ok(calls)
    }

    /// For each recorded tool call: the decision about it and its answer.
    fn decision_on(&self, seq: u64) -> (Option<&Decision>, Option<&str>, Option<(bool, &str)>) {
        let decision = self.answer_to(seq, |e| match &e.body {
            EventType::PolicyDecision {
                subject,
                decision,
                rule,
                ..
            } if *subject == seq => Some((decision, rule.as_deref())),
            _ => None,
        });
        let answer = self.answer_to(seq, |e| match &e.body {
            EventType::PolicyAnswer {
                subject,
                allow,
                answered_by,
            } if *subject == seq => Some((*allow, answered_by.as_str())),
            _ => None,
        });
        (decision.map(|d| d.0), decision.and_then(|d| d.1), answer)
    }

    fn tool_calls(&self) -> Result<Vec<ToolCallRecord>, ReplayError> {
        let mut calls = Vec::new();
        for event in &self.events {
            let EventType::ToolCall(call) = &event.body else {
                continue;
            };
            let (decision, _, answer) = self.decision_on(event.seq);
            let dispatched = match decision {
                Some(Decision::Allow) => true,
                Some(Decision::Ask { .. }) => answer.is_some_and(|(allow, _)| allow),
                _ => false,
            };
            let result = self.answer_to(event.seq, |e| match &e.body {
                EventType::ToolResult(r) if e.parent == Some(event.seq) => Some(r),
                _ => None,
            });
            let result = match result {
                Some(result) => Some(ToolResult {
                    // A spilled result goes back to the agent whole.
                    content: match &result.content {
                        Payload::Blob { blob, .. } => {
                            let bytes = self.blobs.get(blob)?;
                            Payload::Inline(serde_json::from_slice(&bytes).map_err(|e| {
                                invalid(format!("blob {blob} is not a tool result: {e}"))
                            })?)
                        }
                        inline => inline.clone(),
                    },
                    ..result.clone()
                }),
                None => None,
            };
            calls.push(ToolCallRecord {
                seq: event.seq,
                call: call.clone(),
                dispatched,
                result,
                used: false,
            });
        }
        Ok(calls)
    }

    /// The stand-ins for a replay of this recording, handing over to `live`
    /// after a divergence.
    pub fn parts(&self, live: Live) -> Result<ReplayParts, ReplayError> {
        if self.events.iter().any(tether_trace::resumed::is_resume) {
            return Err(invalid(
                "it was resumed, and replaying a resumed trace is not supported yet",
            ));
        }
        let state = Arc::new(ReplayState::default());
        let kill = KillSwitch::new();
        let provider = self.created()["model"]["provider"]
            .as_str()
            .unwrap_or("replay")
            .to_owned();

        // Answers in the order they were given. A denial "answered" by a
        // timeout was the run loop applying a rule to no answer at all.
        let answers = self
            .events
            .iter()
            .filter_map(|event| match &event.body {
                EventType::PolicyAnswer {
                    allow, answered_by, ..
                } => Some((answered_by != "timeout").then(|| (*allow, answered_by.clone()))),
                _ => None,
            })
            .collect();

        // Decisions the policy made, in order. One about an unknown tool was
        // the run loop's own and never reached the policy.
        let mut tool_verdicts = VecDeque::new();
        let mut model_verdicts = VecDeque::new();
        let mut limits = None;
        for event in &self.events {
            let EventType::PolicyDecision {
                subject,
                decision,
                rule,
                budget,
            } = &event.body
            else {
                continue;
            };
            limits.get_or_insert(Limits {
                max_tokens: budget.tokens_limit,
                max_usd: budget.usd_limit,
                max_wall_secs: budget.wall_secs_limit,
                max_tool_calls: budget.tool_calls_limit,
            });
            let timed_out = self
                .decision_on(*subject)
                .2
                .is_some_and(|(_, by)| by == "timeout");
            let verdict = Verdict {
                decision: decision.clone(),
                rule: rule.clone(),
                on_timeout: if timed_out {
                    OnTimeout::Deny
                } else {
                    OnTimeout::Halt
                },
            };
            match self.events.get(*subject as usize).map(|e| &e.body) {
                Some(EventType::ToolCall(call)) if !call.server.is_empty() => {
                    tool_verdicts.push_back(verdict);
                }
                Some(EventType::ModelRequest(_)) => model_verdicts.push_back(verdict),
                _ => {}
            }
        }

        Ok(ReplayParts {
            model: Arc::new(ReplayModel {
                provider,
                calls: Mutex::new(self.model_calls()?),
                served: Mutex::new(Served::Nothing),
                live: live.model,
                state: state.clone(),
            }),
            tools: Arc::new(ReplayTools {
                specs: self.tools(),
                servers: self.servers(),
                calls: Mutex::new(self.tool_calls()?),
                caught_up: AtomicBool::new(false),
                live: live.tools,
                state: state.clone(),
            }),
            answerer: Arc::new(ReplayAnswerer {
                answers: Mutex::new(answers),
                ending: self.halted().map(str::to_owned),
                kill: kill.clone(),
                live: live.answerer,
                state: state.clone(),
            }),
            recorded_policy: Arc::new(RecordedPolicy {
                tools: Mutex::new(tool_verdicts),
                models: Mutex::new(model_verdicts),
                limits: limits.unwrap_or_default(),
            }),
            kill,
            timeline: Arc::new(Timeline {
                events: self.events.clone(),
                state: state.clone(),
            }),
            state,
        })
    }
}

/// Where a halted run is picked back up, and with what.
pub struct ResumePlan {
    /// The step of the checkpoint the run goes back to; 0 for the start.
    pub step: u64,
    /// What the run had used of its budgets at that checkpoint.
    pub budget: BudgetSnapshot,
    pub next: ResumeNext,
    /// The tool calls that ran before the checkpoint, in order, without
    /// those marked `external`. Running them again in a fresh sandbox
    /// rebuilds what the sandbox held.
    pub earlier_calls: Vec<ToolCall>,
    /// How many times the run has been resumed before.
    pub resumes: u64,
}

/// What the agent does first after a resume.
pub enum ResumeNext {
    /// Make this request again: the one the run made after the checkpoint,
    /// whose step is being done over.
    Ask(CompletionRequest),
    /// The run halted between steps. Carry on from the last finished step:
    /// its request, the reply to it, and the results of its tool calls.
    Continue {
        request: CompletionRequest,
        reply: Box<Completion>,
        results: Vec<Block>,
    },
}

impl Recording {
    /// Work out how to resume this run from its last checkpoint.
    pub fn resume_plan(&self) -> Result<ResumePlan, ReplayError> {
        // What an earlier resume abandoned is not part of the run any more.
        let effective = tether_trace::effective(&self.events);
        let state = effective.iter().rev().find_map(|e| match &e.body {
            EventType::RunStateChanged { state, .. } => Some(*state),
            _ => None,
        });
        if state == Some(RunState::Finished) {
            return Err(invalid("the run finished, so there is nothing to resume"));
        }

        let checkpoint = effective.iter().rev().find_map(|e| match &e.body {
            EventType::Checkpoint { step, .. } => Some((*step, e.seq)),
            _ => None,
        });
        // With no checkpoint the run goes back to its start.
        let (step, upto) = checkpoint.unwrap_or((0, 1));
        let standing = |seq: u64| effective.iter().any(|e| e.seq == seq);
        let before = || effective.iter().filter(move |e| e.seq <= upto);

        let budget = before()
            .rev()
            .find_map(|e| match &e.body {
                EventType::PolicyDecision { budget, .. } | EventType::BudgetTick { budget, .. } => {
                    Some(*budget)
                }
                _ => None,
            })
            .unwrap_or_default();

        let calls = self.tool_calls()?;
        let earlier_calls = calls
            .iter()
            .filter(|c| c.seq <= upto && standing(c.seq) && c.dispatched && !c.call.external)
            .map(|c| c.call.clone())
            .collect();

        let asked_after = effective.iter().find_map(|e| match &e.body {
            EventType::ModelRequest(request) if e.seq > upto => Some(request),
            _ => None,
        });
        let next = if let Some(request) = asked_after {
            ResumeNext::Ask(self.completion_request(request)?)
        } else {
            let last = before().rev().find_map(|e| match &e.body {
                EventType::ModelRequest(request) => Some((e.seq, request)),
                _ => None,
            });
            let Some((asked, request)) = last else {
                return Err(invalid(
                    "the run halted before it asked the model anything, so the trace does not say what it was for",
                ));
            };
            let response = self.answer_to(asked, |e| match &e.body {
                EventType::ModelResponse(r) if e.parent == Some(asked) => Some(r),
                _ => None,
            });
            let Some(response) = response else {
                return Err(invalid("its last finished step has no model response"));
            };
            ResumeNext::Continue {
                request: self.completion_request(request)?,
                reply: Box::new(self.completion(response)?),
                results: calls
                    .iter()
                    .filter(|c| c.seq > asked && c.seq <= upto && standing(c.seq))
                    .filter_map(|c| {
                        let result = c.result.as_ref()?;
                        Some(Block::ToolResult {
                            id: c.call.id.clone(),
                            content: serde_json::json!(result.content),
                            is_error: result.is_error,
                        })
                    })
                    .collect(),
            }
        };

        Ok(ResumePlan {
            step,
            budget,
            next,
            earlier_calls,
            resumes: self
                .events
                .iter()
                .filter(|e| tether_trace::resumed::is_resume(e))
                .count() as u64,
        })
    }
}

struct ModelCall {
    /// Hashes of the request's messages: what a request is matched on.
    messages: Vec<String>,
    /// The recorded response, or the provider's error.
    outcome: Result<Completion, String>,
    price: Option<Price>,
    used: bool,
}

/// What the model stand-in served last, so it can price that call.
enum Served {
    Nothing,
    Recorded(Option<Price>),
    Live,
}

/// Returns recorded responses to requests whose messages hash to the
/// recorded ones. The first request that matches nothing is the divergence.
struct ReplayModel {
    provider: String,
    calls: Mutex<Vec<ModelCall>>,
    served: Mutex<Served>,
    live: Option<Arc<dyn ModelAdapter>>,
    state: Arc<ReplayState>,
}

#[async_trait]
impl ModelAdapter for ReplayModel {
    /// The recorded provider, so the replayed trace names the same one.
    fn id(&self) -> &str {
        &self.provider
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ModelError> {
        // Hashed exactly as the run loop hashes a message into a blob.
        let hashes: Vec<String> = req
            .messages
            .iter()
            .map(|message| hash_bytes(&serde_json::to_vec(message).unwrap_or_default()))
            .collect();

        if !self.state.diverged() {
            let mut calls = lock(&self.calls);
            if let Some(call) = calls.iter_mut().find(|c| !c.used && c.messages == hashes) {
                call.used = true;
                *lock(&self.served) = Served::Recorded(call.price.clone());
                return call.outcome.clone().map_err(ModelError::Replayed);
            }
            // Name the first message the recording does not have next.
            let expected = calls
                .iter()
                .find(|c| !c.used)
                .map(|c| c.messages.as_slice());
            let at = expected.map_or(0, |expected| {
                hashes
                    .iter()
                    .zip(expected)
                    .position(|(new, old)| new != old)
                    .unwrap_or(hashes.len().min(expected.len()))
            });
            self.state.diverge(format!(
                "model request has no recorded response: message {} of {} differs from the recording",
                at + 1,
                hashes.len()
            ));
        }
        match &self.live {
            Some(live) => {
                *lock(&self.served) = Served::Live;
                live.complete(req).await
            }
            None => Err(ModelError::ReplayMiss(
                hashes.last().cloned().unwrap_or_default(),
            )),
        }
    }

    fn price(&self, model: &str, usage: &Usage) -> Option<Price> {
        match &*lock(&self.served) {
            Served::Recorded(price) => price.clone(),
            Served::Live => self.live.as_ref()?.price(model, usage),
            Served::Nothing => None,
        }
    }
}

struct ToolCallRecord {
    /// `seq` of the `tool.call`.
    seq: u64,
    call: ToolCall,
    /// Whether the call actually ran: allowed, or asked and answered yes.
    dispatched: bool,
    result: Option<ToolResult>,
    used: bool,
}

/// Returns recorded results to calls with the recorded name and arguments.
/// A call the recording never ran has no result to give and is a divergence.
struct ReplayTools {
    specs: Vec<ToolSpec>,
    servers: Vec<tether_mcp::ServerInfo>,
    calls: Mutex<Vec<ToolCallRecord>>,
    caught_up: AtomicBool,
    live: Option<Arc<dyn McpClient>>,
    state: Arc<ReplayState>,
}

impl ReplayTools {
    /// Bring the live tools to where the recording had got to, by running
    /// again the calls that were replayed from the recording. Tools marked
    /// `external` act outside the sandbox and are never run again.
    async fn catch_up(&self, live: &Arc<dyn McpClient>) {
        if self.caught_up.swap(true, Ordering::SeqCst) {
            return;
        }
        let replayed: Vec<ToolCall> = lock(&self.calls)
            .iter()
            .filter(|c| c.used && c.dispatched && !c.call.external)
            .map(|c| c.call.clone())
            .collect();
        for call in replayed {
            // The result is already in the trace; only the effect is wanted.
            let _ = live.call(&call).await;
        }
    }
}

#[async_trait]
impl McpClient for ReplayTools {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        Ok(self.specs.clone())
    }

    fn servers(&self) -> Vec<tether_mcp::ServerInfo> {
        self.servers.clone()
    }

    async fn shutdown(&self) {
        if let Some(live) = &self.live {
            live.shutdown().await;
        }
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        if !self.state.diverged() {
            let mut calls = lock(&self.calls);
            let recorded = calls.iter_mut().find(|c| {
                !c.used && c.call.name == call.name && c.call.arguments == call.arguments
            });
            match recorded {
                Some(ToolCallRecord {
                    dispatched: true,
                    result: Some(result),
                    used,
                    ..
                }) => {
                    *used = true;
                    return Ok(ToolResult {
                        id: call.id.clone(),
                        ..result.clone()
                    });
                }
                Some(_) => self.state.diverge(format!(
                    "tool call {} ran in this replay but was not run in the recording",
                    call.name
                )),
                None => self.state.diverge(format!(
                    "tool call {} {} is not in the recording",
                    call.name, call.arguments
                )),
            }
        }
        match &self.live {
            Some(live) => {
                self.catch_up(live).await;
                live.call(call).await
            }
            None => Err(McpError::Other(format!(
                "replay has no recorded result for {} and no live tools to run it",
                call.name
            ))),
        }
    }
}

/// Gives each ask the answer the recording gave, in order.
struct ReplayAnswerer {
    /// `None` is an ask nobody answered that a rule then denied.
    answers: Mutex<VecDeque<Option<(bool, String)>>>,
    /// How the recording was halted, for the ask it never got an answer to.
    ending: Option<String>,
    kill: KillSwitch,
    live: Option<Arc<dyn Answerer>>,
    state: Arc<ReplayState>,
}

#[async_trait]
impl Answerer for ReplayAnswerer {
    async fn answer(&self, ask: &AskRequest) -> AskOutcome {
        if !self.state.diverged() {
            match lock(&self.answers).pop_front() {
                Some(Some((allow, answered_by))) => {
                    return AskOutcome::Answered { allow, answered_by };
                }
                Some(None) => return AskOutcome::Unanswered { waited_secs: 0 },
                None => {}
            }
            // No answer was recorded: this is the ask the run ended on.
            let ending = self.ending.as_deref().unwrap_or_default();
            if let Some(waited) = ending
                .strip_prefix("ask unanswered after ")
                .and_then(|rest| rest.strip_suffix(" s"))
                .and_then(|secs| secs.parse().ok())
            {
                return AskOutcome::Unanswered {
                    waited_secs: waited,
                };
            }
            if let Some(why) = ending.strip_prefix("kill switch: ") {
                // The run was stopped while it waited. Stop this one the same
                // way: trip the switch and keep waiting until it is seen.
                self.kill.trip(why);
                return std::future::pending().await;
            }
            self.state
                .diverge("policy asked a question the recording has no answer to");
        }
        match &self.live {
            Some(live) => live.answer(ask).await,
            None => AskOutcome::Unanswered { waited_secs: 0 },
        }
    }
}

/// Plays back the decisions a run recorded, for a trace with no policy file
/// to evaluate again. Past the end of the recording it asks, like any policy
/// with no matching rule.
struct RecordedPolicy {
    tools: Mutex<VecDeque<Verdict>>,
    models: Mutex<VecDeque<Verdict>>,
    limits: Limits,
}

fn next(queue: &Mutex<VecDeque<Verdict>>, what: &str) -> Verdict {
    lock(queue).pop_front().unwrap_or_else(|| {
        Decision::Ask {
            prompt: format!("Allow {what}? The recording has no decision for it."),
        }
        .into()
    })
}

impl Policy for RecordedPolicy {
    fn decide(&self, call: &ToolCall, _ctx: &RunCtx) -> Verdict {
        next(&self.tools, &call.name)
    }

    fn decide_model(&self, _req: &ModelRequest, _ctx: &RunCtx) -> Verdict {
        next(&self.models, "this model call")
    }

    fn limits(&self) -> Limits {
        self.limits
    }
}
