//! B's single bounded reserved attempt. No delivery is enabled by this seam.
//!
//! Work is at most 30 seconds, clipped by the supplied absolute deadline; U8c
//! cleanup gets its separate 10-second budget. A2 owns the recipient slot and
//! admission permit through cleanup. See `messages` for policy-step ownership.

use super::messages::{MessageError, MessageRuntime};
use atm_org::MessageId;
use std::time::Duration;
use tokio::time::Instant;

pub const ATTEMPT_WORK_BUDGET: Duration = Duration::from_secs(30);
pub const ATTEMPT_CLEANUP_BUDGET: Duration = Duration::from_secs(10);
/// Retry/give-up: 30 work + 10 cleanup + 5 FIFO snapshot + 6 org execute +
/// 4 service overhead = 55 seconds, leaving 5 seconds before the 60 s router.
/// Includes recipient-slot admission; timeout never implies accepted work rolled back.
pub const MESSAGE_RESOLUTION_BUDGET: Duration = Duration::from_secs(55);

/// B owns this exact Q2 text; A2 uses it to register replayed expectations too.
#[must_use]
pub fn inbox_reference(id: MessageId) -> String {
    format!("[{id}] Read: atm inbox --id {id}")
}

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

/// Execute at most one attempt; B supplies the implementation after seam review.
/// No stub may claim success, mutate history, reserve a pane or send input.
///
/// A2's error disposition is based on committed/effect facts, never just Err:
/// - InvalidParams/NotFound/Conflict/Refused and Reservation errors before any
///   attempted commit: refresh observations and reevaluate policy; no direct paste.
/// - Timeout/Unavailable/Internal/Organisation and Reservation errors can hide a
///   commit/effect. Account for accepted work through FIFO org state and folded.
/// - After any committed Attempted, ALL errors retain the attempt unless cleanup
///   proves no effect and persists/awaits DeliveryFailed. Otherwise record Held
///   InputOutcomeUnknown and keep it held; failed history/persistence disables
///   rescheduling rather than losing this veto. Staged mismatch/token holds use
///   their specific Held reason. A receipt always remains eligible to win later.
///
/// Cancellation: A2 stops scheduling; B checks before admission and before each
/// effect, and while awaiting receipt. After commit, cancellation enters the same
/// effect-accounted cleanup, never drops a dispatched effect or releases a hold.
/// Cleanup uses its fresh capability/budget and is not cancelled by this token.
pub async fn deliver(
    _rt: MessageRuntime,
    _id: MessageId,
    _deadline: Instant,
) -> Result<(), MessageError> {
    Err(MessageError::Unavailable(
        "message delivery executor is not configured".into(),
    ))
}
