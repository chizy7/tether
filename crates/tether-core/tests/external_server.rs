//! A run against an external MCP server (the fake stdio server from
//! `tether-mcp`'s fixtures), then replay with nothing live: the recorded
//! results come back and no server is started.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use serde_json::json;
use tether_core::{
    tools, Clock, Live, Recording, Run, RunConfig, RunDeps, RunError, SystemClock, Unattended,
};
use tether_mcp::ServerConfig;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, Role};
use tether_policy::{KillSwitch, RulePolicy};
use tether_trace::diff::diff;
use tether_trace::TraceReader;
use tether_types::{Decision, Event, EventType, Payload, RunId};

const FAKE_SERVER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tether-mcp/tests/fixtures/fake_server.py"
);

struct Ticking(AtomicU64);
impl Clock for Ticking {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(100, std::sync::atomic::Ordering::SeqCst)
    }
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

fn policy() -> RulePolicy {
    RulePolicy::from_toml(&format!(
        r#"
[[rule]]
name = "no-fail"
match = {{ server = "fake", tool = "fake.fail" }}
action = "deny"
reason = "fail is not allowed"

[[rule]]
name = "fake-ok"
match = {{ server = "fake" }}
action = "allow"

[[mcp_server]]
id = "fake"
transport = "stdio"
command = "python3"
args = ["{FAKE_SERVER}"]
"#
    ))
    .expect("policy")
}

fn model() -> FakeAdapter {
    FakeAdapter::new([
        Completion::tool_use("c1", "echo", json!({"text": "hello"})),
        Completion::tool_use("c2", "fail", json!({})),
        Completion::text("done"),
    ])
}

async fn agent(run: &mut Run) -> Result<String, RunError> {
    let mut messages = vec![Message::text(Role::User, "Echo then fail.")];
    loop {
        let reply = run
            .complete(CompletionRequest {
                model: "fake-1".into(),
                messages: messages.clone(),
                tools: vec![],
                params: json!({}),
            })
            .await?
            .message;
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

async fn record(dir: &Path) -> PathBuf {
    let trace = dir.join("recorded.jsonl");
    let policy = policy();
    let servers = tools::server_configs(&policy.config().mcp_servers);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0], {
        let mut c = ServerConfig::stdio("fake", "python3", vec![FAKE_SERVER.into()]);
        c.timeout_secs = 60;
        c
    });
    // No sandbox in this test: the registry holds only the external server.
    let registry = tools::connect(None, &servers).await.expect("connect");
    let deps = RunDeps {
        model: Arc::new(model()),
        tools: Arc::new(registry),
        policy: Arc::new(policy),
        sandbox: None,
        answerer: Arc::new(Unattended),
        kill: KillSwitch::new(),
        clock: Arc::new(Ticking(AtomicU64::new(1_700_000_000_000))),
        replay: None,
    };
    let mut run = Run::start(RunConfig::new(RunId("recorded".into()), &trace), deps)
        .await
        .expect("start");
    let answer = agent(&mut run).await.expect("agent");
    run.finish(json!(answer)).await.expect("finish");
    trace
}

#[tokio::test]
async fn external_calls_are_traced_with_their_server_and_decision() {
    let dir = dir("external_server_record");
    let trace = record(&dir).await;
    let events = read(&trace);

    // run.created names the server with its negotiated protocol version and
    // lists its tools as external.
    let created = events
        .iter()
        .find_map(|e| match &e.body {
            EventType::RunCreated(v) => Some(v.clone()),
            _ => None,
        })
        .expect("run.created");
    let servers = created["mcp_servers"].as_array().expect("mcp_servers");
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["id"], "fake");
    assert_eq!(servers[0]["transport"], "stdio");
    assert_eq!(servers[0]["server_name"], "fake-mcp");
    assert!(servers[0]["protocol_version"].is_string());
    let tools = created["tools"].as_array().expect("tools");
    let echo = tools
        .iter()
        .find(|t| t["name"] == "echo")
        .expect("echo listed");
    assert_eq!(echo["server"], "fake");
    assert_eq!(echo["external"], true);

    // The allowed call carries the server id and came back from the server.
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventType::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.server == "fake" && c.external));

    let decisions: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventType::PolicyDecision { decision, rule, .. } => {
                Some((decision.clone(), rule.clone()))
            }
            _ => None,
        })
        .collect();
    // model, tool, model, tool, model
    assert_eq!(decisions.len(), 5);
    assert_eq!(decisions[1].1.as_deref(), Some("fake-ok"));
    assert!(matches!(decisions[1].0, Decision::Allow));
    assert_eq!(decisions[3].1.as_deref(), Some("no-fail"));
    assert!(matches!(decisions[3].0, Decision::Deny { .. }));

    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventType::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(!results[0].is_error);
    assert_eq!(results[0].content, Payload::Inline(json!("hello")));
    assert!(results[1].is_error, "denied call is an error result");
}

#[tokio::test]
async fn replay_of_external_calls_makes_no_connection() {
    let dir = dir("external_server_replay");
    let recorded = record(&dir).await;

    // Nothing live: no model, no tools. If replay tried to reach the server
    // it would have nothing to reach.
    let recording = Recording::load(&recorded).expect("load");
    let parts = recording
        .parts(Live {
            model: None,
            tools: None,
            answerer: None,
        })
        .expect("parts");
    let out = dir.join("replayed.jsonl");
    let deps = RunDeps {
        model: parts.model.clone(),
        tools: parts.tools.clone(),
        policy: parts.recorded_policy.clone(),
        sandbox: None,
        answerer: parts.answerer.clone(),
        kill: parts.kill.clone(),
        clock: Arc::new(SystemClock),
        replay: Some(parts.timeline.clone()),
    };
    let config = RunConfig {
        checkpoint_every: recording.checkpoint_every(),
        ..RunConfig::new(RunId("replayed".into()), &out)
    };
    let mut run = Run::start(config, deps).await.expect("start");
    let answer = agent(&mut run).await.expect("agent replays");
    run.finish(json!(answer)).await.expect("finish");

    assert_eq!(parts.state.why(), None, "no divergence");
    let (before, after) = (read(&recorded), read(&out));
    assert!(diff(&before, &after).is_empty(), "replay diff is empty");
    // The replayed trace still says which server each call belonged to.
    let replayed_call = after
        .iter()
        .find_map(|e| match &e.body {
            EventType::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a tool call");
    assert_eq!(replayed_call.server, "fake");
    assert!(replayed_call.external);
    // With `Live { tools: None }` the replay runtime has nothing to dispatch
    // to, so a successful run with an empty diff is the proof that every
    // external result came from the recording.
}
