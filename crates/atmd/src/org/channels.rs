//! Channel reads over the committed organisation fold.

use super::{
    agents::AgentError,
    message_inbox::{caller_address, caller_pane, INBOX_PAGE_BYTES},
    messages::{MessageError, MessageHandle, ORDINARY_BUDGET},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{CallContext, MethodError};
use atm_comms::channel_types::{
    ChannelHistoryParams, ChannelHistoryResult, ChannelKind, ChannelListParams, ChannelListResult,
    ChannelMember, ChannelMutationResult, ChannelRow, ChannelSection, ChannelSeenParams,
    ChannelSeenResult, PostDecision, PostDecisionAnswer, PostReaction, PostRouting, PostRow,
};
use atm_org::{
    channel_query::{
        channel_history, channels_for, is_lead_seat, ChannelPost, ChannelQueryError,
        ChannelSummary, HistoryQuery, HistoryRow, ListScope,
    },
    Address, ApplyError, ChannelCommand, ChannelId, ChannelRefusal, Decision, DecisionStatus,
    DecisionStep, Mention, MessageId, OrgCommand, OrgState, PostMutation, PostPresentation,
    Refusal, Routing,
};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

impl MessageHandle {
    pub(super) async fn mutate_post(
        &self,
        caller: CallContext,
        post: MessageId,
        change: PostMutation,
    ) -> Result<ChannelMutationResult, MethodError> {
        let pane = caller_pane(&caller)?.map(|pane| pane.to_string());
        let service = self.live()?;
        let command = OrgCommand::Channel(ChannelCommand::Mutate {
            post,
            actor: Address::Owner, // The actor binds this to the current pane attribution.
            change,
        });
        let committed = service
            .call(ORDINARY_BUDGET, service.execute_from(command, pane))
            .await
            .map_err(|error| match error {
                MessageError::Organisation(error) => org_error(error),
                error => error.into(),
            })?;
        let event = committed.first().ok_or_else(|| {
            MethodError::new(error_code::INTERNAL, "post mutation committed no event")
        })?;
        let post_id = serde_json::from_value(event.body.get("post").cloned().unwrap_or_default())
            .map_err(json_error)?;
        Ok(ChannelMutationResult {
            seq: event.seq,
            post_id,
        })
    }
}

pub(super) async fn list(
    org: &OrgHandle,
    caller: &CallContext,
    p: ChannelListParams,
) -> Result<ChannelListResult, MethodError> {
    let (state, seq) = org.snapshot_at().await.map_err(AgentError::from)?;
    let reader = caller_address(&state, caller)?;
    let scope = if p.all {
        ListScope::All
    } else {
        ListScope::Own
    };
    let result = ChannelListResult {
        seq,
        channels: channels_for(&state, &reader, scope)
            .into_iter()
            .filter_map(|summary| channel_row(&state, summary))
            .collect(),
    };
    if json_size(&result)? > INBOX_PAGE_BYTES {
        return Err(MethodError::new(
            error_code::REFUSED,
            "channel.list result exceeds the 512 KiB reply limit",
        ));
    }
    Ok(result)
}

fn channel_row(state: &OrgState, summary: ChannelSummary<'_>) -> Option<ChannelRow> {
    let (kind, section) = match &summary.id {
        ChannelId::Project(_) => (ChannelKind::Project, ChannelSection::Channel),
        ChannelId::Direct(_, Address::System) => (ChannelKind::Direct, ChannelSection::System),
        ChannelId::Direct(Address::Agent(agent), Address::Owner) => (
            ChannelKind::Direct,
            match agent.name.as_str() {
                "chief" => ChannelSection::Chief,
                _ if is_lead_seat(state, agent) => ChannelSection::Lead,
                _ => ChannelSection::Direct,
            },
        ),
        ChannelId::Direct(..) => (ChannelKind::Direct, ChannelSection::Direct),
        ChannelId::Other(_) => return None,
    };
    Some(ChannelRow {
        id: summary.id,
        kind,
        project: summary.project.cloned(),
        section,
        members: summary
            .members
            .into_iter()
            .map(|(address, state)| ChannelMember { address, state })
            .collect(),
        former: summary.former.into_iter().cloned().collect(),
        lead: summary.lead.cloned(),
        member: summary.member,
        // For an external caller posting to a listed channel, U3 requires membership.
        writable: summary.member,
        last_post: summary.last_post.map(|post| post.id()),
        last_activity: summary.last_post.map(|post| post.at()),
        activity_seq: summary.activity_seq,
        unread: summary.unread,
        seen_through: summary.seen_through,
    })
}

pub(super) async fn history(
    org: &OrgHandle,
    caller: &CallContext,
    p: ChannelHistoryParams,
) -> Result<ChannelHistoryResult, MethodError> {
    let (state, seq) = org.snapshot_at().await.map_err(AgentError::from)?;
    // The caller supplies the reader for reaction chips, not a history filter.
    let reader = caller_address(&state, caller)?;
    history_page(&state, seq, &reader, &p)
}

fn history_page(
    state: &OrgState,
    seq: u64,
    reader: &Address,
    p: &ChannelHistoryParams,
) -> Result<ChannelHistoryResult, MethodError> {
    if !p.channel.is_known() {
        return Err(unknown_channel(&p.channel));
    }
    let selected = channel_history(
        state,
        &p.channel,
        &HistoryQuery {
            thread: p.thread,
            include_shared_replies: p.include_shared_replies,
            before: p.before,
            after: p.after,
            limit: usize::from(p.limit),
        },
    )
    .map_err(query_error)?;
    let mut result = ChannelHistoryResult {
        seq,
        channel: p.channel.clone(),
        posts: Vec::new(),
        more: false,
    };
    // `false` is the longer encoding of `more`; the empty array already
    // contributes its brackets. Count actual escaped bytes and commas.
    let mut bytes = json_size(&result)?;
    result.more = selected.more;
    for row in selected.rows {
        let row = post_row(state, &p.channel, row, p.include_shared_replies, reader);
        let next_bytes = bytes + json_size(&row)? + usize::from(!result.posts.is_empty());
        if next_bytes > INBOX_PAGE_BYTES {
            if result.posts.is_empty() {
                return Err(MethodError::new(
                    error_code::REFUSED,
                    "first post exceeds the 512 KiB history result limit",
                ));
            }
            result.more = true;
            break;
        }
        bytes = next_bytes;
        result.posts.push(row);
    }
    Ok(result)
}

fn post_row(
    state: &OrgState,
    channel: &ChannelId,
    row: HistoryRow<'_>,
    expanded: bool,
    reader: &Address,
) -> PostRow {
    let HistoryRow {
        post,
        replies,
        last_reply_at,
        decision,
    } = row;
    match post {
        ChannelPost::Native(post) => {
            let content = &post.content;
            let presentation = state.post_presentation(post.id);
            let removed = matches!(presentation, Some(PostPresentation::Removed));
            PostRow {
                id: post.id,
                channel: channel.clone(),
                thread: content.thread,
                also_in_chat: expanded && content.thread.is_some() && content.also_in_chat,
                author: content.author.clone(),
                session_id: content.session_id.clone(),
                session_started: content
                    .session_id
                    .as_ref()
                    .and_then(|id| state.session_summary(id))
                    .map(|summary| summary.started),
                kind: content.kind,
                body: presentation
                    .map_or(content.body.as_str(), PostPresentation::body)
                    .to_owned(),
                edited: matches!(presentation, Some(PostPresentation::Edited(_))),
                removed,
                reactions: if removed {
                    Vec::new()
                } else {
                    reaction_chips(state, post.id, reader)
                },
                attachments: if removed {
                    Vec::new()
                } else {
                    content.attachments.clone()
                },
                task: if removed { None } else { content.task.clone() },
                mentions: if presentation.is_some() {
                    Vec::new()
                } else {
                    content
                        .mentions
                        .iter()
                        .filter(|m| !matches!(m, Mention::Unknown(_)))
                        .cloned()
                        .collect()
                },
                posted_at: post.posted_at,
                replies,
                last_reply_at,
                routing: match post.routing() {
                    Routing::None | Routing::Unknown => PostRouting::None,
                    Routing::Pending => PostRouting::Pending,
                    Routing::Router => PostRouting::Router,
                    Routing::Stopgap => PostRouting::Stopgap,
                },
                woke: post.woke().cloned().collect(),
                legacy: false,
                decision: content
                    .decision
                    .as_ref()
                    .zip(decision)
                    .map(|(step, decision)| post_decision(step, decision)),
            }
        }
        ChannelPost::Legacy(message) => PostRow {
            id: message.id,
            channel: channel.clone(),
            thread: None,
            also_in_chat: false,
            author: message.content.from.clone(),
            session_id: None,
            session_started: None,
            kind: message.content.kind,
            body: message.content.body.clone(),
            edited: false,
            removed: false,
            reactions: Vec::new(),
            attachments: message.content.attachments.clone(),
            task: message.content.task.clone(),
            mentions: Vec::new(),
            posted_at: message.sent_at,
            replies,
            last_reply_at,
            routing: PostRouting::None,
            woke: vec![message.content.to.clone()],
            legacy: true,
            decision: None,
        },
    }
}

fn reaction_chips(state: &OrgState, post: MessageId, reader: &Address) -> Vec<PostReaction> {
    let mut chips = BTreeMap::new();
    for (emoji, actor) in state.post_reactions(post).into_iter().flatten() {
        let chip = chips.entry(*emoji).or_insert(PostReaction {
            emoji: *emoji,
            count: 0,
            mine: false,
        });
        chip.count += 1;
        chip.mine |= actor == reader;
    }
    chips.into_values().collect()
}

fn post_decision(step: &DecisionStep, decision: &Decision) -> PostDecision {
    let answer =
        match decision.status {
            DecisionStatus::TakenProvisionally => {
                decision
                    .provisional
                    .as_ref()
                    .map(|answer| PostDecisionAnswer {
                        option: answer.option.clone(),
                        by: answer.by.clone(),
                        note: None,
                        relayed_by: answer.relayed_by.clone(),
                    })
            }
            DecisionStatus::Resolved | DecisionStatus::Overruled => decision
                .resolution
                .as_ref()
                .map(|answer| PostDecisionAnswer {
                    option: answer.option.clone(),
                    by: answer.by.clone(),
                    note: answer.note.clone(),
                    relayed_by: None,
                }),
            DecisionStatus::Pending | DecisionStatus::Expired | DecisionStatus::Withdrawn => None,
        };
    PostDecision {
        decision_id: step.decision_id,
        step: step.step.clone(),
        status: decision.status,
        options: decision.request.options.clone(),
        recommended: decision.request.recommended.clone(),
        deadline: decision.request.deadline,
        class: decision.class(),
        answer,
    }
}

pub(super) async fn seen(
    org: &OrgHandle,
    caller: &CallContext,
    p: ChannelSeenParams,
    now: DateTime<Utc>,
) -> Result<ChannelSeenResult, MethodError> {
    let reader = caller_address(&org.snapshot().await.map_err(AgentError::from)?, caller)?;
    if reader != Address::Owner {
        return Err(MethodError::new(
            error_code::REFUSED,
            "only the owner may mark a channel seen",
        ));
    }
    let committed = org
        .execute(
            OrgCommand::Channel(ChannelCommand::Seen {
                channel: p.channel.clone(),
                reader,
                through: p.through,
            }),
            now,
        )
        .await
        .map_err(org_error)?;
    let changed = committed.iter().any(|event| event.kind == "channel.seen");
    if let Some(last) = committed.last().filter(|_| changed) {
        return Ok(ChannelSeenResult {
            seq: last.seq,
            changed,
            through: p.through,
        });
    }

    // A repeat can still commit msg.read. A lower request must return the
    // effective marker, not its own bound. Read that marker and seq together.
    let (state, seq) = org.snapshot_at().await.map_err(AgentError::from)?;
    let through = channels_for(&state, &Address::Owner, ListScope::All)
        .into_iter()
        .find(|row| row.id == p.channel)
        .and_then(|row| row.seen_through)
        .ok_or_else(|| {
            MethodError::new(
                error_code::INTERNAL,
                "successful channel.seen has no marker",
            )
        })?;
    Ok(ChannelSeenResult {
        seq,
        changed,
        through,
    })
}

const NOT_IN_CHANNEL: &str = "not_in_channel";

pub(super) fn org_error(error: OrgServiceError) -> MethodError {
    match &error {
        OrgServiceError::Refused(Refusal::Channel(ChannelRefusal::UnknownChannel(channel))) => {
            unknown_channel(channel)
        }
        OrgServiceError::Refused(Refusal::Channel(ChannelRefusal::NotInChannel { .. })) => {
            MethodError::new(NOT_IN_CHANNEL, error.to_string())
        }
        OrgServiceError::Refused(Refusal::Channel(ChannelRefusal::InvalidEditBody(_))) => {
            MethodError::new(error_code::INVALID_PARAMS, error.to_string())
        }
        OrgServiceError::Refused(Refusal::Invariant(ApplyError::UnknownPost(_))) => {
            MethodError::new(error_code::NOT_FOUND, error.to_string())
        }
        _ => AgentError::Organisation(error).into(),
    }
}

fn query_error(error: ChannelQueryError) -> MethodError {
    let code = match error {
        ChannelQueryError::UnknownPost(_) => error_code::NOT_FOUND,
        ChannelQueryError::NotInChannel { .. } => NOT_IN_CHANNEL,
    };
    MethodError::new(code, error.to_string())
}

fn unknown_channel(channel: &ChannelId) -> MethodError {
    MethodError::new("unknown_channel", format!("channel {channel} is not known; use an id from channel.list (when posting to a person, use to)"))
}

fn json_size(value: &impl Serialize) -> Result<usize, MethodError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(json_error)
}

pub(super) fn json_error(error: serde_json::Error) -> MethodError {
    MethodError::new(error_code::INTERNAL, error.to_string())
}
