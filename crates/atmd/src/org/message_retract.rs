//! s3-5c: atm takes back its own reference after its message failed finally.
//!
//! A message can fail finally (an ack, the owner's give-up, the policy) while
//! its reference sits unsent in the recipient's editor: Enter was withheld.
//! Nothing owns that text afterwards, so it reads as a user draft and every
//! later delivery waits behind it. The scheduler asks this executor, at most
//! once per message, to clear it with the harness's retract key.
//!
//! Under one Delivery reservation it reads the message, the recipient and its
//! screen again. The control actor's own capture, just before the key, must
//! pass the delivery guard for Enter: an editor that holds exactly the
//! reference on one row (the adapter's staged match), an idle turn, no
//! dialog, nobody focused on the pane, and the pane's own tag and input
//! checks. One more capture then says whether the editor reads empty.
//! Nothing else is sent, and nothing is retried here.

use super::agent_screen::{AgentScreen, PaneBaseline};
use super::message_delivery::{guard_allows_input, guard_facts, paste_certainty, GuardedInput};
use super::message_history::EffectCertainty;
use super::messages::{MessageError, MessageRuntime};
use super::start_line::delivery_line;
use crate::control::reservation::{
    PaneCapture, PaneId, PaneKey, ReservationError, ReservationKind, ReservationRequest, WaitPolicy,
};
use atm_core::InputState;
use atm_org::{ControlAudit, DeliveryStatus, MessageId, OrgCommand};
use tokio::time::Instant;

/// What one retract did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Retracted {
    /// Nothing was sent: the fresh capture did not allow the key.
    Refused,
    /// The key was sent and the editor then read empty.
    Cleared,
    /// The key was or may have been sent, and the editor did not read empty.
    Left,
}

/// One guarded retract of message `id`'s reference, within `deadline`.
pub(super) async fn retract(
    rt: MessageRuntime,
    id: MessageId,
    deadline: Instant,
) -> Result<Retracted, MessageError> {
    let snapshot = rt.history.snapshot(id, rt.clock.now()?)?;
    let message = snapshot.message;
    let attempt = (message.last_attempt.as_ref())
        .ok_or_else(|| MessageError::Conflict(format!("message {id} has no attempt")))?;
    if message.status != DeliveryStatus::Failed {
        return Err(MessageError::Conflict(format!(
            "message {id} has not failed finally"
        )));
    }
    let agent = snapshot.recipient.id;
    let recipient = (rt.agents.recipient(&agent).await)
        .map_err(|error| MessageError::Unavailable(error.to_string()))?;
    if recipient.pane.as_str() != attempt.pane {
        return Err(MessageError::Conflict(format!(
            "message {id} was attempted on another pane"
        )));
    }
    let screen = recipient.screen;
    let key = (screen.retract())
        .ok_or_else(|| MessageError::Refused(format!("no retract key for {}", recipient.pane)))?;
    let (pane, baseline) = (recipient.pane, recipient.baseline);
    let reference = delivery_line(id, &message.content);
    let request = ReservationRequest::new(
        agent.clone(),
        Some(pane.clone()),
        ReservationKind::Delivery,
        WaitPolicy::Wait,
        deadline,
    );
    let control = rt.control.clone();
    control
        .run_reserved(
            request,
            move |r| async move {
                let format = screen.capture_format();
                let guard = {
                    let baseline = baseline.clone();
                    move |capture: &PaneCapture| allows(screen, &baseline, &reference, capture)
                };
                let sent = r.keys_guarded(&pane, agent, key, format, guard).await;
                audit(&rt, &pane, id, key, &sent);
                if paste_certainty(&sent) == EffectCertainty::NotStarted {
                    return Ok(Retracted::Refused);
                }
                let after = r.capture(&pane, format).await;
                Ok(match after {
                    Ok(capture) if screen.facts(&capture, &baseline).input == InputState::Empty => {
                        Retracted::Cleared
                    }
                    _ => Retracted::Left,
                })
            },
            |_, _| async { Ok::<(), MessageError>(()) },
        )
        .await
}

/// The delivery guard for Enter, on one capture: the same facts allow
/// atm's retract key, which also needs the exact staged reference.
fn allows(
    screen: &AgentScreen,
    baseline: &PaneBaseline,
    reference: &str,
    capture: &PaneCapture,
) -> bool {
    guard_allows_input(&guard_facts(
        GuardedInput::Enter,
        capture,
        screen,
        baseline,
        reference,
    ))
}

/// One `control.sent` record of the key, queued without waiting.
fn audit(
    rt: &MessageRuntime,
    pane: &PaneId,
    id: MessageId,
    key: PaneKey,
    sent: &Result<(), ReservationError>,
) {
    let Ok(now) = rt.clock.now() else {
        return;
    };
    let action = serde_json::json!({
        "method": "msg.retract",
        "message": id.to_string(),
        "op": "retract",
        "key": format!("{key:?}"),
        "outcome": if sent.is_ok() { "ok" } else { "error" },
        "error": sent.as_ref().err().map(ToString::to_string),
    });
    let audit = OrgCommand::ControlSent(ControlAudit {
        pane: pane.to_string(),
        action,
    });
    rt.control.audit_queue().submit(audit, now, None);
}
