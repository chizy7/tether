//! The end-to-end check: the `raw-loop` example agent, run under Tether,
//! leaves a trace that verifies and shows its real tool calls.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tether_core::SystemClock;
use tether_trace::{verify, TraceReader};
use tether_types::{EventType, Payload};

#[allow(dead_code)]
#[path = "../../../examples/raw-loop/main.rs"]
mod raw_loop;

#[tokio::test]
async fn wrapped_agent_leaves_a_full_trace() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("raw_loop");
    let _ = std::fs::remove_dir_all(&dir);

    let trace = raw_loop::demo(&dir, Arc::new(SystemClock))
        .await
        .expect("demo");

    // The sandboxed tools really ran.
    let written = std::fs::read_to_string(dir.join("workspace/hello.txt")).expect("hello.txt");
    assert_eq!(written, "hello from a wrapped agent\n");

    // 2 setup + 3 model calls of 4 events + 2 tool calls of 4 events
    // + a checkpoint after each of the first two steps + 2 closing.
    let report = verify(&trace).expect("verify");
    assert_eq!(report.events, 26);

    let results: Vec<_> = TraceReader::open(&trace)
        .expect("open")
        .filter_map(|event| match event.expect("read").body {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| !r.is_error));
    assert_eq!(
        results[1].content,
        Payload::Inline(json!({
            "exit_code": 0,
            "stdout": "hello from a wrapped agent\n",
            "stderr": "",
        }))
    );
}
