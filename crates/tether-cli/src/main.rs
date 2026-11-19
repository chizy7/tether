use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tether_trace::{verify, TraceReader};
use tether_types::{Decision, Event, EventType, Payload};

mod run;

#[derive(Parser)]
#[command(
    name = "tether",
    version,
    about = "Run agents you don't trust on infrastructure you do."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a scripted agent under a policy, asking at the terminal when
    /// policy says `ask`.
    Run {
        /// JSON script: `{"task": "...", "replies": [[<content block>, ...], ...]}`,
        /// one list of blocks per model turn.
        #[arg(long)]
        script: PathBuf,
        /// Policy file. Without one there are no rules, so every call asks.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Where the trace and the sandbox workspace go.
        #[arg(long, default_value = ".tether")]
        dir: PathBuf,
    },
    /// Re-run a recorded trace with no network and no cost.
    Replay { run: String },
    /// Align two traces by step and report where they diverge.
    Diff { a: String, b: String },
    /// Run a suite of recorded tasks and score outcomes.
    Eval,
    /// Print a trace in readable form. Takes the path of a `<run>.jsonl`.
    Inspect { run: String },
    /// Continue a halted run from its last checkpoint.
    Resume { run: String },
    /// Verify the BLAKE3 hash chain and blob completeness. Takes the path of a `<run>.jsonl`.
    Verify { run: String },
    /// Bundle a trace and its blobs into one tar.
    Pack { run: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run {
            script,
            policy,
            dir,
        } => run::run(&script, policy.as_deref(), &dir).await,
        Cmd::Inspect { run } => inspect(&run),
        Cmd::Verify { run } => verify_trace(&run),
        Cmd::Replay { .. } | Cmd::Diff { .. } | Cmd::Resume { .. } | Cmd::Pack { .. } => {
            todo!("v0.3")
        }
        Cmd::Eval => todo!("v0.4"),
    };
    match result {
        // The reader went away, as in `tether inspect run.jsonl | head`.
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
}

fn verify_trace(path: &str) -> anyhow::Result<()> {
    let report = verify(path)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "ok: {} events, {} blobs ({} bytes), head {}",
        report.events, report.blobs, report.blob_bytes, report.head
    )?;
    Ok(())
}

/// One line per event. Stops with an error at the first event that fails
/// verification, after printing everything before it.
fn inspect(path: &str) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    for event in TraceReader::open(path)? {
        let event = event?;
        let (kind, summary) = describe(&event);
        writeln!(
            out,
            "{:>5}  step {:<3}  {:<8}  {:<16} {}",
            event.seq, event.step, event.agent, kind, summary
        )?;
    }
    Ok(())
}

fn describe(event: &Event) -> (&str, String) {
    match &event.body {
        EventType::RunCreated(data) => {
            let tools = data["tools"].as_array().map_or(0, Vec::len);
            ("run.created", format!("run {}, {tools} tools", event.run.0))
        }
        EventType::RunStateChanged { state, reason } => {
            ("run.state", format!("{state:?}: {reason}").to_lowercase())
        }
        EventType::RunChild { child } => ("run.child", child.0.clone()),
        EventType::RunFinished(data) => ("run.finished", format!("outcome {}", data["outcome"])),
        EventType::ModelRequest(req) => (
            "model.request",
            format!(
                "{}/{}, {} messages, {} tools",
                req.provider,
                req.model,
                req.messages.len(),
                req.tools.len()
            ),
        ),
        EventType::ModelResponse(resp) => (
            "model.response",
            format!(
                "{} in / {} out tokens, {} ms, {} tool calls",
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                resp.latency_ms,
                resp.tool_calls.len()
            ),
        ),
        EventType::ToolCall(call) => (
            "tool.call",
            format!("{} {}", call.name, clip(&call.arguments.to_string())),
        ),
        EventType::ToolResult(result) => {
            let status = if result.is_error { "error" } else { "ok" };
            let content = match &result.content {
                Payload::Blob { bytes, .. } => format!("blob, {bytes} bytes"),
                Payload::Inline(value) => clip(&value.to_string()),
            };
            (
                "tool.result",
                format!("{status}, {} ms, {content}", result.duration_ms),
            )
        }
        EventType::PolicyDecision {
            subject,
            decision,
            rule,
            ..
        } => {
            let outcome = match decision {
                Decision::Allow => "allow".to_owned(),
                Decision::Deny { reason } => format!("deny: {reason}"),
                Decision::Ask { prompt } => format!("ask: {prompt}"),
            };
            let rule = match rule {
                Some(name) => format!("rule {name}"),
                None => "no rule".to_owned(),
            };
            (
                "policy.decision",
                format!("{outcome} (seq {subject}, {rule})"),
            )
        }
        EventType::PolicyAsk { subject, prompt } => {
            ("policy.ask", format!("{prompt} (seq {subject})"))
        }
        EventType::PolicyAnswer {
            subject,
            answered_by,
            allow,
        } => {
            let answer = if *allow { "allow" } else { "deny" };
            (
                "policy.answer",
                format!("{answer} by {answered_by} (seq {subject})"),
            )
        }
        EventType::SandboxExec(data) => ("sandbox.exec", clip(&data.to_string())),
        EventType::SandboxExit(data) => ("sandbox.exit", clip(&data.to_string())),
        EventType::BudgetTick(budget) => (
            "budget.tick",
            format!(
                "{} tokens, ${}, {} s, {} tool calls",
                of(budget.tokens_used, budget.tokens_limit),
                of(format!("{:.4}", budget.usd_used), budget.usd_limit),
                of(budget.wall_secs_used, budget.wall_secs_limit),
                of(budget.tool_calls_used, budget.tool_calls_limit),
            ),
        ),
        EventType::Checkpoint { step, snapshot } => (
            "checkpoint",
            format!(
                "step {step}, snapshot {}",
                snapshot.as_deref().unwrap_or("none")
            ),
        ),
        EventType::Nondeterminism(data) => ("nondeterminism", clip(&data.to_string())),
        EventType::Ext { r#type, data } => (r#type.as_str(), clip(&data.to_string())),
    }
}

/// `used`, or `used/limit` when there is a limit.
fn of<U: std::fmt::Display, L: std::fmt::Display>(used: U, limit: Option<L>) -> String {
    match limit {
        Some(limit) => format!("{used}/{limit}"),
        None => used.to_string(),
    }
}

/// Keep a summary on one line.
fn clip(text: &str) -> String {
    const MAX: usize = 80;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(MAX).collect();
    clipped.push_str("...");
    clipped
}
