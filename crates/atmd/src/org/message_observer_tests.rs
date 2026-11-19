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
    guarded((Effects::production(&rig.rt).observe)(agent))
        .await
        .map(|(seen, _)| seen)
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
async fn unreadable_screen_delivery_rearms_after_a_readable_busy_screen() {
    let mut rig = Rig::attempt().await;
    let diag = "atm cannot read demo/worker's screen (%1) for 120 s; pi may have changed its screen (version unknown). Delivery, readiness checks, clear, and handover need a readable screen.";
    for count in 1..=2 {
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        let since = Instant::now();
        rig.script(&[pi(UNREADABLE)]); // the deadline's read, not the scheduler's
        tokio::time::advance(Duration::from_millis(119_999)).await;
        assert_eq!(screen_notes(&rig).await.len(), count - 1);
        assert!(Instant::now() < since + Duration::from_secs(120));
        assert_eq!(rig.reads(), 3);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = rig.committed.recv().await.unwrap();
                if e.kind == "msg.sent" {
                    break;
                }
            }
        })
        .await
        .expect("the diagnostic was not committed");
        assert_eq!(screen_notes(&rig).await, vec![diag; count]);
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(screen_notes(&rig).await.len(), count);
        assert_eq!(rig.reads(), 0);
        rig.script(&[pi(RUNNING)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Running);
        assert_eq!(screen_notes(&rig).await.len(), count, "no recovery note");
    }
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_scheduler_keeps_two_messages_queued_without_duplicate_notes() {
    use crate::org::message_service_tests::{owner, page};
    use atm_comms::message_types::DeliveryPhase;

    let production: Arc<OnceLock<Effects>> = Arc::default();
    let (observed, mut observations) = mpsc::unbounded_channel();
    let effects = Effects {
        deliver: Arc::new({
            let production = production.clone();
            move |rt, id, deadline| (production.get().unwrap().deliver)(rt, id, deadline)
        }),
        observe: Arc::new({
            let production = production.clone();
            move |agent| {
                let read = (production.get().unwrap().observe)(agent);
                let observed = observed.clone();
                Box::pin(async move {
                    let result = read.await;
                    let _ = observed.send(result.is_ok());
                    result
                })
            }
        }),
    };
    let policy = DeliveryPolicy::new(DeliverySettings {
        readiness_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .unwrap();
    let rig = Rig::start(Fold::Service, true, policy, Some(effects)).await;
    assert!(production.set(Effects::production(&rig.rt)).is_ok());
    // The diagnostic is already committed before the mail's hold
    // deadline, so a blocked-readiness notice would repeat the same cause.
    rig.script(&[pi(UNREADABLE), pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let mut events = rig.org.committed.subscribe();
    let note = guarded(async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "msg.sent" {
                break event;
            }
        }
    })
    .await;
    rig.folded(note.seq).await;
    rig.script(&vec![pi(UNREADABLE); 24]);
    let first = MessageId::new(rig.commit(Rig::message(worker())).await);
    let follower = MessageId::new(rig.commit(Rig::message(worker())).await);
    assert_eq!(guarded(observations.recv()).await, Some(true));
    let inbox = || async {
        let mut params = page();
        params.recipient = Some(Address::Agent(worker()));
        rig.messages
            .as_ref()
            .unwrap()
            .inbox(params, owner())
            .await
            .unwrap()
    };
    let snapshot = guarded(async {
        loop {
            let result = inbox().await;
            if result
                .messages
                .iter()
                .any(|m| m.message_id == first && m.delivery.phase == DeliveryPhase::Blocked)
            {
                break result;
            }
            assert!(observations.recv().await.is_some());
        }
    })
    .await;
    assert!(snapshot
        .messages
        .iter()
        .all(|m| m.status == DeliveryStatus::Queued && m.delivery.attempt == 0));
    assert_eq!(
        snapshot
            .messages
            .iter()
            .find(|m| m.message_id == follower)
            .unwrap()
            .delivery
            .blocked_by,
        Some(first)
    );
    // The next successful read proves the scheduler finished evaluating
    // the blocked round before we inspect its committed notices.
    assert_eq!(guarded(observations.recv()).await, Some(true));
    assert_eq!(screen_notes(&rig).await, [
        "atm cannot read demo/worker's screen (%1) for 3 s; pi may have changed its screen (version unknown). Delivery, readiness checks, clear, and handover need a readable screen."
    ]);
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_needs_a_successful_attributable_read() {
    let rig = Rig::attempt().await;
    let tracker = rig.rt.agents.screens().unwrap();
    let agent = rig
        .org
        .snapshot()
        .await
        .unwrap()
        .agent(&worker())
        .unwrap()
        .clone();
    for frame in [
        pi(RUNNING),
        pi(DRAFT),
        with(pi(UNREADABLE), |c| c.inspection.in_mode = true),
        with(pi(UNREADABLE), |c| {
            c.inspection.foreground_command = Some("zsh".into())
        }),
        with(pi(UNREADABLE), |c| {
            c.inspection.local_agent_tag = Some("demo/other".into())
        }),
    ] {
        rig.script(&[frame]);
        let _ = observe(&rig).await;
        assert!(!tracker.active(&agent));
        assert_eq!(rig.reads(), 0);
    }
    assert!(observe(&rig).await.is_err());
    assert!(!tracker.active(&agent));
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    assert!(tracker.active(&agent));
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_deadline_does_not_report_an_old_binding() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    rig.commit(OrgCommand::SessionBound {
        agent_id: worker(),
        session_id: SessionId::new("s2"),
    })
    .await;
    rig.script(&[pi(UNREADABLE)]);
    let mut events = rig.org.committed.subscribe();
    tokio::time::advance(Duration::from_secs(120)).await;
    let diagnostic = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if events.recv().await.unwrap().kind == "msg.sent" {
                return;
            }
        }
    })
    .await;
    assert!(
        diagnostic.is_err(),
        "a previous binding's clock produced a stale note"
    );
    assert_eq!(rig.reads(), 3, "the obsolete deadline must not capture");
    assert_eq!(rig.pastes(), 0);
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_transient_retry_ends_on_loss_rebind_or_readable_frame() {
    #[derive(Clone, Copy, Debug)]
    enum End {
        PaneGone,
        Readable,
    }
    for end in [End::PaneGone, End::Readable] {
        let rig = Rig::attempt().await;
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        let mut reads = rig.read_events.clone();
        let before = reads.borrow().0;
        assert_eq!(rig.reads(), 0);
        tokio::time::advance(Duration::from_secs(120)).await;
        // Empty script: the compound capture and the metadata fallback both fail.
        let first_retry = before + 2;
        guarded(reads.wait_for(|event| event.0 >= first_retry))
            .await
            .unwrap();
        assert_eq!(reads.borrow().0, first_retry);
        let failed_at = Instant::now();
        assert!(screen_notes(&rig).await.is_empty());

        let raw_calls = match end {
            End::PaneGone => {
                rig.tmux.script_inspect(&pane(), None).unwrap();
                2 // failed compound followed by positive pane-gone metadata
            }
            End::Readable => {
                rig.script(&[pi(DRAFT)]); // readable does not mean safe for input
                1
            }
        };
        let last = first_retry + raw_calls;
        guarded(reads.wait_for(|event| event.0 >= last))
            .await
            .unwrap();
        assert!(
            Instant::now() >= failed_at + Duration::from_secs(5),
            "{end:?}"
        );
        assert_eq!(rig.reads(), 0);
        // Wait across several retry deadlines, ending on any unexpected read.
        assert!(
            tokio::time::timeout(
                Duration::from_secs(30),
                reads.wait_for(|event| event.0 > last)
            )
            .await
            .is_err(),
            "retry outlived {end:?}"
        );
        let state = rig.org.snapshot().await.unwrap();
        assert!(!rig
            .rt
            .agents
            .screens()
            .unwrap()
            .active(state.agent(&worker()).unwrap()));
        assert!(screen_notes(&rig).await.is_empty());
        assert_eq!(rig.pastes(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_real_observation_loss_still_tells_owner() {
    for previous_note in [false, true] {
        let production: Arc<OnceLock<Effects>> = Arc::default();
        let effects = Effects {
            deliver: Arc::new(|_, _, _| panic!("no usable observation authorizes input")),
            observe: Arc::new({
                let production = production.clone();
                move |agent| (production.get().unwrap().observe)(agent)
            }),
        };
        let policy = DeliveryPolicy::new(DeliverySettings {
            readiness_timeout: Duration::from_secs(3),
            ..Default::default()
        })
        .unwrap();
        let rig = Rig::start(Fold::Service, true, policy, Some(effects)).await;
        assert!(production.set(Effects::production(&rig.rt)).is_ok());
        let mut events = rig.org.committed.subscribe();
        rig.script(&[pi(UNREADABLE)]);
        assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
        if previous_note {
            rig.script(&[pi(UNREADABLE)]);
            tokio::time::advance(Duration::from_secs(3)).await;
            assert_eq!(guarded(events.recv()).await.unwrap().kind, "msg.sent");
        }
        // Both the capture and its metadata fallback now fail. That is a
        // separate problem from a successful but unreadable viewport.
        let source = MessageId::new(rig.commit(Rig::message(worker())).await);
        let owner_notice = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = events.recv().await.unwrap();
                if let Some(OrgEvent::MessageSent(content)) =
                    OrgEvent::from_parts(&event.kind, &event.body).unwrap()
                {
                    if content.delivery_notice.as_ref().is_some_and(|key| {
                        key.source_message_id == source && key.reason == "unobserved"
                    }) {
                        break content;
                    }
                }
            }
        })
        .await
        .expect("real observation loss must tell the owner even during an unreadable episode");
        assert_eq!(owner_notice.to, Address::Owner);
        assert_eq!(rig.snapshot(source).message.status, DeliveryStatus::Queued);
        assert_eq!(rig.pastes(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_failed_capture_cannot_retry_after_rebind() {
    let rig = Rig::attempt().await;
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    let mut reads = rig.read_events.clone();
    let before = reads.borrow().0;
    // The compound capture and the metadata fallback fail.
    tokio::time::advance(Duration::from_secs(120)).await;
    guarded(reads.wait_for(|event| event.0 >= before + 2))
        .await
        .unwrap();
    rig.commit(OrgCommand::SessionBound {
        agent_id: worker(),
        session_id: SessionId::new("new-session"),
    })
    .await;
    // No working frame or metadata is available to learn about the rebind.
    assert!(
        tokio::time::timeout(
            Duration::from_secs(20),
            reads.wait_for(|event| event.0 > before + 2),
        )
        .await
        .is_err(),
        "retried a failed confirmation for an obsolete binding"
    );
    assert!(screen_notes(&rig).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unreadable_screen_rebind_at_the_writer_does_not_change_delivery() {
    let mut rig = Rig::start_with_harness(
        0,
        Fold::Service,
        true,
        DeliveryPolicy::default(),
        None,
        Harness::Pi,
        true,
    )
    .await;
    let mail = MessageId::new(rig.commit(Rig::message(worker())).await);
    rig.script(&[pi(UNREADABLE)]);
    assert_eq!(turn(&observe(&rig).await.unwrap()), TurnState::Unknown);
    rig.script(&[pi(UNREADABLE)]);
    let mut events = rig.org.committed.subscribe();
    tokio::time::advance(Duration::from_secs(120)).await;
    let binding = guarded(async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "agent.session_bound" {
                break event.seq;
            }
        }
    })
    .await;
    let state = rig.org.snapshot().await.unwrap();
    assert_eq!(
        state.agent(&worker()).unwrap().session_id,
        Some(SessionId::new("new-session"))
    );
    assert_eq!(state.message(&mail).unwrap().status, DeliveryStatus::Queued);
    assert_eq!(rig.pastes(), 0);
    // Replay the journal from before the race. The description may no longer
    // be true when the event arrives, so replay must not depend on it.
    let mut replayed = OrgState::default();
    let mut stats = atm_org::ReplayStats::default();
    while let Ok(event) = rig.committed.try_recv() {
        stats
            .apply_parts(&mut replayed, event.seq, event.ts, &event.kind, &event.body)
            .unwrap();
    }
    assert_eq!(stats.skipped_unknown, 0);
    assert_eq!(
        replayed.agent(&worker()).unwrap().session_id,
        Some(SessionId::new("new-session"))
    );
    assert_eq!(
        replayed.message(&mail).unwrap().status,
        DeliveryStatus::Queued
    );
    let mut late = replayed.messages().filter(|m| m.id.seq() > binding);
    // Until a writer-side precondition exists the only allowed late effect
    // is one ordinary, replayable informational Note. No other side effect.
    if let Some(note) = late.next() {
        assert_eq!(
            (note.content.from.clone(), note.content.kind),
            (Address::System, MessageKind::Note)
        );
        assert!(note.content.delivery_notice.is_none());
        assert!(note
            .content
            .body
            .starts_with("atm cannot read demo/worker's screen"));
        assert!(late.next().is_none());
    }
}

async fn screen_notes(rig: &Rig) -> Vec<String> {
    rig.org
        .snapshot()
        .await
        .unwrap()
        .inbox(&Address::Owner)
        .iter()
        .filter(|msg| msg.content.from == Address::System && msg.content.kind == MessageKind::Note)
        .map(|msg| msg.content.body.clone())
        .collect()
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
    let observe_as = |id: &AgentId, recipient: &Recipient| {
        let facts = recipient.screen.facts(&frame, &recipient.baseline);
        observed(id, recipient, &frame, facts)
    };
    let see = |recipient: &Recipient| observe_as(&worker(), recipient);
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
        assert!(observe_as(&"demo/other".parse().unwrap(), &recipient).is_err());
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
