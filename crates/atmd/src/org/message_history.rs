//! A1's pure live-view history contract, serialized by A2's handle boundary.
//!
//! No history is implemented yet: every read/update returns typed unavailable.
//! Future history folds only applied envelopes, never decides/writes org commands.
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
use atm_core::{Harness, ScreenFacts, SessionId, SessionStatus};
use atm_org::{
    delivery::{DeliveryPolicy, DeliveryStep, RecipientLiveView},
    Agent, AgentId, DeliveryReceipt, Message, MessageId, OrgEvent,
};
use chrono::{DateTime, Utc};
use std::sync::Arc;

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

/// Short-lock serialized A1 history. A2 owns the sole Applied writer and wiring.
#[derive(Debug, Clone)]
pub struct MessageHistoryHandle {
    disabled_reason: Arc<str>,
}

impl MessageHistoryHandle {
    /// Live history inputs, frozen for A1. Construct before enabling A2 jobs;
    /// replay from zero through startup_high_water before scheduling.
    pub fn new(
        _startup_high_water: u64,
        _recovery_time: DateTime<Utc>,
        _policy: DeliveryPolicy,
    ) -> Result<Self, MessageError> {
        Err(MessageError::Unavailable(
            "message history is not configured".into(),
        ))
    }

    /// Wait until the all-topic watch has folded seq. B bounds this by its
    /// current deadline; closure/loss returns an error, never synthetic progress.
    pub async fn folded(&self, _seq: u64) -> Result<(), MessageError> {
        Err(MessageError::Unavailable(self.disabled_reason.to_string()))
    }

    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            disabled_reason: Arc::from(reason.into()),
        }
    }

    pub fn snapshot(
        &self,
        _id: MessageId,
        _now: DateTime<Utc>,
    ) -> Result<DeliverySnapshot, MessageError> {
        Err(MessageError::Unavailable(self.disabled_reason.to_string()))
    }

    pub fn record(&self, _update: HistoryUpdate, _now: DateTime<Utc>) -> Result<(), MessageError> {
        Err(MessageError::Unavailable(self.disabled_reason.to_string()))
    }
}
