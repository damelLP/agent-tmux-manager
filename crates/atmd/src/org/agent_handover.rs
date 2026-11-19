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
//! | Admission | the agent is ready with a bound session and no handover of it runs | Preflight refusal queues no request; after dispatch a request may be queued or delivered even when handover fails; nothing is cleared. |
//! | Save requested | the request's delivery is confirmed | delivery failed for good, or the deadline: the request stays queued, a later handover reuses it, the owner's give-up cancels it |
//! | Saving | the session is idle and the memory file has changed | the turn ended and the file did not change; the deadline; the agent or its session went away |
//! | Saved | the same pane and session pass clear preflight | transient no-effect refusals retry for up to 20 s; changes of identity and every post-effect error stop |
//! | Cleared | the new binding has the exact start-line receipt | otherwise the outcome does not claim resume, even though the command acknowledged clear |
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
//! carries on. A waiting caller gets an error or physical-clear reply as
//! before; only unclaimed failures before Saved get a note. After Saved,
//! the job tells the manager once, independent of the RPC. It watches the
//! committed evidence for 30 s after clear, without a pane reservation.
//!
//! The handover policy in `atm_org::idle` is not asked: a command may hand
//! over at any context share.
//!
//! # Limits
//!
//! - A daemon restart loses in-flight work and the outcome note, not any
//!   effects or logs that already happened. A new handover reuses an open save request.
//! - The note at the context line is remembered per agent in memory, so a
//!   restart can repeat one note for a session.
//! - Outcome routing reads the current role with a bounded budget, falling
//!   back to the role at up and then the owner. The separate context-line
//!   note is unchanged.

use super::{
    agent_context::{detached_read, manager_address, paths},
    agent_lifecycle::{ClearAttempt, ClearOutcome, SavedSeat},
    agents::{AgentError, AgentHandle},
    reporting_line::outcome_manager,
    start_line::StartLineDisposition,
    OrgHandle,
};
use crate::{
    control::reservation::PaneId,
    eventlog::Envelope,
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
use tokio_util::sync::CancellationToken;

/// The save may take this long: queueing, delivery and the save turn.
pub const HANDOVER_TIMEOUT_MS: RangeInclusive<u64> = 30_000..=1_800_000;
/// The clear after the save gets `agent.clear`'s longest timeout.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(120);
const CLEAR_RETRY: Duration = Duration::from_secs(20);
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(4);
const RESUME_TIMEOUT: Duration = Duration::from_secs(30);
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
        delivery_notice: None,
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

pub(super) type Answer<T> = Pin<Box<dyn Future<Output = Result<T, AgentError>> + Send>>;
/// Resolves when a handover's save has taken too long.
pub(super) type Deadline = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The agent service as the handover uses it; tests put a recorder here.
pub(super) trait Clearer: Send + Sync + 'static {
    /// Named-agent operations can run at all.
    fn available(&self) -> Answer<()>;
    fn clear(
        &self,
        target: SavedSeat,
        retry_before: Option<Instant>,
        finish_by: Instant,
    ) -> Answer<ClearAttempt>;
    fn clear_wait(&self, _target: &SavedSeat, _reason: Option<String>) {}
}

impl Clearer for AgentHandle {
    fn available(&self) -> Answer<()> {
        let agents = self.clone();
        Box::pin(async move { agents.reconciled().await })
    }
    fn clear(
        &self,
        target: SavedSeat,
        retry_before: Option<Instant>,
        finish_by: Instant,
    ) -> Answer<ClearAttempt> {
        let agents = self.clone();
        Box::pin(async move { agents.clear_handover(target, retry_before, finish_by).await })
    }
    fn clear_wait(&self, target: &SavedSeat, reason: Option<String>) {
        self.set_handover_wait(target, reason);
    }
}

/// Clear the list overlay even if a retained job is dropped.
struct ClearWait<'a> {
    clearer: &'a dyn Clearer,
    target: &'a SavedSeat,
}

impl Drop for ClearWait<'_> {
    fn drop(&mut self) {
        self.clearer.clear_wait(self.target, None);
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
    stop: CancellationToken,
}

fn agent_service_stopped() -> MethodError {
    AgentError::Unavailable("agent service stopped".into()).into()
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
        let stop = agents.stop_token();
        Self::new(org, registry, Arc::new(agents), stop)
    }

    pub(super) fn new(
        org: OrgHandle,
        registry: RegistryHandle,
        clearer: Arc<dyn Clearer>,
        stop: CancellationToken,
    ) -> Self {
        Self {
            org,
            registry,
            clearer,
            running: Arc::default(),
            files: Arc::new(Semaphore::new(1)),
            stop,
        }
    }

    /// A retained job owns handles that keep the broadcast senders alive.
    /// Stop it with the agent service, not with its RPC caller or those channels.
    pub(super) async fn until_stopped<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => None,
            result = work => Some(result),
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

    /// Preflight, then both admission writes and the save as a retained task.
    /// The save is given up when `deadline` resolves; tests resolve it by
    /// hand, so that no test waits for time to pass.
    pub(super) async fn request_until(
        &self,
        p: HandoverParams,
        caller: CallContext,
        deadline: Deadline,
    ) -> Result<HandoverResult, MethodError> {
        let agent = p.agent.clone();
        let admitted = self
            .until_stopped(self.admit(p, caller, deadline, None))
            .await
            .ok_or_else(agent_service_stopped)??;
        let service = self.clone();
        let (answer, waiting) = oneshot::channel();
        tokio::spawn(async move {
            service
                .until_stopped(service.finish_admitted(
                    &agent,
                    admitted,
                    NoteOrigin::Command,
                    Some(answer),
                ))
                .await;
        });
        waiting.await.map_err(|_| {
            if self.stop.is_cancelled() {
                agent_service_stopped()
            } else {
                AgentError::Internal("handover task stopped".into()).into()
            }
        })?
    }

    /// The RPC answer and the notice deliberately have different proof bars.
    /// Release the reply after the physical clear, not after the resume turn.
    pub(super) async fn finish_admitted(
        &self,
        agent: &AgentId,
        admitted: Admitted,
        origin: NoteOrigin,
        mut reply: Option<HandoverReply>,
    ) {
        let Admitted { job, own, manager } = admitted;
        let mut job = match job.await {
            Ok(job) => job,
            Err(error) => {
                self.unsaved_failure(agent, manager, &origin, reply, error)
                    .await;
                return;
            }
        };
        if own {
            if let Some(reply) = reply.take() {
                let _ = reply.send(Ok(HandoverResult {
                    agent_id: agent.clone(),
                    message_id: job.message_id,
                    state: HandoverState::Started,
                    session_id: None,
                }));
            }
        }
        let result = job.run().await;
        let prefix = origin.prefix(agent, job.context_before);
        let save = job.message_id;
        let body = match result {
            Ok(cleared) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(cleared));
                }
                match job.confirm().await {
                    Ok((new, resume)) => format!(
                        "{prefix} complete: saved ({save}); cleared and bound {} -> {new}; resume prompt {resume} delivered.", job.session
                    ),
                    Err(reason) => format!("{prefix} did not complete: saved ({save}); clear confirmed; resume not confirmed: {reason}."),
                }
            }
            Err(mut error) if job.saved_confirmed => {
                let suffix = if job.retry_expired && matches!(origin, NoteOrigin::Automatic { .. })
                {
                    " Nothing more is tried automatically in this session. Inspect atm agent list."
                } else {
                    ""
                };
                let body = format!(
                    "{prefix} did not complete: saved ({save}); {}.{suffix}",
                    error.0.message
                );
                if let Some(reply) = reply {
                    error.0.message =
                        format!("{agent} saved its state ({save}); {}", error.0.message);
                    let _ = reply.send(Err(error));
                }
                body
            }
            Err(error) => {
                self.unsaved_failure(agent, manager, &origin, reply, error)
                    .await;
                return;
            }
        };
        // The next handover may start once the proof/terminal outcome is
        // decided. Publishing the notice does not need the claim.
        drop(job);
        self.publish_outcome(agent, manager, body).await;
    }

    async fn unsaved_failure(
        &self,
        agent: &AgentId,
        manager: Address,
        origin: &NoteOrigin,
        reply: Option<HandoverReply>,
        error: MethodError,
    ) {
        let body = match origin {
            NoteOrigin::Command => format!("handover of {agent} did not complete: {error}"),
            NoteOrigin::Automatic { why, .. } => format!(
                "automatic handover of {agent} ({why}) did not complete: {error}. Nothing more is tried \
                 in this session. By hand: atm agent handover {agent}"),
        };
        if reply.is_none_or(|reply| reply.send(Err(error)).is_err()) {
            self.publish_outcome(agent, manager, body).await;
        }
    }

    async fn publish_outcome(&self, agent: &AgentId, fallback: Address, body: String) {
        let to = self.outcome_manager(agent, fallback).await;
        let note = MessageContent {
            delivery_notice: None,
            from: Address::System,
            to,
            kind: MessageKind::Note,
            body,
            attachments: Vec::new(),
            task: None,
        };
        if let Err(error) = self.execute(OrgCommand::SendMessage(note)).await {
            tracing::warn!(%agent, %error, "Handover outcome note was not recorded");
        }
    }

    async fn outcome_manager(&self, id: &AgentId, fallback: Address) -> Address {
        let Ok(state) = self.org.snapshot().await else {
            return fallback;
        };
        outcome_manager(&state, id, fallback, self.files.clone()).await
    }

    /// Preflight only: refusal here queues no request. The returned work
    /// must be polled by a retained task; dispatch may leave a request even
    /// when either write's result is unconfirmed. Nothing is then cleared.
    pub(super) async fn admit(
        &self,
        p: HandoverParams,
        caller: CallContext,
        deadline: Deadline,
        expected: Option<&SessionId>,
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
        if expected.is_some_and(|expected| *expected != session) {
            return Err(refused(format!(
                "agent {} changed sessions before automatic handover",
                p.agent
            )));
        }
        let (_, memory) = paths(&state, agent)?;
        let own = caller
            .from_pane
            .as_ref()
            .is_some_and(|pane| agent.pane.as_deref() == Some(pane.0.as_str()));
        let manager = manager_address(&agent.id, agent.role.as_ref()).unwrap_or(Address::Owner);
        let running = Running::claim(&self.running, &p.agent)?;
        let before = self.stat(&memory).await?;
        let request = save_request(&p.agent, &memory);
        let open = open_save_request(&state, &request);
        let pane = agent.pane.clone();
        let context_before = if expected.is_none() {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.registry.get_session(session.clone()),
            )
            .await
            .ok()
            .flatten()
            .map(|row| row.context_percentage)
            .filter(|share| share.is_finite() && *share > 0.)
        } else {
            None
        };
        let service = self.clone();
        // Unpolled until a retained owner exists: caller loss cannot land
        // between a queued request and ownership of the remaining work.
        let job = Box::pin(async move {
            let message_id = match open {
                Some(open) => open,
                None => service
                    .execute(OrgCommand::SendMessage(request))
                    .await
                    .and_then(|sent| {
                        sent.first()
                            .map(|event| MessageId::new(event.seq))
                            .ok_or_else(|| {
                                AgentError::Internal("the save request committed no event".into())
                                    .into()
                            })
                    })
                    .map_err(|mut error| {
                        error.0.message = format!(
                            "could not confirm the save request for {}: {}; a request may be \
                             queued or already delivered; nothing was cleared; inspect \
                             `atm inbox --for {}` before retrying",
                            p.agent, error.0.message, p.agent
                        );
                        error
                    })?,
            };
            service
                .execute(OrgCommand::SaveRequested {
                    agent_id: p.agent.clone(),
                    message_id,
                })
                .await
                .map_err(|mut error| {
                    error.0.message = format!(
                        "the save request {message_id} for {} exists, but its handover record was \
                     not confirmed: {}; the request may be queued or already delivered; \
                     nothing was cleared; inspect `atm inbox --for {} --id {message_id}`; \
                     a later handover reuses it while queued or delivering; \
                     `atm msg give-up {message_id}` cancels an undelivered request",
                        p.agent, error.0.message, p.agent
                    );
                    error
                })?;
            let mut facts = SaveFacts::default();
            if let Some(row) = service.registry.get_session(session.clone()).await {
                facts.status(row.status);
            }
            Ok(Job {
                service,
                agent: p.agent,
                pane,
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
                context_before,
                saved_confirmed: false,
                retry_expired: false,
                completion: None,
                _running: running,
            })
        });
        Ok(Admitted { job, own, manager })
    }
}

type HandoverReply = oneshot::Sender<Result<HandoverResult, MethodError>>;

pub(super) enum NoteOrigin {
    Command,
    Automatic {
        why: String,
        idle_share: Option<f64>,
    },
}

impl NoteOrigin {
    fn prefix(&self, agent: &AgentId, context_before: Option<f64>) -> String {
        match self {
            Self::Command => match context_before {
                Some(share) => format!("handover of {agent} (context {share:.0}%)"),
                None => format!("handover of {agent}"),
            },
            Self::Automatic { why, idle_share } => match idle_share {
                Some(share) => {
                    format!("automatic handover of {agent} ({why}; context {share:.0}%)")
                }
                None => format!("automatic handover of {agent} ({why})"),
            },
        }
    }
}

/// A handover that passed preflight; its retained owner queues and audits.
pub(super) struct Admitted {
    pub job: Pin<Box<dyn Future<Output = Result<Job, MethodError>> + Send>>,
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
    context_before: Option<f64>,
    saved_confirmed: bool,
    retry_expired: bool,
    completion: Option<ClearOutcome>,
    _running: Running,
}

impl Job {
    pub(super) async fn run(&mut self) -> Result<HandoverResult, MethodError> {
        self.saved().await?;
        let saved = self
            .service
            .execute(OrgCommand::Saved {
                agent_id: self.agent.clone(),
                message_id: self.message_id,
            })
            .await?;
        let saved_seq = saved
            .first()
            .ok_or_else(|| AgentError::Internal("the saved audit committed no event".into()))?
            .seq;
        self.saved_confirmed = true;
        let cleared = self.clear_saved(saved_seq).await?;
        let session_id = cleared.result.session_id.clone();
        self.completion = Some(cleared);
        Ok(HandoverResult {
            agent_id: self.agent.clone(),
            message_id: self.message_id,
            state: HandoverState::Cleared,
            session_id,
        })
    }

    async fn confirm(&mut self) -> Result<(SessionId, MessageId), String> {
        let Some(clear) = &self.completion else {
            return Err("the start line could not be identified".into());
        };
        use StartLineDisposition::{Off, Track, Unidentified};
        let resume = match clear.start_line {
            Track(id) => id,
            Off => return Err("start lines are disabled".into()),
            Unidentified => return Err("the start line could not be identified".into()),
        };
        let ready = clear
            .ready_seq
            .ok_or_else(|| evidence_unavailable("no clear Ready record"))?;
        // The Ready write returned this cursor under the reservation. Capture
        // it there; a later snapshot cannot tell this clear from another one.
        let mut events = self
            .service
            .org
            .watch(vec!["agent.".into(), "msg.".into()], ready)
            .map_err(evidence_unavailable)?;
        let mut facts = ResumeFacts {
            bound: clear.result.session_id.clone(),
            receipt: None,
        };
        let deadline = tokio::time::sleep(RESUME_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                () = &mut deadline => return Err(format!("no matching new binding and receipt for start line {resume} within 30 s")),
                event = events.next() => {
                    let event = event.map_err(evidence_unavailable)?.ok_or_else(|| evidence_unavailable("watch stopped"))?;
                    if let Some(session) = facts.apply(&event, &self.agent, &self.session, resume)? {
                        return Ok((session, resume));
                    }
                }
            }
        }
    }

    async fn clear_saved(&mut self, saved_seq: u64) -> Result<ClearOutcome, MethodError> {
        let target =
            SavedSeat {
                agent: self.agent.clone(),
                pane: PaneId::parse(self.pane.clone().ok_or_else(|| {
                    AgentError::Refused(format!("agent {} has no pane", self.agent))
                })?)
                .map_err(AgentError::from)?,
                session: self.session.clone(),
            };
        let clearer = self.service.clearer.clone();
        let _reset = ClearWait {
            clearer: clearer.as_ref(),
            target: &target,
        };
        let finish_by = Instant::now() + CLEAR_TIMEOUT;
        let mut cutoff = None;
        let mut last = "the editor is not empty and idle";
        let mut delay = FIRST_BACKOFF;
        loop {
            match clearer.clear(target.clone(), cutoff, finish_by).await? {
                ClearAttempt::Cleared(result) => return Ok(result),
                ClearAttempt::Expired => break,
                ClearAttempt::Blocked(reason) => last = reason,
            }
            let now = Instant::now();
            let at = *cutoff.get_or_insert_with(|| {
                tracing::debug!(agent=%target.agent, pane=%target.pane, session=%target.session,
                    save=%self.message_id, refusal=last, "Handover clear waiting");
                (now + CLEAR_RETRY).min(finish_by)
            });
            clearer.clear_wait(
                &target,
                Some(format!(
                    "handover saved ({}); waiting for a safe editor to clear: {last}",
                    self.message_id
                )),
            );
            if !self
                .next_clear_check(&target, saved_seq, at, now + delay)
                .await
                .map_err(|error| match error {
                    AgentError::Unavailable(reason) => MethodError::new(error_code::UNAVAILABLE, format!(
                        "could not wait for another screen observation ({reason}); last clear refusal: {}",
                        target.refusal(last),
                    )),
                    error => error.into(),
                })?
            {
                break;
            }
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        self.retry_expired = true;
        Err(retry_expired(&target, last).into())
    }

    /// One session wake or a bounded backoff sample, never concurrent with a
    /// preflight. Expiry wins even when both channels are continuously ready.
    async fn next_clear_check(
        &mut self,
        target: &SavedSeat,
        saved_seq: u64,
        cutoff: Instant,
        retry_at: Instant,
    ) -> Result<bool, AgentError> {
        loop {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(cutoff) => return Ok(false),
                event = self.committed.recv() => match event {
                    Ok(event) if event.seq > saved_seq && lifecycle_of(&event, &target.agent) => return Err(target.changed()),
                    Ok(_) => {},
                    Err(error) => return Err(AgentError::Unavailable(format!("handover lifecycle evidence: {error}"))),
                },
                () = tokio::time::sleep_until(retry_at) => break,
                row = self.rows.recv() => {
                    let row = row.map_err(|error| AgentError::Unavailable(format!("session registry evidence: {error}")))?;
                    if target.row_wake(&row)? {
                        break;
                    }
                },
            }
        }
        // Coalesce only after inspecting the backlog, including when the
        // timer won. A move or removal still matters if the row came back.
        for _ in 0..=self.rows.len() {
            match self.rows.try_recv() {
                Ok(row) => {
                    target.row_wake(&row)?;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(error) => {
                    return Err(AgentError::Unavailable(format!(
                        "session registry evidence: {error}"
                    )))
                }
            }
        }
        Ok(Instant::now() < cutoff)
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

fn evidence_unavailable(error: impl std::fmt::Display) -> String {
    format!("committed evidence unavailable ({error})")
}

// The watch is in commit order. Only its first new binding belongs to an
// unbound clear; a later lifecycle change cannot supply the missing proof.
#[derive(Debug, Default)]
pub(super) struct ResumeFacts {
    pub bound: Option<SessionId>,
    pub receipt: Option<SessionId>,
}

impl ResumeFacts {
    pub(super) fn apply(
        &mut self,
        event: &Envelope,
        agent: &AgentId,
        old: &SessionId,
        resume: MessageId,
    ) -> Result<Option<SessionId>, String> {
        let body = |key: &str| event.body.get(key).and_then(serde_json::Value::as_str);
        let session = || {
            body("session_id")
                .map(SessionId::new)
                .filter(|id| !id.is_pending() && id != old)
        };
        if lifecycle_of(event, agent) {
            if event.kind == "agent.session_bound" && self.bound.is_none() {
                self.bound = Some(session().ok_or("the agent left the cleared lifecycle")?);
            } else {
                return Err("the agent left the cleared lifecycle".into());
            }
        }
        let mismatched =
            || format!("start line {resume} was not confirmed in the replacement session");
        if body("message_id") == Some(resume.to_string().as_str()) {
            if event.kind == "msg.delivered" {
                self.receipt = Some(
                    session()
                        .filter(|_| body("confirmation") == Some("prompt_submit_exact_reference"))
                        .ok_or_else(mismatched)?,
                );
            } else if event.kind == "msg.delivery_failed"
                && event
                    .body
                    .get("final_failure")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
                return Err(format!(
                    "start line {resume} failed: {}",
                    body("reason").unwrap_or("no reason recorded")
                ));
            }
        }
        match (&self.bound, &self.receipt) {
            (Some(bound), Some(receipt)) if bound == receipt => Ok(Some(bound.clone())),
            (Some(_), Some(_)) => Err(mismatched()),
            _ => Ok(None),
        }
    }
}

fn retry_expired(target: &SavedSeat, last: &str) -> AgentError {
    AgentError::Refused(format!(
        "clear retry window expired; last clear refusal: {}",
        target.refusal(last)
    ))
}

/// Committed lifecycle changes; the save/saved notices are not transitions.
fn lifecycle_of(event: &Envelope, id: &AgentId) -> bool {
    matches!(
        event.kind.as_str(),
        "agent.up_requested"
            | "agent.adopted"
            | "agent.pane_created"
            | "agent.session_bound"
            | "agent.session_lost"
            | "agent.editor_ready"
            | "agent.revalidation_requested"
            | "agent.clear_requested"
            | "agent.down"
            | "agent.stuck"
    ) && event
        .body
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        == Some(id.to_string().as_str())
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
        delivery_notice: None,
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
