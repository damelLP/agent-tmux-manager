//! Named-agent projection and reservation-serialized reconciliation.
//!
//! Readiness is durable, never a list-only overlay. Unknown screens and absent
//! registry evidence suspend readiness without erasing the recorded binding.
//! Lifecycle effects remain unavailable until the flow slice is installed.

pub use super::agent_methods::register_agent_methods;
pub use super::agent_types::*;
use super::{agent_project::AgentProjects, OrgHandle, OrgServiceError};
use crate::control::reservation::{
    CaptureFormat, PaneId, Reservation, ReservationKind, ReservationRequest, WaitPolicy,
};
use crate::server::methods::CallContext;
use crate::{
    control::service::ControlHandle,
    registry::{RegistryHandle, SessionEvent},
};
use atm_core::{DialogState, Harness, InputState, ScreenFacts, SessionId, SessionView, TurnState};
use atm_org::{Agent, OrgCommand, OrgState};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{timeout, Instant},
};
use tokio_util::sync::CancellationToken;

const LIST_BUDGET: Duration = Duration::from_secs(5);
const OBSERVE_BUDGET: Duration = Duration::from_secs(30);

// A wake may wait both in the coalescer and in control admission. Retain only
// its identity context, never a readiness decision or an event-history mirror.
#[derive(Clone, Debug)]
struct BindingCandidate {
    session: SessionId,
    pane: Option<String>,
    binding: Option<SessionId>,
    excluded: Option<SessionId>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent operation '{operation}' is not implemented")]
    NotImplemented { operation: &'static str },
    #[error("invalid agent parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Refused(String),
    #[error("agent operation timed out; admitted effects may have completed; inspect agent list")]
    Timeout,
    #[error("agent service unavailable: {0}")]
    Unavailable(String),
    #[error("agent operation failed: {0}")]
    Internal(String),
    #[error(transparent)]
    Reservation(#[from] crate::control::reservation::ReservationError),
    #[error(transparent)]
    Organisation(#[from] OrgServiceError),
}

#[derive(Clone)]
pub(super) struct AgentRuntime {
    pub org: OrgHandle,
    pub registry: RegistryHandle,
    pub control: ControlHandle,
}

struct AgentService {
    disabled: Option<String>,
    runtime: Option<AgentRuntime>,
    projects: Option<AgentProjects>,
    ready: watch::Receiver<Option<Result<(), String>>>,
    lifecycle: Option<mpsc::Sender<LifecycleJob>>,
}

type Reply = Box<dyn FnOnce() + Send>;
struct LifecycleJob {
    agent: AgentId,
    deadline: Instant,
    admitted: oneshot::Sender<Result<(), AgentError>>,
    run: std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>,
}

impl std::fmt::Debug for AgentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentService")
            .field("disabled", &self.disabled)
            .field("ready", &*self.ready.borrow())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct AgentHandle {
    inner: Arc<AgentService>,
}

impl AgentHandle {
    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        let (_, ready) = watch::channel(None);
        Self {
            inner: Arc::new(AgentService {
                disabled: Some(reason.into()),
                runtime: None,
                projects: None,
                ready,
                lifecycle: None,
            }),
        }
    }

    fn unavailable(&self, operation: &'static str) -> AgentError {
        match &self.inner.disabled {
            Some(reason) => AgentError::Unavailable(reason.clone()),
            None if self.inner.ready.borrow().is_none() => {
                AgentError::Unavailable("agent reconciliation is in progress".into())
            }
            None => match &*self.inner.ready.borrow() {
                Some(Err(reason)) => AgentError::Unavailable(reason.clone()),
                _ => AgentError::NotImplemented { operation },
            },
        }
    }

    pub(super) async fn lifecycle<T, W, WF, C, CF>(
        &self,
        request: ReservationRequest,
        work: W,
        cleanup: C,
    ) -> Result<T, AgentError>
    where
        T: Send + 'static,
        W: FnOnce(Reservation) -> WF + Send + 'static,
        WF: std::future::Future<Output = Result<T, AgentError>> + Send + 'static,
        C: FnOnce(Reservation, crate::control::reservation::Completion) -> CF + Send + 'static,
        CF: std::future::Future<Output = Result<(), AgentError>> + Send + 'static,
    {
        match self.inner.ready.borrow().clone() {
            Some(Ok(())) => {}
            Some(Err(reason)) => return Err(AgentError::Unavailable(reason)),
            None => return Err(self.unavailable("lifecycle admission")),
        }
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable("lifecycle admission"))?;
        let queue = self
            .inner
            .lifecycle
            .as_ref()
            .ok_or_else(|| self.unavailable("lifecycle admission"))?;
        let control = rt.control.clone();
        let (admitted, admission) = oneshot::channel();
        let (reply, response) = oneshot::channel();
        queue
            .try_send(LifecycleJob {
                agent: request.agent.clone(),
                deadline: request.deadline,
                admitted,
                run: Box::pin(async move {
                    // Do not timeout/drop this waiting future: the slot covers the
                    // supervisor's cleanup acknowledgement, not just the work phase.
                    let result = control.run_reserved(request, work, cleanup).await;
                    Box::new(move || {
                        let _ = reply.send(result);
                    }) as Reply
                }),
            })
            .map_err(|_| {
                AgentError::Unavailable("lifecycle admission is full or stopped".into())
            })?;
        admission
            .await
            .map_err(|_| AgentError::Unavailable("lifecycle dispatcher stopped".into()))??;
        response
            .await
            .map_err(|_| AgentError::Unavailable("lifecycle dispatcher stopped".into()))?
    }

    pub async fn up(&self, p: UpParams, _caller: CallContext) -> Result<UpResult, AgentError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let prepared = self
            .projects("agent.up")?
            .prepare(p.project_root, p.name)
            .await?;
        self.unimplemented(
            ReservationRequest::new(
                prepared.agent_id,
                None,
                ReservationKind::Up,
                WaitPolicy::Wait,
                deadline,
            ),
            "agent.up",
        )
        .await
    }
    pub async fn adopt(&self, p: AdoptParams) -> Result<AdoptResult, AgentError> {
        let deadline = Instant::now() + OBSERVE_BUDGET;
        let projects = self.projects("agent.adopt")?;
        let pane = PaneId::parse(p.pane)?;
        let id = projects.identify(p.project_root, p.name).await?;
        self.unimplemented(
            ReservationRequest::new(
                id,
                Some(pane),
                ReservationKind::Adopt,
                WaitPolicy::Wait,
                deadline,
            ),
            "agent.adopt",
        )
        .await
    }
    fn projects(&self, operation: &'static str) -> Result<&AgentProjects, AgentError> {
        if !matches!(*self.inner.ready.borrow(), Some(Ok(()))) {
            return Err(self.unavailable(operation));
        }
        self.inner
            .projects
            .as_ref()
            .ok_or_else(|| self.unavailable(operation))
    }
    async fn unimplemented<T: Send + 'static>(
        &self,
        request: ReservationRequest,
        operation: &'static str,
    ) -> Result<T, AgentError> {
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(operation))?;
        let org = rt.org.clone();
        self.lifecycle(
            request,
            move |_| async move { Err(AgentError::NotImplemented { operation }) },
            move |_, _| async move {
                org.snapshot().await?;
                Ok(())
            },
        )
        .await
    }
    pub async fn clear(&self, p: ClearParams) -> Result<ClearResult, AgentError> {
        self.checkpoint(
            p.agent,
            ReservationKind::Clear,
            "agent.clear",
            Duration::from_millis(p.timeout_ms),
        )
        .await
    }
    pub async fn respawn(
        &self,
        p: RespawnParams,
        _caller: CallContext,
    ) -> Result<RespawnResult, AgentError> {
        self.checkpoint(
            p.agent,
            ReservationKind::Respawn,
            "agent.respawn",
            Duration::from_secs(60),
        )
        .await
    }
    pub async fn down(&self, p: DownParams) -> Result<DownResult, AgentError> {
        self.checkpoint(p.agent, ReservationKind::Down, "agent.down", OBSERVE_BUDGET)
            .await
    }
    async fn checkpoint<T: Send + 'static>(
        &self,
        id: AgentId,
        kind: ReservationKind,
        operation: &'static str,
        budget: Duration,
    ) -> Result<T, AgentError> {
        if !matches!(*self.inner.ready.borrow(), Some(Ok(()))) {
            return Err(self.unavailable(operation));
        }
        let deadline = Instant::now() + budget;
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(operation))?;
        let state = rt.org.snapshot().await?;
        let agent = state
            .agent(&id)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
        let pane = agent.pane.as_ref().map(PaneId::parse).transpose()?;
        let request = ReservationRequest::new(id, pane, kind, WaitPolicy::Wait, deadline);
        self.unimplemented(request, operation).await
    }
    pub async fn list(&self, p: ListParams) -> Result<ListResult, AgentError> {
        timeout(LIST_BUDGET, async {
            self.reconciled().await?;
            let rt = self
                .inner
                .runtime
                .as_ref()
                .ok_or_else(|| self.unavailable("agent.list"))?;
            let filter = match p.project_root {
                Some(root) => {
                    let projects = self
                        .inner
                        .projects
                        .as_ref()
                        .ok_or_else(|| self.unavailable("agent.list"))?;
                    match projects.filter(root).await? {
                        Some(id) => Some(id),
                        None => return Ok(Vec::new()),
                    }
                }
                None => None,
            };
            let state = rt.org.snapshot().await?;
            let rows = rt.rows().await?;
            Ok(state
                .agents()
                .filter(|a| filter.as_ref().is_none_or(|id| &a.id.project == id))
                .map(|a| list_entry(&state, a, &rows))
                .collect())
        })
        .await
        .map_err(|_| AgentError::Timeout)?
    }
    pub async fn reconciled(&self) -> Result<(), AgentError> {
        if let Some(reason) = &self.inner.disabled {
            return Err(AgentError::Unavailable(reason.clone()));
        }
        let mut ready = self.inner.ready.clone();
        timeout(LIST_BUDGET, async {
            loop {
                if let Some(result) = ready.borrow().clone() {
                    return result.map_err(AgentError::Unavailable);
                }
                ready
                    .changed()
                    .await
                    .map_err(|_| AgentError::Unavailable("agent service stopped".into()))?;
            }
        })
        .await
        .map_err(|_| AgentError::Timeout)?
    }
}

impl AgentRuntime {
    async fn rows(&self) -> Result<Vec<SessionView>, AgentError> {
        if !self.registry.is_connected() {
            return Err(AgentError::Unavailable("session registry stopped".into()));
        }
        let rows = timeout(LIST_BUDGET, self.registry.get_all_sessions())
            .await
            .map_err(|_| AgentError::Timeout)?;
        if !self.registry.is_connected() {
            return Err(AgentError::Unavailable("session registry stopped".into()));
        }
        Ok(rows)
    }

    async fn observe(&self, id: AgentId, bind: Option<BindingCandidate>) -> Result<(), AgentError> {
        let state = self.org.snapshot().await?;
        let Some(agent) = state.agent(&id) else {
            return Ok(());
        };
        if agent.status == AgentStatus::Down {
            return Ok(());
        }
        let pane = agent.pane.clone();
        let request = ReservationRequest::new(
            id.clone(),
            pane.as_ref().map(PaneId::parse).transpose()?,
            ReservationKind::Observe,
            WaitPolicy::Wait,
            Instant::now() + OBSERVE_BUDGET,
        );
        let rt = self.clone();
        let org = self.org.clone();
        self.control
            .run_reserved(
                request,
                move |r| async move {
                    // A queued observation never retains a ready decision across clear or
                    // respawn. A changed pane needs a newly acquired reservation.
                    let state = rt.org.snapshot().await?;
                    let Some(agent) = state.agent(&id) else {
                        return Ok(());
                    };
                    if agent.status == AgentStatus::Down || agent.pane != pane {
                        return Ok(());
                    }
                    let bind = bind.and_then(|candidate| {
                        (agent.pane == candidate.pane
                            && agent.session_id == candidate.binding
                            && agent.excluded_session_id == candidate.excluded)
                            .then_some(candidate.session)
                    });
                    rt.inspect(&r, &state, agent, bind).await
                },
                move |_, _| async move {
                    // FIFO behind accepted commands, including ones whose reply was lost.
                    // A failed read leaves outcome unknown and quarantines the reservation.
                    org.snapshot().await?;
                    Ok::<_, AgentError>(())
                },
            )
            .await
    }

    async fn inspect(
        &self,
        r: &Reservation,
        state: &OrgState,
        agent: &Agent,
        bind: Option<SessionId>,
    ) -> Result<(), AgentError> {
        // Registry failure is never evidence that every agent is gone.
        let rows = self.rows().await?;
        let gone = match &agent.pane {
            None => true,
            Some(pane) => match r.inspect(&PaneId::parse(pane)?).await? {
                None => true,
                Some(p) => p.dead || p.local_agent_tag.as_deref() != Some(&agent.id.to_string()),
            },
        };
        if gone {
            self.org
                .execute(
                    OrgCommand::Down {
                        agent_id: agent.id.clone(),
                        reason: "recorded pane is absent, dead or no longer owned".into(),
                    },
                    chrono::Utc::now(),
                )
                .await?;
            return Ok(());
        }
        if matches!(agent.status, AgentStatus::Clearing | AgentStatus::Stuck) {
            return Ok(());
        }
        if agent.status == AgentStatus::Ready {
            self.org
                .execute(
                    OrgCommand::Revalidate {
                        agent_id: agent.id.clone(),
                    },
                    chrono::Utc::now(),
                )
                .await?;
        }
        let Some(pane) = &agent.pane else {
            return Ok(());
        };
        let capture = r
            .capture(&PaneId::parse(pane)?, CaptureFormat::Plain)
            .await?;
        let harness = harness(state, agent, &rows);
        let facts = screen(harness, &capture.text);
        let rows = self.rows().await?;
        let live: Vec<_> = rows
            .iter()
            .filter(|row| matches_agent(agent, row) && row.harness == harness.short_tag())
            .collect();
        let parents = live.iter().filter(|row| !row.id.is_pending()).count();
        let bind = bind.filter(|id| !id.is_pending() && live.iter().any(|row| &row.id == id));
        let revalidating = agent.status == AgentStatus::Ready || agent.session_id.is_some();
        if (revalidating || bind.is_some())
            && !capture.inspection.dead
            && !capture.inspection.in_mode
            && capture.inspection.local_agent_tag.as_deref() == Some(&agent.id.to_string())
            && !live.is_empty()
            && parents <= 1
            && facts.input == InputState::Empty
            && facts.turn == TurnState::NotRunning
            && facts.dialog == DialogState::None
        {
            let command = match bind {
                Some(session_id) => OrgCommand::SessionBound {
                    agent_id: agent.id.clone(),
                    session_id,
                },
                None => OrgCommand::EditorReady {
                    agent_id: agent.id.clone(),
                },
            };
            self.org.execute(command, chrono::Utc::now()).await?;
        }
        Ok(())
    }
}

fn matches_agent(agent: &Agent, row: &SessionView) -> bool {
    agent.pane.is_some()
        && row.tmux_pane == agent.pane
        && row.parent_session_id.is_none()
        && agent.excluded_session_id.as_ref() != Some(&row.id)
}

fn harness(state: &OrgState, agent: &Agent, rows: &[SessionView]) -> Harness {
    agent
        .role
        .as_ref()
        .map(|r| r.config.harness)
        .or_else(|| state.agent_sessions(&agent.id).last().map(|s| s.harness))
        .unwrap_or_else(|| {
            let mut live = rows.iter().filter(|row| matches_agent(agent, row));
            let first = live.next().map(|row| row.harness.as_str());
            if live.all(|row| Some(row.harness.as_str()) == first) {
                first
                    .and_then(atm_core::find_harness_definition)
                    .map(|d| d.harness)
                    .unwrap_or_default()
            } else {
                Harness::Unknown
            }
        })
}

// One adapter dispatch boundary; all recognition stays in the adapter.
fn screen(harness: Harness, text: &str) -> ScreenFacts {
    match harness {
        Harness::ClaudeCode => atm_claude_adapter::screen::observe_screen(text, None),
        Harness::Codex => atm_codex_adapter::screen::observe_screen(text, None),
        Harness::Pi => atm_pi_adapter::screen::observe_screen(text, None),
        _ => ScreenFacts::default(),
    }
}

fn list_entry(state: &OrgState, agent: &Agent, rows: &[SessionView]) -> AgentListEntry {
    let expected = harness(state, agent, rows);
    let eligible: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches_agent(agent, r) && !r.id.is_pending() && r.harness == expected.short_tag()
        })
        .collect();
    let live = if eligible.len() == 1 {
        eligible
            .first()
            .copied()
            .filter(|r| agent.session_id.as_ref() == Some(&r.id))
    } else {
        None
    };
    let summaries = state.agent_sessions(&agent.id);
    let latest = summaries.last();
    AgentListEntry {
        agent_id: agent.id.clone(),
        state: agent.status,
        activity: live.map(|r| r.status),
        pane: agent.pane.clone(),
        session_id: agent
            .session_id
            .clone()
            .filter(|id| !id.is_pending() && agent.excluded_session_id.as_ref() != Some(id)),
        harness: live
            .map(|r| r.harness.clone())
            .or_else(|| latest.map(|s| s.harness.short_tag().into()))
            .or_else(|| {
                agent
                    .role
                    .as_ref()
                    .map(|r| r.config.harness.short_tag().into())
            }),
        model: live
            .map(|r| r.model.clone())
            .or_else(|| latest.and_then(|s| s.model.clone()))
            .or_else(|| agent.role.as_ref().and_then(|r| r.config.model.clone())),
        cost_usd: state.agent_cost(&agent.id).map(|c| c.as_usd()),
        reason: agent.reason.clone(),
    }
}

/// Starts reconciliation without delaying the daemon's hook listener.
#[must_use]
pub fn spawn_agents(
    org: OrgHandle,
    registry: RegistryHandle,
    control: ControlHandle,
    cancel: CancellationToken,
) -> (AgentHandle, JoinHandle<()>) {
    let rt = AgentRuntime {
        org,
        registry,
        control,
    };
    let (projects, project_task) = AgentProjects::spawn(rt.org.clone(), cancel.child_token());
    let (send, ready) = watch::channel(None);
    let (lifecycle, receive) = mpsc::channel(32);
    let dispatcher = tokio::spawn(dispatch_lifecycle(receive, cancel.child_token()));
    let handle = AgentHandle {
        inner: Arc::new(AgentService {
            disabled: None,
            runtime: Some(rt.clone()),
            projects: Some(projects),
            ready,
            lifecycle: Some(lifecycle),
        }),
    };
    // Subscribe before the initial snapshot: events arriving during inspection
    // remain wakeups, never cached readiness decisions.
    let events = rt.registry.subscribe();
    let committed = rt.org.committed.subscribe();
    let task = tokio::spawn(async move {
        let result = observer(rt, events, committed, &send, &cancel).await;
        let reason = result
            .err()
            .map_or_else(|| "agent service stopped".into(), |error| error.to_string());
        send.send_replace(Some(Err(reason)));
        cancel.cancel();
        let _ = project_task.await;
        let _ = dispatcher.await;
    });
    (handle, task)
}

async fn dispatch_lifecycle(mut requests: mpsc::Receiver<LifecycleJob>, cancel: CancellationToken) {
    let mut active = BTreeSet::new();
    let mut jobs = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = jobs.join_next(), if !jobs.is_empty() => match result {
                Some(Ok((id, reply))) => { active.remove(&id); (reply as Reply)(); },
                Some(Err(error)) => { tracing::error!(%error, "Lifecycle job stopped"); break; },
                None => {},
            },
            job = requests.recv() => {
                let Some(job) = job else { break; };
                let error = if active.contains(&job.agent) {
                    Some(AgentError::Conflict(format!("agent {} is busy; inspect agent list before retrying", job.agent)))
                } else if active.len() >= 32 {
                    Some(AgentError::Unavailable("lifecycle admission is full".into()))
                } else if Instant::now() >= job.deadline { Some(AgentError::Timeout) } else { None };
                if let Some(error) = error { let _ = job.admitted.send(Err(error)); continue; }
                active.insert(job.agent.clone());
                let _ = job.admitted.send(Ok(()));
                jobs.spawn(async move { (job.agent, job.run.await) });
            }
        }
    }
    requests.close();
    while let Some(result) = jobs.join_next().await {
        if let Ok((_, reply)) = result {
            reply();
        }
    }
}

async fn observer(
    rt: AgentRuntime,
    mut events: broadcast::Receiver<SessionEvent>,
    mut committed: broadcast::Receiver<Arc<crate::eventlog::Envelope>>,
    ready: &watch::Sender<Option<Result<(), String>>>,
    cancel: &CancellationToken,
) -> Result<(), AgentError> {
    let state = rt.org.snapshot().await?;
    let mut initial: BTreeSet<_> = state
        .agents()
        .filter(|a| a.status != AgentStatus::Down)
        .map(|a| a.id.clone())
        .collect();
    let mut pending: BTreeMap<_, Option<BindingCandidate>> =
        initial.iter().cloned().map(|id| (id, None)).collect();
    let mut active = BTreeSet::new();
    let mut jobs = JoinSet::new();
    let mut failure = None;
    loop {
        if initial.is_empty() && ready.borrow().is_none() {
            ready.send_replace(Some(failure.take().map_or(Ok(()), Err)));
        }
        while active.len() < 32 {
            let next = pending.keys().find(|id| !active.contains(*id)).cloned();
            let Some(id) = next else {
                break;
            };
            let bind = pending.remove(&id).flatten();
            let bind = if initial.contains(&id) { None } else { bind };
            active.insert(id.clone());
            let rt = rt.clone();
            jobs.spawn(async move {
                let result = rt.observe(id.clone(), bind).await;
                (id, result)
            });
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = jobs.join_next(), if !jobs.is_empty() => {
                match result {
                    Some(Ok((id, result))) => {
                        active.remove(&id);
                        let first = initial.remove(&id);
                        if let Err(error) = result {
                            if first && failure.is_none() { failure = Some(error.to_string()); }
                            tracing::warn!(agent = %id, %error, "Agent observation failed");
                        }
                    }
                    Some(Err(error)) => return Err(AgentError::Internal(format!("agent observer: {error}"))),
                    None => {},
                }
            }
            event = events.recv() => {
                let (id, bind) = match event {
                    Ok(SessionEvent::Registered { session_id, .. }) => (Some(session_id), true),
                    Ok(SessionEvent::Updated { session }) => (Some(session.id), false),
                    Ok(SessionEvent::Removed { session_id, .. }) => (Some(session_id), false),
                    Err(broadcast::error::RecvError::Lagged(_)) => (None, true),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                schedule(&rt, &mut pending, id, bind).await?;
            }
            event = committed.recv() => {
                match event {
                    Ok(event) if matches!(event.kind.as_str(), "agent.up_requested" | "agent.adopted" | "agent.pane_created") => schedule(&rt, &mut pending, None, false).await?,
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(_)) => schedule(&rt, &mut pending, None, false).await?,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    // Keep the control/org alive until all admitted Observe jobs finish cleanup.
    while jobs.join_next().await.is_some() {}
    Ok(())
}

async fn schedule(
    rt: &AgentRuntime,
    pending: &mut BTreeMap<AgentId, Option<BindingCandidate>>,
    event: Option<SessionId>,
    bind: bool,
) -> Result<(), AgentError> {
    let state = rt.org.snapshot().await?;
    let rows = rt.rows().await?;
    for agent in state.agents().filter(|a| a.status != AgentStatus::Down) {
        if event.as_ref().is_some_and(|id| {
            agent.session_id.as_ref() != Some(id)
                && !rows
                    .iter()
                    .any(|row| &row.id == id && matches_agent(agent, row))
        }) {
            continue;
        }
        let eligible: Vec<_> = rows
            .iter()
            .filter(|row| matches_agent(agent, row) && !row.id.is_pending())
            .collect();
        let candidate = if bind && eligible.len() == 1 {
            eligible.first().map(|row| BindingCandidate {
                session: row.id.clone(),
                pane: agent.pane.clone(),
                binding: agent.session_id.clone(),
                excluded: agent.excluded_session_id.clone(),
            })
        } else {
            None
        };
        // One dirty bit/candidate per agent; event floods never create one
        // reservation waiter per event. Acquire re-reads all authoritative state.
        pending
            .entry(agent.id.clone())
            .and_modify(|old| {
                if candidate.is_some() {
                    *old = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    Ok(())
}
