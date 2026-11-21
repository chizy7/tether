use serde_json::json;
use tether_models::fake::FakeAdapter;
use tether_models::{
    Block, Completion, CompletionRequest, Message, ModelAdapter, ModelError, Role,
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
fn fake_has_no_price_unless_given_one() {
    let fake = FakeAdapter::default();
    assert_eq!(fake.price("fake-1", &Usage::default()), None);

    let priced = FakeAdapter::default().priced(0.25);
    let price = priced.price("fake-1", &Usage::default()).expect("price");
    assert_eq!((price.usd, price.table.as_str()), (0.25, "fake"));
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

#[test]
fn provider_block_keeps_its_content_verbatim() {
    let block = Block::Provider {
        provider: "anthropic".into(),
        block: json!({"type": "thinking", "thinking": "", "signature": "sig=="}),
    };
    let text = serde_json::to_string(&block).expect("serialize");
    assert_eq!(
        text,
        r#"{"type":"provider","provider":"anthropic","block":{"signature":"sig==","thinking":"","type":"thinking"}}"#
    );
    assert_eq!(
        serde_json::from_str::<Block>(&text).expect("deserialize"),
        block
    );
}
