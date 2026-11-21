//! `tether ui <run> [<run-b>]`: a trace in the browser.
//!
//! One HTML file, built into the binary, served on localhost with a few JSON
//! endpoints beside it. Everything the page shows comes from `tether-trace`:
//! the same events `inspect` prints, the same blobs, and with two runs the
//! same diff `tether diff` prints. The traces are read once, when the
//! command starts.
//!
//! A trace holds whatever the agent saw and said, so the server keeps it on
//! this machine: it listens on the loopback address only, answers only
//! requests addressed to that address by name, and sends no header that
//! would let another site's page read a response.

use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tether_trace::blob::hash_bytes;
use tether_trace::{diff, BlobStore, TraceReader};
use tether_types::{Decision, Event, EventType, RunState};

use crate::{describe, pack};

/// The page. Its one script and one style block carry a nonce, set afresh
/// for each start of the server.
const PAGE: &str = include_str!("../../../ui/index.html");

pub struct Options {
    pub runs: Vec<String>,
    pub port: u16,
    pub open: bool,
}

/// One trace, read into memory.
struct Loaded {
    /// The path as it was given, for the page to show.
    given: String,
    events: Vec<Event>,
    /// Why the trace does not verify, if it does not. What could be read
    /// of it is still shown.
    damaged: Option<String>,
    blobs: BlobStore,
    /// Keeps a packed trace's scratch copy alive while the server runs.
    _opened: pack::Opened,
}

struct App {
    /// The runs, as `a` and, when there are two, `b`.
    runs: BTreeMap<&'static str, Loaded>,
    /// What a request's `Host` header may be.
    hosts: [String; 2],
    page: String,
    policy: HeaderValue,
}

fn load(given: &str) -> anyhow::Result<Loaded> {
    let opened = pack::open(given)?;
    let path = opened.path().to_owned();
    let mut events = Vec::new();
    let mut damaged = None;
    for event in TraceReader::open(&path).with_context(|| format!("opening {given}"))? {
        match event {
            Ok(event) => events.push(event),
            Err(e) => {
                damaged = Some(e.to_string());
                break;
            }
        }
    }
    if damaged.is_some() {
        // Show what the file holds, checked or not. Lines that are not
        // events at all are left out.
        events = TraceReader::open_unverified(&path)?
            .filter_map(Result::ok)
            .collect();
    }
    Ok(Loaded {
        given: given.to_owned(),
        events,
        damaged,
        blobs: BlobStore::for_trace(FsPath::new(&path)),
        _opened: opened,
    })
}

/// What makes an event stand out: a refusal, a question, a failure, a halt.
fn mark(event: &Event) -> Option<&'static str> {
    match &event.body {
        EventType::PolicyDecision {
            decision: Decision::Deny { .. },
            ..
        }
        | EventType::PolicyAnswer { allow: false, .. } => Some("deny"),
        EventType::PolicyDecision {
            decision: Decision::Ask { .. },
            ..
        }
        | EventType::PolicyAsk { .. } => Some("ask"),
        EventType::RunStateChanged {
            state: RunState::Halted,
            ..
        } => Some("halt"),
        EventType::ToolResult(result) if result.is_error => Some("error"),
        _ => None,
    }
}

/// The strongest mark among several, for a step's dot.
fn strongest(marks: impl Iterator<Item = &'static str>) -> Option<&'static str> {
    let rank = |mark: &str| match mark {
        "halt" => 4,
        "deny" => 3,
        "ask" => 2,
        _ => 1,
    };
    marks.max_by_key(|mark| rank(mark))
}

fn overview(run: &Loaded) -> Value {
    let mut steps: BTreeMap<u64, Vec<&Event>> = BTreeMap::new();
    let (mut state, mut outcome, mut budget) = (None, None, None);
    for event in &run.events {
        steps.entry(event.step).or_default().push(event);
        match &event.body {
            EventType::RunStateChanged { state: now, reason } => {
                state = Some(json!({
                    "state": format!("{now:?}").to_lowercase(),
                    "reason": reason,
                    "step": event.step,
                }));
            }
            EventType::RunFinished(data) => outcome = Some(data["outcome"].clone()),
            EventType::BudgetTick { budget: now, .. } => budget = Some(*now),
            _ => {}
        }
    }
    let count = |events: &[&Event], wanted: fn(&EventType) -> bool| {
        events.iter().filter(|event| wanted(&event.body)).count()
    };
    let steps: Vec<Value> = steps
        .iter()
        .map(|(step, events)| {
            json!({
                "step": step,
                "events": events.len(),
                "model_requests": count(events, |body| matches!(body, EventType::ModelRequest(_))),
                "tool_calls": count(events, |body| matches!(body, EventType::ToolCall(_))),
                "mark": strongest(events.iter().filter_map(|event| mark(event))),
            })
        })
        .collect();
    json!({
        "path": run.given,
        "run": run.events.first().map(|event| event.run.0.as_str()),
        "events": run.events.len(),
        "damaged": run.damaged,
        "state": state,
        "outcome": outcome,
        "budget": budget,
        "steps": steps,
    })
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": what}))).into_response()
}

async fn page(State(app): State<Arc<App>>) -> Html<String> {
    Html(app.page.clone())
}

async fn list_runs(State(app): State<Arc<App>>) -> Json<Value> {
    let listed: BTreeMap<&str, Value> = app
        .runs
        .iter()
        .map(|(key, run)| (*key, overview(run)))
        .collect();
    Json(json!(listed))
}

async fn step(State(app): State<Arc<App>>, Path((key, step)): Path<(String, u64)>) -> Response {
    let Some(run) = app.runs.get(key.as_str()) else {
        return not_found("no such run");
    };
    let events: Vec<Value> = run
        .events
        .iter()
        .filter(|event| event.step == step)
        .map(|event| {
            let (kind, summary) = describe(event);
            json!({
                "seq": event.seq,
                "type": kind,
                "summary": summary,
                "mark": mark(event),
                "event": event,
            })
        })
        .collect();
    Json(json!(events)).into_response()
}

async fn blob(State(app): State<Arc<App>>, Path((key, hash)): Path<(String, String)>) -> Response {
    let Some(run) = app.runs.get(key.as_str()) else {
        return not_found("no such run");
    };
    // The store takes nothing that is not a hash, so this cannot be used to
    // read any other file.
    match run.blobs.get(&hash) {
        Ok(bytes) => {
            let body = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => json!({"kind": "json", "value": value, "bytes": bytes.len()}),
                Err(_) => match String::from_utf8(bytes) {
                    Ok(text) => json!({"kind": "text", "value": text, "bytes": text.len()}),
                    Err(not_text) => json!({"kind": "binary", "bytes": not_text.as_bytes().len()}),
                },
            };
            Json(body).into_response()
        }
        Err(e) => not_found(&e.to_string()),
    }
}

async fn compare(State(app): State<Arc<App>>) -> Response {
    let (Some(a), Some(b)) = (app.runs.get("a"), app.runs.get("b")) else {
        return not_found("there is one run, so nothing to compare");
    };
    let result = diff(&a.events, &b.events);
    let side = |side: &Option<tether_trace::diff::Side>| {
        side.as_ref()
            .map(|side| json!({"seq": side.seq, "summary": side.summary}))
    };
    let counts = |counts: Option<tether_trace::diff::Counts>| {
        counts.map(|c| {
            json!({
                "model_requests": c.model_requests,
                "tool_calls": c.tool_calls,
                "decisions": c.decisions,
            })
        })
    };
    let divergences: Vec<Value> = result
        .divergences
        .iter()
        .map(|d| {
            json!({
                "step": d.step,
                "what": d.what.name(),
                "detail": d.detail,
                "a": side(&d.a),
                "b": side(&d.b),
            })
        })
        .collect();
    let steps: Vec<Value> = result
        .steps
        .iter()
        .map(|s| {
            json!({
                "step": s.step,
                "a": counts(s.a),
                "b": counts(s.b),
                "divergences": s.divergences,
            })
        })
        .collect();
    Json(json!({
        "policy_differs": result.policy_differs,
        "steps": steps,
        "divergences": divergences,
    }))
    .into_response()
}

/// Refuse a request that was not addressed to this server by its loopback
/// name, and say on every response what the page may load.
async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    // Another site's name pointed at this address would arrive here with
    // that site's name as its host.
    if !host.is_some_and(|host| app.hosts.iter().any(|ours| ours == host)) {
        return (
            StatusCode::FORBIDDEN,
            "this server answers only on localhost",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, app.policy.clone());
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Try to open `url` in the user's browser. Not being able to is fine: the
/// address is printed.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub async fn ui(options: Options) -> anyhow::Result<()> {
    let mut runs = BTreeMap::new();
    for (key, given) in ["a", "b"].into_iter().zip(&options.runs) {
        runs.insert(key, load(given)?);
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", options.port))
        .await
        .with_context(|| format!("listening on 127.0.0.1:{}", options.port))?;
    let port = listener.local_addr()?.port();

    // Only the page's own script and style run: each carries this value,
    // which nothing in a trace can know.
    let seed = format!(
        "{}:{}:{port}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    );
    let nonce = hash_bytes(seed.as_bytes())
        .trim_start_matches("blake3:")
        .chars()
        .take(32)
        .collect::<String>();
    let policy = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let app = Arc::new(App {
        runs,
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        page: PAGE.replace("__NONCE__", &nonce),
        policy: HeaderValue::from_str(&policy)?,
    });

    let router = Router::new()
        .route("/", get(page))
        .route("/api/runs", get(list_runs))
        .route("/api/run/{key}/step/{step}", get(step))
        .route("/api/run/{key}/blob/{hash}", get(blob))
        .route("/api/diff", get(compare))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);

    let url = format!("http://127.0.0.1:{port}/");
    // Written at once, and a reader that has gone away is not a reason to
    // stop serving: `println!` would panic on a closed pipe.
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "serving {} at {url}\nCtrl-C to stop",
            options.runs.join(" and ")
        );
        let _ = out.flush();
    }
    if options.open {
        open_browser(&url);
    }
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving")
}
