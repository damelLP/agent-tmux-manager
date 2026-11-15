//! The reminder on a silent turn end: one note to an agent whose turn ended
//! with its task still saying "acting", and one to the task's creator when
//! the next turn ends the same way.
//!
//! The rule is `atm_org::reminder`'s and the audit events and their
//! once-per-state latch are the task module's. This module watches the
//! registry for turn ends, waits, asks the rule, and writes what it answers.
//! It adds no way to type into a pane: both notes are messages, delivered by
//! the message runtime with all its guards.
//!
//! # What counts as a turn end
//!
//! A session's row going from not idle to idle, the only turn-end fact the
//! daemon has. On its own it misleads, so three things stand between it and a
//! typed line:
//!
//! - **The wait is continuous idle.** Any update that is not idle (a tool
//!   call, a prompt, a dialog, compacting) drops the pending wait, and the
//!   next idle edge starts a new one.
//! - **Nothing is decided while a message to the agent is on its way** (the
//!   rule's own row).
//! - **The delivery guards.** A reminder decided while the row wrongly reads
//!   idle is held by the message runtime while the screen shows a running
//!   turn, and typed after it.
//!
//! An idle row that shows an activity has no pending wait either. The
//! registry sets that text on an idle row only for background work the
//! harness reported with the turn's end ("1 bg task", "2 scheduled"); the
//! turn that this work starts ends with a plain idle row and is judged then.
//!
//! The turn end is stamped when it is read, not when the harness sent it.
//! Missed registry events forget every status and wait, so no turn end is
//! made up from a stale status.
//!
//! # What a wake does
//!
//! When a wait is over the agent must still be ready and bound to that
//! session; otherwise the wait is dropped. Then, as the rule answers:
//!
//! 1. each task's audit event (`task.reminded`, `task.flagged`) is written
//!    first. It is the latch: a failure after it loses that note and never
//!    sends a second one. The flag still follows the next silent turn end, so
//!    a person hears of a lost reminder;
//! 2. one note goes to the agent for the tasks whose event was written, and
//!    one to each addressee of the flags.
//!
//! A flag is recorded for the task's creator. Its note goes to the creator,
//! or, when the creator is `system` or the agent itself, to the agent's
//! manager from its role; without a manager that exists, to the owner.
//!
//! # Limits
//!
//! - A wait lives in memory. A daemon restart drops it, and a row first seen
//!   idle after the start has no turn before it, so a stall that began before
//!   the restart is not answered until a later turn ends. The latches are in
//!   the log: a restart never sends a second reminder.
//! - A row that never goes idle (a harness that sends no event for an
//!   interrupted turn or a denied dialog) is not reminded.
//! - A row that reads idle while the harness still answers, for longer than
//!   the delay, gets a reminder it may no longer need, typed after the real
//!   turn end. The text says to ignore it then.
//! - Background work that never reports back keeps its row's activity and so
//!   keeps the reminder off.
//! - One wake answers at most [`MAX_TASKS`] reminders and as many flags; the
//!   rest are due again at the next turn end.
//! - A failure of the organisation service is logged and the wait dropped.
//! - The sessions due at one wake are judged one after the other, and no
//!   registry row is read in between. A turn that starts in that time is
//!   still judged silent, and so is one whose message was confirmed just
//!   before the wake and whose first row is not read yet. Either costs one
//!   reminder too early, typed after that turn, never a second one. Reasoned
//!   from the code; not seen.
//! - A delay of zero makes "look again" mean "now": while a message to the
//!   agent is on its way the scheduler then spins on the organisation
//!   service. The config refuses zero. A caller that builds the settings
//!   itself must not pass it.

use super::{agent_context::manager_address, OrgHandle};
use crate::registry::SessionEvent;
use atm_core::{SessionId, SessionStatus};
use atm_org::{
    reminder::{reminder_step, ReminderSettings, ReminderStep},
    Address, Agent, AgentStatus, MessageContent, MessageKind, OrgCommand, OrgState, Task,
    TaskCommand, TaskId,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use std::collections::HashMap;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "task_reminder_tests.rs"]
mod tests;

/// The most tasks one note names.
const MAX_TASKS: usize = 20;
/// The most characters of a task's title a note shows.
const TITLE_CHARS: usize = 80;

/// `[reminder]` in atm's `config.toml`. An unknown key is an error, so a
/// misspelt one cannot leave reminders on by accident.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReminderSection {
    /// False turns the reminder off.
    enabled: bool,
    /// Seconds an agent stays idle after a silent turn end before the note.
    delay_secs: u64,
}

impl Default for ReminderSection {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_secs: ReminderSettings::default().delay.as_secs(),
        }
    }
}

/// The reminder's settings in the text of atm's `config.toml`: on with the
/// default delay when there is no `[reminder]` section, `None` when the
/// section turns it off. Malformed TOML, an unknown key and a delay of zero
/// are errors, for the caller to leave reminders off. The error never quotes
/// the file, which may hold secrets.
pub fn parse_reminder_settings(text: &str) -> Result<Option<ReminderSettings>, String> {
    #[derive(Default, serde::Deserialize)]
    struct ConfigFile {
        #[serde(default)]
        reminder: ReminderSection,
    }
    let section = toml::from_str::<ConfigFile>(text)
        .ok()
        .map(|file| file.reminder)
        .filter(|section| !section.enabled || section.delay_secs > 0)
        .ok_or("invalid config: malformed TOML or invalid [reminder] settings")?;
    Ok(section.enabled.then(|| ReminderSettings {
        delay: std::time::Duration::from_secs(section.delay_secs),
    }))
}

/// A turn end that waits to be judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    ended_at: DateTime<Utc>,
    /// When to ask the rule.
    wake: DateTime<Utc>,
}

/// What the scheduler remembers of the registry. No I/O.
#[derive(Debug, Default)]
struct Watch {
    seen: HashMap<SessionId, SessionStatus>,
    pending: HashMap<SessionId, Pending>,
}

impl Watch {
    /// Book one registry event, read at `now`.
    fn observe(&mut self, row: &SessionEvent, settings: &ReminderSettings, now: DateTime<Utc>) {
        let session = match row {
            SessionEvent::Updated { session } => session,
            SessionEvent::Removed { session_id, .. } => {
                self.seen.remove(session_id);
                self.pending.remove(session_id);
                return;
            }
            SessionEvent::Registered { .. } => return,
        };
        let before = self.seen.insert(session.id.clone(), session.status);
        if session.status != SessionStatus::Idle || session.activity_detail.is_some() {
            self.pending.remove(&session.id);
            return;
        }
        if before.is_none_or(|before| before == SessionStatus::Idle) {
            return;
        }
        let wake = TimeDelta::from_std(settings.delay)
            .ok()
            .and_then(|delay| now.checked_add_signed(delay));
        if let Some(wake) = wake {
            let ended_at = now;
            self.pending
                .insert(session.id.clone(), Pending { ended_at, wake });
        }
    }

    /// Missed events: no status and no wait can be trusted.
    fn forget(&mut self) {
        self.seen.clear();
        self.pending.clear();
    }

    fn next_wake(&self) -> Option<DateTime<Utc>> {
        self.pending.values().map(|pending| pending.wake).min()
    }
}

/// A task's title on one line, cut to [`TITLE_CHARS`].
fn title(task: &Task) -> String {
    let mut shown: String = task
        .title
        .chars()
        .take(TITLE_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if task.title.chars().nth(TITLE_CHARS).is_some() {
        shown.push('…');
    }
    shown
}

/// The state's name as the task methods print it.
fn state_name(task: &Task) -> String {
    serde_json::to_value(task.state)
        .ok()
        .and_then(|name| name.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Who hears that `agent` left `task` silent twice. See the module docs.
fn told(state: &OrgState, agent: &Agent, task: &Task) -> Address {
    match &task.creator {
        Address::Owner => return Address::Owner,
        Address::Agent(creator) if creator != &agent.id => return task.creator.clone(),
        Address::Agent(_) | Address::System => {}
    }
    manager_address(&agent.id, agent.role.as_ref())
        .filter(|manager| match manager {
            Address::Agent(manager) => manager != &agent.id && state.agent(manager).is_some(),
            Address::Owner | Address::System => true,
        })
        .unwrap_or(Address::Owner)
}

fn note(to: Address, body: String, task: Option<&&Task>) -> MessageContent {
    MessageContent {
        from: Address::System,
        to,
        kind: MessageKind::Note,
        body,
        attachments: Vec::new(),
        task: task.map(|task| task.id.clone()),
    }
}

/// The reminder to `agent` about `tasks`; `None` for no task.
fn reminder_note(state: &OrgState, agent: &Agent, tasks: &[&Task]) -> Option<MessageContent> {
    let first = tasks.first()?;
    let (id, hears) = if tasks.len() == 1 {
        (first.id.to_string(), told(state, agent, first).to_string())
    } else {
        ("<task>".to_owned(), "its creator".to_owned())
    };
    let mut body: String = tasks
        .iter()
        .map(|task| {
            format!(
                "Your turn ended and task {} (\"{}\") is still {}.\n",
                task.id,
                title(task),
                state_name(task)
            )
        })
        .collect();
    body.push_str(&format!(
        "Before you stop, say where it stands: atm task submit {id}, or atm task set-state {id} \
         waiting --reason '...' (or blocked). If you are still working, carry on; if you have \
         updated it since, ignore this. You get this once: if your next turn also ends this way, \
         {hears} is told."
    ));
    Some(note(Address::Agent(agent.id.clone()), body, tasks.first()))
}

/// The note to `to` about the tasks `agent` left silent a second time.
fn flag_note(agent: &Agent, to: Address, tasks: &[&Task]) -> MessageContent {
    let lines: Vec<String> = tasks
        .iter()
        .map(|task| {
            let reminded = task
                .reminded_at
                .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_default();
            format!(
                "Task {id} (\"{}\"): {} ended two turns without moving it from {}; reminded at \
                 {reminded}. atm task get {id}",
                title(task),
                agent.id,
                state_name(task),
                id = task.id
            )
        })
        .collect();
    note(to, lines.join("\n"), tasks.first())
}

/// Write one audit event. False when nothing was written: the task already
/// carries the mark, the rule's snapshot is out of date, or the service failed.
async fn record(org: &OrgHandle, command: TaskCommand, now: DateTime<Utc>) -> bool {
    match org.execute(OrgCommand::Task(command), now).await {
        Ok(events) => !events.is_empty(),
        Err(error) => {
            tracing::warn!(%error, "Reminder audit event was not recorded; nothing is sent for it");
            false
        }
    }
}

async fn send(org: &OrgHandle, note: MessageContent, now: DateTime<Utc>) {
    let to = note.to.clone();
    if let Err(error) = org.execute(OrgCommand::SendMessage(note), now).await {
        tracing::warn!(%to, %error, "Reminder note was not queued; its audit event stands");
    }
}

/// The tasks of `ids` that `state` knows, at most [`MAX_TASKS`].
fn known<'a>(state: &'a OrgState, ids: &'a [TaskId]) -> impl Iterator<Item = &'a Task> {
    ids.iter().filter_map(|id| state.task(id)).take(MAX_TASKS)
}

/// Carry out the rule's answer for `agent`: events first, then the notes.
async fn act(
    org: &OrgHandle,
    state: &OrgState,
    agent: &Agent,
    remind: &[TaskId],
    flag: &[TaskId],
    now: DateTime<Utc>,
) {
    let mut reminded = Vec::new();
    for task in known(state, remind) {
        let command = TaskCommand::Reminded {
            task_id: task.id.clone(),
            agent_id: agent.id.clone(),
        };
        if record(org, command, now).await {
            reminded.push(task);
        }
    }
    if let Some(note) = reminder_note(state, agent, &reminded) {
        send(org, note, now).await;
    }
    let mut flagged: Vec<(Address, Vec<&Task>)> = Vec::new();
    for task in known(state, flag) {
        let command = TaskCommand::Flagged {
            task_id: task.id.clone(),
            to: task.creator.clone(),
        };
        if !record(org, command, now).await {
            continue;
        }
        let to = told(state, agent, task);
        match flagged.iter_mut().find(|(address, _)| address == &to) {
            Some((_, tasks)) => tasks.push(task),
            None => flagged.push((to, vec![task])),
        }
    }
    for (to, tasks) in flagged {
        send(org, flag_note(agent, to, &tasks), now).await;
    }
}

/// Judge the turn end of `session`. The time to ask again, or `None` when the
/// turn end is used up.
async fn answer(
    org: &OrgHandle,
    session: &SessionId,
    ended_at: DateTime<Utc>,
    settings: &ReminderSettings,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let state = match org.snapshot().await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%session, %error, "Reminder skipped: organisation state unavailable");
            return None;
        }
    };
    let agent = state.agents().find(|agent| {
        agent.status == AgentStatus::Ready && agent.session_id.as_ref() == Some(session)
    })?;
    match reminder_step(&state, &agent.id, ended_at, settings, now) {
        ReminderStep::Nothing => None,
        ReminderStep::WaitUntil(at) => Some(at),
        ReminderStep::Act { remind, flag } => {
            act(org, &state, agent, &remind, &flag, now).await;
            None
        }
    }
}

/// Judge every wait that is over at `now`.
async fn settle(
    org: &OrgHandle,
    watch: &mut Watch,
    settings: &ReminderSettings,
    now: DateTime<Utc>,
) {
    let due: Vec<(SessionId, DateTime<Utc>)> = watch
        .pending
        .iter()
        .filter(|(_, pending)| pending.wake <= now)
        .map(|(session, pending)| (session.clone(), pending.ended_at))
        .collect();
    for (session, ended_at) in due {
        // Its own snapshot: a note just queued for another agent is mail on
        // its way to that agent.
        match answer(org, &session, ended_at, settings, now).await {
            Some(wake) => {
                if let Some(pending) = watch.pending.get_mut(&session) {
                    pending.wake = wake;
                }
            }
            None => {
                watch.pending.remove(&session);
            }
        }
    }
}

/// Registry events are read before a wake is taken (`biased`), so a turn
/// that started before the wait was over has dropped it.
async fn run(
    org: OrgHandle,
    mut rows: broadcast::Receiver<SessionEvent>,
    settings: ReminderSettings,
    clock: impl Fn() -> DateTime<Utc>,
    cancel: CancellationToken,
) {
    let mut watch = Watch::default();
    loop {
        // A clock that stepped back makes the wait longer, never shorter.
        let wait = watch
            .next_wake()
            .map(|wake| (wake - clock()).to_std().unwrap_or_default());
        let wake = async {
            match wait {
                Some(wait) => tokio::time::sleep(wait).await,
                None => std::future::pending().await,
            }
        };
        let row = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            row = rows.recv() => Some(row),
            () = wake => None,
        };
        match row {
            Some(Ok(row)) => watch.observe(&row, &settings, clock()),
            Some(Err(RecvError::Lagged(_))) => watch.forget(),
            Some(Err(RecvError::Closed)) => return,
            None => settle(&org, &mut watch, &settings, clock()).await,
        }
    }
}

/// Starts the scheduler on the registry's events `rows`. It runs until
/// `cancel` or until the registry stops.
pub fn spawn_task_reminders(
    org: OrgHandle,
    rows: broadcast::Receiver<SessionEvent>,
    settings: ReminderSettings,
    cancel: CancellationToken,
) {
    tokio::spawn(run(org, rows, settings, Utc::now, cancel));
}
