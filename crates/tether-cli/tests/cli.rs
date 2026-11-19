use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::json;
use tether_trace::{NewEvent, TraceWriter};
use tether_types::{EventType, Payload, RunId, RunState, ToolCall, ToolResult};
use time::OffsetDateTime;

fn tether(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tether"))
        .args(args)
        .output()
        .expect("run tether")
}

fn write_trace(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("run.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_test".into())).expect("create");
    let bodies = [
        EventType::RunStateChanged {
            state: RunState::Running,
            reason: "start".into(),
        },
        EventType::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "sandbox.exec".into(),
            server: "sandbox".into(),
            arguments: json!({"argv": ["ls"]}),
            external: false,
        }),
        EventType::ToolResult(ToolResult {
            id: "call_1".into(),
            content: Payload::Inline(json!("README.md")),
            is_error: false,
            duration_ms: 4,
        }),
    ];
    for body in bodies {
        w.append(NewEvent {
            ts: OffsetDateTime::UNIX_EPOCH,
            step: 1,
            agent: "main".into(),
            parent: None,
            body,
        })
        .expect("append");
    }
    path
}

#[test]
fn verify_and_inspect_read_a_sound_trace() {
    let path = write_trace("cli_sound");
    let path = path.to_str().expect("utf-8 path");

    let out = tether(&["verify", path]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("ok: 3 events, 0 blobs"));

    let out = tether(&["inspect", path]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[0].contains("run.state") && lines[0].contains("running: start"),
        "{text}"
    );
    assert!(
        lines[1].contains(r#"sandbox.exec {"argv":["ls"]}"#),
        "{text}"
    );
    assert!(lines[2].contains(r#"ok, 4 ms, "README.md""#), "{text}");
}

#[test]
fn verify_fails_on_a_tampered_trace() {
    let path = write_trace("cli_tampered");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(&path, text.replace("README.md", "SECRETS.md")).expect("write");

    let out = tether(&["verify", path.to_str().expect("utf-8 path")]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("hash chain broken at seq 2"),
        "{out:?}"
    );
}

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

/// Run the scripted agent in a fresh directory, typing `stdin` at it.
/// Returns the directory and the process output.
fn tether_run(test: &str, policy: &str, stdin: &str) -> (PathBuf, Output) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    fs::write(dir.join("script.json"), SCRIPT).expect("write script");
    fs::write(dir.join("tether.toml"), policy).expect("write policy");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .arg("run")
        .arg("--script")
        .arg(dir.join("script.json"))
        .arg("--policy")
        .arg(dir.join("tether.toml"))
        .arg("--dir")
        .arg(dir.join("out"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tether");
    // Dropping the handle closes stdin once the answers are written.
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    (dir, child.wait_with_output().expect("wait"))
}

/// `tether inspect` of the one trace in the run directory.
fn inspect_run(dir: &std::path::Path) -> String {
    let trace = fs::read_dir(dir.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace");
    let out = tether(&["inspect", trace.to_str().expect("utf-8 path")]);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(unix)]
#[test]
fn run_asks_at_the_terminal_and_yes_lets_the_call_through() {
    let (dir, out) = tether_run("cli_run_yes", POLICY, "y\n");
    assert!(out.status.success(), "{out:?}");
    let prompt = String::from_utf8_lossy(&out.stderr);
    assert!(
        prompt.contains(r#"Allow sandbox.exec {"argv":["rm","notes.txt"]}?"#),
        "{prompt}"
    );
    assert!(prompt.contains("rule: no-rm"), "{prompt}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("finished: Done."),
        "{out:?}"
    );

    // The file was written, then really removed.
    assert!(!dir.join("out/workspace/notes.txt").exists());
    let trace = inspect_run(&dir);
    assert!(trace.contains("allow by cli"), "{trace}");
    assert!(trace.contains("ask: Allow sandbox.exec"), "{trace}");
}

#[cfg(unix)]
#[test]
fn run_no_denies_the_call_and_the_agent_carries_on() {
    // An unrecognised line is asked again rather than taken as an answer.
    let (dir, out) = tether_run("cli_run_no", POLICY, "maybe\nn\n");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr)
            .matches("allow? [y/n]")
            .count(),
        2
    );

    assert_eq!(
        fs::read_to_string(dir.join("out/workspace/notes.txt")).expect("notes.txt"),
        "keep me\n"
    );
    let trace = inspect_run(&dir);
    assert!(trace.contains("deny by cli"), "{trace}");
    assert!(trace.contains("error, 0 ms, \"denied by cli\""), "{trace}");
    assert!(trace.contains("finished: agent done"), "{trace}");
}

#[cfg(unix)]
#[test]
fn run_with_nobody_to_answer_halts() {
    let (dir, out) = tether_run("cli_run_unanswered", POLICY, "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("run halted: ask unanswered"),
        "{out:?}"
    );
    // The trace path is still reported, and the file is untouched.
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("trace: "),
        "{out:?}"
    );
    assert!(dir.join("out/workspace/notes.txt").exists());
    assert!(inspect_run(&dir).contains("halted: ask unanswered after 0 s"));
}

#[cfg(unix)]
#[test]
fn run_on_timeout_deny_needs_nobody() {
    let policy = format!("{POLICY}on_timeout = \"deny\"\n");
    let (dir, out) = tether_run("cli_run_timeout_deny", &policy, "");
    assert!(out.status.success(), "{out:?}");
    assert!(dir.join("out/workspace/notes.txt").exists());
    assert!(inspect_run(&dir).contains("deny by timeout"));
}

#[test]
fn run_rejects_a_bad_policy_before_starting() {
    let (dir, out) = tether_run("cli_run_bad_policy", "[[rule]]\nactoin = \"allow\"\n", "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("loading policy"),
        "{out:?}"
    );
    assert!(!dir.join("out").exists());
}
