//! `tether eval` on suites recorded here with the scripted model. Nothing
//! needs a key: where a task goes live, a script stands in for the model.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn tether(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tether"))
        .args(args)
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

fn path(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

/// Writes `notes.txt`, removes it, reports.
const TIDY: &str = r#"{
  "task": "Tidy up.",
  "replies": [
    [{"type": "tool_use", "id": "call_1", "name": "write_file",
      "arguments": {"path": "notes.txt", "content": "keep me\n"}}],
    [{"type": "tool_use", "id": "call_2", "name": "exec",
      "arguments": {"argv": ["rm", "notes.txt"]}}],
    [{"type": "text", "text": "Tidied: notes.txt is gone."}]
  ]
}"#;

/// Writes `hello.txt` and reports.
const HELLO: &str = r#"{
  "task": "Say hello in a file.",
  "replies": [
    [{"type": "tool_use", "id": "call_1", "name": "write_file",
      "arguments": {"path": "./hello.txt", "content": "hello, world\n"}}],
    [{"type": "text", "text": "Wrote hello.txt."}]
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

/// A fresh directory for one test.
fn fresh(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Record `script` under `POLICY`, answering asks with `stdin`, and put the
/// trace in `suite` as the task `name`. Packed when `packed`.
fn record(suite: &Path, name: &str, script: &str, stdin: &str, packed: bool) {
    let work = suite.join(format!("recording-{name}"));
    fs::create_dir_all(&work).expect("create work dir");
    fs::write(work.join("script.json"), script).expect("write script");
    fs::write(work.join("tether.toml"), POLICY).expect("write policy");
    let out = tether(
        &[
            "run",
            "--script",
            path(&work.join("script.json")),
            "--policy",
            path(&work.join("tether.toml")),
            "--dir",
            path(&work.join("out")),
        ],
        stdin,
    );
    assert!(out.status.success(), "{out:?}");
    let trace = fs::read_dir(work.join("out"))
        .expect("read out dir")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a trace");
    if packed {
        let tar = suite.join(format!("{name}.tar"));
        let out = tether(&["pack", path(&trace), "--out", path(&tar)], "");
        assert!(out.status.success(), "{out:?}");
    } else {
        fs::rename(&trace, suite.join(format!("{name}.jsonl"))).expect("move trace");
        fs::rename(
            trace.with_extension("blobs"),
            suite.join(format!("{name}.blobs")),
        )
        .expect("move blobs");
    }
    fs::remove_dir_all(&work).expect("remove work dir");
}

/// A suite of two tasks, `hello` (packed) and `tidy`, with `eval_toml`.
fn suite(test: &str, eval_toml: &str) -> PathBuf {
    let suite = fresh(test);
    record(&suite, "hello", HELLO, "", true);
    record(&suite, "tidy", TIDY, "y\n", false);
    fs::write(suite.join("eval.toml"), eval_toml).expect("write eval.toml");
    suite
}

fn eval(suite: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut all = vec!["eval", path(suite)];
    all.extend_from_slice(args);
    let out = tether(&all, "");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Assertions the two recorded tasks pass.
const PASSING: &str = r#"
[defaults]
max_steps = 3
max_usd = 0.01

[task.hello]
outcome = "hello.txt"
no_deny = true
tool_called = ["write_file"]
tool_not_called = ["sandbox.exec"]
file_contains = [{ path = "hello.txt", text = "hello" }]

[task.tidy]
outcome = "Tidied"
tool_called = ["sandbox.write_file", "exec"]
"#;

#[test]
fn a_suite_replayed_unchanged_passes_and_needs_no_model() {
    let suite = suite("eval_passes", PASSING);
    let (code, out, err) = eval(&suite, &[]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("2 tasks, nothing changed"), "{out}");
    // One row per task, in name order: result, steps, usd, first divergence.
    let rows: Vec<Vec<&str>> = out
        .lines()
        .filter(|line| line.starts_with("hello") || line.starts_with("tidy"))
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(
        rows,
        [
            ["hello", "pass", "2", "0.0000", "none"],
            ["tidy", "pass", "3", "0.0000", "none"],
        ],
        "{out}"
    );
    assert!(out.contains("2 passed, 0 failed, $0.0000 in all"), "{out}");

    // The new traces are in the suite's own directory, and are sound.
    for task in ["hello", "tidy"] {
        let trace = suite.join(".eval").join(format!("{task}.jsonl"));
        let verified = tether(&["verify", path(&trace)], "");
        assert!(verified.status.success(), "{verified:?}");
    }
    // That directory is replaced on the next run, not refused.
    let (code, out, err) = eval(&suite, &["--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let report: Value = serde_json::from_str(&out).expect("one JSON report");
    assert_eq!(
        report["totals"],
        json!({"passed": 2, "failed": 0, "cost_usd": 0.0}),
    );
}

#[test]
fn json_reports_a_completed_suite_with_a_failed_assertion() {
    let suite = suite("eval_json_mixed", "[defaults]\nmax_steps = 2\n");
    let (code, out, err) = eval(&suite, &["--json"]);
    assert_eq!(code, Some(1), "{out}{err}");
    let report: Value = serde_json::from_str(&out).expect("one JSON report");
    assert_eq!(
        report,
        json!({
            "report_version": 1,
            "tasks": [
                {
                    "name": "hello", "status": "pass", "steps": 2, "cost_usd": 0.0,
                    "failures": [], "first_divergence": null
                },
                {
                    "name": "tidy", "status": "fail", "steps": 3, "cost_usd": 0.0,
                    "failures": [{"check": "max_steps", "reason": "took 3 steps, the limit is 2"}],
                    "first_divergence": null
                }
            ],
            "totals": {"passed": 1, "failed": 1, "cost_usd": 0.0},
            "traces_dir": path(&suite.join(".eval")),
        }),
    );
}

#[test]
fn each_assertion_that_fails_is_named_and_the_exit_code_is_1() {
    let suite = suite(
        "eval_fails",
        r#"
[defaults]
max_steps = 2

[task.hello]
outcome = "goodbye"
tool_called = ["exec"]
tool_not_called = ["write_file"]
file_contains = [
  { path = "hello.txt", text = "farewell" },
  { path = "other.txt", text = "x" },
]

[task.tidy]
max_usd = 0.0
"#,
    );
    let (code, out, err) = eval(&suite, &[]);
    assert_eq!(code, Some(1), "{out}{err}");
    for expected in [
        "hello  FAIL",
        "tidy   FAIL",
        "\nhello:\n",
        r#"  outcome: wanted it to contain "goodbye", got "Wrote hello.txt.""#,
        "  tool_called: exec was never called",
        "  tool_not_called: write_file was called (seq ",
        r#"  file_contains: hello.txt does not contain "farewell""#,
        "  file_contains: other.txt was never written with write_file",
        "\ntidy:\n  max_steps: took 3 steps, the limit is 2",
        "0 passed, 2 failed",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    // The task under its cost limit is not failed for it.
    assert!(!out.contains("max_usd"), "{out}");
}

#[test]
fn no_deny_fails_a_refused_ask_and_a_call_a_new_policy_denies() {
    let suite = fresh("eval_denied");
    // The `rm` is asked about and refused; the agent carries on.
    record(&suite, "refused", TIDY, "n\n", false);
    fs::write(suite.join("eval.toml"), "[defaults]\nno_deny = true\n").expect("write");
    let (code, out, _) = eval(&suite, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("  no_deny: seq "), "{out}");
    assert!(out.contains("was refused by cli"), "{out}");

    // Held to a policy that denies writes outright, the replay shows what
    // that policy would have refused, and the rule is named.
    let strict = suite.join("strict.toml");
    fs::write(
        &strict,
        "[[rule]]\nname = \"read-only\"\nmatch = { tool = \"sandbox.write_file\" }\naction = \"deny\"\nreason = \"no writes\"\n",
    )
    .expect("write policy");
    let (code, out, _) = eval(&suite, &["--policy", path(&strict)]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("step 1, policy.decision"), "{out}");
    assert!(
        out.contains("was denied by rule read-only: no writes"),
        "{out}"
    );

    // Without the assertion a denial is not a failure by itself.
    fs::write(suite.join("eval.toml"), "").expect("write");
    assert_eq!(eval(&suite, &["--policy", path(&strict)]).0, Some(0));
}

/// What the stand-in model says to `hello` under the new prompt: the same
/// file, in capitals.
const HELLO_LIVE: &str = r#"{
  "task": "unused",
  "replies": [
    [{"type": "tool_use", "id": "live_1", "name": "write_file",
      "arguments": {"path": "hello.txt", "content": "HELLO\n"}}],
    [{"type": "text", "text": "Wrote hello.txt, loudly."}]
  ]
}"#;

/// And to `tidy`: it reaches for `rm` at once, and nobody is there to say yes.
const TIDY_LIVE: &str = r#"{
  "task": "unused",
  "replies": [
    [{"type": "tool_use", "id": "live_1", "name": "exec",
      "arguments": {"argv": ["rm", "-rf", "notes.txt"]}}],
    [{"type": "text", "text": "Tidied."}]
  ]
}"#;

fn scripts(suite: &Path) -> PathBuf {
    let scripts = suite.join("scripts");
    fs::create_dir_all(&scripts).expect("create scripts dir");
    fs::write(scripts.join("hello.json"), HELLO_LIVE).expect("write script");
    fs::write(scripts.join("tidy.json"), TIDY_LIVE).expect("write script");
    scripts
}

#[test]
fn a_new_system_prompt_is_answered_live_and_judged_on_the_new_trace() {
    let suite = suite("eval_system", PASSING);
    let scripts = scripts(&suite);
    let (code, out, err) = eval(
        &suite,
        &["--system", "Be loud.", "--scripts", path(&scripts)],
    );
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(out.contains("2 tasks, a new system prompt"), "{out}");

    // `hello` now fails one assertion: the file no longer has the text.
    // Its outcome still names the file, so that one is not reported.
    assert!(out.contains("hello  FAIL"), "{out}");
    assert!(
        out.contains(r#"file_contains: hello.txt does not contain "hello""#),
        "{out}"
    );
    assert!(
        !out.contains("outcome: wanted it to contain \"hello.txt\""),
        "{out}"
    );
    // `tidy` asked for `rm` with nobody to answer, so the run halted.
    assert!(out.contains("tidy   FAIL"), "{out}");
    assert!(out.contains("  the run did not finish: halted:"), "{out}");
    // Both left the recording at the first request.
    assert_eq!(out.matches("step 1, model.request").count(), 2, "{out}");

    // The new trace was written by the stand-in, under the new prompt.
    let inspected = tether(&["inspect", path(&suite.join(".eval/hello.jsonl"))], "");
    let text = String::from_utf8_lossy(&inspected.stdout);
    assert!(text.contains("Wrote hello.txt, loudly."), "{text}");
}

#[test]
fn a_new_model_is_asked_from_the_first_request() {
    let suite = suite("eval_model", "");
    let scripts = scripts(&suite);
    let out_dir = suite.join("new-model");
    let (code, out, err) = eval(
        &suite,
        &[
            "--model",
            "fake-2",
            "--scripts",
            path(&scripts),
            "--out",
            path(&out_dir),
        ],
    );
    // `tidy` halts at its unanswered ask, as above.
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(out.contains("2 tasks, model fake-2"), "{out}");
    assert!(out.contains("hello  pass"), "{out}");
    assert_eq!(out.matches("step 1, model.request").count(), 2, "{out}");

    let trace = fs::read_to_string(out_dir.join("hello.jsonl")).expect("new trace");
    assert!(trace.contains(r#""model":"fake-2""#), "{trace}");
    assert!(!trace.contains(r#""model":"fake-1""#), "{trace}");

    // A directory the user named is not emptied for them.
    let (code, _, err) = eval(&suite, &["--out", path(&out_dir)]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("is not empty"), "{err}");
}

#[test]
fn a_suite_that_cannot_be_run_exits_2() {
    let suite = suite("eval_bad", "[task.hello]\nmax_stepz = 3\n");
    let (code, _, err) = eval(&suite, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("max_stepz"), "{err}");

    let (code, out, err) = eval(&suite, &["--json"]);
    assert_eq!(code, Some(2), "{out}{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("max_stepz"), "{err}");

    fs::write(suite.join("eval.toml"), "[task.missing]\nmax_steps = 3\n").expect("write");
    let (code, _, err) = eval(&suite, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("no trace of that name"), "{err}");

    fs::remove_file(suite.join("eval.toml")).expect("remove");
    let (code, _, err) = eval(&suite, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("eval.toml"), "{err}");

    let empty = fresh("eval_empty");
    fs::write(empty.join("eval.toml"), "").expect("write");
    let (code, _, err) = eval(&empty, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("has no traces"), "{err}");

    // A task with no script when scripts are given fails by itself.
    let suite = suite_with_one_script();
    let (code, out, _) = eval(
        &suite,
        &["--system", "x", "--scripts", path(&suite.join("scripts"))],
    );
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("could not be replayed: no script for this task"),
        "{out}"
    );
    assert!(out.contains("1 passed, 1 failed"), "{out}");

    let traces_dir = suite.join("new traces");
    let (code, out, err) = eval(
        &suite,
        &[
            "--json",
            "--system",
            "x",
            "--scripts",
            path(&suite.join("scripts")),
            "--out",
            path(&traces_dir),
        ],
    );
    assert_eq!(code, Some(1), "{out}{err}");
    let report: Value = serde_json::from_str(&out).expect("one JSON report");
    assert_eq!(report["traces_dir"], path(&traces_dir));
    assert!(traces_dir.join("hello.jsonl").is_file());
    assert_eq!(
        report["tasks"][0]["first_divergence"],
        json!({"step": 1, "event": "model.request"}),
    );
    assert_eq!(
        report["tasks"][1],
        json!({
            "name": "tidy", "status": "fail", "steps": 0, "cost_usd": 0.0,
            "failures": [{
                "check": "replay_error",
                "reason": format!(
                    "could not be replayed: no script for this task at {}",
                    suite.join("scripts/tidy.json").display(),
                ),
            }],
            "first_divergence": null,
        }),
    );
}

fn suite_with_one_script() -> PathBuf {
    let suite = suite("eval_one_script", "");
    scripts(&suite);
    fs::remove_file(suite.join("scripts/tidy.json")).expect("remove");
    suite
}
