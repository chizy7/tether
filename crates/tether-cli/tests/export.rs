//! `tether export otlp` on traces recorded here with the scripted model.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

fn tether_env(args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .args(args)
        // Whatever the developer's shell has set must not steer a test.
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_HEADERS")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
        .env_remove("OTEL_SERVICE_NAME")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tether");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

fn tether(args: &[&str], stdin: &str) -> Output {
    tether_env(args, stdin, &[])
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

/// Writes `notes.txt`, removes it, reports.
const SCRIPT: &str = r#"{
  "task": "Tidy up.",
  "replies": [
    [{"type": "tool_use", "id": "call_1", "name": "write_file",
      "arguments": {"path": "notes.txt", "content": "keep me\n"}}],
    [{"type": "tool_use", "id": "call_2", "name": "exec",
      "arguments": {"argv": ["rm", "notes.txt"]}}],
    [{"type": "text", "text": "Done."}]
  ]
}"#;

/// Writes are allowed; `rm` asks.
const POLICY: &str = r#"
[[rule]]
name = "writes"
match = { tool = "sandbox.write_file" }
action = "allow"

[[rule]]
name = "no-rm"
match = { tool = "sandbox.exec", argv0 = "rm" }
action = "ask"
"#;

/// Record the scripted run in a fresh directory, answering asks with
/// `stdin`, and return the directory and the trace.
fn record(test: &str, stdin: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    fs::write(dir.join("script.json"), SCRIPT).expect("write script");
    fs::write(dir.join("tether.toml"), POLICY).expect("write policy");
    tether(
        &[
            "run",
            "--script",
            path(&dir.join("script.json")),
            "--policy",
            path(&dir.join("tether.toml")),
            "--dir",
            path(&dir.join("out")),
        ],
        stdin,
    );
    let trace = fs::read_dir(dir.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace");
    (dir, trace)
}

/// Every span in a file of OTLP requests, one request per line, with the
/// trace ids seen.
fn spans_in(file: &Path) -> (Vec<Value>, BTreeSet<String>) {
    let mut spans = Vec::new();
    let mut traces = BTreeSet::new();
    for line in fs::read_to_string(file).expect("read export").lines() {
        let request: Value = serde_json::from_str(line).expect("a JSON request per line");
        let resource = &request["resourceSpans"][0];
        assert_eq!(
            resource["resource"]["attributes"][0],
            serde_json::json!({"key": "service.name", "value": {"stringValue": "tether"}})
        );
        let scope = &resource["scopeSpans"][0];
        assert_eq!(scope["scope"]["name"], "tether");
        for span in scope["spans"].as_array().expect("spans") {
            traces.insert(span["traceId"].as_str().expect("traceId").to_owned());
            spans.push(span.clone());
        }
    }
    (spans, traces)
}

fn attribute(span: &Value, key: &str) -> Option<Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| attribute["value"].clone())
}

fn names(spans: &[Value]) -> Vec<&str> {
    spans
        .iter()
        .map(|span| span["name"].as_str().expect("name"))
        .collect()
}

fn export_to_file(trace: &Path, out: &Path) -> Vec<Value> {
    let done = tether(&["export", "otlp", path(trace), "--out", path(out)], "");
    assert!(done.status.success(), "{done:?}");
    let (spans, traces) = spans_in(out);
    assert_eq!(traces.len(), 1, "one trace per run");
    spans
}

#[test]
fn a_run_becomes_one_trace_of_run_step_and_call_spans() {
    let (dir, trace) = record("export_whole", "y\n");
    let spans = export_to_file(&trace, &dir.join("otlp.jsonl"));

    let mut counted = names(&spans);
    counted.sort_unstable();
    assert_eq!(
        counted,
        [
            "chat fake-1",
            "chat fake-1",
            "chat fake-1",
            "execute_tool exec",
            "execute_tool write_file",
            "policy.decision allow",
            "policy.decision allow",
            "policy.decision allow",
            "policy.decision allow",
            "policy.decision ask",
            "step 1",
            "step 2",
            "step 3",
            "tether.run",
        ]
    );

    // Ids are hex of the right length and no two spans share one.
    let by_id: BTreeMap<&str, &Value> = spans
        .iter()
        .map(|span| (span["spanId"].as_str().expect("spanId"), span))
        .collect();
    assert_eq!(by_id.len(), spans.len());
    let hex =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit());
    for span in &spans {
        assert!(
            hex(span["traceId"].as_str().expect("traceId"), 32),
            "{span}"
        );
        assert!(hex(span["spanId"].as_str().expect("spanId"), 16), "{span}");
    }

    // The shape: the run at the root, steps under it, calls under steps,
    // each decision under the call it is about. A span is inside its parent
    // in time too.
    let parent_name = |span: &Value| {
        let parent = span["parentSpanId"].as_str()?;
        by_id.get(parent)?["name"].as_str()
    };
    let nanos = |span: &Value, key: &str| -> u128 {
        span[key].as_str().expect("time").parse().expect("nanos")
    };
    for span in &spans {
        let name = span["name"].as_str().expect("name");
        let under = parent_name(span);
        match name {
            "tether.run" => assert_eq!(under, None),
            _ if name.starts_with("step ") => assert_eq!(under, Some("tether.run")),
            _ if name.starts_with("policy.decision") => {
                let under = under.expect("parent");
                assert!(
                    under.starts_with("chat ") || under.starts_with("execute_tool "),
                    "{under}"
                );
            }
            _ => assert!(under.expect("parent").starts_with("step "), "{name}"),
        }
        assert!(nanos(span, "startTimeUnixNano") <= nanos(span, "endTimeUnixNano"));
        if let Some(parent) = span["parentSpanId"].as_str() {
            let parent = by_id[parent];
            assert!(nanos(parent, "startTimeUnixNano") <= nanos(span, "startTimeUnixNano"));
            assert!(nanos(span, "endTimeUnixNano") <= nanos(parent, "endTimeUnixNano"));
        }
    }

    // Attributes come from the events.
    let find = |name: &str| {
        spans
            .iter()
            .find(|span| span["name"] == name)
            .unwrap_or_else(|| panic!("no span {name}"))
    };
    let string = |text: &str| serde_json::json!({"stringValue": text});
    let chat = find("chat fake-1");
    assert_eq!(
        attribute(chat, "gen_ai.operation.name"),
        Some(string("chat"))
    );
    assert_eq!(
        attribute(chat, "gen_ai.provider.name"),
        Some(string("fake"))
    );
    assert_eq!(
        attribute(chat, "gen_ai.request.model"),
        Some(string("fake-1"))
    );
    assert_eq!(
        attribute(chat, "gen_ai.usage.input_tokens"),
        Some(serde_json::json!({"intValue": "0"}))
    );
    assert_eq!(chat["kind"], 3);
    let tool = find("execute_tool exec");
    assert_eq!(attribute(tool, "gen_ai.tool.name"), Some(string("exec")));
    assert_eq!(
        attribute(tool, "gen_ai.tool.call.id"),
        Some(string("call_2"))
    );
    assert_eq!(tool.get("status"), None);
    let ask = find("policy.decision ask");
    assert_eq!(attribute(ask, "tether.policy.rule"), Some(string("no-rm")));
    assert_eq!(
        attribute(ask, "tether.policy.answered_by"),
        Some(string("cli"))
    );
    assert_eq!(
        attribute(ask, "tether.policy.allow"),
        Some(serde_json::json!({"boolValue": true}))
    );
    let run = find("tether.run");
    assert_eq!(attribute(run, "tether.run.state"), Some(string("finished")));
    assert_eq!(attribute(run, "tether.run.outcome"), Some(string("Done.")));
    assert_eq!(run.get("status"), None);
    let states: Vec<&str> = run["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["name"].as_str().expect("name"))
        .collect();
    assert_eq!(states.first(), Some(&"run.state running"));
    assert_eq!(states.last(), Some(&"run.state finished"));

    // No content leaves: not the file's text, not the command's arguments.
    let exported = fs::read_to_string(dir.join("otlp.jsonl")).expect("read");
    assert!(!exported.contains("keep me") && !exported.contains("notes.txt"));

    // Exporting again gives the same ids, and a packed trace the same spans.
    let again = export_to_file(&trace, &dir.join("again.jsonl"));
    assert_eq!(again, spans);
    let tar = dir.join("run.tar");
    assert!(tether(&["pack", path(&trace), "--out", path(&tar)], "")
        .status
        .success());
    assert_eq!(export_to_file(&tar, &dir.join("packed.jsonl")), spans);
}

#[test]
fn a_halted_run_is_an_error_span_and_what_was_open_is_marked_incomplete() {
    // Nobody answers the ask, so the run halts with the `rm` unanswered.
    let (dir, trace) = record("export_halted", "");
    let spans = export_to_file(&trace, &dir.join("otlp.jsonl"));

    let find = |name: &str| {
        spans
            .iter()
            .find(|span| span["name"] == name)
            .unwrap_or_else(|| panic!("no span {name}"))
    };
    let run = find("tether.run");
    assert_eq!(run["status"]["code"], 2);
    assert!(
        run["status"]["message"]
            .as_str()
            .expect("message")
            .contains("ask unanswered"),
        "{run}"
    );
    let incomplete = Some(serde_json::json!({"boolValue": true}));
    assert_eq!(
        attribute(find("execute_tool exec"), "tether.incomplete"),
        incomplete
    );
    assert_eq!(
        attribute(find("policy.decision ask"), "tether.incomplete"),
        incomplete
    );
    assert_eq!(
        attribute(find("execute_tool write_file"), "tether.incomplete"),
        None
    );
}

struct Received {
    head: String,
    body: Value,
}

/// A stand-in collector on a local port: answers each connection with the
/// next status and reports what it was sent.
fn collector(statuses: Vec<u16>) -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for status in statuses {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let (head, body_start) = loop {
                let read = socket.read(&mut buffer).expect("read");
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    break (text[..end].to_owned(), end + 4);
                }
            };
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().expect("length"))
                })
                .unwrap_or(0);
            while request.len() < body_start + length {
                let read = socket.read(&mut buffer).expect("read");
                request.extend_from_slice(&buffer[..read]);
            }
            let body = serde_json::from_slice(&request[body_start..]).expect("JSON body");
            tx.send(Received { head, body }).expect("report");
            let said = "{\"partialSuccess\":{}}";
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{said}",
                said.len()
            );
            socket.write_all(reply.as_bytes()).expect("write");
        }
    });
    (url, rx)
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

#[test]
fn spans_are_posted_to_the_collector_as_otlp_json() {
    let (dir, trace) = record("export_http", "y\n");
    let on_file = export_to_file(&trace, &dir.join("otlp.jsonl"));

    // The address on the command line, with the standard headers variable.
    let (url, received) = collector(vec![200]);
    let done = tether_env(
        &["export", "otlp", path(&trace), "--endpoint", &url],
        "",
        &[(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "x-team=agents, authorization=Bearer abc",
        )],
    );
    assert!(done.status.success(), "{done:?}");
    let said = String::from_utf8_lossy(&done.stdout);
    assert!(said.contains("exported 14 spans"), "{said}");
    // Credentials are not echoed.
    assert!(!said.contains("abc"), "{said}");

    let sent = received.recv().expect("request");
    assert!(
        sent.head.starts_with("POST /v1/traces HTTP/1.1"),
        "{}",
        sent.head
    );
    assert_eq!(header(&sent.head, "content-type"), Some("application/json"));
    assert_eq!(header(&sent.head, "x-team"), Some("agents"));
    assert_eq!(header(&sent.head, "authorization"), Some("Bearer abc"));
    assert_eq!(
        sent.body["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("spans"),
        &on_file
    );

    // The standard endpoint variables: a base address, or the whole one.
    let (url, received) = collector(vec![200, 200]);
    let done = tether_env(
        &["export", "otlp", path(&trace)],
        "",
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &format!("{url}/")),
            ("OTEL_SERVICE_NAME", "my-agent"),
        ],
    );
    assert!(done.status.success(), "{done:?}");
    let sent = received.recv().expect("request");
    assert!(sent.head.starts_with("POST /v1/traces "), "{}", sent.head);
    assert_eq!(
        sent.body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
        "my-agent"
    );
    let whole = format!("{url}/custom/path");
    let done = tether_env(
        &["export", "otlp", path(&trace)],
        "",
        &[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", &whole)],
    );
    assert!(done.status.success(), "{done:?}");
    let sent = received.recv().expect("request");
    assert!(sent.head.starts_with("POST /custom/path "), "{}", sent.head);

    // A collector that refuses is an error, with its status.
    let (url, _received) = collector(vec![503]);
    let done = tether(&["export", "otlp", path(&trace), "--endpoint", &url], "");
    assert!(!done.status.success());
    let err = String::from_utf8_lossy(&done.stderr);
    assert!(err.contains("503"), "{err}");

    // So is a trace that does not verify: nothing of it is sent.
    let text = fs::read_to_string(&trace).expect("read");
    let broken = dir.join("broken.jsonl");
    fs::write(&broken, text.replacen("write_file", "write_fire", 1)).expect("write");
    let done = tether(
        &[
            "export",
            "otlp",
            path(&broken),
            "--out",
            path(&dir.join("broken-otlp.jsonl")),
        ],
        "",
    );
    assert!(!done.status.success());
    assert_eq!(
        fs::read_to_string(dir.join("broken-otlp.jsonl")).expect("read"),
        ""
    );
}

#[test]
fn follow_exports_a_trace_as_it_is_written_and_ends_with_the_run() {
    let (dir, trace) = record("export_follow", "y\n");
    let whole = export_to_file(&trace, &dir.join("whole.jsonl"));

    // A copy that has got as far as the middle of step 2's second event.
    let bytes = fs::read(&trace).expect("read");
    let text = String::from_utf8_lossy(&bytes);
    let (second, _) = text.match_indices("\"step\":2").nth(1).expect("step 2");
    let cut = second + 40;
    let growing = dir.join("growing.jsonl");
    fs::write(&growing, &bytes[..cut]).expect("write");

    let out = dir.join("followed.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .args([
            "export",
            "otlp",
            path(&growing),
            "--follow",
            "--out",
            path(&out),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tether");

    // Step 1's spans arrive while the run is still going.
    let mut early = Vec::new();
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        early = spans_in(&out).0;
        if !early.is_empty() {
            break;
        }
    }
    let early_names = names(&early);
    assert!(
        early_names.contains(&"execute_tool write_file"),
        "{early_names:?}"
    );
    assert!(early_names.contains(&"step 1"), "{early_names:?}");
    assert!(!early_names.contains(&"tether.run"), "{early_names:?}");
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "still following"
    );

    // The writer finishes the run; the export ends by itself.
    fs::OpenOptions::new()
        .append(true)
        .open(&growing)
        .expect("open")
        .write_all(&bytes[cut..])
        .expect("append");
    let done = child.wait_with_output().expect("wait");
    assert!(done.status.success(), "{done:?}");

    // The same spans as exporting it whole, in the order they completed,
    // with the run last.
    let (followed, traces) = spans_in(&out);
    assert_eq!(traces.len(), 1);
    assert_eq!(followed.last().expect("spans")["name"], "tether.run");
    let key = |span: &Value| span["spanId"].as_str().expect("spanId").to_owned();
    let sorted = |mut spans: Vec<Value>| {
        spans.sort_by_key(key);
        spans
    };
    assert_eq!(sorted(followed), sorted(whole));
}
