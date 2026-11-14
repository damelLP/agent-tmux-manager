//! Named-agent projection and reservation-serialized reconciliation.
//!
//! Readiness is durable, never a list-only overlay. Restart checks may suspend
//! bound Ready agents, preserving their binding; ordinary wakes never do. A
//! session-less Ready agent retains its lifecycle state even on an unsafe screen.
//! Delivery must independently check the screen before input.
//! A Stuck agent recovers only when one observation finds its session row and
//! captures a safe editor; a binding alone never clears Stuck.
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
    collections::{BTreeMap, BTreeSet, HashMap},
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
const MIN_OBSERVE_INTERVAL: Duration = Duration::from_millis(50);
// U8c has 32 active+parked slots. Observers use at most a quarter, leaving
// headroom for lifecycle work and deliveries even during restart or event lag.
const OBSERVER_CONCURRENCY: usize = 8;

// A wake may wait both in the coalescer and in control admission. Retain only
// its identity context, never a readiness decision or an event-history mirror.
#[derive(Debug)]
struct BindingCandidate {
    session: SessionId,
    pane: Option<String>,
    binding: Option<SessionId>,
    excluded: Option<SessionId>,
    cursor: u64,
    events: broadcast::Receiver<Arc<crate::eventlog::Envelope>>,
}

impl BindingCandidate {
    fn valid(&mut self, agent: &Agent) -> bool {
        if agent.pane != self.pane
            || agent.session_id != self.binding
            || agent.excluded_session_id != self.excluded
        {
            return false;
        }
        let id = agent.id.to_string();
        loop {
            match self.events.try_recv() {
                Ok(event)
                    if event.seq > self.cursor
                        && event.kind.starts_with("agent.")
                        && event
                            .body
                            .get("agent_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(id.as_str()) =>
                {
                    return false
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Empty) => return true,
                Err(_) => return false,
            }
        }
    }
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
    #[cfg(test)]
    progress: broadcast::Sender<ObservationProgress>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) struct ObservationProgress {
    pub phase: &'static str,
    pub agent: Option<AgentId>,
}

#[cfg(test)]
impl AgentRuntime {
    fn progress(&self, phase: &'static str, agent: Option<AgentId>) {
        let _ = self.progress.send(ObservationProgress { phase, agent });
    }
}

struct AgentService {
    disabled: Option<String>,
    runtime: Option<AgentRuntime>,
    projects: Option<AgentProjects>,
    ready: watch::Receiver<Option<Result<(), String>>>,
    errors: watch::Receiver<BTreeMap<AgentId, String>>,
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
    #[cfg(test)]
    pub(super) fn observation_events(&self) -> broadcast::Receiver<ObservationProgress> {
        self.inner.runtime.as_ref().unwrap().progress.subscribe()
    }

    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        let (_, ready) = watch::channel(None);
        Self {
            inner: Arc::new(AgentService {
                disabled: Some(reason.into()),
                runtime: None,
                projects: None,
                ready,
                errors: watch::channel(BTreeMap::new()).1,
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
        if let Some(reason) = self.inner.errors.borrow().get(&request.agent) {
            return Err(AgentError::Unavailable(format!(
                "{}: {reason}",
                request.agent
            )));
        }
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
                .map(|a| {
                    let mut entry = list_entry(&state, a, &rows);
                    if let Some(error) = self.inner.errors.borrow().get(&a.id) {
                        entry.reason = Some(match entry.reason {
                            Some(reason) => format!("{reason}; observation unavailable: {error}"),
                            None => format!("observation unavailable: {error}"),
                        });
                    }
                    entry
                })
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

    async fn observe(
        &self,
        id: AgentId,
        bind: Option<BindingCandidate>,
        initial: bool,
    ) -> Result<(), AgentError> {
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
                    #[cfg(test)]
                    rt.progress("entered", Some(id.clone()));
                    // A queued observation never retains a ready decision across clear or
                    // respawn. A changed pane needs a newly acquired reservation.
                    let state = rt.org.snapshot().await?;
                    let Some(agent) = state.agent(&id) else {
                        return Ok(());
                    };
                    if agent.status == AgentStatus::Down || agent.pane != pane {
                        return Ok(());
                    }
                    let bind = bind.filter(|candidate| {
                        agent.pane == candidate.pane
                            && agent.session_id == candidate.binding
                            && agent.excluded_session_id == candidate.excluded
                    });
                    rt.inspect(&r, &state, agent, bind, initial).await
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
        bind: Option<BindingCandidate>,
        initial: bool,
    ) -> Result<(), AgentError> {
        // Registry failure is never evidence that every agent is gone.
        self.rows().await?;
        let mut in_mode = false;
        let gone = match &agent.pane {
            None => true,
            Some(pane) => match r.inspect(&PaneId::parse(pane)?).await? {
                None => true,
                Some(p) => {
                    in_mode = p.in_mode;
                    p.dead || p.local_agent_tag.as_deref() != Some(&agent.id.to_string())
                }
            },
        };
        if gone {
            self.org
                .execute(
                    OrgCommand::Down {
                        agent_id: agent.id.clone(),
                        reason: match agent.reason.as_ref().filter(|_| agent.status == AgentStatus::Stuck) {
                            Some(reason) => format!("recorded pane is absent, dead or no longer owned; previous: {reason}"),
                            None => "recorded pane is absent, dead or no longer owned".into(),
                        },
                    },
                    chrono::Utc::now(),
                )
                .await?;
            return Ok(());
        }
        if let Some(pane) = &agent.pane {
            let reason = format!("clear interrupted by daemon restart; inspect {pane}");
            if (initial && agent.status == AgentStatus::Clearing)
                || (agent.status == AgentStatus::Stuck && agent.reason.as_ref() == Some(&reason))
            {
                if agent.status == AgentStatus::Clearing {
                    self.org
                        .execute(
                            OrgCommand::Stuck {
                                agent_id: agent.id.clone(),
                                reason: reason.clone(),
                            },
                            chrono::Utc::now(),
                        )
                        .await?;
                }
                let content = atm_org::MessageContent {
                    from: atm_org::Address::System,
                    to: atm_org::Address::Owner,
                    kind: atm_org::MessageKind::Note,
                    body: format!("agent {}: {reason}", agent.id),
                    attachments: Vec::new(),
                    task: None,
                };
                // Repair a partial Stuck/Note prefix, without duplicating a note
                // whose append completed after its caller timed out. A note sent
                // before the agent's last change belongs to an earlier incident.
                if !state.messages().any(|message| {
                    message.content == content && message.sent_at >= agent.updated_at
                }) {
                    self.org
                        .execute(OrgCommand::SendMessage(content), chrono::Utc::now())
                        .await?;
                }
            }
        }
        if agent.status == AgentStatus::Clearing {
            return Ok(());
        }
        let rows = self.rows().await?;
        let expected = harness(state, agent, &rows);
        let eligible: Vec<_> = rows
            .iter()
            .filter(|row| {
                matches_agent(agent, row)
                    && !row.id.is_pending()
                    && row.harness == expected.short_tag()
            })
            .collect();
        // The single eligible row this wake may bind, checked again at the write.
        let mut binding = bind.filter(|candidate| {
            !in_mode
                && eligible.len() == 1
                && eligible
                    .first()
                    .is_some_and(|row| row.id == candidate.session)
        });
        // Binding is an identity event and needs no screen read, except to leave
        // Stuck: that takes this observation's own capture of a safe editor.
        let stuck = agent.status == AgentStatus::Stuck;
        if !stuck {
            if let Some(candidate) = binding.as_mut() {
                if candidate.valid(agent) {
                    self.org
                        .execute(
                            OrgCommand::SessionBound {
                                agent_id: agent.id.clone(),
                                session_id: candidate.session.clone(),
                            },
                            chrono::Utc::now(),
                        )
                        .await?;
                    return Ok(());
                }
            }
            binding = None;
        }
        // A capture can change nothing for a Ready agent on an ordinary wake, or
        // for a session-less one at the restart check: don't read, or fail on, it.
        if (stuck && binding.is_none())
            || (agent.status == AgentStatus::Ready && (!initial || agent.session_id.is_none()))
        {
            return Ok(());
        }
        let Some(pane) = &agent.pane else {
            return Ok(());
        };
        #[cfg(test)]
        self.progress("capture", Some(agent.id.clone()));
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
        let safe = !capture.inspection.dead
            && !capture.inspection.in_mode
            && capture.inspection.local_agent_tag.as_deref() == Some(&agent.id.to_string())
            && !live.is_empty()
            && parents <= 1
            && facts.input == InputState::Empty
            && facts.turn == TurnState::NotRunning
            && facts.dialog == DialogState::None;
        if let Some(mut candidate) = binding {
            // A dialog, a running turn or an unknown screen keeps Stuck and its reason.
            if safe && live.iter().any(|row| row.id == candidate.session) && candidate.valid(agent)
            {
                self.org
                    .execute(
                        OrgCommand::SessionBound {
                            agent_id: agent.id.clone(),
                            session_id: candidate.session.clone(),
                        },
                        chrono::Utc::now(),
                    )
                    .await?;
            }
            return Ok(());
        }
        let revalidating = agent.status == AgentStatus::Ready || agent.session_id.is_some();
        if revalidating && safe {
            if agent.status != AgentStatus::Ready {
                self.org
                    .execute(
                        OrgCommand::EditorReady {
                            agent_id: agent.id.clone(),
                        },
                        chrono::Utc::now(),
                    )
                    .await?;
            }
        } else if initial && agent.status == AgentStatus::Ready && agent.session_id.is_some() {
            self.org
                .execute(
                    OrgCommand::Revalidate {
                        agent_id: agent.id.clone(),
                    },
                    chrono::Utc::now(),
                )
                .await?;
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

// One adapter dispatch boundary; future lifecycle flows reuse this rather than
// adding vendor branches. This base's adapters accept Plain/None; consuming their
// styled capture policy waits for the adapter contract, not a local guess.
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
        #[cfg(test)]
        progress: broadcast::channel(4096).0,
    };
    let (projects, project_task) = AgentProjects::spawn(rt.org.clone(), cancel.child_token());
    let (send, ready) = watch::channel(None);
    let (errors, error_state) = watch::channel(BTreeMap::new());
    let (lifecycle, receive) = mpsc::channel(32);
    let dispatcher = tokio::spawn(dispatch_lifecycle(receive, cancel.child_token()));
    let handle = AgentHandle {
        inner: Arc::new(AgentService {
            disabled: None,
            runtime: Some(rt.clone()),
            projects: Some(projects),
            ready,
            errors: error_state,
            lifecycle: Some(lifecycle),
        }),
    };
    // Subscribe before the initial snapshot: events arriving during inspection
    // remain wakeups, never cached readiness decisions.
    let events = rt.registry.subscribe();
    let committed = rt.org.committed.subscribe();
    let task = tokio::spawn(async move {
        let result = observer(rt, events, committed, &send, &errors, &cancel).await;
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

// Routing only: no OrgState/message history or cached readiness authority.
struct AgentRoute {
    pane: Option<String>,
    binding: Option<SessionId>,
    excluded: Option<SessionId>,
    harness: Option<Harness>,
}

impl AgentRoute {
    fn eligible(&self, row: &SessionView) -> bool {
        self.pane.is_some()
            && self.pane == row.tmux_pane
            && row.parent_session_id.is_none()
            && self.excluded.as_ref() != Some(&row.id)
            && self.harness.is_none_or(|h| row.harness == h.short_tag())
    }
}

#[derive(Default)]
struct AgentRoutes {
    agents: BTreeMap<AgentId, AgentRoute>,
    rows: HashMap<SessionId, String>,
    dirty: bool,
}

impl AgentRoutes {
    fn refresh(&mut self, state: &OrgState) {
        self.agents = state
            .agents()
            .filter(|a| a.status != AgentStatus::Down)
            .map(|a| {
                (
                    a.id.clone(),
                    AgentRoute {
                        pane: a.pane.clone(),
                        binding: a.session_id.clone(),
                        excluded: a.excluded_session_id.clone(),
                        harness: a.role.as_ref().map(|r| r.config.harness),
                    },
                )
            })
            .collect();
        self.rows.retain(|id, pane| {
            self.agents
                .values()
                .any(|a| a.pane.as_ref() == Some(pane) && a.excluded.as_ref() != Some(id))
        });
        self.dirty = false;
    }

    fn affected(&mut self, id: &SessionId, row: Option<&SessionView>) -> BTreeSet<AgentId> {
        let old = self.rows.remove(id);
        let pane = row.and_then(|row| row.tmux_pane.as_ref());
        let affected = self
            .agents
            .iter()
            .filter(|(_, route)| {
                route.excluded.as_ref() != Some(id)
                    && (route.binding.as_ref() == Some(id)
                        || route.pane.as_ref().is_some_and(|p| Some(p) == old.as_ref())
                        || row.is_some_and(|row| route.eligible(row)))
            })
            .map(|(id, _)| id.clone())
            .collect();
        if let Some(pane) = pane.filter(|_| {
            row.is_some_and(|row| self.agents.values().any(|route| route.eligible(row)))
        }) {
            self.rows.insert(id.clone(), pane.clone());
        }
        affected
    }

    async fn rebuild(&mut self, rt: &AgentRuntime) -> Result<BTreeSet<AgentId>, AgentError> {
        self.dirty = true;
        let state = rt.org.snapshot().await?;
        let rows = rt.rows().await?;
        self.refresh(&state);
        self.rows.clear();
        for row in rows {
            self.affected(&row.id, Some(&row));
        }
        Ok(self.agents.keys().cloned().collect())
    }

    async fn wake(
        &mut self,
        rt: &AgentRuntime,
        event: Result<SessionEvent, broadcast::error::RecvError>,
    ) -> Result<BTreeSet<AgentId>, AgentError> {
        if self.dirty || matches!(event, Err(broadcast::error::RecvError::Lagged(_))) {
            return self.rebuild(rt).await;
        }
        match event {
            Ok(SessionEvent::Updated { session }) => Ok(self.affected(&session.id, Some(&session))),
            Ok(SessionEvent::Removed { session_id, .. }) => Ok(self.affected(&session_id, None)),
            Ok(SessionEvent::Registered { session_id, .. }) => {
                if self.agents.values().all(|a| a.pane.is_none()) {
                    return Ok(BTreeSet::new());
                }
                // Registered has no pane. Look up only this row, before cloning
                // any organisation state or dumping the entire registry.
                let row = timeout(LIST_BUDGET, rt.registry.get_session(session_id.clone()))
                    .await
                    .map_err(|_| AgentError::Timeout)?;
                Ok(self.affected(&session_id, row.as_ref()))
            }
            Err(_) => Ok(BTreeSet::new()),
        }
    }
}

async fn observer(
    rt: AgentRuntime,
    mut events: broadcast::Receiver<SessionEvent>,
    mut committed: broadcast::Receiver<Arc<crate::eventlog::Envelope>>,
    ready: &watch::Sender<Option<Result<(), String>>>,
    errors: &watch::Sender<BTreeMap<AgentId, String>>,
    cancel: &CancellationToken,
) -> Result<(), AgentError> {
    let state = loop {
        match rt.org.snapshot().await {
            Ok(state) => break state,
            Err(error) => tracing::warn!(%error, "Agent initialization deferred until next wake"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            event = events.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return Ok(()); },
            event = committed.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return Ok(()); },
        }
    };
    let mut initial: BTreeSet<_> = state
        .agents()
        .filter(|a| a.status != AgentStatus::Down)
        .map(|a| a.id.clone())
        .collect();
    let mut pending: BTreeMap<_, Option<BindingCandidate>> =
        initial.iter().cloned().map(|id| (id, None)).collect();
    let mut active = BTreeSet::new();
    let mut jobs = JoinSet::new();
    let mut restart = initial.clone();
    let mut next_observation = BTreeMap::new();
    let mut routes = AgentRoutes::default();
    routes.refresh(&state);
    if let Ok(rows) = rt.rows().await {
        for row in rows {
            routes.affected(&row.id, Some(&row));
        }
    }
    loop {
        if initial.is_empty() && ready.borrow().is_none() {
            ready.send_replace(Some(Ok(())));
        }
        // One clock read per pass: an agent due between two reads would be
        // neither started nor timed, and wait for an unrelated event.
        let now = Instant::now();
        while active.len() < OBSERVER_CONCURRENCY {
            let next = pending
                .keys()
                .find(|id| {
                    !active.contains(*id) && next_observation.get(*id).is_none_or(|at| *at <= now)
                })
                .cloned();
            let Some(id) = next else {
                break;
            };
            let bind = pending.remove(&id).flatten();
            let first = restart.contains(&id);
            let bind = if first { None } else { bind };
            active.insert(id.clone());
            #[cfg(test)]
            rt.progress("started", Some(id.clone()));
            let rt = rt.clone();
            jobs.spawn(async move {
                let result = rt.observe(id.clone(), bind, first).await;
                (id, result)
            });
        }
        let next_wake = pending
            .keys()
            .filter(|id| !active.contains(*id))
            .filter_map(|id| next_observation.get(id).copied())
            .filter(|at| *at > now)
            .min();
        #[cfg(test)]
        rt.progress("dispatched", None);
        #[cfg(test)]
        if active.is_empty() && pending.is_empty() {
            rt.progress("idle", None);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = jobs.join_next(), if !jobs.is_empty() => {
                match result {
                    Some(Ok((id, result))) => {
                        active.remove(&id);
                        next_observation.insert(id.clone(), Instant::now() + MIN_OBSERVE_INTERVAL);
                        #[cfg(test)]
                        rt.progress("finished", Some(id.clone()));
                        initial.remove(&id);
                        match result {
                            Err(error) => {
                                errors.send_modify(|errors| { errors.insert(id.clone(), error.to_string()); });
                                tracing::warn!(agent = %id, %error, "Agent observation failed");
                            }
                            Ok(()) => {
                                restart.remove(&id);
                                errors.send_modify(|errors| { errors.remove(&id); });
                            }
                        }
                    }
                    Some(Err(error)) => return Err(AgentError::Internal(format!("agent observer: {error}"))),
                    None => {},
                }
            }
            _ = async { if let Some(at) = next_wake { tokio::time::sleep_until(at).await; } }, if next_wake.is_some() => {},
            event = events.recv() => {
                if matches!(event, Err(broadcast::error::RecvError::Closed)) { break; }
                #[cfg(test)]
                rt.progress("scheduling", None);
                let result = match routes.wake(&rt, event).await {
                    Ok(ids) => schedule(&rt, &mut pending, ids, true).await,
                    Err(error) => Err(error),
                };
                #[cfg(test)]
                rt.progress(if result.is_ok() { "scheduled" } else { "schedule_failed" }, None);
                if let Err(error) = result {
                    tracing::warn!(%error, "Agent scheduling failed; waiting for next wake");
                }
            }
            event = committed.recv() => {
                match event {
                    Ok(event) if event.kind.starts_with("agent.") => {
                        routes.dirty = true;
                        match rt.org.snapshot().await {
                            Ok(state) => routes.refresh(&state),
                            Err(error) => tracing::warn!(%error, "Agent routing refresh deferred"),
                        }
                        if matches!(event.kind.as_str(), "agent.up_requested" | "agent.adopted" | "agent.pane_created") {
                            if let Some(id) = event.body.get("agent_id").and_then(serde_json::Value::as_str).and_then(|id| id.parse().ok()) {
                                if let Err(error) = schedule(&rt, &mut pending, BTreeSet::from([id]), false).await {
                                    tracing::warn!(%error, "Agent scheduling failed; waiting for next wake");
                                }
                            }
                            #[cfg(test)]
                            rt.progress("committed_scheduled", None);
                        }
                    },
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let result = match routes.rebuild(&rt).await {
                            Ok(ids) => schedule(&rt, &mut pending, ids, true).await,
                            Err(error) => Err(error),
                        };
                        if let Err(error) = result { tracing::warn!(%error, "Agent rescan deferred until next wake"); }
                    },
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
    ids: BTreeSet<AgentId>,
    bind: bool,
) -> Result<(), AgentError> {
    for id in ids {
        // Subscribe before the FIFO snapshot. A complete clear can leave the
        // identity tuple unchanged, and equal timestamps are not a fence.
        let events = rt.org.committed.subscribe();
        let cursor = rt.org.status()?;
        let current = rt.org.snapshot().await?;
        let Some(agent) = current.agent(&id).filter(|a| a.status != AgentStatus::Down) else {
            continue;
        };
        let candidate = if bind {
            let current_rows = rt.rows().await?;
            let expected = harness(&current, agent, &current_rows);
            let eligible: Vec<_> = current_rows
                .iter()
                .filter(|row| {
                    matches_agent(agent, row)
                        && !row.id.is_pending()
                        && row.harness == expected.short_tag()
                })
                .collect();
            if eligible.len() == 1 {
                eligible
                    .first()
                    .filter(|row| {
                        agent.session_id.as_ref() != Some(&row.id)
                            || agent.status == AgentStatus::Stuck
                    })
                    .map(|row| BindingCandidate {
                        session: row.id.clone(),
                        pane: agent.pane.clone(),
                        binding: agent.session_id.clone(),
                        excluded: agent.excluded_session_id.clone(),
                        cursor,
                        events,
                    })
            } else {
                None
            }
        } else {
            None
        };
        // One dirty bit/candidate per agent; event floods never create one
        // reservation waiter per event. Acquire re-reads all authoritative state.
        if candidate.is_some() || !pending.contains_key(&agent.id) {
            pending.insert(agent.id.clone(), candidate);
        }
    }
    Ok(())
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    #[test]
    fn lifecycle_fence_uses_commits_not_timestamps_and_refuses_lag() {
        let agent = Agent {
            id: "p/lead".parse().unwrap(),
            role: None,
            status: AgentStatus::Ready,
            pane: Some("%1".into()),
            session_id: None,
            excluded_session_id: Some(SessionId::new("old")),
            reason: None,
            updated_at: chrono::Utc::now(),
        };
        for (kind, id, seq, valid) in [
            ("agent.clear_requested", "p/lead", 11, false),
            ("agent.down", "p/lead", 11, false),
            ("agent.up_requested", "p/lead", 11, false),
            ("agent.adopted", "p/lead", 11, false),
            ("agent.editor_ready", "p/lead", 11, false),
            ("agent.clear_requested", "p/other", 11, true),
            ("msg.sent", "p/lead", 11, true),
            ("agent.clear_requested", "p/lead", 10, true),
        ] {
            let (send, events) = broadcast::channel(1);
            let mut candidate = BindingCandidate {
                session: SessionId::new("fresh"),
                pane: agent.pane.clone(),
                binding: None,
                excluded: agent.excluded_session_id.clone(),
                cursor: 10,
                events,
            };
            let event = Arc::new(crate::eventlog::Envelope {
                seq,
                ts: agent.updated_at,
                kind: kind.into(),
                body: serde_json::json!({"agent_id": id}),
            });
            send.send(event.clone()).unwrap();
            assert_eq!(candidate.valid(&agent), valid, "{kind}/{id}/{seq}");
            // Even unrelated events overflowing the bounded fence must refuse.
            send.send(event.clone()).unwrap();
            send.send(event).unwrap();
            assert!(!candidate.valid(&agent));
        }
    }
}
