//! Compile and fail-closed contracts, not delivery acceptance tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    message_delivery::{deliver, AttemptPhase, EffectCertainty},
    message_evidence::{AttemptScope, MessageEvidence},
    message_history::{
        DeliverySnapshot, HistoryUpdate, MessageHistoryHandle, PanePresence, RecipientObservation,
        SessionObservation,
    },
    messages::{MessageClock, MessageError, MessageRuntime},
    OrgHandle,
};
use atm_core::{Harness, LifecycleEvent, ScreenFacts, SessionId, SessionStatus};
use atm_org::{delivery::DeliveryStep, DeliveryReceipt, MessageId};
use chrono::{DateTime, TimeDelta, Utc};
use std::{collections::HashSet, time::Duration};
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
        } = snapshot;
        (message, recipient, live, last_attempt_seq, blocked_by)
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
        HistoryUpdate::OwnerRetry {
            id,
            attempt_seq: attempt.attempt_seq,
        },
    ];
    for update in updates {
        unavailable(history.record(update, utc()), "A1/A2 pending");
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
        MessageEvidence::disabled(clock.clone(), "A2 pending"),
        MessageHistoryHandle::disabled("A1/A2 pending"),
        Default::default(),
        clock,
    );
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
    task.await.unwrap();
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
