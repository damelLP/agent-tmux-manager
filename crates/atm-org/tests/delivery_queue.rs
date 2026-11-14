#![allow(clippy::unwrap_used, clippy::expect_used)] // synthetic histories only
mod support;
use atm_core::SessionStatus;
use atm_org::delivery::*;
use atm_org::*;
use chrono::{DateTime, TimeDelta, Utc};
use proptest::prelude::*;
use std::time::Duration;

fn at(seconds: i64) -> DateTime<Utc> {
    support::time() + TimeDelta::seconds(seconds)
}

#[derive(Default)]
struct Queue {
    state: OrgState,
    log: Vec<(DateTime<Utc>, OrgEvent)>,
    last_paste_at: Option<DateTime<Utc>>,
}
impl Queue {
    fn new() -> (Self, [MessageId; 2]) {
        let mut q = Self::default();
        q.command(
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: "/project".into(),
                move_root: false,
            },
            0,
        );
        q.command(
            OrgCommand::Adopt {
                agent_id: support::id(),
                pane: "%1".into(),
                session_id: Some("s1".into()),
            },
            0,
        );
        let ids = std::array::from_fn(|_| {
            q.command(
                OrgCommand::SendMessage(support::content(Address::Agent(support::id()))),
                0,
            );
            MessageId::new(q.log.len() as u64)
        });
        (q, ids)
    }
    fn command(&mut self, command: OrgCommand, now: i64) {
        let events = self.state.decide(&command, at(now)).unwrap();
        assert_eq!(events.len(), 1);
        for event in events {
            self.state
                .apply(self.log.len() as u64 + 1, at(now), &event)
                .unwrap();
            self.log.push((at(now), event));
        }
    }
    fn message(&self, id: MessageId) -> &Message {
        self.state.message(&id).unwrap()
    }
    fn recipient(&self, id: MessageId) -> &AgentId {
        match &self.message(id).content.to {
            Address::Agent(recipient) => recipient,
            Address::Owner | Address::System => panic!("queue fixture messages must target agents"),
        }
    }
    fn view(&self, id: MessageId) -> RecipientLiveView {
        let agent = self.state.agent(self.recipient(id)).unwrap();
        RecipientLiveView {
            session: Some(LiveSession {
                status: SessionStatus::Idle,
                status_since: at(-10),
                last_event_at: at(0),
            }),
            pane: if agent.pane.is_some() {
                PaneLiveness::Alive
            } else {
                PaneLiveness::Unknown
            },
            last_paste_at: self.last_paste_at,
            copy_mode: Some(false),
            dialog_open: Some(false),
            owner_focused: false,
            input_line: InputLine::EmptySince(at(-10)),
            ..RecipientLiveView::new(self.message(id), agent, self.state.messages())
        }
    }
    fn step(
        &self,
        id: MessageId,
        live: &RecipientLiveView,
        now: i64,
        policy: &DeliveryPolicy,
    ) -> DeliveryStep {
        next_delivery(self.message(id), live, at(now), policy).unwrap()
    }
    fn paste(&mut self, id: MessageId, attempt: u32, now: i64) {
        self.command(
            OrgCommand::DeliveryAttempted {
                message_id: id,
                attempt,
                pane: self
                    .state
                    .agent(self.recipient(id))
                    .unwrap()
                    .pane
                    .clone()
                    .unwrap(),
            },
            now,
        );
        self.last_paste_at = Some(at(now));
    }
    fn fail(&mut self, id: MessageId, final_failure: bool, now: i64) {
        self.command(
            OrgCommand::DeliveryFailed {
                message_id: id,
                reason: "timeout or owner give-up".into(),
                final_failure,
            },
            now,
        );
    }
    fn confirm(&mut self, id: MessageId, now: i64) {
        self.command(
            OrgCommand::Delivered {
                message_id: id,
                session_id: "s1".into(),
                confirmation: "prompt token".into(),
            },
            now,
        );
    }
    fn replay(&mut self) {
        let mut replayed = OrgState::default();
        for (index, (ts, event)) in self.log.iter().enumerate() {
            replayed.apply(index as u64 + 1, *ts, event).unwrap();
        }
        assert_eq!(replayed, self.state);
        self.state = replayed;
    }
}

#[test]
fn queued_retry_reserves_the_pane_before_another_message() {
    let (mut q, [a, b]) = Queue::new();
    let policy = DeliveryPolicy::default();
    assert_eq!(
        q.step(a, &q.view(a), 0, &policy),
        DeliveryStep::PasteNow { attempt: 1 }
    );
    q.paste(a, 1, 0);
    for now in 1..=32 {
        let mut live = q.view(a);
        live.pane = PaneLiveness::Unknown;
        live.hold_since = Some(at(1));
        assert!(matches!(
            q.step(a, &live, now, &policy),
            DeliveryStep::Wait {
                reason: WaitReason::UnknownGuards,
                ..
            }
        ));
    }
    assert_eq!(
        q.step(a, &q.view(a), 33, &policy),
        DeliveryStep::RetryAt {
            at: at(34),
            reason: FailureReason::ConfirmationTimeout
        }
    );
    q.fail(a, false, 33);
    assert!(matches!(
        q.step(b, &q.view(b), 33, &policy),
        DeliveryStep::Wait {
            reason: WaitReason::OtherDelivery,
            ..
        }
    ));
    // B never pastes, so its prompt event cannot become A's landing evidence.
    assert_eq!(
        q.step(a, &q.view(a), 34, &policy),
        DeliveryStep::PasteNow { attempt: 2 }
    );
}

#[test]
fn pending_reservation_follows_the_recipient_to_a_new_pane() {
    for blocked in [false, true] {
        let (mut q, [a, b]) = Queue::new();
        let policy = DeliveryPolicy::default();
        q.paste(a, 1, 0);
        q.fail(a, false, 3);
        q.command(
            OrgCommand::Down {
                agent_id: support::id(),
                reason: "pane lost".into(),
            },
            4,
        );
        q.command(
            OrgCommand::Adopt {
                agent_id: support::id(),
                pane: "%2".into(),
                session_id: Some("s2".into()),
            },
            5,
        );
        // Evaluate B first: it must not overtake A on the recipient's new pane.
        assert!(matches!(
            q.step(b, &q.view(b), 20, &policy),
            DeliveryStep::Wait {
                reason: WaitReason::OtherDelivery,
                ..
            }
        ));
        let mut live = q.view(a);
        live.probably_landed = blocked; // evidence from A's original session only
        if blocked {
            assert_eq!(
                q.step(a, &live, 20, &policy),
                DeliveryStep::Blocked {
                    reason: FailureReason::ProbablyDelivered
                }
            );
        } else {
            assert_eq!(
                q.step(a, &live, 20, &policy),
                DeliveryStep::PasteNow { attempt: 2 }
            );
            q.paste(a, 2, 20);
            assert_eq!(q.message(a).last_attempt.as_ref().unwrap().pane, "%2");
            assert!(matches!(
                q.step(b, &q.view(b), 30, &policy),
                DeliveryStep::Wait {
                    reason: WaitReason::OtherDelivery,
                    ..
                }
            ));
        }
    }
}

#[test]
fn reused_pane_does_not_reserve_a_different_recipient() {
    let (mut q, [a, _]) = Queue::new();
    let policy = DeliveryPolicy::default();
    q.paste(a, 1, 0);
    q.fail(a, false, 3);
    q.command(
        OrgCommand::Down {
            agent_id: support::id(),
            reason: "pane lost".into(),
        },
        4,
    );
    let other: AgentId = "p/other".parse().unwrap();
    q.command(
        OrgCommand::Adopt {
            agent_id: other.clone(),
            pane: "%1".into(),
            session_id: Some("s2".into()),
        },
        5,
    );
    q.command(
        OrgCommand::SendMessage(support::content(Address::Agent(other))),
        5,
    );
    let c = MessageId::new(q.log.len() as u64);
    assert_eq!(
        q.step(a, &q.view(a), 86400, &policy),
        DeliveryStep::Blocked {
            reason: FailureReason::AgentUnavailable
        }
    );
    assert_eq!(
        q.step(c, &q.view(c), 86400, &policy),
        DeliveryStep::PasteNow { attempt: 1 }
    );
}

#[test]
fn retry_delays_never_release_pane_ownership() {
    for slow_retry in [false, true] {
        let (mut q, [a, b]) = Queue::new();
        let policy = DeliveryPolicy::new(DeliverySettings {
            retry_delays: if slow_retry {
                vec![10, 20, 40]
            } else {
                vec![1, 2, 4]
            }
            .into_iter()
            .map(Duration::from_secs)
            .collect(),
            ..DeliverySettings::default()
        })
        .unwrap();
        q.paste(a, 1, 0);
        assert_eq!(
            q.step(a, &q.view(a), 3, &policy),
            DeliveryStep::RetryAt {
                at: at(if slow_retry { 13 } else { 4 }),
                reason: FailureReason::ConfirmationTimeout,
            }
        );
        q.fail(a, false, 3);
        // With defaults both would otherwise paste at ten; with slow delays B
        // would paste at ten before A's thirteen-second retry deadline.
        assert!(matches!(
            q.step(b, &q.view(b), 10, &policy),
            DeliveryStep::Wait {
                reason: WaitReason::OtherDelivery,
                ..
            }
        ));
        assert_eq!(
            q.step(a, &q.view(a), if slow_retry { 13 } else { 10 }, &policy),
            DeliveryStep::PasteNow { attempt: 2 }
        );
    }
}

#[test]
fn resolved_attempt_keeps_post_paste_stability() {
    for receipt in [false, true] {
        let (mut q, [a, b]) = Queue::new();
        let policy = DeliveryPolicy::default();
        q.paste(a, 1, 0);
        if receipt {
            q.confirm(a, 1);
        } else {
            q.fail(a, true, 1);
        }
        assert!(!q.view(b).other_delivery_in_flight);
        assert_eq!(
            q.step(b, &q.view(b), 9, &policy),
            DeliveryStep::Wait {
                until: at(10),
                reason: WaitReason::IdleStabilizing,
            }
        );
        assert_eq!(
            q.step(b, &q.view(b), 10, &policy),
            DeliveryStep::PasteNow { attempt: 1 }
        );
    }
}

#[test]
fn delivery_in_flight_conformance() {
    let (mut q, [a, b]) = Queue::new();
    assert!(!delivery_in_flight(q.message(a), &support::id()));
    q.paste(a, 1, 0);
    assert!(delivery_in_flight(q.message(a), &support::id()));
    assert!(!delivery_in_flight(
        q.message(a),
        &"p/other".parse().unwrap()
    ));
    assert!(!q.view(a).other_delivery_in_flight); // exclude the current message
    assert!(q.view(b).other_delivery_in_flight);
    q.fail(a, false, 3);
    assert!(delivery_in_flight(q.message(a), &support::id()));
    q.fail(a, true, 4);
    assert!(!delivery_in_flight(q.message(a), &support::id()));
    q.confirm(a, 5); // late receipt after give-up
    assert!(!delivery_in_flight(q.message(a), &support::id()));
    q.paste(b, 1, 10);
    q.confirm(b, 11);
    assert!(!delivery_in_flight(q.message(b), &support::id()));
}

#[test]
fn blocked_predecessor_keeps_the_queue_waiting() {
    for queued in [false, true] {
        for restarted in [false, true] {
            let (mut q, [a, b]) = Queue::new();
            let policy = DeliveryPolicy::default();
            q.paste(a, 1, 0);
            if queued {
                q.fail(a, false, 3);
            }
            let mut live = q.view(a);
            live.probably_landed = !restarted;
            live.last_attempt_before_start = restarted;
            assert_eq!(
                q.step(a, &live, 86400, &policy),
                DeliveryStep::Blocked {
                    reason: if restarted {
                        FailureReason::Restarted
                    } else {
                        FailureReason::ProbablyDelivered
                    },
                }
            );
            assert!(matches!(
                q.step(b, &q.view(b), 86400, &policy),
                DeliveryStep::Wait {
                    reason: WaitReason::OtherDelivery,
                    ..
                }
            ));
            // Blocked emits no event. Only receipt or explicit owner resolution
            // releases the pane; U9b's notice must list B behind A.
            if restarted {
                q.fail(a, true, 86400);
            } else {
                q.confirm(a, 86400);
            }
            assert_eq!(
                q.step(b, &q.view(b), 86400, &policy),
                DeliveryStep::PasteNow { attempt: 1 }
            );
        }
    }
}

#[test]
fn two_messages_never_overlap_unconfirmed_attempts() {
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use std::cell::Cell;
    let retried = Cell::new(0usize);
    let receipts = Cell::new(0usize);
    let restarts = Cell::new(0usize);
    let evidence = Cell::new(0usize);
    let pending_retry_vetoes = Cell::new(0usize);
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x9a0202),
        failure_persistence: None,
        ..Config::default()
    });
    let cases = (
        1i64..40,
        any::<bool>(),
        prop::collection::vec((any::<bool>(), prop::array::uniform3(0u8..100)), 100..150),
    );
    runner
        .run(&cases, |(unknown_until, slow_retry, ticks)| {
            let (mut q, ids) = Queue::new();
            let [a, b] = ids;
            let policy = DeliveryPolicy::new(DeliverySettings {
                retry_delays: if slow_retry {
                    vec![10, 20, 40]
                } else {
                    vec![1, 2, 4]
                }
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
                ..DeliverySettings::default()
            })
            .unwrap();
            q.paste(a, 1, 0);
            // Independent oracle: nonfinal failure NEVER releases the owner. This
            // does not call the predicate whose safety the property checks.
            let mut owner = Some(a);
            let mut attempts = [1u32, 0];
            let mut failure_since_paste = [false; 2];
            let mut confirmed = [false; 2];
            let mut landed = [false; 2];
            let mut restarted = [false; 2];
            let mut now = 0;
            for (reverse, flags) in ticks {
                now += 1;
                let [action, activity, recipient] = flags;
                let selected = usize::from(recipient % 2);
                if now > unknown_until + 1
                    && q.message(ids[selected]).last_attempt.is_some()
                    && matches!(
                        q.message(ids[selected]).status,
                        DeliveryStatus::Queued | DeliveryStatus::Delivering
                    )
                {
                    if activity == 0 {
                        landed[selected] = true;
                        evidence.set(evidence.get() + 1);
                    }
                    if action == 0 {
                        q.replay();
                        for i in 0..2 {
                            restarted[i] = q.message(ids[i]).last_attempt.is_some();
                            landed[i] = false; // executor loses evidence, oracle keeps ownership
                        }
                        q.last_paste_at = Some(at(now)); // recovery resets pane readiness
                        restarts.set(restarts.get() + 1);
                    } else if action == 1 {
                        q.confirm(ids[selected], now);
                        confirmed[selected] = true;
                        owner = None;
                        receipts.set(receipts.get() + 1);
                    } else if action == 2 {
                        q.fail(ids[selected], true, now);
                        owner = None;
                    }
                }
                for i in if reverse { [1, 0] } else { [0, 1] } {
                    let id = ids[i];
                    let mut live = q.view(id);
                    if now <= unknown_until {
                        live.pane = PaneLiveness::Unknown;
                        live.hold_since = Some(at(1));
                    }
                    live.probably_landed = landed[i];
                    live.last_attempt_before_start = restarted[i];
                    let step = q.step(id, &live, now, &policy);
                    if id == b
                        && q.message(a).status == DeliveryStatus::Queued
                        && q.message(a).last_attempt.is_some()
                        && matches!(
                            step,
                            DeliveryStep::Wait {
                                reason: WaitReason::OtherDelivery,
                                ..
                            }
                        )
                    {
                        pending_retry_vetoes.set(pending_retry_vetoes.get() + 1);
                    }
                    match step {
                        DeliveryStep::PasteNow { attempt } => {
                            prop_assert!(
                                owner.is_none() || owner == Some(id),
                                "another message still owns the pane"
                            );
                            prop_assert!(!confirmed[i] && !landed[i] && !restarted[i]);
                            prop_assert!(attempts[i] == 0 || failure_since_paste[i]);
                            prop_assert!(attempt > attempts[i]);
                            if attempt > 1 {
                                retried.set(retried.get() + 1);
                            }
                            q.paste(id, attempt, now);
                            owner = Some(id);
                            attempts[i] = attempt;
                            failure_since_paste[i] = false;
                        }
                        DeliveryStep::RetryAt { .. }
                            if q.message(id).status == DeliveryStatus::Delivering =>
                        {
                            prop_assert!(!landed[i] && !restarted[i]);
                            q.fail(id, false, now);
                            failure_since_paste[i] = true;
                        }
                        DeliveryStep::Fail { .. } => {
                            q.fail(id, true, now);
                            owner = None;
                        }
                        _ => {}
                    }
                    if owner == Some(a) {
                        prop_assert_eq!(
                            attempts[1],
                            0,
                            "B must not paste or generate evidence for A before A resolves"
                        );
                    }
                }
            }
            // Resolve A if necessary, then prove a never-attempted B is not starved
            // after the pane's post-paste stability window. The fresh guards are safe.
            if matches!(
                q.message(a).status,
                DeliveryStatus::Queued | DeliveryStatus::Delivering
            ) {
                q.fail(a, true, now);
            }
            if attempts[1] == 0 {
                prop_assert_eq!(
                    q.step(b, &q.view(b), now + 10, &policy),
                    DeliveryStep::PasteNow { attempt: 1 }
                );
                q.paste(b, 1, now + 10);
            }
            q.replay();
            Ok(())
        })
        .unwrap();
    println!(
        "pending-retry veto coverage: {}",
        pending_retry_vetoes.get()
    );
    assert!(
        pending_retry_vetoes.get() >= 100,
        "generated histories must exercise B waiting for A's queued retry"
    );
    assert!(retried.get() > 0 && receipts.get() > 0 && restarts.get() > 0 && evidence.get() > 0);
    println!(
        "two-message coverage: retries={}, receipts={}, restarts={}, evidence={}",
        retried.get(),
        receipts.get(),
        restarts.get(),
        evidence.get()
    );
}
