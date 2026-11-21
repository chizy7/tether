//! Schema v0 is frozen. The traces in `tests/fixtures` were written by the
//! v0.3.0 release, and every later build has to read them as they are.
//!
//! If this test fails, the change being made breaks the trace format. Do not
//! regenerate the fixtures to make it pass: a trace on someone's disk cannot
//! be regenerated. Either keep the format readable, or bump `v` and keep a
//! reader for `v: 0`, as `docs/TRACE_FORMAT.md` says.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use tether_trace::pack::unpack;
use tether_trace::resumed::is_resume;
use tether_trace::{diff, effective, verify, TraceReader};
use tether_types::{Event, EventType, Payload};

/// Each fixture, its event count, and the hash of its last event. The hash
/// pins the file: it changes if a single byte of the trace does.
const FIXTURES: [(&str, u64, &str); 5] = [
    // `examples/refuses`: a denied `rm -rf`, run by `tether run --script`.
    (
        "refuses",
        35,
        "blake3:c734bc81d0531de7608271aa30de385aaea2be341831c4d90b29adc8c6e5d518",
    ),
    // An ask nobody answered halted the run; `tether resume` finished it.
    (
        "resumed",
        40,
        "blake3:5acc5a193ec6ce9972f4fd8988bda889c4f7de0bb10b26f3542db504d542e248",
    ),
    // A tool result over the inline limit, then a halt on a budget.
    (
        "spilled",
        12,
        "blake3:4834864c047ff6bf3df913325903492ae84733a7c71021029b206d2d603b823c",
    ),
    // `examples/raw-loop-py` through the daemon and the `openai_compat`
    // adapter, streamed and priced, against a local stand-in server.
    (
        "sdk",
        26,
        "blake3:5e6837555bf3ff5cd4850cf463dbf0850076cb4844e74442b0600673012896ad",
    ),
    // `tether replay` of the trace above.
    (
        "sdk_replay",
        26,
        "blake3:a0f829111a0a76dcf5cc7e1941a27b1cb64632dbb6c8d1a44484ff5987f1c529",
    ),
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn events(name: &str) -> Vec<Event> {
    TraceReader::open(fixture(&format!("{name}.jsonl")))
        .expect("open")
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn type_name(event: &Event) -> String {
    serde_json::to_value(event).expect("event as JSON")["type"]
        .as_str()
        .expect("type")
        .to_owned()
}

#[test]
fn v0_3_0_traces_verify_and_keep_their_hashes() {
    for (name, count, head) in FIXTURES {
        let report = verify(fixture(&format!("{name}.jsonl")))
            .unwrap_or_else(|e| panic!("{name} no longer verifies: {e}"));
        assert_eq!(report.events, count, "{name}");
        assert_eq!(report.head, head, "{name}");
    }
}

/// Reading an event and writing it again gives the line it was read from.
/// A renamed field, a changed default or a reordered envelope shows up here
/// even when the reader still accepts the old line.
#[test]
fn v0_3_0_events_are_written_back_byte_for_byte() {
    for (name, ..) in FIXTURES {
        let text = fs::read_to_string(fixture(&format!("{name}.jsonl"))).expect("read");
        for (line, event) in text.lines().zip(events(name)) {
            assert_eq!(event.v, 0, "{name} seq {}", event.seq);
            assert_eq!(
                serde_json::to_string(&event).expect("serialize"),
                line,
                "{name} seq {}",
                event.seq
            );
        }
    }
}

/// The fixtures hold every event type a release has written, so the test
/// above covers each of them. A type added later needs a fixture of its own
/// from the release that first writes it.
#[test]
fn the_fixtures_cover_every_event_type_v0_3_0_wrote() {
    let mut seen = BTreeSet::new();
    let (mut spilled, mut priced, mut streamed, mut resumed) = (false, false, false, false);
    for (name, ..) in FIXTURES {
        for event in events(name) {
            seen.insert(type_name(&event));
            resumed |= is_resume(&event);
            match &event.body {
                EventType::ToolResult(result) => {
                    spilled |= matches!(result.content, Payload::Blob { .. });
                }
                EventType::BudgetTick { price, .. } => priced |= price.is_some(),
                EventType::ModelResponse(response) => streamed |= response.stream,
                _ => {}
            }
        }
    }
    let expected: BTreeSet<String> = [
        "budget.tick",
        "checkpoint",
        "model.request",
        "model.response",
        "policy.answer",
        "policy.ask",
        "policy.decision",
        "run.created",
        "run.finished",
        "run.state",
        "tool.call",
        "tool.result",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(seen, expected);
    assert!(spilled, "no tool result spilled to a blob");
    assert!(priced, "no priced budget.tick");
    assert!(streamed, "no streamed model.response");
    assert!(resumed, "no resume marker");
}

/// What is built on the reader still means the same thing on old traces.
#[test]
fn v0_3_0_traces_still_diff_and_resume_the_same_way() {
    // Replay's acceptance test: a trace against its own replay is empty.
    let result = diff(&events("sdk"), &events("sdk_replay"));
    assert!(result.is_empty(), "{:?}", result.first());
    assert!(!diff(&events("sdk"), &events("refuses")).is_empty());

    // The resumed run reads as the run it became: the step the halt
    // interrupted is there once, not twice.
    let all = events("resumed");
    let kept = effective(&all);
    assert!(kept.len() < all.len());
    let calls = |events: &[&Event]| {
        events
            .iter()
            .filter(|e| matches!(e.body, EventType::ToolCall(_)))
            .count()
    };
    assert_eq!(calls(&all.iter().collect::<Vec<_>>()), 3);
    assert_eq!(calls(&kept), 2);
}

/// `refuses.tar` is `tether pack` of the `refuses` trace, made by v0.3.0.
#[test]
fn a_v0_3_0_pack_unpacks_to_the_trace_it_was_made_from() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("compat_unpack");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");

    let trace = unpack(fixture("refuses.tar"), &dir).expect("unpack");
    assert_eq!(
        fs::read(&trace).expect("unpacked"),
        fs::read(fixture("refuses.jsonl")).expect("fixture")
    );
    assert_eq!(
        verify(&trace).expect("verify"),
        verify(fixture("refuses.jsonl")).expect("verify")
    );
}
