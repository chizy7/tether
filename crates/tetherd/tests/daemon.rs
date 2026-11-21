//! The daemon driven over a real Unix socket, as an SDK would drive it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tetherd::proto::tether_client::TetherClient;
use tetherd::proto::{
    AskAnswer, CreateRunRequest, FinishRequest, ModelRequest, Run, RunRef, ToolCall,
    ToolCallRequest,
};
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

const SCRIPT: &str = r#"{
  "replies": [
    [{"type": "tool_use", "id": "call_1", "name": "write_file",
      "arguments": {"path": "notes.txt", "content": "from the sdk\n"}}],
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

/// A daemon serving in this process, and where its files are.
struct Served {
    dir: tempfile::TempDir,
    socket: PathBuf,
    daemon: JoinHandle<anyhow::Result<()>>,
}

impl Served {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

async fn wait_for(socket: &Path) -> TetherClient<Channel> {
    for _ in 0..200 {
        if let Ok(client) = tetherd::connect(socket).await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no daemon on {}", socket.display());
}

async fn serve(idle: Duration) -> (Served, TetherClient<Channel>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("d.sock");
    std::fs::write(dir.path().join("script.json"), SCRIPT).expect("write script");
    std::fs::write(dir.path().join("tether.toml"), POLICY).expect("write policy");
    let daemon = tokio::spawn({
        let socket = socket.clone();
        async move { tetherd::serve(&socket, idle).await }
    });
    let client = wait_for(&socket).await;
    (
        Served {
            dir,
            socket,
            daemon,
        },
        client,
    )
}

fn create(served: &Served) -> CreateRunRequest {
    CreateRunRequest {
        policy_path: served.path("tether.toml").to_string_lossy().into_owned(),
        provider: "fake".into(),
        script_path: served.path("script.json").to_string_lossy().into_owned(),
        dir: served.path("runs").to_string_lossy().into_owned(),
        ..CreateRunRequest::default()
    }
}

fn ask(run: &Run, messages: Value) -> ModelRequest {
    ModelRequest {
        run_id: run.run_id.clone(),
        model: "fake-1".into(),
        messages_json: messages.to_string(),
        tools: run.tools.iter().map(|tool| tool.name.clone()).collect(),
        params_json: String::new(),
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn call(run: &Run, call: &ToolCall) -> ToolCallRequest {
    ToolCallRequest {
        run_id: run.run_id.clone(),
        call: Some(call.clone()),
    }
}

/// The types of the events in a trace file.
fn types(trace: &str) -> Vec<String> {
    std::fs::read_to_string(trace)
        .expect("read trace")
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("event")["type"]
                .as_str()
                .expect("type")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn an_agent_runs_end_to_end_over_the_socket_and_the_daemon_then_exits() {
    let (served, mut client) = serve(Duration::from_millis(400)).await;

    // The socket is this user's alone.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&served.socket)
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let run = client
        .create_run(create(&served))
        .await
        .expect("create")
        .into_inner();
    let names: Vec<&str> = run.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["exec", "read_file", "write_file", "list_dir"]);
    assert!(run.tools[0].input_schema_json.contains("argv"));

    // Tail the trace from another task while the agent works.
    let mut tail = client.clone();
    let run_ref = RunRef {
        run_id: run.run_id.clone(),
    };
    let events = tokio::spawn(async move {
        let mut stream = tail.events(run_ref).await.expect("events").into_inner();
        let mut seen = Vec::new();
        while let Some(event) = stream.message().await.expect("event") {
            seen.push(event);
        }
        seen
    });

    // The agent's loop, in the client: ask, run the tool, ask again.
    let mut history = vec![user("Tidy up.")];
    let first = client
        .complete(ask(&run, json!(history)))
        .await
        .expect("complete")
        .into_inner();
    assert_eq!(first.tool_calls.len(), 1);
    assert_eq!(first.tool_calls[0].name, "write_file");
    history.push(serde_json::from_str(&first.message_json).expect("message"));

    let written = client
        .call_tool(call(&run, &first.tool_calls[0]))
        .await
        .expect("call")
        .into_inner();
    assert_eq!((written.id.as_str(), written.is_error), ("call_1", false));
    assert_eq!(written.content_json, r#"{"bytes":13}"#);
    let workspace = served
        .path("runs")
        .join(format!("{}.workspace", run.run_id));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes.txt")).expect("notes"),
        "from the sdk\n"
    );

    // The second turn asks for `rm`, which the policy asks a human about.
    history.push(json!({"role": "tool", "content": [
        {"type": "tool_result", "id": "call_1", "content": {"bytes": 13}}
    ]}));
    let second = client
        .complete(ask(&run, json!(history)))
        .await
        .expect("complete")
        .into_inner();
    let rm = second.tool_calls[0].clone();
    let mut asker = client.clone();
    let request = call(&run, &rm);
    let pending = tokio::spawn(async move { asker.call_tool(request).await });

    // The call waits. Find the ask in the trace, then answer it.
    let mut subject = None;
    for _ in 0..200 {
        let text = std::fs::read_to_string(&run.trace_path).expect("read trace");
        subject = text.lines().find_map(|line| {
            let event: Value = serde_json::from_str(line).ok()?;
            (event["type"] == "policy.ask").then(|| event["data"]["subject"].as_u64())?
        });
        if subject.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let subject = subject.expect("the ask in the trace");
    assert!(!pending.is_finished());
    // An answer about the wrong event is refused and the call keeps waiting.
    let wrong = AskAnswer {
        run_id: run.run_id.clone(),
        subject_seq: subject + 100,
        allow: true,
        answered_by: String::new(),
    };
    assert_eq!(
        client.answer(wrong).await.expect_err("no such ask").code(),
        Code::NotFound
    );
    let yes = AskAnswer {
        run_id: run.run_id.clone(),
        subject_seq: subject,
        allow: true,
        answered_by: "tester".into(),
    };
    client.answer(yes).await.expect("answer");

    let removed = pending.await.expect("join").expect("call").into_inner();
    assert!(!removed.is_error, "{}", removed.content_json);
    assert!(!workspace.join("notes.txt").exists());

    let finish = FinishRequest {
        run_id: run.run_id.clone(),
        outcome_json: r#""Done.""#.into(),
    };
    client.finish(finish.clone()).await.expect("finish");
    // A finished run takes no more calls.
    let again = client.finish(finish).await.expect_err("already finished");
    assert_eq!(again.code(), Code::FailedPrecondition);

    // The trace is whole, and the event stream delivered all of it, in order.
    let report = tether_trace::verify(&run.trace_path).expect("verify");
    let seen = events.await.expect("join");
    assert_eq!(seen.len() as u64, report.events);
    let on_disk = std::fs::read_to_string(&run.trace_path).expect("read trace");
    let streamed: Vec<&str> = seen.iter().map(|event| event.json.as_str()).collect();
    assert_eq!(streamed, on_disk.lines().collect::<Vec<_>>());
    assert_eq!(seen[0].r#type, "run.created");
    assert_eq!(seen.last().expect("last").r#type, "run.finished");
    assert!(on_disk.contains(r#""answered_by":"tester""#));

    // Its last run over and nothing listening, the daemon exits by itself
    // and takes its socket with it.
    drop(client);
    tokio::time::timeout(Duration::from_secs(10), served.daemon)
        .await
        .expect("the daemon must exit when idle")
        .expect("join")
        .expect("serve");
    assert!(!served.socket.exists());
}

#[tokio::test]
async fn an_ask_answered_no_fails_the_call_and_halt_stops_the_run() {
    let (served, mut client) = serve(Duration::from_secs(30)).await;
    let run = client
        .create_run(create(&served))
        .await
        .expect("create")
        .into_inner();
    client
        .complete(ask(&run, json!([user("Tidy up.")])))
        .await
        .expect("complete");

    // Straight to the `rm`: asked about, and this time refused.
    let rm = ToolCall {
        id: "call_x".into(),
        name: "exec".into(),
        arguments_json: r#"{"argv": ["rm", "-rf", "."]}"#.into(),
    };
    let mut asker = client.clone();
    let request = call(&run, &rm);
    let pending = tokio::spawn(async move { asker.call_tool(request).await });
    let subject = loop {
        let text = std::fs::read_to_string(&run.trace_path).expect("read trace");
        let found = text.lines().find_map(|line| {
            let event: Value = serde_json::from_str(line).ok()?;
            (event["type"] == "policy.ask").then(|| event["data"]["subject"].as_u64())?
        });
        if let Some(subject) = found {
            break subject;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let no = AskAnswer {
        run_id: run.run_id.clone(),
        subject_seq: subject,
        allow: false,
        answered_by: String::new(),
    };
    client.answer(no).await.expect("answer");
    let refused = pending.await.expect("join").expect("call").into_inner();
    assert!(refused.is_error);
    assert_eq!(refused.content_json, r#""denied by sdk""#);

    // Halt, with no call in flight: the run halts now and takes nothing more.
    let run_ref = RunRef {
        run_id: run.run_id.clone(),
    };
    client.halt(run_ref.clone()).await.expect("halt");
    let after = client
        .complete(ask(&run, json!([user("More.")])))
        .await
        .expect_err("halted");
    assert_eq!(after.code(), Code::FailedPrecondition);
    assert!(after.message().contains("Halted"), "{}", after.message());
    assert_eq!(
        types(&run.trace_path).last().map(String::as_str),
        Some("run.state")
    );
    let text = std::fs::read_to_string(&run.trace_path).expect("read trace");
    assert!(text.contains("kill switch: halt requested"));
    tether_trace::verify(&run.trace_path).expect("verify");

    // An event stream opened on a run that is over gets it all and ends.
    let mut stream = client.events(run_ref).await.expect("events").into_inner();
    let mut count = 0;
    while stream.message().await.expect("event").is_some() {
        count += 1;
    }
    assert_eq!(count, text.lines().count());
    served.daemon.abort();
}

#[tokio::test]
async fn bad_requests_are_refused_with_a_status_that_says_why() {
    let (served, mut client) = serve(Duration::from_secs(30)).await;

    let nobody = RunRef {
        run_id: "run_nope".into(),
    };
    assert_eq!(
        client
            .halt(nobody.clone())
            .await
            .expect_err("no run")
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client.events(nobody).await.expect_err("no run").code(),
        Code::NotFound
    );

    let docker = CreateRunRequest {
        sandbox: "docker".into(),
        ..create(&served)
    };
    assert_eq!(
        client.create_run(docker).await.expect_err("docker").code(),
        Code::Unimplemented
    );
    let provider = CreateRunRequest {
        provider: "llama".into(),
        ..create(&served)
    };
    assert_eq!(
        client
            .create_run(provider)
            .await
            .expect_err("provider")
            .code(),
        Code::Unimplemented
    );
    let policy = CreateRunRequest {
        policy_path: served.path("missing.toml").to_string_lossy().into_owned(),
        ..create(&served)
    };
    assert_eq!(
        client.create_run(policy).await.expect_err("policy").code(),
        Code::InvalidArgument
    );
    std::fs::write(served.path("bad.toml"), "[[rule]]\nactoin = \"allow\"\n").expect("write");
    let policy = CreateRunRequest {
        policy_path: served.path("bad.toml").to_string_lossy().into_owned(),
        ..create(&served)
    };
    assert_eq!(
        client.create_run(policy).await.expect_err("policy").code(),
        Code::InvalidArgument
    );
    // None of those left a run behind.
    assert!(
        !served.path("runs").exists()
            || std::fs::read_dir(served.path("runs"))
                .expect("read")
                .count()
                == 0
    );

    let run = client
        .create_run(create(&served))
        .await
        .expect("create")
        .into_inner();
    let garbled = ModelRequest {
        messages_json: "not json".into(),
        ..ask(&run, json!([]))
    };
    assert_eq!(
        client.complete(garbled).await.expect_err("json").code(),
        Code::InvalidArgument
    );
    let unknown = ModelRequest {
        tools: vec!["teleport".into()],
        ..ask(&run, json!([user("hi")]))
    };
    assert_eq!(
        client.complete(unknown).await.expect_err("tool").code(),
        Code::InvalidArgument
    );
    let missing = ToolCallRequest {
        run_id: run.run_id.clone(),
        call: None,
    };
    assert_eq!(
        client.call_tool(missing).await.expect_err("call").code(),
        Code::InvalidArgument
    );
    // Refused requests wrote nothing: the trace is still just the start.
    assert_eq!(types(&run.trace_path), ["run.created", "run.state"]);
    served.daemon.abort();
}

#[tokio::test]
async fn the_binary_starts_on_a_socket_exits_when_idle_and_yields_to_one_already_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("d.sock");
    // A leftover socket file from a daemon that died is replaced.
    std::fs::write(&socket, "").expect("stale file");

    let start = |idle: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tetherd"))
            .arg("--socket")
            .arg(&socket)
            .args(["--idle-secs", idle])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn tetherd")
    };
    let mut first = start("1");
    let mut client = wait_for(&socket).await;
    assert_eq!(
        client
            .halt(RunRef { run_id: "x".into() })
            .await
            .expect_err("no run")
            .code(),
        Code::NotFound
    );

    // A second daemon on the same socket leaves the first to it.
    let mut second = start("30");
    let yielded = tokio::task::spawn_blocking(move || second.wait())
        .await
        .expect("join")
        .expect("wait");
    assert!(yielded.success());
    assert!(first.try_wait().expect("try_wait").is_none());

    // With no run, the first exits after its idle second.
    drop(client);
    let exited = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || first.wait()),
    )
    .await
    .expect("tetherd must exit when idle")
    .expect("join")
    .expect("wait");
    assert!(exited.success());
    assert!(!socket.exists());
}
