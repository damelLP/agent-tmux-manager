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
//! Applied sequences are folded once in order, including skipped unknown kinds.

use super::{
    message_delivery::EffectCertainty, message_evidence::AttemptScope, messages::MessageError,
};
use crate::control::reservation::PaneId;
use atm_core::{Harness, ScreenFacts, SessionId, SessionStatus};
use atm_org::{
    delivery::{DeliveryStep, RecipientLiveView},
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
    pub last_attempt_seq: Option<u64>,
    /// Current FIFO predecessor, not a list of followers.
    pub blocked_by: Option<MessageId>,
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
    /// Genuine accepted event receive time; no provenance cannot grant stale readiness.
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
    /// NotStarted must never manufacture a paste timestamp.
    Paste {
        id: MessageId,
        pane: PaneId,
        certainty: EffectCertainty,
    },
    /// Release only this matching attempt's transient hold, never its durable
    /// attempt count or late-receipt scopes. A stale attempt sequence refuses.
    OwnerRetry { id: MessageId, attempt_seq: u64 },
}

/// A2 will serialize the A1 history through this cloneable boundary.
#[derive(Debug, Clone)]
pub struct MessageHistoryHandle {
    disabled_reason: Arc<str>,
}

impl MessageHistoryHandle {
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
