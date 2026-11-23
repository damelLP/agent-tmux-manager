#![allow(clippy::unwrap_used, clippy::expect_used)] // test generators/helpers only
//! Old logs under the channel fold: the message rows must stay exactly what the
//! fold made of them before channels existed.
//!
//! `reference_messages` is a model of the message arms of `OrgState::apply` as
//! they were before channels. It was committed and passing against that fold
//! before the channel fold existed, so later commits that keep it passing keep
//! `messages` equal to the old fold's result.
#[path = "support/decision.rs"]
mod decision_support;
#[path = "support/history.rs"]
mod history;
mod support;
use atm_org::channel_query::{channel_history, channels_for, HistoryQuery, ListScope};
use atm_org::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use std::{cell::RefCell, collections::BTreeMap, collections::BTreeSet};

/// The message rows a log produces, by the pre-channel rules.
fn reference_messages(records: &[history::Record]) -> BTreeMap<MessageId, Message> {
    let mut rows = BTreeMap::new();
    for record in records {
        let Some(event) = OrgEvent::from_parts(&record.kind, &record.body).unwrap() else {
            continue;
        };
        let ts = record.ts;
        match event {
            OrgEvent::MessageSent(content) => {
                let owner = content.to == Address::Owner;
                let id = MessageId::new(record.seq);
                rows.insert(
                    id,
                    Message {
                        id,
                        content,
                        sent_at: ts,
                        status: if owner {
                            DeliveryStatus::Delivered
                        } else {
                            DeliveryStatus::Queued
                        },
                        read_at: None,
                        last_attempt: None,
                        last_attempt_at: None,
                        receipt: None,
                        delivered_at: owner.then_some(ts),
                        failure: None,
                        failed_at: None,
                    },
                );
            }
            OrgEvent::MessageDeliveryAttempted {
                message_id,
                attempt,
                pane,
            } => {
                let row: &mut Message = rows.get_mut(&message_id).unwrap();
                row.status = DeliveryStatus::Delivering;
                row.last_attempt = Some(DeliveryAttempt { attempt, pane });
                row.last_attempt_at = Some(ts);
            }
            OrgEvent::MessageDelivered {
                message_id,
                session_id,
                confirmation,
            } => {
                let row = rows.get_mut(&message_id).unwrap();
                row.status = DeliveryStatus::Delivered;
                row.receipt = Some(DeliveryReceipt {
                    session_id,
                    confirmation,
                });
                row.delivered_at = Some(ts);
            }
            OrgEvent::MessageDeliveryFailed {
                message_id,
                reason,
                final_failure,
            } => {
                let row = rows.get_mut(&message_id).unwrap();
                row.status = if final_failure {
                    DeliveryStatus::Failed
                } else {
                    DeliveryStatus::Queued
                };
                row.failure = Some(reason);
                row.failed_at = Some(ts);
            }
            OrgEvent::MessageRead { message_ids } => {
                for id in message_ids {
                    rows.get_mut(&id).unwrap().read_at.get_or_insert(ts);
                }
            }
            _ => {}
        }
    }
    rows
}

/// Generated decided histories (the same generator as `replay.rs`), folded from
/// their records into a fresh state.
fn for_each_legacy_log(cases: u32, mut check: impl FnMut(&[history::Record], &OrgState)) {
    let mut runner = TestRunner::new(Config {
        cases,
        rng_seed: RngSeed::Fixed(0xc4a7),
        failure_persistence: None, // the fixed seed reproduces the run
        ..Config::default()
    });
    let check = RefCell::new(&mut check);
    runner
        .run(
            &prop::collection::vec(history::arb_seed(), 0..120),
            |actions| {
                let mut history = history::History::seeded();
                for input in actions {
                    let command = input.command(&history.state);
                    let _ = history.step(command.clone());
                    if input.flag {
                        let _ = history.step(command);
                    }
                    if input.value == 0 {
                        history.unknown();
                    }
                }
                let mut state = OrgState::default();
                let mut stats = ReplayStats::default();
                history::fold(&history.records, &mut state, &mut stats);
                prop_assert_eq!(&state, &history.state);
                (check.borrow_mut())(&history.records, &state);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn legacy_logs_fold_messages_as_the_reference_model() {
    let mut kinds = BTreeSet::new();
    for_each_legacy_log(256, |records, state| {
        let folded: BTreeMap<_, _> = state.messages().map(|m| (m.id, m.clone())).collect();
        assert_eq!(folded, reference_messages(records));
        kinds.extend(records.iter().map(|r| r.kind.clone()));
    });
    // Reach: every message fact the model covers occurred in the run.
    for kind in [
        "msg.sent",
        "msg.delivery_attempted",
        "msg.delivered",
        "msg.delivery_failed",
        "msg.read",
    ] {
        assert!(kinds.contains(kind), "{kind} never generated");
    }
}

#[test]
fn legacy_logs_index_each_row_once_in_its_direct_channel() {
    let (mut indexed, mut self_addressed) = (0usize, 0usize);
    for_each_legacy_log(256, |_, state| {
        let mut placed: BTreeMap<MessageId, ChannelId> = BTreeMap::new();
        for listed in channels_for(state, &Address::Owner, ListScope::All) {
            let everything = HistoryQuery {
                limit: usize::MAX,
                ..HistoryQuery::default()
            };
            let page = channel_history(state, &listed.id, &everything).unwrap();
            assert!(!page.more);
            let ids: Vec<_> = page.rows.iter().map(|row| row.post.id()).collect();
            assert!(ids.windows(2).all(|w| w[0] > w[1]), "newest first: {ids:?}");
            for id in ids {
                let earlier = placed.insert(id, listed.id.clone());
                assert_eq!(earlier, None, "{id} is in two channels");
            }
        }
        for message in state.messages() {
            let content = &message.content;
            assert_eq!(content.post, None, "a legacy log has no wakes");
            match ChannelId::direct(content.from.clone(), content.to.clone()) {
                Some(direct) => {
                    assert_eq!(placed.remove(&message.id), Some(direct));
                    indexed += 1;
                }
                None => {
                    assert!(!placed.contains_key(&message.id));
                    self_addressed += 1;
                }
            }
        }
        assert!(
            placed.is_empty(),
            "only messages are posts here: {placed:?}"
        );
    });
    // Reach: the run indexed rows, and met rows that must not be indexed.
    assert!(indexed > 0, "no message indexed");
    assert!(self_addressed > 0, "no self-addressed message generated");
}
