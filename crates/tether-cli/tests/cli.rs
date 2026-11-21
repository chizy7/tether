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
    tether_run_script(test, SCRIPT, policy, stdin)
}

/// The same, for a script other than [`SCRIPT`].
fn tether_run_script(test: &str, script: &str, policy: &str, stdin: &str) -> (PathBuf, Output) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    fs::write(dir.join("script.json"), script).expect("write script");
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

/// Start the scripted run with stdin held open, wait until it is sitting at
/// the `rm` prompt, do `stop` to it, and return how it ended.
#[cfg(unix)]
fn stop_at_the_prompt(test: &str, stop: impl FnOnce(u32, &std::path::Path)) -> (PathBuf, Output) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    fs::write(dir.join("script.json"), SCRIPT).expect("write script");
    fs::write(dir.join("tether.toml"), POLICY).expect("write policy");

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
    // Held, not written to: the prompt gets no answer.
    let stdin = child.stdin.take().expect("stdin");

    let asked = |dir: &std::path::Path| {
        let entries = fs::read_dir(dir.join("out")).ok()?;
        let trace = entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))?;
        fs::read_to_string(&trace)
            .ok()?
            .contains("\"policy.ask\"")
            .then_some(trace)
    };
    let mut trace = None;
    for _ in 0..200 {
        trace = asked(&dir);
        if trace.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let trace = trace.expect("the run never reached the prompt");

    stop(child.id(), &trace);
    let out = child.wait_with_output().expect("wait");
    drop(stdin);
    (dir, out)
}

#[cfg(unix)]
#[test]
fn sigint_at_the_prompt_halts_the_run_and_the_trace_says_why() {
    let (dir, out) = stop_at_the_prompt("cli_sigint", |pid, _| {
        let status = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .expect("send SIGINT");
        assert!(status.success());
    });

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run halted: kill switch: SIGINT"),
        "{stderr}"
    );
    // The file the agent wanted gone is still there.
    assert!(dir.join("out/workspace/notes.txt").exists());

    let trace = inspect_run(&dir);
    assert!(
        trace.trim_end().ends_with("halted: kill switch: sigint"),
        "{trace}"
    );
}

#[cfg(unix)]
#[test]
fn halt_file_at_the_prompt_halts_the_run() {
    let (dir, out) = stop_at_the_prompt("cli_halt_file", |_, trace| {
        fs::write(trace.with_extension("halt"), "").expect("write halt file");
    });

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run halted: kill switch: halt file"),
        "{stderr}"
    );
    assert!(dir.join("out/workspace/notes.txt").exists());

    // Stopped between events: the chain is intact.
    let trace = fs::read_dir(dir.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace");
    let verified = tether(&["verify", trace.to_str().expect("utf-8 path")]);
    assert!(verified.status.success(), "{verified:?}");
}

#[cfg(unix)]
#[test]
fn refuses_example_blocks_rm_and_the_trace_names_the_rule() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/refuses");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli_refuses_example");
    let _ = fs::remove_dir_all(&dir);

    let out = Command::new(env!("CARGO_BIN_EXE_tether"))
        .arg("run")
        .arg("--script")
        .arg(example.join("script.json"))
        .arg("--policy")
        .arg(example.join("tether.toml"))
        .arg("--dir")
        .arg(dir.join("out"))
        .stdin(Stdio::null())
        .output()
        .expect("run tether");
    // Nothing asks, so the run needs nobody and finishes.
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("finished: I was not allowed"),
        "{out:?}"
    );

    // The note the agent saved survived its own `rm -rf .`.
    assert_eq!(
        fs::read_to_string(dir.join("out/workspace/notes.txt")).expect("notes.txt"),
        "work in progress, do not lose\n"
    );

    let trace = inspect_run(&dir);
    let lines: Vec<&str> = trace.lines().collect();
    let denied = lines
        .iter()
        .position(|line| line.contains("rule no-rm"))
        .expect("a decision by rule no-rm");
    assert!(
        lines[denied - 1].contains(r#"tool.call        exec {"argv":["rm","-rf","."]}"#),
        "{trace}"
    );
    assert!(
        lines[denied].contains("policy.decision  deny: deleting files is off limits in this run"),
        "{trace}"
    );
    assert!(
        lines[denied + 1].contains("tool.result      error"),
        "{trace}"
    );
    // The later `ls` shows the file is still there.
    assert!(trace.contains(r#""stdout":"notes.txt\n""#), "{trace}");
}

/// The one trace a `tether_run` left in `dir`.
fn trace_in(dir: &std::path::Path) -> String {
    fs::read_dir(dir.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace")
        .to_str()
        .expect("utf-8 path")
        .to_owned()
}

#[cfg(unix)]
#[test]
fn diff_of_two_runs_that_did_the_same_is_empty() {
    // Two separate runs: different run ids, timestamps and durations.
    let (first, _) = tether_run("cli_diff_same_a", POLICY, "y\n");
    let (second, _) = tether_run("cli_diff_same_b", POLICY, "y\n");

    let out = tether(&["diff", &trace_in(&first), &trace_in(&second)]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("no divergence"), "{text}");
    // One line per step, all the same.
    assert_eq!(text.matches("  same").count(), 3, "{text}");

    // A trace against itself, too.
    let out = tether(&["diff", &trace_in(&first), &trace_in(&first)]);
    assert_eq!(out.status.code(), Some(0));
}

#[cfg(unix)]
#[test]
fn diff_reports_the_first_divergence_and_exits_1() {
    // The same script and policy; the human says yes in one run and no in the other.
    let (yes, _) = tether_run("cli_diff_yes", POLICY, "y\n");
    let (no, _) = tether_run("cli_diff_no", POLICY, "n\n");

    let out = tether(&["diff", &trace_in(&yes), &trace_in(&no)]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("first divergence: step 2, policy.decision: the outcome differs"),
        "{text}"
    );
    assert!(text.contains("ask, answered allow (rule no-rm)"), "{text}");
    assert!(text.contains("ask, answered deny (rule no-rm)"), "{text}");
    // The next request then carries a different tool result, and the diff
    // shows the two messages from the blobs.
    assert!(text.contains("more after it"), "{text}");

    // A changed policy is called out, and a denied call differs from an asked one.
    let deny = POLICY.replace("action = \"ask\"", "action = \"deny\"");
    let (denied, _) = tether_run("cli_diff_deny", &deny, "");
    let out = tether(&["diff", &trace_in(&yes), &trace_in(&denied)]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("held to different policies"), "{text}");
    assert!(
        text.contains("deny (rule no-rm: denied by rule no-rm)"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn diff_shows_the_messages_behind_a_changed_request() {
    // The same run, asked in different words: the first request differs.
    let reworded = SCRIPT.replace("Tidy up.", "Tidy up, and be careful.");
    let (asked, _) = tether_run("cli_diff_msg_a", POLICY, "y\n");
    let (reasked, _) = tether_run_script("cli_diff_msg_b", &reworded, POLICY, "y\n");

    let out = tether(&["diff", &trace_in(&asked), &trace_in(&reasked)]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(
            "first divergence: step 1, model.request: message 1 differs (1 messages in a, 1 in b)"
        ),
        "{text}"
    );
    // The messages themselves, read from each trace's blobs.
    assert!(text.contains("a's message: user: Tidy up."), "{text}");
    assert!(
        text.contains("b's message: user: Tidy up, and be careful."),
        "{text}"
    );
}

#[test]
fn diff_of_a_missing_or_broken_trace_exits_2() {
    let good = write_trace("cli_diff_good");
    let good = good.to_str().expect("utf-8 path");

    let out = tether(&["diff", good, "/no/such/trace.jsonl"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("opening /no/such/trace.jsonl"),
        "{out:?}"
    );

    let broken = write_trace("cli_diff_broken");
    let text = fs::read_to_string(&broken).expect("read");
    fs::write(&broken, text.replace("README.md", "SECRETS.md")).expect("write");
    let out = tether(&["diff", good, broken.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("hash chain broken"),
        "{out:?}"
    );
}
