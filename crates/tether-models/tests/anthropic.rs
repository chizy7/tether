//! The adapter against recorded shapes and a local stand-in for the API.
//! Nothing here needs a key except the one ignored test at the bottom.
#![cfg(feature = "anthropic")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use tether_models::anthropic::{
    parse_message, request_body, AnthropicAdapter, StreamCollector, API_VERSION,
};
use tether_models::{Block, CompletionRequest, Message, ModelAdapter, ModelError, Role, ToolDef};

fn thinking() -> Block {
    Block::Provider {
        provider: "anthropic".into(),
        block: json!({"type": "thinking", "thinking": "", "signature": "c2ln"}),
    }
}

/// A conversation one tool call in: system, user, the assistant's turn with
/// a reasoning block, and the tool's result.
fn conversation() -> CompletionRequest {
    CompletionRequest {
        model: "claude-opus-5-5".into(),
        messages: vec![
            Message::text(Role::System, "Be brief."),
            Message::text(Role::User, "What is in notes.txt?"),
            Message {
                role: Role::Assistant,
                content: vec![
                    thinking(),
                    Block::Text {
                        text: "Reading it.".into(),
                    },
                    Block::ToolUse {
                        id: "toolu_1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "notes.txt"}),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Block::ToolResult {
                    id: "toolu_1".into(),
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
        params: json!({"output_config": {"effort": "low"}}),
    }
}

#[test]
fn request_body_is_the_conversation_in_the_api_shape() {
    let body = request_body(&conversation(), true).expect("body");
    assert_eq!(
        body,
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 64000,
            "stream": true,
            "output_config": {"effort": "low"},
            "system": [{"type": "text", "text": "Be brief."}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "What is in notes.txt?"}]},
                {"role": "assistant", "content": [
                    // The reasoning block goes back exactly as it came.
                    {"type": "thinking", "thinking": "", "signature": "c2ln"},
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "notes.txt"}},
                ]},
                // A tool message is a user turn of tool results.
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "hello\n"},
                ]},
            ],
            "tools": [{
                "name": "read_file",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            }],
        })
    );
}

#[test]
fn request_body_details() {
    let mut req = conversation();
    req.params = json!({"max_tokens": 512});
    req.messages[3].content = vec![
        Block::ToolResult {
            id: "toolu_1".into(),
            content: json!({"exit_code": 1, "stderr": "no"}),
            is_error: true,
        },
        // Another provider's block has no meaning here and is left out.
        Block::Provider {
            provider: "other".into(),
            block: json!({"type": "mystery"}),
        },
    ];
    // A system message after the conversation starts stays in it.
    req.messages
        .push(Message::text(Role::System, "Wrap up now."));
    let body = request_body(&req, false).expect("body");

    assert_eq!(body["max_tokens"], 512);
    assert!(body.get("stream").is_none());
    assert_eq!(
        body["messages"][2]["content"],
        json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": r#"{"exit_code":1,"stderr":"no"}"#,
            "is_error": true,
        }])
    );
    assert_eq!(
        body["messages"][3],
        json!({"role": "system", "content": [{"type": "text", "text": "Wrap up now."}]})
    );

    // Without a caller's max_tokens, a non-streaming request gets less room.
    req.params = json!({});
    assert_eq!(
        request_body(&req, false).expect("body")["max_tokens"],
        16000
    );

    // Tools the agent put in params are kept, ahead of the defined ones.
    req.params = json!({"tools": [{"type": "web_search_20260209", "name": "web_search"}]});
    let tools = &request_body(&req, true).expect("body")["tools"];
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[1]["name"], "read_file");

    req.params = json!({"system": "also here"});
    assert!(request_body(&req, true).is_err());
    req.params = json!(["not", "an", "object"]);
    assert!(request_body(&req, true).is_err());
}

/// A response with reasoning, text and a tool call.
fn response() -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "c2ln"},
            {"type": "text", "text": "Reading it."},
            {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "notes.txt"}},
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 420,
            "output_tokens": 57,
            "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 30,
        },
    })
}

#[test]
fn response_becomes_a_completion() {
    let completion = parse_message(&response()).expect("parse");
    assert_eq!(completion.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(
        (
            completion.usage.input_tokens,
            completion.usage.output_tokens,
            completion.usage.cache_read_tokens,
            completion.usage.cache_write_tokens,
        ),
        (420, 57, 1000, 30)
    );
    assert_eq!(
        completion.message.content,
        vec![
            thinking(),
            Block::Text {
                text: "Reading it.".into()
            },
            Block::ToolUse {
                id: "toolu_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "notes.txt"}),
            },
        ]
    );

    // Sent back as part of the next request, the turn is what the API sent.
    let mut next = conversation();
    next.messages[2] = completion.message;
    let body = request_body(&next, true).expect("body");
    assert_eq!(body["messages"][1]["content"], response()["content"]);
}

#[test]
fn blocks_the_adapter_does_not_model_are_kept_whole() {
    let cited = json!({"type": "text", "text": "Paris.", "citations": [{"type": "char_location", "cited_text": "Paris"}]});
    let search =
        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []});
    let mut message = response();
    message["content"] =
        json!([cited, search, {"type": "text", "text": "plain", "citations": null}]);

    let content = parse_message(&message).expect("parse").message.content;
    let kept = |block: &Value| Block::Provider {
        provider: "anthropic".into(),
        block: block.clone(),
    };
    assert_eq!(
        content,
        vec![
            kept(&cited),
            kept(&search),
            Block::Text {
                text: "plain".into()
            }
        ]
    );
}

/// The stream the API sends for [`response`].
const STREAM: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":420,"output_tokens":1,"cache_read_input_tokens":1000,"cache_creation_input_tokens":30}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"c2ln"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" it."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"no"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"tes.txt\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: some_future_event
data: {"type":"some_future_event","anything":true}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":57}}

event: message_stop
data: {"type":"message_stop"}

"#;

fn collect(stream: &str, chunk_size: usize) -> Result<Value, ModelError> {
    let mut collector = StreamCollector::default();
    for chunk in stream.as_bytes().chunks(chunk_size) {
        collector.feed(chunk)?;
    }
    collector.finish()
}

#[test]
fn a_stream_collapses_to_the_same_message_however_it_is_chunked() {
    // Whole, in awkward pieces, and a byte at a time, with either line ending.
    for chunk_size in [STREAM.len(), 1, 7, 64] {
        assert_eq!(
            collect(STREAM, chunk_size).expect("collect"),
            response(),
            "{chunk_size}"
        );
    }
    let crlf = STREAM.replace('\n', "\r\n");
    assert_eq!(collect(&crlf, 5).expect("collect"), response());
}

#[test]
fn a_stream_reports_content_only_once_some_has_arrived() {
    let mut collector = StreamCollector::default();
    let first_block = STREAM
        .find("event: content_block_start")
        .expect("block start");
    collector
        .feed(&STREAM.as_bytes()[..first_block])
        .expect("feed");
    assert!(!collector.saw_content());
    collector
        .feed(&STREAM.as_bytes()[first_block..])
        .expect("feed");
    assert!(collector.saw_content());
}

#[test]
fn a_stream_that_goes_wrong_is_an_error_not_a_partial_message() {
    // Cut off before the end.
    let cut = &STREAM[..STREAM.find("event: message_delta").expect("delta")];
    assert!(collect(cut, 32).is_err());

    // An error event in the middle, as an overload looks.
    let failed = format!(
        "{}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}}}\n\n",
        &STREAM[..STREAM.find("event: content_block_stop").expect("stop")]
    );
    match collect(&failed, 32) {
        Err(ModelError::Api { kind, message, .. }) => {
            assert_eq!(
                (kind.as_str(), message.as_str()),
                ("overloaded_error", "Overloaded")
            );
        }
        other => panic!("expected an API error, got {other:?}"),
    }

    // Tool input cut off by max_tokens is not valid JSON.
    let truncated = STREAM.replace(r#"tes.txt\"}"#, "tes");
    assert!(collect(&truncated, 32).is_err());

    // A delta type from the future must not be guessed at.
    let unknown = STREAM.replace("signature_delta", "hologram_delta");
    let err = collect(&unknown, 32).expect_err("unknown delta");
    assert!(err.to_string().contains("hologram_delta"), "{err}");
}

/// One request a stand-in server received.
struct Received {
    head: String,
    body: Value,
}

/// A stand-in for the API on a local port: answers each connection with the
/// next canned response and reports what it was sent.
fn serve(responses: Vec<(u16, &'static str, String)>) -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
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
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\nrequest-id: req_test_{status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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
async fn a_streamed_call_goes_out_with_the_right_headers_and_comes_back_whole() {
    let (url, received) = serve(vec![(200, "text/event-stream", STREAM.to_owned())]);
    let adapter = AnthropicAdapter::new("sk-test")
        .base_url(url)
        .beta("server-side-fallback-2026-07-01");
    assert_eq!(adapter.id(), "anthropic");

    let completion = adapter.complete(conversation()).await.expect("complete");

    let sent = received.recv().expect("request");
    assert!(
        sent.head.starts_with("POST /v1/messages HTTP/1.1"),
        "{}",
        sent.head
    );
    assert_eq!(header(&sent.head, "x-api-key"), Some("sk-test"));
    assert_eq!(header(&sent.head, "anthropic-version"), Some(API_VERSION));
    assert_eq!(
        header(&sent.head, "anthropic-beta"),
        Some("server-side-fallback-2026-07-01")
    );
    assert_eq!(header(&sent.head, "content-type"), Some("application/json"));
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
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(
        completion.message,
        parse_message(&response()).expect("parse").message
    );
    // The raw body is the event stream, byte for byte.
    assert_eq!(completion.raw.as_deref(), Some(STREAM.as_bytes()));

    // Priced from the table by the model that answered.
    let price = adapter
        .price("claude-opus-5-5", &completion.usage)
        .expect("price");
    let expected = (420.0 * 4.0 + 57.0 * 20.0 + 1000.0 * 0.20 + 30.0 * 5.0) / 1e6;
    assert!((price.usd - expected).abs() < 1e-12, "{}", price.usd);
    assert_eq!(adapter.price("not-a-model", &completion.usage), None);
}

#[tokio::test]
async fn a_non_streaming_call_reads_one_json_body() {
    let (url, received) = serve(vec![(200, "application/json", response().to_string())]);
    let adapter = AnthropicAdapter::new("sk-test")
        .base_url(url)
        .streaming(false);

    let completion = adapter.complete(conversation()).await.expect("complete");

    assert!(received
        .recv()
        .expect("request")
        .body
        .get("stream")
        .is_none());
    assert!(!completion.stream);
    assert_eq!(completion.ttft_ms, None);
    assert_eq!(
        completion.message,
        parse_message(&response()).expect("parse").message
    );
    assert_eq!(completion.raw, Some(response().to_string().into_bytes()));
}

fn error_body(kind: &str, message: &str) -> String {
    json!({"type": "error", "error": {"type": kind, "message": message}, "request_id": "req_abc"})
        .to_string()
}

#[tokio::test]
async fn an_overload_is_retried_and_a_bad_request_is_not() {
    let fast = |url: String| {
        AnthropicAdapter::new("sk-test")
            .base_url(url)
            .retries(2, Duration::from_millis(1))
    };

    // Overloaded once, then fine: the caller never sees the first answer.
    let (url, received) = serve(vec![
        (
            529,
            "application/json",
            error_body("overloaded_error", "Overloaded"),
        ),
        (200, "text/event-stream", STREAM.to_owned()),
    ]);
    let completion = fast(url).complete(conversation()).await.expect("complete");
    assert_eq!(completion.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(received.iter().count(), 2);

    // A request the API rejects is an error at once, with its details.
    let (url, received) = serve(vec![(
        400,
        "application/json",
        error_body("invalid_request_error", "max_tokens: must be positive"),
    )]);
    let err = fast(url)
        .complete(conversation())
        .await
        .expect_err("must fail");
    match err {
        ModelError::Api {
            status,
            kind,
            message,
            request_id,
        } => {
            assert_eq!(status, 400);
            assert_eq!(kind, "invalid_request_error");
            assert_eq!(message, "max_tokens: must be positive");
            assert_eq!(request_id.as_deref(), Some("req_abc"));
        }
        other => panic!("expected an API error, got {other}"),
    }
    assert_eq!(received.iter().count(), 1);

    // Still overloaded after every retry: the last error comes back.
    let overloaded = || {
        (
            529,
            "application/json",
            error_body("overloaded_error", "Overloaded"),
        )
    };
    let (url, received) = serve(vec![overloaded(), overloaded(), overloaded()]);
    let err = fast(url)
        .complete(conversation())
        .await
        .expect_err("must fail");
    assert!(matches!(err, ModelError::Api { status: 529, .. }), "{err}");
    assert_eq!(received.iter().count(), 3);
}

/// Makes one real, billed API call. Run it on purpose:
/// `ANTHROPIC_API_KEY=... cargo test -p tether-models --test anthropic -- --ignored`
#[tokio::test]
#[ignore = "needs ANTHROPIC_API_KEY and makes a real, billed API call"]
async fn live_call_answers_and_reports_usage() {
    let adapter = AnthropicAdapter::from_env().expect("credentials");
    let request = CompletionRequest {
        model: "claude-opus-5-5".into(),
        messages: vec![Message::text(
            Role::User,
            "Reply with the single word: pong",
        )],
        tools: vec![],
        params: json!({"max_tokens": 2048, "output_config": {"effort": "low"}}),
    };

    let completion = adapter.complete(request.clone()).await.expect("complete");

    let said: String = completion
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(said.to_lowercase().contains("pong"), "{said}");
    assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
    assert!(completion.usage.input_tokens > 0 && completion.usage.output_tokens > 0);
    assert!(completion.stream && completion.ttft_ms.is_some());
    assert!(completion.provider_request_id.is_some());
    assert!(completion.raw.is_some_and(|raw| raw.starts_with(b"event:")));
    let model = completion.model.as_deref().unwrap_or(&request.model);
    assert!(adapter
        .price(model, &completion.usage)
        .is_some_and(|p| p.usd > 0.0));
}
