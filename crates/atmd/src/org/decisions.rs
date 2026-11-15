//! Durable decisions and the single restart-recoverable answer-message bridge.
//! Policy stays in atm-org; answers use the existing confirmed-message runtime.

pub use super::decision_methods::register_decision_methods;
use super::{
    agent_project::AgentProjects,
    message_inbox::caller_address,
    messages::{MessageClock, MessageError},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::CallContext;
use atm_org::{
    Address, DecisionCommand, DecisionRequest, DecisionResolution, DecisionWithdrawal,
    MessageContent, MessageKind, OrgCommand, OrgState,
};
pub use atm_org::{Decision, DecisionClass, DecisionId, DecisionKind, DecisionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "decision_runtime_tests.rs"]
mod tests;

const BUDGET: Duration = Duration::from_secs(20);
const PAGE_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("invalid decision parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotFound(String),
    #[error("decision service unavailable: {0}")]
    Unavailable(String),
    #[error(
        "decision request timed out; accepted work may finish; inspect decisions before retrying"
    )]
    Timeout,
    #[error("decision operation failed: {0}")]
    Internal(String),
    #[error(transparent)]
    Organisation(#[from] OrgServiceError),
    #[error(transparent)]
    Caller(#[from] MessageError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestParams {
    pub kind: DecisionKind,
    pub title: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub files: Vec<String>,
    pub options: Vec<String>,
    pub recommended: Option<String>,
    pub may_be_undone: bool,
    pub changes_intent: bool,
    pub deadline: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestResult {
    pub decision_id: DecisionId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub statuses: Option<Vec<DecisionStatus>>,
    pub after: Option<DecisionId>,
    #[serde(default = "default_limit")]
    pub limit: u16,
    #[serde(default)]
    pub count_only: bool,
}
fn default_limit() -> u16 {
    100
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    pub id: DecisionId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    pub id: DecisionId,
    pub option: String,
    pub note: Option<String>,
    #[serde(default)]
    pub standing_rule: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawParams {
    pub id: DecisionId,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionEntry {
    #[serde(flatten)]
    pub decision: Decision,
    pub class: DecisionClass,
}
impl From<&Decision> for DecisionEntry {
    fn from(decision: &Decision) -> Self {
        Self {
            decision: decision.clone(),
            class: decision.class(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResult {
    pub decisions: Vec<DecisionEntry>,
    pub pending_count: usize,
    pub provisional_count: usize,
    pub next_after: Option<DecisionId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResult {
    pub decision: DecisionEntry,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResult {
    pub decision: DecisionEntry,
    pub changed: bool,
}

fn visible(decision: &Decision, caller: &Address) -> bool {
    *caller == Address::Owner || decision.request.requester == *caller
}
fn entry(state: &OrgState, id: DecisionId) -> Result<DecisionEntry, DecisionError> {
    state
        .decision(&id)
        .map(DecisionEntry::from)
        .ok_or_else(|| DecisionError::NotFound(format!("decision {id} was not found")))
}
fn bounded<T: Serialize>(value: T) -> Result<T, DecisionError> {
    if serde_json::to_vec(&value)
        .map_err(|e| DecisionError::Internal(e.to_string()))?
        .len()
        > PAGE_BYTES
    {
        return Err(DecisionError::Refused(
            "decision exceeds 524288 bytes".into(),
        ));
    }
    Ok(value)
}
fn list(
    state: &OrgState,
    caller: &Address,
    params: &ListParams,
) -> Result<ListResult, DecisionError> {
    let scope: Vec<_> = state
        .decisions()
        .filter(|d| params.count_only || visible(d, caller))
        .collect();
    let mut result = ListResult {
        decisions: vec![],
        pending_count: scope
            .iter()
            .filter(|d| d.status == DecisionStatus::Pending)
            .count(),
        provisional_count: scope
            .iter()
            .filter(|d| d.status == DecisionStatus::TakenProvisionally)
            .count(),
        next_after: None,
    };
    if params.count_only {
        return Ok(result);
    }
    let wanted = |d: &&Decision| {
        params.after.is_none_or(|after| d.id > after)
            && params.statuses.as_ref().map_or_else(
                || {
                    matches!(
                        d.status,
                        DecisionStatus::Pending | DecisionStatus::TakenProvisionally
                    )
                },
                |statuses| statuses.contains(&d.status),
            )
    };
    let mut bytes = 128; // Result framing and the longest canonical cursor.
    for decision in scope.into_iter().filter(wanted) {
        let row = DecisionEntry::from(decision);
        let size = serde_json::to_vec(&row)
            .map_err(|e| DecisionError::Internal(e.to_string()))?
            .len()
            + 1;
        if result.decisions.len() == usize::from(params.limit) || bytes + size > PAGE_BYTES {
            if result.decisions.is_empty() {
                return Err(DecisionError::Refused(format!(
                    "decision {} exceeds the list byte budget",
                    decision.id
                )));
            }
            result.next_after = result.decisions.last().map(|d| d.decision.id);
            break;
        }
        bytes += size;
        result.decisions.push(row);
    }
    bounded(result)
}

/// Canonical system answer: its durable msg.sent is the bridge's completion
/// marker, even after delivery failure/read. Status, not stale fields, decides.
fn resolution_message(decision: &Decision) -> Option<MessageContent> {
    let status = match decision.status {
        DecisionStatus::Resolved => "resolved",
        DecisionStatus::Overruled => "overruled",
        _ => return None,
    };
    let Address::Agent(_) = &decision.request.requester else {
        return None;
    };
    let answer = decision.resolution.as_ref()?;
    let mut body = format!("Decision {} {status}: {}", decision.id, answer.option);
    if let Some(note) = &answer.note {
        body.push_str(&format!("\nNote: {note}"));
    }
    body.push_str(if answer.standing_rule {
        "\nStanding rule: yes"
    } else {
        "\nStanding rule: no"
    });
    Some(MessageContent {
        from: Address::System,
        to: decision.request.requester.clone(),
        kind: MessageKind::Answer,
        body,
        attachments: vec![],
        task: None,
    })
}
fn missing_resolution_messages(state: &OrgState) -> Vec<MessageContent> {
    state
        .decisions()
        .filter_map(resolution_message)
        .filter(|content| !state.messages().any(|m| m.content == *content))
        .collect()
}

#[derive(Clone, Debug)]
pub struct DecisionHandle {
    pub(super) org: OrgHandle,
    pub(super) clock: MessageClock,
    health: watch::Receiver<Result<bool, String>>,
    requests: Arc<Semaphore>,
    /// The away switch's own configuration lane; None while disabled.
    pub(super) projects: Option<AgentProjects>,
}
impl DecisionHandle {
    pub fn disabled(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let (_, health) = watch::channel(Err(reason.clone()));
        Self {
            org: OrgHandle::disabled(reason),
            clock: MessageClock::new(tokio::time::Instant::now(), Utc::now()),
            health,
            requests: Arc::new(Semaphore::new(64)),
            projects: None,
        }
    }
    pub async fn initialized(&self) -> Result<(), DecisionError> {
        let mut health = self.health.clone();
        let state = health
            .wait_for(|state| !matches!(state, Ok(false)))
            .await
            .map_err(|_| DecisionError::Unavailable("answer bridge stopped".into()))?;
        state
            .clone()
            .map(|_| ())
            .map_err(DecisionError::Unavailable)
    }
    pub(super) async fn call<T>(
        &self,
        mutation: bool,
        work: impl Future<Output = Result<T, DecisionError>>,
    ) -> Result<T, DecisionError> {
        let _slot = self
            .requests
            .try_acquire()
            .map_err(|_| DecisionError::Unavailable("request limit reached".into()))?;
        tokio::time::timeout(BUDGET, async {
            if mutation {
                self.initialized().await?;
            }
            work.await
        })
        .await
        .map_err(|_| DecisionError::Timeout)?
    }
    pub async fn request(
        &self,
        p: RequestParams,
        caller: CallContext,
    ) -> Result<RequestResult, DecisionError> {
        self.call(true, async {
            let state = self.org.snapshot().await?;
            let requester = caller_address(&state, &caller)?;
            if requester == Address::Owner {
                return Err(DecisionError::Refused(
                    "only a named agent may request a decision".into(),
                ));
            }
            let events = self
                .org
                .execute(
                    OrgCommand::Decision(DecisionCommand::Request(DecisionRequest {
                        requester,
                        kind: p.kind,
                        title: p.title,
                        context: p.context,
                        files: p.files,
                        options: p.options,
                        recommended: p.recommended,
                        may_be_undone: p.may_be_undone,
                        changes_intent: p.changes_intent,
                        deadline: p.deadline,
                    })),
                    self.clock.now()?,
                )
                .await?;
            let record = events
                .first()
                .ok_or_else(|| DecisionError::Internal("request committed no event".into()))?;
            Ok(RequestResult {
                decision_id: DecisionId::new(record.seq),
            })
        })
        .await
    }
    pub async fn list(
        &self,
        p: ListParams,
        caller: CallContext,
    ) -> Result<ListResult, DecisionError> {
        self.call(false, async {
            let state = self.org.snapshot().await?;
            list(&state, &caller_address(&state, &caller)?, &p)
        })
        .await
    }
    pub async fn get(&self, p: GetParams, caller: CallContext) -> Result<GetResult, DecisionError> {
        self.call(false, async {
            let state = self.org.snapshot().await?;
            let row = entry(&state, p.id)?;
            if !visible(&row.decision, &caller_address(&state, &caller)?) {
                return Err(DecisionError::Refused(format!(
                    "only the requester or owner may read decision {}",
                    p.id
                )));
            }
            bounded(GetResult { decision: row })
        })
        .await
    }
    pub async fn resolve(
        &self,
        p: ResolveParams,
        caller: CallContext,
    ) -> Result<MutationResult, DecisionError> {
        self.call(true, async {
            let state = self.org.snapshot().await?;
            let by = caller_address(&state, &caller)?;
            if by != Address::Owner {
                return Err(DecisionError::Refused(
                    "only the owner may resolve a decision".into(),
                ));
            }
            self.mutate(
                p.id,
                DecisionCommand::Resolve(DecisionResolution {
                    decision_id: p.id,
                    option: p.option,
                    by,
                    note: p.note,
                    standing_rule: p.standing_rule,
                }),
                caller,
            )
            .await
        })
        .await
    }
    pub async fn withdraw(
        &self,
        p: WithdrawParams,
        caller: CallContext,
    ) -> Result<MutationResult, DecisionError> {
        self.call(true, async {
            let state = self.org.snapshot().await?;
            let by = caller_address(&state, &caller)?;
            self.mutate(
                p.id,
                DecisionCommand::Withdraw(DecisionWithdrawal {
                    decision_id: p.id,
                    by,
                    reason: p.reason,
                }),
                caller,
            )
            .await
        })
        .await
    }
    async fn mutate(
        &self,
        id: DecisionId,
        command: DecisionCommand,
        caller: CallContext,
    ) -> Result<MutationResult, DecisionError> {
        let events = self
            .org
            .execute_command(
                OrgCommand::Decision(command),
                self.clock.now()?,
                caller.from_pane.map(|p| p.0),
            )
            .await?;
        Ok(MutationResult {
            decision: entry(&self.org.snapshot().await?, id)?,
            changed: !events.is_empty(),
        })
    }
}

async fn reconcile(org: &OrgHandle, clock: &MessageClock) -> Result<(), DecisionError> {
    loop {
        let result = async {
            super::away::apply_pending(org, clock).await?;
            let state = org.snapshot().await?;
            for message in missing_resolution_messages(&state)
                .into_iter()
                .chain(super::away::missing_notices(&state))
            {
                org.execute(OrgCommand::SendMessage(message), clock.now()?)
                    .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Err(DecisionError::Organisation(OrgServiceError::Timeout)) => {
                // The actor is FIFO: the next snapshot includes any late enqueue.
                // Exact-content dedupe makes retry safe; a stopped actor still fails.
                tracing::warn!("Decision answer reconcile timed out; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            result => return result,
        }
    }
}

/// One bridge per server; watching before the snapshot makes overlap harmless
/// and closes the snapshot/subscription gap. No external delivery work runs here.
#[must_use]
pub fn spawn_decisions(
    org: OrgHandle,
    clock: MessageClock,
    cancel: CancellationToken,
) -> (DecisionHandle, JoinHandle<()>) {
    let watch = org
        .status()
        .and_then(|cursor| org.watch(vec!["decision.".into(), "owner.".into()], cursor));
    let mut watch = match watch {
        Ok(watch) => watch,
        Err(error) => {
            return (
                DecisionHandle::disabled(error.to_string()),
                tokio::spawn(async {}),
            )
        }
    };
    let (health, status) = watch::channel(Ok(false));
    // Detached: the lane stops with this bridge's token.
    let (projects, _) = AgentProjects::spawn(org.clone(), cancel.clone());
    let handle = DecisionHandle {
        org: org.clone(),
        clock: clock.clone(),
        health: status,
        requests: Arc::new(Semaphore::new(64)),
        projects: Some(projects),
    };
    let task = tokio::spawn(async move {
        let work = async {
            reconcile(&org, &clock).await?;
            let _ = health.send_replace(Ok(true));
            loop {
                let Some(_) = watch.next().await? else {
                    return Err(DecisionError::Unavailable("decision watch closed".into()));
                };
                reconcile(&org, &clock).await?;
            }
        };
        let result =
            tokio::select! { biased; () = cancel.cancelled() => Ok(()), result = work => result };
        let reason = match result {
            Ok(()) => "answer bridge stopped".into(),
            Err(error) => {
                tracing::warn!(%error, "Decision answer bridge stopped; restart to recover");
                error.to_string()
            }
        };
        let _ = health.send_replace(Err(reason));
    });
    (handle, task)
}
