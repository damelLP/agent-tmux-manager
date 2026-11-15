//! The scheduler's production observer on the executor's rig: the real
//! control actor over per-pane tmux scripts and the real agent service, so
//! the observer and the executor are tested reading the same screens.
//!
//! A child of the executor's test module, for its rig and nothing else.
//!
//! Time is paused and only in-memory I/O runs under it; tests synchronize on
//! returned calls and on the executor's own entry and return.

use super::*;
use crate::org::{
    agents::Recipient,
    message_history::PanePresence,
    message_scheduler::{observed, Effects},
};
use std::sync::OnceLock;

/// Real pi frames. Each fixture's `Captured:` line is in the adapter's README.
const RUNNING: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_C2-steer-one-held-120x40_1.5.0.txt"
);
const UNREADABLE: &str = include_str!(
    "../../../atm-pi-adapter/tests/fixtures/screen_styled_p1_R-state-001-120x40_1.5.0.txt"
);

/// One call of the production observer for the worker.
async fn observe(rig: &Rig) -> Result<RecipientObservation, MessageError> {
    let state = guarded(rig.org.snapshot()).await.unwrap();
    let agent = state.agent(&worker()).cloned().unwrap();
    guarded((Effects::production(&rig.rt).observe)(agent)).await
}

/// What the agent service answers for the worker: Ready, bound to `s1`,
/// which is the one eligible row on %1.
async fn recipient(rig: &Rig) -> Recipient {
    guarded(rig.rt.agents.recipient(&worker())).await.unwrap()
}

fn turn(observation: &RecipientObservation) -> TurnState {
    observation.facts.turn
}

#[tokio::test(start_paused = true)]
async fn each_call_reads_one_new_capture_and_keeps_nothing() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(RUNNING), pi(IDLE), pi(UNREADABLE)]);
    let mut turns = Vec::new();
    for left in [6, 3, 0] {
        let seen = observe(&rig).await.unwrap();
        // The identity is the recipient's on every call.
        assert_eq!(seen.pane, Some(pane()));
        let row = seen.session.as_ref().unwrap();
        assert_eq!((row.id.as_str(), row.harness), ("s1", Harness::Pi));
        assert_eq!(seen.presence, PanePresence::Alive);
        assert_eq!(seen.focused, Some(false));
        assert_eq!(rig.reads(), left, "one capture per call");
        turns.push(turn(&seen));
    }
    // Running only on a running-turn read, NotRunning only on an idle one.
    assert_eq!(
        turns,
        [
            TurnState::Running,
            TurnState::NotRunning,
            TurnState::Unknown
        ]
    );
    // Nothing was kept: with no frame to read there is no observation.
    assert!(observe(&rig).await.is_err());
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn focus_is_the_captures_sample_and_never_a_guess() {
    let rig = Rig::attempt().await;
    for focus in [None, Some(true), Some(false)] {
        rig.script(&[with(pi(IDLE), |capture| capture.focused = focus)]);
        assert_eq!(observe(&rig).await.unwrap().focused, focus);
    }
}

#[tokio::test(start_paused = true)]
async fn every_failure_is_an_error() {
    // A pane tagged for someone else.
    let mut rig = Rig::attempt().await;
    rig.script(&[with(pi(IDLE), |capture| {
        capture.inspection.local_agent_tag = Some("demo/other".into());
    })]);
    let foreign = observe(&rig).await.unwrap_err();
    assert!(matches!(foreign, MessageError::Conflict(_)), "{foreign:?}");
    // A harness the agent service has not established (a start, an adopt,
    // the pass after a daemon restart).
    (rig.rt.agents).record_baseline(worker(), "%1", PaneBaseline::adopted(100));
    rig.script(&[pi(IDLE)]);
    let unpinned = observe(&rig).await.unwrap_err();
    assert!(matches!(unpinned, MessageError::Refused(_)), "{unpinned:?}");
    // A registry that cannot answer: never "no session".
    rig.registry_stop.cancel();
    guarded(rig.registry_tasks.take().unwrap().actor)
        .await
        .unwrap();
    rig.script(&[pi(IDLE)]);
    let registry = observe(&rig).await.unwrap_err();
    assert!(
        matches!(registry, MessageError::Unavailable(_)),
        "{registry:?}"
    );
    // The frame scripted for it was not read: no capture without a recipient.
    assert_eq!(rig.reads(), 3);
}

#[tokio::test(start_paused = true)]
async fn only_a_ready_agent_passes_the_executors_gate() {
    let rig = Rig::attempt().await;
    let bound = recipient(&rig).await;
    let row = bound.session.clone();
    let frame = pi(RUNNING);
    let see = |recipient: &Recipient| observed(&worker(), recipient, &frame);
    let binding = Some(SessionId::new("s1"));
    for (recorded, eligible, attempted) in [
        (binding.clone(), row.clone(), true),
        // The row is missing for one poll, or the binding is not made yet.
        (binding.clone(), None, false),
        (None, row.clone(), false),
        // A lazy first prompt: no binding, and no row to contradict it.
        (None, None, true),
    ] {
        let mut recipient = bound.clone();
        (recipient.session_id, recipient.session) = (recorded, eligible.clone());
        // Ready: the executor's own answer, so the scheduler never asks for
        // an attempt the gate would refuse under the reservation.
        recipient.status = AgentStatus::Ready;
        let gate = attempt_session(
            recipient.status,
            recipient.session_id.as_ref(),
            recipient.session.as_ref().map(|row| &row.id),
        );
        assert_eq!(gate.is_ok(), attempted);
        assert_eq!(see(&recipient).is_ok(), attempted);
        // Starting or clearing: no attempt is possible, and the turn is
        // observed whatever the binding says.
        for status in [AgentStatus::Starting, AgentStatus::Clearing] {
            recipient.status = status;
            let seen = see(&recipient).unwrap();
            assert_eq!(turn(&seen), TurnState::Running);
            assert_eq!(
                seen.session.map(|row| row.id),
                eligible.as_ref().map(|row| row.id.clone())
            );
        }
    }
    // The gate is the only thing a status changes: these refuse for any.
    for status in [AgentStatus::Ready, AgentStatus::Starting] {
        let mut recipient = bound.clone();
        recipient.status = status;
        assert!(observed(&"demo/other".parse().unwrap(), &recipient, &frame).is_err());
        recipient.baseline = PaneBaseline::unproven();
        assert!(see(&recipient).is_err());
    }
}

/// The scheduler's own job, on the production observer and the production
/// executor: it observes the worker on %1 and asks for an attempt there; the
/// agent moves to %2 before the executor gets its reservation.
#[tokio::test(start_paused = true)]
async fn pane_changed_between_the_observation_and_the_reservation_gets_no_input() {
    // The production effects need the rig's runtime, which exists only once
    // the rig does.
    let production: Arc<OnceLock<Effects>> = Arc::default();
    let (enter, mut entered) = mpsc::unbounded_channel();
    let (leave, mut returned) = mpsc::unbounded_channel();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| {
                let deliver = production.get().unwrap().deliver.clone();
                let (enter, leave) = (enter.clone(), leave.clone());
                Box::pin(async move {
                    let _ = enter.send(id);
                    let result = deliver(rt, id, deadline).await;
                    let _ = leave.send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                    result
                })
            }
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent| (production.get().unwrap().observe)(agent)
        }),
    };
    let mut rig = Rig::start(
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        Some(effects),
    )
    .await;
    assert!(production.set(Effects::production(&rig.rt)).is_ok());
    while rig.committed.try_recv().is_ok() {}
    // Another reservation holds the worker and %1; observing needs none.
    let parked = rig.park().await;
    rig.script(&vec![pi(IDLE); 600]);
    let id = MessageId::new(rig.commit(Rig::message(worker())).await);
    let started = Instant::now();
    let hang = Duration::from_secs(600);
    // The job saw a ready pane for the stable time and asked for an attempt.
    assert_eq!(
        timeout_at(started + hang, entered.recv()).await,
        Ok(Some(id))
    );
    assert!(rig.reads() < 600 * 3, "the observer read %1");

    for command in [
        OrgCommand::Respawn {
            agent_id: worker(),
            role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap(),
            reason: "moved".into(),
        },
        OrgCommand::PaneCreated {
            agent_id: worker(),
            pane: "%2".into(),
        },
    ] {
        rig.commit(command).await;
    }
    parked.send(()).unwrap();

    let result = timeout_at(started + hang, returned.recv()).await.unwrap();
    let refused = result.unwrap().unwrap_err();
    assert!(refused.contains("no longer on %1"), "{refused}");
    // Nothing was typed anywhere, and no attempt was spent.
    assert_eq!(rig.pastes(), 0);
    guarded(rig.org.snapshot()).await.unwrap();
    while let Ok(envelope) = rig.committed.try_recv() {
        assert_ne!(envelope.kind, "msg.delivery_attempted");
    }
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.message.last_attempt, None);
}
