//! Bounded, read-only history over committed messages; no I/O or clock.
//!
//! Send sequence fences the candidate rows; delivery/read facts and presentation
//! are current. Cursors bind normalized filters and attributed reader/scope, not
//! authority: the daemon must admit the scope anew on every page.
//! Attachments are never read.
use crate::{
    Address, AgentId, Message, MessageContent, MessageId, OrgState, PostPresentation, TaskId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    pub agent: Option<AgentId>,
    pub task: Option<TaskId>,
    pub correspondent: Option<Address>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub text: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u16,
    pub cursor: Option<String>,
}
const fn default_limit() -> u16 {
    100
}
impl HistoryParams {
    pub fn validate(&self) -> Result<(), HistoryError> {
        let invalid = if !(1..=1000).contains(&self.limit) {
            Some("limit must be between 1 and 1000")
        } else if self
            .text
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            Some("text must contain 1 to 256 UTF-8 bytes")
        } else if self
            .task
            .as_ref()
            .is_some_and(|s| s.to_string().len() > 256)
        {
            Some("task exceeds 256 UTF-8 bytes")
        } else if self.cursor.as_ref().is_some_and(|s| s.len() > 4096) {
            Some("cursor exceeds 4096 UTF-8 bytes")
        } else if matches!((self.since, self.until), (Some(a), Some(b)) if a > b) {
            Some("since must not be after until")
        } else {
            None
        };
        invalid.map_or(Ok(()), |s| Err(HistoryError::InvalidParams(s)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryScope {
    All,
    Conversation { reader: Address, agent: AgentId },
    DeliveryInbox { reader: AgentId, agent: AgentId },
}
#[derive(Debug)]
pub struct HistorySelection<'a> {
    pub snapshot_seq: u64,
    /// Descending send sequence, at most limit + one lookahead. Borrowed, not copied.
    pub rows: Vec<&'a Message>,
}
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HistoryError {
    #[error("invalid history parameters: {0}")]
    InvalidParams(&'static str),
    #[error("task and text filters are refused for delivery-only history")]
    ForbiddenFilters,
}

// Effective agent is part of scope, so omitted and explicit self normalize alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    task: Option<TaskId>,
    correspondent: Option<Address>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    text: Option<String>,
}
impl From<&HistoryParams> for Filters {
    fn from(p: &HistoryParams) -> Self {
        Self {
            task: p.task.clone(),
            correspondent: p.correspondent.clone(),
            since: p.since,
            until: p.until,
            text: p.text.clone(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    fence: u64,
    last: MessageId,
    filters: Filters,
    scope: HistoryScope,
}
fn validate_scope(p: &HistoryParams, scope: &HistoryScope) -> Result<(), HistoryError> {
    p.validate()?;
    if matches!(scope, HistoryScope::DeliveryInbox { .. }) && (p.task.is_some() || p.text.is_some())
    {
        return Err(HistoryError::ForbiddenFilters);
    }
    let agent = match scope {
        HistoryScope::All => None,
        HistoryScope::Conversation { agent, .. } | HistoryScope::DeliveryInbox { agent, .. } => {
            Some(agent)
        }
    };
    if p.agent.as_ref().is_some_and(|a| Some(a) != agent) {
        return Err(HistoryError::InvalidParams("agent does not match scope"));
    }
    Ok(())
}
fn matches_scope(message: &Message, p: &HistoryParams, scope: &HistoryScope) -> bool {
    let c = &message.content;
    let addressed = match scope {
        HistoryScope::All => p
            .correspondent
            .as_ref()
            .is_none_or(|other| &c.from == other || &c.to == other),
        HistoryScope::Conversation { agent, .. } => {
            let who = Address::Agent(agent.clone());
            p.correspondent
                .as_ref()
                .map_or(c.from == who || c.to == who, |other| {
                    (c.from == who && &c.to == other) || (&c.from == other && c.to == who)
                })
        }
        HistoryScope::DeliveryInbox { agent, .. } => {
            c.to == Address::Agent(agent.clone())
                && p.correspondent
                    .as_ref()
                    .is_none_or(|other| &c.from == other)
        }
    };
    addressed
        && p.since.is_none_or(|since| message.sent_at >= since)
        && p.until.is_none_or(|until| message.sent_at < until)
}

fn matches_content(
    p: &HistoryParams,
    content: &MessageContent,
    presentation: Option<&PostPresentation>,
) -> bool {
    p.task.as_ref().is_none_or(|task| {
        !matches!(presentation, Some(PostPresentation::Removed))
            && content.task.as_ref() == Some(task)
    }) && p.text.as_ref().is_none_or(|text| {
        presentation
            .map_or(content.body.as_str(), PostPresentation::body)
            .contains(text)
    })
}

pub fn history<'a>(
    state: &'a OrgState,
    p: &HistoryParams,
    scope: &HistoryScope,
    seq: u64,
) -> Result<HistorySelection<'a>, HistoryError> {
    validate_scope(p, scope)?;
    let filter_content = p.task.is_some() || p.text.is_some();
    let cursor = p
        .cursor
        .as_ref()
        .map(|raw| {
            serde_json::from_str::<Cursor>(raw)
                .map_err(|_| HistoryError::InvalidParams("malformed cursor"))
        })
        .transpose()?;
    if let Some(c) = &cursor {
        let position = state.message(&c.last);
        if c.version != 1
            || c.fence > seq
            || c.last.seq() == 0
            || c.last.seq() > c.fence
            || c.filters != Filters::from(p)
            || &c.scope != scope
            || !position.is_some_and(|m| {
                // An edited or removed anchor is only an offset. It need not
                // still match, and checking its original text would expose it.
                matches_scope(m, p, scope)
                    && (!filter_content
                        || state.wake_presentation(&m.content).is_some()
                        || matches_content(p, &m.content, None))
            })
        {
            return Err(HistoryError::InvalidParams(
                "cursor does not match history query",
            ));
        }
    }
    let fence = cursor.as_ref().map_or(seq, |c| c.fence);
    // OrgState enumerates messages in ascending numeric sequence. Keep only the
    // newest bounded window, then reverse; no unbounded vector or copied bodies.
    let capacity = usize::from(p.limit) + 1;
    let mut rows = VecDeque::with_capacity(capacity);
    for message in state.messages().filter(|m| {
        m.id.seq() <= fence
            && cursor.as_ref().is_none_or(|c| m.id < c.last)
            && matches_scope(m, p, scope)
            && (!filter_content
                || matches_content(p, &m.content, state.wake_presentation(&m.content)))
    }) {
        if rows.len() == capacity {
            rows.pop_front();
        }
        rows.push_back(message);
    }
    Ok(HistorySelection {
        snapshot_seq: fence,
        rows: rows.into_iter().rev().collect(),
    })
}

pub fn history_cursor(
    p: &HistoryParams,
    scope: &HistoryScope,
    fence: u64,
    last: MessageId,
) -> Result<String, HistoryError> {
    validate_scope(p, scope)?;
    if last.seq() == 0 || last.seq() > fence {
        return Err(HistoryError::InvalidParams("invalid cursor position"));
    }
    let encoded = serde_json::to_string(&Cursor {
        version: 1,
        fence,
        last,
        filters: Filters::from(p),
        scope: scope.clone(),
    })
    .map_err(|_| HistoryError::InvalidParams("cursor could not be encoded"))?;
    if encoded.len() > 4096 {
        return Err(HistoryError::InvalidParams(
            "cursor exceeds 4096 UTF-8 bytes",
        ));
    }
    Ok(encoded)
}
