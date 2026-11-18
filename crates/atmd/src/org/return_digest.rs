//! Return-report RPC orchestration; policy and page models live in atm-org.
//! After committed Back/window selection and before CreateDigest is A3's future
//! prepare_return_digest(org, closed_window) seam. No drift task/model runs here.
use super::{
    away::{BackResult, SwitchParams},
    decisions::{bounded, DecisionError, DecisionHandle, PAGE_BYTES},
    message_inbox::caller_address,
};
use crate::server::methods::CallContext;
use atm_org::{
    digest_page, Address, DecisionId, DigestPageError, OrgCommand, OrgState, OwnerCommand, TaskId,
};
pub use atm_org::{DigestPage, DigestRow};
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "decision_redo_tests.rs"]
mod redo_tests;
#[cfg(test)]
#[path = "return_digest_tests.rs"]
mod tests;

fn default_limit() -> u16 {
    100
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestParams {
    pub snapshot_seq: u64,
    pub after: Option<u64>,
    #[serde(default = "default_limit")]
    pub limit: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedoPage {
    pub required_count: Option<usize>,
    pub completed_count: Option<usize>,
    pub cancelled_count: Option<usize>,
    pub task_ids: Vec<TaskId>,
    pub next_after: Option<TaskId>,
}
fn page_error(error: DigestPageError) -> DecisionError {
    match error {
        DigestPageError::InvalidLimit | DigestPageError::InvalidCursor(_) => {
            DecisionError::InvalidParams(error.to_string())
        }
        DigestPageError::Encoding(error) => DecisionError::Internal(error.to_string()),
        other => DecisionError::Refused(other.to_string()),
    }
}
fn budget(base: &impl Serialize, key_bytes: usize) -> Result<usize, DecisionError> {
    let bytes = serde_json::to_vec(base)
        .map_err(|e| DecisionError::Internal(e.to_string()))?
        .len();
    PAGE_BYTES
        .checked_sub(bytes.saturating_add(key_bytes))
        .ok_or_else(|| DecisionError::Refused("decision exceeds 524288 bytes".into()))
}
pub(super) fn redo_page(
    state: &OrgState,
    id: DecisionId,
    after: Option<&TaskId>,
    limit: u16,
    base: &impl Serialize,
) -> Result<RedoPage, DecisionError> {
    if !(1..=1000).contains(&limit) {
        return Err(DecisionError::InvalidParams(
            "redo_limit must be between 1 and 1000".into(),
        ));
    }
    let progress = state.decision_redo(id).ok_or_else(|| {
        DecisionError::InvalidParams("redo pagination requires an overrule".into())
    })?;
    let ids = progress.required.as_deref().unwrap_or_default();
    let start = match after {
        None => 0,
        Some(after) => ids
            .iter()
            .position(|id| id == after)
            .map(|index| index + 1)
            .ok_or_else(|| DecisionError::InvalidParams(format!("unknown redo cursor {after}")))?,
    };
    let measured = progress.required.is_some();
    let mut page = RedoPage {
        required_count: progress.required.as_ref().map(Vec::len),
        completed_count: measured.then_some(progress.completed.len()),
        cancelled_count: measured.then_some(progress.cancelled.len()),
        task_ids: vec![],
        next_after: None,
    };
    let budget = budget(base, 8)?;
    if serde_json::to_vec(&page)
        .map_err(|e| DecisionError::Internal(e.to_string()))?
        .len()
        > budget
    {
        return Err(DecisionError::Refused(format!(
            "decision {id} redo metadata exceeds 524288 bytes"
        )));
    }
    for (index, task) in ids.iter().enumerate().skip(start) {
        page.task_ids.push(task.clone());
        page.next_after = (index + 1 < ids.len()).then(|| task.clone());
        if serde_json::to_vec(&page)
            .map_err(|e| DecisionError::Internal(e.to_string()))?
            .len()
            > budget
        {
            page.task_ids.pop();
            if page.task_ids.is_empty() {
                return Err(DecisionError::Refused(format!(
                    "decision {id} redo row exceeds 524288 bytes"
                )));
            }
            page.next_after = page.task_ids.last().cloned();
            break;
        }
        if page.task_ids.len() == usize::from(limit) {
            break;
        }
    }
    Ok(page)
}
impl DecisionHandle {
    pub(super) async fn prepare_back(
        &self,
        _params: SwitchParams,
        caller: CallContext,
    ) -> Result<BackResult, DecisionError> {
        let before = self.org.snapshot().await?;
        let by = caller_address(&before, &caller)?;
        let events = self
            .org
            .execute(
                OrgCommand::Owner(OwnerCommand::Back { by: by.clone() }),
                self.clock.now()?,
            )
            .await?;
        let changed = events.iter().any(|e| e.kind == "owner.back");
        let state = self.org.snapshot().await?;
        let back_seq = events
            .iter()
            .find(|e| e.kind == "owner.back")
            .map(|e| e.seq)
            .or_else(|| state.last_away_window().map(|w| w.back_seq));
        let away_since = if changed {
            back_seq
                .and_then(|seq| state.away_window(seq))
                .map(|w| w.since)
        } else {
            None
        };
        // A3's future preparation hook belongs here, against this closed window.
        let created = self
            .org
            .execute(
                OrgCommand::Owner(OwnerCommand::CreateDigest { by, back_seq }),
                self.clock.now()?,
            )
            .await?;
        let seq = created
            .first()
            .ok_or_else(|| DecisionError::Internal("report committed no event".into()))?
            .seq;
        let state = self.org.snapshot().await?;
        let digest = state
            .return_digest()
            .filter(|d| d.snapshot_seq == seq)
            .ok_or_else(|| stale(seq))?;
        let mut result = BackResult {
            changed,
            away_since,
            provisional_count: digest.provisional_count,
            pending_count: digest.pending_count,
            digest: None,
        };
        result.digest =
            Some(digest_page(digest, None, 100, budget(&result, 10)?).map_err(page_error)?);
        bounded(result)
    }
    pub async fn digest(
        &self,
        params: DigestParams,
        caller: CallContext,
    ) -> Result<DigestPage, DecisionError> {
        if !(1..=1000).contains(&params.limit) {
            return Err(DecisionError::InvalidParams(
                "limit must be between 1 and 1000".into(),
            ));
        }
        self.call(false, async {
            let state = self.org.snapshot().await?;
            if caller_address(&state, &caller)? != Address::Owner {
                return Err(DecisionError::Refused(
                    "only the owner may read a return digest".into(),
                ));
            }
            let digest = state
                .return_digest()
                .filter(|d| d.snapshot_seq == params.snapshot_seq)
                .ok_or_else(|| stale(params.snapshot_seq))?;
            bounded(
                digest_page(digest, params.after, params.limit, PAGE_BYTES).map_err(page_error)?,
            )
        })
        .await
    }
}
fn stale(seq: u64) -> DecisionError {
    DecisionError::Conflict(format!(
        "return digest {seq} is no longer current; run atm back again"
    ))
}
