//! Bounded daemon-owned jobs. No caller future owns release or callback lifetime.

use std::{collections::HashMap, future::Future, pin::Pin};
use tokio::{
    sync::oneshot,
    task::{Id, JoinSet},
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;

use super::{
    state::{self, Fence, OperationId, State},
    Completion, ControlHandle, PaneId, Reservation, ReservationError, ReservationRequest,
};

type JobFuture = Pin<Box<dyn Future<Output = Finished> + Send>>;
type Response<T, E> = oneshot::Receiver<Result<T, E>>;

pub(super) trait Job: Send {
    fn start(self: Box<Self>, reservation: Reservation, cancel: CancellationToken) -> JobFuture;
    fn reject(self: Box<Self>, error: ReservationError);
}

struct Call<T, E, W, C> {
    work: W,
    cleanup: C,
    reply: oneshot::Sender<Result<T, E>>,
}

pub(super) fn job<T, E, W, WF, C, CF>(work: W, cleanup: C) -> (Box<dyn Job>, Response<T, E>)
where
    T: Send + 'static,
    E: From<ReservationError> + Send + 'static,
    W: FnOnce(Reservation) -> WF + Send + 'static,
    WF: Future<Output = Result<T, E>> + Send + 'static,
    C: FnOnce(Reservation, Completion) -> CF + Send + 'static,
    CF: Future<Output = Result<(), E>> + Send + 'static,
{
    let (reply, response) = oneshot::channel();
    (
        Box::new(Call {
            work,
            cleanup,
            reply,
        }),
        response,
    )
}

/// The actor releases/quarantines BEFORE invoking this reply continuation.
pub(super) struct Finished {
    reusable: bool,
    reply: Box<dyn FnOnce() + Send>,
}

impl<T, E, W, WF, C, CF> Job for Call<T, E, W, C>
where
    T: Send + 'static,
    E: From<ReservationError> + Send + 'static,
    W: FnOnce(Reservation) -> WF + Send + 'static,
    WF: Future<Output = Result<T, E>> + Send + 'static,
    C: FnOnce(Reservation, Completion) -> CF + Send + 'static,
    CF: Future<Output = Result<(), E>> + Send + 'static,
{
    fn reject(self: Box<Self>, error: ReservationError) {
        let _ = self.reply.send(Err(error.into()));
    }

    fn start(self: Box<Self>, reservation: Reservation, cancel: CancellationToken) -> JobFuture {
        Box::pin(async move {
            let Self {
                work,
                cleanup,
                reply,
            } = *self;
            let work_token = reservation.clone();
            let (result, completion) = phase(reservation.deadline, Some(cancel), async move {
                work(work_token).await
            })
            .await;
            let deadline = Instant::now() + state::CLEANUP_TIMEOUT;
            let cleaned = match reservation.begin_cleanup(deadline).await {
                Ok(cleanup_token) => {
                    phase(deadline, None, async move {
                        cleanup(cleanup_token, completion).await
                    })
                    .await
                    .0
                }
                Err(error) => Err(error.into()),
            };
            let reusable = cleaned.is_ok();
            if !reusable {
                tracing::warn!(agent = %reservation.agent(), "reservation cleanup failed; quarantining agent and panes");
            }
            let result = match (result, cleaned) {
                (Ok(value), Ok(())) => Ok(value),
                (Ok(_), Err(error)) | (Err(error), _) => Err(error),
            };
            Finished {
                reusable,
                reply: Box::new(move || {
                    let _ = reply.send(result);
                }),
            }
        })
    }
}

/// Callback invocation is inside the child task, so a callback panic also joins
/// before phase rotation. Aborting a cooperative async task is not a rollback.
async fn phase<T, E>(
    deadline: Instant,
    cancel: Option<CancellationToken>,
    future: impl Future<Output = Result<T, E>> + Send + 'static,
) -> (Result<T, E>, Completion)
where
    T: Send + 'static,
    E: From<ReservationError> + Send + 'static,
{
    if Instant::now() >= deadline {
        return (Err(ReservationError::Timeout.into()), Completion::TimedOut);
    }
    let mut worker = tokio::spawn(future);
    let stopped = async {
        match cancel {
            Some(cancel) => cancel.cancelled().await,
            None => std::future::pending().await,
        }
    };
    let (error, completion) = tokio::select! {
        biased;
        () = stopped => (ReservationError::Unavailable, Completion::WorkerStopped),
        () = sleep_until(deadline) => (ReservationError::Timeout, Completion::TimedOut),
        result = &mut worker => return match result {
            Ok(Ok(value)) => (Ok(value), Completion::Succeeded),
            Ok(Err(error)) => (Err(error), Completion::Failed),
            Err(_) => (Err(ReservationError::Unavailable.into()), Completion::WorkerStopped),
        },
    };
    worker.abort();
    let _ = worker.await;
    (Err(error.into()), completion)
}

pub(in crate::control) struct Admission {
    pub(super) request: ReservationRequest,
    pub(super) control: ControlHandle,
    pub(super) job: Box<dyn Job>,
}

pub(in crate::control) enum Command {
    Acquire(Admission),
    Check {
        fence: Fence,
        pane: Option<PaneId>,
        reply: oneshot::Sender<Result<(), ReservationError>>,
    },
    Cleanup {
        fence: Fence,
        deadline: Instant,
        reply: oneshot::Sender<Result<Fence, ReservationError>>,
    },
}

impl Command {
    pub(in crate::control) fn reject(self) {
        match self {
            Self::Acquire(admission) => admission.job.reject(ReservationError::Unavailable),
            Self::Check { reply, .. } => {
                let _ = reply.send(Err(ReservationError::Unavailable));
            }
            Self::Cleanup { reply, .. } => {
                let _ = reply.send(Err(ReservationError::Unavailable));
            }
        }
    }
}

pub(in crate::control) struct Jobs {
    state: State,
    waiting: HashMap<OperationId, Admission>,
    running: JoinSet<Finished>,
    task_ids: HashMap<Id, OperationId>,
    cancel: CancellationToken,
}

impl Jobs {
    pub(in crate::control) fn new(cancel: CancellationToken) -> Self {
        Self {
            state: State::default(),
            waiting: HashMap::new(),
            running: JoinSet::new(),
            task_ids: HashMap::new(),
            cancel,
        }
    }

    pub(in crate::control) fn dispatch(&mut self, command: Command, now: Instant, stopping: bool) {
        match command {
            Command::Acquire(admission) => {
                let result = if stopping {
                    Err(ReservationError::Unavailable)
                } else {
                    self.state.admit(admission.request.clone(), now)
                };
                match result {
                    Ok(id) => {
                        self.waiting.insert(id, admission);
                    }
                    Err(error) => admission.job.reject(error),
                }
            }
            Command::Check { fence, pane, reply } => {
                let _ = reply.send(self.state.check(&fence, pane.as_ref(), now));
            }
            Command::Cleanup {
                fence,
                deadline,
                reply,
            } => {
                let result = if now >= deadline {
                    Err(ReservationError::Timeout)
                } else {
                    self.state.begin_cleanup(&fence, deadline)
                };
                let _ = reply.send(result);
            }
        }
    }

    pub(in crate::control) fn ready(&mut self, now: Instant) {
        if self.cancel.is_cancelled() {
            self.stop_waiting();
            return;
        }
        for (id, result) in self.state.ready(now) {
            if let Some(admission) = self.waiting.remove(&id) {
                match result {
                    Ok(fence) => {
                        let reservation = Reservation {
                            agent: admission.request.agent,
                            control: admission.control,
                            fence,
                            deadline: admission.request.deadline,
                        };
                        let task = self
                            .running
                            .spawn(admission.job.start(reservation, self.cancel.clone()));
                        self.task_ids.insert(task.id(), id);
                    }
                    Err(error) => admission.job.reject(error),
                }
            }
        }
    }

    pub(in crate::control) fn next_deadline(&self) -> Option<Instant> {
        self.state.next_deadline()
    }
    pub(in crate::control) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub(in crate::control) fn stop_waiting(&mut self) {
        self.state.stop_waiting();
        for (_, admission) in self.waiting.drain() {
            admission.job.reject(ReservationError::Unavailable);
        }
    }

    pub(in crate::control) async fn join_next(&mut self) {
        if let Some(result) = self.running.join_next_with_id().await {
            let task_id = match &result {
                Ok((id, _)) => *id,
                Err(error) => error.id(),
            };
            if let Some(id) = self.task_ids.remove(&task_id) {
                match result {
                    Ok((_, finished)) => {
                        self.state.finish(id, finished.reusable);
                        (finished.reply)();
                    }
                    Err(error) => {
                        self.state.finish(id, false);
                        tracing::error!(%error, "reservation supervisor stopped; quarantining ownership");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        reservation::{ReservationKind, WaitPolicy},
        service::spawn_control,
    };
    use atm_tmux::MockTmuxClient;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn reservation_shutdown_between_admission_and_grant_never_starts_callbacks() {
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(MockTmuxClient::new()),
            crate::registry::spawn_registry(),
            cancel.clone(),
        );
        let mut jobs = Jobs::new(cancel.clone());
        let (job, response) = job::<(), ReservationError, _, _, _, _>(
            |_| async { panic!("ungranted work ran after shutdown") },
            |_, _| async { panic!("ungranted cleanup ran after shutdown") },
        );
        let now = Instant::now();
        jobs.dispatch(
            Command::Acquire(Admission {
                control,
                request: ReservationRequest {
                    agent: "p/a".parse().unwrap(),
                    pane: None,
                    kind: ReservationKind::Up,
                    wait: WaitPolicy::Wait,
                    deadline: now + Duration::from_secs(60),
                },
                job,
            }),
            now,
            false,
        );
        cancel.cancel();
        jobs.ready(now);
        assert!(
            jobs.is_empty(),
            "shutdown must not start an ungranted supervisor"
        );
        assert!(matches!(
            response.await.unwrap(),
            Err(ReservationError::Unavailable)
        ));
        actor.await.unwrap();
    }
}
