//! The `channel.post` wire types (design 16.1): strict params with their defaults,
//! and the exact result shape a client reads.
#![allow(clippy::unwrap_used)] // Literal fixtures.

use atm_comms::channel_types::{
    ChannelPostParams, ChannelPostResult, PostRouting, UnavailableRow, UnresolvedReason,
    UnresolvedRow, WokeRow,
};
use atm_org::{AgentStatus, Mention, MessageId, MessageKind};
use serde_json::{json, Value};

fn parse(value: Value) -> Result<ChannelPostParams, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

// Break: a field a request could use to mark a post as the daemon's own, a
// wrong default, or a loose schema.
#[test]
fn post_params_are_strict_and_default() {
    let p = parse(json!({"channel":"alpha","body":"hi"})).unwrap();
    assert_eq!(p.channel.unwrap().to_string(), "alpha");
    assert!(p.to.is_none() && p.reply_to.is_none());
    assert_eq!(p.kind, MessageKind::Note);
    assert!(p.attachments.is_empty() && p.tags.is_empty());
    assert!(p.task.is_none() && p.client_key.is_none());

    let p = parse(
        json!({"to":"alpha/lead","body":"","kind":"report","attachments":["a.md"],
        "task":"t1","tags":["build-2","alpha/build-4"],"client_key":"k-1"}),
    )
    .unwrap();
    assert_eq!(p.to.unwrap().to_string(), "alpha/lead");
    assert_eq!(p.kind, MessageKind::Report);
    assert_eq!(p.tags, ["build-2", "alpha/build-4"]);
    let p = parse(json!({"reply_to":"m13","body":"x"})).unwrap();
    assert_eq!(p.reply_to, Some(MessageId::new(13)));

    for (params, field) in [
        (
            json!({"channel":"alpha","body":"x","internal":true}),
            "internal",
        ),
        (
            json!({"channel":"alpha","body":"x","decision":{"decision_id":"d1","step":"asked"}}),
            "decision",
        ),
        (
            json!({"channel":"alpha","body":"x","no_route":true}),
            "no_route",
        ),
        (json!({"channel":"alpha","body":"x","groups":{}}), "groups"),
    ] {
        let error = parse(params).unwrap_err();
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{error}"
        );
    }
    for params in [
        json!({"channel":"alpha"}),
        json!({"channel":"alpha","body":"x","tags":"build-2"}),
        json!({"channel":"alpha","body":"x","reply_to":13}),
        json!({"channel":"alpha","body":"x","kind":"shout"}),
        json!({"to":"nobody at all","body":"x"}),
    ] {
        assert!(parse(params.clone()).is_err(), "{params}");
    }
}

#[test]
fn sharing_param_is_boolean_and_only_true_is_written() {
    let base = json!({"reply_to":"m13","body":"hi"});
    for flag in [None, Some(false), Some(true)] {
        let mut input = base.clone();
        if let Some(flag) = flag {
            input["also_in_chat"] = json!(flag);
        }
        let encoded = serde_json::to_value(parse(input).unwrap()).unwrap();
        assert_eq!(
            encoded.get("also_in_chat"),
            flag.filter(|flag| *flag).map(Value::Bool).as_ref()
        );
    }
    for bad in [Value::Null, json!("true"), json!(1)] {
        let mut input = base.clone();
        input["also_in_chat"] = bad;
        assert!(parse(input).is_err());
    }
}

// Break: a renamed or dropped field, an omitted null, or a non-snake_case tag.
#[test]
fn post_result_serializes_the_documented_shape() {
    let result = ChannelPostResult {
        seq: 503,
        post_id: MessageId::new(500),
        channel: serde_json::from_value(json!("alpha")).unwrap(),
        thread: None,
        mentions: vec![
            Mention::Seat("alpha/build-2".parse().unwrap()),
            Mention::Owner,
        ],
        woke: vec![WokeRow {
            to: "alpha/build-2".parse().unwrap(),
            message_id: MessageId::new(501),
        }],
        unavailable: vec![UnavailableRow {
            to: "alpha/build-2".parse().unwrap(),
            state: AgentStatus::Down,
            since: "2026-01-01T21:40:00Z".parse().unwrap(),
            message_id: MessageId::new(501),
        }],
        route: PostRouting::None,
        unresolved: vec![UnresolvedRow {
            text: "@nobody".into(),
            reason: UnresolvedReason::EmptyGroup,
        }],
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"seq":503,"post_id":"m500","channel":"alpha","thread":null,
            "mentions":[{"seat":"alpha/build-2"},"owner"],
            "woke":[{"to":"alpha/build-2","message_id":"m501"}],
            "unavailable":[{"to":"alpha/build-2","state":"down","since":"2026-01-01T21:40:00Z","message_id":"m501"}],
            "route":"none","unresolved":[{"text":"@nobody","reason":"empty_group"}]})
    );
    for (reason, text) in [
        (UnresolvedReason::Malformed, "malformed"),
        (UnresolvedReason::Unknown, "unknown"),
        (UnresolvedReason::Ambiguous, "ambiguous"),
        (UnresolvedReason::EmptyGroup, "empty_group"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(text));
    }
}
