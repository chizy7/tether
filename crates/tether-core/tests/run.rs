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

/// Advances a fixed amount on every read, 7 ms unless told otherwise, so
/// traces are identical from run to run.
struct StepClock {
    now: AtomicU64,
    step_ms: u64,
}

impl StepClock {
    fn new() -> Arc<Self> {
        Self::every(7)
    }

    fn every(step_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicU64::new(1_700_000_000_000),
            step_ms,
        })
    }
}

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.now.fetch_add(self.step_ms, Ordering::SeqCst)
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
    start_with(path, Arc::new(script()), tools, policy, StepClock::new()).await
}

async fn start_with(
    path: &Path,
    model: Arc<FakeAdapter>,
    tools: Arc<EchoTools>,
    policy: Arc<dyn Policy>,
    clock: Arc<StepClock>,
) -> Result<Run, RunError> {
    let config = RunConfig {
        run: RunId("run_test".into()),
        agent: "main".into(),
        trace: path.to_path_buf(),
    };
    let deps = RunDeps {
        model,
        tools,
        policy,
        clock,
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
            "budget.tick",
            "tool.call",
            "policy.decision",
            "tool.result",
            "budget.tick",
            "model.request",
            "policy.decision",
            "model.response",
            "budget.tick",
            "run.state",
            "run.finished",
        ]
    );
    // Steps: setup is 0, each model request opens a new one.
    let steps: Vec<u64> = events.iter().map(|e| e.step).collect();
    assert_eq!(steps, [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2]);
    // Answers point at what they answer.
    assert_eq!(events[4].parent, Some(2));
    assert_eq!(events[5].parent, Some(2));
    assert_eq!(events[7].parent, Some(6));
    assert_eq!(events[8].parent, Some(6));
    assert_eq!(events[9].parent, Some(6));

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
    } = &events[7].body
    else {
        panic!("not a policy.decision")
    };
    assert_eq!((*subject, decision), (6, &Decision::Allow));
    assert_eq!(budget.tokens_used, 15);

    // The tick after the tool call counts it and the tokens spent so far.
    let EventType::BudgetTick(tick) = &events[9].body else {
        panic!("not a budget.tick")
    };
    assert_eq!((tick.tokens_used, tick.tool_calls_used), (15, 1));
    assert_eq!(tick.tool_calls_limit, None);

    // Chain holds and every message blob is present: 2 requests + 2 responses.
    let report = verify(&path).expect("verify");
    assert_eq!((report.events, report.blobs), (16, 4));
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
    assert_eq!(type_of(&events[6]), "tool.call");
    let EventType::PolicyDecision { decision, .. } = &events[7].body else {
        panic!("not a policy.decision")
    };
    assert!(matches!(decision, Decision::Deny { .. }));
    assert!(matches!(&events[8].body, EventType::ToolResult(r) if r.is_error));
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

    let tail: Vec<String> = read(&path).iter().skip(6).map(type_of).collect();
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

fn budget(limits: &str) -> Arc<dyn Policy> {
    let text = format!("[budget]\n{limits}\n\n[[rule]]\nname = \"all\"\naction = \"allow\"\n");
    Arc::new(RulePolicy::from_toml(&text).expect("policy"))
}

/// The reason on the `run.state` that halted the run, and what came before it.
fn halt_of(path: &Path) -> (String, String) {
    let events = read(path);
    let last = events.last().expect("events");
    let EventType::RunStateChanged {
        state: RunState::Halted,
        reason,
    } = &last.body
    else {
        panic!("trace does not end in a halt: {last:?}")
    };
    (reason.clone(), type_of(&events[events.len() - 2]))
}

#[tokio::test]
async fn tool_call_budget_allows_exactly_its_limit() {
    let path = trace_path("budget_tool_calls");
    let tools = Arc::new(EchoTools::default());
    let mut run = start(&path, tools.clone(), budget("max_tool_calls = 2"))
        .await
        .expect("start");
    run.complete(request("go")).await.expect("completion");
    for id in ["call_1", "call_2"] {
        run.call_tool(id, "sandbox.echo", json!({}))
            .await
            .expect("within budget");
    }

    let err = run
        .call_tool("call_3", "sandbox.echo", json!({}))
        .await
        .expect_err("third call is over budget");
    assert!(
        matches!(&err, RunError::Halted { reason } if reason == "budget exceeded: max_tool_calls (2 of 2)"),
        "{err}"
    );
    assert_eq!(run.state(), RunState::Halted);
    assert_eq!(tools.calls.load(Ordering::SeqCst), 2);
    drop(run);

    // The refused call left no events: the halt follows the last tick.
    let (reason, before) = halt_of(&path);
    assert_eq!(reason, "budget exceeded: max_tool_calls (2 of 2)");
    assert_eq!(before, "budget.tick");
    verify(&path).expect("verify");
}

#[tokio::test]
async fn token_budget_stops_the_next_model_call() {
    let path = trace_path("budget_tokens");
    let model = Arc::new(script());
    // The first scripted completion uses 15 tokens.
    let mut run = start_with(
        &path,
        model.clone(),
        Arc::new(EchoTools::default()),
        budget("max_tokens = 15"),
        StepClock::new(),
    )
    .await
    .expect("start");
    run.complete(request("one"))
        .await
        .expect("first completion");

    let err = run.complete(request("two")).await.expect_err("over budget");
    assert!(
        matches!(&err, RunError::Halted { reason } if reason.contains("max_tokens (15 of 15)")),
        "{err}"
    );
    // The model was not asked again, and the step did not advance.
    assert_eq!(model.remaining(), 1);
    assert_eq!(run.step(), 1);

    // A spent token budget stops tool calls too, but the run is already halted.
    assert!(matches!(
        run.call_tool("call_1", "sandbox.echo", json!({})).await,
        Err(RunError::NotRunning(RunState::Halted))
    ));
}

#[tokio::test]
async fn dollar_budget_names_max_usd() {
    let path = trace_path("budget_usd");
    let model = Arc::new(
        FakeAdapter::new([
            Completion::text("a"),
            Completion::text("b"),
            Completion::text("c"),
        ])
        .priced(0.5),
    );
    let mut run = start_with(
        &path,
        model.clone(),
        Arc::new(EchoTools::default()),
        budget("max_usd = 1.0"),
        StepClock::new(),
    )
    .await
    .expect("start");
    run.complete(request("one")).await.expect("first");
    run.complete(request("two")).await.expect("second");

    let err = run
        .complete(request("three"))
        .await
        .expect_err("over budget");
    assert!(
        matches!(&err, RunError::Halted { reason } if reason.contains("max_usd (1 of 1)")),
        "{err}"
    );
    assert_eq!(model.remaining(), 1);
}

#[tokio::test]
async fn wall_clock_budget_is_checked_at_the_next_call() {
    let path = trace_path("budget_wall");
    // Every clock read moves time on by a second.
    let mut run = start_with(
        &path,
        Arc::new(script()),
        Arc::new(EchoTools::default()),
        budget("max_wall_secs = 10"),
        StepClock::every(1000),
    )
    .await
    .expect("start");
    run.complete(request("one"))
        .await
        .expect("first completion");

    let err = run
        .call_tool("call_1", "sandbox.echo", json!({}))
        .await
        .expect_err("out of time");
    assert!(
        matches!(&err, RunError::Halted { reason } if reason.contains("max_wall_secs")),
        "{err}"
    );
    drop(run);
    assert!(halt_of(&path)
        .0
        .starts_with("budget exceeded: max_wall_secs ("));
}

#[tokio::test]
async fn ticks_and_decisions_carry_the_limits() {
    let path = trace_path("budget_limits_recorded");
    let policy =
        budget("max_tokens = 1000\nmax_usd = 5.0\nmax_wall_secs = 600\nmax_tool_calls = 10");
    one_tool_run(&path, Arc::new(EchoTools::default()), policy).await;

    let events = read(&path);
    let ticks: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventType::BudgetTick(tick) => Some(*tick),
            _ => None,
        })
        .collect();
    // One per model call and one per tool call.
    assert_eq!(ticks.len(), 3);
    let last = ticks[2];
    assert_eq!((last.tokens_used, last.tokens_limit), (15, Some(1000)));
    assert_eq!((last.tool_calls_used, last.tool_calls_limit), (1, Some(10)));
    assert_eq!(
        (last.usd_limit, last.wall_secs_limit),
        (Some(5.0), Some(600))
    );

    let EventType::PolicyDecision { budget, .. } = &events[7].body else {
        panic!("not a policy.decision")
    };
    assert_eq!(budget.tool_calls_limit, Some(10));
}
