//! B's single bounded reserved attempt. No delivery is enabled by this seam.
//!
//! Work is at most 30 seconds, clipped by the supplied absolute deadline; U8c
//! cleanup gets its separate 10-second budget. A2 owns the recipient slot and
//! admission permit through cleanup. See `messages` for policy-step ownership.

use super::messages::{MessageError, MessageRuntime};
use atm_org::MessageId;
use tokio::time::Instant;

/// Transient cleanup progress, never a durable message status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPhase {
    Waiting,
    Reserved,
    AttemptCommitted,
    PasteDispatched,
    EnterDispatched,
    AwaitingReceipt,
    Settling,
    Released,
}

/// Unknown is potentially applied, never permission to retry the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCertainty {
    NotStarted,
    Applied,
    Unknown,
}

/// Execute at most one attempt; B supplies the implementation after seam review.
/// No stub may claim success, mutate history, reserve a pane or send input.
pub async fn deliver(
    _rt: MessageRuntime,
    _id: MessageId,
    _deadline: Instant,
) -> Result<(), MessageError> {
    Err(MessageError::Unavailable(
        "message delivery executor is not configured".into(),
    ))
}
