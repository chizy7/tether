//! A raw agent loop, wrapped by Tether.
//!
//! The agent is `agent` below: ask the model, run the tools it asks for, feed
//! the results back, stop when it asks for none. Its two call sites go through
//! a [`Run`] instead of a provider SDK and a tool dispatcher, and that is the
//! whole integration. The model is the scripted `fake` adapter, so this needs
//! no API key; the tools are real and run in the process sandbox.
//!
//! ```text
//! cargo run -p tether-core --example raw-loop
//! cargo run -p tether-cli -- inspect target/raw-loop/<run>.jsonl
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tether_core::{Clock, Run, RunConfig, RunDeps, RunError, SystemClock, Unattended};
use tether_mcp::sandbox_server::SandboxServer;
use tether_models::fake::FakeAdapter;
use tether_models::{Block, Completion, CompletionRequest, Message, Role, ToolDef};
use tether_policy::{AllowAll, KillSwitch};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::RunId;

/// The agent. Everything Tether-specific is the two `run.` calls.
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
                Block::ToolResult { .. } => {}
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

/// What the scripted model does: write a file, read it back, report.
fn script() -> FakeAdapter {
    FakeAdapter::new([
        Completion::tool_use(
            "call_1",
            "write_file",
            json!({"path": "hello.txt", "content": "hello from a wrapped agent\n"}),
        ),
        Completion::tool_use("call_2", "exec", json!({"argv": ["cat", "hello.txt"]})),
        Completion::text("Wrote hello.txt and read it back."),
    ])
}

/// Run the agent under Tether in `dir` and return the path of its trace.
pub async fn demo(dir: &Path, clock: Arc<dyn Clock>) -> Result<PathBuf, Box<dyn Error>> {
    let sandbox = Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(30),
        ..SandboxSpec::new(dir.join("workspace"))
    };
    let handle = sandbox.start(&spec).await?;

    let run_id = format!("run_{}", clock.now_ms());
    let trace = dir.join(format!("{run_id}.jsonl"));
    let config = RunConfig {
        run: RunId(run_id),
        agent: "main".into(),
        trace: trace.clone(),
    };
    let deps = RunDeps {
        model: Arc::new(script()),
        tools: Arc::new(SandboxServer::new(sandbox, handle)),
        policy: Arc::new(AllowAll),
        answerer: Arc::new(Unattended),
        kill: KillSwitch::new(),
        clock,
    };

    let mut run = Run::start(config, deps).await?;
    let answer = agent(&mut run, "Write hello.txt, then show me what is in it.").await?;
    run.finish(json!(answer)).await?;
    Ok(trace)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let trace = demo(Path::new("target/raw-loop"), Arc::new(SystemClock)).await?;
    println!("trace written to {}", trace.display());
    println!(
        "read it with: cargo run -p tether-cli -- inspect {}",
        trace.display()
    );
    Ok(())
}
