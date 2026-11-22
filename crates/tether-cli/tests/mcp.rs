//! `tether` against an external MCP server: the fake stdio server from
//! `tether-mcp`'s fixtures, which needs only python3.
//!
//! What is covered here is what only the binary can show: that the paths
//! which go live after a recording (`replay --live-after-divergence`, and so
//! `eval --system`, and `resume`) have the external server's tools and not
//! just the sandbox's, and that they start a server a trace names only when
//! told to.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FAKE_SERVER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tether-mcp/tests/fixtures/fake_server.py"
);

fn tether(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tether"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run tether")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A fresh directory with a policy that names the fake server, allows it
/// and the sandbox, and asks about the server's `slow` tool. Starting the
/// server leaves `started.marker` in the directory, so a test can tell
/// whether it was started.
fn setup(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    let policy = format!(
        r#"
[budget]
ask_timeout_secs = 1

[[rule]]
name = "ask-slow"
match = {{ tool = "fake.slow" }}
action = "ask"

[[rule]]
name = "fake-ok"
match = {{ server = "fake" }}
action = "allow"

[[rule]]
name = "sandbox-ok"
match = {{ server = "sandbox" }}
action = "allow"

[[mcp_server]]
id = "fake"
transport = "stdio"
command = "sh"
args = ["-c", "touch started.marker; exec python3 {FAKE_SERVER}"]
timeout_secs = 20
"#
    );
    fs::write(dir.join("tether.toml"), policy).expect("write policy");
    dir
}

fn run(dir: &Path, script: &str) -> (PathBuf, Output) {
    fs::write(dir.join("script.json"), script).expect("write script");
    let out = tether(
        dir,
        &[
            "run",
            "--script",
            "script.json",
            "--policy",
            "tether.toml",
            "--dir",
            "out",
        ],
    );
    let trace = fs::read_dir(dir.join("out"))
        .expect("the run made a directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("the run wrote a trace");
    (trace, out)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether the server has been started since the marker was last removed.
fn started(dir: &Path) -> bool {
    dir.join("started.marker").exists()
}

fn forget_start(dir: &Path) {
    let _ = fs::remove_file(dir.join("started.marker"));
}

/// `tether replay` of `trace` under a changed system prompt, carrying on
/// live, so every tool call after the first request is made for real.
fn live_replay(dir: &Path, trace: &Path, name: &str, extra: &[&str]) -> (PathBuf, Output) {
    let live = dir.join("out").join(name);
    let mut args = vec![
        "replay".to_owned(),
        trace.to_string_lossy().into_owned(),
        "--system".into(),
        "Be brief.".into(),
        "--live-after-divergence".into(),
        "--script".into(),
        "script.json".into(),
        "--out".into(),
        live.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = tether(dir, &args);
    (live, out)
}

fn inspect(dir: &Path, trace: &Path) -> String {
    stdout(&tether(dir, &["inspect", &trace.to_string_lossy()]))
}

const TWO_CALLS: &str = r#"{"task": "t", "replies": [
  [{"type": "tool_use", "id": "c1", "name": "echo", "arguments": {"text": "over mcp"}}],
  [{"type": "tool_use", "id": "c2", "name": "fake__read_file", "arguments": {}}],
  [{"type": "text", "text": "done"}]
]}"#;

#[test]
fn inspect_shows_the_server_of_an_external_call() {
    let dir = setup("mcp_inspect");
    let (trace, out) = run(&dir, TWO_CALLS);
    assert!(out.status.success(), "{out:?}");

    let shown = inspect(&dir, &trace);
    assert!(
        shown.contains("fake.echo {\"text\":\"over mcp\"}"),
        "{shown}"
    );
    // The agent called the colliding tool by its prefixed name; a rule and
    // `inspect` both read it as `<server>.<tool>`.
    assert!(shown.contains("fake.read_file {}"), "{shown}");
    assert!(shown.contains("\"external read_file\""), "{shown}");
    assert!(shown.contains("rule fake-ok"), "{shown}");
}

#[test]
fn a_replay_that_goes_live_has_the_external_tools_when_the_trace_is_trusted() {
    let dir = setup("mcp_live_replay");
    let (trace, out) = run(&dir, TWO_CALLS);
    assert!(out.status.success(), "{out:?}");
    assert!(started(&dir));

    // A plain replay stays on the recording and starts nothing.
    forget_start(&dir);
    let plain = dir.join("out").join("plain.jsonl");
    let out = tether(
        &dir,
        &[
            "replay",
            &trace.to_string_lossy(),
            "--out",
            &plain.to_string_lossy(),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        !started(&dir),
        "a replay on the recording started the server"
    );

    let (live, out) = live_replay(&dir, &trace, "live.jsonl", &["--trust-recorded-servers"]);
    assert!(out.status.success(), "{out:?}");
    assert!(started(&dir));
    // What is about to run is said first.
    assert!(
        stderr(&out).contains("`fake` runs: sh -c"),
        "{}",
        stderr(&out)
    );

    let shown = inspect(&dir, &live);
    assert!(
        shown.contains("ok, ") && shown.contains("\"over mcp\""),
        "{shown}"
    );
    assert!(shown.contains("\"external read_file\""), "{shown}");
    assert!(shown.contains("finished: agent done"), "{shown}");
}

#[test]
fn a_replay_that_goes_live_does_not_start_a_server_the_trace_names() {
    let dir = setup("mcp_untrusted_replay");
    let (trace, out) = run(&dir, TWO_CALLS);
    assert!(out.status.success(), "{out:?}");

    // The trace's policy names a command. Without the user's say-so it is
    // not run: a trace can come from anyone.
    forget_start(&dir);
    let (live, out) = live_replay(&dir, &trace, "live.jsonl", &[]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !started(&dir),
        "a server named only by the trace was started"
    );
    assert!(
        stderr(&out).contains("not starting MCP server `fake`"),
        "{}",
        stderr(&out)
    );

    // The agent's calls to that server fail and say why.
    let shown = inspect(&dir, &live);
    assert!(
        shown.contains("MCP server `fake` was not started"),
        "{shown}"
    );
    // Only a result can hold this text: neither call got through.
    assert!(!shown.contains("\"external read_file\""), "{shown}");

    // A policy file the user passes is theirs, and its servers are started.
    forget_start(&dir);
    let (live, out) = live_replay(&dir, &trace, "own.jsonl", &["--policy", "tether.toml"]);
    assert!(out.status.success(), "{out:?}");
    assert!(started(&dir));
    assert!(inspect(&dir, &live).contains("\"external read_file\""));
}

#[test]
fn a_resumed_run_has_the_external_tools() {
    let dir = setup("mcp_resume");
    // The third call is to a tool the policy asks about, and nobody answers.
    let (trace, out) = run(
        &dir,
        r#"{"task": "t", "replies": [
          [{"type": "tool_use", "id": "c1", "name": "write_file", "arguments": {"path": "a.txt", "content": "hi\n"}}],
          [{"type": "tool_use", "id": "c2", "name": "echo", "arguments": {"text": "before"}}],
          [{"type": "tool_use", "id": "c3", "name": "slow", "arguments": {"secs": 0}}],
          [{"type": "text", "text": "not reached"}]
        ]}"#,
    );
    assert!(!out.status.success(), "the run should have halted: {out:?}");
    assert!(inspect(&dir, &trace).contains("halted: ask unanswered"));

    fs::write(
        dir.join("after.json"),
        r#"{"task": "t", "replies": [
          [{"type": "tool_use", "id": "c4", "name": "echo", "arguments": {"text": "after"}}],
          [{"type": "tool_use", "id": "c5", "name": "read_file", "arguments": {"path": "a.txt"}}],
          [{"type": "text", "text": "resumed"}]
        ]}"#,
    )
    .expect("write script");
    // Resuming would start the trace's server again, so it takes saying so.
    forget_start(&dir);
    let out = tether(
        &dir,
        &["resume", &trace.to_string_lossy(), "--script", "after.json"],
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(
        !started(&dir),
        "resume started a server without being told to"
    );
    assert!(
        stderr(&out).contains("--trust-recorded-servers"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("`fake` runs: sh -c"),
        "{}",
        stderr(&out)
    );

    let out = tether(
        &dir,
        &[
            "resume",
            &trace.to_string_lossy(),
            "--script",
            "after.json",
            "--trust-recorded-servers",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(started(&dir));

    let shown = inspect(&dir, &trace);
    assert!(shown.contains("running: resumed"), "{shown}");
    // The external tool works after the resume, and the sandbox file written
    // before the halt was put back by running that call again.
    assert!(shown.contains("\"after\""), "{shown}");
    assert!(shown.contains("\"hi\\n\""), "{shown}");
    assert!(!shown.contains("has no tool"), "{shown}");
    assert!(shown.contains("finished: agent done"), "{shown}");
    assert!(tether(&dir, &["verify", &trace.to_string_lossy()])
        .status
        .success());
}

#[test]
fn replaying_a_trace_from_before_mcp_adds_nothing_to_run_created() {
    let dir = setup("mcp_old_trace");
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tether-trace/tests/fixtures/refuses.jsonl"
    );
    let replayed = dir.join("replay.jsonl");
    let out = tether(
        &dir,
        &["replay", fixture, "--out", &replayed.to_string_lossy()],
    );
    assert!(out.status.success(), "{out:?}");

    let first = |path: &Path| -> serde_json::Value {
        let text = fs::read_to_string(path).expect("read trace");
        serde_json::from_str(text.lines().next().expect("a first event")).expect("json")
    };
    let (old, new) = (first(Path::new(fixture)), first(&replayed));
    assert_eq!(old["type"], "run.created");
    assert!(old["data"].get("mcp_servers").is_none());
    // Written by v0.3.0, replayed today: the same `run.created`.
    assert_eq!(old["data"], new["data"]);
}
