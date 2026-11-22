#![allow(clippy::unwrap_used)]

//! The rmcp-backed client against a real stdio MCP server
//! (`tests/fixtures/fake_server.py`, no dependencies beyond python3).

use std::sync::Arc;

use serde_json::json;
use tether_mcp::client::ExternalServer;
use tether_mcp::sandbox_server::{SandboxServer, SERVER};
use tether_mcp::{McpClient, McpError, Registry, ServerConfig, Transport};
use tether_sandbox::{process::ProcessSandbox, Sandbox, SandboxSpec};
use tether_types::tool::Payload;
use tether_types::ToolCall;

fn fake(id: &str) -> ServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_server.py");
    ServerConfig::stdio(id, "python3", vec![script.to_owned()])
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        server: String::new(),
        arguments,
        external: false,
    }
}

fn text(result: &tether_types::ToolResult) -> String {
    match &result.content {
        Payload::Inline(v) => v
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string()),
        Payload::Blob { .. } => panic!("unexpected blob"),
    }
}

#[tokio::test]
async fn connects_lists_and_calls() {
    let server = ExternalServer::connect(fake("fake"))
        .await
        .expect("connect");
    let info = server.info();
    assert_eq!(info.id, "fake");
    assert_eq!(info.transport, "stdio");
    assert_eq!(info.server_name.as_deref(), Some("fake-mcp"));
    assert!(info.protocol_version.is_some());

    let tools = server.list_tools().await.expect("list");
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "{names:?}");
    assert!(tools.iter().all(|t| t.external && t.server == "fake"));

    let ok = server
        .call(&call("echo", json!({"text": "hi"})))
        .await
        .expect("call");
    assert!(!ok.is_error);
    assert_eq!(text(&ok), "hi");

    let failed = server.call(&call("fail", json!({}))).await.expect("call");
    assert!(failed.is_error);
    assert_eq!(text(&failed), "fail tool failed");

    // A tool the server does not have is a JSON-RPC error, which comes back
    // as an error result rather than failing the run.
    let unknown = server.call(&call("nope", json!({}))).await.expect("call");
    assert!(unknown.is_error);

    server.shutdown().await;
    server.shutdown().await; // idempotent
}

#[tokio::test]
async fn call_timeout_is_a_tool_error() {
    let mut config = fake("slowpoke");
    config.timeout_secs = 1;
    let server = ExternalServer::connect(config).await.expect("connect");
    let result = server
        .call(&call("slow", json!({"secs": 3})))
        .await
        .expect("call");
    assert!(result.is_error);
    assert!(
        text(&result).contains("longer than 1s"),
        "{}",
        text(&result)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn child_gets_only_the_named_env() {
    std::env::set_var("TETHER_TEST_PASSED", "yes");
    std::env::set_var("TETHER_TEST_SCRUBBED", "secret");
    let mut config = fake("envy");
    config.transport = Transport::Stdio {
        command: "python3".into(),
        args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_server.py").into()],
        env: vec!["PATH".into(), "TETHER_TEST_PASSED".into()],
    };
    let server = ExternalServer::connect(config).await.expect("connect");
    let passed = server
        .call(&call("env", json!({"name": "TETHER_TEST_PASSED"})))
        .await
        .unwrap();
    assert_eq!(text(&passed), "\"yes\"");
    let scrubbed = server
        .call(&call("env", json!({"name": "TETHER_TEST_SCRUBBED"})))
        .await
        .unwrap();
    assert_eq!(text(&scrubbed), "null");
    server.shutdown().await;
}

#[tokio::test]
async fn bad_command_is_a_connect_error() {
    let config = ServerConfig::stdio("ghost", "/nonexistent/mcp-server", vec![]);
    match ExternalServer::connect(config).await {
        Err(McpError::Connect { server, .. }) => assert_eq!(server, "ghost"),
        Ok(_) => panic!("connected to a nonexistent command"),
        Err(other) => panic!("expected Connect error, got {other:?}"),
    }
}

#[tokio::test]
async fn unsupported_protocol_version_is_refused() {
    let mut config = fake("old");
    if let Transport::Stdio { args, .. } = &mut config.transport {
        args.extend(["--protocol".to_owned(), "1999-01-01".to_owned()]);
    }
    match ExternalServer::connect(config).await {
        Err(McpError::ProtocolMismatch { version, .. }) => assert_eq!(version, "1999-01-01"),
        // rmcp may reject the handshake itself before Tether sees the version.
        Err(McpError::Connect { .. }) => {}
        Ok(_) => panic!("connected on an unsupported protocol version"),
        Err(other) => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn validate_rejects_builtin_id_and_bad_ids() {
    assert!(ServerConfig::stdio(SERVER, "x", vec![]).validate().is_err());
    assert!(ServerConfig::stdio("has space", "x", vec![])
        .validate()
        .is_err());
    assert!(ServerConfig::stdio("ok-id_1", "x", vec![])
        .validate()
        .is_ok());
    assert!(ServerConfig::http("h", "ftp://nope").validate().is_err());
    assert!(ServerConfig::http("h", "http://127.0.0.1:1/mcp")
        .validate()
        .is_ok());
}

#[tokio::test]
async fn registry_merges_prefixes_and_routes() {
    let dir = tempdir();
    let sandbox: Arc<dyn Sandbox> = Arc::new(ProcessSandbox::new());
    let handle = sandbox
        .start(&SandboxSpec::new(dir.clone()))
        .await
        .expect("sandbox");
    let builtin: Arc<dyn McpClient> = Arc::new(SandboxServer::new(sandbox.clone(), handle));
    let external: Arc<dyn McpClient> =
        Arc::new(ExternalServer::connect(fake("fake")).await.unwrap());

    let registry = Registry::build(vec![(SERVER.into(), builtin), ("fake".into(), external)])
        .await
        .expect("registry");

    let tools = registry.list_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    // Sandbox keeps the bare name; the external duplicate is prefixed.
    assert!(names.contains(&"read_file"), "{names:?}");
    assert!(names.contains(&"fake.read_file"), "{names:?}");
    assert!(names.contains(&"echo"), "{names:?}");
    let ext = tools.iter().find(|t| t.name == "fake.read_file").unwrap();
    assert_eq!(ext.server, "fake");
    assert!(ext.external);
    let builtin_read = tools.iter().find(|t| t.name == "read_file").unwrap();
    assert_eq!(builtin_read.server, SERVER);
    assert!(!builtin_read.external);

    // Routing: the prefixed name reaches the external server under its own name.
    let r = registry
        .call(&call("fake.read_file", json!({})))
        .await
        .unwrap();
    assert_eq!(text(&r), "external read_file");
    let r = registry
        .call(&call("echo", json!({"text": "via registry"})))
        .await
        .unwrap();
    assert_eq!(text(&r), "via registry");
    match registry.call(&call("missing", json!({}))).await {
        Err(McpError::UnknownTool(name)) => assert_eq!(name, "missing"),
        other => panic!("expected UnknownTool, got {other:?}"),
    }

    let servers = registry.servers();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0].id, SERVER);
    assert_eq!(servers[0].transport, "builtin");
    assert_eq!(servers[1].id, "fake");
    assert_eq!(servers[1].transport, "stdio");

    registry.shutdown().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn registry_rejects_duplicate_server_ids() {
    let a: Arc<dyn McpClient> = Arc::new(ExternalServer::connect(fake("dup")).await.unwrap());
    let b: Arc<dyn McpClient> = Arc::new(ExternalServer::connect(fake("dup")).await.unwrap());
    match Registry::build(vec![("dup".into(), a.clone()), ("dup".into(), b.clone())]).await {
        Err(McpError::DuplicateServer(id)) => assert_eq!(id, "dup"),
        other => panic!("expected DuplicateServer, got {:?}", other.err()),
    }
    a.shutdown().await;
    b.shutdown().await;
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tether-mcp-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
