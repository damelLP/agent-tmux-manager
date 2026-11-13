//! Protocol 1.1 wire contracts; no daemon or harness required.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use serde_json::{json, Value};

fn client(payload: Value) -> Result<ClientMessage, serde_json::Error> {
    let mut envelope = json!({"protocol_version": {"major": 1, "minor": 1}});
    envelope
        .as_object_mut()
        .unwrap()
        .extend(payload.as_object().unwrap().clone());
    serde_json::from_value(envelope)
}

fn roundtrip_client(payload: Value) -> Value {
    let message = client(payload).unwrap();
    serde_json::to_value(message).unwrap()
}

#[test]
fn legacy_messages_keep_exact_json() {
    for payload in [
        r#""type":"connect""#,
        r#""type":"connect","client_id":"old-client""#,
        r#""type":"status_update","data":{}"#,
        r#""type":"hook_event","data":null"#,
        r#""type":"pi_event","data":{}"#,
        r#""type":"codex_event","data":{}"#,
        r#""type":"list_sessions""#,
        r#""type":"subscribe""#,
        r#""type":"subscribe","session_id":"s1""#,
        r#""type":"unsubscribe""#,
        r#""type":"ping","seq":42"#,
        r#""type":"disconnect""#,
        r#""type":"discover""#,
    ] {
        let line = format!(r#"{{"protocol_version":{{"major":1,"minor":0}},{payload}}}"#);
        let parsed: ClientMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), line);
    }
    for line in [
        r#"{"type":"connected","protocol_version":{"major":1,"minor":0},"client_id":"old-client"}"#,
        r#"{"type":"rejected","reason":"no","protocol_version":{"major":1,"minor":0}}"#,
        r#"{"type":"session_list","sessions":[]}"#,
        r#"{"type":"session_removed","session_id":"s1"}"#,
        r#"{"type":"pong","seq":42}"#,
        r#"{"type":"error","message":"no"}"#,
        r#"{"type":"error","message":"no","code":"old-code"}"#,
        r#"{"type":"discovery_complete","discovered":1,"failed":2}"#,
    ] {
        let parsed: DaemonMessage = serde_json::from_str(line).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), line);
    }
    let session = atm_core::SessionView::from_domain(&atm_core::SessionDomain::new(
        atm_core::SessionId::new("s1"),
        atm_core::AgentType::GeneralPurpose,
        atm_core::Model::Sonnet4,
    ));
    let payload = serde_json::to_string(&session).unwrap();
    assert_eq!(
        serde_json::to_string(&DaemonMessage::session_updated(session.clone())).unwrap(),
        format!(r#"{{"type":"session_updated","session":{payload}}}"#)
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::session_list(vec![session])).unwrap(),
        format!(r#"{{"type":"session_list","sessions":[{payload}]}}"#)
    );
}

#[test]
fn hook_connect_lines_remain_compatible() {
    // Source-formatting check at base 09804c3: only the extracted formatters
    // ran, with a synthetic session ID; these are NOT harness captures.
    // Bash 5.3.20 / Node 24.22.0; emitted bytes include the final LF.
    for (line, expected_id) in [
        (
            concat!(
                r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"hook-source-check-1063205"}"#,
                "\n"
            ),
            "hook-source-check-1063205",
        ),
        (
            concat!(
                r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"codex-hook-source-check-1063208"}"#,
                "\n"
            ),
            "codex-hook-source-check-1063208",
        ),
        (
            concat!(
                r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"pi-atm-1063211"}"#,
                "\n"
            ),
            "pi-atm-1063211",
        ),
        // Literal shell source templates, before runtime substitutions.
        (
            r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"hook-$SESSION_ID-$$"}"#,
            "hook-$SESSION_ID-$$",
        ),
        (
            r#"{"protocol_version":{"major":1,"minor":0},"type":"connect","client_id":"codex-hook-$SESSION_ID-$$"}"#,
            "codex-hook-$SESSION_ID-$$",
        ),
    ] {
        let parsed: ClientMessage = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.protocol_version, ProtocolVersion::new(1, 0));
        assert!(parsed
            .protocol_version
            .is_compatible_with(&ProtocolVersion::CURRENT));
        assert!(ProtocolVersion::CURRENT.is_compatible_with(&parsed.protocol_version));
        assert!(
            matches!(parsed.message, MessageType::Connect { client_id: Some(id) } if id == expected_id)
        );
    }
}

#[test]
fn current_version_is_1_1() {
    assert_eq!(ProtocolVersion::CURRENT.to_string(), "1.1");
    assert_eq!(ProtocolVersion::default(), ProtocolVersion::new(1, 1));
    assert_eq!(
        ClientMessage::connect(None).protocol_version,
        ProtocolVersion::new(1, 1)
    );
    assert!(!ProtocolVersion::new(1, 0).is_current());
    assert!(!ProtocolVersion::new(2, 0).is_compatible_with(&ProtocolVersion::CURRENT));
}

#[test]
fn request_contract() {
    let request =
        json!({"type":"request","id":"r1","method":"daemon.info","params":{},"from_pane":"%12"});
    let encoded = roundtrip_client(request.clone());
    for (key, value) in request.as_object().unwrap() {
        assert_eq!(encoded.get(key), Some(value));
    }
    for params in [json!(null), json!(42), json!([{"nested": [true, "é"]}])] {
        let encoded = roundtrip_client(
            json!({"type":"request","id":"r1","method":"daemon.info","params":params}),
        );
        assert_eq!(encoded.get("params"), Some(&params));
        assert!(encoded.get("from_pane").is_none());
    }
    let encoded = roundtrip_client(
        json!({"type":"request","id":"r1","method":"daemon.info","from_pane":null}),
    );
    assert_eq!(encoded.get("params"), Some(&Value::Null));
    assert!(encoded.get("from_pane").is_none());
    for invalid in [
        json!({"type":"request","method":"daemon.info"}),
        json!({"type":"request","id":"r1"}),
        json!({"type":"request","id":1,"method":"daemon.info"}),
        json!({"type":"request","id":"r1","method":false}),
        json!({"type":"request","id":"r1","method":"daemon.info","from_pane":12}),
    ] {
        assert!(client(invalid.clone()).is_err(), "{invalid}");
    }
}

#[test]
fn watch_defaults_and_boundaries() {
    for cursor in [0, 41, u64::MAX] {
        let encoded = roundtrip_client(json!({"type":"watch","cursor":cursor}));
        assert_eq!(encoded.get("topics"), Some(&json!([])));
        assert_eq!(encoded.get("cursor"), Some(&json!(cursor)));
    }
    let encoded = roundtrip_client(json!({"type":"watch","topics":["agent","msg"],"cursor":41}));
    assert_eq!(encoded.get("topics"), Some(&json!(["agent", "msg"])));
    for invalid in [
        json!({"type":"watch"}),
        json!({"type":"watch","topics":[]}),
        json!({"type":"watch","cursor":null}),
        json!({"type":"watch","cursor":-1}),
        json!({"type":"watch","cursor":1.5}),
        json!({"type":"watch","cursor":"41"}),
        json!({"type":"watch","cursor":0,"topics":null}),
        json!({"type":"watch","cursor":0,"topics":"agent"}),
        json!({"type":"watch","cursor":0,"topics":[42]}),
    ] {
        assert!(client(invalid.clone()).is_err(), "{invalid}");
    }
    assert!(serde_json::from_str::<ClientMessage>(
        r#"{"protocol_version":{"major":1,"minor":1},"type":"watch","cursor":18446744073709551616}"#
    )
    .is_err());
    assert_eq!(
        roundtrip_client(json!({"type":"unwatch"})).get("type"),
        Some(&json!("unwatch"))
    );
}

#[test]
fn response_contract() {
    for line in [
        r#"{"type":"response","id":"r1","result":{"protocol":"1.1","methods":["daemon.info"]}}"#,
        r#"{"type":"response","id":"r1","error":{"code":"unknown_method","message":"no method 'x.y'"}}"#,
        r#"{"type":"response","id":"r1","result":null}"#,
        r#"{"type":"response","id":"r1","result":[42,true,{"nested":null}]}"#,
    ] {
        let parsed: DaemonMessage = serde_json::from_str(line).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
        let encoded = serde_json::to_string(&parsed).unwrap();
        let reparsed: DaemonMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            serde_json::to_value(&reparsed).unwrap(),
            serde_json::to_value(parsed).unwrap()
        );
    }
}

#[test]
fn invalid_response_shapes() {
    for line in [
        r#"{"type":"response","id":"r1"}"#,
        r#"{"type":"response","id":"r1","result":{},"error":{"code":"x","message":"y"}}"#,
        r#"{"type":"response","id":"r1","error":{"code":"x","message":"y"},"result":{}}"#,
        r#"{"type":"response","id":"r1","result":null,"error":{"code":"x","message":"y"}}"#,
        r#"{"type":"response","id":"r1","result":{},"error":null}"#,
        r#"{"type":"response","id":"r1","result":null,"error":null}"#,
        r#"{"type":"response","id":"r1","error":null}"#,
        r#"{"type":"response","id":"r1","error":{}}"#,
        r#"{"type":"response","id":"r1","error":{"code":"x"}}"#,
        r#"{"type":"response","id":"r1","error":{"message":"y"}}"#,
        r#"{"type":"response","id":"r1","error":"x"}"#,
        r#"{"type":"response","id":42,"result":null}"#,
        r#"{"type":"response","result":null}"#,
        r#"{"type":"response","id":"r1","result":null,"result":1}"#,
        r#"{"type":"response","id":"r1","error":{"code":"x","message":"y"},"error":{"code":"x","message":"y"}}"#,
    ] {
        assert!(
            serde_json::from_str::<DaemonMessage>(line).is_err(),
            "accepted {line}"
        );
    }
}

#[test]
fn connected_methods_compatibility() {
    for methods in [
        None,
        Some(json!([])),
        Some(json!(["daemon.info", "agent.list"])),
    ] {
        let mut line =
            json!({"type":"connected","protocol_version":{"major":1,"minor":0},"client_id":"old"});
        if let Some(methods) = &methods {
            line.as_object_mut()
                .unwrap()
                .insert("methods".into(), methods.clone());
        }
        let parsed: DaemonMessage = serde_json::from_value(line).unwrap();
        let encoded = serde_json::to_value(parsed).unwrap();
        if methods
            .as_ref()
            .is_some_and(|v| v == &json!(["daemon.info", "agent.list"]))
        {
            assert_eq!(encoded.get("methods"), methods.as_ref());
        } else {
            assert!(encoded.get("methods").is_none());
        }
    }
    for methods in [json!(null), json!("daemon.info"), json!([42])] {
        assert!(serde_json::from_value::<DaemonMessage>(json!({"type":"connected","protocol_version":{"major":1,"minor":0},"client_id":"old","methods":methods})).is_err());
    }
}

#[test]
fn event_contract() {
    for seq in [42, u64::MAX] {
        for body in [json!({}), json!(null), json!({"nested":["é", true]})] {
            let expected = json!({"type":"event","topic":"agent","seq":seq,"ts":"2026-11-13T21:04:05Z","kind":"agent.session_bound","body":body});
            let parsed: DaemonMessage = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), expected);
        }
    }
}

#[test]
fn constructors_follow_wire_contract() {
    use atm_protocol::{error_code, ResponsePayload};
    let request = ClientMessage::request(
        "r1".into(),
        "daemon.info".into(),
        json!({}),
        Some("%12".into()),
    );
    assert_eq!(
        serde_json::to_string(&request).unwrap(),
        r#"{"protocol_version":{"major":1,"minor":1},"type":"request","id":"r1","method":"daemon.info","params":{},"from_pane":"%12"}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::watch(
            vec!["agent".into(), "msg".into()],
            41
        ))
        .unwrap(),
        r#"{"protocol_version":{"major":1,"minor":1},"type":"watch","topics":["agent","msg"],"cursor":41}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::unwatch()).unwrap(),
        r#"{"protocol_version":{"major":1,"minor":1},"type":"unwatch"}"#
    );
    let ok = DaemonMessage::response_ok(
        "r1".into(),
        json!({"protocol":"1.1","methods":["daemon.info"]}),
    );
    assert_eq!(
        serde_json::to_value(ok).unwrap(),
        json!({"type":"response","id":"r1","result":{"protocol":"1.1","methods":["daemon.info"]}})
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::response_ok("r1".into(), Value::Null)).unwrap(),
        r#"{"type":"response","id":"r1","result":null}"#
    );
    let err =
        DaemonMessage::response_err("r1".into(), error_code::UNKNOWN_METHOD, "no method 'x.y'");
    assert_eq!(
        serde_json::to_string(&err).unwrap(),
        r#"{"type":"response","id":"r1","error":{"code":"unknown_method","message":"no method 'x.y'"}}"#
    );
    assert!(matches!(
        err,
        DaemonMessage::Response {
            payload: ResponsePayload::Error(_),
            ..
        }
    ));
    let event = DaemonMessage::event(
        "agent".into(),
        42,
        "2026-11-13T21:04:05Z".into(),
        "agent.session_bound".into(),
        json!({}),
    );
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"type":"event","topic":"agent","seq":42,"ts":"2026-11-13T21:04:05Z","kind":"agent.session_bound","body":{}}"#
    );
    let connected = DaemonMessage::connected("c1".into());
    assert!(matches!(&connected, DaemonMessage::Connected { methods, .. } if methods.is_empty()));
    assert!(serde_json::to_value(connected)
        .unwrap()
        .get("methods")
        .is_none());
    let legacy: DaemonMessage = serde_json::from_str(
        r#"{"type":"connected","protocol_version":{"major":1,"minor":0},"client_id":"old"}"#,
    )
    .unwrap();
    assert!(matches!(legacy, DaemonMessage::Connected { methods, .. } if methods.is_empty()));
}

#[test]
fn error_code_constants() {
    use atm_protocol::error_code::*;
    for (code, expected) in [
        (UNKNOWN_METHOD, "unknown_method"),
        (INVALID_PARAMS, "invalid_params"),
        (NOT_FOUND, "not_found"),
        (CONFLICT, "conflict"),
        (REFUSED, "refused"),
        (TIMEOUT, "timeout"),
        (UNAVAILABLE, "unavailable"),
        (INTERNAL, "internal"),
    ] {
        let encoded =
            serde_json::to_value(DaemonMessage::response_err("r1".into(), code, "message"))
                .unwrap();
        assert_eq!(encoded.pointer("/error/code"), Some(&json!(expected)));
    }
}

#[test]
fn unknown_message_type_rejected() {
    assert!(client(json!({"type":"unknown_future_type"})).is_err());
    assert!(
        serde_json::from_value::<DaemonMessage>(json!({"type":"unknown_future_type"})).is_err()
    );
}
