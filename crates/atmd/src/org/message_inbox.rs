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
use atm_org::{AgentId, Message, MessageContent, OrgState};
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

/// Shared full/metadata row projection; redact before serialized byte accounting.
pub(super) fn project_entry(message: &Message, managed: bool, progress: Progress) -> InboxEntry {
    let content = if managed {
        MessageContent {
            delivery_notice: None,
            from: message.content.from.clone(),
            to: message.content.to.clone(),
            kind: message.content.kind,
            body: String::new(),
            attachments: Vec::new(),
            task: None,
        }
    } else {
        message.content.clone()
    };
    let delivery = delivery_view(message, progress);
    let delivery = if managed {
        // Explicit fields make additions require a manager-visibility decision.
        DeliveryView {
            phase: delivery.phase,
            reason: None,
            attempt: delivery.attempt,
            pane: delivery.pane,
            session_id: delivery.session_id,
            confirmation: None,
            retry_at: delivery.retry_at,
            blocked_by: delivery.blocked_by,
        }
    } else {
        delivery
    };
    InboxEntry {
        message_id: message.id,
        content,
        sent_at: message.sent_at,
        status: message.status,
        read_at: message.read_at,
        delivery,
    }
}

/// One page of `who`'s inbox as `caller` may see it, in sequence order.
///
/// Owner/self read full content; a scoped manager grant exposes delivery metadata.
/// `count_only` gives any
/// caller the owner's unread count and never a body. The unread count covers
/// every message addressed to the inbox, whatever the page or filters show.
pub(super) fn inbox(
    state: &OrgState,
    caller: &Address,
    p: &InboxParams,
    managed_recipient: Option<&AgentId>,
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
    let managed = matches!((caller, &who, managed_recipient),
        (Address::Agent(_), Address::Agent(id), Some(grant)) if id == grant && caller != &who);
    if *caller != Address::Owner && *caller != who && !owner_count && !managed {
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
        let entry = project_entry(message, managed, progress(message));
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

#[cfg(test)]
mod tests {
    use super::*;
    use atm_org::{MessageContent, MessageKind, OrgEvent};
    use std::collections::BTreeMap;

    // Break caught: the shared projector drops full content or leaks any private free text.
    #[test]
    fn shared_projection_preserves_full_rows_and_allowlists_metadata() {
        use serde_json::json;
        let now = "2026-01-15T12:00:00Z".parse().unwrap();
        let message = Message {
            id: MessageId::new(42),
            content: MessageContent {
                delivery_notice: None,
                from: Address::Owner,
                to: "alpha/worker".parse().unwrap(),
                kind: MessageKind::Report,
                body: "PRIVATE body\n\u{001b}\"\\é".repeat(4096),
                attachments: vec!["PRIVATE attachment".into()],
                task: Some("PRIVATE task".parse().unwrap()),
            },
            sent_at: now,
            status: DeliveryStatus::Failed,
            read_at: Some(now),
            last_attempt: Some(atm_org::DeliveryAttempt {
                attempt: 2,
                pane: "%4".into(),
            }),
            last_attempt_at: Some(now),
            receipt: Some(atm_org::DeliveryReceipt {
                session_id: "recorded-session".into(),
                confirmation: "PRIVATE confirmation".into(),
            }),
            delivered_at: Some(now),
            failure: Some("PRIVATE failure".into()),
            failed_at: Some(now),
        };
        let progress = Progress {
            reason: Some("PRIVATE progress".into()),
            retry_at: Some(now),
            blocked_by: Some(MessageId::new(3)),
            ..Default::default()
        };
        let full = serde_json::to_value(project_entry(&message, false, progress.clone())).unwrap();
        assert_eq!(
            full,
            json!({
                "message_id":"m42", "sent_at":"2026-01-15T12:00:00Z",
                "status":"failed", "read_at":"2026-01-15T12:00:00Z",
                "content": {"from":"owner", "to":"alpha/worker", "kind":"report",
                    "body":"PRIVATE body\n\u{001b}\"\\é".repeat(4096),
                    "attachments":["PRIVATE attachment"], "task":"PRIVATE task"},
                "delivery":{"phase":"failed", "reason":"PRIVATE failure", "attempt":2,
                    "pane":"%4", "session_id":"recorded-session", "confirmation":"PRIVATE confirmation",
                    "retry_at":null, "blocked_by":null}
            })
        );
        let metadata = serde_json::to_value(project_entry(&message, true, progress)).unwrap();
        assert_eq!(
            metadata,
            json!({
                "message_id":"m42", "sent_at":"2026-01-15T12:00:00Z",
                "status":"failed", "read_at":"2026-01-15T12:00:00Z",
                "content":{"from":"owner", "to":"alpha/worker", "kind":"report",
                    "body":"", "attachments":[], "task":null},
                "delivery":{"phase":"failed", "reason":null, "attempt":2,
                    "pane":"%4", "session_id":"recorded-session", "confirmation":null,
                    "retry_at":null, "blocked_by":null}
            })
        );
        assert!(!metadata.to_string().contains("PRIVATE"));
    }

    // Break caught: cloning private fields, redacting after paging, or granting another target.
    #[test]
    fn metadata_projection_redacts_free_text_before_paging() {
        let mut state = OrgState::default();
        let now = "2026-01-15T12:00:00Z".parse().unwrap();
        let report: atm_org::AgentId = "alpha/worker".parse().unwrap();
        let mut seq = 0;
        let mut apply = |state: &mut OrgState, event| {
            seq += 1;
            state.apply(seq, now, &event).unwrap();
            MessageId::new(seq)
        };
        for project in ["alpha", "beta", "gamma"] {
            apply(
                &mut state,
                OrgEvent::ProjectRegistered {
                    project_id: project.parse().unwrap(),
                    root: format!("/{project}"),
                },
            );
        }
        for (id, pane) in [("alpha/worker", "%1"), ("alpha/lead", "%2")] {
            apply(
                &mut state,
                OrgEvent::AgentAdopted {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
            );
        }
        let phases = [
            DeliveryPhase::Queued,
            DeliveryPhase::Waiting,
            DeliveryPhase::Delivering,
            DeliveryPhase::Blocked,
            DeliveryPhase::Delivered,
            DeliveryPhase::Failed,
        ];
        let mut facts = BTreeMap::new();
        for phase in phases {
            let id = apply(
                &mut state,
                OrgEvent::MessageSent(MessageContent {
                    delivery_notice: None,
                    from: Address::Owner,
                    to: Address::Agent(report.clone()),
                    kind: MessageKind::Report,
                    body: format!("SECRET body{}", "x".repeat(INBOX_PAGE_BYTES)),
                    attachments: vec![format!("SECRET path{}", "y".repeat(300_000))],
                    task: Some("SECRET task".parse().unwrap()),
                }),
            );
            if matches!(
                phase,
                DeliveryPhase::Delivering | DeliveryPhase::Delivered | DeliveryPhase::Failed
            ) {
                apply(
                    &mut state,
                    OrgEvent::MessageDeliveryAttempted {
                        message_id: id,
                        attempt: 2,
                        pane: "%1".into(),
                    },
                );
            }
            if phase == DeliveryPhase::Delivered {
                apply(
                    &mut state,
                    OrgEvent::MessageDelivered {
                        message_id: id,
                        session_id: "recorded-session".into(),
                        confirmation: "SECRET receipt".into(),
                    },
                );
                apply(
                    &mut state,
                    OrgEvent::MessageRead {
                        message_ids: vec![id],
                    },
                );
            }
            if phase == DeliveryPhase::Failed {
                apply(
                    &mut state,
                    OrgEvent::MessageDeliveryFailed {
                        message_id: id,
                        reason: "SECRET failure".into(),
                        final_failure: true,
                    },
                );
            }
            facts.insert(id, phase);
        }
        // New binding must never replace the committed attempt's pane/receipt.
        apply(
            &mut state,
            OrgEvent::AgentAdopted {
                agent_id: report.clone(),
                pane: "%99".into(),
                session_id: Some("new-session".into()),
            },
        );
        let before = state.clone();
        let progress = |message: &Message| Progress {
            phase: facts.get(&message.id).copied(),
            reason: Some("SECRET progress".into()),
            retry_at: Some(now),
            blocked_by: Some(MessageId::new(999)),
        };
        let params = InboxParams {
            recipient: Some(Address::Agent(report.clone())),
            unread_only: false,
            count_only: false,
            id: None,
            after: None,
            limit: 100,
        };
        let caller = "alpha/lead".parse().unwrap();
        let page = inbox(&state, &caller, &params, Some(&report), progress).unwrap();
        assert_eq!(
            page.messages.len(),
            6,
            "hidden sizes must not choose page boundaries"
        );
        assert_eq!(page.unread_count, 5);
        assert_eq!(page.next_after, None);
        assert!(!serde_json::to_string(&page).unwrap().contains("SECRET"));
        for row in &page.messages {
            let phase = facts[&row.message_id];
            assert_eq!(row.delivery.phase, phase);
            assert_eq!(
                row.status,
                match phase {
                    DeliveryPhase::Delivered => DeliveryStatus::Delivered,
                    DeliveryPhase::Failed => DeliveryStatus::Failed,
                    DeliveryPhase::Delivering => DeliveryStatus::Delivering,
                    _ => DeliveryStatus::Queued,
                }
            );
            assert_eq!(row.sent_at, now);
            assert_eq!(row.content.from, Address::Owner);
            assert_eq!(row.content.to, Address::Agent(report.clone()));
            assert_eq!(row.content.kind, MessageKind::Report);
            assert!(row.content.body.is_empty());
            assert!(row.content.attachments.is_empty());
            assert_eq!(row.content.task, None);
            assert_eq!(row.delivery.reason, None);
            assert_eq!(row.delivery.confirmation, None);
            let attempted = matches!(
                phase,
                DeliveryPhase::Delivering | DeliveryPhase::Delivered | DeliveryPhase::Failed
            );
            assert_eq!(row.delivery.attempt, if attempted { 2 } else { 0 });
            assert_eq!(row.delivery.pane.as_deref(), attempted.then_some("%1"));
            assert_eq!(
                row.delivery.session_id.as_ref().map(|id| id.as_str()),
                (phase == DeliveryPhase::Delivered).then_some("recorded-session")
            );
            assert_eq!(
                row.read_at,
                (phase == DeliveryPhase::Delivered).then_some(now)
            );
            let terminal = matches!(phase, DeliveryPhase::Delivered | DeliveryPhase::Failed);
            assert_eq!(row.delivery.retry_at, (!terminal).then_some(now));
            assert_eq!(
                row.delivery.blocked_by,
                (!terminal).then_some(MessageId::new(999))
            );
        }
        let unread = inbox(
            &state,
            &caller,
            &InboxParams {
                unread_only: true,
                ..params.clone()
            },
            Some(&report),
            progress,
        )
        .unwrap();
        assert_eq!(unread.messages.len(), 5);
        assert_eq!(unread.unread_count, 5);
        assert!(unread.messages.iter().all(|row| row.read_at.is_none()));
        let single = inbox(
            &state,
            &caller,
            &InboxParams {
                id: Some(page.messages[4].message_id),
                ..params.clone()
            },
            Some(&report),
            progress,
        )
        .unwrap();
        assert_eq!(single.messages.len(), 1);
        assert!(single.messages[0].read_at.is_some());
        assert!(!serde_json::to_string(&single).unwrap().contains("SECRET"));
        let different = "beta/worker".parse().unwrap();
        assert!(matches!(
            inbox(&state, &caller, &params, Some(&different), progress),
            Err(MessageError::Refused(_))
        ));
        assert!(
            matches!(
                inbox(&state, &Address::Owner, &params, None, progress),
                Err(MessageError::Refused(_))
            ),
            "owner still observes the original oversized body"
        );
        assert_eq!(state, before);
    }
}
