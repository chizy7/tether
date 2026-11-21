//! A raw agent loop, wrapped by Tether.
//!
//! The agent is `agent` below: ask the model, run the tools it asks for, feed
//! the results back, stop when it asks for none. Its two call sites go through
//! a [`Run`] instead of a provider SDK and a tool dispatcher, and that is the
//! whole integration.
//!
//! By default the model is the scripted `fake` adapter, so this needs no API
//! key. `--model anthropic` swaps in a real model and nothing else changes:
//! the tools are real either way and run in the process sandbox.
//!
//! ```text
//! cargo run -p tether-core --example raw-loop
//! ANTHROPIC_API_KEY=... cargo run -p tether-core --example raw-loop -- --model anthropic
//! cargo run -p tether-cli -- inspect target/raw-loop/<run>.jsonl
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tether_core::{Clock, Run, RunConfig, RunDeps, RunError, SandboxRef, SystemClock, Unattended};
use tether_mcp::sandbox_server::SandboxServer;
use tether_models::anthropic::AnthropicAdapter;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, ModelAdapter, Role, ToolDef};
use tether_policy::{AllowAll, KillSwitch, Policy, RulePolicy};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::RunId;

const SYSTEM: &str = "You are working in a sandboxed workspace directory. \
Use the tools to do what is asked, then say what you did in one sentence.";
const TASK: &str = "Write hello.txt, then show me what is in it.";

/// Which model to ask, and the request fields that go with it.
struct Model {
    adapter: Arc<dyn ModelAdapter>,
    name: String,
    params: Value,
}

/// The agent. Everything Tether-specific is the two `run.` calls.
async fn agent(run: &mut Run, model: &Model, task: &str) -> Result<String, RunError> {
    let tools: Vec<ToolDef> = run
        .tools()
        .iter()
        .map(|t| ToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect();
    let mut messages = vec![
        Message::text(Role::System, SYSTEM),
        Message::text(Role::User, task),
    ];
    loop {
        let request = CompletionRequest {
            model: model.name.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            params: model.params.clone(),
        };
        let reply = run.complete(request).await?;
        // The reply goes back into the history exactly as it came.
        messages.push(reply.message.clone());

        let mut results = Vec::new();
        let mut said = String::new();
        for block in reply.message.content {
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
        // A refusal or a cut-off answer is a reply, but not a finished one.
        match reply.stop_reason.as_deref() {
            Some(reason @ ("refusal" | "max_tokens")) => {
                return Ok(format!("stopped early ({reason}): {said}"));
            }
            _ if results.is_empty() => return Ok(said),
            _ => messages.push(Message {
                role: Role::Tool,
                content: results,
            }),
        }
    }
}

/// What the scripted model does: write a file, read it back, report.
fn scripted() -> Model {
    let script = FakeAdapter::new([
        Completion::tool_use(
            "call_1",
            "write_file",
            json!({"path": "hello.txt", "content": "hello from a wrapped agent\n"}),
        ),
        Completion::tool_use("call_2", "exec", json!({"argv": ["cat", "hello.txt"]})),
        Completion::text("Wrote hello.txt and read it back."),
    ]);
    Model {
        adapter: Arc::new(script),
        name: "fake-1".into(),
        params: json!({}),
    }
}

/// A real model. It can ask for anything, so it gets a policy: the file
/// tools and a few read-only commands are allowed, and the rest asks. Nobody
/// is attached to answer, so an ask halts the run.
const REAL_MODEL_POLICY: &str = r#"
[budget]
max_usd = 0.50
max_tool_calls = 20
max_wall_secs = 300

[[rule]]
name = "files"
match = { tool = ["sandbox.write_file", "sandbox.read_file", "sandbox.list_dir"] }
action = "allow"

[[rule]]
name = "read-only-commands"
match = { tool = "sandbox.exec", argv0 = ["cat", "ls", "head", "wc", "pwd", "echo"] }
action = "allow"
"#;

fn anthropic() -> Result<Model, Box<dyn Error>> {
    // If the model declines a request, let the API retry it on another model
    // in the same call. This is a beta feature and needs its header.
    let adapter = AnthropicAdapter::from_env()?.beta("server-side-fallback-2026-07-01");
    Ok(Model {
        adapter: Arc::new(adapter),
        name: "claude-opus-5-5".into(),
        // A small task: ask for the least reasoning effort.
        params: json!({"output_config": {"effort": "low"}, "fallbacks": "default"}),
    })
}

/// Run the agent under Tether in `dir` and return the path of its trace.
async fn run_demo(
    dir: &Path,
    clock: Arc<dyn Clock>,
    model: Model,
    policy: Arc<dyn Policy>,
) -> Result<PathBuf, Box<dyn Error>> {
    let sandbox = Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(30),
        ..SandboxSpec::new(dir.join("workspace"))
    };
    let handle = sandbox.start(&spec).await?;

    let run_id = format!("run_{}", clock.now_ms());
    let trace = dir.join(format!("{run_id}.jsonl"));
    let config = RunConfig::new(RunId(run_id), trace.clone());
    let deps = RunDeps {
        model: model.adapter.clone(),
        tools: Arc::new(SandboxServer::new(sandbox.clone(), handle.clone())),
        policy,
        sandbox: Some(SandboxRef { sandbox, handle }),
        answerer: Arc::new(Unattended),
        kill: KillSwitch::new(),
        clock,
        replay: None,
    };

    let mut run = Run::start(config, deps).await?;
    let outcome = agent(&mut run, &model, TASK).await;
    // A halted run still left a trace worth reading.
    println!("trace written to {}", trace.display());
    let answer = outcome?;
    run.finish(json!(answer)).await?;
    println!("{answer}");
    Ok(trace)
}

/// The scripted run, which needs no key. The end-to-end test calls this.
pub async fn demo(dir: &Path, clock: Arc<dyn Clock>) -> Result<PathBuf, Box<dyn Error>> {
    run_demo(dir, clock, scripted(), Arc::new(AllowAll)).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (model, policy): (Model, Arc<dyn Policy>) = match args.as_slice() {
        [] | ["--model", "fake"] => (scripted(), Arc::new(AllowAll)),
        ["--model", "anthropic"] => (
            anthropic()?,
            Arc::new(RulePolicy::from_toml(REAL_MODEL_POLICY)?),
        ),
        _ => return Err("usage: raw-loop [--model fake|anthropic]".into()),
    };
    let dir = Path::new("target/raw-loop");
    let trace = run_demo(dir, Arc::new(SystemClock), model, policy).await?;
    println!(
        "read it with: cargo run -p tether-cli -- inspect {}",
        trace.display()
    );
    Ok(())
}
