//! Confirmed-message facade and the reviewed A/B/C runtime boundary.
//!
//! Only disabled handles and unavailable runtime operations exist at this stage.
//! The clock and dependency assembly are real; neither starts work. All callers
//! share one injected monotonic/base-UTC clock. A1 owns pure history, A2 the
//! service/evidence/wiring, B one reserved attempt, and C the CLI/notifications.
//!
//! # Policy-step ownership
//!
//! | Step / transition | Owner |
//! |---|---|
//! | Wait, readiness observe, wake/FIFO scheduling | A2, with A1 clocks; no reservation held |
//! | PasteNow admission | A2 recipient slot and one of eight permits; B reserves and rechecks |
//! | AttemptCommitted, arm, guarded paste/Enter | B; apply Attempted before input; record consumed step/paste before release |
//! | AwaitConfirmation while reserved | B; no more keys |
//! | Confirm while reserved | B persists/folds Delivered, then acknowledges evidence |
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
//! B builds the exact one-line inbox reference for every message and registers
//! that text with evidence. Receipt means reference acceptance, not body reading.
//! B waits for U8c focus/read-only observe and U8's single screen dispatch; this
//! boundary neither adds vendor parsing nor implements a second input path.

pub use super::message_methods::register_message_methods;
pub use super::message_types::*;
use super::{
    message_evidence::MessageEvidence, message_history::MessageHistoryHandle, OrgHandle,
    OrgServiceError,
};
use crate::{
    control::service::ControlHandle, registry::RegistryHandle, server::methods::CallContext,
};
use atm_org::delivery::DeliveryPolicy;
use chrono::{DateTime, TimeDelta, Utc};
use std::sync::Arc;
use tokio::{sync::Semaphore, time::Instant};

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

    /// Uses Tokio's clock so paused-time tests exercise the production time source.
    pub fn now(&self) -> Result<DateTime<Utc>, MessageError> {
        self.at(Instant::now())
    }
}

/// Reserve room in U8c's shared pool for lifecycle work from other services.
pub const MESSAGE_ADMISSION_LIMIT: usize = 8;

/// Cloned dependencies, not a second organisation writer or recipient scheduler.
/// A2 owns construction and must supply evidence with the same clock origin.
#[derive(Clone)]
pub struct MessageRuntime {
    pub org: OrgHandle,
    pub registry: RegistryHandle,
    pub control: ControlHandle,
    pub evidence: MessageEvidence,
    pub history: MessageHistoryHandle,
    pub policy: DeliveryPolicy,
    pub clock: MessageClock,
    /// A2 acquires before B's admission and retains the permit through cleanup.
    pub admissions: Arc<Semaphore>,
}

impl MessageRuntime {
    /// Pure dependency assembly; no tasks, observations, writes or input effects.
    #[must_use]
    pub fn new(
        org: OrgHandle,
        registry: RegistryHandle,
        control: ControlHandle,
        evidence: MessageEvidence,
        history: MessageHistoryHandle,
        policy: DeliveryPolicy,
        clock: MessageClock,
    ) -> Self {
        Self {
            org,
            registry,
            control,
            evidence,
            history,
            policy,
            clock,
            admissions: Arc::new(Semaphore::new(MESSAGE_ADMISSION_LIMIT)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageHandle {
    disabled_reason: Arc<str>,
}

impl MessageHandle {
    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            disabled_reason: Arc::from(reason.into()),
        }
    }

    fn unavailable(&self) -> MessageError {
        MessageError::Unavailable(self.disabled_reason.to_string())
    }

    /// Durable enqueue, not delivery. Caller identity is attribution, not authentication.
    pub async fn send(
        &self,
        _p: SendParams,
        _caller: CallContext,
    ) -> Result<SendResult, MessageError> {
        Err(self.unavailable())
    }

    /// Read-only inbox projection; fetching does not acknowledge reading.
    pub async fn inbox(
        &self,
        _p: InboxParams,
        _caller: CallContext,
    ) -> Result<InboxResult, MessageError> {
        Err(self.unavailable())
    }

    /// Acknowledge a fully validated batch of delivered messages in the caller's inbox.
    pub async fn ack(
        &self,
        _p: AckParams,
        _caller: CallContext,
    ) -> Result<AckResult, MessageError> {
        Err(self.unavailable())
    }

    /// Owner release of a latched hold on this same ID, accepting duplicate risk.
    /// Recoverable waits are not retryable; attempt limits remain in force.
    /// Resolution uses the recipient slot, never an Observe reservation.
    pub async fn retry(
        &self,
        _p: ResolveParams,
        _caller: CallContext,
    ) -> Result<RetryResult, MessageError> {
        Err(self.unavailable())
    }

    /// Owner cancellation of any held Queued/Delivering message, not only Blocked.
    /// An active attempt must settle first; a later receipt may still confirm it.
    /// Resolution uses the recipient slot, never an Observe reservation.
    pub async fn give_up(
        &self,
        _p: ResolveParams,
        _caller: CallContext,
    ) -> Result<GiveUpResult, MessageError> {
        Err(self.unavailable())
    }

    pub async fn initialized(&self) -> Result<(), MessageError> {
        Err(self.unavailable())
    }
}
