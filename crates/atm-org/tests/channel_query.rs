#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! Channel history, listing, membership and unread counts over hand-written
//! facts.
#[path = "support/channel.rs"]
mod channel_support;
use atm_org::channel_query::*;
use atm_org::*;
use channel_support::*;

fn page(state: &OrgState, channel_text: &str, query: HistoryQuery) -> (Vec<MessageId>, bool) {
    let page = channel_history(state, &channel(channel_text), &query).unwrap();
    (page.rows.iter().map(|r| r.post.id()).collect(), page.more)
}
fn query(limit: usize) -> HistoryQuery {
    HistoryQuery {
        limit,
        ..HistoryQuery::default()
    }
}
fn listed<'a>(state: &'a OrgState, reader: &str, scope: ListScope) -> Vec<ChannelSummary<'a>> {
    channels_for(state, &addr(reader), scope)
}
fn ids(summaries: &[ChannelSummary<'_>]) -> Vec<String> {
    summaries.iter().map(|s| s.id.to_string()).collect()
}
fn find<'a>(summaries: &'a [ChannelSummary<'a>], id: &str) -> &'a ChannelSummary<'a> {
    summaries
        .iter()
        .find(|s| s.id.to_string() == id)
        .unwrap_or_else(|| panic!("{id} not listed"))
}

#[test]
fn history_top_level_is_newest_first_with_window_and_more() {
    let mut org = Org::new();
    let posts: Vec<_> = (0..5).map(|_| org.post(note("atm", "owner"))).collect();
    let [p0, p1, p2, p3, p4] = posts[..] else {
        panic!("five posts")
    };
    let reply = org.post(PostContent {
        thread: Some(p1),
        ..note("atm", "atm/lead")
    });
    assert_eq!(
        page(&org.state, "atm", query(50)),
        (vec![p4, p3, p2, p1, p0], false)
    );
    assert_eq!(page(&org.state, "atm", query(2)), (vec![p4, p3], true));
    let older = HistoryQuery {
        before: Some(p3),
        ..query(2)
    };
    assert_eq!(page(&org.state, "atm", older), (vec![p2, p1], true));
    let newer = HistoryQuery {
        after: Some(p2),
        ..query(50)
    };
    assert_eq!(page(&org.state, "atm", newer), (vec![p4, p3], false));
    let between = HistoryQuery {
        after: Some(p0),
        before: Some(p4),
        ..query(2)
    };
    assert_eq!(page(&org.state, "atm", between), (vec![p3, p2], true));
    // Replies are not top-level rows; an empty or inverted window is empty.
    assert!(!page(&org.state, "atm", query(50)).0.contains(&reply));
    for (after, before) in [(p2, p2), (p3, p2), (p2, p3)] {
        let q = HistoryQuery {
            after: Some(after),
            before: Some(before),
            ..query(50)
        };
        assert_eq!(page(&org.state, "atm", q), (vec![], false));
    }
    assert_eq!(page(&org.state, "atm", query(0)), (vec![], true));
    // A listed channel with no posts, or no channel at all, is an empty page.
    assert_eq!(
        page(&org.state, "atm/lead+owner", query(50)),
        (vec![], false)
    );
}

#[test]
fn shared_reply_replays_into_both_views_once() {
    for id in ["atm", "atm/lead+owner"] {
        let mut org = Org::new();
        let root = if id == "atm" {
            org.post(note(id, "owner"))
        } else {
            org.send("owner", "atm/lead", None)
        };
        let ordinary = org.post(PostContent {
            thread: Some(root),
            ..note(id, "atm/lead")
        });
        let mut fact = serde_json::to_value(waking(note(id, "atm/lead"), &["owner"])).unwrap();
        fact["thread"] = serde_json::json!(root);
        fact["also_in_chat"] = serde_json::json!(true);
        let shared = org.post(serde_json::from_value(fact).unwrap());
        let [wake] = org.wake_all()[..] else {
            panic!("one wake")
        };
        let later = org.post(note(id, "owner"));
        let expanded = HistoryQuery {
            include_shared_replies: true,
            ..query(50)
        };
        assert_eq!(page(&org.state, id, query(50)), (vec![later, root], false));
        assert_eq!(
            page(&org.state, id, expanded.clone()),
            (vec![later, shared, root], false)
        );
        for (before, after, want, more) in [
            (None, None, later, true),
            (Some(later), None, shared, true),
            (Some(shared), None, root, false),
            (Some(later), Some(root), shared, false),
        ] {
            assert_eq!(
                page(
                    &org.state,
                    id,
                    HistoryQuery {
                        before,
                        after,
                        limit: 1,
                        ..expanded.clone()
                    }
                ),
                (vec![want], more)
            );
        }
        for thread in [root, shared, wake] {
            let rows = channel_history(
                &org.state,
                &channel(id),
                &HistoryQuery {
                    thread: Some(thread),
                    ..query(50)
                },
            )
            .unwrap()
            .rows;
            assert_eq!(
                rows.iter().map(|r| r.post.id()).collect::<Vec<_>>(),
                [root, ordinary, shared]
            );
            let first = rows.first().unwrap();
            assert_eq!(
                (first.replies, first.last_reply_at),
                (2, Some(at(shared.seq())))
            );
        }
        let rows = channel_history(&org.state, &channel(id), &expanded)
            .unwrap()
            .rows;
        let row = rows.iter().find(|r| r.post.id() == shared).unwrap();
        assert_eq!(row.post.at(), at(shared.seq()));
    }
}

#[test]
fn also_in_chat_does_not_double_unread_or_change_seen() {
    for also_in_chat in [false, true] {
        let mut org = Org::new();
        let root = org.post(note("atm", "owner"));
        let reply = org.post(waking(
            PostContent {
                thread: Some(root),
                also_in_chat,
                ..note("atm", "atm/lead")
            },
            &["owner"],
        ));
        assert_eq!(org.wake_all().len(), 1);
        assert_eq!(unread_in(&org, "atm", "owner"), 1);
        mark_seen(&mut org, "atm", root, &["owner"]);
        assert_eq!(unread_in(&org, "atm", "owner"), 1);
        mark_seen(&mut org, "atm", reply, &["owner"]);
        let summary = listed(&org.state, "owner", ListScope::Own);
        let row = find(&summary, "atm");
        assert_eq!((row.unread, row.seen_through), (0, Some(reply)));
        assert_eq!(row.last_post.unwrap().id(), reply);
        assert_eq!(row.activity_seq, reply.seq());
    }
}

#[test]
fn history_thread_is_root_then_replies_oldest_first() {
    let mut org = Org::new();
    let root = org.post(note("atm", "owner"));
    let other = org.post(note("atm", "owner"));
    let replies: Vec<_> = ["atm/lead", "atm/build-2", "owner"]
        .into_iter()
        .map(|author| {
            org.post(PostContent {
                thread: Some(root),
                ..note("atm", author)
            })
        })
        .collect();
    org.post(PostContent {
        thread: Some(other),
        ..note("atm", "atm/lead")
    });
    let thread = |q: HistoryQuery| {
        page(
            &org.state,
            "atm",
            HistoryQuery {
                thread: Some(root),
                ..q
            },
        )
    };
    let mut all = vec![root];
    all.extend(&replies);
    assert_eq!(thread(query(50)), (all.clone(), false));
    assert_eq!(thread(query(2)), (all[..2].to_vec(), true));
    let rest = HistoryQuery {
        after: Some(all[1]),
        ..query(50)
    };
    assert_eq!(thread(rest), (all[2..].to_vec(), false));
}

#[test]
fn history_thread_by_wake_id_or_reply_id_finds_the_root() {
    let mut org = Org::new();
    let root = org.post(waking(note("atm", "atm/lead"), &["atm/build-2"]));
    let [root_wake] = org.wake_all()[..] else {
        panic!("one wake")
    };
    let reply = org.post(waking(
        PostContent {
            thread: Some(root),
            ..note("atm", "atm/build-2")
        },
        &["atm/lead"],
    ));
    let [reply_wake] = org.wake_all()[..] else {
        panic!("one wake")
    };
    for id in [root, root_wake, reply, reply_wake] {
        let q = HistoryQuery {
            thread: Some(id),
            ..query(50)
        };
        assert_eq!(
            page(&org.state, "atm", q),
            (vec![root, reply], false),
            "{id}"
        );
    }
}

#[test]
fn history_thread_in_another_channel_or_unknown_is_refused() {
    let mut org = Org::new();
    // `atm` has posts of its own, so the check is on membership, not existence.
    org.post(note("atm", "owner"));
    let direct = org.post(note("atm/lead+owner", "owner"));
    let q = |id| HistoryQuery {
        thread: Some(id),
        ..query(50)
    };
    assert_eq!(
        channel_history(&org.state, &channel("atm"), &q(direct)),
        Err(ChannelQueryError::NotInChannel {
            post: direct,
            channel: channel("atm")
        })
    );
    let missing = MessageId::new(999);
    assert_eq!(
        channel_history(&org.state, &channel("atm"), &q(missing)),
        Err(ChannelQueryError::UnknownPost(missing))
    );
}

#[test]
fn history_rows_carry_replies_routing_and_decision() {
    let mut org = Org::new();
    let decision = org.decision();
    let question = org.post(PostContent {
        decision: Some(DecisionStep {
            decision_id: decision,
            step: "asked".into(),
        }),
        ..waking(note("atm", "atm/lead"), &["owner"])
    });
    let open = org.post(PostContent {
        route: true,
        ..note("atm", "owner")
    });
    let answered = org.post(PostContent {
        route: true,
        ..note("atm", "owner")
    });
    org.apply(routed(answered, &["atm/lead"], "disabled"));
    let first = org.post(PostContent {
        thread: Some(question),
        ..note("atm", "owner")
    });
    let last = org.post(PostContent {
        thread: Some(question),
        ..note("atm", "atm/build-2")
    });
    let rows = channel_history(&org.state, &channel("atm"), &query(50))
        .unwrap()
        .rows;
    let row = |id: MessageId| rows.iter().find(|r| r.post.id() == id).unwrap();
    assert_eq!(row(question).replies, 2);
    assert_eq!(row(question).last_reply_at, Some(at(last.seq())));
    assert_ne!(row(question).last_reply_at, Some(at(first.seq())));
    assert_eq!(row(question).decision.map(|d| d.id), Some(decision));
    assert_eq!(row(open).replies, 0);
    assert_eq!(row(open).decision, None);
    let routing = |id: MessageId| match row(id).post {
        ChannelPost::Native(post) => post.routing(),
        ChannelPost::Legacy(_) => panic!("native"),
    };
    assert_eq!(routing(question), Routing::None);
    assert_eq!(routing(open), Routing::Pending);
    assert_eq!(routing(answered), Routing::Stopgap);
}

#[test]
fn history_mixes_legacy_and_native_posts_in_id_order() {
    let mut org = Org::new();
    let old = org.send("owner", "atm/lead", None);
    let native = org.post(note("atm/lead+owner", "atm/lead"));
    let newer = org.send("atm/lead", "owner", None);
    let rows = channel_history(&org.state, &channel("atm/lead+owner"), &query(50))
        .unwrap()
        .rows;
    let shown: Vec<_> = rows
        .iter()
        .map(|r| (r.post.id(), matches!(r.post, ChannelPost::Legacy(_))))
        .collect();
    assert_eq!(shown, [(newer, true), (native, false), (old, true)]);
    assert_eq!(rows.last().unwrap().post.author(), &Address::Owner);
    assert_eq!(rows.last().unwrap().post.at(), at(old.seq()));
}

#[test]
fn self_addressed_message_is_not_indexed() {
    let mut org = Org::new();
    org.send("owner", "owner", None);
    org.send("atm/lead", "atm/lead", None);
    // The owner's list always has its DM with the lead seat; no direct channel has a post.
    assert!(listed(&org.state, "owner", ListScope::All)
        .iter()
        .all(|s| !matches!(s.id, ChannelId::Direct(..)) || s.last_post.is_none()));
}

#[test]
fn unread_counts_posts_by_others_after_the_marker() {
    let mut org = Org::new();
    let p1 = org.post(note("atm", "atm/lead"));
    org.post(note("atm", "atm/build-2"));
    let reply = org.post(PostContent {
        thread: Some(p1),
        ..note("atm", "atm/lead")
    });
    org.post(note("atm", "owner"));
    org.apply(OrgEvent::ChannelSeen(ChannelSeen {
        channel: channel("atm"),
        reader: Address::Owner,
        through: p1,
        session_id: None,
    }));
    let summary = listed(&org.state, "owner", ListScope::Own);
    let atm = find(&summary, "atm");
    // p2 and the reply are by others; the owner's own post does not count.
    assert_eq!((atm.unread, atm.seen_through), (2, Some(p1)));
    org.apply(OrgEvent::ChannelSeen(ChannelSeen {
        channel: channel("atm"),
        reader: Address::Owner,
        through: reply,
        session_id: None,
    }));
    let summary = listed(&org.state, "owner", ListScope::Own);
    assert_eq!(find(&summary, "atm").unread, 0);
}

#[test]
fn seen_markers_only_advance() {
    let mut org = Org::new();
    let p1 = org.post(note("atm", "atm/lead"));
    let p2 = org.post(note("atm", "atm/lead"));
    let seen = |through| {
        OrgEvent::ChannelSeen(ChannelSeen {
            channel: channel("atm"),
            reader: Address::Owner,
            through,
            session_id: None,
        })
    };
    org.apply(seen(p2));
    org.apply(seen(p1));
    org.apply(seen(p2));
    let summary = listed(&org.state, "owner", ListScope::Own);
    assert_eq!(find(&summary, "atm").seen_through, Some(p2));
    let lead = listed(&org.state, "atm/lead", ListScope::Own);
    assert_eq!(
        find(&lead, "atm").seen_through,
        None,
        "markers are per reader"
    );
}

#[test]
fn unread_without_marker_counts_own_unacked_rows() {
    let mut org = Org::new();
    let tagged = org.post(waking(note("atm", "atm/lead"), &["owner", "atm/build-2"]));
    org.post(note("atm", "atm/lead"));
    let rows = org.wake_all();
    org.send("atm/lead", "owner", None);
    let read = org.send("atm/lead", "owner", None);
    org.apply(OrgEvent::MessageRead {
        message_ids: vec![read],
    });
    let owner = listed(&org.state, "owner", ListScope::Own);
    // In `atm`: his wake of the tagged post. The untagged post is no row of his.
    assert_eq!(find(&owner, "atm").unread, 1);
    // In the direct channel: one unread legacy message, one read.
    assert_eq!(find(&owner, "atm/lead+owner").unread, 1);
    let owner_row = wake_for(&org.state, tagged, &Address::Owner).unwrap();
    assert!(rows.contains(&owner_row));
    org.apply(OrgEvent::MessageRead {
        message_ids: vec![owner_row],
    });
    let owner = listed(&org.state, "owner", ListScope::Own);
    assert_eq!(find(&owner, "atm").unread, 0);
    let seat = listed(&org.state, "atm/build-2", ListScope::Own);
    assert_eq!(find(&seat, "atm").unread, 1);
}

fn mark_seen(org: &mut Org, channel_text: &str, through: MessageId, readers: &[&str]) {
    for reader in readers {
        org.apply(OrgEvent::ChannelSeen(ChannelSeen {
            channel: channel(channel_text),
            reader: addr(reader),
            through,
            session_id: None,
        }));
    }
}

fn unread_in(org: &Org, channel_text: &str, reader: &str) -> usize {
    find(&listed(&org.state, reader, ListScope::All), channel_text).unread
}

fn ask_post(
    org: &mut Org,
    channel_text: &str,
    wake: &[&str],
    route: bool,
) -> (DecisionId, MessageId) {
    let decision_id = org.decision();
    let asked = org.post(waking(
        PostContent {
            kind: MessageKind::Question,
            route,
            decision: Some(DecisionStep {
                decision_id,
                step: "asked".into(),
            }),
            ..note(channel_text, "atm/lead")
        },
        wake,
    ));
    (decision_id, asked)
}

#[test]
fn unread_backfilled_question_is_silent_for_owner() {
    for channel_text in ["atm", "atm/lead+owner"] {
        let mut org = Org::new();
        let anchor = org.post(note(channel_text, "owner"));
        mark_seen(&mut org, channel_text, anchor, &["owner", "atm/build-2"]);
        let (_, backfill) = ask_post(&mut org, channel_text, &[], false);
        let (_, live) = ask_post(&mut org, channel_text, &["owner"], false);
        // The live ask counts as soon as it is posted, even before its wake row.
        assert_eq!(unread_in(&org, channel_text, "owner"), 1, "{channel_text}");
        assert_eq!(
            unread_in(&org, channel_text, "atm/build-2"),
            2,
            "{channel_text}"
        );
        assert_eq!(
            page(&org.state, channel_text, query(50)),
            (vec![live, backfill, anchor], false)
        );
    }
}

fn decision_reply(
    org: &mut Org,
    decision_id: DecisionId,
    root: MessageId,
    step: &str,
) -> MessageId {
    org.post(waking(
        PostContent {
            kind: MessageKind::Answer,
            thread: Some(root),
            decision: Some(DecisionStep {
                decision_id,
                step: step.into(),
            }),
            ..note("atm", "system")
        },
        &["atm/lead"],
    ))
}

fn assert_own_decision_answer_is_silent(overruled: bool, expected: (usize, usize)) {
    let mut org = Org::new();
    let (decision_id, root) = ask_post(&mut org, "atm", &[], false);
    mark_seen(&mut org, "atm", root, &["owner", "atm/lead"]);
    let mut thread = vec![root];
    let answer = DecisionResolution {
        decision_id,
        option: if overruled { "b" } else { "a" }.into(),
        by: Address::Owner,
        note: None,
        standing_rule: false,
    };
    let step = if overruled {
        org.apply(OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
            decision_id,
            option: "a".into(),
            by: Address::Owner,
            relayed_by: Some("atm/build-4".parse().unwrap()),
        }));
        thread.push(decision_reply(&mut org, decision_id, root, "taken"));
        org.apply(OrgEvent::DecisionOverruled(answer.into()));
        "overruled"
    } else {
        org.apply(OrgEvent::DecisionResolved(answer.into()));
        "resolved"
    };
    thread.push(decision_reply(&mut org, decision_id, root, step));
    assert_eq!(
        (
            unread_in(&org, "atm", "owner"),
            unread_in(&org, "atm", "atm/lead")
        ),
        expected
    );
    assert_eq!(
        page(
            &org.state,
            "atm",
            HistoryQuery {
                thread: Some(root),
                ..query(50)
            }
        ),
        (thread, false)
    );
}

#[test]
fn unread_own_resolved_answer_is_silent() {
    assert_own_decision_answer_is_silent(false, (0, 1));
}

#[test]
fn unread_own_overruled_answer_is_silent() {
    assert_own_decision_answer_is_silent(true, (1, 2));
}

#[test]
fn unread_other_posts_and_addressed_wakes_still_count() {
    let mut org = Org::new();
    let (decision_id, root) = ask_post(&mut org, "atm", &[], false);
    mark_seen(&mut org, "atm", root, &["owner"]);
    org.post(PostContent {
        kind: MessageKind::Answer,
        ..note("atm", "system")
    });
    assert_eq!(unread_in(&org, "atm", "owner"), 1);
    // Replay can carry an answer by a different agent even though current
    // write policy only allows the owner to resolve decisions.
    org.apply(OrgEvent::DecisionResolved(
        DecisionResolution {
            decision_id,
            option: "a".into(),
            by: addr("atm/build-4"),
            note: None,
            standing_rule: false,
        }
        .into(),
    ));
    decision_reply(&mut org, decision_id, root, "resolved");
    assert_eq!(unread_in(&org, "atm", "owner"), 2);
    // A routable replay fact with no wake is not the silent backfill shape.
    ask_post(&mut org, "atm", &[], true);
    assert_eq!(unread_in(&org, "atm", "owner"), 3);

    org.wake_all();
    assert_eq!(unread_in(&org, "atm", "atm/lead"), 1);
}

#[test]
fn activity_moves_on_posts_routing_and_linked_decisions_only() {
    let mut org = Org::new();
    let activity =
        |org: &Org, id: &str| find(&listed(&org.state, "owner", ListScope::All), id).activity_seq;
    let decision = org.decision();
    let question = org.post(PostContent {
        decision: Some(DecisionStep {
            decision_id: decision,
            step: "asked".into(),
        }),
        route: true,
        ..note("atm", "atm/lead")
    });
    assert_eq!(activity(&org, "atm"), question.seq());
    let answer = org.apply(routed(question, &["owner"], "disabled"));
    assert_eq!(activity(&org, "atm"), answer.seq());
    // A wake row and a marker do not move it.
    org.wake_all();
    org.apply(OrgEvent::ChannelSeen(ChannelSeen {
        channel: channel("atm"),
        reader: Address::Owner,
        through: question,
        session_id: None,
    }));
    assert_eq!(activity(&org, "atm"), answer.seq());
    let expired = org.apply(OrgEvent::DecisionExpired {
        decision_id: decision,
    });
    assert_eq!(activity(&org, "atm"), expired.seq());
    // A decision with no post moves nothing; a legacy message moves its DM.
    let unlinked = org.decision();
    org.apply(OrgEvent::DecisionExpired {
        decision_id: unlinked,
    });
    assert_eq!(activity(&org, "atm"), expired.seq());
    let message = org.send("owner", "atm/lead", None);
    assert_eq!(activity(&org, "atm/lead+owner"), message.seq());
    assert_eq!(activity(&org, "atm"), expired.seq());
}

#[test]
fn project_channel_is_listed_from_its_lead_record() {
    let mut org = Org::new();
    org.apply(OrgEvent::ProjectRegistered {
        project_id: "solo".parse().unwrap(),
        root: "/solo".into(),
    });
    org.apply(OrgEvent::AgentAdopted {
        agent_id: "solo/worker".parse().unwrap(),
        pane: "%9".into(),
        session_id: None,
    });
    // The owner's DM with each lead record is listed too, posts or not.
    assert_eq!(
        ids(&listed(&org.state, "owner", ListScope::All)),
        ["atm", "atm/lead+owner"],
        "solo has no lead"
    );
    org.apply(OrgEvent::AgentAdopted {
        agent_id: "solo/lead".parse().unwrap(),
        pane: "%10".into(),
        session_id: None,
    });
    org.apply(OrgEvent::AgentDown {
        agent_id: "solo/lead".parse().unwrap(),
        reason: "closed".into(),
    });
    // Records are never deleted: a down lead still lists its channel and its DM.
    assert_eq!(
        ids(&listed(&org.state, "owner", ListScope::All)),
        ["atm", "solo", "atm/lead+owner", "solo/lead+owner"]
    );
    let summaries = listed(&org.state, "owner", ListScope::All);
    let solo = find(&summaries, "solo");
    assert_eq!(
        solo.lead.map(ToString::to_string).as_deref(),
        Some("solo/lead")
    );
    assert_eq!(
        project_lead(&org.state, &"solo".parse().unwrap()).map(|a| a.status),
        Some(AgentStatus::Down)
    );
}

#[test]
fn members_former_and_lead() {
    let mut org = Org::new();
    org.apply(OrgEvent::AgentDown {
        agent_id: "atm/build-4".parse().unwrap(),
        reason: "closed".into(),
    });
    org.post(note("atm/build-2+atm/build-4", "atm/build-2"));
    let summaries = listed(&org.state, "owner", ListScope::All);
    let atm = find(&summaries, "atm");
    let members: Vec<_> = atm
        .members
        .iter()
        .map(|(a, s)| (a.to_string(), *s))
        .collect();
    assert_eq!(
        members,
        [
            ("owner".into(), None),
            ("atm/build-2".into(), Some(AgentStatus::Ready)),
            ("atm/lead".into(), Some(AgentStatus::Ready)),
        ]
    );
    let former: Vec<_> = atm.former.iter().map(ToString::to_string).collect();
    assert_eq!(former, ["atm/build-4"]);
    assert_eq!(
        atm.lead.map(ToString::to_string).as_deref(),
        Some("atm/lead")
    );
    assert_eq!(atm.project.map(ToString::to_string).as_deref(), Some("atm"));
    assert_eq!(
        channel_members(&org.state, &channel("atm")),
        [Address::Owner, addr("atm/build-2"), addr("atm/lead")]
    );
    // A direct channel's members are its two addresses, whatever their status.
    let dm = find(&summaries, "atm/build-2+atm/build-4");
    assert_eq!(dm.members.len(), 2);
    assert_eq!(dm.members.get(1).unwrap().1, Some(AgentStatus::Down));
    assert!((dm.former.is_empty(), dm.lead, dm.project) == (true, None, None));
    assert!(!dm.member, "the owner is not a party");
    assert_eq!(
        channel_members(&org.state, &channel("later:form")),
        Vec::<Address>::new()
    );
}

#[test]
fn listing_scopes() {
    let mut org = Org::new();
    org.send("system", "owner", None);
    org.send("system", "atm/lead", None);
    org.send("owner", "atm/lead", None);
    org.post(note("atm/build-2+atm/lead", "atm/lead"));
    org.post(note("atm/build-2+atm/build-4", "atm/build-2"));
    // A later channel form folds but is never listed.
    org.post(note("later:form", "owner"));
    let all = [
        "atm",
        "atm/build-2+atm/build-4",
        "atm/build-2+atm/lead",
        "atm/lead+owner",
        "atm/lead+system",
        "owner+system",
    ];
    for reader in ["owner", "atm/lead", "atm/build-2"] {
        assert_eq!(
            ids(&listed(&org.state, reader, ListScope::All)),
            all,
            "{reader}"
        );
    }
    assert_eq!(
        ids(&listed(&org.state, "owner", ListScope::Own)),
        [
            "atm",
            "atm/build-2+atm/build-4",
            "atm/build-2+atm/lead",
            "atm/lead+owner",
            "owner+system",
        ],
        "the owner's own list leaves out seats' system channels"
    );
    assert_eq!(
        ids(&listed(&org.state, "atm/lead", ListScope::Own)),
        [
            "atm",
            "atm/build-2+atm/lead",
            "atm/lead+owner",
            "atm/lead+system"
        ]
    );
    let lead = listed(&org.state, "atm/lead", ListScope::All);
    assert!(find(&lead, "atm").member);
    assert!(!find(&lead, "atm/build-2+atm/build-4").member);
    org.apply(OrgEvent::AgentDown {
        agent_id: "atm/build-4".parse().unwrap(),
        reason: "closed".into(),
    });
    assert_eq!(
        ids(&listed(&org.state, "atm/build-4", ListScope::Own)),
        ["atm/build-2+atm/build-4"],
        "a down seat is no longer a member of its project channel"
    );
}

// Break caught: a wake row (a msg.sent with `post`) is also indexed as a legacy
// post of the direct channel between its author and recipient.
#[test]
fn a_wake_row_is_never_a_post() {
    let mut org = Org::new();
    let post = org.post(waking(note("atm", "atm/lead"), &["atm/build-2", "owner"]));
    let rows = org.wake_all();
    assert_eq!(rows.len(), 2);
    let listed_ids = ids(&listed(&org.state, "owner", ListScope::All));
    assert!(
        !listed_ids.contains(&"atm/build-2+atm/lead".to_owned()),
        "{listed_ids:?}"
    );
    // The owner's DM with the lead seat is listed, but the wake row is not its post.
    let summaries = listed(&org.state, "owner", ListScope::All);
    assert_eq!(
        find(&summaries, "atm/lead+owner").last_post,
        None,
        "{listed_ids:?}"
    );
    assert_eq!(
        page(&org.state, "atm/lead+owner", query(50)),
        (vec![], false)
    );
    assert_eq!(page(&org.state, "atm", query(50)), (vec![post], false));
    let message = org.send("atm/lead", "owner", None);
    assert_eq!(
        page(&org.state, "atm/lead+owner", query(50)),
        (vec![message], false)
    );
    let summaries = listed(&org.state, "owner", ListScope::All);
    assert_eq!(
        find(&summaries, "atm/lead+owner").activity_seq,
        message.seq()
    );
    assert!(!ids(&summaries).contains(&"atm/build-2+atm/lead".to_owned()));
}
