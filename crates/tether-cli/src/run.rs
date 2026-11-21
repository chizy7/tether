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
pub(crate) struct Script {
    pub(crate) task: String,
    /// One entry per model turn, each a list of content blocks.
    pub(crate) replies: Vec<Vec<Block>>,
}

impl Script {
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading script {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing script {}", path.display()))
    }
}

/// A policy file as a policy.
pub(crate) fn read_policy(path: &Path) -> anyhow::Result<RulePolicy> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading policy {}", path.display()))?;
    RulePolicy::from_toml(&text).with_context(|| format!("loading policy {}", path.display()))
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
    trip_on_sigint(&kill);
    kill
}

/// Make Ctrl-C trip `kill`, and a second Ctrl-C exit at once.
pub(crate) fn trip_on_sigint(kill: &KillSwitch) {
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
    let script = Script::read(script)?;
    // No policy file means no rules, and with no rules every call asks.
    let policy = match policy {
        Some(path) => read_policy(path)?,
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
        replay: None,
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

/// How long to wait for a daemon just started to answer on its socket.
const DAEMON_START: Duration = Duration::from_secs(10);

/// Run `command` as the agent, with the daemon up for it to talk to.
/// Returns the command's exit status.
///
/// The daemon is this same binary, started in the background if nothing
/// answers on `<dir>/tetherd.sock`. It outlives this command and exits by
/// itself once it has been idle. Its output goes to `<dir>/tetherd.log`.
pub async fn run_command(
    command: &[String],
    policy: Option<&Path>,
    dir: &Path,
) -> anyhow::Result<i32> {
    let Some((program, args)) = command.split_first() else {
        anyhow::bail!("give a script with --script, or the agent's command after --");
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.canonicalize()?;
    let socket = dir.join("tetherd.sock");

    if tetherd::connect(&socket).await.is_err() {
        let log = std::fs::File::create(dir.join("tetherd.log"))?;
        let idle = std::env::var("TETHER_IDLE_SECS")
            .unwrap_or_else(|_| tetherd::DEFAULT_IDLE.as_secs().to_string());
        let mut daemon = std::process::Command::new(std::env::current_exe()?);
        daemon
            .arg("daemon")
            .arg("--socket")
            .arg(&socket)
            .args(["--idle-secs", &idle])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Its own process group, so Ctrl-C meant for the agent does not
        // take the daemon, and the runs it holds, down with it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            daemon.process_group(0);
        }
        daemon.spawn().context("starting the daemon")?;
        let started = std::time::Instant::now();
        while tetherd::connect(&socket).await.is_err() {
            if started.elapsed() > DAEMON_START {
                anyhow::bail!(
                    "the daemon did not come up on {}; see {}",
                    socket.display(),
                    dir.join("tetherd.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let mut agent = tokio::process::Command::new(program);
    agent
        .args(args)
        .env("TETHER_SOCKET", &socket)
        .env("TETHER_DIR", &dir);
    if let Some(policy) = policy {
        let policy = policy
            .canonicalize()
            .with_context(|| format!("policy {}", policy.display()))?;
        agent.env("TETHER_POLICY", policy);
    }
    let status = agent
        .status()
        .await
        .with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}
