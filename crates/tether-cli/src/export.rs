//! `tether export otlp <run>`: a trace as OpenTelemetry spans.
//!
//! One OpenTelemetry trace per run. The run is the root span, each step is a
//! child of it, and within a step each model call, tool call and policy
//! decision is a span. Everything comes from the trace file: this works on
//! any trace after the fact, and on a live one by following the file as it
//! grows, so the run loop knows nothing about it.
//!
//! Spans are sent as OTLP over HTTP in the JSON encoding, which collectors
//! and most backends accept on port 4318. Messages, tool arguments and tool
//! results are not exported: what happened and how long it took are. The
//! few pieces of text that are sent are the names of models, tools and
//! rules, the reason for a refusal or a halt, and the first words of the
//! run's outcome.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tether_trace::blob::hash_bytes;
use tether_trace::resumed::is_resume;
use tether_trace::{TraceFollower, TraceReader};
use tether_types::{Decision, Event, EventType, RunState};

use crate::{clip, pack};

pub struct Options {
    pub run: PathBuf,
    /// Keep reading the trace as it is written, until its run ends.
    pub follow: bool,
    /// Where to send spans: a collector's base address.
    pub endpoint: Option<String>,
    /// Write the requests to this file, one JSON document per line, in
    /// place of sending them.
    pub out: Option<PathBuf>,
}

/// Span kinds and status codes, as OTLP numbers them.
const KIND_INTERNAL: u8 = 1;
const KIND_CLIENT: u8 = 3;
const STATUS_ERROR: u8 = 2;

/// The most spans in one request.
const BATCH: usize = 500;

type Attributes = Vec<(String, Value)>;

#[derive(Debug, Clone, PartialEq)]
struct Span {
    span_id: String,
    parent: Option<String>,
    name: String,
    kind: u8,
    start_ns: i128,
    end_ns: i128,
    attributes: Attributes,
    /// Things that happened during the span: a name, a time, attributes.
    events: Vec<(String, i128, Attributes)>,
    /// Set when the span is an error, with why.
    error: Option<String>,
}

impl Span {
    fn new(span_id: String, parent: Option<String>, name: String, kind: u8, at: i128) -> Self {
        Self {
            span_id,
            parent,
            name,
            kind,
            start_ns: at,
            end_ns: at,
            attributes: Vec::new(),
            events: Vec::new(),
            error: None,
        }
    }

    fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.attributes.push((key.to_owned(), value.into()));
    }

    fn ended(mut self, at: i128) -> Self {
        self.end_ns = at.max(self.start_ns);
        self
    }
}

/// Turns the events of one run into spans, one event at a time, so that a
/// trace can be exported while it is being written.
struct Spans {
    trace_id: String,
    run_id: String,
    root: Option<Span>,
    /// The step under way, with its number.
    step: Option<(u64, Span)>,
    /// How many step spans have been opened, to tell a repeated step apart.
    steps_opened: u64,
    /// Model and tool calls not yet answered, by the `seq` of the request.
    calls: BTreeMap<u64, Span>,
    /// Asks not yet answered, by the `seq` of what they are about.
    asks: BTreeMap<u64, Span>,
    /// Answered model calls waiting for the `budget.tick` that prices them.
    unpriced: BTreeMap<u64, Span>,
    last_ns: i128,
    /// Whether the run is over as far as the trace says.
    ended: bool,
}

impl Spans {
    fn new() -> Self {
        Self {
            trace_id: String::new(),
            run_id: String::new(),
            root: None,
            step: None,
            steps_opened: 0,
            calls: BTreeMap::new(),
            asks: BTreeMap::new(),
            unpriced: BTreeMap::new(),
            last_ns: 0,
            ended: false,
        }
    }

    /// An id that is the same every time this trace is exported.
    fn id(&self, what: &str, n: u64) -> String {
        let hash = hash_bytes(format!("{}:{what}:{n}", self.run_id).as_bytes());
        hash.trim_start_matches("blake3:")[..16].to_owned()
    }

    /// The span a new one belongs under: the step, or the run outside one.
    fn parent(&self) -> Option<String> {
        self.step
            .as_ref()
            .map(|(_, step)| &step.span_id)
            .or(self.root.as_ref().map(|root| &root.span_id))
            .cloned()
    }

    fn close_step(&mut self, done: &mut Vec<Span>) {
        if let Some((_, step)) = self.step.take() {
            done.push(step.ended(self.last_ns));
        }
    }

    /// Take the next event and return the spans it completed.
    fn feed(&mut self, event: &Event) -> Vec<Span> {
        let mut done = Vec::new();
        let at = event.ts.unix_timestamp_nanos();

        if self.root.is_none() {
            self.run_id.clone_from(&event.run.0);
            let hash = hash_bytes(self.run_id.as_bytes());
            self.trace_id = hash.trim_start_matches("blake3:")[..32].to_owned();
            let mut root = Span::new(
                self.id("run", 0),
                None,
                "tether.run".into(),
                KIND_INTERNAL,
                at,
            );
            root.set("tether.run.id", self.run_id.as_str());
            root.set("tether.agent", event.agent.as_str());
            self.root = Some(root);
        }

        // A resume goes back to a checkpoint and does a step again: the step
        // that was under way is over, and the next event starts a new one.
        if is_resume(event) {
            self.close_step(&mut done);
        } else if event.step > 0 && self.step.as_ref().map(|(n, _)| *n) != Some(event.step) {
            self.close_step(&mut done);
            let root = self.root.as_ref().map(|root| root.span_id.clone());
            let id = self.id("step", self.steps_opened);
            self.steps_opened += 1;
            let mut step = Span::new(id, root, format!("step {}", event.step), KIND_INTERNAL, at);
            step.set("tether.step", event.step);
            self.step = Some((event.step, step));
        }

        match &event.body {
            EventType::ModelRequest(request) => {
                let mut span = Span::new(
                    self.id("seq", event.seq),
                    self.parent(),
                    format!("chat {}", request.model),
                    KIND_CLIENT,
                    at,
                );
                span.set("gen_ai.operation.name", "chat");
                span.set("gen_ai.provider.name", request.provider.as_str());
                span.set("gen_ai.request.model", request.model.as_str());
                span.set("tether.seq", event.seq);
                span.set("tether.model.messages", request.messages.len());
                span.set("tether.model.tools", request.tools.len());
                self.calls.insert(event.seq, span);
            }
            EventType::ModelResponse(response) => {
                if let Some(mut span) = event.parent.and_then(|seq| self.calls.remove(&seq)) {
                    span.set("gen_ai.usage.input_tokens", response.usage.input_tokens);
                    span.set("gen_ai.usage.output_tokens", response.usage.output_tokens);
                    span.set(
                        "tether.model.cache_read_tokens",
                        response.usage.cache_read_tokens,
                    );
                    span.set(
                        "tether.model.cache_write_tokens",
                        response.usage.cache_write_tokens,
                    );
                    if let Some(reason) = &response.stop_reason {
                        span.set("gen_ai.response.finish_reasons", json!([reason]));
                    }
                    if let Some(id) = &response.provider_request_id {
                        span.set("tether.model.provider_request_id", id.as_str());
                    }
                    if let Some(ttft) = response.ttft_ms {
                        span.set("tether.model.ttft_ms", ttft);
                    }
                    span.set("tether.model.stream", response.stream);
                    span.set("tether.model.tool_calls", response.tool_calls.len());
                    // Its cost comes in the tick that follows.
                    if let Some(seq) = event.parent {
                        self.unpriced.insert(seq, span.ended(at));
                    }
                }
            }
            EventType::ToolCall(call) => {
                let mut span = Span::new(
                    self.id("seq", event.seq),
                    self.parent(),
                    format!("execute_tool {}", call.name),
                    KIND_INTERNAL,
                    at,
                );
                span.set("gen_ai.operation.name", "execute_tool");
                span.set("gen_ai.tool.name", call.name.as_str());
                span.set("gen_ai.tool.call.id", call.id.as_str());
                span.set("tether.seq", event.seq);
                span.set("tether.tool.server", call.server.as_str());
                self.calls.insert(event.seq, span);
            }
            EventType::ToolResult(result) => {
                if let Some(mut span) = event.parent.and_then(|seq| self.calls.remove(&seq)) {
                    span.set("tether.tool.duration_ms", result.duration_ms);
                    span.set("tether.tool.is_error", result.is_error);
                    if result.is_error {
                        span.error = Some("the tool call failed or was refused".into());
                    }
                    done.push(span.ended(at));
                }
            }
            EventType::PolicyDecision {
                subject,
                rule,
                decision,
                ..
            } => {
                let (outcome, why) = match decision {
                    Decision::Allow => ("allow", None),
                    Decision::Deny { reason } => ("deny", Some(reason)),
                    Decision::Ask { .. } => ("ask", None),
                };
                // Under the call it is about, which is still open.
                let parent = self
                    .calls
                    .get(subject)
                    .map(|call| call.span_id.clone())
                    .or_else(|| self.parent());
                let mut span = Span::new(
                    self.id("seq", event.seq),
                    parent,
                    format!("policy.decision {outcome}"),
                    KIND_INTERNAL,
                    at,
                );
                span.set("tether.seq", event.seq);
                span.set("tether.policy.outcome", outcome);
                span.set("tether.policy.subject", *subject);
                if let Some(rule) = rule {
                    span.set("tether.policy.rule", rule.as_str());
                }
                if let Some(why) = why {
                    span.set("tether.policy.reason", why.as_str());
                }
                if outcome == "ask" {
                    // Open until someone answers: the span is the wait.
                    self.asks.insert(*subject, span);
                } else {
                    done.push(span);
                }
            }
            EventType::PolicyAnswer {
                subject,
                answered_by,
                allow,
            } => {
                if let Some(mut span) = self.asks.remove(subject) {
                    span.set("tether.policy.answered_by", answered_by.as_str());
                    span.set("tether.policy.allow", *allow);
                    done.push(span.ended(at));
                }
            }
            EventType::BudgetTick { budget, price } => {
                if let Some(mut span) = event.parent.and_then(|seq| self.unpriced.remove(&seq)) {
                    if let Some(price) = price {
                        span.set("tether.model.usd", price.usd);
                        span.set("tether.model.price_table", price.table.as_str());
                    }
                    done.push(span);
                }
                if let Some(root) = &mut self.root {
                    // The latest totals replace the ones before.
                    root.attributes
                        .retain(|(key, _)| !key.starts_with("tether.budget."));
                    root.set("tether.budget.tokens_used", budget.tokens_used);
                    root.set("tether.budget.usd_used", budget.usd_used);
                    root.set("tether.budget.tool_calls_used", budget.tool_calls_used);
                    root.set("tether.budget.wall_secs_used", budget.wall_secs_used);
                }
            }
            EventType::RunStateChanged { state, reason } => {
                let name = format!("{state:?}").to_lowercase();
                self.ended = matches!(state, RunState::Finished | RunState::Halted);
                if let Some(root) = &mut self.root {
                    root.events.push((
                        format!("run.state {name}"),
                        at,
                        vec![("tether.reason".to_owned(), json!(reason))],
                    ));
                    root.attributes.retain(|(key, _)| key != "tether.run.state");
                    root.set("tether.run.state", name);
                    root.error = (*state == RunState::Halted).then(|| reason.clone());
                }
            }
            EventType::Checkpoint { step, .. } => {
                let checkpoint = (
                    "checkpoint".to_owned(),
                    at,
                    vec![("tether.step".to_owned(), json!(step))],
                );
                match (&mut self.step, &mut self.root) {
                    (Some((_, span)), _) | (None, Some(span)) => span.events.push(checkpoint),
                    (None, None) => {}
                }
            }
            EventType::RunFinished(data) => {
                if let Some(root) = &mut self.root {
                    let outcome = match &data["outcome"] {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    root.set("tether.run.outcome", clip(&outcome));
                }
            }
            _ => {}
        }
        self.last_ns = self.last_ns.max(at);
        done
    }

    /// Close everything still open, as of the last event, and return it.
    /// A call or an ask with no answer in the trace is marked incomplete.
    fn finish(&mut self) -> Vec<Span> {
        let mut done = Vec::new();
        let unanswered = std::mem::take(&mut self.calls)
            .into_values()
            .chain(std::mem::take(&mut self.asks).into_values());
        for mut span in unanswered {
            span.set("tether.incomplete", true);
            done.push(span.ended(self.last_ns));
        }
        done.extend(std::mem::take(&mut self.unpriced).into_values());
        self.close_step(&mut done);
        if let Some(root) = self.root.take() {
            done.push(root.ended(self.last_ns));
        }
        done
    }
}

/// An attribute value as OTLP's JSON spells it.
fn any_value(value: &Value) -> Value {
    match value {
        Value::Bool(flag) => json!({"boolValue": flag}),
        // 64-bit integers travel as strings in this encoding.
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n}),
        Value::String(text) => json!({"stringValue": text}),
        Value::Array(items) => {
            json!({"arrayValue": {"values": items.iter().map(any_value).collect::<Vec<_>>()}})
        }
        other => json!({"stringValue": other.to_string()}),
    }
}

fn attributes(pairs: &[(String, Value)]) -> Value {
    pairs
        .iter()
        .map(|(key, value)| json!({"key": key, "value": any_value(value)}))
        .collect()
}

/// One OTLP export request holding `spans`.
fn request(trace_id: &str, spans: &[Span]) -> Value {
    let service = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "tether".into());
    let spans: Vec<Value> = spans
        .iter()
        .map(|span| {
            let mut out = json!({
                "traceId": trace_id,
                "spanId": span.span_id,
                "name": span.name,
                "kind": span.kind,
                "startTimeUnixNano": span.start_ns.to_string(),
                "endTimeUnixNano": span.end_ns.to_string(),
                "attributes": attributes(&span.attributes),
            });
            if let Some(parent) = &span.parent {
                out["parentSpanId"] = json!(parent);
            }
            if !span.events.is_empty() {
                out["events"] = span
                    .events
                    .iter()
                    .map(|(name, at, attrs)| {
                        json!({
                            "name": name,
                            "timeUnixNano": at.to_string(),
                            "attributes": attributes(attrs),
                        })
                    })
                    .collect();
            }
            if let Some(why) = &span.error {
                out["status"] = json!({"code": STATUS_ERROR, "message": why});
            }
            out
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {"attributes": attributes(&[("service.name".to_owned(), json!(service))])},
            "scopeSpans": [{
                "scope": {"name": "tether", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans,
            }],
        }],
    })
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Where requests go.
enum Sink {
    File(std::fs::File),
    Http {
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl Sink {
    /// The file given, else the collector named on the command line, else
    /// the one the standard OpenTelemetry variables name, else a local one.
    fn open(options: &Options) -> anyhow::Result<Self> {
        if let Some(out) = &options.out {
            let file = std::fs::File::create(out)
                .with_context(|| format!("creating {}", out.display()))?;
            return Ok(Self::File(file));
        }
        let traces_path = |base: String| format!("{}/v1/traces", base.trim_end_matches('/'));
        let url = match (&options.endpoint, env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")) {
            (Some(endpoint), _) => traces_path(endpoint.clone()),
            // This variable is the whole address, used as it is.
            (None, Some(whole)) => whole,
            (None, None) => traces_path(
                env("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .unwrap_or_else(|| "http://localhost:4318".into()),
            ),
        };
        // `key=value` pairs separated by commas: where a hosted backend's
        // credentials go.
        let given = env("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
            .or_else(|| env("OTEL_EXPORTER_OTLP_HEADERS"))
            .unwrap_or_default();
        let mut headers = Vec::new();
        for pair in given.split(',').filter(|pair| !pair.trim().is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                bail!("OTEL_EXPORTER_OTLP_HEADERS: {pair:?} is not key=value");
            };
            headers.push((key.trim().to_owned(), value.trim().to_owned()));
        }
        // No redirects: the headers may hold a collector's key, and it goes
        // to the endpoint that was named and nowhere else.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("tether/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("building the HTTP client")?;
        Ok(Self::Http {
            client,
            url,
            headers,
        })
    }

    fn name(&self, options: &Options) -> String {
        match self {
            Self::File(_) => options
                .out
                .as_ref()
                .map_or_else(String::new, |out| out.display().to_string()),
            Self::Http { url, .. } => url.clone(),
        }
    }

    /// Send `spans`, in as many requests as it takes. Returns how many.
    async fn send(&mut self, trace_id: &str, spans: &[Span]) -> anyhow::Result<usize> {
        for batch in spans.chunks(BATCH) {
            let body = serde_json::to_vec(&request(trace_id, batch))?;
            match self {
                Self::File(file) => {
                    file.write_all(&body)?;
                    file.write_all(b"\n")?;
                }
                Self::Http {
                    client,
                    url,
                    headers,
                } => {
                    let mut post = client
                        .post(url.as_str())
                        .header("content-type", "application/json")
                        .body(body);
                    for (key, value) in headers.iter() {
                        post = post.header(key.as_str(), value.as_str());
                    }
                    let response = post
                        .send()
                        .await
                        .with_context(|| format!("sending spans to {url}"))?;
                    let status = response.status();
                    if !status.is_success() {
                        let said = response.text().await.unwrap_or_default();
                        bail!("{url} answered {status}: {}", clip(said.trim()));
                    }
                }
            }
        }
        Ok(spans.len())
    }
}

/// Export the whole trace at once.
async fn export_whole(trace: &Path, sink: &mut Sink) -> anyhow::Result<usize> {
    let mut spans = Spans::new();
    let mut done = Vec::new();
    for event in TraceReader::open(trace).with_context(|| format!("opening {}", trace.display()))? {
        let event = event.with_context(|| format!("reading {}", trace.display()))?;
        done.extend(spans.feed(&event));
    }
    done.extend(spans.finish());
    sink.send(&spans.trace_id, &done).await
}

/// Export the trace as it is written, until its run ends or Ctrl-C.
async fn export_following(trace: &Path, sink: &mut Sink) -> anyhow::Result<usize> {
    let mut follower =
        TraceFollower::open(trace).with_context(|| format!("opening {}", trace.display()))?;
    let mut spans = Spans::new();
    let mut sent = 0;
    let mut interrupted = Box::pin(tokio::signal::ctrl_c());
    loop {
        let events = follower
            .poll()
            .with_context(|| format!("reading {}", trace.display()))?;
        let caught_up = events.is_empty();
        let mut done = Vec::new();
        for event in &events {
            done.extend(spans.feed(event));
        }
        sent += sink.send(&spans.trace_id, &done).await?;
        // Over once the run has ended and there is nothing more to read. A
        // halted run that is resumed later is a new export.
        if caught_up && spans.ended {
            break;
        }
        if caught_up {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = &mut interrupted => break,
            }
        }
    }
    let rest = spans.finish();
    Ok(sent + sink.send(&spans.trace_id, &rest).await?)
}

pub async fn otlp(options: Options) -> anyhow::Result<()> {
    let mut sink = Sink::open(&options)?;
    let sent = if options.follow {
        if tether_trace::pack::is_packed(&options.run) {
            bail!("a packed trace is finished; export it without --follow");
        }
        export_following(&options.run, &mut sink).await?
    } else {
        let opened = pack::open(&options.run.to_string_lossy())?;
        export_whole(Path::new(opened.path()), &mut sink).await?
    };
    println!(
        "exported {sent} spans from {} to {}",
        options.run.display(),
        sink.name(&options)
    );
    Ok(())
}
