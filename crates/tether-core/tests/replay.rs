//! Replay with the agent itself running again: the case the v0.3 demo needs.
//! The CLI's tests cover replay driven from the recording alone.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tether_core::{
    Clock, Live, Recording, Run, RunConfig, RunDeps, RunError, SystemClock, Unattended,
};
use tether_mcp::{McpClient, McpError, ToolSpec};
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, Role};
use tether_policy::{AllowAll, KillSwitch};
use tether_trace::diff::{diff, What};
use tether_trace::TraceReader;
use tether_types::{Event, Payload, RunId, ToolCall, ToolResult, Usage};

/// Counts calls, and answers with the count so a live result is recognisable.
#[derive(Default)]
struct Counter {
    calls: AtomicU64,
}

#[async_trait]
impl McpClient for Counter {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        Ok(vec![ToolSpec {
            name: "count".into(),
            server: "test".into(),
            description: "Count calls".into(),
            input_schema: json!({"type": "object"}),
            external: false,
        }])
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ToolResult {
            id: call.id.clone(),
            content: Payload::Inline(json!({"call": n})),
            is_error: false,
            duration_ms: 0,
        })
    }
}

/// A clock that moves 250 ms a read, so recorded timings are not all zero.
struct Ticking(AtomicU64);

impl Clock for Ticking {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(250, Ordering::SeqCst)
    }
}

/// The agent: a plain tool loop with a system prompt.
async fn agent(run: &mut Run, system: &str) -> Result<String, RunError> {
    let mut messages = vec![
        Message::text(Role::System, system),
        Message::text(Role::User, "Count twice."),
    ];
    loop {
        let request = CompletionRequest {
            model: "fake-1".into(),
            messages: messages.clone(),
            tools: vec![],
            params: json!({}),
        };
        let reply = run.complete(request).await?.message;
        messages.push(reply.clone());
        let mut results = Vec::new();
        let mut said = String::new();
        for block in reply.content {
            match block {
                Block::Text { text } => said.push_str(&text),
                Block::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    let result = run.call_tool(&id, &name, arguments).await?;
                    results.push(Block::ToolResult {
                        id,
                        content: json!(result.content),
                        is_error: result.is_error,
                    });
                }
                _ => {}
            }
        }
        if results.is_empty() {
            return Ok(said);
        }
        messages.push(Message {
            role: Role::Tool,
            content: results,
        });
    }
}

fn model() -> FakeAdapter {
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 10,
        ..Usage::default()
    };
    FakeAdapter::new([
        Completion::tool_use("call_1", "count", json!({"n": 1})).with_usage(usage),
        Completion::tool_use("call_2", "count", json!({"n": 2})).with_usage(usage),
        Completion::text("Counted twice.").with_usage(usage),
    ])
    .priced(0.01)
}

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn read(path: &Path) -> Vec<Event> {
    TraceReader::open(path)
        .expect("open")
        .collect::<Result<_, _>>()
        .expect("read")
}

/// Record one run of the agent and return its trace.
async fn record(dir: &Path) -> PathBuf {
    let trace = dir.join("recorded.jsonl");
    let deps = RunDeps {
        model: Arc::new(model()),
        tools: Arc::new(Counter::default()),
        policy: Arc::new(AllowAll),
        sandbox: None,
        answerer: Arc::new(Unattended),
        kill: KillSwitch::new(),
        clock: Arc::new(Ticking(AtomicU64::new(1_700_000_000_000))),
        replay: None,
    };
    let mut run = Run::start(RunConfig::new(RunId("recorded".into()), &trace), deps)
        .await
        .expect("start");
    let answer = agent(&mut run, "Be brief.").await.expect("agent");
    run.finish(json!(answer)).await.expect("finish");
    trace
}

/// Run the agent again against `recorded`, with `live` to fall back on.
async fn rerun(
    recorded: &Path,
    out: &Path,
    system: &str,
    live: Live,
) -> (Result<String, RunError>, Option<String>) {
    let recording = Recording::load(recorded).expect("load");
    let parts = recording.parts(live).expect("parts");
    let deps = RunDeps {
        model: parts.model.clone(),
        tools: parts.tools.clone(),
        // The recording had no policy file, so its decisions are played back.
        policy: parts.recorded_policy.clone(),
        sandbox: None,
        answerer: parts.answerer.clone(),
        kill: parts.kill.clone(),
        clock: Arc::new(SystemClock),
        replay: Some(parts.timeline.clone()),
    };
    let config = RunConfig {
        checkpoint_every: recording.checkpoint_every(),
        ..RunConfig::new(RunId("replayed".into()), out)
    };
    let mut run = Run::start(config, deps).await.expect("start");
    let answer = agent(&mut run, system).await;
    if let Ok(answer) = &answer {
        run.finish(json!(answer)).await.expect("finish");
    }
    (answer, parts.state.why())
}

#[tokio::test]
async fn the_same_agent_replays_to_the_same_trace_without_touching_anything() {
    let dir = dir("replay_same_agent");
    let recorded = record(&dir).await;
    let live_tools = Arc::new(Counter::default());
    let live_model = Arc::new(FakeAdapter::new([]));
    let live = Live {
        model: Some(live_model.clone()),
        tools: Some(live_tools.clone()),
        answerer: None,
    };

    let out = dir.join("replayed.jsonl");
    let (answer, diverged) = rerun(&recorded, &out, "Be brief.", live).await;

    assert_eq!(answer.expect("agent"), "Counted twice.");
    assert_eq!(diverged, None);
    // Nothing live was asked for anything.
    assert_eq!(live_tools.calls.load(Ordering::SeqCst), 0);
    assert!(live_model.requests().is_empty());

    let (before, after) = (read(&recorded), read(&out));
    assert!(diff(&before, &after).is_empty());
    // Stricter than diff: every event equal but for run id and hash, so
    // timestamps, latencies, durations, wall time and prices were restored.
    assert_eq!(before.len(), after.len());
    for (old, new) in before.iter().zip(&after) {
        let same = Event {
            run: old.run.clone(),
            hash: old.hash.clone(),
            ..new.clone()
        };
        assert_eq!(*old, same);
    }
    // And the recording did have timings worth restoring.
    assert!(serde_json::to_string(&before)
        .expect("json")
        .contains(r#""latency_ms":250"#));
}

#[tokio::test]
async fn a_changed_prompt_diverges_at_the_first_request_and_goes_live() {
    let dir = dir("replay_changed_prompt");
    let recorded = record(&dir).await;
    let live_tools = Arc::new(Counter::default());
    let live_model = Arc::new(FakeAdapter::new([
        Completion::tool_use("live_1", "count", json!({"n": 9})),
        Completion::text("Counted once, live."),
    ]));
    let live = Live {
        model: Some(live_model.clone()),
        tools: Some(live_tools.clone()),
        answerer: None,
    };

    let out = dir.join("replayed.jsonl");
    let (answer, diverged) = rerun(&recorded, &out, "Be thorough.", live).await;

    assert_eq!(answer.expect("agent"), "Counted once, live.");
    let why = diverged.expect("diverged");
    assert!(why.contains("message 1 of 2 differs"), "{why}");
    // Everything after the divergence was live.
    assert_eq!(live_model.requests().len(), 2);
    assert_eq!(live_tools.calls.load(Ordering::SeqCst), 1);

    let result = diff(&read(&recorded), &read(&out));
    let first = result.first().expect("divergence");
    assert_eq!((first.step, first.what), (1, What::ModelRequest));
}

#[tokio::test]
async fn a_changed_prompt_with_nothing_live_stops_the_run() {
    let dir = dir("replay_no_live");
    let recorded = record(&dir).await;

    let out = dir.join("replayed.jsonl");
    let (answer, diverged) = rerun(&recorded, &out, "Be thorough.", Live::default()).await;

    assert!(matches!(answer, Err(RunError::Model(_))), "{answer:?}");
    assert!(diverged.is_some());
    let events = read(&out);
    let last = serde_json::to_string(events.last().expect("events")).expect("json");
    assert!(
        last.contains("model error: replay: no recorded response"),
        "{last}"
    );
}
