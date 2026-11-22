//! The gRPC service: runs held by the daemon, driven over the socket.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tether_core::tools;
use tether_core::{
    Answerer, AskOutcome, AskRequest, Clock, Run, RunConfig, RunDeps, RunError, SandboxRef,
    SystemClock,
};
use tether_mcp::sandbox_server::{SandboxServer, SERVER};
use tether_mcp::McpClient;
use tether_models::anthropic::AnthropicAdapter;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, ModelAdapter, ToolDef};
use tether_policy::{KillSwitch, RulePolicy};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::{Payload, RunId, RunState};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::proto;
use crate::proto::tether_server::Tether;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What keeps the daemon up: runs in progress and event streams open.
struct Activity {
    busy: AtomicUsize,
    /// When `busy` last dropped to zero, or the daemon started.
    idle_since: Mutex<Instant>,
}

/// The activity `create_run` begins, ended again if the call returns
/// without having made a run.
struct Starting<'a>(&'a Activity, bool);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.end();
        }
    }
}

impl Activity {
    fn begin(&self) {
        self.busy.fetch_add(1, Ordering::SeqCst);
    }

    fn end(&self) {
        if self.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
            *lock(&self.idle_since) = Instant::now();
        }
    }

    fn idle(&self) -> Option<Duration> {
        (self.busy.load(Ordering::SeqCst) == 0).then(|| lock(&self.idle_since).elapsed())
    }
}

/// Asks waiting for an `Answer` call, by the seq they are about.
#[derive(Default)]
struct Asks {
    pending: Mutex<HashMap<u64, oneshot::Sender<(bool, String)>>>,
}

#[async_trait]
impl Answerer for Asks {
    async fn answer(&self, ask: &AskRequest) -> AskOutcome {
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(ask.subject, tx);
        let waited = tokio::time::timeout(Duration::from_secs(ask.timeout_secs), rx).await;
        lock(&self.pending).remove(&ask.subject);
        match waited {
            Ok(Ok((allow, answered_by))) => AskOutcome::Answered { allow, answered_by },
            _ => AskOutcome::Unanswered {
                waited_secs: ask.timeout_secs,
            },
        }
    }
}

struct Entry {
    /// `None` once the run has finished. One call at a time per run.
    run: tokio::sync::Mutex<Option<Run>>,
    trace: PathBuf,
    kill: KillSwitch,
    asks: Arc<Asks>,
    /// True until the run finishes or halts.
    in_progress: AtomicBool,
    /// When the agent last made a call on this run, or one returned.
    last_call: Mutex<Instant>,
    /// How long the run may go without one before it is halted.
    idle_timeout: Option<Duration>,
}

impl Entry {
    /// Note a call on the run now, and again when the returned guard is
    /// dropped: a long model call or tool call is not idleness.
    fn touch(self: &Arc<Self>) -> Touch {
        *lock(&self.last_call) = Instant::now();
        Touch(Arc::clone(self))
    }
}

struct Touch(Arc<Entry>);

impl Drop for Touch {
    fn drop(&mut self) {
        *lock(&self.0.last_call) = Instant::now();
    }
}

struct Inner {
    runs: Mutex<HashMap<String, Arc<Entry>>>,
    activity: Activity,
    next_run: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct Daemon {
    inner: Arc<Inner>,
}

fn invalid(what: &str, e: impl std::fmt::Display) -> Status {
    Status::invalid_argument(format!("{what}: {e}"))
}

fn json<T: serde::de::DeserializeOwned>(what: &str, text: &str) -> Result<T, Status> {
    serde_json::from_str(text).map_err(|e| invalid(what, e))
}

/// JSON that may be left empty, read as `empty`.
fn json_or(what: &str, text: &str, empty: Value) -> Result<Value, Status> {
    if text.trim().is_empty() {
        Ok(empty)
    } else {
        json(what, text)
    }
}

fn run_error(e: RunError) -> Status {
    match e {
        RunError::Halted { .. } | RunError::NotRunning(_) | RunError::Model(_) => {
            Status::failed_precondition(e.to_string())
        }
        RunError::Tools(_) => Status::unavailable(e.to_string()),
        RunError::Trace(_) => Status::internal(e.to_string()),
    }
}

impl Inner {
    /// Halt the runs whose agents have gone quiet for longer than they are
    /// allowed.
    ///
    /// An agent that crashed, or was killed, never says so: it just makes no
    /// more calls. Its run would otherwise stay in progress for ever, with
    /// the daemon, its sandbox and its MCP servers kept alive for nobody.
    /// Halting it writes why to the trace, lets go of the servers, and
    /// leaves the run resumable.
    async fn halt_abandoned(&self) {
        let quiet: Vec<Arc<Entry>> = lock(&self.runs)
            .values()
            .filter(|entry| entry.in_progress.load(Ordering::SeqCst))
            .filter(|entry| {
                entry
                    .idle_timeout
                    .is_some_and(|limit| lock(&entry.last_call).elapsed() >= limit)
            })
            .cloned()
            .collect();
        for entry in quiet {
            // A call in flight holds the run: that is work, not idleness.
            let Ok(mut guard) = entry.run.try_lock() else {
                continue;
            };
            let Some(run) = guard.as_mut() else { continue };
            if !matches!(run.state(), RunState::Running | RunState::Suspended) {
                continue;
            }
            let secs = entry.idle_timeout.map_or(0, |limit| limit.as_secs());
            let reason = format!("kill switch: no call from the agent for {secs} s");
            entry.kill.trip(&reason);
            if let Err(e) = run.halt_as_recorded(&reason, false).await {
                tracing::warn!("halting an abandoned run: {e}");
            }
            if entry.in_progress.swap(false, Ordering::SeqCst) {
                self.activity.end();
            }
        }
    }
}

impl Daemon {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                runs: Mutex::default(),
                activity: Activity {
                    busy: AtomicUsize::new(0),
                    idle_since: Mutex::new(Instant::now()),
                },
                next_run: AtomicU64::new(0),
            }),
        }
    }

    /// Resolves once nothing has been in progress for `idle`.
    pub(crate) fn idle_for(&self, idle: Duration) -> impl Future<Output = ()> + Send + 'static {
        let inner = self.inner.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                inner.halt_abandoned().await;
                if inner.activity.idle().is_some_and(|quiet| quiet >= idle) {
                    return;
                }
            }
        }
    }

    fn entry(&self, run_id: &str) -> Result<Arc<Entry>, Status> {
        lock(&self.inner.runs)
            .get(run_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no such run: {run_id}")))
    }

    /// Note that a run is over, once.
    fn ended(&self, entry: &Entry) {
        if entry.in_progress.swap(false, Ordering::SeqCst) {
            self.inner.activity.end();
        }
    }

    /// Turn a failed call into a status, noting the run's end if it halted.
    fn failed(&self, entry: &Entry, run: &Run, e: RunError) -> Status {
        // A run whose trace cannot be written is over as well: nothing more
        // can be recorded, so nothing more will be done.
        if run.state() != RunState::Running || matches!(e, RunError::Trace(_)) {
            self.ended(entry);
        }
        run_error(e)
    }

    fn model(req: &proto::CreateRunRequest) -> Result<Arc<dyn ModelAdapter>, Status> {
        match req.provider.as_str() {
            "" | "anthropic" => {
                let mut adapter = AnthropicAdapter::from_env()
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
                for beta in &req.betas {
                    adapter = adapter.beta(beta);
                }
                Ok(Arc::new(adapter))
            }
            #[cfg(feature = "openai_compat")]
            "openai_compat" => {
                let adapter = tether_models::openai_compat::OpenAiCompatAdapter::from_env()
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
                Ok(Arc::new(adapter))
            }
            "fake" => {
                let text = std::fs::read_to_string(&req.script_path)
                    .map_err(|e| invalid("script_path", e))?;
                let script: Value = json("script", &text)?;
                let replies: Vec<Vec<Block>> = serde_json::from_value(script["replies"].clone())
                    .map_err(|e| invalid("script replies", e))?;
                Ok(Arc::new(FakeAdapter::new(
                    replies.into_iter().map(Completion::new),
                )))
            }
            other => Err(Status::unimplemented(format!(
                "no adapter for provider {other:?}"
            ))),
        }
    }
}

#[tonic::async_trait]
impl Tether for Daemon {
    async fn create_run(
        &self,
        request: Request<proto::CreateRunRequest>,
    ) -> Result<Response<proto::Run>, Status> {
        let req = request.into_inner();
        // Starting a run is activity from its first moment. Connecting an
        // MCP server can take longer than the idle time, and the daemon must
        // not decide it is idle and leave while a run is being made.
        self.inner.activity.begin();
        let mut starting = Starting(&self.inner.activity, true);
        match req.sandbox.as_str() {
            "" | "process" => {}
            other => {
                return Err(Status::unimplemented(format!(
                    "this build has no {other:?} sandbox, only process"
                )));
            }
        }
        let policy = if req.policy_path.is_empty() {
            RulePolicy::default()
        } else {
            let text =
                std::fs::read_to_string(&req.policy_path).map_err(|e| invalid("policy_path", e))?;
            RulePolicy::from_toml(&text).map_err(|e| invalid("policy", e))?
        };
        let model = Self::model(&req)?;

        let dir = PathBuf::from(if req.dir.is_empty() {
            ".tether"
        } else {
            &req.dir
        });
        std::fs::create_dir_all(&dir).map_err(|e| invalid("dir", e))?;
        let clock = Arc::new(SystemClock);
        let run_id = format!(
            "run_{}_{}",
            clock.now_ms(),
            self.inner.next_run.fetch_add(1, Ordering::SeqCst)
        );
        let trace = dir.join(format!("{run_id}.jsonl"));
        let workspace = if req.workspace.is_empty() {
            dir.join(format!("{run_id}.workspace"))
        } else {
            PathBuf::from(&req.workspace)
        };

        let sandbox: Arc<dyn Sandbox> = Arc::new(ProcessSandbox::new());
        let spec = SandboxSpec {
            time_limit_secs: Some(600),
            ..SandboxSpec::new(workspace)
        };
        let handle = sandbox
            .start(&spec)
            .await
            .map_err(|e| Status::failed_precondition(format!("starting the sandbox: {e}")))?;
        let asks = Arc::new(Asks::default());
        let kill = KillSwitch::new();
        let builtin: Arc<dyn McpClient> =
            Arc::new(SandboxServer::new(sandbox.clone(), handle.clone()));
        let servers = tools::server_configs(&policy.config().mcp_servers);
        let idle_timeout = policy.config().budget.idle_timeout();
        let registry = tools::connect(Some((SERVER.to_owned(), builtin)), &servers)
            .await
            .map_err(|e| Status::failed_precondition(format!("connecting MCP servers: {e}")))?;
        let deps = RunDeps {
            model,
            tools: Arc::new(registry),
            policy: Arc::new(policy),
            sandbox: Some(SandboxRef { sandbox, handle }),
            answerer: asks.clone(),
            kill: kill.clone(),
            clock,
            replay: None,
        };
        let config = RunConfig {
            agent: if req.agent.is_empty() {
                "main".into()
            } else {
                req.agent
            },
            ..RunConfig::new(RunId(run_id.clone()), &trace)
        };
        let run = Run::start(config, deps).await.map_err(run_error)?;

        let tools = run
            .tools()
            .iter()
            .map(|tool| proto::ToolSpec {
                name: tool.name.clone(),
                server: tool.server.clone(),
                description: tool.description.clone(),
                input_schema_json: tool.input_schema.to_string(),
            })
            .collect();
        let entry = Arc::new(Entry {
            run: tokio::sync::Mutex::new(Some(run)),
            trace: trace.clone(),
            kill,
            asks,
            in_progress: AtomicBool::new(true),
            last_call: Mutex::new(Instant::now()),
            idle_timeout,
        });
        // The run now holds the activity this call began.
        starting.1 = false;
        lock(&self.inner.runs).insert(run_id.clone(), entry);
        Ok(Response::new(proto::Run {
            run_id,
            trace_path: trace.to_string_lossy().into_owned(),
            tools,
        }))
    }

    async fn complete(
        &self,
        request: Request<proto::ModelRequest>,
    ) -> Result<Response<proto::ModelResponse>, Status> {
        let req = request.into_inner();
        let entry = self.entry(&req.run_id)?;
        // A call from the agent: the run is not abandoned.
        let _call = entry.touch();
        let messages: Vec<Message> = json("messages_json", &req.messages_json)?;
        let params = json_or(
            "params_json",
            &req.params_json,
            Value::Object(Default::default()),
        )?;

        let mut guard = entry.run.lock().await;
        let run = guard
            .as_mut()
            .ok_or_else(|| Status::failed_precondition("the run has finished"))?;
        let tools = req
            .tools
            .iter()
            .map(|name| {
                let spec = run.tools().iter().find(|tool| tool.name == *name);
                spec.map(|tool| ToolDef {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .ok_or_else(|| Status::invalid_argument(format!("no such tool: {name}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = CompletionRequest {
            model: req.model,
            messages,
            tools,
            params,
        };
        let reply = match run.complete(request).await {
            Ok(reply) => reply,
            Err(e) => return Err(self.failed(&entry, run, e)),
        };

        let tool_calls = reply
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                Block::ToolUse {
                    id,
                    name,
                    arguments,
                } => Some(proto::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments_json: arguments.to_string(),
                }),
                _ => None,
            })
            .collect();
        Ok(Response::new(proto::ModelResponse {
            message_json: serde_json::to_string(&reply.message)
                .map_err(|e| Status::internal(e.to_string()))?,
            tool_calls,
            input_tokens: reply.usage.input_tokens,
            output_tokens: reply.usage.output_tokens,
            stop_reason: reply.stop_reason.unwrap_or_default(),
            cache_read_tokens: reply.usage.cache_read_tokens,
            cache_write_tokens: reply.usage.cache_write_tokens,
        }))
    }

    async fn call_tool(
        &self,
        request: Request<proto::ToolCallRequest>,
    ) -> Result<Response<proto::ToolResult>, Status> {
        let req = request.into_inner();
        let entry = self.entry(&req.run_id)?;
        // A call from the agent: the run is not abandoned.
        let _call = entry.touch();
        let call = req
            .call
            .ok_or_else(|| Status::invalid_argument("call is missing"))?;
        let arguments = json_or(
            "arguments_json",
            &call.arguments_json,
            Value::Object(Default::default()),
        )?;

        let mut guard = entry.run.lock().await;
        let run = guard
            .as_mut()
            .ok_or_else(|| Status::failed_precondition("the run has finished"))?;
        let result = match run.call_tool(&call.id, &call.name, arguments).await {
            Ok(result) => result,
            Err(e) => return Err(self.failed(&entry, run, e)),
        };
        // The agent always gets the content itself, never a blob reference.
        let content = match &result.content {
            Payload::Inline(value) => value.to_string(),
            blob @ Payload::Blob { .. } => {
                serde_json::to_string(blob).map_err(|e| Status::internal(e.to_string()))?
            }
        };
        Ok(Response::new(proto::ToolResult {
            id: result.id,
            content_json: content,
            is_error: result.is_error,
            duration_ms: result.duration_ms,
        }))
    }

    async fn finish(
        &self,
        request: Request<proto::FinishRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        let req = request.into_inner();
        let entry = self.entry(&req.run_id)?;
        // A call from the agent: the run is not abandoned.
        let _call = entry.touch();
        let outcome = json_or("outcome_json", &req.outcome_json, Value::Null)?;

        let mut guard = entry.run.lock().await;
        let run = guard
            .take()
            .ok_or_else(|| Status::failed_precondition("the run has finished"))?;
        let finished = run.finish(outcome).await;
        self.ended(&entry);
        finished.map_err(run_error)?;
        Ok(Response::new(proto::Empty {}))
    }

    type EventsStream = ReceiverStream<Result<proto::Event, Status>>;

    async fn events(
        &self,
        request: Request<proto::RunRef>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        let entry = self.entry(&request.into_inner().run_id)?;
        let (tx, rx) = mpsc::channel(64);
        let daemon = self.clone();
        daemon.inner.activity.begin();
        tokio::spawn(async move {
            let mut sent = 0;
            loop {
                // Read before checking, so the events written as the run
                // ended are not missed.
                let over = !entry.in_progress.load(Ordering::SeqCst);
                let lines = read_lines(&entry.trace, sent);
                for line in &lines {
                    let event: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                    let event = proto::Event {
                        seq: event["seq"].as_u64().unwrap_or(sent as u64),
                        r#type: event["type"].as_str().unwrap_or_default().to_owned(),
                        json: line.clone(),
                    };
                    sent += 1;
                    if tx.send(Ok(event)).await.is_err() {
                        // The client stopped listening.
                        daemon.inner.activity.end();
                        return;
                    }
                }
                if over && lines.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            daemon.inner.activity.end();
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn answer(
        &self,
        request: Request<proto::AskAnswer>,
    ) -> Result<Response<proto::Empty>, Status> {
        let req = request.into_inner();
        let entry = self.entry(&req.run_id)?;
        // A call from the agent: the run is not abandoned.
        let _call = entry.touch();
        let waiting = lock(&entry.asks.pending).remove(&req.subject_seq);
        let Some(waiting) = waiting else {
            return Err(Status::not_found(format!(
                "no ask is waiting about seq {}",
                req.subject_seq
            )));
        };
        let answered_by = if req.answered_by.is_empty() {
            "sdk".to_owned()
        } else {
            req.answered_by
        };
        // The ask may have timed out in the instant before; nothing to do then.
        let _ = waiting.send((req.allow, answered_by));
        Ok(Response::new(proto::Empty {}))
    }

    async fn halt(
        &self,
        request: Request<proto::RunRef>,
    ) -> Result<Response<proto::Empty>, Status> {
        let entry = self.entry(&request.into_inner().run_id)?;
        const REASON: &str = "halt requested";
        // A call in flight sees the switch at its next event boundary.
        entry.kill.trip(REASON);
        // With no call in flight there is no boundary coming: halt now.
        if let Ok(mut guard) = entry.run.try_lock() {
            // A run left waiting on an ask counts: its caller went away
            // mid-question, and without this it would wait for ever.
            if let Some(run) = guard
                .as_mut()
                .filter(|run| matches!(run.state(), RunState::Running | RunState::Suspended))
            {
                let halted = run
                    .halt_as_recorded(&format!("kill switch: {REASON}"), false)
                    .await;
                // Over either way, also when the halt could not be written.
                self.ended(&entry);
                halted.map_err(run_error)?;
            }
        }
        Ok(Response::new(proto::Empty {}))
    }
}

/// The complete lines of the trace at `path` after the first `skip`.
fn read_lines(path: &Path, skip: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // A last line with no newline yet is an event still being written.
    let whole = text.rfind('\n').map_or("", |end| &text[..end]);
    whole.lines().skip(skip).map(str::to_owned).collect()
}
