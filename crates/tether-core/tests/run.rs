use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tether_core::{Clock, Run, RunConfig, RunDeps, RunError};
use tether_mcp::{McpClient, McpError, ToolSpec};
use tether_models::fake::FakeAdapter;
use tether_models::{Completion, CompletionRequest, Message, Role};
use tether_policy::{AllowAll, Policy, RulePolicy, RunCtx, Verdict};
use tether_trace::{verify, TraceReader};
use tether_types::{
    Decision, Event, EventType, Payload, RunId, RunState, ToolCall, ToolResult, Usage,
};

/// Advances 7 ms on every read, so traces are identical from run to run.
struct StepClock(AtomicU64);

impl StepClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(1_700_000_000_000)))
    }
}

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(7, Ordering::SeqCst)
    }
}

/// One tool, `sandbox.echo`, that returns its arguments and counts calls.
#[derive(Default)]
struct EchoTools {
    calls: AtomicU64,
}

#[async_trait]
impl McpClient for EchoTools {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        Ok(vec![ToolSpec {
            name: "sandbox.echo".into(),
            server: "sandbox".into(),
            description: "Return the arguments".into(),
            input_schema: json!({"type": "object"}),
            external: false,
        }])
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            id: call.id.clone(),
            content: Payload::Inline(call.arguments.clone()),
            is_error: false,
            duration_ms: 0,
        })
    }
}

struct DenyTools;

impl Policy for DenyTools {
    fn decide(&self, _call: &ToolCall, _ctx: &RunCtx) -> Verdict {
        Decision::Deny {
            reason: "no tools in this run".into(),
        }
        .into()
    }
}

struct AskTools;

impl Policy for AskTools {
    fn decide(&self, _call: &ToolCall, _ctx: &RunCtx) -> Verdict {
        Decision::Ask {
            prompt: "run echo?".into(),
        }
        .into()
    }
}

fn trace_path(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir.join("run.jsonl")
}

fn request(text: &str) -> CompletionRequest {
    CompletionRequest {
        model: "fake-1".into(),
        messages: vec![Message::text(Role::User, text)],
        tools: vec![],
        params: json!({}),
    }
}

fn script() -> FakeAdapter {
    FakeAdapter::new([
        Completion::tool_use("call_1", "sandbox.echo", json!({"say": "hi"})).with_usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        }),
        Completion::text("done"),
    ])
}

async fn start(
    path: &Path,
    tools: Arc<EchoTools>,
    policy: Arc<dyn Policy>,
) -> Result<Run, RunError> {
    let config = RunConfig {
        run: RunId("run_test".into()),
        agent: "main".into(),
        trace: path.to_path_buf(),
    };
    let deps = RunDeps {
        model: Arc::new(script()),
        tools,
        policy,
        clock: StepClock::new(),
    };
    Run::start(config, deps).await
}

/// Model asks for a tool, the agent calls it, the model wraps up.
async fn one_tool_run(path: &Path, tools: Arc<EchoTools>, policy: Arc<dyn Policy>) -> ToolResult {
    let mut run = start(path, tools, policy).await.expect("start");
    run.complete(request("say hi"))
        .await
        .expect("first completion");
    let result = run
        .call_tool("call_1", "sandbox.echo", json!({"say": "hi"}))
        .await
        .expect("tool call");
    run.complete(request("thanks"))
        .await
        .expect("second completion");
    run.finish(json!("ok")).await.expect("finish");
    result
}

fn read(path: &Path) -> Vec<Event> {
    TraceReader::open(path)
        .expect("open")
        .collect::<Result<_, _>>()
        .expect("read")
}

fn type_of(event: &Event) -> String {
    let value = serde_json::to_value(event).expect("serialize");
    value["type"].as_str().expect("type").to_owned()
}

#[tokio::test]
async fn one_step_leaves_the_events_the_spec_promises() {
    let path = trace_path("one_step");
    let tools = Arc::new(EchoTools::default());
    let result = one_tool_run(&path, tools.clone(), Arc::new(AllowAll)).await;

    assert!(!result.is_error);
    assert_eq!(result.content, Payload::Inline(json!({"say": "hi"})));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);

    let events = read(&path);
    let types: Vec<String> = events.iter().map(type_of).collect();
    assert_eq!(
        types,
        [
            "run.created",
            "run.state",
            "model.request",
            "policy.decision",
            "model.response",
            "tool.call",
            "policy.decision",
            "tool.result",
            "model.request",
            "policy.decision",
            "model.response",
            "run.state",
            "run.finished",
        ]
    );
    // Steps: setup is 0, each model request opens a new one.
    let steps: Vec<u64> = events.iter().map(|e| e.step).collect();
    assert_eq!(steps, [0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2]);
    // Answers point at what they answer.
    assert_eq!(events[4].parent, Some(2));
    assert_eq!(events[6].parent, Some(5));
    assert_eq!(events[7].parent, Some(5));

    let EventType::ModelResponse(response) = &events[4].body else {
        panic!("not a model.response")
    };
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].server, "sandbox");

    let EventType::PolicyDecision {
        subject,
        decision,
        budget,
        ..
    } = &events[6].body
    else {
        panic!("not a policy.decision")
    };
    assert_eq!((*subject, decision), (5, &Decision::Allow));
    assert_eq!(budget.tokens_used, 15);

    // Chain holds and every message blob is present: 2 requests + 2 responses.
    let report = verify(&path).expect("verify");
    assert_eq!((report.events, report.blobs), (13, 4));
}

#[tokio::test]
async fn same_inputs_and_clock_give_the_same_trace_bytes() {
    let a = trace_path("deterministic_a");
    let b = trace_path("deterministic_b");
    one_tool_run(&a, Arc::new(EchoTools::default()), Arc::new(AllowAll)).await;
    one_tool_run(&b, Arc::new(EchoTools::default()), Arc::new(AllowAll)).await;
    assert_eq!(fs::read(&a).expect("read a"), fs::read(&b).expect("read b"));
}

#[tokio::test]
async fn denied_tool_call_is_recorded_and_never_dispatched() {
    let path = trace_path("deny");
    let tools = Arc::new(EchoTools::default());
    let result = one_tool_run(&path, tools.clone(), Arc::new(DenyTools)).await;

    assert!(result.is_error);
    assert_eq!(
        result.content,
        Payload::Inline(json!("denied by policy: no tools in this run"))
    );
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);

    let events = read(&path);
    assert_eq!(type_of(&events[5]), "tool.call");
    let EventType::PolicyDecision { decision, .. } = &events[6].body else {
        panic!("not a policy.decision")
    };
    assert!(matches!(decision, Decision::Deny { .. }));
    assert!(matches!(&events[7].body, EventType::ToolResult(r) if r.is_error));
    verify(&path).expect("verify");
}

#[tokio::test]
async fn unknown_tool_fails_like_a_normal_call() {
    let path = trace_path("unknown_tool");
    let tools = Arc::new(EchoTools::default());
    let mut run = start(&path, tools.clone(), Arc::new(AllowAll))
        .await
        .expect("start");
    run.complete(request("say hi")).await.expect("completion");

    let result = run
        .call_tool("call_9", "net.fetch", json!({}))
        .await
        .expect("tool call");
    assert!(result.is_error);
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
    assert_eq!(run.state(), RunState::Running);
}

#[tokio::test]
async fn ask_suspends_then_halts_the_run() {
    let path = trace_path("ask");
    let tools = Arc::new(EchoTools::default());
    let mut run = start(&path, tools.clone(), Arc::new(AskTools))
        .await
        .expect("start");
    run.complete(request("say hi")).await.expect("completion");

    let err = run
        .call_tool("call_1", "sandbox.echo", json!({}))
        .await
        .expect_err("must halt");
    assert!(matches!(err, RunError::Halted { .. }));
    assert_eq!(run.state(), RunState::Halted);
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        run.complete(request("again")).await,
        Err(RunError::NotRunning(RunState::Halted))
    ));
    drop(run);

    let tail: Vec<String> = read(&path).iter().skip(5).map(type_of).collect();
    assert_eq!(
        tail,
        [
            "tool.call",
            "policy.decision",
            "policy.ask",
            "run.state",
            "run.state"
        ]
    );
}

#[tokio::test]
async fn model_error_halts_the_run() {
    let path = trace_path("model_error");
    let mut run = start(&path, Arc::new(EchoTools::default()), Arc::new(AllowAll))
        .await
        .expect("start");
    run.complete(request("one")).await.expect("first");
    run.complete(request("two")).await.expect("second");

    let err = run
        .complete(request("three"))
        .await
        .expect_err("script is exhausted");
    assert!(matches!(err, RunError::Model(_)));
    assert_eq!(run.state(), RunState::Halted);
}

const NO_ECHO: &str = r#"
# Comments are part of the snapshot too.
[[rule]]
name = "no-echo"
match = { tool = "sandbox.echo" }
action = "deny"
reason = "echo is off limits"
"#;

#[tokio::test]
async fn matched_rule_name_is_recorded_in_the_decision() {
    let path = trace_path("rule_name");
    let tools = Arc::new(EchoTools::default());
    let policy = RulePolicy::from_toml(NO_ECHO).expect("policy");
    let result = one_tool_run(&path, tools.clone(), Arc::new(policy)).await;

    assert!(result.is_error);
    assert_eq!(
        result.content,
        Payload::Inline(json!("denied by policy: echo is off limits"))
    );
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);

    let events = read(&path);
    let rules: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventType::PolicyDecision { rule, .. } => Some(rule.clone()),
            _ => None,
        })
        .collect();
    // Model calls match no rule; the tool call names the rule that denied it.
    assert_eq!(rules, [None, Some("no-echo".to_owned()), None]);
}

#[tokio::test]
async fn run_created_holds_the_policy_the_run_was_held_to() {
    let path = trace_path("policy_snapshot");
    let policy = RulePolicy::from_toml(NO_ECHO).expect("policy");
    one_tool_run(&path, Arc::new(EchoTools::default()), Arc::new(policy)).await;

    let EventType::RunCreated(created) = &read(&path)[0].body else {
        panic!("not run.created")
    };
    assert_eq!(created["policy"], json!(NO_ECHO));

    // The recorded text is enough to rebuild the policy and get the same rules.
    let recorded = created["policy"].as_str().expect("policy text");
    let rebuilt = RulePolicy::from_toml(recorded).expect("rebuild");
    assert_eq!(rebuilt.config().rules[0].name, "no-echo");
}

#[tokio::test]
async fn a_policy_with_no_file_snapshots_as_null() {
    let path = trace_path("policy_snapshot_null");
    one_tool_run(&path, Arc::new(EchoTools::default()), Arc::new(AllowAll)).await;

    let EventType::RunCreated(created) = &read(&path)[0].body else {
        panic!("not run.created")
    };
    assert!(created["policy"].is_null());
}
