//! The daemon's stop: the order its services stop in and how long each may
//! take. Every number of the stop budget is defined here, once.
//!
//! After the shutdown signal the connection handlers get [`CONNECTIONS`].
//! Then all reserved work (lifecycle flows, observations, delivery attempts)
//! is stopped at once and only its cleanups go on, and the stages stop in
//! order: messages, agents, control. A stage's bound is a deadline counted
//! from the moment work was stopped, not from the stage's own start: the
//! cleanups all began at that moment and run alongside the control service's
//! admitted deadlines. A stage that passes its bound is reported and aborted,
//! never waited for.
//!
//! None of this bounds blocking I/O: a stuck filesystem, or work Tokio waits
//! for when the runtime is dropped, can outlast every number here.

use std::time::Duration;

use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

/// Connection handlers, from the shutdown signal; then they are aborted.
pub const CONNECTIONS: Duration = Duration::from_secs(2);
/// Messages, from the work stop: an attempt's cleanup (10 s), the accounting
/// of what it committed (9 s), and a margin.
pub const MESSAGES: Duration = Duration::from_secs(20);
/// Agents, from the work stop: their cleanups (10 s) ended long before; this
/// is 2 s after the latest end of the message stage.
pub const AGENTS: Duration = Duration::from_secs(22);
/// Control and its audits, from the work stop: input admitted before the
/// signal keeps its own deadline (30 s), then the audit drain (30 s), and a
/// margin.
pub const CONTROL: Duration = Duration::from_secs(62);
/// The summary and raw flushes, both at once, after the server has stopped.
pub const FLUSHES: Duration = Duration::from_secs(2);
/// The longest a stop takes when every dependency answers inside its budget.
pub const NOMINAL: Duration =
    Duration::from_secs(CONNECTIONS.as_secs() + CONTROL.as_secs() + FLUSHES.as_secs());
/// How long `atmd stop` waits for the daemon's exit: [`NOMINAL`] and a margin.
/// The candidate kit and the README state the same number.
pub const STOP_WAIT: Duration = Duration::from_secs(75);

/// One service the server started: the token that stops it and its task.
#[derive(Debug, Default)]
pub(crate) struct Stage {
    pub cancel: CancellationToken,
    /// `None` when the server did not start this service.
    pub task: Option<JoinHandle<()>>,
}

/// The server's services, in the order they stop.
#[derive(Debug, Default)]
pub(crate) struct Stages {
    pub messages: Stage,
    pub agents: Stage,
    pub control: Stage,
}

/// A stage that did not end by its bound, and was aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overrun {
    pub stage: &'static str,
    pub bound: Duration,
}

/// Stop reserved work, then each stage in order: cancel it and wait for its
/// task until its bound, before the next one is cancelled. Messages stop
/// first, so an attempt in flight settles while agents, control and the
/// organisation writer are still there for it; control stops last.
pub(crate) async fn stop_stages(stop_work: impl FnOnce(), stages: Stages) -> Vec<Overrun> {
    let stopped = Instant::now();
    stop_work();
    let mut overruns = Vec::new();
    for (stage, bound, Stage { cancel, task }) in [
        ("messages", MESSAGES, stages.messages),
        ("agents", AGENTS, stages.agents),
        ("control", CONTROL, stages.control),
    ] {
        cancel.cancel();
        let Some(mut task) = task else {
            continue;
        };
        if tokio::time::timeout_at(stopped + bound, &mut task)
            .await
            .is_err()
        {
            tracing::warn!(
                stage,
                bound_secs = bound.as_secs(),
                "Shutdown stage did not end by its bound; aborting it"
            );
            // Not awaited: what did not end in time is not waited for again.
            task.abort();
            overruns.push(Overrun { stage, bound });
        }
    }
    overruns
}

#[cfg(test)]
mod tests {
    //! Paused clock; every stage is a task the test holds. A virtual sleep
    //! here returns only once every other task is blocked, so "nothing else
    //! happened" is read after one, never guessed from real time.
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, oneshot};

    /// A stage that reports its cancel, then ends when the test releases it.
    struct Held {
        cancel: CancellationToken,
        release: oneshot::Sender<()>,
        ended: Arc<AtomicBool>,
    }

    fn held(name: &'static str, events: &mpsc::UnboundedSender<&'static str>) -> (Stage, Held) {
        let cancel = CancellationToken::new();
        let (release, released) = oneshot::channel();
        let ended = Arc::new(AtomicBool::new(false));
        let (seen, events, done) = (cancel.clone(), events.clone(), ended.clone());
        let task = tokio::spawn(async move {
            seen.cancelled().await;
            events.send(name).unwrap();
            let _ = released.await;
            done.store(true, Ordering::SeqCst);
        });
        let stage = Stage {
            cancel: cancel.clone(),
            task: Some(task),
        };
        (
            stage,
            Held {
                cancel,
                release,
                ended,
            },
        )
    }

    /// A stage that ends as soon as it is cancelled.
    fn prompt(ended: &Arc<AtomicBool>) -> Stage {
        let cancel = CancellationToken::new();
        let (seen, done) = (cancel.clone(), ended.clone());
        let task = tokio::spawn(async move {
            seen.cancelled().await;
            done.store(true, Ordering::SeqCst);
        });
        Stage {
            cancel,
            task: Some(task),
        }
    }

    /// A stage that never ends, whatever is cancelled. `dropped` is set when
    /// its task is aborted.
    fn stalled(dropped: &Arc<AtomicBool>) -> Stage {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = Flag(dropped.clone());
        let task = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        Stage {
            cancel: CancellationToken::new(),
            task: Some(task),
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    struct Rig {
        events: mpsc::UnboundedReceiver<&'static str>,
        messages: Held,
        agents: Held,
        control: Held,
        stop: JoinHandle<Vec<Overrun>>,
    }

    fn rig() -> Rig {
        let (send, events) = mpsc::unbounded_channel();
        let (messages_stage, messages) = held("messages", &send);
        let (agents_stage, agents) = held("agents", &send);
        let (control_stage, control) = held("control", &send);
        let stages = Stages {
            messages: messages_stage,
            agents: agents_stage,
            control: control_stage,
        };
        let stop = tokio::spawn(stop_stages(move || send.send("work").unwrap(), stages));
        Rig {
            events,
            messages,
            agents,
            control,
            stop,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn work_stops_before_any_stage_is_cancelled() {
        let mut rig = rig();
        assert_eq!(rig.events.recv().await, Some("work"));
        assert_eq!(rig.events.recv().await, Some("messages"));
    }

    #[tokio::test(start_paused = true)]
    async fn stages_stop_in_order_each_awaited_before_the_next() {
        let mut rig = rig();
        assert_eq!(rig.events.recv().await, Some("work"));
        assert_eq!(rig.events.recv().await, Some("messages"));
        rig.messages.release.send(()).unwrap();
        assert_eq!(rig.events.recv().await, Some("agents"));
        assert!(rig.messages.ended.load(Ordering::SeqCst));
        settle().await;
        assert!(!rig.control.cancel.is_cancelled());
        assert!(!rig.stop.is_finished());
        rig.agents.release.send(()).unwrap();
        assert_eq!(rig.events.recv().await, Some("control"));
        assert!(rig.agents.ended.load(Ordering::SeqCst));
        settle().await;
        assert!(!rig.stop.is_finished());
        rig.control.release.send(()).unwrap();
        assert_eq!(rig.stop.await.unwrap(), []);
        assert!(rig.control.ended.load(Ordering::SeqCst));
    }

    /// Messages are cancelled and awaited before agents and control are
    /// stopped: an attempt in flight settles while both are still there.
    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_messages_before_control() {
        let mut rig = rig();
        assert_eq!(rig.events.recv().await, Some("work"));
        // The first stage to see its cancel is messages.
        assert_eq!(rig.events.recv().await, Some("messages"));
        assert!(rig.messages.cancel.is_cancelled());
        // While its task runs, nothing after it is stopped.
        settle().await;
        assert!(!rig.agents.cancel.is_cancelled());
        assert!(!rig.control.cancel.is_cancelled());
        assert!(!rig.messages.ended.load(Ordering::SeqCst));
        rig.messages.release.send(()).unwrap();
        assert_eq!(rig.events.recv().await, Some("agents"));
        assert!(rig.messages.ended.load(Ordering::SeqCst));
        rig.agents.release.send(()).unwrap();
        assert_eq!(rig.events.recv().await, Some("control"));
        rig.control.release.send(()).unwrap();
        // It ended by being awaited, not by passing its bound.
        assert_eq!(rig.stop.await.unwrap(), []);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stage_that_never_ends_is_reported_and_the_rest_still_stop() {
        for (stage, bound) in [
            ("messages", MESSAGES),
            ("agents", AGENTS),
            ("control", CONTROL),
        ] {
            let aborted = Arc::new(AtomicBool::new(false));
            let ended: [Arc<AtomicBool>; 3] = Default::default();
            let build = |name: &str, ended: &Arc<AtomicBool>| {
                if name == stage {
                    stalled(&aborted)
                } else {
                    prompt(ended)
                }
            };
            let stages = Stages {
                messages: build("messages", &ended[0]),
                agents: build("agents", &ended[1]),
                control: build("control", &ended[2]),
            };
            let started = Instant::now();
            let overruns = stop_stages(|| {}, stages).await;
            assert_eq!(overruns, [Overrun { stage, bound }], "{stage}");
            // Reported at its bound, and not waited for after it.
            assert_eq!(started.elapsed(), bound, "{stage}");
            settle().await;
            assert!(aborted.load(Ordering::SeqCst), "{stage} was not aborted");
            let others = ended.iter().filter(|e| e.load(Ordering::SeqCst)).count();
            assert_eq!(others, 2, "the other stages still stop around {stage}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn bounds_are_deadlines_from_the_stop_not_sums() {
        let cancel = CancellationToken::new();
        let seen = cancel.clone();
        // Late, and in time.
        let slow = tokio::spawn(async move {
            seen.cancelled().await;
            tokio::time::sleep(Duration::from_secs(15)).await;
        });
        let aborted = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let stages = Stages {
            messages: Stage {
                cancel,
                task: Some(slow),
            },
            agents: stalled(&aborted),
            control: prompt(&ended),
        };
        let started = Instant::now();
        let overruns = stop_stages(|| {}, stages).await;
        let agents = Overrun {
            stage: "agents",
            bound: AGENTS,
        };
        assert_eq!(overruns, [agents]);
        assert_eq!(started.elapsed(), AGENTS);
        assert!(ended.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stage_the_server_did_not_start_is_cancelled_and_skipped() {
        let stages = Stages::default();
        let tokens = [
            stages.messages.cancel.clone(),
            stages.agents.cancel.clone(),
            stages.control.cancel.clone(),
        ];
        let started = Instant::now();
        assert_eq!(stop_stages(|| {}, stages).await, []);
        assert!(tokens.iter().all(CancellationToken::is_cancelled));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[test]
    fn budget_adds_up() {
        assert_eq!(NOMINAL, Duration::from_secs(66));
        // Each later deadline leaves room after the one before it.
        assert!(MESSAGES < AGENTS && AGENTS < CONTROL);
        assert!(STOP_WAIT > NOMINAL);
        assert_eq!(STOP_WAIT, Duration::from_secs(75));
    }
}
