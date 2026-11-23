//! The start line: after a start, respawn or clear of a named agent ends
//! Ready, the daemon queues one note from `system` to the agent, and the
//! message runtime types it as `[mN] New session: run the shell command atm me`,
//! or, after a saved handover, as the resume instruction. Both type the note
//! itself through the usual guards (an idle session, an empty editor, no dialog, not
//! the owner's focused pane), its exact confirmation and its holds.
//!
//! Only the lifecycle flows queue it, inside the agent's reservation, so
//! two checks for one agent never interleave. Clear queues it after the
//! Ready proof but before the Ready write, so it heads delivery in the
//! replacement editor. Start and respawn still queue it after Ready.
//! This allows at most one open start line per agent and none
//! for a session that has already received one. Adopt, the restart check and
//! the observer's binds queue nothing. `ATM_SPAWN_START_LINE=off` in the
//! environment atmd was started in turns it off.
//!
//! Known limits:
//! - A start or clear without Ready proof queues no line, and neither does
//!   the observer's later recovery of that agent to Ready. An interruption
//!   after a clear queues its line can still leave the clear Stuck.
//! - A clear's open start line precedes ordinary delivery until it is
//!   delivered or finally failed. Start and respawn keep their usual order.
//! - A crash before the line's write, or a failed write (logged), can lose
//!   the line. An open line held for the owner blocks a new one until he
//!   retries or gives up, and giving up queues no replacement.
//! - Nobody acknowledges a start line: it stays unread in `atm inbox`. `atm me`
//!   does not list it.
//! - The Claude and pi matchers refuse wrapped staged drafts: no Enter or automatic
//!   confirmation; a pasted line stays in the editor for inspection. For `m123`
//!   the plain line is 48 columns and the resume line 77. The latter is
//!   `[m123] Run atm me; read its memory file; resume open work or report and stop.`
//!   (70 columns before the ID). Each extra ID digit needs another column.
//!   A saved clear queues the plain line instead if the pane width is unknown
//!   or below the estimated resume reference length (including one spare ID
//!   digit) plus three cells for editor chrome. A resize can still cause a hold.
//! - An agent that goes down or stuck before its line is delivered (a respawn
//!   records it down for a moment, so it can too) leaves the line blocked: the
//!   owner gets a "blocked: recipient agent is stuck or down" notice, and his
//!   `[notify]` command runs. The line resumes once the agent is Ready again.

use super::agents::AgentRuntime;
use super::message_delivery::inbox_reference;
use atm_core::SessionId;
use atm_org::{
    Address, AgentId, DeliveryStatus, MessageContent, MessageId, MessageKind, OrgCommand, OrgState,
};

/// The note's body, and the typed line after its `[mN]` token.
const START_LINE: &str = "New session: run the shell command atm me";
const RESUME_LINE: &str = "Run atm me; read its memory file; resume open work or report and stop.";

pub(super) fn resume_fits(columns: usize, next: MessageId) -> bool {
    // The reference is ASCII. Reserve a digit for intervening journal writes
    // and three cells for chrome: Claude/Codex have two-cell prompts, and
    // pi 1.5.2's default editor reserves a cursor cell.
    columns >= resume_reference(next).len() + 1 + 3
}

/// The `ATM_SPAWN_*` variable that turns start lines off when it is `off`.
const SWITCH: &str = "ATM_SPAWN_START_LINE";

/// The canonical start line for `agent`.
#[must_use]
pub fn start_line(agent: &AgentId) -> MessageContent {
    MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: Address::Agent(agent.clone()),
        kind: MessageKind::Note,
        body: START_LINE.into(),
        attachments: Vec::new(),
        task: None,
        post: None,
    }
}

/// The copy to type for a complete canonical note, including historical
/// plain notes. Nobody but the daemon sends as `system`.
fn start_copy(content: &MessageContent) -> Option<&'static str> {
    let MessageContent {
        delivery_notice: None,
        from: Address::System,
        to: Address::Agent(_),
        kind: MessageKind::Note,
        body,
        attachments,
        task: None,
        // A wake delivers a channel post, never a start line, whatever its body.
        post: None,
    } = content
    else {
        return None;
    };
    if !attachments.is_empty() {
        return None;
    }
    match body.as_str() {
        START_LINE | "New session: run atm me" => Some(START_LINE),
        RESUME_LINE => Some(RESUME_LINE),
        _ => None,
    }
}

#[must_use]
pub fn is_start_line(content: &MessageContent) -> bool {
    start_copy(content).is_some()
}

/// The current line typed for a plain start, including historical notes.
#[must_use]
pub fn start_reference(id: MessageId) -> String {
    format!("[{id}] {START_LINE}")
}

/// The one line typed for message `id`: the start line's own text, or the
/// inbox reference of any other message.
#[must_use]
pub fn delivery_line(id: MessageId, content: &MessageContent) -> String {
    match start_copy(content) {
        Some(RESUME_LINE) => resume_reference(id),
        Some(_) => start_reference(id),
        None => inbox_reference(id),
    }
}

pub(super) fn resume_reference(id: MessageId) -> String {
    format!("[{id}] {RESUME_LINE}")
}

/// Whether `agent`, Ready on `session` after a start or clear, gets a start
/// line: not while one to it is queued or delivering (that one serves the
/// new session), and not if one was delivered to this same session.
fn existing_start_line(
    state: &OrgState,
    agent: &AgentId,
    session: Option<&SessionId>,
) -> Option<(MessageId, &'static str)> {
    state
        .inbox(&Address::Agent(agent.clone()))
        .into_iter()
        .filter_map(|message| Some((message, start_copy(&message.content)?)))
        .find(|(message, _)| match message.status {
            DeliveryStatus::Queued | DeliveryStatus::Delivering => true,
            DeliveryStatus::Delivered => {
                session.is_some()
                    && message.receipt.as_ref().map(|receipt| &receipt.session_id) == session
            }
            DeliveryStatus::Failed => false,
        })
        .map(|(message, copy)| (message.id, copy))
}

#[cfg(test)]
pub(super) fn start_line_due(
    state: &OrgState,
    agent: &AgentId,
    session: Option<&SessionId>,
) -> bool {
    existing_start_line(state, agent, session).is_none()
}

#[derive(Debug, Clone, Copy)]
pub(super) enum StartLineDisposition {
    Track(MessageId),
    PlainFallback(MessageId),
    Off,
    Unidentified,
}

impl AgentRuntime {
    /// Queue `id`'s start line once its start, respawn or clear proved
    /// Ready on `session` (`None`: an editor without a session). Call only
    /// under the agent's reservation. The call logs a failure and returns
    /// Unidentified without vetoing the caller's Ready. `resume_cols` is set
    /// only for a saved handover; zero means the width is unknown.
    pub(super) async fn queue_start_line(
        &self,
        id: &AgentId,
        session: Option<&SessionId>,
        resume_cols: Option<usize>,
    ) -> StartLineDisposition {
        use StartLineDisposition::{Off, PlainFallback, Track, Unidentified};
        if (self.spawn_env)(SWITCH).as_deref() == Some("off") {
            return Off;
        }
        let queued = async {
            let (state, seq) = self.org.snapshot_at().await?;
            if let Some((reused, copy)) = existing_start_line(&state, id, session) {
                // Keep one open slot; a carried plain note is not a resume.
                return Ok((resume_cols.is_none() || copy == RESUME_LINE).then_some(Track(reused)));
            }
            let mut line = start_line(id);
            let fits = resume_cols.is_some_and(|cols| {
                seq.checked_add(1)
                    .is_some_and(|next| resume_fits(cols, MessageId::new(next)))
            });
            if fits {
                line.body = RESUME_LINE.into();
            }
            let disposition: fn(MessageId) -> _ = if resume_cols.is_some() && !fits {
                PlainFallback
            } else {
                Track
            };
            let sent = self
                .org
                .execute(OrgCommand::SendMessage(line), chrono::Utc::now())
                .await?;
            Ok::<_, super::OrgServiceError>(
                sent.first()
                    .map(|event| disposition(MessageId::new(event.seq))),
            )
        };
        match queued.await {
            Ok(Some(disposition)) => disposition,
            Ok(None) => Unidentified,
            Err(error) => {
                tracing::warn!(agent = %id, %error, "Start line could not be identified");
                Unidentified
            }
        }
    }
}
