//! Channel schemas in the existing protocol 1.1 envelope.

use atm_core::SessionId;
use atm_org::{
    Address, AgentId, AgentStatus, ChannelId, DecisionClass, DecisionId, DecisionStatus, Mention,
    MessageId, MessageKind, ProjectId, ReactionEmoji, TaskId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelListParams {
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelListResult {
    pub seq: u64,
    pub channels: Vec<ChannelRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Project,
    Direct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSection {
    Chief,
    Lead,
    Channel,
    Direct,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMember {
    pub address: Address,
    pub state: Option<AgentStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRow {
    pub id: ChannelId,
    pub kind: ChannelKind,
    pub project: Option<ProjectId>,
    pub section: ChannelSection,
    pub members: Vec<ChannelMember>,
    pub former: Vec<AgentId>,
    pub lead: Option<AgentId>,
    pub member: bool,
    pub writable: bool,
    pub last_post: Option<MessageId>,
    pub last_activity: Option<DateTime<Utc>>,
    pub activity_seq: u64,
    pub unread: usize,
    pub seen_through: Option<MessageId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelHistoryParams {
    pub channel: ChannelId,
    pub thread: Option<MessageId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_shared_replies: bool,
    pub before: Option<MessageId>,
    pub after: Option<MessageId>,
    #[serde(default = "default_history_limit")]
    pub limit: u16,
}

const fn default_history_limit() -> u16 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelHistoryResult {
    pub seq: u64,
    pub channel: ChannelId,
    pub posts: Vec<PostRow>,
    pub more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostRouting {
    None,
    Pending,
    Router,
    Stopgap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRow {
    pub id: MessageId,
    pub channel: ChannelId,
    pub thread: Option<MessageId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub also_in_chat: bool,
    pub author: Address,
    pub session_id: Option<SessionId>,
    pub session_started: Option<DateTime<Utc>>,
    pub kind: MessageKind,
    pub body: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<PostReaction>,
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
    pub mentions: Vec<Mention>,
    pub posted_at: DateTime<Utc>,
    pub replies: usize,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub routing: PostRouting,
    pub woke: Vec<Address>,
    pub legacy: bool,
    pub decision: Option<PostDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostReaction {
    pub emoji: ReactionEmoji,
    pub count: usize,
    pub mine: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostDecision {
    pub decision_id: DecisionId,
    pub step: String,
    pub status: DecisionStatus,
    pub options: Vec<String>,
    pub recommended: Option<String>,
    pub deadline: Option<DateTime<Utc>>,
    pub class: DecisionClass,
    pub answer: Option<PostDecisionAnswer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostDecisionAnswer {
    pub option: String,
    pub by: Address,
    pub note: Option<String>,
    pub relayed_by: Option<AgentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSeenParams {
    pub channel: ChannelId,
    pub through: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSeenResult {
    pub seq: u64,
    /// Whether this request advanced the marker. A late wake may still be read when false.
    pub changed: bool,
    pub through: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEditParams {
    pub post: MessageId,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelRemoveParams {
    pub post: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelReactParams {
    pub post: MessageId,
    pub emoji: ReactionEmoji,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMutationResult {
    pub seq: u64,
    pub post_id: MessageId,
}

/// `channel.post`: exactly one of `channel`, `to` and `reply_to`. What only the
/// daemon's own producers set (an internal post, a decision step, no routing)
/// has no field here, and unknown fields are refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelPostParams {
    #[serde(default)]
    pub channel: Option<ChannelId>,
    #[serde(default)]
    pub to: Option<Address>,
    #[serde(default)]
    pub reply_to: Option<MessageId>,
    #[serde(
        default,
        deserialize_with = "present_bool",
        skip_serializing_if = "not_true"
    )]
    pub also_in_chat: Option<bool>,
    pub body: String,
    #[serde(default = "crate::message_types::default_message_kind")]
    pub kind: MessageKind,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub task: Option<TaskId>,
    /// Strict tags, `name` or `project/name`: one that does not resolve
    /// refuses the post.
    #[serde(default)]
    pub tags: Vec<String>,
    /// A frontend's idempotency key: a repeat names the first post.
    #[serde(default)]
    pub client_key: Option<String>,
}

// Keep presence for admission, but don't accept a present null as "missing".
fn present_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(d).map(Some)
}

fn not_true(flag: &Option<bool>) -> bool {
    *flag != Some(true)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPostResult {
    pub seq: u64,
    pub post_id: MessageId,
    pub channel: ChannelId,
    pub thread: Option<MessageId>,
    pub mentions: Vec<Mention>,
    /// Each address the post wakes and its wake row.
    pub woke: Vec<WokeRow>,
    /// Woken seats that are down or stuck: their wake waits.
    pub unavailable: Vec<UnavailableRow>,
    pub route: PostRouting,
    /// Inline tags that stayed text.
    pub unresolved: Vec<UnresolvedRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WokeRow {
    pub to: Address,
    pub message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnavailableRow {
    pub to: AgentId,
    pub state: AgentStatus,
    pub since: DateTime<Utc>,
    pub message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedRow {
    pub text: String,
    pub reason: UnresolvedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    Malformed,
    Unknown,
    Ambiguous,
    EmptyGroup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterAttachParams {
    pub name: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterAttachResult {
    /// Start the channel.posted watch after this snapshot's cursor.
    pub seq: u64,
    pub timeout_secs: u64,
    pub pending: Vec<PendingRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRoute {
    pub post: MessageId,
    pub channel: ChannelId,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ChannelRouteParams {
    Wake {
        post: MessageId,
        wake: Vec<Address>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Decline {
        post: MessageId,
        decline: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRouteResult {
    pub seq: u64,
    pub woke: Vec<WokeRow>,
}
