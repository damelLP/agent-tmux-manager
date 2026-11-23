//! Channel policy: who may post where, what a post wakes, when it is routed,
//! what a router or the stopgap may decide, which wake rows are legal and how
//! markers move. Pure: state, time and the role-group directory are inputs. The rules are in the crate docs ("Channel
//! policy").

use crate::channel_query::{
    channel_members, client_key_post, decision_post, listed, project_lead, thread_root, wake_for,
};
use crate::mention::parse_mentions;
use crate::*;
use atm_core::SessionId;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// A channel command. Its facts depend on state, so decide builds them here
/// and checks each against the fold's references before emitting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelCommand {
    Post(Box<PostRequest>),
    /// A router's or the stopgap's answer to a post with `route: true`.
    Route(PostRouted),
    /// `reader` has been shown `channel` through the post `through`.
    Seen {
        channel: ChannelId,
        reader: Address,
        through: MessageId,
    },
    Mutate {
        post: MessageId,
        actor: Address,
        change: PostMutation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostMutation {
    Edit(String),
    Remove,
    React(ReactionEmoji),
}

/// Where a post goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostTarget {
    /// A listed channel.
    Channel(ChannelId),
    /// The direct channel of the author and this address, which it wakes.
    Direct(Address),
    /// The thread of this post, reply or wake: the post joins its root.
    Reply(MessageId),
}

/// (project, group) to the seats whose role file lists the group.
pub type GroupDirectory = BTreeMap<(ProjectId, String), Vec<AgentId>>;

/// A post as its author asks for it. The daemon attributes `author`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRequest {
    pub target: PostTarget,
    pub author: Address,
    pub kind: MessageKind,
    pub body: String,
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
    /// Strict tags, `name` or `project/name`: one that does not resolve
    /// refuses the post.
    pub tags: Vec<String>,
    /// Role groups, read by the daemon.
    pub groups: GroupDirectory,
    pub decision: Option<DecisionStep>,
    pub client_key: Option<String>,
    pub also_in_chat: bool,
    /// Wake nobody unless tagged.
    pub no_route: bool,
    /// Written by the daemon for the author (a question or task post): the
    /// author may be down. No request parameter sets it.
    pub internal: bool,
}

/// Why a tag stayed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unresolvable {
    /// Not `name` or `project/name` (strict tags only).
    Malformed,
    Unknown,
    /// Both parties of a direct channel have this short name.
    Ambiguous,
    /// A group with no seat that is not down.
    EmptyGroup,
}

/// An inline tag that stayed text, for the post's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub text: String,
    pub reason: Unresolvable,
}

/// What a body's tags and the strict tags name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// In order of first appearance, body first, deduplicated.
    pub mentions: Vec<Mention>,
    /// Whom they wake, deduplicated; may include the author.
    pub targets: Vec<Address>,
    pub unresolved: Vec<Unresolved>,
}
impl Resolution {
    fn add(&mut self, mention: Mention, targets: Vec<Address>) {
        if self.mentions.contains(&mention) {
            return;
        }
        self.mentions.push(mention);
        for to in targets {
            if !self.targets.contains(&to) {
                self.targets.push(to);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChannelRefusal {
    #[error("channel {0} is not listed")]
    UnknownChannel(ChannelId),
    /// The author may not post there.
    #[error("the author may not post in {0}")]
    NotMember(ChannelId),
    #[error("tag {tag:?} does not resolve: {reason:?}")]
    UnknownTag { tag: String, reason: Unresolvable },
    #[error("post {0} is not waiting for routing")]
    NotRouted(MessageId),
    /// A different or late answer: the first one stands.
    #[error("post {0} is already routed")]
    AlreadyRouted(MessageId),
    #[error("post {post} cannot wake {to}")]
    NotRoutable { post: MessageId, to: Address },
    /// A `msg.sent` naming a post that is not its wake.
    #[error("not a wake of post {post}: {reason}")]
    InvalidWake {
        post: MessageId,
        reason: &'static str,
    },
    #[error("post {post} is not in channel {channel}")]
    NotInChannel { post: MessageId, channel: ChannelId },
    #[error("cannot change post {post}: {reason}")]
    PostMutation {
        post: MessageId,
        reason: &'static str,
    },
    #[error("invalid edited body: {0}")]
    InvalidEditBody(&'static str),
}

/// `[router]` in the atm config. Unknown keys are errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterSettings {
    /// Off: the stopgap routes every untagged post at once.
    pub enabled: bool,
    /// How long an attached router has for a post.
    pub timeout_secs: u64,
}
impl Default for RouterSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: 20,
        }
    }
}

/// Whether a router is there. A daemon start and a router detach each open a
/// grace of one timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterPresence {
    Attached { since: DateTime<Utc> },
    Grace { until: DateTime<Utc> },
    Absent,
}

/// What to do with a pending post now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStep {
    /// Look again at this time.
    Wait(DateTime<Utc>),
    /// Route by the stopgap, with this cause.
    Stopgap(&'static str),
}

/// When a pending post falls to the stopgap (design 8.1). While attached, its
/// deadline is one timeout after the later of the post and the attach.
#[must_use]
pub fn route_step(
    posted_at: DateTime<Utc>,
    now: DateTime<Utc>,
    settings: &RouterSettings,
    presence: RouterPresence,
) -> RouteStep {
    let until = |end: DateTime<Utc>, cause| {
        if now < end {
            RouteStep::Wait(end)
        } else {
            RouteStep::Stopgap(cause)
        }
    };
    if !settings.enabled {
        return RouteStep::Stopgap("disabled");
    }
    match presence {
        RouterPresence::Absent => RouteStep::Stopgap("no_router"),
        RouterPresence::Grace { until: end } => until(end, "no_router"),
        RouterPresence::Attached { since } => {
            let deadline = i64::try_from(settings.timeout_secs)
                .ok()
                .and_then(TimeDelta::try_seconds)
                .and_then(|timeout| posted_at.max(since).checked_add_signed(timeout))
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            until(deadline, "timeout")
        }
    }
}

/// Whether a routing answer may wake `to` for `post`: not its author or
/// `system`, and a member of its channel (the owner or a seat that is not down
/// in a project channel; the other party, whatever its status, in a direct
/// one). The stopgap filters by the same test, so `Route` accepts its answers.
fn routable(state: &OrgState, post: &Post, to: &Address) -> bool {
    *to != post.content.author
        && *to != Address::System
        && channel_members(state, &post.content.channel).contains(to)
}

/// The stopgap's answer for a pending post (design 8.3); empty for an unknown
/// post.
#[must_use]
pub fn stopgap_wake(state: &OrgState, post: MessageId) -> Vec<Address> {
    let Some(post) = channel_query::post(state, post) else {
        return vec![];
    };
    let content = &post.content;
    let wakeable = |to: &&Address| routable(state, post, to);
    let lead = match &content.channel {
        ChannelId::Project(p) => project_lead(state, p).map(|a| Address::Agent(a.id.clone())),
        _ => None,
    };
    let one = |to: Option<&Address>| to.filter(wakeable).cloned().into_iter().collect();
    match (&content.channel, content.thread) {
        (ChannelId::Direct(a, b), _) => one(Some(if *a == content.author { b } else { a })),
        (ChannelId::Project(_), Some(root)) => thread_target(state, post, root)
            .cloned()
            .into_iter()
            .collect(),
        (ChannelId::Project(_), None) if Some(&content.author) == lead.as_ref() => {
            channel_members(state, &content.channel)
                .into_iter()
                .filter(|to| match to {
                    Address::Agent(id) => state.agent(id).is_some_and(|a| {
                        matches!(
                            a.status,
                            AgentStatus::Starting | AgentStatus::Ready | AgentStatus::Clearing
                        )
                    }),
                    _ => false,
                })
                .filter(|to| wakeable(&to))
                .collect()
        }
        (ChannelId::Project(_), None) => match &content.author {
            Address::Owner | Address::Agent(_) => one(lead.as_ref()),
            Address::System => vec![],
        },
        (ChannelId::Other(_), _) => vec![],
    }
}

/// A thread reply's one target: the root's author, else the latest earlier
/// replier other than this one, else the root's single wake; each only if it
/// can be woken.
fn thread_target<'a>(state: &'a OrgState, post: &Post, root: MessageId) -> Option<&'a Address> {
    let wakeable = |to: &&Address| routable(state, post, to);
    let root_post = channel_query::post(state, root);
    let replies = state
        .channel
        .channels
        .get(&post.content.channel)
        .and_then(|c| c.threads.get(&root));
    let earlier = replies
        .into_iter()
        .flatten()
        .rev()
        .filter(|id| **id < post.id)
        .filter_map(|id| channel_query::post(state, *id))
        .map(|reply| &reply.content.author)
        .find(wakeable);
    let single = root_post.and_then(|r| {
        let mut woke = r.woke();
        match (woke.next(), woke.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    });
    root_post
        .map(|r| &r.content.author)
        .filter(wakeable)
        .or(earlier)
        .or(single.filter(wakeable))
}

/// Resolve a body's inline tags (lenient) and the strict tags in `channel`
/// (design 9.2). A strict tag that does not resolve is refused.
pub fn resolve_mentions(
    state: &OrgState,
    channel: &ChannelId,
    body: &str,
    tags: &[String],
    groups: &GroupDirectory,
) -> Result<Resolution, ChannelRefusal> {
    let mut out = Resolution::default();
    for token in parse_mentions(body) {
        match resolve(state, channel, token.project, token.name, groups) {
            Ok((mention, targets)) => out.add(mention, targets),
            Err(reason) => {
                let text = body.get(token.span).unwrap_or_default().to_owned();
                if !out.unresolved.iter().any(|u| u.text == text) {
                    out.unresolved.push(Unresolved { text, reason });
                }
            }
        }
    }
    for tag in tags {
        let text = format!("@{tag}");
        let resolved = match parse_mentions(&text).as_slice() {
            [token] if token.span == (0..text.len()) => {
                resolve(state, channel, token.project, token.name, groups)
            }
            _ => Err(Unresolvable::Malformed),
        };
        let (mention, targets) = resolved.map_err(|reason| ChannelRefusal::UnknownTag {
            tag: tag.clone(),
            reason,
        })?;
        out.add(mention, targets);
    }
    Ok(out)
}

/// One token: the owner, then a seat, then a group.
fn resolve(
    state: &OrgState,
    channel: &ChannelId,
    project: Option<&str>,
    name: &str,
    groups: &GroupDirectory,
) -> Result<(Mention, Vec<Address>), Unresolvable> {
    if project.is_none() && name == "owner" {
        return Ok((Mention::Owner, vec![Address::Owner]));
    }
    let scope: Option<ProjectId> = match (project, channel) {
        (Some(project), _) => project.parse().ok(),
        (None, ChannelId::Project(project)) => Some(project.clone()),
        _ => None,
    };
    let seat = match (&scope, channel) {
        (Some(project), _) => AgentId::new(project.clone(), name).ok(),
        (None, ChannelId::Direct(a, b)) => {
            let named: Vec<&AgentId> = [a, b]
                .into_iter()
                .filter_map(|party| match party {
                    Address::Agent(id) if id.name == name => Some(id),
                    _ => None,
                })
                .collect();
            match named.as_slice() {
                [id] => Some((*id).clone()),
                [_, _] => return Err(Unresolvable::Ambiguous),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(id) = seat.filter(|id| state.agent(id).is_some()) {
        return Ok((Mention::Seat(id.clone()), vec![Address::Agent(id)]));
    }
    let Some(project) = scope else {
        return Err(Unresolvable::Unknown);
    };
    let Some(seats) = groups.get(&(project.clone(), name.to_owned())) else {
        return Err(Unresolvable::Unknown);
    };
    let live: Vec<Address> = seats
        .iter()
        .filter(|id| {
            state
                .agent(id)
                .is_some_and(|a| a.status != AgentStatus::Down)
        })
        .map(|id| Address::Agent(id.clone()))
        .collect();
    if live.is_empty() {
        return Err(Unresolvable::EmptyGroup);
    }
    Ok((
        Mention::Group {
            project,
            name: name.to_owned(),
        },
        live,
    ))
}

impl OrgState {
    pub(crate) fn decide_channel(
        &self,
        command: &ChannelCommand,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        match command {
            ChannelCommand::Post(request) => self.decide_post(request),
            ChannelCommand::Route(answer) => self.decide_route(answer),
            ChannelCommand::Seen {
                channel,
                reader,
                through,
            } => self.decide_seen(channel, reader, *through, now),
            ChannelCommand::Mutate {
                post,
                actor,
                change,
            } => self.decide_post_mutation(*post, actor, change),
        }
    }

    fn decide_post_mutation(
        &self,
        id: MessageId,
        actor: &Address,
        change: &PostMutation,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        let refuse = |reason| ChannelRefusal::PostMutation { post: id, reason };
        let reacting = matches!(change, PostMutation::React(_));
        let id = match self.message(&id) {
            Some(message) if reacting && &message.content.to == actor => message
                .content
                .post
                .ok_or_else(|| refuse("a native post or its own wake ID is required"))?,
            Some(_) => return Err(refuse("a native post ID is required").into()),
            None => id,
        };
        let content = &self.require_post(&id)?.content;
        if !content.is_known()
            || matches!(self.post_presentation(id), Some(PostPresentation::Removed))
        {
            return Err(refuse("the post is removed or unsupported").into());
        }
        if reacting {
            if *actor == Address::System || !channel_members(self, &content.channel).contains(actor)
            {
                return Err(refuse("only channel members may react").into());
            }
        } else if actor != &content.author
            || *actor == Address::System
            || content.decision.is_some()
        {
            return Err(
                refuse("only the author of a non-decision post may edit or remove it").into(),
            );
        }
        let actor = actor.clone();
        let event = match change {
            PostMutation::Edit(body) => {
                if body.len() > 16_384 {
                    return Err(
                        ChannelRefusal::InvalidEditBody("body exceeds 16384 UTF-8 bytes").into(),
                    );
                }
                if body.trim().is_empty() && content.attachments.is_empty() {
                    return Err(ChannelRefusal::InvalidEditBody(
                        "blank body requires an attachment",
                    )
                    .into());
                }
                OrgEvent::ChannelPostEdited(PostEdited {
                    post: id,
                    actor,
                    body: body.clone(),
                })
            }
            PostMutation::Remove => OrgEvent::ChannelPostRemoved(PostRemoved { post: id, actor }),
            PostMutation::React(emoji) => {
                let active = !self
                    .post_reactions(id)
                    .is_some_and(|pairs| pairs.contains(&(*emoji, actor.clone())));
                OrgEvent::ChannelReacted(PostReacted {
                    post: id,
                    actor,
                    emoji: *emoji,
                    active,
                })
            }
        };
        Ok(vec![event])
    }

    /// A wake row (`SendMessage` with `post`, design 6.3). False: the pair
    /// already has its row, so nothing is written.
    pub(crate) fn wake_row_allowed(&self, row: &MessageContent) -> Result<bool, Refusal> {
        let Some(id) = row.post else {
            return Ok(true);
        };
        let post = self.require_post(&id)?;
        if self.channel.wakes.contains_key(&(id, row.to.clone())) {
            return Ok(false);
        }
        let reason = if row.from != post.content.author {
            "the sender is not the post's author"
        } else if !post.woke().any(|to| *to == row.to) {
            "the post does not wake this recipient"
        } else if row.kind != post.content.kind
            || row.body != post.content.body
            || row.attachments != post.content.attachments
            || row.task != post.content.task
            || row.delivery_notice.is_some()
        {
            "the row is not a copy of its post"
        } else {
            return Ok(true);
        };
        Err(ChannelRefusal::InvalidWake { post: id, reason }.into())
    }

    /// An ack by post id (design 6.5): a native post's id becomes the reader's
    /// own row for it; every other id is kept.
    pub(crate) fn own_rows(
        &self,
        who: &Address,
        ids: &[MessageId],
    ) -> Result<Vec<MessageId>, Refusal> {
        ids.iter()
            .map(|id| match channel_query::post(self, *id) {
                Some(_) => wake_for(self, *id, who).ok_or_else(|| Refusal::WrongRecipient {
                    message_id: *id,
                    who: who.clone(),
                }),
                None => Ok(*id),
            })
            .collect()
    }

    /// A marker (design 6.4): it only advances. For the owner the same
    /// decision reads his unread rows whose post is at or before `through`,
    /// with or without an advance.
    fn decide_seen(
        &self,
        channel: &ChannelId,
        reader: &Address,
        through: MessageId,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        if !channel.is_known() {
            return Err(ChannelRefusal::UnknownChannel(channel.clone()).into());
        }
        let index = self.channel.channels.get(channel);
        let Some(index) = index.filter(|c| c.all.contains(&through)) else {
            return Err(ChannelRefusal::NotInChannel {
                post: through,
                channel: channel.clone(),
            }
            .into());
        };
        let mut events = Vec::new();
        if index
            .seen
            .get(reader)
            .is_none_or(|(marker, _)| *marker < through)
        {
            let event = OrgEvent::ChannelSeen(ChannelSeen {
                channel: channel.clone(),
                reader: reader.clone(),
                through,
                session_id: self.bound_session(reader),
            });
            self.validate_event(&event)?;
            events.push(event);
        }
        if *reader == Address::Owner {
            let rows: Vec<MessageId> = index
                .all
                .range(..=through)
                .filter_map(|post| wake_for(self, *post, reader))
                .filter(|row| self.message(row).is_some_and(|m| m.read_at.is_none()))
                .collect();
            if !rows.is_empty() {
                let read = OrgCommand::ReadMessages {
                    who: Address::Owner,
                    message_ids: rows,
                };
                events.extend(self.decide(&read, now)?);
            }
        }
        Ok(events)
    }

    /// A seat's bound session, unless it is a pending one.
    fn bound_session(&self, who: &Address) -> Option<SessionId> {
        match who {
            Address::Agent(id) => self
                .agent(id)
                .and_then(|a| a.session_id.clone())
                .filter(|s| !s.is_pending()),
            _ => None,
        }
    }

    fn decide_route(&self, answer: &PostRouted) -> Result<Vec<OrgEvent>, Refusal> {
        if !answer.by.is_known() {
            return Err(Refusal::InvalidValue {
                field: "by",
                reason: "a later router kind or stopgap cause".into(),
            });
        }
        if answer.reason.as_ref().is_some_and(|r| r.len() > 256) {
            return Err(Refusal::InvalidValue {
                field: "reason",
                reason: "at most 256 bytes".into(),
            });
        }
        let post = self.require_post(&answer.post)?;
        if !post.content.route {
            return Err(ChannelRefusal::NotRouted(post.id).into());
        }
        let mut wake: Vec<Address> = Vec::new();
        for to in &answer.wake {
            if !wake.contains(to) {
                wake.push(to.clone());
            }
        }
        if let Some(first) = &post.routed {
            // The first answer stands; the same one again (any reason) is no event.
            let same = first.by == answer.by
                && first.wake.len() == wake.len()
                && wake.iter().all(|to| first.wake.contains(to));
            return if same {
                Ok(vec![])
            } else {
                Err(ChannelRefusal::AlreadyRouted(post.id).into())
            };
        }
        if let Some(to) = wake.iter().find(|to| !routable(self, post, to)) {
            return Err(ChannelRefusal::NotRoutable {
                post: post.id,
                to: to.clone(),
            }
            .into());
        }
        let event = OrgEvent::ChannelRouted(PostRouted {
            post: post.id,
            wake,
            by: answer.by.clone(),
            reason: answer.reason.clone(),
        });
        self.validate_event(&event)?;
        Ok(vec![event])
    }

    fn decide_post(&self, request: &PostRequest) -> Result<Vec<OrgEvent>, Refusal> {
        if request.also_in_chat && !matches!(request.target, PostTarget::Reply(_)) {
            return Err(Refusal::InvalidValue {
                field: "also_in_chat",
                reason: "requires a thread reply".into(),
            });
        }
        let author = &request.author;
        self.validate_address(author)?;
        if let PostTarget::Channel(channel) = &request.target {
            if !channel.is_known() {
                return Err(ChannelRefusal::UnknownChannel(channel.clone()).into());
            }
        }
        if let Some(step) = request.decision.as_ref().filter(|s| !s.is_known()) {
            return Err(Refusal::InvalidValue {
                field: "decision step",
                reason: format!("{:?} is a later form", step.step),
            });
        }
        if let Some(key) = &request.client_key {
            if key.is_empty() || key.len() > 64 {
                return Err(Refusal::InvalidValue {
                    field: "client_key",
                    reason: "must be 1 to 64 bytes".into(),
                });
            }
        }
        // A repeat names the first post (through the queries) and writes nothing.
        let repeat = request
            .client_key
            .as_ref()
            .is_some_and(|key| client_key_post(self, author, key).is_some())
            || request
                .decision
                .as_ref()
                .is_some_and(|s| decision_post(self, s.decision_id, &s.step).is_some());
        if repeat {
            return Ok(vec![]);
        }

        let (channel, thread, to) = match &request.target {
            PostTarget::Channel(channel) => {
                // An internal post may open a direct channel: the decision
                // bridge's question in the asker's DM with the owner (U8). Its
                // tags say whom it wakes; a question asked before the daemon
                // started wakes nobody, so it cannot be a `Direct` post.
                let opens = request.internal && matches!(channel, ChannelId::Direct(..));
                if !opens && !listed(self, channel) {
                    return Err(ChannelRefusal::UnknownChannel(channel.clone()).into());
                }
                (channel.clone(), None, None)
            }
            PostTarget::Direct(to) => {
                let channel = ChannelId::direct(author.clone(), to.clone())
                    .filter(|_| *to != Address::System)
                    .ok_or_else(|| Refusal::InvalidRecipient(to.clone()))?;
                (channel, None, Some(to))
            }
            PostTarget::Reply(id) => {
                let root = thread_root(self, *id).ok_or(ApplyError::UnknownPost(*id))?;
                (self.post_channel(root)?, Some(root), None)
            }
        };

        let members = channel_members(self, &channel);
        // `system` replies in a question's thread wherever the question is,
        // its asker's DM with the owner included (design 12).
        let member = members.contains(author)
            || (request.internal && *author == Address::System && request.decision.is_some())
            || matches!(&channel, ChannelId::Project(project)
                if *author == Address::System
                    || (request.internal
                        && matches!(author, Address::Agent(id) if id.project == *project)));
        if !member {
            return Err(ChannelRefusal::NotMember(channel).into());
        }

        // Generated posts copy text verbatim; only their explicit tags address anyone.
        let inline = if request.internal { "" } else { &request.body };
        let resolution = resolve_mentions(self, &channel, inline, &request.tags, &request.groups)?;
        let mut wake: Vec<Address> = Vec::new();
        for target in to.into_iter().chain(&resolution.targets) {
            if target != author && !wake.contains(target) {
                wake.push(target.clone());
            }
        }
        let route = wake.is_empty()
            && *author != Address::System
            && !request.no_route
            && members.iter().any(|m| m != author && *m != Address::System);
        let event = OrgEvent::ChannelPosted(PostContent {
            channel,
            author: author.clone(),
            session_id: self.bound_session(author),
            kind: request.kind,
            body: request.body.clone(),
            attachments: request.attachments.clone(),
            task: request.task.clone(),
            thread,
            mentions: resolution.mentions,
            wake,
            route,
            decision: request.decision.clone(),
            client_key: request.client_key.clone(),
            also_in_chat: request.also_in_chat,
        });
        self.validate_event(&event)?;
        Ok(vec![event])
    }

    /// The channel of a thread root: a native post's, or a legacy message's
    /// direct channel.
    fn post_channel(&self, root: MessageId) -> Result<ChannelId, Refusal> {
        let channel = match channel_query::post(self, root) {
            Some(post) => Some(post.content.channel.clone()),
            None => self
                .message(&root)
                .and_then(|m| ChannelId::direct(m.content.from.clone(), m.content.to.clone())),
        };
        match channel {
            Some(channel) if channel.is_known() => Ok(channel),
            Some(channel) => Err(ChannelRefusal::UnknownChannel(channel).into()),
            None => Err(ApplyError::UnknownPost(root).into()),
        }
    }
}
