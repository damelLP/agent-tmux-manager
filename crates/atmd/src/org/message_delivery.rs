//! B's single bounded reserved attempt. No delivery is enabled by this seam.
//!
//! Work is at most 30 seconds, clipped by the supplied absolute deadline; U8c
//! cleanup gets its separate 10-second budget. A2 owns the recipient slot and
//! admission permit through cleanup. See `messages` for policy-step ownership.

use super::message_history::{EffectCertainty, ExecutorHold};
use super::messages::{MessageError, MessageRuntime};
use crate::control::reservation::{CaptureFormat, PaneCapture, ReservationError};
use atm_core::{
    styled_screen::StyledScreen, DialogState, InputState, ScreenFacts, SessionId, TurnState,
};
use atm_org::{AgentStatus, MessageId};
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

/// What one guarded paste or key call left behind, read from the error it
/// returned. Only for that call's own error: an error from `run_reserved`, or
/// one a caller built, says nothing about an effect.
///
/// NotStarted only where [`ReservationError`] documents the variant as "before
/// effect" for one guarded paste/key call. `Control` stays Unknown whatever it
/// holds: a tmux failure can follow the input. No error is ever Applied. The
/// match has no catch-all arm, so a new variant does not compile until it is
/// classified.
#[must_use]
pub fn certainty(error: &ReservationError) -> EffectCertainty {
    match error {
        ReservationError::NotImplemented { .. }
        | ReservationError::InvalidRequest { .. }
        | ReservationError::Busy { .. }
        | ReservationError::StaleReservation
        | ReservationError::ScreenChanged { .. }
        | ReservationError::PacingRefused { .. }
        | ReservationError::PaneInputRefused { .. }
        | ReservationError::UnsafePaste { .. }
        | ReservationError::TagMismatch { .. }
        | ReservationError::PaneGone { .. }
        | ReservationError::InvalidObservation { .. }
        | ReservationError::Quarantined { .. } => EffectCertainty::NotStarted,
        ReservationError::Unavailable
        | ReservationError::Timeout
        | ReservationError::Control(_) => EffectCertainty::Unknown,
    }
}

/// The input a delivery guard is asked to allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedInput {
    Paste,
    Enter,
}

/// What one fresh capture shows, as plain facts. `screen` and `staged` come
/// from the one screen dispatch, which already answers Unknown and `None`
/// unless the harness is positively the pane's foreground program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryGuardFacts {
    pub input: GuardedInput,
    pub screen: ScreenFacts,
    /// The adapter's exact match of the staged draft with the reference.
    pub staged: Option<bool>,
    /// Some(false) is the only observed absence of the owner's focus.
    pub focused: Option<bool>,
    /// None: tmux did not report the flag (before 3.7).
    pub bracketed_paste: Option<bool>,
    pub in_mode: bool,
    pub dead: bool,
}

/// Whether this capture allows the one-line reference to be pasted, or Enter
/// to submit it. Every fact must be positively known: Unknown, `None` and a
/// value this build does not know all refuse.
///
/// Paste needs an empty editor; Enter needs the adapter's exact match of the
/// staged draft instead. A dialog of any kind refuses, also when the draft
/// matches: a match never chooses a menu entry or answers a prompt. An
/// unreported bracketed-paste flag is allowed, because the reference is one
/// printable ASCII line; a reported "off" refuses.
#[must_use]
pub fn guard_allows_input(facts: &DeliveryGuardFacts) -> bool {
    let editor = match facts.input {
        GuardedInput::Paste => facts.screen.input == InputState::Empty,
        GuardedInput::Enter => facts.staged == Some(true),
    };
    editor
        && facts.screen.turn == TurnState::NotRunning
        && facts.screen.dialog == DialogState::None
        && facts.focused == Some(false)
        && facts.bracketed_paste != Some(false)
        && !facts.in_mode
        && !facts.dead
}

/// Whether this run's `DeliveryAttempted` is in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptCommit {
    /// Not submitted, or the organisation answered without a new attempt.
    Absent,
    /// Submitted, and neither its reply nor the FIFO fence accounted for it.
    Unknown,
    /// Committed and folded: its log sequence is known.
    Committed,
}

/// What is known when an attempt stops, whatever stopped it: a return, an
/// error, a deadline or cancellation. Those decide when work ends, never what
/// may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptExitFacts {
    pub attempt: AttemptCommit,
    /// An exact receipt is retained for the message, from any of its attempts.
    pub receipt: bool,
    /// The one paste call. NotStarted: never dispatched, or [`certainty`]
    /// proved that it sent nothing.
    pub paste: EffectCertainty,
    /// The `[mN]` token in the final capture's decoded cells. None: that
    /// capture was not taken, failed or could not be decoded.
    pub token_visible: Option<bool>,
}

/// What cleanup owes an attempt before the recipient is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptDisposition {
    /// No attempt of this run exists: no event, no hold.
    NoAttempt,
    /// Persist Delivered, await its fold, then acknowledge the receipt. The
    /// receipt stays retained until then, so a stalled write loses nothing.
    Deliver,
    /// The attempt typed nothing: `DeliveryFailed` may be written. If it
    /// cannot be persisted and folded, hold `InputOutcomeUnknown` instead.
    Fail,
    /// Something may have been typed: never `DeliveryFailed`; retain this hold.
    Hold(ExecutorHold),
    /// Nothing can be retained for this attempt, because its log sequence is
    /// not known: cleanup fails.
    Unaccountable,
}

/// The exit rule: an exact receipt wins over everything; otherwise only a
/// paste that is known to have sent nothing may be failed, and a paste that
/// was or may have been applied is held, with the token as its reason.
#[must_use]
pub fn disposition(facts: &AttemptExitFacts) -> AttemptDisposition {
    if facts.receipt {
        return AttemptDisposition::Deliver;
    }
    match (facts.paste, facts.attempt) {
        (EffectCertainty::NotStarted, AttemptCommit::Absent) => AttemptDisposition::NoAttempt,
        (EffectCertainty::NotStarted, AttemptCommit::Committed) => AttemptDisposition::Fail,
        (EffectCertainty::Applied | EffectCertainty::Unknown, AttemptCommit::Committed) => {
            AttemptDisposition::Hold(if facts.token_visible == Some(true) {
                ExecutorHold::TokenVisible
            } else {
                ExecutorHold::InputOutcomeUnknown
            })
        }
        // No sequence to hold under. Possible input with no known attempt
        // has none either, and fails closed the same way.
        (_, AttemptCommit::Unknown)
        | (EffectCertainty::Applied | EffectCertainty::Unknown, AttemptCommit::Absent) => {
            AttemptDisposition::Unaccountable
        }
    }
}

/// How far cleanup got with a disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// The disposition's event is persisted and folded.
    Written,
    /// History retained a hold for the attempt.
    Held,
    Neither,
}

/// Whether cleanup may return Ok. An error quarantines the agent, so it is
/// kept for an attempt that needs a veto and has none: a slow or failed write
/// is not one, once the hold is retained.
#[must_use]
pub fn cleanup_succeeds(disposition: AttemptDisposition, settled: Settled) -> bool {
    match disposition {
        AttemptDisposition::NoAttempt | AttemptDisposition::Deliver => true,
        AttemptDisposition::Fail => settled != Settled::Neither,
        AttemptDisposition::Hold(_) => settled == Settled::Held,
        AttemptDisposition::Unaccountable => false,
    }
}

/// Whether the message's `[mN]` token is on the screen. For a hold's reason
/// only: an absent token never proves that nothing was typed.
///
/// Neutral: the shared style decoder and no adapter. The token is searched in
/// the decoded text with the rows joined, so neither a style change nor a row
/// break inside it hides it. `None`: a styled capture that does not decode.
#[must_use]
pub fn token_visible(capture: &PaneCapture, id: MessageId) -> Option<bool> {
    let decoded;
    let text = match capture.format {
        CaptureFormat::Plain => capture.text.as_str(),
        CaptureFormat::Ansi => {
            decoded = StyledScreen::parse(&capture.text).ok()?;
            decoded.text()
        }
    };
    Some(text.replace('\n', "").contains(&format!("[{id}]")))
}

/// The session an attempt may be confirmed against: `Some` for a bound agent,
/// `None` for a lazy first prompt. From the agent's recorded status and
/// binding and the pane's one eligible row, all read in one snapshot.
///
/// Only a Ready agent takes a delivery. A recorded binding must be the pane's
/// eligible row: no row, or another one, means the binding is stale. Without
/// a binding only a pane with no eligible row is attempted: a row the agent
/// is not bound to proves nothing yet.
pub fn attempt_session(
    status: AgentStatus,
    recorded: Option<&SessionId>,
    eligible: Option<&SessionId>,
) -> Result<Option<SessionId>, MessageError> {
    if status != AgentStatus::Ready {
        return Err(MessageError::Refused("recipient is not ready".into()));
    }
    if recorded != eligible {
        return Err(MessageError::Refused(
            "recipient's recorded session is not the pane's eligible session".into(),
        ));
    }
    Ok(recorded.cloned())
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

#[cfg(test)]
#[path = "message_delivery_tests.rs"]
mod tests;
