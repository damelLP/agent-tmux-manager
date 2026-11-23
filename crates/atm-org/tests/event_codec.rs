mod support;
use atm_core::Money;
use atm_org::*;
use serde_json::json;

#[test]
fn all_golden_event_bodies() {
    let cases = support::bodies();
    assert_eq!(cases.len(), 24);
    for (kind, body) in cases {
        let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(event.kind(), kind);
        assert!(
            event.to_body().is_object(),
            "{kind} must take the derive's object path"
        );
        assert_eq!(
            event.to_body(),
            serde_json::to_value(&event).unwrap()["body"]
        );
        assert_eq!(event.to_body(), body, "{kind}");
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &event.to_body()).unwrap(),
            Some(event)
        );
    }
}

#[test]
fn task_usage_is_a_known_additive_event() {
    // Synthetic: a baseline observation with no eligible task.
    let body = json!({"session_id":"s", "task_id":null, "turn":true,
        "cumulative_cost":{"microdollars":125000}, "context_percent":70});
    let event = OrgEvent::from_parts("task.usage_observed", &body).unwrap();
    assert!(event.is_some(), "task observations must survive replay");
    assert_eq!(event.unwrap().to_body(), body);
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
fn editor_ready_body_validates_identity_and_tolerates_additions() {
    assert!(OrgEvent::from_parts("agent.editor_ready", &json!({"agent_id":"p/bad name"})).is_err());
    assert_eq!(
        OrgEvent::from_parts(
            "agent.editor_ready",
            &json!({"agent_id":"p/lead", "future":true})
        )
        .unwrap(),
        Some(OrgEvent::AgentEditorReady {
            agent_id: support::id()
        })
    );
}

// Break caught: silently dropping spoken-answer provenance as an unknown field.
#[test]
fn relayed_provisional_body_round_trips_without_changing_legacy_bytes() {
    let legacy = r#"{"by":"system","decision_id":"d3","option":"a"}"#;
    let body = serde_json::from_str(legacy).unwrap();
    let event = OrgEvent::from_parts("decision.taken_provisionally", &body)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_string(&event.to_body()).unwrap(), legacy);
    let marked = json!({"decision_id":"d3", "option":"b", "by":"owner", "relayed_by":"p/lead"});
    let event = OrgEvent::from_parts("decision.taken_provisionally", &marked)
        .unwrap()
        .unwrap();
    assert_eq!(event.to_body(), marked);
}

// Break caught: treating a relay designation/clear as a skipped unknown event.
#[test]
fn owner_relay_set_is_a_known_additive_event_with_explicit_clear() {
    for agent in [json!("p/lead"), json!(null)] {
        let body = json!({"by":"owner", "agent":agent});
        let event = OrgEvent::from_parts("owner.relay_set", &body)
            .unwrap()
            .expect("relay designation is a known event");
        assert_eq!(event.kind(), "owner.relay_set");
        assert_eq!(event.to_body(), body);
        let mut future = body;
        future["future"] = json!(true);
        assert_eq!(
            OrgEvent::from_parts(event.kind(), &future).unwrap(),
            Some(event)
        );
    }
}

// Break caught: accepting a malformed full agent identity in either new fact.
#[test]
fn relay_fact_bodies_refuse_malformed_agent_ids() {
    for bad in [json!("p/bad name"), json!("chief"), json!(1)] {
        assert!(
            OrgEvent::from_parts("owner.relay_set", &json!({"by":"owner", "agent":bad})).is_err()
        );
        assert!(OrgEvent::from_parts(
            "decision.taken_provisionally",
            &json!({
                "decision_id":"d3", "option":"a", "by":"owner", "relayed_by":bad
            })
        )
        .is_err());
    }
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

#[test]
fn delivery_notice_codec_and_legacy_bytes() {
    let legacy = r#"{"from":"owner","to":"p/lead","kind":"brief","body":"Work","attachments":["brief.md"],"task":null}"#;
    let content = support::content(Address::Agent(support::id()));
    assert_eq!(serde_json::to_string(&content).unwrap(), legacy);
    let event = OrgEvent::MessageSent(content);
    assert_eq!(
        serde_json::to_string(&event.to_body()).unwrap(),
        r#"{"attachments":["brief.md"],"body":"Work","from":"owner","kind":"brief","task":null,"to":"p/lead"}"#
    );
    let mut null = event.to_body();
    null["delivery_notice"] = json!(null);
    assert_eq!(
        OrgEvent::from_parts("msg.sent", &null).unwrap(),
        Some(event)
    );

    for source in ["m17".to_owned(), format!("m{}", u64::MAX)] {
        let body = json!({"from":"system", "to":"owner", "kind":"note",
            "body":"Delivery needs attention", "attachments":[], "task":null,
            "delivery_notice":{"source_message_id":source, "reason":"future_reason"}});
        let event = OrgEvent::from_parts("msg.sent", &body).unwrap().unwrap();
        assert_eq!(event.kind(), "msg.sent");
        assert_eq!(
            event.to_body(),
            body,
            "notice metadata must survive encoding"
        );
        assert_eq!(serde_json::to_value(&event).unwrap()["body"], body);
        let mut additive = body.clone();
        additive["future"] = json!(true);
        additive["delivery_notice"]["future"] = json!(true);
        assert_eq!(
            OrgEvent::from_parts("msg.sent", &additive).unwrap(),
            Some(event)
        );
    }
}

#[test]
fn delivery_notice_codec_rejects_malformed_metadata() {
    for notice in [
        json!(true),
        json!(42),
        json!({}),
        json!({"source_message_id":"m17"}),
        json!({"reason":"focus"}),
        json!({"source_message_id":"bad-id", "reason":"focus"}),
        json!({"source_message_id":17, "reason":"focus"}),
        json!({"source_message_id":"m17", "reason":true}),
    ] {
        let body = json!({"from":"system", "to":"owner", "kind":"note",
            "body":"Delivery needs attention", "attachments":[], "task":null,
            "delivery_notice":notice});
        assert!(OrgEvent::from_parts("msg.sent", &body).is_err(), "{body}");
    }
}

#[test]
fn legacy_message_decoder_ignores_delivery_notice() {
    // Frozen pre-addition body decoder: old readers keep the note, not its key.
    #[derive(serde::Deserialize)]
    struct LegacyMessageContent {
        from: Address,
        to: Address,
        kind: MessageKind,
        body: String,
        attachments: Vec<String>,
        task: Option<TaskId>,
    }
    let body = json!({"from":"system", "to":"owner", "kind":"note",
        "body":"Delivery needs attention", "attachments":["details.md"], "task":"t3",
        "delivery_notice":{"source_message_id":"m17", "reason":"focus"}});
    let event = OrgEvent::from_parts("msg.sent", &body).unwrap().unwrap();
    assert_eq!(event.to_body(), body);
    let old: LegacyMessageContent = serde_json::from_value(event.to_body()).unwrap();
    assert_eq!(old.from, Address::System);
    assert_eq!(old.to, Address::Owner);
    assert_eq!(old.kind, MessageKind::Note);
    assert_eq!(old.body, "Delivery needs attention");
    assert_eq!(old.attachments, ["details.md"]);
    assert_eq!(old.task, Some("t3".parse().unwrap()));
}

#[allow(clippy::unwrap_used)] // test builder
fn report(post: Option<MessageId>) -> OrgEvent {
    OrgEvent::MessageSent(MessageContent {
        delivery_notice: None,
        from: "p/lead".parse().unwrap(),
        to: Address::Owner,
        kind: MessageKind::Report,
        body: "done".into(),
        attachments: vec!["r.md".into()],
        task: Some("t1".parse().unwrap()),
        post,
    })
}

#[test]
fn message_without_post_keeps_legacy_bytes() {
    // The journal stores `to_body()`; these are the exact bytes written before `post` existed.
    let legacy = r#"{"attachments":["r.md"],"body":"done","from":"p/lead","kind":"report","task":"t1","to":"owner"}"#;
    let event = report(None);
    assert_eq!(serde_json::to_string(&event.to_body()).unwrap(), legacy);
    let read = OrgEvent::from_parts("msg.sent", &serde_json::from_str(legacy).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(read, event);
    let null = json!({"from":"p/lead", "to":"owner", "kind":"report", "body":"done",
        "attachments":["r.md"], "task":"t1", "post":null});
    let read = OrgEvent::from_parts("msg.sent", &null).unwrap().unwrap();
    assert_eq!(read, event, "an explicit null reads as no post");
    assert_eq!(serde_json::to_string(&read.to_body()).unwrap(), legacy);
}

#[test]
fn message_post_link_round_trips_and_legacy_reader_ignores_it() {
    #[derive(serde::Deserialize)]
    struct LegacyMessageContent {
        from: Address,
        to: Address,
        body: String,
        task: Option<TaskId>,
    }
    let body = json!({"from":"p/lead", "to":"owner", "kind":"report", "body":"done",
        "attachments":["r.md"], "task":"t1", "post":"m500"});
    let event = OrgEvent::from_parts("msg.sent", &body).unwrap().unwrap();
    assert_eq!(event, report(Some(MessageId::new(500))));
    assert_eq!(event.to_body(), body);
    let old: LegacyMessageContent = serde_json::from_value(event.to_body()).unwrap();
    assert_eq!(
        (old.from.to_string(), old.to),
        ("p/lead".into(), Address::Owner)
    );
    assert_eq!(
        (old.body, old.task),
        ("done".into(), Some("t1".parse().unwrap()))
    );
    for bad in [json!("500"), json!(500), json!("m05"), json!({"id":"m5"})] {
        let mut malformed = body.clone();
        malformed["post"] = bad;
        assert!(
            OrgEvent::from_parts("msg.sent", &malformed).is_err(),
            "{malformed}"
        );
    }
}

#[test]
fn same_except_post_ignores_only_post() {
    let OrgEvent::MessageSent(base) = report(None) else {
        panic!("not a message")
    };
    let linked = MessageContent {
        post: Some(MessageId::new(9)),
        ..base.clone()
    };
    assert!(base.same_except_post(&linked) && linked.same_except_post(&base));
    type Edit = fn(&mut MessageContent);
    let edits: [(&str, Edit); 7] = [
        ("from", |c| c.from = Address::System),
        ("to", |c| c.to = "p/other".parse().unwrap()),
        ("kind", |c| c.kind = MessageKind::Note),
        ("body", |c| c.body.push('!')),
        ("attachments", |c| c.attachments.clear()),
        ("task", |c| c.task = None),
        ("delivery_notice", |c| {
            c.delivery_notice = Some(DeliveryNotice {
                source_message_id: MessageId::new(1),
                reason: "focus".into(),
            })
        }),
    ];
    for (field, edit) in edits {
        let mut other = linked.clone();
        edit(&mut other);
        assert!(!base.same_except_post(&other), "{field} must count");
    }
}
