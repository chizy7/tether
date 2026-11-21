use serde_json::{json, Value};
use tether_trace::diff::{diff, Counts, What};
use tether_types::{
    BudgetSnapshot, Decision, Event, EventType, ModelRequest, ModelResponse, Payload, RunId,
    RunState, ToolCall, ToolResult, Usage, SCHEMA_VERSION,
};
use time::OffsetDateTime;

/// Builds a trace event by event. `noise` is added to everything a diff
/// must not look at: timestamps, timings, ids, budgets.
struct Trace {
    events: Vec<Event>,
    step: u64,
    noise: u64,
}

impl Trace {
    fn new(noise: u64) -> Self {
        let mut trace = Self {
            events: Vec::new(),
            step: 0,
            noise,
        };
        trace.push(EventType::RunCreated(json!({"policy": "[[rule]]\n"})));
        trace.push(state(RunState::Running, "start"));
        trace
    }

    fn push(&mut self, body: EventType) -> u64 {
        let seq = self.events.len() as u64;
        self.events.push(Event {
            v: SCHEMA_VERSION,
            run: RunId(format!("run_{}", self.noise)),
            seq,
            ts: OffsetDateTime::from_unix_timestamp(
                1_700_000_000 + (self.noise * 1000 + seq) as i64,
            )
            .expect("ts"),
            step: self.step,
            agent: "main".into(),
            parent: None,
            hash: format!("blake3:{}{seq}", self.noise),
            body,
        });
        seq
    }

    fn decision(&mut self, subject: u64, decision: Decision, rule: Option<&str>) {
        self.push(EventType::PolicyDecision {
            subject,
            rule: rule.map(str::to_owned),
            decision,
            budget: BudgetSnapshot {
                wall_secs_used: self.noise,
                ..BudgetSnapshot::default()
            },
        });
    }

    fn tick(&mut self) {
        self.push(EventType::BudgetTick {
            budget: BudgetSnapshot {
                wall_secs_used: self.noise,
                ..BudgetSnapshot::default()
            },
            price: None,
        });
    }

    /// A model call that opens the next step.
    fn model(&mut self, messages: &[&str]) -> &mut Self {
        self.step += 1;
        let asked = self.push(EventType::ModelRequest(ModelRequest {
            provider: "fake".into(),
            model: "fake-1".into(),
            messages: messages.iter().map(|m| format!("blake3:{m}")).collect(),
            tools: vec!["exec".into()],
            params: json!({}),
        }));
        self.decision(asked, Decision::Allow, None);
        self.push(EventType::ModelResponse(ModelResponse {
            message: format!("blake3:reply{}", self.noise),
            tool_calls: vec![],
            usage: Usage::default(),
            stop_reason: None,
            latency_ms: 100 + self.noise,
            ttft_ms: Some(self.noise),
            stream: true,
            provider_request_id: Some(format!("req_{}", self.noise)),
            raw: None,
        }));
        self.tick();
        self
    }

    /// A tool call, decided by `decision`.
    fn tool(
        &mut self,
        name: &str,
        arguments: Value,
        decision: Decision,
        rule: Option<&str>,
    ) -> &mut Self {
        let id = format!("toolu_{}_{}", self.noise, self.events.len());
        let asked = self.push(EventType::ToolCall(ToolCall {
            id: id.clone(),
            name: name.into(),
            server: "sandbox".into(),
            arguments,
            external: false,
        }));
        let denied = matches!(decision, Decision::Deny { .. });
        self.decision(asked, decision, rule);
        self.push(EventType::ToolResult(ToolResult {
            id,
            content: Payload::Inline(json!("out")),
            is_error: denied,
            duration_ms: self.noise,
        }));
        self.tick();
        self
    }

    fn allowed(&mut self, name: &str, arguments: Value) -> &mut Self {
        self.tool(name, arguments, Decision::Allow, Some("files"))
    }

    fn finish(&mut self, outcome: &str) -> Vec<Event> {
        self.push(state(RunState::Finished, "agent done"));
        self.push(EventType::RunFinished(
            json!({"outcome": outcome, "steps": self.step}),
        ));
        self.events.clone()
    }
}

fn state(state: RunState, reason: &str) -> EventType {
    EventType::RunStateChanged {
        state,
        reason: reason.into(),
    }
}

/// Two steps: write a file, then read it, then finish.
fn run(noise: u64) -> Trace {
    let mut trace = Trace::new(noise);
    trace
        .model(&["sys", "task"])
        .allowed("write_file", json!({"path": "a.txt", "content": "hi"}));
    trace
        .model(&["sys", "task", "reply1", "result1"])
        .allowed("read_file", json!({"path": "a.txt"}));
    trace
}

#[test]
fn runs_that_did_the_same_things_do_not_diverge() {
    // Different run ids, timestamps, latencies, tool ids, request ids, hashes
    // and wall time. None of it is what the run did.
    let a = run(1).finish("done");
    let b = run(2).finish("done");
    let result = diff(&a, &b);

    assert!(result.is_empty(), "{:?}", result.divergences);
    assert_eq!(result.first(), None);
    assert!(!result.policy_differs);

    let steps: Vec<_> = result
        .steps
        .iter()
        .map(|s| (s.step, s.divergences))
        .collect();
    assert_eq!(steps, [(1, 0), (2, 0)]);
    let counts = Counts {
        model_requests: 1,
        tool_calls: 1,
        decisions: 2,
    };
    assert_eq!(
        (result.steps[0].a, result.steps[0].b),
        (Some(counts), Some(counts))
    );
}

#[test]
fn different_tool_arguments_are_the_first_divergence() {
    let a = run(1).finish("done");
    let mut other = Trace::new(2);
    other
        .model(&["sys", "task"])
        .allowed("write_file", json!({"path": "a.txt", "content": "hi"}));
    other
        .model(&["sys", "task", "reply1", "result1"])
        .allowed("read_file", json!({"path": "b.txt"}));
    let b = other.finish("done");

    let result = diff(&a, &b);
    let first = result.first().expect("a divergence");
    assert_eq!((first.step, first.what), (2, What::ToolCall));
    assert_eq!(first.detail, "the arguments differ");
    let (in_a, in_b) = (first.a.as_ref().expect("a"), first.b.as_ref().expect("b"));
    assert_eq!(in_a.summary, r#"read_file {"path":"a.txt"}"#);
    assert_eq!(in_b.summary, r#"read_file {"path":"b.txt"}"#);
    assert_eq!(in_a.seq, in_b.seq);
    assert_eq!(result.divergences.len(), 1);
    assert_eq!(result.steps[1].divergences, 1);
}

#[test]
fn a_different_tool_and_a_changed_message_are_named() {
    let a = run(1).finish("done");

    let mut other_tool = Trace::new(2);
    other_tool
        .model(&["sys", "task"])
        .allowed("exec", json!({"argv": ["touch", "a.txt"]}));
    let first = diff(&a, &other_tool.finish("done"))
        .first()
        .cloned()
        .expect("divergence");
    assert_eq!((first.step, first.what), (1, What::ToolCall));
    assert_eq!(first.detail, "a different tool is called");

    // The system prompt changed: the very first request differs.
    let mut new_prompt = Trace::new(3);
    new_prompt
        .model(&["sys2", "task"])
        .allowed("write_file", json!({"path": "a.txt", "content": "hi"}));
    new_prompt
        .model(&["sys2", "task", "reply1", "result1"])
        .allowed("read_file", json!({"path": "a.txt"}));
    let result = diff(&a, &new_prompt.finish("done"));
    let first = result.first().expect("divergence");
    assert_eq!((first.step, first.what), (1, What::ModelRequest));
    assert_eq!(first.detail, "message 1 differs (2 messages in a, 2 in b)");
    assert_eq!(
        first.messages,
        Some((Some("blake3:sys".into()), Some("blake3:sys2".into())))
    );
    // Both requests differ; the tool calls and decisions still agree.
    assert_eq!(result.divergences.len(), 2);

    // A longer history on one side: the first message past the shared part.
    let mut longer = Trace::new(4);
    longer
        .model(&["sys", "task", "extra"])
        .allowed("write_file", json!({"path": "a.txt", "content": "hi"}));
    let first = diff(&a, &longer.finish("done"))
        .first()
        .cloned()
        .expect("divergence");
    assert_eq!(first.detail, "message 3 differs (2 messages in a, 3 in b)");
    assert_eq!(first.messages, Some((None, Some("blake3:extra".into()))));
}

#[test]
fn a_changed_decision_diverges_but_a_changed_rule_name_does_not() {
    let rm = json!({"argv": ["rm", "-rf", "."]});
    let with = |decision: Decision, rule: &str, noise: u64| {
        let mut trace = Trace::new(noise);
        trace
            .model(&["sys", "task"])
            .tool("exec", rm.clone(), decision, Some(rule));
        trace.finish("done")
    };
    let deny = || Decision::Deny {
        reason: "off limits".into(),
    };

    // Same outcome reached through a rule with another name.
    assert!(diff(
        &with(deny(), "no-rm", 1),
        &with(deny(), "nothing-destructive", 2)
    )
    .is_empty());

    let result = diff(
        &with(deny(), "no-rm", 1),
        &with(Decision::Allow, "anything", 2),
    );
    let first = result.first().expect("divergence");
    assert_eq!((first.step, first.what), (1, What::PolicyDecision));
    assert_eq!(first.detail, "the outcome differs");
    assert_eq!(
        first.a.as_ref().expect("a").summary,
        "deny (rule no-rm: off limits)"
    );
    assert_eq!(
        first.b.as_ref().expect("b").summary,
        "allow (rule anything)"
    );
}

#[test]
fn an_ask_answered_differently_diverges() {
    let asked = |allow: bool, noise: u64| {
        let mut trace = Trace::new(noise);
        trace.model(&["sys", "task"]);
        let call = trace.push(EventType::ToolCall(ToolCall {
            id: "toolu".into(),
            name: "exec".into(),
            server: "sandbox".into(),
            arguments: json!({"argv": ["rm", "x"]}),
            external: false,
        }));
        let ask = Decision::Ask {
            prompt: "Allow?".into(),
        };
        trace.decision(call, ask, Some("risky"));
        trace.push(EventType::PolicyAsk {
            subject: call,
            prompt: "Allow?".into(),
        });
        trace.push(state(RunState::Suspended, "policy.ask"));
        trace.push(EventType::PolicyAnswer {
            subject: call,
            answered_by: "cli".into(),
            allow,
        });
        trace.push(state(RunState::Running, "ask answered"));
        trace.finish("done")
    };

    assert!(diff(&asked(true, 1), &asked(true, 2)).is_empty());

    let result = diff(&asked(true, 1), &asked(false, 2));
    let first = result.first().expect("divergence");
    assert_eq!((first.step, first.what), (1, What::PolicyDecision));
    assert_eq!(
        first.a.as_ref().expect("a").summary,
        "ask, answered allow (rule risky)"
    );
    assert_eq!(
        first.b.as_ref().expect("b").summary,
        "ask, answered deny (rule risky)"
    );
}

#[test]
fn a_run_that_stops_early_or_ends_differently_diverges() {
    let full = run(1).finish("done");

    // One step fewer.
    let mut short = Trace::new(2);
    short
        .model(&["sys", "task"])
        .allowed("write_file", json!({"path": "a.txt", "content": "hi"}));
    let result = diff(&full, &short.finish("done"));
    assert_eq!(result.steps[1].b, None);
    assert!(result.steps[1].a.is_some());
    let first = result.first().expect("divergence");
    assert_eq!((first.step, first.what), (2, What::ModelRequest));
    assert_eq!(
        (first.detail.as_str(), first.b.as_ref()),
        ("only a has this", None)
    );

    // Same steps, different last word.
    let result = diff(&full, &run(2).finish("gave up"));
    let first = result.first().expect("divergence");
    assert_eq!(first.what, What::End);
    assert_eq!(first.detail, "the runs finished with different outcomes");
    assert_eq!(first.a.as_ref().expect("a").summary, r#"outcome "done""#);

    // Same steps, one halted.
    let mut halted = run(2);
    halted.push(state(
        RunState::Halted,
        "budget exceeded: max_tool_calls (2 of 2)",
    ));
    let result = diff(&full, &halted.events);
    let first = result.first().expect("divergence");
    assert_eq!(first.detail, "the runs ended in different states");
    assert_eq!(
        first.b.as_ref().expect("b").summary,
        "halted: budget exceeded: max_tool_calls (2 of 2)"
    );
}

#[test]
fn a_different_policy_is_noted_without_being_a_divergence() {
    let a = run(1).finish("done");
    let mut b = run(2).finish("done");
    b[0].body = EventType::RunCreated(json!({"policy": "[[rule]]\nname = \"x\"\n"}));

    let result = diff(&a, &b);
    assert!(result.policy_differs);
    assert!(result.is_empty());
}

#[test]
fn a_trace_does_not_diverge_from_itself() {
    let a = run(1).finish("done");
    assert!(diff(&a, &a).is_empty());
    assert!(diff(&[], &[]).is_empty());
}
