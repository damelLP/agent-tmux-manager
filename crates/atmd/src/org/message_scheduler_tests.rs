//! The recipient scheduler over the in-memory organisation rig: a real
//! organisation writer, the real history, evidence and policy, and two fakes at
//! the injected seam. The observer answers a scripted observation; the
//! executor is a scheduling fake with entered / work / cleanup gates, not an
//! acceptance test of the real executor.
//!
//! Time is paused and all I/O is in memory. Tests synchronize on committed
//! events, the executor's channels and returned calls. Where a rule is about
//! time passing (a poll, a deadline, a budget), the test moves the paused
//! clock by that span and then reads state; nothing waits on a real clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::{
    message_delivery::inbox_reference,
    message_evidence::{AttemptScope, MAX_MESSAGES},
    message_history::{EffectCertainty, PanePresence, SessionObservation},
    message_service_tests::{
        code, empty_editor, guarded, other, owner, page, pane, poll_pending, spawn_polled, until,
        utc, worker, Rig,
    },
    messages::{
        is_focus_notice, AgentId, GiveUpParams, GiveUpResult, InboxEntry, InboxParams,
        ResolveParams, MESSAGE_ADMISSION_LIMIT,
    },
    start_line::delivery_line,
};
use super::*;
use crate::org::message_delivery::MESSAGE_RESOLUTION_BUDGET;
use crate::server::methods::CallContext;
use atm_core::{Harness, InputState, LifecycleEvent, SessionId, SessionStatus};
use atm_org::OrgEvent;
use std::collections::BTreeMap;
use tokio::sync::{broadcast, mpsc, Semaphore};

const OPEN: usize = 1 << 20;

fn blocked_notice(id: MessageId, reason: &str, behind: &[MessageId], latched: bool) -> String {
    super::blocked_notice(id, reason, behind, latched, false, None)
}

/// The retract effect for a test that must never reach it (s3-5c).
fn no_retract() -> Retract {
    Arc::new(|_, id, _| Box::pin(async move { panic!("no retract expected for {id}") }))
}

/// What the observer answers for each recipient; a test changes it as it goes.
#[derive(Clone)]
struct Screens {
    seen: Arc<Mutex<HashMap<AgentId, RecipientObservation>>>,
    /// The editor's one-row text per recipient, for the exact staged match
    /// (s3-5c); none means the adapter cannot tell.
    boxes: Arc<Mutex<HashMap<AgentId, String>>>,
    calls: Arc<Mutex<BTreeMap<AgentId, usize>>>,
    /// Closed, it holds every observation after it was counted.
    gate: Arc<Semaphore>,
}

impl Default for Screens {
    fn default() -> Self {
        Self {
            seen: Arc::default(),
            boxes: Arc::default(),
            calls: Arc::default(),
            gate: Arc::new(Semaphore::new(OPEN)),
        }
    }
}

impl Screens {
    fn set(&self, agent: AgentId, observation: RecipientObservation) {
        self.seen.lock().unwrap().insert(agent, observation);
    }

    /// What the recipient's editor holds on one row; None: unreadable.
    fn set_box(&self, agent: AgentId, text: Option<&str>) {
        let mut boxes = self.boxes.lock().unwrap();
        match text {
            Some(text) => boxes.insert(agent, text.into()),
            None => boxes.remove(&agent),
        };
    }

    /// The observer fails for this recipient from now on.
    fn lose(&self, agent: &AgentId) {
        self.seen.lock().unwrap().remove(agent);
    }

    fn calls(&self, agent: &AgentId) -> usize {
        self.calls.lock().unwrap().get(agent).copied().unwrap_or(0)
    }

    fn observed(&self) -> Vec<AgentId> {
        self.calls.lock().unwrap().keys().cloned().collect()
    }

    fn effect(&self) -> Observe {
        let screens = self.clone();
        Arc::new(move |agent: Agent, reference: Option<String>| {
            let screens = screens.clone();
            Box::pin(async move {
                let staged = reference.and_then(|line| {
                    (screens.boxes.lock().unwrap().get(&agent.id)).map(|text| *text == line)
                });
                *screens
                    .calls
                    .lock()
                    .unwrap()
                    .entry(agent.id.clone())
                    .or_default() += 1;
                pass(&screens.gate).await;
                (screens.seen.lock().unwrap().get(&agent.id).cloned())
                    .map(|seen| (seen, None, None, staged))
                    .ok_or_else(|| MessageError::Unavailable("no observation scripted".into()))
            })
        })
    }
}

/// An idle, unfocused pane with an empty editor: what lets a paste through
/// once it has been stable.
fn ready(pane: &str, session: &str) -> RecipientObservation {
    RecipientObservation {
        pane: Some(PaneId::parse(pane).unwrap()),
        session: Some(SessionObservation {
            id: session.into(),
            harness: Harness::Pi,
            status: SessionStatus::Idle,
            last_event_at: Some(utc()),
        }),
        presence: PanePresence::Alive,
        facts: empty_editor(),
        inbox_paste: false,
        copy_mode: Some(false),
        focused: Some(false),
    }
}

fn with(
    mut observation: RecipientObservation,
    change: impl FnOnce(&mut RecipientObservation),
) -> RecipientObservation {
    change(&mut observation);
    observation
}

/// What the executor fake does once its work gate lets it through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    /// Refuse before any commit, as a busy reservation does.
    Refuse,
    /// Fail before any commit with an error that is not a decision.
    Offline,
    /// Return Ok having committed nothing.
    Nothing,
    /// Commit the attempt, arm, record the paste as applied; no receipt.
    Paste,
    /// As Paste, then report an error without settling or holding.
    PasteThenError,
    /// Commit the attempt, prove it typed nothing, record the nonfinal
    /// failure, and report the error.
    NoEffect,
    /// As Paste, then the receipt arrives inside the attempt.
    Deliver,
    /// Submit the attempt and report a timeout without its reply: whether
    /// it was committed is for the scheduler's accounting to find out.
    LostReply,
}

async fn perform(rt: MessageRuntime, id: MessageId, act: Act) -> Result<(), MessageError> {
    match act {
        Act::Refuse => return Err(MessageError::Conflict("pane is reserved".into())),
        Act::Offline => return Err(MessageError::Unavailable("executor offline".into())),
        Act::Nothing => return Ok(()),
        _ => {}
    }
    let now = rt.clock.now()?;
    let snapshot = rt.history.snapshot(id, now)?;
    let step = next_delivery(&snapshot.message, &snapshot.live, now, &rt.policy).unwrap();
    let DeliveryStep::PasteNow { attempt } = step else {
        return Err(MessageError::Conflict(format!("not ready: {step:?}")));
    };
    rt.history
        .record(HistoryUpdate::Consumed { id, step }, now)?;
    let pane = snapshot.recipient.pane.clone().unwrap();
    let attempted = OrgCommand::DeliveryAttempted {
        message_id: id,
        attempt,
        pane: pane.clone(),
    };
    if act == Act::LostReply {
        let org = rt.org.clone();
        let capacity = org.requests.capacity();
        spawn_polled(async move { org.execute(attempted, now).await });
        // Polling execute enqueues the command before the executor returns;
        // its FIFO accounting read cannot overtake this unknown append.
        assert_eq!(rt.org.requests.capacity(), capacity - 1);
        return Err(MessageError::Timeout);
    }
    let seq = rt.org.execute(attempted, now).await?.last().unwrap().seq;
    rt.history.folded(seq).await?;
    let session = snapshot.recipient.session_id.clone();
    let scope = AttemptScope {
        message_id: id,
        attempt_seq: seq,
        recipient: snapshot.recipient.id.clone(),
        pane: PaneId::parse(&pane)?,
        harness: Harness::Pi,
        session_id: session.clone(),
        excluded_sessions: snapshot.recipient.excluded_session_id.into_iter().collect(),
    };
    rt.evidence.arm(scope.clone())?;
    rt.history.record(HistoryUpdate::Armed { scope }, now)?;
    let paste = |certainty| HistoryUpdate::Paste {
        id,
        pane: PaneId::parse(&pane).unwrap(),
        certainty,
    };
    if act == Act::NoEffect {
        rt.history.record(paste(EffectCertainty::NotStarted), now)?;
        let failed = OrgCommand::DeliveryFailed {
            message_id: id,
            reason: "staging refused".into(),
            final_failure: false,
        };
        let seq = rt.org.execute(failed, now).await?.last().unwrap().seq;
        rt.history.folded(seq).await?;
        return Err(MessageError::Refused("staging refused".into()));
    }
    rt.history.record(paste(EffectCertainty::Applied), now)?;
    match act {
        Act::PasteThenError => Err(MessageError::Timeout),
        Act::Deliver => {
            let session = session.unwrap();
            let prompt = LifecycleEvent::PromptSubmit {
                prompt: Some(delivery_line(id, &snapshot.message.content)),
            };
            (rt.evidence).observe(&session, Some(&pane), Harness::Pi, &prompt)?;
            let receipt = rt.evidence.receipt(id)?.unwrap();
            let delivered = OrgCommand::Delivered {
                message_id: id,
                session_id: receipt.session_id.clone(),
                confirmation: receipt.confirmation.clone(),
            };
            let seq = rt.org.execute(delivered, now).await?.last().unwrap().seq;
            rt.history.folded(seq).await?;
            rt.evidence.acknowledge(id, &receipt)
        }
        _ => Ok(()),
    }
}

/// Pass a gate: an open gate has permits to spare, a closed one lets exactly
/// as many callers through as the test adds permits.
async fn pass(gate: &Semaphore) {
    gate.acquire().await.unwrap().forget();
}

fn close(gate: &Semaphore) {
    gate.forget_permits(usize::MAX);
}

/// The scheduled rig, its fakes, and the committed events from now on.
struct Bench {
    rig: Rig,
    screens: Screens,
    act: Arc<Mutex<Act>>,
    entered: mpsc::UnboundedReceiver<MessageId>,
    worked: mpsc::UnboundedReceiver<MessageId>,
    work: Arc<Semaphore>,
    cleanup: Arc<Semaphore>,
    /// The work budget the scheduler granted each attempt.
    budgets: Arc<Mutex<Vec<Duration>>>,
    events: broadcast::Receiver<Arc<Envelope>>,
}

impl Bench {
    /// `demo/worker` on %1 and `demo/other` on %2, both ready, executor gates
    /// open, and an executor that pastes and gets no receipt.
    async fn new() -> Self {
        let screens = Screens::default();
        screens.set(worker(), ready("%1", "s1"));
        screens.set(other(), ready("%2", "s2"));
        Self::with_screens(screens).await
    }

    /// No observation ever succeeds: nothing can be pasted.
    async fn blind() -> Self {
        Self::with_screens(Screens::default()).await
    }

    async fn with_screens(screens: Screens) -> Self {
        Self::build(screens, true).await
    }

    async fn build(screens: Screens, scheduled: bool) -> Self {
        let act = Arc::new(Mutex::new(Act::Paste));
        let (enter, entered) = mpsc::unbounded_channel();
        let (work_done, worked) = mpsc::unbounded_channel();
        let work = Arc::new(Semaphore::new(OPEN));
        let cleanup = Arc::new(Semaphore::new(OPEN));
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let deliver: Deliver = Arc::new({
            let (script, work, cleanup, budgets) =
                (act.clone(), work.clone(), cleanup.clone(), budgets.clone());
            move |rt, id, deadline: Instant| {
                let (script, work, cleanup) = (script.clone(), work.clone(), cleanup.clone());
                let (enter, work_done) = (enter.clone(), work_done.clone());
                budgets.lock().unwrap().push(deadline - Instant::now());
                Box::pin(async move {
                    let _ = enter.send(id);
                    pass(&work).await;
                    let script = *script.lock().unwrap();
                    let result = perform(rt, id, script).await;
                    let _ = work_done.send(id);
                    pass(&cleanup).await;
                    result
                })
            }
        });
        let rig = if scheduled {
            Rig::scheduled(Effects {
                deliver,
                observe: screens.effect(),
                retract: no_retract(),
            })
            .await
        } else {
            Rig::seeded().await
        };
        let events = rig.org.committed.subscribe();
        Self {
            rig,
            screens,
            act,
            entered,
            worked,
            work,
            cleanup,
            budgets,
            events,
        }
    }

    fn script(&self, act: Act) {
        *self.act.lock().unwrap() = act;
    }

    async fn send(&self, to: AgentId, body: &str) -> MessageId {
        self.rig.send(owner(), Address::Agent(to), body).await
    }

    /// The executor's next entry.
    async fn entered(&mut self) -> MessageId {
        guarded(self.entered.recv()).await.unwrap()
    }

    async fn worked(&mut self) -> MessageId {
        guarded(self.worked.recv()).await.unwrap()
    }

    /// Every executor entry so far that the test has not taken.
    fn entries(&mut self) -> Vec<MessageId> {
        std::iter::from_fn(|| self.entered.try_recv().ok()).collect()
    }

    /// Move the paused clock; every job runs through the span.
    async fn pass(&self, seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    /// The next committed event that matches, once the service has folded it.
    async fn event(&mut self, what: &str, matches: impl Fn(&OrgEvent) -> bool) -> OrgEvent {
        let found = tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let envelope = self.events.recv().await.unwrap();
                let event = OrgEvent::from_parts(&envelope.kind, &envelope.body)
                    .unwrap()
                    .unwrap();
                if matches(&event) {
                    return (envelope.seq, event);
                }
            }
        })
        .await;
        let (seq, event) = found.unwrap_or_else(|_| panic!("never committed: {what}"));
        self.rig.folded(seq).await;
        event
    }

    /// A deadline makes a note due; only its committed fold makes it visible.
    async fn notice(&mut self, id: MessageId, reason: &str) {
        let expected = key(id, reason);
        self.event(reason, |event| {
            matches!(event, OrgEvent::MessageSent(content) if content.delivery_notice.as_ref() == Some(&expected))
        })
        .await;
    }

    /// Every committed event the test has not taken yet.
    fn committed(&mut self) -> Vec<OrgEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .map(|e| OrgEvent::from_parts(&e.kind, &e.body).unwrap().unwrap())
            .collect()
    }

    async fn entry(&self, agent: AgentId, id: MessageId) -> InboxEntry {
        let p = InboxParams {
            recipient: Some(Address::Agent(agent)),
            id: Some(id),
            ..page()
        };
        self.rig.inbox(owner(), p).await.messages.remove(0)
    }

    /// The system's notes in the owner's inbox, oldest first.
    async fn notes(&self) -> Vec<String> {
        let inbox = self.rig.inbox(owner(), page()).await;
        (inbox.messages.into_iter())
            .filter(|entry| entry.content.from == Address::System)
            .map(|entry| entry.content.body)
            .collect()
    }

    /// A session event the tap accepted: activity, never a receipt.
    fn activity(&self, session: &str, pane: &str) {
        let rt = &self.rig.rt;
        let event = LifecycleEvent::WorkingStart;
        (rt.evidence)
            .tap(
                &SessionId::new(session),
                Some(pane),
                Some(Harness::Pi),
                Some(&event),
                true,
                Some(&rt.history),
            )
            .unwrap();
    }

    /// The message's exact reference submitted in this session and pane.
    fn reference(&self, id: MessageId, session: &str, pane: &str) {
        let rt = &self.rig.rt;
        let event = LifecycleEvent::PromptSubmit {
            prompt: Some(inbox_reference(id)),
        };
        (rt.evidence)
            .tap(
                &SessionId::new(session),
                Some(pane),
                Some(Harness::Pi),
                Some(&event),
                true,
                Some(&rt.history),
            )
            .unwrap();
    }

    async fn retry(&self, id: MessageId) -> Result<DeliveryStatus, MessageError> {
        let result = guarded(self.rig.messages.retry(ResolveParams { id }, owner())).await;
        result.map(|result| result.status)
    }

    async fn give_up(&self, id: MessageId) -> Result<DeliveryStatus, MessageError> {
        let result = guarded(self.rig.messages.give_up(
            GiveUpParams {
                id,
                relay_words: None,
            },
            owner(),
        ))
        .await;
        result.map(|result| result.status)
    }

    async fn give_up_as(
        &self,
        id: MessageId,
        caller: CallContext,
        words: Option<&str>,
    ) -> Result<DeliveryStatus, MessageError> {
        let p = GiveUpParams {
            id,
            relay_words: words.map(str::to_owned),
        };
        let result = guarded(self.rig.messages.give_up(p, caller)).await;
        result.map(|result| result.status)
    }

    /// One message to the worker, pasted without a receipt, then session
    /// activity: after the confirmation timeout it is held as probably
    /// delivered, and the owner has its notice.
    async fn probably_delivered(&mut self) -> MessageId {
        let id = self.send(worker(), "brief").await;
        assert_eq!(self.entered().await, id);
        assert_eq!(self.worked().await, id);
        self.activity("s1", "%1");
        self.event(
            "the blocked notice",
            |event| matches!(event, OrgEvent::MessageSent(note) if note.from == Address::System),
        )
        .await;
        id
    }
}

/// Drive completed scheduler rounds, not elapsed delays: the service and
/// organisation writer are real, but no background recipient job is spawned.
/// This lets negative notice assertions synchronize on the returned round.
struct ManualJob {
    job: Job,
    messages: super::super::messages::MessageHandle,
    screens: Screens,
    gate: Arc<Semaphore>,
    fault: Arc<Mutex<Option<crate::eventlog::EventLogError>>>,
    appends: mpsc::UnboundedReceiver<()>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    journal: Arc<Mutex<Vec<Envelope>>>,
    cancel: tokio_util::sync::CancellationToken,
}

impl ManualJob {
    async fn new() -> Self {
        Self::from_records(Vec::new()).await
    }

    async fn from_records(records: Vec<Envelope>) -> Self {
        use super::super::{
            message_service_tests::runtime, messages::spawn_service_for_test, Request,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        let (requests, receive) = mpsc::channel::<Request>(64);
        let fence = records.last().map_or(0, |e| e.seq);
        let mut state = atm_org::OrgState::default();
        for record in &records {
            if let Some(event) = OrgEvent::from_parts(&record.kind, &record.body).unwrap() {
                state.apply(record.seq, record.ts, &event).unwrap();
            }
        }
        let journal = Arc::new(Mutex::new(records.clone()));
        let written = journal.clone();
        let (health, status) = watch::channel(Ok(fence));
        let (committed, _) = broadcast::channel(1024);
        let cancel = tokio_util::sync::CancellationToken::new();
        let seq = AtomicU64::new(fence);
        let gate = Arc::new(Semaphore::new(1));
        let append_gate = gate.clone();
        let fault = Arc::new(Mutex::new(None));
        let append_fault = fault.clone();
        let (entered, appends) = mpsc::unbounded_channel();
        let actor = tokio::spawn(super::super::actor::run(
            state,
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |mut envelope| {
                let written = written.clone();
                let fault = append_fault.lock().unwrap().take();
                let next = if fault.is_none()
                    || matches!(
                        fault,
                        Some(crate::eventlog::EventLogError::TimeoutAfterHandover)
                    ) {
                    seq.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    0
                };
                let gate = append_gate.clone();
                let _ = entered.send(());
                async move {
                    let _permit = gate.acquire().await.unwrap();
                    match fault {
                        Some(error) => {
                            // A complete record reached disk, but the writer cannot
                            // acknowledge it; the real actor must disable itself.
                            if matches!(error, crate::eventlog::EventLogError::TimeoutAfterHandover)
                            {
                                envelope.seq = next;
                                written.lock().unwrap().push(envelope);
                            }
                            Err(error)
                        }
                        None => {
                            envelope.seq = next;
                            written.lock().unwrap().push(envelope);
                            Ok(next)
                        }
                    }
                }
            },
        ));
        let org = super::super::OrgHandle {
            requests,
            health: status,
            committed,
            reads: None,
        };
        let rt = runtime(org.clone(), fence, &cancel);
        let (messages, service_task) =
            spawn_service_for_test(rt, org.committed.subscribe(), fence, None);
        for record in &records {
            org.committed.send(Arc::new(record.clone())).unwrap();
        }
        guarded(messages.initialized()).await.unwrap();
        let screens = Screens::default();
        let service = messages.service().unwrap();
        let job = Job::new(
            service,
            Effects {
                deliver: Arc::new(|_, _, _| {
                    Box::pin(async { panic!("busy message must not dispatch input") })
                }),
                observe: screens.effect(),
                retract: no_retract(),
            },
            worker(),
        );
        let rig = Self {
            job,
            messages,
            screens,
            gate,
            fault,
            appends,
            tasks: vec![actor, service_task],
            journal,
            cancel,
        };
        if fence != 0 {
            return rig;
        }
        rig.command(OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        })
        .await;
        rig.command(OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        })
        .await;
        rig
    }

    async fn command(&self, command: OrgCommand) {
        guarded(self.job.commit(command)).await.unwrap();
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        self.gate.add_permits(1);
        for task in self.tasks.drain(..) {
            guarded(task).await.unwrap();
        }
    }

    async fn send(&self) -> MessageId {
        self.messages
            .send(
                super::super::message_service_tests::note(Address::Agent(worker()), "brief"),
                owner(),
            )
            .await
            .unwrap()
            .message_id
    }

    fn turn(&self, turn: atm_core::TurnState) {
        self.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.turn = turn;
                o.facts.input = InputState::Draft;
            }),
        );
    }

    /// Synthetic original exit; scheduling fake tests do not claim pane input.
    async fn recovery_original() -> (Self, MessageId, AttemptScope) {
        Self::new()
            .await
            .held_original(ExecutorHold::InputOutcomeUnknown, EffectCertainty::Applied)
            .await
    }

    async fn held_original(
        self,
        hold: ExecutorHold,
        paste: EffectCertainty,
    ) -> (Self, MessageId, AttemptScope) {
        let mut rig = self;
        rig.turn(atm_core::TurnState::NotRunning);
        let id = rig.send().await;
        guarded(rig.job.observe()).await.unwrap();
        rig.command(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: "%1".into(),
        })
        .await;
        let rt = &rig.job.service.rt;
        let now = rt.clock.now().unwrap();
        let seq = rt
            .history
            .snapshot(id, now)
            .unwrap()
            .last_attempt_seq
            .unwrap();
        let scope = AttemptScope {
            message_id: id,
            attempt_seq: seq,
            recipient: worker(),
            pane: PaneId::parse("%1").unwrap(),
            harness: Harness::Pi,
            session_id: Some("s1".into()),
            excluded_sessions: HashSet::new(),
        };
        rt.evidence.register(id, inbox_reference(id)).unwrap();
        rt.evidence.arm(scope.clone()).unwrap();
        for update in [
            HistoryUpdate::Armed {
                scope: scope.clone(),
            },
            HistoryUpdate::Paste {
                id,
                pane: scope.pane.clone(),
                certainty: paste,
            },
            HistoryUpdate::Held {
                id,
                attempt_seq: seq,
                hold,
            },
            HistoryUpdate::AttemptExited {
                id,
                attempt_seq: seq,
                paste,
                enter: EffectCertainty::NotStarted,
                cancelled: false,
            },
        ] {
            rt.history.record(update, now).unwrap();
        }
        tokio::time::advance(Duration::from_secs(4)).await;
        (rig, id, scope)
    }

    async fn recovery_round(&mut self) -> Next {
        self.job.observed = None;
        let mut queued = self.job.service.board.queued.subscribe();
        guarded(self.job.round(&mut queued)).await.unwrap()
    }

    async fn give_up(&mut self, id: MessageId) -> GiveUpResult {
        let messages = self.messages.clone();
        let call = spawn_polled(async move {
            messages
                .give_up(
                    GiveUpParams {
                        id,
                        relay_words: None,
                    },
                    owner(),
                )
                .await
        });
        self.round(0).await;
        guarded(call).await.unwrap().unwrap()
    }

    async fn round(&mut self, advance_ms: u64) {
        tokio::time::advance(Duration::from_millis(advance_ms)).await;
        // Supply a new observation even at the 1 ms boundary; this tests the
        // decision, not polling cadence. All work finishes before assertions.
        self.job.observed = None;
        let mut queued = self.job.service.board.queued.subscribe();
        guarded(self.job.round(&mut queued)).await.unwrap();
    }

    async fn notes(&self) -> Vec<String> {
        self.messages
            .inbox(page(), owner())
            .await
            .unwrap()
            .messages
            .into_iter()
            .filter(|entry| entry.content.from == Address::System)
            .map(|entry| entry.content.body)
            .collect()
    }
}

impl Drop for ManualJob {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.gate.add_permits(1);
        // These tasks use only in-memory I/O; abort also covers assertion unwind.
        for task in &self.tasks {
            task.abort();
        }
    }
}

// Break caught: a historically eligible held draft has no observation wake,
// or an Empty/Unknown preview spends its only claim / a used one dispatches twice.
#[tokio::test(start_paused = true)]
async fn recovery_candidate_reobserves_but_used_hold_does_not() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, id, scope) = ManualJob::recovery_original().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    rig.job.effects.deliver = Arc::new(move |rt, id, _| {
        let called = called.clone();
        Box::pin(async move {
            called.fetch_add(1, Ordering::SeqCst);
            let now = rt.clock.now()?;
            let scope = rt.history.recovery_candidate(id, now)?.unwrap();
            let claim = rt.evidence.claim_recovery(&rt.history, scope, false, now)?;
            drop(claim);
            Ok(())
        })
    });
    rig.screens.set(worker(), ready("%1", "s1"));
    assert!(
        matches!(rig.recovery_round().await, Next::At(_)),
        "eligible Empty preview needs an observation wake"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    rig.turn(atm_core::TurnState::Unknown);
    assert!(matches!(rig.recovery_round().await, Next::At(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    rig.turn(atm_core::TurnState::NotRunning);
    assert!(matches!(rig.recovery_round().await, Next::Again));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.job
            .service
            .rt
            .history
            .snapshot(id, rig.job.service.rt.clock.now().unwrap())
            .unwrap()
            .last_attempt_seq,
        Some(scope.attempt_seq)
    );
    assert!(matches!(rig.recovery_round().await, Next::Change));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.job.service.rt.history.recovery_registrations().unwrap(),
        0
    );
    rig.stop().await;
}

// Break caught: an accepted owner decision leaves an active final guard valid
// while waiting behind its own recipient's reserved recovery.
#[tokio::test(start_paused = true)]
async fn recovery_owner_resolution_revokes_before_recipient_slot() {
    for retry in [false, true] {
        let (mut rig, id, scope) = ManualJob::recovery_original().await;
        let rt = &rig.job.service.rt;
        let claim = rt
            .evidence
            .claim_recovery(&rt.history, scope, false, rt.clock.now().unwrap())
            .unwrap();
        let view = claim.guard();
        let mut queued = rig.job.service.board.queued.subscribe();
        let messages = rig.messages.clone();
        let request = spawn_polled(async move {
            if retry {
                messages
                    .retry(ResolveParams { id }, owner())
                    .await
                    .map(|r| r.status)
            } else {
                messages
                    .give_up(
                        GiveUpParams {
                            id,
                            relay_words: None,
                        },
                        owner(),
                    )
                    .await
                    .map(|r| r.status)
            }
        });
        guarded(queued.changed()).await.unwrap();
        assert!(
            !view.is_valid(),
            "retry={retry}: still valid before recipient slot returned"
        );
        drop(claim);
        rig.round(0).await;
        guarded(request).await.unwrap().unwrap();
        assert_eq!(
            rig.job.service.rt.history.recovery_registrations().unwrap(),
            0
        );
        rig.stop().await;
    }
}

// Break caught: cancellation drains jobs before invalidating a claimed key.
#[tokio::test(start_paused = true)]
async fn recovery_service_shutdown_revokes_claim_before_join() {
    let (rig, _, scope) = ManualJob::recovery_original().await;
    let rt = &rig.job.service.rt;
    let claim = rt
        .evidence
        .claim_recovery(&rt.history, scope, false, rt.clock.now().unwrap())
        .unwrap();
    let view = claim.guard();
    rig.cancel.cancel();
    guarded(run(rig.job.service.clone(), rig.job.effects.clone())).await;
    assert!(!view.is_valid());
    drop(claim);
    assert_eq!(rt.history.recovery_registrations().unwrap(), 0);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn first_terminal_evaluation_does_not_retain_busy_turn_bookkeeping() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    guarded(rig.job.observe()).await.unwrap();
    // Model a terminal commit racing the first evaluation after observation.
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "owner cancelled".into(),
        final_failure: true,
    })
    .await;
    let mut queued = rig.job.service.board.queued.subscribe();
    let next = guarded(rig.job.step(id, &mut queued)).await.unwrap();
    assert!(matches!(next, Next::Change));
    assert!(!rig.job.shown.contains(&id));
    let shared = rig.job.service.board.shared.lock().unwrap();
    assert!(!shared.progress.contains_key(&id));
    assert!(rig.job.pending_notice.is_none());
}

#[tokio::test(start_paused = true)]
async fn running_turn_notice_at_30_minutes_once() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.round(0).await;
    rig.command(OrgCommand::Revalidate { agent_id: worker() })
        .await;
    rig.round(1_799_999).await;
    assert!(rig.notes().await.is_empty());
    rig.round(1).await;
    let expected = format!("demo/worker busy for 30 min; message still queued ({id})");
    assert_eq!(rig.notes().await, std::slice::from_ref(&expected));
    rig.round(1_800_000).await;
    assert_eq!(rig.notes().await, [expected]);
    let now = rig.job.service.rt.clock.now().unwrap();
    let snapshot = rig.job.service.rt.history.snapshot(id, now).unwrap();
    assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
    assert!(snapshot.message.last_attempt.is_none());
    assert!(snapshot.message.failure.is_none());
    assert_eq!(
        rig.job.service.board.progress(id).phase,
        Some(DeliveryPhase::Waiting)
    );
}

#[tokio::test(start_paused = true)]
async fn busy_notice_waits_for_each_messages_own_time_and_goes_to_owner() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let first = rig.send().await;
    rig.round(0).await;
    rig.round(900_000).await;
    // Even an agent sender receives no new note addressed to its pane.
    let second = rig
        .messages
        .send(
            super::super::message_service_tests::note(Address::Agent(worker()), "second"),
            pane("%1"),
        )
        .await
        .unwrap()
        .message_id;
    rig.round(900_000).await;
    assert_eq!(
        rig.notes().await,
        [format!(
            "demo/worker busy for 30 min; message still queued ({first})"
        )]
    );
    rig.round(900_000).await;
    assert_eq!(rig.notes().await.len(), 2);
    assert_eq!(
        rig.notes().await.last().unwrap(),
        &format!("demo/worker busy for 30 min; message still queued ({second})")
    );
    let inbox = rig
        .messages
        .inbox(
            InboxParams {
                recipient: Some(Address::Agent(worker())),
                ..page()
            },
            owner(),
        )
        .await
        .unwrap();
    assert_eq!(inbox.messages.len(), 2);
    assert!(inbox
        .messages
        .iter()
        .all(|entry| entry.status == DeliveryStatus::Queued));
}

#[tokio::test(start_paused = true)]
async fn ended_turn_has_no_busy_notice_and_a_new_turn_restarts_the_interval() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.round(0).await;
    rig.turn(atm_core::TurnState::NotRunning);
    rig.round(1_799_999).await;
    assert!(rig.notes().await.is_empty());
    rig.turn(atm_core::TurnState::Running);
    rig.round(1).await;
    rig.round(1_799_999).await;
    assert!(rig.notes().await.is_empty());
    rig.round(1).await;
    assert_eq!(rig.notes().await.len(), 1);
    rig.turn(atm_core::TurnState::NotRunning);
    rig.round(1).await;
    rig.turn(atm_core::TurnState::Running);
    rig.round(1).await;
    rig.round(1_799_999).await;
    assert_eq!(rig.notes().await.len(), 1);
    rig.round(1).await;
    assert_eq!(rig.notes().await.len(), 1);
    // Terminal cleanup must not remove the committed lifetime key.
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "owner cancelled".into(),
        final_failure: true,
    })
    .await;
    rig.round(0).await;
    assert!(rig
        .job
        .service
        .has_delivery_notice(&key(id, "running_turn"))
        .unwrap());
    assert!(rig.job.pending_notice.is_none());
    let shared = rig.job.service.board.shared.lock().unwrap();
    assert!(!shared.progress.contains_key(&id));
}

#[tokio::test(start_paused = true)]
async fn failed_observations_clear_busy_notice_evidence_without_restarting_readiness() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.round(0).await;
    rig.command(OrgCommand::Revalidate { agent_id: worker() })
        .await;
    rig.screens.lose(&worker());
    rig.round(10_000).await;
    let lost = rig.job.service.rt.clock.now().unwrap();
    let loss_notice = unobserved(id);
    for (elapsed, loss_noted) in [(0, false), (120_000, true), (1_800_000, true)] {
        rig.round(elapsed).await;
        let now = rig.job.service.rt.clock.now().unwrap();
        let live = rig.job.service.rt.history.snapshot(id, now).unwrap().live;
        assert_eq!(live.running_turn_since, None);
        assert_eq!(live.agent_not_ready_since, Some(lost));
        assert_eq!(
            rig.job.service.board.progress(id).reason.as_deref(),
            Some("recipient cannot be observed")
        );
        if loss_noted {
            assert_eq!(rig.notes().await, std::slice::from_ref(&loss_notice));
        } else {
            assert!(rig.notes().await.is_empty());
        }
    }
    rig.turn(atm_core::TurnState::Running);
    rig.round(0).await;
    let resumed = rig.job.service.rt.clock.now().unwrap();
    let live = rig
        .job
        .service
        .rt
        .history
        .snapshot(id, resumed)
        .unwrap()
        .live;
    assert_eq!(live.running_turn_since, Some(resumed));
    assert_eq!(live.agent_not_ready_since, None);
    rig.round(1_799_999).await;
    assert_eq!(rig.notes().await, std::slice::from_ref(&loss_notice));
    rig.round(1).await;
    assert_eq!(
        rig.notes().await,
        [
            loss_notice,
            format!("demo/worker busy for 30 min; message still queued ({id})")
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn busy_notice_unknown_commit_is_not_repeated_and_known_refusal_can_retry() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.round(0).await;
    rig.gate.forget_permits(usize::MAX);
    // The real writer accepts but cannot append until released. The paused
    // clock reaches the commit budget; its unknown outcome stays deduplicated.
    tokio::time::advance(Duration::from_secs(1_800)).await;
    let mut queued = rig.job.service.board.queued.subscribe();
    assert!(guarded(rig.job.round(&mut queued)).await.is_err());
    rig.gate.add_permits(1);
    guarded(rig.job.service.rt.org.snapshot_at()).await.unwrap();
    rig.round(0).await;
    rig.round(1_800_000).await;
    assert_eq!(rig.notes().await.len(), 1);
    assert_eq!(
        rig.job.service.board.progress(id).phase,
        Some(DeliveryPhase::Waiting)
    );

    // A known refusal is different: the same publication key can retry.
    let mut invalid = note("known refusal".into());
    invalid.to = Address::System;
    rig.job
        .publish(id, "test refusal", invalid)
        .await
        .unwrap_err();
    rig.job
        .publish(id, "test refusal", note("retry accepted".into()))
        .await
        .unwrap();
    rig.job
        .publish(id, "test refusal", note("duplicate".into()))
        .await
        .unwrap();
    assert_eq!(rig.notes().await.len(), 2);
    assert_eq!(rig.notes().await.last().unwrap(), "retry accepted");
}

fn system_note(event: &OrgEvent) -> bool {
    matches!(event, OrgEvent::MessageSent(note) if note.from == Address::System)
}

fn attempted(event: &OrgEvent) -> Option<(MessageId, u32, &str)> {
    match event {
        OrgEvent::MessageDeliveryAttempted {
            message_id,
            attempt,
            pane,
        } => Some((*message_id, *attempt, pane.as_str())),
        _ => None,
    }
}

fn delivered(event: &OrgEvent) -> Option<MessageId> {
    match event {
        OrgEvent::MessageDelivered { message_id, .. } => Some(*message_id),
        _ => None,
    }
}

fn failed(event: &OrgEvent) -> Option<(MessageId, &str, bool)> {
    match event {
        OrgEvent::MessageDeliveryFailed {
            message_id,
            reason,
            final_failure,
        } => Some((*message_id, reason.as_str(), *final_failure)),
        _ => None,
    }
}

const PROBABLY: &str = "unconfirmed, probably delivered; inspect before resending";

#[test]
fn blocked_notice_text_is_pinned() {
    let (id, next, last) = (MessageId::new(5), MessageId::new(7), MessageId::new(9));
    assert_eq!(
        blocked_notice(id, "dialog is open", &[], false),
        "m5 blocked: dialog is open; waiting behind it: none. \
         Delivery resumes when this clears; atm wake give-up m5 cancels it"
    );
    assert_eq!(
        blocked_notice(id, PROBABLY, &[next, last], true),
        "m5 blocked: unconfirmed, probably delivered; inspect before resending; \
         waiting behind it: m7, m9. Inspect the pane; \
         atm wake retry m5 --accept-duplicate-risk or atm wake give-up m5"
    );
    // The executor's diagnostic replaces the policy's words for its own holds.
    let reason = |hold| blocked_reason(FailureReason::ProbablyDelivered, hold);
    assert_eq!(reason(None), PROBABLY);
    assert_eq!(reason(Some(ExecutorHold::TokenVisible)), PROBABLY);
    assert_eq!(
        reason(Some(ExecutorHold::InputOutcomeUnknown)),
        "input outcome is unknown"
    );
    assert_eq!(
        reason(Some(ExecutorHold::StagedMismatch)),
        "staged draft does not exactly match; Enter was not sent"
    );
    assert_eq!(
        reason(Some(ExecutorHold::EnterNotSent)),
        "reference token was seen; editor staging is unproven; Enter was not sent"
    );
    assert_eq!(
        blocked_reason(FailureReason::Restarted, Some(ExecutorHold::StagedMismatch)),
        FailureReason::Restarted.to_string()
    );
}

#[tokio::test(start_paused = true)]
async fn ready_recipient_is_attempted_once_in_fifo_order() {
    let mut bench = Bench::new().await;
    bench.script(Act::Deliver);
    close(&bench.cleanup);
    let first = bench.send(worker(), "first").await;
    let second = bench.send(worker(), "second").await;
    assert_eq!(bench.entered().await, first);
    assert_eq!(bench.worked().await, first);
    // The attempt has not returned from its cleanup: nothing else is decided
    // for this recipient, however long it takes.
    bench.pass(60).await;
    assert!(bench.entries().is_empty());
    assert_eq!(
        bench.entry(worker(), second).await.delivery.blocked_by,
        None
    );
    bench.cleanup.add_permits(OPEN);
    assert_eq!(bench.entered().await, second);
    bench
        .event("second delivered", |e| delivered(e) == Some(second))
        .await;
    assert_eq!(*bench.budgets.lock().unwrap(), [ATTEMPT_WORK_BUDGET; 2]);
    // Nothing is kept on the board for a message that is no longer open.
    let board = &bench.rig.messages.service().unwrap().board;
    until("the board forgets delivered messages", || {
        [first, second]
            .iter()
            .all(|id| board.progress(*id) == Progress::default())
    })
    .await;
    let view = bench.entry(worker(), first).await;
    assert_eq!(view.status, DeliveryStatus::Delivered);
    assert_eq!(view.delivery.attempt, 1);
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn follower_waits_behind_an_unattempted_head() {
    let bench = Bench::blind().await;
    bench.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.copy_mode = Some(true)),
    );
    let first = bench.send(worker(), "first").await;
    let second = bench.send(worker(), "second").await;
    bench.pass(5).await;
    let (head, follower) = (
        bench.entry(worker(), first).await.delivery,
        bench.entry(worker(), second).await.delivery,
    );
    assert_eq!(head.phase, DeliveryPhase::Waiting);
    assert_eq!(head.reason.as_deref(), Some("pane is in copy mode"));
    assert_eq!(follower.phase, DeliveryPhase::Waiting);
    assert_eq!(
        follower.reason.as_deref(),
        Some("another delivery is in flight on this pane")
    );
    assert_eq!(follower.blocked_by, Some(first));
}

#[tokio::test(start_paused = true)]
async fn refusing_a_clear_start_line_keeps_guards_and_give_up_releases_fifo() {
    for dialog in [false, true] {
        let mut bench = Bench::blind().await;
        bench.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.turn = TurnState::Running;
            }),
        );
        let older = bench.send(worker(), "older briefing").await;
        bench
            .rig
            .run(OrgCommand::Clear { agent_id: worker() })
            .await;
        let start = MessageId::new(
            bench
                .rig
                .run(OrgCommand::SendMessage(
                    super::super::start_line::start_line(&worker()),
                ))
                .await,
        );
        bench.screens.set(
            worker(),
            with(ready("%1", "fresh"), |o| {
                if dialog {
                    o.facts.dialog = DialogState::Permission;
                } else {
                    o.facts.input = InputState::Draft;
                }
            }),
        );
        bench
            .rig
            .run(OrgCommand::SessionBound {
                agent_id: worker(),
                session_id: "fresh".into(),
            })
            .await;
        let newer = bench.send(worker(), "newer briefing").await;
        let notice = bench.event("blocked head notice", |e| {
            matches!(e, OrgEvent::MessageSent(note) if note.to == Address::Owner && note.body.contains(" blocked: "))
        }).await;
        let OrgEvent::MessageSent(note) = notice else {
            panic!("expected blocked notice")
        };
        assert!(
            note.body.starts_with(&format!("{start} blocked: ")),
            "{}",
            note.body
        );
        assert!(
            note.body
                .contains(&format!("waiting behind it: {older}, {newer}.")),
            "{}",
            note.body
        );
        assert!(bench.entries().is_empty());
        for id in [older, newer] {
            let row = bench.entry(worker(), id).await;
            assert_eq!(row.status, DeliveryStatus::Queued);
            assert_eq!(row.delivery.blocked_by, Some(start));
        }
        assert_eq!(bench.give_up(start).await.unwrap(), DeliveryStatus::Failed);
        assert_eq!(
            bench.entry(worker(), newer).await.delivery.blocked_by,
            Some(older)
        );
        assert!(bench.entries().is_empty()); // give-up does not clear a draft or dialog
        bench.script(Act::Deliver);
        bench.screens.set(worker(), ready("%1", "fresh"));
        assert_eq!(bench.entered().await, older);
        assert_eq!(bench.entered().await, newer);
        bench
            .event("ordinary follower delivered", |e| {
                delivered(e) == Some(newer)
            })
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn at_most_32_recipient_jobs_and_the_rest_stay_queued() {
    let mut bench = Bench::blind().await;
    let mut sent = BTreeMap::new();
    for n in 0..=MAX_JOBS {
        let agent: AgentId = format!("demo/a{n:02}").parse().unwrap();
        let (pane, session) = (format!("%{}", 10 + n), format!("t{n}"));
        bench
            .rig
            .run(OrgCommand::Adopt {
                agent_id: agent.clone(),
                pane: pane.clone(),
                session_id: Some(session.as_str().into()),
            })
            .await;
        // Copy mode: every job keeps polling and none can paste.
        let screen = with(ready(&pane, &session), |o| o.copy_mode = Some(true));
        bench.screens.set(agent.clone(), screen);
        sent.insert(agent.clone(), bench.send(agent, "brief").await);
    }
    bench.pass(10).await;
    let observed = bench.screens.observed();
    assert_eq!(observed.len(), MAX_JOBS);
    let left_out = (sent.keys())
        .find(|agent| !observed.contains(agent))
        .unwrap()
        .clone();
    // Its message is durable and simply not scheduled yet.
    let waiting = bench.entry(left_out.clone(), sent[&left_out]).await;
    assert_eq!(waiting.status, DeliveryStatus::Queued);
    assert_eq!(waiting.delivery.phase, DeliveryPhase::Queued);
    // A second message and more wakes start no second job for a recipient:
    // it is still observed once per guard poll.
    let busy = observed[0].clone();
    let before = bench.screens.calls(&busy);
    bench.send(busy.clone(), "another").await;
    bench.pass(10).await;
    let polls = bench.screens.calls(&busy) - before;
    assert!((30..=41).contains(&polls), "{polls} observations in 10 s");
    assert_eq!(bench.screens.calls(&left_out), 0);
    // A job that ends makes room: the recipient left out is found again.
    let done = observed[1].clone();
    assert_eq!(
        bench.give_up(sent[&done]).await.unwrap(),
        DeliveryStatus::Failed
    );
    bench.pass(1).await;
    assert!(bench.screens.calls(&left_out) > 0);
    assert!(bench.entries().is_empty());
}

#[tokio::test(start_paused = true)]
async fn eight_admissions_leave_lifecycle_capacity() {
    let mut bench = Bench::blind().await;
    close(&bench.cleanup);
    for n in 0..=MESSAGE_ADMISSION_LIMIT {
        let agent: AgentId = format!("demo/a{n}").parse().unwrap();
        let (pane, session) = (format!("%{}", 10 + n), format!("t{n}"));
        bench
            .rig
            .run(OrgCommand::Adopt {
                agent_id: agent.clone(),
                pane: pane.clone(),
                session_id: Some(session.as_str().into()),
            })
            .await;
        bench.screens.set(agent.clone(), ready(&pane, &session));
        bench.send(agent, "brief").await;
    }
    for _ in 0..MESSAGE_ADMISSION_LIMIT {
        bench.worked().await;
    }
    // Eight attempts have finished their work and none has left its cleanup:
    // every permit is still held, and the ninth recipient has not entered.
    bench.pass(60).await;
    assert_eq!(bench.entries().len(), MESSAGE_ADMISSION_LIMIT);
    assert_eq!(bench.rig.rt.admissions.available_permits(), 0);
    bench.cleanup.add_permits(1);
    bench.entered().await;
    assert!(bench.entries().is_empty());
    assert_eq!(bench.rig.rt.admissions.available_permits(), 0);
}

#[tokio::test(start_paused = true)]
async fn owner_message_starts_no_job() {
    let bench = Bench::new().await;
    bench.rig.send(pane("%1"), Address::Owner, "report").await;
    bench.pass(30).await;
    assert!(bench.screens.observed().is_empty());
}

const UNOBSERVED_REASON: &str = "recipient cannot be observed";

/// The pinned text of the one notice for a recipient nobody could observe.
fn unobserved(id: MessageId) -> String {
    format!(
        "{id} waiting: recipient demo/worker could not be observed for 120 s. \
         It stays queued and is delivered when the pane can be read again; \
         atm wake give-up {id} cancels it"
    )
}

async fn waits_unobserved(bench: &Bench, id: MessageId) {
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Queued);
    assert_eq!(view.delivery.phase, DeliveryPhase::Waiting);
    assert_eq!(view.delivery.reason.as_deref(), Some(UNOBSERVED_REASON));
}

// Break caught: collapse an owned shell-list refusal into the generic wait,
// retain its text on the next unrelated failure, or surface arbitrary errors.
#[tokio::test(start_paused = true)]
async fn shell_list_loss_shows_only_its_owned_wait() {
    const MISSING: &str = "/etc/shells is missing (using built-in shell names)";
    const UNREADABLE: &str = "/etc/shells could not be read (using built-in shell names)";
    const EMPTY: &str = "/etc/shells has no usable shell entries";
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    let cases = [
        (MessageError::Refused(MISSING.into()), MISSING),
        (MessageError::Unavailable(MISSING.into()), UNOBSERVED_REASON),
        (MessageError::Refused(UNREADABLE.into()), UNREADABLE),
        (MessageError::Refused(EMPTY.into()), EMPTY),
        (MessageError::Refused(UNCONFIRMED.into()), UNCONFIRMED),
        (
            MessageError::Refused(format!("{MISSING}; other detail")),
            UNOBSERVED_REASON,
        ),
        (MessageError::Timeout, UNOBSERVED_REASON),
    ];
    for (error, want) in cases {
        let error = Arc::new(Mutex::new(Some(error)));
        rig.job.effects.observe = Arc::new(move |_, _| {
            let error = error.lock().unwrap().take().unwrap();
            Box::pin(async move { Err(error) })
        });
        // The owned round completes; the default executor panics if entered.
        rig.round(0).await;
        let entry = rig
            .messages
            .inbox(
                InboxParams {
                    recipient: Some(Address::Agent(worker())),
                    id: Some(id),
                    ..page()
                },
                owner(),
            )
            .await
            .unwrap()
            .messages
            .pop()
            .unwrap();
        assert_eq!(entry.status, DeliveryStatus::Queued);
        assert_eq!(entry.delivery.phase, DeliveryPhase::Waiting);
        assert_eq!(entry.delivery.reason.as_deref(), Some(want));
    }
}

#[tokio::test(start_paused = true)]
async fn unobservable_recipient_says_so_tells_the_owner_once_and_never_pastes() {
    let mut bench = Bench::blind().await;
    let id = bench.send(worker(), "brief").await;
    // At once: nobody knows where the owner is, and the inbox does not
    // claim it.
    bench.pass(1).await;
    assert!(bench.screens.calls(&worker()) > 0);
    waits_unobserved(&bench, id).await;
    // Nothing is told before the loss has lasted the readiness timeout,
    bench.pass(118).await;
    assert!(bench.notes().await.is_empty());
    // one notice at it, by the job's own wake,
    bench.pass(2).await;
    bench.notice(id, "unobserved").await;
    assert_eq!(bench.notes().await, [unobserved(id)]);
    // and none after that. The message only waits: nothing is held.
    bench.pass(3600).await;
    assert_eq!(bench.notes().await.len(), 1);
    waits_unobserved(&bench, id).await;
    assert_eq!(code(bench.retry(id).await.unwrap_err()), "conflict");
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    // A pane that was read, with a focus that could not be, is no loss: the
    // policy waits as for a focused pane, and the owner is not told they are
    // on it.
    bench
        .screens
        .set(worker(), with(ready("%1", "s1"), |o| o.focused = None));
    bench.pass(600).await;
    assert_eq!(
        bench.entry(worker(), id).await.delivery.reason.as_deref(),
        Some("owner is focused on this pane")
    );
    assert_eq!(bench.notes().await.len(), 1);
    assert!(bench.entries().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_observation_slower_than_five_seconds_is_a_loss() {
    let mut bench = Bench::blind().await;
    // It would be a ready pane, if the read ever returned.
    bench.screens.set(worker(), ready("%1", "s1"));
    close(&bench.screens.gate);
    let id = bench.send(worker(), "brief").await;
    bench.pass(4).await;
    // Still reading: nothing is decided, and nothing is claimed.
    assert_eq!(bench.screens.calls(&worker()), 1);
    assert_eq!(bench.entry(worker(), id).await.delivery.reason, None);
    bench.pass(2).await;
    waits_unobserved(&bench, id).await;
    // The slow read was given up, and the next began with the next guard
    // poll: one read at a time, each with its own five seconds.
    assert_eq!(bench.screens.calls(&worker()), 2);
    assert!(bench.entries().is_empty());
}

// Known exception: the coordinator has no processed-wake/admission event.
// Preserve the original scheduler-backed check until that seam exists.
#[tokio::test(start_paused = true)]
async fn one_observation_per_guard_poll_however_often_the_job_wakes() {
    let bench = Bench::blind().await;
    bench.send(worker(), "first").await;
    until("the first observation", || {
        bench.screens.calls(&worker()) == 1
    })
    .await;
    // Every committed event wakes the job, here three times in one instant.
    for body in ["second", "third", "fourth"] {
        bench.send(worker(), body).await;
    }
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert_eq!(bench.screens.calls(&worker()), 1);
    // After that, one per poll: each will be a tmux capture.
    bench.pass(10).await;
    let calls = bench.screens.calls(&worker());
    assert!((40..=42).contains(&calls), "{calls} observations in 10 s");
}

#[tokio::test(start_paused = true)]
async fn one_owned_job_observation_per_guard_poll_however_often_it_wakes() {
    // Own the actual Job::run future so each same-instant wake has a
    // processed boundary; no background job races an empty-queue assertion.
    let bench = Bench::build(Screens::default(), false).await;
    let effects = Effects {
        deliver: Arc::new(|_, _, _| Box::pin(async { panic!("blind job dispatched input") })),
        observe: bench.screens.effect(),
        retract: no_retract(),
    };
    let service = bench.rig.messages.service().unwrap();
    let mut job = Box::pin(Job::new(service, effects, worker()).run());
    bench.send(worker(), "first").await;
    poll_pending(job.as_mut()).await;
    assert_eq!(bench.screens.calls(&worker()), 1);
    // Every committed event wakes the job, here three times in one instant.
    let woke_at = Instant::now();
    for body in ["second", "third", "fourth"] {
        bench.send(worker(), body).await;
        poll_pending(job.as_mut()).await;
    }
    assert_eq!(Instant::now(), woke_at, "wake processing advanced time");
    assert_eq!(bench.screens.calls(&worker()), 1);
    // After that, one per poll: each will be a tmux capture.
    let job = spawn_polled(job);
    bench.pass(10).await;
    let calls = bench.screens.calls(&worker());
    assert!((40..=42).contains(&calls), "{calls} observations in 10 s");
    bench.rig.rt.cancel.cancel();
    assert_eq!(guarded(job).await.unwrap(), worker());
}

#[tokio::test(start_paused = true)]
async fn unobservable_stuck_recipient_keeps_its_reason_and_resumes_when_ready() {
    let mut rig = ManualJob::new().await;
    rig.command(OrgCommand::Stuck {
        agent_id: worker(),
        reason: "needs recovery".into(),
    })
    .await;
    let id = rig.send().await;
    rig.round(0).await;
    assert!(rig.job.lost.is_none());
    let now = rig.job.service.rt.clock.now().unwrap();
    let live = rig.job.service.rt.history.snapshot(id, now).unwrap().live;
    assert_eq!(live.agent_status, atm_org::AgentStatus::Stuck);
    assert!(live.owner_focused);
    let notice = blocked_notice(id, "recipient agent is stuck or down", &[], false);
    // Completed rounds before, at and beyond the 120 s loss deadline must
    // keep the blocked reason and never add an unobserved notice or paste.
    for elapsed in [0, 119_999, 1, 3_600_000] {
        rig.round(elapsed).await;
        let inbox = rig
            .messages
            .inbox(
                InboxParams {
                    recipient: Some(Address::Agent(worker())),
                    id: Some(id),
                    ..page()
                },
                owner(),
            )
            .await
            .unwrap();
        let entry = inbox.messages.first().unwrap();
        assert_eq!(entry.status, DeliveryStatus::Queued);
        assert_eq!(entry.delivery.phase, DeliveryPhase::Blocked);
        assert_eq!(
            entry.delivery.reason.as_deref(),
            Some("recipient agent is stuck or down")
        );
        assert_eq!(entry.delivery.attempt, 0);
        assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    }
    // The fake executor panics on any earlier dispatch. Now allow a real
    // scheduled attempt through the rig, including its receipt and fold.
    rig.job.effects.deliver = Arc::new(|rt, id, _| Box::pin(perform(rt, id, Act::Deliver)));
    rig.screens.set(worker(), ready("%1", "s1"));
    rig.command(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    })
    .await;
    rig.round(0).await;
    assert!(rig.job.lost.is_none());
    rig.round(10_000).await;
    rig.round(0).await;
    let now = rig.job.service.rt.clock.now().unwrap();
    let snapshot = rig.job.service.rt.history.snapshot(id, now).unwrap();
    assert_eq!(snapshot.live.agent_status, atm_org::AgentStatus::Ready);
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivered);
    assert_eq!(snapshot.message.last_attempt.unwrap().attempt, 1);
    assert_eq!(rig.notes().await, [notice]);
}

#[tokio::test(start_paused = true)]
async fn a_short_loss_after_ready_does_not_include_the_stuck_interval() {
    let mut rig = ManualJob::new().await;
    rig.command(OrgCommand::Stuck {
        agent_id: worker(),
        reason: "needs recovery".into(),
    })
    .await;
    let id = rig.send().await;
    let mut queued = rig.job.service.board.queued.subscribe();
    assert!(matches!(
        guarded(rig.job.round(&mut queued)).await.unwrap(),
        Next::Change
    ));
    let notice = blocked_notice(id, "recipient agent is stuck or down", &[], false);
    assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    assert_eq!(rig.screens.calls(&worker()), 1);
    // The blocked job waits for a change, not a poll. The readable hour is
    // not sampled and cannot count towards a later observation loss.
    rig.screens.set(worker(), ready("%1", "s1"));
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(rig.screens.calls(&worker()), 1);
    rig.screens.lose(&worker());
    rig.command(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    })
    .await;
    rig.round(0).await;
    assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    rig.round(999).await;
    assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    // At one second the pane is readable again. Unknown focus prevents
    // input without claiming the owner is there or starting another hold.
    rig.screens
        .set(worker(), with(ready("%1", "s1"), |o| o.focused = None));
    rig.round(1).await;
    assert!(rig.job.lost.is_none());
    rig.round(120_000).await;
    assert_eq!(rig.notes().await, [notice]);
}

#[tokio::test(start_paused = true)]
async fn a_sustained_loss_after_ready_gets_its_full_window_and_one_notice() {
    let mut rig = ManualJob::new().await;
    rig.command(OrgCommand::Stuck {
        agent_id: worker(),
        reason: "needs recovery".into(),
    })
    .await;
    let id = rig.send().await;
    rig.round(0).await;
    let notice = blocked_notice(id, "recipient agent is stuck or down", &[], false);
    tokio::time::advance(Duration::from_secs(3600)).await;
    rig.command(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    })
    .await;
    // Failed looks continue, but their new measured window starts at Ready.
    rig.round(0).await;
    assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    rig.round(119_999).await;
    assert_eq!(rig.notes().await, std::slice::from_ref(&notice));
    rig.round(1).await;
    assert_eq!(rig.notes().await, [notice.clone(), unobserved(id)]);
    rig.round(3_600_000).await;
    assert_eq!(rig.notes().await, [notice, unobserved(id)]);
}

#[tokio::test(start_paused = true)]
async fn one_unobserved_notice_for_the_oldest_open_message() {
    let mut bench = Bench::blind().await;
    let first = bench.send(worker(), "first").await;
    let second = bench.send(worker(), "second").await;
    bench.pass(3600).await;
    bench.notice(first, "unobserved").await;
    assert_eq!(bench.notes().await, [unobserved(first)]);
    for id in [first, second] {
        waits_unobserved(&bench, id).await;
    }
    // The owner gives up on it. The loss is the same one: the next message
    // waits on, and the owner is not told again.
    assert_eq!(bench.give_up(first).await.unwrap(), DeliveryStatus::Failed);
    bench.pass(3600).await;
    assert_eq!(bench.notes().await.len(), 1);
    waits_unobserved(&bench, second).await;
}

#[tokio::test(start_paused = true)]
async fn unobserved_notice_is_not_for_a_message_behind_a_held_one() {
    let mut bench = Bench::new().await;
    let held = bench.probably_delivered().await;
    // A guard poll later the pane can no longer be read, and a message
    // arrives: the job looks again and finds nothing.
    bench.screens.lose(&worker());
    bench.pass(1).await;
    let behind = bench.send(worker(), "behind it").await;
    bench.pass(3600).await;
    // The owner has the held message's notice; the loss adds none for the
    // message that waits behind it.
    assert_eq!(
        bench.notes().await,
        [blocked_notice(held, PROBABLY, &[], true)]
    );
    waits_unobserved(&bench, behind).await;
    assert_eq!(bench.entries(), []);
}

#[tokio::test(start_paused = true)]
async fn non_focus_wait_preserves_observation_loss_for_the_next_message() {
    let mut rig = ManualJob::new().await;
    rig.screens.set(worker(), ready("%1", "s1"));
    rig.job.effects.deliver = Arc::new(|rt, id, _| Box::pin(perform(rt, id, Act::Paste)));
    let first = rig.send().await;
    rig.round(0).await;
    rig.round(10_000).await;
    let rt = &rig.job.service.rt;
    let snapshot = rt.history.snapshot(first, rt.clock.now().unwrap()).unwrap();
    assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    assert_eq!(snapshot.message.last_attempt.unwrap().attempt, 1);

    rig.screens.lose(&worker());
    let second = rig.send().await;
    // The pasted head waits on unknown pane guards, not focus. That wait
    // must keep the loss for its follower, even just before the hold deadline.
    for advance_ms in [0, 1, 119_998] {
        rig.round(advance_ms).await;
        let head = rig.job.service.board.progress(first);
        assert_eq!(head.phase, Some(DeliveryPhase::Waiting));
        assert_eq!(head.reason.as_deref(), Some("pane guards are unknown"));
        let inbox = guarded(rig.messages.inbox(
            InboxParams {
                recipient: Some(Address::Agent(worker())),
                id: Some(second),
                ..page()
            },
            owner(),
        ))
        .await
        .unwrap();
        let entry = inbox.messages.first().unwrap();
        assert_eq!(entry.status, DeliveryStatus::Queued);
        assert_eq!(entry.delivery.phase, DeliveryPhase::Waiting);
        assert_eq!(entry.delivery.reason.as_deref(), Some(UNOBSERVED_REASON));
        assert_eq!(entry.delivery.blocked_by, Some(first));
        assert_eq!(entry.delivery.attempt, 0);
        assert!(rig.notes().await.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn an_agent_without_a_pane_ends_the_loss() {
    let mut bench = Bench::blind().await;
    let id = bench.send(worker(), "brief").await;
    bench.pass(100).await;
    // Down, so without a pane: that is known about the recipient, not lost.
    bench
        .rig
        .run(OrgCommand::Down {
            agent_id: worker(),
            reason: "stopped".into(),
        })
        .await;
    bench.pass(100).await;
    bench.notice(id, "blocked.agent_unavailable").await;
    let down = blocked_notice(id, "recipient agent is stuck or down", &[], false);
    assert_eq!(bench.notes().await, std::slice::from_ref(&down));
    // Back on a pane nobody can read: a new loss, counted from here.
    bench
        .rig
        .run(OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%5".into(),
            session_id: Some("s5".into()),
        })
        .await;
    bench.pass(119).await;
    assert_eq!(bench.notes().await.len(), 1);
    bench.pass(2).await;
    bench.notice(id, "unobserved").await;
    assert_eq!(bench.notes().await, [down, unobserved(id)]);
}

#[tokio::test(start_paused = true)]
async fn paneless_starting_observations_preserve_readiness_and_hold_clocks() {
    let mut rig = ManualJob::new().await;
    // Prime the pane-observation throttle with a failed capture. A committed
    // pane-less Starting state must be seen even within that poll interval.
    guarded(rig.job.observe()).await.unwrap();
    rig.command(OrgCommand::Down {
        agent_id: worker(),
        reason: "restarting".into(),
    })
    .await;
    rig.command(OrgCommand::Up {
        agent_id: worker(),
        role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\nWork").unwrap(),
    })
    .await;
    let started = rig.job.service.rt.clock.now().unwrap();
    let id = rig.send().await;
    for (elapsed, blocked) in [
        (0, false),
        (1, false),
        (249, false),
        (119_749, false),
        (1, true),
        (3_600_000, true),
    ] {
        tokio::time::advance(Duration::from_millis(elapsed)).await;
        // Unlike ManualJob::round, retain the last capture timestamp so the
        // first calls exercise the actual pane-observation throttle.
        let mut queued = rig.job.service.board.queued.subscribe();
        guarded(rig.job.round(&mut queued)).await.unwrap();
        let rt = &rig.job.service.rt;
        let now = rt.clock.now().unwrap();
        let snapshot = rt.history.snapshot(id, now).unwrap();
        assert_eq!(snapshot.recipient.pane, None);
        assert_eq!(snapshot.live.agent_status, atm_org::AgentStatus::Starting);
        assert!(!snapshot.live.owner_focused);
        assert_eq!(snapshot.live.pane, atm_org::delivery::PaneLiveness::Unknown);
        assert_eq!(snapshot.live.agent_not_ready_since, Some(started));
        assert_eq!(snapshot.live.hold_since, None);
        assert_eq!(snapshot.live.stale_blocker_since, None);
        assert!(rig.job.lost.is_none());
        assert_eq!(rig.screens.calls(&worker()), 1);
        let step = next_delivery(&snapshot.message, &snapshot.live, now, &rt.policy).unwrap();
        if blocked {
            assert_eq!(
                step,
                DeliveryStep::Blocked {
                    reason: FailureReason::AgentNotReadyTimeout
                }
            );
        } else {
            assert!(matches!(
                step,
                DeliveryStep::Wait {
                    reason: WaitReason::AgentNotReady,
                    ..
                }
            ));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pane_observations_are_throttled_across_frequent_job_wakes() {
    let mut rig = ManualJob::new().await;
    rig.screens
        .set(worker(), with(ready("%1", "s1"), |o| o.focused = None));
    let id = rig.send().await;
    for (advance_ms, captures) in [
        (0, 1),
        (0, 1),
        (1, 1),
        (248, 1),
        (0, 1),
        (1, 2),
        (0, 2),
        (249, 2),
        (1, 3),
    ] {
        tokio::time::advance(Duration::from_millis(advance_ms)).await;
        // Retain observed across completed rounds: wakes within 250 ms may
        // reevaluate delivery, but must not capture the same pane again.
        let mut queued = rig.job.service.board.queued.subscribe();
        guarded(rig.job.round(&mut queued)).await.unwrap();
        assert_eq!(rig.screens.calls(&worker()), captures);
        let rt = &rig.job.service.rt;
        let snapshot = rt.history.snapshot(id, rt.clock.now().unwrap()).unwrap();
        assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
        assert!(snapshot.message.last_attempt.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn failed_pane_observations_are_throttled_across_frequent_job_wakes() {
    let mut rig = ManualJob::new().await;
    rig.screens.lose(&worker());
    let id = rig.send().await;
    for (advance_ms, captures) in [
        (0, 1),
        (0, 1),
        (1, 1),
        (248, 1),
        (0, 1),
        (1, 2),
        (0, 2),
        (249, 2),
        (1, 3),
    ] {
        tokio::time::advance(Duration::from_millis(advance_ms)).await;
        // Failed captures are paced too: keep observed across rounds so an
        // unrelated wake cannot retry the observer inside the guard poll.
        let mut queued = rig.job.service.board.queued.subscribe();
        guarded(rig.job.round(&mut queued)).await.unwrap();
        assert_eq!(rig.screens.calls(&worker()), captures);
        let rt = &rig.job.service.rt;
        let snapshot = rt.history.snapshot(id, rt.clock.now().unwrap()).unwrap();
        assert_eq!(snapshot.message.status, DeliveryStatus::Queued);
        assert!(snapshot.message.last_attempt.is_none());
        assert_eq!(
            rig.job.service.board.progress(id).reason.as_deref(),
            Some(UNOBSERVED_REASON)
        );
    }
    assert!(rig.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn observation_back_before_the_timeout_tells_nothing_and_delivers() {
    let mut bench = Bench::blind().await;
    bench.script(Act::Deliver);
    let id = bench.send(worker(), "brief").await;
    bench.pass(60).await;
    waits_unobserved(&bench, id).await;
    // The pane can be read again, and the policy has a reason of its own.
    let scrolling = with(ready("%1", "s1"), |o| o.copy_mode = Some(true));
    bench.screens.set(worker(), scrolling);
    bench.pass(70).await;
    assert_eq!(
        bench.entry(worker(), id).await.delivery.reason.as_deref(),
        Some("pane is in copy mode")
    );
    assert!(bench.notes().await.is_empty());
    bench.screens.set(worker(), ready("%1", "s1"));
    assert_eq!(bench.entered().await, id);
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn observation_back_after_the_notice_delivers() {
    let mut bench = Bench::blind().await;
    bench.script(Act::Deliver);
    let id = bench.send(worker(), "brief").await;
    bench.pass(121).await;
    bench.notice(id, "unobserved").await;
    assert_eq!(bench.notes().await, [unobserved(id)]);
    bench.screens.set(worker(), ready("%1", "s1"));
    assert_eq!(bench.entered().await, id);
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    assert_eq!(bench.notes().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn observation_of_another_pane_or_session_is_a_loss() {
    for elsewhere in [ready("%9", "s1"), ready("%1", "someone-else")] {
        let mut bench = Bench::blind().await;
        // History refuses it: it says nothing about this recipient.
        bench.screens.set(worker(), elsewhere);
        let id = bench.send(worker(), "brief").await;
        bench.pass(1).await;
        waits_unobserved(&bench, id).await;
        bench.pass(118).await;
        assert!(bench.notes().await.is_empty());
        bench.pass(3600).await;
        bench.notice(id, "unobserved").await;
        assert_eq!(bench.notes().await, [unobserved(id)]);
        assert!(bench.entries().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn owner_focus_notice_is_inbox_only_and_told_once() {
    let mut bench = Bench::blind().await;
    bench.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.focused = Some(true)),
    );
    let id = bench.send(worker(), "brief").await;
    let notice = bench.event("the focus notice", system_note).await;
    let OrgEvent::MessageSent(content) = notice else {
        unreachable!()
    };
    // The agreed marker: the owner's notifier skips exactly this note.
    assert!(is_focus_notice(&content));
    assert_eq!(content, focus_notice(id, &PaneId::parse("%1").unwrap()));
    assert_eq!(content.delivery_notice, Some(key(id, "focus")));
    bench.pass(300).await;
    assert_eq!(bench.notes().await.len(), 1);
    assert!(bench.entries().is_empty());
    // The observation is lost: the last focus that was seen is no longer
    // known, so a message that arrives now waits without a note.
    bench.screens.lose(&worker());
    bench.pass(1).await;
    bench.send(worker(), "later").await;
    bench.pass(60).await;
    assert_eq!(bench.notes().await.len(), 1);
    // Focus moves away: the message is pasted, with no further note.
    bench.screens.set(worker(), ready("%1", "s1"));
    assert_eq!(bench.entered().await, id);
    assert_eq!(bench.notes().await.len(), 1);
}

// s3-stuckseat case 2. Break: a revalidating seat whose box holds unsent text
// shows "recipient agent is starting or clearing", or the text omits the pane.
#[tokio::test(start_paused = true)]
async fn starting_seat_with_a_draft_names_the_draft_and_its_pane() {
    let mut rig = ManualJob::new().await;
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    rig.command(OrgCommand::Revalidate { agent_id: worker() })
        .await;
    let id = rig.send().await;
    rig.round(0).await;
    assert_eq!(
        rig.job.service.board.progress(id).reason.as_deref(),
        Some("input box holds unsent text in %1")
    );
}

// s3-stuckseat case 3, seen in the sealed rig (R6, R8). Break: a message
// blocked at the readiness deadline stays blocked once its agent is Ready
// again (a latch), instead of being pasted at the next safe look.
#[tokio::test(start_paused = true)]
async fn not_ready_timeout_is_delivered_once_the_agent_is_ready() {
    let mut bench = Bench::new().await;
    bench.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    bench
        .rig
        .run(OrgCommand::Revalidate { agent_id: worker() })
        .await;
    let id = bench.send(worker(), "brief").await;
    // Past the readiness timeout (120 s): blocked, not failed.
    bench.pass(300).await;
    let view = bench.entry(worker(), id).await.delivery;
    assert_eq!(view.phase, DeliveryPhase::Blocked);
    assert_eq!(
        view.reason.as_deref(),
        Some("recipient agent did not become ready before the readiness deadline")
    );
    bench.screens.set(worker(), ready("%1", "s1"));
    bench
        .rig
        .run(OrgCommand::EditorReady { agent_id: worker() })
        .await;
    assert_eq!(bench.entered().await, id);
}

// s3-stuckseat (lead's ask a). Break: a queue behind a seat whose session is
// unconfirmed since a restart says "recipient cannot be observed".
#[tokio::test(start_paused = true)]
async fn unconfirmed_session_wait_says_so() {
    let mut rig = ManualJob::new().await;
    rig.job.effects.observe =
        Arc::new(|_, _| Box::pin(async { Err(MessageError::Refused(UNCONFIRMED.into())) }));
    let id = rig.send().await;
    rig.round(0).await;
    assert_eq!(
        rig.job.service.board.progress(id).reason.as_deref(),
        Some(UNCONFIRMED)
    );
    assert_eq!(
        UNCONFIRMED,
        "recipient session not confirmed since the restart; waits for its next harness event"
    );
}

#[tokio::test(start_paused = true)]
async fn fresh_busy_row_has_no_blocked_notice() {
    let mut bench = Bench::blind().await;
    let busy = with(ready("%1", "s1"), |o| {
        let row = o.session.as_mut().unwrap();
        row.status = SessionStatus::Working;
        row.last_event_at = Some(utc());
    });
    bench.screens.set(worker(), busy);
    let id = bench.send(worker(), "brief").await;
    // Shorter than the stale bound: a turn that runs is no reason to notify.
    bench.pass(100).await;
    assert!(bench.entries().is_empty());
    assert!(bench.notes().await.is_empty());
    let view = bench.entry(worker(), id).await.delivery;
    assert_eq!(view.phase, DeliveryPhase::Waiting);
    assert_eq!(view.reason.as_deref(), Some("session is busy"));
}

#[tokio::test(start_paused = true)]
async fn blocked_reason_notice_is_told_once_and_lists_followers() {
    let mut bench = Bench::blind().await;
    let draft = with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft);
    bench.screens.set(worker(), draft);
    let first = bench.send(worker(), "first").await;
    let second = bench.send(worker(), "second").await;
    bench.event("the blocked notice", system_note).await;
    bench.pass(300).await;
    let reason = "input draft persisted past the delivery hold deadline";
    assert_eq!(
        bench.notes().await,
        [format!(
            "{first} blocked: {reason}; waiting behind it: {second}. \
             Delivery resumes when this clears; atm wake give-up {first} cancels it"
        )]
    );
    // No durable state changed, and the notes themselves start nothing.
    let view = bench.entry(worker(), first).await;
    assert_eq!(view.status, DeliveryStatus::Queued);
    assert_eq!(view.delivery.phase, DeliveryPhase::Blocked);
    assert_eq!(view.delivery.reason.as_deref(), Some(reason));
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    assert!(bench.entries().is_empty());
    // Blocked is a notice, not a latch: the draft goes and delivery resumes.
    bench.script(Act::Deliver);
    bench.screens.set(worker(), ready("%1", "s1"));
    assert_eq!(bench.entered().await, first);
    assert_eq!(bench.entered().await, second);
    // The hold deadline had passed, so the policy reads the stabilizing
    // editor as a second reason until it is stable: one more note, no third.
    let notes = bench.notes().await;
    let stabilizing = "recipient readiness could not be established";
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(
        notes[1],
        blocked_notice(first, stabilizing, &[second], false)
    );
}

#[tokio::test(start_paused = true)]
async fn final_failure_outside_an_attempt_tells_the_owner() {
    let mut bench = Bench::blind().await;
    let gone = with(ready("%1", "s1"), |o| o.presence = PanePresence::Gone);
    bench.screens.set(worker(), gone);
    let id = bench.send(worker(), "brief").await;
    let event = bench
        .event("the final failure", |e| failed(e).is_some())
        .await;
    assert_eq!(
        failed(&event),
        Some((id, "recipient pane is not alive", true))
    );
    bench.pass(60).await;
    assert_eq!(
        bench.notes().await,
        [format!(
            "{id} failed: recipient pane is not alive. Send it again if it is still needed"
        )]
    );
    assert!(bench.entries().is_empty());
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Failed
    );
}

#[tokio::test(start_paused = true)]
async fn failure_on_the_first_evaluation_leaves_no_notice_key() {
    let bench = Bench::blind().await;
    let gone = with(ready("%1", "s1"), |o| o.presence = PanePresence::Gone);
    bench.screens.set(worker(), gone);
    let first = bench.send(worker(), "brief").await;
    bench.pass(120).await;
    // The pane has been gone for longer than the grace: the next message
    // fails on its first evaluation, before anything was shown for it.
    let next = bench.send(worker(), "next").await;
    bench.pass(120).await;
    assert_eq!(
        bench.entry(worker(), next).await.status,
        DeliveryStatus::Failed
    );
    assert_eq!(bench.notes().await.len(), 2);
    // Both are told and gone: the board keeps nothing about either.
    let service = bench.rig.messages.service().unwrap();
    for id in [first, next] {
        assert!(service
            .has_delivery_notice(&key(id, "failed.pane_gone"))
            .unwrap());
    }
    assert!(service.failed_notices(&worker()).unwrap().is_empty());
    let shared = service.board.shared.lock().unwrap();
    assert!(shared.progress.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_attempts_retry_with_the_same_id_up_to_the_limit() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    let last = bench
        .event("the final failure", |e| {
            matches!(failed(e), Some((_, _, true)))
        })
        .await;
    assert_eq!(
        failed(&last),
        Some((
            id,
            "prompt token was not confirmed before the deadline",
            true
        ))
    );
    assert_eq!(bench.entries(), [id; 4]);
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Failed);
    assert_eq!(view.delivery.attempt, 4);
    bench.pass(60).await;
    assert_eq!(bench.entries(), []);
    assert_eq!(bench.notes().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn queued_retry_shows_when_it_is_due() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    bench
        .event("the first failure", |e| failed(e).is_some())
        .await;
    bench.pass(0).await;
    until("the retry is shown", || {
        bench
            .rig
            .messages
            .service()
            .unwrap()
            .board
            .progress(id)
            .retry_at
            .is_some()
    })
    .await;
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Queued);
    assert_eq!(view.delivery.phase, DeliveryPhase::Waiting);
    let failed_at = bench
        .rig
        .rt
        .history
        .snapshot(id, utc())
        .unwrap()
        .message
        .failed_at;
    assert_eq!(
        view.delivery.retry_at,
        failed_at.map(|at| at + chrono::TimeDelta::seconds(1))
    );
}

#[tokio::test(start_paused = true)]
async fn activity_after_the_paste_blocks_as_probably_delivered() {
    let mut bench = Bench::new().await;
    let follower = {
        let id = bench.probably_delivered().await;
        let follower = bench.send(worker(), "next").await;
        assert_eq!(
            bench.notes().await,
            [blocked_notice(id, PROBABLY, &[], true)]
        );
        let view = bench.entry(worker(), id).await;
        assert_eq!(view.status, DeliveryStatus::Delivering);
        assert_eq!(view.delivery.phase, DeliveryPhase::Blocked);
        follower
    };
    // The hold stays, and its follower stays behind it.
    bench.pass(600).await;
    assert!(bench.entries().is_empty());
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    assert_eq!(bench.notes().await.len(), 1);
    assert_eq!(
        bench.entry(worker(), follower).await.delivery.phase,
        DeliveryPhase::Waiting
    );
}

#[tokio::test(start_paused = true)]
async fn exact_receipt_is_retained_until_delivered_is_folded() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.worked().await, id);
    bench.rig.hold();
    bench.reference(id, "s1", "%1");
    // The Delivered append is entered and held: the receipt must survive.
    bench.rig.entered().await;
    let evidence = bench.rig.rt.evidence.clone();
    assert!(evidence.receipt(id).unwrap().is_some());
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Delivering
    );
    bench.rig.release();
    let event = bench.event("delivered", |e| delivered(e).is_some()).await;
    assert_eq!(delivered(&event), Some(id));
    until("evidence forgets the delivered message", || {
        evidence.receipt(id).is_err()
    })
    .await;
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.delivery.session_id, Some("s1".into()));
    assert_eq!(view.delivery.attempt, 1);
    bench.pass(60).await;
    assert_eq!(bench.entries(), [id]);
}

#[tokio::test(start_paused = true)]
async fn receipt_append_reply_lost_is_not_reissued_input() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.worked().await, id);
    bench.rig.hold();
    bench.reference(id, "s1", "%1");
    bench.rig.entered().await;
    // The writer stalls past every budget of the job: its commit is unknown.
    bench.pass(40).await;
    // Reads still answer while the append is stalled.
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Delivering
    );
    bench.rig.release();
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    let evidence = bench.rig.rt.evidence.clone();
    until("evidence forgets the delivered message", || {
        evidence.receipt(id).is_err()
    })
    .await;
    bench.pass(120).await;
    // One attempt, one Delivered, and no failure in between.
    assert_eq!(bench.entries(), [id]);
    let later = bench.committed();
    assert!(
        later
            .iter()
            .all(|e| delivered(e).is_none() && failed(e).is_none()),
        "{later:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn owner_retry_reaches_a_new_attempt_of_the_same_message() {
    let mut rig = ManualJob::new().await;
    rig.screens.set(worker(), ready("%1", "s1"));
    // Match Bench's executor entry/completion events, not terminal Enter keys.
    let (enter, mut entered) = mpsc::unbounded_channel();
    let (work_done, mut worked) = mpsc::unbounded_channel();
    rig.job.effects.deliver = Arc::new(move |rt, id, _| {
        let (enter, work_done) = (enter.clone(), work_done.clone());
        Box::pin(async move {
            enter.send(id).unwrap();
            let result = perform(rt, id, Act::Paste).await;
            work_done.send(id).unwrap();
            result
        })
    });
    let rt = rig.job.service.rt.clone();
    let activity = || {
        rt.evidence
            .tap(
                &SessionId::new("s1"),
                Some("%1"),
                Some(Harness::Pi),
                Some(&LifecycleEvent::WorkingStart),
                true,
                Some(&rt.history),
            )
            .unwrap();
    };
    let id = rig.send().await;
    rig.round(0).await;
    rig.round(10_000).await;
    assert_eq!(guarded(entered.recv()).await.unwrap(), id);
    assert_eq!(guarded(worked.recv()).await.unwrap(), id);
    activity();
    rig.round(3_000).await;
    let original = owner_entries(&rig).await;
    assert_eq!(original.len(), 1);
    assert_eq!(
        original[0].content.body,
        blocked_notice(id, PROBABLY, &[], true)
    );
    assert_eq!(
        original[0].content.delivery_notice,
        Some(key(id, "blocked.probably_delivered"))
    );
    assert_eq!(
        guarded(rig.job.resolve(Resolve::Retry, id, None))
            .await
            .unwrap(),
        DeliveryStatus::Delivering
    );
    // Activity while released must not hold the old attempt again.
    activity();
    rig.round(0).await;
    rig.round(10_000).await;
    assert_eq!(guarded(entered.recv()).await.unwrap(), id);
    assert_eq!(guarded(worked.recv()).await.unwrap(), id);
    // The returned round consumes the second hold even though its notice
    // already exists; advancing time alone is not a completion fence.
    activity();
    rig.round(3_000).await;
    let inbox = rig.messages.inbox(page(), pane("%1")).await.unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message_id, id);
    assert_eq!(inbox.messages[0].delivery.attempt, 2);
    assert_eq!(inbox.messages[0].delivery.pane.as_deref(), Some("%1"));
    assert_eq!(inbox.messages[0].delivery.phase, DeliveryPhase::Blocked);
    assert_eq!(inbox.messages[0].delivery.reason.as_deref(), Some(PROBABLY));
    let retained = owner_entries(&rig).await;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].message_id, original[0].message_id);
    assert_eq!(retained[0].content, original[0].content);
    assert!(entered.try_recv().is_err());
    assert!(worked.try_recv().is_err());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn late_receipt_after_release_cancels_retry() {
    let mut bench = Bench::new().await;
    let id = bench.probably_delivered().await;
    bench.retry(id).await.unwrap();
    // The first paste's reference arrives before the retry's new attempt.
    bench.reference(id, "s1", "%1");
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    bench.pass(120).await;
    assert_eq!(bench.entries(), []);
    let later = bench.committed();
    assert!(later.iter().all(|e| attempted(e).is_none()), "{later:?}");
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Delivered);
    assert_eq!(view.delivery.attempt, 1);
}

#[tokio::test(start_paused = true)]
async fn late_receipt_after_give_up_delivers() {
    let mut bench = Bench::new().await;
    let id = bench.probably_delivered().await;
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
    let event = bench.event("the give-up", |e| failed(e).is_some()).await;
    assert_eq!(
        failed(&event),
        Some((id, "owner gave up on delivery", true))
    );
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Failed
    );
    // Its job is gone; the receipt alone brings the recipient's slot back.
    bench.pass(60).await;
    bench.reference(id, "s1", "%1");
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    let evidence = bench.rig.rt.evidence.clone();
    until("evidence forgets the delivered message", || {
        evidence.receipt(id).is_err()
    })
    .await;
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Delivered
    );
    // The give-up told nobody: the owner did it. One blocked notice only.
    assert_eq!(bench.notes().await.len(), 1);
    // A decision alone starts the recipient's slot too, with nothing open.
    bench.pass(60).await;
    assert_eq!(code(bench.give_up(id).await.unwrap_err()), "conflict");
}

#[tokio::test(start_paused = true)]
async fn recoverable_block_or_wait_refuses_a_retry() {
    let mut bench = Bench::blind().await;
    let draft = with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft);
    bench.screens.set(worker(), draft);
    let id = bench.send(worker(), "brief").await;
    let not_blocked = format!("message {id} is not blocked; inspect atm inbox --id {id}");
    // Waiting for the draft to go, then blocked by it: both resume by
    // themselves, so there is no duplicate risk for the owner to accept.
    for blocked in [false, true] {
        if blocked {
            bench.event("the blocked notice", system_note).await;
        }
        let refused = bench.retry(id).await.unwrap_err();
        assert_eq!(refused.to_string(), not_blocked);
        assert_eq!(code(refused), "conflict");
    }
    assert_eq!(
        bench.entry(worker(), id).await.delivery.phase,
        DeliveryPhase::Blocked
    );
    assert!(bench.entries().is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_at_the_attempt_limit_refuses() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    for attempt in 1..=4 {
        let event = bench.event("an attempt", |e| attempted(e).is_some()).await;
        assert_eq!(attempted(&event), Some((id, attempt, "%1")));
    }
    assert_eq!(bench.entries(), [id; 4]);
    bench.activity("s1", "%1");
    bench.event("the blocked notice", system_note).await;
    let refused = bench.retry(id).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        format!("message {id} has used its 4 delivery attempts; give up or send it again")
    );
    assert_eq!(code(refused), "refused");
    // Nothing was released: it stays held until the owner gives up.
    bench.pass(120).await;
    assert_eq!(bench.entries(), []);
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
}

fn assert_editor_warning(result: GiveUpResult, id: MessageId, line: &str) {
    let warning = format!(
        "Possible leftover in the recipient's editor: {line}. Giving up did not clear the editor; \
         further delivery may wait for an empty editor. Inspect the pane before editing it."
    );
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::json!({"message_id":id,"status":"failed","editor_warning":warning})
    );
}

// Break: warning depends on an attempt, disappears on repeat, or alters late-receipt/attempt accounting.
#[tokio::test(start_paused = true)]
async fn give_up_warning_is_advisory_for_queued_held_and_repeated_calls() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    let line = format!("[{id}] Read: atm inbox --id {id}");
    assert_editor_warning(rig.give_up(id).await, id, &line);
    let count = rig.journal.lock().unwrap().len();
    assert_editor_warning(rig.give_up(id).await, id, &line);
    assert_eq!(rig.journal.lock().unwrap().len(), count);
    rig.stop().await;

    let mut bench = Bench::new().await;
    let id = bench.probably_delivered().await;
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Delivering
    );
    let result = guarded(bench.rig.messages.give_up(
        GiveUpParams {
            id,
            relay_words: None,
        },
        owner(),
    ))
    .await
    .unwrap();
    assert_editor_warning(result, id, &format!("[{id}] Read: atm inbox --id {id}"));
    bench.reference(id, "s1", "%1");
    bench
        .event("late receipt", |event| delivered(event) == Some(id))
        .await;
    let entry = bench.entry(worker(), id).await;
    assert_eq!(entry.status, DeliveryStatus::Delivered);
    assert_eq!(entry.delivery.attempt, 1);
    assert!(bench.entries().is_empty());
}

// Break: ordinary body text is echoed, or start/historical notes are warned with an inbox reference.
#[tokio::test(start_paused = true)]
async fn give_up_warning_uses_canonical_delivery_lines() {
    use super::super::start_line::start_line;
    let start = start_line(&worker());
    let historical = MessageContent {
        body: "New session: run atm me".into(),
        ..start.clone()
    };
    let lookalike = MessageContent {
        from: Address::Owner,
        ..start.clone()
    };
    for (content, start_text) in [(start, true), (historical, true), (lookalike, false)] {
        let mut rig = ManualJob::new().await;
        rig.command(OrgCommand::SendMessage(content)).await;
        let id = MessageId::new(rig.journal.lock().unwrap().last().unwrap().seq);
        let line = if start_text {
            format!("[{id}] New session: run the shell command atm me")
        } else {
            format!("[{id}] Read: atm inbox --id {id}")
        };
        assert_editor_warning(rig.give_up(id).await, id, &line);
        rig.stop().await;
    }
}

// Break: resolution observes the pane, trusts a stale Draft, or suppresses its advisory when blind.
#[tokio::test(start_paused = true)]
async fn give_up_warning_needs_no_fresh_observation() {
    for stale in [false, true] {
        let mut rig = ManualJob::new().await;
        if !stale {
            rig.command(OrgCommand::Down {
                agent_id: worker(),
                reason: "stopped".into(),
            })
            .await;
        }
        let id = rig.send().await;
        if stale {
            rig.turn(atm_core::TurnState::NotRunning);
            rig.round(0).await;
            rig.screens.lose(&worker());
        }
        let observations = rig.screens.calls(&worker());
        assert_eq!(observations, usize::from(stale));
        assert_editor_warning(
            rig.give_up(id).await,
            id,
            &format!("[{id}] Read: atm inbox --id {id}"),
        );
        assert_eq!(rig.screens.calls(&worker()), observations);
        assert!(rig
            .journal
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.kind != "msg.delivery_attempted"));
        rig.stop().await;
    }
}

// Break: give-up treats releasing the FIFO veto as clearing the editor or permits follower input.
#[tokio::test(start_paused = true)]
async fn give_up_leaves_a_draft_blocking_the_follower() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::NotRunning);
    let head = rig.send().await;
    let follower = rig.send().await;
    rig.round(0).await;
    assert_editor_warning(
        rig.give_up(head).await,
        head,
        &format!("[{head}] Read: atm inbox --id {head}"),
    );
    rig.round(120_000).await;
    let inbox = guarded(rig.messages.inbox(
        InboxParams {
            recipient: Some(Address::Agent(worker())),
            ..page()
        },
        owner(),
    ))
    .await
    .unwrap();
    let entry = inbox
        .messages
        .iter()
        .find(|e| e.message_id == follower)
        .unwrap();
    assert_eq!(entry.status, DeliveryStatus::Queued);
    assert_eq!(entry.delivery.phase, DeliveryPhase::Blocked);
    assert_eq!(
        entry.delivery.reason.as_deref(),
        Some("input draft persisted past the delivery hold deadline")
    );
    assert_eq!(entry.delivery.attempt, 0);
    assert_eq!(entry.delivery.blocked_by, None);
    assert_eq!(
        rig.screens
            .seen
            .lock()
            .unwrap()
            .get(&worker())
            .unwrap()
            .facts
            .input,
        InputState::Draft
    );
    assert!(rig
        .journal
        .lock()
        .unwrap()
        .iter()
        .all(|e| e.kind != "msg.delivery_attempted"));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn give_up_accepts_waiting_and_held_messages_and_frees_the_queue() {
    let mut bench = Bench::new().await;
    // A queued message that only waits (the other agent's pane is focused).
    bench
        .screens
        .set(other(), with(ready("%2", "s2"), |o| o.focused = None));
    let waiting = bench.send(other(), "waiting").await;
    bench.pass(1).await;
    assert_eq!(
        bench.give_up(waiting).await.unwrap(),
        DeliveryStatus::Failed
    );
    let view = bench.entry(other(), waiting).await;
    assert_eq!(view.status, DeliveryStatus::Failed);
    assert_eq!(
        view.delivery.reason.as_deref(),
        Some("owner gave up on delivery")
    );
    // A delivering message held as probably delivered, with a follower.
    let held = bench.probably_delivered().await;
    let follower = bench.send(worker(), "next").await;
    assert_eq!(bench.give_up(held).await.unwrap(), DeliveryStatus::Failed);
    assert_eq!(bench.entered().await, follower);
    // Giving up again changes nothing; a delivered message refuses.
    assert_eq!(bench.give_up(held).await.unwrap(), DeliveryStatus::Failed);
    bench.committed();
    assert_eq!(bench.give_up(held).await.unwrap(), DeliveryStatus::Failed);
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    bench.reference(held, "s1", "%1");
    bench
        .event("delivered", |e| delivered(e) == Some(held))
        .await;
    for refused in [bench.give_up(held).await, bench.retry(held).await] {
        assert_eq!(code(refused.unwrap_err()), "conflict");
    }
}

#[tokio::test(start_paused = true)]
async fn give_up_waits_for_active_cleanup() {
    let mut bench = Bench::new().await;
    close(&bench.cleanup);
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.worked().await, id);
    let messages = bench.rig.messages.clone();
    let mut give_up = tokio::spawn(async move {
        messages
            .give_up(
                GiveUpParams {
                    id,
                    relay_words: None,
                },
                owner(),
            )
            .await
    });
    // The attempt is still in its cleanup: the decision is not served.
    bench.pass(20).await;
    assert!(!give_up.is_finished());
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    bench.cleanup.add_permits(OPEN);
    let result = guarded(&mut give_up).await.unwrap().unwrap();
    assert_eq!(result.status, DeliveryStatus::Failed);
    assert_eq!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Failed
    );
}

#[tokio::test(start_paused = true)]
async fn resolution_timeout_is_typed_and_starts_nothing_later() {
    let mut bench = Bench::new().await;
    close(&bench.cleanup);
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.worked().await, id);
    let started = Instant::now();
    let timeout = bench
        .rig
        .messages
        .give_up(
            GiveUpParams {
                id,
                relay_words: None,
            },
            owner(),
        )
        .await;
    assert!(matches!(timeout, Err(MessageError::Timeout)), "{timeout:?}");
    // The service's own budget, below the router's 60 s.
    assert_eq!(started.elapsed(), MESSAGE_RESOLUTION_BUDGET);
    // The cleanup was not cut short, and once it ends the decision nobody
    // waits for any more is not applied.
    bench.cleanup.add_permits(OPEN);
    bench.pass(2).await;
    assert_ne!(
        bench.entry(worker(), id).await.status,
        DeliveryStatus::Failed
    );
    assert!(bench
        .committed()
        .iter()
        .all(|e| !matches!(failed(e), Some((_, _, true)))));
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
}

#[tokio::test(start_paused = true)]
async fn only_the_owner_resolves_and_only_known_agent_messages() {
    let bench = Bench::blind().await;
    let id = bench.send(worker(), "brief").await;
    let report = bench.rig.send(pane("%1"), Address::Owner, "report").await;
    let agent = bench
        .rig
        .messages
        .give_up(
            GiveUpParams {
                id,
                relay_words: None,
            },
            pane("%1"),
        )
        .await;
    let refused = agent.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "only the owner may retry or give up a delivery"
    );
    assert_eq!(code(refused), "refused");
    let unknown = bench.give_up(MessageId::new(999)).await.unwrap_err();
    assert_eq!(unknown.to_string(), "message m999 was not found");
    assert_eq!(code(unknown), "not_found");
    // A message to the owner was delivered at its fold.
    assert_eq!(
        bench.retry(report).await.unwrap_err().to_string(),
        format!("message {report} is not blocked; inspect atm inbox --id {report}")
    );
    assert_eq!(
        bench.give_up(report).await.unwrap_err().to_string(),
        format!("message {report} is already delivered")
    );
}

// s3-giverelay. Break: any agent, the owner with words, or a seat with no
// designation passes; the relay without words passes; the words never reach
// the journal; the owner's plain give-up or the existing conflicts change.
#[tokio::test(start_paused = true)]
async fn designated_relay_gives_up_in_the_owners_words() {
    let mut bench = Bench::new().await;
    // Messages to demo/other only wait (its pane is not focused).
    bench
        .screens
        .set(other(), with(ready("%2", "s2"), |o| o.focused = None));
    let first = bench.send(other(), "stale").await;
    let second = bench.send(other(), "stale too").await;
    bench.pass(1).await;
    let words = "Cancel them";
    let refusal = |who: &str| format!("{who} may not confirm as relay for message {first}");
    // No relay designated yet: demo/worker is refused like anyone else.
    let refused = bench
        .give_up_as(first, pane("%1"), Some(words))
        .await
        .unwrap_err();
    assert_eq!(refused.to_string(), refusal("demo/worker"));
    assert_eq!(code(refused), "refused");
    bench
        .rig
        .run(OrgCommand::Owner(atm_org::OwnerCommand::SetRelay {
            by: Address::Owner,
            agent: Some(worker()),
        }))
        .await;
    bench.committed();
    // Another agent, and the owner himself, may not relay.
    for (caller, who) in [(pane("%2"), "demo/other"), (owner(), "owner")] {
        let refused = bench
            .give_up_as(first, caller, Some(words))
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), refusal(who));
        assert_eq!(code(refused), "refused");
    }
    // Without his words the relay is an ordinary agent.
    let refused = bench.give_up_as(first, pane("%1"), None).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "only the owner may retry or give up a delivery"
    );
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    assert_eq!(
        bench.entry(other(), first).await.status,
        DeliveryStatus::Queued
    );
    // The designated relay with his words gives up; the journal keeps them.
    assert_eq!(
        bench
            .give_up_as(first, pane("%1"), Some(words))
            .await
            .unwrap(),
        DeliveryStatus::Failed
    );
    let reason =
        "owner gave up on delivery; confirmed in his words, relayed by demo/worker: 'Cancel them'";
    let events = bench.committed();
    let failures: Vec<_> = events.iter().filter_map(failed).collect();
    assert_eq!(failures, vec![(first, reason, true)]);
    assert_eq!(
        bench.entry(other(), first).await.delivery.reason.as_deref(),
        Some(reason)
    );
    // Already failed: Failed as today, and nothing more is written.
    assert_eq!(
        bench
            .give_up_as(first, pane("%1"), Some("again"))
            .await
            .unwrap(),
        DeliveryStatus::Failed
    );
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    // The owner's plain give-up is unchanged.
    assert_eq!(bench.give_up(second).await.unwrap(), DeliveryStatus::Failed);
    assert_eq!(
        bench
            .entry(other(), second)
            .await
            .delivery
            .reason
            .as_deref(),
        Some("owner gave up on delivery")
    );
    // A delivered message refuses the relay as it refuses the owner.
    let held = bench.probably_delivered().await;
    bench.reference(held, "s1", "%1");
    bench
        .event("delivered", |e| delivered(e) == Some(held))
        .await;
    let refused = bench
        .give_up_as(held, pane("%1"), Some(words))
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        format!("message {held} is already delivered")
    );
    assert_eq!(code(refused), "conflict");
}

#[tokio::test(start_paused = true)]
async fn resolution_needs_no_observation() {
    let bench = Bench::blind().await;
    // No pane at all: nothing to observe, and the decision is still served.
    bench
        .rig
        .run(OrgCommand::Down {
            agent_id: worker(),
            reason: "stopped".into(),
        })
        .await;
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
    assert_eq!(bench.screens.calls(&worker()), 0);
    // An observer that always fails does not hold a decision either.
    let id = bench.send(other(), "brief").await;
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
}

#[tokio::test(start_paused = true)]
async fn unavailable_agent_blocks_with_a_notice_and_resumes() {
    let mut bench = Bench::new().await;
    bench
        .rig
        .run(OrgCommand::Down {
            agent_id: worker(),
            reason: "stopped".into(),
        })
        .await;
    let id = bench.send(worker(), "brief").await;
    bench
        .event(
            "the blocked notice",
            |e| matches!(e, OrgEvent::MessageSent(note) if note.from == Address::System),
        )
        .await;
    assert_eq!(
        bench.notes().await,
        [blocked_notice(
            id,
            "recipient agent is stuck or down",
            &[],
            false
        )]
    );
    bench.pass(60).await;
    assert!(bench.entries().is_empty());
    // The agent comes back on another pane: the same message follows it.
    bench
        .rig
        .run(OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%5".into(),
            session_id: Some("s5".into()),
        })
        .await;
    bench.screens.set(worker(), ready("%5", "s5"));
    let event = bench.event("the attempt", |e| attempted(e).is_some()).await;
    assert_eq!(attempted(&event), Some((id, 1, "%5")));
}

#[tokio::test(start_paused = true)]
async fn retry_slot_follows_recipient_after_pane_move() {
    let mut bench = Bench::new().await;
    let id = bench.probably_delivered().await;
    bench.entries();
    // The agent goes down and comes back on another pane with a new session.
    bench
        .rig
        .run(OrgCommand::Down {
            agent_id: worker(),
            reason: "moved".into(),
        })
        .await;
    bench
        .rig
        .run(OrgCommand::Adopt {
            agent_id: worker(),
            pane: "%5".into(),
            session_id: Some("s5".into()),
        })
        .await;
    bench.screens.set(worker(), ready("%5", "s5"));
    bench.pass(60).await;
    // Still held: a new pane is no release.
    assert!(bench.entries().is_empty());
    assert_eq!(bench.retry(id).await.unwrap(), DeliveryStatus::Delivering);
    let event = bench
        .event("the second attempt", |e| attempted(e).is_some())
        .await;
    assert_eq!(attempted(&event), Some((id, 2, "%5")));
}

#[tokio::test(start_paused = true)]
async fn attempt_reply_lost_then_fifo_snapshot_accounts_commit() {
    let mut bench = Bench::new().await;
    bench.script(Act::PasteThenError);
    let id = bench.send(worker(), "brief").await;
    bench.event("the blocked notice", system_note).await;
    // The executor reported an error after its attempt was committed and
    // left no hold: the scheduler holds it, and nothing is pasted again.
    let snapshot = bench.rig.rt.history.snapshot(id, utc()).unwrap();
    assert_eq!(snapshot.hold, Some(ExecutorHold::InputOutcomeUnknown));
    bench.pass(600).await;
    assert_eq!(bench.entries(), [id]);
    assert_eq!(
        bench.notes().await,
        [blocked_notice(id, "input outcome is unknown", &[], true)]
    );
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Delivering);
    assert_eq!(view.delivery.phase, DeliveryPhase::Blocked);
    assert!(bench.committed().iter().all(|e| failed(e).is_none()));
    // The owner's retry releases exactly that hold.
    assert_eq!(bench.retry(id).await.unwrap(), DeliveryStatus::Delivering);
    let event = bench
        .event("the second attempt", |e| attempted(e).is_some())
        .await;
    assert_eq!(attempted(&event), Some((id, 2, "%1")));
}

#[tokio::test(start_paused = true)]
async fn unaccounted_attempt_stops_every_round_until_it_is_accounted_for() {
    let mut bench = Bench::new().await;
    bench.script(Act::LostReply);
    close(&bench.work);
    close(&bench.cleanup);
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.entered().await, id);
    // The attempt's append enters the writer and is held there; the
    // executor returns a timeout, knowing nothing about its commit.
    bench.rig.hold();
    bench.work.add_permits(OPEN);
    assert_eq!(bench.worked().await, id);
    bench.rig.entered().await;
    bench.cleanup.add_permits(OPEN);
    // Accounting cannot finish behind the held append. History still shows
    // a message with no attempt and a ready pane, and the job must not
    // decide on that: no second attempt, however long it lasts,
    bench.pass(120).await;
    assert!(bench.entries().is_empty());
    let view = bench.entry(worker(), id).await.delivery;
    assert_eq!(view.phase, DeliveryPhase::Blocked);
    assert!(
        (view.reason.as_deref()).is_some_and(|r| r.starts_with("input outcome is not accounted")),
        "{view:?}"
    );
    // and the owner's give-up waits behind it instead of deciding.
    assert_eq!(code(bench.give_up(id).await.unwrap_err()), "timeout");
    assert!(bench.entries().is_empty());
    // The writer answers again: the attempt was committed. It is held as
    // an input of unknown outcome, told once, and never pasted again.
    bench.rig.release();
    bench.event("the blocked notice", system_note).await;
    bench.pass(600).await;
    assert!(bench.entries().is_empty());
    assert_eq!(
        bench.notes().await,
        [blocked_notice(id, "input outcome is unknown", &[], true)]
    );
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Delivering);
    assert_eq!(view.delivery.attempt, 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_while_waiting_for_admission_starts_no_attempt() {
    let mut bench = Bench::new().await;
    // Every admission is taken, so the worker's attempt has to wait.
    let admissions = bench.rig.rt.admissions.clone();
    let taken = (admissions.acquire_many_owned(MESSAGE_ADMISSION_LIMIT as u32))
        .await
        .unwrap();
    bench.send(worker(), "brief").await;
    bench.pass(30).await;
    assert!(bench.entries().is_empty());
    // The daemon stops, and only then is there room: the job must leave
    // without starting what nothing would wait for.
    bench.rig.rt.cancel.cancel();
    drop(taken);
    guarded(&mut bench.rig.task).await.unwrap();
    assert!(bench.entries().is_empty());
}

#[tokio::test(start_paused = true)]
async fn attempt_settled_without_effect_is_retried_by_the_policy() {
    let mut bench = Bench::new().await;
    bench.script(Act::NoEffect);
    let id = bench.send(worker(), "brief").await;
    for attempt in 1..=2 {
        let event = bench.event("an attempt", |e| attempted(e).is_some()).await;
        assert_eq!(attempted(&event), Some((id, attempt, "%1")));
    }
    // The executor proved no effect and recorded the failure itself: no hold.
    assert_eq!(bench.rig.rt.history.snapshot(id, utc()).unwrap().hold, None);
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn executor_that_commits_nothing_is_not_called_in_a_loop() {
    let mut bench = Bench::new().await;
    bench.script(Act::Nothing);
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.entered().await, id);
    bench.pass(10).await;
    // At most once per guard poll, and never a paste of the scheduler's own.
    let calls = bench.entries().len();
    assert!((30..=41).contains(&calls), "{calls} calls in 10 s");
    assert!(bench.committed().iter().all(|e| attempted(e).is_none()));
}

#[tokio::test(start_paused = true)]
async fn executor_error_before_an_attempt_backs_off_and_shows_why() {
    let mut bench = Bench::new().await;
    bench.script(Act::Offline);
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.entered().await, id);
    // Committed changes wake the job; they do not shorten the back-off.
    bench.send(worker(), "next").await;
    bench.pass(25).await;
    assert_eq!(bench.entries(), []);
    let view = bench.entry(worker(), id).await.delivery;
    assert_eq!(view.phase, DeliveryPhase::Waiting);
    assert_eq!(
        view.reason.as_deref(),
        Some("message service unavailable: executor offline")
    );
    assert_eq!(view.attempt, 0);
    bench.pass(10).await;
    assert_eq!(bench.entries(), [id]);
    // A refusal is a decision about now: look again at the next poll.
    bench.script(Act::Refuse);
    bench.pass(30).await;
    bench.entries();
    bench.pass(10).await;
    let calls = bench.entries().len();
    assert!((30..=41).contains(&calls), "{calls} calls in 10 s");
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn message_without_evidence_is_blocked_until_evidence_has_room() {
    let mut bench = Bench::blind().await;
    bench.script(Act::Deliver);
    // Keep the reference holder observed but busy. An actual observation
    // loss there can independently publish an unobserved note at this deadline.
    bench.screens.set(
        other(),
        with(ready("%2", "s2"), |o| {
            o.facts.turn = TurnState::Running;
        }),
    );
    let known = bench.send(other(), "registered").await;
    // Evidence is full: the next message's reference is refused.
    let evidence = bench.rig.rt.evidence.clone();
    let mut filler = 1_000_000;
    while (evidence.register(
        MessageId::new(filler),
        inbox_reference(MessageId::new(filler)),
    ))
    .is_ok()
    {
        filler += 1;
    }
    // Both messages exist before the worker's job gets past its observation.
    close(&bench.screens.gate);
    let unknown = bench.send(worker(), "unregistered").await;
    let follower = bench.send(worker(), "behind it").await;
    bench.screens.set(worker(), ready("%1", "s1"));
    bench.screens.gate.add_permits(OPEN);
    bench.pass(120).await;
    bench.notice(unknown, "evidence_unavailable").await;
    // A ready pane is not enough: without evidence nothing is decided.
    assert_eq!(bench.entries(), []);
    let reason = "message service unavailable: message evidence reference capacity exhausted";
    assert_eq!(
        bench.notes().await,
        [blocked_notice(unknown, reason, &[follower], false)]
    );
    for id in [unknown, follower] {
        let view = bench.entry(worker(), id).await.delivery;
        assert_eq!(view.phase, DeliveryPhase::Blocked);
        assert_eq!(view.reason.as_deref(), Some(reason));
    }
    // Another recipient's delivery frees a reference: the blocked messages
    // are registered and delivered in their order, with no further note.
    bench.screens.set(other(), ready("%2", "s2"));
    for id in [known, unknown, follower] {
        assert_eq!(bench.entered().await, id);
    }
    bench
        .event("the follower delivered", |e| delivered(e) == Some(follower))
        .await;
    assert_eq!(bench.notes().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn given_up_unattempted_messages_hold_no_evidence_reference() {
    let mut bench = Bench::blind().await;
    // One more message than evidence has references, each given up before
    // any attempt: nothing could ever confirm one of them.
    let mut failed = Vec::new();
    for _ in 0..=MAX_MESSAGES {
        let id = bench.send(worker(), "never attempted").await;
        assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
        failed.push(id);
    }
    let evidence = bench.rig.rt.evidence.clone();
    assert!(failed.iter().all(|id| evidence.snapshot(*id).is_err()));
    // The fill outran the bench's event buffer; read from here on.
    bench.events = bench.events.resubscribe();
    // So a new message to a ready agent is expected, attempted and delivered.
    bench.script(Act::Deliver);
    bench.screens.set(other(), ready("%2", "s2"));
    let id = bench.send(other(), "brief").await;
    assert_eq!(bench.entered().await, id);
    bench.event("delivered", |e| delivered(e) == Some(id)).await;
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn step_on_a_message_delivered_meanwhile_evaluates_again() {
    let mut bench = Bench::blind().await;
    let service = bench.rig.messages.service().unwrap();
    let effects = Effects {
        deliver: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        observe: bench.screens.effect(),
        retract: no_retract(),
    };
    let mut queued = service.board.queued.subscribe();
    let mut job = Job::new(service.clone(), effects, worker());
    // A wake raced the MessageDelivered fold: the job still holds the ID as
    // open while history has it delivered and evidence still knows it.
    let racing = bench.send(worker(), "racing").await;
    bench.rig.deliver(racing, "%1", "s1").await;
    assert!(bench.rig.rt.evidence.snapshot(racing).is_ok());
    let step = job.step(racing, &mut queued).await;
    assert!(
        matches!(step, Ok(Next::Change)),
        "a delivered message is no error"
    );
    // The same after the executor acknowledged it: evidence has forgotten
    // the message, and the step must not register it again.
    bench.script(Act::Deliver);
    bench.screens.set(worker(), ready("%1", "s1"));
    let acknowledged = bench.send(worker(), "acknowledged").await;
    bench
        .event("delivered", |e| delivered(e) == Some(acknowledged))
        .await;
    until("evidence forgets it", || {
        bench.rig.rt.evidence.snapshot(acknowledged).is_err()
    })
    .await;
    let step = job.step(acknowledged, &mut queued).await;
    assert!(matches!(step, Ok(Next::Change)));
    assert!(bench.rig.rt.evidence.snapshot(acknowledged).is_err());
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn step_on_a_message_failed_meanwhile_registers_nothing() {
    let bench = Bench::blind().await;
    let service = bench.rig.messages.service().unwrap();
    let effects = Effects {
        deliver: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
        observe: bench.screens.effect(),
        retract: no_retract(),
    };
    let mut queued = service.board.queued.subscribe();
    let mut job = Job::new(service.clone(), effects, worker());
    // A wake raced the final failure's fold: the job still holds the ID as
    // open, while history has it failed and evidence has dropped it.
    let id = bench.send(worker(), "given up").await;
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
    assert!(bench.rig.rt.evidence.snapshot(id).is_err());
    let step = job.step(id, &mut queued).await;
    assert!(
        matches!(step, Ok(Next::Change)),
        "a failed message is no error"
    );
    // The step must not expect it again: nothing would ever forget it.
    assert!(bench.rig.rt.evidence.snapshot(id).is_err());
    assert!(bench.notes().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancellation_lets_the_active_attempt_settle_before_the_service_stops() {
    let mut bench = Bench::new().await;
    close(&bench.work);
    let id = bench.send(worker(), "brief").await;
    let waiting = bench.send(other(), "not started").await;
    // Whichever recipient entered first, both are inside the executor.
    bench.entered().await;
    bench.entered().await;
    bench.rig.rt.cancel.cancel();
    bench.pass(30).await;
    // The attempts are still in flight: the service keeps folding for them.
    assert!(!bench.rig.task.is_finished());
    bench.rig.messages.initialized().await.unwrap();
    bench.work.add_permits(OPEN);
    guarded(&mut bench.rig.task).await.unwrap();
    assert!(bench.rig.messages.initialized().await.is_err());
    // Both attempts were committed and folded, and nothing was started after.
    let committed = bench.committed();
    let attempts: Vec<_> = committed.iter().filter_map(attempted).collect();
    assert_eq!(attempts.len(), 2, "{committed:?}");
    for id in [id, waiting] {
        let snapshot = bench.rig.rt.history.snapshot(id, utc()).unwrap();
        assert_eq!(snapshot.message.status, DeliveryStatus::Delivering);
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_notice_commit_is_not_repeated_and_grants_nothing() {
    let mut rig = ManualJob::new().await;
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    let id = rig.send().await;
    rig.round(0).await;
    // The paused clock crosses the hold deadline, then an append-entry
    // acknowledgment (not elapsed quiet time) proves the actual write stalled.
    tokio::time::advance(Duration::from_secs(120)).await;
    while rig.appends.try_recv().is_ok() {}
    rig.gate.forget_permits(usize::MAX);
    let mut queued = rig.job.service.board.queued.subscribe();
    let (result, ()) = tokio::join!(guarded(rig.job.round(&mut queued)), async {
        guarded(rig.appends.recv()).await.unwrap();
        tokio::time::advance(Duration::from_secs(11)).await;
    });
    assert!(result.is_err());
    assert_eq!(
        rig.job.service.board.progress(id).phase,
        Some(DeliveryPhase::Blocked)
    );
    assert!(owner_entries(&rig).await.is_empty());
    rig.gate.add_permits(1);
    guarded(rig.job.service.rt.org.snapshot_at()).await.unwrap();
    rig.round(0).await;
    rig.round(300_000).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "blocked.draft_present"))
    );
    assert_eq!(
        rig.job.service.message(id).unwrap().unwrap().status,
        DeliveryStatus::Queued
    );
    // The ManualJob executor panics on any input dispatch, including after
    // reconciliation; completed returned rounds above prove it was not called.
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn retry_before_the_hold_is_latched_conflicts() {
    let mut bench = Bench::new().await;
    let id = bench.send(worker(), "brief").await;
    assert_eq!(bench.worked().await, id);
    bench.activity("s1", "%1");
    // Inside the confirmation window the receipt may still come: there is
    // nothing for the owner to release yet.
    let refused = bench.retry(id).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        format!("message {id} is not blocked; inspect atm inbox --id {id}")
    );
    assert!(bench.rig.rt.evidence.snapshot(id).unwrap().probably_landed);
}

#[tokio::test(start_paused = true)]
async fn owner_decision_does_not_wait_for_admission() {
    let mut bench = Bench::new().await;
    close(&bench.cleanup);
    for n in 0..MESSAGE_ADMISSION_LIMIT {
        let agent: AgentId = format!("demo/a{n}").parse().unwrap();
        let (pane, session) = (format!("%{}", 10 + n), format!("t{n}"));
        bench
            .rig
            .run(OrgCommand::Adopt {
                agent_id: agent.clone(),
                pane: pane.clone(),
                session_id: Some(session.as_str().into()),
            })
            .await;
        bench.screens.set(agent.clone(), ready(&pane, &session));
        bench.send(agent, "brief").await;
    }
    for _ in 0..MESSAGE_ADMISSION_LIMIT {
        bench.worked().await;
    }
    // Every permit is held in a cleanup; the worker's message is ready and
    // waits for one. Giving it up is served at once, without a permit.
    let id = bench.send(worker(), "brief").await;
    bench.pass(30).await;
    assert_eq!(bench.rig.rt.admissions.available_permits(), 0);
    assert_eq!(bench.give_up(id).await.unwrap(), DeliveryStatus::Failed);
    assert_eq!(bench.entries().len(), MESSAGE_ADMISSION_LIMIT);
}

// Break caught: diagnostic prose, a new attempt or a new process must not be
// the identity of an owner notice. These drive the real writer/fold and job.
async fn owner_entries(rig: &ManualJob) -> Vec<InboxEntry> {
    rig.messages
        .inbox(page(), owner())
        .await
        .unwrap()
        .messages
        .into_iter()
        .filter(|entry| entry.content.from == Address::System)
        .collect()
}

fn key(id: MessageId, reason: &str) -> atm_org::DeliveryNotice {
    atm_org::DeliveryNotice {
        source_message_id: id,
        reason: reason.into(),
    }
}

/// A real editable role file, with a different recorded manager in the journal.
async fn notice_roles(rig: &ManualJob, recorded: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let roles = root.path().join(".atm/roles");
    std::fs::create_dir_all(&roles).unwrap();
    rig.command(OrgCommand::RegisterProject {
        project_id: worker().project,
        root: root.path().to_str().unwrap().into(),
        move_root: true,
    })
    .await;
    rig.command(OrgCommand::RegisterProject {
        project_id: "org".parse().unwrap(),
        root: root.path().join("other").to_str().unwrap().into(),
        move_root: false,
    })
    .await;
    for (name, at) in [("demo/old", "%2"), ("org/lead", "%3")] {
        let agent_id: AgentId = name.parse().unwrap();
        rig.command(OrgCommand::Adopt {
            agent_id: agent_id.clone(),
            pane: at.into(),
            session_id: None,
        })
        .await;
        // Registered but Down: manager routing must not require an active pane.
        rig.command(OrgCommand::Down {
            agent_id,
            reason: "not running".into(),
        })
        .await;
    }
    rig.command(OrgCommand::Down {
        agent_id: worker(),
        reason: "install recorded role".into(),
    })
    .await;
    rig.command(OrgCommand::Up {
        agent_id: worker(),
        role: atm_org::parse_role("worker", &manager_role(recorded)).unwrap(),
    })
    .await;
    rig.command(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%1".into(),
        session_id: Some("s1".into()),
    })
    .await;
    (root, roles.join("worker.md"))
}

fn manager_role(manager: &str) -> String {
    format!("+++\nharness='pi'\nmanager='{manager}'\n+++\n")
}

async fn entries_to(rig: &ManualJob, to: &str) -> Vec<InboxEntry> {
    rig.messages
        .inbox(
            InboxParams {
                recipient: Some(to.parse().unwrap()),
                ..page()
            },
            owner(),
        )
        .await
        .unwrap()
        .messages
}

// Break caught: read the recorded manager (or Owner) instead of the current
// one, fan out copies, or turn an informational note into a delivery action.
#[tokio::test(start_paused = true)]
async fn blocked_and_unobserved_follow_current_manager() {
    let mut rig = ManualJob::new().await;
    let (_root, role) = notice_roles(&rig, "old").await;
    std::fs::write(&role, manager_role("old")).unwrap();
    let id = rig.send().await;
    let follower = rig.send().await;
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| {
            o.facts.turn = TurnState::Unknown;
        }),
    );
    rig.round(0).await;
    std::fs::write(&role, manager_role("org/lead")).unwrap();
    tokio::time::advance(Duration::from_secs(120)).await;
    // The shared role reader uses a detached OS thread, not paused-time I/O.
    tokio::time::resume();
    rig.recovery_round().await;
    tokio::time::pause();
    rig.screens.lose(&worker());
    rig.round(0).await;
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::time::resume();
    rig.recovery_round().await;

    assert!(owner_entries(&rig).await.is_empty(), "no Owner copy");
    assert!(
        entries_to(&rig, "demo/old").await.is_empty(),
        "no old manager copy"
    );
    let entries = entries_to(&rig, "org/lead").await;
    let expected = [
        // s3-busy: this look reads an empty editor with no dialog and an
        // unread turn, so the note names that instead of "readiness".
        ("blocked.readiness_timeout", format!("{id} blocked: atm cannot read the turn state on %1; atm will recheck automatically; waiting behind it: {follower}. Delivery resumes when this clears; the owner can cancel it with atm wake give-up {id}")),
        ("unobserved", format!("{id} waiting: recipient demo/worker could not be observed for 120 s. It stays queued; delivery can resume when the pane can be read again. The owner can cancel it with atm wake give-up {id}")),
    ];
    assert_eq!(entries.len(), expected.len());
    for (note, (reason, body)) in entries.iter().zip(expected) {
        assert_eq!(
            (note.status, note.delivery.attempt),
            (DeliveryStatus::Queued, 0)
        );
        assert_eq!(note.content.from, Address::System);
        assert_eq!(note.content.kind, MessageKind::Note);
        assert_eq!(note.content.delivery_notice, Some(key(id, reason)));
        assert_eq!(note.content.body, body);
        assert!(note.content.attachments.is_empty() && note.content.task.is_none());
    }
    let worker_mail = entries_to(&rig, "demo/worker").await;
    assert_eq!(worker_mail[0].delivery.phase, DeliveryPhase::Waiting);
    assert_eq!(worker_mail[1].delivery.blocked_by, Some(id));
    assert!(worker_mail
        .iter()
        .all(|m| m.status == DeliveryStatus::Queued && m.delivery.attempt == 0));
    rig.stop().await;
}

async fn assert_down_notice_stays_with_owner(content: MessageContent) {
    let mut rig = ManualJob::new().await;
    // If the writer wrongly tries to route, the manager exists even when the
    // detached reader cannot find a current role.
    let (_root, _role) = notice_roles(&rig, "org/lead").await;
    rig.command(OrgCommand::SendMessage(content)).await;
    let id = *rig.job.service.open(&worker()).unwrap().last().unwrap();
    rig.command(OrgCommand::Down {
        agent_id: worker(),
        reason: "stopped".into(),
    })
    .await;
    rig.recovery_round().await;

    let manager = entries_to(&rig, "org/lead").await;
    assert!(
        manager.is_empty(),
        "manager got the blocked notice for {id}"
    );
    let owner = owner_entries(&rig).await;
    assert_eq!(owner.len(), 1);
    assert_eq!(
        owner[0].content.delivery_notice,
        Some(key(id, "blocked.agent_unavailable"))
    );
    rig.stop().await;
}

// Break caught: a blocked System bootstrap shifts its Owner notification to an agent.
#[tokio::test(start_paused = true)]
async fn nonordinary_notice_system_start_line_stays_with_owner() {
    assert_down_notice_stays_with_owner(super::super::start_line::start_line(&worker())).await;
}

// Break caught: a self-send routes to a distinct manager, despite the self-manager guard.
#[tokio::test(start_paused = true)]
async fn nonordinary_notice_self_send_stays_with_owner() {
    assert_down_notice_stays_with_owner(MessageContent {
        from: Address::Agent(worker()),
        to: Address::Agent(worker()),
        ..note("a note to myself".into())
    })
    .await;
}

// Break caught: unusable current roles drop the recorded fallback, explicit
// Owner loses to fallback, or self-addressed diagnostic mail loops.
#[tokio::test]
async fn current_manager_owner_and_recorded_fallbacks() {
    for (fresh, recorded, to) in [
        (Some(manager_role("owner")), "old", "owner"),
        (None, "old", "demo/old"),
        (Some("+++\nharness='pi'\n+++\n".into()), "old", "demo/old"),
        (Some("+++\n[harness\n+++\n".into()), "old", "demo/old"),
        (Some(manager_role("missing")), "old", "demo/old"),
        (Some(manager_role("missing")), "missing", "owner"),
        (Some(manager_role("worker")), "old", "owner"),
    ] {
        let mut rig = ManualJob::new().await;
        let (_root, role) = notice_roles(&rig, recorded).await;
        if let Some(text) = &fresh {
            std::fs::write(role, text).unwrap();
        }
        let id = rig.send().await;
        rig.job
            .blocked_notice(
                id,
                "hold.input_outcome_unknown",
                "input outcome unknown",
                true,
            )
            .await
            .unwrap();
        let messages = entries_to(&rig, to).await;
        assert_eq!(messages.len(), 1, "current={fresh:?}, recorded={recorded}");
        let note = &messages[0];
        assert_eq!(
            note.content.delivery_notice,
            Some(key(id, "hold.input_outcome_unknown"))
        );
        let advice = if to == "owner" {
            format!("Inspect the pane; atm wake retry {id} --accept-duplicate-risk or atm wake give-up {id}")
        } else {
            assert!(owner_entries(&rig).await.is_empty());
            format!("Ask the owner to inspect the pane; only the owner can run atm wake retry {id} --accept-duplicate-risk or atm wake give-up {id}")
        };
        assert_eq!(
            note.content.body,
            format!("{id} blocked: input outcome unknown; waiting behind it: none. {advice}")
        );
        rig.stop().await;
    }
}

// Break caught: retarget the replayed lifetime key or widen the family to
// focus, running-turn reminders, or terminal failures.
#[tokio::test]
async fn current_manager_leaves_old_keys_and_owner_only_families_alone() {
    let rig = ManualJob::new().await;
    let (_root, role) = notice_roles(&rig, "old").await;
    std::fs::write(role, manager_role("org/lead")).unwrap();
    let id = rig.send().await;
    let mut legacy = note("legacy diagnostic".into());
    let legacy_key = key(id, "blocked.readiness_timeout");
    legacy.delivery_notice = Some(legacy_key.clone());
    rig.command(OrgCommand::SendMessage(legacy)).await;
    let records = rig.journal.lock().unwrap().clone();
    rig.stop().await;
    let mut rig = ManualJob::from_records(records).await;
    rig.job
        .blocked_notice(id, "blocked.readiness_timeout", "new diagnostic", false)
        .await
        .unwrap();
    let reasons = ["focus", "running_turn", "failed.attempt_limit"];
    for reason in ["focus", "running_turn"] {
        rig.job
            .publish(id, reason, note(reason.into()))
            .await
            .unwrap();
    }
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: FailureReason::AttemptLimit.to_string(),
        final_failure: true,
    })
    .await;
    rig.recovery_round().await;
    let owner = owner_entries(&rig).await;
    assert_eq!(owner.len(), 4);
    assert_eq!(owner[0].content.body, "legacy diagnostic");
    assert_eq!(owner[0].content.delivery_notice, Some(legacy_key));
    for (note, reason) in owner[1..].iter().zip(reasons) {
        assert_eq!(note.content.delivery_notice, Some(key(id, reason)));
    }
    assert!(entries_to(&rig, "org/lead").await.is_empty());
    assert!(entries_to(&rig, "demo/old").await.is_empty());
    rig.stop().await;
}

// Break caught: retry a lost append under the edited manager, or recursively
// direct a blocked manager-addressed note to that manager's manager.
#[tokio::test(start_paused = true)]
async fn current_manager_lost_append_keeps_one_audience_and_one_hop() {
    let mut rig = ManualJob::new().await;
    let (root, role) = notice_roles(&rig, "old").await;
    std::fs::write(&role, manager_role("org/lead")).unwrap();
    let id = rig.send().await;
    rig.turn(TurnState::Running);
    while rig.appends.try_recv().is_ok() {}
    rig.gate.forget_permits(usize::MAX);
    tokio::time::resume();
    {
        let mut publication =
            Box::pin(
                rig.job
                    .blocked_notice(id, "evidence_unavailable", "first diagnostic", false),
            );
        tokio::select! {
            result = &mut publication => panic!("returned before append barrier: {result:?}"),
            entered = guarded(rig.appends.recv()) => assert_eq!(entered, Some(())),
        }
        std::fs::write(&role, manager_role("old")).unwrap();
        tokio::time::pause();
        tokio::time::advance(COMMIT_BUDGET).await;
        assert!(guarded(publication).await.is_err());
    }
    rig.gate.add_permits(1);
    rig.round(0).await;
    rig.job
        .blocked_notice(id, "evidence_unavailable", "changed diagnostic", false)
        .await
        .unwrap();
    let notes = entries_to(&rig, "org/lead").await;
    assert_eq!(notes.len(), 1);
    let first = &notes[0];
    assert_eq!(
        first.content.delivery_notice,
        Some(key(id, "evidence_unavailable"))
    );
    assert_eq!(first.content.body, format!("{id} blocked: first diagnostic; waiting behind it: none. Delivery resumes when this clears; the owner can cancel it with atm wake give-up {id}"));
    assert!(entries_to(&rig, "demo/old").await.is_empty());
    assert!(owner_entries(&rig).await.is_empty());

    let manager_roles = root.path().join("other/.atm/roles");
    std::fs::create_dir_all(&manager_roles).unwrap();
    std::fs::write(manager_roles.join("lead.md"), manager_role("demo/old")).unwrap();
    rig.job.recipient = "org/lead".parse().unwrap();
    tokio::time::resume();
    rig.recovery_round().await; // The manager is Down, so its own mail is blocked.
    let owners = owner_entries(&rig).await;
    assert_eq!(owners.len(), 1);
    assert_eq!(
        owners[0].content.delivery_notice,
        Some(key(first.message_id, "blocked.agent_unavailable"))
    );
    assert!(entries_to(&rig, "demo/old").await.is_empty());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn enter_not_sent_notice_matches_recovery_eligibility() {
    for case in [
        "eligible owner",
        "eligible manager",
        "uncertain",
        "used",
        "revoked",
        "lost",
    ] {
        let rig = ManualJob::new().await;
        let role = if case == "eligible manager" {
            let (root, role) = notice_roles(&rig, "old").await;
            std::fs::write(role, manager_role("org/lead")).unwrap();
            Some(root)
        } else {
            None
        };
        let paste = if case == "uncertain" {
            EffectCertainty::Unknown
        } else {
            EffectCertainty::Applied
        };
        let (mut rig, id, scope) = rig.held_original(ExecutorHold::EnterNotSent, paste).await;
        let rt = &rig.job.service.rt;
        let now = rt.clock.now().unwrap();
        if case == "used" {
            drop(
                rt.evidence
                    .claim_recovery(&rt.history, scope, false, now)
                    .unwrap(),
            );
        } else if case == "revoked" {
            rt.history.revoke_recovery(id).unwrap();
        }
        rig.turn(TurnState::Running);
        if case == "lost" {
            rig.screens.lose(&worker());
        }
        // The scheduler must publish copy without exercising the attempt fake,
        // which panics if a busy or unobserved recipient reaches dispatch.
        tokio::time::resume();
        rig.recovery_round().await;
        tokio::time::pause();
        let to = if role.is_some() { "org/lead" } else { "owner" };
        let notes = entries_to(&rig, to).await;
        assert_eq!(notes.len(), 1, "{case}");
        assert_eq!(
            notes[0].content.delivery_notice,
            Some(key(id, "hold.enter_not_sent")),
            "{case}"
        );
        // s3-stuckseat 5b: the notice names atm's reference and what clears it.
        let advice = match (case.starts_with("eligible"), role.is_some()) {
            (true, false) => "atm presses Enter once when the seat is idle, nobody is in the pane, the editor holds exactly that reference line and the pane passes its safety checks; or press Enter there yourself if the editor holds exactly that reference line.".to_owned(),
            (true, true) => "atm presses Enter once when the seat is idle, nobody is in the pane, the editor holds exactly that reference line and the pane passes its safety checks; or ask the owner to press Enter there if the editor holds exactly that reference line.".to_owned(),
            (false, _) => format!("atm cannot press Enter on the current evidence: press Enter there yourself if the editor holds exactly that reference line, or give it up with atm wake give-up {id}."),
        };
        assert_eq!(notes[0].content.body, format!("{id} blocked: atm's reference {id} is waiting for Enter in %1; waiting behind it: none. Do not type in that pane. {advice}"), "{case}");
        let source = entries_to(&rig, "demo/worker").await;
        assert_eq!(source[0].delivery.phase, DeliveryPhase::Blocked);
        assert_eq!(source[0].delivery.attempt, 1);
        if role.is_some() {
            assert!(owner_entries(&rig).await.is_empty());
            rig.command(OrgCommand::Adopt {
                agent_id: "org/lead".parse().unwrap(),
                pane: "%3".into(),
                session_id: Some("manager-session".into()),
            })
            .await;
            let retry = rig
                .messages
                .retry(ResolveParams { id }, pane("%3"))
                .await
                .unwrap_err();
            let cancel = rig
                .messages
                .give_up(
                    GiveUpParams {
                        id,
                        relay_words: None,
                    },
                    pane("%3"),
                )
                .await
                .unwrap_err();
            assert!(matches!(retry, MessageError::Refused(_)));
            assert!(matches!(cancel, MessageError::Refused(_)));
        }
        rig.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn notice_tags_are_semantic() {
    let mut rig = ManualJob::new().await;
    let cases = [
        (
            FailureReason::AgentUnavailable,
            None,
            "blocked.agent_unavailable",
        ),
        (
            FailureReason::AgentNotReadyTimeout,
            None,
            "blocked.agent_not_ready_timeout",
        ),
        (
            FailureReason::Restarted,
            Some(ExecutorHold::EnterNotSent),
            "blocked.restarted",
        ),
        (FailureReason::CopyMode, None, "blocked.copy_mode"),
        (
            FailureReason::ConfirmationTimeout,
            None,
            "blocked.confirmation_timeout",
        ),
        (FailureReason::PaneGone, None, "blocked.pane_gone"),
        (FailureReason::DraftPresent, None, "blocked.draft_present"),
        (FailureReason::DialogOpen, None, "blocked.dialog_open"),
        (
            FailureReason::ReadinessTimeout,
            None,
            "blocked.readiness_timeout",
        ),
        (FailureReason::AttemptLimit, None, "blocked.attempt_limit"),
        (
            FailureReason::ProbablyDelivered,
            None,
            "blocked.probably_delivered",
        ),
        (
            FailureReason::ProbablyDelivered,
            Some(ExecutorHold::InputOutcomeUnknown),
            "hold.input_outcome_unknown",
        ),
        (
            FailureReason::ProbablyDelivered,
            Some(ExecutorHold::EnterNotSent),
            "hold.enter_not_sent",
        ),
        (
            FailureReason::ProbablyDelivered,
            Some(ExecutorHold::TokenVisible),
            "hold.token_visible",
        ),
        (
            FailureReason::ProbablyDelivered,
            Some(ExecutorHold::StagedMismatch),
            "hold.staged_mismatch",
        ),
    ];
    let mut actual = Vec::new();
    let mut expected_keys = Vec::new();
    for (reason, hold, expected) in cases {
        let id = rig.send().await;
        rig.job
            .blocked_notice(
                id,
                &blocked_notice_tag(reason, hold),
                &blocked_reason(reason, hold),
                false,
            )
            .await
            .unwrap();
        let entry = owner_entries(&rig).await.pop().unwrap();
        actual.push(entry.content.delivery_notice);
        expected_keys.push(Some(key(id, expected)));
    }
    assert_eq!(actual, expected_keys);
}

#[tokio::test(start_paused = true)]
async fn changed_diagnostics_followers_and_read_state_keep_original_note() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.job
        .publish(id, "evidence_unavailable", note("capacity 1".into()))
        .await
        .unwrap();
    let first = owner_entries(&rig).await.remove(0);
    rig.messages
        .ack(
            super::super::messages::AckParams {
                ids: vec![first.message_id],
            },
            owner(),
        )
        .await
        .unwrap();
    let original = owner_entries(&rig).await.remove(0);
    let follower = rig.send().await;
    rig.job
        .publish(
            id,
            "evidence_unavailable",
            note(format!("capacity 2; follower {follower}")),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(owner_entries(&rig).await).unwrap(),
        serde_json::json!([original])
    );
    let inbox = rig
        .messages
        .inbox(
            InboxParams {
                recipient: Some(Address::Agent(worker())),
                ..page()
            },
            owner(),
        )
        .await
        .unwrap();
    assert_eq!(inbox.messages[1].delivery.blocked_by, Some(id));
    rig.job
        .publish(id, "unobserved", note("different cause".into()))
        .await
        .unwrap();
    rig.job
        .publish(id, "unobserved", note("changed body again".into()))
        .await
        .unwrap();
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "evidence_unavailable"))
    );
    assert_eq!(
        entries[1].content.delivery_notice,
        Some(key(id, "unobserved"))
    );
}

#[tokio::test(start_paused = true)]
async fn observation_flaps_keep_focus_and_unobserved_lifetime_notes() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    for _ in 0..2 {
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| o.focused = Some(true)),
        );
        rig.round(0).await;
        rig.screens.lose(&worker());
        rig.round(0).await;
        rig.round(120_000).await;
    }
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content.delivery_notice, Some(key(id, "focus")));
    assert_eq!(
        entries[1].content.delivery_notice,
        Some(key(id, "unobserved"))
    );
}

#[tokio::test(start_paused = true)]
async fn notice_pre_handover_timeout_reconciles_and_retries() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    *rig.fault.lock().unwrap() = Some(crate::eventlog::EventLogError::TimeoutBeforeHandover);
    rig.job
        .publish(
            id,
            "evidence_unavailable",
            note("retain this diagnostic".into()),
        )
        .await
        .unwrap_err();
    assert!(owner_entries(&rig).await.is_empty());
    rig.round(0).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(
        entries.len(),
        1,
        "an absent append must not lose the pending note"
    );
    assert_eq!(entries[0].content.body, "retain this diagnostic");
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "evidence_unavailable"))
    );
}

#[tokio::test(start_paused = true)]
async fn notice_reply_lost_reconciles_committed_key() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    rig.gate.forget_permits(usize::MAX);
    rig.job
        .publish(id, "unobserved", note("accepted before timeout".into()))
        .await
        .unwrap_err();
    rig.gate.add_permits(1);
    guarded(rig.job.service.rt.org.snapshot_at()).await.unwrap();
    rig.round(0).await;
    rig.job.service.board.forget(id); // A new process has no process-local keys.
    rig.job
        .publish(
            id,
            "unobserved",
            note("new diagnostics after restart".into()),
        )
        .await
        .unwrap();
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content.body, "accepted before timeout");
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "unobserved"))
    );
}

#[tokio::test(start_paused = true)]
async fn notice_reconciliation_failure_keeps_pending_without_resending() {
    let mut rig = ManualJob::new().await;
    rig.turn(atm_core::TurnState::Running);
    let id = rig.send().await;
    while rig.appends.try_recv().is_ok() {}
    rig.gate.forget_permits(usize::MAX);
    rig.job
        .publish(id, "evidence_unavailable", note("still pending".into()))
        .await
        .unwrap_err();
    let mut queued = rig.job.service.board.queued.subscribe();
    let _ = guarded(rig.job.round(&mut queued)).await;
    assert_eq!(
        std::iter::from_fn(|| rig.appends.try_recv().ok()).count(),
        1
    );
    rig.gate.add_permits(1);
    guarded(rig.job.service.rt.org.snapshot_at()).await.unwrap();
    rig.round(0).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "evidence_unavailable"))
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_failure_notice_refusal_is_not_lost_when_source_closes() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "recipient pane is not alive".into(),
        final_failure: true,
    })
    .await;
    assert!(rig.job.service.open(&worker()).unwrap().is_empty());
    *rig.fault.lock().unwrap() = Some(crate::eventlog::EventLogError::TimeoutBeforeHandover);
    rig.job
        .publish(id, "failed.pane_gone", note("failure retained".into()))
        .await
        .unwrap_err();
    rig.round(0).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "failed.pane_gone"))
    );
    rig.round(0).await;
    assert_eq!(owner_entries(&rig).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn terminal_failure_commit_reply_lost_still_publishes_once() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    rig.gate.forget_permits(usize::MAX);
    let command = OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "recipient pane is not alive".into(),
        final_failure: true,
    };
    assert!(guarded(rig.job.commit(command)).await.is_err());
    rig.gate.add_permits(1);
    guarded(rig.job.service.rt.org.snapshot_at()).await.unwrap();
    rig.round(0).await;
    rig.round(0).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "failed.pane_gone"))
    );
    assert_eq!(
        rig.job.service.message(id).unwrap().unwrap().status,
        DeliveryStatus::Failed
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_notice_rebuild_ignores_give_up_and_arbitrary_prose() {
    let mut rig = ManualJob::new().await;
    for reason in [
        "owner gave up on delivery",
        "arbitrary diagnostic",
        "recipient pane is not alive",
    ] {
        let id = rig.send().await;
        rig.command(OrgCommand::DeliveryFailed {
            message_id: id,
            reason: reason.into(),
            final_failure: true,
        })
        .await;
    }
    rig.round(0).await;
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice.as_ref().unwrap().reason,
        "failed.pane_gone"
    );
}

#[tokio::test(start_paused = true)]
async fn committed_serialized_replay_keeps_note_id_body_and_read_state() {
    let mut first = ManualJob::new().await;
    let id = first.send().await;
    first
        .job
        .publish(id, "unobserved", note("original".into()))
        .await
        .unwrap();
    let note_id = owner_entries(&first).await[0].message_id;
    first
        .messages
        .ack(
            super::super::messages::AckParams { ids: vec![note_id] },
            owner(),
        )
        .await
        .unwrap();
    let expected = serde_json::to_value(owner_entries(&first).await).unwrap();
    let bytes = serde_json::to_vec(&*first.journal.lock().unwrap()).unwrap();
    first.stop().await;
    let mut second = ManualJob::from_records(serde_json::from_slice(&bytes).unwrap()).await;
    second
        .job
        .publish(id, "unobserved", note("different diagnostics".into()))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(owner_entries(&second).await).unwrap(),
        expected
    );
    second.stop().await;
}

#[tokio::test(start_paused = true)]
async fn missing_terminal_notice_rebuilds_from_serialized_committed_log() {
    let first = ManualJob::new().await;
    let id = first.send().await;
    first
        .command(OrgCommand::DeliveryFailed {
            message_id: id,
            reason: "recipient pane is not alive".into(),
            final_failure: true,
        })
        .await;
    let bytes = serde_json::to_vec(&*first.journal.lock().unwrap()).unwrap();
    first.stop().await;
    let mut second = ManualJob::from_records(serde_json::from_slice(&bytes).unwrap()).await;
    second.round(0).await;
    second.round(0).await;
    let entries = owner_entries(&second).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "failed.pane_gone"))
    );
    second.stop().await;
}

#[tokio::test(start_paused = true)]
async fn legacy_unkeyed_note_allows_only_one_keyed_successor() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    rig.command(OrgCommand::SendMessage(note("legacy notice".into())))
        .await;
    rig.job
        .publish(id, "unobserved", note("keyed successor".into()))
        .await
        .unwrap();
    rig.job.service.board.forget(id);
    rig.job
        .publish(id, "unobserved", note("must not append".into()))
        .await
        .unwrap();
    let entries = owner_entries(&rig).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content.delivery_notice, None);
    assert_eq!(
        entries[1].content.delivery_notice,
        Some(key(id, "unobserved"))
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn ack_cancelled_queue_does_not_run_when_recipient_idles() {
    use super::super::messages::AckParams;
    let screens = Screens::default();
    let idle = ready("%1", "s1");
    screens.set(
        worker(),
        with(idle.clone(), |o| o.facts.turn = TurnState::Running),
    );
    let mut bench = Bench::with_screens(screens).await;
    let head = bench.send(worker(), "already read").await;
    let follower = bench.send(worker(), "waiting for delivery").await;
    assert_eq!(
        bench.entry(worker(), head).await.status,
        DeliveryStatus::Queued
    );
    assert_eq!(
        bench
            .rig
            .messages
            .ack(AckParams { ids: vec![head] }, pane("%1"))
            .await
            .unwrap()
            .count,
        1
    );
    bench.script(Act::Deliver);
    bench.screens.set(worker(), idle);
    bench
        .event("follower receipt", |e| delivered(e) == Some(follower))
        .await;
    assert_eq!(bench.entries(), [follower]);
    let cancelled = bench.entry(worker(), head).await;
    assert_eq!(cancelled.status, DeliveryStatus::Failed);
    assert_eq!(cancelled.delivery.attempt, 0);
    assert!(bench.rig.inbox(owner(), page()).await.messages.is_empty());
    assert!(bench.rig.rt.evidence.snapshot(head).is_err());
}

#[tokio::test(start_paused = true)]
async fn ack_cancelled_while_waiting_for_admission_skips_executor() {
    use super::super::messages::AckParams;
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    let rt = rig.job.service.rt.clone();
    let snapshot = rt.history.snapshot(id, rt.clock.now().unwrap()).unwrap();
    let admission = rt
        .admissions
        .clone()
        .acquire_many_owned(MESSAGE_ADMISSION_LIMIT.try_into().unwrap())
        .await
        .unwrap();
    let mut queued = rig.job.service.board.queued.subscribe();
    let mut dispatch = Box::pin(rig.job.attempt(&snapshot, &mut queued, false));
    poll_pending(dispatch.as_mut()).await;
    assert_eq!(
        rig.messages
            .ack(AckParams { ids: vec![id] }, pane("%1"))
            .await
            .unwrap()
            .count,
        1
    );
    drop(admission);
    assert!(matches!(guarded(dispatch).await.unwrap(), Next::Again));
    assert!(rt.evidence.snapshot(id).is_err());
}

// An open snapshot retained before ack must not use up evidence capacity
// when it gets as far as registration after the terminal fold forgot the ID.
#[tokio::test(start_paused = true)]
async fn ack_discards_registration_from_stale_open_snapshot() {
    use super::super::messages::AckParams;
    let rig = ManualJob::new().await;
    let id = rig.send().await;
    let rt = &rig.job.service.rt;
    let now = rt.clock.now().unwrap();
    let stale = rt.history.snapshot(id, now).unwrap().message;
    assert_eq!(stale.status, DeliveryStatus::Queued);
    assert_eq!(
        rig.messages
            .ack(AckParams { ids: vec![id] }, pane("%1"))
            .await
            .unwrap()
            .count,
        1
    );
    assert!(rt.evidence.snapshot(id).is_err());
    assert!(!rt.register_for_delivery(&stale, now).unwrap());
    assert!(rt.evidence.snapshot(id).is_err());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn ack_leaves_followers_blocked_by_the_staged_draft_not_the_old_id() {
    use super::super::messages::AckParams;
    let (mut rig, id, _) = ManualJob::recovery_original().await;
    assert_eq!(
        rig.messages
            .ack(AckParams { ids: vec![id] }, pane("%1"))
            .await
            .unwrap()
            .count,
        1
    );
    // The stand-in observation still has a draft after the original hold is
    // cancelled. ManualJob's executor panics on any dispatch.
    let follower = rig.send().await;
    rig.round(0).await;
    let timeout = rig.job.service.rt.policy.settings().readiness_timeout;
    tokio::time::advance(timeout).await;
    rig.round(0).await;
    let inbox = rig
        .messages
        .inbox(
            InboxParams {
                id: Some(follower),
                ..page()
            },
            pane("%1"),
        )
        .await
        .unwrap();
    let entry = &inbox.messages[0];
    assert_eq!(entry.status, DeliveryStatus::Queued);
    assert_eq!(entry.delivery.phase, DeliveryPhase::Blocked);
    assert_eq!(
        entry.delivery.reason.as_deref(),
        Some("input draft persisted past the delivery hold deadline")
    );
    assert_eq!(entry.delivery.blocked_by, None);
    assert_eq!(entry.delivery.attempt, 0);
    assert!(rig
        .notes()
        .await
        .iter()
        .any(|body| body.starts_with(&format!("{follower} blocked: input draft persisted"))));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn ack_read_append_failure_leaves_cancelled_prefix() {
    use super::super::messages::AckParams;
    let mut first = ManualJob::new().await;
    let id = first.send().await;
    while first.appends.try_recv().is_ok() {}
    close(&first.gate);
    let ack = |messages: super::super::messages::MessageHandle| async move {
        messages.ack(AckParams { ids: vec![id] }, pane("%1")).await
    };
    let mut call = spawn_polled(ack(first.messages.clone()));
    tokio::select! {
        event = guarded(first.appends.recv()) => assert!(event.is_some()),
        result = &mut call => panic!("ack did not write a cancellation: {result:?}"),
    }
    *first.fault.lock().unwrap() = Some(crate::eventlog::EventLogError::TimeoutBeforeHandover);
    first.gate.add_permits(1);
    assert!(matches!(
        guarded(call).await.unwrap(),
        Err(MessageError::Organisation(OrgServiceError::Timeout))
    ));
    let records = first.journal.lock().unwrap().clone();
    assert_eq!(records.last().unwrap().kind, "msg.delivery_failed");
    assert!(records.iter().all(|record| record.kind != "msg.read"));
    first.stop().await;
    let second = ManualJob::from_records(records).await;
    let row = second.job.service.message(id).unwrap().unwrap();
    assert_eq!(row.status, DeliveryStatus::Failed);
    assert!(row.read_at.is_none());
    assert!(second.job.service.open(&worker()).unwrap().is_empty());
    let cursor = second.job.service.rt.org.status().unwrap();
    assert_eq!(ack(second.messages.clone()).await.unwrap().count, 1);
    {
        let records = second.journal.lock().unwrap();
        assert_eq!(records.last().unwrap().seq, cursor + 1);
        assert_eq!(records.last().unwrap().kind, "msg.read");
    }
    second.stop().await;
}

#[tokio::test(start_paused = true)]
async fn fatal_unknown_append_reconciles_only_after_committed_replay() {
    let mut first = ManualJob::new().await;
    let id = first.send().await;
    *first.fault.lock().unwrap() = Some(crate::eventlog::EventLogError::TimeoutAfterHandover);
    first
        .job
        .publish(
            id,
            "unobserved",
            note("committed without acknowledgment".into()),
        )
        .await
        .unwrap_err();
    assert!(first.job.service.rt.org.status().is_err());
    assert!(owner_entries(&first).await.is_empty());
    let records = first.journal.lock().unwrap().clone();
    first.stop().await;
    let mut second = ManualJob::from_records(records).await;
    second
        .job
        .publish(id, "unobserved", note("must not repeat".into()))
        .await
        .unwrap();
    let entries = owner_entries(&second).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content.body, "committed without acknowledgment");
    assert_eq!(
        entries[0].content.delivery_notice,
        Some(key(id, "unobserved"))
    );
    second.stop().await;
}

#[tokio::test(start_paused = true)]
async fn late_delivered_cancels_unsent_terminal_notice() {
    let mut rig = ManualJob::new().await;
    let id = rig.send().await;
    rig.command(OrgCommand::DeliveryAttempted {
        message_id: id,
        attempt: 1,
        pane: "%1".into(),
    })
    .await;
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "recipient pane is not alive".into(),
        final_failure: true,
    })
    .await;
    *rig.fault.lock().unwrap() = Some(crate::eventlog::EventLogError::TimeoutBeforeHandover);
    rig.job
        .publish(
            id,
            "failed.pane_gone",
            note("must not report a stale failure".into()),
        )
        .await
        .unwrap_err();
    rig.command(OrgCommand::Delivered {
        message_id: id,
        session_id: "s1".into(),
        confirmation: "prompt_submit_exact_reference".into(),
    })
    .await;
    rig.round(0).await;
    assert!(owner_entries(&rig).await.is_empty());
    assert_eq!(
        rig.job.service.message(id).unwrap().unwrap().status,
        DeliveryStatus::Delivered
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn terminal_notice_tags_are_semantic_for_all_known_causes() {
    let mut rig = ManualJob::new().await;
    let mut expected = Vec::new();
    for (reason, suffix) in [
        (FailureReason::AgentUnavailable, "agent_unavailable"),
        (
            FailureReason::AgentNotReadyTimeout,
            "agent_not_ready_timeout",
        ),
        (FailureReason::Restarted, "restarted"),
        (FailureReason::CopyMode, "copy_mode"),
        (FailureReason::ConfirmationTimeout, "confirmation_timeout"),
        (FailureReason::PaneGone, "pane_gone"),
        (FailureReason::DraftPresent, "draft_present"),
        (FailureReason::DialogOpen, "dialog_open"),
        (FailureReason::ReadinessTimeout, "readiness_timeout"),
        (FailureReason::AttemptLimit, "attempt_limit"),
        (FailureReason::ProbablyDelivered, "probably_delivered"),
    ] {
        let id = rig.send().await;
        rig.command(OrgCommand::DeliveryFailed {
            message_id: id,
            reason: reason.to_string(),
            final_failure: true,
        })
        .await;
        expected.push(Some(key(id, &format!("failed.{suffix}"))));
    }
    rig.round(0).await;
    let actual: Vec<_> = owner_entries(&rig)
        .await
        .into_iter()
        .map(|e| e.content.delivery_notice)
        .collect();
    assert_eq!(actual, expected);
    rig.stop().await;
}

// s3-stuckseat case 5a, seen live twice on a Claude lead (journal: the seat
// started a turn of its own 0.4-0.6 s after atm's paste, so Enter was
// withheld): once that turn ends and the box still holds exactly the
// reference, the one-shot recovery presses Enter, unfocused. Break: the
// harness's own turn (a busy session row, then idle again) spends or revokes
// the recovery, leaving atm's reference in the box for good.
#[tokio::test(start_paused = true)]
async fn enter_not_sent_reference_is_finished_once_the_harness_turn_ends() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, id, _) = ManualJob::new()
        .await
        .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    rig.job.effects.deliver = Arc::new(move |rt, id, _| {
        let called = called.clone();
        Box::pin(async move {
            called.fetch_add(1, Ordering::SeqCst);
            let now = rt.clock.now()?;
            let scope = rt.history.recovery_candidate(id, now)?.unwrap();
            let claim = rt.evidence.claim_recovery(&rt.history, scope, false, now)?;
            drop(claim);
            Ok(())
        })
    });
    // The harness's own turn: its row works, the screen shows a turn running
    // with the reference still staged below it.
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| {
            o.facts.turn = atm_core::TurnState::Running;
            o.facts.input = InputState::Draft;
            if let Some(row) = o.session.as_mut() {
                row.status = SessionStatus::Working;
            }
        }),
    );
    rig.recovery_round().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no Enter while the turn runs"
    );
    // The turn ends; the reference is still staged; the owner is elsewhere.
    rig.turn(atm_core::TurnState::NotRunning);
    rig.round(5_000).await;
    rig.recovery_round().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one Enter-only recovery");
    let _ = id;
}

// s3-stuckseat case 5, the lead's ruling Q6. Break: the one-shot Enter runs
// while the owner may be in the pane (focused, or focus unknown).
#[tokio::test(start_paused = true)]
async fn enter_not_sent_reference_waits_while_the_owner_may_be_in_the_pane() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for focus in [Some(true), None] {
        let (mut rig, _, _) = ManualJob::new()
            .await
            .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
            .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        rig.job.effects.deliver = Arc::new(move |_, _, _| {
            let called = called.clone();
            Box::pin(async move {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.input = InputState::Draft;
                o.focused = focus;
            }),
        );
        for _ in 0..3 {
            rig.round(5_000).await;
            rig.recovery_round().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{focus:?}");
    }
}

// s3-stuckseat case 5b. Break: a reference atm pasted without Enter reads as
// "editor staging is unproven" (a person cannot act on that), or the notice
// does not say what clears it: atm's own Enter once nobody is in the pane,
// the owner's Enter, or a give-up.
#[tokio::test(start_paused = true)]
async fn enter_not_sent_names_atms_reference_and_what_clears_it() {
    for (paste, advice) in [
        (
            EffectCertainty::Applied,
            "atm presses Enter once when the seat is idle, nobody is in the pane, the editor holds exactly that reference line and the pane passes its safety checks; or press Enter there yourself if the editor holds exactly that reference line.",
        ),
        (
            EffectCertainty::Unknown,
            "atm cannot press Enter on the current evidence: press Enter there yourself if the editor holds exactly that reference line, or give it up with atm wake give-up",
        ),
    ] {
        let (mut rig, id, _) = ManualJob::new()
            .await
            .held_original(ExecutorHold::EnterNotSent, paste)
            .await;
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.input = InputState::Draft;
                o.focused = Some(true);
            }),
        );
        tokio::time::resume();
        rig.recovery_round().await;
        tokio::time::pause();
        let shown = format!("atm's reference {id} is waiting for Enter in %1");
        assert_eq!(
            rig.job.service.board.progress(id).reason.as_deref(),
            Some(shown.as_str()),
            "{paste:?}"
        );
        let notes = rig.notes().await;
        assert_eq!(notes.len(), 1, "{paste:?}");
        assert!(
            notes[0].starts_with(&format!("{id} blocked: {shown}; waiting behind it: none. Do not type in that pane. {advice}")),
            "{paste:?}: {}",
            notes[0]
        );
    }
}

// s3-5c S2. Break: a held message's text comes from its hold, not from the
// editor: a reference the owner cleared or replaced still "waits for Enter".
// Unread (the adapter cannot tell), the hold's 5b words stay.
#[tokio::test(start_paused = true)]
async fn held_reference_text_comes_from_the_editor() {
    let held = |id| format!("atm's reference {id} is waiting for Enter in %1");
    let gone = |id| {
        format!("atm's reference {id} is no longer in the input box in %1; Enter was not sent")
    };
    for (case, input, editor) in [
        ("holds it", InputState::Draft, Some(true)),
        ("other text", InputState::Draft, Some(false)),
        ("empty", InputState::Empty, None),
        ("unread", InputState::Draft, None),
    ] {
        let (mut rig, id, _) = ManualJob::new()
            .await
            .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
            .await;
        // Focused: no Enter recovery, only the words.
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.input = input;
                o.focused = Some(true);
            }),
        );
        let reference = inbox_reference(id);
        let text = match editor {
            Some(true) => Some(reference.as_str()),
            Some(false) => Some("the owner's own draft"),
            None => None,
        };
        rig.screens.set_box(worker(), text);
        tokio::time::resume();
        rig.recovery_round().await;
        tokio::time::pause();
        let expected = match case {
            "other text" | "empty" => gone(id),
            _ => held(id),
        };
        assert_eq!(
            rig.job.service.board.progress(id).reason.as_deref(),
            Some(expected.as_str()),
            "{case}"
        );
    }
}

// s3-5c S2. Break: after a restart atm's own reference in the editor reads
// as the policy's "restarted" only, as if someone else's text were there.
#[tokio::test(start_paused = true)]
async fn restarted_attempt_names_its_reference_from_the_editor() {
    for (case, own) in [("own reference", true), ("plain draft", false)] {
        let first = ManualJob::new().await;
        let id = first.send().await;
        first
            .command(OrgCommand::DeliveryAttempted {
                message_id: id,
                attempt: 1,
                pane: "%1".into(),
            })
            .await;
        let bytes = serde_json::to_vec(&*first.journal.lock().unwrap()).unwrap();
        first.stop().await;
        let mut second = ManualJob::from_records(serde_json::from_slice(&bytes).unwrap()).await;
        second.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
        );
        let reference = inbox_reference(id);
        let text = if own { reference.as_str() } else { "a draft" };
        second.screens.set_box(worker(), Some(text));
        second.round(0).await;
        let expected = if own {
            format!("atm's reference {id} is waiting for Enter in %1")
        } else {
            FailureReason::Restarted.to_string()
        };
        assert_eq!(
            second.job.service.board.progress(id).reason.as_deref(),
            Some(expected.as_str()),
            "{case}"
        );
        second.stop().await;
    }
}

// s3-5c S2. Break: a message behind atm's own failed reference reads "input
// box holds unsent text", as if the owner were typing there.
#[tokio::test(start_paused = true)]
async fn queued_message_names_a_failed_reference_in_the_editor() {
    for (case, own) in [("failed reference", true), ("plain draft", false)] {
        let (mut rig, first, _) = ManualJob::new()
            .await
            .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
            .await;
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| {
                o.facts.input = InputState::Draft;
                o.focused = Some(true);
            }),
        );
        rig.give_up(first).await;
        let second = rig.send().await;
        rig.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
        );
        let reference = inbox_reference(first);
        let text = if own { reference.as_str() } else { "a draft" };
        rig.screens.set_box(worker(), Some(text));
        // A harness with no retract key: the reference is named, not cleared.
        rig.job.effects.retract = retract_fake(
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            vec![Retracted::Refused],
            rig.screens.clone(),
        );
        rig.round(0).await;
        let expected = if own {
            format!("input box holds atm's unsent reference {first} in %1")
        } else {
            "input box holds unsent text in %1".to_owned()
        };
        assert_eq!(
            rig.job.service.board.progress(second).reason.as_deref(),
            Some(expected.as_str()),
            "{case}"
        );
    }
}

/// A retract fake that counts its calls and answers `outcomes` in turn (the
/// last one from then on). A cleared editor then reads empty, as the real key
/// leaves it (s3-5c).
fn retract_fake(
    calls: Arc<std::sync::atomic::AtomicUsize>,
    outcomes: Vec<Retracted>,
    screens: Screens,
) -> Retract {
    let outcomes = Arc::new(Mutex::new(VecDeque::from(outcomes)));
    Arc::new(move |_, _, _| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let outcome = {
            let mut outcomes = outcomes.lock().unwrap();
            if outcomes.len() > 1 {
                outcomes.pop_front().unwrap()
            } else {
                *outcomes.front().unwrap()
            }
        };
        if outcome == Retracted::Cleared {
            screens.set_box(worker(), None);
            screens.set(worker(), ready("%1", "s1"));
        }
        Box::pin(async move { Ok(outcome) })
    })
}

/// A delivery fake that counts the attempts it is asked for.
fn counted_deliver(calls: Arc<std::sync::atomic::AtomicUsize>) -> Deliver {
    Arc::new(move |_, _, _| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    })
}

/// A message to the worker whose reference atm pasted without Enter, given
/// up by the owner (a final failure), and a second message queued behind it.
async fn failed_reference(rig: ManualJob) -> (ManualJob, MessageId, MessageId) {
    let (mut rig, first, _) = rig
        .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
        .await;
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| {
            o.facts.input = InputState::Draft;
            o.focused = Some(true);
        }),
    );
    rig.give_up(first).await;
    let second = rig.send().await;
    // Idle, unfocused, the editor holding exactly the first reference.
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    rig.screens.set_box(worker(), Some(&inbox_reference(first)));
    (rig, first, second)
}

// s3-5c. Break: atm's own reference left by a finally failed message holds
// every later delivery, or atm retracts it more than once, or says nothing.
#[tokio::test(start_paused = true)]
async fn failed_reference_is_retracted_once_then_the_queue_moves_on() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, first, second) = failed_reference(ManualJob::new().await).await;
    let (retracts, attempts) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    rig.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Cleared],
        rig.screens.clone(),
    );
    rig.job.effects.deliver = counted_deliver(attempts.clone());
    for _ in 0..6 {
        rig.round(5_000).await;
    }
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
    let notes = rig.notes().await;
    let cleared = format!("atm cleared its unsent reference {first} from %1");
    assert_eq!(
        notes.iter().filter(|n| **n == cleared).count(),
        1,
        "{notes:?}"
    );
    assert!(
        attempts.load(Ordering::SeqCst) >= 1,
        "{second} was never attempted"
    );
}

// s3-5c. Break: the retract runs on anything but exactly atm's reference of
// a finally failed message, in an idle editor nobody is focused on. Which
// harness has a key is the executor's to check (an adopted seat has no role).
#[tokio::test(start_paused = true)]
async fn retract_never_runs_on_anything_else() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for case in [
        "beside a draft",
        "unread",
        "focused",
        "unknown focus",
        "running",
        "dialog",
        "copy mode",
        "lost",
        "late receipt",
        "still open",
    ] {
        let base = ManualJob::new().await;
        let (mut rig, first) = if case == "still open" {
            let (rig, first, _) = base
                .held_original(ExecutorHold::EnterNotSent, EffectCertainty::Applied)
                .await;
            rig.screens.set(
                worker(),
                with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
            );
            rig.screens.set_box(worker(), Some(&inbox_reference(first)));
            (rig, first)
        } else {
            let (rig, first, _) = failed_reference(base).await;
            (rig, first)
        };
        let reference = inbox_reference(first);
        let draft = |change: fn(&mut RecipientObservation)| {
            with(
                with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
                change,
            )
        };
        match case {
            "beside a draft" => rig
                .screens
                .set_box(worker(), Some(&format!("{reference} and mine"))),
            "unread" => rig.screens.set_box(worker(), None),
            "focused" => rig.screens.set(worker(), draft(|o| o.focused = Some(true))),
            "unknown focus" => rig.screens.set(worker(), draft(|o| o.focused = None)),
            "running" => rig
                .screens
                .set(worker(), draft(|o| o.facts.turn = TurnState::Running)),
            "dialog" => rig.screens.set(
                worker(),
                draft(|o| o.facts.dialog = DialogState::Permission),
            ),
            "copy mode" => rig
                .screens
                .set(worker(), draft(|o| o.copy_mode = Some(true))),
            "lost" => rig.screens.lose(&worker()),
            "late receipt" => {
                rig.command(OrgCommand::Delivered {
                    message_id: first,
                    session_id: "s1".into(),
                    confirmation: "fixture".into(),
                })
                .await
            }
            _ => {}
        }
        let retracts = Arc::new(AtomicUsize::new(0));
        rig.job.effects.retract = retract_fake(
            retracts.clone(),
            vec![Retracted::Cleared],
            rig.screens.clone(),
        );
        rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
        for _ in 0..3 {
            rig.round(5_000).await;
        }
        assert_eq!(retracts.load(Ordering::SeqCst), 0, "{case}");
    }
}

// s3-5c. Break: a retract whose key left text behind is tried again, or the
// owner's manager hears of it more than once.
#[tokio::test(start_paused = true)]
async fn retract_that_left_text_is_reported_once_and_never_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, first, _) = failed_reference(ManualJob::new().await).await;
    let retracts = Arc::new(AtomicUsize::new(0));
    rig.job.effects.retract =
        retract_fake(retracts.clone(), vec![Retracted::Left], rig.screens.clone());
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    for _ in 0..3 {
        rig.round(40_000).await;
    }
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
    let left = format!("atm tried once to clear its unsent reference {first} from %1, but the input box may still hold text; clear it there. atm will not try again.");
    let notes = rig.notes().await;
    assert_eq!(notes.iter().filter(|n| **n == left).count(), 1, "{notes:?}");
}

// s3-5c. Break: a retract that sent nothing is reported, never tried again,
// or tried again at every guard poll.
#[tokio::test(start_paused = true)]
async fn retract_refused_before_the_key_is_tried_again_after_a_pause() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, first, _) = failed_reference(ManualJob::new().await).await;
    let retracts = Arc::new(AtomicUsize::new(0));
    rig.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Refused, Retracted::Cleared],
        rig.screens.clone(),
    );
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    rig.round(0).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
    assert!(rig
        .notes()
        .await
        .iter()
        .all(|n| !n.contains("unsent reference")));
    rig.round(29_000).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 1, "within the pause");
    rig.round(1_000).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 2);
    let cleared = format!("atm cleared its unsent reference {first} from %1");
    assert!(rig.notes().await.contains(&cleared));
}

// s3-5c (the lead's Q2). Break: after a restart atm no longer knows its own
// failed reference, so a promotion leaves it in the editor for good.
#[tokio::test(start_paused = true)]
async fn failed_reference_is_retracted_after_a_restart_from_the_log() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (rig, first, _) = failed_reference(ManualJob::new().await).await;
    let bytes = serde_json::to_vec(&*rig.journal.lock().unwrap()).unwrap();
    rig.stop().await;
    let mut second = ManualJob::from_records(serde_json::from_slice(&bytes).unwrap()).await;
    second.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    second
        .screens
        .set_box(worker(), Some(&inbox_reference(first)));
    let retracts = Arc::new(AtomicUsize::new(0));
    second.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Cleared],
        second.screens.clone(),
    );
    second.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    second.round(0).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
    second.stop().await;
}

// s3-5c S2. Break: once the draft wait has lasted the readiness budget, the
// blocked message behind atm's own failed reference reads "draft present"
// again, as if the owner had typed it.
#[tokio::test(start_paused = true)]
async fn blocked_draft_wait_names_a_failed_reference() {
    use std::sync::atomic::AtomicUsize;
    let (mut rig, first, second) = failed_reference(ManualJob::new().await).await;
    // A harness with no retract key: the reference stays and is named.
    rig.job.effects.retract = retract_fake(
        Arc::new(AtomicUsize::new(0)),
        vec![Retracted::Refused],
        rig.screens.clone(),
    );
    rig.round(0).await;
    rig.round(121_000).await;
    let progress = rig.job.service.board.progress(second);
    assert_eq!(progress.phase, Some(DeliveryPhase::Blocked));
    assert_eq!(
        progress.reason.as_deref(),
        Some(format!("input box holds atm's unsent reference {first} in %1").as_str())
    );
}

// s3-5c. Break: a reference attempted on the agent's old pane is matched in
// its new pane's editor, so atm clears text there that it never typed.
#[tokio::test(start_paused = true)]
async fn reference_from_an_old_pane_is_never_retracted() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, first, _) = failed_reference(ManualJob::new().await).await;
    rig.command(OrgCommand::Down {
        agent_id: worker(),
        reason: "fixture".into(),
    })
    .await;
    rig.command(OrgCommand::Adopt {
        agent_id: worker(),
        pane: "%2".into(),
        session_id: Some("s1".into()),
    })
    .await;
    rig.send().await;
    // The new pane's editor shows the same text, idle and unfocused.
    rig.screens.set(
        worker(),
        with(ready("%2", "s1"), |o| o.facts.input = InputState::Draft),
    );
    rig.screens.set_box(worker(), Some(&inbox_reference(first)));
    let retracts = Arc::new(AtomicUsize::new(0));
    rig.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Cleared],
        rig.screens.clone(),
    );
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    for _ in 0..3 {
        rig.round(5_000).await;
    }
    assert_eq!(retracts.load(Ordering::SeqCst), 0);
}

// s3-5c. Break: the owner puts atm's cleared reference back (Claude's
// Ctrl+Y) and atm clears it again: once per message means once.
#[tokio::test(start_paused = true)]
async fn retracted_reference_put_back_by_the_owner_is_left_alone() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, first, _) = failed_reference(ManualJob::new().await).await;
    let retracts = Arc::new(AtomicUsize::new(0));
    rig.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Cleared],
        rig.screens.clone(),
    );
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    rig.round(0).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
    );
    rig.screens.set_box(worker(), Some(&inbox_reference(first)));
    for _ in 0..3 {
        rig.round(40_000).await;
    }
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
}

// s3-5c (review SHOULD 2). Break: after a restart (every promotion) atm
// presses Ctrl-U again on a reference it already retracted, because only the
// old job knew: the committed note is the never-again marker.
#[tokio::test(start_paused = true)]
async fn retract_note_holds_once_across_a_restart() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for outcome in [Retracted::Cleared, Retracted::Left] {
        let (mut rig, first, _) = failed_reference(ManualJob::new().await).await;
        let retracts = Arc::new(AtomicUsize::new(0));
        rig.job.effects.retract =
            retract_fake(retracts.clone(), vec![outcome], rig.screens.clone());
        rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
        rig.round(0).await;
        assert_eq!(retracts.load(Ordering::SeqCst), 1, "{outcome:?}");
        let bytes = serde_json::to_vec(&*rig.journal.lock().unwrap()).unwrap();
        rig.stop().await;
        let mut second = ManualJob::from_records(serde_json::from_slice(&bytes).unwrap()).await;
        // The same reference in an idle, unfocused editor (Ctrl+Y after a
        // clear, or the text a Left retract left).
        second.screens.set(
            worker(),
            with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft),
        );
        second
            .screens
            .set_box(worker(), Some(&inbox_reference(first)));
        let again = Arc::new(AtomicUsize::new(0));
        second.job.effects.retract = retract_fake(
            again.clone(),
            vec![Retracted::Cleared],
            second.screens.clone(),
        );
        second.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
        for _ in 0..3 {
            second.round(40_000).await;
        }
        assert_eq!(again.load(Ordering::SeqCst), 0, "{outcome:?}");
        second.stop().await;
    }
}

// s3-5c (review NIT 2). Break: an executor that ended without an outcome
// after it may have sent the key (a deadline, a worker failure) leaves the
// job free to press Ctrl-U again on the next look.
#[tokio::test(start_paused = true)]
async fn retract_without_an_outcome_is_not_tried_again_by_the_job() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, _, _) = failed_reference(ManualJob::new().await).await;
    let retracts = Arc::new(AtomicUsize::new(0));
    let counted = retracts.clone();
    rig.job.effects.retract = Arc::new(move |_, _, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(MessageError::Timeout) })
    });
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    for _ in 0..3 {
        rig.round(40_000).await;
    }
    assert_eq!(retracts.load(Ordering::SeqCst), 1);
}

// s3-5c (review SHOULD 1). Break: the retract waits for an admission permit,
// so the owner's decision and shutdown queue behind eight busy deliveries.
#[tokio::test(start_paused = true)]
async fn retract_never_waits_for_admission() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut rig, _, _) = failed_reference(ManualJob::new().await).await;
    let retracts = Arc::new(AtomicUsize::new(0));
    rig.job.effects.retract = retract_fake(
        retracts.clone(),
        vec![Retracted::Cleared],
        rig.screens.clone(),
    );
    rig.job.effects.deliver = counted_deliver(Arc::new(AtomicUsize::new(0)));
    let admissions = rig.job.service.rt.admissions.clone();
    let all = admissions.available_permits() as u32;
    let held = admissions.clone().try_acquire_many_owned(all).unwrap();
    // `round` returns (guarded against a hang) with nothing sent.
    rig.round(0).await;
    assert_eq!(retracts.load(Ordering::SeqCst), 0);
    drop(held);
    rig.round(1_000).await;
    assert_eq!(
        retracts.load(Ordering::SeqCst),
        1,
        "tried again at the next poll"
    );
}

// Break: the wait or blocked note fails to name the unread turn or implies
// the user must start a turn instead of letting the scheduler recheck.
#[tokio::test(start_paused = true)]
async fn readiness_wait_names_an_unread_turn() {
    let mut rig = ManualJob::new().await;
    rig.screens.set(
        worker(),
        with(ready("%1", "s1"), |o| o.facts.turn = TurnState::Unknown),
    );
    let id = rig.send().await;
    rig.round(0).await;
    let unread = "atm cannot read the turn state on %1; atm will recheck automatically";
    let progress = rig.job.service.board.progress(id);
    assert_eq!(
        (progress.phase, progress.reason.as_deref()),
        (Some(DeliveryPhase::Waiting), Some(unread))
    );
    rig.round(121_000).await;
    let progress = rig.job.service.board.progress(id);
    assert_eq!(
        (progress.phase, progress.reason.as_deref()),
        (Some(DeliveryPhase::Blocked), Some(unread))
    );
    let notes = rig.notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with(&format!("{id} blocked: {unread}; ")),
        "{}",
        notes[0]
    );
}

// Captured: Claude Code 2.1.330, 2026-11-22, read-only tmux capture-pane -p -e -N.
// Break: a partial notice is accepted as idle, or the blocked head stops being
// observed and holds the FIFO even after the idle screen becomes readable.
#[tokio::test(start_paused = true)]
async fn blocked_unread_turn_rechecks_and_delivers_without_a_new_turn() {
    let capture = include_str!("../../../atm-claude-adapter/tests/fixtures/screen_styled_update-installed-idle_2.1.330.txt");
    let idle = capture
        .lines()
        .skip(1)
        .map(|row| row.strip_suffix('|').unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let notice = "✔ Update installed · /restart to apply";
    assert_eq!(idle.matches(notice).count(), 1);
    // Synthetic: drop the restart suffix but keep the two-cell right margin.
    let partial = idle.replace(
        notice,
        &format!(
            "{:>width$}",
            "✔ Update installed ·",
            width = notice.chars().count()
        ),
    );
    let read = |screen| {
        atm_claude_adapter::screen::observe_control_screen(
            atm_core::ScreenInput::Styled(screen),
            None,
        )
        .facts
    };
    let unread = read(&partial);
    assert_eq!(
        (unread.input, unread.turn, &unread.dialog),
        (InputState::Empty, TurnState::Unknown, &DialogState::None)
    );
    let readable = read(&idle);
    assert_eq!(
        (readable.input, readable.turn, &readable.dialog),
        (InputState::Empty, TurnState::NotRunning, &DialogState::None)
    );

    let mut bench = Bench::new().await;
    bench.script(Act::Deliver);
    bench
        .screens
        .set(worker(), with(ready("%1", "s1"), |o| o.facts = unread));
    let first = bench.send(worker(), "head").await;
    let follower = bench.send(worker(), "follower").await;
    bench.notice(first, "blocked.readiness_timeout").await;
    let head = bench.entry(worker(), first).await;
    let behind = bench.entry(worker(), follower).await;
    assert_eq!(
        (head.status, head.delivery.phase, head.delivery.attempt),
        (DeliveryStatus::Queued, DeliveryPhase::Blocked, 0)
    );
    assert_eq!(
        (
            behind.status,
            behind.delivery.blocked_by,
            behind.delivery.attempt
        ),
        (DeliveryStatus::Queued, Some(first), 0)
    );
    assert!(bench.entries().is_empty());

    // Change only what the next observation returns. No lifecycle hook,
    // retry command, or new message wakes the job.
    bench
        .screens
        .set(worker(), with(ready("%1", "s1"), |o| o.facts = readable));
    for id in [first, follower] {
        assert_eq!(bench.entered().await, id);
        let attempt = bench
            .event("next committed attempt", |event| attempted(event).is_some())
            .await;
        assert_eq!(attempted(&attempt), Some((id, 1, "%1")));
        let delivery = bench
            .event("next committed delivery", |event| {
                delivered(event).is_some()
            })
            .await;
        assert_eq!(delivered(&delivery), Some(id));
        let entry = bench.entry(worker(), id).await;
        assert_eq!(
            (entry.status, entry.delivery.attempt),
            (DeliveryStatus::Delivered, 1)
        );
    }
    assert!(bench.entries().is_empty());
    assert_eq!(bench.notes().await.len(), 1);
}

// s3-busy. Break: the unread-turn words replace the policy's own words for
// a look that does not read an empty editor with no dialog.
#[tokio::test(start_paused = true)]
async fn other_readiness_waits_keep_their_words() {
    for (case, change) in [
        (
            "input unread",
            (|o: &mut RecipientObservation| {
                o.facts.turn = TurnState::Unknown;
                o.facts.input = InputState::Unknown;
            }) as fn(&mut RecipientObservation),
        ),
        ("dialog unread", |o: &mut RecipientObservation| {
            o.facts.turn = TurnState::Unknown;
            o.facts.dialog = DialogState::Unknown;
        }),
    ] {
        let mut rig = ManualJob::new().await;
        rig.screens.set(worker(), with(ready("%1", "s1"), change));
        let id = rig.send().await;
        rig.round(0).await;
        rig.round(121_000).await;
        let progress = rig.job.service.board.progress(id);
        assert_eq!(progress.phase, Some(DeliveryPhase::Blocked), "{case}");
        assert_eq!(
            progress.reason.as_deref(),
            Some(FailureReason::ReadinessTimeout.to_string().as_str()),
            "{case}"
        );
    }
}
