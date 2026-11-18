//! Durable decisions and the single restart-recoverable answer-message bridge.
//! Policy stays in atm-org; answers use the existing confirmed-message runtime.

pub use super::decision_methods::register_decision_methods;
use super::{
    agent_project::AgentProjects,
    message_inbox::caller_address,
    messages::{MessageClock, MessageError},
    OrgHandle, OrgServiceError,
};
use crate::server::methods::{CallContext, MethodError};
use atm_org::{
    Address, AgentId, DecisionCommand, DecisionRequest, DecisionResolution, DecisionWithdrawal,
    MessageContent, MessageKind, OrgCommand, OrgState,
};
pub use atm_org::{Decision, DecisionClass, DecisionId, DecisionKind, DecisionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::{watch, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "decision_runtime_tests.rs"]
mod tests;

const BUDGET: Duration = Duration::from_secs(20);
pub(super) const PAGE_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("invalid decision parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_after: Option<atm_org::TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    pub id: DecisionId,
    pub option: String,
    pub note: Option<String>,
    #[serde(default)]
    pub standing_rule: bool,
    /// Typed serialization retains forbidden note/standing_rule keys; relay RPC
    /// callers must send only id/option/relay, as `atm decide --relay` does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relay: bool,
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
    /// The owner has said he is away. Present only while true, so a result
    /// from before the away switch, or while he is present, is unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub away: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResult {
    pub decision: DecisionEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo: Option<super::return_digest::RedoPage>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResult {
    pub decision: DecisionEntry,
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo: Option<super::return_digest::RedoPage>,
}

fn visible(decision: &Decision, caller: &Address, reports: &BTreeSet<AgentId>) -> bool {
    *caller == Address::Owner
        || decision.request.requester == *caller
        || matches!(&decision.request.requester, Address::Agent(id) if reports.contains(id))
}
fn reporting_error(error: MethodError) -> DecisionError {
    match error.0.code.as_str() {
        atm_protocol::error_code::REFUSED => DecisionError::Refused(error.0.message),
        atm_protocol::error_code::TIMEOUT => DecisionError::Timeout,
        atm_protocol::error_code::UNAVAILABLE => DecisionError::Unavailable(error.0.message),
        _ => DecisionError::Internal(error.0.message),
    }
}
fn entry(state: &OrgState, id: DecisionId) -> Result<DecisionEntry, DecisionError> {
    state
        .decision(&id)
        .map(DecisionEntry::from)
        .ok_or_else(|| DecisionError::NotFound(format!("decision {id} was not found")))
}
pub(super) fn bounded<T: Serialize>(value: T) -> Result<T, DecisionError> {
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
    reports: &BTreeSet<AgentId>,
    params: &ListParams,
) -> Result<ListResult, DecisionError> {
    let scope: Vec<_> = state
        .decisions()
        .filter(|d| params.count_only || visible(d, caller, reports))
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
        away: state.owner().away_since.is_some(),
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
        delivery_notice: None,
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
    reporting: super::reporting_line::ReportingFiles,
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
            reporting: super::reporting_line::ReportingFiles::default(),
        }
    }
    pub(crate) fn with_reporting_files(
        mut self,
        files: super::reporting_line::ReportingFiles,
    ) -> Self {
        self.reporting = files;
        self
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
    async fn visible_requesters(
        &self,
        state: &OrgState,
        caller: &Address,
    ) -> Result<BTreeSet<AgentId>, DecisionError> {
        let Address::Agent(caller) = caller else {
            return Ok(BTreeSet::new());
        };
        let requesters: BTreeSet<_> = state
            .decisions()
            .filter_map(|decision| match &decision.request.requester {
                Address::Agent(id) if id != caller => Some(id.clone()),
                _ => None,
            })
            .collect();
        let mut reports = BTreeSet::new();
        for id in requesters {
            match self.reporting.ancestors(state.clone(), id.clone()).await {
                Ok(ancestors) if ancestors.contains(caller) => {
                    reports.insert(id);
                }
                Ok(_) => (),
                Err(error) if error.0.code == atm_protocol::error_code::REFUSED => {
                    tracing::warn!(agent_id = %id, %error, "Reporting line unavailable for decision visibility");
                }
                Err(error) => return Err(reporting_error(error)),
            }
        }
        Ok(reports)
    }

    pub async fn list(
        &self,
        p: ListParams,
        caller: CallContext,
    ) -> Result<ListResult, DecisionError> {
        self.call(false, async {
            let state = self.org.snapshot().await?;
            let caller = caller_address(&state, &caller)?;
            let reports = if p.count_only {
                BTreeSet::new()
            } else {
                self.visible_requesters(&state, &caller).await?
            };
            list(&state, &caller, &reports, &p)
        })
        .await
    }
    pub async fn get(&self, p: GetParams, caller: CallContext) -> Result<GetResult, DecisionError> {
        self.call(false, async {
            let state = self.org.snapshot().await?;
            let row = entry(&state, p.id)?;
            let by = caller_address(&state, &caller)?;
            let explicit = p.redo_after.is_some() || p.redo_limit.is_some();
            if explicit && by != Address::Owner {
                return Err(DecisionError::Refused(
                    "only the owner may page redo tasks".into(),
                ));
            }
            if !visible(&row.decision, &by, &BTreeSet::new()) {
                let permitted = match (&by, &row.decision.request.requester) {
                    (Address::Agent(caller), Address::Agent(requester)) => self
                        .reporting
                        .ancestors(state.clone(), requester.clone())
                        .await
                        .map_err(reporting_error)?
                        .contains(caller),
                    _ => false,
                };
                if !permitted {
                    return Err(DecisionError::Refused(format!(
                        "only the requester or owner may read decision {}",
                        p.id
                    )));
                }
            }
            let mut result = GetResult {
                decision: row,
                redo: None,
            };
            if explicit || (by == Address::Owner && state.decision_redo(p.id).is_some()) {
                result.redo = Some(super::return_digest::redo_page(
                    &state,
                    p.id,
                    p.redo_after.as_ref(),
                    p.redo_limit.unwrap_or(100),
                    &result,
                )?);
            }
            bounded(result)
        })
        .await
    }
    pub async fn resolve(
        &self,
        p: ResolveParams,
        caller: CallContext,
    ) -> Result<MutationResult, DecisionError> {
        if p.relay && (p.note.is_some() || p.standing_rule) {
            return Err(DecisionError::InvalidParams(
                "relays cannot include note or standing_rule".into(),
            ));
        }
        let permit = self
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| DecisionError::Unavailable("request limit reached".into()))?;
        let handle = self.clone();
        // The admitted resolve/append chain, not its RPC waiter, owns capacity.
        // Dropping the waiter detaches this finite worker rather than cancelling
        // an append after the durable resolution. Filesystem work stays detached.
        let worker = tokio::spawn(async move {
            let _permit = permit;
            tokio::time::timeout(BUDGET, handle.initialized())
                .await
                .map_err(|_| DecisionError::Timeout)??;
            let state = handle.org.snapshot().await?;
            let by = caller_address(&state, &caller)?;
            let committed_error = |error: DecisionError| {
                DecisionError::Refused(format!(
                    "decision {} committed; result was not confirmed: {error}",
                    p.id
                ))
            };
            if p.relay {
                let by = match by {
                    Address::Agent(by) if state.owner().relay.as_ref() == Some(&by) => by,
                    _ => {
                        return Err(DecisionError::Refused(
                            "only the designated bound agent may relay a decision".into(),
                        ))
                    }
                };
                return bounded(
                    handle
                        .mutate(
                            p.id,
                            DecisionCommand::Relay {
                                decision_id: p.id,
                                option: p.option,
                                by,
                            },
                            caller,
                        )
                        .await?,
                )
                .map_err(committed_error);
            }
            if by != Address::Owner {
                return Err(DecisionError::Refused(
                    "only the owner may resolve a decision".into(),
                ));
            }
            let root = if p.standing_rule {
                Some(super::standing_rules::project_root(&state, p.id)?)
            } else {
                None
            };
            let result = handle
                .mutate(
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
                .await?;
            if let Some(root) = root {
                super::standing_rules::append(root, result.decision.decision.clone()).await
                    .map_err(|error| DecisionError::Unavailable(format!("decision {} committed; standing-rule append was not confirmed: {error}; retry the identical answer to repair it", p.id)))?;
            }
            let mut result = result;
            if result.decision.decision.status == DecisionStatus::Overruled {
                let state = handle.org.snapshot().await?;
                result.redo = Some(
                    super::return_digest::redo_page(&state, p.id, None, 100, &result)
                        .map_err(committed_error)?,
                );
            }
            bounded(result).map_err(committed_error)
        });
        tokio::time::timeout(BUDGET, worker)
            .await
            .map_err(|_| DecisionError::Timeout)?
            .map_err(|error| DecisionError::Internal(format!("resolution worker: {error}")))?
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
            redo: None,
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
                .chain(state.redo_notices())
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
        reporting: super::reporting_line::ReportingFiles::default(),
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

#[cfg(test)]
mod reporting_tests {
    use super::*;
    use atm_org::OrgEvent;

    // Break caught: paging before visibility or rejecting historical Unknown rows.
    #[test]
    fn reporting_decision_pages_keep_complete_historical_rows() {
        let mut state = OrgState::default();
        let now = Utc::now();
        for (seq, project) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
            state
                .apply(
                    seq as u64 + 1,
                    now,
                    &OrgEvent::ProjectRegistered {
                        project_id: project.parse().unwrap(),
                        root: format!("/{project}"),
                    },
                )
                .unwrap();
        }
        for (seq, id, pane) in [(4, "alpha/worker", "%1"), (5, "alpha/lead", "%2")] {
            state
                .apply(
                    seq,
                    now,
                    &OrgEvent::AgentAdopted {
                        agent_id: id.parse().unwrap(),
                        pane: pane.into(),
                        session_id: None,
                    },
                )
                .unwrap();
        }
        for (seq, requester, size) in [
            (6, Address::System, PAGE_BYTES),
            (7, "alpha/worker".parse().unwrap(), 200_000),
            (8, "alpha/worker".parse().unwrap(), 200_000),
            (9, "alpha/worker".parse().unwrap(), 200_000),
        ] {
            state
                .apply(
                    seq,
                    now,
                    &OrgEvent::DecisionRequested(DecisionRequest {
                        requester,
                        kind: DecisionKind::Unknown,
                        title: "Historical request".into(),
                        context: "x".repeat(size),
                        files: vec!["historical-proof.md".into()],
                        options: vec!["yes".into(), "no".into()],
                        recommended: Some("yes".into()),
                        may_be_undone: true,
                        changes_intent: true,
                        deadline: None,
                    }),
                )
                .unwrap();
        }
        let caller = "alpha/lead".parse().unwrap();
        let reports = BTreeSet::from(["alpha/worker".parse().unwrap()]);
        let params = ListParams {
            statuses: None,
            after: None,
            limit: 100,
            count_only: false,
        };
        let page = list(&state, &caller, &reports, &params).unwrap();
        assert_eq!(page.pending_count, 3);
        assert_eq!(
            page.decisions
                .iter()
                .map(|row| row.decision.id)
                .collect::<Vec<_>>(),
            [DecisionId::new(7), DecisionId::new(8)]
        );
        assert_eq!(page.next_after, Some(DecisionId::new(8)));
        assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
        assert_eq!(page.decisions[0].decision.request.context.len(), 200_000);
        assert_eq!(
            page.decisions[0].decision.request.kind,
            DecisionKind::Unknown
        );
        let next = list(
            &state,
            &caller,
            &reports,
            &ListParams {
                after: page.next_after,
                ..params
            },
        )
        .unwrap();
        assert_eq!(next.decisions.len(), 1);
        assert_eq!(next.decisions[0].decision.id, DecisionId::new(9));
        assert_eq!(next.next_after, None);
    }
}
