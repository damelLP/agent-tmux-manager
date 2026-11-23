#![allow(clippy::unwrap_used, clippy::expect_used)] // test builders only
//! Wake rows, acks by post id and seen markers (design 6.3, 6.4, 6.5). Pure:
//! posts are decided against hand-written facts.
#[path = "support/channel.rs"]
mod channel_support;
#[path = "support/policy.rs"]
mod policy_support;
use atm_core::SessionId;
use atm_org::channel_policy::*;
use atm_org::*;
use channel_support::*;
use policy_support::*;

/// The row a post's wake to `to` must be: a copy of the post.
fn wake_row(org: &Org, post: MessageId, to: &str) -> MessageContent {
    let content = &channel_query::post(&org.state, post).unwrap().content;
    MessageContent {
        from: content.author.clone(),
        to: addr(to),
        kind: content.kind,
        body: content.body.clone(),
        attachments: content.attachments.clone(),
        task: content.task.clone(),
        delivery_notice: None,
        post: Some(post),
    }
}
fn send(row: MessageContent) -> OrgCommand {
    OrgCommand::SendMessage(row)
}
fn read(who: &str, ids: &[MessageId]) -> OrgCommand {
    OrgCommand::ReadMessages {
        who: addr(who),
        message_ids: ids.to_vec(),
    }
}
fn seen(channel_text: &str, reader: &str, through: MessageId) -> OrgCommand {
    OrgCommand::Channel(ChannelCommand::Seen {
        channel: channel(channel_text),
        reader: addr(reader),
        through,
    })
}
fn bad_wake(got: Result<Vec<OrgEvent>, Refusal>, post: MessageId) -> bool {
    matches!(got, Err(Refusal::Channel(ChannelRefusal::InvalidWake { post: p, .. })) if p == post)
}

#[test]
fn wake_row_rules() {
    let mut org = org();
    let named = org.posted(request(in_channel("atm"), "atm/lead", "@build-2 go"));
    let pending = org.posted(request(in_channel("atm"), "atm/build-4", "anyone?"));
    let row = wake_row(&org, named, "atm/build-2");

    assert_eq!(
        org.decide(&send(row.clone())),
        Ok(vec![OrgEvent::MessageSent(row.clone())]),
        "a recipient the post names"
    );
    assert!(
        bad_wake(
            org.decide(&send(wake_row(&org, named, "atm/build-4"))),
            named
        ),
        "a recipient the post does not name"
    );
    assert!(
        bad_wake(
            org.decide(&send(MessageContent {
                from: Address::Owner,
                ..row.clone()
            })),
            named
        ),
        "a wrong sender"
    );
    for (name, changed) in [
        (
            "a changed body",
            MessageContent {
                body: "other".into(),
                ..row.clone()
            },
        ),
        (
            "a changed kind",
            MessageContent {
                kind: MessageKind::Report,
                ..row.clone()
            },
        ),
        (
            "changed attachments",
            MessageContent {
                attachments: vec!["x.md".into()],
                ..row.clone()
            },
        ),
        (
            "a changed task",
            MessageContent {
                task: Some("t1".parse().unwrap()),
                ..row.clone()
            },
        ),
    ] {
        assert!(bad_wake(org.decide(&send(changed)), named), "{name}");
    }
    // A notice is valid only on a system note to the owner, so this row is the
    // wake of a system post that tags him.
    let source = org.send("atm/lead", "atm/build-2", None);
    let note = org.posted(request(in_channel("atm"), "system", "@owner heads-up"));
    assert!(
        bad_wake(
            org.decide(&send(MessageContent {
                delivery_notice: Some(DeliveryNotice {
                    source_message_id: source,
                    reason: "unobserved".into(),
                }),
                ..wake_row(&org, note, "owner")
            })),
            note
        ),
        "a delivery notice"
    );
    assert_eq!(
        org.decide(&send(MessageContent {
            post: Some(MessageId::new(999)),
            ..row.clone()
        })),
        Err(Refusal::Invariant(ApplyError::UnknownPost(MessageId::new(
            999
        )))),
        "an unknown post"
    );

    let routed_row = wake_row(&org, pending, "atm/lead");
    assert!(
        bad_wake(org.decide(&send(routed_row.clone())), pending),
        "a routed recipient before the answer"
    );
    org.run(routing(pending, &["atm/lead"], router())).unwrap();
    assert_eq!(
        org.decide(&send(routed_row.clone())),
        Ok(vec![OrgEvent::MessageSent(routed_row)]),
        "a routed recipient after the answer"
    );

    org.run(send(row.clone())).unwrap();
    assert_eq!(
        org.decide(&send(row.clone())),
        Ok(vec![]),
        "a second row for the pair"
    );
    assert_eq!(
        org.decide(&send(MessageContent {
            body: "other".into(),
            ..row
        })),
        Ok(vec![]),
        "a second, different row for the pair"
    );
}

#[test]
fn read_by_post_id() {
    let mut org = org();
    let post = org.posted(request(in_channel("atm"), "atm/lead", "@build-2 go"));
    let other = org.posted(request(in_channel("atm"), "atm/lead", "@build-4 go"));
    let rows = org.wake_all();
    let (wake, other_wake) = (rows[0], rows[1]);
    let legacy = org.send("owner", "atm/build-2", None);
    let to_owner = org.posted(request(in_channel("atm"), "atm/build-2", "@owner done"));
    let owner_row = org.wake_all()[0];

    let by_post = org.decide(&read("atm/build-2", &[post])).unwrap();
    assert_eq!(
        by_post,
        org.decide(&read("atm/build-2", &[wake])).unwrap(),
        "a post id acks the reader's wake as a row ack does"
    );
    assert_eq!(
        by_post.last(),
        Some(&OrgEvent::MessageRead {
            message_ids: vec![wake]
        }),
        "the event names the row"
    );
    assert!(
        matches!(by_post.first(), Some(OrgEvent::MessageDeliveryFailed { message_id, final_failure: true, .. }) if *message_id == wake),
        "an open wake acked by post id is cancelled"
    );
    type Row<'a> = (&'a str, OrgCommand, Result<Vec<OrgEvent>, Refusal>);
    let rows: Vec<Row> = vec![
        (
            "a post the reader has no row for",
            read("atm/build-4", &[post]),
            Err(Refusal::WrongRecipient {
                message_id: post,
                who: addr("atm/build-4"),
            }),
        ),
        (
            "a batch holding one writes nothing",
            read("atm/build-2", &[wake, other]),
            Err(Refusal::WrongRecipient {
                message_id: other,
                who: addr("atm/build-2"),
            }),
        ),
        (
            "someone else's wake id, as before",
            read("atm/build-2", &[other_wake]),
            Err(Refusal::WrongRecipient {
                message_id: other_wake,
                who: addr("atm/build-2"),
            }),
        ),
        (
            "an unknown id, as before",
            read("atm/build-2", &[MessageId::new(999)]),
            Err(Refusal::Invariant(ApplyError::UnknownMessage(
                MessageId::new(999),
            ))),
        ),
        (
            "the owner's delivered wake by post id",
            read("owner", &[to_owner]),
            Ok(vec![OrgEvent::MessageRead {
                message_ids: vec![owner_row],
            }]),
        ),
    ];
    for (name, command, want) in rows {
        assert_eq!(org.decide(&command), want, "{name}");
    }
    let legacy_ack = org.decide(&read("atm/build-2", &[legacy])).unwrap();
    assert_eq!(
        legacy_ack.last(),
        Some(&OrgEvent::MessageRead {
            message_ids: vec![legacy]
        }),
        "a legacy id, as before"
    );
}

#[test]
fn seen_rules() {
    let mut org = org();
    let legacy = org.send("atm/build-2", "owner", None);
    let untagged = org.posted(request(in_channel("atm"), "atm/build-2", "anyone?"));
    let first = org.posted(request(in_channel("atm"), "atm/lead", "@owner first"));
    let first_row = org.wake_all()[0];
    let second = org.posted(request(in_channel("atm"), "atm/build-4", "@owner second"));
    let second_row = org.wake_all()[0];
    org.run(routing(untagged, &["owner"], router())).unwrap();
    let routed_row = org.wake_all()[0];
    assert!(routed_row > second_row);
    let marker = |through, session: Option<&str>, reader: &str| {
        OrgEvent::ChannelSeen(ChannelSeen {
            channel: channel("atm"),
            reader: addr(reader),
            through,
            session_id: session.map(SessionId::new),
        })
    };
    let read_rows = |ids: &[MessageId]| OrgEvent::MessageRead {
        message_ids: ids.to_vec(),
    };

    assert_eq!(
        org.decide(&seen("atm", "owner", first)),
        Ok(vec![
            marker(first, None, "owner"),
            read_rows(&[routed_row, first_row])
        ]),
        "the first marker reads the owner's rows up to it, a routed wake among them"
    );
    org.run(seen("atm", "owner", first)).unwrap();
    assert_eq!(
        org.decide(&seen("atm", "owner", first)),
        Ok(vec![]),
        "the same marker"
    );
    assert_eq!(
        org.decide(&seen("atm", "owner", untagged)),
        Ok(vec![]),
        "a lower marker"
    );
    assert_eq!(
        org.decide(&seen("atm", "owner", second)),
        Ok(vec![
            marker(second, None, "owner"),
            read_rows(&[second_row])
        ]),
        "a higher marker reads only the rows after the old one"
    );
    org.run(seen("atm", "owner", second)).unwrap();

    let late = org.posted(request(in_channel("atm"), "atm/build-2", "later?"));
    org.run(seen("atm", "owner", late)).unwrap();
    org.run(routing(late, &["owner"], router())).unwrap();
    let late_row = org.wake_all()[0];
    assert_eq!(
        org.decide(&seen("atm", "owner", late)),
        Ok(vec![read_rows(&[late_row])]),
        "a repeat with no advance still reads a row routed after the marker moved"
    );

    let ask = org.posted(request(in_channel("atm"), "atm/lead", "@build-2 look"));
    let ask_row = org.wake_all()[0];
    assert!(org.state.message(&ask_row).unwrap().read_at.is_none());
    assert_eq!(
        org.decide(&seen("atm", "atm/build-2", ask)),
        Ok(vec![marker(ask, Some("s-build-2"), "atm/build-2")]),
        "a seat's marker leaves its unread row alone"
    );
    assert_eq!(
        org.decide(&seen("atm/build-2+owner", "owner", legacy)),
        Ok(vec![
            OrgEvent::ChannelSeen(ChannelSeen {
                channel: channel("atm/build-2+owner"),
                reader: Address::Owner,
                through: legacy,
                session_id: None,
            }),
            read_rows(&[legacy])
        ]),
        "the owner's legacy rows in a direct channel"
    );
    let rows: Vec<(&str, OrgCommand, Refusal)> = vec![
        (
            "a post outside the channel",
            seen("atm", "owner", legacy),
            refused(ChannelRefusal::NotInChannel {
                post: legacy,
                channel: channel("atm"),
            }),
        ),
        (
            "an unknown id",
            seen("atm", "owner", MessageId::new(999)),
            refused(ChannelRefusal::NotInChannel {
                post: MessageId::new(999),
                channel: channel("atm"),
            }),
        ),
        (
            "a wake id",
            seen("atm", "owner", first_row),
            refused(ChannelRefusal::NotInChannel {
                post: first_row,
                channel: channel("atm"),
            }),
        ),
        (
            "a later-form channel",
            seen("x+y+z", "owner", first),
            refused(ChannelRefusal::UnknownChannel(channel("x+y+z"))),
        ),
        (
            "an unknown reader",
            seen("atm", "atm/ghost", first),
            Refusal::Invariant(ApplyError::UnknownAgent(agent_id("atm/ghost"))),
        ),
    ];
    for (name, command, want) in rows {
        assert_eq!(org.decide(&command), Err(want), "{name}");
    }
}
