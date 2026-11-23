//! Committed message history: one atomic fold/sequence snapshot and fresh CS2 admission.
use super::{
    agents::AgentError,
    message_inbox::{caller_address, project_entry, Progress, INBOX_PAGE_BYTES},
    message_types::InboxEntry,
    reporting_line::ReportingFiles,
    OrgHandle,
};
use crate::server::methods::{parse_params, MethodError, MethodRouter};
use atm_org::{
    message_query::{history, history_cursor, HistoryError, HistoryParams, HistoryScope},
    Address, OrgState,
};
use atm_protocol::error_code;
use serde::Serialize;
use std::time::Duration;

impl From<HistoryError> for MethodError {
    fn from(error: HistoryError) -> Self {
        let code = match error {
            HistoryError::InvalidParams(_) => error_code::INVALID_PARAMS,
            HistoryError::ForbiddenFilters => error_code::REFUSED,
        };
        MethodError::new(code, error.to_string())
    }
}

pub(crate) fn register(
    router: &mut MethodRouter,
    org: OrgHandle,
    reporting: ReportingFiles,
) -> Result<(), MethodError> {
    // Snapshot 5 s + shared reporting admission/read 5 s + 2 s margin.
    router.register(
        "msg.history",
        Duration::from_secs(12),
        move |caller, params| {
            let (org, reporting) = (org.clone(), reporting.clone());
            async move {
                let p = parse_params::<HistoryParams>(params)?;
                p.validate()?;
                let (state, seq) = org.snapshot_at().await.map_err(AgentError::from)?;
                let reader = caller_address(&state, &caller)?;
                let agent = p.agent.as_ref().or(match &reader {
                    Address::Agent(id) => Some(id),
                    _ => None,
                });
                if let Some(id) = agent {
                    if state.agent(id).is_none() {
                        return Err(MethodError::new(
                            error_code::NOT_FOUND,
                            format!("unknown agent {id}"),
                        ));
                    }
                }
                let scope = match (&reader, agent) {
                    (Address::Agent(reader), Some(agent)) if reader != agent => {
                        // Same complete current-role walk and retained slot as CS2.
                        let ancestors = reporting.ancestors(state.clone(), agent.clone()).await?;
                        if !ancestors.contains(reader) {
                            return Err(MethodError::new(
                                error_code::REFUSED,
                                "history is outside the caller's reporting scope",
                            ));
                        }
                        HistoryScope::DeliveryInbox {
                            reader: reader.clone(),
                            agent: agent.clone(),
                        }
                    }
                    (_, Some(agent)) => HistoryScope::Conversation {
                        reader: reader.clone(),
                        agent: agent.clone(),
                    },
                    _ => HistoryScope::All,
                };
                let result = page(&state, seq, &p, &scope)?;
                serde_json::to_value(result).map_err(internal)
            }
        },
    )
}

#[derive(Serialize)]
struct HistoryResult {
    seq: u64,
    snapshot_seq: u64,
    visibility: &'static str,
    messages: Vec<InboxEntry>,
    next_cursor: Option<String>,
}
fn internal(error: serde_json::Error) -> MethodError {
    MethodError::new(error_code::INTERNAL, error.to_string())
}
fn size(value: &impl Serialize) -> Result<usize, MethodError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(internal)
}
fn page(
    state: &OrgState,
    seq: u64,
    p: &HistoryParams,
    scope: &HistoryScope,
) -> Result<HistoryResult, MethodError> {
    let selected = history(state, p, scope, seq)?;
    let managed = matches!(scope, HistoryScope::DeliveryInbox { .. });
    let mut result = HistoryResult {
        seq,
        snapshot_seq: selected.snapshot_seq,
        visibility: if managed { "delivery" } else { "full" },
        messages: Vec::new(),
        next_cursor: None,
    };
    let mut bytes = size(&result)?;
    for (index, message) in selected.rows.iter().take(usize::from(p.limit)).enumerate() {
        // The shared allowlist runs before accounting; no hidden body/path sizes
        // can choose a metadata page boundary. Never consult scheduler progress.
        let entry = project_entry(state, message, managed, Progress::default());
        let cursor = if selected.rows.get(index + 1).is_some() {
            Some(history_cursor(p, scope, selected.snapshot_seq, message.id)?)
        } else {
            None
        };
        let entries_bytes = bytes + size(&entry)? + usize::from(index > 0);
        // Empty framing already includes `next_cursor:null`. Count the encoded
        // cursor string too (including JSON escaping), not just its raw bytes.
        if entries_bytes + size(&cursor)? - 4 > INBOX_PAGE_BYTES {
            if result.messages.is_empty() {
                return Err(MethodError::new(
                    error_code::REFUSED,
                    "first message exceeds the 512 KiB history result limit",
                ));
            }
            break;
        }
        bytes = entries_bytes;
        result.messages.push(entry);
        result.next_cursor = cursor;
    }
    Ok(result)
}
