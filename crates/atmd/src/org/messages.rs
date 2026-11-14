//! Confirmed-message facade. Only explicitly disabled handles exist at this stage.
//!
//! Calls return typed unavailability without organisation, file or tmux effects.
//! Runtime startup, evidence and scheduling are deliberately not stubbed here.
//! Their later implementation must share one injected monotonic/base-UTC clock.

pub use super::message_methods::register_message_methods;
pub use super::message_types::*;
use super::OrgServiceError;
use crate::server::methods::CallContext;
use std::sync::Arc;

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
