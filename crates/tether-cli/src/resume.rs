//! `tether resume <run>`: carry on a halted or crashed run.
//!
//! The run goes back to its last checkpoint. The sandbox is rebuilt in a
//! fresh workspace by running again the tool calls that came before it, the
//! conversation is rebuilt from the trace's message blobs, and the step that
//! was under way is done again, live. As with replay, with no agent attached
//! this file is what asks: a plain tool loop.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context};
use serde_json::json;
use tether_core::{
    Recording, ResumeFrom, ResumeNext, Run, RunConfig, RunDeps, SandboxRef, SystemClock,
};
use tether_mcp::sandbox_server::SandboxServer;
use tether_mcp::McpClient;
use tether_policy::{KillSwitch, RulePolicy};
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_trace::writer::drop_partial_line;
use tether_types::RunId;

use crate::replay::{carry_on, live_model, Turn};
use crate::run::{trip_on_sigint, TerminalAnswerer};

pub async fn resume(run: &Path, script: Option<&Path>) -> anyhow::Result<()> {
    // A crash can leave half an event at the end. It was never written.
    if drop_partial_line(run).with_context(|| format!("opening {}", run.display()))? {
        eprintln!("dropped an unfinished last line from {}", run.display());
    }
    let halt_file = run.with_extension("halt");
    if halt_file.exists() {
        bail!(
            "{} exists and would halt the run again at once; remove it first",
            halt_file.display()
        );
    }
    let recording = Recording::load(run).with_context(|| format!("loading {}", run.display()))?;
    let plan = recording.resume_plan()?;

    // The run is held to the policy it started under, which is in the trace.
    let Some(policy) = recording.policy_toml() else {
        bail!("the trace records no policy file, so tether cannot tell what the run was held to; resume it from the program that started it");
    };
    let policy = RulePolicy::from_toml(policy).context("the policy recorded in the trace")?;
    let model = live_model(&recording, script)?;

    // A fresh workspace, brought to where the checkpoint left the old one by
    // running again the calls that came before it.
    let workspace = run.with_extension(format!("resume-{}.workspace", plan.resumes + 1));
    let sandbox: Arc<dyn Sandbox> = Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(60),
        ..SandboxSpec::new(&workspace)
    };
    let handle = sandbox.start(&spec).await?;
    let tools: Arc<dyn McpClient> = Arc::new(SandboxServer::new(sandbox.clone(), handle.clone()));
    for call in &plan.earlier_calls {
        // Its result is already in the trace; only its effect is wanted.
        let _ = tools.call(call).await;
    }

    let kill = KillSwitch::new();
    trip_on_sigint(&kill);
    let config = RunConfig {
        checkpoint_every: recording.checkpoint_every(),
        // Not used: a resumed run keeps the id in its trace.
        ..RunConfig::new(RunId(String::new()), run)
    };
    let deps = RunDeps {
        model,
        tools,
        policy: Arc::new(policy),
        sandbox: Some(SandboxRef { sandbox, handle }),
        answerer: Arc::new(TerminalAnswerer::default()),
        kill,
        clock: Arc::new(SystemClock),
        replay: None,
    };
    let from = ResumeFrom {
        step: plan.step,
        budget: plan.budget,
    };
    let mut resumed = Run::resume(config, deps, from).await?;
    println!(
        "resumed {} after step {}, workspace {}",
        run.display(),
        plan.step,
        workspace.display()
    );

    let turn = match plan.next {
        ResumeNext::Ask(request) => {
            let reply = resumed.complete(request.clone()).await?;
            Turn {
                request,
                reply,
                results: Vec::new(),
            }
        }
        ResumeNext::Continue {
            request,
            reply,
            results,
        } => Turn {
            request,
            reply: *reply,
            results,
        },
    };
    let answer = carry_on(&mut resumed, turn).await?;
    resumed.finish(json!(answer)).await?;
    println!("finished: {answer}");
    Ok(())
}
