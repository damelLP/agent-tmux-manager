//! Compile and fail-closed contracts, not delivery acceptance tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    message_delivery::{deliver, inbox_reference, AttemptPhase},
    message_evidence::{AttemptScope, MessageEvidence},
    message_history::{
        DeliverySnapshot, EffectCertainty, ExecutorHold, HistoryUpdate, MessageHistoryHandle,
        PanePresence, RecipientObservation, SessionObservation,
    },
    messages::{focus_notice, is_focus_notice, MessageClock, MessageError, MessageRuntime},
    OrgHandle,
};
use atm_core::{Harness, LifecycleEvent, ScreenFacts, SessionId, SessionStatus};
use atm_org::{delivery::DeliveryStep, DeliveryReceipt, MessageId};
use chrono::{DateTime, TimeDelta, Utc};
use std::{collections::HashSet, future::Future, task::Poll, time::Duration};
use tokio::time::Instant;

fn utc() -> DateTime<Utc> {
    "2026-11-14T12:00:00Z".parse().unwrap()
}

fn scope() -> AttemptScope {
    AttemptScope {
        message_id: MessageId::new(7),
        attempt_seq: 8,
        recipient: "p/worker".parse().unwrap(),
        pane: crate::control::reservation::PaneId::parse("%1").unwrap(),
        harness: Harness::Pi,
        session_id: Some(SessionId::new("session")),
        excluded_sessions: HashSet::from([SessionId::new("old-session")]),
    }
}

fn unavailable<T>(result: Result<T, MessageError>, cause: &str) {
    assert!(matches!(result, Err(MessageError::Unavailable(reason)) if reason == cause));
}

#[tokio::test(start_paused = true)]
async fn message_clock_now_follows_paused_time_and_clones_share_origin() {
    let instant = Instant::now();
    let clock = MessageClock::new(instant, utc());
    let cloned = clock.clone();
    assert_eq!(clock.now().unwrap(), utc());
    tokio::time::advance(Duration::from_millis(1250)).await;
    assert_eq!(clock.now().unwrap(), utc() + TimeDelta::milliseconds(1250));
    assert_eq!(cloned.now().unwrap(), utc() + TimeDelta::milliseconds(1250));
    assert_eq!(clock.at(instant).unwrap(), utc());
}

#[tokio::test(start_paused = true)]
async fn message_clock_rejects_before_origin_without_saturating() {
    let instant = Instant::now();
    let clock = MessageClock::new(instant + Duration::from_secs(1), utc());
    assert!(matches!(clock.now(), Err(MessageError::Internal(_))));
    assert!(matches!(clock.at(instant), Err(MessageError::Internal(_))));
}

#[tokio::test(start_paused = true)]
async fn message_clock_rejects_utc_overflow() {
    let instant = Instant::now();
    let clock = MessageClock::new(instant, DateTime::<Utc>::MAX_UTC);
    assert_eq!(clock.at(instant).unwrap(), DateTime::<Utc>::MAX_UTC);
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert!(matches!(clock.now(), Err(MessageError::Internal(_))));
}

#[test]
fn message_history_seam_never_reports_a_snapshot_or_successful_record() {
    let history = MessageHistoryHandle::disabled("A1/A2 pending");
    let attempt = scope();
    let id = attempt.message_id;
    // Destructure every owned field: consumers must not depend on an actor guard.
    let snapshot = history.snapshot(id, utc()).map(|snapshot| {
        let DeliverySnapshot {
            message,
            recipient,
            live,
            last_attempt_seq,
            blocked_by,
            hold,
        } = snapshot;
        (message, recipient, live, last_attempt_seq, blocked_by, hold)
    });
    unavailable(snapshot, "A1/A2 pending");
    let observation = RecipientObservation {
        pane: Some(attempt.pane.clone()),
        session: Some(SessionObservation {
            id: SessionId::new("session"),
            harness: Harness::Pi,
            status: SessionStatus::Working,
            last_event_at: Some(utc()),
        }),
        presence: PanePresence::Alive,
        facts: ScreenFacts::default(),
        copy_mode: None,
        focused: None,
    };
    let updates = [
        HistoryUpdate::Applied {
            seq: 1,
            at: utc(),
            event: None,
        },
        HistoryUpdate::Observed {
            recipient: attempt.recipient.clone(),
            observation,
        },
        HistoryUpdate::ObservationLost {
            recipient: attempt.recipient.clone(),
        },
        HistoryUpdate::Activity {
            at: utc(),
            session_id: SessionId::new("session"),
            pane: Some(attempt.pane.clone()),
        },
        HistoryUpdate::Armed {
            scope: attempt.clone(),
        },
        HistoryUpdate::Evidence {
            id,
            receipt: None,
            probably_landed: true,
        },
        HistoryUpdate::Consumed {
            id,
            step: DeliveryStep::PasteNow { attempt: 1 },
        },
        HistoryUpdate::Paste {
            id,
            pane: attempt.pane.clone(),
            certainty: EffectCertainty::Unknown,
        },
        HistoryUpdate::Held {
            id,
            attempt_seq: attempt.attempt_seq,
            hold: ExecutorHold::InputOutcomeUnknown,
        },
        HistoryUpdate::OwnerRetry {
            id,
            attempt_seq: attempt.attempt_seq,
        },
    ];
    for update in updates {
        unavailable(history.record(update, utc()), "A1/A2 pending");
    }
    for hold in [
        ExecutorHold::StagedMismatch,
        ExecutorHold::InputOutcomeUnknown,
        ExecutorHold::TokenVisible,
    ] {
        unavailable(
            history.record(
                HistoryUpdate::Held {
                    id,
                    attempt_seq: 8,
                    hold,
                },
                utc(),
            ),
            "A1/A2 pending",
        );
    }
    // All raw liveness facts and effect/phase names are part of the compiled seam.
    for presence in [
        PanePresence::Alive,
        PanePresence::Gone,
        PanePresence::Unknown,
    ] {
        let _ = presence;
    }
    for certainty in [
        EffectCertainty::NotStarted,
        EffectCertainty::Applied,
        EffectCertainty::Unknown,
    ] {
        unavailable(
            history.record(
                HistoryUpdate::Paste {
                    id,
                    pane: attempt.pane.clone(),
                    certainty,
                },
                utc(),
            ),
            "A1/A2 pending",
        );
    }
}

#[tokio::test(start_paused = true)]
async fn message_evidence_seam_never_turns_unavailability_into_absent_receipt() {
    let evidence =
        MessageEvidence::disabled(MessageClock::new(Instant::now(), utc()), "A2 pending");
    let attempt = scope();
    let id = attempt.message_id;
    unavailable(
        evidence.register(id, "[m7] Read: atm inbox --id m7".into()),
        "A2 pending",
    );
    unavailable(evidence.arm(attempt.clone()), "A2 pending");
    unavailable(evidence.receipt(id), "A2 pending");
    unavailable(
        evidence
            .snapshot(id)
            .map(|view| (view.receipt, view.probably_landed)),
        "A2 pending",
    );
    unavailable(
        evidence.acknowledge(
            id,
            &DeliveryReceipt {
                session_id: SessionId::new("session"),
                confirmation: "prompt_submit_exact_reference".into(),
            },
        ),
        "A2 pending",
    );
    unavailable(
        evidence.observe(
            &SessionId::new("session"),
            Some("%1"),
            Harness::Pi,
            &LifecycleEvent::PromptSubmit {
                prompt: Some("[m7] Read: atm inbox --id m7".into()),
            },
        ),
        "A2 pending",
    );
    unavailable(
        evidence.observe_activity(&SessionId::new("session"), Some("%1")),
        "A2 pending",
    );
    unavailable(evidence.clone().receipt(id), "A2 pending");
}

#[tokio::test(start_paused = true)]
async fn message_runtime_shares_eight_admissions_and_deliver_is_unavailable() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let registry = crate::registry::spawn_registry();
    let org = OrgHandle::disabled("no writes in seam");
    let mock = atm_tmux::MockTmuxClient::new();
    let (control, task) = crate::control::service::spawn_control(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let clock = MessageClock::new(Instant::now(), utc());
    let runtime = MessageRuntime::new(
        org,
        registry,
        control,
        super::agents::AgentHandle::disabled("no agent service in the seam"),
        MessageEvidence::disabled(clock.clone(), "A2 pending"),
        MessageHistoryHandle::disabled("A1/A2 pending"),
        Default::default(),
        cancel.clone(),
    );
    assert_eq!(runtime.clock.now().unwrap(), clock.now().unwrap());
    let clone = runtime.clone();
    let permits: Vec<_> = (0..8)
        .map(|_| runtime.admissions.clone().try_acquire_owned().unwrap())
        .collect();
    assert!(clone.admissions.clone().try_acquire_owned().is_err());
    drop(permits);
    assert!(clone.admissions.clone().try_acquire_owned().is_ok());
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    unavailable(
        assert_send(deliver(
            runtime,
            MessageId::new(7),
            Instant::now() + Duration::from_secs(30),
        ))
        .await,
        "message delivery executor is not configured",
    );
    assert!(mock.calls().is_empty());
    cancel.cancel();
    assert!(clone.cancel.is_cancelled());
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn message_clock_deadline_uses_the_same_origin_and_exact_boundary() {
    let origin = Instant::now();
    let clock = MessageClock::new(origin, utc());
    assert_eq!(clock.deadline(utc()).unwrap(), origin);
    assert_eq!(
        clock.deadline(utc() - TimeDelta::seconds(1)).unwrap(),
        origin
    );
    tokio::time::advance(Duration::from_secs(4)).await;
    let sleep = tokio::time::sleep_until(
        clock
            .deadline(clock.now().unwrap() + TimeDelta::seconds(3))
            .unwrap(),
    );
    tokio::pin!(sleep);
    assert!(std::future::poll_fn(|cx| Poll::Ready(sleep.as_mut().poll(cx).is_pending())).await);
    tokio::time::advance(Duration::from_millis(2999)).await;
    assert!(std::future::poll_fn(|cx| Poll::Ready(sleep.as_mut().poll(cx).is_pending())).await);
    tokio::time::advance(Duration::from_millis(1)).await;
    sleep.await;
}

#[tokio::test(start_paused = true)]
async fn history_live_snapshot_waits_for_startup_replay_gate() {
    let live = MessageHistoryHandle::new(8, utc(), Default::default()).unwrap();
    unavailable(
        live.snapshot(MessageId::new(7), utc()),
        "message history replay is incomplete",
    );
    let history = MessageHistoryHandle::disabled("not folded");
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    unavailable(assert_send(history.folded(8)).await, "not folded");
}

#[test]
fn message_shared_handles_are_send_and_sync() {
    fn assert_shared<T: Send + Sync>() {}
    assert_shared::<MessageRuntime>();
    assert_shared::<MessageEvidence>();
    assert_shared::<MessageHistoryHandle>();
}

#[test]
fn message_reference_and_focus_notice_have_one_shared_representation() {
    assert_eq!(
        inbox_reference(MessageId::new(123)),
        "[m123] Read: atm inbox --id m123"
    );
    let pane = crate::control::reservation::PaneId::parse("%4").unwrap();
    let notice = focus_notice(MessageId::new(123), &pane);
    assert_eq!(notice.body, "m123 waiting: owner is focused on %4; delivery will resume after focus moves and the editor is safely empty");
    assert!(is_focus_notice(&notice));
    for changed in 0..11 {
        let mut other = notice.clone();
        match changed {
            0 => other.from = atm_org::Address::Owner,
            1 => other.to = atm_org::Address::Agent("p/worker".parse().unwrap()),
            2 => other.kind = atm_org::MessageKind::Report,
            3 => other.body.push(' '),
            4 => other.body = other.body.replace("%4", "not-a-pane"),
            5 => other.body = other.body.replace("m123", "not-a-message"),
            6 => other.attachments.push("file".into()),
            7 => other.task = Some("task".parse().unwrap()),
            8 => other.body = other.body.replace("m123", "m0123"),
            9 => other.body = other.body.replace("%4", "%04"),
            _ => other.from = atm_org::Address::Agent("p/worker".parse().unwrap()),
        }
        assert!(!is_focus_notice(&other));
    }
}

#[test]
fn message_attempt_phase_names_are_available_to_b() {
    // A declaration test only; no claim that these states execute yet.
    let phases = [
        AttemptPhase::Waiting,
        AttemptPhase::Reserved,
        AttemptPhase::AttemptCommitted,
        AttemptPhase::PasteDispatched,
        AttemptPhase::EnterDispatched,
        AttemptPhase::AwaitingReceipt,
        AttemptPhase::Settling,
        AttemptPhase::Released,
    ];
    assert_eq!(phases.len(), 8);
}
