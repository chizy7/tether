//! `tether eval <suite>`: replay a suite of recorded tasks and check each
//! new trace against assertions.
//!
//! A suite is a directory of recorded traces and an `eval.toml`. Each trace
//! is a task, named by its file. A task is replayed, under a changed system
//! prompt or model if one is given, and then judged by what its new trace
//! says and nothing else: no workspace is looked at and no model is asked
//! whether the answer is good.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;
use tether_trace::{diff, effective, TraceReader};
use tether_types::{Decision, Event, EventType, RunState};

use crate::{clip, pack, replay};

pub struct Options {
    pub suite: PathBuf,
    pub system: Option<String>,
    pub model: Option<String>,
    pub policy: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    /// As `tether replay --trust-recorded-servers`, for every task.
    pub trust_recorded_servers: bool,
}

/// The assertions on one task. Every field is optional; one left out is
/// not checked.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Assertions {
    /// Text the run's outcome must contain.
    outcome: Option<String>,
    /// The most steps the run may take.
    max_steps: Option<u64>,
    /// The most the run may cost, in dollars.
    max_usd: Option<f64>,
    /// No call may be denied, by a rule or by a no to an ask.
    no_deny: Option<bool>,
    /// Tools that must each be called at least once.
    tool_called: Option<Vec<String>>,
    /// Tools that must never be called.
    tool_not_called: Option<Vec<String>>,
    /// Files in the sandbox and text each must contain, going by the last
    /// `write_file` call to the path.
    file_contains: Option<Vec<FileContains>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContains {
    path: String,
    text: String,
}

impl Assertions {
    /// These assertions, with `defaults` for each one not set here.
    fn over(self, defaults: &Assertions) -> Assertions {
        Assertions {
            outcome: self.outcome.or_else(|| defaults.outcome.clone()),
            max_steps: self.max_steps.or(defaults.max_steps),
            max_usd: self.max_usd.or(defaults.max_usd),
            no_deny: self.no_deny.or(defaults.no_deny),
            tool_called: self.tool_called.or_else(|| defaults.tool_called.clone()),
            tool_not_called: self
                .tool_not_called
                .or_else(|| defaults.tool_not_called.clone()),
            file_contains: self
                .file_contains
                .or_else(|| defaults.file_contains.clone()),
        }
    }
}

/// `eval.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Assertions for every task.
    #[serde(default)]
    defaults: Assertions,
    /// Assertions for one task, by the name of its trace file.
    #[serde(default)]
    task: BTreeMap<String, Assertions>,
}

/// What a trace says about how its run went.
#[derive(Debug, Default, PartialEq)]
struct Summary {
    steps: u64,
    usd: f64,
    /// Each assertion the trace fails, in words.
    failures: Vec<String>,
}

/// A path as the sandbox tools would see it, for comparing two spellings.
fn tidy(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

/// True when `wanted` names the tool of `call`, bare or with its server.
fn names(wanted: &str, call: &tether_types::ToolCall) -> bool {
    wanted == call.name
        || wanted
            .split_once('.')
            .is_some_and(|(server, name)| server == call.server && name == call.name)
}

/// Check `events`, the trace of one run, against `asserted`.
fn check(events: &[&Event], asserted: &Assertions) -> Summary {
    let mut summary = Summary::default();
    let mut state = None;
    let mut outcome = None;
    let mut denials = Vec::new();
    let mut calls: Vec<(u64, &tether_types::ToolCall)> = Vec::new();
    // `write_file` calls by `seq`, and whether each one worked.
    let mut written: BTreeMap<String, String> = BTreeMap::new();
    let mut failed_calls: BTreeSet<String> = BTreeSet::new();

    for event in events {
        summary.steps = summary.steps.max(event.step);
        match &event.body {
            EventType::RunStateChanged { state: now, reason } => state = Some((*now, reason)),
            EventType::RunFinished(data) => outcome = Some(&data["outcome"]),
            EventType::BudgetTick { budget, .. } => summary.usd = budget.usd_used,
            EventType::PolicyDecision {
                subject,
                rule,
                decision: Decision::Deny { reason },
                ..
            } => denials.push(match rule {
                Some(rule) => format!("seq {subject} was denied by rule {rule}: {reason}"),
                None => format!("seq {subject} was denied: {reason}"),
            }),
            EventType::PolicyAnswer {
                subject,
                allow: false,
                answered_by,
            } => denials.push(format!("seq {subject} was refused by {answered_by}")),
            EventType::ToolCall(call) => calls.push((event.seq, call)),
            EventType::ToolResult(result) if result.is_error => {
                failed_calls.insert(result.id.clone());
            }
            _ => {}
        }
    }
    for (_, call) in &calls {
        let path = call.arguments["path"].as_str();
        let content = call.arguments["content"].as_str();
        if let ("write_file", Some(path), Some(content), false) = (
            call.name.as_str(),
            path,
            content,
            failed_calls.contains(&call.id),
        ) {
            written.insert(tidy(path).to_owned(), content.to_owned());
        }
    }

    let failures = &mut summary.failures;
    // A run that did not finish fails whatever else is asserted.
    match state {
        Some((RunState::Finished, _)) => {}
        Some((state, reason)) => failures.push(format!(
            "the run did not finish: {}: {reason}",
            format!("{state:?}").to_lowercase()
        )),
        None => failures.push("the run did not finish: the trace has no run state".into()),
    }
    if let Some(wanted) = &asserted.outcome {
        let got = match outcome {
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        };
        match got {
            Some(got) if got.contains(wanted.as_str()) => {}
            Some(got) => failures.push(format!(
                "outcome: wanted it to contain {wanted:?}, got {:?}",
                clip(&got)
            )),
            None => failures.push(format!(
                "outcome: wanted it to contain {wanted:?}, and the run has none"
            )),
        }
    }
    if let Some(most) = asserted.max_steps {
        if summary.steps > most {
            failures.push(format!(
                "max_steps: took {} steps, the limit is {most}",
                summary.steps
            ));
        }
    }
    if let Some(most) = asserted.max_usd {
        if summary.usd > most {
            failures.push(format!(
                "max_usd: cost ${:.4}, the limit is ${most:.4}",
                summary.usd
            ));
        }
    }
    if asserted.no_deny == Some(true) {
        failures.extend(denials.into_iter().map(|what| format!("no_deny: {what}")));
    }
    for wanted in asserted.tool_called.iter().flatten() {
        if !calls.iter().any(|(_, call)| names(wanted, call)) {
            failures.push(format!("tool_called: {wanted} was never called"));
        }
    }
    for unwanted in asserted.tool_not_called.iter().flatten() {
        if let Some((seq, _)) = calls.iter().find(|(_, call)| names(unwanted, call)) {
            failures.push(format!(
                "tool_not_called: {unwanted} was called (seq {seq})"
            ));
        }
    }
    for file in asserted.file_contains.iter().flatten() {
        match written.get(tidy(&file.path)) {
            Some(content) if content.contains(&file.text) => {}
            Some(_) => failures.push(format!(
                "file_contains: {} does not contain {:?}",
                file.path, file.text
            )),
            None => failures.push(format!(
                "file_contains: {} was never written with write_file",
                file.path
            )),
        }
    }
    summary
}

/// The suite's tasks: each trace in the directory, by name, in name order.
fn tasks(suite: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut found = BTreeMap::new();
    let entries =
        std::fs::read_dir(suite).with_context(|| format!("reading {}", suite.display()))?;
    for entry in entries {
        let path = entry?.path();
        let is_trace = path.extension().is_some_and(|ext| ext == "jsonl");
        if !path.is_file() || !(is_trace || tether_trace::pack::is_packed(&path)) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if let Some(other) = found.insert(name.to_owned(), path.clone()) {
            bail!(
                "two traces are named {name}: {} and {}",
                other.display(),
                path.display()
            );
        }
    }
    if found.is_empty() {
        bail!("{} has no traces (.jsonl or packed .tar)", suite.display());
    }
    Ok(found)
}

fn read(path: &str) -> anyhow::Result<Vec<Event>> {
    TraceReader::open(path)
        .with_context(|| format!("opening {path}"))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {path}"))
}

/// One row of the table.
struct Row {
    task: String,
    summary: Summary,
    /// Where the new trace first differs from the recording.
    diverged: String,
}

/// Replay one task and judge its new trace.
async fn run_task(
    name: &str,
    trace: &Path,
    out: &Path,
    options: &Options,
    asserted: &Assertions,
) -> anyhow::Result<Row> {
    let script = match &options.scripts {
        Some(dir) => {
            let script = dir.join(format!("{name}.json"));
            if !script.is_file() {
                bail!("no script for this task at {}", script.display());
            }
            Some(script)
        }
        None => None,
    };
    let replayed = replay::run(&replay::Options {
        run: trace.to_owned(),
        out: Some(out.join(format!("{name}.jsonl"))),
        policy: options.policy.clone(),
        system: options.system.clone(),
        model: options.model.clone(),
        // Only a change that needs a model's answer goes live. A plain
        // replay needs no credentials.
        live: options.system.is_some() || options.model.is_some(),
        script,
        attended: false,
        trust_recorded_servers: options.trust_recorded_servers,
    })
    .await?;

    let recorded = pack::open(&trace.to_string_lossy())?;
    let (old, new) = (
        read(recorded.path())?,
        read(&replayed.out.to_string_lossy())?,
    );
    let mut summary = check(&effective(&new), asserted);
    if let Some(error) = replayed.stopped {
        // A halt is already in the trace as the run's last state. Anything
        // else that stopped the run is said here, in its own words.
        let unfinished = |failure: &String| failure.starts_with("the run did not finish");
        if !summary.failures.iter().any(unfinished) {
            summary
                .failures
                .insert(0, format!("the run stopped: {error}"));
        }
    }
    let diverged = match diff(&old, &new).first() {
        Some(first) => format!("step {}, {}", first.step, first.what.name()),
        None => "none".to_owned(),
    };
    Ok(Row {
        task: name.to_owned(),
        summary,
        diverged,
    })
}

/// An empty directory for the new traces. The default one is this command's
/// own and is replaced; one the user named is never emptied for them.
fn out_dir(options: &Options) -> anyhow::Result<PathBuf> {
    match &options.out {
        Some(out) => {
            if out.exists() && std::fs::read_dir(out)?.next().is_some() {
                bail!(
                    "{} is not empty; give --out a new or empty directory",
                    out.display()
                );
            }
            std::fs::create_dir_all(out)?;
            Ok(out.clone())
        }
        None => {
            let out = options.suite.join(".eval");
            if out.exists() {
                std::fs::remove_dir_all(&out)
                    .with_context(|| format!("clearing {}", out.display()))?;
            }
            std::fs::create_dir_all(&out)?;
            Ok(out)
        }
    }
}

/// Run the suite and print the table. `Ok(true)` when every task passed.
pub async fn eval(options: Options) -> anyhow::Result<bool> {
    let config_path = options.suite.join("eval.toml");
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", config_path.display()))?;
    let tasks = tasks(&options.suite)?;
    if let Some(unknown) = config.task.keys().find(|name| !tasks.contains_key(*name)) {
        bail!(
            "{} has assertions for {unknown}, and the suite has no trace of that name",
            config_path.display()
        );
    }
    let out = out_dir(&options)?;

    // One Ctrl-C ends the whole suite, not just the task under way.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ninterrupted");
            std::process::exit(130);
        }
    });

    let width = tasks.keys().map(String::len).max().unwrap_or(4).max(4);
    // Not locked: tasks run between the rows.
    let mut stdout = io::stdout();
    let changed = match (&options.system, &options.model) {
        (Some(_), Some(model)) => format!("a new system prompt and model {model}"),
        (Some(_), None) => "a new system prompt".to_owned(),
        (None, Some(model)) => format!("model {model}"),
        (None, None) => "nothing changed".to_owned(),
    };
    writeln!(
        stdout,
        "{}: {} tasks, {changed}\n",
        options.suite.display(),
        tasks.len()
    )?;
    writeln!(
        stdout,
        "{:<width$}  result  steps       usd  first divergence",
        "task"
    )?;

    let mut rows = Vec::new();
    for (name, trace) in &tasks {
        let asserted = config
            .task
            .remove(name)
            .unwrap_or_default()
            .over(&config.defaults);
        let row = match run_task(name, trace, &out, &options, &asserted).await {
            Ok(row) => row,
            // A task that could not be replayed at all fails; the rest go on.
            Err(e) => Row {
                task: name.clone(),
                summary: Summary {
                    failures: vec![format!("could not be replayed: {e:#}")],
                    ..Summary::default()
                },
                diverged: "-".to_owned(),
            },
        };
        writeln!(
            stdout,
            "{:<width$}  {:<6}  {:>5}  {:>8.4}  {}",
            row.task,
            if row.summary.failures.is_empty() {
                "pass"
            } else {
                "FAIL"
            },
            row.summary.steps,
            row.summary.usd,
            row.diverged
        )?;
        // Rows appear as tasks finish; a live suite takes minutes.
        stdout.flush()?;
        rows.push(row);
    }

    let failed: Vec<&Row> = rows
        .iter()
        .filter(|row| !row.summary.failures.is_empty())
        .collect();
    for row in &failed {
        writeln!(stdout, "\n{}:", row.task)?;
        for failure in &row.summary.failures {
            writeln!(stdout, "  {failure}")?;
        }
    }
    writeln!(
        stdout,
        "\n{} passed, {} failed, ${:.4} in all",
        rows.len() - failed.len(),
        failed.len(),
        rows.iter().map(|row| row.summary.usd).sum::<f64>()
    )?;
    writeln!(stdout, "new traces: {}", out.display())?;
    Ok(failed.is_empty())
}
