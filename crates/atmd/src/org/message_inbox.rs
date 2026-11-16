//! Pure inbox projection over committed organisation state.
//!
//! No I/O, no clock and no writes: fetching an inbox never acknowledges reading.
//! The caller's identity is attribution, not authentication. The service owns
//! these state-dependent checks; the router owns the pure parameter bounds.

use super::message_types::{
    Address, DeliveryPhase, DeliveryStatus, DeliveryView, InboxEntry, InboxParams, InboxResult,
    MessageId,
};
use super::messages::MessageError;
use crate::{control::reservation::PaneId, server::methods::CallContext};
use atm_org::{Message, OrgState};
use chrono::{DateTime, Utc};

/// Bound on one serialized inbox page, entries and framing included. A body is
/// never truncated to fit: the page ends before the entry that would not.
pub(super) const INBOX_PAGE_BYTES: usize = 512 * 1024;
/// Room for the result's own fields around the entries.
const PAGE_FRAME_BYTES: usize = 128;

/// The agent bound to the caller's pane, else the owner. A Down agent holds no
/// pane in the applied state, so it is no sender. A malformed pane is an
/// error, never the owner.
pub(super) fn caller_address(
    state: &OrgState,
    caller: &CallContext,
) -> Result<Address, MessageError> {
    let Some(pane) = &caller.from_pane else {
        return Ok(Address::Owner);
    };
    let pane = PaneId::parse(pane.0.as_str())
        .map_err(|_| MessageError::InvalidParams("from_pane is not a pane id".into()))?;
    Ok(state
        .agent_for_pane(pane.as_str())
        .map_or(Address::Owner, |agent| Address::Agent(agent.id.clone())))
}

/// What the scheduler knows about an undelivered message beyond its durable
/// status. Ephemeral: it is presentation, never a stored lifecycle state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Progress {
    pub phase: Option<DeliveryPhase>,
    pub reason: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
    pub blocked_by: Option<MessageId>,
}

/// Attempt and receipt come from committed facts, never from the recipient's
/// current pane or session binding.
pub(super) fn delivery_view(message: &Message, progress: Progress) -> DeliveryView {
    let open = matches!(
        message.status,
        DeliveryStatus::Queued | DeliveryStatus::Delivering
    );
    let (phase, reason) = match message.status {
        DeliveryStatus::Delivered => (DeliveryPhase::Delivered, None),
        DeliveryStatus::Failed => (DeliveryPhase::Failed, message.failure.clone()),
        DeliveryStatus::Queued => (
            progress.phase.unwrap_or(DeliveryPhase::Queued),
            progress.reason,
        ),
        DeliveryStatus::Delivering => (
            progress.phase.unwrap_or(DeliveryPhase::Delivering),
            progress.reason,
        ),
    };
    DeliveryView {
        phase,
        reason,
        attempt: message.last_attempt.as_ref().map_or(0, |a| a.attempt),
        pane: message.last_attempt.as_ref().map(|a| a.pane.clone()),
        session_id: message.receipt.as_ref().map(|r| r.session_id.clone()),
        confirmation: message.receipt.as_ref().map(|r| r.confirmation.clone()),
        retry_at: progress.retry_at.filter(|_| open),
        blocked_by: progress.blocked_by.filter(|_| open),
    }
}

/// One page of `who`'s inbox as `caller` may see it, in sequence order.
///
/// The owner reads any inbox; an agent reads its own. `count_only` gives any
/// caller the owner's unread count and never a body. The unread count covers
/// every message addressed to the inbox, whatever the page or filters show.
pub(super) fn inbox(
    state: &OrgState,
    caller: &Address,
    p: &InboxParams,
    progress: impl Fn(&Message) -> Progress,
) -> Result<InboxResult, MessageError> {
    let who = p.recipient.clone().unwrap_or_else(|| caller.clone());
    match &who {
        Address::System => {
            return Err(MessageError::Refused(
                "system is not a message recipient or inbox".into(),
            ));
        }
        Address::Agent(id) if state.agent(id).is_none() => {
            return Err(MessageError::NotFound(format!("agent {id} was not found")));
        }
        _ => {}
    }
    let owner_count = who == Address::Owner && p.count_only;
    if *caller != Address::Owner && *caller != who && !owner_count {
        return Err(MessageError::Refused(format!(
            "only the owner may read the inbox of {who}"
        )));
    }
    let all = state.inbox(&who);
    let unread_count = all.iter().filter(|m| m.read_at.is_none()).count();
    let mut result = InboxResult {
        messages: Vec::new(),
        unread_count,
        next_after: None,
    };
    if p.count_only {
        return Ok(result);
    }
    let wanted: Vec<&Message> = match p.id {
        Some(id) => {
            let message = state
                .message(&id)
                .ok_or_else(|| MessageError::NotFound(format!("message {id} was not found")))?;
            if message.content.to != who {
                return Err(MessageError::Refused(format!(
                    "message {id} is not addressed to {who}"
                )));
            }
            vec![message]
        }
        None => all,
    };
    let mut bytes = PAGE_FRAME_BYTES;
    let mut more = false;
    for message in wanted.into_iter().filter(|m| {
        p.after.is_none_or(|after| m.id > after) && !(p.unread_only && m.read_at.is_some())
    }) {
        if result.messages.len() >= usize::from(p.limit) {
            more = true;
            break;
        }
        let entry = InboxEntry {
            message_id: message.id,
            content: message.content.clone(),
            sent_at: message.sent_at,
            status: message.status,
            read_at: message.read_at,
            delivery: delivery_view(message, progress(message)),
        };
        let size = serde_json::to_vec(&entry)
            .map_err(|error| MessageError::Internal(error.to_string()))?
            .len()
            + 1;
        if bytes + size > INBOX_PAGE_BYTES {
            if result.messages.is_empty() {
                return Err(MessageError::Refused(format!(
                    "message {} exceeds the {INBOX_PAGE_BYTES}-byte inbox page limit",
                    message.id
                )));
            }
            more = true;
            break;
        }
        bytes += size;
        result.messages.push(entry);
    }
    if more {
        result.next_after = result.messages.last().map(|entry| entry.message_id);
    }
    Ok(result)
}
