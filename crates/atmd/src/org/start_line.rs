//! The start line: after a start, respawn or clear of a named agent ends
//! Ready, the daemon queues one note from `system` to the agent, and the
//! message runtime types it as `[mN] New session: run atm me`, the note's
//! text itself rather than an inbox reference. It goes through the runtime
//! unchanged: its guards (an idle session, an empty editor, no dialog, not
//! the owner's focused pane), its exact confirmation and its holds.
//!
//! Only the lifecycle flows queue it, after their own Ready write and inside
//! the agent's reservation, so two checks for one agent never interleave.
//! [`start_line_due`] allows at most one open start line per agent and none
//! for a session that has already received one. Adopt, the restart check and
//! the observer's binds queue nothing. `ATM_SPAWN_START_LINE=off` in the
//! environment atmd was started in turns it off.
//!
//! Known limits:
//! - A start or clear that ends Stuck queues no line, and neither does the
//!   observer's later recovery of that agent to Ready.
//! - Messages to one agent are typed oldest first: one queued before the line
//!   is typed before it.
//! - A crash between the Ready write and the line's write, or a failed write
//!   (logged), loses the line. An open line held for the owner blocks a new
//!   one until he retries or gives up, and giving up queues no replacement.
//! - Nobody acknowledges a start line: it stays unread in `atm inbox`. `atm me`
//!   does not list it.
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
const START_LINE: &str = "New session: run atm me";

/// The `ATM_SPAWN_*` variable that turns start lines off when it is `off`.
const SWITCH: &str = "ATM_SPAWN_START_LINE";

/// The canonical start line for `agent`.
#[must_use]
pub fn start_line(agent: &AgentId) -> MessageContent {
    MessageContent {
        from: Address::System,
        to: Address::Agent(agent.clone()),
        kind: MessageKind::Note,
        body: START_LINE.into(),
        attachments: Vec::new(),
        task: None,
    }
}

/// Only the complete canonical start line, never a lookalike. Nobody but the
/// daemon sends as `system`.
#[must_use]
pub fn is_start_line(content: &MessageContent) -> bool {
    matches!(&content.to, Address::Agent(agent) if content == &start_line(agent))
}

/// The line typed for start line `id`: never longer than its inbox reference.
#[must_use]
pub fn start_reference(id: MessageId) -> String {
    format!("[{id}] {START_LINE}")
}

/// The one line typed for message `id`: the start line's own text, or the
/// inbox reference of any other message.
#[must_use]
pub fn delivery_line(id: MessageId, content: &MessageContent) -> String {
    if is_start_line(content) {
        start_reference(id)
    } else {
        inbox_reference(id)
    }
}

/// Whether `agent`, Ready on `session` after a start or clear, gets a start
/// line: not while one to it is queued or delivering (that one serves the
/// new session), and not if one was delivered to this same session.
#[must_use]
pub(super) fn start_line_due(
    state: &OrgState,
    agent: &AgentId,
    session: Option<&SessionId>,
) -> bool {
    !state
        .inbox(&Address::Agent(agent.clone()))
        .into_iter()
        .filter(|message| is_start_line(&message.content))
        .any(|message| match message.status {
            DeliveryStatus::Queued | DeliveryStatus::Delivering => true,
            DeliveryStatus::Delivered => {
                session.is_some()
                    && message.receipt.as_ref().map(|receipt| &receipt.session_id) == session
            }
            DeliveryStatus::Failed => false,
        })
}

impl AgentRuntime {
    /// Queue `id`'s start line, after its start, respawn or clear has
    /// recorded Ready on `session` (`None`: an editor without a session).
    /// Call it only under the agent's reservation. A failure is logged and
    /// never fails the operation: the agent is Ready either way.
    pub(super) async fn queue_start_line(&self, id: &AgentId, session: Option<&SessionId>) {
        if (self.spawn_env)(SWITCH).as_deref() == Some("off") {
            return;
        }
        let queued = async {
            let state = self.org.snapshot().await?;
            if start_line_due(&state, id, session) {
                let line = OrgCommand::SendMessage(start_line(id));
                self.org.execute(line, chrono::Utc::now()).await?;
            }
            Ok::<_, super::OrgServiceError>(())
        };
        if let Err(error) = queued.await {
            tracing::warn!(agent = %id, %error, "Start line not queued; the agent is ready without it");
        }
    }
}
