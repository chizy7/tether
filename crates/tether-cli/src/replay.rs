//! `tether replay <run>`: run a recorded trace again.
//!
//! The core answers a replayed run from the recording. Something still has
//! to ask, and with no agent attached that is this file: it re-issues the
//! recorded model requests and tool calls in order, as the agent did. If the
//! replay leaves the recording and `--live-after-divergence` is set, it
//! carries on as a plain tool loop against the live model and tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tether_core::{Live, Recording, Run, RunConfig, RunDeps, RunError, SandboxRef, SystemClock};
use tether_mcp::sandbox_server::SandboxServer;
use tether_mcp::McpClient;
use tether_models::anthropic::AnthropicAdapter;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, ModelAdapter, Role};
use tether_policy::{Policy, RulePolicy};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::{EventType, RunId, RunState};

use crate::run::{read_policy, trip_on_sigint, Script, TerminalAnswerer};

pub struct Options {
    pub run: PathBuf,
    pub out: Option<PathBuf>,
    pub policy: Option<PathBuf>,
    pub system: Option<String>,
    pub live: bool,
    pub script: Option<PathBuf>,
}

/// The model to carry on with after a divergence: the script if one was
/// given, else the provider the recording used.
fn live_model(
    recording: &Recording,
    script: Option<&Path>,
) -> anyhow::Result<Arc<dyn ModelAdapter>> {
    if let Some(path) = script {
        let replies = Script::read(path)?.replies;
        return Ok(Arc::new(FakeAdapter::new(
            replies.into_iter().map(Completion::new),
        )));
    }
    let requests = recording
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventType::ModelRequest(request) => Some(request),
            _ => None,
        });
    let mut provider = None;
    let mut uses_fallbacks = false;
    for request in requests {
        provider.get_or_insert(request.provider.as_str());
        uses_fallbacks |= request.params.get("fallbacks").is_some();
    }
    match provider {
        Some("anthropic") => {
            let mut adapter = AnthropicAdapter::from_env()?;
            // Headers are not in a trace. This request field needs one.
            if uses_fallbacks {
                adapter = adapter.beta("server-side-fallback-2026-07-01");
            }
            Ok(Arc::new(adapter))
        }
        Some(other) => bail!(
            "no live adapter for the recorded provider {other:?}; pass --script to stand in for it"
        ),
        None => bail!("the recording has no model requests to carry on from"),
    }
}

/// Put `system` at the head of `messages` as the system prompt, in place of
/// the one that is there.
fn set_system(messages: &mut Vec<Message>, system: &str) {
    let prompt = Message::text(Role::System, system);
    match messages.first_mut() {
        Some(first) if first.role == Role::System => *first = prompt,
        _ => messages.insert(0, prompt),
    }
}

/// Where a replay had got to when it left the recording: enough to carry on.
struct Turn {
    /// The last request made, to copy its model, tools and params.
    request: CompletionRequest,
    /// The reply to it.
    reply: Completion,
    /// Results of the tool calls the reply asked for that have been made.
    results: Vec<Block>,
}

fn result_block(id: &str, result: &tether_types::ToolResult) -> Block {
    Block::ToolResult {
        id: id.to_owned(),
        content: json!(result.content),
        is_error: result.is_error,
    }
}

/// Carry on from `turn` as a plain tool loop: run the tools the model asked
/// for, feed the results back, stop when it asks for none.
async fn carry_on(run: &mut Run, mut turn: Turn) -> Result<String, RunError> {
    loop {
        let mut said = String::new();
        let mut asked = 0;
        for block in &turn.reply.message.content {
            match block {
                Block::Text { text } => said.push_str(text),
                Block::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    asked += 1;
                    // Calls made before the divergence already have results.
                    if asked > turn.results.len() {
                        let result = run.call_tool(id, name, arguments.clone()).await?;
                        turn.results.push(result_block(id, &result));
                    }
                }
                Block::ToolResult { .. } | Block::Provider { .. } => {}
            }
        }
        match turn.reply.stop_reason.as_deref() {
            Some(reason @ ("refusal" | "max_tokens")) => {
                return Ok(format!("stopped early ({reason}): {said}"));
            }
            _ if turn.results.is_empty() => return Ok(said),
            _ => {}
        }
        let mut request = turn.request;
        request.messages.push(turn.reply.message);
        request.messages.push(Message {
            role: Role::Tool,
            content: turn.results,
        });
        let reply = run.complete(request.clone()).await?;
        turn = Turn {
            request,
            reply,
            results: Vec::new(),
        };
    }
}

/// Replay `options.run`. `Ok(false)` when the replay left the recording and
/// stopped there.
pub async fn replay(options: Options) -> anyhow::Result<bool> {
    let recording = Recording::load(&options.run)
        .with_context(|| format!("loading {}", options.run.display()))?;
    let out = options
        .out
        .clone()
        .unwrap_or_else(|| options.run.with_extension("replay.jsonl"));
    let original = recording
        .events()
        .first()
        .map_or("run", |event| event.run.0.as_str());

    // Live tools work in a fresh directory beside the new trace, never the
    // recorded run's workspace.
    let mut live = Live::default();
    let mut sandbox_ref = None;
    if options.live {
        live.model = Some(live_model(&recording, options.script.as_deref())?);
        let sandbox: Arc<dyn Sandbox> = Arc::new(ProcessSandbox::new());
        let spec = SandboxSpec {
            time_limit_secs: Some(60),
            ..SandboxSpec::new(out.with_extension("workspace"))
        };
        let handle = sandbox.start(&spec).await?;
        let tools: Arc<dyn McpClient> =
            Arc::new(SandboxServer::new(sandbox.clone(), handle.clone()));
        live.tools = Some(tools);
        live.answerer = Some(Arc::new(TerminalAnswerer::default()));
        sandbox_ref = Some(SandboxRef { sandbox, handle });
    }
    let parts = recording.parts(live)?;

    // Policy is evaluated again: the given file, else the recorded one. A
    // trace with no policy file plays back the decisions it recorded.
    let policy: Arc<dyn Policy> = match (&options.policy, recording.policy_toml()) {
        (Some(path), _) => Arc::new(read_policy(path)?),
        (None, Some(recorded)) => {
            Arc::new(RulePolicy::from_toml(recorded).context("the policy recorded in the trace")?)
        }
        (None, None) => parts.recorded_policy.clone(),
    };

    trip_on_sigint(&parts.kill);
    let config = RunConfig {
        checkpoint_every: recording.checkpoint_every(),
        ..RunConfig::new(RunId(format!("{original}-replay")), &out)
    };
    let deps = RunDeps {
        model: parts.model.clone(),
        tools: parts.tools.clone(),
        policy,
        sandbox: sandbox_ref,
        answerer: parts.answerer.clone(),
        kill: parts.kill.clone(),
        clock: Arc::new(SystemClock),
        replay: Some(parts.timeline.clone()),
    };
    let mut run = Run::start(config, deps)
        .await
        .with_context(|| format!("starting the replay trace {}", out.display()))?;

    // Ask what the recorded agent asked, in the order it asked.
    let mut turn: Option<Turn> = None;
    let mut outcome: Option<Value> = None;
    let mut stopped: Option<RunError> = None;
    for event in recording.events() {
        if parts.state.diverged() || stopped.is_some() {
            break;
        }
        match &event.body {
            EventType::ModelRequest(recorded) => {
                let mut request = recording.completion_request(recorded)?;
                if let Some(system) = &options.system {
                    set_system(&mut request.messages, system);
                }
                match run.complete(request.clone()).await {
                    Ok(reply) => {
                        turn = Some(Turn {
                            request,
                            reply,
                            results: Vec::new(),
                        });
                    }
                    Err(e) => stopped = Some(e),
                }
            }
            EventType::ToolCall(call) => {
                match run
                    .call_tool(&call.id, &call.name, call.arguments.clone())
                    .await
                {
                    Ok(result) => {
                        if let Some(turn) = &mut turn {
                            turn.results.push(result_block(&call.id, &result));
                        }
                    }
                    Err(e) => stopped = Some(e),
                }
            }
            EventType::RunFinished(data) => outcome = Some(data["outcome"].clone()),
            _ => {}
        }
    }

    let diverged = parts.state.why();
    let mut carried_on = false;
    if run.state() == RunState::Running {
        match (&diverged, turn, options.live) {
            // Still on the recording: end as it ended.
            (None, _, _) => {
                if let Some(outcome) = outcome {
                    run.finish(outcome).await?;
                } else if let Some(reason) = recording.halted() {
                    // Halted from outside, by its kill switch for one. Was
                    // the finished step checkpointed just before?
                    let events = recording.events();
                    let checkpointed = events.len() >= 2
                        && matches!(events[events.len() - 2].body, EventType::Checkpoint { .. });
                    run.halt_as_recorded(reason, checkpointed).await?;
                }
            }
            (Some(_), Some(turn), true) => {
                carried_on = true;
                match carry_on(&mut run, turn).await {
                    Ok(answer) => run.finish(json!(answer)).await?,
                    Err(e) => stopped = Some(e),
                }
            }
            // Left the recording with nothing to carry on with. The trace
            // ends where it diverged.
            (Some(_), _, _) => {}
        }
    }

    println!("replayed {} to {}", options.run.display(), out.display());
    match &diverged {
        None => println!("every request was answered from the recording"),
        Some(why) if carried_on => println!("left the recording and carried on live: {why}"),
        Some(why) => println!("left the recording and stopped: {why}"),
    }
    if let Some(error) = &stopped {
        println!("{error}");
    }
    println!(
        "compare with: tether diff {} {}",
        options.run.display(),
        out.display()
    );
    Ok(diverged.is_none() || carried_on)
}
