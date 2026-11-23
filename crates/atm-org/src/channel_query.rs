//! Pure reads of the channel fold: no I/O, no clock.
//!
//! Ids resolve both ways: a wake row's id resolves to its post, and a legacy
//! message (a `msg.sent` without `post`, between two different addresses) is
//! its own post and its recipient's row.
//!
//! Membership is derived, never stored. A project channel is listed once the
//! project has a seat named `lead` (or holds posts); its members are the owner
//! and every seat of the project that is not down. A direct channel is listed
//! from its first post; its members are its two addresses. Everyone may read
//! every channel; who may post is the command policy's business.
use crate::{
    channel::ChannelState, Address, Agent, AgentId, AgentStatus, ChannelId, Decision, DecisionId,
    Message, MessageContent, MessageId, OrgState, Post, ProjectId,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Unbounded};
use thiserror::Error;

/// A native post by id.
#[must_use]
pub fn post(state: &OrgState, id: MessageId) -> Option<&Post> {
    state.channel.posts.get(&id)
}

/// The post an id stands for: a post id is itself, a wake row's id is its
/// post, a legacy message is itself. None for anything else.
#[must_use]
pub fn resolve_post(state: &OrgState, id: MessageId) -> Option<MessageId> {
    if state.channel.posts.contains_key(&id) {
        return Some(id);
    }
    let message = state.message(&id)?;
    match message.content.post {
        Some(post) => Some(post),
        None => (message.content.from != message.content.to).then_some(id),
    }
}

/// The root of the thread an id belongs to: its post, or that post's root
/// when it is a reply.
#[must_use]
pub fn thread_root(state: &OrgState, id: MessageId) -> Option<MessageId> {
    let id = resolve_post(state, id)?;
    Some(post(state, id).and_then(|p| p.content.thread).unwrap_or(id))
}

/// The reader's own row for a post: its wake, or for a legacy message
/// addressed to the reader, the message itself.
#[must_use]
pub fn wake_for(state: &OrgState, post: MessageId, reader: &Address) -> Option<MessageId> {
    if state.channel.posts.contains_key(&post) {
        return state.channel.wakes.get(&(post, reader.clone())).copied();
    }
    let message = state.message(&post)?;
    let legacy = message.content.post.is_none() && message.content.from != message.content.to;
    (legacy && &message.content.to == reader).then_some(post)
}

/// One row per wake that is due but not written: the repair list. Each copies
/// its post's content and names the post.
#[must_use]
pub fn missing_wakes(state: &OrgState) -> Vec<MessageContent> {
    state
        .channel
        .unwoken
        .iter()
        .filter_map(|(id, to)| {
            let post = &state.channel.posts.get(id)?.content;
            Some(MessageContent {
                from: post.author.clone(),
                to: to.clone(),
                kind: post.kind,
                body: post.body.clone(),
                attachments: post.attachments.clone(),
                task: post.task.clone(),
                delivery_notice: None,
                post: Some(*id),
            })
        })
        .collect()
}

/// Posts waiting for a routing answer, with their post time, in id order.
pub fn pending_routes(state: &OrgState) -> impl Iterator<Item = (MessageId, DateTime<Utc>)> + '_ {
    state.channel.unrouted.iter().map(|(id, at)| (*id, *at))
}

/// The post showing a decision's step, if one was posted.
#[must_use]
pub fn decision_post(state: &OrgState, decision: DecisionId, step: &str) -> Option<MessageId> {
    state
        .channel
        .decision_posts
        .get(&(decision, step.to_owned()))
        .copied()
}

/// The post an author made with a client key, if any.
#[must_use]
pub fn client_key_post(state: &OrgState, author: &Address, key: &str) -> Option<MessageId> {
    state
        .channel
        .client_keys
        .get(&(author.clone(), key.to_owned()))
        .copied()
}

/// A post as a channel shows it: a native post, or a legacy message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPost<'a> {
    Native(&'a Post),
    Legacy(&'a Message),
}
impl<'a> ChannelPost<'a> {
    #[must_use]
    pub fn id(&self) -> MessageId {
        match self {
            Self::Native(post) => post.id,
            Self::Legacy(message) => message.id,
        }
    }
    #[must_use]
    pub fn author(&self) -> &'a Address {
        match self {
            Self::Native(post) => &post.content.author,
            Self::Legacy(message) => &message.content.from,
        }
    }
    #[must_use]
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Native(post) => post.posted_at,
            Self::Legacy(message) => message.sent_at,
        }
    }
}
fn channel_post(state: &OrgState, id: MessageId) -> Option<ChannelPost<'_>> {
    post(state, id)
        .map(ChannelPost::Native)
        .or_else(|| state.message(&id).map(ChannelPost::Legacy))
}

/// One page of a channel. Without `thread`: top-level posts (and, on opt-in,
/// shared thread replies), newest first.
/// With `thread` (any id of the thread: its root, a reply, or a wake of
/// either): the root, then its replies, oldest first. `after` and `before`
/// bound the page, both exclusive. The daemon checks `limit`.
///
/// A top-level page with only `after` holds the newest `limit` posts after
/// the bound, not the oldest: `more` then means a gap between `after` and the
/// page, which the caller fills by paging with `before` (and the same `after`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    pub thread: Option<MessageId>,
    pub include_shared_replies: bool,
    pub before: Option<MessageId>,
    pub after: Option<MessageId>,
    pub limit: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow<'a> {
    pub post: ChannelPost<'a>,
    pub replies: usize,
    pub last_reply_at: Option<DateTime<Utc>>,
    /// The decision a native post shows a step of.
    pub decision: Option<&'a Decision>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage<'a> {
    pub rows: Vec<HistoryRow<'a>>,
    /// More posts lie in the same window beyond this page.
    pub more: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChannelQueryError {
    #[error("unknown post {0}")]
    UnknownPost(MessageId),
    #[error("thread {post} is not in channel {channel}")]
    NotInChannel { post: MessageId, channel: ChannelId },
}

fn window(
    ids: &BTreeSet<MessageId>,
    after: Option<MessageId>,
    before: Option<MessageId>,
) -> impl DoubleEndedIterator<Item = &MessageId> {
    // BTreeSet::range panics on an empty or inverted pair of bounds.
    let empty = matches!((after, before), (Some(a), Some(b)) if a >= b);
    let lower = after.map_or(Unbounded, Excluded);
    let upper = before.map_or(Unbounded, Excluded);
    (!empty)
        .then(|| ids.range((lower, upper)))
        .into_iter()
        .flatten()
}

/// A page of `channel`'s history. A channel with no posts yet has an empty
/// page.
pub fn channel_history<'a>(
    state: &'a OrgState,
    channel: &ChannelId,
    query: &HistoryQuery,
) -> Result<HistoryPage<'a>, ChannelQueryError> {
    let index = state.channel.channels.get(channel);
    let take = query.limit.saturating_add(1);
    let mut ids: Vec<MessageId> = match query.thread {
        None => index.map_or_else(Vec::new, |c| {
            let ids = if query.include_shared_replies {
                &c.timeline
            } else {
                &c.top_level
            };
            window(ids, query.after, query.before)
                .rev()
                .take(take)
                .copied()
                .collect()
        }),
        Some(id) => {
            let root = thread_root(state, id).ok_or(ChannelQueryError::UnknownPost(id))?;
            let Some(index) = index.filter(|c| c.top_level.contains(&root)) else {
                return Err(ChannelQueryError::NotInChannel {
                    post: root,
                    channel: channel.clone(),
                });
            };
            let replies = index.threads.get(&root).map_or(&[][..], Vec::as_slice);
            std::iter::once(&root)
                .chain(replies)
                .filter(|id| query.after.is_none_or(|a| **id > a))
                .filter(|id| query.before.is_none_or(|b| **id < b))
                .take(take)
                .copied()
                .collect()
        }
    };
    let more = ids.len() > query.limit;
    ids.truncate(query.limit);
    let rows = ids
        .into_iter()
        .filter_map(|id| {
            let replies = index.and_then(|c| c.threads.get(&id));
            let decision = post(state, id)
                .and_then(|p| p.content.decision.as_ref())
                .and_then(|step| state.decision(&step.decision_id));
            Some(HistoryRow {
                post: channel_post(state, id)?,
                replies: replies.map_or(0, Vec::len),
                last_reply_at: replies
                    .and_then(|r| r.last())
                    .and_then(|last| channel_post(state, *last))
                    .map(|p| p.at()),
                decision,
            })
        })
        .collect();
    Ok(HistoryPage { rows, more })
}

/// The project's lead: the seat named `lead`, if its record exists, whatever
/// its status. The one place this rule lives.
#[must_use]
pub fn project_lead<'a>(state: &'a OrgState, project: &ProjectId) -> Option<&'a Agent> {
    AgentId::new(project.clone(), "lead")
        .ok()
        .and_then(|id| state.agent(&id))
}

/// Who belongs to a channel: in a project channel the owner and every seat of
/// the project that is not down, in a direct channel its two addresses.
#[must_use]
pub fn channel_members(state: &OrgState, channel: &ChannelId) -> Vec<Address> {
    match channel {
        ChannelId::Project(project) => std::iter::once(Address::Owner)
            .chain(
                seats(state, project)
                    .filter(|a| a.status != AgentStatus::Down)
                    .map(|a| Address::Agent(a.id.clone())),
            )
            .collect(),
        ChannelId::Direct(a, b) => vec![a.clone(), b.clone()],
        ChannelId::Other(_) => vec![],
    }
}
fn seats<'a>(state: &'a OrgState, project: &'a ProjectId) -> impl Iterator<Item = &'a Agent> {
    state.agents().filter(move |a| &a.id.project == project)
}

/// Which channels a list shows: the reader's own, or all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListScope {
    /// A seat's member and party channels; for the owner, every channel but
    /// the system channels of seats.
    Own,
    All,
}

/// One listed channel, as `reader` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSummary<'a> {
    pub id: ChannelId,
    pub project: Option<&'a ProjectId>,
    /// Members and, for seats, their status.
    pub members: Vec<(Address, Option<AgentStatus>)>,
    /// Seats of the project that are down.
    pub former: Vec<&'a AgentId>,
    pub lead: Option<&'a AgentId>,
    /// Whether the reader is a member.
    pub member: bool,
    pub last_post: Option<ChannelPost<'a>>,
    pub activity_seq: u64,
    pub unread: usize,
    pub seen_through: Option<MessageId>,
}

/// The listed channels: projects first, then direct channels, each in id
/// order. Channels of a later form are not listed. The owner's list also has
/// its DM with every lead seat, posts or not.
#[must_use]
pub fn channels_for<'a>(
    state: &'a OrgState,
    reader: &Address,
    scope: ListScope,
) -> Vec<ChannelSummary<'a>> {
    let projects = state.projects().map(|p| ChannelId::Project(p.id.clone()));
    let owner_dms = state
        .agents()
        .filter(|_| *reader == Address::Owner)
        .filter_map(|a| ChannelId::direct(Address::Agent(a.id.clone()), Address::Owner));
    let directs: BTreeSet<ChannelId> = state
        .channel
        .channels
        .keys()
        .filter(|id| matches!(id, ChannelId::Direct(..)))
        .cloned()
        .chain(owner_dms)
        .collect();
    projects
        .chain(directs)
        .filter(|id| listed(state, id))
        .filter(|id| match (scope, reader) {
            (ListScope::All, _) => true,
            (ListScope::Own, Address::Owner) => {
                !matches!(id, ChannelId::Direct(Address::Agent(_), Address::System))
            }
            (ListScope::Own, _) => is_member(state, reader, id),
        })
        .map(|id| summary(state, reader, id))
        .collect()
}

/// Whether a channel is listed: a project with a `lead` record or posts (either
/// needs the project registered), a direct pair with posts, or the owner's DM
/// with a lead seat that has a record. Later forms never are.
pub fn listed(state: &OrgState, channel: &ChannelId) -> bool {
    let has_posts = || {
        state
            .channel
            .channels
            .get(channel)
            .is_some_and(|c| !c.all.is_empty())
    };
    match channel {
        ChannelId::Project(p) => project_lead(state, p).is_some() || has_posts(),
        ChannelId::Direct(Address::Agent(a), Address::Owner) => {
            has_posts() || (state.agent(a).is_some() && is_lead_seat(state, a))
        }
        ChannelId::Direct(..) => has_posts(),
        ChannelId::Other(_) => false,
    }
}

/// A seat the owner's list files under Leads: one named `lead`, or one whose
/// role loads the `atm-lead` skill. A seat named `chief` never is; a seat with
/// no role or no record goes by its name.
#[must_use]
pub fn is_lead_seat(state: &OrgState, agent: &AgentId) -> bool {
    agent.name != "chief"
        && (agent.name == "lead"
            || state
                .agent(agent)
                .and_then(|a| a.role.as_ref())
                .is_some_and(|role| role.config.skills.iter().any(|s| s == "atm-lead")))
}

fn is_member(state: &OrgState, reader: &Address, channel: &ChannelId) -> bool {
    match channel {
        ChannelId::Direct(a, b) => reader == a || reader == b,
        _ => channel_members(state, channel).contains(reader),
    }
}

fn summary<'a>(state: &'a OrgState, reader: &Address, id: ChannelId) -> ChannelSummary<'a> {
    let index = state.channel.channels.get(&id);
    let status = |address: &Address| match address {
        Address::Agent(agent) => state.agent(agent).map(|a| a.status),
        _ => None,
    };
    // Borrow the project id from the state, not from the local channel id.
    let project = match &id {
        ChannelId::Project(p) => state.project(p).map(|p| &p.id),
        _ => None,
    };
    let former = project.map_or_else(Vec::new, |p| {
        seats(state, p)
            .filter(|a| a.status == AgentStatus::Down)
            .map(|a| &a.id)
            .collect()
    });
    let lead = project.and_then(|p| project_lead(state, p)).map(|a| &a.id);
    ChannelSummary {
        members: channel_members(state, &id)
            .into_iter()
            .map(|m| {
                let s = status(&m);
                (m, s)
            })
            .collect(),
        former,
        lead,
        project,
        member: is_member(state, reader, &id),
        last_post: index
            .and_then(|c| c.all.last())
            .and_then(|last| channel_post(state, *last)),
        activity_seq: index.map_or(0, |c| c.activity_seq),
        unread: index.map_or(0, |c| unread(state, c, reader)),
        seen_through: index.and_then(|c| c.seen.get(reader)).map(|(m, _)| *m),
        id,
    }
}

fn is_decision_noise(state: &OrgState, post: &ChannelPost<'_>, reader: &Address) -> bool {
    let ChannelPost::Native(post) = post else {
        return false;
    };
    let content = &post.content;
    let Some(step) = &content.decision else {
        return false;
    };
    match step.step.as_str() {
        "asked" => {
            *reader == Address::Owner
                && content.thread.is_none()
                && content.wake.is_empty()
                && !content.route
        }
        "resolved" | "overruled" => {
            content.author == Address::System
                && content.thread.is_some()
                && state
                    .decision(&step.decision_id)
                    .and_then(|decision| decision.resolution.as_ref())
                    .is_some_and(|answer| &answer.by == reader)
        }
        _ => false,
    }
}

/// With a marker: posts by others after it, except silent backfilled decision
/// questions for the owner and system replies recording the reader's own final
/// answer. Without a marker: the reader's unread wake and legacy rows.
fn unread(state: &OrgState, channel: &ChannelState, reader: &Address) -> usize {
    match channel.seen.get(reader) {
        Some((through, _)) => channel
            .all
            .range((Excluded(*through), Unbounded))
            .filter_map(|id| channel_post(state, *id))
            .filter(|p| p.author() != reader && !is_decision_noise(state, p, reader))
            .count(),
        None => channel
            .all
            .iter()
            .filter_map(|id| wake_for(state, *id, reader))
            .filter_map(|row| state.message(&row))
            .filter(|row| row.read_at.is_none())
            .count(),
    }
}
