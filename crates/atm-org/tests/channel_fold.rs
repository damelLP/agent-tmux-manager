#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! The channel fold: what each fact indexes, what it refuses, and how ids
//! resolve. Events are hand-written; no daemon, clock or harness.
#[path = "support/channel.rs"]
mod channel_support;
use atm_core::SessionId;
use atm_org::channel_query::*;
use atm_org::*;
use channel_support::*;
use serde_json::json;

#[test]
fn channel_kinds_decode_and_unknown_kinds_still_skip() {
    let bodies = [
        (
            "channel.posted",
            serde_json::to_value(note("atm", "owner")).unwrap(),
        ),
        (
            "channel.routed",
            json!({"post":"m5", "wake":[], "by":{"kind":"stopgap","cause":"disabled"}, "reason":null}),
        ),
        (
            "channel.seen",
            json!({"channel":"atm", "reader":"owner", "through":"m5", "session_id":null}),
        ),
    ];
    for (kind, body) in bodies {
        let event = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(event.kind(), kind);
        assert_eq!(event.to_body(), body);
    }
    assert!(OrgEvent::from_parts("channel.future", &json!([1]))
        .unwrap()
        .is_none());
    let mut bad = serde_json::to_value(note("atm", "owner")).unwrap();
    bad["route"] = json!("yes");
    assert!(OrgEvent::from_parts("channel.posted", &bad).is_err());
}

#[test]
fn post_mutation_facts_round_trip_without_moving_a_thread() {
    let mut org = Org::new();
    let root = org.post(note("atm", "owner"));
    let reply = org.post(PostContent {
        thread: Some(root),
        ..note("atm", "atm/lead")
    });
    let original = post(&org.state, root).unwrap().clone();
    let mut stats = ReplayStats::default();
    for (kind, body) in [
        (
            "channel.post_edited",
            json!({"post":root,"actor":"owner","body":"new text"}),
        ),
        ("channel.post_removed", json!({"post":root,"actor":"owner"})),
    ] {
        let fact = OrgEvent::from_parts(kind, &body).unwrap().unwrap();
        assert_eq!(fact.kind(), kind);
        assert_eq!(fact.to_body(), body);
        org.seq += 1;
        stats
            .apply_parts(&mut org.state, org.seq, at(org.seq), kind, &body)
            .unwrap();
    }
    assert_eq!((stats.applied, stats.skipped_unknown), (2, 0));
    assert_eq!(post(&org.state, root), Some(&original));
    assert_eq!(thread_root(&org.state, reply), Some(root));
    let history = channel_history(
        &org.state,
        &ChannelId::Project("atm".parse().unwrap()),
        &HistoryQuery {
            limit: 10,
            thread: Some(reply),
            ..HistoryQuery::default()
        },
    )
    .unwrap();
    assert_eq!(
        history.rows.iter().map(|r| r.post.id()).collect::<Vec<_>>(),
        [root, reply]
    );
    assert_eq!(history.rows[0].replies, 1);
}

#[test]
fn reaction_facts_roundtrip_as_absolute_membership() {
    let mut org = Org::new();
    let id = org.post(note("atm", "owner"));
    let mut stats = ReplayStats::default();
    for (active, want) in [(true, 1), (true, 1), (false, 0)] {
        let body = json!({"post":id,"actor":"owner","emoji":"👀","active":active});
        let event = OrgEvent::from_parts("channel.reacted", &body)
            .unwrap()
            .unwrap();
        assert_eq!(event.kind(), "channel.reacted");
        assert_eq!(event.to_body(), body);
        org.seq += 1;
        stats
            .apply_parts(&mut org.state, org.seq, at(org.seq), event.kind(), &body)
            .unwrap();
        assert_eq!(org.state.post_reactions(id).map_or(0, |p| p.len()), want);
    }
    assert_eq!((stats.applied, stats.skipped_unknown), (3, 0));
}

#[test]
fn later_forms_fold_without_error() {
    let mut org = Org::new();
    let decision = org.decision();
    let mut stats = ReplayStats::default();
    let mut body = serde_json::to_value(note("x+y+z", "atm/lead")).unwrap();
    body["mentions"] = json!([{"role": "builders"}, "everyone", {"seat": "Bad"}]);
    body["decision"] = json!({"decision_id": decision.to_string(), "step": "deferred"});
    body["route"] = json!(true);
    let later = MessageId::new(org.seq + 1);
    let lines = [
        ("channel.posted", body),
        (
            "channel.routed",
            json!({"post": later.to_string(), "wake": [], "by": {"kind": "oracle", "votes": 3}, "reason": null}),
        ),
        (
            "channel.seen",
            json!({"channel": "x+y+z", "reader": "owner", "through": later.to_string(), "session_id": null}),
        ),
    ];
    for (kind, body) in lines {
        org.seq += 1;
        stats
            .apply_parts(&mut org.state, org.seq, at(org.seq), kind, &body)
            .unwrap();
    }
    assert_eq!(stats.applied, 3);
    assert_eq!(stats.skipped_unknown, 0);
    let folded = post(&org.state, later).unwrap();
    assert!(!folded.content.is_known());
    assert_eq!(folded.routing(), Routing::Unknown);
    assert_eq!(decision_post(&org.state, decision, "deferred"), Some(later));
    // An unknown stopgap cause is an open tag: it folds as a stopgap answer.
    let pending = org.post(PostContent {
        route: true,
        ..note("atm", "atm/lead")
    });
    org.apply(routed(pending, &[], "storm"));
    assert_eq!(
        post(&org.state, pending).unwrap().routing(),
        Routing::Stopgap
    );
}

#[test]
fn wake_row_fills_wakes_and_leaves_unwoken() {
    let mut org = Org::new();
    let lead_post = org.post(waking(
        note("atm", "atm/lead"),
        &["atm/build-2", "atm/build-4"],
    ));
    assert_eq!(
        due(&org.state),
        [
            (lead_post, "atm/build-2".into()),
            (lead_post, "atm/build-4".into())
        ]
    );
    let rows = missing_wakes(&org.state);
    let first = rows.first().unwrap().clone();
    assert_eq!(
        first,
        MessageContent {
            from: addr("atm/lead"),
            to: addr("atm/build-2"),
            kind: MessageKind::Note,
            body: "hold W2".into(),
            attachments: vec!["plan.md".into()],
            task: Some("t9".parse().unwrap()),
            delivery_notice: None,
            post: Some(lead_post),
        },
        "a wake copies its post"
    );
    let row = org.apply(OrgEvent::MessageSent(first));
    assert_eq!(
        wake_for(&org.state, lead_post, &addr("atm/build-2")),
        Some(row)
    );
    assert_eq!(wake_for(&org.state, lead_post, &addr("atm/build-4")), None);
    assert_eq!(due(&org.state), [(lead_post, "atm/build-4".into())]);
    let message = org.state.message(&row).unwrap();
    assert_eq!(
        message.status,
        DeliveryStatus::Queued,
        "a wake is a message"
    );
}

#[test]
fn second_wake_row_for_a_pair_folds_and_the_first_stays_indexed() {
    let mut org = Org::new();
    let lead_post = org.post(waking(note("atm", "atm/lead"), &["atm/build-2"]));
    let row = missing_wakes(&org.state).pop().unwrap();
    let first = org.apply(OrgEvent::MessageSent(row.clone()));
    let second = org.apply(OrgEvent::MessageSent(row));
    assert_eq!(
        wake_for(&org.state, lead_post, &addr("atm/build-2")),
        Some(first)
    );
    assert!(org.state.message(&second).is_some());
    assert_eq!(resolve_post(&org.state, second), Some(lead_post));
    assert!(due(&org.state).is_empty());
}

#[test]
fn routed_adds_targets_and_clears_unrouted() {
    let mut org = Org::new();
    let question = org.post(PostContent {
        route: true,
        ..note("atm", "owner")
    });
    assert_eq!(
        pending_routes(&org.state).collect::<Vec<_>>(),
        [(question, at(question.seq()))]
    );
    assert_eq!(
        post(&org.state, question).unwrap().routing(),
        Routing::Pending
    );
    assert!(due(&org.state).is_empty());
    org.apply(routed(question, &["atm/lead"], "disabled"));
    assert_eq!(pending_routes(&org.state).count(), 0);
    assert_eq!(due(&org.state), [(question, "atm/lead".into())]);
    let folded = post(&org.state, question).unwrap();
    assert_eq!(folded.routing(), Routing::Stopgap);
    assert_eq!(folded.woke().collect::<Vec<_>>(), [&addr("atm/lead")]);
    let [row] = org.wake_all()[..] else {
        panic!("one wake")
    };
    assert_eq!(wake_for(&org.state, question, &addr("atm/lead")), Some(row));
    // An empty answer is final: nobody is woken and nothing waits.
    let quiet = org.post(PostContent {
        route: true,
        ..note("atm", "owner")
    });
    org.apply(routed(quiet, &[], "declined"));
    assert_eq!(pending_routes(&org.state).count(), 0);
    assert!(due(&org.state).is_empty());
}

#[test]
fn a_second_routed_changes_nothing() {
    let mut org = Org::new();
    let question = org.post(PostContent {
        route: true,
        ..note("atm", "owner")
    });
    org.apply(routed(question, &["atm/lead"], "timeout"));
    let before = post(&org.state, question).unwrap().clone();
    let due_before = due(&org.state);
    org.apply(OrgEvent::ChannelRouted(PostRouted {
        post: question,
        wake: vec![addr("atm/build-2")],
        by: RouteBy::Router {
            name: "late".into(),
            model: None,
        },
        reason: Some("too late".into()),
    }));
    assert_eq!(post(&org.state, question).unwrap(), &before);
    assert_eq!(due(&org.state), due_before);
}

#[test]
fn unwoken_never_holds_author_system_or_a_filled_pair() {
    let mut org = Org::new();
    let own = org.post(waking(
        PostContent {
            route: true,
            ..note("atm", "atm/lead")
        },
        &["atm/lead", "system", "atm/build-2"],
    ));
    assert_eq!(due(&org.state), [(own, "atm/build-2".into())]);
    org.wake_all();
    // A routing answer naming the author, system or a seat already woken
    // adds nothing to the repair list.
    org.apply(routed(
        own,
        &["atm/lead", "system", "atm/build-2"],
        "disabled",
    ));
    assert!(due(&org.state).is_empty());
}

#[test]
fn first_client_key_and_decision_step_win() {
    let mut org = Org::new();
    let decision = org.decision();
    let keyed = |author: &str| PostContent {
        client_key: Some("k-1".into()),
        decision: Some(DecisionStep {
            decision_id: decision,
            step: "asked".into(),
        }),
        ..note("atm", author)
    };
    let first = org.post(keyed("owner"));
    let repeat = org.post(keyed("owner"));
    let other = org.post(keyed("atm/lead"));
    assert_ne!(first, repeat);
    assert_eq!(
        client_key_post(&org.state, &Address::Owner, "k-1"),
        Some(first)
    );
    assert_eq!(
        client_key_post(&org.state, &addr("atm/lead"), "k-1"),
        Some(other)
    );
    assert_eq!(client_key_post(&org.state, &Address::Owner, "k-2"), None);
    assert_eq!(decision_post(&org.state, decision, "asked"), Some(first));
    assert_eq!(decision_post(&org.state, decision, "resolved"), None);
}

#[test]
fn posts_and_messages_share_one_id_space() {
    let mut org = Org::new();
    let message = org.send("owner", "atm/lead", None);
    let lead_post = org.post(note("atm", "atm/lead"));
    for (seq, event) in [
        (message.seq(), OrgEvent::ChannelPosted(note("atm", "owner"))),
        (
            lead_post.seq(),
            OrgEvent::MessageSent(MessageContent {
                from: Address::Owner,
                to: addr("atm/lead"),
                kind: MessageKind::Note,
                body: "x".into(),
                attachments: vec![],
                task: None,
                delivery_notice: None,
                post: None,
            }),
        ),
        (
            lead_post.seq(),
            OrgEvent::ChannelPosted(note("atm", "owner")),
        ),
    ] {
        let before = org.state.clone();
        assert_eq!(
            org.state.apply(seq, at(seq), &event),
            Err(ApplyError::DuplicateMessage(MessageId::new(seq)))
        );
        assert_eq!(org.state, before);
    }
}

#[test]
fn channel_references_are_checked_atomically() {
    let mut org = Org::new();
    let decision = org.decision();
    let legacy = org.send("owner", "atm/lead", None);
    let real = org.post(note("atm", "atm/lead"));
    let ghost = "atm/ghost";
    let unknown_agent = || ApplyError::UnknownAgent(ghost.parse().unwrap());
    let unknown_project = || ApplyError::UnknownProject("nope".parse().unwrap());
    let missing = MessageId::new(999);
    let posted = |edit: &dyn Fn(&mut PostContent)| {
        let mut content = note("atm", "atm/lead");
        edit(&mut content);
        OrgEvent::ChannelPosted(content)
    };
    let cases: Vec<(&str, OrgEvent, ApplyError)> = vec![
        (
            "project channel",
            posted(&|p| p.channel = channel("nope")),
            unknown_project(),
        ),
        (
            "direct channel",
            posted(&|p| p.channel = ChannelId::direct(Address::Owner, addr(ghost)).unwrap()),
            unknown_agent(),
        ),
        (
            "author",
            posted(&|p| p.author = addr(ghost)),
            unknown_agent(),
        ),
        (
            "thread root",
            posted(&|p| p.thread = Some(missing)),
            ApplyError::UnknownPost(missing),
        ),
        (
            "seat mention",
            posted(&|p| p.mentions = vec![Mention::Seat(ghost.parse().unwrap())]),
            unknown_agent(),
        ),
        (
            "group mention",
            posted(&|p| {
                p.mentions = vec![Mention::Group {
                    project: "nope".parse().unwrap(),
                    name: "b".into(),
                }]
            }),
            unknown_project(),
        ),
        (
            "wake",
            posted(&|p| p.wake = vec![addr(ghost)]),
            unknown_agent(),
        ),
        (
            "decision",
            posted(&|p| {
                p.decision = Some(DecisionStep {
                    decision_id: DecisionId::new(999),
                    step: "asked".into(),
                })
            }),
            ApplyError::UnknownDecision(DecisionId::new(999)),
        ),
        (
            "routed post",
            routed(missing, &[], "disabled"),
            ApplyError::UnknownPost(missing),
        ),
        (
            "routed legacy message",
            routed(legacy, &[], "disabled"),
            ApplyError::UnknownPost(legacy),
        ),
        (
            "routed wake",
            routed(real, &[ghost], "disabled"),
            unknown_agent(),
        ),
        (
            "seen channel",
            OrgEvent::ChannelSeen(ChannelSeen {
                channel: channel("nope"),
                reader: Address::Owner,
                through: real,
                session_id: None,
            }),
            unknown_project(),
        ),
        (
            "seen reader",
            OrgEvent::ChannelSeen(ChannelSeen {
                channel: channel("atm"),
                reader: addr(ghost),
                through: real,
                session_id: None,
            }),
            unknown_agent(),
        ),
        (
            "seen through",
            OrgEvent::ChannelSeen(ChannelSeen {
                channel: channel("atm"),
                reader: Address::Owner,
                through: missing,
                session_id: None,
            }),
            ApplyError::UnknownPost(missing),
        ),
    ];
    for (case, event, error) in cases {
        assert_eq!(org.try_apply(event), Err(error), "{case}");
    }
    for (case, post) in [("unknown post", missing), ("legacy message", legacy)] {
        let wake = OrgEvent::MessageSent(MessageContent {
            from: addr("atm/lead"),
            to: addr("atm/build-2"),
            kind: MessageKind::Note,
            body: "x".into(),
            attachments: vec![],
            task: None,
            delivery_notice: None,
            post: Some(post),
        });
        assert_eq!(
            org.try_apply(wake),
            Err(ApplyError::UnknownPost(post)),
            "{case}"
        );
    }
    let empty_session = posted(&|p| p.session_id = Some(SessionId::new("")));
    assert!(matches!(
        org.try_apply(empty_session),
        Err(ApplyError::InvalidId(_))
    ));
    // References that exist: a legacy message and a decision may be named.
    org.try_apply(posted(&|p| {
        p.thread = Some(legacy);
        p.decision = Some(DecisionStep {
            decision_id: decision,
            step: "asked".into(),
        });
    }))
    .unwrap();
    // Any text that is not a known channel form folds; commands refuse it.
    org.try_apply(posted(&|p| p.channel = channel("later:form")))
        .unwrap();
}

#[test]
fn ids_resolve_both_ways() {
    let mut org = Org::new();
    let legacy = org.send("owner", "atm/lead", None);
    let to_self = org.send("owner", "owner", None);
    let root = org.post(waking(note("atm", "owner"), &["atm/build-2"]));
    let [root_wake] = org.wake_all()[..] else {
        panic!("one wake")
    };
    let reply = org.post(waking(
        PostContent {
            thread: Some(root),
            ..note("atm", "atm/build-2")
        },
        &["owner"],
    ));
    let [reply_wake] = org.wake_all()[..] else {
        panic!("one wake")
    };
    let legacy_reply = org.post(PostContent {
        thread: Some(legacy),
        ..note("atm/lead+owner", "atm/lead")
    });

    for (id, post) in [
        (root, Some(root)),
        (root_wake, Some(root)),
        (reply, Some(reply)),
        (reply_wake, Some(reply)),
        (legacy, Some(legacy)),
        (to_self, None),
        (MessageId::new(999), None),
    ] {
        assert_eq!(resolve_post(&org.state, id), post, "{id}");
    }
    for (id, root_of) in [
        (root, Some(root)),
        (root_wake, Some(root)),
        (reply, Some(root)),
        (reply_wake, Some(root)),
        (legacy, Some(legacy)),
        (legacy_reply, Some(legacy)),
        (to_self, None),
    ] {
        assert_eq!(thread_root(&org.state, id), root_of, "{id}");
    }
    assert_eq!(
        wake_for(&org.state, root, &addr("atm/build-2")),
        Some(root_wake)
    );
    assert_eq!(
        wake_for(&org.state, reply, &Address::Owner),
        Some(reply_wake)
    );
    assert_eq!(wake_for(&org.state, root, &addr("atm/lead")), None);
    assert_eq!(
        wake_for(&org.state, legacy, &addr("atm/lead")),
        Some(legacy)
    );
    assert_eq!(wake_for(&org.state, legacy, &Address::Owner), None);
    assert_eq!(wake_for(&org.state, to_self, &Address::Owner), None);
    assert_eq!(wake_for(&org.state, root_wake, &addr("atm/build-2")), None);
}

// Break caught: a hand-built direct pair out of order is folded under a key
// that replay never produces (its text decodes as `ChannelId::Other`).
#[test]
fn an_out_of_order_direct_channel_is_refused() {
    let mut org = Org::new();
    let first = org.post(note("atm", "owner"));
    for pair in [
        ChannelId::Direct(Address::Owner, addr("atm/lead")),
        ChannelId::Direct(Address::System, Address::Owner),
        ChannelId::Direct(Address::Owner, Address::Owner),
    ] {
        let text = pair.to_string();
        let posted = OrgEvent::ChannelPosted(PostContent {
            channel: pair.clone(),
            ..note("atm", "owner")
        });
        let seen = OrgEvent::ChannelSeen(ChannelSeen {
            channel: pair,
            reader: Address::Owner,
            through: first,
            session_id: None,
        });
        for event in [posted, seen] {
            assert!(
                matches!(org.try_apply(event), Err(ApplyError::InvalidId(e)) if e.value == text),
                "{text}"
            );
        }
    }
    let canonical = ChannelId::direct(Address::Owner, addr("atm/lead")).unwrap();
    org.post(PostContent {
        channel: canonical,
        ..note("atm", "owner")
    });
}
