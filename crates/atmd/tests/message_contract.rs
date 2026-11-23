//! Disabled confirmed-message surface: real registration, no delivery success.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atmd::org::messages::*;
use atmd::{
    registry::spawn_registry,
    server::methods::{router, CallContext, CallerId, MethodServices},
};
use serde_json::{json, Value};

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("message-contract".into()),
        from_pane: None,
    }
}

fn valid_calls() -> Vec<(&'static str, Value)> {
    vec![
        ("msg.send", json!({"to":"owner","body":"hello"})),
        ("msg.inbox", Value::Null),
        ("msg.ack", json!({"ids":["m1"]})),
        ("msg.retry", json!({"id":"m1"})),
        ("msg.give_up", json!({"id":"m1"})),
    ]
}

#[tokio::test]
async fn advertises_all_five_methods_even_without_service() {
    let router = router(MethodServices::new(spawn_registry())).unwrap();
    let info = router
        .call(caller(), "daemon.info", Value::Null)
        .await
        .unwrap();
    for (name, _) in valid_calls() {
        assert!(router.names().iter().any(|n| n == name), "missing {name}");
        assert!(info["methods"].as_array().unwrap().contains(&json!(name)));
    }
}

#[tokio::test]
async fn disabled_calls_return_unavailable_without_org_or_tmux_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let org = atmd::org::OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let mut services = MethodServices::new(spawn_registry());
    services.org = org.clone();
    let mock = atm_tmux::MockTmuxClient::new();
    let (control, task) = atmd::control::service::spawn_control(
        Box::new(mock.clone()),
        services.registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    services.control = Some(control.clone());
    let router = router(services).unwrap();
    for (method, params) in valid_calls() {
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable", "{method}");
        assert_eq!(
            error.0.message,
            "message service unavailable: confirmed-message operations are not configured"
        );
    }
    // Returned calls and a FIFO snapshot fence synchronize observations.
    assert!(org.snapshot().await.unwrap().messages().next().is_none());
    assert!(org.tail(Default::default()).await.unwrap().is_empty());
    assert!(mock.calls().is_empty());
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn strict_schemas_reject_unknown_fields_nonobjects_and_bad_ids() {
    let router = router(MethodServices::new(spawn_registry())).unwrap();
    for (name, params) in valid_calls() {
        let mut unknown = if params.is_null() { json!({}) } else { params };
        unknown["unexpected"] = json!(true);
        for params in [unknown, json!([]), json!(true), json!("x")] {
            assert_eq!(
                router
                    .call(caller(), name, params)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                "invalid_params",
                "{name}"
            );
        }
    }
    for (name, params) in [
        ("msg.send", json!({"to":"not-an-address","body":"hi"})),
        (
            "msg.send",
            json!({"to":"owner","body":"hi","kind":"unknown"}),
        ),
        (
            "msg.send",
            json!({"to":"owner","body":"hi","from":"system"}),
        ),
        ("msg.inbox", json!({"id":"m01"})),
        ("msg.inbox", json!({"after":"m-1"})),
        ("msg.ack", json!({"ids":[1]})),
        ("msg.retry", json!({"id":"m01"})),
        ("msg.give_up", json!({"id":"m18446744073709551616"})),
    ] {
        assert_eq!(
            router
                .call(caller(), name, params)
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params",
            "{name}"
        );
    }
}

#[test]
fn schema_defaults_preserve_body_and_serialize_optional_fields_as_null() {
    let send: SendParams =
        serde_json::from_value(json!({"to":"p/agent","body":"  first\n\nsecond 雪"})).unwrap();
    assert_eq!(
        serde_json::to_value(send).unwrap(),
        json!({"to":"p/agent","kind":"note","body":"  first\n\nsecond 雪","attachments":[],"task":null})
    );
    let inbox: InboxParams = atmd::server::methods::parse_params(Value::Null).unwrap();
    assert_eq!(
        serde_json::to_value(inbox).unwrap(),
        json!({"for":null,"unread_only":false,"count_only":false,"id":null,"after":null,"limit":100})
    );
    for phase in [
        "queued",
        "waiting",
        "delivering",
        "blocked",
        "delivered",
        "failed",
    ] {
        let view: DeliveryView =
            serde_json::from_value(json!({"phase":phase,"attempt":0})).unwrap();
        assert_eq!(
            serde_json::to_value(view).unwrap(),
            json!({"phase":phase,"reason":null,"attempt":0,"pane":null,"session_id":null,"confirmation":null,"retry_at":null,"blocked_by":null})
        );
    }
}

#[test]
fn result_projection_reuses_domain_content_and_same_id_retry() {
    let entry = json!({
        "message_id":"m7", "content":{"from":"p/agent","to":"owner","kind":"report","body":"\nbody\n","attachments":["brief.md"],"task":null},
        "sent_at":"2026-11-14T12:00:00Z", "status":"delivered", "read_at":null,
        "delivery":{"phase":"delivered","reason":null,"attempt":1,"pane":"%7","session_id":"real-session","confirmation":"prompt_submit_exact","retry_at":null,"blocked_by":null}
    });
    let result = json!({"messages":[entry],"unread_count":1,"next_after":null});
    let parsed: InboxResult = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), result);
    let retry: RetryResult =
        serde_json::from_value(json!({"message_id":"m7","status":"queued","future":true})).unwrap();
    assert_eq!(
        serde_json::to_value(retry).unwrap(),
        json!({"message_id":"m7","status":"queued"})
    );
    let send: SendResult =
        serde_json::from_value(json!({"message_id":"m7","future":true})).unwrap();
    assert_eq!(
        serde_json::to_value(send).unwrap(),
        json!({"message_id":"m7"})
    );
    let ack: AckResult = serde_json::from_value(json!({"count":2,"future":true})).unwrap();
    assert_eq!(serde_json::to_value(ack).unwrap(), json!({"count":2}));
    let give_up: GiveUpResult =
        serde_json::from_value(json!({"message_id":"m7","status":"failed","future":true})).unwrap();
    assert_eq!(
        serde_json::to_value(give_up).unwrap(),
        json!({"message_id":"m7","status":"failed"})
    );
}

// s3-giverelay. Break: the owner's give-up gains a key an old daemon refuses,
// the relay words are dropped, or retry starts accepting them.
#[tokio::test]
async fn give_up_relay_words_are_optional_and_give_up_only() {
    let plain: GiveUpParams = serde_json::from_value(json!({"id":"m7"})).unwrap();
    assert_eq!(plain.relay_words, None);
    assert_eq!(serde_json::to_value(plain).unwrap(), json!({"id":"m7"}));
    let relayed = json!({"id":"m7","relay_words":"Cancel them"});
    let parsed: GiveUpParams = serde_json::from_value(relayed.clone()).unwrap();
    assert_eq!(parsed.relay_words.as_deref(), Some("Cancel them"));
    assert_eq!(serde_json::to_value(parsed).unwrap(), relayed);
    let router = router(MethodServices::new(spawn_registry())).unwrap();
    for method in ["msg.retry", "wake.retry"] {
        let error = router
            .call(caller(), method, relayed.clone())
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "invalid_params", "{method}");
    }
    for method in ["msg.give_up", "wake.give_up"] {
        let error = router
            .call(caller(), method, relayed.clone())
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "unavailable", "{method}");
    }
}

// Break: dropping the additive advisory, rejecting old replies, or accepting a malformed known field.
#[test]
fn give_up_result_roundtrips_optional_editor_warning() {
    let warning = "Possible leftover in the recipient's editor: [m7] Read: atm inbox --id m7. Giving up did not clear the editor; further delivery may wait for an empty editor. Inspect the pane before editing it.";
    let reply = json!({"message_id":"m7","status":"failed","editor_warning":warning});
    let parsed: GiveUpResult = serde_json::from_value(reply.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), reply);
    for old in [
        json!({"message_id":"m7","status":"failed"}),
        json!({"message_id":"m7","status":"failed","editor_warning":null,"future":true}),
    ] {
        let parsed: GiveUpResult = serde_json::from_value(old).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            json!({"message_id":"m7","status":"failed"})
        );
    }
    let parsed: GiveUpResult = serde_json::from_value(
        json!({"message_id":"m7","status":"failed","editor_warning":warning,"future":true}),
    )
    .unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), reply);
    for invalid in [json!(true), json!(7), json!([]), json!({})] {
        assert!(serde_json::from_value::<GiveUpResult>(
            json!({"message_id":"m7","status":"failed","editor_warning":invalid})
        )
        .is_err());
    }
}

#[tokio::test]
async fn explicit_disabled_facade_preserves_its_cause_for_every_call() {
    let handle = MessageHandle::disabled("test cause");
    let mut errors = vec![handle.initialized().await.unwrap_err()];
    errors.push(
        handle
            .send(
                serde_json::from_value(json!({"to":"owner","body":"hi"})).unwrap(),
                caller(),
            )
            .await
            .unwrap_err(),
    );
    errors.push(
        handle
            .inbox(serde_json::from_value(json!({})).unwrap(), caller())
            .await
            .unwrap_err(),
    );
    errors.push(
        handle
            .ack(
                serde_json::from_value(json!({"ids":["m1"]})).unwrap(),
                caller(),
            )
            .await
            .unwrap_err(),
    );
    errors.push(
        handle
            .retry(
                ResolveParams {
                    id: MessageId::new(1),
                },
                caller(),
            )
            .await
            .unwrap_err(),
    );
    errors.push(
        handle
            .give_up(
                GiveUpParams {
                    id: MessageId::new(1),
                    relay_words: None,
                },
                caller(),
            )
            .await
            .unwrap_err(),
    );
    for error in errors {
        assert!(matches!(error, MessageError::Unavailable(ref cause) if cause == "test cause"));
    }
    let mut services = MethodServices::new(spawn_registry());
    services.messages = Some(handle);
    let router = router(services).unwrap();
    for (name, params) in valid_calls() {
        assert_eq!(
            router
                .call(caller(), name, params)
                .await
                .unwrap_err()
                .0
                .message,
            "message service unavailable: test cause"
        );
    }
}

#[test]
fn typed_errors_use_existing_wire_codes_and_do_not_claim_rollback() {
    use atmd::{
        control::reservation::ReservationError, org::OrgServiceError, server::methods::MethodError,
    };
    for (error, code) in [
        (MessageError::InvalidParams("bad".into()), "invalid_params"),
        (MessageError::NotFound("missing".into()), "not_found"),
        (MessageError::Conflict("busy".into()), "conflict"),
        (MessageError::Refused("unsafe".into()), "refused"),
        (MessageError::Timeout, "timeout"),
        (MessageError::Unavailable("off".into()), "unavailable"),
        (MessageError::Internal("broken".into()), "internal"),
        (
            MessageError::Organisation(OrgServiceError::Unavailable("writer lost".into())),
            "unavailable",
        ),
        (
            MessageError::Reservation(ReservationError::Unavailable),
            "unavailable",
        ),
        (
            MessageError::Reservation(ReservationError::Timeout),
            "timeout",
        ),
        (
            MessageError::Reservation(ReservationError::StaleReservation),
            "refused",
        ),
    ] {
        let expected_message = error.to_string();
        let mapped: MethodError = error.into();
        assert_eq!(mapped.0.code, code);
        assert_eq!(mapped.0.message, expected_message);
    }
    assert_eq!(MessageError::Timeout.to_string(), "message request timed out; accepted work may have completed; inspect inbox before retrying");
}

#[test]
fn organisation_delivery_state_conflicts_are_not_recipient_refusals() {
    use atm_org::{ApplyError, Refusal};
    use atmd::{org::OrgServiceError, server::methods::MethodError};
    for (refusal, code) in [
        (
            Refusal::InvalidDeliveryState {
                message_id: MessageId::new(1),
                state: DeliveryStatus::Queued,
                operation: "read",
            },
            "conflict",
        ),
        (
            Refusal::WrongRecipient {
                message_id: MessageId::new(1),
                who: Address::Owner,
            },
            "refused",
        ),
        (Refusal::InvalidRecipient(Address::System), "refused"),
        (
            Refusal::Invariant(ApplyError::UnknownMessage(MessageId::new(1))),
            "not_found",
        ),
    ] {
        let error: MethodError =
            MessageError::Organisation(OrgServiceError::Refused(refusal)).into();
        assert_eq!(error.0.code, code);
    }
}

#[tokio::test]
async fn system_is_never_a_message_recipient_or_inbox() {
    let router = router(MethodServices::new(spawn_registry())).unwrap();
    for (name, params) in [
        ("msg.send", json!({"to":"system","body":"hi"})),
        ("msg.inbox", json!({"for":"system"})),
    ] {
        assert_eq!(
            router
                .call(caller(), name, params)
                .await
                .unwrap_err()
                .0
                .code,
            "refused"
        );
    }
}
