//! The adapter against recorded shapes and a local stand-in for a server.
//! Nothing here needs a key or a network.
#![cfg(feature = "openai_compat")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use tether_models::openai_compat::{
    parse_response, request_body, OpenAiCompatAdapter, StreamCollector,
};
use tether_models::{
    Block, CompletionRequest, Message, ModelAdapter, ModelError, Pricing, Role, ToolDef,
};

/// A conversation one tool call in: system, user, the assistant's turn with
/// a tool call, and the tool's result.
fn conversation() -> CompletionRequest {
    CompletionRequest {
        model: "local-model".into(),
        messages: vec![
            Message::text(Role::System, "Be brief."),
            Message::text(Role::User, "What is in notes.txt?"),
            Message {
                role: Role::Assistant,
                content: vec![
                    // Another provider's block has no place here and is left out.
                    Block::Provider {
                        provider: "anthropic".into(),
                        block: json!({"type": "thinking", "thinking": "", "signature": "c2ln"}),
                    },
                    Block::Text {
                        text: "Reading it.".into(),
                    },
                    Block::ToolUse {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "notes.txt"}),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Block::ToolResult {
                    id: "call_1".into(),
                    content: json!("hello\n"),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDef {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
        params: json!({"temperature": 0}),
    }
}

#[test]
fn request_body_is_the_conversation_in_the_chat_completions_shape() {
    let body = request_body(&conversation(), true).expect("body");
    assert_eq!(
        body,
        json!({
            "model": "local-model",
            "temperature": 0,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "What is in notes.txt?"},
                {"role": "assistant", "content": "Reading it.", "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    // Arguments go as JSON text.
                    "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"},
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "hello\n"},
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                },
            }],
        })
    );
}

#[test]
fn request_body_details() {
    let mut req = conversation();
    req.tools.clear();
    req.params = json!({"stream_options": {"include_usage": false}});
    req.messages = vec![
        // Several texts go as parts; none is altered by joining.
        Message {
            role: Role::User,
            content: vec![
                Block::Text { text: "one".into() },
                Block::Text { text: "two".into() },
            ],
        },
        // A call with nothing said.
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "call_a".into(),
                name: "list_dir".into(),
                arguments: json!({}),
            }],
        },
        // Two results and a remark: the results come first, one message each.
        Message {
            role: Role::Tool,
            content: vec![
                Block::Text {
                    text: "and now?".into(),
                },
                Block::ToolResult {
                    id: "call_a".into(),
                    content: json!({"entries": ["a"]}),
                    is_error: true,
                },
                Block::ToolResult {
                    id: "call_b".into(),
                    content: json!("b"),
                    is_error: false,
                },
            ],
        },
        // A refusal goes back as one.
        Message {
            role: Role::Assistant,
            content: vec![Block::Provider {
                provider: "openai_compat".into(),
                block: json!({"type": "refusal", "refusal": "No."}),
            }],
        },
        // Only another provider's blocks: the message is dropped.
        Message {
            role: Role::Assistant,
            content: vec![Block::Provider {
                provider: "anthropic".into(),
                block: json!({"type": "thinking"}),
            }],
        },
    ];

    let body = request_body(&req, false).expect("body");
    assert_eq!(
        body,
        json!({
            "model": "local-model",
            "stream_options": {"include_usage": false},
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two"},
                ]},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": {"name": "list_dir", "arguments": "{}"},
                }]},
                // Not text, so sent as its JSON. The error flag has nowhere to go.
                {"role": "tool", "tool_call_id": "call_a", "content": "{\"entries\":[\"a\"]}"},
                {"role": "tool", "tool_call_id": "call_b", "content": "b"},
                {"role": "user", "content": "and now?"},
                {"role": "assistant", "content": null, "refusal": "No."},
            ],
        })
    );

    // The agent's own stream options are kept when streaming too.
    let streamed = request_body(&req, true).expect("body");
    assert_eq!(streamed["stream_options"], json!({"include_usage": false}));

    req.params = json!({"tools": [{"type": "web_search"}]});
    req.tools = conversation().tools;
    let tools = request_body(&req, false).expect("body")["tools"].clone();
    assert_eq!(tools[0], json!({"type": "web_search"}));
    assert_eq!(tools[1]["function"]["name"], "read_file");

    req.params = json!("fast");
    assert!(matches!(
        request_body(&req, false),
        Err(ModelError::Provider(_))
    ));
}

/// What a non-streaming call returns for the conversation above.
fn response() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1795000000,
        "model": "local-model-0613",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Reading both.",
                "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "read_file", "arguments": "{\"path\": \"notes.txt\"}"}},
                    {"id": "call_2", "type": "function",
                     "function": {"name": "list_dir", "arguments": ""}},
                ],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {
            "prompt_tokens": 1420,
            "completion_tokens": 57,
            "total_tokens": 1477,
            "prompt_tokens_details": {"cached_tokens": 1000},
        },
    })
}

#[test]
fn response_becomes_a_completion() {
    let completion = parse_response(&response()).expect("parse");
    assert_eq!(completion.message.role, Role::Assistant);
    assert_eq!(
        completion.message.content,
        vec![
            Block::Text {
                text: "Reading both.".into()
            },
            Block::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "notes.txt"}),
            },
            // No argument text at all means no arguments.
            Block::ToolUse {
                id: "call_2".into(),
                name: "list_dir".into(),
                arguments: json!({}),
            },
        ]
    );
    assert_eq!(completion.model.as_deref(), Some("local-model-0613"));
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_calls"));
    // Cached tokens are inside prompt_tokens on the wire and apart here.
    assert_eq!(
        (
            completion.usage.input_tokens,
            completion.usage.output_tokens,
            completion.usage.cache_read_tokens,
            completion.usage.cache_write_tokens,
        ),
        (420, 57, 1000, 0)
    );
}

#[test]
fn odd_responses() {
    // A refusal is kept as a provider block; a server with no usage counts zero.
    let refusal = parse_response(&json!({
        "choices": [{
            "message": {"role": "assistant", "content": null, "refusal": "No."},
            "finish_reason": "stop",
        }],
    }))
    .expect("parse");
    assert_eq!(
        refusal.message.content,
        vec![Block::Provider {
            provider: "openai_compat".into(),
            block: json!({"type": "refusal", "refusal": "No."}),
        }]
    );
    assert_eq!(refusal.usage.input_tokens, 0);
    assert_eq!(refusal.model, None);

    // Content in parts.
    let parts = parse_response(&json!({
        "choices": [{"message": {"content": [{"type": "text", "text": "hi"}]}}],
    }))
    .expect("parse");
    assert_eq!(
        parts.message.content,
        vec![Block::Text { text: "hi".into() }]
    );
    assert_eq!(parts.stop_reason, None);

    // Arguments cut off by a token limit are an error, not a guess.
    let cut = parse_response(&json!({
        "choices": [{
            "message": {"tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "exec", "arguments": "{\"argv\": [\"ls"}},
            ]},
            "finish_reason": "length",
        }],
    }));
    assert!(
        matches!(&cut, Err(ModelError::Provider(m)) if m.contains("cut off")),
        "{cut:?}"
    );

    assert!(matches!(
        parse_response(&json!({"choices": []})),
        Err(ModelError::Provider(_))
    ));
    let error = parse_response(&json!({"error": {"type": "server_error", "message": "boom"}}));
    assert!(
        matches!(&error, Err(ModelError::Api { kind, message, .. }) if kind == "server_error" && message == "boom"),
        "{error:?}"
    );
}

/// The response above as a stream: text, then two tool calls whose arguments
/// arrive in pieces, then the finish reason, then usage in a chunk with no
/// choices.
const STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    ": keep-alive\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Reading \"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"both.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"notes.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1795000000,\"model\":\"local-model-0613\",\"choices\":[],\"usage\":{\"prompt_tokens\":1420,\"completion_tokens\":57,\"total_tokens\":1477,\"prompt_tokens_details\":{\"cached_tokens\":1000}}}\n\n",
    "data: [DONE]\n\n",
);

fn collect(stream: &str, chunk_size: usize) -> Result<Value, ModelError> {
    let mut collector = StreamCollector::default();
    for chunk in stream.as_bytes().chunks(chunk_size) {
        collector.feed(chunk)?;
    }
    collector.finish()
}

#[test]
fn a_stream_collapses_to_the_same_response_however_it_is_chunked() {
    for chunk_size in [1, 7, 64, STREAM.len()] {
        let body = collect(STREAM, chunk_size).expect("collect");
        assert_eq!(body, response(), "chunks of {chunk_size}");
    }
}

#[test]
fn a_stream_reports_content_only_once_some_has_arrived() {
    let mut collector = StreamCollector::default();
    let (first, rest) = STREAM.split_at(STREAM.find(": keep-alive").expect("comment"));
    collector.feed(first.as_bytes()).expect("feed");
    // The opening chunk carries a role and an empty string, which is not content.
    assert!(!collector.saw_content());
    collector.feed(rest.as_bytes()).expect("feed");
    assert!(collector.saw_content());
}

#[test]
fn a_stream_that_goes_wrong_is_an_error_not_a_partial_response() {
    // Cut off before any finish reason.
    let cut = &STREAM[..STREAM
        .find("\"finish_reason\":\"tool_calls\"")
        .expect("finish")];
    let cut = &cut[..cut.rfind("data:").expect("event")];
    assert!(matches!(collect(cut, 64), Err(ModelError::Provider(_))));

    // A server that sends a finish reason but never `[DONE]` is still whole.
    let no_done = STREAM.replace("data: [DONE]\n\n", "");
    assert_eq!(collect(&no_done, 64).expect("collect"), response());

    let error = "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n";
    let got = collect(error, 64);
    assert!(
        matches!(&got, Err(ModelError::Api { kind, .. }) if kind == "server_error"),
        "{got:?}"
    );

    assert!(matches!(
        collect("data: not json\n\n", 64),
        Err(ModelError::Provider(_))
    ));
}

struct Received {
    head: String,
    body: Value,
}

/// A stand-in for a server on a local port: answers each connection with the
/// next canned response and reports what it was sent.
fn serve(responses: Vec<(u16, &'static str, String)>) -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/v1/", listener.local_addr().expect("addr"));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, content_type, body) in responses {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let (head, body_start) = loop {
                let read = socket.read(&mut buffer).expect("read");
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    break (text[..end].to_owned(), end + 4);
                }
            };
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().expect("length"))
                })
                .unwrap_or(0);
            while request.len() < body_start + length {
                let read = socket.read(&mut buffer).expect("read");
                request.extend_from_slice(&buffer[..read]);
            }
            let sent = serde_json::from_slice(&request[body_start..]).expect("JSON body");
            tx.send(Received { head, body: sent }).expect("report");
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\nx-request-id: req_test_{status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).expect("write");
        }
    });
    (url, rx)
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

#[tokio::test]
async fn a_streamed_call_goes_out_with_a_bearer_key_and_comes_back_whole() {
    let (url, received) = serve(vec![(200, "text/event-stream", STREAM.to_owned())]);
    let pricing = Pricing::from_toml(
        "version = \"mine\"\ncache_read_multiplier = 0.5\ncache_write_multiplier = 1.0\n\n[models.local-model-0613]\ninput = 2.0\noutput = 8.0\n",
    )
    .expect("pricing");
    let adapter = OpenAiCompatAdapter::new(url)
        .api_key("sk-test")
        .pricing(pricing);
    assert_eq!(adapter.id(), "openai_compat");

    let completion = adapter.complete(conversation()).await.expect("complete");

    let sent = received.recv().expect("request");
    assert!(
        sent.head.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "{}",
        sent.head
    );
    assert_eq!(header(&sent.head, "authorization"), Some("Bearer sk-test"));
    assert_eq!(
        sent.body,
        request_body(&conversation(), true).expect("body")
    );

    assert!(completion.stream);
    assert!(completion.ttft_ms.is_some());
    assert_eq!(
        completion.provider_request_id.as_deref(),
        Some("req_test_200")
    );
    assert_eq!(
        completion.message,
        parse_response(&response()).expect("parse").message
    );
    assert_eq!(completion.raw.as_deref(), Some(STREAM.as_bytes()));

    // Priced from the given table by the model that answered; the built-in
    // table has no row for it.
    let price = adapter
        .price("local-model-0613", &completion.usage)
        .expect("price");
    let expected = (420.0 * 2.0 + 57.0 * 8.0 + 1000.0 * 1.0) / 1e6;
    assert!((price.usd - expected).abs() < 1e-12, "{}", price.usd);
    assert_eq!(price.table, "mine");
    assert_eq!(
        Pricing::builtin().price("local-model-0613", &completion.usage),
        None
    );
}

#[tokio::test]
async fn a_non_streaming_call_without_a_key_sends_no_credentials() {
    let (url, received) = serve(vec![(200, "application/json", response().to_string())]);
    let adapter = OpenAiCompatAdapter::new(url).streaming(false);

    let completion = adapter.complete(conversation()).await.expect("complete");

    let sent = received.recv().expect("request");
    assert_eq!(header(&sent.head, "authorization"), None);
    assert_eq!(sent.body.get("stream"), None);
    assert!(!completion.stream);
    assert_eq!(completion.ttft_ms, None);
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        completion.raw.as_deref(),
        Some(response().to_string().as_bytes())
    );
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_a_bad_request_is_not() {
    let limited =
        json!({"error": {"type": "rate_limit_error", "message": "slow down"}}).to_string();
    let (url, received) = serve(vec![
        (429, "application/json", limited),
        (200, "application/json", response().to_string()),
    ]);
    let adapter = OpenAiCompatAdapter::new(url)
        .streaming(false)
        .retries(2, Duration::from_millis(1));
    adapter.complete(conversation()).await.expect("retried");
    assert_eq!(received.iter().count(), 2);

    // A server whose error is a bare string, with a code and no type.
    let (url, received) = serve(vec![(
        400,
        "application/json",
        json!({"error": "model not found"}).to_string(),
    )]);
    let adapter = OpenAiCompatAdapter::new(url).retries(2, Duration::from_millis(1));
    let error = adapter.complete(conversation()).await.expect_err("fails");
    assert!(
        matches!(
            &error,
            ModelError::Api { status: 400, kind, message, request_id }
                if kind == "error" && message == "model not found"
                    && request_id.as_deref() == Some("req_test_400")
        ),
        "{error:?}"
    );
    assert_eq!(received.iter().count(), 1);
}
