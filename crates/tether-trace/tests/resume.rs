use std::fs;
use std::path::PathBuf;

use tether_trace::writer::drop_partial_line;
use tether_trace::{effective, verify, NewEvent, TraceError, TraceReader, TraceWriter};
use tether_types::{Event, EventType, RunId, RunState};
use time::OffsetDateTime;

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn new(step: u64, body: EventType) -> NewEvent {
    NewEvent {
        ts: OffsetDateTime::UNIX_EPOCH,
        step,
        agent: "main".into(),
        parent: None,
        body,
    }
}

fn state(state: RunState, reason: &str) -> EventType {
    EventType::RunStateChanged {
        state,
        reason: reason.into(),
    }
}

fn checkpoint(step: u64) -> EventType {
    EventType::Checkpoint {
        step,
        snapshot: None,
    }
}

fn ext(name: &str) -> EventType {
    EventType::Ext {
        r#type: format!("ext.{name}"),
        data: serde_json::json!({}),
    }
}

#[test]
fn a_reopened_trace_carries_the_chain_on() {
    let path = dir("append").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    w.append(new(0, state(RunState::Running, "start")))
        .expect("append");
    let last = w
        .append(new(1, state(RunState::Halted, "kill switch: SIGINT")))
        .expect("append");
    drop(w);

    let mut w = TraceWriter::open_append(&path).expect("reopen");
    assert_eq!(w.run().0, "run_test");
    assert_eq!(w.next_seq(), 2);
    let resumed = w
        .append(new(1, state(RunState::Running, "resumed")))
        .expect("append");
    assert_eq!(resumed.seq, last.seq + 1);
    drop(w);

    // One trace, one unbroken chain across the reopen.
    let report = verify(&path).expect("verify");
    assert_eq!((report.events, report.head), (3, resumed.hash));
}

#[test]
fn a_half_written_last_line_is_dropped_and_a_broken_chain_is_not_reopened() {
    let path = dir("partial").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    w.append(new(0, state(RunState::Running, "start")))
        .expect("append");
    w.append(new(1, ext("work"))).expect("append");
    drop(w);
    let whole = fs::read_to_string(&path).expect("read");

    // A crash partway through writing a third event.
    fs::write(&path, format!("{whole}{{\"v\":0,\"run\":\"run_te")).expect("write");
    assert!(matches!(verify(&path), Err(TraceError::Malformed { .. })));
    assert!(drop_partial_line(&path).expect("repair"));
    assert_eq!(fs::read_to_string(&path).expect("read"), whole);
    assert!(!drop_partial_line(&path).expect("nothing to repair"));

    // Reopening does the same repair itself.
    fs::write(&path, format!("{whole}{{\"v\":0")).expect("write");
    let w = TraceWriter::open_append(&path).expect("reopen");
    assert_eq!(w.next_seq(), 2);
    drop(w);

    // A trace that was edited is not something to append to.
    fs::write(&path, whole.replace("ext.work", "ext.play")).expect("write");
    assert!(matches!(
        TraceWriter::open_append(&path),
        Err(TraceError::ChainBroken { seq: 1, .. })
    ));
}

/// The types and steps of `events`, for reading in assertions.
fn shape(events: &[&Event]) -> Vec<(u64, String)> {
    events
        .iter()
        .map(|e| {
            let value = serde_json::to_value(e).expect("json");
            let name = match &e.body {
                EventType::RunStateChanged { reason, .. } => reason.clone(),
                _ => value["type"].as_str().expect("type").to_owned(),
            };
            (e.step, name)
        })
        .collect()
}

#[test]
fn effective_leaves_out_what_each_resume_abandoned() {
    let path = dir("effective").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let bodies = [
        (0, ext("created")),
        (0, state(RunState::Running, "start")),
        (1, ext("step1")),
        (1, checkpoint(1)),
        // Step 2 is under way when the run halts.
        (2, ext("step2-first-try")),
        (2, state(RunState::Halted, "ask unanswered after 600 s")),
        // Resumed from the checkpoint, and halted again.
        (1, state(RunState::Running, "resumed")),
        (2, ext("step2-second-try")),
        (2, state(RunState::Halted, "kill switch: SIGINT")),
        // Resumed again; this time step 2 completes.
        (1, state(RunState::Running, "resumed")),
        (2, ext("step2-done")),
        (2, checkpoint(2)),
        (3, ext("step3")),
        (3, state(RunState::Finished, "agent done")),
    ];
    for (step, body) in bodies {
        w.append(new(step, body)).expect("append");
    }
    let events: Vec<Event> = TraceReader::open(&path)
        .expect("open")
        .collect::<Result<_, _>>()
        .expect("read");

    let kept = effective(&events);
    let expected: Vec<(u64, String)> = [
        (0, "ext.created"),
        (0, "start"),
        (1, "ext.step1"),
        (1, "checkpoint"),
        (1, "resumed"),
        (2, "ext.step2-done"),
        (2, "checkpoint"),
        (3, "ext.step3"),
        (3, "agent done"),
    ]
    .into_iter()
    .map(|(step, name)| (step, name.to_owned()))
    .collect();
    assert_eq!(shape(&kept), expected);

    // A trace never resumed comes back whole.
    let never: Vec<&Event> = events[..6].iter().collect();
    assert_eq!(effective(&events[..6]), never);
}

#[test]
fn a_resume_with_no_checkpoint_goes_back_to_the_start() {
    let path = dir("effective_no_checkpoint").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    for (step, body) in [
        (0, ext("created")),
        (0, state(RunState::Running, "start")),
        (1, ext("step1-first-try")),
        (1, state(RunState::Halted, "model error: overloaded")),
        (0, state(RunState::Running, "resumed")),
        (1, ext("step1-done")),
    ] {
        w.append(new(step, body)).expect("append");
    }
    let events: Vec<Event> = TraceReader::open(&path)
        .expect("open")
        .collect::<Result<_, _>>()
        .expect("read");
    let names: Vec<String> = shape(&effective(&events))
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(names, ["ext.created", "start", "resumed", "ext.step1-done"]);
}
