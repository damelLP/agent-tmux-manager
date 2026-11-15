//! Handover on command, and one note when a turn ends above the context line.
//!
//! `agent.handover` asks an agent to write its state to its memory file,
//! waits until that turn has ended and the file has changed, and clears the
//! session. It adds no way to type into a pane: the request is a message
//! delivered by the message runtime with all its guards, and the clear is
//! `agent.clear` with its own reservation, guards and cleanup. No reservation
//! is held while the agent saves, because delivery needs the pane.
//!
//! The watcher that sends the note, and that runs this handover itself for a
//! seat whose role file switches that on, is `agent_auto_handover`.
//!
//! # Steps, and what each refusal leaves behind
//!
//! | Step | Goes on when | Refused when |
//! |---|---|---|
//! | Admission | the agent is ready with a bound session and no handover of it runs | otherwise; nothing was sent |
//! | Save requested | the request's delivery is confirmed | delivery failed for good, or the deadline: the request stays queued, a later handover reuses it, the owner's give-up cancels it |
//! | Saving | the session is idle and the memory file has changed | the turn ended and the file did not change; the deadline; the agent or its session went away |
//! | Saved | `agent.clear` answers | every refusal of `agent.clear`, with "saved its state" in front |
//!
//! Nothing is cleared unless the file changed. Changed means: a regular file
//! whose length or modification time differs from what was read before the
//! request was queued, or that did not exist then. Its content is not read.
//!
//! The turn's end is taken from events only. The task subscribes to the
//! registry and to committed events before it queues the request. It reads
//! registry events first, and reads that channel empty every time it looks
//! at committed state, so neither the end of an earlier turn nor a status
//! from before the delivery is taken for the end of the save turn. It refuses early only for a turn it saw start
//! and end after the delivery; otherwise an idle session with an unchanged
//! file waits for the deadline. A harness that shows idle while it works can
//! at worst lead to a clear on a busy screen, which the clear refuses.
//!
//! # Callers
//!
//! Anyone may ask (attribution, not protection), also an agent about itself.
//! An agent calls from inside its own turn, which must end before the request
//! can be delivered, so that call answers `started` at once and the handover
//! carries on. If it then fails, the agent's manager gets one note. A caller
//! that waits gets the refusal as its answer and no note. A caller that goes
//! away does not stop the handover; if its call has ended by the time the
//! handover fails, the manager gets that same note. The daemon ends a call
//! when the caller's socket closes (the connection drops this method's wait
//! then) and at the method's deadline. A client that only half-closes keeps
//! its call, and gets the answer.
//!
//! The handover policy in `atm_org::idle` is not asked: a command may hand
//! over at any context share.
//!
//! # Limits
//!
//! - A daemon restart loses a running handover. Its request is still
//!   delivered and nothing is cleared; a new handover reuses an open request.
//! - The note at the context line is remembered per agent in memory, so a
//!   restart can repeat one note for a session.
//! - The manager comes from the role as it was at `agent up`. An adopted
//!   agent has no role on record and its notes go to the owner.

use super::{
    agent_context::{detached_read, manager_address, paths},
    agents::{AgentError, AgentHandle, ClearParams, ClearResult},
    OrgHandle,
};
use crate::{
    registry::{RegistryHandle, SessionEvent},
    server::methods::{parse_params, CallContext, MethodError, MethodRouter},
};
use atm_core::{SessionId, SessionStatus};
use atm_org::{
    idle::{handover_step, HandoverInput, HandoverLimits, HandoverReason, HandoverStep, WaitsFor},
    Address, AgentId, AgentStatus, DeliveryStatus, MessageContent, MessageId, MessageKind,
    OrgCommand, OrgState,
};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    future::Future,
    io,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{broadcast, oneshot, Semaphore},
    time::Instant,
};

/// The save may take this long: queueing, delivery and the save turn.
pub const HANDOVER_TIMEOUT_MS: RangeInclusive<u64> = 30_000..=1_800_000;
/// The clear after the save gets `agent.clear`'s longest timeout.
const CLEAR_TIMEOUT_MS: u64 = 120_000;
/// The longest save, the clear, and room for the answer.
const METHOD_DEADLINE: Duration = Duration::from_secs(1_800 + 150);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoverParams {
    pub agent: AgentId,
    #[serde(default = "default_handover_timeout_ms")]
    pub timeout_ms: u64,
}
#[must_use]
pub const fn default_handover_timeout_ms() -> u64 {
    600_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoverState {
    /// The agent saved and its session was cleared.
    Cleared,
    /// The caller is the agent itself: the handover carries on after its turn.
    Started,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoverResult {
    pub agent_id: AgentId,
    /// The message that asked the agent to save.
    pub message_id: MessageId,
    pub state: HandoverState,
    /// The session the clear announced, if the harness announces one.
    pub session_id: Option<SessionId>,
}

/// The message that asks an agent to save. Recognized by equality, so an open
/// one is found again and no other message is taken for it.
pub(super) fn save_request(agent: &AgentId, memory: &Path) -> MessageContent {
    MessageContent {
        from: Address::System,
        to: Address::Agent(agent.clone()),
        kind: MessageKind::Note,
        body: format!(
            "Handover: write what your next session needs to {}, then end your turn. \
             Your session is cleared after that.",
            memory.display()
        ),
        attachments: Vec::new(),
        task: None,
    }
}

/// The oldest save request to this agent that is not yet delivered or given up.
pub(super) fn open_save_request(state: &OrgState, request: &MessageContent) -> Option<MessageId> {
    state
        .messages()
        .find(|message| {
            message.content == *request
                && matches!(
                    message.status,
                    DeliveryStatus::Queued | DeliveryStatus::Delivering
                )
        })
        .map(|message| message.id)
}

/// What a file looked like, without its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

/// `None` for a file that does not exist; anything but a regular file is refused.
pub(super) fn fingerprint(path: &Path) -> Result<Option<Fingerprint>, AgentError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(Fingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })),
        Ok(_) => Err(AgentError::Refused(format!(
            "{}: memory is not a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AgentError::Internal(format!("{}: {error}", path.display()))),
    }
}

/// The agent saved: the file is there and is not what it was before.
pub(super) fn changed(before: Option<&Fingerprint>, after: Option<&Fingerprint>) -> bool {
    after.is_some() && after != before
}

/// What the handover has seen of the save so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SaveFacts {
    /// The request's delivery is confirmed.
    pub delivered: bool,
    /// The session's last known status is idle.
    pub idle: bool,
    /// The session was seen not idle after the delivery: the save turn ran.
    pub worked: bool,
}

impl SaveFacts {
    pub(super) fn status(&mut self, status: SessionStatus) {
        self.idle = status == SessionStatus::Idle;
        self.worked |= self.delivered && !self.idle;
    }
    pub(super) fn mark_delivered(&mut self) {
        self.delivered = true;
        self.worked |= !self.idle;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveProgress {
    Wait,
    Saved,
    /// The save turn was seen to end and the file is as it was.
    Unchanged,
}

/// The one decision of the save: go on only when the request was delivered,
/// the session is idle and the file changed; give up early only for a turn
/// that was seen to run after the delivery.
pub(super) fn save_progress(facts: SaveFacts, changed: bool) -> SaveProgress {
    match (facts.delivered && facts.idle, changed, facts.worked) {
        (false, ..) | (true, false, false) => SaveProgress::Wait,
        (true, true, _) => SaveProgress::Saved,
        (true, false, true) => SaveProgress::Unchanged,
    }
}

type Answer<T> = Pin<Box<dyn Future<Output = Result<T, AgentError>> + Send>>;
/// Resolves when a handover's save has taken too long.
pub(super) type Deadline = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The agent service as the handover uses it; tests put a recorder here.
pub(super) trait Clearer: Send + Sync + 'static {
    /// Named-agent operations can run at all.
    fn available(&self) -> Answer<()>;
    fn clear(&self, agent: AgentId) -> Answer<ClearResult>;
}

impl Clearer for AgentHandle {
    fn available(&self) -> Answer<()> {
        let agents = self.clone();
        Box::pin(async move { agents.reconciled().await })
    }
    fn clear(&self, agent: AgentId) -> Answer<ClearResult> {
        let agents = self.clone();
        Box::pin(async move {
            let params = ClearParams {
                agent,
                timeout_ms: CLEAR_TIMEOUT_MS,
            };
            agents.clear(params).await
        })
    }
}

/// One handover at a time per agent; released when the handover ends.
struct Running {
    all: Arc<Mutex<BTreeSet<AgentId>>>,
    agent: AgentId,
}

impl Running {
    fn claim(all: &Arc<Mutex<BTreeSet<AgentId>>>, agent: &AgentId) -> Result<Self, AgentError> {
        let mut running = all
            .lock()
            .map_err(|_| AgentError::Internal("handover bookkeeping was lost".into()))?;
        if !running.insert(agent.clone()) {
            return Err(AgentError::Conflict(format!(
                "a handover of {agent} is already running"
            )));
        }
        Ok(Self {
            all: all.clone(),
            agent: agent.clone(),
        })
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(mut running) = self.all.lock() {
            running.remove(&self.agent);
        }
    }
}

/// The handover service: one per daemon, shared by the method and the
/// watcher, so that a command and the daemon never hand one agent over at
/// the same time.
#[derive(Clone)]
pub struct Handovers {
    pub(super) org: OrgHandle,
    pub(super) registry: RegistryHandle,
    clearer: Arc<dyn Clearer>,
    running: Arc<Mutex<BTreeSet<AgentId>>>,
    files: Arc<Semaphore>,
}

fn registry_stopped() -> MethodError {
    AgentError::Unavailable("session registry stopped".into()).into()
}

fn refused(message: String) -> MethodError {
    MethodError::new(error_code::REFUSED, message)
}

impl Handovers {
    /// The service that clears through `agents`.
    #[must_use]
    pub fn for_agents(agents: AgentHandle, org: OrgHandle, registry: RegistryHandle) -> Self {
        Self::new(org, registry, Arc::new(agents))
    }

    pub(super) fn new(org: OrgHandle, registry: RegistryHandle, clearer: Arc<dyn Clearer>) -> Self {
        Self {
            org,
            registry,
            clearer,
            running: Arc::default(),
            files: Arc::new(Semaphore::new(1)),
        }
    }

    async fn stat(&self, path: &Path) -> Result<Option<Fingerprint>, MethodError> {
        let file = path.to_owned();
        detached_read(self.files.clone(), path, move || fingerprint(&file)).await
    }

    async fn execute(
        &self,
        command: OrgCommand,
    ) -> Result<Vec<crate::eventlog::Envelope>, MethodError> {
        Ok(self
            .org
            .execute(command, Utc::now())
            .await
            .map_err(AgentError::from)?)
    }

    pub(super) async fn request(
        &self,
        p: HandoverParams,
        caller: CallContext,
    ) -> Result<HandoverResult, MethodError> {
        let deadline = Instant::now() + Duration::from_millis(p.timeout_ms);
        self.request_until(p, caller, Box::pin(tokio::time::sleep_until(deadline)))
            .await
    }

    /// Admission and the save request, then the rest as a task of its own.
    /// The save is given up when `deadline` resolves; tests resolve it by
    /// hand, so that no test waits for time to pass.
    pub(super) async fn request_until(
        &self,
        p: HandoverParams,
        caller: CallContext,
        deadline: Deadline,
    ) -> Result<HandoverResult, MethodError> {
        let Admitted { job, own, manager } = self.admit(p, caller, deadline).await?;
        let (agent_id, message_id) = (job.agent.clone(), job.message_id);
        let org = self.org.clone();
        let (answer, waiting) = oneshot::channel();
        // The agent itself does not wait: with no receiver from the start,
        // its handover's failure cannot be taken for a delivered answer.
        let waiting = (!own).then_some(waiting);
        tokio::spawn(async move {
            let agent = job.agent.clone();
            // An answer nobody takes comes back. That is the agent's own
            // handover, or a caller whose call has ended: a failure is then
            // told to the agent's manager instead.
            if let Err(Err(error)) = answer.send(job.run().await) {
                let note = MessageContent {
                    from: Address::System,
                    to: manager,
                    kind: MessageKind::Note,
                    body: format!("handover of {agent} did not complete: {}", error.0.message),
                    attachments: Vec::new(),
                    task: None,
                };
                if let Err(error) = org.execute(OrgCommand::SendMessage(note), Utc::now()).await {
                    tracing::warn!(%agent, %error, "Handover failure note was not recorded");
                }
            }
        });
        let Some(waiting) = waiting else {
            return Ok(HandoverResult {
                agent_id,
                message_id,
                state: HandoverState::Started,
                session_id: None,
            });
        };
        waiting
            .await
            .map_err(|_| AgentError::Internal("handover task stopped".into()))?
    }

    /// The checks, the save request and its audit: everything before the
    /// handover waits. Refused here, nothing was sent.
    pub(super) async fn admit(
        &self,
        p: HandoverParams,
        caller: CallContext,
        deadline: Deadline,
    ) -> Result<Admitted, MethodError> {
        self.clearer.available().await?;
        // Subscribed before anything is read or sent: what follows is an
        // event, and what is read below is what was there before.
        let committed = self.org.committed.subscribe();
        let rows = self.registry.subscribe();
        let state = self.org.snapshot().await.map_err(AgentError::from)?;
        let agent = state
            .agent(&p.agent)
            .ok_or_else(|| AgentError::NotFound(format!("agent {} was not found", p.agent)))?;
        if agent.status != AgentStatus::Ready {
            return Err(refused(format!(
                "agent {} is {}; only a ready agent can hand over",
                p.agent,
                format!("{:?}", agent.status).to_lowercase()
            )));
        }
        let session = agent.session_id.clone().ok_or_else(|| {
            refused(format!(
                "agent {} has no session; atm cannot see its turn end",
                p.agent
            ))
        })?;
        let (_, memory) = paths(&state, agent)?;
        let own = caller
            .from_pane
            .as_ref()
            .is_some_and(|pane| agent.pane.as_deref() == Some(pane.0.as_str()));
        let manager = manager_address(&agent.id, agent.role.as_ref()).unwrap_or(Address::Owner);
        let running = Running::claim(&self.running, &p.agent)?;
        let before = self.stat(&memory).await?;
        let request = save_request(&p.agent, &memory);
        let message_id = match open_save_request(&state, &request) {
            Some(open) => open,
            None => {
                let sent = self.execute(OrgCommand::SendMessage(request)).await?;
                let sent = sent.first().ok_or_else(|| {
                    AgentError::Internal("the save request committed no event".into())
                })?;
                MessageId::new(sent.seq)
            }
        };
        self.execute(OrgCommand::SaveRequested {
            agent_id: p.agent.clone(),
            message_id,
        })
        .await?;
        let mut facts = SaveFacts::default();
        if let Some(row) = self.registry.get_session(session.clone()).await {
            facts.status(row.status);
        }
        let job = Job {
            service: self.clone(),
            agent: p.agent.clone(),
            pane: agent.pane.clone(),
            session,
            memory,
            before,
            message_id,
            deadline,
            seconds: p.timeout_ms / 1000,
            committed,
            rows,
            facts,
            needs_input: false,
            _running: running,
        };
        Ok(Admitted { job, own, manager })
    }
}

/// A handover that passed admission: its request is queued and recorded.
pub(super) struct Admitted {
    pub job: Job,
    /// The caller is the agent itself, so nobody waits for the answer.
    pub own: bool,
    /// Who is told when a handover nobody waits for fails.
    pub manager: Address,
}

/// One handover from its save request on.
pub(super) struct Job {
    service: Handovers,
    agent: AgentId,
    pane: Option<String>,
    /// The session that was asked to save.
    session: SessionId,
    memory: PathBuf,
    before: Option<Fingerprint>,
    pub(super) message_id: MessageId,
    deadline: Deadline,
    seconds: u64,
    committed: broadcast::Receiver<Arc<crate::eventlog::Envelope>>,
    rows: broadcast::Receiver<SessionEvent>,
    facts: SaveFacts,
    /// The session's last status was "needs input": a dialog holds the turn.
    needs_input: bool,
    _running: Running,
}

impl Job {
    pub(super) async fn run(mut self) -> Result<HandoverResult, MethodError> {
        self.saved().await?;
        self.service
            .execute(OrgCommand::Saved {
                agent_id: self.agent.clone(),
                message_id: self.message_id,
            })
            .await?;
        let cleared = self
            .service
            .clearer
            .clear(self.agent.clone())
            .await
            .map_err(|error| {
                let mut error = MethodError::from(error);
                error.0.message = format!(
                    "{} saved its state ({}); {}",
                    self.agent, self.message_id, error.0.message
                );
                error
            })?;
        Ok(HandoverResult {
            agent_id: self.agent.clone(),
            message_id: self.message_id,
            state: HandoverState::Cleared,
            session_id: cleared.session_id,
        })
    }

    /// Returns once the save turn has ended with the memory file changed.
    async fn saved(&mut self) -> Result<(), MethodError> {
        // The request may have been delivered before this task looked.
        self.refresh().await?;
        loop {
            if self.facts.delivered && self.facts.idle {
                let after = self.service.stat(&self.memory).await?;
                match save_progress(self.facts, changed(self.before.as_ref(), after.as_ref())) {
                    SaveProgress::Saved => return Ok(()),
                    SaveProgress::Unchanged => {
                        return Err(refused(format!(
                            "{} ended its save turn and {} did not change; nothing was cleared",
                            self.agent,
                            self.memory.display()
                        )))
                    }
                    SaveProgress::Wait => {}
                }
            }
            tokio::select! {
                // Registry events first: the end of an earlier turn is in that
                // channel before the delivery it allowed is committed, and
                // must be read before it.
                biased;
                row = self.rows.recv() => match row {
                    Ok(event) => self.row(event)?,
                    Err(broadcast::error::RecvError::Lagged(_)) => self.reread_row().await,
                    Err(broadcast::error::RecvError::Closed) => return Err(registry_stopped()),
                },
                event = self.committed.recv() => match event {
                    Ok(event) if !self.concerns(&event) => {}
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => self.refresh().await?,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(AgentError::Unavailable("organisation service stopped".into()).into());
                    }
                },
                () = &mut self.deadline => return Err(self.timed_out()),
            }
        }
    }

    fn observe(&mut self, status: SessionStatus) {
        self.facts.status(status);
        self.needs_input = status == SessionStatus::AttentionNeeded;
    }

    /// The event names this handover's agent or its save request.
    fn concerns(&self, event: &crate::eventlog::Envelope) -> bool {
        let named = |key: &str, value: String| {
            event.body.get(key).and_then(serde_json::Value::as_str) == Some(value.as_str())
        };
        named("agent_id", self.agent.to_string())
            || named("message_id", self.message_id.to_string())
    }

    /// One registry event: this session's status, or its end.
    fn row(&mut self, event: SessionEvent) -> Result<(), MethodError> {
        match event {
            SessionEvent::Updated { session } if session.id == self.session => {
                self.observe(session.status);
            }
            SessionEvent::Removed { session_id, .. } if session_id == self.session => {
                return Err(refused(format!(
                    "{}'s session ended before it had saved; nothing was cleared",
                    self.agent
                )));
            }
            _ => {}
        }
        Ok(())
    }

    /// After missed registry events: the session's row as it is now.
    async fn reread_row(&mut self) {
        let row = self.service.registry.get_session(self.session.clone());
        if let Some(row) = row.await {
            self.observe(row.status);
        }
    }

    /// Read the request's delivery and the agent's state from committed
    /// state, and before that every registry event that is already there.
    ///
    /// The harness's prompt event sets the session working in the registry
    /// before the delivery it confirms is committed. So once the snapshot
    /// shows the delivery, the save turn's first status is in the registry
    /// channel, read or not. Reading the channel empty here means a delivery
    /// is never judged against a status from before the save turn.
    async fn refresh(&mut self) -> Result<(), MethodError> {
        let state = self
            .service
            .org
            .snapshot()
            .await
            .map_err(AgentError::from)?;
        loop {
            match self.rows.try_recv() {
                Ok(event) => self.row(event)?,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => self.reread_row().await,
                Err(broadcast::error::TryRecvError::Closed) => return Err(registry_stopped()),
            }
        }
        let message = state.message(&self.message_id).ok_or_else(|| {
            AgentError::Internal(format!("save request {} was not found", self.message_id))
        })?;
        match message.status {
            DeliveryStatus::Delivered if !self.facts.delivered => self.facts.mark_delivered(),
            DeliveryStatus::Failed => {
                return Err(refused(format!(
                    "the save request {} could not be delivered to {}: {}; nothing was cleared",
                    self.message_id,
                    self.agent,
                    message.failure.as_deref().unwrap_or("no reason recorded")
                )))
            }
            _ => {}
        }
        let still = state.agent(&self.agent).filter(|agent| {
            agent.status == AgentStatus::Ready && agent.session_id.as_ref() == Some(&self.session)
        });
        if still.is_none() {
            return Err(refused(format!(
                "{} is no longer ready in the session it was asked to save in; nothing was cleared",
                self.agent
            )));
        }
        Ok(())
    }

    fn timed_out(&self) -> MethodError {
        let (agent, id, seconds) = (&self.agent, self.message_id, self.seconds);
        let reason = if !self.facts.delivered {
            format!(
                "the save request {id} was not delivered to {agent} in {seconds} s; \
                 `atm inbox --for {agent} --id {id}` shows what holds it; it stays queued and a \
                 later handover reuses it; `atm msg give-up {id}` cancels it"
            )
        } else if !self.facts.idle {
            let dialog = match (&self.pane, self.needs_input) {
                (Some(pane), true) => format!("; it waits at a dialog in {pane}"),
                _ => String::new(),
            };
            format!("{agent}'s save turn did not end in {seconds} s{dialog}")
        } else {
            format!(
                "{agent} was idle after the save request {id} and {} did not change in {seconds} s",
                self.memory.display()
            )
        };
        MethodError::new(
            error_code::TIMEOUT,
            format!("{reason}; nothing was cleared"),
        )
    }
}

/// Always advertised; with agents or persistence disabled it answers unavailable.
pub fn register_agent_handover_method(
    router: &mut MethodRouter,
    service: Handovers,
) -> Result<(), MethodError> {
    router.register("agent.handover", METHOD_DEADLINE, move |caller, params| {
        let service = service.clone();
        async move {
            let p: HandoverParams = parse_params(params)?;
            if !HANDOVER_TIMEOUT_MS.contains(&p.timeout_ms) {
                return Err(AgentError::InvalidParams(
                    "timeout_ms must be in 30000..=1800000".into(),
                )
                .into());
            }
            let result = service.request(p, caller).await?;
            serde_json::to_value(result)
                .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))
        }
    })
}

/// The handover policy flags a turn that ended at `share` percent.
fn at_the_line(share: f64, limits: &HandoverLimits, now: DateTime<Utc>) -> bool {
    let input = HandoverInput {
        context_percent: Some(share),
        idle_since: Some(now),
        // A turn end has no idle time, so what the agent waits for and the
        // away flag cannot change the answer.
        waits_for: WaitsFor::Agent,
        away: false,
        automatic: false,
    };
    handover_step(&input, limits, now) == HandoverStep::Flag(HandoverReason::ContextLine)
}

/// The note for an idle `session` at `share` percent, and whose agent it is:
/// addressed to the manager of the ready agent bound to that session, when
/// the handover policy flags the share. `None` when nothing is due.
pub(super) fn context_note(
    state: &OrgState,
    session: &SessionId,
    share: f64,
    limits: &HandoverLimits,
    now: DateTime<Utc>,
) -> Option<(AgentId, MessageContent)> {
    if !at_the_line(share, limits, now) {
        return None;
    }
    let agent = state.agents().find(|agent| {
        agent.status == AgentStatus::Ready && agent.session_id.as_ref() == Some(session)
    })?;
    let note = MessageContent {
        from: Address::System,
        to: manager_address(&agent.id, agent.role.as_ref()).unwrap_or(Address::Owner),
        kind: MessageKind::Note,
        body: format!(
            "agent {} is at {share:.0}% of its context (line {}%). Hand it over: atm agent handover {}",
            agent.id, limits.context_line_percent, agent.id
        ),
        attachments: Vec::new(),
        task: None,
    };
    Some((agent.id.clone(), note))
}
