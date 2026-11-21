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

/// Replay the one trace in `dir` with extra arguments, typing `stdin` at it.
/// Returns the paths of the original and the replay, and the output.
fn tether_replay(dir: &std::path::Path, args: &[&str], stdin: &str) -> (String, String, Output) {
    let original = trace_in(dir);
    let replayed = original.replace(".jsonl", ".replay.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .arg("replay")
        .arg(&original)
        .args(args)
        // A replay must never reach for credentials or the network.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
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
    (original, replayed, child.wait_with_output().expect("wait"))
}

/// A trace's events with what is allowed to differ in a replay taken out:
/// the run id and the hash chain.
fn events_of(trace: &str) -> Vec<serde_json::Value> {
    fs::read_to_string(trace)
        .expect("read trace")
        .lines()
        .map(|line| {
            let mut event: serde_json::Value = serde_json::from_str(line).expect("event");
            let fields = event.as_object_mut().expect("object");
            fields.remove("run");
            fields.remove("hash");
            event
        })
        .collect()
}

/// The acceptance test for replay: `tether diff` of a trace and its replay
/// is empty. Checked the strict way too: every event is the same, down to
/// timestamps and timings.
fn assert_faithful(original: &str, replayed: &str, out: &Output) {
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("every request was answered from the recording"),
        "{said}"
    );

    let diff = tether(&["diff", original, replayed]);
    let text = String::from_utf8_lossy(&diff.stdout);
    assert_eq!(diff.status.code(), Some(0), "{text}");
    assert!(text.contains("no divergence"), "{text}");

    let (before, after) = (events_of(original), events_of(replayed));
    assert_eq!(before.len(), after.len());
    for (old, new) in before.iter().zip(&after) {
        assert_eq!(old, new);
    }
    let verified = tether(&["verify", replayed]);
    assert!(verified.status.success(), "{verified:?}");
}

#[cfg(unix)]
#[test]
fn replay_of_a_finished_run_is_the_same_trace_and_touches_nothing() {
    let (dir, _) = tether_run("cli_replay_finished", POLICY, "y\n");
    // The original run removed notes.txt. Put a marker where it was: a
    // replay must not run the tools again.
    let marker = dir.join("out/workspace/notes.txt");
    fs::write(&marker, "left alone").expect("write marker");

    // Stdin is closed: the answer to the ask comes from the recording.
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);

    assert_eq!(fs::read_to_string(&marker).expect("marker"), "left alone");
    // No workspace is made for a replay that stays on its recording.
    assert!(!PathBuf::from(replayed.replace(".jsonl", ".workspace")).exists());
    // The new trace has its own run id.
    let first = fs::read_to_string(&replayed).expect("read");
    assert!(
        first.lines().next().expect("line").contains("-replay\""),
        "{first}"
    );
}

#[cfg(unix)]
#[test]
fn replay_of_a_denied_call_and_of_the_refuses_example_is_faithful() {
    // Answered no: the denial and the failed result replay as recorded.
    let (dir, _) = tether_run("cli_replay_no", POLICY, "n\n");
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);

    // A rule's denial, evaluated again from the recorded policy.
    let deny = POLICY.replace("action = \"ask\"", "action = \"deny\"");
    let (dir, _) = tether_run("cli_replay_deny", &deny, "");
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);

    // An ask nobody answered, denied by on_timeout.
    let timeout = format!("{POLICY}on_timeout = \"deny\"\n");
    let (dir, _) = tether_run("cli_replay_timeout_deny", &timeout, "");
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);
}

#[cfg(unix)]
#[test]
fn replay_of_a_halted_run_halts_the_same_way() {
    // Nobody answered the ask.
    let (dir, out) = tether_run("cli_replay_unanswered", POLICY, "");
    assert!(!out.status.success());
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);
    assert!(inspect_run_at(&replayed).contains("halted: ask unanswered after 0 s"));

    // Out of budget between two tool calls.
    let budget = format!("[budget]\nmax_tool_calls = 1\n{POLICY}");
    let (dir, out) = tether_run("cli_replay_budget", &budget, "y\n");
    assert!(!out.status.success());
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);
    assert!(inspect_run_at(&replayed).contains("halted: budget exceeded: max_tool_calls (1 of 1)"));

    // Stopped by Ctrl-C while it waited at the prompt.
    let (dir, _) = stop_at_the_prompt("cli_replay_sigint", |pid, _| {
        let status = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .expect("send SIGINT");
        assert!(status.success());
    });
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    assert_faithful(&original, &replayed, &out);
    assert!(inspect_run_at(&replayed).contains("halted: kill switch: sigint"));
}

fn inspect_run_at(trace: &str) -> String {
    let out = tether(&["inspect", trace]);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(unix)]
#[test]
fn replay_under_a_stricter_policy_shows_what_it_would_have_denied() {
    // Recorded with the human saying yes to `rm`.
    let (dir, _) = tether_run("cli_replay_stricter", POLICY, "y\n");
    let deny = dir.join("deny.toml");
    fs::write(
        &deny,
        POLICY.replace("action = \"ask\"", "action = \"deny\""),
    )
    .expect("write");

    let (original, replayed, out) =
        tether_replay(&dir, &["--policy", deny.to_str().expect("utf-8 path")], "");
    // The replay runs to the end: a denial needs nothing the recording lacks.
    assert!(out.status.success(), "{out:?}");

    let diff = tether(&["diff", &original, &replayed]);
    let text = String::from_utf8_lossy(&diff.stdout);
    assert_eq!(diff.status.code(), Some(1), "{text}");
    assert!(text.contains("held to different policies"), "{text}");
    assert!(
        text.contains("first divergence: step 2, policy.decision: the outcome differs"),
        "{text}"
    );
    assert!(
        text.contains("a (seq 16): ask, answered allow (rule no-rm)"),
        "{text}"
    );
    assert!(
        text.contains("deny (rule no-rm: denied by rule no-rm)"),
        "{text}"
    );
    assert!(inspect_run_at(&replayed).contains("finished: agent done"));
}

#[cfg(unix)]
#[test]
fn replay_under_a_looser_policy_stops_where_the_recording_has_no_result() {
    // Recorded with `rm` denied, so the recording has no real result for it.
    let deny = POLICY.replace("action = \"ask\"", "action = \"deny\"");
    let (dir, _) = tether_run("cli_replay_looser", &deny, "");
    let allow = dir.join("allow.toml");
    fs::write(
        &allow,
        POLICY.replace("action = \"ask\"", "action = \"allow\""),
    )
    .expect("write");

    let (_, replayed, out) =
        tether_replay(&dir, &["--policy", allow.to_str().expect("utf-8 path")], "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("left the recording and stopped: tool call exec ran in this replay but was not run in the recording"),
        "{said}"
    );
    // The original workspace still has the file the agent wanted gone.
    assert!(dir.join("out/workspace/notes.txt").exists());
    assert!(inspect_run_at(&replayed).contains("allow (seq 15, rule no-rm)"));
}

/// What a stand-in live model says once a replay has gone live.
const LIVE_SCRIPT: &str = r#"{
  "task": "unused",
  "replies": [
    [{"type": "tool_use", "id": "live_1", "name": "write_file",
      "arguments": {"path": "live.txt", "content": "written live\n"}}],
    [{"type": "text", "text": "Done, live."}]
  ]
}"#;

#[cfg(unix)]
#[test]
fn a_changed_system_prompt_stops_the_replay_or_goes_live() {
    let (dir, _) = tether_run("cli_replay_system", POLICY, "y\n");

    // Without the flag the replay stops at the first request, which the
    // recording has no response to.
    let (_, replayed, out) = tether_replay(&dir, &["--system", "Be careful."], "");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("left the recording and stopped: model request has no recorded response"),
        "{said}"
    );
    assert!(inspect_run_at(&replayed).contains("halted: model error: replay: no recorded response"));
    fs::remove_file(&replayed).expect("remove first replay");

    // With it, the stand-in live model answers and its tool call really runs,
    // in a fresh workspace beside the new trace.
    let script = dir.join("live.json");
    fs::write(&script, LIVE_SCRIPT).expect("write script");
    let (original, replayed, out) = tether_replay(
        &dir,
        &[
            "--system",
            "Be careful.",
            "--live-after-divergence",
            "--script",
            script.to_str().expect("utf-8 path"),
        ],
        "",
    );
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("left the recording and carried on live"),
        "{said}"
    );

    let workspace = PathBuf::from(replayed.replace(".jsonl", ".workspace"));
    assert_eq!(
        fs::read_to_string(workspace.join("live.txt")).expect("live.txt"),
        "written live\n"
    );
    assert!(inspect_run_at(&replayed).contains("outcome \"Done, live.\""));

    let diff = tether(&["diff", &original, &replayed]);
    let text = String::from_utf8_lossy(&diff.stdout);
    assert_eq!(diff.status.code(), Some(1), "{text}");
    assert!(
        text.contains("first divergence: step 1, model.request: message 1 differs"),
        "{text}"
    );
    assert!(text.contains("b's message: system: Be careful."), "{text}");
}

#[cfg(unix)]
#[test]
fn going_live_mid_run_first_brings_the_workspace_up_to_date() {
    // Recorded: notes.txt written, `rm notes.txt` denied.
    let deny = POLICY.replace("action = \"ask\"", "action = \"deny\"");
    let (dir, _) = tether_run("cli_replay_catch_up", &deny, "");
    let allow = dir.join("allow.toml");
    fs::write(
        &allow,
        POLICY.replace("action = \"ask\"", "action = \"allow\""),
    )
    .expect("write");
    let script = dir.join("live.json");
    fs::write(
        &script,
        r#"{"task": "unused", "replies": [[{"type": "text", "text": "Removed it."}]]}"#,
    )
    .expect("write script");

    // Now `rm` is allowed. The replay goes live at that call, in a fresh
    // workspace, which must already hold notes.txt for `rm` to remove.
    let (_, replayed, out) = tether_replay(
        &dir,
        &[
            "--policy",
            allow.to_str().expect("utf-8 path"),
            "--live-after-divergence",
            "--script",
            script.to_str().expect("utf-8 path"),
        ],
        "",
    );
    assert!(out.status.success(), "{out:?}");

    let trace = inspect_run_at(&replayed);
    // `rm` ran for real and succeeded, which it only can if the file was there.
    assert!(trace.contains(r#""exit_code":0"#), "{trace}");
    assert!(trace.contains("outcome \"Removed it.\""), "{trace}");
    let workspace = PathBuf::from(replayed.replace(".jsonl", ".workspace"));
    assert!(workspace.is_dir());
    assert!(!workspace.join("notes.txt").exists());
    // The recorded run's own workspace is untouched.
    assert!(dir.join("out/workspace/notes.txt").exists());
}

#[test]
fn replay_refuses_to_overwrite_and_reports_a_bad_trace() {
    let good = write_trace("cli_replay_bad");
    // That hand-made trace does not start with run.created.
    let out = tether(&["replay", good.to_str().expect("utf-8 path")]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not start with run.created"),
        "{out:?}"
    );
}

#[cfg(unix)]
#[test]
fn replay_restores_elapsed_time_without_waiting_for_it() {
    // A run with a real one-second wait in it, halted by its wall budget.
    let script = r#"{
      "task": "Wait.",
      "replies": [
        [{"type": "tool_use", "id": "call_1", "name": "exec", "arguments": {"argv": ["sleep", "1"]}}],
        [{"type": "text", "text": "Never said."}]
      ]
    }"#;
    let policy = "[budget]\nmax_wall_secs = 1\n\n[[rule]]\nname = \"all\"\naction = \"allow\"\n";
    let (dir, out) = tether_run_script("cli_replay_wall", script, policy, "");
    assert!(!out.status.success());
    assert!(inspect_run(&dir).contains("halted: budget exceeded: max_wall_secs"));

    let started = std::time::Instant::now();
    let (original, replayed, out) = tether_replay(&dir, &[], "");
    // The sleep is not run again, and its recorded duration is in the trace.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    assert_faithful(&original, &replayed, &out);
    assert!(inspect_run_at(&replayed).contains("halted: budget exceeded: max_wall_secs"));
}

/// The replies the scripted model still owes after its first turn: the
/// script a resume carries on with.
const REST_OF_SCRIPT: &str = r#"{
  "task": "unused",
  "replies": [
    [{"type": "tool_use", "id": "call_2", "name": "exec",
      "arguments": {"argv": ["rm", "notes.txt"]}}],
    [{"type": "text", "text": "Done."}]
  ]
}"#;

/// Resume the one trace in `dir` with the rest of the script, typing `stdin`.
fn tether_resume(dir: &std::path::Path, stdin: &str) -> Output {
    let script = dir.join("rest.json");
    fs::write(&script, REST_OF_SCRIPT).expect("write script");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .arg("resume")
        .arg(trace_in(dir))
        .arg("--script")
        .arg(&script)
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

/// A resumed run did the same things as one that was never interrupted.
fn assert_same_as_uninterrupted(test: &str, resumed: &std::path::Path) {
    let (straight, out) = tether_run(test, POLICY, "y\n");
    assert!(out.status.success(), "{out:?}");
    let diff = tether(&["diff", &trace_in(&straight), &trace_in(resumed)]);
    let text = String::from_utf8_lossy(&diff.stdout);
    assert_eq!(diff.status.code(), Some(0), "{text}");
    assert!(text.contains("no divergence"), "{text}");
}

#[cfg(unix)]
#[test]
fn resume_carries_a_halted_run_to_the_end_in_the_same_trace() {
    // Halted in step 2: nobody answered the ask about `rm`.
    let (dir, out) = tether_run("cli_resume_halted", POLICY, "");
    assert!(!out.status.success());
    let before = fs::read_to_string(trace_in(&dir)).expect("read");

    let out = tether_resume(&dir, "y\n");
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("finished: Done."),
        "{out:?}"
    );

    // The same trace, appended to: what was there is untouched and the
    // chain still verifies across the resume.
    let after = fs::read_to_string(trace_in(&dir)).expect("read");
    assert!(after.starts_with(&before));
    let verified = tether(&["verify", &trace_in(&dir)]);
    assert!(verified.status.success(), "{verified:?}");

    let trace = inspect_run(&dir);
    let lines: Vec<&str> = trace.lines().collect();
    let halted = lines
        .iter()
        .position(|l| l.contains("halted: ask unanswered"))
        .expect("halt");
    // The resume marker follows the halt, in the step of the checkpoint.
    assert!(
        lines[halted + 1].contains("step 1") && lines[halted + 1].contains("running: resumed"),
        "{trace}"
    );
    // Step 2 is then done again from its request.
    assert!(
        lines[halted + 2].contains("step 2") && lines[halted + 2].contains("model.request"),
        "{trace}"
    );
    assert!(trace.contains("allow by cli"), "{trace}");
    assert!(
        trace
            .trim_end()
            .lines()
            .last()
            .expect("line")
            .contains("run.finished"),
        "{trace}"
    );

    // A fresh workspace: notes.txt was put back by running the earlier
    // write again, then really removed by the resumed `rm`.
    let workspace = PathBuf::from(trace_in(&dir).replace(".jsonl", ".resume-1.workspace"));
    assert!(workspace.is_dir());
    assert!(!workspace.join("notes.txt").exists());
    assert!(trace.contains(r#""exit_code":0"#), "{trace}");
    // The halted run's own workspace is left as it was.
    assert!(dir.join("out/workspace/notes.txt").exists());

    assert_same_as_uninterrupted("cli_resume_halted_straight", &dir);
}

#[cfg(unix)]
#[test]
fn resume_recovers_a_run_that_crashed_mid_write() {
    // A whole run, cut off partway through step 2 as a crash would leave it:
    // no halt, and half an event at the end.
    let (dir, _) = tether_run("cli_resume_crashed", POLICY, "y\n");
    let trace = trace_in(&dir);
    let text = fs::read_to_string(&trace).expect("read");
    let cut = text
        .find("\"type\":\"policy.ask\"")
        .expect("the ask in step 2");
    fs::write(&trace, &text[..cut]).expect("truncate");
    assert!(!tether(&["verify", &trace]).status.success());

    let out = tether_resume(&dir, "y\n");
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dropped an unfinished last line"),
        "{out:?}"
    );
    assert!(tether(&["verify", &trace]).status.success());
    assert!(inspect_run(&dir).contains("running: resumed"));

    assert_same_as_uninterrupted("cli_resume_crashed_straight", &dir);
}

#[cfg(unix)]
#[test]
fn a_run_can_be_resumed_more_than_once() {
    let (dir, _) = tether_run("cli_resume_twice", POLICY, "");
    // Resumed, and again nobody answers: halted a second time.
    let out = tether_resume(&dir, "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ask unanswered"),
        "{out:?}"
    );

    let out = tether_resume(&dir, "y\n");
    assert!(out.status.success(), "{out:?}");
    let trace = inspect_run(&dir);
    assert_eq!(trace.matches("running: resumed").count(), 2, "{trace}");
    assert_eq!(
        trace.matches("halted: ask unanswered").count(),
        2,
        "{trace}"
    );
    assert!(tether(&["verify", &trace_in(&dir)]).status.success());
    // Each resume gets its own workspace.
    assert!(PathBuf::from(trace_in(&dir).replace(".jsonl", ".resume-2.workspace")).is_dir());

    // Both abandoned attempts at step 2 are left out of the comparison.
    assert_same_as_uninterrupted("cli_resume_twice_straight", &dir);
}

#[cfg(unix)]
#[test]
fn resume_between_steps_carries_on_from_the_finished_step() {
    // The sleep spends the one-second wall budget, so the run halts as it
    // asks for step 2, with step 1 finished and checkpointed.
    let script = r#"{
      "task": "Wait.",
      "replies": [
        [{"type": "tool_use", "id": "call_1", "name": "exec", "arguments": {"argv": ["sleep", "1"]}}],
        [{"type": "text", "text": "Never said."}]
      ]
    }"#;
    let policy = "[budget]\nmax_wall_secs = 1\n\n[[rule]]\nname = \"all\"\naction = \"allow\"\n";
    let (dir, out) = tether_run_script("cli_resume_between", script, policy, "");
    assert!(!out.status.success());

    // Elapsed time carries on from the checkpoint, so the budget is still
    // spent and the resumed run halts again, before asking the model.
    let out = tether_resume(&dir, "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("budget exceeded: max_wall_secs"),
        "{out:?}"
    );
    let trace = inspect_run(&dir);
    let tail: Vec<&str> = trace.lines().rev().take(2).collect();
    assert!(tail[1].contains("running: resumed"), "{trace}");
    assert!(
        tail[0].contains("halted: budget exceeded: max_wall_secs"),
        "{trace}"
    );
    assert!(tether(&["verify", &trace_in(&dir)]).status.success());
}

#[cfg(unix)]
#[test]
fn resume_refuses_what_it_cannot_resume() {
    // A finished run.
    let (dir, _) = tether_run("cli_resume_finished", POLICY, "y\n");
    let out = tether_resume(&dir, "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the run finished"),
        "{out:?}"
    );

    // A halt file still in place would stop the run again at once.
    let (dir, _) = tether_run("cli_resume_halt_file", POLICY, "");
    let halt_file = trace_in(&dir).replace(".jsonl", ".halt");
    fs::write(&halt_file, "").expect("write halt file");
    let before = fs::read_to_string(trace_in(&dir)).expect("read");
    let out = tether_resume(&dir, "y\n");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("remove it first"),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(trace_in(&dir)).expect("read"), before);

    // A resumed trace cannot be replayed yet, and says so.
    fs::remove_file(&halt_file).expect("remove halt file");
    assert!(tether_resume(&dir, "y\n").status.success());
    let (_, _, out) = tether_replay(&dir, &[], "");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("replaying a resumed trace is not supported yet"),
        "{out:?}"
    );
}
