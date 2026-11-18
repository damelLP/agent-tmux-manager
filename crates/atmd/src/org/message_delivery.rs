//! B's single bounded reserved attempt.
//!
//! Work is at most 30 seconds, clipped by the supplied absolute deadline; U8c
//! cleanup gets its separate 10-second budget. A2 owns the recipient slot and
//! admission permit through cleanup. See `messages` for policy-step ownership.
//!
//! An attempt runs under one Delivery reservation. Inside it the recipient,
//! its screen and the policy are read again; only `PasteNow` goes on. The
//! attempt's event is committed and folded, evidence and then history are
//! armed with its scope, and the reference is pasted once, behind a guard on
//! a fresh capture. The editor is then watched, with no key sent, until the
//! adapter reads the reference as the staged draft; one Enter follows, behind
//! the same guard, and the attempt waits for the session's own prompt event
//! until the confirmation window that began at the attempt's event ends.
//! Cleanup settles by [`disposition`] over what work left behind, whatever
//! stopped it. Before release it retains the stopped original attempt's input
//! certainties in process-local history. A historically eligible held draft
//! may later consume one claim under a new Delivery reservation: fresh guards
//! authorize one Enter only, never another paste/attempt or confirmation window.

use super::agent_screen::{AgentScreen, PaneBaseline};
use super::agents::{AgentError, Recipient};
use super::message_evidence::AttemptScope;
use super::message_history::{
    EffectCertainty, ExecutorHold, HistoryUpdate, RecipientObservation, SessionObservation,
};
use super::messages::{recipient_observation, MessageError, MessageRuntime};
use super::start_line::delivery_line;
use super::OrgServiceError;
use crate::control::reservation::{
    CaptureFormat, PaneCapture, PaneId, PaneKey, Reservation, ReservationError, ReservationKind,
    ReservationRequest, WaitPolicy,
};
use atm_core::{
    harness_control::ControlDialog, styled_screen::StyledScreen, DialogState, InputState,
    ScreenFacts, SessionId, TurnState,
};
use atm_org::{
    delivery::{next_delivery, DeliveryStep},
    AgentStatus, ControlAudit, DeliveryReceipt, MessageId, OrgCommand, OrgEvent,
};
use chrono::TimeDelta;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{sleep_until, timeout_at, Instant};

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
#[cfg(test)]
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

/// What the one guarded paste call left behind, read from its own result: Ok
/// is Applied. Only for that call's result, taken where it returns: an error
/// from `run_reserved`, from a later call or one a caller built says nothing
/// about the paste. The Enter call's own result is read the same way, for
/// the words of a hold only: it never decides another key.
#[must_use]
pub fn paste_certainty(result: &Result<(), ReservationError>) -> EffectCertainty {
    match result {
        Ok(()) => EffectCertainty::Applied,
        Err(error) => certainty(error),
    }
}

/// The error of one guarded paste or key call, as what that call left behind.
///
/// NotStarted only where [`ReservationError`] documents the variant as "before
/// effect" for one guarded paste/key call. `Control` stays Unknown whatever it
/// holds: a tmux failure can follow the input. No error is ever Applied. The
/// match has no catch-all arm, so a new variant does not compile until it is
/// classified.
fn certainty(error: &ReservationError) -> EffectCertainty {
    match error {
        #[cfg(test)]
        ReservationError::NotImplemented { .. } => EffectCertainty::NotStarted,
        ReservationError::InvalidRequest { .. }
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

/// What one fresh capture shows, as plain facts. `screen`, `control_dialog`
/// and `staged` come from the one screen dispatch, which already answers
/// Unknown and `None` unless the harness is positively the pane's foreground
/// program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryGuardFacts {
    pub input: GuardedInput,
    /// The facts of the dispatch's control read of the capture.
    pub screen: ScreenFacts,
    /// That same read's dialog: a recognised start dialog, Blocked, or
    /// Unknown also refuses input. Completion popups may instead be refused
    /// by the screen facts; this additional check only ever refuses.
    pub control_dialog: ControlDialog,
    /// The adapter's exact match of the staged draft with the reference.
    pub staged: Option<bool>,
    /// Some(false) is the only observed absence of the owner's focus.
    pub focused: Option<bool>,
    /// None: tmux did not report the flag (before 3.7).
    pub bracketed_paste: Option<bool>,
    pub in_mode: bool,
    pub dead: bool,
    /// tmux would fan input out to the window's other panes, or drop it.
    pub synchronized: bool,
    pub input_off: bool,
}

/// Whether this capture allows the one-line reference to be pasted, or Enter
/// to submit it. Every fact must be positively known: Unknown, `None` and a
/// value this build does not know all refuse.
///
/// Paste needs an empty editor. Enter needs an editor that holds a draft and
/// the adapter's exact match of that draft with the reference: a match in an
/// editor read as empty or unknown means the screen is not understood. A
/// dialog of any kind, in either read, refuses, also when the draft matches:
/// a match never chooses a menu entry or answers a prompt. An unreported
/// bracketed-paste flag is allowed, because the reference is one printable
/// ASCII line; a reported "off" refuses.
#[must_use]
pub fn guard_allows_input(facts: &DeliveryGuardFacts) -> bool {
    let editor = match facts.input {
        GuardedInput::Paste => facts.screen.input == InputState::Empty,
        GuardedInput::Enter => {
            facts.screen.input == InputState::Draft && facts.staged == Some(true)
        }
    };
    editor
        && facts.screen.turn == TurnState::NotRunning
        && facts.screen.dialog == DialogState::None
        && facts.control_dialog == ControlDialog::None
        && facts.focused == Some(false)
        && facts.bracketed_paste != Some(false)
        && !facts.in_mode
        && !facts.dead
        && !facts.synchronized
        && !facts.input_off
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
    /// The one paste call. NotStarted: never dispatched, or
    /// [`paste_certainty`] proved that it sent nothing.
    pub paste: EffectCertainty,
    /// The `[mN]` token in the final capture's decoded cells. None: that
    /// capture was not taken, failed or could not be decoded.
    pub token_visible: Option<bool>,
    /// The attempt's Enter. NotStarted: no Enter call was made, or its own
    /// result proved that it sent nothing. Applied: a call returned Ok.
    /// Unknown: a call was made and failed otherwise, or never returned.
    pub enter: EffectCertainty,
}

/// What cleanup owes an attempt before the recipient is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptDisposition {
    /// No attempt of this run exists: no event, no hold.
    NoAttempt,
    /// Persist Delivered, await its fold, then acknowledge the receipt. The
    /// receipt stays retained until then, so a stalled write loses nothing.
    Deliver,
    /// The attempt typed nothing, which is recorded. A final attempt is then
    /// left to the policy; for any other `DeliveryFailed` is written. If it
    /// cannot be persisted and folded, hold `InputOutcomeUnknown` instead.
    Fail,
    /// Something may have been typed: never `DeliveryFailed`; retain this
    /// hold, whose reason says what is known of the Enter.
    Hold(ExecutorHold),
    /// Nothing can be retained for this attempt, because its log sequence is
    /// not known: cleanup fails.
    Unaccountable,
}

/// The exit rule: an exact receipt wins over everything; otherwise only a
/// paste that is known to have sent nothing may be failed, and a paste that
/// was or may have been applied is held. The hold's reason: an Enter whose
/// outcome is unknown is "input outcome unknown" whatever the screen shows;
/// with the token on screen, "Enter was not sent" when none was, and the
/// token when an Enter returned Ok and nothing confirmed it; without the
/// token, unknown.
#[must_use]
pub fn disposition(facts: &AttemptExitFacts) -> AttemptDisposition {
    if facts.receipt {
        return AttemptDisposition::Deliver;
    }
    match (facts.paste, facts.attempt) {
        (EffectCertainty::NotStarted, AttemptCommit::Absent) => AttemptDisposition::NoAttempt,
        (EffectCertainty::NotStarted, AttemptCommit::Committed) => AttemptDisposition::Fail,
        (EffectCertainty::Applied | EffectCertainty::Unknown, AttemptCommit::Committed) => {
            AttemptDisposition::Hold(match (facts.enter, facts.token_visible) {
                (EffectCertainty::NotStarted, Some(true)) => ExecutorHold::EnterNotSent,
                (EffectCertainty::Applied, Some(true)) => ExecutorHold::TokenVisible,
                _ => ExecutorHold::InputOutcomeUnknown,
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
    /// A final attempt recorded as having typed nothing. No retry can follow
    /// it; the policy fails the message after the confirmation window.
    LeftToPolicy,
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

/// What an attempt has done so far, shared by its work and its cleanup. Work
/// writes each fact before the await it guards: the control supervisor aborts
/// work at its deadline, and cleanup must still find the fact.
#[derive(Debug, Clone, Copy)]
struct Progress {
    attempt: AttemptCommit,
    /// The attempt number submitted, and the committed cursor just before.
    number: u32,
    before: u64,
    /// The log sequence of the attempt's event, once known.
    seq: Option<u64>,
    paste: EffectCertainty,
    enter: EffectCertainty,
    /// The recipient's capture format, once known.
    format: Option<CaptureFormat>,
    /// How far cleanup got, once it ran.
    settled: Option<Settled>,
}

/// One attempt's dependencies, cloned into its work and its cleanup.
#[derive(Clone)]
struct Attempt {
    rt: MessageRuntime,
    id: MessageId,
    pane: PaneId,
    progress: Arc<Mutex<Progress>>,
}

/// Cleanup's waits end this long before its budget does, so that a hold is
/// always retained in time.
const SETTLE_MARGIN: Duration = Duration::from_secs(2);
const NO_EFFECT: &str = "delivery attempt typed nothing";

fn shutting_down() -> MessageError {
    MessageError::Unavailable("message delivery is shutting down".into())
}

/// The agent service's error as the message error of the same name.
fn agent_error(error: AgentError) -> MessageError {
    match error {
        AgentError::NotImplemented { operation } => {
            MessageError::Unavailable(format!("agent operation '{operation}' is not implemented"))
        }
        AgentError::InvalidParams(reason) => MessageError::InvalidParams(reason),
        AgentError::NotFound(reason) => MessageError::NotFound(reason),
        AgentError::Conflict(reason) => MessageError::Conflict(reason),
        AgentError::Refused(reason) => MessageError::Refused(reason),
        AgentError::Timeout => MessageError::Timeout,
        AgentError::Unavailable(reason) => MessageError::Unavailable(reason),
        AgentError::Internal(reason) => MessageError::Internal(reason),
        AgentError::Reservation(error) => MessageError::Reservation(error),
        AgentError::Organisation(error) => MessageError::Organisation(error),
    }
}

/// One capture of a recipient's pane as history's observation. `recipient`
/// is one [`AgentHandle::recipient`](super::agents::AgentHandle::recipient)
/// answer and `capture` a capture of its pane in its screen's format, both
/// taken for this call: the session row and the screen facts are read from
/// these two and nothing else.
///
/// Shared by the executor, under its reservation, and the scheduler's
/// observer, so that both describe a recipient to history in one way. It
/// answers for an agent in any status: whether the agent may take an attempt
/// is [`attempt_session`]'s question, which the executor asks beside it. An
/// error means the recipient could not be observed, and the caller records a
/// lost observation: also while the agent service has not pinned the pane's
/// harness (a start, an adopt, the pass after a daemon restart), when the
/// screen dispatch would answer unknown facts.
pub(super) fn observation(
    recipient: &Recipient,
    capture: &PaneCapture,
) -> Result<RecipientObservation, MessageError> {
    if recipient.baseline.harness_command().is_none() {
        return Err(MessageError::Refused(format!(
            "the harness on {} is not established yet",
            recipient.pane
        )));
    }
    // History keeps its own activity clock for a session it already saw.
    let session = recipient.session.as_ref().map(|row| SessionObservation {
        id: row.id.clone(),
        harness: recipient.harness,
        status: row.status,
        last_event_at: None,
    });
    let facts = recipient.screen.facts(capture, &recipient.baseline);
    recipient_observation(capture, session, facts)
}

/// [`attempt_session`] on one recipient read.
fn session_gate(recipient: &Recipient) -> Result<Option<SessionId>, MessageError> {
    attempt_session(
        recipient.status,
        recipient.session_id.as_ref(),
        recipient.session.as_ref().map(|row| &row.id),
    )
}

/// One capture as the plain facts a delivery guard decides on: one control
/// read of it for the screen facts and the dialog, and the staged match and
/// the pane's flags from that same capture.
fn guard_facts(
    input: GuardedInput,
    capture: &PaneCapture,
    screen: &AgentScreen,
    baseline: &PaneBaseline,
    reference: &str,
) -> DeliveryGuardFacts {
    let control = screen.control(capture, baseline);
    DeliveryGuardFacts {
        input,
        screen: control.facts,
        control_dialog: control.dialog,
        staged: screen.staged(capture, baseline, reference),
        focused: capture.focused,
        bracketed_paste: capture.inspection.bracketed_paste,
        in_mode: capture.inspection.in_mode,
        dead: capture.inspection.dead,
        synchronized: capture.inspection.synchronized,
        input_off: capture.inspection.input_off,
    }
}

/// Retain only an accounted original attempt, never infer input from its hold.
fn attempt_exit_update(
    id: MessageId,
    progress: Progress,
    cancelled: bool,
) -> Option<HistoryUpdate> {
    if progress.attempt != AttemptCommit::Committed {
        return None;
    }
    Some(HistoryUpdate::AttemptExited {
        id,
        attempt_seq: progress.seq?,
        paste: progress.paste,
        enter: progress.enter,
        cancelled,
    })
}

impl Attempt {
    fn progress(&self) -> Result<Progress, MessageError> {
        (self.progress.lock().map(|progress| *progress))
            .map_err(|_| MessageError::Internal("delivery attempt progress lock poisoned".into()))
    }

    fn set(&self, change: impl FnOnce(&mut Progress)) -> Result<(), MessageError> {
        (self
            .progress
            .lock()
            .map(|mut progress| change(&mut progress)))
        .map_err(|_| MessageError::Internal("delivery attempt progress lock poisoned".into()))
    }

    /// Before each input: the recipient, read again, must still be the one
    /// the attempt was armed for. [`attempt_session`] cannot know the armed
    /// scope, so its answer is compared here, with the pane and the harness;
    /// the scope's baseline must still hold the agent's excluded session.
    async fn still_armed(&self, scope: &AttemptScope) -> Result<(), MessageError> {
        let agents = &self.rt.agents;
        let recipient = (agents.recipient(&scope.recipient).await).map_err(agent_error)?;
        let session = session_gate(&recipient)?;
        let excluded = (recipient.excluded.iter()).all(|old| scope.excluded_sessions.contains(old));
        if recipient.pane == scope.pane
            && recipient.harness == scope.harness
            && session == scope.session_id
            && excluded
        {
            Ok(())
        } else {
            Err(MessageError::Conflict(format!(
                "agent {} changed after its attempt was armed",
                scope.recipient
            )))
        }
    }

    /// One audit record for the attempt's Enter, when its call returns or
    /// recovery cancellation abandons its reply. The queue never waits: when
    /// full or stalled the record is dropped with a warning. Input is not repeated.
    fn audit(&self, attempt: u32, entered: &Result<(), ReservationError>) {
        let Ok(now) = self.rt.clock.now() else {
            return;
        };
        let action = serde_json::json!({
            "method": "msg.deliver",
            "message": self.id.to_string(),
            "attempt": attempt,
            "op": "enter",
            "outcome": if entered.is_ok() { "ok" } else { "error" },
            "error": entered.as_ref().err().map(ToString::to_string),
        });
        let sent = OrgCommand::ControlSent(ControlAudit {
            pane: self.pane.to_string(),
            action,
        });
        self.rt.control.audit_queue().submit(sent, now, None);
    }

    /// Inside the reservation: at most one guarded paste, then at most one
    /// guarded Enter that was sent or may have been, then the wait for the
    /// receipt. Ok also when the policy asked for no paste, a receipt made an
    /// input unnecessary, the pane refused the paste with nothing sent, or
    /// the confirmation window ended: cleanup settles each of those.
    async fn work(&self, r: &Reservation) -> Result<(), MessageError> {
        let (rt, id, pane) = (&self.rt, self.id, &self.pane);
        let agent = r.agent().clone();
        let recipient = rt.agents.recipient(&agent).await.map_err(agent_error)?;
        if recipient.pane != *pane {
            // Lifecycle moved the agent while admission was parked.
            return Err(MessageError::Conflict(format!(
                "agent {agent} is no longer on {pane}"
            )));
        }
        let (screen, format) = (recipient.screen, recipient.screen.capture_format());
        self.set(|progress| progress.format = Some(format))?;
        let tagged = |capture: PaneCapture| {
            if capture.inspection.local_agent_tag.as_deref() == Some(agent.to_string().as_str()) {
                Ok(capture)
            } else {
                Err(MessageError::Conflict(format!(
                    "{pane} is not tagged for agent {agent}"
                )))
            }
        };
        let seen = match r.capture(pane, format).await {
            Ok(capture) => tagged(capture).and_then(|capture| {
                let observation = observation(&recipient, &capture)?;
                Ok((capture, session_gate(&recipient)?, observation))
            }),
            Err(error) => Err(error.into()),
        };
        let now = rt.clock.now()?;
        let (capture, session_id, observation) = match seen {
            Ok(seen) => seen,
            Err(error) => {
                // The recipient could not be observed: what history knew
                // about the pane is no longer continuous.
                let lost = HistoryUpdate::ObservationLost { recipient: agent };
                let _ = rt.history.record(lost, now);
                return Err(error);
            }
        };
        let observed = HistoryUpdate::Observed {
            recipient: agent.clone(),
            observation,
        };
        rt.history.record(observed, now)?;

        let evidence = rt.evidence.snapshot(id)?;
        let copy = HistoryUpdate::Evidence {
            id,
            receipt: evidence.receipt,
            probably_landed: evidence.probably_landed,
        };
        let copied = rt.history.record(copy, now);
        let snapshot = rt.history.snapshot(id, now)?;
        let step = next_delivery(&snapshot.message, &snapshot.live, now, &rt.policy)
            .map_err(|error| MessageError::Internal(error.to_string()))?;
        match copied {
            Ok(()) => {}
            // Delivered meanwhile: the snapshot says so.
            Err(MessageError::Conflict(_)) if step == DeliveryStep::Complete => {}
            Err(error) => return Err(error),
        }
        let DeliveryStep::PasteNow { attempt } = step else {
            // Nothing to attempt: the recipient's job reads the step again.
            return match rt.history.record(HistoryUpdate::Consumed { id, step }, now) {
                Ok(()) | Err(MessageError::Conflict(_)) => Ok(()),
                Err(error) => Err(error),
            };
        };
        let reference = delivery_line(id, &snapshot.message.content);
        let paste = GuardedInput::Paste;
        if !guard_allows_input(&guard_facts(
            paste,
            &capture,
            screen,
            &recipient.baseline,
            &reference,
        )) {
            // What the policy does not see (bracketed paste off, synchronized
            // panes, input off) refuses before an attempt is spent on it.
            return Err(MessageError::Refused(format!(
                "{pane} does not allow a paste now"
            )));
        }
        rt.history
            .record(HistoryUpdate::Consumed { id, step }, now)?;
        if rt.cancel.is_cancelled() {
            return Err(shutting_down());
        }

        // The attempt's event, committed and folded before anything is armed.
        let before = rt.org.status()?;
        self.set(|progress| {
            progress.attempt = AttemptCommit::Unknown;
            progress.number = attempt;
            progress.before = before;
        })?;
        let attempted = OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt,
            pane: pane.to_string(),
        };
        let committed = match rt.org.execute(attempted, now).await {
            Ok(committed) => committed,
            Err(OrgServiceError::Refused(refusal)) => {
                // The writer decided against it: nothing was appended.
                self.set(|progress| progress.attempt = AttemptCommit::Absent)?;
                return Err(MessageError::Conflict(refusal.to_string()));
            }
            Err(error) => return Err(error.into()),
        };
        let ours = |event| {
            matches!(event, Ok(Some(OrgEvent::MessageDeliveryAttempted { message_id, attempt: number, pane: on }))
                if message_id == id && number == attempt && on == pane.as_str())
        };
        let event = (committed.iter())
            .find(|envelope| ours(OrgEvent::from_parts(&envelope.kind, &envelope.body)))
            .map(|envelope| (envelope.seq, envelope.ts));
        let Some((seq, attempted_at)) = event else {
            // An attempt the log already held gives no permission.
            self.set(|progress| progress.attempt = AttemptCommit::Absent)?;
            return Err(MessageError::Conflict(format!(
                "message {id} attempt {attempt} was not committed by this run"
            )));
        };
        self.set(|progress| {
            progress.attempt = AttemptCommit::Committed;
            progress.seq = Some(seq);
        })?;
        rt.history.folded(seq).await?;
        // The confirmation window starts at the attempt's event, not at Enter.
        let settings = rt.policy.settings();
        let confirm_by = (TimeDelta::from_std(settings.confirmation_timeout).ok())
            .and_then(|window| attempted_at.checked_add_signed(window))
            .ok_or_else(|| MessageError::Internal("confirmation deadline overflows".into()))
            .and_then(|at| rt.clock.deadline(at))?;

        // Evidence first, then history, with one scope; then input. History
        // refuses a scope whose sequence is not the message's last attempt.
        let scope = AttemptScope {
            message_id: id,
            attempt_seq: seq,
            recipient: agent.clone(),
            pane: pane.clone(),
            harness: recipient.harness,
            session_id,
            excluded_sessions: recipient.excluded.iter().cloned().collect(),
        };
        rt.evidence.arm(scope.clone())?;
        let armed = HistoryUpdate::Armed {
            scope: scope.clone(),
        };
        rt.history.record(armed, rt.clock.now()?)?;
        if rt.evidence.snapshot(id)?.receipt.is_some() {
            // An earlier attempt's prompt arrived meanwhile: no paste.
            return Ok(());
        }
        self.still_armed(&scope).await?;
        if rt.cancel.is_cancelled() {
            return Err(shutting_down());
        }
        if Instant::now() >= confirm_by {
            // The window ended before anything was typed (a slow commit or
            // fold). No paste: cleanup records that, and the attempt is
            // failed for its retry instead of leaving a draft staged.
            return Ok(());
        }
        self.set(|progress| progress.paste = EffectCertainty::Unknown)?;
        // Every decision about an input is this one function of one capture:
        // the actor's own for each input, a fresh one for each look between.
        let guard = |input| {
            let (baseline, expected) = (recipient.baseline.clone(), reference.clone());
            move |capture: &PaneCapture| {
                guard_allows_input(&guard_facts(input, capture, screen, &baseline, &expected))
            }
        };
        let text = reference.clone();
        let pasted = (r.paste_guarded(pane, agent.clone(), text, format, guard(paste))).await;
        // No await from the call's return to the record: work can be aborted
        // at any await, and the paste's outcome must not be lost with it.
        let landed = paste_certainty(&pasted);
        self.set(|progress| progress.paste = landed)?;
        let recorded = HistoryUpdate::Paste {
            id,
            pane: pane.clone(),
            certainty: landed,
        };
        rt.history.record(recorded, rt.clock.now()?)?;
        match pasted {
            Ok(()) => {}
            Err(error) if landed == EffectCertainty::NotStarted => {
                tracing::debug!(%id, %error, "Message paste was refused; nothing was sent");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }

        // The harness draws the paste after tmux has returned: look at the
        // editor, sending nothing, until the reference is read as the staged
        // draft and every guard fact allows. A frame here only says when to
        // try: the Enter's own guard decides again on the actor's capture.
        let enter = GuardedInput::Enter;
        loop {
            if Instant::now() >= confirm_by {
                // No key at the bound: cleanup looks for the token and holds.
                return Ok(());
            }
            let look_again = Instant::now() + settings.guard_poll_interval;
            if guard(enter)(&r.capture(pane, format).await?) {
                if rt.evidence.snapshot(id)?.receipt.is_some() {
                    // An earlier attempt's prompt arrived: no second one.
                    return Ok(());
                }
                self.still_armed(&scope).await?;
                if rt.cancel.is_cancelled() {
                    return Err(shutting_down());
                }
                // The window may have ended during this look. The Enter
                // still goes out: it is as guarded as any other, a receipt
                // that comes late is honoured, and a key refused here would
                // leave the reference staged for the owner to clear.
                let key = PaneKey::Enter;
                self.set(|progress| progress.enter = EffectCertainty::Unknown)?;
                let entered =
                    (r.keys_guarded(pane, agent.clone(), key, format, guard(enter))).await;
                // As for the paste: no await before the outcome is noted.
                let sent = paste_certainty(&entered);
                self.set(|progress| progress.enter = sent)?;
                // A refused screen check alone may be looked at again. Any
                // other return ends the keys, also one that names no effect:
                // an Enter that was or may have been sent is never repeated.
                if !matches!(entered, Err(ReservationError::ScreenChanged { .. })) {
                    self.audit(attempt, &entered);
                    entered?;
                    break;
                }
            }
            sleep_until(look_again.min(confirm_by)).await;
        }

        // Subscribed before the first look, so no receipt is missed between
        // a look and the wait. A wake is a hint; evidence retains the fact.
        let mut wake = rt.evidence.subscribe();
        loop {
            if rt.evidence.snapshot(id)?.receipt.is_some() {
                return Ok(());
            }
            tokio::select! {
                changed = wake.changed() => changed.map_err(|_| {
                    MessageError::Unavailable("message evidence stopped".into())
                })?,
                () = rt.cancel.cancelled() => return Err(shutting_down()),
                // Nothing is failed here: cleanup holds what Enter left.
                () = sleep_until(confirm_by) => return Ok(()),
            }
        }
    }

    /// One claimed recovery, never another paste/attempt or guarded-key retry.
    async fn recovery_work(
        &self,
        r: &Reservation,
        scope: &AttemptScope,
    ) -> Result<(), MessageError> {
        let rt = &self.rt;
        let snapshot = rt.history.snapshot(self.id, rt.clock.now()?)?;
        if snapshot.blocked_by.is_some() || r.agent() != &scope.recipient {
            return Err(MessageError::Conflict(
                "recovery is not the recipient's FIFO head".into(),
            ));
        }
        let number = snapshot
            .message
            .last_attempt
            .as_ref()
            .ok_or_else(|| MessageError::Conflict("recovery has no original attempt".into()))?
            .attempt;
        // Claim BEFORE the fresh guards. Holding evidence through registration
        // closes receive→history delay; every later veto revokes this handle.
        let claim = rt.evidence.claim_recovery(
            &rt.history,
            scope.clone(),
            rt.cancel.is_cancelled(),
            rt.clock.now()?,
        )?;
        self.set(|progress| {
            progress.attempt = AttemptCommit::Committed;
            progress.seq = Some(scope.attempt_seq);
            progress.number = number;
            progress.paste = EffectCertainty::Applied;
        })?;
        let recipient = rt
            .agents
            .recipient(&scope.recipient)
            .await
            .map_err(agent_error)?;
        self.still_armed(claim.scope()).await?;
        let screen = recipient.screen;
        let format = screen.capture_format();
        let reference = delivery_line(self.id, &snapshot.message.content);
        let (baseline, view) = (recipient.baseline, claim.guard());
        let allows = move |capture: &PaneCapture| {
            guard_allows_input(&guard_facts(
                GuardedInput::Enter,
                capture,
                screen,
                &baseline,
                &reference,
            )) && view.is_valid()
        };
        let first = tokio::select! {
            biased;
            () = rt.cancel.cancelled() => return Err(shutting_down()),
            captured = r.capture(&self.pane, format) => captured?,
        };
        if !allows(&first) {
            return Err(MessageError::Refused(
                "recovery draft or guard is not positively safe".into(),
            ));
        }
        self.still_armed(scope).await?;
        self.set(|progress| progress.enter = EffectCertainty::Unknown)?;
        let entered = tokio::select! {
            biased;
            () = rt.cancel.cancelled() => {
                self.audit(number, &Err(ReservationError::Unavailable));
                return Err(shutting_down());
            },
            entered = r.keys_guarded(&self.pane, scope.recipient.clone(), PaneKey::Enter, format, allows) => entered,
        };
        let sent = paste_certainty(&entered);
        self.set(|progress| progress.enter = sent)?;
        self.audit(number, &entered);
        entered?;
        // No fresh confirmation window. Drop invalidates and deregisters the
        // claim even if an actor predicate clone survives a timed-out call.
        Ok(())
    }

    /// Recovery never overwrites the original exit or releases its FIFO veto.
    async fn recovery_settle(&self, _r: &Reservation) -> Result<(), MessageError> {
        let rt = &self.rt;
        let limit = Instant::now() + ATTEMPT_CLEANUP_BUDGET - SETTLE_MARGIN;
        if let Some(receipt) = rt.evidence.receipt(self.id).ok().flatten() {
            let _ = self.delivered(&receipt, limit).await;
            return Ok(());
        }
        let progress = self.progress()?;
        let Some(seq) = progress.seq else {
            return Ok(());
        };
        let snapshot = rt.history.snapshot(self.id, rt.clock.now()?)?;
        // A proven no-Enter recovery adds no uncertainty or new notice reason.
        if snapshot.message.status == atm_org::DeliveryStatus::Delivered
            || (progress.enter == EffectCertainty::NotStarted && snapshot.hold.is_some())
        {
            return Ok(());
        }
        if self.hold(Some(seq), ExecutorHold::InputOutcomeUnknown) == Settled::Held {
            Ok(())
        } else {
            Err(MessageError::Internal(
                "recovery could not retain the original hold".into(),
            ))
        }
    }

    /// The attempt's reply was lost. The writer answers a snapshot after
    /// every command it accepted before, so after one, and the fold of what
    /// is committed, an attempt of this message past the cursor noted before
    /// the submission is this run's, and no such attempt means none exists.
    async fn fence(&self, mut progress: Progress, limit: Instant) -> Progress {
        let rt = &self.rt;
        let fenced = async {
            rt.org.snapshot().await?;
            rt.history.folded(rt.org.status()?).await?;
            rt.history.snapshot(self.id, rt.clock.now()?)
        };
        if let Ok(Ok(snapshot)) = timeout_at(limit, fenced).await {
            let seq = (snapshot.last_attempt_seq).filter(|seq| *seq > progress.before);
            progress.seq = seq;
            progress.attempt = match seq {
                Some(_) => AttemptCommit::Committed,
                None => AttemptCommit::Absent,
            };
        }
        progress
    }

    /// Execute a command and wait for the fold of what it committed.
    async fn write(&self, command: OrgCommand, limit: Instant) -> bool {
        let rt = &self.rt;
        let written = async {
            let committed = rt.org.execute(command, rt.clock.now()?).await?;
            match committed.last() {
                Some(last) => rt.history.folded(last.seq).await,
                None => Ok(()),
            }
        };
        matches!(timeout_at(limit, written).await, Ok(Ok(())))
    }

    fn hold(&self, seq: Option<u64>, hold: ExecutorHold) -> Settled {
        let rt = &self.rt;
        let held = seq
            .zip(rt.clock.now().ok())
            .is_some_and(|(attempt_seq, now)| {
                let held = HistoryUpdate::Held {
                    id: self.id,
                    attempt_seq,
                    hold,
                };
                rt.history.record(held, now).is_ok()
            });
        if held {
            Settled::Held
        } else {
            Settled::Neither
        }
    }

    /// Persist Delivered, wait for its fold, then let evidence forget the
    /// message. If any of that fails the receipt stays retained, and the
    /// recipient's job persists it later.
    async fn delivered(&self, receipt: &DeliveryReceipt, limit: Instant) -> Settled {
        let delivered = OrgCommand::Delivered {
            message_id: self.id,
            session_id: receipt.session_id.clone(),
            confirmation: receipt.confirmation.clone(),
        };
        if self.write(delivered, limit).await
            && self.rt.evidence.acknowledge(self.id, receipt).is_ok()
        {
            Settled::Written
        } else {
            Settled::Neither
        }
    }

    /// The attempt typed nothing, and history is told so. A final attempt is
    /// then left to the policy, which fails the message after the
    /// confirmation window and tells the owner once. Any other is failed
    /// here, for its retry. What cannot be recorded or written is held.
    async fn fail(&self, progress: Progress, limit: Instant) -> Settled {
        let rt = &self.rt;
        let untyped = HistoryUpdate::Paste {
            id: self.id,
            pane: self.pane.clone(),
            certainty: EffectCertainty::NotStarted,
        };
        let recorded = (rt.clock.now())
            .and_then(|now| rt.history.record(untyped, now))
            .is_ok();
        if recorded && rt.policy.is_final_attempt(progress.number) {
            return Settled::LeftToPolicy;
        }
        let failed = OrgCommand::DeliveryFailed {
            message_id: self.id,
            reason: NO_EFFECT.into(),
            final_failure: false,
        };
        if recorded && self.write(failed, limit).await {
            return Settled::Written;
        }
        self.hold(progress.seq, ExecutorHold::InputOutcomeUnknown)
    }

    /// After work has stopped, whatever stopped it: settle what it left.
    async fn settle(&self, r: &Reservation) -> Result<(), MessageError> {
        let (rt, id) = (&self.rt, self.id);
        let limit = Instant::now() + ATTEMPT_CLEANUP_BUDGET - SETTLE_MARGIN;
        let mut progress = self.progress()?;
        if progress.attempt == AttemptCommit::Unknown {
            progress = self.fence(progress, limit).await;
        }
        if let Some(seq) = progress.seq {
            let folded = timeout_at(limit, rt.history.folded(seq)).await;
            if !matches!(folded, Ok(Ok(()))) {
                // Committed, and history can hold nothing under it yet.
                progress.attempt = AttemptCommit::Unknown;
            }
        }
        let receipt = rt.evidence.receipt(id).ok().flatten();
        // The token is a hold's reason only: one capture, and no key.
        let token = match progress.format {
            Some(format) if receipt.is_none() && progress.paste != EffectCertainty::NotStarted => {
                (timeout_at(limit, r.capture(&self.pane, format)).await)
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|capture| token_visible(&capture, id))
            }
            _ => None,
        };
        let decided = disposition(&AttemptExitFacts {
            attempt: progress.attempt,
            receipt: receipt.is_some(),
            paste: progress.paste,
            token_visible: token,
            enter: progress.enter,
        });
        let settled = match (decided, &receipt) {
            (AttemptDisposition::Deliver, Some(receipt)) => self.delivered(receipt, limit).await,
            (AttemptDisposition::Fail, _) => self.fail(progress, limit).await,
            (AttemptDisposition::Hold(hold), _) => {
                // Work may have been stopped inside the paste call, before it
                // could record the paste. Recording one paste a second time
                // only resets the same pane clocks again.
                let pasted = HistoryUpdate::Paste {
                    id,
                    pane: self.pane.clone(),
                    certainty: progress.paste,
                };
                let _ = (rt.clock.now()).and_then(|now| rt.history.record(pasted, now));
                self.hold(progress.seq, hold)
            }
            _ => Settled::Neither,
        };
        // Work has stopped and no more input can change these certainties.
        // Delivered drops continuity; every other accounted exit retains facts
        // before the reservation is released. Failure never grants recovery.
        if decided != AttemptDisposition::Deliver {
            if let Some(update) = attempt_exit_update(id, progress, rt.cancel.is_cancelled()) {
                if let Err(error) = rt
                    .clock
                    .now()
                    .and_then(|now| rt.history.record(update, now))
                {
                    tracing::warn!(%id, %error, "Original input outcome not retained for recovery");
                }
            }
        }
        let _ = self.set(|progress| progress.settled = Some(settled));
        if cleanup_succeeds(decided, settled) {
            Ok(())
        } else {
            Err(MessageError::Internal(format!(
                "message {id}: the attempt could be neither settled nor held ({decided:?})"
            )))
        }
    }
}

/// Execute at most one attempt of the message, under one Delivery reservation
/// of its recipient and pane, as the holder of the recipient's slot.
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
/// - One exception: a final attempt that cleanup recorded as having typed
///   nothing is reported as Ok, whatever stopped its work. No retry can follow
///   it, so nothing needs a veto, and the policy fails the message.
///
/// Ok does not mean delivered: the policy asked for no paste, the attempt was
/// settled (delivered, failed for a retry, or held), or a final attempt typed
/// nothing and is left for the policy to fail.
///
/// Cancellation: A2 stops scheduling; B checks before admission and before each
/// effect, and while awaiting receipt. After commit, cancellation enters the same
/// effect-accounted cleanup, never drops a dispatched effect or releases a hold.
/// Cleanup uses its fresh capability/budget and is not cancelled by this token.
pub async fn deliver(
    rt: MessageRuntime,
    id: MessageId,
    deadline: Instant,
) -> Result<(), MessageError> {
    if rt.cancel.is_cancelled() {
        return Err(shutting_down());
    }
    let snapshot = rt.history.snapshot(id, rt.clock.now()?)?;
    let recovery = rt.history.recovery_candidate(id, rt.clock.now()?)?;
    let recovering = recovery.is_some();
    let recipient = snapshot.recipient;
    let Some(pane) = recipient.pane.as_deref() else {
        return Err(MessageError::Refused(format!(
            "agent {} has no pane",
            recipient.id
        )));
    };
    let pane = PaneId::parse(pane)?;
    rt.evidence
        .register(id, delivery_line(id, &snapshot.message.content))?;
    let request = ReservationRequest::new(
        recipient.id,
        Some(pane.clone()),
        ReservationKind::Delivery,
        WaitPolicy::Wait,
        deadline.min(Instant::now() + ATTEMPT_WORK_BUDGET),
    );
    let control = rt.control.clone();
    let progress = Progress {
        attempt: AttemptCommit::Absent,
        number: 0,
        before: 0,
        seq: None,
        paste: EffectCertainty::NotStarted,
        enter: EffectCertainty::NotStarted,
        format: None,
        settled: None,
    };
    let attempt = Attempt {
        rt,
        id,
        pane,
        progress: Arc::new(Mutex::new(progress)),
    };
    let (work, cleanup) = (attempt.clone(), attempt.clone());
    let ran = control
        .run_reserved(
            request,
            move |r| async move {
                match recovery {
                    Some(scope) => work.recovery_work(&r, &scope).await,
                    None => work.work(&r).await,
                }
            },
            move |r, _| async move {
                if recovering {
                    cleanup.recovery_settle(&r).await
                } else {
                    cleanup.settle(&r).await
                }
            },
        )
        .await;
    match attempt.progress() {
        // An error of work says nothing against what cleanup then proved.
        Ok(progress) if progress.settled == Some(Settled::LeftToPolicy) => Ok(()),
        _ => ran,
    }
}

#[cfg(test)]
#[path = "message_delivery_tests.rs"]
mod tests;
