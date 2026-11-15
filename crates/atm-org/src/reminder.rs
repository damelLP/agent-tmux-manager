//! Pure reminder policy: what to do when an agent's turn has ended and its
//! task does not say why it stopped.
//!
//! [`reminder_step`] answers from the organisation's state, the end of the
//! agent's last turn and the time. Time is an argument; nothing here reads a
//! clock, does I/O or names a harness. The audit events, their refusals and
//! the "once per state" latch are the task module's
//! ([`TaskCommand::Reminded`](crate::TaskCommand::Reminded),
//! [`TaskCommand::Flagged`](crate::TaskCommand::Flagged)); this module only
//! says when to use them.
//!
//! # When a turn end is silent
//!
//! A turn end is silent for a task when the task is still in a state that
//! says its agent is acting: `assigned`, `working` or `changes requested`.
//! `waiting` and `blocked` are the agent saying why it stopped, and from
//! `submitted` on the next move is not the agent's. Whether the state changed
//! during the turn does not matter: a turn that took a task from `assigned`
//! to `working` and then stopped is silent, and a turn that left a task
//! `waiting` is not.
//!
//! A task counts for an agent when the agent holds it, it is in one of those
//! three states, and that state began no later than the turn's end. A task
//! assigned after the agent's last turn has not had a turn yet.
//!
//! | The task is due for | when it counts and |
//! |---|---|
//! | a reminder | it has not been reminded in this state |
//! | a flag | it was reminded at `r`, is not flagged, and the turn ended after `r` |
//!
//! A change of state clears both marks, so a task can be reminded once in
//! `assigned` and once more in `working`. Asking for the state a task is
//! already in writes nothing and clears nothing.
//!
//! # Decision table
//!
//! The earlier row wins. Every limit is inclusive.
//!
//! | # | Condition | Step |
//! |---|---|---|
//! | 1 | no task of the agent is due | `Nothing` |
//! | 2 | `now` is before `ended_at + delay` | `WaitUntil(ended_at + delay)` |
//! | 3 | a message to the agent is queued or delivering | `WaitUntil(now + delay)` |
//! | 4 | otherwise | `Act` with every due task, in the order the tasks were created |
//!
//! Row 3: mail on its way starts a turn of its own (a brief just assigned,
//! the reminder itself), and that turn's end is judged instead. The step asks
//! to look again rather than to wait for an event, because a message that
//! fails for good starts no turn. A time the calendar cannot hold is never
//! due and answers `Nothing`.
//!
//! # What the caller owns
//!
//! - `ended_at` is the end of the last turn seen in the agent's session, and
//!   the session has been idle ever since. When a turn starts, or the agent
//!   leaves that session, the caller drops the turn end and does not ask.
//! - What counts as a turn's end for a harness, and when that signal misleads.
//! - `Act`: record each task's audit event first, then send one message for
//!   the tasks whose event was written. The event is the latch, so a failure
//!   between the two loses a reminder and never sends a second one.
//! - After `Act` or `Nothing` the turn end is used up. The policy keeps no
//!   memory: the marks it reads are in the state.

use crate::{Address, AgentId, DeliveryStatus, OrgState, TaskId, TaskState};
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;

/// How long an agent stays idle after a silent turn end before anything is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderSettings {
    pub delay: Duration,
}

impl Default for ReminderSettings {
    /// One minute.
    fn default() -> Self {
        Self {
            delay: Duration::from_secs(60),
        }
    }
}

/// What to do about one agent's last turn end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderStep {
    /// Nothing is due for this turn end, now or later.
    Nothing,
    /// Ask again at this time, if the agent is still idle then.
    WaitUntil(DateTime<Utc>),
    /// Remind the agent of `remind`, and tell the creators of `flag`.
    Act {
        remind: Vec<TaskId>,
        flag: Vec<TaskId>,
    },
}

/// `delay` after `from`, when the calendar can hold it.
fn after(from: DateTime<Utc>, delay: Duration) -> Option<DateTime<Utc>> {
    from.checked_add_signed(TimeDelta::from_std(delay).ok()?)
}

/// The step for `agent`, whose last turn ended at `ended_at` and who has been
/// idle since. See the module's tables.
#[must_use]
pub fn reminder_step(
    state: &OrgState,
    agent: &AgentId,
    ended_at: DateTime<Utc>,
    settings: &ReminderSettings,
    now: DateTime<Utc>,
) -> ReminderStep {
    let mut remind = Vec::new();
    let mut flag = Vec::new();
    let counts = state.tasks().filter(|task| {
        task.agent_id.as_ref() == Some(agent)
            && matches!(
                task.state,
                TaskState::Assigned | TaskState::Working | TaskState::ChangesRequested
            )
            && task.changed_at <= ended_at
    });
    for task in counts {
        match (task.reminded_at, task.flagged_at) {
            (None, _) => remind.push(task.id.clone()),
            (Some(reminded), None) if ended_at > reminded => flag.push(task.id.clone()),
            _ => {}
        }
    }
    if remind.is_empty() && flag.is_empty() {
        return ReminderStep::Nothing;
    }
    let Some(limit) = after(ended_at, settings.delay) else {
        return ReminderStep::Nothing;
    };
    if now < limit {
        return ReminderStep::WaitUntil(limit);
    }
    let mail_on_its_way = state.messages().any(|message| {
        matches!(&message.content.to, Address::Agent(to) if to == agent)
            && matches!(
                message.status,
                DeliveryStatus::Queued | DeliveryStatus::Delivering
            )
    });
    if mail_on_its_way {
        return after(now, settings.delay).map_or(ReminderStep::Nothing, ReminderStep::WaitUntil);
    }
    ReminderStep::Act { remind, flag }
}
