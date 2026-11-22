#![cfg(feature = "rmcp")]
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
    assert!(names.contains(&"fake__read_file"), "{names:?}");
    assert!(names.contains(&"echo"), "{names:?}");
    let ext = tools.iter().find(|t| t.name == "fake__read_file").unwrap();
    assert_eq!(ext.server, "fake");
    assert!(ext.external);
    let builtin_read = tools.iter().find(|t| t.name == "read_file").unwrap();
    assert_eq!(builtin_read.server, SERVER);
    assert!(!builtin_read.external);

    // Routing: the prefixed name reaches the external server under its own name.
    let r = registry
        .call(&call("fake__read_file", json!({})))
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

#[tokio::test]
async fn path_reaches_the_child_whatever_env_names() {
    // `env` names one variable and not PATH. The server still needs PATH to
    // find its interpreter, so it is passed regardless.
    std::env::set_var("TETHER_TEST_ONLY_THIS", "yes");
    let mut config = fake("pathy");
    config.transport = Transport::Stdio {
        command: "python3".into(),
        args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_server.py").into()],
        env: vec!["TETHER_TEST_ONLY_THIS".into()],
    };
    let server = ExternalServer::connect(config).await.expect("connect");
    let path = server
        .call(&call("env", json!({"name": "PATH"})))
        .await
        .unwrap();
    assert_ne!(text(&path), "null");
    let home = server
        .call(&call("env", json!({"name": "HOME"})))
        .await
        .unwrap();
    assert_eq!(
        text(&home),
        "null",
        "HOME was not named, so it is not passed"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_server_that_never_answers_initialize_is_a_connect_error() {
    // `sleep` reads nothing and says nothing: the handshake never completes.
    let mut config = ServerConfig::stdio("mute", "sleep", vec!["30".into()]);
    config.timeout_secs = 1;
    let started = std::time::Instant::now();
    match ExternalServer::connect(config).await {
        Err(McpError::Connect { server, reason }) => {
            assert_eq!(server, "mute");
            assert!(reason.contains("initialize"), "{reason}");
        }
        Ok(_) => panic!("connected to a server that never answered"),
        Err(other) => panic!("expected Connect error, got {other:?}"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "gave up after {:?}",
        started.elapsed()
    );
}

/// A streamable HTTP MCP server small enough to read: every POST gets one
/// JSON answer. Its one tool, `whoami`, returns the Authorization header the
/// request came with.
async fn http_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                loop {
                    // One request: headers, then `content-length` bytes.
                    let head_end = loop {
                        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                        let mut chunk = [0u8; 4096];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let header = |name: &str| {
                        head.lines().find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case(name)
                                .then(|| value.trim().to_owned())
                        })
                    };
                    let length: usize = header("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < head_end + length {
                        let mut chunk = [0u8; 4096];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body: serde_json::Value =
                        serde_json::from_slice(&buffer[head_end..head_end + length])
                            .unwrap_or(serde_json::Value::Null);
                    buffer.drain(..head_end + length);

                    let method = head.split(' ').next().unwrap_or("");
                    let answer = if method != "POST" {
                        "HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n".to_owned()
                    } else if body.get("id").is_none() {
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned()
                    } else {
                        let result = match body["method"].as_str() {
                            Some("initialize") => json!({
                                "protocolVersion": body["params"]["protocolVersion"],
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "http-fake", "version": "1"},
                            }),
                            Some("tools/list") => json!({"tools": [{
                                "name": "whoami",
                                "description": "The Authorization header of the request",
                                "inputSchema": {"type": "object", "properties": {}},
                            }]}),
                            Some("tools/call") => json!({
                                "content": [{
                                    "type": "text",
                                    "text": header("authorization").unwrap_or_default(),
                                }],
                                "isError": false,
                            }),
                            _ => json!({}),
                        };
                        let out = json!({"jsonrpc": "2.0", "id": body["id"], "result": result})
                            .to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{out}",
                            out.len()
                        )
                    };
                    if socket.write_all(answer.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{address}/mcp")
}

#[tokio::test]
async fn http_server_is_called_with_headers_filled_in_from_the_environment() {
    std::env::set_var("TETHER_TEST_HTTP_TOKEN", "from-the-environment");
    let mut config = ServerConfig::http("web", http_server().await);
    config.timeout_secs = 10;
    if let Transport::Http { headers, .. } = &mut config.transport {
        headers.insert(
            "Authorization".into(),
            "Bearer ${TETHER_TEST_HTTP_TOKEN}".into(),
        );
    }
    let server = ExternalServer::connect(config).await.expect("connect");
    assert_eq!(server.info().transport, "http");
    assert_eq!(server.info().server_name.as_deref(), Some("http-fake"));
    let tools = server.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "whoami");
    let seen = server.call(&call("whoami", json!({}))).await.unwrap();
    assert!(!seen.is_error);
    assert_eq!(text(&seen), "Bearer from-the-environment");
    server.shutdown().await;
}

#[tokio::test]
async fn a_header_naming_an_unset_variable_fails_the_connect() {
    let mut config = ServerConfig::http("web", "http://127.0.0.1:9/mcp");
    if let Transport::Http { headers, .. } = &mut config.transport {
        headers.insert(
            "Authorization".into(),
            "Bearer ${TETHER_TEST_NOT_SET_ANYWHERE}".into(),
        );
    }
    match ExternalServer::connect(config).await {
        Err(McpError::Connect { reason, .. }) => {
            assert!(reason.contains("TETHER_TEST_NOT_SET_ANYWHERE"), "{reason}");
        }
        Ok(_) => panic!("connected with a header that could not be filled in"),
        Err(other) => panic!("expected Connect error, got {other:?}"),
    }
}
