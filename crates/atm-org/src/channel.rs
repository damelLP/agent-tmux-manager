//! Channel facts: posting, routing, read markers and post presentation.
//!
//! A known event kind whose body cannot be decoded disables the organisation,
//! so a later daemon must not be able to write anything into these three kinds
//! that this one cannot read. Open tags are plain strings (`cause`, `step`),
//! and each enum has a read-only catch-all that keeps the raw value:
//! [`Mention::Unknown`], [`RouteBy::Unknown`] and [`ChannelId::Other`]. The fold
//! accepts them; `is_known` is false for them and commands refuse them. A known
//! shape whose content this version cannot read (`{"seat":"Bad"}`) is a
//! catch-all too, and keys a known shape does not use are ignored.
//!
//! Decoding is not the whole rule. This version still refuses a record that
//! names something it does not know (a post, thread root or decision that a
//! later kind created), and that disables the organisation too. So a later
//! event kind must not create anything a `channel.*` record or a wake row then
//! names; such a record needs a new kind as well.
//!
//! [`MessageKind`] and [`Address`] have no catch-all. A new message kind or
//! address form must not appear inside `channel.posted` or `msg.sent` until a
//! catch-all for it exists in a promoted binary; until then a new form arrives
//! as a new event kind.
//!
//! # Fold
//!
//! [`OrgState::apply`] indexes every post and every message into channels. A
//! `msg.sent` without `post` is a top-level post in the direct channel of its
//! two ends (not indexed when they are equal); with `post` it is a wake: the
//! first row for a (post, recipient) pair is that pair's wake, later ones are
//! plain messages. A post's `wake` addresses, and its routing answer's, wait in
//! a repair list until their row lands; the author and `system` never do,
//! because no wake row may be made for them. The first `channel.routed` for a
//! post stands and later ones change nothing; the first post for a decision step
//! or a client key is the one indexed. Markers only advance. `apply` checks
//! references only (addresses, project, thread root, post, decision); where a
//! post may go is policy and belongs to `decide`, so a later form with, say,
//! deeper threads still folds.

use crate::{
    state::valid_external_id, Address, AgentId, ApplyError, ChannelId, DecisionId,
    DecisionResolution, DecisionResolutionEvent, DecisionWithdrawal, IdError, MessageContent,
    MessageId, MessageKind, OrgEvent, OrgState, ProjectId, ProvisionalChoice, TaskId,
};
use atm_core::SessionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

/// The raw JSON of a later form this version does not know. Only decoding
/// makes one, and it encodes back to the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaterForm(Value);
impl LaterForm {
    #[must_use]
    pub fn raw(&self) -> &Value {
        &self.0
    }
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        entries.map(|(key, value)| (key.to_owned(), value)),
    ))
}
fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// A resolved tag. Text: `"owner"`, `{"seat":"<agent id>"}` or
/// `{"group":{"project":"<id>","name":"<slug>"}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    Owner,
    Seat(AgentId),
    Group {
        project: ProjectId,
        name: String,
    },
    /// Read-only: a later shape, kept as written.
    Unknown(LaterForm),
}
impl Mention {
    #[must_use]
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
    fn read(value: Value) -> Self {
        let known = match (&value, value.get("seat"), value.get("group")) {
            (Value::String(word), _, _) if word == "owner" => Some(Self::Owner),
            (Value::Object(_), Some(seat), None) => {
                seat.as_str().and_then(|s| s.parse().ok()).map(Self::Seat)
            }
            (Value::Object(_), None, Some(group)) => text(group, "project")
                .and_then(|p| p.parse().ok())
                .zip(text(group, "name"))
                .map(|(project, name)| Self::Group { project, name }),
            _ => None,
        };
        known.unwrap_or(Self::Unknown(LaterForm(value)))
    }
    fn write(&self) -> Value {
        match self {
            Self::Owner => Value::String("owner".into()),
            Self::Seat(id) => object([("seat", Value::String(id.to_string()))]),
            Self::Group { project, name } => object([(
                "group",
                object([
                    ("project", Value::String(project.to_string())),
                    ("name", Value::String(name.clone())),
                ]),
            )]),
            Self::Unknown(form) => form.0.clone(),
        }
    }
}
impl Serialize for Mention {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.write().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Mention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::read)
    }
}

/// Who answered a routed post. Text: `{"kind":"router","name":..,"model":..}`
/// or `{"kind":"stopgap","cause":..}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteBy {
    Router {
        name: String,
        model: Option<String>,
    },
    /// `cause` is an open tag: `disabled`, `no_router`, `timeout`, `declined`.
    Stopgap {
        cause: String,
    },
    /// Read-only: a later kind, kept as written.
    Unknown(LaterForm),
}
impl RouteBy {
    const CAUSES: [&'static str; 4] = ["disabled", "no_router", "timeout", "declined"];

    /// False for a later kind or cause, which commands refuse.
    #[must_use]
    pub fn is_known(&self) -> bool {
        match self {
            Self::Router { .. } => true,
            Self::Stopgap { cause } => Self::CAUSES.contains(&cause.as_str()),
            Self::Unknown(_) => false,
        }
    }
    fn read(value: Value) -> Self {
        let model = match value.get("model") {
            None | Some(Value::Null) => Some(None),
            Some(Value::String(model)) => Some(Some(model.clone())),
            Some(_) => None,
        };
        let known = match value.get("kind").and_then(Value::as_str) {
            Some("router") => text(&value, "name")
                .zip(model)
                .map(|(name, model)| Self::Router { name, model }),
            Some("stopgap") => text(&value, "cause").map(|cause| Self::Stopgap { cause }),
            _ => None,
        };
        known.unwrap_or(Self::Unknown(LaterForm(value)))
    }
    fn write(&self) -> Value {
        match self {
            Self::Router { name, model } => object([
                ("kind", Value::String("router".into())),
                ("name", Value::String(name.clone())),
                ("model", model.clone().map_or(Value::Null, Value::String)),
            ]),
            Self::Stopgap { cause } => object([
                ("kind", Value::String("stopgap".into())),
                ("cause", Value::String(cause.clone())),
            ]),
            Self::Unknown(form) => form.0.clone(),
        }
    }
}
impl Serialize for RouteBy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.write().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for RouteBy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::read)
    }
}

/// The step of a decision a post shows. `step` is an open tag: `asked`,
/// `taken`, `resolved`, `overruled`, `expired`, `withdrawn`, `waits`,
/// `redo:<task id>`. The pair is also the post's dedupe key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionStep {
    pub decision_id: DecisionId,
    pub step: String,
}
impl DecisionStep {
    const STEPS: [&'static str; 7] = [
        "asked",
        "taken",
        "resolved",
        "overruled",
        "expired",
        "withdrawn",
        "waits",
    ];

    /// False for a later step, which commands refuse.
    #[must_use]
    pub fn is_known(&self) -> bool {
        Self::STEPS.contains(&self.step.as_str())
            || self
                .step
                .strip_prefix("redo:")
                .is_some_and(|task| !task.is_empty())
    }
}

/// Body of `channel.posted`. The post's id is `m<seq>`, in the message id
/// space. `thread` names the root post, null for a top-level post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostContent {
    pub channel: ChannelId,
    pub author: Address,
    pub session_id: Option<SessionId>,
    pub kind: MessageKind,
    pub body: String,
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
    pub thread: Option<MessageId>,
    pub mentions: Vec<Mention>,
    /// Addresses woken by rule (tags, an explicit recipient).
    pub wake: Vec<Address>,
    /// True when no rule addressed the post and routing must decide.
    pub route: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionStep>,
    /// A frontend's idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// Show this reply in the main timeline as well as its thread.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub also_in_chat: bool,
}
impl PostContent {
    /// False when any part is a later form, which commands refuse.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.channel.is_known()
            && self.mentions.iter().all(Mention::is_known)
            && self.decision.as_ref().is_none_or(DecisionStep::is_known)
    }
}

/// Body of `channel.routed`: the one answer to a post with `route: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRouted {
    pub post: MessageId,
    /// Empty means nobody, and is final.
    pub wake: Vec<Address>,
    pub by: RouteBy,
    /// The router's one-line reason; audit only.
    pub reason: Option<String>,
}

/// Body of `channel.seen`: `reader` has been shown `channel` through `through`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSeen {
    pub channel: ChannelId,
    pub reader: Address,
    pub through: MessageId,
    pub session_id: Option<SessionId>,
}

/// The replacement text of a native post. Audience and placement do not change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostEdited {
    pub post: MessageId,
    pub actor: Address,
    pub body: String,
}

/// The presentation of a post is gone, but the original fact and ID remain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRemoved {
    pub post: MessageId,
    pub actor: Address,
}

/// The fixed reactions, in chip display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReactionEmoji {
    #[serde(rename = "👍")]
    ThumbsUp,
    #[serde(rename = "✅")]
    Check,
    #[serde(rename = "👀")]
    Eyes,
    #[serde(rename = "❌")]
    Cross,
}

impl ReactionEmoji {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThumbsUp => "👍",
            Self::Check => "✅",
            Self::Eyes => "👀",
            Self::Cross => "❌",
        }
    }
}

/// The actor's resulting membership, not an instruction to flip on replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostReacted {
    pub post: MessageId,
    pub actor: Address,
    pub emoji: ReactionEmoji,
    pub active: bool,
}

/// The overlay on a post, absent until someone changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostPresentation {
    Edited(String),
    Removed,
}

impl PostPresentation {
    #[must_use]
    pub fn body(&self) -> &str {
        match self {
            Self::Edited(body) => body,
            Self::Removed => "removed",
        }
    }
}

/// A committed post: its original content, when it was posted, and the answer
/// that routed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub id: MessageId,
    pub content: PostContent,
    pub posted_at: DateTime<Utc>,
    pub routed: Option<PostRouted>,
}

/// Where a post's routing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Addressed by rule; nothing to route.
    None,
    /// Waiting for a router or the stopgap.
    Pending,
    Router,
    Stopgap,
    /// Answered by a later kind of router.
    Unknown,
}

impl Post {
    #[must_use]
    pub fn routing(&self) -> Routing {
        match (&self.routed, self.content.route) {
            (Some(answer), _) => match answer.by {
                RouteBy::Router { .. } => Routing::Router,
                RouteBy::Stopgap { .. } => Routing::Stopgap,
                RouteBy::Unknown(_) => Routing::Unknown,
            },
            (None, true) => Routing::Pending,
            (None, false) => Routing::None,
        }
    }

    /// Everyone the post wakes: by rule, then by its routing answer.
    pub fn woke(&self) -> impl Iterator<Item = &Address> {
        self.content
            .wake
            .iter()
            .chain(self.routed.iter().flat_map(|answer| answer.wake.iter()))
    }
}

/// One channel's index. Legacy messages are indexed by id only; their content
/// stays in the message rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChannelState {
    pub(crate) top_level: BTreeSet<MessageId>,
    /// True roots and shared replies, for the opt-in main history.
    pub(crate) timeline: BTreeSet<MessageId>,
    /// Root to replies, in id order.
    pub(crate) threads: BTreeMap<MessageId, Vec<MessageId>>,
    /// Every post, top level and replies.
    pub(crate) all: BTreeSet<MessageId>,
    /// Reader to (marker, sequence of the `channel.seen` that set it).
    pub(crate) seen: BTreeMap<Address, (MessageId, u64)>,
    /// Sequence of the last post, routing answer or linked decision step.
    pub(crate) activity_seq: u64,
}
impl ChannelState {
    fn add(&mut self, id: MessageId, thread: Option<MessageId>, also_in_chat: bool, seq: u64) {
        if thread.is_none() || also_in_chat {
            self.timeline.insert(id);
        }
        match thread {
            Some(root) => self.threads.entry(root).or_default().push(id),
            None => {
                self.top_level.insert(id);
            }
        }
        self.all.insert(id);
        self.activity_seq = seq;
    }
}

/// The channel indexes of the fold; every update is a bounded number of map
/// operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChannelIndex {
    pub(crate) posts: BTreeMap<MessageId, Post>,
    presentation: BTreeMap<MessageId, PostPresentation>,
    reactions: BTreeMap<MessageId, BTreeSet<(ReactionEmoji, Address)>>,
    pub(crate) channels: BTreeMap<ChannelId, ChannelState>,
    /// (post, recipient) to its wake row.
    pub(crate) wakes: BTreeMap<(MessageId, Address), MessageId>,
    /// Wakes due but not yet written: the repair list.
    pub(crate) unwoken: BTreeSet<(MessageId, Address)>,
    /// Posts waiting for a routing answer, with their post time.
    pub(crate) unrouted: BTreeMap<MessageId, DateTime<Utc>>,
    pub(crate) decision_posts: BTreeMap<(DecisionId, String), MessageId>,
    pub(crate) client_keys: BTreeMap<(Address, String), MessageId>,
}
impl ChannelIndex {
    /// Fold a validated event. Infallible: references were checked first.
    pub(crate) fn apply(&mut self, seq: u64, ts: DateTime<Utc>, event: &OrgEvent) {
        let id = MessageId::new(seq);
        match event {
            OrgEvent::MessageSent(content) => self.message(id, seq, content),
            OrgEvent::ChannelPosted(content) => self.post(id, seq, ts, content),
            OrgEvent::ChannelRouted(answer) => self.route(seq, answer),
            OrgEvent::ChannelSeen(seen) => self.see(seq, seen),
            OrgEvent::ChannelPostEdited(edit) => {
                self.presentation
                    .insert(edit.post, PostPresentation::Edited(edit.body.clone()));
            }
            OrgEvent::ChannelPostRemoved(remove) => {
                self.presentation
                    .insert(remove.post, PostPresentation::Removed);
                self.reactions.remove(&remove.post);
            }
            OrgEvent::ChannelReacted(react) => self.react(react),
            OrgEvent::DecisionTakenProvisionally(ProvisionalChoice { decision_id, .. })
            | OrgEvent::DecisionResolved(DecisionResolutionEvent {
                resolution: DecisionResolution { decision_id, .. },
                ..
            })
            | OrgEvent::DecisionOverruled(DecisionResolutionEvent {
                resolution: DecisionResolution { decision_id, .. },
                ..
            })
            | OrgEvent::DecisionExpired { decision_id }
            | OrgEvent::DecisionWithdrawn(DecisionWithdrawal { decision_id, .. }) => {
                self.touch_decision(seq, *decision_id);
            }
            _ => {}
        }
    }

    fn react(&mut self, event: &PostReacted) {
        let pair = (event.emoji, event.actor.clone());
        if event.active {
            self.reactions.entry(event.post).or_default().insert(pair);
        } else if let Entry::Occupied(mut row) = self.reactions.entry(event.post) {
            row.get_mut().remove(&pair);
            if row.get().is_empty() {
                row.remove();
            }
        }
    }

    fn message(&mut self, id: MessageId, seq: u64, content: &MessageContent) {
        if let Some(post) = content.post {
            let pair = (post, content.to.clone());
            self.unwoken.remove(&pair);
            self.wakes.entry(pair).or_insert(id);
        } else if let Some(channel) = ChannelId::direct(content.from.clone(), content.to.clone()) {
            self.channels
                .entry(channel)
                .or_default()
                .add(id, None, false, seq);
        }
    }

    fn post(&mut self, id: MessageId, seq: u64, ts: DateTime<Utc>, content: &PostContent) {
        self.channels
            .entry(content.channel.clone())
            .or_default()
            .add(id, content.thread, content.also_in_chat, seq);
        self.want(id, &content.author, &content.wake);
        if content.route {
            self.unrouted.insert(id, ts);
        }
        if let Some(step) = &content.decision {
            self.decision_posts
                .entry((step.decision_id, step.step.clone()))
                .or_insert(id);
        }
        if let Some(key) = &content.client_key {
            self.client_keys
                .entry((content.author.clone(), key.clone()))
                .or_insert(id);
        }
        self.posts.insert(
            id,
            Post {
                id,
                content: content.clone(),
                posted_at: ts,
                routed: None,
            },
        );
    }

    /// Queue the wakes no row fills yet. No wake row may be made for the
    /// author or `system`, so they never wait.
    fn want(&mut self, post: MessageId, author: &Address, targets: &[Address]) {
        for to in targets {
            let pair = (post, to.clone());
            if to != author && *to != Address::System && !self.wakes.contains_key(&pair) {
                self.unwoken.insert(pair);
            }
        }
    }

    fn route(&mut self, seq: u64, answer: &PostRouted) {
        let Some(post) = self.posts.get_mut(&answer.post) else {
            return;
        };
        if post.routed.is_some() {
            return; // The first answer stands.
        }
        post.routed = Some(answer.clone());
        let (author, channel) = (post.content.author.clone(), post.content.channel.clone());
        self.unrouted.remove(&answer.post);
        self.want(answer.post, &author, &answer.wake);
        if let Some(state) = self.channels.get_mut(&channel) {
            state.activity_seq = seq;
        }
    }

    fn see(&mut self, seq: u64, seen: &ChannelSeen) {
        let markers = &mut self.channels.entry(seen.channel.clone()).or_default().seen;
        match markers.entry(seen.reader.clone()) {
            Entry::Vacant(entry) => {
                entry.insert((seen.through, seq));
            }
            Entry::Occupied(mut entry) if entry.get().0 < seen.through => {
                entry.insert((seen.through, seq));
            }
            Entry::Occupied(_) => {}
        }
    }

    fn touch_decision(&mut self, seq: u64, decision: DecisionId) {
        let linked = self
            .decision_posts
            .range((decision, String::new())..)
            .take_while(|((id, _), _)| *id == decision);
        for (_, post) in linked {
            let channel = self.posts.get(post).map(|p| &p.content.channel);
            if let Some(state) = channel.and_then(|c| self.channels.get_mut(c)) {
                state.activity_seq = seq;
            }
        }
    }
}

fn session(id: Option<&SessionId>) -> Result<(), ApplyError> {
    if let Some(id) = id {
        valid_external_id("session id", id.as_str())?;
    }
    Ok(())
}

impl OrgState {
    #[must_use]
    pub fn post_presentation(&self, id: MessageId) -> Option<&PostPresentation> {
        self.channel.presentation.get(&id)
    }

    /// The current overlay on a linked wake; ordinary messages have none.
    #[must_use]
    pub fn wake_presentation(&self, content: &MessageContent) -> Option<&PostPresentation> {
        content.post.and_then(|id| self.post_presentation(id))
    }

    #[must_use]
    pub fn post_reactions(&self, id: MessageId) -> Option<&BTreeSet<(ReactionEmoji, Address)>> {
        self.channel.reactions.get(&id)
    }

    fn require_channel(&self, channel: &ChannelId) -> Result<(), ApplyError> {
        match channel {
            ChannelId::Project(project) => self.require_project(project),
            // Decoding never makes an out-of-order pair (its text is `Other`), so
            // only a hand-built one can arrive here; replay would key it apart.
            ChannelId::Direct(..) if !channel.is_known() => Err(ApplyError::InvalidId(IdError {
                kind: "channel id",
                value: channel.to_string(),
                rule: "a direct pair is two distinct addresses in ascending order",
            })),
            ChannelId::Direct(a, b) => {
                self.validate_address(a)?;
                self.validate_address(b)
            }
            ChannelId::Other(_) => Ok(()),
        }
    }

    /// A native post, or a message: a legacy post or a wake row.
    fn require_post_or_message(&self, id: &MessageId) -> Result<(), ApplyError> {
        if self.channel.posts.contains_key(id) || self.message(id).is_some() {
            Ok(())
        } else {
            Err(ApplyError::UnknownPost(*id))
        }
    }

    pub(crate) fn require_post(&self, id: &MessageId) -> Result<&Post, ApplyError> {
        self.channel
            .posts
            .get(id)
            .ok_or(ApplyError::UnknownPost(*id))
    }

    /// The reference checks of channel facts and of a wake row's post.
    pub(crate) fn validate_channel_event(&self, event: &OrgEvent) -> Result<(), ApplyError> {
        match event {
            OrgEvent::MessageSent(MessageContent {
                post: Some(post), ..
            }) => {
                self.require_post(post)?;
            }
            OrgEvent::ChannelPosted(post) => {
                self.require_channel(&post.channel)?;
                self.validate_address(&post.author)?;
                session(post.session_id.as_ref())?;
                if let Some(root) = &post.thread {
                    self.require_post_or_message(root)?;
                }
                for mention in &post.mentions {
                    match mention {
                        Mention::Seat(id) => {
                            self.require_agent(id)?;
                        }
                        Mention::Group { project, .. } => self.require_project(project)?,
                        Mention::Owner | Mention::Unknown(_) => {}
                    }
                }
                for to in &post.wake {
                    self.validate_address(to)?;
                }
                if let Some(step) = &post.decision {
                    self.require_decision(&step.decision_id)?;
                }
            }
            OrgEvent::ChannelRouted(answer) => {
                self.require_post(&answer.post)?;
                for to in &answer.wake {
                    self.validate_address(to)?;
                }
            }
            OrgEvent::ChannelSeen(seen) => {
                self.require_channel(&seen.channel)?;
                self.validate_address(&seen.reader)?;
                self.require_post_or_message(&seen.through)?;
                session(seen.session_id.as_ref())?;
            }
            OrgEvent::ChannelPostEdited(PostEdited { post, actor, .. })
            | OrgEvent::ChannelPostRemoved(PostRemoved { post, actor })
            | OrgEvent::ChannelReacted(PostReacted { post, actor, .. }) => {
                self.require_post(post)?;
                self.validate_address(actor)?;
            }
            _ => {}
        }
        Ok(())
    }
}
