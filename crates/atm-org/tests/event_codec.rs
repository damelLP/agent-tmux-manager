mod support;
use atm_core::Money;
use atm_org::*;
use serde_json::json;

#[test]
fn all_eighteen_golden_event_bodies() {
    let cases = support::bodies();
    assert_eq!(cases.len(), 18);
    for (kind, body) in cases {
        let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(event.kind(), kind);
        assert_eq!(event.to_body(), body, "{kind}");
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
        );
    }
}

#[test]
fn malformed_known_events_error_unknown_kinds_skip() {
    for (kind, _) in support::bodies() {
        let error = OrgEvent::from_parts(kind, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains(kind), "{error}");
    }
    for body in [json!(null), json!(42), json!({"anything":true})] {
        assert_eq!(OrgEvent::from_parts("future.event", &body).unwrap(), None);
    }
    assert!(OrgEvent::from_parts(
        "agent.down",
        &json!({"agent_id":"p/bad name", "reason":"done"})
    )
    .is_err());
    assert!(OrgEvent::from_parts(
        "msg.delivery_failed",
        &json!({"message_id":"m1", "reason":"no", "final":"true"})
    )
    .is_err());
}

#[test]
fn event_json_tolerates_additive_fields_even_in_roles() {
    let event = OrgEvent::AgentUpRequested {
        agent_id: support::id(),
        role: support::role(),
    };
    let mut body = event.to_body();
    body["future"] = json!(1);
    body["role"]["future"] = json!("new");
    assert_eq!(
        OrgEvent::from_parts(event.kind(), &body).unwrap(),
        Some(event)
    );
    assert!(parse_role("lead", "+++\nharness='pi'\nfuture=1\n+++").is_err());
}

#[test]
fn summary_cost_and_message_ids_are_lossless_at_integer_extremes() {
    for amount in [i64::MIN, -1, 0, i64::MAX] {
        let mut summary = support::summary();
        summary.cost = Some(Money::from_microdollars(amount));
        let event = OrgEvent::SessionSummary(summary);
        assert_eq!(event.to_body()["cost"], json!({"microdollars":amount}));
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
        );
    }
    let event = OrgEvent::MessageRead {
        message_ids: vec![MessageId::new(u64::MAX)],
    };
    assert_eq!(
        OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
        Some(event)
    );
}
