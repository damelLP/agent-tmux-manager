#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! The post policy (design 6.1, 9.2): refusals, placement, mentions and wakes,
//! the route flag, internal posts, the author's session and repeats. Each table
//! row is named; every case decides against hand-written facts.
#[path = "support/channel.rs"]
mod channel_support;
#[path = "support/policy.rs"]
mod policy_support;
use atm_core::SessionId;
use atm_org::channel_policy::*;
use atm_org::channel_query::*;
use atm_org::*;
use channel_support::*;
use policy_support::*;

/// `InvalidValue` rows compare their field only; the reason is prose.
fn same(got: &Refusal, want: &Refusal) -> bool {
    match (got, want) {
        (Refusal::InvalidValue { field: a, .. }, Refusal::InvalidValue { field: b, .. }) => a == b,
        _ => got == want,
    }
}

// Break: the owner's first post into a listed lead DM with no posts is refused, or lands
// somewhere other than that DM.
#[test]
fn owner_posts_by_id_into_an_empty_lead_dm() {
    let org = org();
    let lead = channel("web/lead+owner");
    assert!(listed(&org.state, &lead));
    let events = org
        .decide(&posting(request(
            PostTarget::Channel(lead.clone()),
            "owner",
            "hi",
        )))
        .unwrap();
    assert_eq!(the_post(&events).channel, lead);
}

#[test]
fn post_refusals() {
    let mut org = org();
    let agent_dm = org.send("atm/build-2", "web/lead", None);
    org.send("atm/lead", "web/lead", None);
    let self_row = org.send("atm/build-2", "atm/build-2", None);
    let root = org.post(note("atm", "atm/lead"));
    let decision = org.decision();
    let empty = groups(&[("atm", "empty", &["atm/build-6"])]);
    let dm = channel("atm/build-2+web/lead");
    let leads = channel("atm/lead+web/lead");
    let value = |field| Refusal::InvalidValue {
        field,
        reason: String::new(),
    };
    let with = |mut r: PostRequest, f: &dyn Fn(&mut PostRequest)| {
        f(&mut r);
        r
    };
    let rows: Vec<(&str, PostRequest, Refusal)> = vec![
        (
            "unregistered project",
            request(in_channel("nope"), "owner", "hi"),
            refused(ChannelRefusal::UnknownChannel(channel("nope"))),
        ),
        (
            "later-form channel",
            request(in_channel("x+y+z"), "owner", "hi"),
            refused(ChannelRefusal::UnknownChannel(channel("x+y+z"))),
        ),
        (
            "direct id built out of order",
            request(
                PostTarget::Channel(ChannelId::Direct(Address::Owner, addr("atm/lead"))),
                "owner",
                "hi",
            ),
            refused(ChannelRefusal::UnknownChannel(ChannelId::Direct(
                Address::Owner,
                addr("atm/lead"),
            ))),
        ),
        (
            "project with no lead and no posts is not listed",
            request(in_channel("solo"), "owner", "hi"),
            refused(ChannelRefusal::UnknownChannel(channel("solo"))),
        ),
        (
            "the owner's DM with a seat that is not a lead has no posts, so is not listed",
            request(in_channel("atm/build-2+owner"), "owner", "hi"),
            refused(ChannelRefusal::UnknownChannel(channel("atm/build-2+owner"))),
        ),
        (
            "seat of another project",
            request(in_channel("atm"), "web/lead", "hi"),
            refused(ChannelRefusal::NotMember(channel("atm"))),
        ),
        (
            "down seat",
            request(in_channel("atm"), "atm/build-6", "hi"),
            refused(ChannelRefusal::NotMember(channel("atm"))),
        ),
        (
            "owner into an agent-to-agent DM",
            request(PostTarget::Channel(dm.clone()), "owner", "hi"),
            refused(ChannelRefusal::NotMember(dm.clone())),
        ),
        (
            "a seat that is not a party of the DM",
            request(PostTarget::Channel(dm.clone()), "atm/lead", "hi"),
            refused(ChannelRefusal::NotMember(dm.clone())),
        ),
        (
            "system into a DM it is no party of",
            request(PostTarget::Channel(dm.clone()), "system", "note"),
            refused(ChannelRefusal::NotMember(dm.clone())),
        ),
        (
            "direct to an unknown seat",
            request(direct("atm/ghost"), "owner", "hi"),
            Refusal::Invariant(ApplyError::UnknownAgent(agent_id("atm/ghost"))),
        ),
        (
            "direct to system",
            request(direct("system"), "atm/lead", "hi"),
            Refusal::InvalidRecipient(Address::System),
        ),
        (
            "direct to self",
            request(direct("atm/lead"), "atm/lead", "hi"),
            Refusal::InvalidRecipient(addr("atm/lead")),
        ),
        (
            "reply to an unknown id",
            request(PostTarget::Reply(MessageId::new(999)), "owner", "hi"),
            Refusal::Invariant(ApplyError::UnknownPost(MessageId::new(999))),
        ),
        (
            "reply to a self-addressed legacy row",
            request(PostTarget::Reply(self_row), "owner", "hi"),
            Refusal::Invariant(ApplyError::UnknownPost(self_row)),
        ),
        (
            "reply from outside the root's channel",
            request(PostTarget::Reply(root), "web/lead", "hi"),
            refused(ChannelRefusal::NotMember(channel("atm"))),
        ),
        (
            "unknown strict tag",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.tags = vec!["nobody".into()]
            }),
            refused(ChannelRefusal::UnknownTag {
                tag: "nobody".into(),
                reason: Unresolvable::Unknown,
            }),
        ),
        (
            "strict tag written with its own @",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.tags = vec!["@lead".into()]
            }),
            refused(ChannelRefusal::UnknownTag {
                tag: "@lead".into(),
                reason: Unresolvable::Malformed,
            }),
        ),
        (
            "strict tag with trailing text",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.tags = vec!["lead now".into()]
            }),
            refused(ChannelRefusal::UnknownTag {
                tag: "lead now".into(),
                reason: Unresolvable::Malformed,
            }),
        ),
        (
            "strict tag of a group with no live member",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.tags = vec!["empty".into()];
                r.groups = empty.clone();
            }),
            refused(ChannelRefusal::UnknownTag {
                tag: "empty".into(),
                reason: Unresolvable::EmptyGroup,
            }),
        ),
        (
            "strict short tag both DM parties share",
            with(
                request(PostTarget::Channel(leads.clone()), "atm/lead", "hi"),
                &|r| r.tags = vec!["lead".into()],
            ),
            refused(ChannelRefusal::UnknownTag {
                tag: "lead".into(),
                reason: Unresolvable::Ambiguous,
            }),
        ),
        (
            "later decision step",
            with(request(in_channel("atm"), "atm/lead", "q"), &|r| {
                r.decision = Some(DecisionStep {
                    decision_id: decision,
                    step: "deferred".into(),
                })
            }),
            value("decision step"),
        ),
        (
            "unknown decision",
            with(request(in_channel("atm"), "atm/lead", "q"), &|r| {
                r.decision = Some(DecisionStep {
                    decision_id: DecisionId::new(999),
                    step: "asked".into(),
                })
            }),
            Refusal::Invariant(ApplyError::UnknownDecision(DecisionId::new(999))),
        ),
        (
            "unknown author",
            request(in_channel("atm"), "atm/ghost", "hi"),
            Refusal::Invariant(ApplyError::UnknownAgent(agent_id("atm/ghost"))),
        ),
        (
            "empty client key",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.client_key = Some(String::new())
            }),
            value("client_key"),
        ),
        (
            "client key over 64 bytes",
            with(request(in_channel("atm"), "owner", "hi"), &|r| {
                r.client_key = Some("k".repeat(65))
            }),
            value("client_key"),
        ),
    ];
    let _ = agent_dm;
    for (name, request, want) in rows {
        let got = org.decide(&posting(request)).expect_err(name);
        assert!(same(&got, &want), "{name}: got {got:?}, want {want:?}");
    }
}

#[test]
fn sharing_a_top_level_post_is_refused_by_the_pure_policy() {
    let org = Org::new();
    for target in [
        PostTarget::Channel(channel("atm")),
        PostTarget::Direct(addr("atm/lead")),
    ] {
        let mut p = request(target, "owner", "top level");
        p.also_in_chat = true;
        assert!(matches!(
            org.state.decide(&posting(p), at(org.seq + 1)),
            Err(Refusal::InvalidValue {
                field: "also_in_chat",
                ..
            })
        ));
    }
}

#[test]
fn post_placement() {
    let mut org = org();
    let legacy = org.send("atm/build-2", "owner", None);
    org.send("atm/build-2", "web/lead", None);
    let root = org.posted(request(in_channel("atm"), "atm/lead", "@build-2 plan"));
    let wake = org.wake_all()[0];
    let reply = org.posted(request(PostTarget::Reply(root), "atm/build-2", "ok"));
    // (name, request, channel, thread, wake)
    type Row<'a> = (
        &'a str,
        PostRequest,
        &'a str,
        Option<MessageId>,
        &'a [&'a str],
    );
    let rows: Vec<Row> = vec![
        (
            "project top level",
            request(in_channel("atm"), "owner", "hi"),
            "atm",
            None,
            &[],
        ),
        (
            "direct is the ordered pair and wakes its party",
            request(direct("owner"), "atm/build-2", "hi"),
            "atm/build-2+owner",
            None,
            &["owner"],
        ),
        (
            "direct to a down seat is placed and wakes it",
            request(direct("atm/build-6"), "owner", "back?"),
            "atm/build-6+owner",
            None,
            &["atm/build-6"],
        ),
        (
            "a 64-byte client key is accepted",
            PostRequest {
                client_key: Some("k".repeat(64)),
                ..request(in_channel("atm"), "owner", "hi")
            },
            "atm",
            None,
            &[],
        ),
        (
            "direct from the other side",
            request(direct("atm/build-2"), "owner", "hi"),
            "atm/build-2+owner",
            None,
            &["atm/build-2"],
        ),
        (
            "a listed direct channel by its id",
            request(in_channel("atm/build-2+web/lead"), "web/lead", "hi"),
            "atm/build-2+web/lead",
            None,
            &[],
        ),
        (
            "reply to a post",
            request(PostTarget::Reply(root), "owner", "hi"),
            "atm",
            Some(root),
            &[],
        ),
        (
            "reply to a reply goes to its root",
            request(PostTarget::Reply(reply), "owner", "hi"),
            "atm",
            Some(root),
            &[],
        ),
        (
            "reply to a wake goes to its post",
            request(PostTarget::Reply(wake), "owner", "hi"),
            "atm",
            Some(root),
            &[],
        ),
        (
            "reply to a legacy message",
            request(PostTarget::Reply(legacy), "owner", "hi"),
            "atm/build-2+owner",
            Some(legacy),
            &[],
        ),
        (
            "system in a project channel",
            request(in_channel("atm"), "system", "note"),
            "atm",
            None,
            &[],
        ),
    ];
    for (name, request, want_channel, want_thread, want_wake) in rows {
        let events = org
            .decide(&posting(request))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let content = the_post(&events);
        assert_eq!(content.channel, channel(want_channel), "{name}");
        assert_eq!(content.thread, want_thread, "{name}");
        assert_eq!(content.wake, addrs(want_wake), "{name}");
    }
}

#[test]
fn post_mentions_and_wakes() {
    let mut org = org();
    org.send("atm/lead", "web/lead", None);
    org.send("atm/build-2", "web/lead", None);
    let directory = groups(&[
        ("atm", "builders", &["atm/build-2", "atm/build-6"]),
        ("atm", "empty", &["atm/build-6"]),
        ("atm", "lead", &["atm/build-4"]),
        ("web", "devs", &["web/build-1"]),
    ]);
    type Row<'a> = (
        &'a str,
        PostTarget,
        &'a str,
        &'a str,
        &'a [&'a str],
        Vec<Mention>,
        &'a [&'a str],
        Vec<(&'a str, Unresolvable)>,
    );
    let rows: Vec<Row> = vec![
        (
            "short seat",
            in_channel("atm"),
            "owner",
            "@build-2 hold",
            &[],
            vec![seat("atm/build-2")],
            &["atm/build-2"],
            vec![],
        ),
        (
            "owner",
            in_channel("atm"),
            "atm/lead",
            "@owner ok?",
            &[],
            vec![Mention::Owner],
            &["owner"],
            vec![],
        ),
        (
            "long-form seat outside the channel",
            in_channel("atm"),
            "atm/lead",
            "ask @web/build-1",
            &[],
            vec![seat("web/build-1")],
            &["web/build-1"],
            vec![],
        ),
        (
            "short group skips its down member",
            in_channel("atm"),
            "atm/lead",
            "@builders go",
            &[],
            vec![group("atm", "builders")],
            &["atm/build-2"],
            vec![],
        ),
        (
            "long-form group from a DM",
            in_channel("atm/lead+web/lead"),
            "atm/lead",
            "@web/devs",
            &[],
            vec![group("web", "devs")],
            &["web/build-1"],
            vec![],
        ),
        (
            "a down seat is still woken",
            in_channel("atm"),
            "owner",
            "@build-6 back?",
            &[],
            vec![seat("atm/build-6")],
            &["atm/build-6"],
            vec![],
        ),
        (
            "an inline empty group stays text",
            in_channel("atm"),
            "owner",
            "@empty hi",
            &[],
            vec![],
            &[],
            vec![("@empty", Unresolvable::EmptyGroup)],
        ),
        (
            "the author's own mention is kept, not woken",
            in_channel("atm"),
            "atm/build-2",
            "@build-2 notes",
            &[],
            vec![seat("atm/build-2")],
            &[],
            vec![],
        ),
        (
            "order and dedupe across inline and strict",
            in_channel("atm"),
            "owner",
            "@build-4 @build-2 @build-4 @owner",
            &["build-2", "web/build-1"],
            vec![
                seat("atm/build-4"),
                seat("atm/build-2"),
                Mention::Owner,
                seat("web/build-1"),
            ],
            &["atm/build-4", "atm/build-2", "web/build-1"],
            vec![],
        ),
        (
            "a group and its member are woken once",
            in_channel("atm"),
            "atm/lead",
            "@builders @build-2",
            &[],
            vec![group("atm", "builders"), seat("atm/build-2")],
            &["atm/build-2"],
            vec![],
        ),
        (
            "a short name both DM parties share resolves to neither",
            in_channel("atm/lead+web/lead"),
            "atm/lead",
            "@lead hi",
            &[],
            vec![],
            &[],
            vec![("@lead", Unresolvable::Ambiguous)],
        ),
        (
            "the short name of a DM party",
            in_channel("atm/build-2+web/lead"),
            "atm/build-2",
            "@lead see",
            &[],
            vec![seat("web/lead")],
            &["web/lead"],
            vec![],
        ),
        (
            "a short name that is no DM party",
            in_channel("atm/build-2+web/lead"),
            "atm/build-2",
            "@build-4",
            &[],
            vec![],
            &[],
            vec![("@build-4", Unresolvable::Unknown)],
        ),
        (
            "a seat beats a group of the same name",
            in_channel("atm"),
            "owner",
            "@lead go",
            &[],
            vec![seat("atm/lead")],
            &["atm/lead"],
            vec![],
        ),
        (
            "a code span is not a mention",
            in_channel("atm"),
            "owner",
            "`@build-2` then @build-4",
            &[],
            vec![seat("atm/build-4")],
            &["atm/build-4"],
            vec![],
        ),
        (
            "system is not a tag",
            in_channel("atm"),
            "owner",
            "@system",
            &[],
            vec![],
            &[],
            vec![("@system", Unresolvable::Unknown)],
        ),
        (
            "a strict long-form group from another project",
            in_channel("atm"),
            "atm/lead",
            "go",
            &["web/devs"],
            vec![group("web", "devs")],
            &["web/build-1"],
            vec![],
        ),
        (
            "a direct target is woken first",
            direct("atm/build-4"),
            "atm/lead",
            "@atm/build-2",
            &[],
            vec![seat("atm/build-2")],
            &["atm/build-4", "atm/build-2"],
            vec![],
        ),
    ];
    for (name, target, author, body, tags, mentions, wake, unresolved) in rows {
        let mut req = request(target, author, body);
        req.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        req.groups = directory.clone();
        let events = org
            .decide(&posting(req.clone()))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let content = the_post(&events);
        assert_eq!(content.mentions, mentions, "{name}");
        assert_eq!(content.wake, addrs(wake), "{name}");
        let resolution =
            resolve_mentions(&org.state, &content.channel, body, &req.tags, &directory).unwrap();
        let reported: Vec<(&str, Unresolvable)> = resolution
            .unresolved
            .iter()
            .map(|u| (u.text.as_str(), u.reason))
            .collect();
        assert_eq!(reported, unresolved, "{name}");
        assert_eq!(resolution.mentions, mentions, "{name}");
    }
}

#[test]
fn post_route_truth_table() {
    let mut org = org();
    org.register("quiet");
    org.adopt("quiet/lead", Some("s-quiet"));
    org.down("quiet/lead");
    org.send("atm/build-2", "owner", None);
    org.send("system", "atm/build-2", None);
    let root = org.post(note("atm", "atm/lead"));
    let solo = groups(&[("atm", "solo", &["atm/build-2"])]);
    let rows: Vec<(&str, PostRequest, bool)> = vec![
        (
            "untagged seat post",
            request(in_channel("atm"), "atm/build-2", "hello"),
            true,
        ),
        (
            "the owner untagged while a seat is up",
            request(in_channel("atm"), "owner", "status?"),
            true,
        ),
        (
            "the owner untagged while every seat is down",
            request(in_channel("quiet"), "owner", "anyone?"),
            false,
        ),
        (
            "a post that mentions only its author",
            request(in_channel("atm"), "atm/build-2", "@build-2 notes"),
            true,
        ),
        (
            "a tag of the owner only",
            request(in_channel("atm"), "atm/build-2", "@owner done"),
            false,
        ),
        (
            "a direct post",
            request(direct("owner"), "atm/build-2", "hi"),
            false,
        ),
        (
            "a seat untagged in its DM with the owner",
            request(in_channel("atm/build-2+owner"), "atm/build-2", "hi"),
            true,
        ),
        (
            "system untagged",
            request(in_channel("atm"), "system", "note"),
            false,
        ),
        (
            "no_route",
            PostRequest {
                no_route: true,
                ..request(in_channel("atm"), "atm/build-2", "fyi")
            },
            false,
        ),
        (
            "a seat untagged in its DM with system",
            request(in_channel("atm/build-2+system"), "atm/build-2", "ok"),
            false,
        ),
        (
            "an untagged thread reply",
            request(PostTarget::Reply(root), "atm/build-2", "on it"),
            true,
        ),
        (
            "a group whose only live member is the author",
            PostRequest {
                groups: solo.clone(),
                ..request(in_channel("atm"), "atm/build-2", "@solo")
            },
            true,
        ),
        (
            "a tag of a down seat",
            request(in_channel("atm"), "owner", "@build-6"),
            false,
        ),
    ];
    for (name, request, route) in rows {
        let events = org
            .decide(&posting(request))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(the_post(&events).route, route, "{name}");
    }
}

#[test]
fn internal_post_skips_only_the_down_test() {
    let org = org();
    let internal = PostRequest {
        internal: true,
        ..request(in_channel("atm"), "atm/build-6", "question")
    };
    let events = org.decide(&posting(internal.clone())).unwrap();
    assert_eq!(the_post(&events).author, addr("atm/build-6"));
    assert_eq!(
        org.decide(&posting(PostRequest {
            internal: false,
            ..internal
        })),
        Err(refused(ChannelRefusal::NotMember(channel("atm"))))
    );
    assert_eq!(
        org.decide(&posting(PostRequest {
            internal: true,
            ..request(in_channel("atm"), "web/lead", "question")
        })),
        Err(refused(ChannelRefusal::NotMember(channel("atm"))))
    );
}

// Break caught: copied task text adds a wake, or ordinary RPC text stops tagging.
#[test]
fn internal_task_post_uses_only_its_strict_audience() {
    let org = org();
    let body = "Check @atm/build-6 @owner";
    for target in [in_channel("atm"), direct("atm/lead")] {
        let task = PostRequest {
            internal: true,
            no_route: true,
            kind: MessageKind::Brief,
            task: Some("t9".parse().unwrap()),
            tags: vec!["atm/lead".into()],
            ..request(target, "atm/build-2", body)
        };
        let events = org.decide(&posting(task.clone())).unwrap();
        let post = the_post(&events);
        assert_eq!(post.wake, addrs(&["atm/lead"]));
        assert_eq!(post.mentions, vec![seat("atm/lead")]);
        assert_eq!(post.body, body);
        assert!(!post.route);
        let events = org
            .decide(&posting(PostRequest {
                internal: false,
                ..task
            }))
            .unwrap();
        let post = the_post(&events);
        assert_eq!(post.wake.len(), 3);
        assert!(post.wake.contains(&addr("atm/build-6")));
        assert!(post.wake.contains(&Address::Owner));
    }
}

// Break caught: an unaddressed generated question wakes a seat named in the context.
#[test]
fn internal_question_with_no_strict_tags_does_not_wake() {
    let mut org = org();
    let body = "Context mentions @lead and @owner";
    let question = PostRequest {
        internal: true,
        no_route: true,
        kind: MessageKind::Question,
        decision: Some(DecisionStep {
            decision_id: org.decision(),
            step: "asked".into(),
        }),
        ..request(in_channel("atm"), "atm/build-2", body)
    };
    let events = org.decide(&posting(question)).unwrap();
    let post = the_post(&events);
    assert_eq!(post.body, body);
    assert!(post.wake.is_empty(), "context woke {:?}", post.wake);
    assert!(post.mentions.is_empty());
    assert!(!post.route);
}

#[test]
fn post_session_is_the_authors_bound_session() {
    let mut org = org();
    org.adopt("atm/build-8", Some("pending-42"));
    org.adopt("atm/build-9", None);
    let rows = [
        ("a bound seat", "atm/build-2", Some("s-build-2")),
        ("a pending session", "atm/build-8", None),
        ("an unbound seat", "atm/build-9", None),
        ("the owner", "owner", None),
        ("system", "system", None),
    ];
    for (name, author, session) in rows {
        let events = org
            .decide(&posting(request(in_channel("atm"), author, "hi")))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            the_post(&events).session_id,
            session.map(SessionId::new),
            "{name}"
        );
    }
}

#[test]
fn post_fact_carries_the_request() {
    let org = org();
    let req = PostRequest {
        kind: MessageKind::Report,
        attachments: vec!["report.md".into()],
        task: Some("t9".parse().unwrap()),
        client_key: Some("k-1".into()),
        ..request(in_channel("atm"), "atm/build-2", "done @lead")
    };
    let events = org.decide(&posting(req)).unwrap();
    assert_eq!(
        the_post(&events),
        &PostContent {
            channel: channel("atm"),
            author: addr("atm/build-2"),
            session_id: Some(SessionId::new("s-build-2")),
            kind: MessageKind::Report,
            body: "done @lead".into(),
            attachments: vec!["report.md".into()],
            task: Some("t9".parse().unwrap()),
            thread: None,
            mentions: vec![seat("atm/lead")],
            wake: addrs(&["atm/lead"]),
            route: false,
            decision: None,
            client_key: Some("k-1".into()),
            also_in_chat: false,
        }
    );
}

#[test]
fn repeated_client_key_and_decision_step_emit_nothing() {
    let mut org = org();
    let keyed = |author: &str, body: &str, key: &str| PostRequest {
        client_key: Some(key.into()),
        ..request(in_channel("atm"), author, body)
    };
    let first = org.posted(keyed("owner", "hi", "k1"));
    assert_eq!(org.decide(&posting(keyed("owner", "hi", "k1"))), Ok(vec![]));
    assert_eq!(
        org.decide(&posting(keyed("owner", "other", "k1"))),
        Ok(vec![])
    );
    assert_eq!(
        client_key_post(&org.state, &Address::Owner, "k1"),
        Some(first)
    );
    let other = org.posted(keyed("atm/lead", "hi", "k1"));
    assert_ne!(other, first);

    let decision = org.decision();
    let question = PostRequest {
        kind: MessageKind::Question,
        tags: vec!["owner".into()],
        decision: Some(DecisionStep {
            decision_id: decision,
            step: "asked".into(),
        }),
        internal: true,
        ..request(in_channel("atm"), "atm/lead", "W2 order?")
    };
    let asked = org.posted(question.clone());
    assert_eq!(org.decide(&posting(question)), Ok(vec![]));
    assert_eq!(decision_post(&org.state, decision, "asked"), Some(asked));

    // A repeat answers before placement and tags: its group has emptied since.
    let builders = groups(&[("atm", "builders", &["atm/build-2"])]);
    let tagged = PostRequest {
        tags: vec!["builders".into()],
        groups: builders,
        ..keyed("owner", "go", "k2")
    };
    org.posted(tagged.clone());
    org.down("atm/build-2");
    assert_eq!(org.decide(&posting(tagged)), Ok(vec![]));
}

#[test]
fn inline_unresolved_tokens_are_reported() {
    let org = org();
    let directory = groups(&[("atm", "empty", &["atm/build-6"])]);
    let resolution = resolve_mentions(
        &org.state,
        &channel("atm"),
        "@nobody @empty @system @nobody @build-2",
        &[],
        &directory,
    )
    .unwrap();
    assert_eq!(resolution.mentions, vec![seat("atm/build-2")]);
    assert_eq!(resolution.targets, addrs(&["atm/build-2"]));
    let reported: Vec<(&str, Unresolvable)> = resolution
        .unresolved
        .iter()
        .map(|u| (u.text.as_str(), u.reason))
        .collect();
    assert_eq!(
        reported,
        vec![
            ("@nobody", Unresolvable::Unknown),
            ("@empty", Unresolvable::EmptyGroup),
            ("@system", Unresolvable::Unknown),
        ]
    );
}

/// `solo` has no lead, so its channel is not listed; `solo/worker` asks d.
fn solo_question(org: &mut Org) -> (DecisionId, ChannelId) {
    org.adopt("solo/worker", Some("s-solo-worker"));
    let id = org.apply(OrgEvent::DecisionRequested(DecisionRequest {
        requester: addr("solo/worker"),
        kind: DecisionKind::Plan,
        title: "Ship?".into(),
        context: String::new(),
        files: vec![],
        options: vec!["yes".into(), "no".into()],
        recommended: None,
        may_be_undone: true,
        changes_intent: false,
        deadline: None,
    }));
    (
        DecisionId::new(id.seq()),
        ChannelId::direct(addr("solo/worker"), Address::Owner).unwrap(),
    )
}
fn step(decision: DecisionId, step: &str) -> Option<DecisionStep> {
    Some(DecisionStep {
        decision_id: decision,
        step: step.into(),
    })
}

// U8, the lead's Q3 ruling: a question asked before the daemon started wakes
// nobody, so it cannot be a `Direct(owner)` post. Break caught: an internal
// post refused for an unlisted DM, a non-internal one let through, or the
// opening widened to unlisted project channels.
#[test]
fn internal_post_may_open_a_direct_channel() {
    let mut org = org();
    let (decision, dm) = solo_question(&mut org);
    let question = PostRequest {
        kind: MessageKind::Question,
        decision: step(decision, "asked"),
        no_route: true,
        internal: true,
        ..request(PostTarget::Channel(dm.clone()), "solo/worker", "Ship?")
    };
    let events = org.decide(&posting(question.clone())).unwrap();
    let post = the_post(&events);
    assert_eq!(post.channel, dm);
    assert_eq!(post.wake, Vec::<Address>::new());
    assert!(!post.route);
    assert_eq!(
        org.decide(&posting(PostRequest {
            internal: false,
            ..question.clone()
        })),
        Err(refused(ChannelRefusal::UnknownChannel(dm.clone())))
    );
    assert_eq!(
        org.decide(&posting(PostRequest {
            target: in_channel("solo"),
            ..question
        })),
        Err(refused(ChannelRefusal::UnknownChannel(channel("solo"))))
    );
}

// U8, Q1: design 12 puts a question in the asker's DM with the owner when its
// project channel is not listed, and `system` replies in its thread. Break
// caught: system refused there, or let in without the internal mark or
// without a decision step.
#[test]
fn system_may_post_a_decision_step_in_a_direct_thread_only_internally() {
    let mut org = org();
    let (decision, dm) = solo_question(&mut org);
    let asked = org.posted(PostRequest {
        kind: MessageKind::Question,
        tags: vec!["owner".into()],
        decision: step(decision, "asked"),
        internal: true,
        ..request(PostTarget::Channel(dm.clone()), "solo/worker", "Ship?")
    });
    let reply = PostRequest {
        kind: MessageKind::Answer,
        tags: vec!["solo/worker".into()],
        decision: step(decision, "resolved"),
        internal: true,
        ..request(PostTarget::Reply(asked), "system", "Decision resolved: yes")
    };
    let events = org.decide(&posting(reply.clone())).unwrap();
    let post = the_post(&events);
    assert_eq!(
        (post.channel.clone(), post.thread),
        (dm.clone(), Some(asked))
    );
    assert_eq!(post.wake, addrs(&["solo/worker"]));
    for (name, request) in [
        (
            "not internal",
            PostRequest {
                internal: false,
                ..reply.clone()
            },
        ),
        (
            "no decision step",
            PostRequest {
                decision: None,
                ..reply.clone()
            },
        ),
    ] {
        assert_eq!(
            org.decide(&posting(request)),
            Err(refused(ChannelRefusal::NotMember(dm.clone()))),
            "{name}"
        );
    }
    // A seat stays out of a DM it is no party of, internal or not.
    assert_eq!(
        org.decide(&posting(PostRequest {
            author: addr("atm/lead"),
            ..reply
        })),
        Err(refused(ChannelRefusal::NotMember(dm)))
    );
}
