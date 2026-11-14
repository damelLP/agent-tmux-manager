//! A2's exact-receipt and receive-order activity boundary; unavailable for now.
//!
//! Only already-translated neutral lifecycle events enter here. A2 owns the one
//! connection tap: apply to the registry first, exclude child contexts, and check
//! original session/pane compatibility (registry Ok alone does not prove it).
//! Never repair old-event identity from the new occupant of a pane. Accepted
//! status-line activity calls `observe_activity`; a poll is not activity.
//!
//! Register/arm/observe must share one short, synchronous ordering point. Wakes
//! may coalesce but facts cannot be dropped; retention failure disables evidence
//! and blocks input/retries. No await or callback into the service on ingestion.
//! Keep earlier attempt scopes for late receipts, including after final failure.
//! Activity/token visibility may latch probably-landed, never a receipt.

use super::messages::{MessageClock, MessageError};
use crate::control::reservation::PaneId;
use atm_core::{Harness, LifecycleEvent, SessionId};
use atm_org::{AgentId, DeliveryReceipt, MessageId};
use std::{collections::HashSet, sync::Arc};

/// Scope recorded after Attempted is applied, before paste can be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptScope {
    pub message_id: MessageId,
    /// Org log sequence of the applied DeliveryAttempted, not the attempt number.
    pub attempt_seq: u64,
    pub recipient: AgentId,
    pub pane: PaneId,
    pub harness: Harness,
    /// Some requires this exact session and pane, not a later binding.
    pub session_id: Option<SessionId>,
    /// For a lazy first prompt, require a new eligible parent ID outside this set
    /// and the same live owned pane/harness. Child/pending IDs never confirm.
    pub excluded_sessions: HashSet<SessionId>,
}

/// Small retained facts, not a transcript or a status-derived receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptEvidence {
    pub receipt: Option<DeliveryReceipt>,
    pub probably_landed: bool,
}

/// A2 supplies live retention later. All operations now fail explicitly, even
/// receipt reads; Ok(None) could incorrectly authorize an automatic retry.
#[derive(Debug, Clone)]
pub struct MessageEvidence {
    /// The runtime clones this origin; the connection never supplies Utc::now.
    clock: MessageClock,
    disabled_reason: Arc<str>,
}

impl MessageEvidence {
    #[must_use]
    pub fn disabled(clock: MessageClock, reason: impl Into<String>) -> Self {
        Self {
            clock,
            disabled_reason: Arc::from(reason.into()),
        }
    }

    /// The single injected origin shared by runtime, evidence and scheduler.
    #[must_use]
    pub fn clock(&self) -> &MessageClock {
        &self.clock
    }

    fn unavailable(&self) -> MessageError {
        MessageError::Unavailable(self.disabled_reason.to_string())
    }

    /// B registers the full exact inbox reference before any attempt, not a token
    /// substring, body, first_prompt preview, prompt_id or turn_id.
    pub fn register(&self, _id: MessageId, _expected_prompt: String) -> Result<(), MessageError> {
        Err(self.unavailable())
    }

    pub fn arm(&self, _scope: AttemptScope) -> Result<(), MessageError> {
        Err(self.unavailable())
    }

    pub fn receipt(&self, _id: MessageId) -> Result<Option<DeliveryReceipt>, MessageError> {
        Err(self.unavailable())
    }

    pub fn snapshot(&self, _id: MessageId) -> Result<AttemptEvidence, MessageError> {
        Err(self.unavailable())
    }

    /// Only after this receipt's Delivered is committed and folded, not merely
    /// after submitting the command. Failed append/reply loss retains evidence.
    pub fn acknowledge(
        &self,
        _id: MessageId,
        _receipt: &DeliveryReceipt,
    ) -> Result<(), MessageError> {
        Err(self.unavailable())
    }

    pub fn observe(
        &self,
        _session: &SessionId,
        _pane: Option<&str>,
        _harness: Harness,
        _event: &LifecycleEvent,
    ) -> Result<(), MessageError> {
        Err(self.unavailable())
    }

    /// Accepted genuine activity only; this path can never create a receipt.
    pub fn observe_activity(
        &self,
        _session: &SessionId,
        _pane: Option<&str>,
    ) -> Result<(), MessageError> {
        Err(self.unavailable())
    }
}
