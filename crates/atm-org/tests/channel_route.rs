#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! Routing (design 6.2, 8.1, 8.3): what a routing answer may say, the
//! stopgap's rules, and when the daemon falls back to them. Pure: posts are
//! decided against hand-written facts, and time is an argument.
#[path = "support/channel.rs"]
mod channel_support;
#[path = "support/policy.rs"]
mod policy_support;
use atm_core::SessionId;
use atm_org::channel_policy::*;
use atm_org::channel_query::*;
use atm_org::*;
use channel_support::*;
use chrono::{DateTime, Duration, Utc};
use policy_support::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// `org()` plus `atm/build-8` stuck, `build-9` starting, `build-10` clearing.
fn scenario() -> Org {
    let mut org = org();
    org.adopt("atm/build-8", Some("s-build-8"));
    org.stuck("atm/build-8");
    org.adopt("atm/build-9", None);
    org.adopt("atm/build-10", Some("s-build-10"));
    org.clearing("atm/build-10");
    org
}
fn untagged(org: &mut Org, target: PostTarget, author: &str) -> MessageId {
    org.posted(request(target, author, "untagged"))
}
fn answer(post: MessageId, to: &[&str], by: RouteBy) -> Vec<OrgEvent> {
    vec![OrgEvent::ChannelRouted(PostRouted {
        post,
        wake: addrs(to),
        by,
        reason: None,
    })]
}
fn with_reason(command: OrgCommand, reason: &str) -> OrgCommand {
    match command {
        OrgCommand::Channel(ChannelCommand::Route(mut answer)) => {
            answer.reason = Some(reason.into());
            OrgCommand::Channel(ChannelCommand::Route(answer))
        }
        other => other,
    }
}
fn invalid(field: &'static str, got: Result<Vec<OrgEvent>, Refusal>) -> bool {
    matches!(got, Err(Refusal::InvalidValue { field: f, .. }) if f == field)
}

#[test]
fn route_rules() {
    // Each row starts from the same org: a pending post in `atm` by build-2, a
    // tagged one, and a pending post in build-4's DM with web/lead.
    let fresh = || {
        let mut org = scenario();
        let pending = untagged(&mut org, in_channel("atm"), "atm/build-2");
        let tagged = org.posted(request(in_channel("atm"), "atm/build-2", "@lead done"));
        org.send("atm/build-4", "web/lead", None);
        let dm = untagged(&mut org, in_channel("atm/build-4+web/lead"), "atm/build-4");
        (org, pending, tagged, dm)
    };

    let (org, p, _, _) = fresh();
    assert_eq!(
        org.decide(&routing(p, &["atm/lead"], router())),
        Ok(answer(p, &["atm/lead"], router())),
        "first answer"
    );

    let (mut org, p, _, _) = fresh();
    org.run(routing(p, &["atm/lead"], router())).unwrap();
    assert_eq!(
        org.decide(&routing(p, &["atm/lead"], router())),
        Ok(vec![]),
        "identical repeat"
    );
    assert_eq!(
        org.decide(&with_reason(routing(p, &["atm/lead"], router()), "again")),
        Ok(vec![]),
        "a repeat with another reason"
    );
    assert_eq!(
        org.decide(&routing(p, &["atm/build-4"], router())),
        Err(refused(ChannelRefusal::AlreadyRouted(p))),
        "a different set"
    );
    assert_eq!(
        org.decide(&routing(p, &["atm/lead"], stopgap("timeout"))),
        Err(refused(ChannelRefusal::AlreadyRouted(p))),
        "the same set by someone else"
    );

    let (mut org, p, _, _) = fresh();
    org.run(routing(p, &["atm/lead", "atm/build-4"], router()))
        .unwrap();
    assert_eq!(
        org.decide(&routing(p, &["atm/build-4", "atm/lead"], router())),
        Ok(vec![]),
        "the same set in another order"
    );

    let (mut org, p, _, _) = fresh();
    org.run(routing(p, &["atm/lead"], stopgap("timeout")))
        .unwrap();
    assert_eq!(
        org.decide(&routing(p, &["atm/lead"], router())),
        Err(refused(ChannelRefusal::AlreadyRouted(p))),
        "a router after the stopgap"
    );

    let (mut org, p, _, _) = fresh();
    org.run(routing(p, &[], stopgap("declined"))).unwrap();
    assert_eq!(
        org.decide(&routing(p, &["atm/lead"], stopgap("declined"))),
        Err(refused(ChannelRefusal::AlreadyRouted(p))),
        "the empty set is final"
    );

    // A late answer for a seat that has gone down is a conflict, not a
    // membership error: the first answer stands.
    let (mut org, p, _, _) = fresh();
    org.run(routing(p, &["atm/lead"], router())).unwrap();
    org.down("atm/build-4");
    assert_eq!(
        org.decide(&routing(p, &["atm/build-4"], router())),
        Err(refused(ChannelRefusal::AlreadyRouted(p))),
        "a late answer naming a seat that went down"
    );

    let (mut org, p, tagged, dm) = fresh();
    type Row<'a> = (&'a str, OrgCommand, Result<Vec<OrgEvent>, Refusal>);
    let rows: Vec<Row> = vec![
        (
            "a post that was not routed",
            routing(tagged, &["atm/lead"], router()),
            Err(refused(ChannelRefusal::NotRouted(tagged))),
        ),
        (
            "an unknown post",
            routing(MessageId::new(999), &[], router()),
            Err(Refusal::Invariant(ApplyError::UnknownPost(MessageId::new(
                999,
            )))),
        ),
        (
            "a seat of another project",
            routing(p, &["web/lead"], router()),
            Err(refused(ChannelRefusal::NotRoutable {
                post: p,
                to: addr("web/lead"),
            })),
        ),
        (
            "a down seat in a project channel",
            routing(p, &["atm/build-6"], router()),
            Err(refused(ChannelRefusal::NotRoutable {
                post: p,
                to: addr("atm/build-6"),
            })),
        ),
        (
            "the author",
            routing(p, &["atm/build-2"], router()),
            Err(refused(ChannelRefusal::NotRoutable {
                post: p,
                to: addr("atm/build-2"),
            })),
        ),
        (
            "system",
            routing(p, &["system"], router()),
            Err(refused(ChannelRefusal::NotRoutable {
                post: p,
                to: Address::System,
            })),
        ),
        (
            "the owner outside an agent DM",
            routing(dm, &["owner"], router()),
            Err(refused(ChannelRefusal::NotRoutable {
                post: dm,
                to: Address::Owner,
            })),
        ),
        (
            "a stuck seat is accepted",
            routing(p, &["atm/build-8"], router()),
            Ok(answer(p, &["atm/build-8"], router())),
        ),
        (
            "the owner in a project channel",
            routing(p, &["owner"], stopgap("no_router")),
            Ok(answer(p, &["owner"], stopgap("no_router"))),
        ),
        (
            "duplicate addresses are dropped",
            routing(p, &["atm/lead", "atm/lead"], router()),
            Ok(answer(p, &["atm/lead"], router())),
        ),
        (
            "the empty set",
            routing(p, &[], stopgap("declined")),
            Ok(answer(p, &[], stopgap("declined"))),
        ),
    ];
    for (name, command, want) in rows {
        assert_eq!(org.decide(&command), want, "{name}");
    }

    org.down("web/lead");
    assert_eq!(
        org.decide(&routing(dm, &["web/lead"], router())),
        Ok(answer(dm, &["web/lead"], router())),
        "a DM party that is down"
    );

    let later: RouteBy = serde_json::from_value(serde_json::json!({"kind": "oracle"})).unwrap();
    assert!(
        invalid("by", org.decide(&routing(p, &["atm/lead"], later))),
        "a later router kind"
    );
    assert!(
        invalid(
            "by",
            org.decide(&routing(p, &["atm/lead"], stopgap("storm")))
        ),
        "an unknown stopgap cause"
    );
    assert!(
        invalid(
            "reason",
            org.decide(&with_reason(
                routing(p, &["atm/lead"], router()),
                &"r".repeat(257)
            ))
        ),
        "a reason over 256 bytes"
    );
    assert!(
        org.decide(&with_reason(
            routing(p, &["atm/lead"], router()),
            &"r".repeat(256)
        ))
        .is_ok(),
        "a reason of 256 bytes"
    );
}

#[test]
fn stopgap_rules() {
    type Build = fn(&mut Org) -> MessageId;
    let rows: Vec<(&str, Build, &[&str])> = vec![
        (
            "1 direct: the other seat",
            |org| {
                org.send("atm/build-2", "web/lead", None);
                untagged(org, in_channel("atm/build-2+web/lead"), "atm/build-2")
            },
            &["web/lead"],
        ),
        (
            "1 direct: a party that is down still gets its row",
            |org| {
                org.send("atm/build-2", "web/lead", None);
                let post = untagged(org, in_channel("atm/build-2+web/lead"), "atm/build-2");
                org.down("web/lead");
                post
            },
            &["web/lead"],
        ),
        (
            "1 direct: a seat's post reaches the owner",
            |org| {
                org.send("atm/build-2", "owner", None);
                untagged(org, in_channel("atm/build-2+owner"), "atm/build-2")
            },
            &["owner"],
        ),
        (
            "1 direct: the owner's post reaches the seat",
            |org| {
                org.send("atm/build-2", "owner", None);
                untagged(org, in_channel("atm/build-2+owner"), "owner")
            },
            &["atm/build-2"],
        ),
        (
            "1 direct with system: nobody",
            |org| {
                org.send("system", "atm/build-2", None);
                untagged(org, in_channel("atm/build-2+system"), "atm/build-2")
            },
            &[],
        ),
        (
            "2a the lead answers in the owner's thread: the owner alone",
            |org| {
                let root = untagged(org, in_channel("atm"), "owner");
                org.stopgap_route(root);
                untagged(org, PostTarget::Reply(root), "atm/lead")
            },
            &["owner"],
        ),
        (
            "2b the owner's next reply reaches the lead",
            |org| {
                let root = untagged(org, in_channel("atm"), "owner");
                org.stopgap_route(root);
                let answer = untagged(org, PostTarget::Reply(root), "atm/lead");
                org.stopgap_route(answer);
                untagged(org, PostTarget::Reply(root), "owner")
            },
            &["atm/lead"],
        ),
        (
            "2b the lead answers a worker in its own broadcast thread: that worker alone",
            |org| {
                let root = untagged(org, in_channel("atm"), "atm/lead");
                org.stopgap_route(root);
                let question = untagged(org, PostTarget::Reply(root), "atm/build-2");
                org.stopgap_route(question);
                untagged(org, PostTarget::Reply(root), "atm/lead")
            },
            &["atm/build-2"],
        ),
        (
            // The row that separates the two readings of 2b: the latest other
            // replier is down, an earlier one is up. Read literally, 2b would
            // name nobody and fall to 2c, which names nobody for a broadcast.
            "2b a down latest replier gives way to an earlier live one",
            |org| {
                let root = untagged(org, in_channel("atm"), "atm/lead");
                org.stopgap_route(root);
                untagged(org, PostTarget::Reply(root), "atm/build-2");
                untagged(org, PostTarget::Reply(root), "atm/build-4");
                org.down("atm/build-4");
                untagged(org, PostTarget::Reply(root), "atm/lead")
            },
            &["atm/build-2"],
        ),
        (
            "2a before 2b: the root's author first",
            |org| {
                let root = untagged(org, in_channel("atm"), "owner");
                org.stopgap_route(root);
                untagged(org, PostTarget::Reply(root), "atm/lead");
                untagged(org, PostTarget::Reply(root), "atm/build-4")
            },
            &["owner"],
        ),
        (
            "2b looks only at earlier replies",
            |org| {
                let root = untagged(org, in_channel("atm"), "atm/lead");
                org.stopgap_route(root);
                untagged(org, PostTarget::Reply(root), "atm/build-2");
                let answer = untagged(org, PostTarget::Reply(root), "atm/lead");
                untagged(org, PostTarget::Reply(root), "atm/build-4");
                answer
            },
            &["atm/build-2"],
        ),
        (
            "2c a root that woke several: nobody",
            |org| {
                let root = org.posted(request(
                    in_channel("atm"),
                    "system",
                    "@build-2 @build-4 check",
                ));
                untagged(org, PostTarget::Reply(root), "atm/lead")
            },
            &[],
        ),
        (
            "2c a single wake that is down: nobody",
            |org| {
                let root = org.posted(request(in_channel("atm"), "system", "@build-6 check"));
                untagged(org, PostTarget::Reply(root), "atm/build-4")
            },
            &[],
        ),
        (
            "2b picks the latest earlier replier when several are live",
            |org| {
                let root = untagged(org, in_channel("atm"), "atm/lead");
                org.stopgap_route(root);
                untagged(org, PostTarget::Reply(root), "atm/build-2");
                untagged(org, PostTarget::Reply(root), "atm/build-4");
                untagged(org, PostTarget::Reply(root), "atm/lead")
            },
            &["atm/build-4"],
        ),
        (
            "2a a down root author is skipped",
            |org| {
                let root = untagged(org, in_channel("atm"), "atm/build-4");
                org.down("atm/build-4");
                untagged(org, PostTarget::Reply(root), "atm/build-2")
            },
            &[],
        ),
        (
            "2c the root's single wake",
            |org| {
                let root = org.posted(request(in_channel("atm"), "system", "@build-2 check"));
                untagged(org, PostTarget::Reply(root), "atm/build-4")
            },
            &["atm/build-2"],
        ),
        (
            "2c never names the replier",
            |org| {
                let root = org.posted(request(in_channel("atm"), "system", "@build-2 check"));
                untagged(org, PostTarget::Reply(root), "atm/build-2")
            },
            &[],
        ),
        (
            "2d nobody",
            |org| {
                let root = untagged(org, in_channel("atm"), "system");
                untagged(org, PostTarget::Reply(root), "atm/build-4")
            },
            &[],
        ),
        (
            "3 the owner's top-level post: the lead",
            |org| untagged(org, in_channel("atm"), "owner"),
            &["atm/lead"],
        ),
        (
            "3 a down lead yields nobody",
            |org| {
                org.down("atm/lead");
                untagged(org, in_channel("atm"), "owner")
            },
            &[],
        ),
        (
            "3 a stuck lead is still the owner's target",
            |org| {
                org.stuck("atm/lead");
                untagged(org, in_channel("atm"), "owner")
            },
            &["atm/lead"],
        ),
        (
            "4 the lead's broadcast: seats starting, ready or clearing",
            |org| untagged(org, in_channel("atm"), "atm/lead"),
            &["atm/build-10", "atm/build-2", "atm/build-4", "atm/build-9"],
        ),
        (
            "5 a worker's top-level post: the lead",
            |org| untagged(org, in_channel("atm"), "atm/build-2"),
            &["atm/lead"],
        ),
        (
            "5 a stuck lead is still a worker's target",
            |org| {
                org.stuck("atm/lead");
                untagged(org, in_channel("atm"), "atm/build-2")
            },
            &["atm/lead"],
        ),
        (
            "5 a worker's post while the lead is down: nobody",
            |org| {
                org.down("atm/lead");
                untagged(org, in_channel("atm"), "atm/build-2")
            },
            &[],
        ),
        (
            "6 a system post: nobody",
            |org| untagged(org, in_channel("atm"), "system"),
            &[],
        ),
        ("an unknown post: nobody", |_| MessageId::new(999), &[]),
    ];
    for (name, build, want) in rows {
        let mut org = scenario();
        let post = build(&mut org);
        assert_eq!(stopgap_wake(&org.state, post), addrs(want), "{name}");
    }
}

// The generated org: the seats of `org()`, each moved between statuses.
const SEATS: [&str; 6] = [
    "atm/lead",
    "atm/build-2",
    "atm/build-4",
    "atm/build-6",
    "web/lead",
    "web/build-1",
];
const AUTHORS: [&str; 8] = [
    "owner",
    "system",
    "atm/lead",
    "atm/build-2",
    "atm/build-4",
    "atm/build-6",
    "web/lead",
    "web/build-1",
];

#[derive(Debug, Clone)]
enum Step {
    Status(usize, u8),
    Post {
        author: usize,
        target: u8,
        pick: usize,
        tag: Option<usize>,
    },
    Route(usize),
}
fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        1 => (0..SEATS.len(), 0u8..5).prop_map(|(seat, status)| Step::Status(seat, status)),
        3 => (0..AUTHORS.len(), 0u8..5, any::<usize>(), prop::option::weighted(0.2, 0..SEATS.len()))
            .prop_map(|(author, target, pick, tag)| Step::Post { author, target, pick, tag }),
        1 => any::<usize>().prop_map(Step::Route),
    ]
}

/// The kind of post the stopgap answered, for the reach check.
fn rule_of(org: &Org, post: MessageId) -> &'static str {
    let content = &channel_query::post(&org.state, post).unwrap().content;
    let lead = |p: &ProjectId| project_lead(&org.state, p).map(|a| Address::Agent(a.id.clone()));
    match (&content.channel, content.thread, &content.author) {
        (ChannelId::Direct(..), _, _) => "direct",
        (_, Some(_), _) => "reply",
        (_, None, Address::Owner) => "owner",
        (ChannelId::Project(p), None, author) if Some(author) == lead(p).as_ref() => "lead",
        _ => "worker",
    }
}

/// Every pending post: `Route` accepts the stopgap's answer, and a reply gets
/// at most one target.
fn check_pending(org: &Org, reach: &RefCell<BTreeMap<&'static str, usize>>) {
    let pending: Vec<MessageId> = pending_routes(&org.state).map(|(id, _)| id).collect();
    for post in pending {
        let wake = stopgap_wake(&org.state, post);
        let is_reply = channel_query::post(&org.state, post)
            .unwrap()
            .content
            .thread
            .is_some();
        assert!(!is_reply || wake.len() <= 1, "{post}: {wake:?}");
        let command = OrgCommand::Channel(ChannelCommand::Route(PostRouted {
            post,
            wake: wake.clone(),
            by: stopgap("disabled"),
            reason: None,
        }));
        let events = org
            .decide(&command)
            .unwrap_or_else(|e| panic!("{post}: {e}"));
        assert_eq!(events.len(), 1, "{post}");
        if !wake.is_empty() {
            *reach.borrow_mut().entry(rule_of(org, post)).or_default() += 1;
        }
    }
}

#[test]
fn stopgap_answers_are_accepted_by_route() {
    let reach = RefCell::new(BTreeMap::new());
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x5709),
        failure_persistence: None, // the fixed seed reproduces the run
        ..Config::default()
    });
    runner
        .run(&prop::collection::vec(arb_step(), 1..60), |steps| {
            let mut org = org();
            org.send("atm/build-2", "owner", None);
            org.send("atm/lead", "web/lead", None);
            let mut posts: Vec<MessageId> = Vec::new();
            for step in steps {
                match step {
                    Step::Status(seat, status) => {
                        let agent_id = agent_id(SEATS[seat]);
                        let adopt = |session: Option<String>| OrgEvent::AgentAdopted {
                            agent_id: agent_id.clone(),
                            pane: format!("%{seat}"),
                            session_id: session.map(SessionId::new),
                        };
                        let event = match status {
                            0 => adopt(Some(format!("s-{seat}"))),
                            1 => adopt(None),
                            2 => OrgEvent::AgentClearRequested { agent_id },
                            3 => OrgEvent::AgentStuck {
                                agent_id,
                                reason: "dialog".into(),
                            },
                            _ => OrgEvent::AgentDown {
                                agent_id,
                                reason: "gone".into(),
                            },
                        };
                        // Any status the fold accepts; a refused fact changes nothing.
                        let _ = org.try_apply(event);
                    }
                    Step::Post {
                        author,
                        target,
                        pick,
                        tag,
                    } => {
                        let author = AUTHORS[author];
                        let other = AUTHORS[pick % AUTHORS.len()];
                        let target = match target {
                            0 => in_channel("atm"),
                            1 => in_channel("web"),
                            2 => direct(other),
                            3 if !posts.is_empty() => PostTarget::Reply(posts[pick % posts.len()]),
                            _ => match ChannelId::direct(addr(author), addr(other)) {
                                Some(dm) => PostTarget::Channel(dm),
                                None => in_channel("atm"),
                            },
                        };
                        let body = tag.map_or("untagged".into(), |t| format!("@{}", SEATS[t]));
                        if let Ok(events) = org.decide(&posting(request(target, author, &body))) {
                            for event in events {
                                posts.push(org.apply(event));
                            }
                        }
                    }
                    Step::Route(pick) => {
                        let pending: Vec<MessageId> =
                            pending_routes(&org.state).map(|(id, _)| id).collect();
                        if let Some(post) = pending.get(pick % pending.len().max(1)) {
                            org.stopgap_route(*post);
                        }
                    }
                }
                check_pending(&org, &reach);
            }
            Ok(())
        })
        .unwrap();
    let reach = reach.into_inner();
    println!("stopgap reach (256 cases): {reach:?}");
    for rule in ["direct", "reply", "owner", "lead", "worker"] {
        assert!(
            reach.get(rule).is_some_and(|n| *n > 0),
            "{rule} never woke anyone"
        );
    }
}

fn t(seconds: i64) -> DateTime<Utc> {
    at(0) + Duration::seconds(seconds)
}

#[test]
fn route_step_table() {
    let on = RouterSettings {
        enabled: true,
        timeout_secs: 20,
    };
    let attached = |since| RouterPresence::Attached { since: t(since) };
    let rows: Vec<(&str, RouterSettings, RouterPresence, i64, i64, RouteStep)> = vec![
        (
            "disabled",
            RouterSettings::default(),
            attached(-100),
            0,
            0,
            RouteStep::Stopgap("disabled"),
        ),
        (
            "absent",
            on,
            RouterPresence::Absent,
            0,
            0,
            RouteStep::Stopgap("no_router"),
        ),
        (
            "in grace before its end",
            on,
            RouterPresence::Grace { until: t(20) },
            0,
            5,
            RouteStep::Wait(t(20)),
        ),
        (
            "in grace at its end",
            on,
            RouterPresence::Grace { until: t(20) },
            0,
            20,
            RouteStep::Stopgap("no_router"),
        ),
        (
            "attached before the deadline",
            on,
            attached(-100),
            0,
            19,
            RouteStep::Wait(t(20)),
        ),
        (
            "attached at the deadline",
            on,
            attached(-100),
            0,
            20,
            RouteStep::Stopgap("timeout"),
        ),
        (
            "an attach after the post moves the deadline",
            on,
            attached(30),
            0,
            35,
            RouteStep::Wait(t(50)),
        ),
        (
            "a zero timeout falls back at once",
            RouterSettings {
                enabled: true,
                timeout_secs: 0,
            },
            attached(-100),
            0,
            0,
            RouteStep::Stopgap("timeout"),
        ),
        (
            "an overflowing timeout waits",
            RouterSettings {
                enabled: true,
                timeout_secs: u64::MAX,
            },
            attached(-100),
            0,
            1000,
            RouteStep::Wait(DateTime::<Utc>::MAX_UTC),
        ),
    ];
    for (name, settings, presence, posted, now, want) in rows {
        assert_eq!(
            route_step(t(posted), t(now), &settings, presence),
            want,
            "{name}"
        );
    }
}

#[test]
fn router_settings_default_and_refuse_unknown_keys() {
    assert_eq!(
        RouterSettings::default(),
        RouterSettings {
            enabled: false,
            timeout_secs: 20,
        }
    );
    assert_eq!(
        serde_json::from_str::<RouterSettings>("{}").unwrap(),
        RouterSettings::default()
    );
    assert_eq!(
        serde_json::from_str::<RouterSettings>(r#"{"enabled": true}"#).unwrap(),
        RouterSettings {
            enabled: true,
            timeout_secs: 20,
        }
    );
    assert!(serde_json::from_str::<RouterSettings>(r#"{"enable": true}"#).is_err());
}
