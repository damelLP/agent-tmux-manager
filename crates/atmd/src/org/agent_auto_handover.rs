//! The handover watcher: at a turn end it sends the note at the context
//! line, or, for a seat whose role file switches it on, runs the handover
//! itself.
//!
//! One task decides both, so a seat never gets a note and a handover for the
//! same turn end. What is due is `atm_org::auto_handover`'s answer; this file
//! is its side effects. It adds no way to type into a pane and none to clear
//! one: an automatic handover is the handover on command, started by the
//! daemon, with the same admission, save request, guards and refusals.
//!
//! # What it looks at, and when
//!
//! It reads registry events only. For every session it keeps the last
//! status, share and background mark, the end of the last turn it saw
//! (a status that was not idle, then idle) and how many turns it saw end. A
//! session is looked at when a turn ends in it, when its share or its
//! background mark changes while it is idle after a turn end (a harness may
//! report the share after the turn's end), and at the time the policy asked
//! to look again. Events are taken one at a time, in order. A handover is
//! started only on the latest row: the registry is asked for the session
//! once more, and if its row is not the idle row that was judged (a turn has
//! started, background work has shown up, the share has moved) nothing is
//! started, and the event that is on its way brings the next look.
//!
//! # The switch
//!
//! The role file `.atm/roles/<name>.md` is read at every look, not taken from
//! the role recorded at `agent up`. So an edit takes effect at the seat's
//! next turn end, and removing `auto_handover` stops the daemon without a
//! respawn. No role file, an invalid one or one that cannot be read in time
//! is off: the seat gets the note at the default line.
//!
//! # An automatic handover
//!
//! - At most one is started per session. Whatever becomes of it, the daemon
//!   tries no more in that session.
//! - It is refused at admission like any handover (the agent is not ready, a
//!   handover of it already runs): nothing was sent, nobody is told, and the
//!   session's one attempt is not used up.
//! - Once admitted it ends in exactly one note from `system` to the agent's
//!   manager (the owner for an agent whose role names none): done, or why
//!   not. That note replaces the note at the context line.
//! - The save may take ten minutes at the context line, and until the cache
//!   expires for an idle seat.
//!
//! # Limits
//!
//! - Everything here is in memory. After a restart of `atmd` no turn end is
//!   on record, so nothing is done until a turn ends; a handover the restart
//!   cut off is lost with its note, and its session can be asked to save once
//!   more.
//! - A save request that was not delivered when its handover gave up stays
//!   queued, as for the handover on command: the daemon withdraws nothing.
//!   When what held it is gone it is typed, the agent saves, and nothing
//!   clears the session; for an idle seat that turn is paid for on a cold
//!   cache, which is what the idle rule is there to avoid. The owner's
//!   give-up cancels the request. This needs `cache_minutes` in a role file
//!   to happen to an idle seat.
//! - The organisation's state is copied once per look, and the role file is
//!   read on the watcher's own task with a five-second budget. A filesystem
//!   that hangs delays every look by that long; registry events missed
//!   meanwhile make the watcher forget what it saw.
//! - Whether the owner is away is not known yet: he is taken to be present.

use super::{
    agent_context::{detached_read, paths, read_files},
    agent_handover::{context_note, Admitted, Deadline, HandoverParams, HandoverResult, Handovers},
    start_line::is_start_line,
};
use crate::{
    registry::SessionEvent,
    server::methods::{CallContext, CallerId, MethodError},
};
use atm_core::{SessionId, SessionStatus, SessionView};
use atm_org::{
    auto_handover::{auto_step, handover_settings, AutoInput, AutoStep, HandoverSettings},
    idle::HandoverReason,
    Address, Agent, AgentId, AgentStatus, MessageContent, MessageKind, OrgCommand, OrgState,
};
use chrono::{DateTime, TimeDelta, Utc};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

/// How long the save of a handover at the context line may take: the
/// default of the handover on command.
const SAVE_TIMEOUT: Duration = Duration::from_secs(600);

/// The wall clock and its timers; tests put a clock they move by hand here.
pub(super) trait Timer: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
    /// Resolves at `at`; at once when that time has passed.
    fn at(&self, at: DateTime<Utc>) -> Deadline;
}

struct WallClock;

impl Timer for WallClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn at(&self, at: DateTime<Utc>) -> Deadline {
        let wait = at.signed_duration_since(Utc::now()).to_std();
        Box::pin(tokio::time::sleep(wait.unwrap_or_default()))
    }
}

/// How many start lines were delivered to `session` of `agent`.
pub(super) fn start_lines(state: &OrgState, agent: &AgentId, session: &SessionId) -> u32 {
    let delivered = state
        .inbox(&Address::Agent(agent.clone()))
        .into_iter()
        .filter(|message| {
            // Only a delivered message has a receipt.
            is_start_line(&message.content)
                && message.receipt.as_ref().map(|receipt| &receipt.session_id) == Some(session)
        })
        .count();
    u32::try_from(delivered).unwrap_or(u32::MAX)
}

/// Why a handover is run, in the words of its note.
pub(super) fn why(
    reason: HandoverReason,
    share: f64,
    settings: &HandoverSettings,
    idle: Duration,
) -> String {
    let line = settings.limits.context_line_percent;
    match (reason, settings.limits.cache) {
        (HandoverReason::ContextLine, _) | (_, None) => {
            format!("context {share:.0}%, line {line}%")
        }
        (_, Some(cache)) => format!(
            "idle {} min; nothing it waits for comes before its cache expires at {} min",
            idle.as_secs() / 60,
            cache.lifetime.as_secs() / 60
        ),
    }
}

/// The one note that ends an automatic handover of `agent`.
pub(super) fn outcome_note(
    agent: &AgentId,
    why: &str,
    memory: &Path,
    manager: Address,
    result: &Result<HandoverResult, MethodError>,
) -> MessageContent {
    let body = match result {
        Ok(done) => format!(
            "automatic handover of {agent} done ({why}): it saved to {} ({}) and has a new session.",
            memory.display(),
            done.message_id
        ),
        Err(error) => format!(
            "automatic handover of {agent} ({why}) did not complete: {}. Nothing more is tried \
             in this session. By hand: atm agent handover {agent}",
            error.0.message
        ),
    };
    MessageContent {
        from: Address::System,
        to: manager,
        kind: MessageKind::Note,
        body,
        attachments: Vec::new(),
        task: None,
    }
}

/// What follows an automatic handover's end: its one note, to the agent's
/// manager. A handover refused at admission sent nothing, so it gets no note
/// and its session gets its one attempt back.
pub(super) fn settle(
    attempted: &mut BTreeMap<AgentId, SessionId>,
    attempt: Attempt,
    outcome: Outcome,
) -> Option<MessageContent> {
    match outcome {
        Ok((manager, result)) => Some(outcome_note(
            &attempt.agent,
            &attempt.why,
            &attempt.memory,
            manager,
            &result,
        )),
        Err(error) => {
            tracing::debug!(agent = %attempt.agent, %error, "Automatic handover not admitted");
            if attempted.get(&attempt.agent) == Some(&attempt.session) {
                attempted.remove(&attempt.agent);
            }
            None
        }
    }
}

/// What the watcher has seen of one session.
#[derive(Debug, Clone, Copy)]
struct Row {
    status: SessionStatus,
    share: f64,
    /// The idle row shows background or scheduled work: the registry sets an
    /// activity on an idle row for nothing else.
    background: bool,
    /// The end of the last turn seen, while the session has been idle since.
    idle_since: Option<DateTime<Utc>>,
    turn_ends: u32,
    /// When the policy asked to look again.
    wake: Option<DateTime<Utc>>,
    /// To be looked at.
    due: bool,
}

/// `view` is still the idle row that was judged at `share` percent.
pub(super) fn still_shows(view: &SessionView, share: f64, background: bool) -> bool {
    view.status == SessionStatus::Idle
        && view.context_percentage == share
        && view.activity_detail.is_some() == background
}

/// One automatic handover: whose, in which session, and why.
pub(super) struct Attempt {
    pub agent: AgentId,
    pub session: SessionId,
    pub why: String,
    pub memory: PathBuf,
}

/// How an automatic handover ended. `Err`: refused at admission, so nothing
/// was sent. `Ok`: the agent's manager, and the handover's own answer.
pub(super) type Outcome = Result<(Address, Result<HandoverResult, MethodError>), MethodError>;

struct Watch {
    handovers: Handovers,
    timer: Arc<dyn Timer>,
    files: Arc<Semaphore>,
    rows: broadcast::Receiver<SessionEvent>,
    seen: HashMap<SessionId, Row>,
    /// The session each agent was last noted in.
    noted: BTreeMap<AgentId, SessionId>,
    /// The session each agent's last automatic handover was started in.
    attempted: BTreeMap<AgentId, SessionId>,
    ended: mpsc::Sender<(Attempt, Outcome)>,
}

/// Starts the watcher for the agents `handovers` serves. Runs until `cancel`
/// or until the registry stops.
pub fn spawn_handover_watch(handovers: Handovers, cancel: CancellationToken) {
    spawn_watch(handovers, Arc::new(WallClock), cancel);
}

pub(super) fn spawn_watch(handovers: Handovers, timer: Arc<dyn Timer>, cancel: CancellationToken) {
    let (ended, mut endings) = mpsc::channel(64);
    let mut watch = Watch {
        rows: handovers.registry.subscribe(),
        handovers,
        timer,
        files: Arc::new(Semaphore::new(1)),
        seen: HashMap::new(),
        noted: BTreeMap::new(),
        attempted: BTreeMap::new(),
        ended,
    };
    tokio::spawn(async move {
        loop {
            let due = watch.seen.iter().find(|(_, row)| row.due);
            if let Some(session) = due.map(|(session, _)| session.clone()) {
                watch.look(&session).await;
                continue;
            }
            let wake = watch.seen.values().filter_map(|row| row.wake).min();
            let woken = match wake {
                Some(at) => watch.timer.at(at),
                None => Box::pin(std::future::pending()),
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                row = watch.rows.recv() => match row {
                    Ok(event) => watch.row(event),
                    // Missed events: forget what was seen, so that no turn
                    // end is made up from a stale status.
                    Err(broadcast::error::RecvError::Lagged(_)) => watch.seen.clear(),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                () = woken => watch.wake(),
                Some((attempt, outcome)) = endings.recv() => watch.ended(attempt, outcome).await,
            }
        }
    });
}

impl Watch {
    fn row(&mut self, event: SessionEvent) {
        let session = match event {
            SessionEvent::Updated { session } => session,
            SessionEvent::Removed { session_id, .. } => {
                self.seen.remove(&session_id);
                return;
            }
            SessionEvent::Registered { .. } => return,
        };
        let before = self.seen.get(&session.id).copied();
        let idle = session.status == SessionStatus::Idle;
        let ended = idle && before.is_some_and(|before| before.status != SessionStatus::Idle);
        let mut row = Row {
            status: session.status,
            share: session.context_percentage,
            background: session.activity_detail.is_some(),
            idle_since: None,
            turn_ends: before.map_or(0, |before| before.turn_ends),
            wake: None,
            due: false,
        };
        if ended {
            row.idle_since = Some(self.timer.now());
            row.turn_ends = row.turn_ends.saturating_add(1);
            row.due = true;
        } else if let Some(before) = before.filter(|before| idle && before.idle_since.is_some()) {
            let changed = before.share != row.share || before.background != row.background;
            row.idle_since = before.idle_since;
            row.wake = before.wake;
            row.due = before.due || changed;
        }
        self.seen.insert(session.id.clone(), row);
    }

    /// The time to look again has come for every session whose wake is due.
    fn wake(&mut self) {
        let now = self.timer.now();
        for row in self.seen.values_mut() {
            if row.wake.is_some_and(|wake| wake <= now) {
                row.wake = None;
                row.due = true;
            }
        }
    }

    /// What the seat's role file says now. Anything but a valid role file
    /// that was read in time is off.
    async fn settings(&self, state: &OrgState, agent: &Agent) -> HandoverSettings {
        let read = async {
            let (role_file, memory) = paths(state, agent)?;
            let (reading, name) = (role_file.clone(), agent.id.name.clone());
            let (role, _) = detached_read(self.files.clone(), &reading, move || {
                read_files(&role_file, &memory, &name)
            })
            .await?;
            Ok::<_, MethodError>(role)
        };
        let role = match read.await {
            Ok(role) => role,
            Err(error) => {
                tracing::warn!(agent = %agent.id, %error, "Role file not read: no automatic handover");
                None
            }
        };
        // A role that parsed has valid handover keys.
        role.and_then(|role| handover_settings(&role.config).ok())
            .unwrap_or_default()
    }

    /// One look at an idle session after a turn end: the note at the line,
    /// a handover, a time to look again, or nothing.
    async fn look(&mut self, session: &SessionId) {
        let Some(row) = self.seen.get_mut(session) else {
            return;
        };
        row.due = false;
        let seen = *row;
        let org = self.handovers.org.clone();
        let state = match org.snapshot().await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(%error, "Handover watch skipped a turn end: organisation state unavailable");
                return;
            }
        };
        let Some(agent) = state.agents().find(|agent| {
            agent.status == AgentStatus::Ready && agent.session_id.as_ref() == Some(session)
        }) else {
            return;
        };
        let settings = self.settings(&state, agent).await;
        let now = self.timer.now();
        let input = AutoInput {
            state: &state,
            agent: &agent.id,
            session,
            settings: &settings,
            context_percent: Some(seen.share),
            idle_since: seen.idle_since,
            background: seen.background,
            worked: seen.turn_ends > start_lines(&state, &agent.id, session),
            attempted: self.attempted.get(&agent.id) == Some(session),
            // Nothing tells the daemon yet that the owner is away.
            away: false,
        };
        let step = auto_step(&input, now);
        match step {
            AutoStep::Nothing => {}
            AutoStep::LookAgainAt(at) => {
                if let Some(row) = self.seen.get_mut(session) {
                    row.wake = Some(at);
                }
            }
            AutoStep::Flag { look_again_at } => {
                if let Some(row) = self.seen.get_mut(session) {
                    row.wake = look_again_at;
                }
                let note = context_note(&state, session, seen.share, &settings.limits, now);
                let Some((agent, note)) = note else {
                    return;
                };
                if self.noted.get(&agent) == Some(session) {
                    return;
                }
                match org.execute(OrgCommand::SendMessage(note), now).await {
                    Ok(_) => {
                        self.noted.insert(agent, session.clone());
                    }
                    Err(error) => {
                        tracing::warn!(%agent, %error, "Context note was not recorded");
                    }
                }
            }
            AutoStep::Run { reason, give_up_at } => {
                // Only on the latest row: an event that is still on its way
                // (a new turn, background work) must not be overtaken.
                let latest = self.handovers.registry.get_session(session.clone()).await;
                if !latest.is_some_and(|row| still_shows(&row, seen.share, seen.background)) {
                    return;
                }
                let memory = match paths(&state, agent) {
                    Ok((_, memory)) => memory,
                    Err(error) => {
                        tracing::warn!(agent = %agent.id, %error, "No automatic handover: no memory file path");
                        return;
                    }
                };
                let give_up_at = give_up_at
                    .or_else(|| now.checked_add_signed(TimeDelta::from_std(SAVE_TIMEOUT).ok()?))
                    .unwrap_or(now);
                let left = give_up_at.signed_duration_since(now).to_std();
                let idle = seen
                    .idle_since
                    .map(|since| now.signed_duration_since(since));
                let idle = idle.and_then(|idle| idle.to_std().ok()).unwrap_or_default();
                self.attempted.insert(agent.id.clone(), session.clone());
                let attempt = Attempt {
                    agent: agent.id.clone(),
                    session: session.clone(),
                    why: why(reason, seen.share, &settings, idle),
                    memory,
                };
                self.run(attempt, left.unwrap_or_default(), self.timer.at(give_up_at));
            }
        }
    }

    /// Runs one handover as a task of its own; how it ended comes back on
    /// the watcher's channel.
    fn run(&self, attempt: Attempt, save: Duration, deadline: Deadline) {
        let (handovers, ended) = (self.handovers.clone(), self.ended.clone());
        tokio::spawn(async move {
            let p = HandoverParams {
                agent: attempt.agent.clone(),
                timeout_ms: u64::try_from(save.as_millis()).unwrap_or(u64::MAX),
            };
            let caller = CallContext {
                client_id: CallerId("atmd".into()),
                from_pane: None,
            };
            let outcome = match handovers.admit(p, caller, deadline).await {
                Err(refused) => Err(refused),
                Ok(Admitted { job, manager, .. }) => Ok((manager, job.run().await)),
            };
            // A closed channel is the watcher's end: nobody is left to tell.
            let _ = ended.send((attempt, outcome)).await;
        });
    }

    /// An automatic handover has ended.
    async fn ended(&mut self, attempt: Attempt, outcome: Outcome) {
        let agent = attempt.agent.clone();
        let Some(note) = settle(&mut self.attempted, attempt, outcome) else {
            return;
        };
        let org = &self.handovers.org;
        let sent = org.execute(OrgCommand::SendMessage(note), self.timer.now());
        if let Err(error) = sent.await {
            tracing::warn!(%agent, %error, "Automatic handover's note was not recorded");
        }
    }
}
