#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! Channel ids and the bodies of the three channel kinds, byte for byte.
use atm_org::*;
use proptest::prelude::*;
use serde_json::{json, Value};

fn address(text: &str) -> Address {
    text.parse().unwrap()
}
fn channel(text: &str) -> ChannelId {
    ChannelId::from(text.to_owned())
}

#[test]
fn channel_id_text_forms() {
    assert_eq!(
        channel("agent-tmux-manager"),
        ChannelId::Project("agent-tmux-manager".parse().unwrap())
    );
    assert_eq!(
        channel("atm/lead+owner"),
        ChannelId::Direct(address("atm/lead"), address("owner"))
    );
    assert_eq!(
        channel("owner+system"),
        ChannelId::Direct(Address::Owner, Address::System)
    );
    assert_eq!(
        channel("atm/build-2+atm/lead"),
        ChannelId::Direct(address("atm/build-2"), address("atm/lead"))
    );
    // Neither a project id nor a canonical pair of distinct addresses: kept
    // as written, never normalised, never an error.
    for text in [
        "owner+atm/lead",
        "atm/lead+atm/build-2",
        "system+owner",
        "owner+owner",
        "atm/lead+atm/lead",
        "atm/lead+owner+system",
        "atm/lead",
        "Atm",
        "atm/lead+",
        "+owner",
        "atm/Lead+owner",
        "",
        "x+y+z",
    ] {
        let id = channel(text);
        assert!(
            matches!(&id, ChannelId::Other(other) if other.as_str() == text),
            "{text}"
        );
        assert!(!id.is_known(), "{text}");
        assert_eq!(id.to_string(), text);
    }
    // `owner` is a valid project id: only agent names reserve it.
    assert_eq!(
        channel("owner"),
        ChannelId::Project("owner".parse().unwrap())
    );
    for known in ["atm", "atm/lead+owner", "owner+system"] {
        assert!(channel(known).is_known(), "{known}");
    }
}

#[test]
fn direct_ids_follow_address_order() {
    // The derived Address order: agents by project then name, then owner, then
    // system. It is not text order: "a-b/c" < "a/z" as text, but project "a"
    // sorts before project "a-b".
    for (a, b, text) in [
        ("zeta/lead", "owner", "zeta/lead+owner"),
        ("a-b/c", "a/z", "a/z+a-b/c"),
        ("system", "owner", "owner+system"),
        ("p/b", "p/a", "p/a+p/b"),
    ] {
        let id = ChannelId::direct(address(a), address(b)).unwrap();
        assert_eq!(id, ChannelId::direct(address(b), address(a)).unwrap());
        assert_eq!(id.to_string(), text);
        assert_eq!(channel(text), id);
        assert!(id.is_known());
    }
    assert_eq!(ChannelId::direct(Address::Owner, Address::Owner), None);
    // A pair built out of order by hand is not a form commands may write.
    let backwards = ChannelId::Direct(Address::Owner, address("p/a"));
    assert!(!backwards.is_known());
}

fn arb_address() -> impl Strategy<Value = Address> {
    prop_oneof![
        Just(Address::Owner),
        Just(Address::System),
        ("[a-z0-9][a-z0-9-]{0,8}", "[a-z0-9][a-z0-9-]{0,8}").prop_filter_map(
            "reserved name",
            |(project, name)| AgentId::new(project.parse().ok()?, name)
                .ok()
                .map(Address::Agent)
        ),
    ]
}

proptest! {
    #[test]
    fn channel_id_text_is_lossless(text in any::<String>(), pick in "[a-z0-9/+-]{0,20}") {
        for text in [text, pick] {
            let id = ChannelId::from(text.clone());
            prop_assert_eq!(id.to_string(), text.clone());
            prop_assert_eq!(serde_json::to_value(&id).unwrap(), Value::String(text.clone()));
            let read: ChannelId = serde_json::from_value(Value::String(text)).unwrap();
            prop_assert_eq!(read, id);
        }
    }

    #[test]
    fn known_channel_ids_round_trip(a in arb_address(), b in arb_address(), project in "[a-z0-9][a-z0-9-]{0,31}") {
        let project = ChannelId::Project(project.parse().unwrap());
        prop_assert_eq!(ChannelId::from(project.to_string()), project);
        if let Some(direct) = ChannelId::direct(a.clone(), b.clone()) {
            prop_assert!(direct.is_known());
            prop_assert_eq!(ChannelId::from(direct.to_string()), direct);
        } else {
            prop_assert_eq!(a, b);
        }
    }
}

#[test]
fn channel_ids_order_projects_then_directs() {
    let mut ids = [
        channel("x+y+z"),
        channel("owner+system"),
        channel("zeta"),
        channel("atm/lead+owner"),
        channel("atm"),
    ];
    ids.sort();
    let text: Vec<_> = ids.iter().map(ToString::to_string).collect();
    assert_eq!(
        text,
        ["atm", "zeta", "atm/lead+owner", "owner+system", "x+y+z"]
    );
}

/// The journal stores a body as a `Value`: object keys are written sorted.
fn bytes(value: &impl serde::Serialize) -> String {
    serde_json::to_string(&serde_json::to_value(value).unwrap()).unwrap()
}

fn lead_post() -> PostContent {
    PostContent {
        channel: channel("atm"),
        author: address("atm/lead"),
        session_id: Some(atm_core::SessionId::new("s-41")),
        kind: MessageKind::Note,
        body: "@build-2 hold W2".into(),
        attachments: vec!["plan.md".into()],
        task: Some("t9".parse().unwrap()),
        thread: Some("m480".parse().unwrap()),
        mentions: vec![
            Mention::Owner,
            Mention::Seat("atm/build-2".parse().unwrap()),
            Mention::Group {
                project: "atm".parse().unwrap(),
                name: "builders".into(),
            },
        ],
        wake: vec![address("atm/build-2"), Address::Owner],
        route: false,
        decision: Some(DecisionStep {
            decision_id: "d500".parse().unwrap(),
            step: "asked".into(),
        }),
        client_key: Some("k-1".into()),
        also_in_chat: false,
    }
}

#[test]
fn post_body_golden_full_and_minimal() {
    let full = r#"{"attachments":["plan.md"],"author":"atm/lead","body":"@build-2 hold W2","channel":"atm","client_key":"k-1","decision":{"decision_id":"d500","step":"asked"},"kind":"note","mentions":["owner",{"seat":"atm/build-2"},{"group":{"name":"builders","project":"atm"}}],"route":false,"session_id":"s-41","task":"t9","thread":"m480","wake":["atm/build-2","owner"]}"#;
    assert_eq!(bytes(&lead_post()), full);
    let read: PostContent = serde_json::from_str(full).unwrap();
    assert_eq!(read, lead_post());
    assert!(read.is_known());

    // Optional facts are null; `decision` and `client_key` are omitted.
    let minimal = PostContent {
        channel: channel("atm"),
        author: Address::Owner,
        session_id: None,
        kind: MessageKind::Note,
        body: "is W1 still on track?".into(),
        attachments: vec![],
        task: None,
        thread: None,
        mentions: vec![],
        wake: vec![],
        route: true,
        decision: None,
        client_key: None,
        also_in_chat: false,
    };
    let text = r#"{"attachments":[],"author":"owner","body":"is W1 still on track?","channel":"atm","kind":"note","mentions":[],"route":true,"session_id":null,"task":null,"thread":null,"wake":[]}"#;
    assert_eq!(bytes(&minimal), text);
    assert_eq!(serde_json::from_str::<PostContent>(text).unwrap(), minimal);
}

#[test]
fn also_in_chat_survives_replay_and_false_keeps_the_old_body() {
    let original = serde_json::to_value(lead_post()).unwrap();
    assert!(original.get("also_in_chat").is_none());
    for flag in [true, false] {
        let mut body = original.clone();
        body["also_in_chat"] = json!(flag);
        let decoded: PostContent = serde_json::from_value(body.clone()).unwrap();
        let encoded = serde_json::to_value(decoded).unwrap();
        assert_eq!(encoded, if flag { body } else { original.clone() });
    }
}

#[test]
fn post_body_requires_its_fields() {
    let full = serde_json::to_value(lead_post()).unwrap();
    for key in [
        "channel",
        "author",
        "kind",
        "body",
        "attachments",
        "mentions",
        "wake",
        "route",
    ] {
        let mut body = full.clone();
        body.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<PostContent>(body).is_err(),
            "{key} is required"
        );
    }
    for key in ["session_id", "task", "thread", "decision", "client_key"] {
        let mut body = full.clone();
        body.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<PostContent>(body).is_ok(),
            "{key} may be absent"
        );
    }
    let mut kind = full.clone();
    kind["kind"] = json!("digest");
    assert!(
        serde_json::from_value::<PostContent>(kind).is_err(),
        "MessageKind has no catch-all: a later kind must not be written here"
    );
}

#[test]
fn routed_body_golden_router_and_stopgap() {
    let router = PostRouted {
        post: "m510".parse().unwrap(),
        wake: vec![address("atm/lead")],
        by: RouteBy::Router {
            name: "local".into(),
            model: Some("m-7b".into()),
        },
        reason: Some("asked about W1".into()),
    };
    let text = r#"{"by":{"kind":"router","model":"m-7b","name":"local"},"post":"m510","reason":"asked about W1","wake":["atm/lead"]}"#;
    assert_eq!(bytes(&router), text);
    assert_eq!(serde_json::from_str::<PostRouted>(text).unwrap(), router);

    let modelless: RouteBy =
        serde_json::from_value(json!({"kind":"router","name":"fake"})).unwrap();
    assert_eq!(
        modelless,
        RouteBy::Router {
            name: "fake".into(),
            model: None
        }
    );
    assert_eq!(
        bytes(&modelless),
        r#"{"kind":"router","model":null,"name":"fake"}"#
    );

    let stopgap = PostRouted {
        post: "m510".parse().unwrap(),
        wake: vec![],
        by: RouteBy::Stopgap {
            cause: "disabled".into(),
        },
        reason: None,
    };
    let text =
        r#"{"by":{"cause":"disabled","kind":"stopgap"},"post":"m510","reason":null,"wake":[]}"#;
    assert_eq!(bytes(&stopgap), text);
    assert_eq!(serde_json::from_str::<PostRouted>(text).unwrap(), stopgap);
    for cause in ["disabled", "no_router", "timeout", "declined"] {
        assert!(RouteBy::Stopgap {
            cause: cause.into()
        }
        .is_known());
    }
}

#[test]
fn seen_body_golden() {
    let seen = ChannelSeen {
        channel: channel("atm/lead+owner"),
        reader: Address::Owner,
        through: "m512".parse().unwrap(),
        session_id: None,
    };
    let text =
        r#"{"channel":"atm/lead+owner","reader":"owner","session_id":null,"through":"m512"}"#;
    assert_eq!(bytes(&seen), text);
    assert_eq!(serde_json::from_str::<ChannelSeen>(text).unwrap(), seen);
}

#[test]
fn later_forms_decode_into_catch_alls_and_keep_their_bytes() {
    let mentions = json!([
        "everyone",
        {"role": "builders"},
        {"seat": "Bad/Id"},
        {"seat": 7},
        {"seat": "atm/a", "group": {"project": "atm", "name": "b"}},
        {"group": {"project": "Bad", "name": "b"}},
        {"group": {"project": "atm"}},
        ["owner"],
        null,
        "owner",
    ]);
    let mut body = serde_json::to_value(lead_post()).unwrap();
    body["mentions"] = mentions.clone();
    body["channel"] = json!("x+y+z");
    body["decision"]["step"] = json!("deferred");
    let post: PostContent = serde_json::from_value(body.clone()).unwrap();
    let known: Vec<_> = post.mentions.iter().map(Mention::is_known).collect();
    let mut expected = vec![false; 9];
    expected.push(true);
    assert_eq!(known, expected);
    for (mention, raw) in post.mentions.iter().zip(mentions.as_array().unwrap()) {
        if let Mention::Unknown(form) = mention {
            assert_eq!(form.raw(), raw);
        }
    }
    assert!(!post.channel.is_known());
    assert!(!post.decision.as_ref().unwrap().is_known());
    assert!(!post.is_known());
    assert_eq!(
        serde_json::to_value(&post).unwrap(),
        body,
        "a later form re-encodes as written"
    );

    for by in [
        json!({"kind": "oracle", "votes": 3}),
        json!({"kind": "router", "name": 5}),
        json!({"kind": "router"}),
        json!({"kind": "router", "name": "r", "model": 7}),
        json!({"kind": "stopgap"}),
        json!({"name": "r"}),
        json!("router"),
    ] {
        let read: RouteBy = serde_json::from_value(by.clone()).unwrap();
        assert!(
            matches!(&read, RouteBy::Unknown(form) if form.raw() == &by),
            "{by}"
        );
        assert!(!read.is_known());
        assert_eq!(serde_json::to_value(&read).unwrap(), by);
    }
    let storm: RouteBy =
        serde_json::from_value(json!({"kind": "stopgap", "cause": "storm"})).unwrap();
    assert_eq!(
        storm,
        RouteBy::Stopgap {
            cause: "storm".into()
        }
    );
    assert!(
        !storm.is_known(),
        "an unknown cause is an open tag, not known"
    );
}

#[test]
fn later_forms_are_not_known() {
    for step in [
        "asked",
        "taken",
        "resolved",
        "overruled",
        "expired",
        "withdrawn",
        "waits",
        "redo:t7",
    ] {
        let known = DecisionStep {
            decision_id: "d1".parse().unwrap(),
            step: step.into(),
        };
        assert!(known.is_known(), "{step}");
    }
    for step in ["deferred", "redo:", "redo", "Asked", ""] {
        let later = DecisionStep {
            decision_id: "d1".parse().unwrap(),
            step: step.into(),
        };
        assert!(!later.is_known(), "{step:?}");
    }
    let mut post = lead_post();
    assert!(post.is_known());
    post.decision = None;
    assert!(post.is_known(), "no decision is known");
    post.mentions
        .push(serde_json::from_value(json!({"role": "x"})).unwrap());
    assert!(!post.is_known());
    assert!(RouteBy::Router {
        name: "r".into(),
        model: None
    }
    .is_known());
}

#[test]
fn known_forms_tolerate_additive_fields() {
    let seat: Mention = serde_json::from_value(json!({"seat": "atm/a", "future": 1})).unwrap();
    assert_eq!(seat, Mention::Seat("atm/a".parse().unwrap()));
    let group: Mention =
        serde_json::from_value(json!({"group": {"project": "atm", "name": "b", "future": true}}))
            .unwrap();
    assert_eq!(
        group,
        Mention::Group {
            project: "atm".parse().unwrap(),
            name: "b".into()
        }
    );
    let router: RouteBy = serde_json::from_value(
        json!({"kind": "router", "name": "r", "model": null, "future": [1]}),
    )
    .unwrap();
    assert_eq!(
        router,
        RouteBy::Router {
            name: "r".into(),
            model: None
        }
    );
    let mut body = serde_json::to_value(lead_post()).unwrap();
    body["future"] = json!({"nested": true});
    body["decision"]["future"] = json!(1);
    assert_eq!(
        serde_json::from_value::<PostContent>(body).unwrap(),
        lead_post()
    );
    let mut seen =
        json!({"channel": "atm", "reader": "owner", "through": "m1", "session_id": null});
    seen["future"] = json!(1);
    assert!(serde_json::from_value::<ChannelSeen>(seen).is_ok());
}
