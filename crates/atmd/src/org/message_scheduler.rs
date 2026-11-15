//! Recipient scheduling outside a reserved attempt: A2's rows of the
//! policy-step table in `messages`.
//!
//! One job per recipient holds that recipient's slot. Each round it accounts
//! for an attempt the executor returned from, persists late receipts (for a
//! finally failed message too), serves the owner's retry and give-up, takes a
//! fresh observation, and evaluates the real policy on history's snapshot for
//! every open message in FIFO order. A job never sends input: on PasteNow it
//! takes one of the eight admission permits and calls the executor once, and
//! nothing else is decided for that recipient until the executor has returned
//! from its cleanup and what it committed is folded.
//!
//! Bounds: at most 32 jobs, one per recipient. A recipient beyond that is
//! found again from committed state when a job ends, so nothing is parked per
//! wake; its messages stay queued meanwhile. Waits for readiness hold no
//! permit and no reservation. The owner's decisions wait per recipient, at
//! most as many as the service admits requests.
//!
//! Every step is evaluated on evidence copied immediately before it. An
//! error is never permission: no step, no input, and the round is tried again
//! later. After an attempt whose outcome cannot be accounted for, the job
//! decides nothing for its recipient until it can.
//!
//! Notices go to the owner's inbox once per message and reason while the
//! message is open, after the state that caused them is applied. A focus
//! notice needs a positive observation of the owner's focus: an unknown focus
//! waits like a focused pane, but does not claim the owner is there.
//! A positively running turn gets one informational owner note per queued,
//! unattempted message after thirty minutes; a new observed interval may notify
//! again. Lost observations break that interval, including across restart.

use super::message_delivery::{deliver, inbox_reference, ATTEMPT_WORK_BUDGET};
use super::message_history::{
    DeliverySnapshot, ExecutorHold, HistoryUpdate, PanePresence, RecipientObservation,
};
use super::message_inbox::Progress;
use super::messages::{
    focus_notice, Address, DeliveryPhase, DeliveryStatus, MessageContent, MessageError, MessageId,
    MessageKind, MessageRuntime, Service,
};
use super::OrgServiceError;
use crate::{control::reservation::PaneId, eventlog::Envelope};
use atm_core::ScreenFacts;
use atm_org::{
    delivery::{busy_notice_due, next_delivery, DeliveryStep, FailureReason, WaitReason},
    Agent, AgentId, DeliveryReceipt, OrgCommand,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinSet,
    time::Instant,
};

/// Recipient jobs alive at once, each for a different recipient.
pub(super) const MAX_JOBS: usize = 32;
/// After an error that is not a decision, look again no sooner than this.
const ERROR_BACKOFF: Duration = Duration::from_secs(30);
/// One observation of a recipient; a slower one counts as lost.
const OBSERVE_BUDGET: Duration = Duration::from_secs(5);
/// One write of the job's own and its fold: 6 s organisation execute and the
/// 4 s fold margin of the resolution budget (see `message_delivery`).
const COMMIT_BUDGET: Duration = Duration::from_secs(10);
/// Accounting after an attempt: the 5 s FIFO snapshot and the same 4 s margin.
const ACCOUNTING_BUDGET: Duration = Duration::from_secs(9);
const GIVE_UP_REASON: &str = "owner gave up on delivery";
const BUSY_NOTICE_REASON: &str = "running turn";

type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// The executor of one reserved attempt: `message_delivery::deliver`.
pub(super) type Deliver = Arc<
    dyn Fn(MessageRuntime, MessageId, Instant) -> Boxed<Result<(), MessageError>> + Send + Sync,
>;
/// A fresh observation of one recipient, or why there is none. It takes no
/// reservation and sends nothing.
pub(super) type Observe =
    Arc<dyn Fn(Agent) -> Boxed<Result<RecipientObservation, MessageError>> + Send + Sync>;

#[derive(Clone)]
pub(super) struct Effects {
    pub deliver: Deliver,
    pub observe: Observe,
}

impl Effects {
    /// The executor seam, and no observer yet: the shared screen dispatch it
    /// needs is not wired here. Without an observation the policy only waits.
    pub(super) fn production() -> Self {
        Self {
            deliver: Arc::new(|rt, id, deadline| Box::pin(deliver(rt, id, deadline))),
            observe: Arc::new(|_| {
                Box::pin(async {
                    Err(MessageError::Unavailable(
                        "recipient observation is not configured".into(),
                    ))
                })
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resolve {
    Retry,
    GiveUp,
}

fn not_blocked(id: MessageId) -> MessageError {
    MessageError::Conflict(format!(
        "message {id} is not blocked; inspect atm inbox --id {id}"
    ))
}

impl Resolve {
    /// The answer for a message that is already delivered.
    pub(super) fn refuse_delivered(self, id: MessageId) -> MessageError {
        match self {
            Self::Retry => not_blocked(id),
            Self::GiveUp => MessageError::Conflict(format!("message {id} is already delivered")),
        }
    }
}

pub(super) struct Resolution {
    pub kind: Resolve,
    pub id: MessageId,
    pub reply: oneshot::Sender<Result<DeliveryStatus, MessageError>>,
}

#[derive(Default)]
struct Shared {
    /// Presentation for the inbox; never a decision input.
    progress: HashMap<MessageId, Progress>,
    /// Notices already published for an open message, by reason.
    noticed: HashMap<MessageId, HashSet<String>>,
    /// Only the current observed turn, never an unbounded set of past turns.
    busy_turn: HashMap<MessageId, chrono::DateTime<chrono::Utc>>,
    /// The owner's decisions, waiting for their recipient's slot.
    resolutions: HashMap<AgentId, VecDeque<Resolution>>,
}

/// What jobs and requests share beside the committed projection.
pub(super) struct Board {
    shared: Mutex<Shared>,
    queued: watch::Sender<u64>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            shared: Mutex::default(),
            queued: watch::Sender::new(0),
        }
    }
}

impl Board {
    pub(super) fn progress(&self, id: MessageId) -> Progress {
        (self.shared.lock().ok())
            .and_then(|shared| shared.progress.get(&id).cloned())
            .unwrap_or_default()
    }

    fn show(&self, id: MessageId, progress: Progress) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.progress.insert(id, progress);
        }
    }

    /// The message is no longer open: nothing to show or to deduplicate.
    fn forget(&self, id: MessageId) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.progress.remove(&id);
            shared.noticed.remove(&id);
            shared.busy_turn.remove(&id);
        }
    }

    fn track_busy_turn(&self, id: MessageId, since: Option<chrono::DateTime<chrono::Utc>>) {
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        if shared.busy_turn.get(&id).copied() == since {
            return;
        }
        match since {
            Some(since) => {
                shared.busy_turn.insert(id, since);
            }
            None => {
                shared.busy_turn.remove(&id);
            }
        }
        if let Some(reasons) = shared.noticed.get_mut(&id) {
            reasons.remove(BUSY_NOTICE_REASON);
        }
    }

    /// Whether this notice is still to be published; `told` false takes a
    /// failed publication back.
    fn notice(&self, id: MessageId, reason: &str, told: bool) -> bool {
        let Ok(mut shared) = self.shared.lock() else {
            return false;
        };
        if told {
            shared.noticed.entry(id).or_default().insert(reason.into())
        } else {
            shared
                .noticed
                .get_mut(&id)
                .is_some_and(|reasons| reasons.remove(reason))
        }
    }

    /// Queue a decision for the recipient's slot and wake the scheduler.
    /// Decisions whose caller has gone are dropped here, so the queue is
    /// bounded by the requests the service admits.
    pub(super) fn resolve(
        &self,
        recipient: AgentId,
        resolution: Resolution,
    ) -> Result<(), MessageError> {
        let mut shared = (self.shared.lock())
            .map_err(|_| MessageError::Unavailable("message scheduler lock poisoned".into()))?;
        let queue = shared.resolutions.entry(recipient).or_default();
        queue.retain(|waiting| !waiting.reply.is_closed());
        queue.push_back(resolution);
        drop(shared);
        self.queued.send_modify(|n| *n = n.wrapping_add(1));
        Ok(())
    }

    /// The next decision somebody still waits for.
    fn next_resolution(&self, recipient: &AgentId) -> Option<Resolution> {
        let mut shared = self.shared.lock().ok()?;
        let queue = shared.resolutions.get_mut(recipient)?;
        let mut next = queue.pop_front();
        while next.as_ref().is_some_and(|next| next.reply.is_closed()) {
            next = queue.pop_front();
        }
        if queue.is_empty() {
            shared.resolutions.remove(recipient);
        }
        next
    }

    fn waiting(&self) -> Vec<AgentId> {
        (self.shared.lock())
            .map(|shared| shared.resolutions.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// Every recipient that needs its slot served: a waiting decision, an open
/// message, or a retained receipt.
fn due(service: &Service) -> Vec<AgentId> {
    let mut due = service.board.waiting();
    due.extend(service.recipients().unwrap_or_default());
    due.extend(
        (service.receipts(None).unwrap_or_default())
            .into_iter()
            .map(|receipt| receipt.recipient),
    );
    due
}

/// Start jobs for the recipients that need one, at most 32 at a time, until
/// the service is cancelled or fails; then let the running jobs finish their
/// attempt and stop.
pub(super) async fn run(service: Arc<Service>, effects: Effects) {
    let cancel = service.rt.cancel.clone();
    let ready = tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        ready = service.ready() => ready.is_ok(),
    };
    let mut folded = service.folded.subscribe();
    let mut evidence = service.rt.evidence.subscribe();
    let mut queued = service.board.queued.subscribe();
    let mut jobs: JoinSet<AgentId> = JoinSet::new();
    let mut running: HashSet<AgentId> = HashSet::new();
    while ready && service.running() {
        for recipient in due(&service) {
            if running.contains(&recipient) {
                continue;
            }
            if running.len() >= MAX_JOBS {
                break;
            }
            running.insert(recipient.clone());
            jobs.spawn(Job::new(service.clone(), effects.clone(), recipient).run());
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = service.stopped() => break,
            Some(done) = jobs.join_next() => match done {
                Ok(recipient) => {
                    running.remove(&recipient);
                }
                // Its recipient keeps its slot: an attempt of unknown outcome
                // must not be followed by another.
                Err(error) => tracing::error!(%error, "Message job failed; its recipient gets no delivery until restart"),
            },
            _ = folded.changed() => {}
            _ = evidence.changed() => {}
            _ = queued.changed() => {}
        }
    }
    while jobs.join_next().await.is_some() {}
}

enum Next {
    /// Nothing is left for this recipient.
    Idle,
    /// State changed under the round; evaluate again.
    Again,
    /// Evaluate at this instant, or earlier on any change.
    At(Instant),
    /// Only a change can move this recipient.
    Change,
}

fn sooner(a: Next, b: Next) -> Next {
    match (a, b) {
        (Next::Again, _) | (_, Next::Again) => Next::Again,
        (Next::At(a), Next::At(b)) => Next::At(a.min(b)),
        (Next::At(at), _) | (_, Next::At(at)) => Next::At(at),
        (Next::Change, _) | (_, Next::Change) => Next::Change,
        (Next::Idle, Next::Idle) => Next::Idle,
    }
}

/// A hold only the owner's retry (or a receipt) ends; every other block
/// resumes by itself once its cause clears.
fn latched(step: &DeliveryStep) -> bool {
    matches!(
        step,
        DeliveryStep::Blocked {
            reason: FailureReason::ProbablyDelivered | FailureReason::Restarted
        }
    )
}

/// What a blocked message is blocked by, in the executor's words when it left
/// a diagnostic for the attempt.
fn blocked_reason(reason: FailureReason, hold: Option<ExecutorHold>) -> String {
    match (reason, hold) {
        (FailureReason::ProbablyDelivered, Some(ExecutorHold::StagedMismatch)) => {
            "staged draft does not exactly match; Enter was not sent".into()
        }
        (FailureReason::ProbablyDelivered, Some(ExecutorHold::InputOutcomeUnknown)) => {
            "input outcome is unknown".into()
        }
        _ => reason.to_string(),
    }
}

/// The owner's notice for a blocked message, with the messages that wait
/// behind it. A retry is offered only for a latched hold: any other block
/// refuses it.
fn blocked_notice(id: MessageId, reason: &str, behind: &[MessageId], latched: bool) -> String {
    let behind = if behind.is_empty() {
        "none".to_string()
    } else {
        let ids: Vec<String> = behind.iter().map(ToString::to_string).collect();
        ids.join(", ")
    };
    let advice = if latched {
        format!(
            "Inspect the pane; atm msg retry {id} --accept-duplicate-risk or atm msg give-up {id}"
        )
    } else {
        format!("Delivery resumes when this clears; atm msg give-up {id} cancels it")
    };
    format!("{id} blocked: {reason}; waiting behind it: {behind}. {advice}")
}

fn note(body: String) -> MessageContent {
    MessageContent {
        from: Address::System,
        to: Address::Owner,
        kind: MessageKind::Note,
        body,
        attachments: vec![],
        task: None,
    }
}

/// An attempt the executor returned from, until what it committed is folded
/// and its veto, if it needs one, is recorded.
#[derive(Clone)]
struct Unaccounted {
    id: MessageId,
    /// The attempt number committed before the executor ran.
    before: Option<u32>,
    /// The executor's error, and whether it is one to look again soon after.
    error: Option<(String, bool)>,
}

struct Job {
    service: Arc<Service>,
    effects: Effects,
    recipient: AgentId,
    observed: Option<Instant>,
    /// The last recorded observation's focus sample; None when it was lost.
    focused: Option<bool>,
    unaccounted: Option<Unaccounted>,
    /// No attempt before this instant: the last one could not start.
    not_before: Option<Instant>,
    /// Messages this job has put on the board.
    shown: BTreeSet<MessageId>,
}

impl Job {
    fn new(service: Arc<Service>, effects: Effects, recipient: AgentId) -> Self {
        Self {
            service,
            effects,
            recipient,
            observed: None,
            focused: None,
            unaccounted: None,
            not_before: None,
            shown: BTreeSet::new(),
        }
    }

    async fn run(mut self) -> AgentId {
        let service = self.service.clone();
        let cancel = service.rt.cancel.clone();
        let mut folded = service.folded.subscribe();
        let mut evidence = service.rt.evidence.subscribe();
        let mut queued = service.board.queued.subscribe();
        while !cancel.is_cancelled() && service.running() {
            // Mark what this round will have seen before it looks.
            folded.mark_unchanged();
            evidence.mark_unchanged();
            queued.mark_unchanged();
            let next = match self.round(&mut queued).await {
                Ok(next) => next,
                Err(error) => {
                    tracing::warn!(recipient = %self.recipient, %error, "Message scheduling round failed; no input");
                    Next::At(Instant::now() + ERROR_BACKOFF)
                }
            };
            let wake = match next {
                Next::Idle => break,
                Next::Again => continue,
                Next::At(at) => Some(at),
                Next::Change => None,
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = service.stopped() => break,
                _ = folded.changed() => {}
                _ = evidence.changed() => {}
                _ = queued.changed() => {}
                () = async {
                    match wake {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
        for id in &self.shown {
            service.board.forget(*id);
        }
        self.recipient
    }

    async fn round(&mut self, queued: &mut watch::Receiver<u64>) -> Result<Next, MessageError> {
        let service = self.service.clone();
        if self.unaccounted.is_some() {
            return self.account().await;
        }
        service.settled().await?;
        // A receipt wins before anything else is decided for this recipient,
        // for a finally failed message too.
        for settle in service.receipts(Some(&self.recipient))? {
            if !settle.delivered {
                self.confirm(settle.id, &settle.receipt).await?;
            }
            service
                .rt
                .evidence
                .acknowledge(settle.id, &settle.receipt)?;
        }
        while let Some(resolution) = service.board.next_resolution(&self.recipient) {
            let result = self.resolve(resolution.kind, resolution.id).await;
            let _ = resolution.reply.send(result);
        }
        let open = service.open(&self.recipient)?;
        self.shown.retain(|id| {
            let kept = open.contains(id);
            if !kept {
                service.board.forget(*id);
            }
            kept
        });
        if open.is_empty() {
            return Ok(Next::Idle);
        }
        self.observe().await?;
        let mut next = Next::Change;
        for id in open {
            let step = self.step(id, queued).await?;
            if matches!(step, Next::Again) {
                return Ok(step);
            }
            next = sooner(next, step);
        }
        Ok(next)
    }

    /// One write of this job's own, bounded with its fold: a stopped fold
    /// loop must not hold the job, and with it the shutdown, for ever.
    async fn commit(&self, command: OrgCommand) -> Result<Vec<Envelope>, MessageError> {
        tokio::time::timeout(COMMIT_BUDGET, self.service.execute(command))
            .await
            .map_err(|_| MessageError::Timeout)?
    }

    fn show(
        &mut self,
        id: MessageId,
        phase: DeliveryPhase,
        reason: Option<String>,
        retry_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.shown.insert(id);
        self.service.board.show(
            id,
            Progress {
                phase: Some(phase),
                reason,
                retry_at,
                // The inbox reads the FIFO predecessor from history itself.
                blocked_by: None,
            },
        );
    }

    /// One observation per guard poll at most; history keeps the last one.
    async fn observe(&mut self) -> Result<(), MessageError> {
        let service = self.service.clone();
        let rt = &service.rt;
        let poll = rt.policy.settings().guard_poll_interval;
        if self.observed.is_some_and(|at| at.elapsed() < poll) {
            return Ok(());
        }
        let now = rt.clock.now()?;
        let recipient = self.recipient.clone();
        let agent = service.agent(&self.recipient)?;
        let Some(agent) = agent.filter(|agent| agent.pane.is_some()) else {
            // No pane: nothing to look at, and nobody can be focused on it.
            // Said positively, so that the agent's applied status decides,
            // not the unknown focus of a pane that does not exist.
            self.focused = Some(false);
            let observation = RecipientObservation {
                pane: None,
                session: None,
                presence: PanePresence::Unknown,
                facts: ScreenFacts::default(),
                copy_mode: None,
                focused: Some(false),
            };
            let update = HistoryUpdate::Observed {
                recipient,
                observation,
            };
            return match rt.history.record(update, now) {
                // The agent is not in history's state: nothing to decide on.
                Ok(()) | Err(MessageError::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            };
        };
        let observed = tokio::time::timeout(OBSERVE_BUDGET, (self.effects.observe)(agent))
            .await
            .unwrap_or(Err(MessageError::Timeout));
        self.observed = Some(Instant::now());
        let now = rt.clock.now()?;
        let recorded = observed.and_then(|observation| {
            let focused = observation.focused;
            let update = HistoryUpdate::Observed {
                recipient: recipient.clone(),
                observation,
            };
            rt.history.record(update, now).map(|()| focused)
        });
        match recorded {
            Ok(focused) => self.focused = focused,
            Err(error) => {
                // A failed or mismatched observation proves nothing: it is a
                // loss of continuity, never a dead pane. A recipient history
                // has never observed has nothing to lose, and a history that
                // cannot take the loss fails the step that follows.
                tracing::debug!(%recipient, %error, "Recipient observation lost");
                self.focused = None;
                let _ = (rt.history).record(HistoryUpdate::ObservationLost { recipient }, now);
            }
        }
        Ok(())
    }

    /// Evaluate one open message on fresh evidence and act on A2's rows.
    async fn step(
        &mut self,
        id: MessageId,
        queued: &mut watch::Receiver<u64>,
    ) -> Result<Next, MessageError> {
        let service = self.service.clone();
        let rt = &service.rt;
        let now = rt.clock.now()?;
        let facts = match rt.evidence.snapshot(id) {
            Ok(facts) => facts,
            Err(_) => return self.without_evidence(id, now).await,
        };
        let copied = rt.history.record(
            HistoryUpdate::Evidence {
                id,
                receipt: facts.receipt,
                probably_landed: facts.probably_landed,
            },
            now,
        );
        let snapshot = rt.history.snapshot(id, now)?;
        let step = next_delivery(&snapshot.message, &snapshot.live, now, &rt.policy)
            .map_err(|error| MessageError::Internal(error.to_string()))?;
        match copied {
            Ok(()) => {}
            // Delivered while this round ran: the snapshot says so.
            Err(MessageError::Conflict(_)) if step == DeliveryStep::Complete => {}
            Err(error) => return Err(error),
        }
        if !matches!(step, DeliveryStep::PasteNow { .. }) {
            // The executor consumes PasteNow itself, inside its reservation.
            let consumed = HistoryUpdate::Consumed {
                id,
                step: step.clone(),
            };
            match rt.history.record(consumed, now) {
                Ok(()) => {}
                // No longer the current decision: evaluate again.
                Err(MessageError::Conflict(_)) => return Ok(Next::Again),
                Err(error) => return Err(error),
            }
        }
        service
            .board
            .track_busy_turn(id, snapshot.live.running_turn_since);
        let poll = Instant::now() + rt.policy.settings().guard_poll_interval;
        match step {
            // Delivered while this round ran. Its fold, which this job has
            // yet to see, removes it from the open messages.
            DeliveryStep::Complete => Ok(Next::Change),
            DeliveryStep::Wait { until, reason } => {
                self.show(id, DeliveryPhase::Waiting, Some(reason.to_string()), None);
                if busy_notice_due(&snapshot.message, &snapshot.live, now) {
                    let body = format!(
                        "{} busy for 30 min; message still queued ({id})",
                        snapshot.recipient.id
                    );
                    self.publish(id, BUSY_NOTICE_REASON, note(body)).await;
                }
                if reason == WaitReason::OwnerFocused && self.focused == Some(true) {
                    self.focus_notice(id, &snapshot.recipient).await;
                }
                Ok(if reason == WaitReason::OtherDelivery {
                    // Its turn comes with its predecessor's committed outcome.
                    Next::Change
                } else {
                    Next::At(rt.clock.deadline(until)?)
                })
            }
            DeliveryStep::AwaitConfirmation { until } => {
                self.show(id, DeliveryPhase::Delivering, None, None);
                Ok(Next::At(rt.clock.deadline(until)?.min(poll)))
            }
            DeliveryStep::Confirm { receipt } => {
                self.confirm(id, &receipt).await?;
                rt.evidence.acknowledge(id, &receipt)?;
                Ok(Next::Again)
            }
            DeliveryStep::RetryAt { at, reason } => {
                if snapshot.message.status == DeliveryStatus::Delivering {
                    // Outside an attempt the retry is A2's to record; the next
                    // round schedules from the committed failure time.
                    self.commit(OrgCommand::DeliveryFailed {
                        message_id: id,
                        reason: reason.to_string(),
                        final_failure: false,
                    })
                    .await?;
                    return Ok(Next::Again);
                }
                self.show(
                    id,
                    DeliveryPhase::Waiting,
                    Some(reason.to_string()),
                    Some(at),
                );
                Ok(Next::At(rt.clock.deadline(at)?))
            }
            DeliveryStep::Fail { reason } => {
                let failed = self
                    .commit(OrgCommand::DeliveryFailed {
                        message_id: id,
                        reason: reason.to_string(),
                        final_failure: true,
                    })
                    .await?;
                if !failed.is_empty() {
                    let body =
                        format!("{id} failed: {reason}. Send it again if it is still needed");
                    self.publish(id, &format!("failed: {reason}"), note(body))
                        .await;
                }
                Ok(Next::Again)
            }
            DeliveryStep::Blocked { reason } => {
                let latched = latched(&step);
                let text = blocked_reason(reason, snapshot.hold);
                self.show(id, DeliveryPhase::Blocked, Some(text.clone()), None);
                self.blocked_notice(id, &text, latched).await;
                // Only these reasons end with a new observation of the pane.
                Ok(match reason {
                    FailureReason::CopyMode
                    | FailureReason::DraftPresent
                    | FailureReason::DialogOpen
                    | FailureReason::ReadinessTimeout => Next::At(poll),
                    _ => Next::Change,
                })
            }
            DeliveryStep::PasteNow { .. } => self.attempt(&snapshot, queued).await,
            // A step this build does not know grants nothing.
            _ => Err(MessageError::Internal(
                "unknown delivery step; no input".into(),
            )),
        }
    }

    /// Evidence cannot answer for this message (its reference was refused at
    /// capacity, or the store is disabled). Nothing is decided without it:
    /// the message is shown as blocked and the owner is told once, unless it
    /// only waits behind another message.
    async fn without_evidence(
        &mut self,
        id: MessageId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Next, MessageError> {
        let service = self.service.clone();
        let snapshot = service.rt.history.snapshot(id, now)?;
        if matches!(
            snapshot.message.status,
            DeliveryStatus::Delivered | DeliveryStatus::Failed
        ) {
            // Settled while this round ran: delivered and acknowledged, or
            // failed for good and forgotten. Expecting it again would hold
            // its reference for ever.
            return Ok(Next::Change);
        }
        let Err(error) = service.rt.evidence.register(id, inbox_reference(id)) else {
            return Ok(Next::Again);
        };
        let text = error.to_string();
        self.show(id, DeliveryPhase::Blocked, Some(text.clone()), None);
        if snapshot.blocked_by.is_none() {
            self.blocked_notice(id, &text, false).await;
        }
        Ok(Next::At(Instant::now() + ERROR_BACKOFF))
    }

    /// Persist a receipt as Delivered and wait for its fold; the caller then
    /// lets evidence forget the message. A failure leaves the receipt
    /// retained, and it is settled again later: no input follows it.
    async fn confirm(&self, id: MessageId, receipt: &DeliveryReceipt) -> Result<(), MessageError> {
        self.commit(OrgCommand::Delivered {
            message_id: id,
            session_id: receipt.session_id.clone(),
            confirmation: receipt.confirmation.clone(),
        })
        .await?;
        Ok(())
    }

    /// One reserved attempt by the executor, under one of eight permits held
    /// until it has returned from its cleanup.
    async fn attempt(
        &mut self,
        snapshot: &DeliverySnapshot,
        queued: &mut watch::Receiver<u64>,
    ) -> Result<Next, MessageError> {
        if let Some(until) = self.not_before.filter(|until| Instant::now() < *until) {
            return Ok(Next::At(until));
        }
        let id = snapshot.message.id;
        let service = self.service.clone();
        let rt = &service.rt;
        let permit = tokio::select! {
            biased;
            () = rt.cancel.cancelled() => return Ok(Next::Change),
            // The owner's decision does not wait behind a wait for admission.
            _ = queued.changed() => return Ok(Next::Again),
            permit = rt.admissions.clone().acquire_owned() => permit
                .map_err(|_| MessageError::Unavailable("message admissions closed".into()))?,
        };
        self.show(id, DeliveryPhase::Delivering, None, None);
        let deadline = Instant::now() + ATTEMPT_WORK_BUDGET;
        let delivered = (self.effects.deliver)(rt.clone(), id, deadline).await;
        drop(permit);
        self.unaccounted = Some(Unaccounted {
            id,
            before: snapshot.message.last_attempt.as_ref().map(|a| a.attempt),
            error: delivered.err().map(|error| {
                let soon = matches!(
                    error,
                    MessageError::InvalidParams(_)
                        | MessageError::NotFound(_)
                        | MessageError::Conflict(_)
                        | MessageError::Refused(_)
                        | MessageError::Reservation(_)
                );
                (error.to_string(), soon)
            }),
        });
        self.account().await
    }

    /// Account for what the executor committed before anything else is
    /// decided, and never lose a veto to an error. Its commands went through
    /// the one FIFO writer: a snapshot answered after them, then the fold,
    /// covers a commit whose reply was lost. Until this succeeds the attempt
    /// stays unaccounted and the job decides nothing.
    async fn account(&mut self) -> Result<Next, MessageError> {
        let Some(attempt) = self.unaccounted.clone() else {
            return Ok(Next::Again);
        };
        let service = self.service.clone();
        let rt = &service.rt;
        let id = attempt.id;
        let folded = async {
            rt.org.snapshot().await?;
            let committed = rt.org.status()?;
            rt.history.folded(committed).await
        };
        let accounted = tokio::time::timeout(ACCOUNTING_BUDGET, folded)
            .await
            .unwrap_or(Err(MessageError::Timeout))
            .and_then(|()| rt.history.snapshot(id, rt.clock.now()?));
        let snapshot = match accounted {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let reason = format!("input outcome is not accounted for: {error}");
                self.show(id, DeliveryPhase::Blocked, Some(reason), None);
                return Err(error);
            }
        };
        let poll = rt.policy.settings().guard_poll_interval;
        let attempted = snapshot.message.last_attempt.as_ref().map(|a| a.attempt) != attempt.before;
        if !attempted {
            // Nothing was attempted: look again later, never paste directly.
            let wait = match &attempt.error {
                Some((_, false)) => ERROR_BACKOFF,
                _ => poll,
            };
            if let Some((error, _)) = attempt.error {
                tracing::debug!(%id, %error, "Message attempt did not start");
                self.show(id, DeliveryPhase::Waiting, Some(error), None);
            }
            let until = Instant::now() + wait;
            self.not_before = Some(until);
            self.unaccounted = None;
            return Ok(Next::At(until));
        }
        // An attempt is committed. If the executor reported an error and
        // neither settled the attempt nor left a hold, record one: no error
        // leads to another paste without a definite no-effect fact.
        if attempt.error.is_some()
            && snapshot.message.status == DeliveryStatus::Delivering
            && snapshot.hold.is_none()
        {
            let attempt_seq = snapshot.last_attempt_seq.ok_or_else(|| {
                MessageError::Internal("committed attempt has no sequence".into())
            })?;
            let held = HistoryUpdate::Held {
                id,
                attempt_seq,
                hold: ExecutorHold::InputOutcomeUnknown,
            };
            rt.history.record(held, rt.clock.now()?)?;
        }
        self.unaccounted = None;
        Ok(Next::Again)
    }

    /// The owner's retry or give-up, inside the recipient's slot: an active
    /// attempt has settled, and a receipt has already won.
    async fn resolve(
        &mut self,
        kind: Resolve,
        id: MessageId,
    ) -> Result<DeliveryStatus, MessageError> {
        let service = self.service.clone();
        let rt = &service.rt;
        let message = service
            .message(id)?
            .ok_or_else(|| MessageError::NotFound(format!("message {id} was not found")))?;
        match (kind, message.status) {
            (_, DeliveryStatus::Delivered) => Err(kind.refuse_delivered(id)),
            (Resolve::GiveUp, DeliveryStatus::Failed) => Ok(DeliveryStatus::Failed),
            (Resolve::GiveUp, _) => {
                self.commit(OrgCommand::DeliveryFailed {
                    message_id: id,
                    reason: GIVE_UP_REASON.into(),
                    final_failure: true,
                })
                .await?;
                Ok(DeliveryStatus::Failed)
            }
            (Resolve::Retry, DeliveryStatus::Failed) => Err(not_blocked(id)),
            (Resolve::Retry, status) => {
                let now = rt.clock.now()?;
                let facts = rt.evidence.snapshot(id)?;
                rt.history.record(
                    HistoryUpdate::Evidence {
                        id,
                        receipt: facts.receipt,
                        probably_landed: facts.probably_landed,
                    },
                    now,
                )?;
                let snapshot = rt.history.snapshot(id, now)?;
                let step = next_delivery(&snapshot.message, &snapshot.live, now, &rt.policy)
                    .map_err(|error| MessageError::Internal(error.to_string()))?;
                // Only a latched hold is the owner's to release. A wait or a
                // recoverable block resumes without a duplicate risk.
                let (Some(attempt), Some(attempt_seq), true) = (
                    snapshot.message.last_attempt.as_ref(),
                    snapshot.last_attempt_seq,
                    latched(&step),
                ) else {
                    return Err(not_blocked(id));
                };
                if rt.policy.is_final_attempt(attempt.attempt) {
                    return Err(MessageError::Refused(format!(
                        "message {id} has used its {} delivery attempts; give up or send it again",
                        attempt.attempt
                    )));
                }
                // Evidence first: history's next copy must not find the old
                // latch. Scopes and receipts stay for a late receipt.
                rt.evidence.release(id, attempt_seq)?;
                rt.history
                    .record(HistoryUpdate::OwnerRetry { id, attempt_seq }, now)?;
                // A block after this retry is news to the owner again.
                service.board.forget(id);
                Ok(status)
            }
        }
    }

    async fn blocked_notice(&self, id: MessageId, reason: &str, latched: bool) {
        let behind: Vec<MessageId> = (self.service.open(&self.recipient).unwrap_or_default())
            .into_iter()
            .filter(|other| *other > id)
            .collect();
        let body = blocked_notice(id, reason, &behind, latched);
        self.publish(id, reason, note(body)).await;
    }

    async fn focus_notice(&self, id: MessageId, recipient: &Agent) {
        let pane = recipient.pane.as_deref().map(PaneId::parse);
        if let Some(Ok(pane)) = pane {
            self.publish(id, "owner focused", focus_notice(id, &pane))
                .await;
        }
    }

    /// One inbox note per message and reason while the message is open. A
    /// notice is told after the state that caused it was applied, and a
    /// failure to tell it grants nothing: the block stays. A notice the
    /// writer did not take is told again later; one whose commit is unknown
    /// is not repeated.
    async fn publish(&self, id: MessageId, reason: &str, content: MessageContent) {
        let board = &self.service.board;
        if !board.notice(id, reason, true) {
            return;
        }
        if let Err(error) = self.commit(OrgCommand::SendMessage(content)).await {
            tracing::warn!(%id, %error, "Owner notice was not committed");
            let unknown = matches!(
                error,
                MessageError::Timeout | MessageError::Organisation(OrgServiceError::Timeout)
            );
            if !unknown {
                board.notice(id, reason, false);
            }
        }
    }
}

#[cfg(test)]
#[path = "message_scheduler_tests.rs"]
mod tests;
