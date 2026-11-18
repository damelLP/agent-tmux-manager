//! Bounded daemon-owned jobs. No caller future owns release or callback lifetime.

use crate::control::service::dialog_request::Deferred;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Display,
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio::{
    sync::oneshot,
    task::{Id, JoinSet},
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;

use super::{
    kill_audit::KillAudit,
    state::{self, Fence, OperationId, State},
    Completion, ControlHandle, Reservation, ReservationError, ReservationRequest,
};

#[path = "capture_pacing.rs"]
mod capture_pacing;

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
    E: From<ReservationError> + Display + Send + 'static,
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
    E: From<ReservationError> + Display + Send + 'static,
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
            // Capability admission consumes this budget too: a delayed reply
            // can exhaust a consumer's cleanup margin before its callback starts.
            // A missed supervisor deadline still quarantines ownership.
            let deadline = Instant::now() + state::CLEANUP_TIMEOUT;
            let cleaned = match reservation.begin_cleanup(deadline).await {
                Ok(cleanup_token) => {
                    let drain_token = cleanup_token.clone();
                    let cleaned = phase(deadline, None, async move {
                        cleanup(cleanup_token, completion).await
                    })
                    .await
                    .0;
                    // Even if work dropped its kill future, the actor retained
                    // its ack. Never release ownership while it is pending.
                    let drained = drain_token.drain_audits(deadline).await.map_err(E::from);
                    cleaned.and(drained)
                }
                Err(error) => Err(error.into()),
            };
            let reusable = cleaned.is_ok();
            if let Err(error) = &cleaned {
                tracing::warn!(agent = %reservation.agent(), %error, "reservation cleanup failed; quarantining agent and panes");
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
/// Stop takes precedence; a finished worker beats a simultaneously ready deadline.
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
        result = &mut worker => return match result {
            Ok(Ok(value)) => (Ok(value), Completion::Succeeded),
            Ok(Err(error)) => (Err(error), Completion::Failed),
            Err(_) => (Err(ReservationError::Unavailable.into()), Completion::WorkerStopped),
        },
        () = sleep_until(deadline) => (ReservationError::Timeout, Completion::TimedOut),
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
    LegacyCapture(Box<Deferred>),
    Observe {
        pane: super::PaneId,
        format: super::CaptureFormat,
        deadline: Instant,
        reply: oneshot::Sender<Result<super::PaneCapture, ReservationError>>,
    },
    Pane {
        fence: Fence,
        action: super::pane_ops::Action,
        deadline: Instant,
        reply: oneshot::Sender<Result<super::pane_ops::Output, ReservationError>>,
    },
    Drain {
        fence: Fence,
        reply: oneshot::Sender<Result<Vec<Arc<KillAudit>>, ReservationError>>,
    },
    Cleanup {
        fence: Fence,
        deadline: Instant,
        reply: oneshot::Sender<Result<Fence, ReservationError>>,
    },
    Health {
        reply: oneshot::Sender<Result<crate::server::health::ReservationHealth, ReservationError>>,
    },
    /// Read the quarantined operations; changes nothing.
    Quarantines {
        reply: oneshot::Sender<Result<Vec<super::Quarantine>, ReservationError>>,
    },
}

impl Command {
    pub(in crate::control) fn reject(self) {
        self.reject_with(ReservationError::Unavailable);
    }

    pub(in crate::control) fn reject_with(self, error: ReservationError) {
        match self {
            Self::Acquire(admission) => admission.job.reject(error),
            Self::LegacyCapture(request) => request.reject(error),
            Self::Observe { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Pane { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Drain { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Cleanup { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Quarantines { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::Health { reply } => {
                let _ = reply.send(Err(error));
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
    audit_queue: crate::control::service::AuditQueue,
    kill_audits: HashMap<OperationId, Vec<Arc<KillAudit>>>,
    captures: capture_pacing::Pacing,
    legacy_captures: VecDeque<Deferred>,
}

impl Jobs {
    pub(in crate::control) fn new(
        cancel: CancellationToken,
        audit_queue: crate::control::service::AuditQueue,
    ) -> Self {
        Self {
            state: State::default(),
            waiting: HashMap::new(),
            running: JoinSet::new(),
            task_ids: HashMap::new(),
            cancel,
            audit_queue,
            kill_audits: HashMap::new(),
            captures: capture_pacing::Pacing::default(),
            legacy_captures: VecDeque::new(),
        }
    }

    pub(in crate::control) async fn dispatch(
        &mut self,
        command: Command,
        now: Instant,
        stopping: bool,
        client: &dyn atm_tmux::TmuxClient,
    ) {
        // Fence before pacing as well as at execution: an old capability must
        // fail immediately, and a queued capture must recheck after cleanup.
        if let Command::Pane { fence, .. } = &command {
            if let Err(error) = self.state.check(fence, None, now) {
                command.reject_with(error);
                return;
            }
        }
        if (stopping && matches!(command, Command::Observe { .. }))
            || ((stopping || self.cancel.is_cancelled())
                && matches!(command, Command::LegacyCapture(_)))
        {
            command.reject();
            return;
        }
        let Some(command) = self.captures.admit(command, now) else {
            return;
        };
        match command {
            Command::LegacyCapture(request) => self.legacy_captures.push_back(*request),
            Command::Observe {
                pane,
                format,
                deadline,
                reply,
            } => {
                let result = if stopping {
                    Err(ReservationError::Unavailable)
                } else if now >= deadline {
                    Err(ReservationError::Timeout)
                } else {
                    tokio::time::timeout_at(
                        deadline,
                        super::pane_ops::capture(client, &pane, format),
                    )
                    .await
                    .unwrap_or(Err(ReservationError::Timeout))
                };
                let _ = reply.send(result);
            }
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
            Command::Pane {
                fence,
                action,
                deadline,
                reply,
            } => {
                let audit = action.audit();
                if let Some(pending) = self.kill_audits.get_mut(&fence.operation) {
                    pending.retain(|ack| ack.pending());
                }
                let mut permit = None;
                let mut result = match self
                    .state
                    .check(&fence, None, now)
                    .and_then(|()| self.state.deadline(&fence))
                {
                    Err(error) => Err(error),
                    Ok(_) if now >= deadline => Err(ReservationError::Timeout),
                    Ok(_)
                        if audit.is_some()
                            && self
                                .kill_audits
                                .get(&fence.operation)
                                .is_some_and(|audits| audits.len() >= 2) =>
                    {
                        Err(ReservationError::InvalidRequest {
                            reason: "at most two pending kill audits per job".into(),
                        })
                    }
                    Ok(work_deadline) => {
                        let reserved = audit
                            .as_ref()
                            .map(|_| self.audit_queue.reserve())
                            .transpose();
                        match reserved {
                            Err(error) => Err(error),
                            Ok(slot) => {
                                permit = slot;
                                tokio::time::timeout_at(
                                    deadline.min(work_deadline),
                                    super::pane_ops::execute(
                                        client,
                                        &mut self.state,
                                        &fence,
                                        action,
                                        now,
                                    ),
                                )
                                .await
                                .unwrap_or(Err(ReservationError::Timeout))
                            }
                        }
                    }
                };
                if result.is_ok() {
                    if let (Some((command, time)), Some(permit)) = (audit, permit) {
                        let ack = KillAudit::submit(permit, command, time);
                        self.kill_audits
                            .entry(fence.operation)
                            .or_default()
                            .push(ack.clone());
                        result = Ok(super::pane_ops::Output::Audit(ack));
                    }
                }
                let _ = reply.send(result);
            }
            Command::Quarantines { reply } => {
                let _ = reply.send(Ok(self.state.quarantines()));
            }
            Command::Health { reply } => {
                let _ = reply.send(Ok(self.state.health()));
            }
            Command::Drain { fence, reply } => {
                let result = self.state.check(&fence, None, now).map(|()| {
                    self.kill_audits
                        .get(&fence.operation)
                        .cloned()
                        .unwrap_or_default()
                });
                let _ = reply.send(result);
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

    pub(in crate::control) async fn capture_ready(
        &mut self,
        client: &dyn atm_tmux::TmuxClient,
        stopping: bool,
    ) {
        // A legacy capture must run in the actor before a later ready frame
        // can overtake its pacing grant or block it on another pane.
        while self.legacy_captures.is_empty() {
            let Some(command) = self.captures.take_ready(Instant::now()) else {
                break;
            };
            self.dispatch(command, Instant::now(), stopping, client)
                .await;
        }
    }

    pub(in crate::control) fn take_legacy_capture(&mut self) -> Option<Deferred> {
        self.legacy_captures.pop_front()
    }

    pub(in crate::control) fn idle(&self) -> bool {
        self.is_empty() && self.captures.is_empty() && self.legacy_captures.is_empty()
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

    pub(in crate::control) fn input_gate(&self, pane: &super::PaneId) -> super::state::InputGate {
        self.state.input_gate(pane)
    }

    pub(in crate::control) fn next_deadline(&self) -> Option<Instant> {
        self.state
            .next_deadline()
            .into_iter()
            .chain(self.captures.next_deadline())
            .min()
    }
    pub(in crate::control) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub(in crate::control) fn stop_waiting(&mut self) {
        self.captures.stop();
        for request in self.legacy_captures.drain(..) {
            request.reject(ReservationError::Unavailable);
        }
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
                self.kill_audits.remove(&id);
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
    use atm_tmux::{MockTmuxClient, PaneDirection, PaneInfo, TmuxClient, TmuxError};
    use std::{
        future::poll_fn,
        sync::atomic::{AtomicBool, Ordering},
        task::Poll,
        time::Duration,
    };
    use tokio::sync::{Notify, Semaphore};

    struct HeldActorRead {
        mock: MockTmuxClient,
        entered: Arc<Notify>,
        release: Arc<Semaphore>,
    }

    type TmuxCall<'a, T> = Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
    macro_rules! forward {
        ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
            fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> TmuxCall<'async_trait, $result>
            where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
                Box::pin(self.mock.$method($($arg),*))
            }
        };
    }

    impl TmuxClient for HeldActorRead {
        fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
            &'s self,
            command: &'c str,
            args: &'a [&'v str],
        ) -> TmuxCall<'async_trait, String>
        where
            's: 'async_trait,
            'c: 'async_trait,
            'a: 'async_trait,
            'v: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
                self.mock.raw_command(command, args).await
            })
        }
        forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
        forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
        forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
        forward!(kill_pane<'a>(pane: &'a str) -> ());
        forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
        forward!(list_panes<>() -> Vec<PaneInfo>);
        forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
        forward!(select_pane<'a>(pane: &'a str) -> ());
        forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
        forward!(send_keys<'a, 'b>(pane: &'a str, keys: &'b str) -> ());
        forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
        forward!(new_session<'a>(name: &'a str) -> String);
    }

    struct CleanupAdmissionObserved {
        job: Box<dyn Job>,
        work_returned: Arc<AtomicBool>,
        waiting: oneshot::Sender<Instant>,
    }

    impl Job for CleanupAdmissionObserved {
        fn reject(self: Box<Self>, error: ReservationError) {
            self.job.reject(error);
        }

        fn start(
            self: Box<Self>,
            reservation: Reservation,
            cancel: CancellationToken,
        ) -> JobFuture {
            let mut future = self.job.start(reservation, cancel);
            let work_returned = self.work_returned;
            let mut waiting = Some(self.waiting);
            Box::pin(poll_fn(move |cx| {
                let result = future.as_mut().poll(cx);
                // On the current-thread runtime the work returns without an
                // await after setting the flag. This pending poll has joined
                // it and enqueued begin_cleanup behind the held actor read.
                if result.is_pending() && work_returned.load(Ordering::SeqCst) {
                    if let Some(waiting) = waiting.take() {
                        waiting.send(Instant::now()).unwrap();
                    }
                }
                result
            }))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_delayed_cleanup_capability_consumes_settlement_margin_and_quarantines() {
        use crate::control::reservation::{CaptureFormat, PaneId, Quarantine};

        let cancel = CancellationToken::new();
        let read_entered = Arc::new(Notify::new());
        let read_release = Arc::new(Semaphore::new(0));
        let (control, actor) = spawn_control(
            Box::new(HeldActorRead {
                mock: MockTmuxClient::new(),
                entered: read_entered.clone(),
                release: read_release.clone(),
            }),
            crate::registry::spawn_registry(),
            crate::org::OrgHandle::disabled("cleanup admission test"),
            cancel.clone(),
        );
        let (entered, entry) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let (waiting, admission_wait) = oneshot::channel();
        let (cleaning, clean) = oneshot::channel();
        let (settled, settlement) = oneshot::channel();
        let work_returned = Arc::new(AtomicBool::new(false));
        let returned = work_returned.clone();
        let (job, response) = job::<(), ReservationError, _, _, _, _>(
            move |_| async move {
                entered.send(()).unwrap();
                finished.await.unwrap();
                returned.store(true, Ordering::SeqCst);
                Ok(())
            },
            move |_, completion| async move {
                assert_eq!(completion, Completion::Succeeded);
                let started = Instant::now();
                cleaning.send(started).unwrap();
                // Model delivery's eight-second settlement limit, measured
                // from callback entry rather than before capability admission.
                let _ = tokio::time::timeout_at(
                    started + Duration::from_secs(8),
                    std::future::pending::<()>(),
                )
                .await;
                settled.send(()).unwrap();
                Ok(())
            },
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        control
            .enqueue_reserved(
                Command::Acquire(Admission {
                    control: control.clone(),
                    request: ReservationRequest {
                        agent: "p/a".parse().unwrap(),
                        pane: Some(PaneId::parse("%1").unwrap()),
                        kind: ReservationKind::Delivery,
                        wait: WaitPolicy::Wait,
                        deadline,
                    },
                    job: Box::new(CleanupAdmissionObserved {
                        job,
                        work_returned,
                        waiting,
                    }),
                }),
                deadline,
            )
            .await
            .unwrap();
        entry.await.unwrap();
        let other = control.clone();
        let read = tokio::spawn(async move {
            other
                .observe(&PaneId::parse("%2").unwrap(), CaptureFormat::Plain)
                .await
        });
        read_entered.notified().await;
        finish.send(()).unwrap();
        let budget_started = admission_wait.await.unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        read_release.add_permits(1);
        // The unrelated read intentionally has no valid frame scripted.
        assert!(read.await.unwrap().is_err());
        assert_eq!(
            clean.await.unwrap(),
            budget_started + Duration::from_secs(3)
        );
        tokio::time::advance(Duration::from_secs(7)).await;
        assert!(matches!(
            response.await.unwrap(),
            Err(ReservationError::Timeout)
        ));
        assert!(
            settlement.await.is_err(),
            "cleanup was stopped before its own limit"
        );
        let quarantines = control
            .quarantines(Instant::now() + Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            quarantines,
            vec![Quarantine {
                agent: "p/a".parse().unwrap(),
                pane: Some(PaneId::parse("%1").unwrap()),
                kind: ReservationKind::Delivery,
            }]
        );
        cancel.cancel();
        actor.await.unwrap();
    }

    async fn finished_worker_phase(
        deadline: Instant,
        cancel: Option<CancellationToken>,
        result: Result<u8, ReservationError>,
    ) -> impl Future<Output = (Result<u8, ReservationError>, Completion)> {
        let (returned, finished) = oneshot::channel();
        let mut call = Box::pin(phase(deadline, cancel, async move {
            // No await after this signal: on the current-thread runtime the
            // worker has returned before the test resumes from finished.await.
            returned.send(()).unwrap();
            result
        }));
        poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        finished.await.unwrap();
        // Keep the supervisor unpolled until the test makes its timer/stop ready.
        call
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_phase_finished_ok_beats_simultaneously_ready_deadline() {
        // None is cleanup's policy; an uncancelled token is work's policy.
        for cancel in [None, Some(CancellationToken::new())] {
            let deadline = Instant::now() + Duration::from_secs(5);
            let call = finished_worker_phase(deadline, cancel, Ok(17)).await;
            tokio::time::advance(Duration::from_secs(5)).await;
            let (result, completion) = call.await;
            assert_eq!(result.unwrap(), 17);
            assert_eq!(completion, Completion::Succeeded);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_phase_finished_error_beats_simultaneously_ready_deadline() {
        for cancel in [None, Some(CancellationToken::new())] {
            let deadline = Instant::now() + Duration::from_secs(5);
            let call = finished_worker_phase(
                deadline,
                cancel,
                Err(ReservationError::InvalidRequest {
                    reason: "worker failed".into(),
                }),
            )
            .await;
            tokio::time::advance(Duration::from_secs(5)).await;
            let (result, completion) = call.await;
            assert!(
                matches!(result, Err(ReservationError::InvalidRequest { reason }) if reason == "worker failed")
            );
            assert_eq!(completion, Completion::Failed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_phase_stop_beats_finished_worker_and_ready_deadline() {
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let call = finished_worker_phase(deadline, Some(cancel.clone()), Ok(17)).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        cancel.cancel();
        let (result, completion) = call.await;
        assert!(matches!(result, Err(ReservationError::Unavailable)));
        assert_eq!(completion, Completion::WorkerStopped);
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_phase_deadline_aborts_and_joins_unfinished_worker() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (entered, entry) = oneshot::channel();
        let (alive, mut lifetime) = oneshot::channel::<()>();
        let mut call = Box::pin(phase(deadline, None, async move {
            let _alive = alive;
            entered.send(()).unwrap();
            std::future::pending::<Result<(), ReservationError>>().await
        }));
        poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        entry.await.unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        let (result, completion) = call.await;
        assert!(matches!(result, Err(ReservationError::Timeout)));
        assert_eq!(completion, Completion::TimedOut);
        assert!(matches!(
            lifetime.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn reservation_shutdown_between_admission_and_grant_never_starts_callbacks() {
        let cancel = CancellationToken::new();
        let (control, actor) = spawn_control(
            Box::new(MockTmuxClient::new()),
            crate::registry::spawn_registry(),
            crate::org::OrgHandle::disabled("supervisor test"),
            cancel.clone(),
        );
        let mut jobs = Jobs::new(cancel.clone(), control.audit_queue());
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
            &MockTmuxClient::new(),
        )
        .await;
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
