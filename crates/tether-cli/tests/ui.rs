//! `tether ui`: the server behind the page, over real traces recorded here
//! with the scripted model.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use serde_json::Value;

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
/// `stdin`, and return the trace.
fn record(test: &str, stdin: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    fs::write(dir.join("script.json"), SCRIPT).expect("write script");
    fs::write(dir.join("tether.toml"), POLICY).expect("write policy");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .args([
            "run",
            "--script",
            path(&dir.join("script.json")),
            "--policy",
            path(&dir.join("tether.toml")),
            "--dir",
            path(&dir.join("out")),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tether");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait().expect("wait");
    fs::read_dir(dir.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace")
}

/// A running `tether ui`, stopped when dropped.
struct Server {
    child: Child,
    address: String,
    /// Kept open: a server whose reader has gone away is writing to a
    /// closed pipe.
    _stdout: BufReader<ChildStdout>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(traces: &[&Path]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .arg("ui")
        .args(traces)
        .arg("--no-open")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tether ui");
    // The first line names the address it chose.
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    stdout.read_line(&mut line).expect("read address");
    let address = line
        .split("http://")
        .nth(1)
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .trim()
        .trim_end_matches('/')
        .to_owned();
    assert!(address.starts_with("127.0.0.1:"), "{address}");
    Server {
        child,
        address,
        _stdout: stdout,
    }
}

/// One GET, by hand: the status, the header block and the body.
fn get_as(server: &Server, host: &str, target: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(&server.address).expect("connect");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status");
    (status, head.to_lowercase(), body.to_owned())
}

fn get(server: &Server, target: &str) -> (u16, String, String) {
    get_as(server, &server.address, target)
}

fn json(server: &Server, target: &str) -> Value {
    let (status, _, body) = get(server, target);
    assert_eq!(status, 200, "{target}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{target}: {e}: {body}"))
}

#[test]
fn one_run_is_served_as_steps_events_and_stored_content() {
    let trace = record("ui_one", "y\n");
    let server = serve(&[&trace]);

    // The page, with a policy that lets only its own script and style run.
    let (status, head, page) = get(&server, "/");
    assert_eq!(status, 200);
    assert!(page.contains("<title>Tether trace</title>"), "{page}");
    assert!(!page.contains("__NONCE__"));
    let policy = head
        .lines()
        .find(|line| line.starts_with("content-security-policy:"))
        .expect("a content security policy");
    assert!(policy.contains("default-src 'none'"), "{policy}");
    let nonce = policy
        .split("script-src 'nonce-")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("a nonce");
    assert!(page.contains(&format!("<script nonce=\"{nonce}\">")));
    assert!(!policy.contains("unsafe-inline"), "{policy}");
    // Nothing lets another site's page read a response.
    assert!(!head.contains("access-control-allow-origin"), "{head}");

    let runs = json(&server, "/api/runs");
    let run = &runs["a"];
    assert_eq!(runs.get("b"), None);
    assert_eq!(run["state"]["state"], "finished");
    assert_eq!(run["outcome"], "Done.");
    assert_eq!(run["damaged"], Value::Null);
    assert_eq!(run["budget"]["tool_calls_used"], 2);
    let steps: Vec<(u64, Value)> = run["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| (step["step"].as_u64().expect("step"), step["mark"].clone()))
        .collect();
    // The step that asked is marked.
    assert_eq!(
        steps,
        [
            (0, Value::Null),
            (1, Value::Null),
            (2, Value::from("ask")),
            (3, Value::Null)
        ]
    );

    // A step's events, each with the line `inspect` prints and the event whole.
    let events = json(&server, "/api/run/a/step/2");
    let events = events.as_array().expect("events");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().expect("type"))
        .collect();
    assert!(
        kinds.contains(&"policy.ask") && kinds.contains(&"policy.answer"),
        "{kinds:?}"
    );
    let ask = events
        .iter()
        .find(|event| event["type"] == "policy.ask")
        .expect("ask");
    assert_eq!(ask["mark"], "ask");
    assert!(ask["summary"].as_str().expect("summary").contains("rm"));
    assert_eq!(ask["event"]["type"], "policy.ask");
    assert_eq!(ask["event"]["step"], 2);

    // A message, by the hash a request names it with.
    let first = json(&server, "/api/run/a/step/1");
    let hash = first[0]["event"]["data"]["messages"][0]
        .as_str()
        .expect("a message hash");
    let blob = json(&server, &format!("/api/run/a/blob/{hash}"));
    assert_eq!(blob["kind"], "json");
    assert_eq!(blob["value"]["role"], "user");
    assert_eq!(blob["value"]["content"][0]["text"], "Tidy up.");

    // What is not there is a 404, and nothing but a hash is looked up.
    for missing in [
        "/api/run/a/blob/blake3:0000000000000000000000000000000000000000000000000000000000000000",
        "/api/run/a/blob/..%2F..%2Ftether.toml",
        "/api/run/b/step/1",
        "/api/diff",
        "/nothing",
    ] {
        assert_eq!(get(&server, missing).0, 404, "{missing}");
    }
    assert_eq!(json(&server, "/api/run/a/step/99"), Value::Array(vec![]));
}

#[test]
fn only_requests_addressed_to_localhost_are_answered() {
    let trace = record("ui_host", "y\n");
    let server = serve(&[&trace]);
    let port = server.address.rsplit(':').next().expect("port");

    assert_eq!(
        get_as(&server, &format!("localhost:{port}"), "/api/runs").0,
        200
    );
    // Another site's name pointed at this address: refused, page and data.
    for host in [
        "evil.example",
        &format!("evil.example:{port}"),
        "127.0.0.1:1",
    ] {
        for target in ["/", "/api/runs"] {
            let (status, _, body) = get_as(&server, host, target);
            assert_eq!(status, 403, "{host} {target}");
            assert!(!body.contains("run"), "{body}");
        }
    }
}

#[test]
fn two_runs_are_compared_and_a_damaged_trace_says_so() {
    let finished = record("ui_two_a", "y\n");
    // Nobody answers the ask here, so this run halts where the other went on.
    let halted = record("ui_two_b", "");
    // And a copy of the first with one event edited.
    let damaged = finished.with_file_name("damaged.jsonl");
    let text = fs::read_to_string(&finished).expect("read");
    fs::write(&damaged, text.replacen("keep me", "lose me", 1)).expect("write");

    let server = serve(&[&finished, &halted]);
    let runs = json(&server, "/api/runs");
    assert_eq!(runs["b"]["state"]["state"], "halted");
    let last = runs["b"]["steps"]
        .as_array()
        .expect("steps")
        .last()
        .expect("a step")
        .clone();
    assert_eq!(last["mark"], "halt");

    let diff = json(&server, "/api/diff");
    let first = &diff["divergences"][0];
    assert_eq!(first["step"], 2);
    assert_eq!(first["what"], "policy.decision");
    assert!(
        first["a"]["seq"].is_u64() && first["b"]["seq"].is_u64(),
        "{first}"
    );
    let steps = diff["steps"].as_array().expect("steps");
    assert_eq!(steps[0]["divergences"], 0);
    assert_eq!(steps[2]["b"], Value::Null);
    drop(server);

    let server = serve(&[&damaged]);
    let run = &json(&server, "/api/runs")["a"];
    assert!(
        run["damaged"].as_str().expect("why").contains("hash chain"),
        "{run}"
    );
    // What could be read is still served.
    assert_eq!(run["state"]["state"], "finished");
    assert!(!json(&server, "/api/run/a/step/1")
        .as_array()
        .expect("events")
        .is_empty());
}
