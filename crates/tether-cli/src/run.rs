//! `tether run`: run a scripted agent under a policy.
//!
//! Until the SDK boundary lands in v0.3 there is no way for an outside agent
//! to talk to Tether, so this runs a script: a task and the model's replies,
//! played by the `fake` adapter. Everything else is real. Tool calls go
//! through the policy file, run in the process sandbox, and an `ask` is put
//! to whoever is at the terminal.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tether_core::{
    Answerer, AskOutcome, AskRequest, Clock, Run, RunConfig, RunDeps, RunError, SandboxRef,
    SystemClock,
};
use tether_mcp::sandbox_server::SandboxServer;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, Role, ToolDef};
use tether_policy::{KillSwitch, RulePolicy};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::RunId;
use tokio::sync::{mpsc, Mutex};

/// A scripted agent: what it is asked, and what the model says each turn.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    task: String,
    /// One entry per model turn, each a list of content blocks.
    replies: Vec<Vec<Block>>,
}

/// Puts each `ask` to the terminal and reads `y` or `n` from stdin.
///
/// Stdin is read on its own thread, started at the first ask, so a run that
/// never asks never touches it. Closed stdin means nobody can answer.
#[derive(Default)]
pub struct TerminalAnswerer {
    lines: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
}

fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

#[async_trait]
impl Answerer for TerminalAnswerer {
    async fn answer(&self, ask: &AskRequest) -> AskOutcome {
        let mut lines = self.lines.lock().await;
        let lines = lines.get_or_insert_with(stdin_lines);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(ask.timeout_secs);
        let rule = ask
            .rule
            .as_deref()
            .unwrap_or("none matched, default is ask");
        loop {
            eprint!(
                "\n{}\n  rule: {rule}\n  allow? [y/n] (unanswered after {} s) ",
                ask.prompt, ask.timeout_secs
            );
            let _ = io::stderr().flush();
            let line = match tokio::time::timeout_at(deadline, lines.recv()).await {
                Ok(Some(line)) => line,
                // Stdin is closed, or time is up. End the prompt's line.
                unanswered => {
                    eprintln!();
                    let waited_secs = if unanswered.is_err() {
                        ask.timeout_secs
                    } else {
                        0
                    };
                    return AskOutcome::Unanswered { waited_secs };
                }
            };
            let allow = match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => true,
                "n" | "no" => false,
                _ => continue,
            };
            return AskOutcome::Answered {
                allow,
                answered_by: "cli".into(),
            };
        }
    }
}

/// A kill switch tripped by Ctrl-C. The first one halts the run at its next
/// event boundary, so the trace ends with the reason; a second one, for a run
/// stuck inside a long call, exits at once.
fn kill_on_sigint() -> KillSwitch {
    let kill = KillSwitch::new();
    let handle = kill.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            handle.trip("SIGINT");
            eprintln!("\nhalting at the next event boundary; Ctrl-C again to exit now");
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
    kill
}

/// The agent loop: ask the model, run the tools it asks for, feed the
/// results back, stop when it asks for none.
async fn agent(run: &mut Run, task: &str) -> Result<String, RunError> {
    let tools: Vec<ToolDef> = run
        .tools()
        .iter()
        .map(|t| ToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect();
    let mut messages = vec![Message::text(Role::User, task)];
    loop {
        let request = CompletionRequest {
            model: "fake-1".into(),
            messages: messages.clone(),
            tools: tools.clone(),
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
                Block::ToolResult { .. } | Block::Provider { .. } => {}
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

pub async fn run(script: &Path, policy: Option<&Path>, dir: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(script)
        .with_context(|| format!("reading script {}", script.display()))?;
    let script: Script = serde_json::from_str(&text)
        .with_context(|| format!("parsing script {}", script.display()))?;
    // No policy file means no rules, and with no rules every call asks.
    let policy = match policy {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading policy {}", path.display()))?;
            RulePolicy::from_toml(&text)
                .with_context(|| format!("loading policy {}", path.display()))?
        }
        None => RulePolicy::default(),
    };

    let sandbox = Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(60),
        ..SandboxSpec::new(dir.join("workspace"))
    };
    let handle = sandbox.start(&spec).await?;

    let clock = Arc::new(SystemClock);
    let run_id = format!("run_{}", clock.now_ms());
    let trace = dir.join(format!("{run_id}.jsonl"));
    let config = RunConfig::new(RunId(run_id), trace.clone());
    let deps = RunDeps {
        model: Arc::new(FakeAdapter::new(
            script.replies.into_iter().map(Completion::new),
        )),
        tools: Arc::new(SandboxServer::new(sandbox.clone(), handle.clone())),
        policy: Arc::new(policy),
        sandbox: Some(SandboxRef { sandbox, handle }),
        answerer: Arc::new(TerminalAnswerer::default()),
        kill: kill_on_sigint(),
        clock,
    };

    let mut run = Run::start(config, deps).await?;
    let outcome = agent(&mut run, &script.task).await;
    // The trace is the record of a halted run too, so always say where it is.
    println!("trace: {}", trace.display());
    let answer = outcome?;
    run.finish(json!(answer)).await?;
    println!("finished: {answer}");
    Ok(())
}
