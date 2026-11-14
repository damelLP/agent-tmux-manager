//! Synthetic neutral observations and committed org facts; no harness payloads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{message_evidence::AttemptScope, message_history::*};
use crate::control::reservation::PaneId;
use atm_core::{DialogState, Harness, InputState, ScreenFacts, SessionStatus, TurnState};
use atm_org::{
    delivery::{
        next_delivery, DeliveryPolicy, DeliveryStep, FailureReason, InputLine, PaneLiveness,
        WaitReason,
    },
    Address, AgentId, DeliveryReceipt, MessageContent, MessageId, MessageKind, OrgEvent,
};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::HashSet;

fn at(ms: i64) -> DateTime<Utc> {
    "2026-11-14T12:00:00Z".parse::<DateTime<Utc>>().unwrap() + TimeDelta::milliseconds(ms)
}
fn agent() -> AgentId {
    "p/worker".parse().unwrap()
}
fn pane() -> PaneId {
    PaneId::parse("%1").unwrap()
}
fn content(to: AgentId) -> MessageContent {
    MessageContent {
        from: Address::Owner,
        to: Address::Agent(to),
        kind: MessageKind::Note,
        body: "brief".into(),
        attachments: vec![],
        task: None,
    }
}
fn observation(status: SessionStatus) -> RecipientObservation {
    let mut facts = ScreenFacts::default();
    facts.input = InputState::Empty;
    facts.turn = TurnState::NotRunning;
    facts.dialog = DialogState::None;
    RecipientObservation {
        pane: Some(pane()),
        session: Some(SessionObservation {
            id: "s1".into(),
            harness: Harness::Pi,
            status,
            last_event_at: Some(at(0)),
        }),
        presence: PanePresence::Alive,
        facts,
        copy_mode: Some(false),
        focused: Some(false),
    }
}

struct Fixture {
    history: MessageHistory,
    seq: u64,
    id: MessageId,
}
impl Fixture {
    fn new(high_water: u64) -> Self {
        let mut f = Self {
            history: MessageHistory::new(high_water, at(0), DeliveryPolicy::default()),
            seq: 0,
            id: MessageId::new(3),
        };
        f.apply(
            OrgEvent::ProjectRegistered {
                project_id: "p".parse().unwrap(),
                root: "/project".into(),
            },
            0,
        );
        f.apply(
            OrgEvent::AgentAdopted {
                agent_id: agent(),
                pane: "%1".into(),
                session_id: Some("s1".into()),
            },
            0,
        );
        f.apply(OrgEvent::MessageSent(content(agent())), 0);
        f
    }
    fn apply(&mut self, event: OrgEvent, ms: i64) {
        self.seq += 1;
        self.history
            .record(
                HistoryUpdate::Applied {
                    seq: self.seq,
                    at: at(ms),
                    event: Some(event),
                },
                at(ms),
            )
            .unwrap();
    }
    fn record(&mut self, update: HistoryUpdate, ms: i64) {
        self.history.record(update, at(ms)).unwrap();
    }
    fn observe(&mut self, observation: RecipientObservation, ms: i64) {
        self.record(
            HistoryUpdate::Observed {
                recipient: agent(),
                observation,
            },
            ms,
        );
    }
    fn snapshot(&self, ms: i64) -> DeliverySnapshot {
        self.history.snapshot(self.id, at(ms)).unwrap()
    }
    fn step(&self, ms: i64) -> DeliveryStep {
        let s = self.snapshot(ms);
        next_delivery(&s.message, &s.live, at(ms), &DeliveryPolicy::default()).unwrap()
    }
    fn consume(&mut self, ms: i64) -> DeliveryStep {
        let step = self.step(ms);
        self.record(
            HistoryUpdate::Consumed {
                id: self.id,
                step: step.clone(),
            },
            ms,
        );
        step
    }
    fn attempt(&mut self, ms: i64) -> AttemptScope {
        self.apply(
            OrgEvent::MessageDeliveryAttempted {
                message_id: self.id,
                attempt: 1,
                pane: "%1".into(),
            },
            ms,
        );
        AttemptScope {
            message_id: self.id,
            attempt_seq: self.seq,
            recipient: agent(),
            pane: pane(),
            harness: Harness::Pi,
            session_id: Some("s1".into()),
            excluded_sessions: HashSet::new(),
        }
    }
}
fn wait(step: DeliveryStep, expected: WaitReason) {
    assert!(
        matches!(step, DeliveryStep::Wait { reason, .. } if reason == expected),
        "{step:?}"
    );
}
fn blocked(step: DeliveryStep, expected: FailureReason) {
    assert_eq!(step, DeliveryStep::Blocked { reason: expected });
}

#[test]
fn changing_blockers_keep_hold() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    wait(f.consume(0), WaitReason::InputNotEmpty);
    obs.facts.dialog = DialogState::Permission;
    f.observe(obs, 60_000);
    wait(f.consume(60_000), WaitReason::DialogOpen);
    assert_eq!(f.snapshot(60_000).live.hold_since, Some(at(0)));
    blocked(f.step(120_000), FailureReason::DialogOpen);
}

#[test]
fn flapping_empty_is_bounded() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    for ms in [0, 30_000, 60_000, 90_000] {
        obs.facts.input = InputState::Draft;
        f.observe(obs.clone(), ms);
        wait(f.consume(ms), WaitReason::InputNotEmpty);
        obs.facts.input = InputState::Empty;
        f.observe(obs.clone(), ms + 1);
        wait(f.consume(ms + 1), WaitReason::IdleStabilizing);
    }
    obs.facts.input = InputState::Draft;
    f.observe(obs, 120_000);
    assert_eq!(f.snapshot(120_000).live.hold_since, Some(at(0)));
    blocked(f.step(120_000), FailureReason::DraftPresent);
}

#[test]
fn agent_outage_resets_hold_on_recovery() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    f.consume(0);
    f.apply(
        OrgEvent::AgentStuck {
            agent_id: agent(),
            reason: "outage".into(),
        },
        100_000,
    );
    blocked(f.consume(100_000), FailureReason::AgentUnavailable);
    f.apply(OrgEvent::AgentEditorReady { agent_id: agent() }, 200_000);
    f.observe(obs, 200_000);
    wait(f.consume(200_000), WaitReason::InputNotEmpty);
    assert_eq!(f.snapshot(200_000).live.hold_since, Some(at(200_000)));
    blocked(f.step(320_000), FailureReason::DraftPresent);
}

#[test]
fn revalidation_starts_not_ready_clock_and_keeps_observation() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    wait(f.consume(0), WaitReason::InputNotEmpty);
    obs.facts.input = InputState::Empty;
    f.observe(obs, 1_000);
    wait(f.consume(1_000), WaitReason::IdleStabilizing);
    let before = f.snapshot(5_000);
    assert_eq!(before.live.hold_since, Some(at(0)));
    assert_eq!(before.live.input_line, InputLine::EmptySince(at(1_000)));
    assert_eq!(before.live.agent_not_ready_since, None);

    f.apply(
        OrgEvent::AgentRevalidationRequested { agent_id: agent() },
        5_000,
    );
    let snapshot = f.snapshot(5_000);
    assert_eq!(snapshot.live.agent_status, atm_org::AgentStatus::Starting);
    assert_eq!(snapshot.recipient.session_id, Some("s1".into()));
    assert_eq!(snapshot.recipient.pane.as_deref(), Some("%1"));
    // Revalidation names the agent: its not-ready window starts at the event.
    assert_eq!(snapshot.live.agent_not_ready_since, Some(at(5_000)));
    // The status change resets holds, but neither pane nor session moved.
    assert_eq!(snapshot.live.hold_since, None);
    assert_eq!(snapshot.live.stale_blocker_since, None);
    assert_eq!(snapshot.live.input_line, InputLine::EmptySince(at(1_000)));
    assert_eq!(snapshot.live.pane, PaneLiveness::Alive);
    assert!(snapshot.live.session.is_some());
    wait(f.consume(6_000), WaitReason::AgentNotReady);
    wait(f.step(124_999), WaitReason::AgentNotReady);
    blocked(f.step(125_000), FailureReason::AgentNotReadyTimeout);

    f.apply(OrgEvent::AgentEditorReady { agent_id: agent() }, 200_000);
    let snapshot = f.snapshot(200_000);
    assert_eq!(snapshot.live.agent_status, atm_org::AgentStatus::Ready);
    assert_eq!(snapshot.live.agent_not_ready_since, None);
    assert_eq!(snapshot.live.input_line, InputLine::EmptySince(at(1_000)));
    assert_eq!(f.step(200_000), DeliveryStep::PasteNow { attempt: 1 });
}

#[test]
fn revalidation_leaves_an_attempt_in_flight_alone() {
    let mut f = Fixture::new(0);
    let id = f.id;
    f.observe(observation(SessionStatus::Idle), 0);
    let scope = f.attempt(20_000);
    let attempt_seq = scope.attempt_seq;
    f.record(HistoryUpdate::Armed { scope }, 20_000);
    f.record(
        HistoryUpdate::Paste {
            id,
            pane: pane(),
            certainty: EffectCertainty::Applied,
        },
        20_000,
    );
    f.record(
        HistoryUpdate::Evidence {
            id,
            receipt: None,
            probably_landed: true,
        },
        20_500,
    );
    let before = f.step(21_000);
    assert!(
        matches!(before, DeliveryStep::AwaitConfirmation { .. }),
        "{before:?}"
    );
    f.apply(
        OrgEvent::AgentRevalidationRequested { agent_id: agent() },
        21_000,
    );
    assert_eq!(f.step(21_000), before);
    let snapshot = f.snapshot(21_000);
    assert!(snapshot.live.probably_landed);
    assert_eq!(snapshot.last_attempt_seq, Some(attempt_seq));
    assert_eq!(snapshot.live.agent_not_ready_since, Some(at(21_000)));
    blocked(f.step(23_000), FailureReason::ProbablyDelivered);
}

#[test]
fn starting_clearing_preserve_clock() {
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::AgentSessionLost { agent_id: agent() }, 10_000);
    f.apply(OrgEvent::AgentClearRequested { agent_id: agent() }, 50_000);
    let mut obs = observation(SessionStatus::Idle);
    obs.session = None;
    f.observe(obs, 100_000);
    assert_eq!(
        f.snapshot(100_000).live.agent_not_ready_since,
        Some(at(10_000))
    );
    wait(f.step(129_999), WaitReason::AgentNotReady);
    blocked(f.step(130_000), FailureReason::AgentNotReadyTimeout);
}

#[test]
fn down_to_starting_gets_new_budget() {
    let mut f = Fixture::new(0);
    f.apply(
        OrgEvent::AgentDown {
            agent_id: agent(),
            reason: "down".into(),
        },
        1_000,
    );
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: agent(),
            pane: "%1".into(),
            session_id: None,
        },
        200_000,
    );
    let mut obs = observation(SessionStatus::Idle);
    obs.session = None;
    f.observe(obs, 200_000);
    assert_eq!(
        f.snapshot(200_000).live.agent_not_ready_since,
        Some(at(200_000))
    );
    wait(f.step(319_999), WaitReason::AgentNotReady);
    blocked(f.step(320_000), FailureReason::AgentNotReadyTimeout);
}

#[test]
fn stale_blocker_masking_resets_short_clock() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = Some(at(-120_000));
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    wait(f.consume(0), WaitReason::InputNotEmpty);
    obs.copy_mode = Some(true);
    f.observe(obs.clone(), 500);
    wait(f.consume(500), WaitReason::CopyMode);
    assert_eq!(f.snapshot(500).live.stale_blocker_since, None);
    obs.copy_mode = Some(false);
    f.observe(obs, 1_000);
    assert_eq!(f.snapshot(1_000).live.stale_blocker_since, Some(at(1_000)));
    assert_eq!(f.snapshot(1_000).live.hold_since, Some(at(0)));
    wait(f.step(1_749), WaitReason::InputNotEmpty);
    blocked(f.step(1_750), FailureReason::DraftPresent);
}

#[test]
fn unknown_query_not_pane_death() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.presence = PanePresence::Gone;
    f.observe(obs.clone(), 0);
    wait(f.consume(0), WaitReason::PaneGoneGrace);
    obs.presence = PanePresence::Unknown;
    f.observe(obs.clone(), 1_999);
    assert_eq!(f.snapshot(2_000).live.pane, PaneLiveness::Unknown);
    // Past the grace the pane had as Gone: an unknown sample is not its death.
    wait(f.step(2_000), WaitReason::UnknownGuards);
    obs.presence = PanePresence::Gone;
    f.observe(obs, 3_000);
    assert_eq!(
        f.snapshot(3_000).live.pane,
        PaneLiveness::GoneSince(at(3_000))
    );
    wait(f.step(4_999), WaitReason::PaneGoneGrace);
    assert_eq!(
        f.step(5_000),
        DeliveryStep::Fail {
            reason: FailureReason::PaneGone
        }
    );
}

#[test]
fn working_to_working_counts_in_receive_order() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Working), 0);
    let scope = f.attempt(10_000);
    f.record(HistoryUpdate::Armed { scope }, 10_000);
    f.record(
        HistoryUpdate::Activity {
            session_id: "s1".into(),
            pane: Some(pane()),
            at: at(-1_000),
        },
        20_000,
    );
    let live = f.snapshot(20_000).live;
    assert_eq!(live.session.as_ref().unwrap().last_event_at, at(-1_000));
    assert_eq!(live.session.unwrap().status_since, at(0));
    // Evidence's receive-order latch is copied by the slot holder, not inferred
    // from row status by history. A1 has no second evidence implementation.
    f.record(
        HistoryUpdate::Evidence {
            id: f.id,
            receipt: None,
            probably_landed: true,
        },
        20_000,
    );
    blocked(f.step(20_000), FailureReason::ProbablyDelivered);
    let mut poll = observation(SessionStatus::Working);
    poll.session.as_mut().unwrap().last_event_at = Some(at(50_000));
    f.observe(poll, 50_000);
    assert_eq!(
        f.snapshot(50_000).live.session.unwrap().last_event_at,
        at(-1_000)
    );
}

#[test]
fn clock_backwards_not_process_restart() {
    let mut f = Fixture::new(3);
    f.observe(observation(SessionStatus::Idle), 0);
    f.attempt(-10_000);
    assert!(!f.snapshot(0).live.last_attempt_before_start);
    assert!(matches!(f.step(0), DeliveryStep::RetryAt { .. }));
}

#[test]
fn queued_attempt_replay_is_restarted() {
    let mut f = Fixture::new(5);
    f.attempt(100_000);
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "timeout".into(),
            final_failure: false,
        },
        101_000,
    );
    f.observe(observation(SessionStatus::Idle), 200_000);
    assert!(f.snapshot(300_000).live.last_attempt_before_start);
    assert_eq!(f.snapshot(300_000).last_attempt_seq, Some(4));
    blocked(f.step(300_000), FailureReason::Restarted);
}

#[test]
fn retry_follows_recipient_not_reused_pane() {
    let mut f = Fixture::new(0);
    f.attempt(0);
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "retry".into(),
            final_failure: false,
        },
        3_000,
    );
    f.apply(OrgEvent::MessageSent(content(agent())), 4_000);
    let follower = MessageId::new(f.seq);
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: agent(),
            pane: "%2".into(),
            session_id: Some("s2".into()),
        },
        5_000,
    );
    let other: AgentId = "p/other".parse().unwrap();
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: other.clone(),
            pane: "%1".into(),
            session_id: Some("s3".into()),
        },
        5_000,
    );
    f.apply(OrgEvent::MessageSent(content(other.clone())), 5_000);
    let other_message = MessageId::new(f.seq);
    let mut obs = observation(SessionStatus::Idle);
    obs.pane = Some(PaneId::parse("%2").unwrap());
    obs.session.as_mut().unwrap().id = "s2".into();
    f.observe(obs, 5_000);
    let follower_view = f.history.snapshot(follower, at(20_000)).unwrap();
    assert!(follower_view.live.other_delivery_in_flight);
    assert_eq!(follower_view.blocked_by, Some(f.id));
    wait(
        next_delivery(
            &follower_view.message,
            &follower_view.live,
            at(20_000),
            &DeliveryPolicy::default(),
        )
        .unwrap(),
        WaitReason::OtherDelivery,
    );
    // The pending retry itself goes on, to the recipient's new pane.
    assert_eq!(f.snapshot(20_000).recipient.pane.as_deref(), Some("%2"));
    assert_eq!(f.snapshot(20_000).blocked_by, None);
    assert_eq!(f.step(20_000), DeliveryStep::PasteNow { attempt: 2 });
    let mut obs = observation(SessionStatus::Idle);
    obs.session.as_mut().unwrap().id = "s3".into();
    f.record(
        HistoryUpdate::Observed {
            recipient: other,
            observation: obs,
        },
        5_000,
    );
    let free = f.history.snapshot(other_message, at(20_000)).unwrap();
    assert!(!free.live.other_delivery_in_flight);
    assert_eq!(free.blocked_by, None);
    assert_eq!(
        next_delivery(
            &free.message,
            &free.live,
            at(20_000),
            &DeliveryPolicy::default()
        )
        .unwrap(),
        DeliveryStep::PasteNow { attempt: 1 }
    );
}

#[test]
fn input_turn_dialog_truth_table_and_unknown_focus() {
    for input in [InputState::Empty, InputState::Draft, InputState::Unknown] {
        for turn in [
            TurnState::NotRunning,
            TurnState::Running,
            TurnState::Unknown,
        ] {
            for dialog in [
                DialogState::None,
                DialogState::Permission,
                DialogState::Unknown,
            ] {
                let mut f = Fixture::new(0);
                let mut obs = observation(SessionStatus::Idle);
                obs.facts.input = input;
                obs.facts.turn = turn;
                obs.facts.dialog = dialog.clone();
                f.observe(obs, 0);
                let live = f.snapshot(20_000).live;
                let expected = if input == InputState::Draft {
                    InputLine::NonEmpty
                } else if input == InputState::Empty
                    && turn == TurnState::NotRunning
                    && dialog == DialogState::None
                {
                    InputLine::EmptySince(at(0))
                } else {
                    InputLine::Unknown
                };
                assert_eq!(live.input_line, expected);
                assert_eq!(
                    live.dialog_open,
                    match dialog {
                        DialogState::None => Some(false),
                        DialogState::Permission => Some(true),
                        _ => None,
                    }
                );
                assert_eq!(
                    matches!(f.step(20_000), DeliveryStep::PasteNow { .. }),
                    expected == InputLine::EmptySince(at(0))
                );
            }
        }
    }
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.focused = None;
    f.observe(obs, 0);
    wait(f.step(20_000), WaitReason::OwnerFocused);
}

#[test]
fn loss_identity_and_paste_reset_continuity_without_fabricating_landing() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Idle), 0);
    assert_eq!(f.step(10_000), DeliveryStep::PasteNow { attempt: 1 });
    f.record(
        HistoryUpdate::ObservationLost { recipient: agent() },
        10_000,
    );
    assert_eq!(f.snapshot(10_000).live.input_line, InputLine::Unknown);
    assert_eq!(f.snapshot(10_000).live.pane, PaneLiveness::Unknown);
    f.observe(observation(SessionStatus::Idle), 20_000);
    assert_eq!(
        f.snapshot(20_000).live.input_line,
        InputLine::EmptySince(at(20_000))
    );
    f.apply(
        OrgEvent::AgentSessionBound {
            agent_id: agent(),
            session_id: "s2".into(),
        },
        21_000,
    );
    let mut obs = observation(SessionStatus::Idle);
    obs.session.as_mut().unwrap().id = "s2".into();
    f.observe(obs.clone(), 22_000);
    assert_eq!(
        f.snapshot(22_000).live.session.unwrap().status_since,
        at(22_000)
    );
    let scope = f.attempt(23_000);
    f.record(HistoryUpdate::Armed { scope }, 23_000);
    let before = f.snapshot(23_000).live;
    f.record(
        HistoryUpdate::Paste {
            id: f.id,
            pane: pane(),
            certainty: EffectCertainty::NotStarted,
        },
        24_000,
    );
    assert_eq!(f.snapshot(24_000).live, before);
    for certainty in [EffectCertainty::Applied, EffectCertainty::Unknown] {
        f.record(
            HistoryUpdate::Paste {
                id: f.id,
                pane: pane(),
                certainty,
            },
            25_000,
        );
        let live = f.snapshot(25_000).live;
        assert_eq!(live.input_line, InputLine::Unknown);
        assert_eq!(live.last_paste_at, Some(at(25_000)));
        assert!(!live.probably_landed);
        f.observe(obs.clone(), 26_000);
        assert_eq!(
            f.snapshot(26_000).live.session.unwrap().status_since,
            at(26_000)
        );
    }
}

#[test]
fn post_paste_and_unknown_presence_restart_status_once_then_stabilize() {
    let mut f = Fixture::new(0);
    let obs = observation(SessionStatus::Idle);
    f.observe(obs.clone(), 0);
    f.attempt(10_000);
    f.record(
        HistoryUpdate::Paste {
            id: f.id,
            pane: pane(),
            certainty: EffectCertainty::Applied,
        },
        11_000,
    );
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "no submit".into(),
            final_failure: false,
        },
        12_000,
    );
    f.observe(obs.clone(), 13_000);
    f.observe(obs.clone(), 14_000);
    assert_eq!(
        f.snapshot(23_000).live.session.unwrap().status_since,
        at(13_000)
    );
    assert_eq!(f.step(23_000), DeliveryStep::PasteNow { attempt: 2 });
    let mut unknown = obs.clone();
    unknown.presence = PanePresence::Unknown;
    f.observe(unknown, 24_000);
    f.observe(obs, 25_000);
    assert_eq!(
        f.snapshot(35_000).live.session.unwrap().status_since,
        at(25_000)
    );
    assert_eq!(f.step(35_000), DeliveryStep::PasteNow { attempt: 2 });
}

#[test]
fn missing_provenance_and_repeated_polls_never_make_a_busy_row_stale() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = None;
    f.observe(obs.clone(), 0);
    f.observe(obs, 1_000_000);
    wait(f.step(1_000_000), WaitReason::SessionBusy);
    assert_eq!(
        f.snapshot(1_000_000).live.session.unwrap().status_since,
        at(0)
    );
    f.record(
        HistoryUpdate::Activity {
            session_id: "replacement".into(),
            pane: Some(pane()),
            at: at(-200_000),
        },
        1_000_000,
    );
    wait(f.step(1_000_000), WaitReason::SessionBusy);
}

#[test]
fn applied_duplicates_and_gaps_never_reapply_or_skip_state() {
    let mut f = Fixture::new(0);
    f.record(
        HistoryUpdate::Applied {
            seq: 3,
            at: at(0),
            event: Some(OrgEvent::MessageSent(content(agent()))),
        },
        1_000,
    );
    assert_eq!(f.history.cursor(), 3);
    assert!(f
        .history
        .record(
            HistoryUpdate::Applied {
                seq: 5,
                at: at(0),
                event: None
            },
            at(0)
        )
        .is_err());
    assert_eq!(f.history.cursor(), 3);
    f.record(
        HistoryUpdate::Applied {
            seq: 4,
            at: at(0),
            event: None,
        },
        0,
    );
    assert_eq!(f.history.cursor(), 4);
    assert!(f.history.snapshot(MessageId::new(4), at(0)).is_err());
}

#[test]
fn executor_holds_and_retry_are_attempt_scoped_and_receipt_wins() {
    for hold in [
        ExecutorHold::StagedMismatch,
        ExecutorHold::InputOutcomeUnknown,
        ExecutorHold::TokenVisible,
    ] {
        let mut f = Fixture::new(0);
        let scope = f.attempt(0);
        f.record(
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            0,
        );
        f.record(
            HistoryUpdate::Held {
                id: f.id,
                attempt_seq: scope.attempt_seq,
                hold,
            },
            1_000,
        );
        f.record(
            HistoryUpdate::Evidence {
                id: f.id,
                receipt: None,
                probably_landed: false,
            },
            2_000,
        );
        f.record(
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            2_000,
        );
        assert_eq!(f.snapshot(3_000).hold, Some(hold));
        blocked(f.step(3_000), FailureReason::ProbablyDelivered);
        assert!(f
            .history
            .record(
                HistoryUpdate::OwnerRetry {
                    id: f.id,
                    attempt_seq: scope.attempt_seq + 1
                },
                at(3_000)
            )
            .is_err());
        assert_eq!(f.snapshot(3_000).hold, Some(hold));
        let receipt = DeliveryReceipt {
            session_id: "s1".into(),
            confirmation: "exact".into(),
        };
        f.record(
            HistoryUpdate::Evidence {
                id: f.id,
                receipt: Some(receipt.clone()),
                probably_landed: false,
            },
            3_000,
        );
        f.record(
            HistoryUpdate::OwnerRetry {
                id: f.id,
                attempt_seq: scope.attempt_seq,
            },
            3_000,
        );
        assert_eq!(f.snapshot(3_000).hold, None);
        assert!(!f.snapshot(3_000).live.probably_landed);
        assert_eq!(f.snapshot(3_000).message.last_attempt.unwrap().attempt, 1);
        assert_eq!(f.step(3_000), DeliveryStep::Confirm { receipt });
    }
}

#[test]
fn snapshots_do_not_consume_steps_and_errors_leave_history_unchanged() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    wait(f.step(0), WaitReason::InputNotEmpty);
    assert_eq!(f.snapshot(1_000).live.hold_since, None);
    f.consume(1_000);
    assert_eq!(f.snapshot(2_000).live.hold_since, Some(at(1_000)));
    let before = f.snapshot(2_000).live;
    obs.session.as_mut().unwrap().status = SessionStatus::Working;
    obs.session.as_mut().unwrap().last_event_at = Some(DateTime::<Utc>::MAX_UTC);
    // A new source may supply initial provenance; an existing source's polls
    // cannot replace its activity clock. Lose observation first.
    f.record(HistoryUpdate::ObservationLost { recipient: agent() }, 2_000);
    let lost = f.snapshot(2_000).live;
    assert!(f
        .history
        .record(
            HistoryUpdate::Observed {
                recipient: agent(),
                observation: obs
            },
            at(2_000)
        )
        .is_err());
    assert_eq!(f.snapshot(2_000).live, lost);
    assert_eq!(lost.hold_since, before.hold_since);
    assert!(f
        .history
        .record(
            HistoryUpdate::Consumed {
                id: f.id,
                step: DeliveryStep::PasteNow { attempt: 99 }
            },
            at(2_000)
        )
        .is_err());
    assert_eq!(f.snapshot(2_000).live, lost);
}

#[test]
fn replay_gate_owner_messages_and_fifo_are_explicit() {
    let mut f = Fixture::new(4);
    assert!(matches!(
        f.history.snapshot(f.id, at(0)),
        Err(super::messages::MessageError::Unavailable(_))
    ));
    f.record(
        HistoryUpdate::Applied {
            seq: 4,
            at: at(0),
            event: None,
        },
        0,
    );
    assert_eq!(f.snapshot(0).live.last_paste_at, Some(at(0)));
    assert_eq!(f.snapshot(0).live.hold_since, Some(at(0)));
    f.seq = 4;
    f.apply(OrgEvent::MessageSent(content(agent())), 0);
    assert_eq!(
        f.history
            .snapshot(MessageId::new(5), at(0))
            .unwrap()
            .blocked_by,
        Some(f.id)
    );
    let mut owner = content(agent());
    owner.to = Address::Owner;
    f.apply(OrgEvent::MessageSent(owner), 0);
    assert!(matches!(
        f.history.snapshot(MessageId::new(6), at(0)),
        Err(super::messages::MessageError::Refused(_))
    ));
}

#[test]
fn matching_retry_releases_restart_but_not_attempt_budget_or_late_receipt() {
    let mut f = Fixture::new(4);
    let scope = f.attempt(0);
    assert!(f
        .history
        .record(
            HistoryUpdate::Armed {
                scope: scope.clone()
            },
            at(0)
        )
        .is_err());
    blocked(f.step(10_000), FailureReason::Restarted);
    f.record(
        HistoryUpdate::OwnerRetry {
            id: f.id,
            attempt_seq: 4,
        },
        10_000,
    );
    assert!(!f.snapshot(10_000).live.last_attempt_before_start);
    assert_eq!(f.snapshot(10_000).message.last_attempt.unwrap().attempt, 1);
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "gave up".into(),
            final_failure: true,
        },
        10_000,
    );
    assert_eq!(f.step(10_000), DeliveryStep::Complete);
    let receipt = DeliveryReceipt {
        session_id: "s1".into(),
        confirmation: "late exact".into(),
    };
    f.record(
        HistoryUpdate::Evidence {
            id: f.id,
            receipt: Some(receipt.clone()),
            probably_landed: false,
        },
        11_000,
    );
    assert_eq!(f.step(11_000), DeliveryStep::Confirm { receipt });
    f.apply(
        OrgEvent::MessageDelivered {
            message_id: f.id,
            session_id: "s1".into(),
            confirmation: "late exact".into(),
        },
        11_000,
    );
    assert_eq!(f.step(11_000), DeliveryStep::Complete);
}

#[test]
fn only_a_new_attempt_arm_releases_executor_hold() {
    let mut f = Fixture::new(0);
    let scope = f.attempt(0);
    f.record(
        HistoryUpdate::Armed {
            scope: scope.clone(),
        },
        0,
    );
    f.record(
        HistoryUpdate::Held {
            id: f.id,
            attempt_seq: 4,
            hold: ExecutorHold::TokenVisible,
        },
        1_000,
    );
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "retry".into(),
            final_failure: false,
        },
        3_000,
    );
    f.apply(
        OrgEvent::MessageDeliveryAttempted {
            message_id: f.id,
            attempt: 2,
            pane: "%1".into(),
        },
        5_000,
    );
    assert_eq!(f.snapshot(5_000).hold, Some(ExecutorHold::TokenVisible));
    let mut new_scope = scope;
    new_scope.attempt_seq = f.seq;
    f.record(HistoryUpdate::Armed { scope: new_scope }, 5_000);
    assert_eq!(f.snapshot(5_000).hold, None);
    assert!(!f.snapshot(5_000).live.probably_landed);
}

#[test]
fn metadata_preserves_clocks_and_activity_requires_current_session_and_pane() {
    let mut f = Fixture::new(0);
    let obs = observation(SessionStatus::Working);
    f.observe(obs.clone(), 0);
    f.apply(
        OrgEvent::AgentPaneCreated {
            agent_id: agent(),
            pane: "%1".into(),
        },
        50_000,
    );
    assert_eq!(f.snapshot(50_000).live.session.unwrap().status_since, at(0));
    f.record(
        HistoryUpdate::Activity {
            session_id: "s1".into(),
            pane: Some(PaneId::parse("%9").unwrap()),
            at: at(50_000),
        },
        50_000,
    );
    assert_eq!(
        f.snapshot(50_000).live.session.unwrap().last_event_at,
        at(0)
    );
    f.record(
        HistoryUpdate::Activity {
            session_id: "s1".into(),
            pane: Some(pane()),
            at: at(80_000),
        },
        90_000,
    );
    f.observe(obs, 100_000);
    assert_eq!(
        f.snapshot(100_000).live.session.unwrap().last_event_at,
        at(80_000)
    );
    wait(f.step(199_999), WaitReason::SessionBusy);
    assert_eq!(f.step(200_000), DeliveryStep::PasteNow { attempt: 1 });
}

#[test]
fn invalid_identity_scope_and_applied_reference_leave_state_unchanged() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Idle), 0);
    let before = f.snapshot(0).live;
    let mut obs = observation(SessionStatus::Idle);
    obs.pane = Some(PaneId::parse("%9").unwrap());
    assert!(f
        .history
        .record(
            HistoryUpdate::Observed {
                recipient: agent(),
                observation: obs
            },
            at(0)
        )
        .is_err());
    assert_eq!(f.snapshot(0).live, before);
    assert!(f
        .history
        .record(
            HistoryUpdate::Applied {
                seq: 4,
                at: at(0),
                event: Some(OrgEvent::AgentDown {
                    agent_id: "p/missing".parse().unwrap(),
                    reason: "invalid".into()
                })
            },
            at(0)
        )
        .is_err());
    assert_eq!(f.history.cursor(), 3);
    let mut scope = f.attempt(0);
    scope.pane = PaneId::parse("%9").unwrap();
    assert!(f
        .history
        .record(HistoryUpdate::Armed { scope }, at(0))
        .is_err());
    assert!(!f.snapshot(0).live.probably_landed);
}

#[tokio::test(start_paused = true)]
async fn handle_fold_barrier_and_clones_share_applied_progress() {
    let history = MessageHistoryHandle::new(1, at(0), DeliveryPolicy::default()).unwrap();
    let copy = history.clone();
    let wait = history.folded(1);
    tokio::pin!(wait);
    use std::{future::Future, task::Poll};
    assert!(std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx).is_pending())).await);
    copy.record(
        HistoryUpdate::Applied {
            seq: 1,
            at: at(0),
            event: None,
        },
        at(0),
    )
    .unwrap();
    wait.await.unwrap();
    history.folded(0).await.unwrap();
    history.folded(1).await.unwrap();
}

/// A Working row whose event time overflows the stale deadline: any message
/// evaluated against it fails the update.
fn overflowing_observation() -> RecipientObservation {
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = Some(DateTime::<Utc>::MAX_UTC);
    obs
}
fn try_observe(f: &mut Fixture, observation: RecipientObservation, ms: i64) -> bool {
    f.history
        .record(
            HistoryUpdate::Observed {
                recipient: agent(),
                observation,
            },
            at(ms),
        )
        .is_ok()
}

#[test]
fn observed_for_a_recipient_with_no_open_message_evaluates_nothing() {
    // Work is counted, not timed: the observation fails as soon as one message
    // is evaluated against it.
    let mut f = Fixture::new(0);
    assert!(!try_observe(&mut f, overflowing_observation(), 0));
    f.attempt(0);
    f.apply(
        OrgEvent::MessageDelivered {
            message_id: f.id,
            session_id: "s1".into(),
            confirmation: "exact".into(),
        },
        1_000,
    );
    // Its only message is delivered: nothing is evaluated.
    assert!(try_observe(&mut f, overflowing_observation(), 2_000));
    // A new message is open and is evaluated again.
    f.apply(OrgEvent::MessageSent(content(agent())), 3_000);
    let second = MessageId::new(f.seq);
    assert!(!try_observe(&mut f, overflowing_observation(), 4_000));
    // A final failure closes it; a failure that queues a retry would not.
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: second,
            reason: "gave up".into(),
            final_failure: true,
        },
        5_000,
    );
    assert!(try_observe(&mut f, overflowing_observation(), 6_000));
    f.apply(OrgEvent::MessageSent(content(agent())), 7_000);
    let third = MessageId::new(f.seq);
    f.apply(
        OrgEvent::MessageDeliveryAttempted {
            message_id: third,
            attempt: 1,
            pane: "%1".into(),
        },
        8_000,
    );
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: third,
            reason: "retry".into(),
            final_failure: false,
        },
        9_000,
    );
    assert!(!try_observe(&mut f, overflowing_observation(), 10_000));
}

#[test]
fn failed_activity_changes_no_clock_and_is_not_remembered() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Working), 0);
    let before = f.snapshot(1_000).live;
    assert!(f
        .history
        .record(
            HistoryUpdate::Activity {
                session_id: "s1".into(),
                pane: Some(pane()),
                at: DateTime::<Utc>::MAX_UTC,
            },
            at(1_000),
        )
        .is_err());
    assert_eq!(f.snapshot(1_000).live, before);
    // A new source would pick a remembered activity time up again.
    f.record(HistoryUpdate::ObservationLost { recipient: agent() }, 2_000);
    f.observe(observation(SessionStatus::Working), 3_000);
    assert_eq!(f.snapshot(3_000).live.session.unwrap().last_event_at, at(0));
}

fn activity(f: &mut Fixture, session: &str, pane: Option<PaneId>, at_ms: i64) {
    f.record(
        HistoryUpdate::Activity {
            session_id: session.into(),
            pane,
            at: at(at_ms),
        },
        at_ms,
    );
}

#[test]
fn pane_less_activity_reaches_the_recipient_session() {
    // A status line reports no pane. The observation already tied the session
    // to the recipient, and a fresher event clock can only delay a paste.
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Working), 0);
    activity(&mut f, "s1", None, 100_000);
    assert_eq!(
        f.snapshot(100_000).live.session.unwrap().last_event_at,
        at(100_000)
    );
    wait(f.step(120_000), WaitReason::SessionBusy);
    // Another session's pane-less activity is still not this recipient's.
    activity(&mut f, "other", None, 150_000);
    assert_eq!(
        f.snapshot(150_000).live.session.unwrap().last_event_at,
        at(100_000)
    );
}

#[test]
fn pane_less_activity_never_erases_the_stored_pane() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Working), 0);
    activity(&mut f, "s1", Some(pane()), 50_000);
    activity(&mut f, "s1", None, 60_000);
    // A new source finds the session's latest event time under its pane.
    f.record(
        HistoryUpdate::ObservationLost { recipient: agent() },
        70_000,
    );
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = None;
    f.observe(obs, 80_000);
    assert_eq!(
        f.snapshot(80_000).live.session.unwrap().last_event_at,
        at(60_000)
    );
    // The pane it reported is still %1: the same session seen on another pane
    // is not given that time, as for any reported pane that differs.
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: agent(),
            pane: "%2".into(),
            session_id: Some("s1".into()),
        },
        90_000,
    );
    let mut obs = observation(SessionStatus::Working);
    obs.pane = Some(PaneId::parse("%2").unwrap());
    obs.session.as_mut().unwrap().last_event_at = None;
    f.observe(obs, 95_000);
    assert_eq!(
        f.snapshot(99_000).live.session.unwrap().last_event_at,
        at(99_000)
    );
}

#[test]
fn pane_less_activity_alone_gives_a_new_source_its_event_time() {
    let mut f = Fixture::new(0);
    activity(&mut f, "s1", None, 50_000);
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = None;
    f.observe(obs, 80_000);
    assert_eq!(
        f.snapshot(80_000).live.session.unwrap().last_event_at,
        at(50_000)
    );
}

/// A Working observation with no event time of its own, so the history's
/// remembered activity (or its absence) decides the event clock.
fn observe_without_provenance(f: &mut Fixture, session: Option<&str>, ms: i64) {
    let mut obs = observation(SessionStatus::Working);
    obs.session = session.map(|id| SessionObservation {
        id: id.into(),
        harness: Harness::Pi,
        status: SessionStatus::Working,
        last_event_at: None,
    });
    f.observe(obs, ms);
}

#[test]
fn activity_of_a_session_no_recipient_uses_is_pruned() {
    let mut f = Fixture::new(0);
    activity(&mut f, "gone", Some(pane()), 0);
    // Older than stale_after (120 s) and bound to no agent: the next new
    // session's activity drops it.
    activity(&mut f, "fresh", None, 200_000);
    f.apply(
        OrgEvent::AgentSessionBound {
            agent_id: agent(),
            session_id: "gone".into(),
        },
        201_000,
    );
    observe_without_provenance(&mut f, Some("gone"), 201_000);
    // No remembered time: the row stays busy instead of going stale at once.
    assert_eq!(
        f.snapshot(400_000).live.session.unwrap().last_event_at,
        at(400_000)
    );
    wait(f.step(400_000), WaitReason::SessionBusy);
}

#[test]
fn activity_of_a_bound_or_observed_session_survives_pruning() {
    // Bound to an agent: after an observation loss its old time is what lets
    // a stuck Working row go stale again.
    let mut f = Fixture::new(0);
    activity(&mut f, "s1", Some(pane()), 0);
    activity(&mut f, "fresh", None, 300_000);
    observe_without_provenance(&mut f, Some("s1"), 300_000);
    assert_eq!(
        f.snapshot(300_000).live.session.unwrap().last_event_at,
        at(0)
    );

    // Named by the stored observation only (the agent has no session yet).
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::AgentSessionLost { agent_id: agent() }, 0);
    observe_without_provenance(&mut f, Some("s9"), 0);
    activity(&mut f, "s9", Some(pane()), 1_000);
    activity(&mut f, "fresh", None, 300_000);
    observe_without_provenance(&mut f, None, 301_000);
    observe_without_provenance(&mut f, Some("s9"), 302_000);
    assert_eq!(
        f.snapshot(302_000).live.session.unwrap().last_event_at,
        at(1_000)
    );
}

#[test]
fn paste_time_is_dropped_when_no_agent_holds_the_pane() {
    let mut f = Fixture::new(0);
    f.attempt(10_000);
    f.record(
        HistoryUpdate::Paste {
            id: f.id,
            pane: pane(),
            certainty: EffectCertainty::Applied,
        },
        11_000,
    );
    // A status change keeps the pane, and with it the paste time.
    f.apply(
        OrgEvent::AgentStuck {
            agent_id: agent(),
            reason: "stuck".into(),
        },
        12_000,
    );
    assert_eq!(f.snapshot(12_000).live.last_paste_at, Some(at(11_000)));
    // The agent leaves %1; nobody holds it.
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: agent(),
            pane: "%2".into(),
            session_id: Some("s2".into()),
        },
        20_000,
    );
    let other: AgentId = "p/other".parse().unwrap();
    f.apply(
        OrgEvent::AgentAdopted {
            agent_id: other.clone(),
            pane: "%1".into(),
            session_id: Some("s3".into()),
        },
        30_000,
    );
    f.apply(OrgEvent::MessageSent(content(other)), 30_000);
    let live = f
        .history
        .snapshot(MessageId::new(f.seq), at(30_000))
        .unwrap()
        .live;
    // Back to the recovery time, the default for a pane with no known paste.
    assert_eq!(live.last_paste_at, Some(at(0)));
}

fn continuities(f: &Fixture) -> usize {
    format!("{:?}", f.history)
        .matches("MessageContinuity {")
        .count()
}

#[test]
fn only_undelivered_agent_messages_keep_continuity() {
    let mut f = Fixture::new(0);
    assert_eq!(continuities(&f), 1);
    let mut owner = content(agent());
    owner.to = Address::Owner;
    f.apply(OrgEvent::MessageSent(owner), 0);
    assert_eq!(continuities(&f), 1);
    // A failed message keeps its continuity: a late receipt may confirm it.
    f.attempt(1_000);
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "gave up".into(),
            final_failure: true,
        },
        2_000,
    );
    assert_eq!(continuities(&f), 1);
    let receipt = DeliveryReceipt {
        session_id: "s1".into(),
        confirmation: "late exact".into(),
    };
    f.record(
        HistoryUpdate::Evidence {
            id: f.id,
            receipt: Some(receipt.clone()),
            probably_landed: false,
        },
        3_000,
    );
    assert_eq!(f.consume(3_000), DeliveryStep::Confirm { receipt });
    // Delivered is terminal: the continuity goes, the snapshot still answers.
    f.apply(
        OrgEvent::MessageDelivered {
            message_id: f.id,
            session_id: "s1".into(),
            confirmation: "late exact".into(),
        },
        4_000,
    );
    assert_eq!(continuities(&f), 0);
    assert_eq!(f.consume(4_000), DeliveryStep::Complete);
    assert_eq!(f.snapshot(4_000).last_attempt_seq, None);
    assert!(matches!(
        f.history.record(
            HistoryUpdate::Evidence {
                id: f.id,
                receipt: None,
                probably_landed: false,
            },
            at(5_000),
        ),
        Err(super::messages::MessageError::Conflict(_))
    ));
}

fn paste(f: &mut Fixture, certainty: EffectCertainty, ms: i64) {
    f.record(
        HistoryUpdate::Paste {
            id: f.id,
            pane: pane(),
            certainty,
        },
        ms,
    );
}
fn evidence_latch(f: &mut Fixture, ms: i64) {
    f.record(
        HistoryUpdate::Evidence {
            id: f.id,
            receipt: None,
            probably_landed: true,
        },
        ms,
    );
}
fn fail_for_retry(f: &mut Fixture, ms: i64) {
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "retry".into(),
            final_failure: false,
        },
        ms,
    );
}
/// A second attempt of the fixture's message, armed.
fn second_attempt(f: &mut Fixture, first: &AttemptScope, ms: i64) {
    f.apply(
        OrgEvent::MessageDeliveryAttempted {
            message_id: f.id,
            attempt: 2,
            pane: "%1".into(),
        },
        ms,
    );
    let mut scope = first.clone();
    scope.attempt_seq = f.seq;
    f.record(HistoryUpdate::Armed { scope }, ms);
}

#[test]
fn zero_effect_attempt_discounts_the_evidence_latch_in_either_order() {
    for latch_first in [true, false] {
        let mut f = Fixture::new(0);
        let scope = f.attempt(10_000);
        f.record(HistoryUpdate::Armed { scope }, 10_000);
        if latch_first {
            evidence_latch(&mut f, 11_000);
            paste(&mut f, EffectCertainty::NotStarted, 11_000);
        } else {
            paste(&mut f, EffectCertainty::NotStarted, 11_000);
            evidence_latch(&mut f, 11_000);
        }
        // Nothing of ours was typed, so the session's activity is not ours.
        assert!(!f.snapshot(11_000).live.probably_landed, "{latch_first}");
        fail_for_retry(&mut f, 12_000);
        assert!(
            matches!(f.step(12_000), DeliveryStep::RetryAt { .. }),
            "{latch_first}"
        );
        // A receipt is no latch: it still confirms.
        let receipt = DeliveryReceipt {
            session_id: "s1".into(),
            confirmation: "exact".into(),
        };
        f.record(
            HistoryUpdate::Evidence {
                id: f.id,
                receipt: Some(receipt.clone()),
                probably_landed: true,
            },
            12_000,
        );
        assert_eq!(f.step(12_000), DeliveryStep::Confirm { receipt });
    }
}

#[test]
fn an_earlier_real_paste_keeps_every_latch_in_force() {
    for certainty in [EffectCertainty::Applied, EffectCertainty::Unknown] {
        let mut f = Fixture::new(0);
        let scope = f.attempt(10_000);
        f.record(
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            10_000,
        );
        paste(&mut f, certainty, 11_000);
        fail_for_retry(&mut f, 12_000);
        // The second attempt types nothing, in either arrival order.
        for latch_first in [true, false] {
            let mut f = Fixture {
                history: f.history.clone(),
                seq: f.seq,
                id: f.id,
            };
            second_attempt(&mut f, &scope, 20_000);
            if latch_first {
                evidence_latch(&mut f, 21_000);
                paste(&mut f, EffectCertainty::NotStarted, 21_000);
            } else {
                paste(&mut f, EffectCertainty::NotStarted, 21_000);
                evidence_latch(&mut f, 21_000);
            }
            // The activity may be the first attempt's paste landing late.
            assert!(
                f.snapshot(21_000).live.probably_landed,
                "{certainty:?} {latch_first}"
            );
            fail_for_retry(&mut f, 22_000);
            blocked(f.step(22_000), FailureReason::ProbablyDelivered);
        }
    }
}

#[test]
fn a_held_or_replayed_attempt_is_never_discounted() {
    // The executor held the first attempt: its effect is uncertain.
    let mut f = Fixture::new(0);
    let scope = f.attempt(10_000);
    f.record(
        HistoryUpdate::Armed {
            scope: scope.clone(),
        },
        10_000,
    );
    f.record(
        HistoryUpdate::Held {
            id: f.id,
            attempt_seq: scope.attempt_seq,
            hold: ExecutorHold::InputOutcomeUnknown,
        },
        11_000,
    );
    fail_for_retry(&mut f, 12_000);
    second_attempt(&mut f, &scope, 20_000);
    paste(&mut f, EffectCertainty::NotStarted, 21_000);
    evidence_latch(&mut f, 21_000);
    assert!(f.snapshot(21_000).live.probably_landed);

    // The first attempt predates this process: what it typed is not known.
    let mut f = Fixture::new(4);
    let scope = f.attempt(10_000);
    f.record(
        HistoryUpdate::OwnerRetry {
            id: f.id,
            attempt_seq: scope.attempt_seq,
        },
        11_000,
    );
    fail_for_retry(&mut f, 12_000);
    second_attempt(&mut f, &scope, 20_000);
    paste(&mut f, EffectCertainty::NotStarted, 21_000);
    evidence_latch(&mut f, 21_000);
    assert!(f.snapshot(21_000).live.probably_landed);
}

#[test]
fn held_before_the_first_arm_of_its_attempt_survives_that_arm() {
    let mut f = Fixture::new(0);
    let scope = f.attempt(0);
    f.record(
        HistoryUpdate::Held {
            id: f.id,
            attempt_seq: scope.attempt_seq,
            hold: ExecutorHold::TokenVisible,
        },
        1_000,
    );
    // Only an arm for another attempt, or the owner's retry, releases a hold.
    f.record(HistoryUpdate::Armed { scope }, 2_000);
    assert_eq!(f.snapshot(2_000).hold, Some(ExecutorHold::TokenVisible));
    assert!(f.snapshot(2_000).live.probably_landed);
}

#[test]
fn landing_evidence_without_an_attempt_is_refused() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Idle), 0);
    assert!(matches!(
        f.history.record(
            HistoryUpdate::Evidence {
                id: f.id,
                receipt: None,
                probably_landed: true,
            },
            at(1_000),
        ),
        Err(super::messages::MessageError::Conflict(_))
    ));
    // Accepted, it would make the policy answer InvalidState for ever.
    assert_eq!(f.step(20_000), DeliveryStep::PasteNow { attempt: 1 });
    // "Nothing seen" is a valid copy for a message with no attempt.
    f.record(
        HistoryUpdate::Evidence {
            id: f.id,
            receipt: None,
            probably_landed: false,
        },
        1_000,
    );
}

#[test]
fn applied_status_change_resets_the_hold_without_a_consumed_step() {
    let mut f = Fixture::new(0);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 0);
    wait(f.consume(0), WaitReason::InputNotEmpty);
    // The outage comes and goes with no step consumed in between.
    f.apply(
        OrgEvent::AgentStuck {
            agent_id: agent(),
            reason: "outage".into(),
        },
        100_000,
    );
    f.apply(OrgEvent::AgentEditorReady { agent_id: agent() }, 110_000);
    f.observe(obs, 110_000);
    assert_eq!(f.snapshot(110_000).live.hold_since, None);
    wait(f.consume(110_000), WaitReason::InputNotEmpty);
    wait(f.step(229_999), WaitReason::InputNotEmpty);
    blocked(f.step(230_000), FailureReason::DraftPresent);
}

#[test]
fn only_replay_and_activity_are_accepted_before_the_watermark() {
    let mut f = Fixture::new(5);
    activity(&mut f, "s1", Some(pane()), 0);
    let scope = AttemptScope {
        message_id: f.id,
        attempt_seq: 4,
        recipient: agent(),
        pane: pane(),
        harness: Harness::Pi,
        session_id: Some("s1".into()),
        excluded_sessions: HashSet::new(),
    };
    let id = f.id;
    let live_inputs = || {
        vec![
            HistoryUpdate::Observed {
                recipient: agent(),
                observation: observation(SessionStatus::Idle),
            },
            HistoryUpdate::ObservationLost { recipient: agent() },
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            HistoryUpdate::Evidence {
                id,
                receipt: None,
                probably_landed: false,
            },
            HistoryUpdate::Consumed {
                id,
                step: DeliveryStep::Complete,
            },
            HistoryUpdate::Paste {
                id,
                pane: pane(),
                certainty: EffectCertainty::NotStarted,
            },
            HistoryUpdate::Held {
                id,
                attempt_seq: 4,
                hold: ExecutorHold::TokenVisible,
            },
            HistoryUpdate::OwnerRetry { id, attempt_seq: 4 },
        ]
    };
    for update in live_inputs() {
        let kind = format!("{update:?}");
        assert!(
            matches!(
                f.history.record(update, at(0)),
                Err(super::messages::MessageError::Unavailable(_))
            ),
            "{kind}"
        );
    }
    // The replay reaches the watermark: the same inputs are now judged on
    // their own terms, none of them as "replay is incomplete".
    f.attempt(0);
    f.record(
        HistoryUpdate::Applied {
            seq: 5,
            at: at(0),
            event: None,
        },
        0,
    );
    assert_eq!(id, f.id);
    for update in live_inputs() {
        let kind = format!("{update:?}");
        assert!(
            !matches!(
                f.history.record(update, at(0)),
                Err(super::messages::MessageError::Unavailable(_))
            ),
            "{kind}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn failed_record_leaves_the_fold_cursor_unchanged() {
    use std::{future::Future, task::Poll};
    let history = MessageHistoryHandle::new(0, at(0), DeliveryPolicy::default()).unwrap();
    let applied = |seq, event| HistoryUpdate::Applied {
        seq,
        at: at(0),
        event,
    };
    history.record(applied(1, None), at(0)).unwrap();
    // A gap, and an event the org state refuses.
    assert!(history.record(applied(3, None), at(0)).is_err());
    let invalid = OrgEvent::AgentDown {
        agent_id: "p/missing".parse().unwrap(),
        reason: "invalid".into(),
    };
    assert!(history.record(applied(2, Some(invalid)), at(0)).is_err());
    let wait = history.folded(2);
    tokio::pin!(wait);
    assert!(std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx).is_pending())).await);
    history.folded(1).await.unwrap();
    history.record(applied(2, None), at(0)).unwrap();
    wait.await.unwrap();
}

#[test]
fn veto_sees_every_other_message_in_flight() {
    // Two attempted messages for one recipient: each is the other's veto,
    // including for the one that is first in the queue.
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::MessageSent(content(agent())), 0);
    let second = MessageId::new(f.seq);
    f.attempt(1_000);
    f.apply(
        OrgEvent::MessageDeliveryAttempted {
            message_id: second,
            attempt: 1,
            pane: "%1".into(),
        },
        2_000,
    );
    for (id, other) in [(f.id, second), (second, f.id)] {
        let view = f.history.snapshot(id, at(3_000)).unwrap();
        assert!(view.live.other_delivery_in_flight, "{id}");
        assert_eq!(view.blocked_by, Some(other));
    }
    // A third, unattempted one waits behind the oldest of them.
    f.apply(OrgEvent::MessageSent(content(agent())), 3_000);
    let third = f
        .history
        .snapshot(MessageId::new(f.seq), at(3_000))
        .unwrap();
    assert!(third.live.other_delivery_in_flight);
    assert_eq!(third.blocked_by, Some(f.id));
}

#[test]
fn stale_evidence_copy_before_the_new_arm_does_not_pin_the_old_latch() {
    let mut f = Fixture::new(0);
    let scope = f.attempt(0);
    f.record(
        HistoryUpdate::Armed {
            scope: scope.clone(),
        },
        0,
    );
    evidence_latch(&mut f, 1_000);
    fail_for_retry(&mut f, 2_000);
    f.apply(
        OrgEvent::MessageDeliveryAttempted {
            message_id: f.id,
            attempt: 2,
            pane: "%1".into(),
        },
        5_000,
    );
    // The evidence store keeps its latch until it is armed again, so a copy
    // taken now still says true. It is the first attempt's latch.
    evidence_latch(&mut f, 5_000);
    assert!(f.snapshot(5_000).live.probably_landed);
    let mut new_scope = scope;
    new_scope.attempt_seq = f.seq;
    f.record(HistoryUpdate::Armed { scope: new_scope }, 5_000);
    assert!(!f.snapshot(5_000).live.probably_landed);
    // A latch copied after the arm is the new attempt's.
    evidence_latch(&mut f, 6_000);
    assert!(f.snapshot(6_000).live.probably_landed);
}

#[test]
fn applied_identity_change_drops_the_observation_at_once() {
    let mut f = Fixture::new(0);
    f.observe(observation(SessionStatus::Idle), 0);
    assert_eq!(f.step(20_000), DeliveryStep::PasteNow { attempt: 1 });
    // Another session is bound. What was observed belongs to the old one, and
    // must not grant a paste before the new one is observed.
    f.apply(
        OrgEvent::AgentSessionBound {
            agent_id: agent(),
            session_id: "s2".into(),
        },
        20_000,
    );
    let live = f.snapshot(20_000).live;
    assert_eq!(live.session, None);
    assert_eq!(live.input_line, InputLine::Unknown);
    assert_eq!(live.pane, PaneLiveness::Unknown);
    assert!(!matches!(f.step(20_000), DeliveryStep::PasteNow { .. }));
}

#[test]
fn ordinary_activity_is_never_an_error() {
    // One failed Activity write turns evidence off for every message, so only
    // a broken contract (an event time outside the clock's range) may fail.
    let try_all = |f: &mut Fixture, ms: i64| {
        for session in ["s1", "s2", "never-seen", "pending-42"] {
            for pane in [None, Some(pane()), Some(PaneId::parse("%9").unwrap())] {
                // In receive order, then with an older event time and an
                // older `now` than the writer before it.
                for (event_ms, now_ms) in [(ms, ms), (ms - 500_000, ms), (ms, ms - 500_000)] {
                    f.history
                        .record(
                            HistoryUpdate::Activity {
                                session_id: session.into(),
                                pane: pane.clone(),
                                at: at(event_ms),
                            },
                            at(now_ms),
                        )
                        .unwrap();
                }
            }
        }
    };
    // Before the replay is complete.
    let mut f = Fixture::new(9);
    try_all(&mut f, 0);

    let mut f = Fixture::new(0);
    // No observation yet; then an open, never-attempted message on a stale
    // Working row with a draft, where every clock is in use.
    try_all(&mut f, 1_000);
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = Some(at(-200_000));
    obs.facts.input = InputState::Draft;
    f.observe(obs.clone(), 2_000);
    f.consume(2_000);
    try_all(&mut f, 3_000);
    // Delivering, queued for a retry, and with a second message behind it.
    let scope = f.attempt(4_000);
    f.record(HistoryUpdate::Armed { scope }, 4_000);
    try_all(&mut f, 5_000);
    fail_for_retry(&mut f, 6_000);
    f.apply(OrgEvent::MessageSent(content(agent())), 6_000);
    try_all(&mut f, 7_000);
    // The observation lost, the agent rebound, and the agent down.
    f.record(HistoryUpdate::ObservationLost { recipient: agent() }, 8_000);
    try_all(&mut f, 9_000);
    f.apply(
        OrgEvent::AgentSessionBound {
            agent_id: agent(),
            session_id: "s2".into(),
        },
        10_000,
    );
    obs.session.as_mut().unwrap().id = "s2".into();
    f.observe(obs, 10_000);
    try_all(&mut f, 11_000);
    f.apply(
        OrgEvent::AgentDown {
            agent_id: agent(),
            reason: "down".into(),
        },
        12_000,
    );
    try_all(&mut f, 13_000);
    // Long after: old entries are pruned on the way.
    try_all(&mut f, 1_000_000);
    // The same through the shared handle, for a session it has never seen.
    let handle = MessageHistoryHandle::new(5, at(0), DeliveryPolicy::default()).unwrap();
    handle
        .record(
            HistoryUpdate::Activity {
                session_id: "never-seen".into(),
                pane: None,
                at: at(0),
            },
            at(0),
        )
        .unwrap();
}

fn step_of(f: &Fixture, id: MessageId, ms: i64) -> DeliveryStep {
    let s = f.history.snapshot(id, at(ms)).unwrap();
    next_delivery(&s.message, &s.live, at(ms), &DeliveryPolicy::default()).unwrap()
}
fn consume_of(f: &mut Fixture, id: MessageId, ms: i64) -> DeliveryStep {
    let step = step_of(f, id, ms);
    f.record(
        HistoryUpdate::Consumed {
            id,
            step: step.clone(),
        },
        ms,
    );
    step
}

#[test]
fn two_unattempted_messages_are_pasted_in_fifo_order() {
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::MessageSent(content(agent())), 0);
    let follower = MessageId::new(f.seq);
    f.observe(observation(SessionStatus::Idle), 0);
    // Both are ready at the same instant; only the head may paste.
    assert_eq!(f.step(20_000), DeliveryStep::PasteNow { attempt: 1 });
    let view = f.history.snapshot(follower, at(20_000)).unwrap();
    assert_eq!(view.blocked_by, Some(f.id));
    assert!(view.live.other_delivery_in_flight);
    wait(step_of(&f, follower, 20_000), WaitReason::OtherDelivery);
    // The history itself refuses the follower's paste: FIFO does not depend
    // on the caller also reading blocked_by.
    assert!(matches!(
        f.history.record(
            HistoryUpdate::Consumed {
                id: follower,
                step: DeliveryStep::PasteNow { attempt: 1 },
            },
            at(20_000),
        ),
        Err(super::messages::MessageError::Conflict(_))
    ));
    // The head is delivered: the follower is the head now.
    f.attempt(21_000);
    f.apply(
        OrgEvent::MessageDelivered {
            message_id: f.id,
            session_id: "s1".into(),
            confirmation: "exact".into(),
        },
        22_000,
    );
    let view = f.history.snapshot(follower, at(22_000)).unwrap();
    assert_eq!(view.blocked_by, None);
    assert!(!view.live.other_delivery_in_flight);
}

#[test]
fn follower_behind_an_unattempted_head_does_not_time_out_while_it_waits() {
    // A draft sits in the editor. The head holds for it and is blocked after
    // the readiness timeout (120 s); the follower only waits its turn.
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::MessageSent(content(agent())), 0);
    let follower = MessageId::new(f.seq);
    let mut obs = observation(SessionStatus::Idle);
    obs.facts.input = InputState::Draft;
    for ms in [0, 60_000, 119_999, 120_000, 500_000] {
        f.observe(obs.clone(), ms);
        let head = f.consume(ms);
        if ms < 120_000 {
            wait(head, WaitReason::InputNotEmpty);
        } else {
            blocked(head, FailureReason::DraftPresent);
        }
        wait(consume_of(&mut f, follower, ms), WaitReason::OtherDelivery);
        let live = f.history.snapshot(follower, at(ms)).unwrap().live;
        assert_eq!(live.hold_since, None, "{ms}");
        assert_eq!(live.stale_blocker_since, None, "{ms}");
    }
    // The owner gives the head up. The follower becomes the head, and its
    // readiness budget starts now, not when it was sent.
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "gave up".into(),
            final_failure: true,
        },
        500_000,
    );
    f.observe(obs, 500_000);
    wait(
        consume_of(&mut f, follower, 500_000),
        WaitReason::InputNotEmpty,
    );
    assert_eq!(
        f.history
            .snapshot(follower, at(500_001))
            .unwrap()
            .live
            .hold_since,
        Some(at(500_000))
    );
    wait(step_of(&f, follower, 619_999), WaitReason::InputNotEmpty);
    blocked(step_of(&f, follower, 620_000), FailureReason::DraftPresent);
}

#[test]
fn follower_stale_blocker_bound_starts_when_it_becomes_the_head() {
    // A stale Working row with a draft: the short bound is three guard polls
    // (750 ms). It runs for the head only.
    let mut f = Fixture::new(0);
    f.apply(OrgEvent::MessageSent(content(agent())), 0);
    let follower = MessageId::new(f.seq);
    let mut obs = observation(SessionStatus::Working);
    obs.session.as_mut().unwrap().last_event_at = Some(at(-120_000));
    obs.facts.input = InputState::Draft;
    for ms in [0, 750, 60_000] {
        f.observe(obs.clone(), ms);
        let head = f.consume(ms);
        if ms < 750 {
            wait(head, WaitReason::InputNotEmpty);
        } else {
            blocked(head, FailureReason::DraftPresent);
        }
        wait(consume_of(&mut f, follower, ms), WaitReason::OtherDelivery);
    }
    // An observation alone, with no step consumed after it, starts no clock
    // for the follower either.
    f.observe(obs.clone(), 90_000);
    let live = f.history.snapshot(follower, at(90_000)).unwrap().live;
    assert_eq!(live.stale_blocker_since, None);
    f.apply(
        OrgEvent::MessageDeliveryFailed {
            message_id: f.id,
            reason: "gave up".into(),
            final_failure: true,
        },
        100_000,
    );
    f.observe(obs, 100_000);
    wait(
        consume_of(&mut f, follower, 100_000),
        WaitReason::InputNotEmpty,
    );
    wait(step_of(&f, follower, 100_749), WaitReason::InputNotEmpty);
    blocked(step_of(&f, follower, 100_750), FailureReason::DraftPresent);
}
