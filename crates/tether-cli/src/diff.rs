//! `tether diff <a> <b>`: where two traces diverge.

use std::io::{self, Write};
use std::path::Path;

use anyhow::Context;
use tether_trace::diff::{diff, Counts, Divergence, Side};
use tether_trace::{BlobStore, TraceReader};
use tether_types::Event;

use crate::clip;

fn read(path: &str) -> anyhow::Result<Vec<Event>> {
    let reader = TraceReader::open(path).with_context(|| format!("opening {path}"))?;
    reader
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {path}"))
}

fn run_of(events: &[Event]) -> &str {
    events.first().map_or("empty trace", |event| &event.run.0)
}

/// `a/b` counts, or a dash for a trace with nothing in the step.
fn pair(a: Option<Counts>, b: Option<Counts>, pick: fn(&Counts) -> usize) -> String {
    let show = |counts: Option<Counts>| counts.map_or("-".to_owned(), |c| pick(&c).to_string());
    format!("{}/{}", show(a), show(b))
}

fn side(out: &mut impl Write, name: &str, side: &Option<Side>) -> io::Result<()> {
    match side {
        Some(side) => writeln!(out, "  {name} (seq {}): {}", side.seq, clip(&side.summary)),
        None => writeln!(out, "  {name}: nothing"),
    }
}

/// The message behind a hash, in one line: its role and the start of its
/// text. `None` when the blob is missing or is not a message.
fn message(trace: &str, hash: &str) -> Option<String> {
    let bytes = BlobStore::for_trace(Path::new(trace)).get(hash).ok()?;
    let message: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let role = message["role"].as_str()?;
    let said: Vec<String> = message["content"]
        .as_array()?
        .iter()
        .map(|block| match block["type"].as_str() {
            Some("text") => block["text"].as_str().unwrap_or_default().to_owned(),
            Some("tool_use") => format!("[calls {}]", block["name"].as_str().unwrap_or("?")),
            Some("tool_result") => format!("[result {}]", block["content"]),
            _ => "[provider block]".to_owned(),
        })
        .collect();
    Some(format!("{role}: {}", clip(&said.join(" "))))
}

fn first(out: &mut impl Write, a: &str, b: &str, first: &Divergence) -> io::Result<()> {
    writeln!(
        out,
        "\nfirst divergence: step {}, {}: {}",
        first.step,
        first.what.name(),
        first.detail
    )?;
    side(out, "a", &first.a)?;
    side(out, "b", &first.b)?;
    if let Some((in_a, in_b)) = &first.messages {
        let show = |trace: &str, hash: &Option<String>| match hash {
            Some(hash) => message(trace, hash).unwrap_or_else(|| hash.clone()),
            None => "no message here".to_owned(),
        };
        writeln!(out, "  a's message: {}", show(a, in_a))?;
        writeln!(out, "  b's message: {}", show(b, in_b))?;
    }
    Ok(())
}

/// Print the diff. `Ok(true)` when the traces agree.
pub fn diff_traces(a: &str, b: &str) -> anyhow::Result<bool> {
    let (events_a, events_b) = (read(a)?, read(b)?);
    let result = diff(&events_a, &events_b);

    let mut out = io::stdout().lock();
    writeln!(out, "a: {} ({} events)", run_of(&events_a), events_a.len())?;
    writeln!(out, "b: {} ({} events)", run_of(&events_b), events_b.len())?;
    if result.policy_differs {
        writeln!(out, "the two runs were held to different policies")?;
    }

    writeln!(out, "\nstep  requests  tool calls  decisions  (a/b)")?;
    for step in &result.steps {
        let verdict = match (step.a, step.b, step.divergences) {
            (Some(_), None, _) => "only in a".to_owned(),
            (None, Some(_), _) => "only in b".to_owned(),
            (_, _, 0) => "same".to_owned(),
            (_, _, 1) => "1 divergence".to_owned(),
            (_, _, n) => format!("{n} divergences"),
        };
        writeln!(
            out,
            "{:>4}  {:>8}  {:>10}  {:>9}  {verdict}",
            step.step,
            pair(step.a, step.b, |c| c.model_requests),
            pair(step.a, step.b, |c| c.tool_calls),
            pair(step.a, step.b, |c| c.decisions),
        )?;
    }

    match result.first() {
        None => writeln!(out, "\nno divergence")?,
        Some(divergence) => {
            first(&mut out, a, b, divergence)?;
            let more = result.divergences.len() - 1;
            if more > 0 {
                writeln!(out, "\n{more} more after it")?;
            }
        }
    }
    Ok(result.is_empty())
}
