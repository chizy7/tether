use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde_json::json;
use tether_trace::{
    verify, BlobStore, NewEvent, TraceError, TraceFollower, TraceReader, TraceWriter, INLINE_LIMIT,
};
use tether_types::{
    Event, EventType, ModelRequest, Payload, RunId, RunState, ToolCall, ToolResult,
};
use time::OffsetDateTime;

/// A fresh directory per test under cargo's target tmpdir.
fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn new(step: u64, parent: Option<u64>, body: EventType) -> NewEvent {
    NewEvent {
        ts: OffsetDateTime::UNIX_EPOCH,
        step,
        agent: "main".into(),
        parent,
        body,
    }
}

fn state(state: RunState, reason: &str) -> EventType {
    EventType::RunStateChanged {
        state,
        reason: reason.into(),
    }
}

fn tool_result(content: serde_json::Value) -> EventType {
    EventType::ToolResult(ToolResult {
        id: "call_1".into(),
        content: Payload::Inline(content),
        is_error: false,
        duration_ms: 3,
    })
}

/// A three-event trace; returns its path and the events as written.
fn small_trace(test: &str) -> (PathBuf, Vec<Event>) {
    let path = dir(test).join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let events = vec![
        w.append(new(0, None, state(RunState::Running, "start")))
            .expect("append"),
        w.append(new(1, Some(0), tool_result(json!({"stdout": "ok"}))))
            .expect("append"),
        w.append(new(1, None, state(RunState::Finished, "agent done")))
            .expect("append"),
    ];
    (path, events)
}

#[test]
fn written_trace_reads_back_and_verifies() {
    let (path, written) = small_trace("roundtrip");
    assert_eq!(
        written.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    let read: Vec<Event> = TraceReader::open(&path)
        .expect("open")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(read, written);

    let report = verify(&path).expect("verify");
    assert_eq!(report.events, 3);
    assert_eq!(report.blobs, 0);
    assert_eq!(report.head, written[2].hash);
}

#[test]
fn hash_is_blake3_of_the_line_with_the_previous_hash_in_place() {
    let (path, written) = small_trace("spec_hash");
    let text = fs::read_to_string(&path).expect("read");
    let lines: Vec<&str> = text.lines().collect();

    let first = lines[0].replace(&written[0].hash, "");
    assert_eq!(
        written[0].hash,
        format!("blake3:{}", blake3::hash(first.as_bytes()).to_hex())
    );
    let second = lines[1].replace(&written[1].hash, &written[0].hash);
    assert_eq!(
        written[1].hash,
        format!("blake3:{}", blake3::hash(second.as_bytes()).to_hex())
    );
}

#[test]
fn edited_event_breaks_the_chain() {
    let (path, _) = small_trace("tamper");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(
        &path,
        text.replace("\"stdout\":\"ok\"", "\"stdout\":\"no\""),
    )
    .expect("write");

    match verify(&path) {
        Err(TraceError::ChainBroken { seq: 1, .. }) => {}
        other => panic!("expected ChainBroken at seq 1, got {other:?}"),
    }
    // The damaged trace can still be read for inspection.
    let events = TraceReader::open_unverified(&path).expect("open").count();
    assert_eq!(events, 3);
}

#[test]
fn removed_event_is_a_seq_gap() {
    let (path, _) = small_trace("gap");
    let text = fs::read_to_string(&path).expect("read");
    let kept: Vec<&str> = text
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, l)| l)
        .collect();
    fs::write(&path, kept.join("\n") + "\n").expect("write");

    match verify(&path) {
        Err(TraceError::SeqGap {
            expected: 1,
            found: 2,
        }) => {}
        other => panic!("expected SeqGap, got {other:?}"),
    }
}

#[test]
fn truncated_last_line_is_malformed() {
    let (path, _) = small_trace("truncated");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(&path, &text[..text.len() - 20]).expect("write");

    match verify(&path) {
        Err(TraceError::Malformed { line: 3, .. }) => {}
        other => panic!("expected Malformed at line 3, got {other:?}"),
    }
}

#[test]
fn large_tool_result_spills_to_a_blob() {
    let path = dir("spill").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let big = json!({"stdout": "x".repeat(INLINE_LIMIT + 1)});
    let encoded = serde_json::to_vec(&big).expect("encode");

    let small = w
        .append(new(0, None, tool_result(json!("ok"))))
        .expect("append");
    let spilled = w.append(new(0, None, tool_result(big))).expect("append");

    let EventType::ToolResult(small) = small.body else {
        panic!("not a tool result")
    };
    assert_eq!(small.content, Payload::Inline(json!("ok")));

    let EventType::ToolResult(spilled) = spilled.body else {
        panic!("not a tool result")
    };
    let Payload::Blob { blob, bytes } = spilled.content else {
        panic!("large result stayed inline")
    };
    assert_eq!(bytes, encoded.len() as u64);
    assert_eq!(w.blobs().get(&blob).expect("get"), encoded);

    // Layout from TRACE_FORMAT.md: <run>.blobs/blake3/<ab>/<full hex>.
    let hex = blob.strip_prefix("blake3:").expect("prefix");
    let file = path
        .with_extension("blobs")
        .join("blake3")
        .join(&hex[..2])
        .join(hex);
    assert!(file.is_file(), "{}", file.display());

    let report = verify(&path).expect("verify");
    assert_eq!(
        (report.events, report.blobs, report.blob_bytes),
        (2, 1, bytes)
    );

    fs::write(&file, b"tampered").expect("write");
    assert!(matches!(verify(&path), Err(TraceError::BlobCorrupt { .. })));

    fs::remove_file(&file).expect("remove");
    assert!(matches!(verify(&path), Err(TraceError::MissingBlob(h)) if h == blob));
}

#[test]
fn inline_value_shaped_like_a_blob_ref_is_spilled() {
    let path = dir("lookalike").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let lookalike = json!({"blob": "blake3:not-a-real-blob", "bytes": 1});

    w.append(new(0, None, tool_result(lookalike.clone())))
        .expect("append");

    let event = TraceReader::open(&path)
        .expect("open")
        .next()
        .expect("one event")
        .expect("read");
    let EventType::ToolResult(result) = event.body else {
        panic!("not a tool result")
    };
    let Payload::Blob { blob, .. } = result.content else {
        panic!("lookalike stayed inline")
    };
    let stored: serde_json::Value =
        serde_json::from_slice(&w.blobs().get(&blob).expect("get")).expect("json");
    assert_eq!(stored, lookalike);
    verify(&path).expect("verify");
}

#[test]
fn model_request_blobs_are_checked() {
    let path = dir("model_blobs").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let stored = w
        .blobs()
        .put(br#"{"role":"user","content":"hi"}"#)
        .expect("put");
    let request = |messages| {
        EventType::ModelRequest(ModelRequest {
            provider: "fake".into(),
            model: "fake-1".into(),
            messages,
            tools: vec![],
            params: json!({}),
        })
    };

    w.append(new(0, None, request(vec![stored.clone()])))
        .expect("append");
    assert_eq!(verify(&path).expect("verify").blobs, 1);

    let absent = format!("blake3:{}", "0".repeat(64));
    w.append(new(1, None, request(vec![stored, absent.clone()])))
        .expect("append");
    assert!(matches!(verify(&path), Err(TraceError::MissingBlob(h)) if h == absent));
}

#[test]
fn oversized_event_is_rejected_and_nothing_is_written() {
    let path = dir("too_large").join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let huge = EventType::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "sandbox.write_file".into(),
        server: "sandbox".into(),
        arguments: json!({"data": "x".repeat(2 * 1024 * 1024)}),
        external: false,
    });

    match w.append(new(0, None, huge)) {
        Err(TraceError::EventTooLarge { seq: 0, .. }) => {}
        other => panic!("expected EventTooLarge, got {other:?}"),
    }
    assert_eq!(fs::metadata(&path).expect("stat").len(), 0);

    // The writer is still usable and seq 0 was not consumed.
    let next = w
        .append(new(0, None, state(RunState::Running, "start")))
        .expect("append");
    assert_eq!(next.seq, 0);
    verify(&path).expect("verify");
}

#[test]
fn create_refuses_to_overwrite_a_trace() {
    let (path, _) = small_trace("no_overwrite");
    let err = TraceWriter::create(&path, RunId("run_other".into())).expect_err("must fail");
    assert!(matches!(err, TraceError::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists));
}

#[test]
fn blob_store_rejects_hashes_that_are_not_hashes() {
    let store = BlobStore::new(dir("bad_hash").join("blobs"));
    for bad in ["blake3:../../etc/passwd", "sha256:00", "blake3:ABCD", ""] {
        assert!(
            matches!(store.get(bad), Err(TraceError::InvalidBlobHash(_))),
            "{bad}"
        );
        assert!(!store.contains(bad));
    }
}

#[test]
fn a_follower_reads_a_trace_as_it_is_written_and_waits_out_a_half_line() {
    let (path, events) = small_trace("follower");
    let bytes = fs::read(&path).expect("read");
    let line_ends: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter_map(|(at, byte)| (*byte == b'\n').then_some(at + 1))
        .collect();
    assert!(events.len() >= 3);

    // The trace so far: one whole event and half of the next.
    let growing = path.with_file_name("growing.jsonl");
    let cut = line_ends[0] + 10;
    fs::write(&growing, &bytes[..cut]).expect("write");
    let mut follower = TraceFollower::open(&growing).expect("open");
    assert_eq!(follower.poll().expect("poll"), events[..1]);
    // Nothing new, and the half line is not damage.
    assert_eq!(follower.poll().expect("poll"), []);

    // The writer finishes that event and writes the rest.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&growing)
        .expect("open");
    file.write_all(&bytes[cut..]).expect("append");
    assert_eq!(follower.poll().expect("poll"), events[1..]);
    assert_eq!(follower.poll().expect("poll"), []);

    // It verifies as it goes: an event out of order is an error.
    file.write_all(&bytes[..line_ends[0]]).expect("append");
    assert!(matches!(
        follower.poll(),
        Err(TraceError::SeqGap { found: 0, .. })
    ));
}
