//! Confirmed-message schemas in the existing protocol 1.1 JSON envelope.
//!
//! Delivery is harness acceptance of the delivered prompt, not proof of reading
//! or execution. Read acknowledgment is separate. The executor chooses the prompt
//! representation; this contract does not prescribe inline text or a reference.

use atm_core::SessionId;
pub use atm_org::{
    Address, AgentId, DeliveryStatus, MessageContent, MessageId, MessageKind, TaskId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendParams {
    pub to: Address,
    #[serde(default = "default_message_kind")]
    pub kind: MessageKind,
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
}
#[must_use]
pub const fn default_message_kind() -> MessageKind {
    MessageKind::Note
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxParams {
    #[serde(rename = "for")]
    pub recipient: Option<Address>,
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default)]
    pub count_only: bool,
    pub id: Option<MessageId>,
    pub after: Option<MessageId>,
    #[serde(default = "default_inbox_limit")]
    pub limit: u16,
}
#[must_use]
pub const fn default_inbox_limit() -> u16 {
    100
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPhase {
    Queued,
    Waiting,
    Delivering,
    Blocked,
    Delivered,
    Failed,
}
/// Ephemeral progress, not a new durable message state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryView {
    pub phase: DeliveryPhase,
    pub reason: Option<String>,
    pub attempt: u32,
    pub pane: Option<String>,
    pub session_id: Option<SessionId>,
    pub confirmation: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
    pub blocked_by: Option<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub message_id: MessageId,
    pub content: MessageContent,
    pub sent_at: DateTime<Utc>,
    pub status: DeliveryStatus,
    pub read_at: Option<DateTime<Utc>>,
    pub delivery: DeliveryView,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxResult {
    pub messages: Vec<InboxEntry>,
    pub unread_count: usize,
    pub next_after: Option<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckParams {
    pub ids: Vec<MessageId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckResult {
    pub count: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    pub id: MessageId,
}
/// Owner retry keeps the ID and attempt history; it never enqueues a replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryResult {
    pub message_id: MessageId,
    pub status: DeliveryStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiveUpResult {
    pub message_id: MessageId,
    pub status: DeliveryStatus,
}
