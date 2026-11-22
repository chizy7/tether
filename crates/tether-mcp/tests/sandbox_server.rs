#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tether_mcp::sandbox_server::SandboxServer;
use tether_mcp::McpClient;
use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Sandbox, SandboxSpec};
use tether_types::tool::Payload;
use tether_types::ToolCall;

async fn server(test: &str) -> SandboxServer {
    let workspace = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&workspace);
    let sandbox = Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(10),
        ..SandboxSpec::new(workspace)
    };
    let handle = sandbox.start(&spec).await.expect("start");
    SandboxServer::new(sandbox, handle)
}

async fn call(server: &SandboxServer, name: &str, arguments: Value) -> (Value, bool) {
    let call = ToolCall {
        id: "call_1".into(),
        name: name.into(),
        server: "sandbox".into(),
        arguments,
        external: false,
    };
    let result = server.call(&call).await.expect("call");
    assert_eq!(result.id, "call_1");
    let Payload::Inline(content) = result.content else {
        panic!("expected inline content")
    };
    (content, result.is_error)
}

#[tokio::test]
async fn lists_the_four_sandbox_tools() {
    let tools = server("server_list")
        .await
        .list_tools()
        .await
        .expect("list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["exec", "read_file", "write_file", "list_dir"]);
    assert!(tools.iter().all(|t| t.server == "sandbox" && !t.external));
}

#[tokio::test]
async fn files_and_commands_work_through_tool_calls() {
    let server = server("server_calls").await;

    let (content, is_error) = call(
        &server,
        "write_file",
        json!({"path": "notes/a.txt", "content": "hi\n"}),
    )
    .await;
    assert_eq!((content, is_error), (json!({"bytes": 3}), false));

    let (content, is_error) = call(&server, "read_file", json!({"path": "notes/a.txt"})).await;
    assert_eq!((content, is_error), (json!("hi\n"), false));

    let (content, is_error) = call(&server, "list_dir", json!({"path": "notes"})).await;
    assert_eq!((content, is_error), (json!(["a.txt"]), false));

    let (content, is_error) = call(&server, "exec", json!({"argv": ["cat", "notes/a.txt"]})).await;
    assert_eq!(
        (content, is_error),
        (
            json!({"exit_code": 0, "stdout": "hi\n", "stderr": ""}),
            false
        )
    );
}

#[tokio::test]
async fn failures_come_back_as_error_results() {
    let server = server("server_errors").await;

    let (content, is_error) = call(&server, "exec", json!({"argv": ["sh", "-c", "exit 2"]})).await;
    assert!(is_error);
    assert_eq!(content["exit_code"], 2);

    for (name, arguments) in [
        ("read_file", json!({"path": "../outside"})),
        ("read_file", json!({"path": "missing.txt"})),
        ("exec", json!({"argv": "not a list"})),
        ("format_disk", json!({})),
    ] {
        let (content, is_error) = call(&server, name, arguments).await;
        assert!(is_error && content.is_string(), "{name}: {content}");
    }
}

#[tokio::test]
async fn arguments_given_as_a_list_are_refused() {
    // serde would fill the argument struct from a list, position by
    // position, and a rule looking for `argv` or `path` by name would have
    // seen nothing: the call would run past every rule.
    let server = server("server_positional").await;
    let (_, is_error) = call(
        &server,
        "write_file",
        json!({"path": "victim.txt", "content": "keep"}),
    )
    .await;
    assert!(!is_error);

    for (tool, arguments) in [
        ("exec", json!([["rm", "victim.txt"], null, null])),
        ("read_file", json!(["victim.txt"])),
        ("write_file", json!(["victim.txt", "overwritten"])),
        ("list_dir", json!(["."])),
    ] {
        let (content, is_error) = call(&server, tool, arguments).await;
        assert!(is_error, "{tool} took a list: {content}");
        assert!(content.to_string().contains("JSON object"), "{content}");
    }
    let (content, _) = call(&server, "read_file", json!({"path": "victim.txt"})).await;
    assert_eq!(content, "keep");
}

#[tokio::test]
async fn list_dir_stays_in_the_workspace() {
    let server = server("server_list_dir").await;
    for outside in ["/etc", "..", "a/../..", "/"] {
        let (content, is_error) = call(&server, "list_dir", json!({"path": outside})).await;
        assert!(is_error, "listed {outside}: {content}");
        assert!(
            content.to_string().contains("leaves the workspace"),
            "{content}"
        );
    }
    let (_, is_error) = call(&server, "list_dir", json!({})).await;
    assert!(!is_error);
}
