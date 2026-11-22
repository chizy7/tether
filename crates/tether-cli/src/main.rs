use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tether_trace::{verify, TraceReader};
use tether_types::{Decision, Event, EventType, Payload};

mod diff;
mod eval;
mod export;
mod pack;
mod replay;
mod resume;
mod run;
mod ui;

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
enum ExportCmd {
    /// Export a trace as OpenTelemetry spans: one trace per run, a span per
    /// step, and under it a span for each model call, tool call and policy
    /// decision. Sent as OTLP over HTTP in JSON to a collector, by default
    /// the one at `http://localhost:4318`. The standard variables
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` and
    /// `OTEL_SERVICE_NAME` are read. No message or tool content is sent.
    Otlp {
        /// Path of the `<run>.jsonl`, or a packed `<run>.tar`.
        run: PathBuf,
        /// Keep reading the trace as it is written and export each span as
        /// it completes, until the run finishes or halts, or Ctrl-C.
        #[arg(long)]
        follow: bool,
        /// The collector's address; `/v1/traces` is added to it.
        #[arg(long, conflicts_with = "out")]
        endpoint: Option<String>,
        /// Write the OTLP requests to this file, one JSON document per
        /// line, and send nothing.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an agent under a policy.
    ///
    /// With `--script`, the agent is a script: a task and the model's
    /// replies, played by tether itself, asking at the terminal when policy
    /// says `ask`. With a command after `--`, the agent is that program: the
    /// daemon is started if it is not running and the program is told where
    /// to find it, in TETHER_SOCKET, TETHER_DIR and TETHER_POLICY.
    Run {
        /// JSON script: `{"task": "...", "replies": [[<content block>, ...], ...]}`,
        /// one list of blocks per model turn.
        #[arg(long, conflicts_with = "command")]
        script: Option<PathBuf>,
        /// Policy file. Without one there are no rules, so every call asks.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Where the trace and the sandbox workspace go.
        #[arg(long, default_value = ".tether")]
        dir: PathBuf,
        /// The agent program and its arguments.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Serve the daemon on a socket. Started by `tether run -- <command>`.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, default_value_t = tetherd::DEFAULT_IDLE.as_secs())]
        idle_secs: u64,
    },
    /// Re-run a recorded trace, answering it from the recording: no network
    /// and no cost. Policy is evaluated again, so `--policy` shows what a
    /// changed policy would have decided. Writes a new trace to compare with
    /// `tether diff`. Exits 1 if the replay left the recording and stopped.
    Replay {
        /// Path of the `<run>.jsonl` to replay.
        run: PathBuf,
        /// Where to write the new trace. Default: `<run>.replay.jsonl`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Hold the replay to this policy instead of the recorded one.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Replace the system prompt of every replayed request.
        #[arg(long)]
        system: Option<String>,
        /// Ask this model instead of the recorded one, through the same
        /// provider. The recording has no answer from it, so this needs
        /// `--live-after-divergence`.
        #[arg(long, requires = "live_after_divergence")]
        model: Option<String>,
        /// When the replay asks for something the recording does not have,
        /// carry on live: the real model, and real tools in a fresh
        /// workspace beside the new trace.
        #[arg(long)]
        live_after_divergence: bool,
        /// A script to stand in for the live model, as in `tether run`.
        #[arg(long, requires = "live_after_divergence")]
        script: Option<PathBuf>,
        /// When carrying on live, start the MCP servers named by the policy
        /// recorded in the trace. Without this only the servers in a
        /// `--policy` file are started, because a trace can come from
        /// anyone and a stdio server is a command to run.
        #[arg(long, requires = "live_after_divergence")]
        trust_recorded_servers: bool,
    },
    /// Align two traces by step and report where they diverge. Takes the
    /// paths of two `<run>.jsonl`. Exits 0 if they agree, 1 if they diverge
    /// and 2 if either cannot be read.
    Diff { a: String, b: String },
    /// Replay every recorded task in a suite and check each against the
    /// assertions in the suite's `eval.toml`. With `--system` or `--model`
    /// each task is asked again under the change, live from the first
    /// request that differs, so this makes real, billed model calls. Exits 0
    /// if every task passes, 1 if any fails and 2 if the suite cannot be run.
    Eval {
        /// A directory of recorded traces (`.jsonl` or packed `.tar`) and
        /// an `eval.toml`.
        suite: PathBuf,
        /// Replace the system prompt of every request.
        #[arg(long)]
        system: Option<String>,
        /// Ask this model instead of the recorded one, through the same
        /// provider.
        #[arg(long)]
        model: Option<String>,
        /// Hold every task to this policy instead of its recorded one.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Where the new traces go. Default: `<suite>/.eval`, replaced on
        /// each run.
        #[arg(long)]
        out: Option<PathBuf>,
        /// A directory of scripts, `<task>.json` each, to stand in for the
        /// live model, as in `tether run --script`. For tests and demos.
        #[arg(long)]
        scripts: Option<PathBuf>,
        /// For tasks asked again live, start the MCP servers named by each
        /// recording's own policy. Without this only the servers in a
        /// `--policy` file are started.
        #[arg(long)]
        trust_recorded_servers: bool,
    },
    /// Look at a trace in the browser: steps down the side, the events of
    /// each, and any stored message or result on a click. Policy refusals,
    /// asks and halts stand out. With two traces, also where they diverge.
    /// Serves on this machine only, until Ctrl-C.
    Ui {
        /// Path of a `<run>.jsonl` or a packed `<run>.tar`.
        run: String,
        /// A second trace, to compare the first with.
        run_b: Option<String>,
        /// The port to serve on. Default: one that is free.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Do not open a browser; just print the address.
        #[arg(long)]
        no_open: bool,
    },
    /// Send a trace somewhere else in another format.
    Export {
        #[command(subcommand)]
        format: ExportCmd,
    },
    /// List the tools an agent would see under a policy, by server, without
    /// starting a run. Connects every `[[mcp_server]]` the policy names, so
    /// a server that cannot be reached shows up here first.
    Tools {
        /// Policy file. Without one, only the built-in sandbox tools.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Print the full tool list as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Print a trace in readable form. Takes the path of a `<run>.jsonl`.
    Inspect { run: String },
    /// Continue a halted or crashed run from its last checkpoint, appending
    /// to the same trace. The step that was under way is done again, against
    /// the live model and with real tools in a fresh workspace.
    Resume {
        /// Path of the `<run>.jsonl` to resume.
        run: PathBuf,
        /// A script to stand in for the live model, as in `tether run`. Its
        /// replies are the ones still to come.
        #[arg(long)]
        script: Option<PathBuf>,
        /// Start the MCP servers the run's recorded policy names. Needed to
        /// resume a run that used any, and only right for a trace you made:
        /// a stdio server is a command to run.
        #[arg(long)]
        trust_recorded_servers: bool,
    },
    /// Verify the BLAKE3 hash chain and blob completeness. Takes the path of a `<run>.jsonl`.
    Verify { run: String },
    /// Bundle a trace and the blobs it refers to into one tar, after
    /// verifying it. `inspect`, `verify`, `diff` and `replay` take the tar
    /// in place of a `<run>.jsonl`.
    Pack {
        /// Path of the `<run>.jsonl` to pack.
        run: PathBuf,
        /// Where to write the tar. Default: `<run>.tar` beside the trace.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Unpack a packed trace and verify what comes out.
    Unpack {
        /// Path of the `<run>.tar`.
        tar: PathBuf,
        /// Directory to unpack into. Default: beside the tar.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run {
            script: Some(script),
            policy,
            dir,
            ..
        } => run::run(&script, policy.as_deref(), &dir).await,
        Cmd::Run {
            script: None,
            policy,
            dir,
            command,
        } => {
            let status = run::run_command(&command, policy.as_deref(), &dir).await?;
            // The agent's exit status is this command's.
            std::process::exit(status)
        }
        Cmd::Daemon { socket, idle_secs } => {
            tetherd::serve(&socket, std::time::Duration::from_secs(idle_secs)).await
        }
        Cmd::Tools { policy, json } => run::tools(policy.as_deref(), json).await,
        Cmd::Inspect { run } => pack::open(&run).and_then(|trace| inspect(trace.path())),
        Cmd::Verify { run } => pack::open(&run).and_then(|trace| verify_trace(trace.path())),
        Cmd::Diff { a, b } => match diff_either(&a, &b) {
            Ok(true) => Ok(()),
            Ok(false) => std::process::exit(1),
            Err(e) if is_broken_pipe(&e) => Ok(()),
            Err(e) => {
                eprintln!("Error: {e:#}");
                std::process::exit(2)
            }
        },
        Cmd::Replay {
            run,
            out,
            policy,
            system,
            model,
            live_after_divergence,
            script,
            trust_recorded_servers,
        } => {
            let options = replay::Options {
                run,
                out,
                policy,
                system,
                model,
                live: live_after_divergence,
                script,
                attended: true,
                trust_recorded_servers,
            };
            match replay::replay(options).await {
                // The replay left the recording and had nothing to carry on with.
                Ok(false) => std::process::exit(1),
                Ok(true) => Ok(()),
                Err(e) => Err(e),
            }
        }
        Cmd::Export {
            format:
                ExportCmd::Otlp {
                    run,
                    follow,
                    endpoint,
                    out,
                },
        } => {
            let options = export::Options {
                run,
                follow,
                endpoint,
                out,
            };
            export::otlp(options).await
        }
        Cmd::Ui {
            run,
            run_b,
            port,
            no_open,
        } => {
            let options = ui::Options {
                runs: std::iter::once(run).chain(run_b).collect(),
                port,
                open: !no_open,
            };
            ui::ui(options).await
        }
        Cmd::Resume {
            run,
            script,
            trust_recorded_servers,
        } => resume::resume(&run, script.as_deref(), trust_recorded_servers).await,
        Cmd::Pack { run, out } => pack::pack(&run, out.as_deref()),
        Cmd::Unpack { tar, dir } => pack::unpack(&tar, dir.as_deref()),
        Cmd::Eval {
            suite,
            system,
            model,
            policy,
            out,
            scripts,
            trust_recorded_servers,
        } => {
            let options = eval::Options {
                suite,
                system,
                model,
                policy,
                out,
                scripts,
                trust_recorded_servers,
            };
            match eval::eval(options).await {
                Ok(true) => Ok(()),
                Ok(false) => std::process::exit(1),
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(2)
                }
            }
        }
    };
    match result {
        // The reader went away, as in `tether inspect run.jsonl | head`.
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

/// Diff two traces, each a `.jsonl` or a packed tar.
fn diff_either(a: &str, b: &str) -> anyhow::Result<bool> {
    let (a, b) = (pack::open(a)?, pack::open(b)?);
    diff::diff_traces(a.path(), b.path())
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
            event.seq,
            event.step,
            printable(&event.agent),
            printable(kind),
            printable(&summary)
        )?;
    }
    Ok(())
}

pub(crate) fn describe(event: &Event) -> (&str, String) {
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
        EventType::ModelResponse(resp) => {
            let mut line = format!(
                "{} in / {} out tokens, {} ms, {} tool calls",
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                resp.latency_ms,
                resp.tool_calls.len()
            );
            if let Some(reason) = &resp.stop_reason {
                line.push_str(&format!(", {reason}"));
            }
            ("model.response", line)
        }
        EventType::ToolCall(call) => {
            // A call to an external server is shown as rules see it,
            // `<server>.<tool>`, so the server is on the line.
            let name = if call.external {
                tether_policy::rules::qualified_name(call)
            } else {
                call.name.clone()
            };
            (
                "tool.call",
                format!("{name} {}", clip(&call.arguments.to_string())),
            )
        }
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
        EventType::BudgetTick { budget, price } => {
            let mut line = format!(
                "{} tokens, ${}, {} s, {} tool calls",
                of(budget.tokens_used, budget.tokens_limit),
                of(format!("{:.4}", budget.usd_used), budget.usd_limit),
                of(budget.wall_secs_used, budget.wall_secs_limit),
                of(budget.tool_calls_used, budget.tool_calls_limit),
            );
            if let Some(price) = price {
                line.push_str(&format!(
                    " (call ${:.4}, prices {})",
                    price.usd, price.table
                ));
            }
            ("budget.tick", line)
        }
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
/// `text` made safe to print to a terminal. What is printed often comes out
/// of a trace or from an MCP server, which is to say from anyone, and a
/// control character there could move the cursor, erase a line or forge
/// one. Each is shown as its escape instead.
pub(crate) fn printable(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_owned();
    }
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<char>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub(crate) fn clip(text: &str) -> String {
    const MAX: usize = 80;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(MAX).collect();
    clipped.push_str("...");
    clipped
}
