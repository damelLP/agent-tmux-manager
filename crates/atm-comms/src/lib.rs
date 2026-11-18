//! Vendor-neutral durable message rows shared by atmd and chat.
//!
//! Thin for now: row schemas live here, while domain types remain in atm-org.

pub mod message_types;

pub use atm_core::SessionId;
pub use atm_org::message_query::HistoryParams;
pub use atm_org::{
    Address, AgentId, AgentStatus, DeliveryStatus, MessageContent, MessageId, MessageKind,
    ProjectId, TaskId,
};
