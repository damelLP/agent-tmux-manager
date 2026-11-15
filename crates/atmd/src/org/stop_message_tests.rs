// The daemon's stop over the real delivery executor and the real control
// actor.
//
// Included at the top of `message_delivery_tests.rs`, into the same module,
// for its rig: no module docs. A test here is the recipient's slot, as in
// that file, and the slot's task is the message stage that the server's own
// `stop_stages` cancels and awaits. Paused clock, in-memory I/O only.

use crate::server::stop::{stop_stages, Overrun, Stage, Stages, MESSAGES};

struct StoppedDelivery {
    overruns: Vec<Overrun>,
    took: Duration,
    /// What the executor answered, if the slot got to hear it.
    delivered: Option<Result<(), MessageError>>,
}

/// Stop the daemon while `attempt` is in flight: reserved work first (or
/// not, for a twin), then the message stage, which here is the recipient's
/// slot waiting for its attempt, under the message service's own token.
async fn stop_during(
    rig: &Rig,
    attempt: JoinHandle<Result<(), MessageError>>,
    stop_work: bool,
) -> StoppedDelivery {
    let (returned, delivered) = oneshot::channel();
    let slot = tokio::spawn(async move {
        let _ = returned.send(attempt.await.unwrap());
    });
    let stages = Stages {
        messages: Stage {
            cancel: rig.rt.cancel.clone(),
            task: Some(slot),
        },
        ..Stages::default()
    };
    let control = rig.rt.control.clone();
    let started = Instant::now();
    let overruns = stop_stages(
        || {
            if stop_work {
                control.stop_reserved_work();
            }
        },
        stages,
    )
    .await;
    StoppedDelivery {
        overruns,
        took: started.elapsed(),
        delivered: delivered.await.ok(),
    }
}

/// The daemon stops after the paste and before the Enter. The attempt's work
/// is stopped where it stands and its cleanup settles it as a deadline would:
/// no Enter, no failure written, and the attempt held with the reference
/// still in the editor as its reason. The stage ends well inside its bound.
#[tokio::test(start_paused = true)]
async fn stop_with_a_held_delivery_attempt_ends_inside_the_budget() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    // Cleanup's one capture. The look that would allow the Enter has no
    // frame: the stop comes before it.
    rig.script(&[pi(IDLE), pi(IDLE), staged(id)]);
    let before = rig.org.status().unwrap();
    rig.fold_rest();
    let attempt = rig.spawn_deliver(id);
    rig.pasted().await;

    let stopped = stop_during(&rig, attempt, true).await;

    assert_eq!(stopped.overruns, []);
    assert!(stopped.took < MESSAGES, "{:?}", stopped.took);
    assert!(
        matches!(
            stopped.delivered,
            Some(Err(MessageError::Reservation(
                ReservationError::Unavailable
            )))
        ),
        "{:?}",
        stopped.delivered
    );
    assert_eq!((rig.pastes(), rig.keys(), rig.reads()), (1, 0, 0));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.hold, Some(ExecutorHold::TokenVisible));
    // The attempt's event, and nothing after it.
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before + 1);
}

/// The attempt has not started: it waits for its reservation behind another
/// job on the recipient's pane. The stop refuses what waits, so the executor
/// returns at once, having committed and typed nothing; the message stays
/// queued for the next daemon.
#[tokio::test(start_paused = true)]
async fn stop_ends_a_delivery_that_waits_for_its_reservation() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    let before = rig.org.status().unwrap();
    rig.fold_rest();
    let _holder = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;

    let stopped = stop_during(&rig, attempt, true).await;

    assert_eq!(stopped.overruns, []);
    assert!(stopped.took < MESSAGES, "{:?}", stopped.took);
    assert!(
        matches!(
            stopped.delivered,
            Some(Err(MessageError::Reservation(
                ReservationError::Unavailable
            )))
        ),
        "{:?}",
        stopped.delivered
    );
    assert_eq!((rig.pastes(), rig.keys()), (0, 0));
    let snapshot = rig.snapshot(id);
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert_eq!(snapshot.hold, None);
    guarded(rig.org.snapshot()).await.unwrap();
    assert_eq!(rig.org.status().unwrap(), before);
}

/// Twin of the test above: without the work stop the attempt waits for its
/// reservation as long as its own budget allows, which is longer than the
/// stage's bound. The stage is reported and abandoned.
#[tokio::test(start_paused = true)]
async fn without_the_work_stop_a_waiting_delivery_outlives_the_message_stage() {
    let mut rig = Rig::hand().await;
    let id = rig.ready_message().await;
    rig.fold_rest();
    let _holder = rig.park().await;
    let attempt = rig.spawn_deliver(id);
    rig.admission_requested(id).await;

    let stopped = stop_during(&rig, attempt, false).await;

    let messages = Overrun {
        stage: "messages",
        bound: MESSAGES,
    };
    assert_eq!(stopped.overruns, [messages]);
    assert_eq!(stopped.took, MESSAGES);
    assert!(stopped.delivered.is_none(), "{:?}", stopped.delivered);
    assert_eq!((rig.pastes(), rig.keys()), (0, 0));
    assert_eq!(rig.snapshot(id).message.status, DeliveryStatus::Queued);
}
