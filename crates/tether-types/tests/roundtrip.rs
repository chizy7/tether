use tether_types::*;
use time::OffsetDateTime;

#[test]
fn event_roundtrips_through_json() {
    let ev = Event {
        v: SCHEMA_VERSION,
        run: RunId("run_01TEST".into()),
        seq: 0,
        ts: OffsetDateTime::UNIX_EPOCH,
        step: 0,
        agent: "main".into(),
        parent: None,
        hash: "blake3:0000".into(),
        body: EventType::RunStateChanged {
            state: RunState::Running,
            reason: "start".into(),
        },
    };
    let line = serde_json::to_string(&ev).expect("serialize");
    assert!(line.contains("\"type\":\"run.state\""), "{line}");
    let back: Event = serde_json::from_str(&line).expect("deserialize");
    assert_eq!(back, ev);
}

#[test]
fn unknown_event_type_is_ext() {
    let line = r#"{"v":0,"run":"r","seq":3,"ts":"1970-01-01T00:00:00Z","step":1,"agent":"main","hash":"blake3:00","type":"ext.custom","data":{"k":1}}"#;
    let ev: Event = serde_json::from_str(line).expect("deserialize");
    match ev.body {
        EventType::Ext { r#type, .. } => assert_eq!(r#type, "ext.custom"),
        other => panic!("expected Ext, got {other:?}"),
    }
}

#[test]
fn payload_blob_reference_roundtrips_as_blob() {
    let blob = Payload::Blob {
        blob: "blake3:ab".into(),
        bytes: 40_000,
    };
    let json = serde_json::to_string(&blob).expect("serialize");
    assert_eq!(json, r#"{"blob":"blake3:ab","bytes":40000}"#);
    let back: Payload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, blob);

    let inline: Payload = serde_json::from_str(r#"{"stdout":"ok"}"#).expect("deserialize");
    assert_eq!(inline, Payload::Inline(serde_json::json!({"stdout": "ok"})));
}

#[test]
fn budget_tick_carries_an_optional_price_beside_the_snapshot() {
    // A tick written before prices existed still reads.
    let old = r#"{"v":0,"run":"r","seq":5,"ts":"1970-01-01T00:00:00Z","step":1,"agent":"main","parent":2,"hash":"blake3:00","type":"budget.tick","data":{"tokens_used":15,"tokens_limit":null,"usd_used":0.0,"usd_limit":5.0,"wall_secs_used":0,"wall_secs_limit":null,"tool_calls_used":1,"tool_calls_limit":20}}"#;
    let ev: Event = serde_json::from_str(old).expect("deserialize");
    let EventType::BudgetTick { budget, price } = &ev.body else {
        panic!("expected budget.tick, got {:?}", ev.body)
    };
    assert_eq!(
        (budget.tokens_used, budget.tool_calls_limit),
        (15, Some(20))
    );
    assert_eq!(*price, None);
    // And writes back byte for byte, with no `price` key.
    assert_eq!(serde_json::to_string(&ev).expect("serialize"), old);

    let priced = Event {
        body: EventType::BudgetTick {
            budget: *budget,
            price: Some(Price {
                usd: 0.0125,
                table: "2026-11-03".into(),
            }),
        },
        ..ev
    };
    let line = serde_json::to_string(&priced).expect("serialize");
    assert!(
        line.ends_with(r#""tool_calls_limit":20,"price":{"usd":0.0125,"table":"2026-11-03"}}}"#),
        "{line}"
    );
    assert_eq!(
        serde_json::from_str::<Event>(&line).expect("deserialize"),
        priced
    );
}

#[test]
fn model_response_stop_reason_is_optional() {
    let without = r#"{"message":"blake3:aa","tool_calls":[],"usage":{"input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0},"latency_ms":7,"stream":false,"provider_request_id":null,"raw":null}"#;
    let response: ModelResponse = serde_json::from_str(without).expect("deserialize");
    assert_eq!(response.stop_reason, None);
    assert_eq!(
        serde_json::to_string(&response).expect("serialize"),
        without
    );

    let with = without.replace(
        r#""latency_ms""#,
        r#""stop_reason":"tool_use","latency_ms""#,
    );
    let response: ModelResponse = serde_json::from_str(&with).expect("deserialize");
    assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(serde_json::to_string(&response).expect("serialize"), with);
}
