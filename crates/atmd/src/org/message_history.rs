//! A1's pure live-view history contract, serialized by A2's handle boundary.
//!
//! Pure history folds only applied envelopes, never decides/writes org commands.
//! The handle serializes updates and exposes a fold cursor; A2 owns ingestion.
//! Capture the startup high-water mark before enabling jobs; an attempt sequence
//! at or below that mark is Restarted regardless of wall-clock order. Supplied
//! recovery time starts missing observation continuity, not message sent_at.
//!
//! `snapshot` is read-only. Evaluate the real `next_delivery`, then explicitly
//! record consumption; a failed decision/update must not advance any clock.
//! A2's all-topic (`[]`) watch is the sole Applied writer. Sequences must be
//! contiguous, including unknown kinds: gaps error without advancing the cursor;
//! A2 recovers from that cursor. Duplicate sequences do not refold. B awaits
//! `folded` after execute, bounded by its work deadline, before snapshot/arm or
//! evidence acknowledgment. Synchronous handle calls take a short lock, not an actor.
//!
//! # Update writers
//!
//! | Update | Writer |
//! |---|---|
//! | Applied | A2 all-topic watch only |
//! | Observed / ObservationLost | A2 poller or B while reserved; shared converter |
//! | Activity | Accepted-event tap; evidence clock stamps receive time |
//! | Armed | B immediately after evidence.arm, using the same scope |
//! | Evidence | Recipient-slot holder immediately before each policy evaluation: A2 outside an attempt, B inside |
//! | Consumed | Recipient-slot holder that evaluated the step |
//! | Paste / Held | B |
//! | OwnerRetry | A2 |
//!
//! B arms evidence first, then history, before any input. If either fails, no
//! input is allowed; settle the committed attempt with definite no-effect evidence
//! or retain an unknown-outcome hold. Never roll back a successful evidence arm.

use super::{message_evidence::AttemptScope, messages::MessageError};
use crate::control::reservation::PaneId;
use atm_core::{
    DialogState, Harness, InputState, ScreenFacts, SessionId, SessionStatus, TurnState,
};
use atm_org::{
    delivery::{
        delivery_in_flight, next_delivery, track_agent_not_ready, track_hold, track_stale_blocker,
        DeliveryPolicy, DeliveryStep, InputLine, LiveSession, PaneLiveness, RecipientLiveView,
        WaitReason,
    },
    Address, Agent, AgentId, AgentStatus, DeliveryReceipt, DeliveryStatus, Message, MessageId,
    OrgEvent, OrgState,
};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// Owned applied state and live inputs; no lock/actor guard escapes the handle.
/// Owner-addressed messages are terminal and never need a delivery snapshot.
#[derive(Debug, Clone)]
pub struct DeliverySnapshot {
    pub message: Message,
    pub recipient: Agent,
    pub live: RecipientLiveView,
    /// Org log sequence of the applied DeliveryAttempted, not the attempt number.
    pub last_attempt_seq: Option<u64>,
    /// Executor diagnostic; latches probably_landed until new arm or matching retry.
    pub hold: Option<ExecutorHold>,
    /// Current FIFO predecessor, not a list of followers.
    pub blocked_by: Option<MessageId>,
}

/// Unknown is potentially applied, never permission to retry the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCertainty {
    NotStarted,
    Applied,
    Unknown,
}

/// Executor-only reasons for retaining a potentially applied attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorHold {
    StagedMismatch,
    InputOutcomeUnknown,
    TokenVisible,
}

/// Raw inspection fact, before A1 attaches continuous GoneSince history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePresence {
    Alive,
    Gone,
    Unknown,
}

/// Unique eligible parent row, resolved by A2/U8 before history observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionObservation {
    pub id: SessionId,
    pub harness: Harness,
    pub status: SessionStatus,
    /// Initial genuine-event provenance for LiveSession.last_event_at. Missing
    /// provenance cannot grant stale readiness. Activity wins after receipt;
    /// repeated polls never overwrite the activity clock, even with newer UTC.
    pub last_event_at: Option<DateTime<Utc>>,
}

/// Fresh neutral facts, not parser input or a permission to send keys.
/// Identity change/loss breaks empty/status continuity; unknown focus blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientObservation {
    pub pane: Option<PaneId>,
    pub session: Option<SessionObservation>,
    pub presence: PanePresence,
    pub facts: ScreenFacts,
    pub copy_mode: Option<bool>,
    pub focused: Option<bool>,
}

/// Pure inputs in receive/apply order. IDs live on updates, not `record`, because
/// one applied envelope can affect an agent or several messages at once.
#[derive(Debug, Clone)]
pub enum HistoryUpdate {
    /// Committed fact only; None advances the cursor for a skipped unknown kind.
    Applied {
        seq: u64,
        at: DateTime<Utc>,
        event: Option<OrgEvent>,
    },
    Observed {
        recipient: AgentId,
        observation: RecipientObservation,
    },
    /// Loss breaks observation continuity but is never positive pane death.
    ObservationLost { recipient: AgentId },
    /// Genuine event receive order; polls/discovery refreshes must not emit this.
    Activity {
        /// Stamped by evidence's clock at receipt, never at a later wake/drain.
        at: DateTime<Utc>,
        session_id: SessionId,
        pane: Option<PaneId>,
    },
    /// After this attempted sequence is applied and before input dispatch.
    Armed { scope: AttemptScope },
    /// Retained facts; absence/false is not permission to clear a prior latch.
    /// Only a new arm or explicit matching owner retry can release that latch.
    Evidence {
        id: MessageId,
        receipt: Option<DeliveryReceipt>,
        probably_landed: bool,
    },
    /// Successful serialized policy consumption; PasteNow resets hold, not paste time.
    Consumed { id: MessageId, step: DeliveryStep },
    /// Applied/Unknown records now and resets pane readiness before job release.
    /// NotStarted must never manufacture a paste timestamp. Paste never latches
    /// probably_landed by itself: B records Held for uncertain effects.
    Paste {
        id: MessageId,
        pane: PaneId,
        certainty: EffectCertainty,
    },
    /// Latch probably_landed and this diagnostic for the matching attempted log
    /// sequence. Only new Armed or matching OwnerRetry releases it.
    Held {
        id: MessageId,
        attempt_seq: u64,
        hold: ExecutorHold,
    },
    /// Release only this matching attempt's transient hold, never its durable
    /// attempt count or late-receipt scopes. A stale attempt sequence refuses.
    OwnerRetry { id: MessageId, attempt_seq: u64 },
}

#[derive(Debug, Clone)]
struct RecipientHistory {
    observation: Option<RecipientObservation>,
    status_since: Option<DateTime<Utc>>,
    last_event_at: Option<DateTime<Utc>>,
    pane: PaneLiveness,
    input: InputLine,
    not_ready_since: Option<DateTime<Utc>>,
}

impl Default for RecipientHistory {
    fn default() -> Self {
        Self {
            observation: None,
            status_since: None,
            last_event_at: None,
            pane: PaneLiveness::Unknown,
            input: InputLine::Unknown,
            not_ready_since: None,
        }
    }
}

impl RecipientHistory {
    fn lose(&mut self) {
        self.observation = None;
        self.status_since = None;
        self.last_event_at = None;
        self.pane = PaneLiveness::Unknown;
        self.input = InputLine::Unknown;
    }
}

#[derive(Debug, Clone, Default)]
struct MessageContinuity {
    last_attempt_seq: Option<u64>,
    restarted: bool,
    scope: Option<AttemptScope>,
    receipt: Option<DeliveryReceipt>,
    probably_landed: bool,
    hold: Option<ExecutorHold>,
    hold_since: Option<DateTime<Utc>>,
    stale_since: Option<DateTime<Utc>>,
    stale_blocker: Option<WaitReason>,
}

impl MessageContinuity {
    /// This message's own clocks and evidence, on top of its recipient's view.
    fn apply(&self, live: &mut RecipientLiveView) {
        live.hold_since = self.hold_since;
        live.stale_blocker_since = self.stale_since;
        live.last_attempt_before_start = self.restarted;
        live.confirmation = self.receipt.clone();
        live.probably_landed = self.probably_landed;
    }
}

/// A recipient's open messages as any one of them sees the others.
#[derive(Debug, Clone, Copy, Default)]
struct Queue {
    /// The first two attempted, unconfirmed messages: enough to find one that
    /// is not the message asking.
    in_flight: [Option<MessageId>; 2],
    head: Option<MessageId>,
}

impl Queue {
    /// The policy's reservation veto.
    fn other_in_flight(&self, id: MessageId) -> Option<MessageId> {
        self.in_flight
            .iter()
            .flatten()
            .copied()
            .find(|other| *other != id)
    }

    /// The veto first, else the FIFO head while it is older than `id`.
    fn blocked_by(&self, id: MessageId) -> Option<MessageId> {
        self.other_in_flight(id)
            .or(self.head.filter(|head| *head < id))
    }
}

/// New stale-blocker clocks, computed before any of them is written.
type ShortClocks = Vec<(MessageId, Option<DateTime<Utc>>, Option<WaitReason>)>;

/// The one agent an applied event can change. Exhaustive on purpose: a new
/// event kind must say here whether it names an agent.
fn named_agent(event: &OrgEvent) -> Option<&AgentId> {
    match event {
        OrgEvent::AgentUpRequested { agent_id, .. }
        | OrgEvent::AgentPaneCreated { agent_id, .. }
        | OrgEvent::AgentAdopted { agent_id, .. }
        | OrgEvent::AgentSessionBound { agent_id, .. }
        | OrgEvent::AgentSessionLost { agent_id }
        | OrgEvent::AgentEditorReady { agent_id }
        | OrgEvent::AgentClearRequested { agent_id }
        | OrgEvent::AgentStuck { agent_id, .. }
        | OrgEvent::AgentDown { agent_id, .. } => Some(agent_id),
        OrgEvent::ProjectRegistered { .. }
        | OrgEvent::MessageSent(_)
        | OrgEvent::MessageDeliveryAttempted { .. }
        | OrgEvent::MessageDelivered { .. }
        | OrgEvent::MessageDeliveryFailed { .. }
        | OrgEvent::MessageRead { .. }
        | OrgEvent::SessionSummary(_)
        | OrgEvent::ControlSent(_)
        | OrgEvent::ControlSpawned(_)
        | OrgEvent::ControlKilled(_) => None,
    }
}

/// The applied facts of an agent that its live continuity depends on.
type AgentFacts = (
    AgentStatus,
    Option<String>,
    Option<SessionId>,
    Option<SessionId>,
);

fn agent_facts(agent: &Agent) -> AgentFacts {
    (
        agent.status,
        agent.pane.clone(),
        agent.session_id.clone(),
        agent.excluded_session_id.clone(),
    )
}

/// Pure projection, not an org command writer. Every clock and replay boundary
/// is supplied. Updates are transactional: invalid input/arithmetic cannot leave
/// partially updated continuity or advance the applied cursor.
///
/// An update costs what it touches, not what the log holds. `Applied` is
/// validated and then folded in place. Every other update computes what can
/// fail first, on a staged copy of the one recipient entry it changes, and
/// writes afterwards. Clocks are refreshed only for a recipient's open
/// messages (queued or delivering), never for delivered or failed ones.
#[derive(Debug, Clone)]
pub struct MessageHistory {
    state: Arc<OrgState>,
    cursor: u64,
    startup_high_water: u64,
    recovery_time: DateTime<Utc>,
    policy: DeliveryPolicy,
    recipients: BTreeMap<AgentId, RecipientHistory>,
    /// Per recipient, in FIFO order: its messages that can still be scheduled.
    open: BTreeMap<AgentId, BTreeSet<MessageId>>,
    messages: BTreeMap<MessageId, MessageContinuity>,
    activity: HashMap<SessionId, (Option<PaneId>, DateTime<Utc>)>,
    pastes: HashMap<PaneId, DateTime<Utc>>,
}

impl MessageHistory {
    #[must_use]
    pub fn new(
        startup_high_water: u64,
        recovery_time: DateTime<Utc>,
        policy: DeliveryPolicy,
    ) -> Self {
        Self {
            state: Arc::new(OrgState::default()),
            cursor: 0,
            startup_high_water,
            recovery_time,
            policy,
            recipients: BTreeMap::new(),
            open: BTreeMap::new(),
            messages: BTreeMap::new(),
            activity: HashMap::new(),
            pastes: HashMap::new(),
        }
    }

    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    fn replayed(&self) -> Result<(), MessageError> {
        if self.cursor < self.startup_high_water {
            return Err(MessageError::Unavailable(
                "message history replay is incomplete".into(),
            ));
        }
        Ok(())
    }

    fn queue(&self, recipient: &AgentId) -> Queue {
        let Some(open) = self.open.get(recipient) else {
            return Queue::default();
        };
        let mut in_flight = open.iter().copied().filter(|id| {
            self.state
                .message(id)
                .is_some_and(|message| delivery_in_flight(message, recipient))
        });
        Queue {
            in_flight: [in_flight.next(), in_flight.next()],
            head: open.first().copied(),
        }
    }

    /// The live view all of a recipient's messages share at `now`; the veto
    /// and each message's own continuity are added by the caller. `history`
    /// may be a staged copy.
    fn shared_live(
        &self,
        message: &Message,
        agent: &Agent,
        history: Option<&RecipientHistory>,
        now: DateTime<Utc>,
    ) -> RecipientLiveView {
        let mut live = RecipientLiveView::new(message, agent, std::iter::empty());
        if let Some(history) = history {
            live.agent_not_ready_since = history.not_ready_since;
            live.pane = history.pane;
            live.input_line = history.input;
            if let Some(obs) = &history.observation {
                live.copy_mode = obs.copy_mode;
                live.owner_focused = obs.focused.unwrap_or(true);
                live.dialog_open = match obs.facts.dialog {
                    DialogState::None => Some(false),
                    DialogState::Permission
                    | DialogState::FolderTrust { .. }
                    | DialogState::Other => Some(true),
                    _ => None,
                };
                live.session = obs.session.as_ref().map(|row| LiveSession {
                    status: row.status,
                    status_since: history.status_since.unwrap_or(now),
                    // No provenance must remain busy, not become stale from polls.
                    last_event_at: history.last_event_at.unwrap_or(now),
                });
            }
        }
        live.last_paste_at = agent
            .pane
            .as_deref()
            .and_then(|pane| PaneId::parse(pane).ok())
            .and_then(|pane| self.pastes.get(&pane).copied())
            .or(Some(self.recovery_time));
        live
    }

    pub fn snapshot(
        &self,
        id: MessageId,
        now: DateTime<Utc>,
    ) -> Result<DeliverySnapshot, MessageError> {
        self.replayed()?;
        let message = self
            .state
            .message(&id)
            .ok_or_else(|| MessageError::NotFound(format!("message {id} was not found")))?;
        let Address::Agent(recipient) = &message.content.to else {
            return Err(MessageError::Refused(
                "only agent messages have delivery snapshots".into(),
            ));
        };
        let agent = self
            .state
            .agent(recipient)
            .ok_or_else(|| MessageError::NotFound(format!("agent {recipient} was not found")))?;
        let track = self
            .messages
            .get(&id)
            .ok_or_else(|| MessageError::Internal("message continuity missing".into()))?;
        let queue = self.queue(recipient);
        let mut live = self.shared_live(message, agent, self.recipients.get(recipient), now);
        live.other_delivery_in_flight = queue.other_in_flight(id).is_some();
        track.apply(&mut live);
        Ok(DeliverySnapshot {
            message: message.clone(),
            recipient: agent.clone(),
            live,
            last_attempt_seq: track.last_attempt_seq,
            blocked_by: queue.blocked_by(id),
            hold: track.hold,
        })
    }

    /// All-or-nothing without copying the history: see the type's docs.
    pub fn record(
        &mut self,
        update: HistoryUpdate,
        now: DateTime<Utc>,
    ) -> Result<(), MessageError> {
        self.update(update, now)
    }

    fn track(&mut self, id: MessageId) -> Result<&mut MessageContinuity, MessageError> {
        self.messages
            .get_mut(&id)
            .ok_or_else(|| MessageError::NotFound(format!("message {id} was not found")))
    }

    fn matching_attempt(
        &mut self,
        id: MessageId,
        seq: u64,
    ) -> Result<&mut MessageContinuity, MessageError> {
        let track = self.track(id)?;
        if track.last_attempt_seq != Some(seq) {
            return Err(MessageError::Conflict(
                "message attempt sequence does not match".into(),
            ));
        }
        Ok(track)
    }

    /// Restart the stale-blocker clocks of the recipient's open messages, and
    /// their holds too when `hold` is set.
    fn reset_clocks(&mut self, recipient: &AgentId, hold: bool) {
        for id in self.open.get(recipient).into_iter().flatten() {
            if let Some(track) = self.messages.get_mut(id) {
                if hold {
                    track.hold_since = None;
                }
                track.stale_since = None;
                track.stale_blocker = None;
            }
        }
    }

    /// The stale-blocker clocks of the recipient's open messages under
    /// `history`, which may be a staged copy. `reset` restarts them first.
    /// Nothing is written, so a failure changes no clock, and a recipient with
    /// no open message evaluates nothing.
    fn short_clocks(
        &self,
        recipient: &AgentId,
        history: &RecipientHistory,
        reset: bool,
        now: DateTime<Utc>,
    ) -> Result<ShortClocks, MessageError> {
        let Some(open) = self.open.get(recipient).filter(|open| !open.is_empty()) else {
            return Ok(vec![]);
        };
        self.replayed()?;
        let agent = self
            .state
            .agent(recipient)
            .ok_or_else(|| MessageError::NotFound(format!("agent {recipient} was not found")))?;
        let queue = self.queue(recipient);
        let mut shared = None;
        let mut clocks = Vec::with_capacity(open.len());
        for id in open {
            let (Some(message), Some(track)) = (self.state.message(id), self.messages.get(id))
            else {
                continue;
            };
            let mut live: RecipientLiveView = shared
                .get_or_insert_with(|| self.shared_live(message, agent, Some(history), now))
                .clone();
            live.other_delivery_in_flight = queue.other_in_flight(*id).is_some();
            track.apply(&mut live);
            let mut previous = track.stale_blocker;
            if reset {
                live.stale_blocker_since = None;
                previous = None;
            }
            track_stale_blocker(&mut live, &mut previous, now, &self.policy)
                .map_err(|error| MessageError::Internal(error.to_string()))?;
            clocks.push((*id, live.stale_blocker_since, previous));
        }
        Ok(clocks)
    }

    fn set_short_clocks(&mut self, clocks: ShortClocks) {
        for (id, since, blocker) in clocks {
            if let Some(track) = self.messages.get_mut(&id) {
                track.stale_since = since;
                track.stale_blocker = blocker;
            }
        }
    }

    /// Fold one applied event. `OrgState::apply` validates before it changes
    /// anything and this history holds the only reference to the state, so
    /// nothing is copied; nothing after the apply can fail.
    fn fold(&mut self, seq: u64, at: DateTime<Utc>, event: &OrgEvent) -> Result<(), MessageError> {
        let named = named_agent(event);
        let before = named.and_then(|id| self.state.agent(id)).map(agent_facts);
        Arc::make_mut(&mut self.state)
            .apply(seq, at, event)
            .map_err(|error| MessageError::Internal(error.to_string()))?;
        if let Some((id, after)) =
            named.and_then(|id| Some((id, agent_facts(self.state.agent(id)?))))
        {
            let history = self.recipients.entry(id.clone()).or_default();
            track_agent_not_ready(
                &mut history.not_ready_since,
                before.as_ref().map(|old| old.0),
                after.0,
                at,
            );
            let moved = before
                .as_ref()
                .is_none_or(|old| (&old.1, &old.2, &old.3) != (&after.1, &after.2, &after.3));
            if moved {
                history.lose();
            }
            let status_changed = before.as_ref().is_some_and(|old| old.0 != after.0)
                && (after.0 != AgentStatus::Ready
                    || before
                        .as_ref()
                        .is_some_and(|old| old.0 != AgentStatus::Ready));
            if moved || status_changed {
                self.reset_clocks(id, status_changed);
            }
        }
        let id = match event {
            OrgEvent::MessageSent(_) => MessageId::new(seq),
            OrgEvent::MessageDeliveryAttempted { message_id, .. }
            | OrgEvent::MessageDelivered { message_id, .. }
            | OrgEvent::MessageDeliveryFailed { message_id, .. } => *message_id,
            _ => return Ok(()),
        };
        let Some(message) = self.state.message(&id) else {
            return Ok(());
        };
        if let Address::Agent(recipient) = &message.content.to {
            let open = self.open.entry(recipient.clone()).or_default();
            if matches!(
                message.status,
                DeliveryStatus::Queued | DeliveryStatus::Delivering
            ) {
                open.insert(id);
            } else {
                open.remove(&id);
            }
        }
        let replayed = seq <= self.startup_high_water;
        match event {
            OrgEvent::MessageSent(_) => {
                self.messages.insert(
                    id,
                    MessageContinuity {
                        hold_since: replayed.then_some(self.recovery_time),
                        ..MessageContinuity::default()
                    },
                );
            }
            OrgEvent::MessageDeliveryAttempted { .. } => {
                let track = self.messages.entry(id).or_default();
                track.last_attempt_seq = Some(seq);
                track.restarted = replayed;
            }
            _ => {}
        }
        Ok(())
    }

    fn update(&mut self, update: HistoryUpdate, now: DateTime<Utc>) -> Result<(), MessageError> {
        match update {
            HistoryUpdate::Applied { seq, at, event } => {
                if seq <= self.cursor {
                    return Ok(());
                }
                if self.cursor.checked_add(1) != Some(seq) {
                    return Err(MessageError::Conflict(
                        "message history sequence gap".into(),
                    ));
                }
                if let Some(event) = event {
                    self.fold(seq, at, &event)?;
                }
                self.cursor = seq;
            }
            HistoryUpdate::Observed {
                recipient,
                observation,
            } => {
                let agent = self.state.agent(&recipient).ok_or_else(|| {
                    MessageError::NotFound(format!("agent {recipient} was not found"))
                })?;
                if observation
                    .pane
                    .as_ref()
                    .is_some_and(|pane| agent.pane.as_deref() != Some(pane.as_str()))
                    || (observation.presence != PanePresence::Unknown && observation.pane.is_none())
                    || observation.session.as_ref().is_some_and(|row| {
                        agent.excluded_session_id.as_ref() == Some(&row.id)
                            || agent.session_id.as_ref().is_some_and(|id| id != &row.id)
                    })
                {
                    return Err(MessageError::Conflict(
                        "observation does not match recipient identity".into(),
                    ));
                }
                // Staged: the stored entry changes only after the clocks are known.
                let mut history = self.recipients.get(&recipient).cloned().unwrap_or_default();
                let same_source = history.observation.as_ref().is_some_and(|old| {
                    old.pane == observation.pane
                        && old.session.as_ref().map(|s| (&s.id, s.harness))
                            == observation.session.as_ref().map(|s| (&s.id, s.harness))
                });
                let same_status = same_source
                    && history.status_since.is_some()
                    && history.pane == PaneLiveness::Alive
                    && observation.presence == PanePresence::Alive
                    && history
                        .observation
                        .as_ref()
                        .and_then(|o| o.session.as_ref().map(|s| s.status))
                        == observation.session.as_ref().map(|s| s.status);
                if !same_source {
                    history.input = InputLine::Unknown;
                    history.pane = PaneLiveness::Unknown;
                    history.last_event_at = observation.session.as_ref().and_then(|row| {
                        self.activity
                            .get(&row.id)
                            .filter(|(pane, _)| pane == &observation.pane)
                            .map(|(_, at)| *at)
                            .or(row.last_event_at)
                    });
                }
                if !same_status {
                    history.status_since = observation
                        .session
                        .as_ref()
                        .filter(|_| observation.presence == PanePresence::Alive)
                        .map(|_| now);
                }
                history.pane = match observation.presence {
                    PanePresence::Alive => PaneLiveness::Alive,
                    PanePresence::Gone => match history.pane {
                        PaneLiveness::GoneSince(at) => PaneLiveness::GoneSince(at),
                        _ => PaneLiveness::GoneSince(now),
                    },
                    PanePresence::Unknown => PaneLiveness::Unknown,
                };
                history.input = if observation.presence != PanePresence::Alive {
                    InputLine::Unknown
                } else {
                    match (
                        observation.facts.input,
                        observation.facts.turn,
                        &observation.facts.dialog,
                    ) {
                        (InputState::Draft, _, _) => InputLine::NonEmpty,
                        (InputState::Empty, TurnState::NotRunning, DialogState::None) => {
                            match history.input {
                                InputLine::EmptySince(at) => InputLine::EmptySince(at),
                                _ => InputLine::EmptySince(now),
                            }
                        }
                        _ => InputLine::Unknown,
                    }
                };
                history.observation = Some(observation);
                let clocks = self.short_clocks(&recipient, &history, !same_status, now)?;
                self.recipients.insert(recipient, history);
                self.set_short_clocks(clocks);
            }
            HistoryUpdate::ObservationLost { recipient } => {
                let history = self.recipients.get_mut(&recipient).ok_or_else(|| {
                    MessageError::NotFound(format!("agent {recipient} was not found"))
                })?;
                history.lose();
                self.reset_clocks(&recipient, false);
            }
            HistoryUpdate::Activity {
                session_id,
                pane,
                at,
            } => {
                // Staged per matching recipient: all clocks are known before
                // any entry is written.
                let mut staged = vec![];
                for (recipient, history) in &self.recipients {
                    if history.observation.as_ref().is_some_and(|obs| {
                        obs.pane == pane
                            && obs.session.as_ref().is_some_and(|row| row.id == session_id)
                    }) {
                        let mut history = history.clone();
                        history.last_event_at = Some(at);
                        let clocks = self.short_clocks(recipient, &history, false, now)?;
                        staged.push((recipient.clone(), history, clocks));
                    }
                }
                for (recipient, history, clocks) in staged {
                    self.recipients.insert(recipient, history);
                    self.set_short_clocks(clocks);
                }
                self.activity.insert(session_id, (pane, at));
            }
            HistoryUpdate::Armed { scope } => {
                let message = self.state.message(&scope.message_id).ok_or_else(|| {
                    MessageError::NotFound(format!("message {} was not found", scope.message_id))
                })?;
                if message.content.to != Address::Agent(scope.recipient.clone())
                    || message
                        .last_attempt
                        .as_ref()
                        .is_none_or(|attempt| attempt.pane != scope.pane.as_str())
                    || scope
                        .session_id
                        .as_ref()
                        .is_some_and(|id| scope.excluded_sessions.contains(id))
                {
                    return Err(MessageError::Conflict(
                        "attempt scope does not match applied attempt".into(),
                    ));
                }
                let track = self.matching_attempt(scope.message_id, scope.attempt_seq)?;
                if let Some(old) = &track.scope {
                    if old.attempt_seq == scope.attempt_seq {
                        return if old == &scope {
                            Ok(())
                        } else {
                            Err(MessageError::Conflict(
                                "attempt is already armed with another scope".into(),
                            ))
                        };
                    }
                }
                if track.restarted {
                    return Err(MessageError::Conflict(
                        "replayed attempt requires owner resolution".into(),
                    ));
                }
                track.probably_landed = false;
                track.hold = None;
                track.scope = Some(scope);
            }
            HistoryUpdate::Evidence {
                id,
                receipt,
                probably_landed,
            } => {
                let track = self.track(id)?;
                if receipt.is_some() {
                    track.receipt = receipt;
                }
                track.probably_landed |= probably_landed;
            }
            HistoryUpdate::Consumed { id, step } => {
                let snapshot = self.snapshot(id, now)?;
                let expected = next_delivery(&snapshot.message, &snapshot.live, now, &self.policy)
                    .map_err(|error| MessageError::Internal(error.to_string()))?;
                if expected != step {
                    return Err(MessageError::Conflict(
                        "consumed step is not the current policy decision".into(),
                    ));
                }
                let mut live = snapshot.live;
                track_hold(&mut live, &step, now);
                let track = self.track(id)?;
                track.hold_since = live.hold_since;
                track.stale_since = live.stale_blocker_since;
                if track.stale_since.is_none() {
                    track.stale_blocker = None;
                }
            }
            HistoryUpdate::Paste {
                id,
                pane,
                certainty,
            } => {
                let message = self
                    .state
                    .message(&id)
                    .ok_or_else(|| MessageError::NotFound(format!("message {id} was not found")))?;
                if message
                    .last_attempt
                    .as_ref()
                    .is_none_or(|attempt| attempt.pane != pane.as_str())
                {
                    return Err(MessageError::Conflict(
                        "paste does not match applied attempted pane".into(),
                    ));
                }
                if certainty != EffectCertainty::NotStarted {
                    let mut changed = vec![];
                    for (recipient, history) in &mut self.recipients {
                        if history
                            .observation
                            .as_ref()
                            .is_some_and(|obs| obs.pane.as_ref() == Some(&pane))
                        {
                            history.input = InputLine::Unknown;
                            history.status_since = None;
                            changed.push(recipient.clone());
                        }
                    }
                    for recipient in changed {
                        self.reset_clocks(&recipient, false);
                    }
                    self.pastes.insert(pane, now);
                }
            }
            HistoryUpdate::Held {
                id,
                attempt_seq,
                hold,
            } => {
                let track = self.matching_attempt(id, attempt_seq)?;
                track.probably_landed = true;
                track.hold = Some(hold);
            }
            HistoryUpdate::OwnerRetry { id, attempt_seq } => {
                let track = self.matching_attempt(id, attempt_seq)?;
                track.restarted = false;
                track.probably_landed = false;
                track.hold = None;
            }
        }
        Ok(())
    }
}

/// Short-lock serialized A1 history. A2 owns the sole Applied writer and wiring.
#[derive(Debug, Clone)]
pub struct MessageHistoryHandle {
    inner: Arc<HistoryBackend>,
}

#[derive(Debug)]
enum HistoryBackend {
    Disabled(String),
    Live {
        state: Box<Mutex<MessageHistory>>,
        cursor: watch::Sender<u64>,
    },
}

impl MessageHistoryHandle {
    /// Construct before enabling A2 jobs; replay from zero through the startup
    /// high-water mark before scheduling. This starts no task or org writer.
    pub fn new(
        startup_high_water: u64,
        recovery_time: DateTime<Utc>,
        policy: DeliveryPolicy,
    ) -> Result<Self, MessageError> {
        Ok(Self {
            inner: Arc::new(HistoryBackend::Live {
                state: Box::new(Mutex::new(MessageHistory::new(
                    startup_high_water,
                    recovery_time,
                    policy,
                ))),
                cursor: watch::channel(0).0,
            }),
        })
    }

    /// Wait for the sole watch writer's fold. B bounds this by its work deadline;
    /// no mutex is held over the wait and notifications cannot miss prior folds.
    pub async fn folded(&self, seq: u64) -> Result<(), MessageError> {
        match self.inner.as_ref() {
            HistoryBackend::Disabled(reason) => Err(MessageError::Unavailable(reason.clone())),
            HistoryBackend::Live { cursor, .. } => {
                let mut receiver = cursor.subscribe();
                receiver
                    .wait_for(|cursor| *cursor >= seq)
                    .await
                    .map_err(|_| {
                        MessageError::Unavailable("message history fold channel closed".into())
                    })?;
                Ok(())
            }
        }
    }

    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(HistoryBackend::Disabled(reason.into())),
        }
    }

    pub fn snapshot(
        &self,
        id: MessageId,
        now: DateTime<Utc>,
    ) -> Result<DeliverySnapshot, MessageError> {
        match self.inner.as_ref() {
            HistoryBackend::Disabled(reason) => Err(MessageError::Unavailable(reason.clone())),
            HistoryBackend::Live { state, .. } => state
                .lock()
                .map_err(|_| MessageError::Unavailable("message history lock poisoned".into()))?
                .snapshot(id, now),
        }
    }

    pub fn record(&self, update: HistoryUpdate, now: DateTime<Utc>) -> Result<(), MessageError> {
        match self.inner.as_ref() {
            HistoryBackend::Disabled(reason) => Err(MessageError::Unavailable(reason.clone())),
            HistoryBackend::Live { state, cursor } => {
                let mut state = state.lock().map_err(|_| {
                    MessageError::Unavailable("message history lock poisoned".into())
                })?;
                state.record(update, now)?;
                cursor.send_replace(state.cursor());
                Ok(())
            }
        }
    }
}
