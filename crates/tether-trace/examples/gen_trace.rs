//! Writes a long, valid trace for measuring the tools that read one.
//!
//!   cargo run --release -p tether-trace --example gen_trace -- <template.jsonl> <events> <out.jsonl> [change-at-seq]
//!
//! The steps of the template trace are repeated, renumbered, until the
//! output has `events` events. With `change-at-seq`, the first tool call at
//! or after that event gets different arguments, so that two outputs made
//! from one template diverge there.

use std::error::Error;

use tether_trace::{NewEvent, TraceReader, TraceWriter};
use tether_types::{Event, EventType, RunId};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [template, events, out, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: gen_trace <template.jsonl> <events> <out.jsonl> [change-at-seq]".into(),
        );
    };
    let events: u64 = events.parse()?;
    let mut change_at: Option<u64> = rest.first().map(|at| at.parse()).transpose()?;

    let all: Vec<Event> = TraceReader::open(template)?.collect::<Result<_, _>>()?;
    // The run's first step, start to end, is what repeats.
    let first_step = all
        .iter()
        .map(|e| e.step)
        .find(|s| *s > 0)
        .ok_or("no steps")?;
    let head: Vec<&Event> = all.iter().take_while(|e| e.step < first_step).collect();
    let body: Vec<&Event> = all.iter().filter(|e| e.step == first_step).collect();
    // The run's ending: its last state change and `run.finished`.
    let tail: Vec<&Event> = all.iter().skip(all.len().saturating_sub(2)).collect();

    let mut writer = TraceWriter::create(out, RunId("run_generated".into()))?;
    let mut put = |event: &Event, step: u64, shift: u64, body: EventType| {
        writer.append(NewEvent {
            ts: event.ts,
            step,
            agent: event.agent.clone(),
            // Keep each event pointing at the same neighbour it did.
            parent: event.parent.map(|p| p + shift),
            body,
        })
    };

    let mut seq = 0u64;
    for event in &head {
        put(event, event.step, 0, event.body.clone())?;
        seq += 1;
    }
    let first_seq = body.first().map_or(0, |e| e.seq);
    let mut step = first_step;
    while seq + (tail.len() as u64) < events {
        let shift = seq - first_seq;
        for event in &body {
            let mut body = event.body.clone();
            if let (EventType::ToolCall(call), Some(at)) = (&mut body, change_at) {
                if seq >= at {
                    call.arguments = serde_json::json!({"changed": true});
                    change_at = None;
                }
            }
            put(event, step, shift, body)?;
            seq += 1;
        }
        step += 1;
    }
    for event in &tail {
        put(event, step, 0, event.body.clone())?;
    }
    writer.sync()?;
    Ok(())
}
