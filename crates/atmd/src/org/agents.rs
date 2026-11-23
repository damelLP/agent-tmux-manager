//! Named-agent projection and reservation-serialized reconciliation.
//!
//! Readiness is durable, never a list-only overlay. Restart checks may suspend
//! bound Ready agents, preserving their binding; ordinary wakes never do. A
//! session-less Ready agent retains its lifecycle state even on an unsafe screen.
//! Delivery must independently check the screen before input.
//! A Stuck agent recovers only when one observation finds its session row and
//! captures a safe editor; a binding alone never clears Stuck.
//! A flow never outlives its daemon: at the restart check an agent found
//! Clearing, or Starting with no session bound, is recorded Stuck and its
//! owner gets one note.
//! The lifecycle flows (`agent_lifecycle`) run as jobs admitted here: up, down,
//! adopt, clear and respawn act on panes.
//!
//! List cost sums saved session costs, substituting the single pinned live
//! session once without writing summaries. Known limit: live zero retains a
//! saved nonzero cost to protect restart defaults; a genuine correction to
//! zero remains delayed until summary persistence. Nonzero decreases are live.

use super::agent_flow::{adopt_baseline, classify_pane, PaneShape};
use super::agent_lifecycle::{deadline_answer, pin, stuck_record, ClearAttempt, SavedSeat};
pub use super::agent_methods::register_agent_methods;
pub use super::agent_screen::{agent_screen, foreground, AgentScreen, Foreground, PaneBaseline};
pub use super::agent_types::*;
use super::{
    agent_hire::Hiring,
    agent_proc::ShellListIssue,
    agent_project::AgentProjects,
    agent_unreadable::{foreground_session, ScreenEpisodes, ScreenSample},
    department_directory::DepartmentFiles,
    reporting_line::ReportingFiles,
    session_process::{process_confirmed, ProcessClock},
    OrgHandle, OrgServiceError,
};
use crate::control::reservation::{
    CaptureFormat, PaneId, Quarantine, Reservation, ReservationKind, ReservationRequest, WaitPolicy,
};
use crate::server::methods::CallContext;
use crate::{
    control::service::ControlHandle,
    registry::{RegistryHandle, SessionEvent},
};
use atm_core::{
    harness_control::ControlDialog, DialogState, Harness, InputState, Money, SessionId,
    SessionView, TurnState,
};
use atm_org::{Agent, OrgCommand, OrgState, SessionProcess};
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

const LIFECYCLE_CAPACITY: usize = 32;
const LIST_BUDGET: Duration = Duration::from_secs(5);
// The list's read of the quarantined operations, inside `LIST_BUDGET`.
const QUARANTINE_BUDGET: Duration = Duration::from_secs(1);
const OBSERVE_BUDGET: Duration = Duration::from_secs(30);
const MIN_OBSERVE_INTERVAL: Duration = Duration::from_millis(50);
// U8c has 32 active+parked slots. Observers use at most a quarter, leaving
// headroom for lifecycle work and deliveries even during restart or event lag.
const OBSERVER_CONCURRENCY: usize = 8;
// A failed observation is retried without waiting for a registry event: the
// harness may be quiet, or its only row excluded. Doubling, capped.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);
// A Starting agent with a session (revalidating) is looked at again this often:
// its pane can become safe with no harness event (a draft cleared, copy mode
// left, an idle dim suggestion), and nothing else would wake it.
const REVALIDATE_EVERY: Duration = Duration::from_secs(5);

/// Why an agent's last observation failed; cleared by its next success.
#[derive(Debug, Clone)]
struct ObservationFailure {
    reason: String,
    /// The agent's restart check has not completed yet.
    restart: bool,
}

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

/// Reads one variable of the environment a launch takes its overrides from.
pub(super) type SpawnEnv = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

type PaneRefusal = (Option<String>, Option<ShellListIssue>);
/// An agent's pane, its baseline, and its last observation's refusal and shell-list issue.
type PaneRecord = (String, PaneBaseline, PaneRefusal);
type HandoverWaits = Arc<std::sync::Mutex<BTreeMap<AgentId, (SavedSeat, String)>>>;

#[derive(Clone)]
pub(super) struct AgentRuntime {
    pub org: OrgHandle,
    pub registry: RegistryHandle,
    pub control: ControlHandle,
    pub(super) screens: Arc<ScreenEpisodes>,
    /// Each agent's pane, what was recorded about its processes, and why its
    /// last observation found every input to it refused, if it did. In
    /// memory only; written under the agent's reservation.
    baselines: Arc<std::sync::Mutex<HashMap<AgentId, PaneRecord>>>,
    /// A display-only reason for a saved handover that has not admitted Clear.
    handover_waits: HandoverWaits,
    /// Reads a pane's root process from procfs; scripted in tests.
    pub proc: super::agent_proc::ProcReader<super::agent_proc::ProcSample>,
    /// The `ATM_SPAWN_*` overrides of the daemon's start environment.
    pub spawn_env: SpawnEnv,
    #[cfg(test)]
    progress: broadcast::Sender<ObservationProgress>,
    /// Test-only: stops the revalidation poll, for tests whose scripted tmux
    /// reads cannot take its background captures in an unknown order.
    #[cfg(test)]
    revalidation_paused: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentRuntime {
    /// The baseline recorded for `agent` on `pane`, or one that refuses every
    /// read: none was recorded, or it belongs to a pane the agent has left.
    pub(super) fn baseline(&self, agent: &AgentId, pane: &str) -> PaneBaseline {
        self.pane_record(agent, pane)
            .map_or_else(PaneBaseline::unproven, |(_, baseline, _)| baseline)
    }

    fn pane_record(&self, agent: &AgentId, pane: &str) -> Option<PaneRecord> {
        self.baselines
            .lock()
            .ok()
            .and_then(|baselines| baselines.get(agent).cloned())
            .filter(|(recorded, ..)| recorded == pane)
    }

    /// Record what was established about an agent's pane. A poisoned store
    /// records nothing, and reads then refuse. A refusal reason read before
    /// this goes with what it replaces.
    pub(super) fn record_baseline(&self, agent: AgentId, pane: &str, baseline: PaneBaseline) {
        if let Ok(mut baselines) = self.baselines.lock() {
            baselines.insert(agent, (pane.into(), baseline, (None, None)));
        }
    }

    /// Why the last observation of `agent` on `pane` found every input to it
    /// refused, if it did.
    fn refusal(&self, agent: &AgentId, pane: &str) -> Option<String> {
        self.pane_record(agent, pane)
            .and_then(|(.., (reason, _))| reason)
    }

    fn listed_refusal(&self, agent: &AgentId, pane: &str) -> Option<String> {
        match self.pane_record(agent, pane)?.2 {
            (Some(reason), Some(issue)) => Some(format!("{reason}; {}", issue.detail())),
            (None, Some(issue)) => Some(issue.detail().into()),
            (reason, None) => reason,
        }
    }

    /// Set or clear the last observation's refusal and diagnostic; keep the baseline.
    fn set_refusal(&self, agent: &AgentId, pane: &str, refusal: PaneRefusal) {
        if let Ok(mut baselines) = self.baselines.lock() {
            match baselines
                .get_mut(agent)
                .filter(|(recorded, ..)| recorded == pane)
            {
                Some(entry) => entry.2 = refusal,
                None if refusal != (None, None) => {
                    let record = (pane.into(), PaneBaseline::unproven(), refusal);
                    baselines.insert(agent.clone(), record);
                }
                None => {}
            }
        }
    }

    /// Drop what was recorded about an agent's pane; it has left that pane.
    pub(super) fn forget_baseline(&self, agent: &AgentId) {
        if let Ok(cursor) = self.org.status() {
            self.screens.invalidate(agent, cursor);
        }
        if let Ok(mut baselines) = self.baselines.lock() {
            baselines.remove(agent);
        }
    }
}

/// What a delivery needs to know about its recipient, read in one call.
#[derive(Debug, Clone)]
pub struct Recipient {
    pub(super) cursor: u64,
    /// The agent's recorded status, from the same snapshot as everything
    /// else here. An answer is not permission: only a Ready agent takes input.
    pub status: AgentStatus,
    /// The session the agent is recorded as bound to, if any. A delivery is
    /// confirmed against this, never against `session`.
    pub session_id: Option<SessionId>,
    pub pane: PaneId,
    pub harness: Harness,
    /// The harness's screen dispatch, for use with `baseline`.
    pub screen: &'static AgentScreen,
    /// What was recorded about the pane's processes. When nothing was, this is
    /// [`PaneBaseline::unproven`] and every read through `screen` refuses.
    pub baseline: PaneBaseline,
    pub(super) shell_list_issue: Option<ShellListIssue>,
    /// The agent's excluded (old) session: a session that appears later must
    /// not be this one.
    pub excluded: Option<SessionId>,
    /// The pane's one eligible parent session row; `None` when there is none.
    /// It is what the registry shows on the pane now and may differ from the
    /// session the agent is recorded as bound to: a new session the observer
    /// has not bound yet, or a binding whose row is not announced yet.
    pub session: Option<SessionView>,
    /// The pane shows a discovery row of the agent's harness, as every row is
    /// after a daemon restart until the harness's next event.
    pub unconfirmed: bool,
    /// `session` is a discovery row standing for the recorded session by its
    /// process: the harness has reported nothing since the restart.
    pub process_matched: bool,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) struct ObservationProgress {
    pub phase: &'static str,
    pub agent: Option<AgentId>,
}

#[cfg(test)]
impl AgentRuntime {
    pub(super) fn progress(&self, phase: &'static str, agent: Option<AgentId>) {
        let _ = self.progress.send(ObservationProgress { phase, agent });
    }
}

struct AgentService {
    disabled: Option<String>,
    runtime: Option<AgentRuntime>,
    projects: Option<AgentProjects>,
    ready: watch::Receiver<Option<Result<(), String>>>,
    errors: watch::Receiver<BTreeMap<AgentId, ObservationFailure>>,
    lifecycle: Option<mpsc::Sender<LifecycleJob>>,
    stop: CancellationToken,
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
    hiring: Option<Hiring>,
}

impl AgentHandle {
    pub(crate) fn with_hiring(
        mut self,
        departments: DepartmentFiles,
        reporting: ReportingFiles,
    ) -> Self {
        self.hiring = Some(Hiring {
            departments,
            reporting,
        });
        self
    }

    pub(super) fn screens(&self) -> Option<&ScreenEpisodes> {
        Some(&self.inner.runtime.as_ref()?.screens)
    }

    #[cfg(test)]
    pub(super) fn observation_events(&self) -> broadcast::Receiver<ObservationProgress> {
        self.inner.runtime.as_ref().unwrap().progress.subscribe()
    }

    /// Resolves once no observation failure is stored for `agent`.
    #[cfg(test)]
    pub(super) async fn failure_cleared(&self, agent: &AgentId) {
        let mut errors = self.inner.errors.clone();
        errors
            .wait_for(|errors| !errors.contains_key(agent))
            .await
            .unwrap();
    }

    #[cfg(test)]
    pub(super) fn record_baseline(&self, agent: AgentId, pane: &str, baseline: PaneBaseline) {
        let rt = self.inner.runtime.as_ref().unwrap();
        rt.record_baseline(agent, pane, baseline);
    }

    #[cfg(test)]
    pub(super) fn pause_revalidation(&self) {
        let rt = self.inner.runtime.as_ref().unwrap();
        rt.revalidation_paused
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn baseline_of(&self, agent: &AgentId, pane: &str) -> PaneBaseline {
        self.inner.runtime.as_ref().unwrap().baseline(agent, pane)
    }

    #[cfg(test)]
    pub(super) fn refusal_of(&self, agent: &AgentId, pane: &str) -> Option<String> {
        self.inner.runtime.as_ref().unwrap().refusal(agent, pane)
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
                stop: CancellationToken::new(),
            }),
            hiring: None,
        }
    }

    pub(super) fn stop_token(&self) -> CancellationToken {
        self.inner.stop.clone()
    }

    /// Snapshot the sender held by this service, not a second admission lane.
    pub(crate) fn lifecycle_health(&self) -> crate::server::health::LifecycleHealth {
        use crate::server::health::{LifecycleHealth, LifecycleState};
        let state = match self.inner.lifecycle.as_ref() {
            None => LifecycleState::Absent,
            Some(sender) if sender.is_closed() => LifecycleState::Stopped,
            Some(sender) if sender.capacity() == 0 => LifecycleState::Full,
            Some(_) => LifecycleState::Ok,
        };
        LifecycleHealth {
            state,
            capacity: LIFECYCLE_CAPACITY,
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
        self.lifecycle_for(None, request, work, cleanup).await
    }

    /// As [`Self::lifecycle`], for a pane id that came from a caller: when the
    /// caller named its tmux server, the reservation is refused unless that is
    /// the server atmd controls.
    pub(super) async fn lifecycle_for<T, W, WF, C, CF>(
        &self,
        server: Option<String>,
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
        // Down and Respawn inspect the pane under their own reservation, so a failed
        // observation never refuses them. Other work waits only for an agent whose
        // restart check is still open, not for a runtime failure of a checked one.
        if !matches!(
            request.kind,
            ReservationKind::Down | ReservationKind::Respawn
        ) {
            if let Some(failure) = self
                .inner
                .errors
                .borrow()
                .get(&request.agent)
                .filter(|failure| failure.restart)
            {
                return Err(AgentError::Unavailable(format!(
                    "{}: restart check is being retried: {}",
                    request.agent, failure.reason
                )));
            }
        }
        let control = rt.control.clone().for_server(server);
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

    pub async fn up(&self, p: UpParams, caller: CallContext) -> Result<UpResult, AgentError> {
        const OPERATION: &str = "agent.up";
        let deadline = Instant::now() + Duration::from_secs(60);
        let prepared = self
            .projects(OPERATION)?
            .prepare(p.project_root, p.name)
            .await?;
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let warning = match &self.hiring {
            Some(hiring) => hiring.check(&rt.org, &prepared, &caller, p.hire).await?,
            None => None,
        };
        let id = prepared.agent_id.clone();
        let launch = rt
            .prepare_launch(prepared, caller, p.caller_server.clone())
            .await?;
        #[cfg(test)]
        rt.progress("up_prepared", Some(id.clone()));
        let request = ReservationRequest::new(
            id.clone(),
            None,
            ReservationKind::Up,
            WaitPolicy::Wait,
            deadline,
        );
        let waiting = super::agent_lifecycle::Waiting::default();
        let (work, cleanup) = (rt.clone(), rt.clone());
        let (why, record, whose) = (waiting.clone(), waiting.clone(), id.clone());
        let mut result = self
            .lifecycle_for(
                p.caller_server,
                request,
                move |r| async move { work.up(&r, launch, None, &waiting).await },
                move |_, _| async move { cleanup.settle_up(&id, &why).await },
            )
            .await
            .map_err(|error| deadline_answer(error, &whose, stuck_record(&record)))?;
        result.warning = warning;
        Ok(result)
    }
    pub async fn adopt(&self, p: AdoptParams) -> Result<AdoptResult, AgentError> {
        const OPERATION: &str = "agent.adopt";
        let deadline = Instant::now() + OBSERVE_BUDGET;
        let projects = self.projects(OPERATION)?;
        let pane = PaneId::parse(p.pane)?;
        let id = projects.identify(p.project_root, p.name).await?;
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let request = ReservationRequest::new(
            id.clone(),
            Some(pane.clone()),
            ReservationKind::Adopt,
            WaitPolicy::Wait,
            deadline,
        );
        let effect = super::agent_lifecycle::Effect::default();
        let (work, cleanup) = (rt.clone(), rt.clone());
        let (agent, held, changed) = (id.clone(), pane.clone(), effect.clone());
        self.lifecycle_for(
            p.caller_server,
            request,
            move |r| async move { work.adopt(&r, &id, &pane, &effect).await },
            move |_, _| async move { cleanup.settle_adopt(&agent, &held, &changed).await },
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
    pub async fn clear(&self, p: ClearParams) -> Result<ClearResult, AgentError> {
        match self
            .clear_once(p.agent, ClearMode::Standalone(p.timeout_ms))
            .await?
        {
            ClearAttempt::Cleared(cleared) => Ok(cleared.result),
            ClearAttempt::Blocked(_) | ClearAttempt::Expired => Err(AgentError::Internal(
                "standalone clear returned a handover-only wait".into(),
            )),
        }
    }

    pub(super) async fn clear_handover(
        &self,
        saved: SavedSeat,
        retry_before: Option<Instant>,
        finish_by: Instant,
    ) -> Result<ClearAttempt, AgentError> {
        self.clear_once(
            saved.agent.clone(),
            ClearMode::Handover {
                saved,
                retry_before,
                finish_by,
            },
        )
        .await
    }

    pub(super) fn set_handover_wait(&self, saved: &SavedSeat, reason: Option<String>) {
        if let Some(rt) = &self.inner.runtime {
            if let Ok(mut waits) = rt.handover_waits.lock() {
                if let Some(reason) = reason {
                    waits.insert(saved.agent.clone(), (saved.clone(), reason));
                } else {
                    waits.remove(&saved.agent);
                }
            }
        }
    }

    async fn clear_once(&self, id: AgentId, mode: ClearMode) -> Result<ClearAttempt, AgentError> {
        const OPERATION: &str = "agent.clear";
        if !matches!(*self.inner.ready.borrow(), Some(Ok(()))) {
            return Err(self.unavailable(OPERATION));
        }
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let (pane, saved, retry_before, finish_by) = match mode {
            ClearMode::Standalone(timeout_ms) => {
                let state = rt.org.snapshot().await?;
                let agent = state
                    .agent(&id)
                    .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
                let pane = agent.pane.as_ref().map(PaneId::parse).transpose()?;
                (
                    pane,
                    None,
                    None,
                    Instant::now() + Duration::from_millis(timeout_ms),
                )
            }
            ClearMode::Handover {
                saved,
                retry_before,
                finish_by,
            } => (
                Some(saved.pane.clone()),
                Some(saved),
                retry_before,
                finish_by,
            ),
        };
        let wait = if retry_before.is_some() {
            WaitPolicy::Refuse
        } else {
            WaitPolicy::Wait
        };
        let request = ReservationRequest::new(
            id.clone(),
            pane.clone(),
            ReservationKind::Clear,
            wait,
            finish_by,
        );
        let waiting = super::agent_lifecycle::Waiting::default();
        let (work, cleanup) = (rt.clone(), rt.clone());
        let (whose, why) = (id.clone(), waiting.clone());
        let (record, cleaning) = (waiting.clone(), id.clone());
        let saved_handover = saved.is_some();
        self.lifecycle(
            request,
            move |r| async move {
                work.clear(
                    &r,
                    &id,
                    pane.as_ref(),
                    &waiting,
                    saved.as_ref(),
                    retry_before,
                )
                .await
            },
            move |_, _| async move { cleanup.settle_clear(&cleaning, &why, saved_handover).await },
        )
        .await
        .map_err(|error| deadline_answer(error, &whose, stuck_record(&record)))
    }
    pub async fn respawn(
        &self,
        p: RespawnParams,
        caller: CallContext,
    ) -> Result<RespawnResult, AgentError> {
        const OPERATION: &str = "agent.respawn";
        let deadline = Instant::now() + Duration::from_secs(60);
        let projects = self.projects(OPERATION)?;
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let id = p.agent;
        let state = rt.org.snapshot().await?;
        let agent = state
            .agent(&id)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id} was not found")))?;
        let old = agent.pane.as_ref().map(PaneId::parse).transpose()?;
        let root = state
            .project(&id.project)
            .ok_or_else(|| AgentError::NotFound(format!("agent {id}'s project was not found")))?
            .root
            .clone();
        // The role is read again, and where to split is settled, before
        // anything is reserved or killed.
        let prepared = projects.prepare(root.into(), id.name.clone()).await?;
        if prepared.agent_id != id {
            return Err(AgentError::Refused(format!(
                "agent {id} was not respawned: its project directory now belongs to project {}",
                prepared.agent_id.project
            )));
        }
        let launch = rt
            .prepare_launch(prepared, caller, p.caller_server.clone())
            .await?;
        #[cfg(test)]
        rt.progress("up_prepared", Some(id.clone()));
        let request = ReservationRequest::new(
            id.clone(),
            old.clone(),
            ReservationKind::Respawn,
            WaitPolicy::Wait,
            deadline,
        );
        let waiting = super::agent_lifecycle::Waiting::default();
        let replace = super::agent_lifecycle::Replace {
            reserved: old.clone(),
            effect: super::agent_lifecycle::Effect::default(),
        };
        let (work, cleanup) = (rt.clone(), rt.clone());
        let (why, changed) = (waiting.clone(), replace.effect.clone());
        let (record, whose) = (waiting.clone(), id.clone());
        let started = self
            .lifecycle_for(
                p.caller_server,
                request,
                move |r| async move { work.up(&r, launch, Some(replace), &waiting).await },
                move |_, _| async move {
                    cleanup
                        .settle_respawn(&id, old.as_ref(), &changed, &why)
                        .await
                },
            )
            .await
            .map_err(|error| deadline_answer(error, &whose, stuck_record(&record)))?;
        Ok(RespawnResult {
            agent_id: started.agent_id,
            pane: started.pane,
        })
    }
    pub async fn down(&self, p: DownParams) -> Result<DownResult, AgentError> {
        const OPERATION: &str = "agent.down";
        if !matches!(*self.inner.ready.borrow(), Some(Ok(()))) {
            return Err(self.unavailable(OPERATION));
        }
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let state = rt.org.snapshot().await?;
        let agent = state
            .agent(&p.agent)
            .ok_or_else(|| AgentError::NotFound(format!("agent {} was not found", p.agent)))?;
        let pane = agent.pane.as_ref().map(PaneId::parse).transpose()?;
        let request = ReservationRequest::new(
            p.agent.clone(),
            pane.clone(),
            ReservationKind::Down,
            WaitPolicy::Wait,
            Instant::now() + OBSERVE_BUDGET,
        );
        let effect = super::agent_lifecycle::Effect::default();
        let (work, cleanup) = (rt.clone(), rt.clone());
        let (id, changed) = (p.agent.clone(), effect.clone());
        self.lifecycle(
            request,
            move |r| async move {
                work.down(&r, &p.agent, pane.as_ref(), p.kill, &effect, "agent.down")
                    .await
            },
            move |_, _| async move { cleanup.settle_down(&id, &changed).await },
        )
        .await
    }
    pub async fn list(&self, p: ListParams) -> Result<ListResult, AgentError> {
        let deadline = Instant::now() + LIST_BUDGET;
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
            // The operations whose cleanup failed. A control service that does
            // not answer in time leaves the list without them, never without
            // its agents. Reserve the final timer tick to finish the list:
            // even a healthy control read first reached then is omitted.
            let quarantines = rt
                .control
                .quarantines(
                    (Instant::now() + QUARANTINE_BUDGET).min(deadline - Duration::from_millis(1)),
                )
                .await
                .unwrap_or_default();
            Ok(state
                .agents()
                .filter(|a| filter.as_ref().is_none_or(|id| &a.id.project == id))
                .map(|a| {
                    let mut entry = list_entry(&state, a, &rows);
                    // A quarantined agent takes no call at all, its owner's
                    // included, on a pane that may be gone: that is shown in
                    // place of a refusal read on that pane and of the failed
                    // observation, which fails on the quarantine itself. A
                    // recorded reason stays in front of it.
                    if let Some(quarantine) = quarantines.iter().find(|q| q.agent == a.id) {
                        let quarantined = quarantine_text(quarantine);
                        entry.reason = Some(match entry.reason {
                            Some(reason) => format!("{reason}; {quarantined}"),
                            None => quarantined,
                        });
                        return entry;
                    }
                    // A Ready or Starting agent has no recorded reason: what
                    // the last observation found refused is its reason.
                    if matches!(a.status, AgentStatus::Ready | AgentStatus::Starting) {
                        if let Some(refusal) = a
                            .pane
                            .as_deref()
                            .and_then(|pane| rt.listed_refusal(&a.id, pane))
                        {
                            entry.reason = Some(refusal);
                        }
                    }
                    if let Some(reason) = rt.handover_waits.lock().ok().and_then(|waits| {
                        waits
                            .get(&a.id)
                            .filter(|(saved, _)| saved.matches(a))
                            .map(|(_, reason)| reason.clone())
                    }) {
                        entry.reason = Some(reason);
                    }
                    if let Some(ObservationFailure { reason: error, .. }) =
                        self.inner.errors.borrow().get(&a.id)
                    {
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
    /// The recipient's pane, screen dispatch, baseline and session, for a
    /// delivery. Call it inside the caller's reservation for `agent`, so no
    /// lifecycle job changes the answer before it is used.
    ///
    /// A registry that cannot answer, and a pane with more than one eligible
    /// session, are errors: neither is ever reported as "no session".
    pub async fn recipient(&self, agent: &AgentId) -> Result<Recipient, AgentError> {
        const OPERATION: &str = "agent recipient";
        if !matches!(*self.inner.ready.borrow(), Some(Ok(()))) {
            return Err(self.unavailable(OPERATION));
        }
        let rt = self
            .inner
            .runtime
            .as_ref()
            .ok_or_else(|| self.unavailable(OPERATION))?;
        let (state, cursor) = rt.org.snapshot_at().await?;
        let found = state
            .agent(agent)
            .ok_or_else(|| AgentError::NotFound(format!("agent {agent} was not found")))?;
        let pane = found
            .pane
            .as_ref()
            .ok_or_else(|| AgentError::Refused(format!("agent {agent} has no pane")))?;
        let sessions = timeout(LIST_BUDGET, rt.registry.pane_sessions(pane))
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|error| AgentError::Unavailable(format!("session registry: {error}")))?;
        let rows: Vec<SessionView> = sessions.iter().map(|row| row.view.clone()).collect();
        let harness = harness(&state, found, &rows);
        let screen = agent_screen(harness).ok_or_else(|| {
            AgentError::Refused(format!(
                "agent {agent}: no screen reader for harness {}",
                harness.short_tag()
            ))
        })?;
        let mut unconfirmed = rows.iter().any(|row| {
            matches_agent(found, row) && row.id.is_pending() && row.harness == harness.short_tag()
        });
        let mut eligible = rows.into_iter().filter(|row| {
            matches_agent(found, row) && !row.id.is_pending() && row.harness == harness.short_tag()
        });
        let mut session = eligible.next();
        let mut process_matched = false;
        if eligible.next().is_some() {
            return Err(AgentError::Conflict(format!(
                "agent {agent} has more than one eligible session on {pane}"
            )));
        }
        // After a daemon restart, the discovery row that is the recorded
        // session's own process stands for that session.
        if session.is_none() && unconfirmed {
            let recorded = (found.session_id.as_ref()).and_then(|id| state.session_process(id));
            let clock = recorded.is_none().then(ProcessClock::read).flatten();
            if let Some(row) = process_confirmed(found, recorded, harness, &sessions, clock) {
                session = (found.session_id.clone()).map(|id| SessionView {
                    id,
                    ..row.view.clone()
                });
                unconfirmed = false;
                process_matched = true;
            }
        }
        let (baseline, shell_list_issue) = rt.pane_record(agent, pane).map_or_else(
            || (PaneBaseline::unproven(), None),
            |(_, baseline, (_, issue))| (baseline, issue),
        );
        Ok(Recipient {
            cursor,
            status: found.status,
            session_id: found.session_id.clone(),
            pane: PaneId::parse(pane)?,
            harness,
            screen,
            baseline,
            shell_list_issue,
            excluded: found.excluded_session_id.clone(),
            session,
            unconfirmed,
            process_matched,
        })
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

enum ClearMode {
    Standalone(u64),
    Handover {
        saved: SavedSeat,
        retry_before: Option<Instant>,
        finish_by: Instant,
    },
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
                    let (state, cursor) = rt.org.snapshot_at().await?;
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
                    rt.inspect(&r, &state, agent, bind, initial, cursor).await
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

    /// Record the process `session`'s confirmed row on the agent's pane runs
    /// in, when it is not the session's recorded one, so that a restarted
    /// daemon knows that row while it is a discovery row again. Callers pass
    /// only a session from a non-pending row: a guess is never recorded.
    async fn record_process(&self, state: &OrgState, agent: &Agent, session: &SessionId) {
        // Advisory: a failure leaves the pass's own outcome as it was.
        if let Err(error) = self.try_record_process(state, agent, session).await {
            tracing::warn!(agent = %agent.id, %error, "Session process not recorded");
        }
    }

    async fn try_record_process(
        &self,
        state: &OrgState,
        agent: &Agent,
        session: &SessionId,
    ) -> Result<(), AgentError> {
        let Some(pane) = agent.pane.as_deref() else {
            return Ok(());
        };
        let rows = self.pane_sessions(&PaneId::parse(pane)?).await?;
        let process = (rows.iter().filter(|row| &row.view.id == session)).find_map(|row| {
            Some(SessionProcess {
                session_id: session.clone(),
                pid: row.pid?,
                process_start: row.process_start?,
            })
        });
        if let Some(process) = process.filter(|p| state.session_process(session) != Some(p)) {
            self.org
                .execute(OrgCommand::RecordProcess(process), chrono::Utc::now())
                .await?;
        }
        Ok(())
    }

    async fn inspect(
        &self,
        r: &Reservation,
        state: &OrgState,
        agent: &Agent,
        bind: Option<BindingCandidate>,
        initial: bool,
        cursor: u64,
    ) -> Result<(), AgentError> {
        // Registry failure is never evidence that every agent is gone.
        self.rows().await?;
        let mut in_mode = false;
        // The root process and foreground command the inspection shows.
        let mut look = None;
        let gone = match &agent.pane {
            None => true,
            Some(pane) => match r.inspect(&PaneId::parse(pane)?).await? {
                None => true,
                Some(p) => {
                    in_mode = p.in_mode;
                    look = p.pane_root_pid().zip(p.foreground_command.clone());
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
            self.forget_baseline(&agent.id);
            return Ok(());
        }
        // What is recorded about the pane's processes: nothing after a daemon
        // restart, until a pass (or an adopt) pins the harness.
        let stored = agent
            .pane
            .as_deref()
            .map_or_else(PaneBaseline::unproven, |pane| {
                self.baseline(&agent.id, pane)
            });
        // The harness is not known to be in front: read the pane's processes,
        // once. An idle shell under an agent that was Ready or bound is the
        // positive evidence that its harness exited, with or without an end
        // event. Any other shape records nothing: the agent keeps its state,
        // and `agent.list` says why every input to it is refused.
        let mut processes = None;
        let mut refused = None;
        let mut shell_list_issue = None;
        let (root, command) = look
            .as_ref()
            .map(|(root, command)| (*root, command.as_str()))
            .unzip();
        if foreground(&stored, root, command) != Some(Foreground::Harness) {
            if let Some(root) = root {
                processes = (self.proc)(root).await.map(|sample| {
                    if stored.harness_command().is_none() {
                        shell_list_issue = sample.shell_list_issue;
                    }
                    sample.facts
                });
            }
            if let Some(pane) = agent.pane.as_ref().filter(|_| {
                (agent.status == AgentStatus::Ready || agent.session_id.is_some())
                    && processes.as_ref().and_then(classify_pane) == Some(PaneShape::IdleShell)
            }) {
                self.org
                    .execute(
                        OrgCommand::Down {
                            agent_id: agent.id.clone(),
                            reason: format!("the harness exited to the shell in {pane}"),
                        },
                        chrono::Utc::now(),
                    )
                    .await?;
                self.forget_baseline(&agent.id);
                return Ok(());
            }
            if agent.status == AgentStatus::Ready
                || (agent.status == AgentStatus::Starting && agent.session_id.is_some())
            {
                let shape = processes.as_ref().and_then(classify_pane);
                refused = agent
                    .pane
                    .as_deref()
                    .map(|pane| refusal_text(pane, &stored, command, shape));
            }
        }
        // One write per pass: the reason, or none once the harness is in front.
        if let Some(pane) = &agent.pane {
            self.set_refusal(&agent.id, pane, (refused, shell_list_issue));
        }
        // A flow never outlives its daemon. At the restart check a clear, and
        // a start that has bound no session, have nobody driving them: the
        // agent is Stuck and its owner is told, once.
        let starting = agent.status == AgentStatus::Starting && agent.session_id.is_none();
        let interrupted = initial && (agent.status == AgentStatus::Clearing || starting);
        if let Some(pane) = &agent.pane {
            let cleared = format!("clear interrupted by daemon restart; inspect {pane}");
            let started = format!("was still starting when the daemon restarted; inspect {pane}");
            // This pass's reason follows the status. An agent already Stuck
            // with either reason may still lack its note.
            let reason = if interrupted {
                Some(if starting { started } else { cleared })
            } else {
                agent.reason.clone().filter(|recorded| {
                    agent.status == AgentStatus::Stuck
                        && (*recorded == cleared || *recorded == started)
                })
            };
            if let Some(reason) = reason {
                if interrupted {
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
                    delivery_notice: None,
                    from: atm_org::Address::System,
                    to: atm_org::Address::Owner,
                    kind: atm_org::MessageKind::Note,
                    body: format!("agent {}: {reason}", agent.id),
                    attachments: Vec::new(),
                    task: None,
                    post: None,
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
        // The snapshot still shows an interrupted start as Starting: nothing
        // below may pin, bind or capture for an agent just recorded Stuck.
        if agent.status == AgentStatus::Clearing || interrupted {
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
        if let [row] = eligible.as_slice() {
            if agent.session_id.as_ref() == Some(&row.id) {
                self.record_process(state, agent, &row.id).await;
            }
        }
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
                    self.record_process(state, agent, &candidate.session).await;
                    // The agent is Ready now. The reason above was decided
                    // for what it was before (Starting without a session
                    // has none): when its harness is not known to be in
                    // front, say so, and with that let its own session
                    // start the pass that pins it.
                    if foreground(&stored, root, command) != Some(Foreground::Harness) {
                        if let Some(pane) = agent.pane.as_deref() {
                            let shape = processes.as_ref().and_then(classify_pane);
                            let text = refusal_text(pane, &stored, command, shape);
                            self.set_refusal(&agent.id, pane, (Some(text), shell_list_issue));
                        }
                    }
                    return Ok(());
                }
            }
            binding = None;
        }
        let harness = harness(state, agent, &rows);
        let screen = agent_screen(harness);
        // Whether this pass can pin the harness by the adopt rule: nothing is
        // recorded yet, the agent is Starting or Ready, the inspection and the
        // process read that followed it allow a baseline, and the pane has a
        // live row of the agent's harness (without one the pin must fail; the
        // row's arrival starts its own pass). The capture below is the rule's
        // second look.
        let pinnable = screen.zip(look.as_ref()).zip(processes.as_ref()).filter(
            |((_, (root, command)), processes)| {
                stored == PaneBaseline::unproven()
                    && matches!(agent.status, AgentStatus::Ready | AgentStatus::Starting)
                    && adopt_baseline(processes, *root, command).is_some()
                    && rows
                        .iter()
                        .any(|row| matches_agent(agent, row) && row.harness == harness.short_tag())
            },
        );
        // A capture can change nothing for a Ready agent on an ordinary wake, or
        // for a session-less one at the restart check, unless it can pin: don't
        // read, or fail on, it.
        if (stuck && binding.is_none())
            || (agent.status == AgentStatus::Ready
                && (!initial || agent.session_id.is_none())
                && pinnable.is_none())
        {
            return Ok(());
        }
        let Some(pane) = &agent.pane else {
            return Ok(());
        };
        #[cfg(test)]
        self.progress("capture", Some(agent.id.clone()));
        let pane_id = PaneId::parse(pane)?;
        let capture = r
            .capture(
                &pane_id,
                screen.map_or(CaptureFormat::Plain, AgentScreen::capture_format),
            )
            .await?;
        let at = Instant::now();
        let rows = self.pane_sessions(&pane_id).await?;
        let roots = rows.iter().filter(|row| {
            matches_agent(agent, &row.view) && row.view.harness == harness.short_tag()
        });
        let live: Vec<_> = roots.clone().map(|row| &row.view).collect();
        // The adopt rule also needs a live row of the agent's harness, and the
        // pane must still be the agent's on the capture.
        let pinned = pinnable
            .filter(|_| {
                !live.is_empty()
                    && !capture.inspection.dead
                    && capture.inspection.local_agent_tag.as_deref() == Some(&agent.id.to_string())
            })
            .and_then(|((screen, (root, command)), processes)| {
                pin(screen, (*root, command), processes, &capture)
            });
        let (baseline, control, cold) = match pinned {
            Some((baseline, control)) => {
                if control.dialog != ControlDialog::Unknown {
                    self.record_baseline(agent.id.clone(), pane, baseline.clone());
                }
                (baseline, Some(control), true)
            }
            None => {
                let control = screen.map(|s| s.control(&capture, &stored));
                (stored, control, false)
            }
        };
        if matches!(agent.status, AgentStatus::Ready | AgentStatus::Starting) {
            if let Some(read) = screen.zip(control.as_ref()).and_then(|(screen, control)| {
                ScreenSample::read(
                    screen,
                    &baseline,
                    &capture,
                    &pane_id,
                    Some(&agent.id.to_string()),
                    agent.session_id.as_ref(),
                    control,
                )
            }) {
                let foreground = processes
                    .as_ref()
                    .filter(|_| cold && read.unreadable)
                    .and_then(|facts| foreground_session(roots, facts, agent.session_id.as_ref()));
                if !cold || !read.unreadable || foreground.is_some() {
                    self.screens.record(&agent.id, read, at, cursor, foreground);
                }
            }
        }
        // An unrecognized cold read is diagnostic evidence only; no pin.
        let facts = control
            .filter(|c| !cold || c.dialog != ControlDialog::Unknown)
            .map(|c| c.facts)
            .unwrap_or_default();
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
                self.record_process(state, agent, &candidate.session).await;
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

/// Why every input to an agent's pane is refused while the agent keeps its
/// recorded state: its harness is not known to be the pane's foreground
/// program. `command` is the foreground command the inspection shows (with
/// the root pid, or not at all), `shape` what the process read made of the
/// pane, `stored` what was recorded about it before.
#[must_use]
pub(super) fn refusal_text(
    pane: &str,
    stored: &PaneBaseline,
    command: Option<&str>,
    shape: Option<PaneShape>,
) -> String {
    let why = match (command, shape) {
        (None, _) => format!("tmux reports no root process or foreground command for {pane}"),
        (_, None) => format!("the processes of {pane} could not be read"),
        // Not reached from an observation: that records Down.
        (_, Some(PaneShape::IdleShell)) => format!("no harness is running in {pane}"),
        (_, Some(PaneShape::ShellHoldingChild)) => {
            format!("the shell in {pane} has a child and atm cannot tell which is in front")
        }
        (Some(command), Some(_)) => {
            match stored.harness_command().filter(|pinned| *pinned != command) {
                Some(pinned) => format!("{command} is in front in {pane}, not {pinned}"),
                None => format!(
                    "the program in front in {pane} is not confirmed as the agent's harness"
                ),
            }
        }
    };
    format!("input refused: {why}")
}

/// What `agent.list` says of an agent whose operation could not be settled.
/// The reservation keeps the operation and its pane, not why its cleanup
/// failed. The operation is named as its `agent.<kind>` method is; the
/// observer's own reservation is an observation. Each word is written out:
/// a renamed variant must not change what users read.
#[must_use]
pub(super) fn quarantine_text(quarantine: &Quarantine) -> String {
    let kind = match quarantine.kind {
        ReservationKind::Up => "up",
        ReservationKind::Adopt => "adopt",
        ReservationKind::Clear => "clear",
        ReservationKind::Respawn => "respawn",
        ReservationKind::Down => "down",
        ReservationKind::Delivery => "delivery",
        ReservationKind::Observe => "observation",
    };
    let on = quarantine
        .pane
        .as_ref()
        .map_or_else(String::new, |pane| format!(" on {pane}"));
    format!("quarantined: the agent's {kind}{on} could not be settled; restart atmd and reconcile")
}

pub(super) fn matches_agent(agent: &Agent, row: &SessionView) -> bool {
    agent.pane.is_some()
        && row.tmux_pane == agent.pane
        && row.parent_session_id.is_none()
        && agent.excluded_session_id.as_ref() != Some(&row.id)
}

pub(super) fn harness(state: &OrgState, agent: &Agent, rows: &[SessionView]) -> Harness {
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

pub(super) fn eligible_live_row<'a>(
    state: &OrgState,
    agent: &Agent,
    rows: &'a [SessionView],
) -> Option<&'a SessionView> {
    let expected = harness(state, agent, rows);
    let eligible: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches_agent(agent, r) && !r.id.is_pending() && r.harness == expected.short_tag()
        })
        .collect();
    if eligible.len() == 1 {
        eligible
            .first()
            .copied()
            .filter(|r| agent.session_id.as_ref() == Some(&r.id))
    } else {
        None
    }
}

pub(super) fn list_entry(state: &OrgState, agent: &Agent, rows: &[SessionView]) -> AgentListEntry {
    let live = eligible_live_row(state, agent, rows);
    let summaries = state.agent_sessions(&agent.id);
    let latest = summaries.last();
    let live_cost = live.and_then(|row| {
        let reported = row
            .cost_usd
            .is_finite()
            .then(|| Money::from_usd(row.cost_usd));
        let saved = summaries
            .iter()
            .find(|s| s.session_id == row.id)
            .and_then(|s| s.cost);
        reported
            .filter(|cost| !cost.is_zero())
            .or(saved)
            .or(reported)
    });
    let cost_usd = summaries
        .iter()
        .filter(|s| live.is_none_or(|row| row.id != s.session_id))
        .filter_map(|s| s.cost)
        .chain(live_cost)
        .reduce(|a, b| a + b)
        .map(|cost| cost.as_usd());
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
        cost_usd,
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
    spawn_agents_configured(
        org,
        registry,
        control,
        cancel,
        atm_org::delivery::DeliverySettings::default().readiness_timeout,
    )
}

pub(crate) fn spawn_agents_configured(
    org: OrgHandle,
    registry: RegistryHandle,
    control: ControlHandle,
    cancel: CancellationToken,
    readiness_timeout: Duration,
) -> (AgentHandle, JoinHandle<()>) {
    let proc = super::agent_proc::procfs_reader();
    // Read once: a launch takes its overrides from the daemon's start.
    let env: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| key.starts_with("ATM_SPAWN_"))
        .collect();
    let spawn_env: SpawnEnv = Arc::new(move |key| env.get(key).cloned());
    spawn_agents_sampled(
        org,
        registry,
        control,
        cancel,
        proc,
        spawn_env,
        readiness_timeout,
    )
}

/// [`spawn_agents`] with the procfs facts given, so existing tests can script them.
#[cfg(test)]
pub(super) fn spawn_agents_with(
    org: OrgHandle,
    registry: RegistryHandle,
    control: ControlHandle,
    cancel: CancellationToken,
    proc: super::agent_proc::ProcReader,
    spawn_env: SpawnEnv,
    readiness_timeout: Duration,
) -> (AgentHandle, JoinHandle<()>) {
    use super::agent_proc::{ProcReader, ProcSample};

    let proc: ProcReader<ProcSample> = Arc::new(move |pid| {
        let read = proc(pid);
        Box::pin(async move {
            read.await.map(|facts| ProcSample {
                facts,
                shell_list_issue: None,
            })
        })
    });
    spawn_agents_sampled(
        org,
        registry,
        control,
        cancel,
        proc,
        spawn_env,
        readiness_timeout,
    )
}

pub(super) fn spawn_agents_sampled(
    org: OrgHandle,
    registry: RegistryHandle,
    control: ControlHandle,
    cancel: CancellationToken,
    proc: super::agent_proc::ProcReader<super::agent_proc::ProcSample>,
    spawn_env: SpawnEnv,
    readiness_timeout: Duration,
) -> (AgentHandle, JoinHandle<()>) {
    let rt = AgentRuntime {
        org,
        registry,
        control,
        screens: Arc::new(ScreenEpisodes::new(readiness_timeout)),
        baselines: Arc::default(),
        handover_waits: Arc::default(),
        proc,
        spawn_env,
        #[cfg(test)]
        progress: broadcast::channel(4096).0,
        #[cfg(test)]
        revalidation_paused: Arc::default(),
    };
    let (projects, project_task) = AgentProjects::spawn(rt.org.clone(), cancel.child_token());
    let (send, ready) = watch::channel(None);
    let (errors, error_state) = watch::channel(BTreeMap::new());
    let (lifecycle, receive) = mpsc::channel(LIFECYCLE_CAPACITY);
    let dispatcher = tokio::spawn(dispatch_lifecycle(receive, cancel.child_token()));
    let handle = AgentHandle {
        inner: Arc::new(AgentService {
            disabled: None,
            runtime: Some(rt.clone()),
            projects: Some(projects),
            ready,
            errors: error_state,
            lifecycle: Some(lifecycle),
            stop: cancel.clone(),
        }),
        hiring: None,
    };
    // Subscribe before the initial snapshot: events arriving during inspection
    // remain wakeups, never cached readiness decisions.
    let events = rt.registry.subscribe();
    let committed = rt.org.committed.subscribe();
    let screens = tokio::spawn(ScreenEpisodes::run(rt.clone(), cancel.clone()));
    let task = tokio::spawn(async move {
        let result = observer(rt, events, committed, &send, &errors, &cancel).await;
        let reason = result
            .err()
            .map_or_else(|| "agent service stopped".into(), |error| error.to_string());
        send.send_replace(Some(Err(reason)));
        cancel.cancel();
        let _ = screens.await;
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
                } else if active.len() >= LIFECYCLE_CAPACITY {
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
    ready: bool,
    revalidating: bool,
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
                        ready: a.status == AgentStatus::Ready,
                        revalidating: a.status == AgentStatus::Starting && a.session_id.is_some(),
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

    /// A Ready agent's own session reporting in from its own pane has nothing to
    /// bind and is plainly alive: no observation. Its removal, a pane move, any
    /// other row, and agents in every other state (Stuck included) still wake.
    /// So does an agent whose last observation found its input refused: only
    /// a pass that finds its harness in front again drops that reason, and a
    /// resumed harness may send nothing but its own session's events.
    fn own_ready_session(
        &self,
        rt: &AgentRuntime,
        ids: &mut BTreeSet<AgentId>,
        row: Option<&SessionView>,
    ) {
        let Some(row) = row else {
            return;
        };
        ids.retain(|id| {
            !self.agents.get(id).is_some_and(|route| {
                route.ready
                    && route.binding.as_ref() == Some(&row.id)
                    && route.pane == row.tmux_pane
                    && !route
                        .pane
                        .as_deref()
                        .is_some_and(|pane| rt.refusal(id, pane).is_some())
            })
        });
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
            Ok(SessionEvent::Updated { session }) => {
                let mut ids = self.affected(&session.id, Some(&session));
                self.own_ready_session(rt, &mut ids, Some(&session));
                Ok(ids)
            }
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
                let mut ids = self.affected(&session_id, row.as_ref());
                self.own_ready_session(rt, &mut ids, row.as_ref());
                Ok(ids)
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
    errors: &watch::Sender<BTreeMap<AgentId, ObservationFailure>>,
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
    let mut backoff: BTreeMap<AgentId, Duration> = BTreeMap::new();
    let mut routes = AgentRoutes::default();
    let mut revalidate = tokio::time::interval_at(
        tokio::time::Instant::now() + REVALIDATE_EVERY,
        REVALIDATE_EVERY,
    );
    revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                        #[cfg(test)]
                        rt.progress("finished", Some(id.clone()));
                        initial.remove(&id);
                        match result {
                            Err(error) => {
                                let delay = backoff.get(&id).map_or(RETRY_MIN, |last| last.saturating_mul(2).min(RETRY_MAX));
                                backoff.insert(id.clone(), delay);
                                next_observation.insert(id.clone(), Instant::now() + delay);
                                pending.entry(id.clone()).or_insert(None);
                                let failure = ObservationFailure { reason: error.to_string(), restart: restart.contains(&id) };
                                errors.send_modify(|errors| { errors.insert(id.clone(), failure); });
                                tracing::warn!(agent = %id, %error, retry_in = ?delay, "Agent observation failed");
                            }
                            Ok(()) => {
                                // An agent that is Down by now keeps no timer entry.
                                if routes.agents.contains_key(&id) {
                                    next_observation.insert(id.clone(), Instant::now() + MIN_OBSERVE_INTERVAL);
                                } else {
                                    next_observation.remove(&id);
                                }
                                backoff.remove(&id);
                                // A binding wake that arrived before or during the restart check was
                                // dropped, or fenced by the check's own write: look at the rows once more.
                                if restart.remove(&id) {
                                    if let Err(error) = schedule(&rt, &mut pending, BTreeSet::from([id.clone()]), Wake::BindOnly).await {
                                        tracing::warn!(agent = %id, %error, "Agent scheduling failed; waiting for next wake");
                                    }
                                }
                                errors.send_modify(|errors| { errors.remove(&id); });
                            }
                        }
                    }
                    Some(Err(error)) => return Err(AgentError::Internal(format!("agent observer: {error}"))),
                    None => {},
                }
            }
            _ = async { if let Some(at) = next_wake { tokio::time::sleep_until(at).await; } }, if next_wake.is_some() => {},
            _ = revalidate.tick() => {
                #[cfg(test)]
                if rt.revalidation_paused.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                // A seat in an unreadable episode is the diagnosis worker's. Once
                // the worker is done with it (dormant), it is looked at again once
                // a minute: its screen can become readable with no harness event.
                let now = Instant::now();
                for (id, _) in routes.agents.iter().filter(|(id, route)| route.revalidating && (!rt.screens.unreadable(id) || rt.screens.take_relook(id, now))) {
                    pending.entry(id.clone()).or_insert(None);
                }
            },
            // Before registry events: routing must reflect a committed lifecycle change
            // (an agent that just left Ready) before a later row event is filtered by it.
            event = committed.recv() => {
                match event {
                    Ok(event) if event.kind.starts_with("agent.") => {
                        let id: Option<AgentId> = event.body.get("agent_id").and_then(serde_json::Value::as_str).and_then(|id| id.parse().ok());
                        if !matches!(event.kind.as_str(), "agent.revalidation_requested" | "agent.editor_ready") {
                            if let Some(id) = &id { rt.screens.invalidate(id, event.seq); }
                        }
                        routes.dirty = true;
                        match rt.org.snapshot().await {
                            Ok(state) => routes.refresh(&state),
                            Err(error) => tracing::warn!(%error, "Agent routing refresh deferred"),
                        }
                        if matches!(event.kind.as_str(), "agent.up_requested" | "agent.adopted" | "agent.pane_created") {
                            if let Some(id) = id {
                                pull_in(&BTreeSet::from([id.clone()]), &backoff, &mut next_observation);
                                if let Err(error) = schedule(&rt, &mut pending, BTreeSet::from([id]), Wake::Observe).await {
                                    tracing::warn!(%error, "Agent scheduling failed; waiting for next wake");
                                }
                            }
                            #[cfg(test)]
                            rt.progress("committed_scheduled", None);
                        } else if event.kind == "agent.clear_requested" {
                            // A clear types only after its own capture showed the harness
                            // in front: a refusal reason still stored is out of date, and
                            // no pass need follow the clear to drop it.
                            if let Some((id, route)) = id.as_ref().and_then(|id| Some((id, routes.agents.get(id)?))) {
                                if let Some(pane) = &route.pane {
                                    rt.set_refusal(id, pane, (None, None));
                                }
                            }
                            #[cfg(test)]
                            rt.progress("refusal_dropped", id);
                        } else if let Some(id) = id.filter(|_| event.kind == "agent.down") {
                            // The failure was about a pane the agent no longer has: it
                            // must not outlive the Down and hold the next `up`. A queued
                            // retry now runs at once and finds the agent Down.
                            restart.remove(&id);
                            backoff.remove(&id);
                            next_observation.remove(&id);
                            errors.send_modify(|errors| { errors.remove(&id); });
                        }
                    },
                    // `session.kill` closed a pane without a reservation: look at the
                    // agent that had it now. The registry's row removal comes later,
                    // and never for an agent whose session has no row.
                    Ok(event) if killed_pane(&event).is_some() => {
                        let pane = killed_pane(&event);
                        let ids: BTreeSet<AgentId> = routes.agents.iter()
                            .filter(|(_, route)| route.pane.as_deref() == pane)
                            .map(|(id, _)| id.clone())
                            .collect();
                        if !ids.is_empty() {
                            pull_in(&ids, &backoff, &mut next_observation);
                            if let Err(error) = schedule(&rt, &mut pending, ids, Wake::Observe).await {
                                tracing::warn!(%error, "Agent scheduling failed; waiting for next wake");
                            }
                            #[cfg(test)]
                            rt.progress("killed_scheduled", None);
                        }
                    },
                    Ok(_) => {},
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let result = match routes.rebuild(&rt).await {
                            Ok(ids) => schedule(&rt, &mut pending, ids, Wake::Bind).await,
                            Err(error) => Err(error),
                        };
                        if let Err(error) = result { tracing::warn!(%error, "Agent rescan deferred until next wake"); }
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            event = events.recv() => {
                if matches!(event, Err(broadcast::error::RecvError::Closed)) { break; }
                #[cfg(test)]
                rt.progress("scheduling", None);
                let result = match routes.wake(&rt, event).await {
                    Ok(ids) => {
                        pull_in(&ids, &backoff, &mut next_observation);
                        schedule(&rt, &mut pending, ids, Wake::Bind).await
                    }
                    Err(error) => Err(error),
                };
                #[cfg(test)]
                rt.progress(if result.is_ok() { "scheduled" } else { "schedule_failed" }, None);
                if let Err(error) = result {
                    tracing::warn!(%error, "Agent scheduling failed; waiting for next wake");
                }
            }
        }
    }
    // Keep the control/org alive until all admitted Observe jobs finish cleanup.
    while jobs.join_next().await.is_some() {}
    Ok(())
}

/// The pane that a successful `session.kill` closed, from its audit event.
/// A lifecycle flow's own kill (`agent.down`, a respawn) is logged under its
/// own method and records its agent itself.
///
/// The event is best effort: the control audit queue holds 32 and drops what
/// does not fit. A dropped one leaves the old wake, the registry removing the
/// session's row. An agent whose session has no row then stays recorded as it
/// was, on a dead pane, until another wake of it (a later event, a lagged
/// rebuild or a daemon restart): no worse than before this wake existed.
fn killed_pane(event: &crate::eventlog::Envelope) -> Option<&str> {
    let field = |pointer| {
        event
            .body
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
    };
    (event.kind == "control.killed"
        && field("/action/method") == Some("session.kill")
        && field("/action/outcome") == Some("ok"))
    .then(|| field("/pane"))
    .flatten()
}

/// A wake for an agent whose observations are failing brings its next retry in
/// to `RETRY_MIN` from now. Never sooner: a busy agent on an unreadable pane
/// would otherwise be retried, and logged, on every hook.
fn pull_in(
    ids: &BTreeSet<AgentId>,
    backoff: &BTreeMap<AgentId, Duration>,
    next_observation: &mut BTreeMap<AgentId, Instant>,
) {
    let soonest = Instant::now() + RETRY_MIN;
    for id in ids.iter().filter(|id| backoff.contains_key(*id)) {
        if let Some(at) = next_observation.get_mut(id) {
            *at = (*at).min(soonest);
        }
    }
}

/// What a scheduled observation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// Observe without a binding candidate.
    Observe,
    /// Observe, with a candidate when the current rows offer one.
    Bind,
    /// Queue nothing unless the current rows offer a candidate.
    BindOnly,
}

async fn schedule(
    rt: &AgentRuntime,
    pending: &mut BTreeMap<AgentId, Option<BindingCandidate>>,
    ids: BTreeSet<AgentId>,
    wake: Wake,
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
        let candidate = if wake != Wake::Observe {
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
        if candidate.is_some() || (wake != Wake::BindOnly && !pending.contains_key(&agent.id)) {
            pending.insert(agent.id.clone(), candidate);
        }
    }
    Ok(())
}

#[cfg(test)]
mod projection_join_tests {
    use super::*;
    use atm_core::SessionStatus;

    // Break caught: org projections borrow context/activity from a row that
    // agent.list rejects (or lose a valid row through a different fallback).
    #[test]
    fn eligible_live_row_matches_agent_list() {
        let now = "2026-01-15T12:00:00Z".parse().unwrap();
        let base = Agent {
            id: "alpha/worker".parse().unwrap(),
            role: Some(atm_org::parse_role("worker", "+++\nharness='pi'\n+++\n").unwrap()),
            status: AgentStatus::Ready,
            pane: Some("%7".into()),
            session_id: Some("live".into()),
            excluded_session_id: None,
            reason: None,
            updated_at: now,
            bound_at: None,
        };
        let row = SessionView {
            id: "live".into(),
            tmux_pane: Some("%7".into()),
            harness: "pi".into(),
            model: "live-model".into(),
            status: SessionStatus::AttentionNeeded,
            context_percentage: 12.25,
            ..Default::default()
        };
        let state = OrgState::default();
        let check = |label: &str,
                     state: &OrgState,
                     agent: &Agent,
                     rows: &[SessionView],
                     want: Option<&str>| {
            let live = eligible_live_row(state, agent, rows);
            assert_eq!(live.map(|r| r.id.as_str()), want, "{label}");
            let entry = list_entry(state, agent, rows);
            assert_eq!(
                entry.activity,
                want.map(|_| SessionStatus::AttentionNeeded),
                "{label}"
            );
            if want.is_some() {
                assert_eq!(entry.model.as_deref(), Some("live-model"), "{label}");
                assert_eq!(live.unwrap().context_percentage, 12.25, "{label}");
            }
        };
        for status in [AgentStatus::Ready, AgentStatus::Down, AgentStatus::Stuck] {
            let agent = Agent {
                status,
                ..base.clone()
            };
            check(
                "durable state does not change eligibility",
                &state,
                &agent,
                std::slice::from_ref(&row),
                Some("live"),
            );
        }
        for (label, agent) in [
            (
                "no pane",
                Agent {
                    pane: None,
                    ..base.clone()
                },
            ),
            (
                "unpinned",
                Agent {
                    session_id: None,
                    ..base.clone()
                },
            ),
            (
                "other pin",
                Agent {
                    session_id: Some("other".into()),
                    ..base.clone()
                },
            ),
            (
                "excluded",
                Agent {
                    excluded_session_id: Some("live".into()),
                    ..base.clone()
                },
            ),
        ] {
            check(label, &state, &agent, std::slice::from_ref(&row), None);
        }
        for (label, bad) in [
            (
                "wrong pane",
                SessionView {
                    tmux_pane: Some("%8".into()),
                    ..row.clone()
                },
            ),
            (
                "no row pane",
                SessionView {
                    tmux_pane: None,
                    ..row.clone()
                },
            ),
            (
                "child",
                SessionView {
                    parent_session_id: Some("parent".into()),
                    ..row.clone()
                },
            ),
            (
                "wrong harness",
                SessionView {
                    harness: "codex".into(),
                    ..row.clone()
                },
            ),
            (
                "pending",
                SessionView {
                    id: "pending-1".into(),
                    ..row.clone()
                },
            ),
        ] {
            check(label, &state, &base, &[bad], None);
        }
        check(
            "ambiguous",
            &state,
            &base,
            &[
                row.clone(),
                SessionView {
                    id: "other".into(),
                    ..row.clone()
                },
            ],
            None,
        );
        check(
            "ineligible sibling",
            &state,
            &base,
            &[
                row.clone(),
                SessionView {
                    harness: "codex".into(),
                    ..row.clone()
                },
            ],
            Some("live"),
        );
        check("empty", &state, &base, &[], None);
        let adopted = Agent {
            role: None,
            ..base.clone()
        };
        check(
            "adopted unanimous live harness",
            &state,
            &adopted,
            std::slice::from_ref(&row),
            Some("live"),
        );
        check(
            "adopted mixed live harness",
            &state,
            &adopted,
            &[
                row.clone(),
                SessionView {
                    harness: "codex".into(),
                    ..row.clone()
                },
            ],
            None,
        );
        let mut saved = OrgState::default();
        saved
            .apply(
                1,
                now,
                &atm_org::OrgEvent::ProjectRegistered {
                    project_id: base.id.project.clone(),
                    root: "/alpha".into(),
                },
            )
            .unwrap();
        saved
            .apply(
                2,
                now,
                &atm_org::OrgEvent::AgentAdopted {
                    agent_id: base.id.clone(),
                    pane: "%7".into(),
                    session_id: Some("live".into()),
                },
            )
            .unwrap();
        saved
            .apply(
                3,
                now,
                &atm_org::OrgEvent::SessionSummary(atm_org::SessionSummary {
                    session_id: "saved".into(),
                    pane: Some("%7".into()),
                    agent_id: Some(base.id.clone()),
                    harness: Harness::Pi,
                    model: Some("saved-model".into()),
                    cost: None,
                    first_prompt: None,
                    started: now,
                    ended: None,
                }),
            )
            .unwrap();
        check(
            "saved harness wins over mixed rows",
            &saved,
            &adopted,
            &[
                row.clone(),
                SessionView {
                    harness: "codex".into(),
                    ..row
                },
            ],
            Some("live"),
        );
        assert_eq!(
            list_entry(&saved, &adopted, &[]).model.as_deref(),
            Some("saved-model")
        );
    }
}

#[cfg(test)]
#[path = "agent_health_tests.rs"]
mod health_tests;

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
            bound_at: None,
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
