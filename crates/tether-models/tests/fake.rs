use serde_json::json;
use tether_models::fake::FakeAdapter;
use tether_models::{
    Block, Completion, CompletionRequest, Message, ModelAdapter, ModelError, Money, Role,
};
use tether_types::Usage;

fn request(text: &str) -> CompletionRequest {
    CompletionRequest {
        model: "fake-1".into(),
        messages: vec![Message::text(Role::User, text)],
        tools: vec![],
        params: json!({}),
    }
}

#[tokio::test]
async fn fake_plays_its_script_in_order_and_records_requests() {
    let fake = FakeAdapter::new([
        Completion::tool_use("call_1", "sandbox.exec", json!({"argv": ["ls"]})),
        Completion::text("done"),
    ]);
    assert_eq!(fake.id(), "fake");

    let first = fake.complete(request("list files")).await.expect("first");
    assert!(
        matches!(&first.message.content[0], Block::ToolUse { name, .. } if name == "sandbox.exec")
    );
    assert_eq!(fake.remaining(), 1);

    let second = fake.complete(request("thanks")).await.expect("second");
    assert_eq!(second, Completion::text("done"));
    assert_eq!(
        fake.requests(),
        vec![request("list files"), request("thanks")]
    );
}

#[tokio::test]
async fn fake_errors_when_the_script_runs_out() {
    let fake = FakeAdapter::new([]);
    let err = fake.complete(request("hi")).await.expect_err("must fail");
    assert!(matches!(err, ModelError::Provider(_)));
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn fake_is_free() {
    let fake = FakeAdapter::default();
    assert_eq!(fake.price(&Usage::default()), Some(Money(0.0)));
    let priced = FakeAdapter::default().priced(0.25);
    assert_eq!(priced.price(&Usage::default()), Some(Money(0.25)));
}

#[test]
fn message_serializes_in_tether_form() {
    let message = Completion::tool_use("call_1", "sandbox.exec", json!({"argv": ["ls"]})).message;
    let text = serde_json::to_string(&message).expect("serialize");
    assert_eq!(
        text,
        r#"{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"sandbox.exec","arguments":{"argv":["ls"]}}]}"#
    );
    assert_eq!(
        serde_json::from_str::<Message>(&text).expect("deserialize"),
        message
    );
}
