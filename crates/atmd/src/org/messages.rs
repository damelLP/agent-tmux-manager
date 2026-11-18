//! Confirmed-message facade and the reviewed A/B/C runtime boundary.
//!
//! All callers share one injected monotonic/base-UTC clock. A1 owns pure
//! history, A2 the service/evidence/wiring, B one reserved attempt, and C the
//! CLI/notifications.
//!
//! # The service
//!
//! `spawn_messages` starts one fold loop over every committed organisation
//! event. It is the only writer of `HistoryUpdate::Applied` and of the
//! service's own committed projection, in sequence order, unknown kinds
//! included; a gap or an event it cannot apply stops the service (typed
//! unavailable) rather than skipping. Requests never run on that loop: they
//! read the projection under a short lock and submit commands to the one
//! organisation writer, so a stalled append blocks neither the fold nor an
//! inbox read. A reply to a write waits until its committed sequence is folded.
//! At most 64 requests are in flight; one more is refused, never parked.
//! A caller that goes away does not cancel a command the writer accepted.
//!
//! Delivery itself is scheduled by `message_scheduler`: one job per recipient,
//! which also serves the owner's retry and give-up. On cancellation the fold
//! loop keeps running until those jobs have let their attempt settle.
//!
//! # Policy-step ownership
//!
//! | Step / transition | Owner |
//! |---|---|
//! | Wait, readiness observe, wake/FIFO scheduling | A2, with A1 clocks; no reservation held |
//! | PasteNow admission | A2 recipient slot and one of eight permits; B reserves and rechecks |
//! | AttemptCommitted, arm, guarded paste/Enter | B persists Attempted, then awaits its fold before arm/input; record consumed step/paste before release |
//! | AwaitConfirmation while reserved | B; no more keys |
//! | Confirm while reserved | B persists Delivered, then awaits its fold before acknowledging evidence |
//! | Late receipt after release/failure/give-up | A2 persists/folds Delivered; same ID cancels pending retry |
//! | RetryAt while Delivering | B while active; A2 alone after explicit handoff of non-active/recovered work |
//! | RetryAt while Queued | A2 schedules only; no duplicate failure event |
//! | Fail during work/cleanup | B, only after safe effect accounting; uncertain input remains held |
//! | Fail outside an attempt | A2 persists/folds, then publishes notice |
//! | Blocked | B settles/releases; A2 retains logical recipient ownership and deduplicates notices |
//! | Complete | No input/event; A2 retains eligible late-receipt evidence |
//! | Owner retry/give-up | A2 recipient slot, await active B settlement; no Observe reservation |
//!
//! A2 holds the admission permit through B's cleanup, not through long readiness
//! waits. Its recipient slot also serializes resolution and post-release receipt
//! writes: B and A2 must never independently settle the same active attempt.
//! Retry retains the ID and attempt budget; give-up may later be superseded by
//! a receipt. OwnerFocused notices stay in the inbox, without external notify.
//! B builds the exact one-line inbox reference for every message (for a start
//! line, its own text: see `start_line`) and registers that text with evidence.
//! Receipt means reference acceptance, not body reading.
//! Delivery-notice keys are retained owner notes in the committed projection,
//! replayed before scheduling. Recognized terminal failure facts retain missing
//! note work separately from the open delivery queue. A vanished transient
//! observation before its first note commit is not recoverable after a crash;
//! external notification remains best-effort, not exactly-once.
//! B waits for U8c focus/read-only observe and U8's single screen dispatch; this
//! boundary neither adds vendor parsing nor implements a second input path.

use super::message_history::{
    HistoryUpdate, PanePresence, RecipientObservation, SessionObservation,
};
use super::message_inbox::{caller_address, inbox, Progress};
pub use super::message_methods::register_message_methods;
use super::message_scheduler::{self, Board, Effects, Resolution, Resolve};
pub use super::message_types::*;
use super::{
    agents::{matches_agent, AgentHandle},
    message_delivery::MESSAGE_RESOLUTION_BUDGET,
    message_evidence::{AttemptScope, MessageEvidence},
    message_history::MessageHistoryHandle,
    start_line::delivery_line,
    EventWatch, OrgHandle, OrgServiceError,
};
use crate::{
    control::{
        reservation::{PaneCapture, PaneId},
        service::ControlHandle,
    },
    eventlog::Envelope,
    registry::RegistryHandle,
    server::methods::{CallContext, MethodError},
};
use atm_core::{ScreenFacts, SessionView};
use atm_org::{
    delivery::DeliveryPolicy, Agent, AgentStatus, DeliveryNotice, DeliveryReceipt, Message,
    OrgCommand, OrgEvent, OrgState,
};
use chrono::{DateTime, TimeDelta, Utc};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch, Semaphore},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("invalid message parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Refused(String),
    #[error("message request timed out; accepted work may have completed; inspect inbox before retrying")]
    Timeout,
    #[error("message service unavailable: {0}")]
    Unavailable(String),
    #[error("message operation failed: {0}")]
    Internal(String),
    #[error(transparent)]
    Organisation(#[from] OrgServiceError),
    #[error(transparent)]
    Reservation(#[from] crate::control::reservation::ReservationError),
}

/// One injected time origin shared by the service, executor and evidence.
/// Pure history takes the derived UTC as an argument, never reads a clock itself.
#[derive(Debug, Clone)]
pub struct MessageClock {
    base_instant: Instant,
    base_utc: DateTime<Utc>,
}

impl MessageClock {
    #[must_use]
    pub fn new(base_instant: Instant, base_utc: DateTime<Utc>) -> Self {
        Self {
            base_instant,
            base_utc,
        }
    }

    /// Checked projection, including when a supplied instant precedes the origin.
    pub fn at(&self, instant: Instant) -> Result<DateTime<Utc>, MessageError> {
        let elapsed = instant
            .checked_duration_since(self.base_instant)
            .and_then(|elapsed| TimeDelta::from_std(elapsed).ok());
        elapsed
            .and_then(|elapsed| self.base_utc.checked_add_signed(elapsed))
            .ok_or_else(|| MessageError::Internal("message clock is outside its time range".into()))
    }

    /// Convert a policy UTC deadline to the same monotonic origin. A time at or
    /// before the UTC origin is already due and returns the monotonic origin.
    pub fn deadline(&self, at: DateTime<Utc>) -> Result<Instant, MessageError> {
        if at <= self.base_utc {
            return Ok(self.base_instant);
        }
        at.signed_duration_since(self.base_utc)
            .to_std()
            .ok()
            .and_then(|elapsed| self.base_instant.checked_add(elapsed))
            .ok_or_else(|| {
                MessageError::Internal("message deadline is outside its time range".into())
            })
    }

    /// Uses Tokio's clock so paused-time tests exercise the production time source.
    pub fn now(&self) -> Result<DateTime<Utc>, MessageError> {
        self.at(Instant::now())
    }
}

/// Reserve room in U8c's shared pool for lifecycle work from other services.
pub const MESSAGE_ADMISSION_LIMIT: usize = 8;

/// Cloned dependencies, not a second organisation writer or recipient scheduler.
/// A2 builds once per daemon; clone to share the admission cap and evidence clock.
#[derive(Clone)]
pub struct MessageRuntime {
    pub org: OrgHandle,
    pub registry: RegistryHandle,
    pub control: ControlHandle,
    /// What is recorded about a recipient's pane lives in the agent service's
    /// memory: the executor and the observer read a recipient through it.
    pub agents: AgentHandle,
    pub evidence: MessageEvidence,
    pub history: MessageHistoryHandle,
    pub policy: DeliveryPolicy,
    pub clock: MessageClock,
    /// A2 stops scheduling on cancellation; B settles cooperatively at safe points.
    pub cancel: CancellationToken,
    /// A2 acquires before B's admission and retains the permit through cleanup.
    pub admissions: Arc<Semaphore>,
}

impl MessageRuntime {
    /// Pure dependency assembly; no tasks, observations, writes or input effects.
    // One argument per shared daemon service: they have no common owner to bundle.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        org: OrgHandle,
        registry: RegistryHandle,
        control: ControlHandle,
        agents: AgentHandle,
        evidence: MessageEvidence,
        history: MessageHistoryHandle,
        policy: DeliveryPolicy,
        cancel: CancellationToken,
    ) -> Self {
        let clock = evidence.clock().clone();
        Self {
            org,
            registry,
            control,
            agents,
            evidence,
            history,
            policy,
            clock,
            cancel,
            admissions: Arc::new(Semaphore::new(MESSAGE_ADMISSION_LIMIT)),
        }
    }
}

/// A2 owns the one capture converter; B calls it while reserved. Facts come
/// from U8's single adapter dispatch and session is already uniquely eligible.
///
/// A pure mapping of what one capture positively shows. The pane is the
/// capture's own canonical pane; a capture without one is an error, and the
/// caller records ObservationLost for it, as for a failed capture. A dead pane
/// (its process exited, the pane retained) is Gone: nothing there can take
/// input. Copy mode is the inspected flag. Focus is the capture's sample and
/// stays unknown when the capture could not tell; it is never guessed. The
/// session and the screen facts pass through unchanged: this function reads
/// no screen text and knows no harness.
pub fn recipient_observation(
    capture: &PaneCapture,
    session: Option<SessionObservation>,
    facts: ScreenFacts,
) -> Result<RecipientObservation, MessageError> {
    let pane = PaneId::parse(capture.inspection.pane.pane_id.as_str())?;
    Ok(RecipientObservation {
        pane: Some(pane),
        session,
        presence: if capture.inspection.dead {
            PanePresence::Gone
        } else {
            PanePresence::Alive
        },
        facts,
        copy_mode: Some(capture.inspection.in_mode),
        focused: capture.focused,
    })
}

pub(super) const FOCUS_NOTICE_REASON: &str = "focus";
const FOCUS_NOTICE_SUFFIX: &str =
    "; delivery will resume after focus moves and the editor is safely empty";

/// A2 produces this inbox-only notice; C suppresses its external notification.
#[must_use]
pub fn focus_notice(id: MessageId, pane: &PaneId) -> MessageContent {
    MessageContent {
        delivery_notice: Some(DeliveryNotice {
            source_message_id: id,
            reason: FOCUS_NOTICE_REASON.into(),
        }),
        from: Address::System,
        to: Address::Owner,
        kind: MessageKind::Note,
        body: format!("{id} waiting: owner is focused on {pane}{FOCUS_NOTICE_SUFFIX}"),
        attachments: vec![],
        task: None,
    }
}

/// Recognize only the complete canonical system focus notice, not a substring.
#[must_use]
pub fn is_focus_notice(content: &MessageContent) -> bool {
    let Some((id, pane)) = content
        .body
        .strip_suffix(FOCUS_NOTICE_SUFFIX)
        .and_then(|body| body.split_once(" waiting: owner is focused on "))
    else {
        return false;
    };
    match (id.parse::<MessageId>(), PaneId::parse(pane)) {
        (Ok(id), Ok(pane)) => {
            let mut canonical = focus_notice(id, &pane);
            // Retained pre-upgrade canonical focus notes stay inbox-only too.
            if content.delivery_notice.is_none() {
                canonical.delivery_notice = None;
            }
            content == &canonical
        }
        _ => false,
    }
}

/// Send, inbox and ack answer within 8 s, queue wait and organisation work
/// included; the router allows 10 s (see `message_methods`).
const ORDINARY_BUDGET: Duration = Duration::from_secs(8);

const SEND_IDENTITY_BUDGET: Duration = Duration::from_secs(5);
/// Requests in flight. A full service refuses the next one; nothing is parked.
const MAX_REQUESTS: usize = 64;
/// Read-transition counts kept for acknowledgements that await their fold.
const MAX_READ_COUNTS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Startup {
    Replaying,
    Ready,
    Failed(String),
}

/// The committed projection: every applied event, in order, exactly once.
#[derive(Debug, Default)]
struct View {
    state: OrgState,
    cursor: u64,
    /// For each recent MessageRead event, how many messages it marked read.
    /// Counted against the state just before that event, so a batch that
    /// repeats already-read IDs counts only its transitions.
    reads: BTreeMap<u64, usize>,
    /// Per recipient, in FIFO order: its queued and delivering messages. Kept
    /// at the fold, so the scheduler never scans the log's messages.
    open: BTreeMap<AgentId, BTreeSet<MessageId>>,
    /// Recognized final failures lacking their keyed owner note. Rebuilt by the
    /// same fold as open/state, not a separate persisted outbox or key index.
    failed_notices: BTreeMap<AgentId, BTreeSet<MessageId>>,
    /// Startup replay only: for each message, where its last attempt went,
    /// read from the agent's binding as it was at that attempt. Taken when
    /// the replay completes, so a live attempt is never recorded.
    replayed: Option<BTreeMap<MessageId, ReplayedAttempt>>,
}

/// The attempted pane and the session the recipient was bound to then.
#[derive(Debug)]
struct ReplayedAttempt {
    attempt_seq: u64,
    recipient: AgentId,
    pane: String,
    /// None: the agent held another pane or no session at that attempt (a
    /// lazy first prompt). Such an attempt cannot be told from a later
    /// occupant's activity, so nothing may confirm it.
    session: Option<atm_core::SessionId>,
}

impl View {
    /// File a message under its recipient while it can still be scheduled.
    fn index(&mut self, id: MessageId) {
        let Some(message) = self.state.message(&id) else {
            return;
        };
        let Address::Agent(recipient) = &message.content.to else {
            return;
        };
        if matches!(
            message.status,
            DeliveryStatus::Queued | DeliveryStatus::Delivering
        ) {
            self.open.entry(recipient.clone()).or_default().insert(id);
        } else if let Some(open) = self.open.get_mut(recipient) {
            open.remove(&id);
            if open.is_empty() {
                self.open.remove(recipient);
            }
        }
        let missing = (message.status == DeliveryStatus::Failed)
            .then(|| {
                message
                    .failure
                    .as_deref()
                    .and_then(|reason| message_scheduler::failure_notice(id, reason))
            })
            .flatten()
            .and_then(|content| content.delivery_notice)
            .is_some_and(|key| !self.state.has_delivery_notice(&key));
        if missing {
            self.failed_notices
                .entry(recipient.clone())
                .or_default()
                .insert(id);
        } else if let Some(pending) = self.failed_notices.get_mut(recipient) {
            pending.remove(&id);
            if pending.is_empty() {
                self.failed_notices.remove(recipient);
            }
        }
    }
}

impl View {
    /// During the startup replay, remember where a message's last attempt
    /// went. The state is exactly as it was at that attempt, so the binding
    /// read here is the attempt's own, not today's occupant of the pane.
    fn replay_attempt(&mut self, seq: u64, event: &OrgEvent) {
        let Some(replayed) = &mut self.replayed else {
            return;
        };
        match event {
            OrgEvent::MessageDeliveryAttempted {
                message_id, pane, ..
            } => {
                let Some(Address::Agent(recipient)) =
                    (self.state.message(message_id)).map(|message| &message.content.to)
                else {
                    return;
                };
                let session = (self.state.agent(recipient))
                    .filter(|agent| agent.pane.as_ref() == Some(pane))
                    .and_then(|agent| agent.session_id.clone());
                let attempt = ReplayedAttempt {
                    attempt_seq: seq,
                    recipient: recipient.clone(),
                    pane: pane.clone(),
                    session,
                };
                replayed.insert(*message_id, attempt);
            }
            OrgEvent::MessageDelivered { message_id, .. } => {
                replayed.remove(message_id);
            }
            _ => {}
        }
    }

    /// The scope of a replayed attempt, if it is unambiguous: the recipient
    /// was bound to a session on the attempted pane, and that session's
    /// harness is on record. Anything less stays without a scope, so only the
    /// owner can resolve it.
    fn replayed_scope(&self, id: MessageId, attempt: ReplayedAttempt) -> Option<AttemptScope> {
        let session = attempt.session?;
        Some(AttemptScope {
            message_id: id,
            attempt_seq: attempt.attempt_seq,
            recipient: attempt.recipient,
            pane: PaneId::parse(&attempt.pane).ok()?,
            harness: self.state.session_summary(&session)?.harness,
            session_id: Some(session),
            excluded_sessions: Default::default(),
        })
    }
}

/// A retained receipt and the message it confirms, as committed state has it.
pub(super) struct Receipt {
    pub recipient: AgentId,
    pub id: MessageId,
    pub receipt: DeliveryReceipt,
    /// Already Delivered: only evidence still has to forget the message.
    pub delivered: bool,
}

pub(super) struct Service {
    pub(super) rt: MessageRuntime,
    view: Mutex<View>,
    startup: watch::Sender<Startup>,
    requests: Semaphore,
    /// The last sequence whose fold is complete: projection, history and the
    /// evidence registration. The scheduler's wake for committed changes.
    pub(super) folded: watch::Sender<u64>,
    pub(super) board: Board,
}

/// Committed events in sequence order: the organisation's own watch, which
/// replays from the log and fills gaps from disk.
enum Events {
    Watch(Box<EventWatch>),
    /// Test-only: envelopes handed over in memory, with no log to replay.
    #[cfg(test)]
    Memory(tokio::sync::broadcast::Receiver<Arc<Envelope>>),
}

impl Events {
    async fn next(&mut self) -> Result<Option<Arc<Envelope>>, OrgServiceError> {
        match self {
            Self::Watch(watch) => watch.next().await,
            #[cfg(test)]
            Self::Memory(live) => match live.recv().await {
                Ok(envelope) => Ok(Some(envelope)),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => Ok(None),
                Err(error) => Err(OrgServiceError::Unavailable(error.to_string())),
            },
        }
    }
}

impl Service {
    fn view(&self) -> Result<MutexGuard<'_, View>, MessageError> {
        self.view
            .lock()
            .map_err(|_| MessageError::Unavailable("message projection lock poisoned".into()))
    }

    /// Replay complete, or the reason the service cannot be used.
    pub(super) async fn ready(&self) -> Result<(), MessageError> {
        let mut startup = self.startup.subscribe();
        let state = startup
            .wait_for(|state| *state != Startup::Replaying)
            .await
            .map_err(|_| MessageError::Unavailable("message service stopped".into()))?;
        match &*state {
            Startup::Failed(reason) => Err(MessageError::Unavailable(reason.clone())),
            _ => Ok(()),
        }
    }

    /// Replayed and still folding: the only state in which jobs may decide.
    pub(super) fn running(&self) -> bool {
        *self.startup.borrow() == Startup::Ready
    }

    /// Resolves when the service stops or fails.
    pub(super) async fn stopped(&self) {
        let mut startup = self.startup.subscribe();
        let _ = startup
            .wait_for(|state| matches!(state, Startup::Failed(_)))
            .await;
    }

    /// Wait until history and evidence have everything the projection shows.
    /// The fold writes the projection first, so a reader of the projection
    /// could otherwise name a message history has not folded yet.
    pub(super) async fn settled(&self) -> Result<(), MessageError> {
        let cursor = self.view()?.cursor;
        let mut folded = self.folded.subscribe();
        tokio::time::timeout(ORDINARY_BUDGET, folded.wait_for(|seq| *seq >= cursor))
            .await
            .map_err(|_| MessageError::Timeout)?
            .map_err(|_| MessageError::Unavailable("message service stopped".into()))?;
        Ok(())
    }

    /// The recipient's queued and delivering messages, in FIFO order.
    pub(super) fn open(&self, recipient: &AgentId) -> Result<Vec<MessageId>, MessageError> {
        let view = self.view()?;
        Ok((view.open.get(recipient).into_iter().flatten().copied()).collect())
    }

    /// Every recipient with a message that can still be scheduled.
    pub(super) fn recipients(&self) -> Result<Vec<AgentId>, MessageError> {
        let view = self.view()?;
        Ok(view
            .open
            .keys()
            .chain(view.failed_notices.keys())
            .cloned()
            .collect())
    }

    pub(super) fn failed_notices(
        &self,
        recipient: &AgentId,
    ) -> Result<Vec<MessageContent>, MessageError> {
        let view = self.view()?;
        Ok(view
            .failed_notices
            .get(recipient)
            .into_iter()
            .flatten()
            .filter_map(|id| {
                let message = view.state.message(id)?;
                message_scheduler::failure_notice(*id, message.failure.as_deref()?)
            })
            .collect())
    }

    pub(super) fn has_delivery_notice(&self, key: &DeliveryNotice) -> Result<bool, MessageError> {
        Ok(self.view()?.state.has_delivery_notice(key))
    }

    /// The snapshot is queued behind accepted writes; an unhealthy/unknown
    /// writer is never treated as an empty key index. Wait for its exact fold.
    pub(super) async fn reconcile_notice(
        &self,
        key: &DeliveryNotice,
    ) -> Result<bool, MessageError> {
        tokio::time::timeout(ORDINARY_BUDGET, async {
            let (state, cursor) = self.rt.org.snapshot_at().await?;
            self.rt.history.folded(cursor).await?;
            Ok(state.has_delivery_notice(key))
        })
        .await
        .map_err(|_| MessageError::Timeout)?
    }

    pub(super) fn agent(&self, id: &AgentId) -> Result<Option<Agent>, MessageError> {
        Ok(self.view()?.state.agent(id).cloned())
    }

    pub(super) fn message(&self, id: MessageId) -> Result<Option<Message>, MessageError> {
        Ok(self.view()?.state.message(&id).cloned())
    }

    /// Receipts evidence retains, for one recipient or for all. Bounded by
    /// evidence's own capacity, not by the log. An evidence store that cannot
    /// answer has no receipt to settle.
    pub(super) fn receipts(&self, only: Option<&AgentId>) -> Result<Vec<Receipt>, MessageError> {
        let confirmed = self.rt.evidence.confirmed().unwrap_or_default();
        let view = self.view()?;
        Ok(confirmed
            .into_iter()
            .filter_map(|(id, receipt)| {
                let message = view.state.message(&id)?;
                let Address::Agent(recipient) = &message.content.to else {
                    return None;
                };
                only.is_none_or(|only| only == recipient).then(|| Receipt {
                    recipient: recipient.clone(),
                    id,
                    receipt,
                    delivered: message.status == DeliveryStatus::Delivered,
                })
            })
            .collect())
    }

    /// One bounded request: admission without a waiter, then a deadline that
    /// covers the startup wait and all organisation work. A timeout does not
    /// roll back a command the writer already accepted.
    async fn call<T>(
        &self,
        budget: Duration,
        work: impl Future<Output = Result<T, MessageError>>,
    ) -> Result<T, MessageError> {
        let _admitted = self
            .requests
            .try_acquire()
            .map_err(|_| MessageError::Unavailable("message service is busy".into()))?;
        tokio::time::timeout(budget, async {
            self.ready().await?;
            work.await
        })
        .await
        .map_err(|_| MessageError::Timeout)?
    }

    /// Submit to the one organisation writer, then wait until the fold loop
    /// has accounted for what it committed, so the caller reads its own write.
    pub(super) async fn execute(&self, command: OrgCommand) -> Result<Vec<Envelope>, MessageError> {
        let notice = match &command {
            OrgCommand::SendMessage(content) => content.delivery_notice.clone(),
            _ => None,
        };
        let committed = self.rt.org.execute(command, self.rt.clock.now()?).await?;
        if let Some(last) = committed.last() {
            self.rt.history.folded(last.seq).await?;
        } else if let Some(key) = notice {
            if !self.reconcile_notice(&key).await? {
                return Err(MessageError::Internal(
                    "duplicate notice key absent from committed state".into(),
                ));
            }
        }
        Ok(committed)
    }

    /// Apply one committed envelope to the projection, then to history.
    /// Sequences are contiguous: a duplicate is skipped, a gap is an error.
    fn fold(&self, envelope: &Envelope) -> Result<(), MessageError> {
        let failed = |what: &str, error: &dyn std::fmt::Display| {
            MessageError::Internal(format!("{what} at sequence {}: {error}", envelope.seq))
        };
        let mut view = self.view()?;
        if envelope.seq <= view.cursor {
            return Ok(());
        }
        if view.cursor.checked_add(1) != Some(envelope.seq) {
            return Err(failed("organisation event gap", &view.cursor));
        }
        let event = OrgEvent::from_parts(&envelope.kind, &envelope.body)
            .map_err(|error| failed("unreadable organisation event", &error))?;
        if let Some(event) = &event {
            let marked = match event {
                OrgEvent::MessageRead { message_ids } => Some(
                    message_ids
                        .iter()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .filter(|id| view.state.message(id).is_some_and(|m| m.read_at.is_none()))
                        .count(),
                ),
                _ => None,
            };
            view.state
                .apply(envelope.seq, envelope.ts, event)
                .map_err(|error| failed("inapplicable organisation event", &error))?;
            if let Some(marked) = marked {
                view.reads.insert(envelope.seq, marked);
                if view.reads.len() > MAX_READ_COUNTS {
                    view.reads.pop_first();
                }
            }
            match event {
                OrgEvent::MessageSent(content) => {
                    view.index(MessageId::new(envelope.seq));
                    if let Some(key) = &content.delivery_notice {
                        view.index(key.source_message_id);
                    }
                }
                OrgEvent::MessageDeliveryAttempted { message_id, .. }
                | OrgEvent::MessageDelivered { message_id, .. }
                | OrgEvent::MessageDeliveryFailed { message_id, .. } => view.index(*message_id),
                _ => {}
            }
            view.replay_attempt(envelope.seq, event);
        }
        view.cursor = envelope.seq;
        drop(view);
        // The projection is ahead of history's cursor, never behind it: a
        // caller woken by `folded(seq)` finds its write in the projection.
        self.rt.history.record(
            HistoryUpdate::Applied {
                seq: envelope.seq,
                at: envelope.ts,
                event: event.clone(),
            },
            envelope.ts,
        )?;
        if let Some(OrgEvent::MessageSent(content)) = &event {
            if *self.startup.borrow() == Startup::Ready {
                self.expect(MessageId::new(envelope.seq), content);
            }
        }
        if let Some(OrgEvent::MessageDeliveryFailed {
            message_id,
            final_failure: true,
            ..
        }) = &event
        {
            // After history has the failure: a job that finds the reference
            // gone also finds the message failed.
            self.rt.evidence.forget_unscoped(*message_id);
        }
        self.folded.send_replace(envelope.seq);
        Ok(())
    }

    /// Evidence must know a message's reference before any decision about it:
    /// an unknown ID is unavailable there, never "no receipt".
    pub(super) fn expect(&self, id: MessageId, content: &MessageContent) {
        if !matches!(content.to, Address::Agent(_)) {
            return;
        }
        if let Err(error) = self.rt.evidence.register(id, delivery_line(id, content)) {
            tracing::warn!(%id, %error, "Message stays held: evidence cannot expect its reference");
        }
    }

    /// The startup replay reached its fence: open agent messages become
    /// known to evidence, and requests are admitted. An attempt of the
    /// previous process stays held for the owner (history's restart rule),
    /// but where its scope is unambiguous evidence expects it again, so a
    /// receipt that arrives late still confirms it instead of a retry
    /// pasting it twice. A message that failed for good is expected only
    /// for such a scope: without one nothing could confirm it, and its
    /// reference would be held for ever.
    fn replayed(&self) -> Result<(), MessageError> {
        if *self.startup.borrow() != Startup::Replaying {
            return Ok(());
        }
        let mut view = self.view()?;
        let mut replayed = view.replayed.take().unwrap_or_default();
        for message in view.state.messages() {
            let id = message.id;
            let scope = (replayed.remove(&id)).and_then(|attempt| view.replayed_scope(id, attempt));
            let failed = match message.status {
                DeliveryStatus::Queued | DeliveryStatus::Delivering => false,
                DeliveryStatus::Failed if scope.is_some() => true,
                _ => continue,
            };
            self.expect(id, &message.content);
            if let Some(Err(error)) = scope.map(|scope| self.rt.evidence.arm(scope)) {
                tracing::warn!(%id, %error, "Replayed attempt stays unconfirmable: evidence refused its scope");
            }
            if failed {
                // Its scope was refused: nothing is left to wait for.
                self.rt.evidence.forget_unscoped(id);
            }
        }
        drop(view);
        self.startup.send_replace(Startup::Ready);
        Ok(())
    }

    /// Fold until the scheduler has stopped (it stops on cancellation, once
    /// its jobs have let their attempt settle) or the fold itself fails.
    async fn run(&self, mut events: Events, fence: u64, scheduler: impl Future<Output = ()>) {
        tokio::pin!(scheduler);
        let stopped = async {
            if fence == 0 {
                self.replayed()?;
            }
            loop {
                let Some(envelope) = events.next().await? else {
                    return Err(MessageError::Unavailable(
                        "organisation event stream closed".into(),
                    ));
                };
                self.fold(&envelope)?;
                if envelope.seq >= fence {
                    self.replayed()?;
                }
            }
        };
        let failed = tokio::select! {
            biased;
            () = &mut scheduler => None,
            Err::<(), MessageError>(error) = stopped => {
                tracing::error!(%error, "Message service stopped; restart to recover");
                Some(error.to_string())
            }
        };
        let drained = failed.is_none();
        self.startup.send_replace(Startup::Failed(
            failed.unwrap_or_else(|| "message service stopped".into()),
        ));
        if !drained {
            // Jobs see the failure and stop; an attempt in flight is awaited,
            // never dropped.
            scheduler.await;
        }
    }
}

/// Start the message service: one fold loop, and a handle that serves send,
/// inbox and ack. `startup_high_water` is the committed cursor the runtime's
/// history was built with; requests are admitted once the replay reaches it.
/// Build the runtime once per daemon, after the organisation service is open.
#[must_use]
pub fn spawn_messages(
    runtime: MessageRuntime,
    startup_high_water: u64,
) -> (MessageHandle, JoinHandle<()>) {
    let effects = Effects::production(&runtime);
    match runtime.org.watch(Vec::new(), 0) {
        Ok(events) => spawn_service(
            runtime,
            Events::Watch(Box::new(events)),
            startup_high_water,
            Some(effects),
        ),
        Err(error) => (
            MessageHandle::disabled(error.to_string()),
            tokio::spawn(async {}),
        ),
    }
}

/// Without effects nothing is scheduled: send, inbox and ack only (tests).
fn spawn_service(
    runtime: MessageRuntime,
    events: Events,
    startup_high_water: u64,
    effects: Option<Effects>,
) -> (MessageHandle, JoinHandle<()>) {
    let board = Board::with_recovery(runtime.history.clone());
    let service = Arc::new(Service {
        rt: runtime,
        view: Mutex::new(View {
            replayed: Some(BTreeMap::new()),
            ..View::default()
        }),
        startup: watch::Sender::new(Startup::Replaying),
        requests: Semaphore::new(MAX_REQUESTS),
        folded: watch::Sender::new(0),
        board,
    });
    let task = tokio::spawn({
        let service = service.clone();
        async move {
            let scheduler = async {
                match effects {
                    Some(effects) => message_scheduler::run(service.clone(), effects).await,
                    None => service.rt.cancel.cancelled().await,
                }
            };
            service.run(events, startup_high_water, scheduler).await;
        }
    });
    (
        MessageHandle {
            inner: Backend::Live(service),
            reporting: super::reporting_line::ReportingFiles::default(),
        },
        task,
    )
}

#[cfg(test)]
pub(super) fn spawn_service_for_test(
    runtime: MessageRuntime,
    events: tokio::sync::broadcast::Receiver<Arc<Envelope>>,
    startup_high_water: u64,
    effects: Option<Effects>,
) -> (MessageHandle, JoinHandle<()>) {
    spawn_service(runtime, Events::Memory(events), startup_high_water, effects)
}

#[derive(Clone)]
enum Backend {
    Disabled(Arc<str>),
    Live(Arc<Service>),
}

#[derive(Clone)]
pub struct MessageHandle {
    inner: Backend,
    reporting: super::reporting_line::ReportingFiles,
}

impl std::fmt::Debug for MessageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            Backend::Disabled(reason) => f.debug_tuple("Disabled").field(reason).finish(),
            Backend::Live(_) => f.write_str("Live"),
        }
    }
}

/// A discovery row cannot confirm a Ready agent's recorded real session.
/// Absence of all rows and lazy, unbound starts retain ordinary queue policy.
pub(super) fn unconfirmed_discovery(agent: &Agent, rows: &[SessionView]) -> bool {
    let Some(recorded) = agent.session_id.as_ref().filter(|id| !id.is_pending()) else {
        return false;
    };
    let in_pane = || rows.iter().filter(|row| matches_agent(agent, row));
    agent.status == AgentStatus::Ready
        && in_pane().any(|row| row.id.is_pending())
        && !in_pane().any(|row| &row.id == recorded)
}

fn reporting_error(error: MethodError) -> MessageError {
    match error.0.code.as_str() {
        atm_protocol::error_code::REFUSED => MessageError::Refused(error.0.message),
        atm_protocol::error_code::NOT_FOUND => MessageError::NotFound(error.0.message),
        atm_protocol::error_code::TIMEOUT => MessageError::Timeout,
        atm_protocol::error_code::UNAVAILABLE => MessageError::Unavailable(error.0.message),
        _ => MessageError::Internal(error.0.message),
    }
}

impl MessageHandle {
    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            inner: Backend::Disabled(Arc::from(reason.into())),
            reporting: super::reporting_line::ReportingFiles::default(),
        }
    }

    pub(crate) fn with_reporting_files(
        mut self,
        files: super::reporting_line::ReportingFiles,
    ) -> Self {
        self.reporting = files;
        self
    }

    /// The live service, for tests that drive one scheduling step by hand.
    #[cfg(test)]
    pub(super) fn service(&self) -> Option<Arc<Service>> {
        match &self.inner {
            Backend::Disabled(_) => None,
            Backend::Live(service) => Some(service.clone()),
        }
    }

    fn live(&self) -> Result<&Service, MessageError> {
        match &self.inner {
            Backend::Disabled(reason) => Err(MessageError::Unavailable(reason.to_string())),
            Backend::Live(service) => Ok(service),
        }
    }

    /// Durable enqueue, not delivery. Caller identity is attribution, not authentication.
    /// Answers once the message is committed and folded. The sender is the
    /// agent bound to the caller's pane, else the owner.
    pub async fn send(
        &self,
        p: SendParams,
        caller: CallContext,
    ) -> Result<SendResult, MessageError> {
        let service = self.live()?;
        service
            .call(ORDINARY_BUDGET, async {
                if p.to == Address::System {
                    return Err(MessageError::Refused(
                        "system is not a message recipient or inbox".into(),
                    ));
                }
                let from = caller_address(&service.view()?.state, &caller)?;
                if let Address::Agent(id) = &p.to {
                    // Admission must include lifecycle changes committed since startup.
                    service.rt.history.folded(service.rt.org.status()?).await?;
                    if let Some(agent) = service.agent(id)?.filter(|agent| {
                        agent.status == AgentStatus::Ready
                            && agent.session_id.as_ref().is_some_and(|id| !id.is_pending())
                    }) {
                        if let Some(pane) = agent.pane.as_deref() {
                            let rows: Vec<_> = tokio::time::timeout(
                                SEND_IDENTITY_BUDGET,
                                service.rt.registry.pane_sessions(pane),
                            )
                            .await
                            .map_err(|_| MessageError::Timeout)?
                            .map_err(|error| MessageError::Unavailable(format!("session registry: {error}")))?
                            .into_iter()
                            .map(|row| row.view)
                            .collect();
                            if unconfirmed_discovery(&agent, &rows) {
                                return Err(MessageError::Refused(format!(
                                    "agent {id} has a discovered process but its recorded session is not confirmed; message not queued; inspect atm list and atm agent list before retrying"
                                )));
                            }
                        }
                    }
                }
                let content = MessageContent {
                    delivery_notice: None,
                    from,
                    to: p.to,
                    kind: p.kind,
                    body: p.body,
                    attachments: p.attachments,
                    task: p.task,
                };
                let committed = service.execute(OrgCommand::SendMessage(content)).await?;
                let sent = committed.first().ok_or_else(|| {
                    MessageError::Internal("message send committed no event".into())
                })?;
                Ok(SendResult {
                    message_id: MessageId::new(sent.seq),
                })
            })
            .await
    }

    /// Read-only inbox projection; fetching does not acknowledge reading.
    pub async fn inbox(
        &self,
        p: InboxParams,
        caller: CallContext,
    ) -> Result<InboxResult, MessageError> {
        let service = self.live()?;
        service
            .call(ORDINARY_BUDGET, async {
                let now = service.rt.clock.now()?;
                // History owns FIFO progress; committed recipient facts come
                // from the same snapshot that supplies reporting admission.
                let progress = |message: &Message| Progress {
                    blocked_by: (service.rt.history.snapshot(message.id, now).ok())
                        .and_then(|snapshot| snapshot.blocked_by),
                    ..service.board.progress(message.id)
                };
                let (state, caller, recipient) = {
                    let view = service.view()?;
                    let who = caller_address(&view.state, &caller)?;
                    let report = match (&who, &p.recipient) {
                        (Address::Agent(caller), Some(Address::Agent(report)))
                            if caller != report && view.state.agent(report).is_some() =>
                        {
                            Some(report.clone())
                        }
                        _ => None,
                    };
                    let Some(report) = report else {
                        return inbox(&view.state, &who, &p, None, progress);
                    };
                    (view.state.clone(), who, report)
                };
                // No message-view guard survives this filesystem await.
                let ancestors = self
                    .reporting
                    .ancestors(state.clone(), recipient.clone())
                    .await
                    .map_err(reporting_error)?;
                let grant = match &caller {
                    Address::Agent(id) if ancestors.contains(id) => Some(&recipient),
                    _ => None,
                };
                inbox(&state, &caller, &p, grant, progress)
            })
            .await
    }

    /// Acknowledge a fully validated batch of delivered messages in the caller's inbox.
    /// The count is the number of messages this call marked read, taken at the
    /// ordered fold of its committed event; already-read IDs add nothing.
    pub async fn ack(&self, p: AckParams, caller: CallContext) -> Result<AckResult, MessageError> {
        let service = self.live()?;
        service
            .call(ORDINARY_BUDGET, async {
                let who = caller_address(&service.view()?.state, &caller)?;
                let committed = service
                    .execute(OrgCommand::ReadMessages {
                        who,
                        message_ids: p.ids,
                    })
                    .await?;
                let view = service.view()?;
                let mut count = 0;
                for envelope in &committed {
                    count += view.reads.get(&envelope.seq).ok_or_else(|| {
                        MessageError::Internal("acknowledgement fold was not retained".into())
                    })?;
                }
                Ok(AckResult { count })
            })
            .await
    }

    /// Owner release of a latched hold on this same ID, accepting duplicate risk.
    /// Recoverable waits are not retryable; attempt limits remain in force.
    /// Resolution uses the recipient slot, never an Observe reservation.
    pub async fn retry(
        &self,
        p: ResolveParams,
        caller: CallContext,
    ) -> Result<RetryResult, MessageError> {
        let (status, _) = self.resolve(Resolve::Retry, p.id, caller).await?;
        Ok(RetryResult {
            message_id: p.id,
            status,
        })
    }

    /// Owner cancellation of any held Queued/Delivering message, not only Blocked.
    /// An active attempt must settle first; a later receipt may still confirm it.
    /// Resolution uses the recipient slot, never an Observe reservation.
    pub async fn give_up(
        &self,
        p: ResolveParams,
        caller: CallContext,
    ) -> Result<GiveUpResult, MessageError> {
        let (status, line) = self.resolve(Resolve::GiveUp, p.id, caller).await?;
        Ok(GiveUpResult {
            message_id: p.id,
            status,
            editor_warning: Some(format!(
                "Possible leftover in the recipient's editor: {line}. Giving up did not clear the editor; \
                 further delivery may wait for an empty editor. Inspect the pane before editing it."
            )),
        })
    }

    /// Queue the owner's decision for the recipient's slot and wait for its
    /// answer within the resolution budget. The slot serves it only between
    /// attempts, so an attempt in flight settles first. A timeout cancels
    /// nothing that was started; a decision nobody waits for any more is not
    /// started.
    async fn resolve(
        &self,
        kind: Resolve,
        id: MessageId,
        caller: CallContext,
    ) -> Result<(DeliveryStatus, String), MessageError> {
        let service = self.live()?;
        service
            .call(MESSAGE_RESOLUTION_BUDGET, async {
                let (reply, answer) = oneshot::channel();
                let line = {
                    let view = service.view()?;
                    if caller_address(&view.state, &caller)? != Address::Owner {
                        return Err(MessageError::Refused(
                            "only the owner may retry or give up a delivery".into(),
                        ));
                    }
                    let message = view.state.message(&id).ok_or_else(|| {
                        MessageError::NotFound(format!("message {id} was not found"))
                    })?;
                    let Address::Agent(recipient) = &message.content.to else {
                        // Never scheduled: delivered to the owner at its fold.
                        return Err(kind.refuse_delivered(id));
                    };
                    let line = delivery_line(id, &message.content);
                    let resolution = Resolution { kind, id, reply };
                    service.board.resolve(recipient.clone(), resolution)?;
                    line
                };
                let status = answer
                    .await
                    .map_err(|_| MessageError::Unavailable("message service stopped".into()))??;
                Ok((status, line))
            })
            .await
    }

    /// The startup replay is complete, or the reason the service cannot be used.
    pub async fn initialized(&self) -> Result<(), MessageError> {
        self.live()?.ready().await
    }
}
