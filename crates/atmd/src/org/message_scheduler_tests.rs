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
    message_evidence::{AttemptScope, MAX_MESSAGES},
    message_history::{EffectCertainty, PanePresence, SessionObservation},
    message_service_tests::{
        code, empty_editor, guarded, other, owner, page, pane, until, utc, worker, Rig,
    },
    messages::{
        is_focus_notice, AgentId, InboxEntry, InboxParams, ResolveParams, MESSAGE_ADMISSION_LIMIT,
    },
};
use super::*;
use crate::org::message_delivery::MESSAGE_RESOLUTION_BUDGET;
use atm_core::{Harness, InputState, LifecycleEvent, SessionId, SessionStatus};
use atm_org::OrgEvent;
use std::collections::BTreeMap;
use tokio::sync::{broadcast, mpsc, Semaphore};

const OPEN: usize = 1 << 20;

/// What the observer answers for each recipient; a test changes it as it goes.
#[derive(Clone)]
struct Screens {
    seen: Arc<Mutex<HashMap<AgentId, RecipientObservation>>>,
    calls: Arc<Mutex<BTreeMap<AgentId, usize>>>,
    /// Closed, it holds every observation after it was counted.
    gate: Arc<Semaphore>,
}

impl Default for Screens {
    fn default() -> Self {
        Self {
            seen: Arc::default(),
            calls: Arc::default(),
            gate: Arc::new(Semaphore::new(OPEN)),
        }
    }
}

impl Screens {
    fn set(&self, agent: AgentId, observation: RecipientObservation) {
        self.seen.lock().unwrap().insert(agent, observation);
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
        Arc::new(move |agent: Agent| {
            let screens = screens.clone();
            Box::pin(async move {
                *screens
                    .calls
                    .lock()
                    .unwrap()
                    .entry(agent.id.clone())
                    .or_default() += 1;
                pass(&screens.gate).await;
                (screens.seen.lock().unwrap().get(&agent.id).cloned())
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
        tokio::spawn(async move { org.execute(attempted, now).await });
        // The writer has the command before the executor returns.
        tokio::task::yield_now().await;
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
        excluded_sessions: HashSet::new(),
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
                prompt: Some(inbox_reference(id)),
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
        let rig = Rig::scheduled(Effects {
            deliver,
            observe: screens.effect(),
        })
        .await;
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
        let result = guarded(self.rig.messages.give_up(ResolveParams { id }, owner())).await;
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
    cancel: tokio_util::sync::CancellationToken,
}

impl ManualJob {
    async fn new() -> Self {
        use super::super::{
            message_service_tests::runtime, messages::spawn_service_for_test, Request,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        let (requests, receive) = mpsc::channel::<Request>(64);
        let (health, status) = watch::channel(Ok(0));
        let (committed, _) = broadcast::channel(1024);
        let cancel = tokio_util::sync::CancellationToken::new();
        let seq = AtomicU64::new(0);
        let gate = Arc::new(Semaphore::new(1));
        let append_gate = gate.clone();
        tokio::spawn(super::super::actor::run(
            atm_org::OrgState::default(),
            receive,
            health,
            committed.clone(),
            cancel.clone(),
            move |_| {
                let next = seq.fetch_add(1, Ordering::SeqCst) + 1;
                let gate = append_gate.clone();
                async move {
                    let _permit = gate.acquire().await.unwrap();
                    Ok::<u64, crate::eventlog::EventLogError>(next)
                }
            },
        ));
        let org = super::super::OrgHandle {
            requests,
            health: status,
            committed,
            reads: None,
        };
        let rt = runtime(org.clone(), 0, &cancel);
        let (messages, _) = spawn_service_for_test(rt, org.committed.subscribe(), 0, None);
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
            },
            worker(),
        );
        let rig = Self {
            job,
            messages,
            screens,
            gate,
            cancel,
        };
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
    }
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
    assert!(!shared.busy_turn.contains_key(&id));
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
    assert_eq!(rig.notes().await.len(), 2);
    // Terminal cleanup discards the per-turn marker and notice dedupe.
    rig.command(OrgCommand::DeliveryFailed {
        message_id: id,
        reason: "owner cancelled".into(),
        final_failure: true,
    })
    .await;
    rig.round(0).await;
    let shared = rig.job.service.board.shared.lock().unwrap();
    assert!(!shared.busy_turn.contains_key(&id));
    assert!(!shared.noticed.contains_key(&id));
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
    rig.round(1_800_000).await;
    rig.gate.add_permits(1);
    guarded(rig.job.service.settled()).await.unwrap();
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
    rig.job.publish(id, "test refusal", invalid).await;
    rig.job
        .publish(id, "test refusal", note("retry accepted".into()))
        .await;
    rig.job
        .publish(id, "test refusal", note("duplicate".into()))
        .await;
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
         Delivery resumes when this clears; atm msg give-up m5 cancels it"
    );
    assert_eq!(
        blocked_notice(id, PROBABLY, &[next, last], true),
        "m5 blocked: unconfirmed, probably delivered; inspect before resending; \
         waiting behind it: m7, m9. Inspect the pane; \
         atm msg retry m5 --accept-duplicate-risk or atm msg give-up m5"
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
         atm msg give-up {id} cancels it"
    )
}

async fn waits_unobserved(bench: &Bench, id: MessageId) {
    let view = bench.entry(worker(), id).await;
    assert_eq!(view.status, DeliveryStatus::Queued);
    assert_eq!(view.delivery.phase, DeliveryPhase::Waiting);
    assert_eq!(view.delivery.reason.as_deref(), Some(UNOBSERVED_REASON));
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
async fn one_unobserved_notice_for_the_oldest_open_message() {
    let bench = Bench::blind().await;
    let first = bench.send(worker(), "first").await;
    let second = bench.send(worker(), "second").await;
    bench.pass(3600).await;
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
async fn an_agent_without_a_pane_ends_the_loss() {
    let bench = Bench::blind().await;
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
    assert_eq!(bench.notes().await, [down, unobserved(id)]);
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
             Delivery resumes when this clears; atm msg give-up {first} cancels it"
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
    bench.send(worker(), "brief").await;
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
    let shared = service.board.shared.lock().unwrap();
    assert!(shared.noticed.is_empty(), "{:?}", shared.noticed);
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
    let mut bench = Bench::new().await;
    let id = bench.probably_delivered().await;
    assert_eq!(bench.entries(), []);
    assert_eq!(bench.retry(id).await.unwrap(), DeliveryStatus::Delivering);
    // The session keeps working between the release and the new attempt:
    // that must not hold the released attempt again.
    bench.activity("s1", "%1");
    let retried = bench
        .event("the second attempt", |e| attempted(e).is_some())
        .await;
    assert_eq!(attempted(&retried), Some((id, 2, "%1")));
    assert_eq!(bench.entered().await, id);
    assert_eq!(bench.worked().await, id);
    // After the new attempt is armed, activity holds it as before, and that
    // block is told again.
    bench.activity("s1", "%1");
    bench.event("the second notice", system_note).await;
    bench.pass(120).await;
    assert_eq!(bench.entries(), []);
    let notes = bench.notes().await;
    assert_eq!(notes, vec![blocked_notice(id, PROBABLY, &[], true); 2]);
    // The same message throughout: nothing new was sent to the agent.
    let inbox = bench.rig.inbox(pane("%1"), page()).await;
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].delivery.attempt, 2);
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
    let mut give_up =
        tokio::spawn(async move { messages.give_up(ResolveParams { id }, owner()).await });
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
        .give_up(ResolveParams { id }, owner())
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
        .give_up(ResolveParams { id }, pane("%1"))
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
    let mut bench = Bench::blind().await;
    let draft = with(ready("%1", "s1"), |o| o.facts.input = InputState::Draft);
    bench.screens.set(worker(), draft);
    let id = bench.send(worker(), "brief").await;
    // The writer stalls just before the block is due: the notice's append is
    // entered and held past the job's budget, so its commit is unknown.
    bench.pass(110).await;
    bench.rig.hold();
    bench.rig.entered().await;
    bench.pass(60).await;
    assert!(bench.entries().is_empty());
    bench.rig.release();
    bench.event("the blocked notice", system_note).await;
    bench.pass(300).await;
    // Told once: a commit of unknown outcome is not sent again.
    assert_eq!(bench.notes().await.len(), 1);
    assert!(bench.entries().is_empty());
    assert_eq!(
        bench.entry(worker(), id).await.delivery.phase,
        DeliveryPhase::Blocked
    );
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
